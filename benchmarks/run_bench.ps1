# Lullaby performance baseline / regression harness.
#
# Measures a fixed workload (recursive fib) across every execution tier and a
# pure-C reference, normalizing to nanoseconds per fib-call so results at
# different depths are comparable. Imports the MSVC environment (vcvars64) so
# both `cl` and `lullaby native` can link. Best-of-K wall time per tier.
#
#   powershell -ExecutionPolicy Bypass -File benchmarks/run_bench.ps1
[CmdletBinding()]
param(
    [int]$NativeN = 40,   # C + lullaby native (compiled tiers)
    [int]$InterpN = 30,   # ast / ir / bytecode (interpreted tiers)
    [int]$Reps = 5,
    [string]$Label = "baseline"
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$bench = $PSScriptRoot
$lb = Join-Path $root 'target\release\lullaby.exe'
if (-not (Test-Path $lb)) { throw "build release first: cargo build --release -p lullaby_cli" }

# fib(n) makes 2*fib(n+1)-1 calls; use that to normalize to ns/call.
function Fib([long]$n) { $a=[long]0; $b=[long]1; for($i=0;$i -lt $n;$i++){ $t=$a+$b; $a=$b; $b=$t }; return $a }
function Calls([long]$n) { return 2*(Fib ($n+1)) - 1 }

# --- import vcvars64 so cl + native linking work ---
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^(.*?)=(.*)$') {
        [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
    }
}
Write-Host "[env] MSVC imported (LIB set: $([bool]$env:LIB))"

function Best-Time([scriptblock]$run, [int]$reps) {
    $min = [double]::MaxValue
    for ($i = 0; $i -lt $reps; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        & $run | Out-Null
        $sw.Stop()
        if ($sw.Elapsed.TotalMilliseconds -lt $min) { $min = $sw.Elapsed.TotalMilliseconds }
    }
    return $min
}

$rows = @()
function Add-Row($tier, $n, $ms, $expected, $got) {
    $calls = Calls $n
    $nsPerCall = ($ms * 1e6) / $calls
    $ok = ($expected -eq $got)
    $script:rows += [pscustomobject]@{
        Tier=$tier; N=$n; ms=[math]::Round($ms,2); 'ns/call'=[math]::Round($nsPerCall,2); ok=$ok
    }
    "{0,-16} N={1,-3} {2,9:N2} ms {3,8:N2} ns/call  {4}" -f $tier,$n,$ms,$nsPerCall,$(if($ok){'ok'}else{"MISMATCH($got)"}) | Write-Host
}

# --- generate depth-specific Lullaby sources ---
$fibTemplate = @'
fn fib n i64 -> i64
    if n < 2
        return n
    fib(n - 1) + fib(n - 2)

fn main -> i64
    fib(__N__)
'@
$natLby = Join-Path $bench "fib_$NativeN.lby"; ($fibTemplate -replace '__N__', $NativeN) | Set-Content -Encoding ASCII $natLby
$intLby = Join-Path $bench "fib_$InterpN.lby"; ($fibTemplate -replace '__N__', $InterpN) | Set-Content -Encoding ASCII $intLby
$expNat = (Fib $NativeN); $expInt = (Fib $InterpN)

Write-Host "`n=== $Label : fib native/C @ N=$NativeN, interpreters @ N=$InterpN, best of $Reps ===`n"

# --- C reference ---
$cexe = Join-Path $bench 'fib_c.exe'
Push-Location $bench
& cl /nologo /O2 /Fe:fib_c.exe fib.c | Out-Null
Pop-Location
$got = (& $cexe $NativeN | Select-Object -First 1)
Add-Row 'C (cl /O2)' $NativeN (Best-Time { & $cexe $NativeN } $Reps) $expNat $got

# --- lullaby native ---
$natexe = Join-Path $bench "fib_native_$NativeN.exe"
& $lb native -o $natexe $natLby | Out-Null
if (Test-Path $natexe) {
    & $natexe | Out-Null; $got = $LASTEXITCODE   # native carries result via the Windows 32-bit exit code
    Add-Row 'lullaby native' $NativeN (Best-Time { & $natexe } $Reps) $expNat $got
} else { Write-Host 'lullaby native: link failed' }

# --- interpreter tiers (--optimize applies only to ir/bytecode) ---
foreach ($b in 'bytecode','ir','ast') {
    $opt = if ($b -eq 'ast') { @() } else { @('--optimize','full') }
    $got = (& $lb run --backend $b @opt $intLby | Select-Object -First 1)
    Add-Row "lullaby $b" $InterpN (Best-Time { & $lb run --backend $b @opt $intLby } ([math]::Max(3,$Reps-2))) $expInt $got
}

Write-Host ''
$rows | Format-Table -AutoSize
# persist for before/after comparison
$out = Join-Path $bench "results_$Label.csv"
$rows | Export-Csv -NoTypeInformation -Encoding UTF8 $out
Write-Host "wrote $out"
