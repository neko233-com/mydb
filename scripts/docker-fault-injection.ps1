#Requires -Version 5.1
<##
.SYNOPSIS
    Runs low-resource, Docker-only MyDB durability fault injection.

.DESCRIPTION
    Uses only uniquely named Docker containers, a private network, and named
    volumes. It never mounts a host data directory and never publishes ports.
    SIGKILL models power loss; the recovery marker/pause is test-only and is
    enabled only when the image is started with MYDB_TEST_FAULT_INJECTION=1.

.EXAMPLE
    .\scripts\docker-fault-injection.ps1 -Build
    .\scripts\docker-fault-injection.ps1 -Image mydb:fault-test
#>

param(
    [switch]$Build,
    [string]$Image = "mydb:fault-test",
    [switch]$Keep
)

$ErrorActionPreference = "Stop"
if ($PSVersionTable.PSVersion.Major -ge 7) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$suffix = "${PID}-$(Get-Random -Minimum 1000 -Maximum 9999)"
$network = "mydb-fault-$suffix"
$containers = @(
    "mydb-fault-power-$suffix",
    "mydb-fault-readonly-$suffix",
    "mydb-fault-enospc-$suffix",
    "mydb-fault-recovery-$suffix"
)
$volumes = @(
    "mydb-fault-power-data-$suffix",
    "mydb-fault-readonly-data-$suffix",
    "mydb-fault-recovery-data-$suffix"
)

function Assert-Docker([string]$message) {
    if ($LASTEXITCODE -ne 0) { throw $message }
}

function Wait-Healthy([string]$container, [int]$seconds = 60) {
    $deadline = (Get-Date).AddSeconds($seconds)
    do {
        $state = ((docker inspect --format '{{.State.Status}}' $container 2>$null) -join "").Trim()
        $health = ((docker inspect --format '{{.State.Health.Status}}' $container 2>$null) -join "").Trim()
        if ($health -eq "healthy") { return }
        if ($state -eq "exited" -or $state -eq "dead") {
            docker logs $container 2>&1 | Select-Object -Last 80
            throw "fixture $container exited before health (state=$state)"
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    docker logs $container 2>&1 | Select-Object -Last 80
    throw "fixture $container did not become healthy (health=$health, state=$state)"
}

function Start-Fixture(
    [string]$container,
    [string]$volume,
    [hashtable]$extraEnvironment = @{},
    [string]$tmpfs = ""
) {
    $arguments = @(
        "run", "--detach", "--init",
        "--name", $container,
        "--network", $network,
        "--cpus", "0.50",
        "--memory", "512m",
        "--memory-swap", "512m",
        "--pids-limit", "128",
        "--read-only",
        "--tmpfs", "/tmp:mode=1777,noexec,nosuid,nodev",
        "--security-opt", "no-new-privileges:true",
        "--cap-drop", "ALL",
        "-e", "MYDB_ROOT_PASSWORD=root",
        "-e", "MYDB_ADMIN_PASSWORD=root",
        "-e", "MYDB_ENFORCE_STRONG_PASSWORDS=false"
    )
    if ($volume) {
        $arguments += @("--mount", "type=volume,src=$volume,dst=/var/lib/mydb")
    }
    if ($tmpfs) {
        $arguments += @("--tmpfs", $tmpfs)
    }
    foreach ($entry in $extraEnvironment.GetEnumerator()) {
        $arguments += @("-e", "$($entry.Key)=$($entry.Value)")
    }
    $arguments += $Image
    docker @arguments | Out-Null
    Assert-Docker "cannot start Docker fixture $container"
}

function Stop-Fixture([string]$container, [int]$timeout = 30) {
    docker stop --time $timeout $container | Out-Null
    Assert-Docker "cannot stop Docker fixture $container"
}

function Invoke-Sql([string]$container, [string]$sql, [switch]$AllowFailure) {
    $output = docker exec $container mydb-cli -h 127.0.0.1 -P 3306 -u root -p root -e $sql 2>&1
    $exitCode = $LASTEXITCODE
    if (-not $AllowFailure -and $exitCode -ne 0) {
        throw ("SQL failed in {0}: {1}" -f $container, ($output -join "`n"))
    }
    [pscustomobject]@{ ExitCode = $exitCode; Text = ($output -join "`n").Trim() }
}

function Run-RootVolumeCommand([string]$container, [string]$command) {
    docker run --rm --user root --volumes-from $container --entrypoint sh $Image -c $command | Out-Null
    Assert-Docker "root volume command failed for $container"
}

function Remove-Fixture([string]$container) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    docker rm --force $container 2>&1 | Out-Null
    $ErrorActionPreference = $previous
}

function Test-PowerLoss([string]$container, [string]$volume) {
    Write-Host "[1/4] SIGKILL power-loss simulation" -ForegroundColor Cyan
    Start-Fixture $container $volume
    Wait-Healthy $container
    Invoke-Sql $container "CREATE DATABASE fault_power; CREATE TABLE fault_power.probe (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO fault_power.probe VALUES (1,10);" | Out-Null
    docker exec -d $container mydb-cli -h 127.0.0.1 -P 3306 -u root -p root -e "USE fault_power; START TRANSACTION; UPDATE probe SET value=99 WHERE id=1; SELECT SLEEP(30); COMMIT;" | Out-Null
    Assert-Docker "cannot start uncommitted power-loss writer"

    $dirty = $false
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        $probe = Invoke-Sql $container "SET SESSION TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; SELECT CONCAT('MYDB_DIRTY:',value) FROM fault_power.probe WHERE id=1;"
        if ($probe.Text -match "MYDB_DIRTY:99") { $dirty = $true; break }
        Start-Sleep -Milliseconds 250
    }
    if (-not $dirty) { throw "power-loss fixture never reached an uncommitted dirty write" }
    docker kill --signal KILL $container | Out-Null
    Assert-Docker "SIGKILL power-loss simulation failed"
    docker start $container | Out-Null
    Assert-Docker "power-loss fixture restart failed"
    Wait-Healthy $container
    $state = Invoke-Sql $container "SELECT CONCAT('MYDB_RESULT:',value) FROM fault_power.probe WHERE id=1; START TRANSACTION; UPDATE fault_power.probe SET value=value+1 WHERE id=1; COMMIT; SELECT CONCAT('MYDB_RESULT:',value) FROM fault_power.probe WHERE id=1;"
    if ($state.Text -notmatch "MYDB_RESULT:10" -or $state.Text -notmatch "MYDB_RESULT:11") {
        throw "power-loss recovery state invalid: $($state.Text)"
    }
    Write-Host "      committed data kept, uncommitted data discarded, new commit accepted" -ForegroundColor Green
}

