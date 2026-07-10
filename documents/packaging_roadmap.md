# Lullaby Packaging & Distribution Roadmap

How users get Lullaby. This tracks every distribution channel: what ships today,
what is planned, the machinery each needs, and the real-world prerequisites that are
the owner's to provide (a domain, signing certificates, registry accounts). Nothing
here is stubbed — a channel is either shipped, or its build machinery plus the exact
"flip the switch" step is written down.

The install layout every channel targets is the portable layout the CLI resolves at
runtime: `…/bin/lullaby[.exe]`, `…/docs/index.html`, `…/examples/…`, with `bin` on
the PATH. See `scripts/package_portable.py` for the canonical staging.

## Shipped

| Channel | Artifact | Build |
| --- | --- | --- |
| **Windows installer** | `dist/lullaby-<ver>-x64.msi` | `python scripts/build_windows_installer.py` (WiX v7; `installer/lullaby.wxs`). Installs to `Program Files\Lullaby`, adds `bin` to the system PATH, creates Start Menu shortcuts, brands the wizard + Add/Remove Programs entry, and supports clean major upgrades. |
| **Portable archives** | `.zip` (Windows) / `.tar.gz` (Unix) | `python scripts/package_portable.py` — staged `bin/`+`docs/`+`examples/` with a PATH install/uninstall helper and a checksum. |

## Planned channels

Priority order below. Each is small once its prerequisite is met; the blocker is
almost always a public release to point at, or an account to publish under.

### 1. Release automation (unblocks everything downstream)
- **Build:** a GitHub Actions workflow (`.github/workflows/release.yml`) that, on a
  version tag, builds the toolchain per OS/arch (Windows x64, Linux x64/arm64, macOS
  x64/arm64), runs `package_portable.py`, builds the Windows `.msi`, and attaches all
  artifacts + `.sha256`s to a GitHub Release. `scripts/publish_github_release.ps1`
  already drafts releases locally.
- **Prerequisite (owner):** none technical — just enabling Actions on the repo. Every
  channel below consumes the public release assets this produces.
- **Cross-build note:** Linux/macOS toolchain binaries are Rust builds for those
  targets; produced by CI runners (or cross-compilation), not on the Windows dev box.

### 2. One-line web installer
- **Build:** `install.sh` (POSIX) and `install.ps1` (Windows) that detect OS/arch,
  download the matching portable archive from the latest GitHub Release, verify its
  `.sha256`, unpack to a per-user prefix, and add `bin` to PATH. The existing
  `scripts/install_unix_path.sh` / `install_windows_path.ps1` cover the PATH step.
- **Prerequisite (owner):** a **domain** to host the short URL
  (`curl -fsSL lullaby.dev/install | sh`) — a redirect/static host pointing at the raw
  scripts is enough. Until then the scripts install from a full GitHub raw URL.

### 3. winget (Windows)
- **Build:** a manifest set (`Lullaby.Lullaby.yaml`) referencing the released `.msi`
  and its SHA-256; validated with `winget validate` / `wingetcreate`.
- **Prerequisite (owner):** a public GitHub Release with the `.msi`, then a PR to
  `microsoft/winget-pkgs`. Silent-install args are already satisfied (the MSI installs
  per-machine with the standard UI; `msiexec /qn` works).

### 4. Homebrew (macOS + Linux)
- **Build:** a formula `lullaby.rb` (bottle-less to start) that downloads the release
  tarball, installs `bin/lullaby`, and ships `docs/`+`examples/` under the prefix.
- **Prerequisite (owner):** a tap repository (`emilfilipov/homebrew-lullaby`) or a
  homebrew-core PR, pointing at public release tarballs.

### 5. Linux packages (`.deb` / `.rpm`)
- **Build:** `fpm` (or `dpkg-deb` / `rpmbuild`) recipes that place `lullaby` in
  `/usr/bin`, docs in `/usr/share/doc/lullaby`, and examples in
  `/usr/share/lullaby/examples`. Driven by CI from the staged layout.
- **Prerequisite (owner):** optional — an APT/YUM repo (or Cloudsmith/OBS) to host
  them for `apt install`; otherwise they are downloadable `.deb`/`.rpm` files on the
  Release. GPG repo signing needs a key.

### 6. macOS tarball
- **Build:** the portable `.tar.gz` (already produced by `package_portable.py`) for
  x64 and arm64, plus a Homebrew formula (#4).
- **Prerequisite (owner):** none for the tarball (users clear the Gatekeeper
  quarantine). A signed/notarized `.pkg`/`.dmg` needs a paid **Apple Developer
  account** and a Mac to sign on — a gated follow-up, not a 1.0 blocker
  (see `documents/roadmap_1_0.md`).

## Code signing (applies to Windows + macOS)
- **Windows:** an Authenticode certificate to sign `lullaby.exe` and the `.msi`.
  Unsigned installers work but trip SmartScreen/"unknown publisher". `signtool`
  slots into the release workflow once a cert (or an EV/cloud-signing service) is
  available. **Prerequisite (owner):** the certificate.
- **macOS:** Developer ID signing + notarization (see #6). **Prerequisite (owner):**
  Apple Developer account.

## Related first-run UX (CLI, not packaging)
- `lullaby new <name>` project scaffolding and a friendly first-run/version banner are
  planned CLI work (the branded "get started" story previewed in the identity board).
  These live in `crates/lullaby_cli` and are independent of the channels above.

## Owner checklist to light up the remaining channels
1. Enable GitHub Actions and cut the first tagged release (→ #1, unblocks all).
2. Register a domain for the web installer (→ #2).
3. Decide a license and add a `LICENSE` file (used by the installer notice, Homebrew,
   and the Linux packages' metadata).
4. Acquire a Windows code-signing certificate (→ signing).
5. (Optional, macOS-signed) enroll in the Apple Developer Program.
