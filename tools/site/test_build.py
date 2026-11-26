"""Unit tests for the documentation site build, on synthetic inputs only."""

import contextlib
import io
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import build  # noqa: E402

PAGES = {"Home", "Getting-Started", "FAQ"}


def rewrite(text):
    problems = []
    return build.rewrite_links(text, PAGES, "Test.md", problems), problems


class RewriteLinksTest(unittest.TestCase):
    def test_page_links_become_md_links(self):
        out, problems = rewrite("See [start](Getting-Started) and [faq](FAQ#why).")
        self.assertEqual(out, "See [start](Getting-Started.md) and [faq](FAQ.md#why).")
        self.assertEqual(problems, [])

    def test_home_maps_to_index(self):
        out, _ = rewrite("[back](Home) [top](Home#what-it-does)")
        self.assertEqual(out, "[back](index.md) [top](index.md#what-it-does)")

    def test_external_anchor_and_file_links_are_untouched(self):
        text = (
            "[a](https://example.com/FAQ) [b](#local) [c](/abs) [d](<FAQ>) "
            "![img](images/page/picture.jpg) [e](mailto:someone@example.com) [f](notes.txt)"
        )
        out, problems = rewrite(text)
        self.assertEqual(out, text)
        self.assertEqual(problems, [])

    def test_missing_page_is_reported(self):
        out, problems = rewrite("[gone](No-Such-Page#x)")
        self.assertEqual(out, "[gone](No-Such-Page#x)")
        self.assertEqual(problems, ["Test.md: link to missing page 'No-Such-Page#x'"])

    def test_code_label_is_rewritten(self):
        out, _ = rewrite("| [`OUTPACE_X`](FAQ#outpace-x) | `false` |")
        self.assertEqual(out, "| [`OUTPACE_X`](FAQ.md#outpace-x) | `false` |")

    def test_links_inside_code_are_untouched(self):
        text = (
            "Inline `[x](FAQ)` stays.\n\n"
            "```md\n[y](FAQ)\n```\n\n"
            "1. Step:\n\n   ```sh\n   echo '[z](Missing)'\n   ```\n\n"
            "After [w](FAQ).\n"
        )
        out, problems = rewrite(text)
        self.assertEqual(out, text.replace("[w](FAQ)", "[w](FAQ.md)"))
        self.assertEqual(problems, [])

    def test_longer_fence_wraps_a_shorter_one(self):
        text = (
            "````md\n```sh\n[x](FAQ)\n```\n[y](FAQ)\n````\n\n"
            "~~~\n```\n[z](FAQ)\n~~~~\n\nAfter [w](FAQ).\n"
        )
        out, problems = rewrite(text)
        self.assertEqual(out, text.replace("[w](FAQ)", "[w](FAQ.md)"))
        self.assertEqual(problems, [])

    def test_url_encoded_page_name(self):
        out, _ = rewrite("[s](Getting%2DStarted)")
        self.assertEqual(out, "[s](Getting-Started.md)")


class DescriptionTest(unittest.TestCase):
    def test_first_prose_paragraph_as_plain_text(self):
        text = (
            "<!-- generated note -->\n\n# Title\n\n"
            "Outpace **plays** [public streams](FAQ) with `mpv`,\nover _HTTP_.\n\n"
            "Second paragraph.\n"
        )
        self.assertEqual(build.describe(text), "Outpace plays public streams with mpv, over HTTP.")

    def test_skips_lists_tables_code_quotes_and_images(self):
        text = (
            "# T\n\n![pic](images/a.jpg)\n\n- item\n- item\n\n| a | b |\n|---|---|\n\n"
            "> quote\n\n```sh\nnot prose\n```\n\n    indented code\n\nReal prose here.\n"
        )
        self.assertEqual(build.describe(text), "Real prose here.")

    def test_truncates_at_a_word(self):
        text = "# T\n\n" + " ".join(["word"] * 60) + "\n"
        description = build.describe(text, limit=40)
        self.assertLessEqual(len(description), 40)
        self.assertTrue(description.endswith("word…"))

    def test_empty_when_no_prose(self):
        self.assertEqual(build.describe("# Only a title\n\n- a list\n"), "")


