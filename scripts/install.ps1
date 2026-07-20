#Requires -Version 5.1
<#
.SYNOPSIS
    MyDB Installation Script for Windows

.DESCRIPTION
    Installs MyDB server and/or CLI on Windows systems.
    
.PARAMETER Component
    Components to install: server, cli, all (default: all)
    
.PARAMETER InstallDir
    Installation directory (default: $env:LOCALAPPDATA\MyDB)
    
.PARAMETER ConfigDir
    Configuration directory (default: $env:APPDATA\mydb)
    
.EXAMPLE
    .\install.ps1
    .\install.ps1 -Component server
    .\install.ps1 -Component cli -InstallDir "C:\MyDB"
#>

param(
    [ValidateSet("server", "cli", "migrate", "dump", "all")]
    [string]$Component = "all",

    [string]$Version = "latest",
    
    [string]$InstallDir = "$env:LOCALAPPDATA\MyDB",
    
    [string]$ConfigDir = "$env:APPDATA\mydb",
    
    [switch]$NoPath,

    [switch]$Service
)

$ErrorActionPreference = "Stop"

# Colors
function Write-Info { Write-Host "[INFO] $args" -ForegroundColor Blue }
function Write-Success { Write-Host "[OK] $args" -ForegroundColor Green }
function Write-Warn { Write-Host "[WARN] $args" -ForegroundColor Yellow }
function Write-Error { Write-Host "[ERROR] $args" -ForegroundColor Red; exit 1 }

# Detect architecture
function Get-Arch {
    $arch = [System.Environment]::GetEnvironmentVariable("PROCESSOR_ARCHITECTURE")
    switch ($arch) {
        "AMD64" { return "x86_64" }
        "ARM64" { return "aarch64" }
        default { Write-Error "Unsupported architecture: $arch" }
    }
}

# Download file
function Download-File {
    param(
        [string]$Url,
        [string]$OutFile
    )
    
    Write-Info "Downloading..."
    
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing
    } catch {
        Write-Error "Failed to download: $_"
    }
}

# Install component
function Install-Component {
    param(
        [string]$Name
    )
    
    $repo = "neko233-com/mydb"
    $arch = Get-Arch
    $os = "windows"
    
    $filename = "mydb-${os}-${arch}"
    $releaseBase = if ($Version -eq "latest") {
        "https://github.com/${repo}/releases/latest/download"
    } else {
        "https://github.com/${repo}/releases/download/${Version}"
    }
    $url = "${releaseBase}/${filename}.zip"
    
    $tmpDir = Join-Path $env:TEMP "mydb-install-$([guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null
    
    try {
        # Download
        $zipFile = Join-Path $tmpDir "${filename}.zip"
        Download-File -Url $url -OutFile $zipFile
        
        # Extract
        Expand-Archive -Path $zipFile -DestinationPath $tmpDir -Force
        
        # Move to install dir
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        
        $sourceDir = Join-Path $tmpDir $filename
        if (-not (Test-Path $sourceDir)) { $sourceDir = $tmpDir }
        $binaries = switch ($Name) {
            "server" { @("mydb-server.exe") }
            "cli" { @("mydb-cli.exe") }
            "migrate" { @("mydb-migrate.exe") }
            "dump" { @("mydbdump.exe") }
            default { @("mydb-server.exe", "mydb-cli.exe", "mydb-migrate.exe", "mydbdump.exe") }
        }
        foreach ($binary in $binaries) {
            $source = Join-Path $sourceDir $binary
            if (-not (Test-Path $source)) { Write-Error "$binary missing from release package" }
            Copy-Item $source $InstallDir -Force
        }
        
        Write-Success "$Name installed to $InstallDir"
    } finally {
        Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Create config
function New-Config {
    New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null
    
    $configFile = Join-Path $ConfigDir "config.yaml"
    
    if (-not (Test-Path $configFile)) {
        $config = @"
server:
  host: "0.0.0.0"
  port: 3306
  max_connections: 1000
  thread_count: 0

storage:
  data_dir: "$($ConfigDir -replace '\\', '/')/data"
  engine: "innodb"
  buffer_pool_size: "128M"
  page_size: 16384

security:
  authentication: "mysql_native_password"
  require_secure_transport: false

logging:
  level: "info"
  file: ""
"@
        Set-Content -Path $configFile -Value $config
        Write-Success "Config created at $configFile"
    }
}

# Create data directory
function New-DataDir {
    $dataDir = Join-Path $ConfigDir "data"
    New-Item -ItemType Directory -Path $dataDir -Force | Out-Null
    Write-Success "Data directory created at $dataDir"
}

# Setup PATH
function Set-Path {
    if ($NoPath) { return }
    
    $currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
    
    if ($currentPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$currentPath;$InstallDir", "User")
        Write-Success "Added to user PATH"
        Write-Warn "Restart your terminal or run: refreshenv"
    }
}

# Install as Windows Service
function Install-Service {
    Write-Info "Installing as Windows Service..."
    
    $serverPath = Join-Path $InstallDir "mydb-server.exe"
    $configPath = Join-Path $ConfigDir "config.yaml"
    
    if (-not (Test-Path $serverPath)) {
        Write-Error "mydb-server.exe not found at $serverPath"
    }
    
    # Create service
    $serviceName = "MyDBServer"
    $displayName = "MyDB Server"
    $description = "MySQL 8.x compatible database server"
    
    try {
        New-Service -Name $serviceName `
            -BinaryPathName "`"$serverPath`" --config `"$configPath`" --service run" `
            -DisplayName $displayName `
            -Description $description `
            -StartupType Automatic `
            -ErrorAction Stop
        
        Write-Success "Service '$serviceName' installed"
        Write-Info "Start with: Start-Service $serviceName"
    } catch {
        Write-Error "Failed to install service: $_"
    }
}

# Main
function Main {
    Write-Host "MyDB Installer" -ForegroundColor Cyan
    Write-Host "==================" -ForegroundColor Cyan
    Write-Host ""
    
    $arch = Get-Arch
    Write-Info "Architecture: $arch"
    
    switch ($Component) {
        "server" { Install-Component -Name "server" }
        "cli" { Install-Component -Name "cli" }
        "migrate" { Install-Component -Name "migrate" }
        "dump" { Install-Component -Name "dump" }
        "all" { Install-Component -Name "all" }
    }
    
    New-Config
    New-DataDir
    Set-Path
    if ($Service) { Install-Service }
    
    Write-Host ""
    Write-Success "Installation complete!"
    Write-Host ""
    Write-Host "Quick start:"
    Write-Host "  $InstallDir\mydb-server.exe --config $ConfigDir\config.yaml"
    Write-Host ""
    Write-Host "Connect with:"
    Write-Host "  $InstallDir\mydb-cli.exe -h 127.0.0.1 -P 3306 -u root"
    Write-Host "  $InstallDir\mydb-migrate.exe --help"
    Write-Host "  $InstallDir\mydbdump.exe --help"
}

Main
