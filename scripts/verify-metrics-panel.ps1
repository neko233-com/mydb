#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$ServerExe = (Join-Path $PSScriptRoot '..\target\release\mydb-server.exe'),
    [string]$MetricsPanelExe = '',
    [string]$OutputDirectory = (Join-Path $PSScriptRoot ('..\target\monitoring-' + [guid]::NewGuid().ToString('N')))
)
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $root) { throw 'Use a new verification directory' }
New-Item -ItemType Directory -Path $root -Force | Out-Null
$identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
icacls $root /inheritance:r /grant:r "${identity}:(OI)(CI)F" '*S-1-5-18:(OI)(CI)F' | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot protect verification directory' }

function New-Secret { [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32)) }
function New-Port {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start(); $port = $listener.LocalEndpoint.Port; $listener.Stop(); return $port
}
function Check($condition, [string]$message) { if (-not $condition) { throw $message } }
function Request([string]$path, [string]$token = '', [string]$method = 'GET', $body = $null) {
    $headers = @{}
    if ($token) { $headers.Authorization = 'Bearer ' + $token }
    $args = @{ Uri = $path; Headers = $headers; Method = $method; SkipHttpErrorCheck = $true; TimeoutSec = 8 }
    if ($null -ne $body) { $args.ContentType = 'application/json'; $args.Body = ($body | ConvertTo-Json -Depth 40 -Compress) }
    Invoke-WebRequest @args
}

