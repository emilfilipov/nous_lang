# Cross-language performance benchmark: count_primes_below(300000) (trial
# division). Compiles C/C++/Rust/Lullaby-native and times all five externally
# (whole-program wall time, best-of-N; Python is CPython). Every language must
# print/return 25997 (correctness gate) before its timing counts.
[CmdletBinding()]
param([int]$Reps = 5, [int]$PyReps = 1)
$ErrorActionPreference = 'Stop'
$cross = $PSScriptRoot
$repo = Split-Path (Split-Path $cross)
$lb = Join-Path $repo 'target\release\lullaby.exe'
$py = 'C:\Users\emil\AppData\Local\Programs\Python\Python314\python.exe'
$EXPECT = 25997

# MSVC env for cl + lullaby-native linking.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
cmd /c "`"$vsPath\VC\Auxiliary\Build\vcvars64.bat`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^(.*?)=(.*)$') { [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process') }
}
if (-not $env:LIB) { throw 'vcvars import failed' }

function Best([scriptblock]$run, [int]$reps) {
    $min = [double]::MaxValue
    for ($i = 0; $i -lt $reps; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew(); & $run | Out-Null; $sw.Stop()
        if ($sw.Elapsed.TotalMilliseconds -lt $min) { $min = $sw.Elapsed.TotalMilliseconds }
    }
    $min
}

$rows = @()
function Row($lang, $ms, $ok) {
    $script:rows += [pscustomobject]@{ Language = $lang; ms = [math]::Round($ms, 1); ok = $ok }
}

Write-Host "=== building ==="
$cexe = Join-Path $cross 'c\bench_c.exe'
& cl /nologo /O2 /Fe:$cexe (Join-Path $cross 'c\bench_primes.c') | Out-Null
$cppexe = Join-Path $cross 'cpp\bench_cpp.exe'
& cl /nologo /O2 /EHsc /std:c++17 /Fe:$cppexe (Join-Path $cross 'cpp\bench_primes.cpp') | Out-Null
$rustexe = Join-Path $cross 'rust\bench_rust.exe'
& rustc -O -o $rustexe (Join-Path $cross 'rust\bench_primes.rs') 2>$null | Out-Null
$lbexe = Join-Path $cross 'lullaby\bench_lb.exe'
& $lb native -o $lbexe (Join-Path $cross 'lullaby\bench_primes.lby') | Out-Null

Write-Host "=== running (expect $EXPECT) ===`n"
# C / C++ / Rust: stdout
foreach ($t in @(@('C (cl /O2)', $cexe), @('C++ (cl /O2)', $cppexe), @('Rust (rustc -O)', $rustexe))) {
    $got = (& $t[1] | Select-Object -First 1).Trim()
    Row $t[0] (Best { & $t[1] } $Reps) ($got -eq "$EXPECT")
}
# Lullaby native: result via exit code
& $lbexe | Out-Null; $lbgot = $LASTEXITCODE
Row 'Lullaby (native)' (Best { & $lbexe } $Reps) ($lbgot -eq $EXPECT)
# Python (CPython) — slow, fewer reps
$pyfile = Join-Path $cross 'python\bench_primes.py'
$pygot = (& $py $pyfile | Select-Object -First 1).Trim()
Row 'Python (CPython)' (Best { & $py $pyfile } $PyReps) ($pygot -eq "$EXPECT")

$cms = ($rows | Where-Object Language -like 'C (*').ms
$rows | ForEach-Object {
    $_ | Add-Member -NotePropertyName 'vsC' -NotePropertyValue ([math]::Round($_.ms / $cms, 2)) -PassThru
} | Format-Table -AutoSize
