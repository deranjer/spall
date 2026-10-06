param(
    [string]$OutputDirectory = (Join-Path (Split-Path -Parent $PSScriptRoot) "dist\spall-portable")
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$targetRelease = Join-Path $repoRoot "target\release"

Push-Location $repoRoot
try {
    cargo build --release -p spall_editor
    if ($LASTEXITCODE -ne 0) { throw "spall_editor release build failed ($LASTEXITCODE)" }
    cargo build --release -p xtask
    if ($LASTEXITCODE -ne 0) { throw "xtask release build failed ($LASTEXITCODE)" }
    cargo build --release -p sandbox --features client --bin sandbox-server --bin sandbox-client
    if ($LASTEXITCODE -ne 0) { throw "sandbox release build failed ($LASTEXITCODE)" }
}
finally {
    Pop-Location
}

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
foreach ($name in @("spall-editor.exe", "xtask.exe", "sandbox-server.exe", "sandbox-client.exe")) {
    $source = Join-Path $targetRelease $name
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Release binary missing: $source"
    }
    Copy-Item -LiteralPath $source -Destination (Join-Path $OutputDirectory $name) -Force
}

@'
Spall Editor portable build

Run spall-editor.exe. Open File > Generate World, enter or randomize a seed,
choose a world size and starting season, generate the preview, then choose Run
in game. The world runs in the sandbox client with the FPS HUD enabled.
Check Uncapped before Run in game (or scene Run) to request non-VSync game
presentation where supported and see maximum throughput in the FPS HUD.
Generated-world games initially load terrain within 80 m of the spawn brick
on each axis, then fetch surrounding regions as you move. Distant terrain stays
as compact revision/hash records; bodies remain complete. The authoritative
server still generates the whole world. Startup therefore still takes time,
especially for the 1024 m choice. The preview checks bounded water capacity
and shows any refusal beside the Run button. The client retains an 8 GiB
staging admission ceiling for atomic resets; it does not allocate that ceiling.
Use a 32 GiB host for the largest choice. GPU presentation and long-session
memory need separate validation. Smaller choices need less memory.
No water or terrain is removed
to make a world fit. Startup progress and failures appear in the
editor; wait for the current launch or close the game before launching again.

F10 opens the in-game admin menu. Reset world rebuilds the generated world.
The Environment section includes sun direction controls.

Keep all four executables together. The folder may be moved to another Windows
machine with a supported GPU. Each session writes diagnostics under runs\,
including launcher.log and server.stderr.log for startup failures.
'@ | Set-Content -LiteralPath (Join-Path $OutputDirectory "README.txt") -Encoding utf8

Write-Output "Portable build created at $OutputDirectory"
