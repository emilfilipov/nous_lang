#!/usr/bin/env python3
"""Verify the self-contained Lullaby offline documentation entry point."""

from __future__ import annotations

import argparse
import html
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
ENTRY = ROOT / "index.html"

REQUIRED_IDS = [
    "overview",
    "quick-start",
    "source-files",
    "functions",
    "variables-assignment",
    "arrays",
    "control-flow",
    "boolean-logic",
    "memory-builtins",
    "io-system",
    "cli",
    "examples",
    "diagnostics",
    "limitations",
    "maintainers",
]

REQUIRED_PHRASES = [
    ".lby",
    "active development toward 1.0",
    "currently implemented language surface",
    "Indentation defines scope",
    "Last-expression return",
    "zero-argument main",
    "void",
    "let",
    "inferred local type",
    "assignment",
    "array&lt;T&gt;",
    "non-empty array literals",
    "bounds-checked indexing",
    "if",
    "elif",
    "else",
    "while",
    "for",
    "from",
    "to",
    "by",
    "loop",
    "break",
    "continue",
    "and",
    "or",
    "not",
    "short-circuit",
    "alloc(value)",
    "load(ptr)",
    "store(ptr, value)",
    "dealloc(ptr)",
    "read_file(path)",
    "write_file(path, content)",
    "append_file(path, content)",
    "file_exists(path)",
    "sys_status(program, args)",
    "sys_output(program, args)",
    "array&lt;string&gt;",
    "cargo run -p lullaby_cli -- check",
    "cargo run -p lullaby_cli -- compile",
    "cargo run -p lullaby_cli -- build",
    "cargo run -p lullaby_cli -- inspect",
    "cargo run -p lullaby_cli -- run",
    "cargo run -p lullaby_cli -- docs",
    "cargo run -p lullaby_cli -- examples",
    "lullaby inspect",
    "lullaby docs",
    "lullaby examples",
    "bin\\lullaby.exe",
    "docs\\index.html",
    "examples\\valid\\calculator.lby",
    "lullaby-alpha1-windows-x64.zip.sha256",
    "install.cmd",
    "uninstall.cmd",
    "package_windows_portable.ps1",
    "verify_release.ps1",
    "publish_github_release.ps1",
    "RELEASE_NOTES.md",
    ".lbc",
    "instruction-bytecode",
    "instruction contract",
    "function instructions",
    "function table",
    "memory_operations",
    "memory operation",
    "metadata",
    "--backend ir",
    "--backend bytecode",
    "--optimize constant-fold",
    "common subexpression elimination",
    "loop-invariant motion",
    "copy propagation",
    "--optimize dead-code",
    "--optimize alpha",
    "--optimize none",
    "--verbose",
    "--format json",
    "--diagnostic-format json",
    "root cause",
    "suggested fix",
    "traceback",
    "diagnostic registry",
    "documents/diagnostic_registry.md",
    "Diagnostics",
    "L0324",
    "L0326",
    "L0327",
    "L0328",
    "L0329",
    "L0211",
    "L0501",
    "L0502",
    "L0601",
    "L0413",
    "L0414 [resource]",
    "L0415 [resource]",
    "L0416 [resource]",
    "Current Limitations",
]

GENERATED_REQUIRED_IDS = [
    "project-overview",
    "core-language-rules",
    "alpha-1-language-surface",
    "diagnostics-registry",
    "release-notes",
    "post-alpha-roadmap",
    "executable-examples",
    "overview",
    "quick-start",
    "source-files",
    "functions",
    "variables-assignment",
    "arrays",
    "control-flow",
    "boolean-logic",
    "memory-builtins",
    "io-system",
    "cli",
    "examples",
    "package-layout",
    "diagnostics",
    "limitations",
    "maintainers",
]

GENERATED_REQUIRED_PHRASES = REQUIRED_PHRASES + [
    "Self-contained HTML generated from canonical repository Markdown.",
    "Source: <code>README.md</code>",
    "Source: <code>documents/core_language_rules.md</code>",
    "Source: <code>documents/alpha1_language_surface.md</code>",
    "Source: <code>documents/diagnostic_registry.md</code>",
    "Source: <code>documents/alpha1_release_notes.md</code>",
    "Source: <code>documents/post_alpha_roadmap.md</code>",
    "Executable Examples",
    "tests/fixtures/valid/docs_quick_start.lby",
    "examples/valid/calculator.lby",
    "examples/valid/arrays_control_flow.lby",
    ".lby",
    "Alpha 1",
    "diagnostic registry",
    "Memory-Aware IR",
    "Native Code Generation Roadmap",
    "lullaby check [--verbose|--format json] file.lby",
    "lullaby compile [--optimize none|constant-fold|dead-code|alpha]",
    "lullaby inspect file.lbc",
    "lullaby run [--backend ast|ir|bytecode] file.lby",
    "lullaby docs",
    "lullaby examples",
    "docs/index.html",
    "RELEASE_NOTES.md",
    "MANIFEST.json",
    "VERSION.txt",
    "*.sha256",
    "--diagnostic-format json",
    "Current Limitations",
    "L0211",
    "active development toward 1.0",
    "native x86-64 backend",
]

