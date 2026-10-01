$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$composeFile = Join-Path $PSScriptRoot "..\docker-compose.yml"
$certificateScript = Join-Path $PSScriptRoot "..\dev\make-nats-certs.ps1"
$relayManifest = Join-Path $PSScriptRoot "..\Cargo.toml"
$aegisBinary = Join-Path $repoRoot "target\debug\arun.exe"
$projectName = "aegis-e2e-$([guid]::NewGuid().ToString('N'))"
$workspaceRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath()).TrimEnd([System.IO.Path]::DirectorySeparatorChar)
$workspaceName = "aegis-e2e-workspace-$([guid]::NewGuid().ToString('N'))"
$e2eWorkspace = Join-Path $workspaceRoot $workspaceName
$certificateDirectory = Join-Path $e2eWorkspace "nats-certs"
$certificate = Join-Path $certificateDirectory "nats.crt"
$stackMayExist = $false
$workspaceMayExist = $false

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

function New-LocalPassword {
    return ("e2e-" + [guid]::NewGuid().ToString("N") + [guid]::NewGuid().ToString("N"))
}

function Set-DevicePrincipal {
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
        CONSUMER_INFO = ('"$JS.API.CONSUMER.INFO.AEGIS_COMMANDS.{0}"' -f $consumer)
        CONSUMER_CREATE = ('"$JS.API.CONSUMER.CREATE.AEGIS_COMMANDS.{0}.aegis.commands.{1}"' -f $consumer, $DeviceId)
        CONSUMER_NEXT = ('"$JS.API.CONSUMER.MSG.NEXT.AEGIS_COMMANDS.{0}"' -f $consumer)
        ACK = ('"$JS.ACK.AEGIS_COMMANDS.{0}.*.*.*.*.*"' -f $consumer)
        INBOX = "_INBOX.aegis.device.$DeviceId.>"
    }
    foreach ($entry in $values.GetEnumerator()) {
        Set-Item -Path "Env:$($Prefix)$($entry.Key)" -Value $entry.Value
    }
}

$postgresPort = Get-LoopbackPort
do {
    $natsPort = Get-LoopbackPort
} while ($natsPort -eq $postgresPort)

$environmentNames = @(
    "RELAY_POSTGRES_PORT", "RELAY_NATS_PORT", "RELAY_NATS_CERT_DIR", "NATS_AUTH_TOKEN",
    "AEGIS_NATS_DEVICE_USERNAME", "AEGIS_NATS_DEVICE_PASSWORD", "AEGIS_NATS_DEVICE_EVENT_SUBJECT",
    "AEGIS_NATS_DEVICE_CONSUMER_INFO", "AEGIS_NATS_DEVICE_CONSUMER_CREATE", "AEGIS_NATS_DEVICE_CONSUMER_NEXT",
    "AEGIS_NATS_DEVICE_ACK", "AEGIS_NATS_DEVICE_INBOX",
    "AEGIS_NATS_DEVICE2_USERNAME", "AEGIS_NATS_DEVICE2_PASSWORD", "AEGIS_NATS_DEVICE2_EVENT_SUBJECT",
    "AEGIS_NATS_DEVICE2_CONSUMER_INFO", "AEGIS_NATS_DEVICE2_CONSUMER_CREATE", "AEGIS_NATS_DEVICE2_CONSUMER_NEXT",
    "AEGIS_NATS_DEVICE2_ACK", "AEGIS_NATS_DEVICE2_INBOX",
    "RELAY_E2E_DATABASE_URL", "RELAY_TEST_DATABASE_URL", "RELAY_E2E_NATS_URL", "RELAY_E2E_NATS_TOKEN",
    "RELAY_E2E_NATS_ROOT_CERT", "RELAY_E2E_INSTALLATION_ID", "RELAY_E2E_WORKSPACE",
    "AEGIS_E2E_NATS_TOKEN", "AEGIS_E2E_DEVICE2_NATS_TOKEN", "AEGIS_E2E_DEVICE2_INSTALLATION_ID",
    "AEGIS_E2E_BINARY"
)
$previousEnvironment = @{}
foreach ($name in $environmentNames) {
    $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
}

