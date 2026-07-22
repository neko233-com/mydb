param(
    [switch]$SkipBuild,
    [string]$Url = "mysql://root:root@127.0.0.1:13307",
    [string]$MySqlUrl = "mysql://root:root@127.0.0.1:3306",
    [int]$WaitTimeoutSec = 120,
    [ValidateRange(1, 5)]
    [int]$Samples = 3
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
Set-Location $ProjectRoot

function Parse-BenchUrl([string]$Value, [string]$Name) {
    try {
        $uri = [Uri]$Value
    } catch {
        throw "$Name is not a valid URL: $Value"
    }
    $port = if ($uri.IsDefaultPort) { 3306 } else { $uri.Port }
    if ([string]::IsNullOrWhiteSpace($uri.Host) -or $port -le 0) {
        throw "$Name must include a host and port: $Value"
    }
    return [pscustomobject]@{ Url = $Value; Host = $uri.Host; Port = $port }
}

$MyDbTarget = Parse-BenchUrl $Url "Url"
$MySqlTarget = Parse-BenchUrl $MySqlUrl "MySqlUrl"
if ($MyDbTarget.Host -eq $MySqlTarget.Host -and $MyDbTarget.Port -eq $MySqlTarget.Port) {
    throw "MyDB and MySQL benchmark endpoints must differ"
}

$RunId = Get-Date -Format "yyyyMMdd-HHmmss"
$BenchRoot = Join-Path $ProjectRoot "target\bench"
$LogDir = Join-Path $BenchRoot "logs"
$DataDir = Join-Path $BenchRoot "native-$RunId"
$CheckpointCommitInterval = 1024
New-Item -ItemType Directory -Force -Path $BenchRoot, $LogDir | Out-Null

function Write-Step([string]$Message) {
    Write-Host "`n=== $Message ===" -ForegroundColor Cyan
}

function Invoke-External([string]$Name, [string]$Exe, [string[]]$ArgList, [string]$LogTag) {
    Write-Step $Name
    $outLog = Join-Path $LogDir "$LogTag.stdout"
    $errLog = Join-Path $LogDir "$LogTag.stderr"
    $process = Start-Process -FilePath $Exe -ArgumentList $ArgList -Wait -NoNewWindow -PassThru `
        -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    if ($process.ExitCode -ne 0) {
        if (Test-Path $errLog) { Get-Content $errLog -Tail 40 }
        if (Test-Path $outLog) { Get-Content $outLog -Tail 20 }
        throw "$Name failed with exit code $($process.ExitCode)"
    }
}

function Wait-Tcp([string]$ServerHost, [int]$ServerPort) {
    Write-Step "Waiting for $ServerHost`:$ServerPort"
    $deadline = (Get-Date).AddSeconds($WaitTimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $tcp = $null
        try {
            $tcp = [System.Net.Sockets.TcpClient]::new()
            $connect = $tcp.BeginConnect($ServerHost, $ServerPort, $null, $null)
            if ($connect.AsyncWaitHandle.WaitOne(500, $false) -and $tcp.Connected) {
                $tcp.EndConnect($connect)
                return
            }
        } catch {
        } finally {
            if ($null -ne $tcp) { $tcp.Dispose() }
        }
        Start-Sleep -Milliseconds 200
    }
    throw "Timed out waiting for $ServerHost`:$ServerPort"
}

function Convert-BenchOutput([object[]]$Output, [string]$Tag) {
    $raw = $Output | Out-String
    [System.IO.File]::WriteAllText((Join-Path $BenchRoot "$Tag.raw"), $raw, [System.Text.UTF8Encoding]::new($false))
    $start = $raw.IndexOf('{')
    $end = $raw.LastIndexOf('}')
    if ($start -lt 0 -or $end -le $start) {
        throw "Benchmark $Tag produced no JSON result: $raw"
    }
    $json = $raw.Substring($start, $end - $start + 1)
    [System.IO.File]::WriteAllText((Join-Path $BenchRoot "$Tag.json"), $json, [System.Text.UTF8Encoding]::new($false))
    try {
        return $json | ConvertFrom-Json
    } catch {
        throw "Benchmark $Tag returned invalid JSON: $($_.Exception.Message)"
    }
}

function Invoke-Bench([string]$Target, [string[]]$BenchArgs, [string]$Tag) {
    Write-Host "[$Target] $Tag" -ForegroundColor DarkGray
    $output = & $BenchExe @BenchArgs 2>&1
    if ($LASTEXITCODE -ne 0) {
        $output | ForEach-Object { Write-Host $_ }
        throw "$Target benchmark $Tag failed with exit code $LASTEXITCODE"
    }
    return Convert-BenchOutput -Output $output -Tag $Tag
}

function Get-Median([object[]]$Values) {
    $numbers = @($Values | ForEach-Object { [double]$_ } | Sort-Object)
    $middle = [int][Math]::Floor($numbers.Count / 2)
    if ($numbers.Count % 2 -eq 1) {
        return $numbers[$middle]
    }
    return ($numbers[$middle - 1] + $numbers[$middle]) / 2
}

function Summarize([object[]]$Results) {
    return [pscustomobject]@{
        operations_per_second = Get-Median -Values @($Results | ForEach-Object { $_.operations_per_second })
        write_transactions_p50_us = Get-Median -Values @($Results | ForEach-Object { $_.write_transactions_p50_us })
        write_transactions_p95_us = Get-Median -Values @($Results | ForEach-Object { $_.write_transactions_p95_us })
        write_transactions_p99_us = Get-Median -Values @($Results | ForEach-Object { $_.write_transactions_p99_us })
        reads_p50_us = Get-Median -Values @($Results | ForEach-Object { $_.reads_p50_us })
        verified_rows = Get-Median -Values @($Results | ForEach-Object { $_.verified_rows })
    }
}

function Invoke-Scenario([string]$Name, [string[]]$ScenarioArgs) {
    $mydbResults = @()
    $mysqlResults = @()
    for ($sample = 1; $sample -le $Samples; $sample++) {
        $mydbResults += Invoke-Bench "MyDB" (@("--url", $MyDbTarget.Url, "--reconnect-every-transactions", "0", "--payload-bytes", "256") + $ScenarioArgs) "mydb-$Name-$sample"
        $mysqlResults += Invoke-Bench "MySQL" (@("--url", $MySqlTarget.Url, "--reconnect-every-transactions", "0", "--payload-bytes", "256") + $ScenarioArgs) "mysql-$Name-$sample"
    }
    return [pscustomobject]@{
        mydb = Summarize -Results $mydbResults
        mysql = Summarize -Results $mysqlResults
    }
}

if (-not $SkipBuild) {
    Invoke-External "cargo check" "cargo.exe" @("check", "--workspace") "01_check"
    Invoke-External "cargo clippy" "cargo.exe" @("clippy", "--workspace", "--all-targets", "--", "-D", "warnings") "02_clippy"
    Invoke-External "cargo test" "cargo.exe" @("test", "--workspace") "03_test"
    Invoke-External "cargo build --release" "cargo.exe" @("build", "--release", "-p", "mydb-server", "-p", "mydb-bench") "04_build"
}

$BenchExe = Join-Path $ProjectRoot "target\release\mydb-bench.exe"
$ServerExe = Join-Path $ProjectRoot "target\release\mydb-server.exe"
if (-not (Test-Path $BenchExe) -or -not (Test-Path $ServerExe)) {
    throw "Release binaries are missing; run without -SkipBuild"
}

$httpPort = $MyDbTarget.Port + 1000
if ($httpPort -gt 65535) { throw "MyDB port leaves no valid HTTP benchmark port" }
$previousHttpPort = $env:MYDB_HTTP_PORT
$previousDataDir = $env:MYDB_DATA_DIR
$previousWindow = $env:MYDB_GROUP_COMMIT_WINDOW_US
$previousRootPassword = $env:MYDB_ROOT_PASSWORD
$previousAdminPassword = $env:MYDB_ADMIN_PASSWORD
$serverProcess = $null

try {
    $env:MYDB_HTTP_PORT = "$httpPort"
    $env:MYDB_DATA_DIR = $DataDir
    $env:MYDB_GROUP_COMMIT_WINDOW_US = "250"
    $env:MYDB_ROOT_PASSWORD = "root"
    $env:MYDB_ADMIN_PASSWORD = "root"
    $serverOut = Join-Path $LogDir "native-$RunId.stdout"
    $serverErr = Join-Path $LogDir "native-$RunId.stderr"

    Write-Step "Starting native unlimited MyDB"
    $serverProcess = Start-Process -FilePath $ServerExe -ArgumentList @("--config", "configs/default.yaml", "--port", "$($MyDbTarget.Port)", "--data-dir", $DataDir) `
        -WorkingDirectory $ProjectRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $serverOut -RedirectStandardError $serverErr
    Wait-Tcp $MyDbTarget.Host $MyDbTarget.Port
    Wait-Tcp $MySqlTarget.Host $MySqlTarget.Port

    $mysqlProbe = Invoke-Bench "MySQL" @("--url", $MySqlTarget.Url, "--probe-server") "mysql-probe"
    Write-Step "Warmup"
    $warmup = @("--actors", "8", "--table-count", "8", "--transaction-size", "10", "--writes-per-actor", "100", "--reads-per-actor", "0", "--write-mode", "actor-batch")
    [void](Invoke-Bench "MyDB" (@("--url", $MyDbTarget.Url, "--reconnect-every-transactions", "0", "--payload-bytes", "256") + $warmup) "mydb-warmup")
    [void](Invoke-Bench "MySQL" (@("--url", $MySqlTarget.Url, "--reconnect-every-transactions", "0", "--payload-bytes", "256") + $warmup) "mysql-warmup")

    Write-Step "Benchmark: single-table fsync-per-commit"
    $single = Invoke-Scenario "single" @("--actors", "1", "--table-count", "1", "--transaction-size", "1", "--writes-per-actor", "1000", "--reads-per-actor", "0")
    Write-Step "Benchmark: 8-actor / 8-table P99 latency"
    $concurrentP99 = Invoke-Scenario "concurrent-p99" @("--actors", "8", "--table-count", "8", "--transaction-size", "10", "--writes-per-actor", "500", "--reads-per-actor", "50")
    Write-Step "Benchmark: 8-actor / 8-table group throughput"
    $concurrentTp = Invoke-Scenario "concurrent-tp" @("--actors", "8", "--table-count", "8", "--transaction-size", "1", "--writes-per-actor", "1000", "--reads-per-actor", "0")

    $metrics = ""
    try {
        $metrics = (Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$httpPort/metrics" -TimeoutSec 5).Content
        [System.IO.File]::WriteAllText((Join-Path $BenchRoot "mydb-metrics-$RunId.prom"), $metrics, [System.Text.UTF8Encoding]::new($false))
    } catch {
        Write-Warning "Could not collect MyDB metrics: $($_.Exception.Message)"
    }

    function Get-Metric([string]$MetricsText, [string]$Name) {
        $match = [regex]::Match($MetricsText, "(?m)^$([regex]::Escape($Name))\s+([0-9]+(?:\.[0-9]+)?)$")
        if ($match.Success) { return $match.Groups[1].Value }
        return "n/a"
    }

    $groupCommits = Get-Metric -MetricsText $metrics -Name "mydb_group_commits_total"
    $groupedRequests = Get-Metric -MetricsText $metrics -Name "mydb_grouped_requests_total"
    $checkpoints = Get-Metric -MetricsText $metrics -Name "mydb_checkpoints_total"
    $walSyncMicros = Get-Metric -MetricsText $metrics -Name "mydb_wal_sync_microseconds_total"
    $checkpointMicros = Get-Metric -MetricsText $metrics -Name "mydb_checkpoint_microseconds_total"

    $singleMyDbOps = [math]::Round($single.mydb.operations_per_second, 0)
    $singleMySqlOps = [math]::Round($single.mysql.operations_per_second, 0)
    $p99MyDbMs = [math]::Round($concurrentP99.mydb.write_transactions_p99_us / 1000.0, 1)
    $p99MySqlMs = [math]::Round($concurrentP99.mysql.write_transactions_p99_us / 1000.0, 1)
    $tpMyDbOps = [math]::Round($concurrentTp.mydb.operations_per_second, 0)
    $tpMySqlOps = [math]::Round($concurrentTp.mysql.operations_per_second, 0)
    $readMyDbUs = [math]::Round($concurrentP99.mydb.reads_p50_us, 0)
    $readMySqlUs = [math]::Round($concurrentP99.mysql.reads_p50_us, 0)
    $singleRatio = [math]::Round($single.mydb.operations_per_second / $single.mysql.operations_per_second, 2)
    $p99Ratio = [math]::Round($concurrentP99.mysql.write_transactions_p99_us / $concurrentP99.mydb.write_transactions_p99_us, 2)
    $tpRatio = [math]::Round($concurrentTp.mydb.operations_per_second / $concurrentTp.mysql.operations_per_second, 2)
    $commitHash = (git rev-parse --short HEAD).Trim()
    $dateStr = Get-Date -Format "yyyy-MM-dd"
    $cpu = "{0}; {1} logical CPUs" -f $env:PROCESSOR_IDENTIFIER, [Environment]::ProcessorCount

    $bt = [char]96
    $report = @"
# MyDB 性能报告

> 本报告由 ${bt}scripts/bench.ps1${bt} 生成。MyDB 与 MySQL 均为同机实际服务、相同 ${bt}mydb-bench${bt} 负载；每项取 $Samples 次采样中位数。
>
> MyDB 使用本机 release 进程、独立临时数据目录、无 Docker CPU/内存限制；MySQL 使用 ${bt}$($MySqlTarget.Host):$($MySqlTarget.Port)${bt} 的实际服务。基准仅创建并删除唯一命名的 ${bt}mydb_game_bench_*${bt} 临时库。

---

## 测试方法

| 场景 | actors | tables | transaction_size | writes_per_actor | 说明 |
|------|--------|--------|------------------|------------------|------|
| 单表写 | 1 | 1 | 1 | 1000 | 单连接单行事务，每次 COMMIT 持久化 |
| 并发 P99 | 8 | 8 | 10 | 500 | 8 并发写、50 点查/actor |
| 并发吞吐 | 8 | 8 | 1 | 1000 | 8000 次单行事务，测真实 group commit |

## 当前实测

### Git Revision: ${bt}$commitHash${bt}
### 测试日期: $dateStr
### 测试环境: 本机 Windows；$cpu
### MySQL: $($mysqlProbe.version) — $($mysqlProbe.version_comment)
### MySQL 持久化设置: ${bt}innodb_flush_log_at_trx_commit=$($mysqlProbe.innodb_flush_log_at_trx_commit)${bt}, ${bt}sync_binlog=$($mysqlProbe.sync_binlog)${bt}, ${bt}transaction_isolation=$($mysqlProbe.transaction_isolation)${bt}
### MyDB 设置: ${bt}group_commit_window_us=250${bt}, checkpoint 每 $CheckpointCommitInterval 个已提交请求

| 场景 | MyDB | MySQL | MyDB / MySQL |
|------|------|-------|---------------|
| 单表写 | $singleMyDbOps ops/s | $singleMySqlOps ops/s | ${singleRatio}x |
| 8 actor / 8表 写 P99 | $p99MyDbMs ms | $p99MySqlMs ms | ${p99Ratio}x（低更好） |
| 8 actor / 8表 吞吐 | $tpMyDbOps ops/s | $tpMySqlOps ops/s | ${tpRatio}x |
| 读 P50 | $readMyDbUs μs | $readMySqlUs μs | - |

## MyDB 观测

- write groups: $groupCommits
- grouped requests: $groupedRequests
- checkpoints: $checkpoints
- WAL sync total: $walSyncMicros μs
- checkpoint total: $checkpointMicros μs

## 性能原则

- 已确认提交必须由一次 ${bt}sync_data()${bt} WAL durability boundary 覆盖；不以关闭持久化换吞吐。
- 默认 250μs Group Commit 窗口优先并发吞吐；${bt}0${bt} 是显式低延迟档，报告必须写明所用档位。
- checkpoint 按已提交请求数触发，不与合批大小耦合；崩溃恢复、${bt}flush_consistent${bt}、shutdown 仍强制落盘。
- MySQL 比较必须同机、实际运行、相同负载，并记录 MySQL 版本与持久化设置；禁止硬编码历史比值。
"@
    [System.IO.File]::WriteAllText((Join-Path $ProjectRoot "性能报告.md"), $report, [System.Text.UTF8Encoding]::new($false))
    Write-Host "`nPerformance report written to: $ProjectRoot\性能报告.md" -ForegroundColor Green
    Write-Host "MyDB/MySQL throughput: $tpMyDbOps / $tpMySqlOps ops/s" -ForegroundColor Yellow
} finally {
    if ($null -ne $serverProcess -and -not $serverProcess.HasExited) {
        Stop-Process -Id $serverProcess.Id -Force
    }
    $env:MYDB_HTTP_PORT = $previousHttpPort
    $env:MYDB_DATA_DIR = $previousDataDir
    $env:MYDB_GROUP_COMMIT_WINDOW_US = $previousWindow
    $env:MYDB_ROOT_PASSWORD = $previousRootPassword
    $env:MYDB_ADMIN_PASSWORD = $previousAdminPassword
}
