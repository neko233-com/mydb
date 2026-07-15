#!/usr/bin/env bash
set -euo pipefail

# MyDB Local Build & Package Script
# 构建并打包，上传到 GitHub Releases

VERSION="${1:-}"
TAG="${2:-}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info() { echo -e "${BLUE}[INFO]${NC} $1"; }
success() { echo -e "${GREEN}[OK]${NC} $1"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

# 检查参数
if [ -z "$VERSION" ]; then
    echo "Usage: $0 <version> [tag]"
    echo "Example: $0 0.1.0 v0.1.0"
    exit 1
fi

if [ -z "$TAG" ]; then
    TAG="v$VERSION"
fi

# 检查 gh 是否可用
if ! command -v gh &> /dev/null; then
    error "gh (GitHub CLI) not found. Install: https://cli.github.com/"
fi

# 检查 gh 是否登录
if ! gh auth status &> /dev/null; then
    error "gh not logged in. Run: gh auth login"
fi

# 获取当前平台
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case $ARCH in
    x86_64|amd64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) error "Unsupported architecture: $ARCH" ;;
esac

PLATFORM="${OS}-${ARCH}"
PACKAGE_NAME="mydb-${PLATFORM}"

info "Building for: $PLATFORM"
info "Version: $VERSION"
info "Tag: $TAG"

# 清理旧的构建
info "Cleaning old builds..."
cargo clean --release 2>/dev/null || true

# 构建 release 版本
info "Building release..."
cargo build --release

# 创建打包目录
BUILD_DIR="target/release/package"
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"

# 复制二进制文件
if [ "$OS" = "windows" ]; then
    cp target/release/mydb-server.exe "$BUILD_DIR/"
    cp target/release/mydb-cli.exe "$BUILD_DIR/"
else
    cp target/release/mydb-server "$BUILD_DIR/"
    cp target/release/mydb-cli "$BUILD_DIR/"
fi

# 复制配置文件
cp configs/default.yaml "$BUILD_DIR/config.yaml.example"

# 复制安装脚本
cp scripts/install.sh "$BUILD_DIR/"
cp scripts/install.ps1 "$BUILD_DIR/"

# 复制文档
cp README.md "$BUILD_DIR/"
cp LICENSE "$BUILD_DIR/" 2>/dev/null || true

# 打包
info "Packaging..."
cd "$BUILD_DIR"

if [ "$OS" = "windows" ]; then
    PACKAGE_FILE="../${PACKAGE_NAME}.zip"
    7z a "$PACKAGE_FILE" .
else
    PACKAGE_FILE="../${PACKAGE_NAME}.tar.gz"
    tar -czf "$PACKAGE_FILE" .
fi

cd ../..

PACKAGE_PATH="target/release/package/${PACKAGE_FILE}"
PACKAGE_SIZE=$(du -h "$PACKAGE_PATH" | cut -f1)

success "Package created: $PACKAGE_PATH ($PACKAGE_SIZE)"

# 检查 tag 是否已存在
if gh release view "$TAG" &> /dev/null; then
    warn "Release $TAG already exists. Deleting..."
    gh release delete "$TAG" -y
fi

# 创建 release
info "Creating GitHub release: $TAG"
gh release create "$TAG" \
    --title "MyDB $VERSION" \
    --notes "MyDB $VERSION - MySQL 8.x compatible database" \
    "$PACKAGE_PATH"

success "Release created: https://github.com/neko233-com/mydb/releases/tag/$TAG"
