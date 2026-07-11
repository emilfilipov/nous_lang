# Distribution, Branding & Installer Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
This is the implementation-grade design for Lullaby's "ease of access" half of 1.0
(Phase 8 of the repo-owned 1.0 roadmap, `roadmap_1_0`): branding, the toolchain
bundle, and every install/distribution channel across Windows, Linux, and macOS. It is the
source of truth for what the packaging work must build; granular tickets live in
the ClickUp `Lullaby` folder, lists **07 CLI Build Installer** and **17 Tooling and
Ecosystem**.

Scope note: this document designs the channels and their mechanics. It does not
change compiler/runtime behavior. It builds on the delivered portable packaging
(`scripts/package_windows_portable.ps1`, `scripts/package_portable.py`,
`scripts/verify_release.ps1`, `scripts/publish_github_release.ps1`, the PATH
helpers, and the CI template [portable_package_ci_workflow.yml](portable_package_ci_workflow.yml)),
and extends it into first-class OS-native installers and a one-line web installer.

Everything here is held to the Production Quality Standard in
[../CLAUDE.md](../CLAUDE.md): no placeholder installers, no "good enough" scripts.
Every channel ships with clean install, PATH integration, uninstall, and a
verification gate, or it is not shipped.

## 1. Branding

### 1.1 Canonical name and extension (confirmed)

- **Language name: Lullaby.** This is fixed. The `nous_lang` repository directory
  and any historical "Nous"/"Nous Lang" naming are legacy only and must never
  appear as the language's name in code, docs, diagnostics, tickets, package
  metadata, or user-facing surfaces. "Nous" was evaluated as a rename candidate
  and **rejected** — see [name_research.md](name_research.md) for the collision
  evidence (occupied PyPI `nous-lang`, `nous-lang.org`, `contrario/nous`).
- **Compiler/CLI command: `lullaby`.** One binary, one name, everywhere.
- **Canonical source extension: `.lby`.** The only accepted extension; the long
  `.lullaby` form is fully retired and the lexer rejects it. `.lby` is
  distinctive, maps cleanly to Lullaby, follows the short-extension convention
  (`.rs`, `.py`, `.rb`), and has no meaningful source-language collision.
- **Compiled artifact extension: `.lbc`** (Lullaby bytecode container), already
  produced by `lullaby compile`/`build`.

### 1.2 Package and namespace identity

One identity string per channel, chosen now to avoid churn later. Every one must
pass the clearance pass in [name_research.md](name_research.md) (§ Follow-Up
Clearance) before first public publish:

| Channel | Identifier |
| :--- | :--- |
| winget package id | `Lullaby.Lullaby` |
| Homebrew formula | `lullaby` (tap `lullaby-lang/tap` until core-eligible) |
| Debian/Ubuntu package | `lullaby` |
| Fedora/RHEL package | `lullaby` |
| crates.io (compiler crates) | `lullaby_*` (already used in the workspace) |
| VS Code extension id | `lullaby-lang.lullaby` |
| GitHub org/repo | `lullaby-lang/lullaby` (rename the legacy `nous_lang` remote) |
| Primary domain | `lullaby-lang.org` (fallbacks `lullabylang.org`, `lullaby.dev`) |
| Windows install dir | `%LOCALAPPDATA%\Programs\Lullaby` (per-user) / `%ProgramFiles%\Lullaby` (machine) |
| Unix install prefix | `/usr/local` (system) or `~/.lullaby` (per-user web installer) |

### 1.3 Logo, wordmark, tagline

- **Wordmark:** lowercase `lullaby` set in a rounded humanist sans (e.g. Inter /
  Manrope), letter-spaced slightly, to read calm and approachable — matching the
  name's meaning and the "LLM-friendly, gentle to write" positioning.
- **Icon:** a plain lowercase `l` monogram on the lavender→sky tile that doubles as
  the `.lby` file-type icon (the full `lullaby` wordmark is the identity everywhere a
  lockup fits). Ships as SVG plus the raster sizes each channel needs:
  - Windows `.ico` (16/32/48/256 px) for the MSI/EXE and the `.lby` file
    association icon;
  - Linux `.png` set (16–512 px) plus a scalable SVG for the freedesktop icon
    theme (`hicolor`), referenced by the `.desktop` and MIME entries;
  - macOS `.icns` for a future signed `.pkg`/`.dmg` (not required for the
    tarball/Homebrew path).
- **Tagline:** *"A quiet language for clear programs."* Alternates for
  constrained surfaces: *"Type less. Say more."* / *"Compiled. Concise.
  LLM-native."* The chosen tagline appears on the docs landing page, the web
  installer banner, and release notes; it must not overstate the current surface.
