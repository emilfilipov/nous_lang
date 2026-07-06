#!/usr/bin/env python3
"""Build and verify a portable Lullaby package with only the standard library."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
DIST_DIR = REPO_ROOT / "dist"
DOCS_GENERATOR = REPO_ROOT / "offline_docs" / "generate_offline_docs.py"
DOCS_VERIFIER = REPO_ROOT / "offline_docs" / "verify_offline_docs.py"


def run(command: list[str | Path], *, cwd: Path = REPO_ROOT, expect_success: bool = True) -> subprocess.CompletedProcess[str]:
    printable = " ".join(str(part) for part in command)
    print(f"running: {printable}")
    result = subprocess.run(
        [str(part) for part in command],
        cwd=cwd,
        text=True,
        capture_output=True,
    )
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    if expect_success and result.returncode != 0:
        raise RuntimeError(f"command failed with exit code {result.returncode}: {printable}")
    return result


def host_tag() -> str:
    system = platform.system().lower()
    machine = platform.machine().lower()

    if system == "windows":
        os_name = "windows"
    elif system == "darwin":
        os_name = "macos"
    elif system == "linux":
        os_name = "linux"
    else:
        os_name = system or "unknown"

    if machine in {"amd64", "x86_64"}:
        arch = "x64"
    elif machine in {"arm64", "aarch64"}:
        arch = "arm64"
    else:
        arch = machine or "unknown"

    return f"{os_name}-{arch}"


def binary_name(target_tag: str) -> str:
    return "lullaby.exe" if "windows" in target_tag else "lullaby"


def release_binary_path(target: str | None, target_tag: str) -> Path:
    target_root = REPO_ROOT / "target"
    if target:
        target_root = target_root / target
    return target_root / "release" / binary_name(target_tag)


def default_archive_extension(target_tag: str) -> str:
    return ".zip" if "windows" in target_tag else ".tar.gz"


def ensure_inside(parent: Path, child: Path) -> None:
    parent_resolved = parent.resolve()
    child_resolved = child.resolve()
    if child_resolved != parent_resolved and parent_resolved not in child_resolved.parents:
        raise RuntimeError(f"refusing to operate outside {parent_resolved}: {child_resolved}")


def copy_tree(source: Path, destination: Path) -> None:
    if destination.exists():
        shutil.rmtree(destination)
    shutil.copytree(source, destination)


def find_license() -> tuple[str, Path | None]:
    for name in ("LICENSE", "LICENSE.txt", "LICENSE.md", "COPYING", "COPYING.txt"):
        path = REPO_ROOT / name
        if path.exists():
            return f"License file: {name}", path
    return "No repository license file was present when this package was created.", None


def git_commit() -> str:
    result = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        cwd=REPO_ROOT,
        text=True,
        capture_output=True,
    )
    if result.returncode != 0:
        return "unknown"
    return result.stdout.strip() or "unknown"


def write_text(path: Path, contents: str) -> None:
    path.write_text(contents, encoding="utf-8", newline="\n")


def package_readme(package_name: str, target_tag: str, commit: str, license_status: str) -> str:
    exe = "lullaby.exe" if "windows" in target_tag else "lullaby"
    prefix = ".\\bin\\" if "windows" in target_tag else "./bin/"
    example = ".\\examples\\valid\\calculator.lby" if "windows" in target_tag else "./examples/valid/calculator.lby"
    artifact = ".\\examples\\valid\\calculator.lbc" if "windows" in target_tag else "./examples/valid/calculator.lbc"

    path_helpers = "install.cmd / install.ps1 and uninstall.cmd / uninstall.ps1"
    if "windows" in target_tag:
        optional_path = """
Optional PATH setup:
- Run install.cmd from this directory to add bin\\lullaby.exe to your user PATH.
- Open a new shell, then run: lullaby --version
- Run uninstall.cmd from this directory to remove this package from your user PATH.
"""
    else:
        path_helpers = "install.sh and uninstall.sh"
        optional_path = """
Optional PATH setup:
- Run ./install.sh from this directory to add bin/lullaby to your user PATH.
- Open a new shell, then run: lullaby --version
- Run ./uninstall.sh from this directory to remove this package from your user PATH.
"""

    return f"""Lullaby Alpha 1 portable package
Package: {package_name}
Target: {target_tag}
Commit: {commit}
{license_status}

Layout:
- bin/{exe}: command-line tool
- docs/index.html: generated offline documentation
- examples/: executable and invalid diagnostic .lby examples
- RELEASE_NOTES.md: release notes, verification evidence, and known limitations
- MANIFEST.json: package metadata
- {path_helpers}: optional user PATH setup and cleanup

