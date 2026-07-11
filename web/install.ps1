# Lullaby one-line web installer (Windows).
#
#   irm https://lullaby.skazkasolutions.com/install.ps1 | iex
#
# Downloads the correct portable package for this architecture from the latest
# GitHub Release, verifies its published SHA-256, installs it under a per-user
# prefix (no admin), and wires bin onto the user PATH by delegating to the
# package's own install.ps1 helper. Re-running upgrades in place.
#
# To read before running, or to uninstall / pin a version:
#   irm https://lullaby.skazkasolutions.com/install.ps1 -OutFile install.ps1
#   ./install.ps1 -Uninstall
#   $env:LULLABY_VERSION = 'v1.0.0-preview'; ./install.ps1
#
# Overrides (environment variables or parameters):
#   LULLABY_VERSION / -Version   install a specific tag instead of latest
#   LULLABY_PREFIX  / -Prefix    install prefix (default: %LOCALAPPDATA%\Programs\Lullaby)
#   LULLABY_REPO    / -Repo      owner/repo (default: emilfilipov/lullaby-lang)
[CmdletBinding()]
param(
    [switch]$Uninstall,
    [string]$Version = $env:LULLABY_VERSION,
    [string]$Prefix = $env:LULLABY_PREFIX,
    [string]$Repo = $(if ($env:LULLABY_REPO) { $env:LULLABY_REPO } else { 'emilfilipov/lullaby-lang' })
)

$ErrorActionPreference = 'Stop'
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch { }

if (-not $Prefix) {
    $Prefix = Join-Path $env:LOCALAPPDATA 'Programs\Lullaby'
}

function Info($msg) { Write-Host "lullaby: $msg" }
function Die($msg) { throw "lullaby: error: $msg" }

# --- uninstall mode -------------------------------------------------------
if ($Uninstall) {
    $helper = Join-Path $Prefix 'uninstall.ps1'
    if (Test-Path -LiteralPath $helper) {
        try { & powershell -NoProfile -ExecutionPolicy Bypass -File $helper } catch { }
    }
    if (Test-Path -LiteralPath $Prefix) {
        Remove-Item -LiteralPath $Prefix -Recurse -Force
        Info "removed $Prefix"
    } else {
        Info "nothing to uninstall at $Prefix"
    }
    Info "open a new shell to refresh PATH"
    return
}

# --- detect architecture -> target tag -----------------------------------
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ($arch) {
    'X64'   { $archTag = 'x64' }
    'Arm64' { $archTag = 'arm64' }
    default { Die "unsupported architecture '$arch'" }
}
$targetTag = "windows-$archTag"

# --- resolve the release asset -------------------------------------------
Info "resolving $targetTag package from $Repo"
$ua = @{ 'User-Agent' = 'lullaby-install' }
if ($Version) {
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/tags/$Version" -Headers $ua
    } catch {
        Die "no release tagged ${Version} in ${Repo}: $($_.Exception.Message)"
    }
} else {
    try {
        # Newest stable release.
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $ua
    } catch {
        # No stable release yet (pre-1.0: every release is a prerelease). Fall
        # back to the newest release of any kind.
        try {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases?per_page=1" -Headers $ua | Select-Object -First 1
        } catch {
            Die "could not query GitHub Releases for ${Repo}: $($_.Exception.Message)"
        }
    }
}
$ref = $release.tag_name
if (-not $ref) { Die 'could not determine the release tag' }

# Match the portable archive for this target; the package-name prefix may vary
# between releases, so match on the trailing <target_tag>.zip.
$asset = $release.assets | Where-Object { $_.name -match "$targetTag\.zip$" } | Select-Object -First 1
if (-not $asset) {
    Die "no prebuilt package for $targetTag in release $ref (it may not be built for this platform yet)"
}
$checksumAsset = $release.assets | Where-Object { $_.name -eq ($asset.name + '.sha256') } | Select-Object -First 1
if (-not $checksumAsset) {
    Die "published checksum $($asset.name).sha256 not found in release $ref"
}

# --- download + verify ----------------------------------------------------
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("lullaby-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    $archive = Join-Path $tmp 'package.zip'
    $shaFile = Join-Path $tmp 'package.zip.sha256'

    Info "downloading $($asset.name)"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $archive -Headers @{ 'User-Agent' = 'lullaby-install' }
    Invoke-WebRequest -Uri $checksumAsset.browser_download_url -OutFile $shaFile -Headers @{ 'User-Agent' = 'lullaby-install' }

    $expected = ((Get-Content -LiteralPath $shaFile -Raw).Trim() -split '\s+')[0]
    $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
    if (-not $expected) { Die 'empty published checksum' }
    if ($expected -ine $actual) {
        Die "checksum mismatch (expected $expected, got $actual) - refusing to install"
    }
    Info 'checksum verified'

    # --- install into the prefix -----------------------------------------
    $extract = Join-Path $tmp 'extract'
    Expand-Archive -LiteralPath $archive -DestinationPath $extract -Force
    $top = Get-ChildItem -LiteralPath $extract -Directory | Select-Object -First 1
    if (-not $top) { Die 'unexpected archive layout (no top-level package directory)' }

    if (Test-Path -LiteralPath $Prefix) { Remove-Item -LiteralPath $Prefix -Recurse -Force }
    New-Item -ItemType Directory -Path $Prefix -Force | Out-Null
    Copy-Item -Path (Join-Path $top.FullName '*') -Destination $Prefix -Recurse -Force
    Info "installed to $Prefix"

    # --- wire PATH via the package's own helper --------------------------
    $helper = Join-Path $Prefix 'install.ps1'
    if (Test-Path -LiteralPath $helper) {
        & powershell -NoProfile -ExecutionPolicy Bypass -File $helper
    } else {
        Info "add $Prefix\bin to your PATH manually"
    }

    Write-Host ''
    Info "done - Lullaby $ref ($targetTag)"
    Info 'open a new shell, then run:  lullaby --version'
    Info 'start a project with:        lullaby new my_app'
} finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
