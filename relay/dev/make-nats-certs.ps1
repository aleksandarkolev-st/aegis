param([string]$OutputDirectory)

$ErrorActionPreference = "Stop"

$certDirectory = if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    Join-Path $PSScriptRoot "certs"
}
else {
    [System.IO.Path]::GetFullPath($OutputDirectory)
}
New-Item -ItemType Directory -Force -Path $certDirectory | Out-Null
$certificate = Join-Path $certDirectory "nats.crt"
$privateKey = Join-Path $certDirectory "nats.key"

$openssl = Get-Command openssl -ErrorAction SilentlyContinue
if ($openssl) {
    $previousOpenSslConfig = [Environment]::GetEnvironmentVariable("OPENSSL_CONF", "Process")
    try {
        if ($previousOpenSslConfig -and -not (Test-Path -LiteralPath $previousOpenSslConfig -PathType Leaf)) {
            $candidateConfigs = @(
                (Join-Path (Split-Path $openssl.Source -Parent) "..\ssl\openssl.cnf"),
                (Join-Path $env:ProgramFiles "Git\mingw64\etc\ssl\openssl.cnf"),
                (Join-Path $env:ProgramFiles "Common Files\SSL\openssl.cnf")
            )
            $usableConfig = $candidateConfigs |
                Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
                Select-Object -First 1
            if ($usableConfig) {
                $env:OPENSSL_CONF = (Resolve-Path -LiteralPath $usableConfig).Path
            }
            else {
                Remove-Item Env:OPENSSL_CONF -ErrorAction SilentlyContinue
            }
        }
        & $openssl.Source req -x509 -newkey rsa:2048 -nodes -sha256 -days 365 -keyout $privateKey -out $certificate -subj "/CN=nats" -addext "basicConstraints=critical,CA:FALSE" -addext "keyUsage=critical,digitalSignature,keyEncipherment" -addext "extendedKeyUsage=serverAuth" -addext "subjectAltName=DNS:nats,DNS:localhost,IP:127.0.0.1"
    }
    finally {
        [Environment]::SetEnvironmentVariable("OPENSSL_CONF", $previousOpenSslConfig, "Process")
    }
}
else {
    & docker run --rm --mount "type=bind,source=$certDirectory,target=/certs" alpine:latest sh -c "apk add --no-cache openssl >/dev/null && openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 365 -keyout /certs/nats.key -out /certs/nats.crt -subj '/CN=nats' -addext 'basicConstraints=critical,CA:FALSE' -addext 'keyUsage=critical,digitalSignature,keyEncipherment' -addext 'extendedKeyUsage=serverAuth' -addext 'subjectAltName=DNS:nats,DNS:localhost,IP:127.0.0.1'"
}
if ($LASTEXITCODE -ne 0) {
    throw "OpenSSL could not generate the local NATS certificate. Install OpenSSL or make Docker available."
}
