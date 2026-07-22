param(
    [switch]$SkipBuild,
    [switch]$SkipDocker,
    [string]$Url = "mysql://root:root@127.0.0.1:3306",
    [int]$WaitTimeoutSec = 120
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
Set-Location $ProjectRoot

try {
    $BenchUri = [Uri]$Url
} catch {
    Write-Host "FAILED: Invalid benchmark URL: $Url" -ForegroundColor Red
    exit 1
}
$BenchHost = $BenchUri.Host
$BenchPort = if ($BenchUri.IsDefaultPort) { 3306 } else { $BenchUri.Port }
if ([string]::IsNullOrWhiteSpace($BenchHost) -or $BenchPort -le 0) {
    Write-Host "FAILED: Benchmark URL must include a host and port: $Url" -ForegroundColor Red
    exit 1
}

$LogDir = "$ProjectRoot\target\bench\logs"
$TmpDir = "$ProjectRoot\target\bench"
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

function Write-Step($msg) {
    Write-Host "`n=== $msg ===" -ForegroundColor Cyan
}

function Invoke-External($name, $exe, [string[]]$argList, $logTag) {
    Write-Step $name
    $outLog = "$LogDir\$logTag.stdout"
    $errLog = "$LogDir\$logTag.stderr"
    $p = Start-Process -FilePath $exe -ArgumentList $argList -Wait -NoNewWindow `
        -RedirectStandardOutput $outLog -RedirectStandardError $errLog -PassThru
    if ($p.ExitCode -ne 0) {
        Write-Host "FAILED: $name (exit $($p.ExitCode))" -ForegroundColor Red
        if (Test-Path $errLog) {
            Write-Host "--- stderr (last 30 lines) ---" -ForegroundColor Red
            Get-Content $errLog -Tail 30
        }
        if (Test-Path $outLog) {
            Write-Host "--- stdout (last 10 lines) ---" -ForegroundColor Red
            Get-Content $outLog -Tail 10
        }
        exit $p.ExitCode
    }
    Write-Host "$name OK" -ForegroundColor Green
}

if (-not $SkipBuild) {
    Invoke-External "cargo check" "cargo.exe" @("check", "--workspace") "01_check"
    Invoke-External "cargo clippy" "cargo.exe" @("clippy", "--workspace", "--all-targets", "--", "-D", "warnings") "02_clippy"
    Invoke-External "cargo test" "cargo.exe" @("test", "--workspace") "03_test"
    Invoke-External "cargo build --release" "cargo.exe" @("build", "--release", "-p", "mydb-server", "-p", "mydb-bench") "04_build"
}

if (-not $SkipDocker) {
    foreach ($name in @("MYDB_ROOT_PASSWORD", "MYDB_ADMIN_PASSWORD")) {
        if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) {
            Write-Host "FAILED: $name must be set before Docker benchmark" -ForegroundColor Red
            exit 1
        }
    }
    if ([string]::IsNullOrWhiteSpace($env:MYDB_HOST_PORT)) {
        $env:MYDB_HOST_PORT = "$BenchPort"
    } elseif ([int]$env:MYDB_HOST_PORT -ne $BenchPort) {
        Write-Host "FAILED: MYDB_HOST_PORT must match the benchmark URL port $BenchPort" -ForegroundColor Red
        exit 1
    }

    Write-Step "docker compose up -d --build"
    docker compose up -d --build
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: docker compose up" -ForegroundColor Red
        exit 1
    }

    Write-Step "Waiting for MyDB on $BenchHost`:$BenchPort..."
    $deadline = (Get-Date).AddSeconds($WaitTimeoutSec)
    $ready = $false
    while ((Get-Date) -lt $deadline) {
        try {
            $tcp = New-Object System.Net.Sockets.TcpClient
            $iar = $tcp.BeginConnect($BenchHost, $BenchPort, $null, $null)
            if ($iar.AsyncWaitHandle.WaitOne(2000, $false) -and $tcp.Connected) {
                $tcp.EndConnect($iar)
                $tcp.Close()
                $ready = $true
                break
            }
            $tcp.Close()
        } catch {}
        Start-Sleep -Seconds 2
    }
    if (-not $ready) {
        Write-Host "FAILED: MyDB did not start within ${WaitTimeoutSec}s" -ForegroundColor Red
        docker compose logs --tail=30
        exit 1
    }
    Write-Host "MyDB is ready" -ForegroundColor Green
    Start-Sleep -Seconds 3
}

$BenchExe = if ($IsWindows -or (-not $IsLinux -and -not $IsMacOS)) {
    "$ProjectRoot\target\release\mydb-bench.exe"
} else {
    "$ProjectRoot/target/release/mydb-bench"
}

if (-not (Test-Path $BenchExe)) {
    Write-Host "Benchmark binary not found: $BenchExe" -ForegroundColor Red
    Write-Host "Run with -SkipBuild only after building with -SkipBuild=$false first." -ForegroundColor Red
    exit 1
}

function Run-Bench($scenario, [string[]]$benchArgs, $outFile) {
    Write-Step "Benchmark: $scenario"
    $jsonPath = "$TmpDir\$outFile"
    $allOutput = & $BenchExe @benchArgs 2>&1
    $allOutput | Out-File -FilePath "$jsonPath.raw" -Encoding utf8
    $jsonText = ($allOutput | Out-String)
    $start = $jsonText.IndexOf('{')
    $end = $jsonText.LastIndexOf('}')
    if ($start -lt 0 -or $end -le $start) {
        Write-Host "FAILED: Could not extract JSON from benchmark output" -ForegroundColor Red
        $allOutput | ForEach-Object { Write-Host $_ }
        exit 1
    }
    $jsonText = $jsonText.Substring($start, $end - $start + 1)
    try {
        $jsonText | ConvertFrom-Json | Out-Null
    } catch {
        Write-Host "FAILED: Invalid benchmark JSON: $($_.Exception.Message)" -ForegroundColor Red
        exit 1
    }
    $jsonText | Out-File -FilePath $jsonPath -Encoding utf8
    Write-Host "Result saved to $jsonPath" -ForegroundColor Gray
}

$commonArgs = @("--url", $Url, "--reconnect-every-transactions", "0", "--payload-bytes", "256")

Run-Bench "single-table fsync-per-commit" ($commonArgs + @(
    "--actors", "1", "--table-count", "1", "--transaction-size", "1",
    "--writes-per-actor", "1000", "--reads-per-actor", "0"
)) "single.json"

Run-Bench "8-actor / 8-table P99 latency (10 rows/tx, 50 reads)" ($commonArgs + @(
    "--actors", "8", "--table-count", "8", "--transaction-size", "10",
    "--writes-per-actor", "500", "--reads-per-actor", "50"
)) "concurrent_p99.json"

Run-Bench "8-actor / 8-table group commit throughput (1 row/tx)" ($commonArgs + @(
    "--actors", "8", "--table-count", "8", "--transaction-size", "1",
    "--writes-per-actor", "1000", "--reads-per-actor", "0"
)) "concurrent_tp.json"

function Get-Metric($jsonFile, $field) {
    $text = Get-Content "$TmpDir\$jsonFile" -Raw
    $obj = $text | ConvertFrom-Json
    return $obj.$field
}

$singleOps = [math]::Round((Get-Metric "single.json" "operations_per_second"), 0)
$concP99ms = [math]::Round((Get-Metric "concurrent_p99.json" "write_transactions_p99_us") / 1000.0, 1)
$concOps = [math]::Round((Get-Metric "concurrent_tp.json" "operations_per_second"), 0)
$readP50us = [math]::Round((Get-Metric "concurrent_p99.json" "reads_p50_us"), 0)

$commitHash = (git rev-parse --short HEAD).Trim()
$isDirty = -not [string]::IsNullOrWhiteSpace((git status --porcelain | Out-String))
$revision = if ($isDirty) { "${commitHash}-dirty" } else { $commitHash }
$dateStr = Get-Date -Format "yyyy-MM-dd"

$mysqlSingleOps = 3318
$mysqlConcP99 = 495.9
$mysqlConcOps = 7315

$singleRatio = [math]::Round($singleOps / $mysqlSingleOps, 2)
$concP99Ratio = [math]::Round($mysqlConcP99 / $concP99ms, 1)
$concOpsRatio = [math]::Round($concOps / $mysqlConcOps, 2)

$bt = [char]96
$b3 = "$bt$bt$bt"

$report = @"
# MyDB 性能报告

> ⚠️ 以下为开发机 Docker 限速回归数据，不代表物理生产硬件正式验收结论。正式性能验收需在 Ubuntu 24.04 物理机、双方同配置、I/O 限速条件下进行。
>
> MySQL 数字是历史参考值；本脚本不启动或重测 MySQL，因此比例不是同轮环境对比结论。
>
> 本文件由 ${bt}scripts/bench.ps1${bt} 自动生成，每次发布前必须重新运行。

---

## 测试方法

基准工具：${bt}mydb-bench${bt}（${bt}crates/mydb-bench${bt}）

对比参考：MySQL 8.0.46（历史数据，${bt}innodb_flush_log_at_trx_commit=1${bt}）

| 场景 | actors | tables | transaction_size | writes_per_actor | 说明 |
|------|--------|--------|-----------------|------------------|------|
| 单表写 (fsync-per-commit) | 1 | 1 | 1 | 1000 | 单连接单行 INSERT，每次 COMMIT 一次 fsync |
| 8 actor / 8表 写 P99 延迟 | 8 | 8 | 10 | 500 | 高并发游戏负载，P99 尾延迟 |
| 8 actor / 8表 Group Commit | 8 | 8 | 1 | 1000 | 并发写入自然批量聚合吞吐 |
| 读 P50 延迟 | 8 | 8 | - | 50 reads/actor | 单连接点查主键延迟 |

---

## 当前版本性能

### Git Revision: ${bt}${revision}${bt}
### 测试日期: $dateStr
### 测试环境: Docker（WSL2 后端，0.5 CPU / 512MiB 限速）

| 场景 | MyDB | MySQL 8.0.46（历史参考） | 参考比 |
|------|------|--------------|------|
| 单表写 (fsync-per-commit) | ${singleOps} ops/s | ${mysqlSingleOps} ops/s | ${singleRatio}x |
| 8 actor / 8表 写 P99 延迟 | ${concP99ms} ms | ${mysqlConcP99} ms | **${concP99Ratio}x faster** |
| 8 actor / 8表 Group Commit | ${concOps} ops/s | ${mysqlConcOps} ops/s | ${concOpsRatio}x |
| 读 P50 延迟 | ${readP50us} μs | - | - |

---

## 发布前更新流程

${b3}bash
# 自动化（推荐）
pwsh -File scripts/bench.ps1

# 或手动：
# 1. cargo clippy --workspace --all-targets -- -D warnings
# 2. cargo test --workspace
# 3. cargo build --release -p mydb-server -p mydb-bench
# 4. docker compose up -d --build
# 5. cargo run -p mydb-bench --release -- --url mysql://root:root@127.0.0.1:3306 --actors 1 --table-count 1 --transaction-size 1 --writes-per-actor 1000 --reads-per-actor 0
# 6. cargo run -p mydb-bench --release -- --url mysql://root:root@127.0.0.1:3306 --actors 8 --table-count 8 --transaction-size 10 --writes-per-actor 500 --reads-per-actor 50
# 7. cargo run -p mydb-bench --release -- --url mysql://root:root@127.0.0.1:3306 --actors 8 --table-count 8 --transaction-size 1 --writes-per-actor 1000 --reads-per-actor 0
${b3}

---

## 设计目标

MyDB 设计目标是**同资源、同持久化级别下达到 MySQL 10x 写性能**，不以关闭持久化换取数字。

关键性能原则：
- 每次提交仅一次 ${bt}sync_data()${bt} 顺序 fsync
- 锁内不做序列化/分配
- 自然批量优先，不人为引入延迟
- 热路径零堆分配
"@

$reportPath = "$ProjectRoot\性能报告.md"
[System.IO.File]::WriteAllText($reportPath, $report, [System.Text.UTF8Encoding]::new($false))
Write-Host "`nPerformance report written to: $reportPath" -ForegroundColor Green

Write-Host "`n========== BENCHMARK SUMMARY ==========" -ForegroundColor Yellow
Write-Host "Revision:            $revision ($dateStr)"
Write-Host "Single-table write:  $singleOps ops/s (MySQL: $mysqlSingleOps, ratio: ${singleRatio}x)"
Write-Host "8-actor P99 latency:  $concP99ms ms (MySQL historical: $mysqlConcP99 ms, reference: ${concP99Ratio}x faster)"
Write-Host "8-actor throughput:   $concOps ops/s (MySQL historical: $mysqlConcOps, reference: ${concOpsRatio}x)"
Write-Host "Read P50 latency:     $readP50us us"
Write-Host "========================================" -ForegroundColor Yellow
