[CmdletBinding()]
param(
    [string]$DownloadOrigin = 'https://meshrmm.com'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repositoryRoot 'agent\Cargo.toml'
$sourceExecutable = Join-Path $repositoryRoot 'target\release\meshrmm-agent.exe'
$distributionDirectory = Join-Path $repositoryRoot 'dist\agent'
$destinationExecutable = Join-Path $distributionDirectory 'meshrmm-agent.exe'
$dashboardDownloadDirectory = Join-Path $repositoryRoot 'dashboard\public\downloads'
$dashboardExecutable = Join-Path $dashboardDownloadDirectory 'meshrmm-agent-windows-x64.exe'
$dashboardChecksum = "$dashboardExecutable.sha256"
$updateManifest = Join-Path $dashboardDownloadDirectory 'update-manifest.json'
$manifestWriter = Join-Path $PSScriptRoot 'update-release-manifest.mjs'
$workspaceManifest = Join-Path $repositoryRoot 'Cargo.toml'

& cargo build --locked --release --manifest-path $manifestPath
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

New-Item -ItemType Directory -Path $distributionDirectory -Force | Out-Null
New-Item -ItemType Directory -Path $dashboardDownloadDirectory -Force | Out-Null
Copy-Item -LiteralPath $sourceExecutable -Destination $dashboardExecutable -Force
try {
    Copy-Item -LiteralPath $sourceExecutable -Destination $destinationExecutable -Force
} catch [System.IO.IOException] {
    Write-Warning "The portable dist Agent is currently running and could not be replaced. The dashboard installer asset was still published."
}

$artifact = Get-Item -LiteralPath $sourceExecutable
$checksum = Get-FileHash -Algorithm SHA256 -LiteralPath $sourceExecutable
$checksum.Hash.ToLowerInvariant() | Set-Content -LiteralPath $dashboardChecksum -Encoding ascii -NoNewline
$metadata = (& cargo metadata --no-deps --format-version 1 --manifest-path $workspaceManifest) | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -EQ 'meshrmm-agent').version
& node $manifestWriter `
    $updateManifest `
    'agent-windows-x64' `
    $version `
    "$($DownloadOrigin.TrimEnd('/'))/downloads/meshrmm-agent-windows-x64.exe" `
    $dashboardExecutable
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
Write-Output "Agent built at $($artifact.FullName)"
Write-Output "Dashboard installer asset copied to $dashboardExecutable"
Write-Output "Size: $($artifact.Length) bytes"
Write-Output "SHA256: $($checksum.Hash)"
