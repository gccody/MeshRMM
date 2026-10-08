# Builds the Windows Agent into dist\downloads, the directory a local server
# can serve as its downloads, and describes it in dist\downloads\artifacts.json.
# The build is signed when MESHRMM_RELEASE_SIGNING_KEY holds the signing key.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repositoryRoot 'agent\Cargo.toml'
$sourceExecutable = Join-Path $repositoryRoot 'target\release\meshrmm-agent.exe'
$distributionDirectory = Join-Path $repositoryRoot 'dist\agent'
$destinationExecutable = Join-Path $distributionDirectory 'meshrmm-agent.exe'
$downloadDirectory = Join-Path $repositoryRoot 'dist\downloads'
$downloadExecutable = Join-Path $downloadDirectory 'meshrmm-agent-windows-x64.exe'
$artifactsWriter = Join-Path $PSScriptRoot 'release-artifacts.mjs'

. (Join-Path $PSScriptRoot 'use-cmake.ps1')
& cargo build --locked --release --manifest-path $manifestPath
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

New-Item -ItemType Directory -Path $distributionDirectory -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $repositoryRoot 'THIRD_PARTY_NOTICES.txt') -Destination $distributionDirectory -Force
New-Item -ItemType Directory -Path $downloadDirectory -Force | Out-Null
Copy-Item -LiteralPath $sourceExecutable -Destination $downloadExecutable -Force
try {
    Copy-Item -LiteralPath $sourceExecutable -Destination $destinationExecutable -Force
} catch [System.IO.IOException] {
    Write-Warning "The portable dist Agent is currently running and could not be replaced. The download was still updated."
}

$artifact = Get-Item -LiteralPath $sourceExecutable
$checksum = Get-FileHash -Algorithm SHA256 -LiteralPath $sourceExecutable
& node $artifactsWriter 'write' $downloadDirectory
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
Write-Output "Agent built at $($artifact.FullName)"
Write-Output "Download copied to $downloadExecutable"
Write-Output "Size: $($artifact.Length) bytes"
Write-Output "SHA256: $($checksum.Hash)"
