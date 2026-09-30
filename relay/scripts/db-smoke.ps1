$ErrorActionPreference = "Stop"

$composeFile = Join-Path $PSScriptRoot "..\docker-compose.yml"
docker compose -f $composeFile up -d postgres
if ($LASTEXITCODE -ne 0) {
    throw "The local PostgreSQL service did not start."
}

$env:RELAY_TEST_DATABASE_URL = "postgres://relay:local-development-password@localhost:55432/aegis_relay?sslmode=disable"
cargo test --manifest-path (Join-Path $PSScriptRoot "..\Cargo.toml") --test postgres_receipts -- --nocapture
if ($LASTEXITCODE -ne 0) {
    throw "The PostgreSQL relay test failed."
}
