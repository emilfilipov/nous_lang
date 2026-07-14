# CSV-integer parse+aggregate benchmark - a small realistic data transform.
# Complements the single-feature fib/loop/array/strscan harnesses with a program
# that exercises more than one feature at once: it walks a fixed ASCII string of
# comma-separated decimal integers, parses each field with a digit accumulator
# (`cur = cur*10 + (c-48)`, flushing on any non-digit), and sums the parsed
# integers - linear `for c in s` char iteration + per-character branching +
# multiply/add + running accumulation. Repeated `reps` times; the string (one
# immutable heap record, allocated once) stays fixed, keeping it native-eligible.
# C + native at NativeReps, interpreters at InterpReps; reports ns per character
# processed. (Lullaby chars are Unicode scalar values, so each step decodes
# UTF-8, a per-char cost the C byte-walk reference does not pay; on ASCII the
# results are identical and the gap is honest native-string coverage.)
#
# The string literal here is byte-for-byte identical to benchmarks/csvsum.c; if
# they drift, the interpreter rows MISMATCH against the C-derived expected value.
#
#   powershell -ExecutionPolicy Bypass -File benchmarks/run_csvsum.ps1
[CmdletBinding()]
param([long]$NativeReps = 500000, [long]$InterpReps = 100000, [int]$Reps = 5, [string]$Label = "csvsum")
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$bench = $PSScriptRoot
$lb = Join-Path $root 'target\release\lullaby.exe'
if (-not (Test-Path $lb)) { throw "build release first: cargo build --release -p lullaby_cli" }

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

# Fixed data - MUST match the S string in csvsum.c (ASCII digits + commas).
$str = '12,345,6,7890,42,1,999,23,4567,8,90,123,4,56,7,808,11,222,3,44'
$innerL = $str.Length   # characters processed per repetition (for ns/char)

$tpl = @'
fn parse_sum s string -> i64
    let total i64 = 0
    let cur i64 = 0
    for c in s
        let d i64 = char_code(c)
        if d >= 48 and d <= 57
            cur = cur * 10 + (d - 48)
        else
            total = total + cur
            cur = 0
    total = total + cur
    total

fn main -> i64
    let s string = "__STR__"
    let acc i64 = 0
    let r i64 = 0
    while r < __REPS__
        acc = acc + parse_sum(s)
        r = r + 1
    acc
'@
$tpl = $tpl -replace '__STR__', $str
function Gen([long]$reps) {
    $p = Join-Path $bench "csvsum_$reps.lby"
    ($tpl -replace '__REPS__', $reps) | Set-Content -Encoding ASCII $p
    $p
}

# --- C reference (ground truth); reps via argv, one compile ---
Push-Location $bench; & cl /nologo /O2 /Fe:csvsum_c.exe csvsum.c | Out-Null; Pop-Location
$cexe = Join-Path $bench 'csvsum_c.exe'
$perScan = [long](& $cexe 1 | Select-Object -First 1)   # sum of one parse; acc = reps * perScan
function Expected([long]$reps) { return $perScan * $reps }

Write-Host "`n=== $Label : parse+sum a $innerL-char CSV integer string, x reps ; C+native @ reps=$NativeReps, interpreters @ reps=$InterpReps, best of $Reps ===`n"
$rows = @()
function Row($tier, $reps, $ms, $exp, $got) {
    $ops = $reps * $innerL
    $ns = ($ms * 1e6) / $ops
    $ok = ($exp -eq $got)
    $rows += [pscustomobject]@{ Tier = $tier; ms = [math]::Round($ms, 2); 'ns/char' = [math]::Round($ns, 4); ok = $ok }
    "{0,-16} {1,10:N2} ms {2,9:N4} ns/char  {3}" -f $tier, $ms, $ns, $(if ($ok) { 'ok' } else { "MISMATCH($got)" }) | Write-Host
}

# C - timed at NativeReps
$cGot = [long](& $cexe $NativeReps | Select-Object -First 1)
Row 'C (cl /O2)' $NativeReps (Best { & $cexe $NativeReps } $Reps) (Expected $NativeReps) $cGot

# native - result overflows a 32-bit exit code at NativeReps, so correctness is
# spot-checked at the largest reps whose result still fits a signed 32-bit exit
# code (< 2^31); timing is at NativeReps. Native string/parse correctness is also
# covered by `cargo test --all`.
$spotReps = [long][math]::Min($NativeReps, [math]::Floor(2e9 / $perScan))
if ($spotReps -lt 1) { $spotReps = 1 }
$spotLby = Gen $spotReps
$spotExe = Join-Path $bench "csvsum_native_$spotReps.exe"
& $lb native -o $spotExe $spotLby | Out-Null
& $spotExe | Out-Null; $spotOk = ($LASTEXITCODE -eq [int](Expected $spotReps))
$natLby = Gen $NativeReps
$natexe = Join-Path $bench "csvsum_native_$NativeReps.exe"
& $lb native -o $natexe $natLby | Out-Null
if (Test-Path $natexe) {
    $ms = Best { & $natexe } $Reps
    $ns = ($ms * 1e6) / ($NativeReps * $innerL)
    $status = if ($spotOk) { "ok (spot-checked @ reps=$spotReps)" } else { 'SPOT MISMATCH' }
    "{0,-16} {1,10:N2} ms {2,9:N4} ns/char  {3}" -f 'lullaby native', $ms, $ns, $status | Write-Host
    $rows += [pscustomobject]@{ Tier = 'lullaby native'; ms = [math]::Round($ms, 2); 'ns/char' = [math]::Round($ns, 4); ok = $spotOk }
} else { Write-Host 'lullaby native: link failed' }

# interpreters - at InterpReps
$intLby = Gen $InterpReps
$intExp = Expected $InterpReps
foreach ($b in 'bytecode', 'ir', 'ast') {
    $opt = if ($b -eq 'ast') { @() } else { @('--optimize', 'full') }
    $got = [long](& $lb run --backend $b @opt $intLby | Select-Object -First 1)
    Row "lullaby $b" $InterpReps (Best { & $lb run --backend $b @opt $intLby } $Reps) $intExp $got
}
Write-Host ''; $rows | Format-Table -AutoSize
$rows | Export-Csv -NoTypeInformation -Encoding UTF8 (Join-Path $bench "results_csvsum_$Label.csv")