- **Color:** one accent (deep indigo `#3B3B7A`) plus ink/paper neutrals, reused
  by the offline docs bundle so branding is consistent between installer and docs.
- All brand assets live under a new `branding/` directory in the repo (SVG
  sources + generated raster/ico/icns) so every packager reads from one source of
  truth. Generation and layout of that directory is a list-07 implementation
  ticket, not part of this design doc's footprint.

### 1.4 The install command surface (what users see)

The headline promise is *one line per OS*. These are the exact strings we
advertise on the site, the README, and the docs:

```text
# Windows (PowerShell)
winget install Lullaby.Lullaby
#   or the one-line bootstrap:
irm https://lullaby-lang.org/install.ps1 | iex

# macOS / Linux (Homebrew)
brew install lullaby-lang/tap/lullaby
#   or the one-line bootstrap:
curl -fsSL https://lullaby-lang.org/install.sh | sh

# Any OS, no package manager: download a portable archive from GitHub Releases,
# unpack, and run the bundled install helper.
```

## 2. Toolchain bundle

### 2.1 What ships

Every channel installs the same logical bundle. The unit of distribution is the
**Lullaby toolchain**, versioned as one semver number (matching the
`lullaby_cli` crate version, currently `1.0.0-preview`).

| Component | Contents | Source today |
| :--- | :--- | :--- |
| `lullaby` CLI/compiler | the single binary: `check`/`compile`/`build`/`run`/`test`/`wasm`/`native`/`inspect`/`fmt`/`lsp`/`docs`/`examples`/`new` | `crates/lullaby_cli` release build |
| Formatter | `lullaby fmt` (canonical source formatter) — same binary, no separate tool | `crates/lullaby_parser` `format.rs` |
| Language server | `lullaby lsp` (JSON-RPC over stdio) — same binary | `crates/lullaby_lsp` |
| Offline docs | self-contained `index.html` bundle, openable with no server/CDN | `offline_docs/generate_offline_docs.py` |
| Examples | the curated `examples/valid` + `examples/invalid` tree | `examples/` |
| Standard library / prelude | compiler-provided prelude (built into the binary today); a future on-disk `std/` module tree when stdlib modules move out of core (Phase 7) | `documents/standard_library.md` |
| Native runtime support | the linker glue the `native` command drives (`rust-lld` discovery + `kernel32.lib`/`ucrt.lib` resolution); documented as a host-toolchain dependency, not re-shipped | `crates/lullaby_ir` native path |
| Metadata | `VERSION`/`MANIFEST.json`, `LICENSE`, `RELEASE_NOTES`, `README` | packaging scripts |

Notes:

- The prelude is compiled into the binary today, so "standard library" ships for
  free inside `lullaby`. When Phase 7 externalizes stdlib modules into an on-disk
  `std/` tree, that tree joins the bundle under `lib/lullaby/std/` (see layout
  below) and every packager picks it up automatically because it targets the
  canonical layout, not a hand-listed file set.
- The **native backend** needs a host linker at run time (`rust-lld` from a Rust
  toolchain, or the platform linker, plus the platform C import libraries). We do
  **not** vendor a linker into the installer. The docs' "native compile"
  section states the host prerequisite; interpreter/`run`, `wasm`, `check`,
  `test`, `fmt`, `lsp`, and `docs` all work with zero external dependencies.

### 2.2 Canonical on-disk layout

Every packager (MSI, deb, rpm, tarball, Homebrew, web installer) lays the bundle
down in this layout, relative to an install prefix `$PREFIX`. This is the
contract: packagers differ only in `$PREFIX` and in how they wire PATH and file
associations.

```text
$PREFIX/
  bin/
    lullaby(.exe)              # the one binary; fmt/lsp/new are subcommands
  lib/lullaby/
    std/                       # on-disk stdlib modules (Phase 7+; absent until then)
    runtime/                   # native runtime support files, if any
  share/lullaby/
    docs/index.html            # offline docs bundle (+ assets)
    examples/                  # valid/ and invalid/ trees
    branding/                  # icons used by desktop/MIME integration
  share/doc/lullaby/
    RELEASE_NOTES.md
    README(.txt)
    LICENSE
```

Per-OS `$PREFIX` mapping:

