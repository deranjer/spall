param(
    [string]$Scenario = 'eng114-worldgen-1024m-stress',
    [string]$OutputDirectory = '.local/runs/eng114-1024m-stress',
    [int]$TimeoutMs = 600000
)
$ErrorActionPreference = 'Stop'
$workspace = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$runDirectory = [IO.Path]::GetFullPath((Join-Path $workspace $OutputDirectory))
if (!$runDirectory.StartsWith($workspace + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Output must be inside the workspace.'
}
if (Test-Path -LiteralPath $runDirectory) { throw 'Use a new output directory to avoid mixing run evidence.' }
New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null
$samplerStart = [DateTime]::UtcNow
$tracked = @{}
$peaks = @{}
$csvPath = Join-Path $runDirectory 'process-memory.csv'
Set-Content -LiteralPath $csvPath -Value 'elapsed_ms,role,pid,working_set_bytes,peak_working_set_bytes,private_bytes'
$xtask = Start-Process -FilePath (Join-Path $workspace 'target/debug/xtask.exe') -WorkingDirectory $workspace -ArgumentList @('scenario','--name',$Scenario,'--timeout-ms',"$TimeoutMs",'--output',('"' + $runDirectory + '"')) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $runDirectory 'harness.stdout.log') -RedirectStandardError (Join-Path $runDirectory 'harness.stderr.log')
try {
    while (!$xtask.HasExited) {
        # Only processes that identify themselves in this fresh run's owned logs
        # are sampled. Never enumerate or terminate unrelated game processes.
        foreach ($logFile in Get-ChildItem -LiteralPath $runDirectory -Filter '*.jsonl') {
            $roleName = $logFile.BaseName
            if (!$tracked.ContainsKey($roleName)) {
                $firstLine = Get-Content -LiteralPath $logFile.FullName -TotalCount 1
                if ($firstLine) {
                    $event = $firstLine | ConvertFrom-Json
                    if ($event.event -eq 'started' -and $event.pid) {
                        $tracked[$roleName] = [int]$event.pid
                    }
                }
            }
        }
        foreach ($roleName in $tracked.Keys) {
            $sampleProcess = Get-Process -Id $tracked[$roleName] -ErrorAction SilentlyContinue
            if ($sampleProcess -and !$sampleProcess.HasExited) {
                $peaks[$roleName] = [Math]::Max([long]$peaks[$roleName], $sampleProcess.PeakWorkingSet64)
                $elapsed = [long]([DateTime]::UtcNow - $samplerStart).TotalMilliseconds
                Add-Content -LiteralPath $csvPath -Value "$elapsed,$roleName,$($sampleProcess.Id),$($sampleProcess.WorkingSet64),$($sampleProcess.PeakWorkingSet64),$($sampleProcess.PrivateMemorySize64)"
            }
        }
        if (([DateTime]::UtcNow - $samplerStart).TotalMilliseconds -gt $TimeoutMs + 120000) {
            throw 'Harness exceeded scenario deadline plus build/cleanup allowance.'
        }
        Start-Sleep -Milliseconds 500
        $xtask.Refresh()
    }
    $xtask.WaitForExit()
    [pscustomobject]@{ scenario=$Scenario; exit_code=$xtask.ExitCode; elapsed_ms=[long]([DateTime]::UtcNow-$samplerStart).TotalMilliseconds; peak_working_set_bytes=$peaks } |
        ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $runDirectory 'process-memory.summary.json')
    Get-Content -LiteralPath (Join-Path $runDirectory 'process-memory.summary.json')
    Get-Content -LiteralPath (Join-Path $runDirectory 'harness.stderr.log') -Tail 8
    if ($xtask.ExitCode -ne 0) { throw "Scenario failed with exit $($xtask.ExitCode); preserved evidence in $runDirectory" }
}
finally {
    # xtask owns child cancellation/cleanup. Its ordinary deadline terminates
    # its children; if this supervisor itself fails, terminate only recorded
    # children whose executable still belongs to this workspace.
    if (!$xtask.HasExited) {
        foreach ($ownedPid in $tracked.Values) {
            $ownedProcess = Get-Process -Id $ownedPid -ErrorAction SilentlyContinue
            if ($ownedProcess -and $ownedProcess.Path -and $ownedProcess.Path.StartsWith((Join-Path $workspace 'target'), [StringComparison]::OrdinalIgnoreCase)) {
                $ownedProcess.Kill()
            }
        }
        $xtask.Kill()
        $xtask.WaitForExit()
    }
    $xtask.Dispose()
}
