$ErrorActionPreference = "Stop"

if (-not (Test-Path (Join-Path $PSScriptRoot "certs\nats.crt"))) {
    & (Join-Path $PSScriptRoot "make-nats-certs.ps1")
}

docker compose -f (Join-Path $PSScriptRoot "..\docker-compose.yml") up -d
if ($LASTEXITCODE -ne 0) {
    throw "The local Postgres/NATS services did not start."
}

$env:RELAY_BIND_ADDR = "127.0.0.1:8787"
$env:DATABASE_URL = "postgres://relay:local-development-password@localhost:55432/aegis_relay?sslmode=disable"
$env:RELAY_ALLOW_INSECURE_LOCAL_DATABASE = "true"
$env:NATS_URL = "tls://localhost:4422"
$env:NATS_AUTH_TOKEN = "local-development-nats-token-not-for-production"
$env:NATS_TLS_ROOT_CERT = Join-Path $PSScriptRoot "certs\nats.crt"
$env:RELAY_ADMIN_TOKEN = "local-development-admin-token-not-for-production"
$env:EVOLUTION_BASE_URL = "https://evolution.invalid"
$env:EVOLUTION_INSTANCE = "local"
$env:EVOLUTION_API_KEY = "local-development-evolution-key"
$env:EVOLUTION_WEBHOOK_SECRET = "local-development-webhook-secret-32-bytes"

cargo run --manifest-path (Join-Path $PSScriptRoot "..\Cargo.toml")
