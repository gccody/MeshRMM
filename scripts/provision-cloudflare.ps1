[CmdletBinding()]
param(
    [string]$AccountId = "023f5fc9ffca88a15dac8e9f27b8d21f",
    [Security.SecureString]$CallsApiToken
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($null -eq $CallsApiToken) {
    throw "Creating a TURN key requires -CallsApiToken with Cloudflare Calls Write permission."
}

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$serverDirectory = Join-Path $repositoryRoot "server"
$credential = New-Object Management.Automation.PSCredential("cloudflare", $CallsApiToken)
$callsToken = $credential.GetNetworkCredential().Password

try {
    $turnResponse = Invoke-RestMethod `
        -Method Post `
        -Uri "https://api.cloudflare.com/client/v4/accounts/$AccountId/calls/turn_keys" `
        -Headers @{ Authorization = "Bearer $callsToken" } `
        -ContentType "application/json" `
        -Body (@{ name = "meshrmm-production" } | ConvertTo-Json -Compress)

    if (-not $turnResponse.success -or -not $turnResponse.result.uid -or -not $turnResponse.result.key) {
        throw "Cloudflare did not return a usable TURN key."
    }

    $secretFile = [IO.Path]::GetTempFileName()
    try {
        $secrets = @{
            TURN_KEY_ID = [string]$turnResponse.result.uid
            TURN_KEY_API_TOKEN = [string]$turnResponse.result.key
        }
        [IO.File]::WriteAllText(
            $secretFile,
            ($secrets | ConvertTo-Json),
            (New-Object Text.UTF8Encoding($false))
        )

        Push-Location $serverDirectory
        try {
            & npx wrangler secret bulk $secretFile
            if ($LASTEXITCODE -ne 0) {
                throw "Wrangler failed to install the TURN secrets."
            }
        }
        finally {
            Pop-Location
        }
    }
    finally {
        Remove-Item -LiteralPath $secretFile -Force -ErrorAction SilentlyContinue
    }
}
finally {
    $callsToken = $null
}

Write-Host "Cloudflare TURN credentials are configured."
Write-Host "Create companies in WorkOS and create tenant-scoped Agents from https://meshrmm.com."
