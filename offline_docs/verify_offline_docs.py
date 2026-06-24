#!/usr/bin/env python3
"""Verify the self-contained Nous Lang offline documentation entry point."""

from __future__ import annotations

import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent
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
    "cli",
    "examples",
    "diagnostics",
    "limitations",
    "maintainers",
]

REQUIRED_PHRASES = [
    ".nl",
    "Indentation defines scope",
    "Last-expression return",
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
    "dealloc(ptr)",
    "cargo run -p nous_cli -- check",
    "cargo run -p nous_cli -- run",
    "Diagnostics",
    "N0324",
    "N0326",
    "N0327",
    "N0413",
    "Current Limitations",
]

FORBIDDEN_REMOTE_PATTERNS = [
    "http://",
    "https://",
    "//cdn",
    "@import",
]


def fail(message: str) -> int:
    print(f"offline docs verification failed: {message}", file=sys.stderr)
    return 1


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

    print(f"offline docs verification passed: {ENTRY}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
