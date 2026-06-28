param(
    [string]$PackageName = "lullaby-alpha1-windows-x64"
)

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $false

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $ScriptDir "..")
$PackageRoot = Join-Path $RepoRoot "dist\$PackageName"
$ArchivePath = Join-Path $RepoRoot "dist\$PackageName.zip"
$ChecksumPath = "$ArchivePath.sha256"
$Lullaby = Join-Path $PackageRoot "bin\lullaby.exe"
$Example = Join-Path $PackageRoot "examples\valid\calculator.lullaby"
$InvalidExample = Join-Path $PackageRoot "examples\invalid\type_mismatch.lullaby"
$Artifact = Join-Path $PackageRoot "examples\valid\calculator.lbc"
$BuildArtifact = Join-Path $PackageRoot "examples\valid\calculator-build.lbc"
$InstallScript = Join-Path $PackageRoot "install.ps1"
$UninstallScript = Join-Path $PackageRoot "uninstall.ps1"

Push-Location $RepoRoot
try {
    cargo fmt --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt --check failed" }
    cargo test --all
    if ($LASTEXITCODE -ne 0) { throw "cargo test --all failed" }
    cargo clippy --all-targets --all-features -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed" }
    python offline_docs\verify_offline_docs.py
    if ($LASTEXITCODE -ne 0) { throw "offline docs verification failed" }
    python offline_docs\generate_offline_docs.py
    if ($LASTEXITCODE -ne 0) { throw "generated offline docs build failed" }
    python offline_docs\verify_offline_docs.py target\offline_docs\index.html --profile generated
    if ($LASTEXITCODE -ne 0) { throw "generated offline docs verification failed" }
    & (Join-Path $ScriptDir "verify_markdown_refs.ps1")
    if ($LASTEXITCODE -ne 0) { throw "markdown reference verification failed" }

    & (Join-Path $ScriptDir "package_windows_portable.ps1") -PackageName $PackageName
    python scripts\package_portable.py --package-name "$PackageName-portable" --skip-build --verify
    if ($LASTEXITCODE -ne 0) { throw "cross-platform portable package verification failed" }

    if (-not (Test-Path -LiteralPath $Lullaby)) {
        throw "packaged lullaby.exe not found: $Lullaby"
    }
    if (-not (Test-Path -LiteralPath (Join-Path $PackageRoot "docs\index.html"))) {
        throw "packaged offline docs not found"
    }
    if (-not (Test-Path -LiteralPath $Example)) {
        throw "packaged example not found: $Example"
    }
    if (-not (Test-Path -LiteralPath $InvalidExample)) {
        throw "packaged invalid example not found: $InvalidExample"
    }
    if (-not (Test-Path -LiteralPath (Join-Path $PackageRoot "RELEASE_NOTES.md"))) {
        throw "packaged release notes not found"
    }
    if (-not (Test-Path -LiteralPath $ArchivePath)) {
        throw "package archive not found: $ArchivePath"
    }
    if (-not (Test-Path -LiteralPath $ChecksumPath)) {
        throw "package checksum not found: $ChecksumPath"
    }
    if (-not (Test-Path -LiteralPath $InstallScript)) {
        throw "packaged install.ps1 not found"
    }
    if (-not (Test-Path -LiteralPath $UninstallScript)) {
        throw "packaged uninstall.ps1 not found"
    }
    if (-not (Test-Path -LiteralPath (Join-Path $PackageRoot "install.cmd"))) {
        throw "packaged install.cmd not found"
    }
    if (-not (Test-Path -LiteralPath (Join-Path $PackageRoot "uninstall.cmd"))) {
        throw "packaged uninstall.cmd not found"
    }

    & $Lullaby --version
    if ($LASTEXITCODE -ne 0) { throw "lullaby --version failed" }
    & $Lullaby docs
    if ($LASTEXITCODE -ne 0) { throw "lullaby docs failed" }
    & $Lullaby examples
    if ($LASTEXITCODE -ne 0) { throw "lullaby examples failed" }
    & $Lullaby check $Example
    if ($LASTEXITCODE -ne 0) { throw "lullaby check failed: $Example" }
    & $Lullaby run $Example
    if ($LASTEXITCODE -ne 0) { throw "lullaby run failed: $Example" }
    & $Lullaby check $InvalidExample
    if ($LASTEXITCODE -eq 0) {
        throw "invalid example unexpectedly passed check: $InvalidExample"
    }
    Remove-Item -LiteralPath $Artifact -Force -ErrorAction SilentlyContinue
    & $Lullaby compile --optimize alpha -o $Artifact $Example
    if ($LASTEXITCODE -ne 0) { throw "lullaby compile failed: $Example" }
    Remove-Item -LiteralPath $BuildArtifact -Force -ErrorAction SilentlyContinue
    & $Lullaby build --optimize alpha -o $BuildArtifact $Example
    if ($LASTEXITCODE -ne 0) { throw "lullaby build failed: $Example" }
    & $Lullaby inspect $Artifact
    if ($LASTEXITCODE -ne 0) { throw "lullaby inspect failed: $Artifact" }
    & $Lullaby run $Artifact
    if ($LASTEXITCODE -ne 0) { throw "lullaby run failed: $Artifact" }
    powershell -ExecutionPolicy Bypass -File $InstallScript -DryRun
    if ($LASTEXITCODE -ne 0) { throw "install.ps1 dry-run failed" }
    powershell -ExecutionPolicy Bypass -File $UninstallScript -DryRun
    if ($LASTEXITCODE -ne 0) { throw "uninstall.ps1 dry-run failed" }

    $ExpectedChecksum = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $ChecksumText = (Get-Content -LiteralPath $ChecksumPath -Raw).Trim()
    if ($ChecksumText -ne "$ExpectedChecksum  $PackageName.zip") {
        throw "checksum mismatch in $ChecksumPath"
    }

    Write-Output "release verification passed: $PackageRoot"
} finally {
    Pop-Location
}
