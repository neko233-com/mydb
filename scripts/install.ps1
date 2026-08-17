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
    .\install.ps1 -Quiet -PackagePath .\mydb-windows-x86_64.zip
#>

param(
    [ValidateSet("server", "cli", "migrate", "dump", "all")]
    [string]$Component = "all",

    [string]$Version = "latest",

    [string]$PackagePath = "",

    [string]$InstallDir = "C:\Server\mydb\mydb-server",

    [string]$ConfigDir = "C:\Server\mydb\mydb-server\config",

    [string]$DataDir = "C:\Server\mydb\mydb-server\data",

    [string]$ServiceName = "MyDBServer",

    [switch]$NoPath,

    [switch]$NoService,

    [switch]$Quiet,

    [switch]$BinariesOnly,

    [uint32]$WaitForProcessId = 0,

    [string]$UpdateTempRoot = "",

    [switch]$NoRemoteFirewall
)

$ErrorActionPreference = "Stop"
$repo = "neko233-com/mydb"
$firewallRules = @(
    @{ Name = "MyDB Server (MySQL TCP 3306)"; Port = 3306; Label = "MySQL-compatible TCP 3306" },
    @{ Name = "MyDB Web Admin (HTTP TCP 4306)"; Port = 4306; Label = "Web admin TCP 4306" }
)

function Write-Info { if (-not $Quiet) { Write-Host "[INFO] $args" -ForegroundColor Blue } }
function Write-Success { if (-not $Quiet) { Write-Host "[OK] $args" -ForegroundColor Green } }
function Write-Warn { if (-not $Quiet) { Write-Host "[WARN] $args" -ForegroundColor Yellow } }
function Stop-WithError {
    param([string]$Message)
    if (-not $Quiet) {
        Write-Host "[ERROR] $Message" -ForegroundColor Red
    }
    exit 1
}

