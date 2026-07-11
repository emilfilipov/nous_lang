# Compute-bound loop benchmark (sum 0..N) - complements the call-bound fib
# harness. Exercises the fused while-condition and the i+1 immediate fold.
# C + native at NativeN; interpreters at InterpN. Reports ns/iteration.
[CmdletBinding()]
param([long]$NativeN = 1000000000, [long]$InterpN = 10000000, [int]$Reps = 3, [string]$Label = "loop")
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$bench = $PSScriptRoot
$lb = Join-Path $root 'target\release\lullaby.exe'

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
function Sum([long]$n) { return ($n - 1) * $n / 2 }  # 0+1+..+(n-1)

$tpl = @'
fn main -> i64
    let acc i64 = 0
    let i i64 = 0
    while i < __N__
        acc = acc + i
        i = i + 1
    acc
'@
$natLby = Join-Path $bench "loop_$NativeN.lby"; ($tpl -replace '__N__', $NativeN) | Set-Content -Encoding ASCII $natLby
$intLby = Join-Path $bench "loop_$InterpN.lby"; ($tpl -replace '__N__', $InterpN) | Set-Content -Encoding ASCII $intLby

Write-Host "`n=== $Label : sum 0..N ; C+native @ N=$NativeN, interpreters @ N=$InterpN, best of $Reps ===`n"
$rows = @()
function Row($tier, $n, $ms, $exp, $got) {
    $ns = ($ms * 1e6) / $n
    $rows += [pscustomobject]@{ Tier=$tier; ms=[math]::Round($ms,2); 'ns/iter'=[math]::Round($ns,3); ok=($exp -eq $got) }
    "{0,-16} {1,10:N2} ms {2,8:N3} ns/iter  {3}" -f $tier,$ms,$ns,$(if($exp -eq $got){'ok'}else{"MISMATCH($got)"}) | Write-Host
}

# C
Push-Location $bench; & cl /nologo /O2 /Fe:loop_c.exe loopsum.c | Out-Null; Pop-Location
$cexe = Join-Path $bench 'loop_c.exe'
Row 'C (cl /O2)' $NativeN (Best { & $cexe } $Reps) (Sum $NativeN) (& $cexe | Select-Object -First 1)

# native — the loop sum overflows the 32-bit process exit code at large N, so
# correctness is spot-checked at a small N (fits i32) and timing is taken at
# NativeN. Full native loop correctness is covered by `cargo test --all`.
$smallN = 60000
$smallLby = Join-Path $bench "loop_$smallN.lby"; ($tpl -replace '__N__', $smallN) | Set-Content -Encoding ASCII $smallLby
$smallExe = Join-Path $bench "loop_native_$smallN.exe"
& $lb native -o $smallExe $smallLby | Out-Null
& $smallExe | Out-Null; $smallOk = ($LASTEXITCODE -eq [int](Sum $smallN))
$natexe = Join-Path $bench "loop_native_$NativeN.exe"
& $lb native -o $natexe $natLby | Out-Null
if (Test-Path $natexe) {
    $ms = Best { & $natexe } $Reps
    $ns = ($ms * 1e6) / $NativeN
    "{0,-16} {1,10:N2} ms {2,8:N3} ns/iter  {3}" -f 'lullaby native', $ms, $ns, $(if ($smallOk) { 'ok (small-N verified)' } else { 'SMALL-N MISMATCH' }) | Write-Host
    $rows += [pscustomobject]@{ Tier='lullaby native'; ms=[math]::Round($ms,2); 'ns/iter'=[math]::Round($ns,3); ok=$smallOk }
} else { Write-Host 'lullaby native: link failed' }

# interpreters
foreach ($b in 'bytecode','ir','ast') {
    $opt = if ($b -eq 'ast') { @() } else { @('--optimize','full') }
    $got = (& $lb run --backend $b @opt $intLby | Select-Object -First 1)
    Row "lullaby $b" $InterpN (Best { & $lb run --backend $b @opt $intLby } $Reps) (Sum $InterpN) $got
}
Write-Host ''; $rows | Format-Table -AutoSize
$rows | Export-Csv -NoTypeInformation -Encoding UTF8 (Join-Path $bench "results_loop_$Label.csv")
