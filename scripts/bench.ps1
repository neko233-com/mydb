#Requires -Version 5.1
param(
    [switch]$SkipBuild,
    [switch]$Keep,
    [ValidateRange(1, 5)]
    [int]$Samples = 3,
    [string]$MemoryLimit = "2g",
    [ValidateRange(1, 8)]
    [int]$CpuLimit = 2
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw "Docker is required; Windows-native Rust tests are disabled"
}

if (-not $SkipBuild) {
    & (Join-Path $PSScriptRoot "test-docker.ps1") `
        -MemoryLimit $MemoryLimit -CpuLimit $CpuLimit -BuildJobs ([Math]::Min($CpuLimit, 2))
    if ($LASTEXITCODE -ne 0) { throw "Docker Rust gate failed" }
}

$python = Get-Command python -ErrorAction SilentlyContinue
if ($null -eq $python) { $python = Get-Command python3 -ErrorAction SilentlyContinue }
if ($null -eq $python) { throw "Python is required to drive the Docker benchmark" }

$arguments = @(
    (Join-Path $PSScriptRoot "bench_docker.py"),
    "--samples", "$Samples",
    "--memory", $MemoryLimit,
    "--cpus", "$CpuLimit"
)
if ($SkipBuild) { $arguments += "--skip-build" }
if ($Keep) { $arguments += "--keep" }

Write-Host "Docker benchmark: memory=$MemoryLimit cpus=$CpuLimit samples=$Samples" -ForegroundColor Cyan
& $python.Source @arguments
if ($LASTEXITCODE -ne 0) { throw "Docker benchmark failed" }
