#Requires -Version 5.1
param(
    [string]$MemoryLimit = "2g",
    [ValidateRange(1, 8)]
    [int]$CpuLimit = 2,
    [ValidateRange(1, 8)]
    [int]$BuildJobs = 1,
    [ValidateRange(1, 64)]
    [int]$DiskBudgetGiB = 10,
    [ValidateRange(1, 128)]
    [int]$MinimumFreeGiB = 6,
    [ValidateRange(1, 180)]
    [int]$TimeoutMinutes = 45,
    [switch]$SkipReleaseBuild,
    [switch]$KeepCache
)

$ErrorActionPreference = "Stop"
if ($PSVersionTable.PSVersion.Major -ge 7) { $PSNativeCommandUseErrorActionPreference = $false }
$root = Split-Path -Parent $PSScriptRoot
$runId = [guid]::NewGuid().ToString('N')
$container = "mydb-quality-$runId"
$targetVolume = "mydb-quality-target-$runId"
$registryVolume = "mydb-quality-registry-$runId"
$gitVolume = "mydb-quality-git-$runId"
$rustImage = if ([string]::IsNullOrWhiteSpace($env:MYDB_RUST_TEST_IMAGE)) { "rust:1-trixie" } else { $env:MYDB_RUST_TEST_IMAGE }
$output = Join-Path $root "target/docker-gate-$runId"
$createdVolumes = @()
$peakBytes = [long]0
$failure = $null
$success = $false
$containerCreated = $false
$artifacts = @()
New-Item -ItemType Directory -Path $output -Force | Out-Null

function Assert-Exit([string]$message) {
    if ($LASTEXITCODE -ne 0) { throw $message }
}

function Source-Hash {
    $files = @(git -C $root ls-files --cached --others --exclude-standard -- Cargo.toml Cargo.lock crates vendor | Sort-Object -Unique)
    Assert-Exit 'Cannot enumerate immutable Rust build inputs'
    $manifest = New-Object Text.StringBuilder
    foreach ($file in $files) {
        [void]$manifest.Append($file).Append('=').Append((Get-FileHash -LiteralPath (Join-Path $root $file) -Algorithm SHA256).Hash).Append("`n")
    }
    $hasher = [Security.Cryptography.SHA256]::Create()
    try { return [BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes($manifest.ToString()))).Replace('-', '').ToLowerInvariant() }
    finally { $hasher.Dispose() }
}
$sourceHash = Source-Hash

docker info | Out-Null
Assert-Exit "Docker daemon is unavailable"

$commands = @(
    "set -eu",
    "export CARGO_BUILD_JOBS=$BuildJobs",
    "export CARGO_INCREMENTAL=0",
    "export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0",
    "rustup component add rustfmt clippy",
    "cargo fmt --all -- --check",
    "cargo clippy --workspace --all-targets --locked -- -D warnings",
    "cargo test --workspace --locked -- --test-threads=1"
)
if (-not $SkipReleaseBuild) {
    $commands += "cargo build --release --locked -p mydb-server -p mydb-cli -p mydb-migrate -p mydb-dump -p mydb-bench"
}
$command = $commands -join " && "

