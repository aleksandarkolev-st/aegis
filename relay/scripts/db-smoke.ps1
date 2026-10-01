$ErrorActionPreference = "Stop"

function Set-SmokeDevicePrincipal {
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

# Compose validates every service's required interpolation even though this smoke test
# starts only PostgreSQL. These exact test identities and grants are never used by NATS.
$composeFile = Join-Path $PSScriptRoot "..\docker-compose.yml"
$composeEnvNames = @(
    "NATS_AUTH_TOKEN",
    "AEGIS_NATS_DEVICE_USERNAME", "AEGIS_NATS_DEVICE_PASSWORD", "AEGIS_NATS_DEVICE_EVENT_SUBJECT",
    "AEGIS_NATS_DEVICE_CONSUMER_INFO", "AEGIS_NATS_DEVICE_CONSUMER_CREATE", "AEGIS_NATS_DEVICE_CONSUMER_NEXT",
    "AEGIS_NATS_DEVICE_ACK", "AEGIS_NATS_DEVICE_INBOX",
    "AEGIS_NATS_DEVICE2_USERNAME", "AEGIS_NATS_DEVICE2_PASSWORD", "AEGIS_NATS_DEVICE2_EVENT_SUBJECT",
    "AEGIS_NATS_DEVICE2_CONSUMER_INFO", "AEGIS_NATS_DEVICE2_CONSUMER_CREATE", "AEGIS_NATS_DEVICE2_CONSUMER_NEXT",
    "AEGIS_NATS_DEVICE2_ACK", "AEGIS_NATS_DEVICE2_INBOX"
)
$previousComposeEnv = @{}
foreach ($name in $composeEnvNames) {
    $previousComposeEnv[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
}

$composeExitCode = -1
try {
    $device1Id = "00000000-0000-4000-8000-000000000001"
    $device2Id = "00000000-0000-4000-8000-000000000002"
    $env:NATS_AUTH_TOKEN = "postgres-smoke-unused-relay-password"
    Set-SmokeDevicePrincipal -Prefix "AEGIS_NATS_DEVICE_" -DeviceId $device1Id -Password "postgres-smoke-unused-device-one"
    Set-SmokeDevicePrincipal -Prefix "AEGIS_NATS_DEVICE2_" -DeviceId $device2Id -Password "postgres-smoke-unused-device-two"

    docker compose -f $composeFile up -d postgres
    $composeExitCode = $LASTEXITCODE
}
finally {
    foreach ($name in $composeEnvNames) {
        $previousValue = $previousComposeEnv[$name]
        if ($null -eq $previousValue) {
            Remove-Item -Path "Env:$name" -ErrorAction SilentlyContinue
        }
        else {
            Set-Item -Path "Env:$name" -Value $previousValue
        }
    }
}

if ($composeExitCode -ne 0) {
    throw "The local PostgreSQL service did not start."
}

$env:RELAY_TEST_DATABASE_URL = "postgres://relay:local-development-password@localhost:55432/aegis_relay?sslmode=disable"
cargo test --manifest-path (Join-Path $PSScriptRoot "..\Cargo.toml") --test postgres_receipts -- --nocapture
if ($LASTEXITCODE -ne 0) {
    throw "The PostgreSQL relay test failed."
}
