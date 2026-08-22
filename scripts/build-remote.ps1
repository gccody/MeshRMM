[CmdletBinding()]
param(
    [string]$DownloadOrigin = 'https://pulsermm.gccody.dev'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repositoryRoot 'remote\Cargo.toml'
$sourceExecutable = Join-Path $repositoryRoot 'target\release\pulsermm-remote.exe'
$distributionDirectory = Join-Path $repositoryRoot 'dist\remote'
$destinationExecutable = Join-Path $distributionDirectory 'pulsermm-remote.exe'
$dashboardDownloadDirectory = Join-Path $repositoryRoot 'dashboard\public\downloads'
$dashboardExecutable = Join-Path $dashboardDownloadDirectory 'pulsermm-remote-windows-x64.exe'
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
Copy-Item -LiteralPath $sourceExecutable -Destination $destinationExecutable -Force
Copy-Item -LiteralPath $sourceExecutable -Destination $dashboardExecutable -Force

$artifact = Get-Item -LiteralPath $sourceExecutable
$checksum = Get-FileHash -Algorithm SHA256 -LiteralPath $sourceExecutable
$checksum.Hash.ToLowerInvariant() | Set-Content -LiteralPath $dashboardChecksum -Encoding ascii -NoNewline
$metadata = (& cargo metadata --no-deps --format-version 1 --manifest-path $workspaceManifest) | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -EQ 'pulsermm-remote').version
& node $manifestWriter `
    $updateManifest `
    'client-windows-x64' `
    $version `
    "$($DownloadOrigin.TrimEnd('/'))/downloads/pulsermm-remote-windows-x64.exe" `
    $dashboardExecutable
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Write-Output "Remote client built at $($artifact.FullName)"
Write-Output "Dashboard update asset copied to $dashboardExecutable"
Write-Output "Size: $($artifact.Length) bytes"
Write-Output "SHA256: $($checksum.Hash)"