class TitleTest(unittest.TestCase):
    def test_first_h1(self):
        self.assertEqual(build.page_title("intro\n\n# The `serve` command\n\n# Later\n", "X"), "The serve command")

    def test_h1_inside_code_is_ignored(self):
        text = "```sh\n# not a heading\n```\n\n## Section\n"
        self.assertEqual(build.page_title(text, "Getting-Started"), "Getting Started")


class SidebarTest(unittest.TestCase):
    def test_sidebar_becomes_summary(self):
        sidebar = (
            "**[Home](Home)**\n\n**Start**\n\n- [FAQ](FAQ)\n- [Getting started](Getting-Started)\n"
        )
        summary, order, problems = build.summary_from_sidebar(sidebar, PAGES)
        self.assertEqual(
            summary,
            "# Summary\n\n[Home](index.md)\n\n# Start\n\n"
            "- [FAQ](FAQ.md)\n- [Getting started](Getting-Started.md)\n",
        )
        self.assertEqual(order, ["Home", "FAQ", "Getting-Started"])
        self.assertEqual(problems, [])

    def test_unlisted_missing_and_unknown_lines_are_reported(self):
        sidebar = "**[Home](Home)**\n\n- [Gone](Gone)\n\nSome text\n"
        _, _, problems = build.summary_from_sidebar(sidebar, PAGES)
        self.assertEqual(
            problems,
            [
                "_Sidebar.md: link to missing page in '- [Gone](Gone)'",
                "_Sidebar.md: unrecognised line 'Some text'",
                "_Sidebar.md: FAQ is not listed, so the site would not render it",
                "_Sidebar.md: Getting-Started is not listed, so the site would not render it",
            ],
        )


class MetaTest(unittest.TestCase):
    DOC = (
        "<head>\n<title>FAQ - Outpace</title>\n"
        '<meta name="description" content="book wide">\n</head>'
    )

    def test_title_description_and_open_graph(self):
        meta = build.PageMeta("FAQ", 'Q & "A"', "Short <answer>.", "https://example.org/docs/FAQ.html")
        out = build.apply_meta(self.DOC, meta, "https://example.org/docs/social.png", (1280, 640))
        self.assertIn("<title>Q &amp; \"A\" - Outpace</title>", out)
        self.assertIn('<meta name="description" content="Short &lt;answer&gt;.">', out)
        self.assertIn('<meta property="og:title" content="Q &amp; &quot;A&quot;">', out)
        self.assertIn('<meta property="og:url" content="https://example.org/docs/FAQ.html">', out)
        self.assertIn('<link rel="canonical" href="https://example.org/docs/FAQ.html">', out)
        self.assertIn('<meta property="og:image" content="https://example.org/docs/social.png">', out)
        self.assertIn('<meta property="og:image:width" content="1280">', out)
        self.assertNotIn("book wide", out)

    def test_home_title_has_no_suffix(self):
        meta = build.PageMeta("Home", "Outpace docs", "d", "https://example.org/")
        self.assertIn("<title>Outpace docs</title>", build.apply_meta(self.DOC, meta, "i", (1, 1)))

    def test_unexpected_template_fails(self):
        meta = build.PageMeta("FAQ", "t", "d", "u")
        with self.assertRaises(build.BuildError):
            build.apply_meta("<head></head>", meta, "i", (1, 1))

    def test_sitemap(self):
        xml = build.sitemap(["https://example.org/", "https://example.org/FAQ.html?a&b"])
        self.assertIn("<url><loc>https://example.org/</loc></url>", xml)
        self.assertIn("<loc>https://example.org/FAQ.html?a&amp;b</loc>", xml)