function Test-ReadOnly([string]$container, [string]$volume) {
    Write-Host "[2/4] read-only data directory simulation" -ForegroundColor Cyan
    Start-Fixture $container $volume
    Wait-Healthy $container
    Invoke-Sql $container "CREATE DATABASE fault_readonly; CREATE TABLE fault_readonly.probe (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO fault_readonly.probe VALUES (1,10);" | Out-Null
    Stop-Fixture $container
    Run-RootVolumeCommand $container "chmod -R a-w /var/lib/mydb/data"
    docker start $container | Out-Null
    Assert-Docker "read-only fixture restart failed"
    Start-Sleep -Seconds 3
    $health = ((docker inspect --format '{{.State.Health.Status}}' $container 2>$null) -join "").Trim()
    $log = (docker logs $container 2>&1) -join "`n"
    if ($health -eq "healthy" -or $log -notmatch "Permission denied|os error 13") {
        throw "read-only startup did not fail safely (health=$health)"
    }
    docker stop --time 1 $container | Out-Null
    Assert-Docker "read-only fixture stop failed"
    Run-RootVolumeCommand $container "chmod -R u+rwX /var/lib/mydb/data"
    docker start $container | Out-Null
    Assert-Docker "read-only recovery restart failed"
    Wait-Healthy $container
    $state = Invoke-Sql $container "UPDATE fault_readonly.probe SET value=value+1 WHERE id=1; SELECT CONCAT('MYDB_RESULT:',value) FROM fault_readonly.probe WHERE id=1;"
    if ($state.Text -notmatch "MYDB_RESULT:11") { throw "read-only recovery could not commit: $($state.Text)" }
    Write-Host "      startup refused safely, permissions restored, new WAL write accepted" -ForegroundColor Green
}

function Test-DiskFull([string]$container) {
    Write-Host "[3/4] ENOSPC disk-full simulation" -ForegroundColor Cyan
    Start-Fixture $container "" @{} "/var/lib/mydb:rw,size=8m,mode=1777"
    Wait-Healthy $container
    $result = Invoke-Sql $container "CREATE DATABASE fault_enospc; CREATE TABLE fault_enospc.blobs (id BIGINT PRIMARY KEY,payload LONGBLOB); INSERT INTO fault_enospc.blobs VALUES (1,REPEAT('x',12000000));" -AllowFailure
    if ($result.ExitCode -eq 0 -or $result.Text -notmatch "No space left on device") {
        throw "ENOSPC did not return a safe error: $($result.Text)"
    }
    $health = ((docker inspect --format '{{.State.Status}}' $container 2>$null) -join "").Trim()
    $rows = Invoke-Sql $container "SELECT COUNT(*) FROM fault_enospc.blobs;"
    if ($health -ne "running" -or $rows.Text -notmatch "(?m)\b0\s+1 rows in set") {
        throw "ENOSPC left partial data or crashed server (state=$health, rows=$($rows.Text))"
    }
    Write-Host "      write rejected, server stayed alive, partial row absent" -ForegroundColor Green
}

