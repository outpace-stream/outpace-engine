"""Unit tests for check_reference.py, on small synthetic trees."""

import contextlib
import io
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_reference as cr  # noqa: E402

ROUTER = textwrap.dedent(
    """
    pub fn router() -> Router {
        Router::new()
            .route("/healthz", get(health))
            .route(
                "/things/{id}/status",
                get(status),
            )
            .route("/ace/getstream", get(compat))
    }
    """
)

CONFIG = textwrap.dedent(
    """
    # Configuration reference

    Audio settings are documented in [live audio](live-audio.md), SRT in
    [SRT ingest](srt-ingest.md) and private mode in [private](private-peer-mode.md#limits).

    - `OUTPACE_BIND` - HTTP bind address.
    """
)


class Tree:
    """A throwaway repo root with just the files the checker reads."""

    def __init__(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)
        self.write("docs/live-audio.md", "- `OUTPACE_FFMPEG_PATH` - FFmpeg.\n")
        self.write("docs/srt-ingest.md", "")
        self.write("docs/private-peer-mode.md", "")
        self.write("docs/configuration.md", CONFIG)
        self.write("docs/native-api.md", "| `GET /healthz` | ok |\n| `GET /things/<id>/status` |\n")
        self.write("docs/protocol/compat-matrix.md", "`GET /ace/getstream`\n")
        self.write("crates/ace-engine/src/http.rs", ROUTER)
        self.write(
            "crates/ace-engine/src/runtime.rs",
            'fn f() { std::env::var("OUTPACE_BIND"); std::env::var("OUTPACE_FFMPEG_PATH"); }\n',
        )

    def write(self, relative: str, text: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def close(self) -> None:
        self._dir.cleanup()


class LexingTests(unittest.TestCase):
    def test_comments_are_not_string_literals(self):
        source = textwrap.dedent(
            """
            // "OUTPACE_IN_LINE_COMMENT"
            /* "OUTPACE_IN_BLOCK_COMMENT" */
            /// docs mention OUTPACE_IN_DOC_COMMENT
            const A: &str = "OUTPACE_REAL"; // trailing "OUTPACE_TRAILING"
            """
        )
        bodies = [body for _, body in cr.string_literals(source)]
        self.assertEqual(bodies, ["OUTPACE_REAL"])

    def test_raw_byte_and_escaped_strings(self):
        source = (
            'let a = r#"OUTPACE_RAW "quoted""#;\n'
            'let b = b"OUTPACE_BYTES";\n'
            'let c = "esc \\" OUTPACE_ESCAPED";\n'
            "let q = '\"'; let d = \"OUTPACE_AFTER_CHAR\";\n"
            "fn l<'a>(x: &'a str) -> &'a str { \"OUTPACE_AFTER_LIFETIME\" }\n"
        )
        names = {n for _, body in cr.string_literals(source) for n in cr.ENV_NAME.findall(body)}
        self.assertEqual(
            names,
            {
                "OUTPACE_RAW",
                "OUTPACE_BYTES",
                "OUTPACE_ESCAPED",
                "OUTPACE_AFTER_CHAR",
                "OUTPACE_AFTER_LIFETIME",
            },
        )

    def test_cfg_test_modules_are_skipped(self):
        source = textwrap.dedent(
            """
            const A: &str = "OUTPACE_KEPT";
            #[cfg(test)]
            mod tests {
                fn t() { let s = "}"; std::env::set_var("OUTPACE_IN_TESTS", s); }
            }
            const B: &str = "OUTPACE_AFTER_TESTS";
            """
        )
        bodies = [body for _, body in cr.string_literals(source)]
        self.assertEqual(bodies, ["OUTPACE_KEPT", "OUTPACE_AFTER_TESTS"])

    def test_route_paths_handle_multiline_calls(self):
        self.assertEqual(
            cr.route_paths(ROUTER), ["/healthz", "/things/{id}/status", "/ace/getstream"]
        )

    def test_route_paths_ignore_other_strings(self):
        self.assertEqual(cr.route_paths('let x = "/not/a/route"; f.get("/x");'), [])


class ClassificationTests(unittest.TestCase):
    def test_test_files(self):
        tests = ("a/tests/x.rs", "a/tests.rs", "a/tests_perf.rs", "a/x_tests.rs", "a/test_x.rs")
        for name in tests:
            self.assertTrue(cr.is_test_file(Path(name)), name)
        for name in ("a/testsuite.rs", "a/latest.rs", "a/http.rs"):
            self.assertFalse(cr.is_test_file(Path(name)), name)

    def test_test_only_allowlist(self):
        self.assertTrue(cr.is_test_only("OUTPACE_TEST_SRT_FIFO"))
        self.assertTrue(cr.is_test_only("OUTPACE_DHT_COMPARE_"))
        self.assertTrue(cr.is_test_only("OUTPACE_CAST_LIVE"))
        self.assertFalse(cr.is_test_only("OUTPACE_BIND"))
        self.assertFalse(cr.is_test_only("OUTPACE_TESTING"))

    def test_route_keys(self):
        self.assertEqual(cr.route_key("/debug/memstats"), "/debug/memstats")
        self.assertEqual(cr.route_key("/support/{name}/start"), "/support/")
        self.assertEqual(cr.route_key("/cast/{*path}"), "/cast/")

    def test_static_mentions_need_a_boundary(self):
        self.assertTrue(cr.mentions("`GET /networks` and more", "/networks"))
        self.assertTrue(cr.mentions("`GET /streams/<id>`", "/streams"))
        self.assertFalse(cr.mentions("`GET /network-policy`", "/network"))
        self.assertFalse(cr.mentions("`GET /networks`", "/network"))
        self.assertTrue(cr.mentions("`/support/<name>/stop`", "/support/"))

    def test_documented_names_are_exact(self):
        self.assertEqual(
            cr.documented_names("`OUTPACE_HISTORY_TOTAL_BYTES` and `OUTPACE_SRT_*`"),
            {"OUTPACE_HISTORY_TOTAL_BYTES", "OUTPACE_SRT_"},
        )


class CheckTests(unittest.TestCase):
    def setUp(self):
        self.tree = Tree()
        self.addCleanup(self.tree.close)

    def test_complete_tree_passes(self):
        self.assertEqual(cr.check(self.tree.root), [])

    def test_undocumented_env_var_fails(self):
        self.tree.write(
            "crates/ace-swarm/src/seed.rs", 'fn f() { std::env::var_os("OUTPACE_NEW_KNOB"); }\n'
        )
        errors = cr.check(self.tree.root)
        self.assertEqual(len(errors), 1)
        self.assertIn("OUTPACE_NEW_KNOB", errors[0])
        self.assertIn("crates/ace-swarm/src/seed.rs", errors[0])

    def test_env_vars_in_tests_and_allowlist_are_ignored(self):
        self.tree.write("crates/ace-engine/src/x/tests.rs", '"OUTPACE_ONLY_IN_TESTS"\n')
        self.tree.write("crates/ace-engine/tests/it.rs", '"OUTPACE_ONLY_IN_IT"\n')
        self.tree.write("crates/ace-engine/src/y.rs", 'const T: &str = "OUTPACE_TEST_HOOK";\n')
        self.assertEqual(cr.check(self.tree.root), [])

    def test_delegated_page_counts_only_while_linked(self):
        self.tree.write("docs/configuration.md", "- `OUTPACE_BIND` - bind.\n")
        errors = cr.check(self.tree.root)
        unlinked = "docs/configuration.md no longer links delegated page docs/live-audio.md"
        self.assertIn(unlinked, errors)
        self.assertTrue(any("OUTPACE_FFMPEG_PATH" in error for error in errors))

    def test_undocumented_route_fails(self):
        self.tree.write(
            "crates/ace-engine/src/http/extra.rs",
            'fn r() -> Router { Router::new().route("/debug/new", get(x)) }\n',
        )
        errors = cr.check(self.tree.root)
        self.assertEqual(len(errors), 1)
        self.assertIn("/debug/new", errors[0])

    def test_compat_route_may_live_in_the_matrix_only(self):
        self.tree.write("docs/protocol/compat-matrix.md", "nothing here\n")
        errors = cr.check(self.tree.root)
        self.assertEqual(len(errors), 1)
        self.assertIn("/ace/getstream", errors[0])
        self.assertIn("compat-matrix.md", errors[0])

    def test_native_route_is_not_covered_by_the_matrix(self):
        self.tree.write("docs/native-api.md", "| `GET /things/<id>/status` |\n")
        self.tree.write("docs/protocol/compat-matrix.md", "`/ace/getstream` `/healthz`\n")
        errors = cr.check(self.tree.root)
        self.assertEqual(len(errors), 1)
        self.assertIn("/healthz", errors[0])

    def test_main_reports_exit_status(self):
        argv = ["check_reference.py", str(self.tree.root)]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(cr.main(argv), 0)
        self.tree.write("docs/native-api.md", "")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(cr.main(argv), 1)
        self.assertIn("2 reference doc gap(s)", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
