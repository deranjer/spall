param(
    [Parameter(Mandatory = $true)][string]$Name,
    [Parameter(Mandatory = $true)][string[]]$ScenarioArgs
)

$exePath = Join-Path $PSScriptRoot '..\..\..\target\release\fluid-scenario.exe'
$outPath = Join-Path $PSScriptRoot "$Name.jsonl"
$errPath = Join-Path $PSScriptRoot "$Name.stderr.txt"
$process = Start-Process -FilePath $exePath -ArgumentList $ScenarioArgs -PassThru `
    -RedirectStandardOutput $outPath -RedirectStandardError $errPath -NoNewWindow
$peakWorkingSet = 0L
$sampleCount = 0

do {
    $sample = Get-Process -Id $process.Id -ErrorAction SilentlyContinue
    if ($sample) {
        $peakWorkingSet = [Math]::Max($peakWorkingSet, $sample.WorkingSet64)
        $sampleCount++
    }
    if (-not $process.HasExited) { Start-Sleep -Milliseconds 50 }
} while (-not $process.HasExited)

$process.WaitForExit()
$memoryRecord = [ordered]@{
    type = 'process_memory_sample'
    peak_sampled_working_set_bytes = $peakWorkingSet
    sampling_interval_ms = 50
    samples = $sampleCount
    exit_code = $process.ExitCode
}
Add-Content -LiteralPath $outPath -Value ($memoryRecord | ConvertTo-Json -Compress)
Get-Content -LiteralPath $outPath
if ($process.ExitCode -ne 0) { exit $process.ExitCode }
