$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$composeFile = Join-Path $PSScriptRoot "..\docker-compose.yml"
$certificateScript = Join-Path $PSScriptRoot "..\dev\make-nats-certs.ps1"
$certificate = (Join-Path $PSScriptRoot "..\dev\certs\nats.crt")
$relayManifest = Join-Path $PSScriptRoot "..\Cargo.toml"
$aegisBinary = Join-Path $repoRoot "target\debug\arun.exe"
$projectName = "aegis-e2e-$([guid]::NewGuid().ToString('N'))"
$stackMayExist = $false

function Get-LoopbackPort {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    try {
        $listener.Start()
        return ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
    }
    finally {
        $listener.Stop()
    }
}

$postgresPort = Get-LoopbackPort
do {
    $natsPort = Get-LoopbackPort
} while ($natsPort -eq $postgresPort)

$environmentNames = @(
    "RELAY_POSTGRES_PORT",
    "RELAY_NATS_PORT",
    "RELAY_E2E_DATABASE_URL",
    "RELAY_TEST_DATABASE_URL",
    "RELAY_E2E_NATS_URL",
    "RELAY_E2E_NATS_TOKEN",
    "RELAY_E2E_NATS_ROOT_CERT",
    "AEGIS_E2E_BINARY"
)
$previousEnvironment = @{}
foreach ($name in $environmentNames) {
    $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
}

try {
    Push-Location -LiteralPath $repoRoot
    if (-not (Test-Path -LiteralPath $certificate)) {
        & $certificateScript
        if ($LASTEXITCODE -ne 0) {
            throw "Could not create the local NATS TLS certificate."
        }
    }

    & cargo build --offline --bin arun
    if ($LASTEXITCODE -ne 0) {
        throw "The Aegis binary did not build."
    }

    $env:RELAY_POSTGRES_PORT = [string]$postgresPort
    $env:RELAY_NATS_PORT = [string]$natsPort
    $databaseUrl = "postgres://relay:local-development-password@127.0.0.1:$postgresPort/aegis_relay?sslmode=disable"
    $env:RELAY_E2E_DATABASE_URL = $databaseUrl
    $env:RELAY_TEST_DATABASE_URL = $databaseUrl
    $env:RELAY_E2E_NATS_URL = "tls://localhost:$natsPort"
    $env:RELAY_E2E_NATS_TOKEN = "local-development-nats-token-not-for-production"
    $env:RELAY_E2E_NATS_ROOT_CERT = (Resolve-Path -LiteralPath $certificate).Path
    $env:AEGIS_E2E_BINARY = $aegisBinary

    $stackMayExist = $true
    & docker compose --project-name $projectName -f $composeFile up -d --wait postgres nats
    if ($LASTEXITCODE -ne 0) {
        throw "The isolated PostgreSQL/NATS services did not become ready."
    }

    & cargo test --offline --manifest-path $relayManifest --config profile.dev.debug=0 -j 1
    if ($LASTEXITCODE -ne 0) {
        throw "The relay test suite failed against the local PostgreSQL service."
    }

    & cargo test --offline --manifest-path $relayManifest --test local_smoke jetstream_remote_command_and_event_round_trip_uses_local_authority -- --ignored --exact --nocapture --test-threads=1
    if ($LASTEXITCODE -ne 0) {
        throw "The Aegis/relay PostgreSQL and JetStream end-to-end test failed."
    }
}
finally {
    try {
        if ($stackMayExist -and $projectName -match "^aegis-e2e-[0-9a-f]{32}$") {
            & docker compose --project-name $projectName -f $composeFile down --volumes --remove-orphans
        }
    }
    finally {
        foreach ($name in $environmentNames) {
            [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], "Process")
        }
        if ((Get-Location).Path -eq $repoRoot) {
            Pop-Location
        }
    }
}
