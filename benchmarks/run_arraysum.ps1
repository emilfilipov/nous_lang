# Array-scan benchmark - reduction over a fixed array<i64>. Complements the
# call-bound fib and loop-bound sum harnesses with an ARRAY shape: a read-only
# scalar array is passed by fat pointer to a `scan` helper that sums it with the
# `for i: s += a[i]` reduction, repeated `reps` times so total work scales while
# the array (and therefore the native bump heap) stays fixed. C + native at
# NativeReps, interpreters at InterpReps; reports ns per element read.
#
# The array literal here is byte-for-byte identical to benchmarks/arraysum.c so
# the two are directly comparable; if they ever drift, the interpreter rows
# MISMATCH against the C-derived expected value and the run fails loudly.
#
#   powershell -ExecutionPolicy Bypass -File benchmarks/run_arraysum.ps1
[CmdletBinding()]
param([long]$NativeReps = 10000000, [long]$InterpReps = 200000, [int]$Reps = 5, [string]$Label = "arraysum")
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$bench = $PSScriptRoot
$lb = Join-Path $root 'target\release\lullaby.exe'
if (-not (Test-Path $lb)) { throw "build release first: cargo build --release -p lullaby_cli" }

# --- import vcvars64 so cl + native linking work ---
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^(.*?)=(.*)$') { [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process') }
}
if (-not $env:LIB) { throw "vcvars import failed (no LIB); vsPath='$vsPath'" }

function Best([scriptblock]$run, [int]$reps) {
    $min = [double]::MaxValue
    for ($i = 0; $i -lt $reps; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew(); & $run | Out-Null; $sw.Stop()
        if ($sw.Elapsed.TotalMilliseconds -lt $min) { $min = $sw.Elapsed.TotalMilliseconds }
    }
    $min
}

# Fixed data - MUST match the A[] array in arraysum.c.
$data = '3, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5, 8, 9, 7, 9, 3, 2, 3, 8, 4, 6, 2, 6, 4, 3, 3, 8, 3, 2, 7, 9, 5, 0, 2, 8, 8, 4, 1, 9, 7, 1, 6, 9, 3, 9, 9, 3, 7, 5, 1, 0, 5, 8, 2, 0, 9, 7, 4, 9, 4, 4, 5, 9, 2'
$innerL = ($data -split ',').Count   # elements read per repetition (for ns/element)

$tpl = @'
fn scan a array<i64> -> i64
    let s i64 = 0
    for i from 0 to len(a) - 1
        s += a[i]
    s

fn main -> i64
    let a array<i64> = [__DATA__]
    let acc i64 = 0
    let r i64 = 0
    while r < __REPS__
        acc = acc + scan(a)
        r = r + 1
    acc
'@
$tpl = $tpl -replace '__DATA__', $data
function Gen([long]$reps) {
    $p = Join-Path $bench "arraysum_$reps.lby"
    ($tpl -replace '__REPS__', $reps) | Set-Content -Encoding ASCII $p
    $p
}

# --- C reference (ground truth); reps via argv, one compile ---
Push-Location $bench; & cl /nologo /O2 /Fe:arraysum_c.exe arraysum.c | Out-Null; Pop-Location
$cexe = Join-Path $bench 'arraysum_c.exe'
$perScan = [long](& $cexe 1 | Select-Object -First 1)   # sum of one scan; acc = reps * perScan
function Expected([long]$reps) { return $perScan * $reps }

Write-Host "`n=== $Label : sum of $innerL-element array<i64>, x reps ; C+native @ reps=$NativeReps, interpreters @ reps=$InterpReps, best of $Reps ===`n"
$rows = @()
function Row($tier, $reps, $ms, $exp, $got, $note) {
    $ops = $reps * $innerL
    $ns = ($ms * 1e6) / $ops
    $ok = ($exp -eq $got)
    $rows += [pscustomobject]@{ Tier = $tier; ms = [math]::Round($ms, 2); 'ns/elem' = [math]::Round($ns, 4); ok = $ok }
    $status = if ($ok) { if ($note) { $note } else { 'ok' } } else { "MISMATCH($got)" }
    "{0,-16} {1,10:N2} ms {2,9:N4} ns/elem  {3}" -f $tier, $ms, $ns, $status | Write-Host
}

# C - timed at NativeReps
$cExp = Expected $NativeReps
$cGot = [long](& $cexe $NativeReps | Select-Object -First 1)
Row 'C (cl /O2)' $NativeReps (Best { & $cexe $NativeReps } $Reps) $cExp $cGot $null

# native - the accumulated result overflows a 32-bit exit code at NativeReps, so
# native correctness is spot-checked at the largest reps whose result still fits
# a signed 32-bit exit code (< 2^31), and timing is taken at NativeReps. Full
# native array correctness is also covered by `cargo test --all`.
$spotReps = [long][math]::Min($NativeReps, [math]::Floor(2e9 / $perScan))
if ($spotReps -lt 1) { $spotReps = 1 }
$spotLby = Gen $spotReps
$spotExe = Join-Path $bench "arraysum_native_$spotReps.exe"
& $lb native -o $spotExe $spotLby | Out-Null
& $spotExe | Out-Null; $spotOk = ($LASTEXITCODE -eq [int](Expected $spotReps))
$natLby = Gen $NativeReps
$natexe = Join-Path $bench "arraysum_native_$NativeReps.exe"
& $lb native -o $natexe $natLby | Out-Null
if (Test-Path $natexe) {
    $ms = Best { & $natexe } $Reps
    $ops = $NativeReps * $innerL; $ns = ($ms * 1e6) / $ops
    $status = if ($spotOk) { "ok (spot-checked @ reps=$spotReps)" } else { 'SPOT MISMATCH' }
    "{0,-16} {1,10:N2} ms {2,9:N4} ns/elem  {3}" -f 'lullaby native', $ms, $ns, $status | Write-Host
    $rows += [pscustomobject]@{ Tier = 'lullaby native'; ms = [math]::Round($ms, 2); 'ns/elem' = [math]::Round($ns, 4); ok = $spotOk }
} else { Write-Host 'lullaby native: link failed' }

# interpreters - at InterpReps
$intLby = Gen $InterpReps
$intExp = Expected $InterpReps
foreach ($b in 'bytecode', 'ir', 'ast') {
    $opt = if ($b -eq 'ast') { @() } else { @('--optimize', 'full') }
    $got = [long](& $lb run --backend $b @opt $intLby | Select-Object -First 1)
    Row "lullaby $b" $InterpReps (Best { & $lb run --backend $b @opt $intLby } $Reps) $intExp $got $null
}
Write-Host ''; $rows | Format-Table -AutoSize
$rows | Export-Csv -NoTypeInformation -Encoding UTF8 (Join-Path $bench "results_arraysum_$Label.csv")
