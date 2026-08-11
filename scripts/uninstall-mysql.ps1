#Requires -Version 5.1
<##
.SYNOPSIS
    Idempotently remove installed MySQL/MariaDB services and products.

.DESCRIPTION
    Never matches MyDB. Program directories are removed by default after the
    registered products are uninstalled. Database files are removed only with
    -PurgeData; migrate or back them up before using that switch.
#>

param(
    [switch]$PurgeData,
    [switch]$WhatIf
)

$ErrorActionPreference = "Stop"

function Write-Info { Write-Host "[INFO] $args" -ForegroundColor Blue }
function Write-Success { Write-Host "[OK] $args" -ForegroundColor Green }
function Write-Warn { Write-Host "[WARN] $args" -ForegroundColor Yellow }
function Stop-WithError { param([string]$Message); Write-Host "[ERROR] $Message" -ForegroundColor Red; exit 1 }

function Invoke-Removal {
    param([scriptblock]$Action, [string]$Description)
    if ($WhatIf) {
        Write-Info "WHATIF: $Description"
        return
    }
    & $Action
}

if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Stop-WithError "Run PowerShell as Administrator. MySQL service/product removal requires elevation."
}

$services = @(Get-CimInstance Win32_Service -ErrorAction SilentlyContinue | Where-Object {
    ($_.Name -match '(?i)mysql|maria' -or $_.DisplayName -match '(?i)mysql|maria') -and
    $_.Name -notmatch '(?i)mydb' -and $_.DisplayName -notmatch '(?i)mydb'
})
foreach ($service in $services) {
    if ($service.State -ne "Stopped") {
        Invoke-Removal { Stop-Service -Name $service.Name -Force -ErrorAction SilentlyContinue } "stop service $($service.Name)"
    }
    Invoke-Removal { & sc.exe delete $service.Name | Out-Null } "delete service $($service.Name)"
    Write-Info "Removed MySQL-compatible service: $($service.Name)"
}

$uninstallRoots = @(
    'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*'
)
$products = @(Get-ItemProperty $uninstallRoots -ErrorAction SilentlyContinue | Where-Object {
    $_.DisplayName -and
    $_.DisplayName -match '(?i)mysql|mariadb' -and
    $_.DisplayName -notmatch '(?i)mydb'
}) | Sort-Object PSPath -Unique

foreach ($product in $products) {
    $uninstall = if ($product.QuietUninstallString) { $product.QuietUninstallString } else { $product.UninstallString }
    if ([string]::IsNullOrWhiteSpace($uninstall)) {
        Write-Warn "No uninstall command for $($product.DisplayName); registry entry left intact"
        continue
    }
    if ($uninstall -match '(?i)msiexec(?:\.exe)?\s+/I\s*(\{[^}]+\})') {
        $guid = $Matches[1]
        Invoke-Removal { Start-Process -FilePath "msiexec.exe" -ArgumentList @('/x', $guid, '/qn', '/norestart') -Wait } "uninstall $($product.DisplayName)"
    } else {
        Invoke-Removal { Start-Process -FilePath "cmd.exe" -ArgumentList @('/d', '/s', '/c', "$uninstall /quiet /norestart") -Wait } "uninstall $($product.DisplayName)"
    }
    Write-Info "Uninstall requested: $($product.DisplayName)"
}

$programRoots = @(
    'C:\Program Files\MySQL',
    'C:\Program Files (x86)\MySQL',
    'C:\Program Files\MariaDB',
    'C:\Program Files (x86)\MariaDB',
    'C:\ProgramData\MySQL\MySQL Installer for Windows'
)
foreach ($root in $programRoots) {
    if (Test-Path -LiteralPath $root) {
        Invoke-Removal { Remove-Item -LiteralPath $root -Recurse -Force } "remove MySQL program path $root"
        Write-Info "Removed: $root"
    }
}

$dataRoots = @(
    'C:\ProgramData\MySQL',
    'C:\ProgramData\MariaDB'
)
foreach ($root in $dataRoots) {
    if (-not (Test-Path -LiteralPath $root)) { continue }
    if ($PurgeData) {
        Invoke-Removal { Remove-Item -LiteralPath $root -Recurse -Force } "purge MySQL data path $root"
        Write-Info "Purged: $root"
    } else {
        Write-Warn "MySQL data retained: $root (use -PurgeData after backup/migration)"
    }
}

if ($WhatIf) {
    Write-Success "MySQL removal plan complete (WHATIF; no changes made)"
} elseif ($PurgeData) {
    Write-Success "MySQL services, products, program files, and data removed"
} else {
    Write-Success "MySQL services, products, and program files removed; data retained"
}