| OS / channel | `$PREFIX` | PATH entry |
| :--- | :--- | :--- |
| Windows MSI (per-user) | `%LOCALAPPDATA%\Programs\Lullaby` | user `Path` |
| Windows MSI (per-machine) | `%ProgramFiles%\Lullaby` | machine `Path` |
| Windows portable `.zip` | wherever unpacked | optional via `install.cmd` |
| Linux `.deb` / `.rpm` | `/usr` (files under `/usr/bin`, `/usr/lib/lullaby`, `/usr/share/...`) | already on PATH |
| Linux tarball | wherever unpacked, helper targets `/usr/local` or `~/.local` | via `install.sh` |
| macOS Homebrew | Homebrew Cellar, symlinked into the Homebrew prefix `bin` | already on PATH |
| macOS tarball | wherever unpacked | via `install.sh` |
| Web installer (per-user) | `~/.lullaby` (Unix) / `%LOCALAPPDATA%\Programs\Lullaby` (Win) | user profile / user `Path` |

The existing portable packages use a flatter `bin/ docs/ examples/` shape. Phase 8
migrates the portable layout onto the `share/lullaby/...` canonical layout so all
channels agree; the portable-package driver
(`scripts/package_portable.py`) is the one place that shape is defined, so the
migration is localized there and re-verified by `scripts/verify_release.ps1`.

## 3. Windows

Four channels, in priority order: **winget** (discovery + updates), **MSI**
(enterprise/GPO, the artifact winget points at), **portable `.zip`** (already
shipping), **NSIS `.exe`** (a lightweight double-click alternative).

### 3.1 MSI (WiX Toolset v4)

The MSI is the authoritative Windows installer and the artifact the winget
manifest references.

- **Toolchain:** WiX v4 (`wix build`), authored in a `packaging/windows/lullaby.wxs`
  source. Built on the `windows-latest` CI runner where `wix` is installed via
  `dotnet tool install --global wix`.
- **UpgradeCode:** a single fixed GUID for the product line, generated once and
  never changed, so every future version is a **major upgrade** of the prior one
  (`MajorUpgrade` element with `DowngradeErrorMessage`, `Schedule="afterInstallValidate"`).
  The `ProductCode` is `*` (auto-regenerated per build); `Version` is the
  toolchain semver (WiX ignores the 4th field and pre-release tags, so the MSI
  `ProductVersion` is the numeric `MAJOR.MINOR.PATCH` and the full semver lives in
  a property + ARP `DisplayVersion`).
- **Install dir:** default per-user `%LOCALAPPDATA%\Programs\Lullaby`
  (`InstallScope` per-user, no elevation) with an optional per-machine mode
  (`%ProgramFiles%\Lullaby`, elevated). Per-user default keeps "install in 60
  seconds, no admin prompt" true for individual developers; enterprises push the
  per-machine MSI via GPO/Intune.
- **PATH:** add `$PREFIX\bin` via a WiX `Environment` element
  (`Action="set" Part="last" System="no"` for per-user;
  `System="yes"` for per-machine). WiX removes the exact entry it added on
  uninstall — no manual PATH surgery, no stale entries.
- **`.lby` file association:** WiX `ProgId` + `Extension` + `Verb` registering:
  - a `Lullaby.Source` ProgId with the `.lby` icon from `branding/lullaby.ico`,
  - a friendly type name "Lullaby source file",
  - an `open` verb wired to an editor is intentionally **not** set (we do not ship
    an editor); instead the association exists for icon + "Open with" and a
    `run` context-menu verb `"%PREFIX%\bin\lullaby.exe" run "%1"`.
  Associations are registered under `HKCU` (per-user) or `HKLM` (per-machine) to
  match `InstallScope`, and removed on uninstall.
- **Components/features:** one feature tree — `bin` (required), `docs`,
  `examples`. Each file is its own `Component` with a stable GUID under a
  directory that mirrors the canonical layout. `RemoveFolder`/`RemoveFile`
  entries guarantee **clean uninstall** (the install dir is empty and deleted
  afterward).
- **ARP metadata:** `DisplayName=Lullaby`, `Publisher`, `DisplayVersion`,
  `HelpLink`/`URLInfoAbout=https://lullaby-lang.org`, `ARPPRODUCTICON` set to the
  brand icon, `ARPNOMODIFY=1`.
- **Signing:** Authenticode sign `lullaby.exe` and the `.msi` with a code-signing
  certificate when one is configured as a CI secret (EV or OV cert;
  `signtool sign /fd sha256 /tr <RFC3161-TSA> /td sha256`). Signing is **gated on
  the secret being present** — unsigned artifacts still build and publish for
  1.0, with a documented SmartScreen note; a purchased cert upgrades the
  experience without changing the pipeline.
- **Verification:** a headless `msiexec /i lullaby.msi /qn INSTALLDIR=... /l*v`
  install on the CI runner, then assert `lullaby --version` resolves via the new
  PATH (in a fresh process), the `.lby` ProgId exists, and
  `msiexec /x /qn` leaves no files and no PATH entry behind.

### 3.2 winget manifest + submission

