#!/bin/sh
# Lullaby one-line web installer (Linux/macOS).
#
#   curl -fsSL https://lullaby.skazkasolutions.com/install.sh | sh
#
# Downloads the correct portable package for this OS/arch from the latest
# GitHub Release, verifies its published SHA-256, installs it under a per-user
# prefix (no root), and wires `bin` onto PATH by delegating to the package's own
# `install.sh` helper. Re-running upgrades in place. Uninstall with:
#
#   curl -fsSL https://lullaby.skazkasolutions.com/install.sh | sh -s -- uninstall
#
# Overrides (environment variables):
#   LULLABY_VERSION   install a specific tag (e.g. v1.0.0-preview) instead of latest
#   LULLABY_PREFIX    install prefix (default: $HOME/.lullaby)
#   LULLABY_REPO      owner/repo to pull releases from (default: emilfilipov/lullaby-lang)
set -eu

REPO="${LULLABY_REPO:-emilfilipov/lullaby-lang}"
PREFIX="${LULLABY_PREFIX:-${HOME:-}/.lullaby}"

log() { printf '%s\n' "lullaby: $*"; }
die() { printf '%s\n' "lullaby: error: $*" >&2; exit 1; }

# --- uninstall mode -------------------------------------------------------
if [ "${1:-}" = "uninstall" ]; then
    if [ -x "$PREFIX/uninstall.sh" ]; then
        sh "$PREFIX/uninstall.sh" || true
    fi
    if [ -d "$PREFIX" ]; then
        rm -rf "$PREFIX"
        log "removed $PREFIX"
    else
        log "nothing to uninstall at $PREFIX"
    fi
    log "if PATH still references it, open a new shell (the profile source line is now a no-op)"
    exit 0
fi

[ -n "${HOME:-}" ] || die "HOME is not set; cannot choose an install prefix"

# --- detect OS + arch -> target tag --------------------------------------
os=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)
case "$os" in
    Linux) os_tag=linux ;;
    Darwin) os_tag=macos ;;
    *) die "unsupported OS '$os' (this installer covers Linux and macOS; on Windows use install.ps1)" ;;
esac
case "$arch" in
    x86_64 | amd64) arch_tag=x64 ;;
    arm64 | aarch64) arch_tag=arm64 ;;
    *) die "unsupported architecture '$arch'" ;;
esac
target_tag="${os_tag}-${arch_tag}"

# --- pick a downloader ----------------------------------------------------
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1"; }
    download() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO- "$1"; }
    download() { wget -qO "$2" "$1"; }
else
    die "need curl or wget to download the package"
fi

# --- checksum tool --------------------------------------------------------
if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    die "need sha256sum or shasum to verify the download"
fi

# --- resolve the release asset -------------------------------------------
log "resolving $target_tag package from $REPO"
if [ -n "${LULLABY_VERSION:-}" ]; then
    release_json=$(fetch "https://api.github.com/repos/${REPO}/releases/tags/${LULLABY_VERSION}") \
        || die "no release tagged ${LULLABY_VERSION} in ${REPO}"
elif release_json=$(fetch "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null); then
    : # newest stable release
else
    # No stable release yet (pre-1.0: every release is a prerelease). Fall back
    # to the newest release of any kind.
    release_json=$(fetch "https://api.github.com/repos/${REPO}/releases?per_page=1") \
        || die "could not query GitHub Releases for ${REPO}"
fi

ref=$(
    printf '%s' "$release_json" \
        | grep -o '"tag_name":[[:space:]]*"[^"]*"' \
        | sed 's/.*"\([^"]*\)"$/\1/' | head -n1
)
[ -n "$ref" ] || die "could not determine the release tag"

# Match the portable archive for this target among the release's assets. The
# package-name prefix may vary between releases, so we match on the trailing
# <target_tag>.tar.gz rather than a hard-coded full name.
asset_url=$(
    printf '%s' "$release_json" \
        | grep -o '"browser_download_url":[[:space:]]*"[^"]*"' \
        | sed 's/.*"\(https[^"]*\)"$/\1/' \
        | grep -E "${target_tag}\.tar\.gz$" \
        | head -n1
)
[ -n "$asset_url" ] || die "no prebuilt package for $target_tag in release $ref (it may not be built for this platform yet)"
checksum_url="${asset_url}.sha256"

# --- download + verify ----------------------------------------------------
tmp=$(mktemp -d "${TMPDIR:-/tmp}/lullaby-install.XXXXXX") || die "could not create a temp dir"
trap 'rm -rf "$tmp"' EXIT INT TERM
archive="$tmp/package.tar.gz"

log "downloading $(basename "$asset_url")"
download "$asset_url" "$archive" || die "download failed: $asset_url"
download "$checksum_url" "$archive.sha256" || die "download failed: $checksum_url"

expected=$(cut -d' ' -f1 <"$archive.sha256")
actual=$(sha256 "$archive")
[ -n "$expected" ] || die "empty published checksum"
if [ "$expected" != "$actual" ]; then
    die "checksum mismatch (expected $expected, got $actual) - refusing to install"
fi
log "checksum verified"

# --- install into the prefix ---------------------------------------------
extract="$tmp/extract"
mkdir -p "$extract"
tar -xzf "$archive" -C "$extract" || die "could not extract the archive"

# The archive holds a single top-level package directory; install its contents.
top=$(find "$extract" -mindepth 1 -maxdepth 1 -type d | head -n1)
[ -n "$top" ] || die "unexpected archive layout (no top-level package directory)"

rm -rf "$PREFIX"
mkdir -p "$PREFIX"
# Copy contents (including dotfiles) of the package dir into the prefix.
(cd "$top" && tar -cf - .) | (cd "$PREFIX" && tar -xf -)
[ -x "$PREFIX/bin/lullaby" ] || chmod +x "$PREFIX/bin/lullaby" 2>/dev/null || true

log "installed to $PREFIX"

# --- wire PATH via the package's own helper ------------------------------
if [ -x "$PREFIX/install.sh" ]; then
    sh "$PREFIX/install.sh" || log "PATH helper reported an issue; add $PREFIX/bin to PATH manually"
else
    log "add $PREFIX/bin to your PATH manually"
fi

printf '\n'
log "done - Lullaby $ref ($target_tag)"
log "open a new shell, then run:  lullaby --version"
log "start a project with:        lullaby new my_app"
