param(
    [string]$PackageName = "lullaby-windows-x64",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $false

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $ScriptDir "..")
$PackageRoot = Join-Path $RepoRoot "dist\$PackageName"
$ArchivePath = Join-Path $RepoRoot "dist\$PackageName.zip"
$ChecksumPath = "$ArchivePath.sha256"

Push-Location $RepoRoot
try {
    if (-not $SkipBuild) {
        cargo build --release -p lullaby_cli
        if ($LASTEXITCODE -ne 0) {
            throw "cargo release build failed"
        }
    }

    $Binary = Join-Path $RepoRoot "target\release\lullaby.exe"
    if (-not (Test-Path -LiteralPath $Binary)) {
        throw "release binary not found: $Binary"
    }

    Remove-Item -LiteralPath $PackageRoot -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path (Join-Path $PackageRoot "bin") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $PackageRoot "docs") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $PackageRoot "examples") | Out-Null

    Copy-Item -LiteralPath $Binary -Destination (Join-Path $PackageRoot "bin\lullaby.exe")
    $PackageDocs = Join-Path $PackageRoot "docs\index.html"
    python (Join-Path $RepoRoot "offline_docs\generate_offline_docs.py") $PackageDocs
    if ($LASTEXITCODE -ne 0) {
        throw "offline docs generation failed"
    }
    python (Join-Path $RepoRoot "offline_docs\verify_offline_docs.py") $PackageDocs --profile generated
    if ($LASTEXITCODE -ne 0) {
        throw "offline docs verification failed"
    }
    Copy-Item -LiteralPath (Join-Path $RepoRoot "examples\README.md") -Destination (Join-Path $PackageRoot "examples\README.md")
    Copy-Item -LiteralPath (Join-Path $RepoRoot "examples\valid") -Destination (Join-Path $PackageRoot "examples\valid") -Recurse
    Copy-Item -LiteralPath (Join-Path $RepoRoot "examples\invalid") -Destination (Join-Path $PackageRoot "examples\invalid") -Recurse
    Copy-Item -LiteralPath (Join-Path $RepoRoot "documents\release_notes.md") -Destination (Join-Path $PackageRoot "RELEASE_NOTES.md")
    Copy-Item -LiteralPath (Join-Path $RepoRoot "scripts\install_windows_path.ps1") -Destination (Join-Path $PackageRoot "install.ps1")
    Copy-Item -LiteralPath (Join-Path $RepoRoot "scripts\uninstall_windows_path.ps1") -Destination (Join-Path $PackageRoot "uninstall.ps1")
    Copy-Item -LiteralPath (Join-Path $RepoRoot "scripts\install.cmd") -Destination (Join-Path $PackageRoot "install.cmd")
    Copy-Item -LiteralPath (Join-Path $RepoRoot "scripts\uninstall.cmd") -Destination (Join-Path $PackageRoot "uninstall.cmd")

    $LicenseStatus = "No repository license file was present when this package was created."
    foreach ($LicenseName in @("LICENSE", "LICENSE.txt", "LICENSE.md", "COPYING", "COPYING.txt")) {
        $LicensePath = Join-Path $RepoRoot $LicenseName
        if (Test-Path -LiteralPath $LicensePath) {
            Copy-Item -LiteralPath $LicensePath -Destination (Join-Path $PackageRoot $LicenseName)
            $LicenseStatus = "License file: $LicenseName"
            break
        }
    }

    $Commit = "unknown"
    try {
        $Commit = (git rev-parse --short HEAD).Trim()
    } catch {
        $Commit = "unknown"
    }

    @"
Lullaby portable package
Commit: $Commit
$LicenseStatus

Layout:
- bin\lullaby.exe: command-line tool
- docs\index.html: offline documentation
- examples\: executable and invalid diagnostic .lby examples
- RELEASE_NOTES.md: release notes, verification evidence, and known limitations
- install.cmd / install.ps1: optional user PATH setup
- uninstall.cmd / uninstall.ps1: optional user PATH cleanup

Quick start:
1. Open PowerShell in this directory.
2. Run: .\bin\lullaby.exe --version
3. Run: .\bin\lullaby.exe docs
4. Run: .\bin\lullaby.exe examples
5. Run: .\bin\lullaby.exe check .\examples\valid\calculator.lby
6. Run: .\bin\lullaby.exe run .\examples\valid\calculator.lby
7. Run: .\bin\lullaby.exe compile --optimize full -o .\examples\valid\calculator.lbc .\examples\valid\calculator.lby
8. Run: .\bin\lullaby.exe build --optimize full -o .\examples\valid\calculator-build.lbc .\examples\valid\calculator.lby
9. Run: .\bin\lullaby.exe inspect .\examples\valid\calculator.lbc
10. Run: .\bin\lullaby.exe run .\examples\valid\calculator.lbc

Optional PATH setup:
- Run .\install.cmd from this directory to add bin\lullaby.exe to your user PATH.
- Open a new shell, then run: lullaby --version
- Run .\uninstall.cmd from this directory to remove this package from your user PATH.

Checksum:
- The release process writes $PackageName.zip.sha256 beside the zip archive.
- Compare it with Get-FileHash before unpacking downloaded archives.
"@ | Set-Content -Path (Join-Path $PackageRoot "README.txt") -Encoding UTF8

    @"
package=$PackageName
commit=$Commit
binary=bin\lullaby.exe
docs=docs\index.html
release_notes=RELEASE_NOTES.md
installer=install.cmd
uninstaller=uninstall.cmd
license_status=$LicenseStatus
"@ | Set-Content -Path (Join-Path $PackageRoot "VERSION.txt") -Encoding UTF8

    Remove-Item -LiteralPath $ArchivePath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $ChecksumPath -Force -ErrorAction SilentlyContinue
    Compress-Archive -Path (Join-Path $PackageRoot "*") -DestinationPath $ArchivePath -Force
    $ArchiveHash = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    "$ArchiveHash  $PackageName.zip" | Set-Content -Path $ChecksumPath -Encoding ASCII

    Write-Output "package: $PackageRoot"
    Write-Output "archive: $ArchivePath"
    Write-Output "sha256: $ChecksumPath"
} finally {
    Pop-Location
}
