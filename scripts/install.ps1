#Requires -Version 5.1
<##
.SYNOPSIS
    Idempotent MyDB Windows installer/updater.

.DESCRIPTION
    Installs the MySQL-wire-compatible server and client tools. The default
    layout is stable across updates: binaries, config, and data live below
    C:\Server\mydb\mydb-server. Existing config and data are never overwritten.
    The server is registered as an automatic Windows service by default.

.EXAMPLE
    .\install.ps1
    .\install.ps1 -Version v0.1.0
    .\install.ps1 -NoService -NoRemoteFirewall
    .\install.ps1 -Component server -InstallDir C:\Server\mydb\mydb-server
    .\install.ps1 -Component router -NoService
#>

param(
    [ValidateSet("server", "cli", "router", "migrate", "dump", "all")]
    [string]$Component = "all",

    [string]$Version = "latest",

    [string]$PackagePath = "",

    [string]$InstallDir = "C:\Server\mydb\mydb-server",

    [string]$ConfigDir = "C:\Server\mydb\mydb-server\config",

    [string]$DataDir = "C:\Server\mydb\mydb-server\data",

    [string]$ServiceName = "MyDBServer",

    [switch]$NoPath,

    [switch]$NoService,

    [switch]$NoRemoteFirewall
)

$ErrorActionPreference = "Stop"
$repo = "neko233-com/mydb"
$firewallRuleName = "MyDB Server (MySQL TCP 3306)"
$routerFirewallRuleName = "MyDB Router (MySQL TCP 13306)"
$routerServiceName = "MyDBRouter"

function Write-Info { Write-Host "[INFO] $args" -ForegroundColor Blue }
function Write-Success { Write-Host "[OK] $args" -ForegroundColor Green }
function Write-Warn { Write-Host "[WARN] $args" -ForegroundColor Yellow }
function Stop-WithError { param([string]$Message); Write-Host "[ERROR] $Message" -ForegroundColor Red; exit 1 }

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-Arch {
    $arch = [Environment]::GetEnvironmentVariable("PROCESSOR_ARCHITEW6432")
    if ([string]::IsNullOrWhiteSpace($arch)) {
        $arch = [Environment]::GetEnvironmentVariable("PROCESSOR_ARCHITECTURE")
    }
    switch ($arch) {
        "AMD64" { return "x86_64" }
        "ARM64" { return "aarch64" }
        default { Stop-WithError "Unsupported Windows architecture: $arch" }
    }
}

function Invoke-CheckedNative {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList,
        [string]$FailureMessage
    )
    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        Stop-WithError $FailureMessage
    }
}

function Get-ReleaseUrl {
    param([string]$FileName)
    $base = if ($Version -eq "latest") {
        "https://github.com/$repo/releases/latest/download"
    } else {
        "https://github.com/$repo/releases/download/$Version"
    }
    return "$base/$FileName"
}