$scrape = New-Secret; $admin = New-Secret; $dbPassword = New-Secret
$tokenFile = Join-Path $root 'metrics.token'
Set-Content -LiteralPath $tokenFile -Value $scrape -Encoding ascii -NoNewline
$sqlPort = New-Port; $httpPort = New-Port
$base = 'http://127.0.0.1:' + $httpPort
$config = @{
    server = @{ host = '127.0.0.1'; port = $sqlPort; thread_count = 2 }
    http = @{ host = '127.0.0.1'; port = $httpPort; enabled = $true; admin_password = $admin }
    storage = @{ data_dir = (Join-Path $root 'data'); buffer_pool_size = '32M' }
    security = @{ default_password = $dbPassword; enforce_strong_passwords = $true; local_infile = $false }
    observability = @{ metrics_token_file = $tokenFile }
}
$configFile = Join-Path $root 'config.json'
$config | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $configFile -Encoding utf8
$server = $null; $panel = $null
try {
    $server = Start-Process -FilePath $ServerExe -ArgumentList @('--config', $configFile) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $root 'server.out.log') -RedirectStandardError (Join-Path $root 'server.err.log')
    $ready = $false
    for ($i = 0; $i -lt 80; $i++) {
        if ($server.HasExited) { throw 'Isolated MyDB exited before readiness; inspect protected local logs' }
        try { $response = Request ($base + '/metrics') $scrape; if ($response.StatusCode -eq 200) { $ready = $true; break } } catch { }
        Start-Sleep -Milliseconds 250
    }
    Check $ready 'Isolated MyDB did not become ready'
    Check ((Request ($base + '/metrics')).StatusCode -eq 401) 'Anonymous scrape must fail'
    Check ((Request ($base + '/metrics') ('x' * 64)).StatusCode -eq 401) 'Wrong scrape credential must fail'
    foreach ($path in @('/api/v1/status', '/api/v1/config', '/api/v1/backup/list')) {
        Check ((Request ($base + $path) $scrape).StatusCode -eq 401) 'Scrape credential must not read management APIs'
    }
    Check ((Request ($base + '/api/v1/sql/query') $scrape 'POST' @{sql='SELECT 1'}).StatusCode -eq 401) 'Scrape credential must not execute SQL'
    Check ((Request ($base + '/metrics') $scrape 'POST').StatusCode -eq 405) 'Metrics endpoint must reject mutation'
    $login = Request ($base + '/api/v1/auth/login') '' 'POST' @{username='root';password=$admin}
    Check ($login.StatusCode -eq 200) 'Administrator login failed'
    $session = ($login.Content | ConvertFrom-Json).token
    Check (($login.Content | ConvertFrom-Json).administrator -eq $true) 'HTTP administrator role was not established'
    foreach ($sql in @('CREATE DATABASE IF NOT EXISTS mydb', 'CREATE TABLE mydb.monitoring_smoke (id INT PRIMARY KEY, secret VARCHAR(100))', "INSERT INTO mydb.monitoring_smoke VALUES (1, 'private-monitoring-smoke-value')", 'SELECT * FROM mydb.monitoring_smoke')) {
        Check ((Request ($base + '/api/v1/sql/query') $session 'POST' @{sql=$sql}).StatusCode -eq 200) 'SQL workload failed'
    }
    $readerPassword = New-Secret
    foreach ($sql in @("CREATE USER 'monitoring_reader'@'%' IDENTIFIED BY '$readerPassword'", "GRANT SELECT ON mydb.* TO 'monitoring_reader'@'%'") ) {
        Check ((Request ($base + '/api/v1/sql/query') $session 'POST' @{sql=$sql}).StatusCode -eq 200) 'Cannot prepare restricted SQL principal'
    }
    $readerLogin = Request ($base + '/api/v1/auth/login') '' 'POST' @{username='monitoring_reader';password=$readerPassword}
    Check ($readerLogin.StatusCode -eq 200) 'Restricted SQL principal cannot sign in'
    $readerSession = ($readerLogin.Content | ConvertFrom-Json).token
    Check (($readerLogin.Content | ConvertFrom-Json).administrator -eq $false) 'SQL login unexpectedly acquired the administrator role'
    Check ((Request ($base + '/api/v1/sql/query') $readerSession 'POST' @{sql='SELECT * FROM mydb.monitoring_smoke'}).StatusCode -eq 200) 'Restricted SQL reader lost its SQL privileges'
    Check ((Request ($base + '/api/v1/sql/query') $readerSession 'POST' @{sql="INSERT INTO mydb.monitoring_smoke VALUES (2, 'denied-write')"}).StatusCode -eq 422) 'Restricted SQL reader unexpectedly gained write privileges'
    foreach ($path in @('/api/v1/config', '/api/v1/status', '/api/v1/backup/list')) {
        Check ((Request ($base + $path) $readerSession).StatusCode -eq 403) 'Restricted SQL principal unexpectedly has management access'
    }
    Check ((Request ($base + '/api/v1/backup/forbidden-probe') $readerSession 'DELETE').StatusCode -eq 403) 'Restricted SQL principal reached backup deletion'
    Check ((Request ($base + '/api/v1/backup/restore') $readerSession 'POST' @{id='forbidden-probe';confirmation='RESTORE_BACKUP:forbidden-probe'}).StatusCode -eq 403) 'Restricted SQL principal reached backup restoration'
    Check ((Request ($base + '/metrics') $readerSession).StatusCode -eq 403) 'Restricted SQL principal can read operational metrics without a scrape credential'
    Check ((Request ($base + '/api/v1/auth/logout') $readerSession 'POST').StatusCode -eq 200) 'SQL principal cannot sign out'
    Check ((Request ($base + '/api/v1/sql/query') $readerSession 'POST' @{sql='SELECT 1'}).StatusCode -eq 401) 'Signed-out SQL session remains valid'
    Check ((Request ($base + '/api/v1/auth/login') '' 'POST' @{username='root';password='invalid-verification-attempt'}).StatusCode -eq 401) 'Invalid login was accepted'
    $response = Request ($base + '/metrics') $scrape
    Check ($response.Content -match 'mydb_authentication_failures_total 1\b') 'Failed authentication was not observed'
    Check ($response.Content -match 'mydb_security_local_infile_enabled 0\b') 'Security configuration was not observed'
    foreach ($privateValue in @($admin, $dbPassword, $scrape, 'private-monitoring-smoke-value', 'monitoring_smoke')) {
        Check (-not $response.Content.Contains($privateValue)) 'Public metrics disclosed private SQL or credential data'
    }
    Set-Content -LiteralPath (Join-Path $root 'public-metrics.prom') -Value $response.Content -Encoding utf8
    $receipt = [ordered]@{ mydb_version = (& $ServerExe --version); authenticated_metrics = $true; admin_isolation = $true; sql_principal_management_isolation = $true; sql_privacy = $true; failed_logins_observed = $true }
    if ($MetricsPanelExe) {
        $panelPort = New-Port; $panelToken = New-Secret; $env:METRICSPANEL_TOKEN = $panelToken
        try { $panel = Start-Process -FilePath $MetricsPanelExe -ArgumentList @('serve','--listen',('127.0.0.1:'+$panelPort),'--db',(Join-Path $root 'panel.db')) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $root 'panel.out.log') -RedirectStandardError (Join-Path $root 'panel.err.log') } finally { Remove-Item Env:METRICSPANEL_TOKEN }
        $panelBase = 'http://127.0.0.1:' + $panelPort
        $panelReady = $false
        for ($i = 0; $i -lt 80; $i++) {
            try { if ((Request ($panelBase + '/api/health')).StatusCode -eq 200) { $panelReady = $true; break } } catch { }
            Start-Sleep -Milliseconds 250
        }
        Check $panelReady 'MetricsPanel233 did not become ready'
        $setup = @{ id=[guid]::NewGuid().ToString(); language='zh'; alerts=$true; target=@{name='MyDB verification';kind='mydb';url=($base+'/metrics');enabled=$true;interval_seconds=5;secure_settings=@{bearer_token=$scrape}} }
        $preview = Request ($panelBase+'/api/v1/connections/preview') $panelToken 'POST' @{setup=$setup}
        Check ($preview.StatusCode -eq 200) ('Native MyDB preview failed: HTTP ' + $preview.StatusCode + ' ' + $preview.Content)
        $plan = $preview.Content | ConvertFrom-Json
        Check ($plan.profile -eq 'mydb') 'Native MyDB profile was not selected'
        $apply = Request ($panelBase+'/api/v1/connections/apply') $panelToken 'POST' @{setup=$setup;plan_hash=$plan.plan_hash}
        Check ($apply.StatusCode -eq 200) 'Native MyDB apply failed'
        Start-Sleep -Seconds 12
        $query = Request ($panelBase+'/prometheus/api/v1/query?query=mydb_up') $panelToken
        Check ($query.StatusCode -eq 200) 'Grafana-compatible Prometheus query failed'
        $result = $query.Content | ConvertFrom-Json
        Check ($result.status -eq 'success' -and $result.data.result.Count -eq 1) 'Ordinary scrape did not reach Prometheus query API'
        Check ($result.data.result[0].value[1] -eq '1') 'MyDB availability value is incorrect'
        $receipt.metrics233_panels = $plan.dashboard.panels.Count
        $receipt.metrics233_rules = $plan.rules.Count
        $receipt.prometheus_query_api = $true
    }
    $receipt | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $root 'receipt.json') -Encoding utf8
    $receipt | ConvertTo-Json -Compress
} finally {
    foreach ($process in @($panel, $server)) { if ($null -ne $process -and -not $process.HasExited) { Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue; $process.WaitForExit(10000) | Out-Null } }
    # Remove only the credentials created by this run; retain diagnostics and receipt.
    foreach ($path in @($tokenFile,$configFile)) { if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force } }
}
