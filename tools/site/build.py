#!/usr/bin/env python3
"""Build the documentation website from docs/wiki/ with mdBook.

Usage: build.py [--wiki DIR] [--out DIR] [--mdbook PATH] [--site-url URL]
                [--social-image PATH] [--strict]

docs/wiki/ stays the only source: this script copies it into a temporary mdBook
project, converts what GitHub-wiki Markdown needs converting, runs mdBook, and
then fills in what mdBook cannot produce per page. Nothing in docs/wiki/ is
changed.

- `[text](Page-Name)` and `[text](Page-Name#anchor)` links to existing wiki
  pages become `Page-Name.md` links, which mdBook turns into `.html` links.
  External links, `#anchor` links, images and other files are left alone.
- Home.md becomes the book's index.md, so it renders as the site's index.html.
- _Sidebar.md becomes SUMMARY.md, the site navigation: a bold line is a section
  title, a bold link is the leading Home entry and list links are pages.
- _Footer.md is appended to every page.
- Each page gets a <title> and meta description from its first H1 and first
  prose paragraph, a canonical URL and Open Graph tags using the social preview
  image; sitemap.xml lists every page.

Links between pages are checked with tools/wiki/check_links.py first, and the
generated site is crawled afterwards: every relative href/src must exist and
every fragment must match an id on its target page. With --strict any problem
fails the build; without it problems are printed as warnings.
"""

from __future__ import annotations

import argparse
import html
import importlib.util
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

REPO = Path(__file__).resolve().parents[2]
SITE_NAME = "Outpace"
SITE_URL = "https://outpace-stream.github.io/outpace-engine/"
REPOSITORY_URL = "https://github.com/outpace-stream/outpace-engine"
SOCIAL_IMAGE = "social-preview.png"
DESCRIPTION_LIMIT = 160


