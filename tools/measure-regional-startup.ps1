param(
    [ValidateSet(512, 1024, 2048, 4096)][int]$WorldSizeCells = 1024,
    [switch]$FullBaseline,
    [ValidateRange(300, 9000)][int]$ServerTicks = 3000,
    [Parameter(Mandatory = $true)][string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$probeDir = [System.IO.Path]::GetFullPath($OutputDirectory, $repoRoot)
if (Test-Path -LiteralPath $probeDir) { throw 'Use a fresh output directory to avoid stale readiness files.' }
New-Item -ItemType Directory -Path $probeDir | Out-Null
$token = [Convert]::ToHexString([System.Security.Cryptography.RandomNumberGenerator]::GetBytes(32)).ToLowerInvariant()
Set-Content -LiteralPath "$probeDir/join.token" -Value $token -NoNewline
$server = $null
$client = $null
$started = [DateTime]::UtcNow
$clientStarted = $null
$serverPeak = 0L
$clientPeak = 0L
try {
    $serverArgs = @('--serve','--listen','127.0.0.1:0','--join-token-file',"$probeDir/join.token",'--worldgen','showcase','--seed','1','--worldgen-size',"$WorldSizeCells",'--ticks',"$ServerTicks",'--min-clients','1','--quiescence-ticks','0','--paced','--fingerprint-out',"$probeDir/server.fingerprint",'--addr-out',"$probeDir/server.addr",'--log-json',"$probeDir/server.jsonl",'--summary-json',"$probeDir/server.summary.json")
    $server = Start-Process -FilePath "$repoRoot/target/release/sandbox-server.exe" -WorkingDirectory $repoRoot -ArgumentList $serverArgs -WindowStyle Hidden -PassThru -RedirectStandardOutput "$probeDir/server.stdout.log" -RedirectStandardError "$probeDir/server.stderr.log"
    while (([DateTime]::UtcNow - $started).TotalSeconds -lt 240) {
        $server.Refresh()
        if (!$server.HasExited) { $serverPeak = [Math]::Max($serverPeak, $server.PeakWorkingSet64) }
        if (!$client -and (Test-Path -LiteralPath "$probeDir/server.addr") -and (Test-Path -LiteralPath "$probeDir/server.fingerprint")) {
            $addr = (Get-Content -LiteralPath "$probeDir/server.addr" -Raw).Trim()
            $clientArgs = @('--connect',$addr,'--server-fingerprint',"$probeDir/server.fingerprint",'--join-token-file',"$probeDir/join.token",'--late-join','--baseline-budget-mib','8192','--timeout-ms','180000','--run-ticks','120','--log-json',"$probeDir/client.jsonl",'--summary-json',"$probeDir/client.summary.json")
            if (!$FullBaseline) { $clientArgs += '--stream-regions' }
            $clientStarted = [DateTime]::UtcNow
            $client = Start-Process -FilePath "$repoRoot/target/release/sandbox-client.exe" -WorkingDirectory $repoRoot -ArgumentList $clientArgs -WindowStyle Hidden -PassThru -RedirectStandardOutput "$probeDir/client.stdout.log" -RedirectStandardError "$probeDir/client.stderr.log"
        }
        if ($client) {
            $client.Refresh()
            if (!$client.HasExited) { $clientPeak = [Math]::Max($clientPeak, $client.PeakWorkingSet64) }
            if ($client.HasExited -and $server.HasExited) { break }
        } elseif ($server.HasExited) { break }
        Start-Sleep -Milliseconds 100
    }
    $clientSummary = if (Test-Path -LiteralPath "$probeDir/client.summary.json") { Get-Content -LiteralPath "$probeDir/client.summary.json" -Raw | ConvertFrom-Json } else { $null }
    $serverSummary = if (Test-Path -LiteralPath "$probeDir/server.summary.json") { Get-Content -LiteralPath "$probeDir/server.summary.json" -Raw | ConvertFrom-Json } else { $null }
    $passed = $client -and $client.HasExited -and $client.ExitCode -eq 0 -and $server.HasExited -and $server.ExitCode -eq 0 -and $clientSummary.final_world_hash -eq $serverSummary.final_world_hash
    $result = [ordered]@{
        passed = [bool]$passed
        world_size_cells = $WorldSizeCells
        regional = !$FullBaseline
        server_startup_ms = if ($clientStarted) { [int]($clientStarted - $started).TotalMilliseconds } else { $null }
        ready_ms = $clientSummary.late_join_ready_ms
        baseline_bytes = $clientSummary.late_join_baseline_compressed_bytes
        initial_resident_bricks = $clientSummary.initial_terrain_resident_bricks
        initial_digest_bricks = $clientSummary.initial_terrain_digest_bricks
        server_peak_working_set = $serverPeak
        client_peak_working_set = $clientPeak
        client_exit = if ($client -and $client.HasExited) { $client.ExitCode } else { $null }
        server_exit = if ($server.HasExited) { $server.ExitCode } else { $null }
        final_hash = $clientSummary.final_world_hash
    }
    $json = $result | ConvertTo-Json
    Set-Content -LiteralPath "$probeDir/startup.measurement.json" -Value $json
    Write-Output $json
    if (!$passed) { throw "Startup probe failed; evidence retained in $probeDir" }
} finally {
    foreach ($ownedProcess in @($client, $server)) {
        if ($ownedProcess) {
            $ownedProcess.Refresh()
            if (!$ownedProcess.HasExited) { $ownedProcess.Kill(); $ownedProcess.WaitForExit() }
            $ownedProcess.Dispose()
        }
    }
}