Write-Host "Docker Rust gate: image=$rustImage memory=$MemoryLimit cpus=$CpuLimit jobs=$BuildJobs disk=${DiskBudgetGiB}GiB timeout=${TimeoutMinutes}m" -ForegroundColor Cyan
try {
    # Check the daemon's filesystem, which may be different from the host drive.
    # Include enough headroom for the entire permitted run before allocating it.
    $disk = @(docker run --rm --memory 128m --cpus 1 --network none --read-only --entrypoint sh $rustImage -c 'df -Pk /')
    Assert-Exit "cannot inspect Docker filesystem capacity"
    $fields = ($disk[-1].Trim() -split '\s+')
    $availableBytes = [long]$fields[3] * 1024
    if ($availableBytes -lt ([long]$DiskBudgetGiB + $MinimumFreeGiB) * 1GB) {
        throw "Insufficient Docker disk headroom for the bounded quality gate"
    }
    foreach ($volume in @($targetVolume, $registryVolume, $gitVolume)) {
        docker volume create --label "com.neko233.mydb.quality-run=$runId" $volume | Out-Null
        Assert-Exit "cannot create Docker volume: $volume"
        $createdVolumes += $volume
    }
    docker create --init --name $container `
        --label "com.neko233.mydb.quality-run=$runId" `
        --cpus $CpuLimit --memory $MemoryLimit --memory-swap $MemoryLimit `
        --pids-limit 256 --log-driver json-file --log-opt max-size=1m --log-opt max-file=3 `
        --security-opt no-new-privileges `
        --mount "type=bind,source=$root,target=/workspace,readonly" `
        --mount "type=volume,source=$targetVolume,target=/workspace/target" `
        --mount "type=volume,source=$registryVolume,target=/usr/local/cargo/registry" `
        --mount "type=volume,source=$gitVolume,target=/usr/local/cargo/git" `
        --workdir /workspace `
        $rustImage sh -c $command | Out-Null
    Assert-Exit "cannot create Docker Rust gate"
    $containerCreated = $true
    docker start $container | Out-Null
    Assert-Exit "cannot start Docker Rust gate"
    $deadline = (Get-Date).AddMinutes($TimeoutMinutes)
    while ($true) {
        $details = (docker inspect --size $container | ConvertFrom-Json)[0]
        Assert-Exit "cannot inspect Docker Rust gate"
        $bytes = [long]$details.SizeRw
        if ($details.State.Status -notin @('running', 'created')) {
            if ($details.State.ExitCode -ne 0) { throw "Docker Rust gate failed with exit code $($details.State.ExitCode)" }
            break
        }
        $usage = @(docker exec $container sh -c 'du -sk /workspace/target /usr/local/cargo/registry /usr/local/cargo/git 2>/dev/null')
        if ($LASTEXITCODE -eq 0) {
            foreach ($line in $usage) { $bytes += [long](($line.Trim() -split '\s+')[0]) * 1024 }
            $peakBytes = [math]::Max($peakBytes, $bytes)
            if ($bytes -gt [long]$DiskBudgetGiB * 1GB) { throw "Docker Rust gate exceeded its ${DiskBudgetGiB}GiB disk budget" }
            $free = @(docker exec $container sh -c 'df -Pk /')
            if ($LASTEXITCODE -eq 0 -and [long](($free[-1].Trim() -split '\s+')[3]) * 1024 -lt [long]$MinimumFreeGiB * 1GB) {
                throw "Docker Rust gate reached the reserved disk headroom"
            }
        }
        if ((Get-Date) -ge $deadline) { throw "Docker Rust gate exceeded its ${TimeoutMinutes} minute timeout" }
        Start-Sleep -Seconds 5
    }
    if ((Source-Hash) -ne $sourceHash) { throw 'Rust build inputs changed during validation; no candidate is accepted' }
    if (-not $SkipReleaseBuild) {
        foreach ($name in @('mydb-server', 'mydb-cli', 'mydb', 'mydb-migrate', 'mydbdump', 'mydb-bench')) {
            $destination = Join-Path $output $name
            docker cp "${container}:/workspace/target/release/$name" $destination
            Assert-Exit 'Cannot retain a small Linux release artifact before cleanup'
            $artifacts += @{file=$name;sha256=(Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash;bytes=(Get-Item -LiteralPath $destination).Length}
        }
    }
    $success = $true
    Write-Host "Docker Rust gate passed" -ForegroundColor Green
} catch {
    $failure = $_.Exception.Message
    throw
} finally {
    # Only this run's resources can be removed. A concurrent gate uses other names.
    $cleanupSucceeded = $true
    if ($containerCreated) {
        docker logs $container 2>&1 | Set-Content -LiteralPath (Join-Path $output 'cargo.log') -Encoding UTF8
        docker rm --force $container 2>$null | Out-Null
        if ($LASTEXITCODE -ne 0) { $cleanupSucceeded = $false }
    }
    if (-not $KeepCache) {
        foreach ($volume in $createdVolumes) {
            docker volume rm $volume 2>$null | Out-Null
            if ($LASTEXITCODE -ne 0) { $cleanupSucceeded = $false }
        }
    }
    if (-not $cleanupSucceeded) { $success = $false; if ($null -eq $failure) { $failure = 'Run-owned Docker resources were not fully cleaned' } }
    [ordered]@{ success=$success; reason=$failure; source_sha256=$sourceHash; artifacts=$artifacts; cleanup_succeeded=$cleanupSucceeded; peak_observed_disk_bytes=$peakBytes; disk_budget_gib=$DiskBudgetGiB; minimum_free_gib=$MinimumFreeGiB; timeout_minutes=$TimeoutMinutes; retained_volumes=@(if ($KeepCache) { $createdVolumes }); image=$rustImage } |
        ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $output 'receipt.json') -Encoding UTF8
    if (Test-Path -LiteralPath (Join-Path $output 'cargo.log')) { Get-Content -LiteralPath (Join-Path $output 'cargo.log') -Tail 25 }
    Write-Host "Bounded gate receipt: $output"
    if (-not $cleanupSucceeded) { throw $failure }
}