try {
    Push-Location -LiteralPath $repoRoot
    New-Item -ItemType Directory -Path $e2eWorkspace | Out-Null
    $workspaceMayExist = $true
    & $certificateScript -OutputDirectory $certificateDirectory
    if ($LASTEXITCODE -ne 0) { throw "Could not create the isolated NATS TLS certificate." }
    $env:RELAY_NATS_CERT_DIR = $certificateDirectory

    & cargo build --offline --bin arun
    if ($LASTEXITCODE -ne 0) { throw "The Aegis binary did not build." }
    Push-Location -LiteralPath $e2eWorkspace
    try {
        $installationId = (& $aegisBinary remote identity | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw "Could not create the E2E Aegis installation identity." }
    }
    finally { Pop-Location }

    $parsedInstallationId = [guid]::Empty
    if (-not [guid]::TryParseExact($installationId, "D", [ref]$parsedInstallationId) -or
        $parsedInstallationId.ToString("D") -cne $installationId) {
        throw "Aegis returned a non-canonical installation identity: '$installationId'."
    }
    $device2Id = [guid]::NewGuid().ToString("D")
    while ($device2Id -ceq $installationId) { $device2Id = [guid]::NewGuid().ToString("D") }

    $relayPassword = New-LocalPassword
    $device1Password = New-LocalPassword
    $device2Password = New-LocalPassword
    $env:RELAY_POSTGRES_PORT = [string]$postgresPort
    $env:RELAY_NATS_PORT = [string]$natsPort
    $env:NATS_AUTH_TOKEN = $relayPassword
    $env:AEGIS_NATS_DEVICE_PASSWORD = $device1Password
    $env:AEGIS_NATS_DEVICE2_PASSWORD = $device2Password
    Set-DevicePrincipal -Prefix "AEGIS_NATS_DEVICE_" -DeviceId $installationId -Password $device1Password
    Set-DevicePrincipal -Prefix "AEGIS_NATS_DEVICE2_" -DeviceId $device2Id -Password $device2Password

    $databaseUrl = "postgres://relay:local-development-password@127.0.0.1:$postgresPort/aegis_relay?sslmode=disable"
    $env:RELAY_E2E_DATABASE_URL = $databaseUrl
    $env:RELAY_TEST_DATABASE_URL = $databaseUrl
    $env:RELAY_E2E_NATS_URL = "tls://localhost:$natsPort"
    $env:RELAY_E2E_NATS_TOKEN = $relayPassword
    $env:RELAY_E2E_NATS_ROOT_CERT = (Resolve-Path -LiteralPath $certificate).Path
    $env:RELAY_E2E_INSTALLATION_ID = $installationId
    $env:RELAY_E2E_WORKSPACE = $e2eWorkspace
    $env:AEGIS_E2E_NATS_TOKEN = $device1Password
    $env:AEGIS_E2E_DEVICE2_NATS_TOKEN = $device2Password
    $env:AEGIS_E2E_DEVICE2_INSTALLATION_ID = $device2Id
    $env:AEGIS_E2E_BINARY = $aegisBinary

    & docker compose --project-name $projectName -f $composeFile config --quiet
    if ($LASTEXITCODE -ne 0) { throw "The isolated PostgreSQL/NATS Compose configuration is invalid." }

    $stackMayExist = $true
    & docker compose --project-name $projectName -f $composeFile up -d --wait postgres nats
    if ($LASTEXITCODE -ne 0) { throw "The isolated PostgreSQL/NATS services did not become ready." }

    & cargo test --offline --manifest-path $relayManifest --config profile.dev.debug=0 --config profile.test.debug=0 -j 1 -- --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "The relay test suite failed against local PostgreSQL." }

    & cargo test --offline --manifest-path $relayManifest --test local_smoke jetstream_remote_phone_lifecycle_uses_local_authority_and_isolates_devices -- --ignored --exact --nocapture --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "The Aegis/relay phone lifecycle and NATS isolation E2E failed." }
}
finally {
    try {
        if ($stackMayExist -and $projectName -match "^aegis-e2e-[0-9a-f]{32}$") {
            & docker compose --project-name $projectName -f $composeFile down --volumes --remove-orphans
            if ($LASTEXITCODE -ne 0) { Write-Warning "Docker Compose cleanup reported an error for the isolated E2E project." }
        }
    }
    finally {
        if ($workspaceMayExist -and (Test-Path -LiteralPath $e2eWorkspace)) {
            $resolvedWorkspace = (Resolve-Path -LiteralPath $e2eWorkspace).Path
            $resolvedParent = [System.IO.Path]::GetDirectoryName($resolvedWorkspace)
            $resolvedName = [System.IO.Path]::GetFileName($resolvedWorkspace)
            if ($resolvedParent -eq $workspaceRoot -and $resolvedName -match "^aegis-e2e-workspace-[0-9a-f]{32}$") {
                Remove-Item -LiteralPath $resolvedWorkspace -Recurse -Force
            }
        }
        foreach ($name in $environmentNames) {
            [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], "Process")
        }
        if ((Get-Location).Path -eq $repoRoot) { Pop-Location }
    }
}
