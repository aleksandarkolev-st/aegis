$ErrorActionPreference = "Stop"

$manifest = Join-Path $PSScriptRoot "..\Cargo.toml"
cargo test --manifest-path $manifest --test local_smoke
if ($LASTEXITCODE -ne 0) {
    throw "The local relay smoke test failed."
}
