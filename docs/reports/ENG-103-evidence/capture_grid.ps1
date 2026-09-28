param(
    [Parameter(Mandatory = $true)][ValidateSet('sealed','canal','breach','closure','tunnel','basin','sensitivity')][string]$Scenario,
    [Parameter(Mandatory = $true)][ValidateSet(1,2)][int]$Scale,
    [ValidateSet(1,2)][int]$Refinement = 1,
    [Parameter(Mandatory = $true)][int]$Ticks,
    [string]$RunTag = '',
    [double]$Dt = 0.0166666666666667,
    [double]$PressureTolerance = 1e-8,
    [ValidateSet('jacobi','ic0')][string]$Preconditioner = 'jacobi',
    [switch]$PressureDiagnostics,
    [switch]$FractionDiagnostics,
    [switch]$StageDiagnostics,
    [switch]$AllocationDiagnostics
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
$exe = Join-Path $repoRoot 'target\release\grid-fluid-scenario.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    throw "Missing release binary: $exe. Build it with cargo build --release --locked --offline -p spall_fluid --bin grid-fluid-scenario first."
}

$tagPart = if ([string]::IsNullOrWhiteSpace($RunTag)) { '' } else { "-$RunTag" }
$stem = "grid-$Scenario-scale$Scale-r$Refinement-${Ticks}ticks$tagPart"
$jsonl = Join-Path $PSScriptRoot "$stem.jsonl"
$stderr = Join-Path $PSScriptRoot "$stem.stderr.txt"
$arguments = @('--scenario', $Scenario, '--scale', "$Scale", '--refinement', "$Refinement", '--ticks', "$Ticks", '--dt', "$Dt", '--pressure-tolerance', "$PressureTolerance", '--preconditioner', $Preconditioner)
if ($PressureDiagnostics) { $arguments += '--pressure-diagnostics' }
if ($FractionDiagnostics) { $arguments += '--fraction-diagnostics' }
if ($StageDiagnostics) { $arguments += '--stage-diagnostics' }
if ($AllocationDiagnostics) { $arguments += '--allocation-diagnostics' }
$process = Start-Process -FilePath $exe -ArgumentList $arguments -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput $jsonl -RedirectStandardError $stderr
$peakWorkingSet = 0L
$samples = 0
while (-not $process.HasExited) {
    $process.Refresh()
    if ($process.WorkingSet64 -gt $peakWorkingSet) { $peakWorkingSet = $process.WorkingSet64 }
    $samples++
    Start-Sleep -Milliseconds 10
}
$process.WaitForExit()
if ($process.ExitCode -ne 0) { throw "Scenario process exited with code $($process.ExitCode); see $stderr" }
if ($process.WorkingSet64 -gt $peakWorkingSet) { $peakWorkingSet = $process.WorkingSet64 }
$memoryRecord = [ordered]@{
    type = 'process_memory_sample'
    process_working_set_peak_bytes = $peakWorkingSet
    samples = $samples
    interval_ms = 10
    executable = 'target/release/grid-fluid-scenario.exe'
    scenario = $Scenario
    scale = $Scale
    ticks = $Ticks
    preconditioner = $Preconditioner
    fraction_diagnostics = [bool]$FractionDiagnostics
    stage_diagnostics = [bool]$StageDiagnostics
    allocation_diagnostics = [bool]$AllocationDiagnostics
}
Add-Content -LiteralPath $jsonl -Value ($memoryRecord | ConvertTo-Json -Compress)
Write-Output "JSONL=$jsonl"
Write-Output "PEAK_WORKING_SET_BYTES=$peakWorkingSet"
Write-Output "SAMPLES=$samples"