- **Manifest:** the three-file winget v1.6 multi-file manifest under
  `manifests/l/Lullaby/Lullaby/<version>/`:
  - `Lullaby.Lullaby.yaml` (version manifest),
  - `Lullaby.Lullaby.installer.yaml` (installer type `wix`, `Architecture: x64`,
    the release-asset `InstallerUrl`, the `InstallerSha256`, `Scope: user` with a
    per-machine entry, silent switches `/qn`, and `AppsAndFeaturesEntries` with the
    fixed `UpgradeCode` so winget correlates upgrades),
  - `Lullaby.Lullaby.locale.en-US.yaml` (publisher, license, short description,
    tags `programming-language`, `compiler`, `lullaby`, moniker `lullaby`).
- **Source of truth:** the manifest is generated by CI with `wingetcreate` from
  the just-published MSI URL + hash so the hash is never hand-edited.
- **Submission flow:** on a tagged stable release, CI runs
  `wingetcreate submit --token $WINGET_PAT` which forks
  `microsoft/winget-pkgs`, commits the manifest, and opens the PR. The PR runs
  Microsoft's automated validation (installs the MSI in a sandbox, checks silent
  install/uninstall). We keep a `winget/` copy of the manifests in-repo for
  provenance and local `winget validate --manifest` runs.
- **Constraint:** winget requires a **stable** (non-prerelease) version and a
  publicly downloadable installer URL; prerelease tags are published to GitHub
  Releases and the web installer but not submitted to winget until the first
  stable `1.0.0`.

### 3.3 NSIS `.exe` alternative

A single self-extracting `.exe` for users who prefer a double-click wizard over
`msiexec`, and as a fallback where MSI/GPO is awkward.

- **Toolchain:** NSIS 3 with a `packaging/windows/lullaby.nsi` script, built on
  the Windows runner (`makensis`).
- **Behavior parity with the MSI:** installs the canonical layout under
  `%LOCALAPPDATA%\Programs\Lullaby`, prepends `bin` to the user PATH
  (`EnVar` plugin, with the matching removal in the uninstaller), registers the
  `.lby` ProgId/icon, writes an ARP uninstall entry
  (`HKCU\...\Uninstall\Lullaby`), and drops `uninstall.exe`.
- **Signing:** same Authenticode gating as the MSI.
- **Positioning:** the MSI is primary (winget + enterprise); the NSIS `.exe` is a
  documented convenience download. Both are byte-verified by checksum in the
  release.

### 3.4 Portable `.zip` (already shipping)

`scripts/package_windows_portable.ps1` already produces
`lullaby-windows-x64.zip` + `.sha256` with `bin/`, `docs/`, `examples/`,
release notes, and `install.cmd`/`install.ps1` PATH helpers. Phase 8 keeps this,
migrates it onto the canonical `share/lullaby/...` layout, and renames the archive
to the versioned scheme in § 7.

## 4. Linux

Four channels: **`.deb`** (Debian/Ubuntu, `apt`), **`.rpm`** (Fedora/RHEL,
`dnf`/`yum`), a **distro-agnostic** package, and the **tarball**.

### 4.1 `.deb` (apt)

- **Build:** `dpkg-deb --build` from a staged tree
  (`packaging/linux/deb/`), or `cargo-deb` for convenience; either way the
  emitted control metadata is what matters. Built for `amd64` and `arm64`.
- **Layout:** files land under `/usr` — `/usr/bin/lullaby`,
  `/usr/lib/lullaby/`, `/usr/share/lullaby/{docs,examples,branding}`,
  `/usr/share/doc/lullaby/` (with a Debian `changelog.Debian.gz` and `copyright`).
- **Control:** `Package: lullaby`, `Architecture: amd64|arm64`,
  `Maintainer`, `Version: <semver>-1`, `Section: devel`, `Priority: optional`,
  `Depends:` kept minimal — the release binary is statically linked against musl
  where feasible (see § 4.5) so runtime deps are empty or just `libc6`;
  `Homepage: https://lullaby-lang.org`, a concise `Description`.
- **PATH:** none needed — `/usr/bin` is already on PATH.
- **File association + icon:** ship a freedesktop MIME package
  (`/usr/share/mime/packages/lullaby.xml` declaring `text/x-lullaby` for `*.lby`),
  hicolor icons, and update caches from `postinst`
  (`update-mime-database`, `gtk-update-icon-cache`) with matching `postrm`
  cleanup. No `.desktop` launcher (Lullaby is a CLI, not a GUI app).
- **Clean uninstall:** `apt remove` removes all packaged files; `postrm`
  refreshes the MIME/icon caches.
- **Verification:** `lintian` on the `.deb` (no errors), a container install
  (`apt-get install ./lullaby_*.deb`), `lullaby --version`, then
  `apt-get remove` leaving nothing behind.

