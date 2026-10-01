$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$manifest = Join-Path $repoRoot "Cargo.toml"
Push-Location $repoRoot
try {
    $installationId = (& cargo run --quiet --manifest-path $manifest -- remote identity | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "Could not read the local Aegis installation identity."
    }
}
finally {
    Pop-Location
}

$parsedInstallationId = [Guid]::Empty
if (-not [Guid]::TryParseExact($installationId, "D", [ref]$parsedInstallationId) -or
    $parsedInstallationId.ToString("D") -cne $installationId) {
    throw "Aegis returned a non-canonical installation identity: '$installationId'."
}

function New-LocalPassword {
    return ([Guid]::NewGuid().ToString("N") + [Guid]::NewGuid().ToString("N"))
}

function Set-LocalDevicePrincipal {
    param(
        [Parameter(Mandatory = $true)][string]$Prefix,
        [Parameter(Mandatory = $true)][string]$DeviceId,
        [Parameter(Mandatory = $true)][string]$Password
    )

    $consumer = "aegis-$DeviceId"
    $values = @{
        USERNAME = $consumer
        PASSWORD = $Password
        EVENT_SUBJECT = "aegis.events.$DeviceId"
        CONSUMER_INFO = ('$JS.API.CONSUMER.INFO.AEGIS_COMMANDS.{0}' -f $consumer)
        CONSUMER_CREATE = ('$JS.API.CONSUMER.CREATE.AEGIS_COMMANDS.{0}.aegis.commands.{1}' -f $consumer, $DeviceId)
        CONSUMER_NEXT = ('$JS.API.CONSUMER.MSG.NEXT.AEGIS_COMMANDS.{0}' -f $consumer)
        ACK = ('$JS.ACK.AEGIS_COMMANDS.{0}.*.*.*.*.*' -f $consumer)
        INBOX = "_INBOX.aegis.device.$DeviceId.>"
    }

    foreach ($entry in $values.GetEnumerator()) {
        Set-Item -Path "Env:$($Prefix)$($entry.Key)" -Value $entry.Value
    }
}

$device2Id = [Guid]::NewGuid().ToString("D")
while ($device2Id -ceq $installationId) {
    $device2Id = [Guid]::NewGuid().ToString("D")
}
$env:NATS_AUTH_TOKEN = New-LocalPassword
$env:AEGIS_NATS_DEVICE_PASSWORD = New-LocalPassword
$env:AEGIS_NATS_DEVICE2_PASSWORD = New-LocalPassword
Set-LocalDevicePrincipal -Prefix "AEGIS_NATS_DEVICE_" -DeviceId $installationId -Password $env:AEGIS_NATS_DEVICE_PASSWORD
Set-LocalDevicePrincipal -Prefix "AEGIS_NATS_DEVICE2_" -DeviceId $device2Id -Password $env:AEGIS_NATS_DEVICE2_PASSWORD

if (-not (Test-Path (Join-Path $PSScriptRoot "certs\nats.crt"))) {
    & (Join-Path $PSScriptRoot "make-nats-certs.ps1")
}

$composeFile = Join-Path $PSScriptRoot "..\docker-compose.yml"
docker compose -f $composeFile up -d
if ($LASTEXITCODE -ne 0) {
    throw "The local Postgres/NATS services did not start."
}

$env:RELAY_BIND_ADDR = "127.0.0.1:8787"
$env:DATABASE_URL = "postgres://relay:local-development-password@localhost:55432/aegis_relay?sslmode=disable"
$env:RELAY_ALLOW_INSECURE_LOCAL_DATABASE = "true"
$env:NATS_URL = "tls://localhost:4422"
$env:NATS_TLS_ROOT_CERT = Join-Path $PSScriptRoot "certs\nats.crt"
$env:RELAY_ADMIN_TOKEN = "local-development-admin-token-not-for-production"
$env:EVOLUTION_BASE_URL = "https://evolution.invalid"
$env:EVOLUTION_INSTANCE = "local"
$env:EVOLUTION_API_KEY = "local-development-evolution-key"
$env:EVOLUTION_WEBHOOK_SECRET = "local-development-webhook-secret-32-bytes"

Write-Host "Relay NATS principal: aegis-relay"
Write-Host "First device installation: $installationId"
Write-Host "First device password for the pairing session: $env:AEGIS_NATS_DEVICE_PASSWORD"
Write-Host "In the pairing session, assign that password to AEGIS_NATS_TOKEN before running the Aegis remote command."

cargo run --manifest-path (Join-Path $PSScriptRoot "..\Cargo.toml")
if ($LASTEXITCODE -ne 0) {
    throw "The relay process exited with an error."
}
