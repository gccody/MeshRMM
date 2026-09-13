<#
.SYNOPSIS
Build and install the local Agent on this machine without publishing a release.
.DESCRIPTION
Preserves the existing service registration and protected configuration. Keeps a
timestamped executable backup and restores it if startup/reconnection fails.
Run from a normal PowerShell; UAC is requested after the build succeeds.
.EXAMPLE
& .\scripts\install-agent-local.ps1
.EXAMPLE
& .\scripts\install-agent-local.ps1 -SkipBuild
#>
[CmdletBinding()]
param([switch]$SkipBuild)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$source = Join-Path $repositoryRoot 'target\release\meshrmm-agent.exe'
$target = Join-Path $env:ProgramFiles 'MeshRMM\Agent\meshrmm-agent.exe'
$config = Join-Path $env:ProgramData 'MeshRMM\Agent\agent.json'
$log = Join-Path $env:ProgramData 'MeshRMM\Agent\agent.log'
$dist = Join-Path $repositoryRoot 'dist'
$resultPath = Join-Path $dist 'local-agent-install-result.json'
$logCopy = Join-Path $dist 'agent-after-local-install.log'
if (-not $SkipBuild) {
    Push-Location $repositoryRoot
    try {
        & cargo build --locked --release -p meshrmm-agent --target-dir (Join-Path $repositoryRoot 'target')
        if ($LASTEXITCODE -ne 0) { throw 'Agent build failed; installed service was not changed.' }
    } finally { Pop-Location }
}
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
    throw "Built Agent not found: $source"
}
New-Item -ItemType Directory -Path $dist -Force | Out-Null
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'Requesting administrator access to replace the local Agent...'
    $arguments = '-NoProfile -ExecutionPolicy Bypass -File "{0}" -SkipBuild' -f $PSCommandPath
    $process = Start-Process -FilePath powershell.exe -Verb RunAs -WindowStyle Hidden -ArgumentList $arguments -PassThru -Wait
    if (Test-Path -LiteralPath $resultPath) { Get-Content -LiteralPath $resultPath }
    if ($process.ExitCode -ne 0) { throw 'Local Agent installation failed. Review the result above.' }
    return
}
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$backup = "$target.before-local-$stamp"
$staged = "$target.local-$stamp"
$replaced = $false
$stopped = $false
$result = [ordered]@{ success = $false; backup = $backup }
function Stop-Agent {
    $service = Get-Service -Name MeshRMMAgent
    if ($service.Status -ne 'Stopped') {
        Stop-Service -Name MeshRMMAgent
        $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(45))
    }
}
function Start-Agent {
    Start-Service -Name MeshRMMAgent
    (Get-Service -Name MeshRMMAgent).WaitForStatus('Running', [TimeSpan]::FromSeconds(30))
}
try {
    $service = Get-CimInstance Win32_Service -Filter "Name='MeshRMMAgent'"
    if ($service.PathName -ne ('"' + $target + '" --service --config ' + $config)) {
        throw 'Installed service path differs from the reviewed Agent configuration.'
    }
    $configHash = (Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash
    $sourceHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
    $result.source_sha256 = $sourceHash
    $result.previous_sha256 = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
    Copy-Item -LiteralPath $source -Destination $staged
    if ((Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash -ne $sourceHash) {
        throw 'Staged executable verification failed.'
    }
    Stop-Agent
    $stopped = $true
    [System.IO.File]::Replace($staged, $target, $backup)
    $replaced = $true
    $startedAt = [DateTime]::UtcNow
    Start-Agent
    $connected = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    do {
        Start-Sleep -Milliseconds 500
        $lines = Get-Content -LiteralPath $log -Tail 100
        foreach ($line in $lines) {
            if ($line -match '^(\S+)\s+.*Agent signaling connected') {
                $eventTime = [DateTimeOffset]::Parse($Matches[1]).UtcDateTime
                if ($eventTime -ge $startedAt) { $connected = $true }
            }
        }
    } while (-not $connected -and [DateTime]::UtcNow -lt $deadline)
    if (-not $connected) { throw 'Updated Agent did not reconnect within 45 seconds.' }
    if ((Get-Service -Name MeshRMMAgent).Status -ne 'Running') { throw 'Updated service stopped.' }
    if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ne $sourceHash) {
        throw 'Installed executable differs from the local build.'
    }
    if ((Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash -ne $configHash) {
        throw 'Agent configuration changed during installation.'
    }
    $result.success = $true
    $result.signaling_connected = $connected
    $result.config_unchanged = $true
    $result.service_process_id = (Get-CimInstance Win32_Service -Filter "Name='MeshRMMAgent'").ProcessId
} catch {
    $result.error = $_.Exception.Message
    if ($replaced) {
        try {
            Stop-Agent
            Copy-Item -LiteralPath $backup -Destination $target -Force
            Start-Agent
            $result.rolled_back = $true
        } catch { $result.rollback_error = $_.Exception.Message }
    } elseif ($stopped) {
        try { Start-Agent } catch { $result.restart_error = $_.Exception.Message }
    }
} finally {
    try { Copy-Item -LiteralPath $log -Destination $logCopy -Force } catch {}
    $result | ConvertTo-Json | Set-Content -LiteralPath $resultPath -Encoding UTF8
}
if (-not $result.success) { throw "Local Agent installation failed: $($result.error)" }
Write-Host "Agent installed and connected. Backup: $backup"
Write-Host "Result: $resultPath"