### 4.2 `.rpm` (dnf/yum)

- **Build:** `rpmbuild -bb packaging/linux/rpm/lullaby.spec` (or `cargo-generate-rpm`),
  for `x86_64` and `aarch64`.
- **Spec:** `Name: lullaby`, `Version`/`Release`, `License`, `URL`,
  `Summary`, `BuildArch` per target, minimal `Requires`, a `%files` list mirroring
  the canonical layout, and `%post`/`%postun` scriptlets to run
  `update-mime-database` and `gtk-update-icon-cache`. `%changelog` is required by
  `rpmlint`.
- **Verification:** `rpmlint` clean, a container install
  (`dnf install ./lullaby-*.rpm` on Fedora), `lullaby --version`, and
  `dnf remove` verification.

### 4.3 Distro-agnostic: **AppImage** (recommended)

We recommend **AppImage** as the single-file, distro-agnostic Linux artifact, over
Snap and Flatpak:

- **Why AppImage:** Lullaby is a self-contained CLI with essentially no runtime
  dependencies. AppImage is a single executable file the user downloads,
  `chmod +x`, and runs — no daemon (Snap's `snapd`), no runtime framework and
  sandbox portal story (Flatpak), and no store account. That matches the "one
  download, run it" promise for CLI tools better than Snap/Flatpak, which are
  optimized for sandboxed **GUI desktop apps** and add a heavy dependency and,
  for Snap, a single-vendor store.
- **Why not Snap:** `snapd` is not present or default on most non-Ubuntu distros,
  classic confinement (needed for a compiler that writes arbitrary output files
  and shells out to a linker) requires manual review/approval, and the store is
  Canonical-controlled.
- **Why not Flatpak:** designed around a runtime + sandbox portal model aimed at
  GUI apps; a CLI compiler that must read/write the user's project tree and invoke
  an external linker fights the sandbox. Flatpak remains a reasonable **later**
  addition if desktop-store presence is wanted, but it is not the 1.0 pick.
- **Build:** `linuxdeploy` + `appimagetool` from the staged canonical layout,
  producing `Lullaby-<version>-x86_64.AppImage` and `-aarch64.AppImage`. The
  AppRun entry point execs `bin/lullaby`. PATH is the user's responsibility (they
  can symlink it or use the web installer, which can fetch the AppImage);
  optionally ship `appimaged`-friendly metadata.
- **Verification:** run the AppImage in a minimal container (no Rust, no extra
  libs) and assert `lullaby --version`, `run`, and `wasm` all work — this is the
  strongest proof the bundle is genuinely self-contained.

### 4.4 Tarball (already shipping)

`scripts/package_portable.py` already emits `*.tar.gz` + `.sha256` with the
`install.sh`/`uninstall.sh` PATH helpers for Linux and macOS. Phase 8 migrates it
onto the canonical layout and versioned naming (§ 7).

### 4.5 Static linking note

To keep `.deb`/`.rpm`/AppImage dependency-free and portable across distro libc
versions, the release Linux binaries target
`x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` (fully static) where
the toolchain builds cleanly, falling back to `-gnu` with a documented minimum
glibc otherwise. The choice is recorded per release in the manifest so downstream
packagers and the web installer pick the right artifact.

## 5. macOS

macOS is supported for 1.0 through the **license-free** path only: **tarball +
Homebrew**. Signed/notarized installers are a gated follow-up.

### 5.1 Tarball + Gatekeeper quarantine

- The `*.tar.gz` from `scripts/package_portable.py` (targets `x86_64-apple-darwin`
  and `aarch64-apple-darwin`) is the base macOS artifact, with `install.sh`.
- **Gatekeeper:** a binary downloaded via a browser gets the
  `com.apple.quarantine` extended attribute; because it is unsigned/unnotarized,
  Gatekeeper blocks first run. We document the one-time clear:

  ```sh
  xattr -d com.apple.quarantine ~/.lullaby/bin/lullaby   # or the install dir
  # or, from Finder: right-click → Open → Open (first run only)
  ```

  The web installer and `brew` paths avoid this entirely (curl-downloaded files
  and Homebrew-managed files are not quarantined), so the documented `xattr` step
  is only for users who manually download the tarball in a browser.

### 5.2 Homebrew formula (recommended macOS path)

- **Formula:** `Formula/lullaby.rb` in a `lullaby-lang/homebrew-tap` repo
  (`brew install lullaby-lang/tap/lullaby`). It is a **bottle-style binary
  formula** — `url` points at the release `*.tar.gz`, `sha256` is the checksum,
  and `install` copies the canonical layout into the Homebrew prefix
  (`bin.install "bin/lullaby"`, `pkgshare.install "share/lullaby/..."`,
  `man`/`doc` as appropriate). Separate `url`/`sha256` per `arm` vs `intel` via
  `on_arm`/`on_intel`.
