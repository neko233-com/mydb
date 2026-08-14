#Requires -Version 5.1
<#
.SYNOPSIS
    MyDB Local Build & Package Script for Windows

.DESCRIPTION
    构建并打包，上传到 GitHub Releases
    
.PARAMETER Version
    版本号，如 0.1.0
    
.PARAMETER Tag
    Git tag，如 v0.1.0（可选，默认为 v<version>）
    
.EXAMPLE
    .\build-release.ps1 -Version "0.1.0"
    .\build-release.ps1 -Version "0.1.0" -Tag "v0.1.0"
#>

param(
    [Parameter(Mandatory=$true)]
    [string]$Version,
    
    [string]$Tag = "v$Version"
)

$ErrorActionPreference = "Stop"

# Colors
function Write-Info { Write-Host "[INFO] $args" -ForegroundColor Blue }
function Write-Success { Write-Host "[OK] $args" -ForegroundColor Green }
function Write-Warn { Write-Host "[WARN] $args" -ForegroundColor Yellow }
function Write-Error { Write-Host "[ERROR] $args" -ForegroundColor Red; exit 1 }

# 检查 gh 是否可用
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    Write-Error "gh (GitHub CLI) not found. Install: https://cli.github.com/"
}

# 检查 gh 是否登录
gh auth status 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Error "gh not logged in. Run: gh auth login"
}

# 获取架构
$arch = [System.Environment]::GetEnvironmentVariable("PROCESSOR_ARCHITEW6432")
if ([string]::IsNullOrWhiteSpace($arch)) {
    $arch = [System.Environment]::GetEnvironmentVariable("PROCESSOR_ARCHITECTURE")
}
switch ($arch) {
    "AMD64" { $arch = "x86_64" }
    "ARM64" { $arch = "aarch64" }
    default { Write-Error "Unsupported architecture: $arch" }
}

$platform = "windows-${arch}"
$packageName = "mydb-${platform}"

Write-Info "Building for: $platform"
Write-Info "Version: $Version"
Write-Info "Tag: $Tag"

# 发布物不可覆盖；提前失败，避免无意义地重建本地包。
$existingRelease = gh release view $Tag 2>&1
if ($LASTEXITCODE -eq 0) {
    Write-Error "Release $Tag already exists. Refusing to delete or replace it."
}

# 发布物不可覆盖；先做 Rust 质量门禁，再构建。
Write-Info "Running Docker Rust quality gate (2 GiB limit)..."
& (Join-Path $PSScriptRoot "test-docker.ps1") -MemoryLimit "2g" -CpuLimit 2 -BuildJobs 1
if ($LASTEXITCODE -ne 0) { Write-Error "Docker Rust quality gate failed" }

# 构建 release 版本
Write-Info "Building release..."
cargo build --release -p mydb-server -p mydb-cli -p mydb-migrate -p mydb-dump
if ($LASTEXITCODE -ne 0) { Write-Error "Build failed" }

# 创建打包目录
$buildDir = "target/release/package"
# 仅清理明确的打包目录；保留 Cargo release 缓存，避免测试/增量构建拖慢后续打包。
if (Test-Path $buildDir) { Remove-Item -Recurse -Force $buildDir }
New-Item -ItemType Directory -Path $buildDir -Force | Out-Null

# 复制二进制文件
Copy-Item "target/release/mydb-server.exe" "$buildDir/"
Copy-Item "target/release/mydb-cli.exe" "$buildDir/"
Copy-Item "target/release/mydb.exe" "$buildDir/"
Copy-Item "target/release/mydb-migrate.exe" "$buildDir/"
Copy-Item "target/release/mydbdump.exe" "$buildDir/"

# 复制配置文件
Copy-Item "configs/default.yaml" "$buildDir/config.yaml.example"

# 复制安装脚本
Copy-Item "scripts/install.sh" "$buildDir/"
Copy-Item "scripts/install.ps1" "$buildDir/"
Copy-Item "scripts/install-silent.vbs" "$buildDir/"

# 复制文档
Copy-Item "README.md" "$buildDir/"
Copy-Item "CheckList.md" "$buildDir/"
Copy-Item "SYNTAX_MATRIX.md" "$buildDir/"
Copy-Item "性能报告.md" "$buildDir/"
if (Test-Path "LICENSE") { Copy-Item "LICENSE" "$buildDir/" }

# 打包
Write-Info "Packaging..."
$packagePath = "target/release/${packageName}.zip"
if (Test-Path -LiteralPath $packagePath) {
    Remove-Item -LiteralPath $packagePath -Force
}
$packageFile = "target/release/package/${packageName}.zip"
$sevenZip = Get-Command 7z -ErrorAction SilentlyContinue
if ($null -ne $sevenZip) {
    Push-Location $buildDir
    & $sevenZip.Source a "../${packageName}.zip" .
    Pop-Location
} else {
    Compress-Archive -Path (Join-Path $buildDir '*') -DestinationPath (Join-Path (Split-Path $buildDir -Parent) "${packageName}.zip") -CompressionLevel Optimal
}

$packageSize = (Get-Item $packagePath).Length / 1MB
$checksumPath = "$packagePath.sha256"
$checksum = (Get-FileHash -LiteralPath $packagePath -Algorithm SHA256).Hash.ToLowerInvariant()
"$checksum  $packageName.zip" | Set-Content -LiteralPath $checksumPath -Encoding ASCII -NoNewline

Write-Success "Package created: $packagePath ($([math]::Round($packageSize, 2)) MB)"
Write-Success "Checksum created: $checksumPath"

# 创建 release
Write-Info "Creating GitHub release: $Tag"
gh release create $Tag `
    --title "MyDB $Version" `
    --notes "MyDB $Version - MySQL 8.x compatible database" `
    $packagePath `
    $checksumPath

Write-Success "Release created: https://github.com/neko233-com/mydb/releases/tag/$Tag"