class TomlTest(unittest.TestCase):
    def test_utf8_quotes_backslashes_and_controls(self):
        value = 'Live \U0001F4FA "streams" \\ caf\u00e9\tend\x7f'
        rendered = build.toml_string(value)
        self.assertIn("\U0001F4FA", rendered)
        self.assertNotIn("\\ud83d", rendered.lower())
        self.assertEqual(tomllib.loads(f"v = {rendered}")["v"], value)

    def test_book_toml_parses(self):
        config = tomllib.loads(build.book_toml("Outpace \U0001F680 docs", "/docs/"))
        self.assertEqual(config["book"]["description"], "Outpace \U0001F680 docs")


FAKE_MDBOOK = """#!/usr/bin/env python3
# Stands in for mdBook: renders each SUMMARY.md entry and copies other source files.
import re, shutil, sys
from pathlib import Path
dest, book = Path(sys.argv[3]), Path(sys.argv[4])
src = book / "src"
shutil.copytree(src, dest, ignore=shutil.ignore_patterns("*.md"))
for page in re.findall(r"\\]\\(([^)]+)\\.md\\)", (src / "SUMMARY.md").read_text()):
    (dest / (page + ".html")).write_text(
        '<html><head><title>x</title><meta name="description" content="x"></head>'
        '<body><a href="notes.txt">notes</a></body></html>'
    )
"""


class BuildTest(unittest.TestCase):
    def run_build(self, sidebar, strict):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            wiki = root / "wiki"
            wiki.mkdir()
            (wiki / "Home.md").write_text("# Home\n\nThe home page.\n")
            (wiki / "Extra.md").write_text("# Extra\n\nAn extra page.\n")
            (wiki / "notes.txt").write_text("plain notes\n")
            (wiki / "_Sidebar.md").write_text(sidebar)
            mdbook = root / "mdbook"
            mdbook.write_text(FAKE_MDBOOK)
            mdbook.chmod(0o755)
            social = root / "social.png"
            social.write_bytes(b"\x89PNG\r\n\x1a\n" + bytes(8) + (2).to_bytes(4, "big") + (1).to_bytes(4, "big"))
            out = root / "site"
            with contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(io.StringIO()):
                code = build.build(wiki, out, str(mdbook), "https://example.org/docs", social, strict)
            files = sorted(str(p.relative_to(out)) for p in out.rglob("*")) if out.exists() else []
            return code, files

    def test_root_files_are_copied_and_listed_pages_built(self):
        code, files = self.run_build("**[Home](Home)**\n\n- [Extra](Extra)\n", strict=True)
        self.assertEqual(code, 0)
        self.assertIn("notes.txt", files)
        self.assertIn("Extra.html", files)

    def test_unlisted_page_fails_strict_and_is_skipped_otherwise(self):
        sidebar = "**[Home](Home)**\n"
        self.assertEqual(self.run_build(sidebar, strict=True), (1, []))
        code, files = self.run_build(sidebar, strict=False)
        self.assertEqual(code, 0)
        self.assertIn("index.html", files)
        self.assertNotIn("Extra.html", files)


class CheckOutputTest(unittest.TestCase):
    def test_broken_links_and_anchors(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "img.png").write_bytes(b"")
            (root / "a.html").write_text(
                '<h2 id="here">x</h2><a href="b.html#there">ok</a><a href="#here">ok</a>'
                '<img src="img.png"><a href="https://example.com/x">ext</a>'
                '<a href="missing.html">bad</a><a href="b.html#nowhere">bad</a>',
                encoding="utf-8",
            )
            (root / "b.html").write_text('<p id="there"></p><a href="a.html?v=1">ok</a>', encoding="utf-8")
            self.assertEqual(
                build.check_output(root),
                ["a.html: broken link 'missing.html'", "a.html: missing anchor 'b.html#nowhere'"],
            )


if __name__ == "__main__":
    unittest.main()
