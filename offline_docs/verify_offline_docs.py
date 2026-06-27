#!/usr/bin/env python3
"""Verify the self-contained Nous Lang offline documentation entry point."""

from __future__ import annotations

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
    ".nl",
    "Alpha 1 language surface",
    "Indentation defines scope",
    "Last-expression return",
    "zero-argument main",
    "void",
    "let",
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
    "cargo run -p nous_cli -- check",
    "cargo run -p nous_cli -- compile",
    "cargo run -p nous_cli -- inspect",
    "cargo run -p nous_cli -- run",
    "cargo run -p nous_cli -- docs",
    "cargo run -p nous_cli -- examples",
    "nlang inspect",
    "nlang docs",
    "nlang examples",
    "bin\\nlang.exe",
    "docs\\index.html",
    "examples\\valid\\calculator.nl",
    "nous-lang-alpha1-windows-x64.zip.sha256",
    "install.cmd",
    "uninstall.cmd",
    "package_windows_portable.ps1",
    "verify_release.ps1",
    "publish_github_release.ps1",
    "RELEASE_NOTES.md",
    ".nbc",
    "instruction-bytecode",
    "function instructions",
    "function table",
    "metadata",
    "--backend ir",
    "--backend bytecode",
    "--optimize constant-fold",
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
    "N0324",
    "N0326",
    "N0327",
    "N0328",
    "N0329",
    "N0211",
    "N0501",
    "N0502",
    "N0601",
    "N0413",
    "N0414 [resource]",
    "N0415 [resource]",
    "N0416 [resource]",
    "Current Limitations",
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
        looks_like_nlang = "\n" in snippet and (
            snippet.startswith("fn ") or "\nfn " in snippet
        )

        if not looks_like_nlang:
            continue

        if "data-planned" in attrs:
            continue

        fixture_attr = attrs.get("data-fixture")
        if not fixture_attr:
            return f"nlang example missing data-fixture metadata: {snippet[:80]}"

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
        return "no executable nlang examples were found"

    for fixture_path, mode in sorted(fixture_modes.items()):
        result = subprocess.run(
            ["cargo", "run", "--quiet", "-p", "nous_cli", "--", mode, str(fixture_path)],
            cwd=REPO,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode != 0:
            detail = (result.stderr or result.stdout).strip()
            return f"fixture did not pass `nlang {mode}`: {fixture_path} {detail}"

    return None


def main() -> int:
    if not ENTRY.is_file():
        return fail(f"missing entry point: {ENTRY}")

    html = ENTRY.read_text(encoding="utf-8")

    for section_id in REQUIRED_IDS:
        if f'id="{section_id}"' not in html:
            return fail(f"missing section id #{section_id}")

    for phrase in REQUIRED_PHRASES:
        if phrase not in html:
            return fail(f"missing required phrase: {phrase}")

    lowered = html.lower()
    for pattern in FORBIDDEN_REMOTE_PATTERNS:
        if pattern in lowered:
            return fail(f"remote dependency pattern found: {pattern}")

    ids = set(re.findall(r'id="([^"]+)"', html))
    hrefs = re.findall(r'href="([^"]+)"', html)
    for href in hrefs:
        if href.startswith("#"):
            target = href[1:]
            if target not in ids:
                return fail(f"anchor link has no matching section: {href}")
            continue
        if re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*:", href):
            return fail(f"non-local href found: {href}")
        local_target = (ROOT / href).resolve()
        if ROOT not in local_target.parents and local_target != ROOT:
            return fail(f"href escapes offline docs directory: {href}")
        if not local_target.exists():
            return fail(f"local href target is missing: {href}")

    example_error = verify_examples(html)
    if example_error:
        return fail(example_error)

    print(f"offline docs verification passed: {ENTRY}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
