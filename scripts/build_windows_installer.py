#!/usr/bin/env python3
"""Build the branded Lullaby Windows installer (.msi) with WiX v7.

Reproducibly stages the toolchain payload and packages it:

  1. cargo build --release (the `lullaby` binary)
  2. regenerate + verify the offline docs bundle (embedded, branded)
  3. regenerate the installer wizard bitmaps
  4. wix build installer/lullaby.wxs -> dist/lullaby-<version>-x64.msi

The install layout matches what the CLI expects at runtime
(`Program Files\\Lullaby\\{bin,docs,examples}`): the installer adds `bin` to the
system PATH and creates Start Menu shortcuts.

Prerequisites: a Rust toolchain, Python with Pillow (for the bitmaps), and the
WiX v7 CLI with the UI extension (`wix extension add -g WixToolset.UI.wixext`).

Usage:
    python scripts/build_windows_installer.py [--skip-build] [--skip-docs]
"""
from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
CLI_MANIFEST = REPO_ROOT / "crates" / "lullaby_cli" / "Cargo.toml"
WXS = REPO_ROOT / "installer" / "lullaby.wxs"
DOCS_OUTPUT = REPO_ROOT / "target" / "offline_docs" / "index.html"
DIST = REPO_ROOT / "dist"


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    print("+ " + " ".join(str(c) for c in cmd))
    return subprocess.run(cmd, cwd=REPO_ROOT, check=True, **kw)


def crate_version() -> str:
    text = CLI_MANIFEST.read_text(encoding="utf-8")
    match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.MULTILINE)
    if not match:
        sys.exit("could not read version from crates/lullaby_cli/Cargo.toml")
    # MSI ProductVersion must be numeric x.y.z — drop any pre-release suffix.
    return match.group(1).split("-", 1)[0]


def find_wix() -> str:
    found = shutil.which("wix")
    if found:
        return found
    for base in (os.environ.get("ProgramFiles", r"C:\Program Files"),
                 os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")):
        if not base:
            continue
        for candidate in Path(base).glob("WiX Toolset*/bin/wix.exe"):
            return str(candidate)
    sys.exit("wix CLI not found. Install WiX v7 and ensure `wix` is on PATH.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--skip-build", action="store_true", help="reuse the existing release binary")
    parser.add_argument("--skip-docs", action="store_true", help="reuse the existing generated docs")
    args = parser.parse_args()

    version = crate_version()
    wix = find_wix()
    py = sys.executable

    if not args.skip_build:
        run(["cargo", "build", "--release", "-p", "lullaby_cli"])
    exe = REPO_ROOT / "target" / "release" / "lullaby.exe"
    if not exe.exists():
        sys.exit(f"release binary not found: {exe} (drop --skip-build)")

    if not args.skip_docs:
        run([py, "offline_docs/generate_offline_docs.py"])
        run([py, "offline_docs/verify_offline_docs.py", str(DOCS_OUTPUT), "--profile", "generated"])
    if not DOCS_OUTPUT.exists():
        sys.exit(f"generated docs not found: {DOCS_OUTPUT} (drop --skip-docs)")

    # Wizard bitmaps (deterministic; committed, but refreshed here so a change to
    # the mark geometry flows through without a manual step).
    run([py, "installer/render_installer_art.py"])

    DIST.mkdir(exist_ok=True)
    out = DIST / f"lullaby-{version}-x64.msi"
    if out.exists():
        out.unlink()
    run([wix, "build", str(WXS), "-arch", "x64",
         "-ext", "WixToolset.UI.wixext", "-d", f"Version={version}", "-o", str(out)])

    if not out.exists():
        sys.exit("wix build reported success but the .msi is missing")
    print(f"\nBuilt {out.relative_to(REPO_ROOT)}  ({out.stat().st_size // 1024} KiB)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
