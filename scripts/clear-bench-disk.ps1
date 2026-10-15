#Requires -Version 5.1
<#
.SYNOPSIS
    Remove MyDB benchmark and test leftovers. Safe to run repeatedly.

.DESCRIPTION
    Deletes only names owned by MyDB benches and tests: compose projects
    mydb-bench and mydb-smoke, IO-bench containers and volumes, fault-injection
    leftovers, the Rust test volumes, and local io-bench result directories.
    It does not remove rust, mysql, debian, golang, or ubuntu base images,
    and it does not prune the whole Docker builder cache.
#>
param(
    [switch]$KeepCache,
    [switch]$IncludeDevImage
)

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot

function Write-Step([string]$message) {
    Write-Host $message
}

function Remove-ExactContainer([string]$name) {
    $names = @(docker ps --all --format "{{.Names}}" 2>$null)
    if ($names -contains $name) {
        docker rm --force $name 2>&1 | Out-Null
        Write-Step "removed container $name"
    }
}

function Remove-PrefixContainers([string]$prefix) {
    $names = @(docker ps --all --format "{{.Names}}" 2>$null)
    foreach ($name in $names) {
        if ($name.StartsWith($prefix)) {
            docker rm --force $name 2>&1 | Out-Null
            Write-Step "removed container $name"
        }
    }
}

function Remove-ExactVolume([string]$name) {
    $names = @(docker volume ls --format "{{.Name}}" 2>$null)
    if ($names -contains $name) {
        docker volume rm --force $name 2>&1 | Out-Null
        Write-Step "removed volume $name"
    }
}

function Remove-PrefixVolumes([string]$prefix) {
    $names = @(docker volume ls --format "{{.Name}}" 2>$null)
    foreach ($name in $names) {
        if ($name.StartsWith($prefix)) {
            docker volume rm --force $name 2>&1 | Out-Null
            Write-Step "removed volume $name"
        }
    }
}

function Remove-ExactImage([string]$reference) {
    docker image inspect $reference 2>$null | Out-Null
    if ($LASTEXITCODE -eq 0) {
        docker image rm --force $reference 2>&1 | Out-Null
        Write-Step "removed image $reference"
    }
}

$dockerReady = $true
docker info 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) {
    $dockerReady = $false
    Write-Step "Docker is not available; only local result directories will be removed"
}

if ($dockerReady) {
    foreach ($project in @("mydb-bench", "mydb-smoke")) {
        docker compose -p $project down --volumes --remove-orphans 2>&1 | Out-Null
        Write-Step "compose project $project is down"
    }

    foreach ($name in @(
            "mydb-io-desktop",
            "mysql80-io-desktop",
            "mydb-page-corrupt",
            "mydb-bench",
            "mysql-bench"
        )) {
        Remove-ExactContainer $name
    }
    foreach ($prefix in @("mydb-io-bench-", "mysql-io-bench-", "mydb-fault-", "mydb-enospc-", "mydb-bench-")) {
        Remove-PrefixContainers $prefix
    }

    foreach ($name in @(
            "mydb-io-desktop-data",
            "mysql80-io-desktop-data",
            "mydb-smoke-page-corrupt-data"
        )) {
        Remove-ExactVolume $name
    }
    if (-not $KeepCache) {
        foreach ($name in @("mydb-rust-target", "mydb-rust-registry", "mydb-rust-git")) {
            Remove-ExactVolume $name
        }
    }
    Remove-PrefixVolumes "mydb-fault-"

    foreach ($reference in @("mydb:io-bench", "mydb:bench", "mydb-bench:bench", "mydb-bench:ubuntu24")) {
        Remove-ExactImage $reference
    }
    if ($IncludeDevImage) {
        Remove-ExactImage "mydb:dev"
    }

    foreach ($cacheId in @("mydb-target-trixie", "mydb-target-bookworm", "mydb-bench-bookworm-target")) {
        docker buildx prune --force --filter "id=$cacheId" 2>&1 | Out-Null
    }
}

foreach ($relative in @("target\io-bench", "target\io-bench-desktop")) {
    $path = Join-Path $root $relative
    if (Test-Path -LiteralPath $path) {
        Remove-Item -LiteralPath $path -Recurse -Force
        Write-Step "removed $relative"
    }
}
$targetRoot = Join-Path $root "target"
if (Test-Path -LiteralPath $targetRoot) {
    Get-ChildItem -LiteralPath $targetRoot -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like "io-bench*" } |
        ForEach-Object {
            Remove-Item -LiteralPath $_.FullName -Recurse -Force
            Write-Step "removed target\$($_.Name)"
        }
}

Write-Step "MyDB bench disk cleanup finished"
exit 0
