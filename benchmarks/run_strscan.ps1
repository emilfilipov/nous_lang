# String char-scan benchmark - a checksum over a fixed ASCII string. Complements
# the numeric fib/loop/array harnesses with a STRING shape: each repetition walks
# the string with `for c in s: sum += char_code(c)`, repeated `reps` times so
# total work scales while the string (one immutable heap record, allocated once)
# stays fixed - which is what keeps it native-eligible. C + native at NativeReps,
# interpreters at InterpReps; reports ns per character scanned.
#
# `for c in s` is the LINEAR char-iteration idiom (O(n) per scan); it is not the
# same as `s[i]` indexing, which is O(i) per access on a UTF-8 string (an index
# walks from the start to find the i-th code point) and would make a scan O(n^2).
# Note also: Lullaby chars are Unicode scalar values, so each step decodes UTF-8,
# whereas the C reference walks raw bytes - on an ASCII string the checksums are
# identical, but native pays a real per-character decode cost the C byte-walk
# does not. That gap is exactly the honest native-string coverage this workload
# exists to track for future optimization.
#
# NOTE on the accumulation *build* idiom (`s = s + "a"` in a loop): Lullaby
# strings are immutable and the native bump heap has no reclamation, so a build
# loop orphans O(n^2) bytes and exhausts the fixed native heap past ~1000 chars.
# The build idiom therefore does NOT scale on the native backend; this workload
# measures the SCAN, which allocates nothing per iteration and scales cleanly.
#
# The string literal here is byte-for-byte identical to benchmarks/strscan.c; if
# they drift, the interpreter rows MISMATCH against the C-derived expected value.
#
#   powershell -ExecutionPolicy Bypass -File benchmarks/run_strscan.ps1
[CmdletBinding()]
param([long]$NativeReps = 300000, [long]$InterpReps = 100000, [int]$Reps = 5, [string]$Label = "strscan")
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

# Fixed data - MUST match the S string in strscan.c (ASCII only).
$str = 'the quick brown fox jumps over the lazy dog while 12345 sheep leap'
$innerL = $str.Length   # characters scanned per repetition (for ns/char)

$tpl = @'
fn checksum s string -> i64
    let sum i64 = 0
    for c in s
        sum = sum + char_code(c)
    sum

fn main -> i64
    let s string = "__STR__"
    let acc i64 = 0
    let r i64 = 0
    while r < __REPS__
        acc = acc + checksum(s)
        r = r + 1
    acc
'@
$tpl = $tpl -replace '__STR__', $str
function Gen([long]$reps) {
    $p = Join-Path $bench "strscan_$reps.lby"
    ($tpl -replace '__REPS__', $reps) | Set-Content -Encoding ASCII $p
    $p
}

# --- C reference (ground truth); reps via argv, one compile ---
Push-Location $bench; & cl /nologo /O2 /Fe:strscan_c.exe strscan.c | Out-Null; Pop-Location
$cexe = Join-Path $bench 'strscan_c.exe'
$perScan = [long](& $cexe 1 | Select-Object -First 1)   # checksum of one scan; acc = reps * perScan
function Expected([long]$reps) { return $perScan * $reps }

Write-Host "`n=== $Label : char-code checksum over a $innerL-char string, x reps ; C+native @ reps=$NativeReps, interpreters @ reps=$InterpReps, best of $Reps ===`n"
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
# code (< 2^31); timing is at NativeReps. Native string-scan correctness is also
# covered by `cargo test --all`.
$spotReps = [long][math]::Min($NativeReps, [math]::Floor(2e9 / $perScan))
if ($spotReps -lt 1) { $spotReps = 1 }
$spotLby = Gen $spotReps
$spotExe = Join-Path $bench "strscan_native_$spotReps.exe"
& $lb native -o $spotExe $spotLby | Out-Null
& $spotExe | Out-Null; $spotOk = ($LASTEXITCODE -eq [int](Expected $spotReps))
$natLby = Gen $NativeReps
$natexe = Join-Path $bench "strscan_native_$NativeReps.exe"
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
$rows | Export-Csv -NoTypeInformation -Encoding UTF8 (Join-Path $bench "results_strscan_$Label.csv")