Quick start:
1. Open a shell in this directory.
2. Run: {prefix}{exe} --version
3. Run: {prefix}{exe} docs
4. Run: {prefix}{exe} examples
5. Run: {prefix}{exe} check {example}
6. Run: {prefix}{exe} run {example}
7. Run: {prefix}{exe} compile --optimize alpha -o {artifact} {example}
8. Run: {prefix}{exe} inspect {artifact}
9. Run: {prefix}{exe} run {artifact}
{optional_path}
Checksum:
- The package process writes an archive checksum file beside the archive.
- Compare it with a local SHA-256 tool before unpacking downloaded archives.
"""


def make_archive(package_root: Path, archive_path: Path) -> None:
    if archive_path.suffix == ".zip":
        with zipfile.ZipFile(archive_path, "w", zipfile.ZIP_DEFLATED) as archive:
            for path in sorted(package_root.rglob("*")):
                archive.write(path, path.relative_to(package_root.parent))
        return

    if archive_path.name.endswith(".tar.gz"):
        with tarfile.open(archive_path, "w:gz") as archive:
            archive.add(package_root, arcname=package_root.name)
        return

    raise RuntimeError(f"unsupported archive type: {archive_path}")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_package(args: argparse.Namespace) -> tuple[Path, Path, Path]:
    target_tag = args.target_tag or host_tag()
    package_name = args.package_name or f"lullaby-alpha1-{target_tag}"
    archive_extension = args.archive_extension or default_archive_extension(target_tag)
    dist_dir = args.dist_dir.resolve()
    package_root = dist_dir / package_name
    archive_path = dist_dir / f"{package_name}{archive_extension}"
    checksum_path = Path(f"{archive_path}.sha256")

    ensure_inside(dist_dir, package_root)
    ensure_inside(dist_dir, archive_path)
    ensure_inside(dist_dir, checksum_path)

    if not args.skip_build:
        command = ["cargo", "build", "--release", "-p", "lullaby_cli"]
        if args.target:
            command.extend(["--target", args.target])
        run(command)

    binary = release_binary_path(args.target, target_tag)
    if not binary.exists():
        raise FileNotFoundError(f"release binary not found: {binary}")

    if package_root.exists():
        shutil.rmtree(package_root)
    package_root.mkdir(parents=True)
    (package_root / "bin").mkdir()
    (package_root / "docs").mkdir()
    (package_root / "examples").mkdir()

    packaged_binary = package_root / "bin" / binary_name(target_tag)
    shutil.copy2(binary, packaged_binary)
    if "windows" not in target_tag:
        packaged_binary.chmod(packaged_binary.stat().st_mode | 0o755)

    generated_docs = package_root / "docs" / "index.html"
    run([sys.executable, DOCS_GENERATOR, generated_docs])
    run([sys.executable, DOCS_VERIFIER, generated_docs, "--profile", "generated"])

    shutil.copy2(REPO_ROOT / "examples" / "README.md", package_root / "examples" / "README.md")
    copy_tree(REPO_ROOT / "examples" / "valid", package_root / "examples" / "valid")
    copy_tree(REPO_ROOT / "examples" / "invalid", package_root / "examples" / "invalid")
    shutil.copy2(REPO_ROOT / "documents" / "alpha1_release_notes.md", package_root / "RELEASE_NOTES.md")

    license_status, license_path = find_license()
    if license_path:
        shutil.copy2(license_path, package_root / license_path.name)

    if "windows" in target_tag:
        shutil.copy2(REPO_ROOT / "scripts" / "install_windows_path.ps1", package_root / "install.ps1")
        shutil.copy2(REPO_ROOT / "scripts" / "uninstall_windows_path.ps1", package_root / "uninstall.ps1")
        shutil.copy2(REPO_ROOT / "scripts" / "install.cmd", package_root / "install.cmd")
        shutil.copy2(REPO_ROOT / "scripts" / "uninstall.cmd", package_root / "uninstall.cmd")
        path_helpers = ["install.cmd", "install.ps1", "uninstall.cmd", "uninstall.ps1"]
    else:
        install_sh = package_root / "install.sh"
        uninstall_sh = package_root / "uninstall.sh"
        shutil.copy2(REPO_ROOT / "scripts" / "install_unix_path.sh", install_sh)
        shutil.copy2(REPO_ROOT / "scripts" / "uninstall_unix_path.sh", uninstall_sh)
        install_sh.chmod(0o755)
        uninstall_sh.chmod(0o755)
        path_helpers = ["install.sh", "uninstall.sh"]

    commit = git_commit()
    write_text(package_root / "README.txt", package_readme(package_name, target_tag, commit, license_status))

    manifest = {
        "package": package_name,
        "target": target_tag,
        "commit": commit,
        "binary": f"bin/{binary_name(target_tag)}",
        "docs": "docs/index.html",
        "docs_source": "generated",
        "examples": "examples",
        "release_notes": "RELEASE_NOTES.md",
        "archive": archive_path.name,
        "path_helpers": path_helpers,
        "license_status": license_status,
    }
    write_text(package_root / "MANIFEST.json", json.dumps(manifest, indent=2, sort_keys=True) + "\n")

    if archive_path.exists():
        archive_path.unlink()
    if checksum_path.exists():
        checksum_path.unlink()
    make_archive(package_root, archive_path)
    checksum = sha256_file(archive_path)
    write_text(checksum_path, f"{checksum}  {archive_path.name}\n")

    print(f"package: {package_root}")
    print(f"archive: {archive_path}")
    print(f"sha256: {checksum_path}")
    return package_root, archive_path, checksum_path


def verify_package(package_root: Path, archive_path: Path, checksum_path: Path, target_tag: str) -> None:
    executable = package_root / "bin" / binary_name(target_tag)
    docs = package_root / "docs" / "index.html"
    example = package_root / "examples" / "valid" / "calculator.lby"
    invalid_example = package_root / "examples" / "invalid" / "type_mismatch.lby"
    artifact = package_root / "examples" / "valid" / "calculator.lbc"

    for path in (
        executable,
        docs,
        example,
        invalid_example,
        package_root / "RELEASE_NOTES.md",
        package_root / "README.txt",
        package_root / "MANIFEST.json",
        archive_path,
        checksum_path,
    ):
        if not path.exists():
            raise FileNotFoundError(f"missing packaged file: {path}")

    if "windows" in target_tag:
        helper_names = ("install.cmd", "install.ps1", "uninstall.cmd", "uninstall.ps1")
    else:
        helper_names = ("install.sh", "uninstall.sh")
    for helper_name in helper_names:
        helper_path = package_root / helper_name
        if not helper_path.exists():
            raise FileNotFoundError(f"missing packaged PATH helper: {helper_path}")

    manifest = json.loads((package_root / "MANIFEST.json").read_text(encoding="utf-8"))
    if manifest.get("binary") != f"bin/{binary_name(target_tag)}":
        raise RuntimeError("package manifest binary path does not match the target")
    if manifest.get("path_helpers") != list(helper_names):
        raise RuntimeError("package manifest PATH helpers do not match the packaged helper files")

    expected_checksum = f"{sha256_file(archive_path)}  {archive_path.name}"
    actual_checksum = checksum_path.read_text(encoding="utf-8").strip()
    if actual_checksum != expected_checksum:
        raise RuntimeError(f"checksum mismatch in {checksum_path}")

    if target_tag != host_tag():
        print(f"skipping executable smoke tests for non-host target {target_tag}")
        return

    run([executable, "--version"])
    run([executable, "docs"])
    run([executable, "examples"])
    run([executable, "check", example])
    run([executable, "run", example])
    invalid_result = run([executable, "check", invalid_example], expect_success=False)
    if invalid_result.returncode == 0:
        raise RuntimeError(f"invalid example unexpectedly passed check: {invalid_example}")
    if artifact.exists():
        artifact.unlink()
    run([executable, "compile", "--optimize", "alpha", "-o", artifact, example])
    run([executable, "inspect", artifact])
    run([executable, "run", artifact])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package-name", help="package directory/archive base name")
    parser.add_argument("--target", help="optional Cargo target triple")
    parser.add_argument("--target-tag", help="portable package tag, default: detected host tag")
    parser.add_argument("--archive-extension", choices=(".zip", ".tar.gz"), help="override archive extension")
    parser.add_argument("--dist-dir", type=Path, default=DIST_DIR, help=f"dist directory, default: {DIST_DIR}")
    parser.add_argument("--skip-build", action="store_true", help="reuse an existing release binary")
    parser.add_argument("--verify", action="store_true", help="verify layout, checksum, and host executable smoke tests")
    args = parser.parse_args()

    try:
        package_root, archive_path, checksum_path = build_package(args)
        if args.verify:
            verify_package(package_root, archive_path, checksum_path, args.target_tag or host_tag())
    except Exception as error:
        print(f"portable package failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
