[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repositoryRoot 'agent\Cargo.toml'
$sourceExecutable = Join-Path $repositoryRoot 'agent\target\release\pulsermm-agent.exe'
$distributionDirectory = Join-Path $repositoryRoot 'dist\agent'
$destinationExecutable = Join-Path $distributionDirectory 'pulsermm-agent.exe'

& cargo build --release --manifest-path $manifestPath
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

New-Item -ItemType Directory -Path $distributionDirectory -Force | Out-Null
Copy-Item -LiteralPath $sourceExecutable -Destination $destinationExecutable -Force

$artifact = Get-Item -LiteralPath $destinationExecutable
$checksum = Get-FileHash -Algorithm SHA256 -LiteralPath $destinationExecutable
Write-Output "Agent copied to $($artifact.FullName)"
Write-Output "Size: $($artifact.Length) bytes"
Write-Output "SHA256: $($checksum.Hash)"