- **Why a tap, not homebrew-core first:** homebrew-core requires a notable,
  stable, well-tested project and does its own build-from-source; a personal tap
  ships immediately and is the standard on-ramp. Migrating to core is a post-1.0
  goal once adoption and stability warrant it.
- **PATH/quarantine:** Homebrew manages the `bin` symlink (already on PATH) and
  Homebrew-installed files are not quarantined, so no `xattr` step.
- **Verification:** `brew install`, `brew test lullaby` (a `test do` block that
  runs `lullaby --version` and compiles a trivial `.lby`), and `brew audit
  --strict --online lullaby` in CI against the tap.

### 5.3 Signed/notarized `.pkg`/`.dmg` (gated follow-up — NOT a 1.0 blocker)

- A signed `.pkg`/`.dmg` (no Gatekeeper friction, no `xattr`) requires a **paid
  Apple Developer account** ($99/yr) for a Developer ID certificate **and a Mac**
  to run `codesign`, `productbuild`/`pkgbuild`, and `notarytool` (notarization +
  stapling). None of these can be done without the account, and codesigning/
  notarization cannot run on Linux/Windows.
- Therefore this is an **optional, gated follow-up**, explicitly out of the 1.0
  critical path, matching the macOS note in the 1.0 roadmap (`roadmap_1_0`). When
  a Developer ID and a macOS signer (a `macos-*` GitHub runner is a Mac and can
  sign) are available, we add a notarized `.pkg` channel behind the same
  secret-gated pattern as Windows signing.
- **Cross-build feasibility:** the macOS **binaries** can in principle be
  cross-compiled from Linux CI with an osxcross/SDK setup, but this is brittle and
  we prefer building them natively on the `macos-14` (arm64) and `macos-15-intel`
  (x64) runners already used by the CI template. What **strictly needs a Mac** is
  code **signing, notarization, and `.pkg`/`.dmg` assembly** — those never run off
  a Mac. So: native macOS runners build + test + tarball for 1.0; signing is the
  only piece deferred to the gated Apple-account follow-up.

## 6. One-line web installer

The bootstrap scripts behind `install.sh` / `install.ps1` in § 1.4. They are the
lowest-friction path and the recommended default in docs.

### 6.1 Behavior (both scripts)

1. **Detect OS + arch:** Unix via `uname -s`/`uname -m` → `{linux,macos} × {x64,arm64}`;
   Windows via `$env:PROCESSOR_ARCHITECTURE` / .NET `RuntimeInformation`.
2. **Resolve the release:** default to the latest stable GitHub Release (or a
   pinned `LULLABY_VERSION`), reading the release's `manifest.json` (§ 7) to map
   OS/arch → the correct portable archive asset and its published SHA-256. musl vs
   glibc selection on Linux is read from the manifest, not guessed.
3. **Download** the archive over HTTPS to a temp dir.
4. **Verify checksum:** recompute SHA-256 (`sha256sum`/`shasum -a 256` /
   `Get-FileHash`) and compare to the manifest value; **abort on mismatch**. When
   a release signature (§ 7 minisign/GPG) is present, verify it too.
5. **Install** the canonical layout into the per-user prefix (`~/.lullaby` on
   Unix, `%LOCALAPPDATA%\Programs\Lullaby` on Windows) — no root/admin, reusing
   the same PATH-helper logic as the portable `install.sh`/`install.ps1`
   (`~/.config/lullaby/env` sourced from the profile on Unix; user `Path` on
   Windows).
6. **PATH + first-run hint:** add `bin` to PATH and print the exact
   "open a new shell, then run `lullaby --version`" line, plus `lullaby new`.
7. **Idempotent + uninstall:** re-running upgrades in place; the scripts support a
   documented `uninstall` mode (or point at `~/.lullaby/uninstall.sh`).

### 6.2 Hosting and integrity

- The scripts are served from `https://lullaby-lang.org/install.sh` and
  `/install.ps1`, which are just **stable redirects/aliases** to the versioned,
  checksum-pinned copies committed in the repo under `web/` and published as
  release assets — so the "curl | sh" content is itself reviewable and
  reproducible, not an opaque server-side blob.
- The scripts pull **binaries** from GitHub Releases (durable, CDN-backed);
  `lullaby-lang.org` only serves the small bootstrap text and the redirect. This
  keeps the trust surface small: the site can change the pointer, but every binary
  is checksum- (and, where available, signature-) verified before it runs.
