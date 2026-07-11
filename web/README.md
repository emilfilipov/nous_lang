# Web installers

The one-line bootstrap installers for Lullaby. They are intentionally small,
pure-ASCII, and reviewable: each downloads the correct portable package for the
host from the latest GitHub Release, **verifies its published SHA-256 before
running anything**, installs it under a per-user prefix (no root/admin), and
wires `bin` onto PATH by delegating to the package's own PATH helper.

| Script | Platforms | One-liner |
| --- | --- | --- |
| [`install.sh`](install.sh) | Linux, macOS | `curl -fsSL https://lullaby.skazkasolutions.com/install.sh \| sh` |
| [`install.ps1`](install.ps1) | Windows | `irm https://lullaby.skazkasolutions.com/install.ps1 \| iex` |

## Behavior

1. Detect OS + arch → target tag (`linux-x64`, `macos-arm64`, `macos-x64`,
   `windows-x64`).
2. Resolve the release: newest **stable** release, falling back to the newest
   prerelease (so the command works while every release is still a
   prerelease). Pin an exact tag with `LULLABY_VERSION`.
3. Download the matching portable archive **and its `.sha256`** over HTTPS.
4. Recompute the SHA-256 and compare; **abort on mismatch**.
5. Extract into a per-user prefix (`~/.lullaby`, or
   `%LOCALAPPDATA%\Programs\Lullaby` on Windows).
6. Add `bin` to PATH via the package's bundled `install.sh` / `install.ps1`.
7. Re-running upgrades in place.

### Overrides

`LULLABY_VERSION` (exact tag), `LULLABY_PREFIX` (install location), and
`LULLABY_REPO` (`owner/repo`) are honored by both scripts; the PowerShell script
also accepts `-Version`, `-Prefix`, `-Repo`, and `-Uninstall`.

### Uninstall

```sh
curl -fsSL https://lullaby.skazkasolutions.com/install.sh | sh -s -- uninstall
```
```powershell
irm https://lullaby.skazkasolutions.com/install.ps1 -OutFile install.ps1; ./install.ps1 -Uninstall
```

## Why `curl | sh` is safe here

The site serves only this small, version-controlled bootstrap text — the actual
binaries come from GitHub Releases and are **checksum-verified before they run**.
Nothing is executed until the download matches its published SHA-256. If you'd
rather read before running, save the script first:

```sh
curl -fsSL https://lullaby.skazkasolutions.com/install.sh -o install.sh
less install.sh    # read it
sh install.sh
```

## Hosting

These two files are served verbatim at `https://lullaby.skazkasolutions.com/install.sh`
and `/install.ps1`. They pull binaries from GitHub Releases, so the site only
needs to serve the static bootstrap text.
