$ErrorActionPreference = "Stop"

$certDirectory = Join-Path $PSScriptRoot "certs"
New-Item -ItemType Directory -Force -Path $certDirectory | Out-Null
$certificate = Join-Path $certDirectory "nats.crt"
$privateKey = Join-Path $certDirectory "nats.key"

if (-not (Get-Command openssl -ErrorAction SilentlyContinue)) {
    throw "OpenSSL is required to generate the local NATS certificate."
}

& openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 365 -keyout $privateKey -out $certificate -subj "/CN=nats" -addext "subjectAltName=DNS:nats,DNS:localhost,IP:127.0.0.1"
if ($LASTEXITCODE -ne 0) {
    throw "OpenSSL could not generate the local NATS certificate."
}