function Test-RecoveryInterruption([string]$container, [string]$volume) {
    Write-Host "[4/4] recovery interruption simulation" -ForegroundColor Cyan
    $marker = "/var/lib/mydb/recovery.marker"
    Start-Fixture $container $volume @{
        MYDB_TEST_FAULT_INJECTION = "1"
        MYDB_TEST_RECOVERY_PAUSE_MS = "50"
        MYDB_TEST_RECOVERY_MARKER = $marker
    }
    Wait-Healthy $container
    $statements = 1..160 | ForEach-Object { "INSERT INTO fault_recovery.probe (id,value) VALUES ($_, $_);" }
    Invoke-Sql $container ("CREATE DATABASE fault_recovery; CREATE TABLE fault_recovery.probe (id BIGINT PRIMARY KEY,value BIGINT);" + ($statements -join "")) | Out-Null
    docker kill --signal KILL $container | Out-Null
    Assert-Docker "initial recovery-interruption crash failed"
    docker start $container | Out-Null
    Assert-Docker "recovery-interruption first restart failed"

    $running = $false
    $deadline = (Get-Date).AddSeconds(30)
    do {
        $markerText = ((docker exec $container cat $marker 2>$null) -join "").Trim()
        if ($markerText -like "running *") { $running = $true; break }
        $state = ((docker inspect --format '{{.State.Status}}' $container 2>$null) -join "").Trim()
        if ($state -eq "exited" -or $state -eq "dead") { break }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    if (-not $running) { throw "recovery marker never entered running state: $markerText" }
    docker kill --signal KILL $container | Out-Null
    Assert-Docker "second SIGKILL during recovery failed"
    docker start $container | Out-Null
    Assert-Docker "recovery-interruption final restart failed"
    Wait-Healthy $container
    $markerText = ((docker exec $container cat $marker 2>$null) -join "").Trim()
    $state = Invoke-Sql $container "SELECT COUNT(*) FROM fault_recovery.probe; SELECT SUM(value) FROM fault_recovery.probe;"
    if ($markerText -notmatch "^complete recovered=") { throw "recovery did not finish after interruption: $markerText" }
    if ($state.Text -notmatch "(?m)\b160\s+1 rows in set" -or $state.Text -notmatch "(?m)\b12880\s+1 rows in set") {
        throw "recovery interruption lost or duplicated rows: $($state.Text)"
    }
    Write-Host "      second crash during replay recovered idempotently; 160 rows exact" -ForegroundColor Green
}

try {
    docker info | Out-Null
    Assert-Docker "Docker daemon is unavailable"
    if ($Build) {
        Write-Host "Building $Image with 0.5 CPU / 768 MiB cap" -ForegroundColor Cyan
        docker build --memory 768m --cpu-period 100000 --cpu-quota 50000 --tag $Image .
        Assert-Docker "low-resource Docker image build failed"
    } else {
        docker image inspect $Image | Out-Null
        Assert-Docker "Docker image $Image is missing; use -Build once when convenient"
    }
    docker network create --internal $network | Out-Null
    Assert-Docker "cannot create isolated Docker network"
    foreach ($volume in $volumes) {
        docker volume create $volume | Out-Null
        Assert-Docker "cannot create isolated Docker volume $volume"
    }

    Test-PowerLoss $containers[0] $volumes[0]
    Remove-Fixture $containers[0]
    Test-ReadOnly $containers[1] $volumes[1]
    Remove-Fixture $containers[1]
    Test-DiskFull $containers[2]
    Remove-Fixture $containers[2]
    Test-RecoveryInterruption $containers[3] $volumes[2]
    Write-Host "Docker fault injection passed: power loss, read-only, ENOSPC, recovery interruption" -ForegroundColor Green
} finally {
    if (-not $Keep) {
        foreach ($container in $containers) { Remove-Fixture $container }
        $previous = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        foreach ($volume in $volumes) { docker volume rm --force $volume 2>&1 | Out-Null }
        docker network rm $network 2>&1 | Out-Null
        $ErrorActionPreference = $previous
    } else {
        Write-Host "Kept Docker fixtures: network=$network containers=$($containers -join ',') volumes=$($volumes -join ',')" -ForegroundColor Yellow
    }
}
