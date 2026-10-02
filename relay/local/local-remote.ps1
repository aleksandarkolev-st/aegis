param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("init", "start", "stop", "status", "create-instance", "webhook", "qr", "self-account", "pair", "daemon")]
    [string]$Action,
    [string]$Workspace,
    [string]$AegisBinary,
    [string]$RelayBinary
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
if ($env:OS -ne "Windows_NT") { throw "This controller requires Windows and Docker Desktop." }
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if (-not $Workspace) { $Workspace = $repoRoot }
$Workspace = (Resolve-Path -LiteralPath $Workspace).Path
$stateDirectory = Join-Path $Workspace ".arun\local-remote"
$stateFile = Join-Path $stateDirectory "state.json"
$composeFile = Join-Path $PSScriptRoot "compose.yaml"
$envFile = Join-Path $stateDirectory "compose.env"
$nativeRelay = if ($RelayBinary) { (Resolve-Path -LiteralPath $RelayBinary).Path }
else {
    $bundledRelay = Join-Path $repoRoot "vendor\x86_64-pc-windows-msvc\aegis-relay.exe"
    if (Test-Path -LiteralPath $bundledRelay -PathType Leaf) { (Resolve-Path -LiteralPath $bundledRelay).Path }
    else { Join-Path $repoRoot "relay\target\debug\aegis-relay.exe" }
}

function Protect-Directory([string]$Path) {
    if (Test-Path -LiteralPath $Path) {
        if ((Get-Item -LiteralPath $Path).Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "Refusing a linked private directory: $Path"
        }
    }
    else { New-Item -ItemType Directory -Path $Path | Out-Null }
    $acl = Get-Acl -LiteralPath $Path
    $owner = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $allowedSids = @($owner.Value, "S-1-5-18")
    $rules = @($acl.Access)
    $correct = $acl.AreAccessRulesProtected -and $rules.Count -eq 2
    foreach ($rule in $rules) {
        if ($rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -notin $allowedSids -or
            $rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or
            $rule.FileSystemRights -ne [Security.AccessControl.FileSystemRights]::FullControl -or
            $rule.InheritanceFlags -ne [Security.AccessControl.InheritanceFlags]"ContainerInherit,ObjectInherit") { $correct = $false }
    }
    if ($correct) { return }
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($existing in @($acl.Access)) { $acl.RemoveAccessRuleSpecific($existing) }
    $inherit = [Security.AccessControl.InheritanceFlags]"ContainerInherit,ObjectInherit"
    $propagate = [Security.AccessControl.PropagationFlags]::None
    $allow = [Security.AccessControl.AccessControlType]::Allow
    foreach ($sid in @($owner, [Security.Principal.SecurityIdentifier]::new("S-1-5-18"))) {
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid, "FullControl", $inherit, $propagate, $allow))
    }
    # Set only the modified DACL; Set-Acl can also request audit privileges on Windows.
    [IO.Directory]::SetAccessControl($Path, $acl)
}