function Install-Binaries {
    param([string[]]$Names)

    $arch = Get-Arch
    $packageName = "mydb-windows-$arch"
    $tempRoot = Join-Path $env:TEMP "mydb-install-$([guid]::NewGuid().ToString('N'))"
    $zipFile = Join-Path $tempRoot "$packageName.zip"
    New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null

    try {
        if ([string]::IsNullOrWhiteSpace($PackagePath)) {
            Write-Info "Downloading $packageName ($Version)..."
            [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
            $ProgressPreference = "SilentlyContinue"
            Invoke-WebRequest -Uri (Get-ReleaseUrl "$packageName.zip") -OutFile $zipFile -UseBasicParsing
        } else {
            if (-not (Test-Path -LiteralPath $PackagePath -PathType Leaf)) {
                Stop-WithError "Local package not found: $PackagePath"
            }
            Write-Info "Using local package: $PackagePath"
            Copy-Item -LiteralPath $PackagePath -Destination $zipFile -Force
        }
        Expand-Archive -Path $zipFile -DestinationPath $tempRoot -Force

        $sourceDir = Join-Path $tempRoot $packageName
        if (-not (Test-Path -LiteralPath $sourceDir)) {
            $sourceDir = $tempRoot
        }

        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        foreach ($name in $Names) {
            $source = Join-Path $sourceDir $name
            if (-not (Test-Path -LiteralPath $source)) {
                Stop-WithError "$name missing from release package $packageName"
            }
            $target = Join-Path $InstallDir $name
            Copy-Item -LiteralPath $source -Destination $target -Force
        }
        Write-Success "Binaries updated in $InstallDir"
    } finally {
        if (Test-Path -LiteralPath $tempRoot) {
            Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

function Stop-MyDbService {
    $service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($null -ne $service -and $service.Status -ne "Stopped") {
        Write-Info "Stopping $ServiceName before update..."
        Stop-Service -Name $ServiceName -Force
        $service.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
    }
}

function Stop-MyDbRouterService {
    $service = Get-Service -Name $routerServiceName -ErrorAction SilentlyContinue
    if ($null -ne $service -and $service.Status -ne "Stopped") {
        Write-Info "Stopping $routerServiceName before update..."
        Stop-Service -Name $routerServiceName -Force
        $service.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
    }
}

function Ensure-Config {
    New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null
    New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
    $configFile = Join-Path $ConfigDir "config.yaml"
    if (Test-Path -LiteralPath $configFile) {
        Write-Info "Keeping existing config: $configFile"
        return
    }

    $yamlDataDir = $DataDir -replace "\\", "/"
    $yaml = @"
# MyDB Windows local/remote configuration. Existing config is preserved on update.
server:
  host: "0.0.0.0"
  port: 3306
  max_connections: 1000
  thread_count: 0
  connect_timeout: 10
  interactive_timeout: 28800

http:
  host: "127.0.0.1"
  port: 4306
  admin_username: "root"
  admin_password: "root"
  enabled: true

storage:
  data_dir: "$yamlDataDir"
  engine: "neko233"
  buffer_pool_size: "800M"
  log_file_size: "256M"
  page_size: 16384
  group_commit_window_us: 250
  shard_count: 0

memory:
  max_memory: "1G"
  query_cache_size: "0"
  sort_buffer_size: "4M"

security:
  default_username: "root"
  default_password: "root"
  authentication: "mysql_native_password"
  require_secure_transport: false
  tls_cert: null
  tls_key: null
  enforce_strong_passwords: false
  local_infile: true
  secure_file_priv: "$yamlDataDir/imports"
  max_load_data_size: 1073741824

logging:
  level: "info"
  file: ""
  max_size: "100M"
  max_files: 10

character_set:
  server: "utf8mb4"
  connection: "utf8mb4"
  results: "utf8mb4"

agent:
  enabled: true
  slow_query_threshold_ms: 100
  max_slow_queries: 1024
"@
    Set-Content -LiteralPath $configFile -Value $yaml -Encoding UTF8
    New-Item -ItemType Directory -Path (Join-Path $DataDir "imports") -Force | Out-Null
    Write-Success "Config created: $configFile"
}

function Ensure-Path {
    if ($NoPath) { return }
    $current = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @($current -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if (-not ($entries | Where-Object { $_.TrimEnd("\") -ieq $InstallDir.TrimEnd("\") })) {
        [Environment]::SetEnvironmentVariable("Path", (($entries + $InstallDir) -join ";"), "User")
        Write-Success "Added $InstallDir to user PATH"
    }
}

function Ensure-RouterConfig {
    $routerConfig = Join-Path $InstallDir "router.yaml"
    if (Test-Path -LiteralPath $routerConfig) {
        Write-Info "Keeping existing router config: $routerConfig"
        return
    }
    @"
# Transparent MySQL wire router; each TCP session stays on one backend.
listen_host: "0.0.0.0"
listen_port: 13306
connect_timeout_ms: 2000
max_connections: 4096
tcp_nodelay: true
backends:
  - host: "127.0.0.1"
    port: 3306
    weight: 1
"@ | Set-Content -LiteralPath $routerConfig -Encoding UTF8
    Write-Success "Router config created: $routerConfig"
}

function Ensure-Firewall {
    if ($NoRemoteFirewall) { return }
    if (-not (Get-Command New-NetFirewallRule -ErrorAction SilentlyContinue)) {
        Write-Warn "New-NetFirewallRule unavailable; TCP 3306 firewall rule not created"
        return
    }
    $rule = Get-NetFirewallRule -DisplayName $firewallRuleName -ErrorAction SilentlyContinue
    if ($null -eq $rule) {
        New-NetFirewallRule -DisplayName $firewallRuleName -Direction Inbound -Action Allow -Protocol TCP -LocalPort 3306 -Profile Any | Out-Null
        Write-Success "Allowed inbound MySQL-compatible TCP 3306"
    } else {
        Write-Info "Firewall rule already exists: $firewallRuleName"
    }
}

function Ensure-RouterFirewall {
    if ($NoRemoteFirewall) { return }
    if (-not (Get-Command New-NetFirewallRule -ErrorAction SilentlyContinue)) {
        Write-Warn "New-NetFirewallRule unavailable; TCP 13306 firewall rule not created"
        return
    }
    $rule = Get-NetFirewallRule -DisplayName $routerFirewallRuleName -ErrorAction SilentlyContinue
    if ($null -eq $rule) {
        New-NetFirewallRule -DisplayName $routerFirewallRuleName -Direction Inbound -Action Allow -Protocol TCP -LocalPort 13306 -Profile Any | Out-Null
        Write-Success "Allowed inbound MySQL-compatible TCP 13306"
    } else {
        Write-Info "Router firewall rule already exists: $routerFirewallRuleName"
    }
}

function Ensure-Service {
    $serverPath = Join-Path $InstallDir "mydb-server.exe"
    $configPath = Join-Path $ConfigDir "config.yaml"
    if (-not (Test-Path -LiteralPath $serverPath)) {
        Stop-WithError "mydb-server.exe not found: $serverPath"
    }

    $binPath = '"' + $serverPath + '" --config "' + $configPath + '" --service run'
    $service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($null -eq $service) {
        New-Service -Name $ServiceName -BinaryPathName $binPath -DisplayName "MyDB Server" -Description "MySQL-compatible Neko233 database server" -StartupType Automatic | Out-Null
        Write-Success "Windows service created: $ServiceName"
    } else {
        Invoke-CheckedNative "sc.exe" @("config", $ServiceName, "binPath= $binPath", "start= auto") "Failed to update Windows service $ServiceName"
        Write-Info "Windows service updated: $ServiceName"
    }
    Invoke-CheckedNative "sc.exe" @("description", $ServiceName, "MySQL-compatible Neko233 database server") "Failed to update service description"
    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus("Running", [TimeSpan]::FromSeconds(30))
    Write-Success "Windows service running and set to Automatic"
}

function Ensure-RouterService {
    $routerPath = Join-Path $InstallDir "mydb-router.exe"
    $routerConfig = Join-Path $InstallDir "router.yaml"
    if (-not (Test-Path -LiteralPath $routerPath)) {
        Stop-WithError "mydb-router.exe not found: $routerPath"
    }
    if (-not (Test-Path -LiteralPath $routerConfig)) {
        Stop-WithError "router.yaml not found: $routerConfig"
    }

    $binPath = '"' + $routerPath + '" --service run --config "' + $routerConfig + '"'
    $service = Get-Service -Name $routerServiceName -ErrorAction SilentlyContinue
    if ($null -eq $service) {
        New-Service -Name $routerServiceName -BinaryPathName $binPath -DisplayName "MyDB Router" -Description "Transparent MySQL wire router for MyDB" -StartupType Automatic | Out-Null
        Write-Success "Windows service created: $routerServiceName"
    } else {
        Invoke-CheckedNative "sc.exe" @("config", $routerServiceName, "binPath= $binPath", "start= auto") "Failed to update Windows service $routerServiceName"
        Write-Info "Windows service updated: $routerServiceName"
    }
    Invoke-CheckedNative "sc.exe" @("description", $routerServiceName, "Transparent MySQL wire router for MyDB") "Failed to update router service description"
    Start-Service -Name $routerServiceName
    (Get-Service -Name $routerServiceName).WaitForStatus("Running", [TimeSpan]::FromSeconds(30))
    Write-Success "MyDB router running on TCP 13306"
}

function Main {
    if (-not (Test-IsAdministrator)) {
        Stop-WithError "Run PowerShell as Administrator. Service/firewall setup requires elevation."
    }

    $binaries = switch ($Component) {
        "server" { @("mydb-server.exe") }
        "cli" { @("mydb-cli.exe") }
        "router" { @("mydb-router.exe") }
        "migrate" { @("mydb-migrate.exe") }
        "dump" { @("mydbdump.exe") }
        default { @("mydb-server.exe", "mydb-cli.exe", "mydb-router.exe", "mydb-migrate.exe", "mydbdump.exe") }
    }

    if ($binaries -contains "mydb-server.exe") {
        Stop-MyDbService
    }
    if ($binaries -contains "mydb-router.exe") {
        Stop-MyDbRouterService
    }
    Install-Binaries -Names $binaries
    Ensure-Config
    if ($binaries -contains "mydb-router.exe") {
        Ensure-RouterConfig
    }
    Ensure-Path

    if (($binaries -contains "mydb-server.exe") -and -not $NoService) {
        Ensure-Firewall
        Ensure-Service
    }
    if (($binaries -contains "mydb-router.exe") -and -not $NoService) {
        Ensure-RouterFirewall
        Ensure-RouterService
    }

    Write-Host ""
    Write-Success "MyDB install/update complete"
    Write-Host "Config: $ConfigDir\config.yaml"
    Write-Host "Data:   $DataDir"
    Write-Host "JDBC/Go/Node endpoint: 127.0.0.1:3306, user root, password root"
    Write-Host "Remote endpoint: <server-ip>:3306 (firewall rule enabled unless -NoRemoteFirewall)"
    if ($binaries -contains "mydb-router.exe") {
        Write-Host "Router endpoint: <server-ip>:13306 (fixed-backend TCP session routing)"
    }
}

Main