def _load_check_links():
    path = REPO / "tools" / "wiki" / "check_links.py"
    spec = importlib.util.spec_from_file_location("check_links", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


check_links = _load_check_links()
LINK = check_links.LINK
SCHEME = check_links.SCHEME
FILE_EXTENSION = check_links.FILE_EXTENSION

FENCE_LINE = re.compile(r"^\s*(`{3,}|~{3,})")
INLINE_CODE = re.compile(r"(`+)(?:(?!\n[ \t]*\n).)+?\1", re.S)
H1 = re.compile(r"^#\s+(.*?)\s*#*\s*$")
HTML_COMMENT = re.compile(r"<!--.*?-->", re.S)


class BuildError(Exception):
    pass


# --- Markdown conversion ---------------------------------------------------


def book_file(page: str) -> str:
    """The mdBook source file a wiki page is written to."""
    return "index.md" if page == "Home" else f"{page}.md"


def html_file(page: str) -> str:
    """The output file mdBook renders a wiki page to."""
    return "index.html" if page == "Home" else f"{page}.html"


def closes_fence(line: str, fence: str) -> bool:
    """CommonMark: a closing fence uses the opening character, at least as many times."""
    run = line.strip()
    return len(run) >= len(fence) and run == fence[0] * len(run)


def _blank(chars: list[str], start: int, end: int, keep: str = "\n") -> None:
    for i in range(start, end):
        if chars[i] not in keep:
            chars[i] = "\0"


def mask_code(text: str) -> str:
    """`text` with fenced blocks and inline code blanked out, positions unchanged."""
    chars = list(text)
    marker = None
    start = offset = 0
    for line in text.splitlines(keepends=True):
        match = FENCE_LINE.match(line)
        if marker is None and match:
            marker, start = match.group(1), offset
        elif marker is not None and closes_fence(line, marker):
            _blank(chars, start, offset + len(line))
            marker = None
        offset += len(line)
    if marker is not None:
        _blank(chars, start, len(text))
    fenced = "".join(chars)
    for match in INLINE_CODE.finditer(fenced):
        _blank(chars, match.start(), match.end(), keep="`\n")
    return "".join(chars)


def rewrite_links(text: str, pages: set[str], source: str, problems: list[str]) -> str:
    """Rewrite wiki page links to mdBook `.md` links; leave everything else alone."""

    def replace(match: re.Match) -> str:
        label, target = match.group(1), match.group(2)
        if SCHEME.match(target) or target.startswith(("<", "/", "#")):
            return match.group(0)
        page, sep, anchor = target.partition("#")
        name = unquote(page.partition("?")[0])
        if name in pages:
            return f"[{label}]({book_file(name)}{sep}{anchor})"
        if "/" in name or FILE_EXTENSION.search(name):
            return match.group(0)
        problems.append(f"{source}: link to missing page {target!r}")
        return match.group(0)

    masked = mask_code(text)
    out, last = [], 0
    for match in LINK.finditer(masked):
        original = LINK.fullmatch(text, match.start(), match.end())
        if original is None or "\0" in match.group(2):
            continue
        out += [text[last : match.start()], replace(original)]
        last = match.end()
    out.append(text[last:])
    return "".join(out)


def plain_text(markdown: str) -> str:
    """Reduce inline Markdown to plain text."""
    text = HTML_COMMENT.sub("", markdown)
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", "", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"<(https?://[^>]+)>", r"\1", text)
    text = re.sub(r"<[^>]+>", "", text)
    text = re.sub(r"(\*\*|__|\*|`)", "", text)
    text = re.sub(r"(?<!\w)_(.+?)_(?!\w)", r"\1", text)
    text = text.replace("\\", "")
    return " ".join(text.split())


def page_title(markdown: str, page: str) -> str:
    masked = mask_code(HTML_COMMENT.sub("", markdown))
    original = HTML_COMMENT.sub("", markdown)
    offset = 0
    for line in masked.splitlines(keepends=True):
        if H1.match(line):
            return plain_text(H1.match(original[offset : offset + len(line)].rstrip("\n")).group(1))
        offset += len(line)
    return page.replace("-", " ")


def first_paragraph(markdown: str) -> str:
    """The first prose paragraph: not a heading, list, quote, table, code or HTML."""
    text = HTML_COMMENT.sub("", markdown)
    lines: list[str] = []
    in_fence = None
    for line in text.splitlines():
        fence = FENCE_LINE.match(line)
        if in_fence:
            if closes_fence(line, in_fence):
                in_fence = None
            continue
        if fence:
            if lines:
                break
            in_fence = fence.group(1)
            continue
        stripped = line.strip()
        if not stripped:
            if lines:
                break
            continue
        if lines:
            lines.append(stripped)
            continue
        if (
            stripped.startswith(("#", ">", "|", "<", "![", "---", "***", "==="))
            or re.match(r"([-*+]|\d+[.)])\s", stripped)
            or line.startswith(("    ", "\t"))
        ):
            continue
        lines.append(stripped)
    return plain_text(" ".join(lines))


def describe(markdown: str, limit: int = DESCRIPTION_LIMIT) -> str:
    """A meta description: the first prose paragraph, cut at a word near `limit`."""
    text = first_paragraph(markdown)
    if len(text) <= limit:
        return text
    cut = text[: limit - 1].rsplit(" ", 1)[0].rstrip(" ,;:.-")
    return cut + "…"


SIDEBAR_SECTION = re.compile(r"^\*\*([^*\[\]]+)\*\*$")
SIDEBAR_HOME = re.compile(r"^\*\*\[([^\]]+)\]\(([^)\s]+)\)\*\*$")
SIDEBAR_ITEM = re.compile(r"^[-*]\s+\[([^\]]+)\]\(([^)\s]+)\)$")


def summary_from_sidebar(sidebar: str, pages: set[str]) -> tuple[str, list[str], list[str]]:
    """Turn _Sidebar.md into SUMMARY.md. Returns (summary, page order, problems)."""
    out = ["# Summary", ""]
    order: list[str] = []
    problems: list[str] = []
    in_part = False

    def page_of(target: str, line: str) -> str | None:
        page = unquote(target.partition("#")[0])
        if page not in pages:
            problems.append(f"_Sidebar.md: link to missing page in {line!r}")
            return None
        if page in order:
            problems.append(f"_Sidebar.md: {page} is listed twice")
            return None
        order.append(page)
        return page

    for raw in sidebar.splitlines():
        line = raw.strip()
        if not line:
            continue
        if match := SIDEBAR_HOME.match(line):
            if in_part:
                problems.append(f"_Sidebar.md: bold page link after a section title: {line!r}")
            if page := page_of(match.group(2), line):
                out += [f"[{match.group(1)}]({book_file(page)})", ""]
        elif match := SIDEBAR_SECTION.match(line):
            in_part = True
            out += [f"# {match.group(1).strip()}", ""]
        elif match := SIDEBAR_ITEM.match(line):
            if page := page_of(match.group(2), line):
                prefix = "- " if in_part else ""
                out.append(f"{prefix}[{match.group(1)}]({book_file(page)})")
                if not in_part:
                    out.append("")
        else:
            problems.append(f"_Sidebar.md: unrecognised line {line!r}")
    missing = sorted(pages - set(order))
    for page in missing:
        problems.append(f"_Sidebar.md: {page} is not listed, so the site would not render it")
    return "\n".join(out).rstrip() + "\n", order, problems


# --- Generated files ---------------------------------------------------------


def toml_string(value: str) -> str:
    """A TOML basic string: UTF-8 as is, escaping only quotes, backslashes and controls."""
    out = []
    for char in value:
        if char in '"\\':
            out.append("\\" + char)
        elif ord(char) < 0x20 or ord(char) == 0x7F:
            out.append(f"\\u{ord(char):04X}")
        else:
            out.append(char)
    return '"' + "".join(out) + '"'


def book_toml(description: str, site_path: str) -> str:
    return f"""[book]
title = {toml_string(SITE_NAME)}
description = {toml_string(description)}
language = "en"
src = "src"

[build]
create-missing = false

[output.html]
site-url = {toml_string(site_path)}
git-repository-url = {toml_string(REPOSITORY_URL)}
no-section-label = true
smart-punctuation = false

[output.html.print]
enable = false
"""


def png_size(path: Path) -> tuple[int, int]:
    with path.open("rb") as handle:
        header = handle.read(24)
    if header[:8] != b"\x89PNG\r\n\x1a\n":
        raise BuildError(f"{path} is not a PNG")
    return struct.unpack(">II", header[16:24])


@dataclass
class PageMeta:
    page: str
    title: str
    description: str
    url: str


def head_tags(meta: PageMeta, image_url: str, image_size: tuple[int, int]) -> str:
    e = lambda value: html.escape(value, quote=True)  # noqa: E731
    tags = [
        f'<link rel="canonical" href="{e(meta.url)}">',
        '<meta property="og:type" content="website">',
        f'<meta property="og:site_name" content="{e(SITE_NAME)}">',
        f'<meta property="og:title" content="{e(meta.title)}">',
        f'<meta property="og:description" content="{e(meta.description)}">',
        f'<meta property="og:url" content="{e(meta.url)}">',
        f'<meta property="og:image" content="{e(image_url)}">',
        f'<meta property="og:image:width" content="{image_size[0]}">',
        f'<meta property="og:image:height" content="{image_size[1]}">',
        '<meta name="twitter:card" content="summary_large_image">',
    ]
    return "\n        ".join(tags)


TITLE_TAG = re.compile(r"<title>[^<]*</title>")
DESCRIPTION_TAG = re.compile(r'<meta name="description" content="[^"]*">')


def apply_meta(document: str, meta: PageMeta, image_url: str, image_size) -> str:
    """Set a rendered page's <title> and description and add canonical/OG tags."""
    full_title = meta.title if meta.page == "Home" else f"{meta.title} - {SITE_NAME}"
    for pattern in (TITLE_TAG, DESCRIPTION_TAG):
        if len(pattern.findall(document)) != 1:
            raise BuildError(f"{html_file(meta.page)}: expected one {pattern.pattern!r} in mdBook output")
    title = f"<title>{html.escape(full_title, quote=False)}</title>"
    description = f'<meta name="description" content="{html.escape(meta.description, quote=True)}">'
    document = TITLE_TAG.sub(lambda _: title, document)
    return DESCRIPTION_TAG.sub(
        lambda _: description + "\n        " + head_tags(meta, image_url, image_size), document
    )


def sitemap(urls: list[str]) -> str:
    entries = "".join(f"  <url><loc>{html.escape(url)}</loc></url>\n" for url in urls)
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n'
        f"{entries}</urlset>\n"
    )


# --- Output check ------------------------------------------------------------


class _Refs(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.refs: list[str] = []
        self.ids: set[str] = set()

    def handle_starttag(self, tag, attrs):
        for name, value in attrs:
            if name == "id" and value:
                self.ids.add(value)
            if tag == "a" and name == "name" and value:
                self.ids.add(value)
            if value and (
                (name == "href" and tag in ("a", "link"))
                or (name == "src" and tag in ("img", "script", "iframe", "source"))
            ):
                self.refs.append(value)


def check_output(out: Path) -> list[str]:
    """Every relative href/src in the site must exist; fragments must match an id."""
    parsed: dict[Path, _Refs] = {}

    def parse(path: Path) -> _Refs:
        if path not in parsed:
            refs = _Refs()
            refs.feed(path.read_text(encoding="utf-8"))
            parsed[path] = refs
        return parsed[path]

    problems = []
    root = out.resolve()
    for page in sorted(root.rglob("*.html")):
        for ref in parse(page).refs:
            parts = urlsplit(ref)
            if parts.scheme or parts.netloc or ref.startswith(("/", "mailto:", "javascript:")):
                continue
            target = page if not parts.path else (page.parent / unquote(parts.path)).resolve()
            if target.is_dir():
                target = target / "index.html"
            rel = page.relative_to(root)
            if not target.is_relative_to(root) or not target.is_file():
                problems.append(f"{rel}: broken link {ref!r}")
            elif parts.fragment and target.suffix == ".html":
                if unquote(parts.fragment) not in parse(target).ids:
                    problems.append(f"{rel}: missing anchor {ref!r}")
    return problems


# --- Build -------------------------------------------------------------------


def build(wiki: Path, out: Path, mdbook: str, site_url: str, social: Path, strict: bool) -> int:
    site_url = site_url.rstrip("/") + "/"
    problems: list[str] = []

    def report(found: list[str]) -> None:
        for problem in found:
            print(f"{'error' if strict else 'warning'}: {problem}", file=sys.stderr)
        problems.extend(found)

    checker = subprocess.run(
        [sys.executable, str(REPO / "tools" / "wiki" / "check_links.py"), str(wiki)]
    )
    if checker.returncode != 0:
        report(["tools/wiki/check_links.py found broken wiki links"])

    sources = {p.stem: p.read_text(encoding="utf-8") for p in sorted(wiki.glob("*.md"))}
    pages = {name for name in sources if not name.startswith("_")}
    if "Home" not in pages:
        raise BuildError(f"{wiki} has no Home.md")
    footer = sources.get("_Footer", "").strip()

    summary, order, found = summary_from_sidebar(sources.get("_Sidebar", ""), pages)
    report(found)

    # Only pages listed in the sidebar are rendered; strict mode already rejects the rest.
    home_description = describe(sources["Home"])
    metas = {
        page: PageMeta(
            page=page,
            title=page_title(sources[page], page),
            description=describe(sources[page]),
            url=site_url if page == "Home" else site_url + html_file(page),
        )
        for page in order
    }
    for meta in metas.values():
        if not meta.description:
            report([f"{meta.page}.md: no prose paragraph for a meta description"])
            meta.description = home_description

    with tempfile.TemporaryDirectory(prefix="outpace-site-") as tmp:
        book = Path(tmp)
        src = book / "src"
        src.mkdir()
        for entry in wiki.iterdir():
            if entry.is_dir():
                shutil.copytree(entry, src / entry.name)
            elif entry.is_file() and entry.suffix != ".md":
                shutil.copyfile(entry, src / entry.name)
        shutil.copyfile(social, src / SOCIAL_IMAGE)
        for page in pages:
            found = []
            text = rewrite_links(sources[page], pages, f"{page}.md", found)
            report(found)
            if footer:
                text = text.rstrip() + "\n\n---\n\n" + rewrite_links(footer, pages, "_Footer.md", []) + "\n"
            (src / book_file(page)).write_text(text, encoding="utf-8")
        (src / "SUMMARY.md").write_text(summary, encoding="utf-8")
        (book / "book.toml").write_text(
            book_toml(home_description, urlsplit(site_url).path or "/"), encoding="utf-8"
        )
        if strict and problems:
            print(f"site build failed: {len(problems)} problem(s)", file=sys.stderr)
            return 1

        rendered = book / "book"
        subprocess.run([mdbook, "build", "--dest-dir", str(rendered), str(book)], check=True)

        image_url = site_url + SOCIAL_IMAGE
        image_size = png_size(social)
        for page, meta in metas.items():
            path = rendered / html_file(page)
            path.write_text(
                apply_meta(path.read_text(encoding="utf-8"), meta, image_url, image_size),
                encoding="utf-8",
            )
        urls = [metas[page].url for page in order]
        (rendered / "sitemap.xml").write_text(sitemap(urls), encoding="utf-8")
        (rendered / ".nojekyll").write_text("", encoding="utf-8")

        report(check_output(rendered))

        if out.exists():
            if any(out.iterdir()) and not (out / ".nojekyll").is_file():
                raise BuildError(f"{out} exists and is not a previous site build; not replacing it")
            shutil.rmtree(out)
        shutil.copytree(rendered, out)

    size = sum(p.stat().st_size for p in out.rglob("*") if p.is_file())
    print(f"built {len(pages)} pages into {out} ({size / 1024:.0f} KiB)")
    if problems:
        print(f"{len(problems)} problem(s)", file=sys.stderr)
        return 1 if strict else 0
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--wiki", type=Path, default=REPO / "docs" / "wiki")
    parser.add_argument("--out", type=Path, default=Path("_site"))
    parser.add_argument("--mdbook", default=os.environ.get("MDBOOK", "mdbook"))
    parser.add_argument("--site-url", default=SITE_URL)
    parser.add_argument("--social-image", type=Path, default=REPO / "docs" / "assets" / SOCIAL_IMAGE)
    parser.add_argument("--strict", action="store_true", help="fail on any broken link or page")
    args = parser.parse_args(argv)
    try:
        return build(args.wiki, args.out, args.mdbook, args.site_url, args.social_image, args.strict)
    except BuildError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