- The security posture ("we run curl | sh; here's why it's checksum-verified and
  how to read the script first") is documented next to the command, and `sh`
  users can always `curl -o install.sh`, read it, then run it.

## 7. Release automation

### 7.1 CI matrix

A tag-triggered `release` workflow that extends the existing verification template
([portable_package_ci_workflow.yml](portable_package_ci_workflow.yml)) from
*verify-only* into *build-verify-publish*. Matrix:

| Runner | Target triple | Artifact tag | Produces |
| :--- | :--- | :--- | :--- |
| `windows-latest` | `x86_64-pc-windows-msvc` | `windows-x64` | portable `.zip`, MSI, NSIS `.exe` |
| `ubuntu-latest` | `x86_64-unknown-linux-musl` | `linux-x64` | `.tar.gz`, `.deb`, `.rpm`, AppImage |
| `ubuntu-24.04-arm` (or QEMU/cross) | `aarch64-unknown-linux-musl` | `linux-arm64` | `.tar.gz`, `.deb`, `.rpm`, AppImage |
| `macos-15-intel` | `x86_64-apple-darwin` | `macos-x64` | `.tar.gz` (+ signed `.pkg` when gated) |
| `macos-14` | `aarch64-apple-darwin` | `macos-arm64` | `.tar.gz` (+ signed `.pkg` when gated) |

### 7.2 Per-job gate (unchanged, mandatory)

Every job runs the **full existing gate before packaging**, exactly as the
template and `scripts/verify_release.ps1` do: `cargo fmt --check`,
`cargo test --all`, `cargo clippy --all-targets --all-features -- -D warnings`,
shipped offline-docs verification, generated offline-docs build + verification,
markdown-reference verification, and the portable-package `--verify` smoke test
(build → check → run → compile → inspect → run artifact → checksum). No artifact
is produced from a job whose gate is red.

### 7.3 Artifacts, checksums, signing, publish

1. Each job builds its portable archive via the existing drivers, then its
   OS-native packages (MSI/NSIS on Windows, deb/rpm/AppImage on Linux, tarball on
   macOS).
2. Each job emits a `.sha256` beside every artifact (existing behavior) and, where
   a signing secret exists, an Authenticode signature (Windows) / a detached
   **minisign** or GPG signature over the checksums (Linux/macOS). Signing is
   secret-gated; its absence never fails the build.
3. A **collect** job downloads every job's artifacts and generates one
   `manifest.json` (version, git commit, and per-OS/arch asset name + sha256 +
   libc flavor + optional signature) — the file the web installer reads.
4. **Publish:** on a version tag, create/update the GitHub Release with all
   artifacts + checksums + `manifest.json` (following
   `scripts/publish_github_release.ps1`, generalized beyond the single Windows
   zip). Then update the install endpoints: refresh the
   `lullaby-lang.org/install.{sh,ps1}` version pointer and, on a **stable** tag,
   run the `wingetcreate submit` and Homebrew-tap-bump steps (§ 3.2, § 5.2). The
   deb/rpm can additionally be pushed to a hosted apt/dnf repo as a post-1.0
   enhancement; for 1.0 they are GitHub Release assets installable with
   `apt-get install ./file.deb` / `dnf install ./file.rpm`.
5. **Provenance:** enable GitHub Actions artifact attestations/SLSA provenance for
   the release assets so downloads are verifiably CI-built.

### 7.4 Relationship to the existing template

[portable_package_ci_workflow.yml](portable_package_ci_workflow.yml) stays as the
**PR/`main` verification** workflow (verify portable packages on all four
targets). The new `release.yml` reuses its gate steps verbatim and adds the
native-package build + sign + publish steps, triggered only on tags. Both are
copied under `.github/workflows/` from an authenticated session with the GitHub
`workflow` scope (the template already documents this activation constraint).

## 8. First-run UX

The "hello world in 60 seconds" path, measured from `install` to running output.

### 8.1 `lullaby new` scaffolding

A new `lullaby new <name>` subcommand (list-07 implementation ticket) creates a
minimal, buildable project:

```text
<name>/
  lullaby.json        # name, entry = src/main.lby, src = ["src"]
  src/main.lby        # a working "hello" program
  .gitignore          # ignores build artifacts (*.lbc, *.wasm, *.obj, *.exe)
  README.md           # the two commands to run it
```

`src/main.lby` is a real, checked, runnable program in the current surface:

```lby
fn main -> i64
    println("Hello from Lullaby!")
    0
```

Flags: `--lib` scaffolds an entry-less library project (a `lullaby.json` with no
`entry` plus `src/<name>.lby`, mirroring `examples/valid/mathx/`); default is the
runnable app above. The scaffolder writes a `lullaby.json` shaped exactly like the
manifests the loader already accepts (`crates/lullaby_cli/src/manifest.rs`), so
`lullaby run <name>` works immediately.

### 8.2 The 60-second path

The docs quick-start and the web installer's closing message both print this:

```text
# 1. install (pick your OS line from § 1.4), open a fresh shell
lullaby --version

# 2. scaffold and run
lullaby new hello
lullaby run hello        # -> Hello from Lullaby!

# 3. explore
lullaby examples         # list bundled examples
lullaby docs             # open the offline docs bundle
```

Every command here already exists except `lullaby new`; the bundled `docs`/
`examples` commands already open the shipped offline docs and example tree. The
success criterion for 1.0 is that a fresh machine goes from the install one-liner
to `Hello from Lullaby!` in under a minute with no extra downloads (native compile
being the only feature that documents a host-linker prerequisite).

## 9. Scope and sequencing

### 9.1 In scope for 1.0

- Branding finalized: name/extension confirmed (done), brand assets under
  `branding/`, tagline, and the advertised install command surface (§ 1).
- Canonical on-disk layout adopted by the portable drivers (§ 2.2).
- Windows: MSI (PATH + `.lby` association + clean uninstall), winget manifest +
  submission wiring, NSIS `.exe`, portable `.zip` (§ 3).
- Linux: `.deb`, `.rpm`, AppImage, tarball (§ 4).
- macOS: tarball (+ documented Gatekeeper step) and Homebrew tap formula (§ 5).
- One-line web installer for both platforms with checksum verification (§ 6).
- `release.yml` automation producing every artifact + checksums + `manifest.json`
  and publishing to GitHub Releases and the install endpoints (§ 7).
- `lullaby new` + the 60-second first-run path (§ 8).

### 9.2 Deferred (post-1.0, gated, or later)

- Signed/notarized macOS `.pkg`/`.dmg` — gated on a paid Apple Developer account
  and a Mac signer (§ 5.3).
- Authenticode signing — ships as soon as a code-signing cert secret exists;
  unsigned artifacts are acceptable for 1.0 (§ 3.1).
- Hosted apt/dnf repositories and a Flatpak build (§ 4.1, § 4.3).
- Homebrew **core** submission (tap is the 1.0 path) (§ 5.2).
- winget submission for pre-release tags (winget wants stable versions) (§ 3.2).
- A vendored/bundled linker for native compilation (documented host prerequisite
  for 1.0) (§ 2.1).

### 9.3 Order

Layout migration (§ 2.2) first, since every packager targets it. Then, in
parallel across disjoint packaging files: MSI + NSIS (Windows), deb + rpm +
AppImage (Linux), Homebrew formula (macOS), and the web installer. `lullaby new`
(a CLI change, hot file) is sequenced on its own. `release.yml` lands last, once
each packager builds locally, and is the integration point. This matches the
parallel-agent, disjoint-footprint execution model in the 1.0 roadmap
(`roadmap_1_0`).

## 10. Why these choices

- **One binary, subcommands, one version.** `fmt`, `lsp`, and `new` are
  subcommands of `lullaby`, not separate tools — fewer things to install, one
  semver, trivial packaging. Matches the current CLI reality.
- **One canonical on-disk layout for every packager.** Packagers differ only in
  `$PREFIX` and PATH/association wiring; the file set is defined once
  (in the portable driver), so a change to what ships is a one-place edit that all
  channels inherit.
- **Native installers per OS, plus a portable escape hatch.** winget/MSI, apt/dnf,
  and Homebrew are where each platform's users actually look; the portable
  archives and the AppImage cover everyone else with zero package manager.
- **AppImage over Snap/Flatpak** for a dependency-free CLI: single file, no daemon,
  no store, no sandbox fight with a compiler that writes files and shells out to a
  linker.
- **License-free macOS path for 1.0.** Tarball + Homebrew need no Apple account;
  signing/notarization is real work behind a paid account and a Mac, so it is a
  gated follow-up, not a release blocker — exactly as the 1.0 roadmap states.
- **Checksum-verified, reviewable one-liners.** The web installer never runs an
  unverified binary: it reads a signed manifest, checks SHA-256 (and signatures
  where present), and the bootstrap text itself is committed and reproducible.
- **Reuse the existing gate.** Release automation extends the proven verification
  template and `scripts/verify_release.ps1` rather than inventing a new pipeline;
  no artifact escapes a green `fmt`/`test`/`clippy`/docs/package gate.
- **Secret-gated signing.** Signing improves the experience the moment a
  certificate/account exists, without ever blocking or branching the core
  pipeline — so 1.0 ships on time and hardens continuously.
