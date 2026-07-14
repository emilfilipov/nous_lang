$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $ScriptDir "..")
$SearchRoots = @(
    (Join-Path $RepoRoot "README.md"),
    (Join-Path $RepoRoot "CLAUDE.md"),
    (Join-Path $RepoRoot "documents")
)
$StaleMarkers = @(
    "DELETED",
    "Clean start",
    "compiled_programming_languages_overview",
    "programming_paradigms",
    "top_programming_languages",
    "language_comparison_guide"
)

function Resolve-LocalTarget {
    param(
        [Parameter(Mandatory = $true)][string]$BaseDirectory,
        [Parameter(Mandatory = $true)][string]$RepositoryRoot,
        [Parameter(Mandatory = $true)][string]$Target
    )

    $WithoutAnchor = $Target -replace '#.*$', ''
    if ([string]::IsNullOrWhiteSpace($WithoutAnchor)) {
        return $null
    }

    if ($WithoutAnchor -match '^(documents|scripts|tests|examples|crates|README\.md|AGENTS\.md)[\\/]') {
        return Join-Path $RepositoryRoot $WithoutAnchor
    }

    return Join-Path $BaseDirectory $WithoutAnchor
}

$Errors = New-Object System.Collections.Generic.List[string]
$MarkdownFiles = Get-ChildItem -LiteralPath $SearchRoots -Recurse -Include *.md -File

foreach ($File in $MarkdownFiles) {
    $Text = Get-Content -LiteralPath $File.FullName -Raw
    if ($File.FullName.StartsWith((Join-Path $RepoRoot "documents"))) {
        foreach ($Marker in $StaleMarkers) {
            if ($Text.Contains($Marker)) {
                $Errors.Add("$($File.FullName): stale marker found: $Marker")
            }
        }
    }

    $BaseDirectory = $File.DirectoryName

    foreach ($Match in [regex]::Matches($Text, '\[[^\]]+\]\(([^)]+)\)')) {
        $Target = $Match.Groups[1].Value
        if ($Target -match '^(https?:|mailto:|#)') {
            continue
        }
        if ($Target -notmatch '\.(md|html|lullaby|lbc|ps1|cmd|py|toml|rs|txt)(#.*)?$') {
            continue
        }

        $Resolved = Resolve-LocalTarget -BaseDirectory $BaseDirectory -RepositoryRoot $RepoRoot -Target $Target
        if ($null -ne $Resolved -and -not (Test-Path -LiteralPath $Resolved)) {
            $Errors.Add("$($File.FullName): missing Markdown link target: $Target")
        }
    }

    foreach ($Match in [regex]::Matches($Text, '`([^`]+\.md)`')) {
        $Target = $Match.Groups[1].Value
        if ($Target -eq "RELEASE_NOTES.md") {
            continue
        }
        $Resolved = Resolve-LocalTarget -BaseDirectory $BaseDirectory -RepositoryRoot $RepoRoot -Target $Target
        if ($null -ne $Resolved -and -not (Test-Path -LiteralPath $Resolved)) {
            $Errors.Add("$($File.FullName): missing backticked Markdown reference: $Target")
        }
    }
}

if ($Errors.Count -gt 0) {
    foreach ($ErrorText in $Errors) {
        Write-Error $ErrorText
    }
    exit 1
}

Write-Output "markdown reference verification passed"