FORBIDDEN_REMOTE_PATTERNS = [
    "http://",
    "https://",
    "//cdn",
    "@import",
]

PRE_BLOCK_RE = re.compile(
    r"<pre(?P<attrs>[^>]*)>\s*<code>(?P<code>.*?)</code>\s*</pre>",
    re.DOTALL,
)
ATTR_RE = re.compile(r'\s([a-zA-Z0-9_-]+)="([^"]*)"')


def fail(message: str) -> int:
    print(f"offline docs verification failed: {message}", file=sys.stderr)
    return 1


def attrs_from(raw_attrs: str) -> dict[str, str]:
    return {match.group(1): match.group(2) for match in ATTR_RE.finditer(raw_attrs)}


def normalize_snippet(text: str) -> str:
    return html.unescape(text).replace("\r\n", "\n").strip()


def verify_examples(html_text: str) -> str | None:
    fixture_modes: dict[Path, str] = {}

    for block in PRE_BLOCK_RE.finditer(html_text):
        attrs = attrs_from(block.group("attrs"))
        snippet = normalize_snippet(block.group("code"))
        looks_like_lullaby = "\n" in snippet and (
            snippet.startswith("fn ") or "\nfn " in snippet
        )

        if not looks_like_lullaby:
            continue

        if "data-planned" in attrs:
            continue

        fixture_attr = attrs.get("data-fixture")
        if not fixture_attr:
            return f"lullaby example missing data-fixture metadata: {snippet[:80]}"

        mode = attrs.get("data-mode", "check")
        if mode not in {"check", "run"}:
            return f"invalid data-mode for {fixture_attr}: {mode}"

        fixture_path = (REPO / fixture_attr).resolve()
        if REPO not in fixture_path.parents:
            return f"fixture path escapes repository: {fixture_attr}"
        if not fixture_path.is_file():
            return f"fixture file is missing: {fixture_attr}"

        fixture_text = normalize_snippet(fixture_path.read_text(encoding="utf-8"))
        if snippet != fixture_text:
            return f"example snippet does not match fixture: {fixture_attr}"

        previous_mode = fixture_modes.get(fixture_path)
        if previous_mode != "run":
            fixture_modes[fixture_path] = mode

    if not fixture_modes:
        return "no executable lullaby examples were found"

    for fixture_path, mode in sorted(fixture_modes.items()):
        result = subprocess.run(
            ["cargo", "run", "--quiet", "-p", "lullaby_cli", "--", mode, str(fixture_path)],
            cwd=REPO,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode != 0:
            detail = (result.stderr or result.stdout).strip()
            return f"fixture did not pass `lullaby {mode}`: {fixture_path} {detail}"

    return None


def verify_html(entry: Path, required_ids: list[str], required_phrases: list[str]) -> int:
    if not entry.is_file():
        return fail(f"missing entry point: {entry}")

    html_text = entry.read_text(encoding="utf-8")

    for section_id in required_ids:
        if f'id="{section_id}"' not in html_text:
            return fail(f"missing section id #{section_id}")

    for phrase in required_phrases:
        if phrase not in html_text:
            return fail(f"missing required phrase: {phrase}")

    lowered = html_text.lower()
    for pattern in FORBIDDEN_REMOTE_PATTERNS:
        if pattern in lowered:
            return fail(f"remote dependency pattern found: {pattern}")

    ids = set(re.findall(r'id="([^"]+)"', html_text))
    hrefs = re.findall(r'href="([^"]+)"', html_text)
    for href in hrefs:
        if href.startswith("#"):
            target = href[1:]
            if target not in ids:
                return fail(f"anchor link has no matching section: {href}")
            continue
        if href.startswith("data:"):
            # A self-contained embedded asset (bundled font/icon). This IS the
            # offline story; the remote-dependency scan above still rejects any
            # http(s)/CDN/@import reference.
            continue
        if re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*:", href):
            return fail(f"non-local href found: {href}")
        local_target = (entry.parent / href).resolve()
        if entry.parent not in local_target.parents and local_target != entry.parent:
            return fail(f"href escapes offline docs directory: {href}")
        if not local_target.exists():
            return fail(f"local href target is missing: {href}")

    example_error = verify_examples(html_text)
    if example_error:
        return fail(example_error)

    print(f"offline docs verification passed: {entry}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "entry",
        nargs="?",
        type=Path,
        default=ENTRY,
        help=f"HTML entry to verify, default: {ENTRY}",
    )
    parser.add_argument(
        "--profile",
        choices=("shipped", "generated"),
        default="shipped",
        help="verification profile for required sections and phrases",
    )
    args = parser.parse_args()

    entry = args.entry
    if not entry.is_absolute():
        entry = (REPO / entry).resolve()

    if args.profile == "generated":
        return verify_html(entry, GENERATED_REQUIRED_IDS, GENERATED_REQUIRED_PHRASES)
    return verify_html(entry, REQUIRED_IDS, REQUIRED_PHRASES)


if __name__ == "__main__":
    raise SystemExit(main())
