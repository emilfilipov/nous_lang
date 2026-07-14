# gcd (Euclid) benchmark - exercises the native inline `gcd` div-loop that the
# call-bound fib and loop-bound sum harnesses don't touch. Accumulates
# gcd(i, 1071) for i in 1..N. C reference is compiled with the same N via
# /DGCD_N. Reports ns per gcd call; every tier is correctness-checked against
# the C reference output at its own N.
[CmdletBinding()]
param([long]$NativeN = 50000000, [long]$InterpN = 2000000, [int]$Reps = 5, [string]$Label = "gcd")
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

$tpl = @'
fn main -> i64
    let acc i64 = 0
    let i i64 = 1
    while i < __N__
        acc = acc + gcd(i, 1071)
        i = i + 1
    acc
'@
$natLby = Join-Path $bench "gcd_$NativeN.lby"; ($tpl -replace '__N__', $NativeN) | Set-Content -Encoding ASCII $natLby
$intLby = Join-Path $bench "gcd_$InterpN.lby"; ($tpl -replace '__N__', $InterpN) | Set-Content -Encoding ASCII $intLby

# C reference (ground truth) at each N.
Push-Location $bench
& cl /nologo /O2 /DGCD_N=$NativeN /Fe:gcd_c_nat.exe gcdbench.c | Out-Null
& cl /nologo /O2 /DGCD_N=$InterpN /Fe:gcd_c_int.exe gcdbench.c | Out-Null
Pop-Location
$cNatExe = Join-Path $bench 'gcd_c_nat.exe'
$cIntExe = Join-Path $bench 'gcd_c_int.exe'
$expNat = [long](& $cNatExe | Select-Object -First 1)
$expInt = [long](& $cIntExe | Select-Object -First 1)

Write-Host "`n=== $Label : sum gcd(i,1071) i=1..N ; C+native @ N=$NativeN, interpreters @ N=$InterpN, best of $Reps ===`n"
$rows = @()
function Row($tier, $n, $ms, $exp, $got) {
    $ns = ($ms * 1e6) / $n
    $ok = ($exp -eq $got)
    $rows += [pscustomobject]@{ Tier = $tier; ms = [math]::Round($ms, 2); 'ns/gcd' = [math]::Round($ns, 3); ok = $ok }
    "{0,-16} {1,10:N2} ms {2,8:N3} ns/gcd  {3}" -f $tier, $ms, $ns, $(if ($ok) { 'ok' } else { "MISMATCH($got)" }) | Write-Host
}

# C
Row 'C (cl /O2)' $NativeN (Best { & $cNatExe } $Reps) $expNat $expNat

# native - the accumulated sum fits a 32-bit exit code at the default N
# (< 2^31), so native correctness is checked directly against the C output.
$natexe = Join-Path $bench "gcd_native_$NativeN.exe"
& $lb native -o $natexe $natLby | Out-Null
if (Test-Path $natexe) {
    & $natexe | Out-Null; $natGot = $LASTEXITCODE
    Row 'lullaby native' $NativeN (Best { & $natexe } $Reps) $expNat $natGot
} else { Write-Host 'lullaby native: link failed' }

# interpreters
foreach ($b in 'bytecode', 'ir', 'ast') {
    $opt = if ($b -eq 'ast') { @() } else { @('--optimize', 'full') }
    $got = [long](& $lb run --backend $b @opt $intLby | Select-Object -First 1)
    Row "lullaby $b" $InterpN (Best { & $lb run --backend $b @opt $intLby } $Reps) $expInt $got
}
Write-Host ''; $rows | Format-Table -AutoSize
$rows | Export-Csv -NoTypeInformation -Encoding UTF8 (Join-Path $bench "results_gcd_$Label.csv")
