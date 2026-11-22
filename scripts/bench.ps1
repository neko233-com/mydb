#Requires -Version 5.1
param(
    [switch]$SkipBuild,
    [switch]$Keep,
    [ValidateRange(1, 5)]
    [int]$Samples = 1,
    [string]$MemoryLimit = "2g",
    [ValidateRange(1, 8)]
    [int]$CpuLimit = 2
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Podman is the preferred container runtime (AGENTS.md); Docker remains a
# compatibility fallback for hosts that have not migrated yet.
$podman = Get-Command podman -ErrorAction SilentlyContinue
$docker = Get-Command docker -ErrorAction SilentlyContinue
if ($null -ne $podman) {
    $env:MYDB_CONTAINER_RUNTIME = "podman"
    $runtimeName = "podman"
} elseif ($null -ne $docker) {
    $env:MYDB_CONTAINER_RUNTIME = "docker"
    $runtimeName = "docker"
} else {
    throw "Podman or Docker is required; Windows-native Rust tests are disabled"
}

if (-not $SkipBuild) {
    if ($null -ne $docker) {
        & (Join-Path $PSScriptRoot "test-docker.ps1") `
            -MemoryLimit $MemoryLimit -CpuLimit $CpuLimit -BuildJobs 1
        if ($LASTEXITCODE -ne 0) { throw "Container Rust gate failed" }
    } else {
        Write-Warning "test-docker.ps1 still requires Docker; run cargo clippy/test natively before benchmarking with podman."
    }
}

$python = Get-Command python -ErrorAction SilentlyContinue
if ($null -eq $python) { $python = Get-Command python3 -ErrorAction SilentlyContinue }
if ($null -eq $python) { throw "Python is required to drive the container benchmark" }

$arguments = @(
    (Join-Path $PSScriptRoot "bench_docker.py"),
    "--samples", "$Samples",
    "--memory", $MemoryLimit,
    "--cpus", "$CpuLimit"
)
if ($SkipBuild) { $arguments += "--skip-build" }
if ($Keep) { $arguments += "--keep" }

Write-Host "$runtimeName benchmark: memory=$MemoryLimit cpus=$CpuLimit samples=$Samples" -ForegroundColor Cyan
& $python.Source @arguments
if ($LASTEXITCODE -ne 0) { throw "$runtimeName benchmark failed" }
