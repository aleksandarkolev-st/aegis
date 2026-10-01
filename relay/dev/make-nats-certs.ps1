$ErrorActionPreference = "Stop"

$certDirectory = Join-Path $PSScriptRoot "certs"
New-Item -ItemType Directory -Force -Path $certDirectory | Out-Null
$certificate = Join-Path $certDirectory "nats.crt"
$privateKey = Join-Path $certDirectory "nats.key"

$openssl = Get-Command openssl -ErrorAction SilentlyContinue
if ($openssl) {
    & $openssl.Source req -x509 -newkey rsa:2048 -nodes -sha256 -days 365 -keyout $privateKey -out $certificate -subj "/CN=nats" -addext "subjectAltName=DNS:nats,DNS:localhost,IP:127.0.0.1"
}
else {
    & docker run --rm --mount "type=bind,source=$certDirectory,target=/certs" alpine:latest sh -c "apk add --no-cache openssl >/dev/null && openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 365 -keyout /certs/nats.key -out /certs/nats.crt -subj '/CN=nats' -addext 'subjectAltName=DNS:nats,DNS:localhost,IP:127.0.0.1'"
}
if ($LASTEXITCODE -ne 0) {
    throw "OpenSSL could not generate the local NATS certificate. Install OpenSSL or make Docker available."
}