function Write-PrivateFile([string]$Path, [string]$Content) {
    if ((Test-Path -LiteralPath $Path) -and
        ((Get-Item -LiteralPath $Path).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Refusing a linked private file: $Path"
    }
    [IO.File]::WriteAllText($Path, $Content, [Text.UTF8Encoding]::new($false))
}

function New-Secret {
    $bytes = [byte[]]::new(32)
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    try { $rng.GetBytes($bytes) } finally { $rng.Dispose() }
    return ([BitConverter]::ToString($bytes)).Replace("-", "").ToLowerInvariant()
}

function Get-FreePort {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    try { $listener.Start(); return ([Net.IPEndPoint]$listener.LocalEndpoint).Port }
    finally { $listener.Stop() }
}

function Get-InstalledAegis {
    if ($AegisBinary) { return (Resolve-Path -LiteralPath $AegisBinary).Path }
    if ($env:ARUN_BINARY) { return (Resolve-Path -LiteralPath $env:ARUN_BINARY).Path }
    $launcher = (Get-Command aegis.cmd -ErrorAction Stop).Source
    $vendor = Join-Path (Split-Path $launcher -Parent) "node_modules\aegis-arun\vendor\x86_64-pc-windows-msvc\arun.exe"
    if (-not (Test-Path -LiteralPath $vendor -PathType Leaf)) {
        throw "Pass -AegisBinary with the installed native Aegis executable."
    }
    return (Resolve-Path -LiteralPath $vendor).Path
}

function Invoke-WithEnvironment([hashtable]$Values, [scriptblock]$Body) {
    $previous = @{}
    foreach ($name in $Values.Keys) {
        $previous[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
        [Environment]::SetEnvironmentVariable($name, [string]$Values[$name], "Process")
    }
    try { & $Body }
    finally {
        foreach ($name in $Values.Keys) { [Environment]::SetEnvironmentVariable($name, $previous[$name], "Process") }
    }
}

function Get-Environment {
    $device = "aegis-$($state.installation_id)"
    $selfOwner = if ($state.PSObject.Properties.Name -contains "self_account_owner") { [string]$state.self_account_owner } else { "" }
    return @{
        LOCAL_REMOTE_DIR = $stateDirectory.Replace("\", "/")
        RELAY_DATABASE_PORT = [string]$state.ports.database
        RELAY_DATABASE_PASSWORD = $state.secrets.relay_database
        EVOLUTION_DATABASE_PASSWORD = $state.secrets.evolution_database
        NATS_PORT = [string]$state.ports.nats
        EVOLUTION_PORT = [string]$state.ports.evolution
        EVOLUTION_API_KEY = $state.secrets.evolution_api
        NATS_AUTH_TOKEN = $state.secrets.nats_relay
        AEGIS_NATS_DEVICE_USERNAME = $device
        AEGIS_NATS_DEVICE_PASSWORD = $state.secrets.nats_device
        AEGIS_NATS_DEVICE_EVENT_SUBJECT = "aegis.events.$($state.installation_id)"
        AEGIS_NATS_DEVICE_CONSUMER_INFO = ('"$JS.API.CONSUMER.INFO.AEGIS_COMMANDS.{0}"' -f $device)
        AEGIS_NATS_DEVICE_CONSUMER_CREATE = ('"$JS.API.CONSUMER.CREATE.AEGIS_COMMANDS.{0}.aegis.commands.{1}"' -f $device, $state.installation_id)
        AEGIS_NATS_DEVICE_CONSUMER_NEXT = ('"$JS.API.CONSUMER.MSG.NEXT.AEGIS_COMMANDS.{0}"' -f $device)
        AEGIS_NATS_DEVICE_ACK = ('"$JS.ACK.AEGIS_COMMANDS.{0}.*.*.*.*.*"' -f $device)
        AEGIS_NATS_DEVICE_INBOX = "_INBOX.aegis.device.$($state.installation_id).>"
        DATABASE_URL = "postgres://relay:$($state.secrets.relay_database)@127.0.0.1:$($state.ports.database)/aegis_relay?sslmode=disable"
        RELAY_ALLOW_INSECURE_LOCAL_DATABASE = "true"
        RELAY_BIND_ADDR = "127.0.0.1:$($state.ports.relay)"
        NATS_URL = "tls://localhost:$($state.ports.nats)"
        NATS_TLS_ROOT_CERT = (Join-Path $stateDirectory "nats-certs\nats.crt")
        RELAY_ADMIN_TOKEN = $state.secrets.relay_admin
        EVOLUTION_BASE_URL = "http://127.0.0.1:$($state.ports.evolution)/"
        EVOLUTION_INSTANCE = $state.instance
        EVOLUTION_WEBHOOK_SECRET = $state.secrets.webhook
        WHATSAPP_SELF_ACCOUNT = $(if ($selfOwner) { "true" } else { "false" })
        WHATSAPP_SELF_ACCOUNT_PENDING = $(if ($selfOwner) { "false" } else { "true" })
        WHATSAPP_SELF_OWNER_PHONE = $selfOwner
        AEGIS_LOCAL_RELAY_ADMIN_TOKEN = $state.secrets.relay_admin
        AEGIS_LOCAL_NATS_TOKEN = $state.secrets.nats_device
        RUST_LOG = "info"
    }
}

function Invoke-Compose([string[]]$Arguments) {
    Invoke-WithEnvironment (Get-Environment) {
        & docker compose --project-name $state.project --env-file $envFile -f $composeFile @Arguments
        if ($LASTEXITCODE -ne 0) { throw "Local Docker Compose command failed ($LASTEXITCODE)." }
    }
}

function Invoke-Evolution([string]$Method, [string]$Route, $Body = $null) {
    $request = @{ Method = $Method; Uri = "http://127.0.0.1:$($state.ports.evolution)/$Route";
        Headers = @{ apikey = $state.secrets.evolution_api }; TimeoutSec = 20 }
    if ($null -ne $Body) { $request.ContentType = "application/json"; $request.Body = ($Body | ConvertTo-Json -Depth 8 -Compress) }
    Invoke-RestMethod @request
}

function Get-LocalInstance {
    foreach ($instance in @(Invoke-Evolution "GET" "instance/fetchInstances")) {
        foreach ($field in @("name", "instanceName")) {
            $property = $instance.PSObject.Properties[$field]
            if ($property -and $property.Value -eq $state.instance) { return $instance }
        }
    }
    return $null
}

function Get-OwnedProcess([string]$Name) {
    $recordPath = Join-Path $stateDirectory "$Name.process.json"
    if (-not (Test-Path -LiteralPath $recordPath -PathType Leaf)) { return $null }
    $record = Get-Content -LiteralPath $recordPath -Raw | ConvertFrom-Json
    $process = Get-Process -Id $record.process_id -ErrorAction SilentlyContinue
    if ($process -and $process.Path -ceq $record.executable -and
        $process.StartTime.ToUniversalTime().Ticks.ToString() -ceq $record.started_ticks) { return $process }
    return $null
}

function Start-OwnedProcess([string]$Name, [string]$Executable, [string[]]$Arguments) {
    if (Get-OwnedProcess $Name) { Write-Host "$Name already running."; return }
    $process = Invoke-WithEnvironment (Get-Environment) {
        $options = @{ FilePath = $Executable; WorkingDirectory = $Workspace; WindowStyle = "Hidden"; PassThru = $true;
            RedirectStandardOutput = (Join-Path $stateDirectory "$Name.stdout.log");
            RedirectStandardError = (Join-Path $stateDirectory "$Name.stderr.log") }
        if ($Arguments.Count -gt 0) { $options.ArgumentList = $Arguments }
        Start-Process @options
    }
    Write-PrivateFile (Join-Path $stateDirectory "$Name.process.json") (@{
        process_id = $process.Id; executable = $Executable; started_ticks = $process.StartTime.ToUniversalTime().Ticks.ToString()
    } | ConvertTo-Json)
}

function Wait-Relay {
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (-not (Get-OwnedProcess "relay")) { throw "Relay stopped; inspect the private relay.stderr.log." }
        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$($state.ports.relay)/healthz" -TimeoutSec 2
            if ($response.StatusCode -eq 200 -and $response.Content -eq "ok") { return }
        }
        catch { }
        Start-Sleep -Milliseconds 250
    }
    throw "Relay health timed out; inspect the private relay logs."
}

function Set-Webhook {
    Wait-Relay
    # Docker Desktop routes this name to Windows. Verify the actual route, never broaden the bind.
    $check = "fetch('http://host.docker.internal:$($state.ports.relay)/healthz').then(async r=>process.exit(r.ok&&(await r.text())==='ok'?0:1)).catch(()=>process.exit(1))"
    Invoke-Compose @("exec", "-T", "evolution", "node", "-e", $check)
    $body = @{ webhook = @{ enabled = $true; byEvents = $false; base64 = $false;
        url = "http://host.docker.internal:$($state.ports.relay)/v1/webhooks/whatsapp/evolution";
        headers = @{ "x-aegis-webhook-token" = $state.secrets.webhook }; events = @("MESSAGES_UPSERT") } }
    $null = Invoke-Evolution "POST" "webhook/set/$($state.instance)" $body
    Write-Host "Authenticated Evolution webhook registered."
}

if ($Action -eq "init") {
    $aegisDirectory = Join-Path $Workspace ".arun"
    if ((Test-Path -LiteralPath $aegisDirectory) -and
        ((Get-Item -LiteralPath $aegisDirectory).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Aegis workspace data directory cannot be a link."
    }
    if (-not (Test-Path -LiteralPath $aegisDirectory)) { New-Item -ItemType Directory -Path $aegisDirectory | Out-Null }
    Protect-Directory $stateDirectory
    if (Test-Path -LiteralPath $stateFile) { Write-Host "Local setup already initialized; credentials preserved."; return }
    $nativeAegis = Get-InstalledAegis
    Push-Location -LiteralPath $Workspace
    try {
        $installation = (& $nativeAegis remote identity | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw "Installed Aegis could not return its identity." }
    }
    finally { Pop-Location }
    $parsed = [guid]::Empty
    if (-not [guid]::TryParseExact($installation, "D", [ref]$parsed) -or $parsed.ToString("D") -cne $installation) {
        throw "Installed Aegis did not return a canonical installation UUID."
    }
    $ports = @()
    while ($ports.Count -lt 4) { $candidate = Get-FreePort; if ($candidate -notin $ports) { $ports += $candidate } }
    $state = @{ version = 1; project = "aegis-local-$($parsed.ToString('N'))"; installation_id = $installation;
        workspace = $Workspace; aegis_binary = $nativeAegis; instance = "aegis-local";
        ports = @{ database = $ports[0]; nats = $ports[1]; evolution = $ports[2]; relay = $ports[3] };
        secrets = @{ relay_database = New-Secret; evolution_database = New-Secret; nats_relay = New-Secret;
            nats_device = New-Secret; relay_admin = New-Secret; evolution_api = New-Secret; webhook = New-Secret } }
    Write-PrivateFile $stateFile ($state | ConvertTo-Json -Depth 5)
    Write-Host "Initialized local remote setup for installed Aegis. Credentials are private and excluded from Git."
    return
}

if (-not (Test-Path -LiteralPath $stateFile -PathType Leaf)) { throw "Run local-remote.ps1 init first." }
foreach ($path in @((Join-Path $Workspace ".arun"), $stateFile)) {
    if ((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Local remote state cannot be a link." }
}
Protect-Directory $stateDirectory
$state = Get-Content -LiteralPath $stateFile -Raw | ConvertFrom-Json
if ($state.version -ne 1 -or $state.workspace -cne $Workspace -or $state.project -notmatch '^aegis-local-[0-9a-f]{32}$') {
    throw "Local remote state does not match this workspace."
}
$values = Get-Environment
$lines = foreach ($name in ($values.Keys | Sort-Object)) {
    $value = [string]$values[$name]
    if ($value.Contains("'") -or $value.Contains("`n") -or $value.Contains("`r")) { throw "Invalid local environment value." }
    "$name='$value'"
}
Write-PrivateFile $envFile ($lines -join "`n")

switch ($Action) {
    "start" {
        $certDirectory = Join-Path $stateDirectory "nats-certs"
        Protect-Directory $certDirectory
        if (-not (Test-Path -LiteralPath (Join-Path $certDirectory "nats.crt"))) {
            & (Join-Path $PSScriptRoot "make-nats-certs.ps1") -OutputDirectory $certDirectory
            if ($LASTEXITCODE -ne 0) { throw "NATS certificate generation failed." }
        }
        Invoke-Compose @("config", "--quiet")
        if (-not (Get-OwnedProcess "relay") -and -not $RelayBinary -and
            $nativeRelay -ceq (Join-Path $repoRoot "relay\target\debug\aegis-relay.exe")) {
            if (-not (Test-Path -LiteralPath (Join-Path $repoRoot "relay\Cargo.toml"))) { throw "Install the native relay executable or pass -RelayBinary." }
            & cargo build --offline --manifest-path (Join-Path $repoRoot "relay\Cargo.toml") --config profile.dev.debug=0 -j 1
            if ($LASTEXITCODE -ne 0) { throw "Relay build failed." }
        }
        Invoke-Compose @("up", "-d", "--wait", "--wait-timeout", "180")
        Start-OwnedProcess "relay" $nativeRelay @()
        Wait-Relay
        Write-Host "Local services ready. Run create-instance, then qr to prepare phone linking."
    }
    "stop" {
        foreach ($name in @("daemon", "relay")) {
            $owned = Get-OwnedProcess $name
            if ($owned) { Stop-Process -Id $owned.Id -Force; $owned.WaitForExit() }
        }
        Invoke-Compose @("stop")
        Write-Host "Local services stopped; credentials, WhatsApp session and volumes preserved."
    }
    "status" {
        Invoke-Compose @("ps")
        foreach ($name in @("relay", "daemon")) { Write-Host "$name running: $([bool](Get-OwnedProcess $name))" }
        Write-Host "Evolution: http://127.0.0.1:$($state.ports.evolution)"
        Write-Host "Relay: http://127.0.0.1:$($state.ports.relay)/healthz"
        Write-Host "NATS: tls://localhost:$($state.ports.nats)"
        try { $connection = Invoke-Evolution "GET" "instance/connectionState/$($state.instance)"; Write-Host "WhatsApp state: $($connection.instance.state)" }
        catch { Write-Host "WhatsApp instance not ready or not created." }
    }
    "create-instance" {
        if (-not (Get-LocalInstance)) {
            $null = Invoke-Evolution "POST" "instance/create" @{ instanceName = $state.instance; integration = "WHATSAPP-BAILEYS";
                qrcode = $false; groupsIgnore = $false; readMessages = $false; readStatus = $false; syncFullHistory = $false }
        }
        Set-Webhook
        Write-Host "Aegis WhatsApp instance exists. It is not linked until the QR is scanned."
    }
    "webhook" { Set-Webhook }
    "self-account" {
        $instance = Get-LocalInstance
        if (-not $instance) { throw "Create and link the Evolution instance first." }
        $ownerProperty = $instance.PSObject.Properties["ownerJid"]
        $connectionProperty = $instance.PSObject.Properties["connectionStatus"]
        if (-not $ownerProperty -or -not $ownerProperty.Value -or
            -not $connectionProperty -or $connectionProperty.Value -ne "open") {
            throw "The WhatsApp account is not linked yet. Scan a fresh QR before enabling self-account mode."
        }
        $ownerJid = [string]$ownerProperty.Value
        if ($ownerJid -notmatch '^(\d{8,15})(?::\d+)?@s\.whatsapp\.net$') { throw "Evolution did not report a canonical owner phone JID." }
        $ownerPhone = "+$($Matches[1])"
        $state | Add-Member -NotePropertyName self_account_owner -NotePropertyValue $ownerPhone -Force
        Write-PrivateFile $stateFile ($state | ConvertTo-Json -Depth 5)
        $owned = Get-OwnedProcess "relay"
        if ($owned) { Stop-Process -Id $owned.Id -Force; $owned.WaitForExit() }
        Start-OwnedProcess "relay" $nativeRelay @()
        Wait-Relay
        Set-Webhook
        Write-Host "Self-account mode enabled for the linked owner. Use WhatsApp Message yourself, then the known Aegis task groups."
    }
    "qr" {
        $base64 = $null
        for ($attempt = 0; $attempt -lt 20; $attempt++) {
            $response = Invoke-Evolution "GET" "instance/connect/$($state.instance)"
            if ($response.PSObject.Properties.Name -contains "base64") { $base64 = $response.base64; if ($base64) { break } }
            Start-Sleep -Milliseconds 500
        }
        if (-not $base64 -or $base64 -notmatch '^data:image/png;base64,([A-Za-z0-9+/=]+)$') {
            throw "No fresh QR available. Check status; an already linked instance needs no QR."
        }
        [IO.File]::WriteAllBytes((Join-Path $stateDirectory "whatsapp-qr.png"), [Convert]::FromBase64String($Matches[1]))
        $html = '<!doctype html><meta charset="utf-8"><title>Aegis WhatsApp linking</title><style>body{font:18px system-ui;background:#111;color:#eee;text-align:center;padding:40px}img{background:white;padding:20px;max-width:80vw}</style><h1>Link WhatsApp to Aegis</h1><p>WhatsApp &rarr; Settings &rarr; Linked devices &rarr; Link a device.</p><img alt="WhatsApp linking QR" src="' + $base64 + '"><p>This QR expires. Run the qr command again for a fresh one.</p>'
        Write-PrivateFile (Join-Path $stateDirectory "whatsapp-qr.html") $html
        Write-Host "Fresh QR saved: $(Join-Path $stateDirectory 'whatsapp-qr.html')"
    }
    "pair" {
        Wait-Relay
        Push-Location -LiteralPath $Workspace
        try {
            Invoke-WithEnvironment $values {
                & $state.aegis_binary remote pair --relay-admin-url "http://127.0.0.1:$($state.ports.relay)" `
                    --admin-token-env AEGIS_LOCAL_RELAY_ADMIN_TOKEN --nats-url "tls://localhost:$($state.ports.nats)" `
                    --nats-token-env AEGIS_LOCAL_NATS_TOKEN --nats-root-cert (Join-Path $stateDirectory "nats-certs\nats.crt")
                if ($LASTEXITCODE -ne 0) { throw "Installed Aegis pairing failed." }
            }
        }
        finally { Pop-Location }
    }
    "daemon" {
        Wait-Relay
        if (-not (Test-Path -LiteralPath (Join-Path $Workspace ".arun\remote.json"))) { throw "Run pair before daemon." }
        Start-OwnedProcess "daemon" $state.aegis_binary @("remote", "run")
        Write-Host "Aegis remote daemon started with local device credentials."
    }
}
