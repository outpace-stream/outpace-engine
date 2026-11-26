#!/usr/bin/env python3
"""Check that the reference docs cover every env var and HTTP route the daemon defines.

Usage: check_reference.py [REPO_ROOT]   (default: the current directory)

Two checks, both over the non-test Rust source under `crates/*/src`:

- Every `OUTPACE_*` name that appears inside a string literal must be documented in
  `docs/configuration.md`, or in one of the pages that page explicitly delegates to
  (`DELEGATED_CONFIG_DOCS`, each of which must still be linked from it). Names matching
  `TEST_ONLY_VARS` are skipped.
- Every path registered with axum's `.route(...)` / `.nest(...)` must be mentioned in
  `docs/native-api.md`. A path is reduced to its static prefix: a fully static path such as
  `/debug/memstats` must appear whole, and a parameterized one such as
  `/support/{name}/start` must have its prefix up to the first parameter (`/support/`)
  appear. Compatibility routes (`COMPAT_PREFIXES`) may instead be documented in
  `docs/protocol/compat-matrix.md`.

A file is test code when it lives under a `tests` directory or is named `tests.rs`,
`tests_*.rs`, `*_tests.rs` or `test_*.rs`; `#[cfg(test)] mod name { ... }` blocks inside
other files are skipped too. Comments are ignored. Exits non-zero on any gap.
"""

from __future__ import annotations

import fnmatch
import re
import sys
from pathlib import Path

CONFIG_DOC = "docs/configuration.md"
API_DOC = "docs/native-api.md"
COMPAT_DOC = "docs/protocol/compat-matrix.md"

# Pages `docs/configuration.md` hands whole groups of variables to. A name documented in one of
# these counts as covered, but only while configuration.md still links to the page.
DELEGATED_CONFIG_DOCS = (
    "docs/live-audio.md",  # OUTPACE_HLS_AUDIO_NORMALIZE, OUTPACE_LIVE_AUDIO_NORMALIZE, FFmpeg
    "docs/srt-ingest.md",  # OUTPACE_SRT_* (optional `srt` build feature)
    "docs/private-peer-mode.md",  # OUTPACE_PRIVATE_PEERS / OUTPACE_PRIVATE_DESCRIPTORS detail
)

# Names that are not operator settings, as shell-style patterns. Keep this list to test and
# harness hooks only; the wiki's Environment-Variables page declares the same families
# test-only. Everything else the daemon reads must be documented.
TEST_ONLY_VARS = (
    "OUTPACE_TEST_*",  # test-suite hooks (fake children, fixture paths, live-test inputs)
    "OUTPACE_RUN_*",  # opt-in runs of slow or external-tool tests
    "OUTPACE_DHT_COMPARE_*",  # private env protocol between tools/dhtcompare and its worker
    "OUTPACE_CAST_LIVE",  # ignored live renderer-discovery test
)

# Route prefixes owned by the Ace compatibility adapter, documented in the compat matrix.
COMPAT_PREFIXES = ("/ace/", "/server/", "/webui/")

ENV_NAME = re.compile(r"OUTPACE_[A-Z0-9_]+")
# Rust lexemes that can hide or contain quotes, in priority order. Char literals come before
# strings so `'"'` is not read as a string opener; a lifetime (`'a`) never matches `'.'`.
LEXEME = re.compile(
    r"""
    (?P<line_comment>//[^\n]*)
  | (?P<block_comment>/\*)
  | (?P<raw_string>b?r(?P<hashes>\#*)"(?P<raw_body>.*?)"(?P=hashes))
  | (?P<char>b?'(?:\\.|\\u\{[0-9a-fA-F]+\}|[^'\\\n])')
  | (?P<string>b?"(?P<body>(?:\\.|[^"\\])*)")
    """,
    re.S | re.X,
)
CFG_TEST_MOD = re.compile(
    r"\#\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:\#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{"
)
ROUTE_METHODS = frozenset({"route", "nest", "route_service", "nest_service"})
BLOCK_DELIMITER = re.compile(r"/\*|\*/")
LINK = re.compile(r"\]\(([^)\s#]+)(?:#[^)\s]*)?\)")


def is_test_file(path: Path) -> bool:
    name = path.name
    return (
        "tests" in path.parts
        or name == "tests.rs"
        or name.startswith(("tests_", "test_"))
        or name.endswith("_tests.rs")
    )


def lexemes(source: str) -> list[tuple[int, int, str | None]]:
    """`(start, end, string body or None)` for every comment and string literal, in order.

    Block comments nest in Rust, so their end is found by depth rather than by the regex.
    """
    found: list[tuple[int, int, str | None]] = []
    at = 0
    while (match := LEXEME.search(source, at)) is not None:
        kind, start, end = match.lastgroup, match.start(), match.end()
        body = None
        if kind == "block_comment":
            depth = 1
            while depth and (delimiter := BLOCK_DELIMITER.search(source, end)) is not None:
                depth += 1 if delimiter.group() == "/*" else -1
                end = delimiter.end()
            if depth:
                end = len(source)
        elif kind == "raw_string":
            body = match.group("raw_body")
        elif kind == "string":
            body = match.group("body")
        found.append((start, end, body))
        at = end
    return found


