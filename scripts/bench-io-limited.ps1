#Requires -Version 5.1
param(
    [int]$MeasureSeconds = 60,
    [int]$Rounds = 3,
    [int]$Actors = 8,
    [int]$WritesPerActor = 500,
    [int]$TableCount = 1,
    [int]$TransactionSize = 10,
    [int]$ReadsPerActor = 50,
    [int]$PayloadBytes = 256,
    [string]$WriteMode = "actor-batch",
    [string]$WriteBps = "20mb",
    [string]$ReadBps = "100mb",
    [int]$WriteIops = 500,
    [int]$ReadIops = 2000,
    [int]$CpuLimit = 1,
    [string]$ResultsDir = "",
    [switch]$Keep,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
if ($MeasureSeconds -lt 1 -or $MeasureSeconds -gt 60) {
    throw "MeasureSeconds must be between 1 and 60"
}
if ($Rounds -lt 1) { throw "Rounds must be greater than zero" }
if ($Actors -lt 1) { throw "Actors must be greater than zero" }
if ($WritesPerActor -lt 1) { throw "WritesPerActor must be greater than zero" }
if ($TableCount -lt 1) { throw "TableCount must be greater than zero" }
if ($CpuLimit -lt 1) { throw "CpuLimit must be greater than zero" }
if ($TransactionSize -lt 1) { throw "TransactionSize must be greater than zero" }
if ($ReadsPerActor -lt 0) { throw "ReadsPerActor cannot be negative" }
if ($PayloadBytes -lt 0) { throw "PayloadBytes cannot be negative" }
$root = Split-Path -Parent $PSScriptRoot
$results = if ([string]::IsNullOrWhiteSpace($ResultsDir)) {
    Join-Path $root "target\io-bench-desktop"
} elseif ([System.IO.Path]::IsPathRooted($ResultsDir)) {
    $ResultsDir
} else {
    Join-Path $root $ResultsDir
}
$mydbContainer = "mydb-io-desktop"
$mysqlContainer = "mysql80-io-desktop"
$mydbVolume = "mydb-io-desktop-data"
$mysqlVolume = "mysql80-io-desktop-data"
$mydbPort = 13316
$mysqlPort = 13306
$mydbHttpPort = 14316

function Assert-Exit([string]$message) {
    if ($LASTEXITCODE -ne 0) { throw $message }
}

function Remove-BenchmarkResources {
    $containers = @(docker ps --all --format '{{.Names}}')
    foreach ($container in @($mydbContainer, $mysqlContainer)) {
        if ($containers -contains $container) {
            docker rm --force $container | Out-Null
            Assert-Exit "cannot remove stale benchmark container: $container"
        }
    }
    $volumes = @(docker volume ls --format '{{.Name}}')
    foreach ($volume in @($mydbVolume, $mysqlVolume)) {
        if ($volumes -contains $volume) {
            docker volume rm --force $volume | Out-Null
            Assert-Exit "cannot remove stale benchmark volume: $volume"
        }
    }
}

function Wait-Ready([string]$container, [string]$kind) {
    $deadline = (Get-Date).AddMinutes(4)
    do {
        Start-Sleep -Seconds 2
        $state = docker inspect --format '{{.State.Status}}' $container 2>$null
        if ($state -ne "running") {
            docker logs $container
            throw "$kind container stopped during initialization"
        }
        if ($kind -eq "mydb") {
            docker exec $container mydb-server --healthcheck 2>$null | Out-Null
        } else {
            # Do not accept the MySQL image's temporary socket-only server.
            $previousErrorAction = $ErrorActionPreference
            $ErrorActionPreference = "Continue"
            try {
                docker exec $container mysqladmin --protocol=TCP --host=127.0.0.1 --port=3306 `
                    --user=root --password=root ping --silent 2>&1 | Out-Null
            } finally {
                $ErrorActionPreference = $previousErrorAction
            }
        }
        $ready = $LASTEXITCODE -eq 0
    } while (-not $ready -and (Get-Date) -lt $deadline)
    if (-not $ready) {
        docker logs $container
        throw "$kind did not become TCP-ready"
    }
}

function Get-Median([double[]]$values) {
    $sorted = @($values | Sort-Object)
    if ($sorted.Count -eq 0) { throw "cannot summarize an empty sample" }
    $middle = [int][Math]::Floor($sorted.Count / 2)
    if ($sorted.Count % 2 -eq 1) { return [double]$sorted[$middle] }
    return ([double]$sorted[$middle - 1] + [double]$sorted[$middle]) / 2
}

function Get-PrometheusMetric([string]$text, [string]$name) {
    $match = [regex]::Match(
        $text,
        "(?m)^$([regex]::Escape($name))\s+([0-9]+(?:\.[0-9]+)?)$"
    )
    if (-not $match.Success) { throw "missing Prometheus metric: $name" }
    return [double]$match.Groups[1].Value
}

function Run-One([string]$engine, [int]$port, [string]$round) {
    $elapsed = [int][Math]::Floor(((Get-Date) - $script:started).TotalSeconds)
    $remaining = $MeasureSeconds - $elapsed
    if ($remaining -le 0) { throw "performance sampling exceeded ${MeasureSeconds}s" }
    $runName = "mydb-bench-$engine-$round-$PID"
    $output = docker run --rm --name $runName --network host --platform linux/amd64 `
        --entrypoint timeout mydb-bench:ubuntu24 --signal=TERM "${remaining}s" `
        mydb-bench --url "mysql://root:root@127.0.0.1:$port" `
        --actors $Actors --writes-per-actor $WritesPerActor --table-count $TableCount `
        --transaction-size $TransactionSize --reads-per-actor $ReadsPerActor `
        --payload-bytes $PayloadBytes --write-mode $WriteMode
    Assert-Exit "$engine benchmark failed or exceeded the one-minute budget"
    $json = $output -join "`n"
    $report = $json | ConvertFrom-Json
    if ($report.sql_engine -ne "InnoDB") { throw "$engine did not report InnoDB SQL" }
    $json | Set-Content -LiteralPath (Join-Path $results "$engine-$round.json") -Encoding UTF8
}

docker info | Out-Null
$architecture = docker info --format '{{.Architecture}}'
if ($architecture.Trim() -ne "x86_64") { throw "performance gate requires linux/amd64 Docker" }

New-Item -ItemType Directory -Force -Path $results | Out-Null
Get-ChildItem -LiteralPath $results -File -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match '^(mydb|mysql)-.*\.json$|^summary\.json$|^mydb-metrics\.prom$' } |
    Remove-Item -Force

Remove-BenchmarkResources

try {
    if ($SkipBuild) {
        foreach ($image in @("mysql:8.0", "mydb:io-bench", "mydb-bench:ubuntu24")) {
            docker image inspect $image 2>$null | Out-Null
            Assert-Exit "SkipBuild requires local image: $image"
        }
    } else {
        $previousErrorAction = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            $null = docker image inspect mysql:8.0 2>&1
            if ($LASTEXITCODE -ne 0) {
                $pullOutput = docker pull mysql:8.0 2>&1
                if ($LASTEXITCODE -ne 0) {
                    $pullOutput | Write-Host
                    throw "cannot pull official MySQL80 image"
                }
            }
            $mydbBuild = docker build --platform linux/amd64 --build-arg RUST_IMAGE=rust:1-bookworm `
                --build-arg RUNTIME_IMAGE=ubuntu:24.04 `
                --build-arg TARGET_CACHE_ID=mydb-target-bookworm -t mydb:io-bench $root 2>&1
            if ($LASTEXITCODE -ne 0) {
                $mydbBuild | Write-Host
                throw "cannot build Ubuntu24 MyDB image"
            }
            $benchBuild = docker build --platform linux/amd64 -f (Join-Path $root "scripts\Dockerfile.bench") `
                -t mydb-bench:ubuntu24 $root 2>&1
            if ($LASTEXITCODE -ne 0) {
                $benchBuild | Write-Host
                throw "cannot build Ubuntu24 benchmark image"
            }
        } finally {
            $ErrorActionPreference = $previousErrorAction
        }
    }

    $source = docker run --rm --privileged --pid=host `
        --mount "type=bind,source=/,target=/host" ubuntu:24.04 `
        sh -c "findmnt -n -T /host/var/lib/docker -o SOURCE"
    Assert-Exit "cannot discover Docker data block device"
    $device = (($source -join "`n").Trim() -replace '\[.*$', '')
    if ($device -notmatch '^/dev/') { throw "unsafe Docker block device: $device" }
    docker run --rm --device-write-bps "${device}:$WriteBps" `
        --device-write-iops "${device}:$WriteIops" ubuntu:24.04 true
    Assert-Exit "Docker cgroup block-I/O throttling is unavailable for $device"

    docker volume create $mydbVolume | Out-Null
    docker volume create $mysqlVolume | Out-Null
    docker run -d --name $mydbContainer --cpus $CpuLimit --memory 768m --memory-swap 768m `
        --device-read-bps "${device}:$ReadBps" --device-write-bps "${device}:$WriteBps" `
        --device-read-iops "${device}:$ReadIops" --device-write-iops "${device}:$WriteIops" `
        --mount "type=volume,source=$mydbVolume,target=/var/lib/mydb" `
        -p "127.0.0.1:${mydbPort}:3306" -p "127.0.0.1:${mydbHttpPort}:4306" `
        -e MYDB_ROOT_PASSWORD=root `
        -e MYDB_ADMIN_PASSWORD=root `
        -e MYDB_THREAD_COUNT=$CpuLimit `
        -e MYDB_ENFORCE_STRONG_PASSWORDS=false mydb:io-bench | Out-Null
    Assert-Exit "cannot start rate-limited MyDB"
    docker run -d --name $mysqlContainer --cpus $CpuLimit --memory 768m --memory-swap 768m `
        --device-read-bps "${device}:$ReadBps" --device-write-bps "${device}:$WriteBps" `
        --device-read-iops "${device}:$ReadIops" --device-write-iops "${device}:$WriteIops" `
        --mount "type=volume,source=$mysqlVolume,target=/var/lib/mysql" `
        -p "127.0.0.1:${mysqlPort}:3306" -e MYSQL_ROOT_PASSWORD=root `
        -e 'MYSQL_ROOT_HOST=%' mysql:8.0 --innodb-buffer-pool-size=256M `
        --innodb-flush-log-at-trx-commit=1 --performance-schema=OFF --skip-log-bin | Out-Null
    Assert-Exit "cannot start rate-limited MySQL80"

    Wait-Ready $mydbContainer "mydb"
    Wait-Ready $mysqlContainer "mysql"
    $mydbVersion = (docker exec $mydbContainer mydb-server --version) -join " "
    $mysqlVersion = (docker exec $mysqlContainer mysqld --version) -join " "
    $mydbImage = docker inspect --format '{{.Image}}' $mydbContainer
    $mysqlImage = docker inspect --format '{{.Image}}' $mysqlContainer

    $script:started = Get-Date
    Run-One "mydb" $mydbPort "warmup"
    Run-One "mysql" $mysqlPort "warmup"
    Remove-Item -LiteralPath (Join-Path $results "mydb-warmup.json"), `
        (Join-Path $results "mysql-warmup.json") -Force
    for ($round = 1; $round -le $Rounds; $round++) {
        if ($round % 2 -eq 1) {
            Run-One "mydb" $mydbPort "$round"
            Run-One "mysql" $mysqlPort "$round"
        } else {
            Run-One "mysql" $mysqlPort "$round"
            Run-One "mydb" $mydbPort "$round"
        }
    }
    $samplingElapsed = [int][Math]::Ceiling(((Get-Date) - $script:started).TotalSeconds)
    $metrics = (Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$mydbHttpPort/metrics").Content
    $metrics | Set-Content -LiteralPath (Join-Path $results "mydb-metrics.prom") -Encoding UTF8
    $groups = Get-PrometheusMetric $metrics "mydb_group_commits_total"
    $groupedRequests = Get-PrometheusMetric $metrics "mydb_grouped_requests_total"

    $mydb = @(Get-ChildItem -LiteralPath $results -Filter 'mydb-*.json' |
        ForEach-Object { Get-Content -Raw -LiteralPath $_.FullName | ConvertFrom-Json })
    $mysql = @(Get-ChildItem -LiteralPath $results -Filter 'mysql-*.json' |
        ForEach-Object { Get-Content -Raw -LiteralPath $_.FullName | ConvertFrom-Json })
    if ($mydb.Count -ne $Rounds -or $mysql.Count -ne $Rounds) {
        throw "sample count mismatch"
    }
    $mydbOps = Get-Median @($mydb | ForEach-Object { [double]$_.operations_per_second })
    $mysqlOps = Get-Median @($mysql | ForEach-Object { [double]$_.operations_per_second })
    $summary = [ordered]@{
        platform = "Docker Ubuntu 24.04 linux/amd64 (official MySQL 8.0 baseline)"
        sql_engine = "InnoDB on both servers"
        workload = $WriteMode
        table_count = $TableCount
        cpu_limit = $CpuLimit
        durability = "fsync-on-commit"
        docker_data_device = $device
        write_bps_per_engine = $WriteBps
        write_iops_per_engine = $WriteIops
        rounds = $Rounds
        sampling_budget_seconds = $MeasureSeconds
        sampling_elapsed_seconds = $samplingElapsed
        mydb_version = $mydbVersion
        mysql80_version = $mysqlVersion
        mydb_image_id = ($mydbImage -join "")
        mysql80_image_id = ($mysqlImage -join "")
        mydb_actor_phases = [ordered]@{
            group_commits = $groups
            grouped_requests = $groupedRequests
            requests_per_group = if ($groups -gt 0) { $groupedRequests / $groups } else { 0 }
            prepare_validation_microseconds_total = Get-PrometheusMetric $metrics "mydb_prepare_validation_microseconds_total"
            wal_sync_microseconds_total = Get-PrometheusMetric $metrics "mydb_wal_sync_microseconds_total"
            apply_microseconds_total = Get-PrometheusMetric $metrics "mydb_apply_microseconds_total"
            checkpoint_microseconds_total = Get-PrometheusMetric $metrics "mydb_checkpoint_microseconds_total"
        }
        mydb_median = [ordered]@{
            operations_per_second = $mydbOps
            write_transactions_p50_us = Get-Median @($mydb | ForEach-Object { [double]$_.write_transactions_p50_us })
            write_transactions_p95_us = Get-Median @($mydb | ForEach-Object { [double]$_.write_transactions_p95_us })
            write_transactions_p99_us = Get-Median @($mydb | ForEach-Object { [double]$_.write_transactions_p99_us })
        }
        mysql80_median = [ordered]@{
            operations_per_second = $mysqlOps
            write_transactions_p50_us = Get-Median @($mysql | ForEach-Object { [double]$_.write_transactions_p50_us })
            write_transactions_p95_us = Get-Median @($mysql | ForEach-Object { [double]$_.write_transactions_p95_us })
            write_transactions_p99_us = Get-Median @($mysql | ForEach-Object { [double]$_.write_transactions_p99_us })
        }
        mydb_over_mysql_ops_ratio = $mydbOps / $mysqlOps
    }
    $summaryJson = $summary | ConvertTo-Json -Depth 6
    $summaryJson | Set-Content -LiteralPath (Join-Path $results "summary.json") -Encoding UTF8
    $summaryJson
} finally {
    if (-not $Keep) {
        Remove-BenchmarkResources
    }
}
