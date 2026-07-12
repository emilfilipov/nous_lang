# Whole-corpus COMPUTE performance harness. Turns every corpus category into a hot
# workload (gen_perf_driver.py wraps its functions in an N-iteration loop, folding
# results, no I/O) and times all three interpreter tiers on real compute across the
# whole 26-category / 434-function set — not just the fib/loop/primes micros.
#
# Per category N is auto-calibrated so the AST run does ~300 ms of work above the
# parse+launch floor; the reported metric is per-iteration compute (floor removed,
# N divided out), so categories and tiers compare directly. Native is excluded
# here: the hot driver's main uses strings (len/to_string), so it is not
# i64-scalar-native-eligible — native compute lives in the fib/loop/primes harness.
[CmdletBinding()]
param([int]$Reps = 5, [double]$TargetMs = 300)
$ErrorActionPreference = 'Continue'
$cross = $PSScriptRoot
$repo = Split-Path (Split-Path $cross)
$lb = Join-Path $repo 'target\release\lullaby.exe'
$py = 'C:\Users\emil\AppData\Local\Programs\Python\Python314\python.exe'
$gen = Join-Path $cross 'gen_perf_driver.py'
if (-not (Test-Path $lb)) { throw "build release first: cargo build --release -p lullaby_cli" }

function Best([string]$backend, [string]$file, [int]$reps) {
    $min = [double]::MaxValue
    for ($i = 0; $i -lt $reps; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        & $lb run --backend $backend $file *> $null
        $sw.Stop()
        if ($sw.Elapsed.TotalMilliseconds -lt $min) { $min = $sw.Elapsed.TotalMilliseconds }
    }
    $min
}

# Parse+launch floor (empty program).
$floorLby = Join-Path $env:TEMP 'corpus_perf_floor.lby'
"fn main -> i64`n    0" | Set-Content -Encoding ASCII $floorLby
$floor = ([double[]](1..5 | ForEach-Object { (Best 'ast' $floorLby 1) }) | Measure-Object -Minimum).Minimum

$cats = Get-ChildItem (Join-Path $cross 'corpus') -Directory | Sort-Object Name
Write-Host ("`n=== whole-corpus COMPUTE perf: ns per hot iteration (floor {0:N1} ms removed), best-of-{1} ===`n" -f $floor, $Reps)
Write-Host ("{0,-20} {1,10} {2,10} {3,10}   {4,7}" -f "category", "ast(ns)", "ir(ns)", "bc(ns)", "N")
Write-Host ("-" * 64)

$agg = @{ ast = 0.0; ir = 0.0; bytecode = 0.0 }
$logIr = 0.0; $logBc = 0.0; $ratioN = 0   # geomean of per-category ir/ast, bc/ast
foreach ($cat in $cats) {
    $src = Join-Path $cat.FullName 'lullaby.lby'
    $hot = Join-Path $env:TEMP ("hot_" + $cat.Name + ".lby")
    # Calibrate N: probe at N=64, scale so AST work ~= TargetMs.
    & $py $gen $src 64 $hot *> $null
    $probe = Best 'ast' $hot $Reps
    $perIterMs = [Math]::Max(0.0005, ($probe - $floor) / 64.0)
    $n = [int][Math]::Max(1, [Math]::Round($TargetMs / $perIterMs))
    & $py $gen $src $n $hot *> $null

    $ast = Best 'ast' $hot $Reps
    $ir = Best 'ir' $hot $Reps
    $bc = Best 'bytecode' $hot $Reps
    $astNs = ([Math]::Max(0, $ast - $floor)) * 1e6 / $n
    $irNs = ([Math]::Max(0, $ir - $floor)) * 1e6 / $n
    $bcNs = ([Math]::Max(0, $bc - $floor)) * 1e6 / $n
    # Aggregate excludes combinatorics (a huge exponential outlier that would
    # dominate the sum); the geomean of per-category ratios includes all categories.
    if ($cat.Name -ne 'combinatorics') { $agg.ast += $astNs; $agg.ir += $irNs; $agg.bytecode += $bcNs }
    if ($astNs -gt 0) { $logIr += [Math]::Log($irNs / $astNs); $logBc += [Math]::Log($bcNs / $astNs); $ratioN++ }
    Write-Host ("{0,-20} {1,10:N0} {2,10:N0} {3,10:N0}   {4,7}" -f $cat.Name, $astNs, $irNs, $bcNs, $n)
    Remove-Item $hot -ErrorAction SilentlyContinue
}
Write-Host ("-" * 64)
Write-Host ("{0,-20} {1,10:N0} {2,10:N0} {3,10:N0}" -f "SUM ex-combinatorics", $agg.ast, $agg.ir, $agg.bytecode)
Write-Host ""
Write-Host ("Aggregate (25 cats, ex-combinatorics): ast {0:N0} ns, ir {1:P0} of ast, bytecode {2:P0} of ast" -f `
    $agg.ast, ($agg.ir / $agg.ast), ($agg.bytecode / $agg.ast))
Write-Host ("Geomean of per-category ratios (all 26): ir {0:P1} of ast, bytecode {1:P1} of ast" -f `
    [Math]::Exp($logIr / $ratioN), [Math]::Exp($logBc / $ratioN))