function Get-Sha256Hex {
    param([Parameter(Mandatory = $true)][string]$Path)
    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $bytes = $sha256.ComputeHash($stream)
    } finally {
        $stream.Dispose()
        $sha256.Dispose()
    }
    (($bytes | ForEach-Object { $_.ToString("x2") }) -join "").ToLowerInvariant()
}

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
    if ($Quiet) {
        & $FilePath @ArgumentList *> $null
    } else {
        & $FilePath @ArgumentList
    }
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
        $checksumSource = "${PackagePath}.sha256"
        if ([string]::IsNullOrWhiteSpace($PackagePath)) {
            $checksumSource = Get-ReleaseUrl "$packageName.zip.sha256"
            $checksumFile = Join-Path $tempRoot "$packageName.zip.sha256"
            Invoke-WebRequest -Uri $checksumSource -OutFile $checksumFile -UseBasicParsing
        } elseif (Test-Path -LiteralPath $checksumSource -PathType Leaf) {
            $checksumFile = Join-Path $tempRoot "$packageName.zip.sha256"
            Copy-Item -LiteralPath $checksumSource -Destination $checksumFile -Force
        } else {
            $checksumFile = $null
            Write-Warn "Local package has no SHA-256 sidecar: $PackagePath"
        }
        if ($null -ne $checksumFile) {
            $expectedHash = ((Get-Content -LiteralPath $checksumFile -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
            $actualHash = Get-Sha256Hex -Path $zipFile
            if ($expectedHash -ne $actualHash) {
                Stop-WithError "SHA-256 verification failed for $PackagePath"
            }
            Write-Success "SHA-256 verified: $packageName.zip"
        }
        Expand-Archive -Path $zipFile -DestinationPath $tempRoot -Force

        $sourceDir = Join-Path $tempRoot $packageName
        if (-not (Test-Path -LiteralPath $sourceDir)) {
            $sourceDir = $tempRoot
        }

        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        $sources = @{}
        foreach ($name in $Names) {
            $source = Join-Path $sourceDir $name
            if (-not (Test-Path -LiteralPath $source)) {
                Stop-WithError "$name missing from release package $packageName"
            }
            $sources[$name] = $source
        }

        $backupPaths = @{}
        $stagedPaths = @{}
        $replacedTargets = @()
        try {
            foreach ($name in $Names) {
                $target = Join-Path $InstallDir $name
                if (Test-Path -LiteralPath $target -PathType Leaf) {
                    $backup = Join-Path $tempRoot "backup-$name"
                    Copy-Item -LiteralPath $target -Destination $backup -Force
                    $backupPaths[$target] = $backup
                }
            }
            foreach ($name in $Names) {
                $target = Join-Path $InstallDir $name
                $staged = Join-Path $InstallDir ".mydb-update-$name"
                if (Test-Path -LiteralPath $staged) {
                    Remove-Item -LiteralPath $staged -Force
                }
                $stagedPaths[$name] = $staged
                Copy-Item -LiteralPath $sources[$name] -Destination $staged -Force
                Move-Item -LiteralPath $staged -Destination $target -Force
                $replacedTargets += $target
            }
        } catch {
            foreach ($target in $replacedTargets) {
                if ($backupPaths.ContainsKey($target)) {
                    Copy-Item -LiteralPath $backupPaths[$target] -Destination $target -Force -ErrorAction SilentlyContinue
                } else {
                    Remove-Item -LiteralPath $target -Force -ErrorAction SilentlyContinue
                }
            }
            foreach ($staged in $stagedPaths.Values) {
                Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
            }
            throw
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

function Wait-ForParentExit {
    if ($WaitForProcessId -eq 0) { return }
    for ($attempt = 0; $attempt -lt 240; $attempt++) {
        if ($null -eq (Get-Process -Id $WaitForProcessId -ErrorAction SilentlyContinue)) {
            return
        }
        Start-Sleep -Milliseconds 250
    }
    Stop-WithError "Timed out waiting for the mydb update command to exit"
}

function Update-BinariesOnly {
    Wait-ForParentExit
    $service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($null -ne $service -and $service.Status -ne "Stopped" -and -not (Test-IsAdministrator)) {
        $elevatedArguments = @(
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            "`"$PSCommandPath`"",
            "-BinariesOnly",
            "-PackagePath",
            "`"$PackagePath`"",
            "-InstallDir",
            "`"$InstallDir`"",
            "-ServiceName",
            "`"$ServiceName`"",
            "-WaitForProcessId",
            "0",
            "-UpdateTempRoot",
            "`"$UpdateTempRoot`""
        )
        $elevated = Start-Process -FilePath "powershell.exe" -Verb RunAs -ArgumentList $elevatedArguments -Wait -PassThru -WindowStyle Hidden
        if ($elevated.ExitCode -ne 0) {
            Stop-WithError "Elevated MyDB update helper failed with exit code $($elevated.ExitCode)."
        }
        return
    }
    $wasRunning = $null -ne $service -and $service.Status -ne "Stopped"
    try {
        if ($wasRunning) {
            Stop-MyDbService
        }
        Install-Binaries -Names @(
            "mydb-server.exe",
            "mydb-cli.exe",
            "mydb.exe",
            "mydb-migrate.exe",
            "mydbdump.exe"
        )
    } finally {
        if ($wasRunning) {
            Start-Service -Name $ServiceName
            (Get-Service -Name $ServiceName).WaitForStatus("Running", [TimeSpan]::FromSeconds(30))
        }
        if (-not [string]::IsNullOrWhiteSpace($UpdateTempRoot) -and (Test-Path -LiteralPath $UpdateTempRoot)) {
            Remove-Item -LiteralPath $UpdateTempRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    Write-Success "MyDB binaries updated in $InstallDir"
}

function Remove-LegacyRouter {
    $legacyService = Get-Service -Name "MyDBRouter" -ErrorAction SilentlyContinue
    if ($null -ne $legacyService) {
        if ($legacyService.Status -ne "Stopped") {
            Write-Info "Stopping legacy MyDBRouter service..."
            Stop-Service -Name "MyDBRouter" -Force
            (Get-Service -Name "MyDBRouter").WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
        }
        Invoke-CheckedNative "sc.exe" @("delete", "MyDBRouter") "Failed to remove legacy MyDBRouter service"
    }
    $legacyRule = Get-NetFirewallRule -DisplayName "MyDB Router (MySQL TCP 13306)" -ErrorAction SilentlyContinue
    if ($null -ne $legacyRule) {
        Remove-NetFirewallRule -DisplayName "MyDB Router (MySQL TCP 13306)"
    }
    foreach ($path in @(
        (Join-Path $InstallDir "mydb-router.exe"),
        (Join-Path $InstallDir "router.yaml")
    )) {
        if (Test-Path -LiteralPath $path) {
            Remove-Item -LiteralPath $path -Force
        }
    }
}

function New-RandomSecret {
    $bytes = New-Object byte[] 36
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $rng.GetBytes($bytes)
    } finally {
        $rng.Dispose()
    }
    return [Convert]::ToBase64String($bytes)
}

function Ensure-SecretFile {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        [IO.File]::WriteAllText($Path, (New-RandomSecret), [Text.UTF8Encoding]::new($false))
    }
    $acl = Get-Acl -LiteralPath $Path
    $acl.SetAccessRuleProtection($true, $false)
    $acl.Access | ForEach-Object { $acl.RemoveAccessRule($_) | Out-Null }
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule(
        [Security.Principal.NTAccount]::new($env:USERNAME), "FullControl", "Allow")))
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule(
        [Security.Principal.NTAccount]::new("SYSTEM"), "FullControl", "Allow")))
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule(
        [Security.Principal.NTAccount]::new("Administrators"), "FullControl", "Allow")))
    Set-Acl -LiteralPath $Path -AclObject $acl
}

function Ensure-Config {
    New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null
    New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
    $configFile = Join-Path $ConfigDir "config.yaml"
    if (Test-Path -LiteralPath $configFile) {
        Write-Info "Keeping existing config: $configFile"
        return
    }

    $secretsDir = Join-Path $ConfigDir "secrets"
    New-Item -ItemType Directory -Path $secretsDir -Force | Out-Null
    $rootSecret = Join-Path $secretsDir "root"
    $adminSecret = Join-Path $secretsDir "admin"
    Ensure-SecretFile $rootSecret
    Ensure-SecretFile $adminSecret

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
  host: "0.0.0.0"
  port: 4306
  admin_username: "admin"
  admin_password: "CHANGE_ME_USE_MYDB_ADMIN_PASSWORD_FILE"
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
  default_password: "CHANGE_ME_USE_MYDB_ROOT_PASSWORD_FILE"
  authentication: "caching_sha2_password"
  require_secure_transport: false
  tls_cert: null
  tls_key: null
  enforce_strong_passwords: true
  local_infile: false
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
    Write-Info "Generated MySQL and HTTP secrets: $secretsDir\root and $secretsDir\admin"
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

function Ensure-Firewall {
    if ($NoRemoteFirewall) { return }
    if (-not (Get-Command New-NetFirewallRule -ErrorAction SilentlyContinue)) {
        Write-Warn "New-NetFirewallRule unavailable; remote MySQL/Web firewall rules not created"
        return
    }
    foreach ($firewallRule in $firewallRules) {
        $rule = Get-NetFirewallRule -DisplayName $firewallRule.Name -ErrorAction SilentlyContinue
        if ($null -eq $rule) {
            New-NetFirewallRule -DisplayName $firewallRule.Name -Direction Inbound -Action Allow -Protocol TCP -LocalPort $firewallRule.Port -Profile Any | Out-Null
            Write-Success "Allowed inbound $($firewallRule.Label)"
        } else {
            Write-Info "Firewall rule already exists: $($firewallRule.Name)"
        }
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
        Invoke-CheckedNative "sc.exe" @("config", $ServiceName, "binPath=", $binPath, "start=", "auto") "Failed to update Windows service $ServiceName"
        Write-Info "Windows service updated: $ServiceName"
    }
    Invoke-CheckedNative "sc.exe" @("description", $ServiceName, "MySQL-compatible Neko233 database server") "Failed to update service description"
    $serviceEnvironment = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName"
    $secretsDir = Join-Path $ConfigDir "secrets"
    if (Test-Path -LiteralPath (Join-Path $secretsDir "root") -PathType Leaf) {
        New-ItemProperty -LiteralPath $serviceEnvironment -Name Environment -PropertyType MultiString -Value @(
            "MYDB_ROOT_PASSWORD_FILE=$(Join-Path $secretsDir 'root')",
            "MYDB_ADMIN_PASSWORD_FILE=$(Join-Path $secretsDir 'admin')",
            "MYDB_ENFORCE_STRONG_PASSWORDS=true"
        ) -Force | Out-Null
    }
    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus("Running", [TimeSpan]::FromSeconds(30))
    Write-Success "Windows service running and set to Automatic"
}

function Main {
    if (-not $BinariesOnly -and -not (Test-IsAdministrator)) {
        Stop-WithError "Run PowerShell as Administrator. Service/firewall setup requires elevation."
    }

    if ($BinariesOnly) {
        Update-BinariesOnly
        return
    }

    Remove-LegacyRouter

    $binaries = switch ($Component) {
        "server" { @("mydb-server.exe") }
        "cli" { @("mydb-cli.exe", "mydb.exe") }
        "migrate" { @("mydb-migrate.exe") }
        "dump" { @("mydbdump.exe") }
        default { @("mydb-server.exe", "mydb-cli.exe", "mydb.exe", "mydb-migrate.exe", "mydbdump.exe") }
    }

    if ($binaries -contains "mydb-server.exe") {
        Stop-MyDbService
    }
    Install-Binaries -Names $binaries
    Ensure-Config
    Ensure-Path

    if (($binaries -contains "mydb-server.exe") -and -not $NoService) {
        Ensure-Firewall
        Ensure-Service
    }
    if (-not $Quiet) {
        Write-Host ""
        Write-Success "MyDB install/update complete"
        Write-Host "Config: $ConfigDir\config.yaml"
        Write-Host "Data:   $DataDir"
        Write-Host "LAN MySQL endpoint: <server-ip>:3306, user root; generated password is in $ConfigDir\secrets\root"
        Write-Host "LAN Web endpoint: http://<server-ip>:4306/admin (firewall rule enabled unless -NoRemoteFirewall)"
    }
}

Main
