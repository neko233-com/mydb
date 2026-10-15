#Requires -Version 5.1
param(
    [string]$MemoryLimit = "2g",
    [ValidateRange(1, 8)]
    [int]$CpuLimit = 2,
    [ValidateRange(1, 8)]
    [int]$BuildJobs = 1,
    [switch]$SkipReleaseBuild,
    [switch]$KeepCache
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$targetVolume = "mydb-rust-target"
$registryVolume = "mydb-rust-registry"
$gitVolume = "mydb-rust-git"
$rustImage = if ([string]::IsNullOrWhiteSpace($env:MYDB_RUST_TEST_IMAGE)) { "rust:1-trixie" } else { $env:MYDB_RUST_TEST_IMAGE }

function Assert-Exit([string]$message) {
    if ($LASTEXITCODE -ne 0) { throw $message }
}

docker info | Out-Null
Assert-Exit "Docker daemon is unavailable"

foreach ($volume in @($targetVolume, $registryVolume, $gitVolume)) {
    docker volume create $volume | Out-Null
    Assert-Exit "cannot create Docker volume: $volume"
}

$commands = @(
    "set -eu",
    "export CARGO_BUILD_JOBS=$BuildJobs",
    "export CARGO_INCREMENTAL=0",
    "rustup component add rustfmt clippy",
    "cargo fmt --all -- --check",
    "cargo clippy --workspace --all-targets --locked -- -D warnings",
    "cargo test --workspace --locked -- --test-threads=1"
)
if (-not $SkipReleaseBuild) {
    $commands += "cargo build --release --locked -p mydb-server -p mydb-bench"
}
$command = $commands -join " && "

Write-Host "Docker Rust gate: image=$rustImage memory=$MemoryLimit cpus=$CpuLimit jobs=$BuildJobs" -ForegroundColor Cyan
try {
    docker run --rm --init `
        --cpus $CpuLimit --memory $MemoryLimit --memory-swap $MemoryLimit `
        --mount "type=bind,source=$root,target=/workspace" `
        --mount "type=volume,source=$targetVolume,target=/workspace/target" `
        --mount "type=volume,source=$registryVolume,target=/usr/local/cargo/registry" `
        --mount "type=volume,source=$gitVolume,target=/usr/local/cargo/git" `
        --workdir /workspace `
        $rustImage sh -c $command
    Assert-Exit "Docker Rust gate failed"
    Write-Host "Docker Rust gate passed" -ForegroundColor Green
} finally {
    # debug 和 release 产物留在命名卷里会持续占数 GB。默认每次跑完删掉编译卷。
    # registry 与 git 卷留给下一次加速；需要一并清掉时运行 scripts/clear-bench-disk.ps1。
    if (-not $KeepCache) {
        docker volume rm --force $targetVolume 2>$null | Out-Null
    }
}
