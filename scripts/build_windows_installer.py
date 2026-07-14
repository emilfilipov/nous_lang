#!/usr/bin/env python3
"""Build the branded Lullaby Windows installer (.msi) with WiX v7.

Reproducibly stages the toolchain payload and packages it:

  1. cargo build --release (the `lullaby` binary)
  2. regenerate the installer wizard bitmaps
  3. wix build installer/lullaby.wxs -> dist/lullaby-<version>-x64.msi

The install layout matches what the CLI expects at runtime
(`Program Files\\Lullaby\\{bin,examples}`): the installer adds `bin` to the
system PATH and creates Start Menu shortcuts. User-facing documentation is the
hosted online website, not bundled with the installer.

Prerequisites: a Rust toolchain, Python with Pillow (for the bitmaps), and the
WiX v7 CLI with the UI extension (`wix extension add -g WixToolset.UI.wixext`).

Usage:
    python scripts/build_windows_installer.py [--skip-build]
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
DIST = REPO_ROOT / "dist"


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    print("+ " + " ".join(str(c) for c in cmd))
    return subprocess.run(cmd, cwd=REPO_ROOT, check=True, **kw)


def workspace_version() -> str:
    """The full semver version from the root [workspace.package] (e.g. 1.0.0-preview)."""
    text = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.MULTILINE)
    if not match:
        sys.exit("could not read [workspace.package] version from Cargo.toml")
    return match.group(1)


# Maturity rank baked into the MSI ProductVersion PATCH field. MSI compares only
# the numeric ProductVersion (MAJOR.MINOR.PATCH), and our scheme parks semver's
# patch slot at 0 for every build, so without this BOTH 1.0-preview and
# 1.0-stable would map to ProductVersion 1.0.0 and a stable MSI would NOT upgrade
# an installed preview (same version = no major upgrade). Encoding the status as
# an increasing PATCH makes each successive release of the same MAJOR.PATCH
# strictly increase: experimental < preview < stable. Values leave headroom for
# future statuses; an unknown status falls back to the stable rank.
STATUS_RANK = {"experimental": 10, "preview": 20, "stable": 30}


def numeric_version(full: str) -> str:
    """Numeric MSI ProductVersion (MAJOR.MINOR.PATCH, no suffix).

    MAJOR.MINOR come straight from the semver string; the PATCH field encodes
    release maturity (see STATUS_RANK) so that preview -> stable of the same
    number is a real, strictly-increasing upgrade rather than an identical
    ProductVersion that Windows Installer would refuse to replace.
    """
    nums, _, status = full.partition("-")
    parts = nums.split(".")
    major = parts[0] if parts else "0"
    minor = parts[1] if len(parts) > 1 else "0"
    rank = STATUS_RANK.get(status or "stable", STATUS_RANK["stable"])
    return f"{major}.{minor}.{rank}"


def display_version(full: str) -> str:
    # The MAJOR.PATCH-STATUS display/tag form (see documents/versioning.md): PATCH
    # is semver's minor, status is the prerelease (or "stable" when there is none).
    nums, _, status = full.partition("-")
    parts = nums.split(".")
    major = parts[0] if parts else "0"
    minor = parts[1] if len(parts) > 1 else "0"
    return f"{major}.{minor}-{status or 'stable'}"


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
    args = parser.parse_args()

    full_version = workspace_version()
    version = numeric_version(full_version)  # numeric MSI ProductVersion
    disp_version = display_version(full_version)  # MAJOR.PATCH-STATUS for the filename
    wix = find_wix()
    py = sys.executable

    if not args.skip_build:
        run(["cargo", "build", "--release", "-p", "lullaby_cli"])
    exe = REPO_ROOT / "target" / "release" / "lullaby.exe"
    if not exe.exists():
        sys.exit(f"release binary not found: {exe} (drop --skip-build)")

    # Wizard bitmaps (deterministic; committed, but refreshed here so a change to
    # the mark geometry flows through without a manual step).
    run([py, "installer/render_installer_art.py"])

    DIST.mkdir(exist_ok=True)
    out = DIST / f"lullaby-{disp_version}-x64.msi"
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
