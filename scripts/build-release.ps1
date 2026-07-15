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
try {
    gh auth status 2>&1 | Out-Null
} catch {
    Write-Error "gh not logged in. Run: gh auth login"
}

# 获取架构
$arch = [System.Environment]::GetEnvironmentVariable("PROCESSOR_ARCHITECTURE")
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

# 清理旧的构建
Write-Info "Cleaning old builds..."
cargo clean --release 2>$null

# 构建 release 版本
Write-Info "Building release..."
cargo build --release
if ($LASTEXITCODE -ne 0) { Write-Error "Build failed" }

# 创建打包目录
$buildDir = "target/release/package"
if (Test-Path $buildDir) { Remove-Item -Recurse -Force $buildDir }
New-Item -ItemType Directory -Path $buildDir -Force | Out-Null

# 复制二进制文件
Copy-Item "target/release/mydb-server.exe" "$buildDir/"
Copy-Item "target/release/mydb-cli.exe" "$buildDir/"

# 复制配置文件
Copy-Item "configs/default.yaml" "$buildDir/config.yaml.example"

# 复制安装脚本
Copy-Item "scripts/install.sh" "$buildDir/"
Copy-Item "scripts/install.ps1" "$buildDir/"

# 复制文档
Copy-Item "README.md" "$buildDir/"
if (Test-Path "LICENSE") { Copy-Item "LICENSE" "$buildDir/" }

# 打包
Write-Info "Packaging..."
$packageFile = "target/release/package/${packageName}.zip"

Push-Location $buildDir
7z a "../${packageName}.zip" .
Pop-Location

$packagePath = "target/release/package/${packageName}.zip"
$packageSize = (Get-Item $packagePath).Length / 1MB

Write-Success "Package created: $packagePath ($([math]::Round($packageSize, 2)) MB)"

# 检查 tag 是否已存在
$existingRelease = gh release view $Tag 2>&1
if ($LASTEXITCODE -eq 0) {
    Write-Warn "Release $Tag already exists. Deleting..."
    gh release delete $Tag -y
}

# 创建 release
Write-Info "Creating GitHub release: $Tag"
gh release create $Tag `
    --title "MyDB $Version" `
    --notes "MyDB $Version - MySQL 8.x compatible database" `
    $packagePath

Write-Success "Release created: https://github.com/neko233-com/mydb/releases/tag/$Tag"