def mask(source: str, spans: list[tuple[int, int, str | None]]) -> str:
    """Blank every lexeme, keeping newlines, so braces and calls inside them do not count."""
    masked = list(source)
    for start, end, _ in spans:
        for i in range(start, end):
            if masked[i] != "\n":
                masked[i] = " "
    return "".join(masked)


def string_literals(source: str) -> list[tuple[int, str]]:
    """Return `(offset, body)` for each string literal outside comments and test modules."""
    spans = lexemes(source)
    skipped = test_module_spans(mask(source, spans))
    return [
        (start, body)
        for start, _, body in spans
        if body is not None and not any(a <= start < b for a, b in skipped)
    ]


def test_module_spans(code: str) -> list[tuple[int, int]]:
    spans = []
    for match in CFG_TEST_MOD.finditer(code):
        depth, i = 1, match.end()
        while i < len(code) and depth:
            depth += {"{": 1, "}": -1}.get(code[i], 0)
            i += 1
        spans.append((match.start(), i))
    return spans


def route_paths(source: str) -> list[str]:
    """Paths passed as the first argument of a router `.route(...)`-style call."""
    paths = []
    masked = mask(source, lexemes(source))
    for at, body in string_literals(source):
        if not body.startswith("/"):
            continue
        if is_first_route_argument(masked, at):
            paths.append(body)
    return paths


def is_first_route_argument(masked: str, at: int) -> bool:
    """Whether the `(` before `at` opens a router call, skipping masked comments of any length."""
    i = skip_space_back(masked, at)
    if not i or masked[i - 1] != "(":
        return False
    i = skip_space_back(masked, i - 1)
    start = i
    while start and (masked[start - 1].isalnum() or masked[start - 1] == "_"):
        start -= 1
    if masked[start:i] not in ROUTE_METHODS:
        return False
    start = skip_space_back(masked, start)
    return bool(start) and masked[start - 1] == "."


def skip_space_back(masked: str, i: int) -> int:
    while i and masked[i - 1].isspace():
        i -= 1
    return i


def rust_sources(root: Path) -> list[Path]:
    return sorted(
        path
        for path in root.glob("crates/*/src/**/*.rs")
        if not is_test_file(path.relative_to(root))
    )


def env_names(root: Path) -> dict[str, str]:
    """Every referenced name, mapped to the first file that references it."""
    names: dict[str, str] = {}
    for path in rust_sources(root):
        for _, body in string_literals(path.read_text(encoding="utf-8")):
            for name in ENV_NAME.findall(body):
                names.setdefault(name, str(path.relative_to(root)))
    return names


def routes(root: Path) -> dict[str, str]:
    found: dict[str, str] = {}
    for path in rust_sources(root):
        for route in route_paths(path.read_text(encoding="utf-8")):
            found.setdefault(route, str(path.relative_to(root)))
    return found


def is_test_only(name: str) -> bool:
    # A trailing `_` is a prefix the code completes at runtime (`OUTPACE_DHT_COMPARE_{key}`).
    probe = name + "X" if name.endswith("_") else name
    return any(fnmatch.fnmatchcase(probe, pattern) for pattern in TEST_ONLY_VARS)


def documented_names(text: str) -> set[str]:
    return set(ENV_NAME.findall(text))


def route_key(route: str) -> str:
    """The part of a route the docs must mention: all of it, or up to the first parameter."""
    return route.split("{", 1)[0]


def mentions(text: str, key: str) -> bool:
    """Whether `key` appears as a whole route, not as the tail of a longer path."""
    start = r"(?<![A-Za-z0-9_.\-/])"
    end = "" if key.endswith("/") else r"(?![A-Za-z0-9_.-])"
    return re.search(start + re.escape(key) + end, text) is not None


def linked_docs(doc: Path) -> set[Path]:
    text = doc.read_text(encoding="utf-8")
    return {(doc.parent / target).resolve() for target in LINK.findall(text)}


def check(root: Path) -> list[str]:
    errors: list[str] = []
    config = root / CONFIG_DOC
    covered = documented_names(config.read_text(encoding="utf-8"))
    links = linked_docs(config)
    for delegate in DELEGATED_CONFIG_DOCS:
        path = root / delegate
        if path.resolve() not in links:
            errors.append(f"{CONFIG_DOC} no longer links delegated page {delegate}")
            continue
        covered |= documented_names(path.read_text(encoding="utf-8"))
    for name, source in sorted(env_names(root).items()):
        if not is_test_only(name) and name not in covered:
            errors.append(f"{name} (referenced in {source}) is not documented in {CONFIG_DOC}")

    api = (root / API_DOC).read_text(encoding="utf-8")
    compat = (root / COMPAT_DOC).read_text(encoding="utf-8")
    for route, source in sorted(routes(root).items()):
        key = route_key(route)
        if mentions(api, key):
            continue
        if key.startswith(COMPAT_PREFIXES) and mentions(compat, key):
            continue
        where = f"{API_DOC} or {COMPAT_DOC}" if key.startswith(COMPAT_PREFIXES) else API_DOC
        errors.append(f"route {route} (in {source}) is not documented in {where} as {key!r}")
    return errors


def main(argv: list[str]) -> int:
    root = Path(argv[1] if len(argv) > 1 else ".")
    errors = check(root)
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        print(f"{len(errors)} reference doc gap(s)", file=sys.stderr)
        return 1
    print(f"reference docs cover {len(env_names(root))} env names and {len(routes(root))} routes")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
