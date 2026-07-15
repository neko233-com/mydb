#!/usr/bin/env bash
set -euo pipefail

# MyDB Installation Script
# Supports: Linux (x86_64, aarch64), macOS (x86_64, aarch64)

REPO="neko233-com/mydb"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.mydb}"
CONFIG_DIR="${CONFIG_DIR:-$HOME/.config/mydb}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info() { echo -e "${BLUE}[INFO]${NC} $1"; }
success() { echo -e "${GREEN}[OK]${NC} $1"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

# Detect architecture
detect_arch() {
    local arch
    arch=$(uname -m)
    case $arch in
        x86_64|amd64) echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        *) error "Unsupported architecture: $arch" ;;
    esac
}

# Detect OS
detect_os() {
    local os
    os=$(uname -s)
    case $os in
        Linux) echo "linux" ;;
        Darwin) echo "macos" ;;
        *) error "Unsupported OS: $os" ;;
    esac
}

# Download binary
download_binary() {
    local name=$1
    local os=$2
    local arch=$3
    
    local filename="mydb-${os}-${arch}"
    local url="https://github.com/${REPO}/releases/download/${VERSION}/${filename}.tar.gz"
    
    info "Downloading ${name}..."
    
    local tmp_dir
    tmp_dir=$(mktemp -d)
    
    if command -v curl &> /dev/null; then
        curl -fsSL "$url" -o "${tmp_dir}/${filename}.tar.gz"
    elif command -v wget &> /dev/null; then
        wget -q "$url" -O "${tmp_dir}/${filename}.tar.gz"
    else
        error "Neither curl nor wget found"
    fi
    
    # Extract
    tar -xzf "${tmp_dir}/${filename}.tar.gz" -C "$tmp_dir"
    
    # Move binaries
    mkdir -p "$INSTALL_DIR"
    mv "${tmp_dir}/${filename}/mydb-server" "$INSTALL_DIR/"
    mv "${tmp_dir}/${filename}/mydb-cli" "$INSTALL_DIR/"
    chmod +x "$INSTALL_DIR/mydb-server" "$INSTALL_DIR/mydb-cli"
    
    rm -rf "$tmp_dir"
}

# Create config
create_config() {
    mkdir -p "$CONFIG_DIR"
    
    if [ ! -f "${CONFIG_DIR}/config.yaml" ]; then
        cat > "${CONFIG_DIR}/config.yaml" << 'EOF'
server:
  host: "0.0.0.0"
  port: 3306
  max_connections: 1000
  thread_count: 0

storage:
  data_dir: "~/.mydb/data"
  engine: "innodb"
  buffer_pool_size: "1G"
  page_size: 16384

security:
  authentication: "mysql_native_password"
  require_secure_transport: false

logging:
  level: "info"
  file: ""
EOF
        success "Config created at ${CONFIG_DIR}/config.yaml"
    fi
}

# Create data directory
create_data_dir() {
    local data_dir="$HOME/.mydb/data"
    mkdir -p "$data_dir"
    success "Data directory created at ${data_dir}"
}

# Install service (optional)
install_service() {
    local os=$(detect_os)
    
    if [ "$os" = "linux" ]; then
        info "Installing systemd service..."
        
        sudo tee /etc/systemd/system/mydb.service > /dev/null << EOF
[Unit]
Description=MyDB Server
After=network.target

[Service]
Type=simple
ExecStart=${INSTALL_DIR}/mydb-server --config ${CONFIG_DIR}/config.yaml
Restart=always
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

        sudo systemctl daemon-reload
        success "Systemd service installed"
        info "Enable with: sudo systemctl enable mydb"
        info "Start with: sudo systemctl start mydb"
        
    elif [ "$os" = "macos" ]; then
        info "For macOS, use launchctl or run mydb-server directly"
    fi
}

# Add to PATH
setup_path() {
    local shell_rc=""
    
    if [ -f "$HOME/.bashrc" ]; then
        shell_rc="$HOME/.bashrc"
    elif [ -f "$HOME/.zshrc" ]; then
        shell_rc="$HOME/.zshrc"
    fi
    
    if [ -n "$shell_rc" ]; then
        if ! grep -q "$INSTALL_DIR" "$shell_rc"; then
            echo "export PATH=\"\$PATH:$INSTALL_DIR\"" >> "$shell_rc"
            success "Added to PATH in ${shell_rc}"
            info "Run: source ${shell_rc}"
        fi
    fi
}

# Main
main() {
    local component="${1:-all}"
    
    echo -e "${BLUE}MyDB Installer${NC}"
    echo "=================="
    
    local os=$(detect_os)
    local arch=$(detect_arch)
    
    info "OS: ${os}"
    info "Architecture: ${arch}"
    
    case "$component" in
        server)
            download_binary "server" "$os" "$arch"
            success "Server installed to ${INSTALL_DIR}/mydb-server"
            ;;
        cli)
            download_binary "cli" "$os" "$arch"
            success "CLI installed to ${INSTALL_DIR}/mydb-cli"
            ;;
        all)
            download_binary "server" "$os" "$arch"
            download_binary "cli" "$os" "$arch"
            success "Server and CLI installed to ${INSTALL_DIR}"
            ;;
        service)
            install_service
            ;;
        *)
            echo "Usage: $0 [server|cli|all|service]"
            exit 1
            ;;
    esac
    
    if [ "$component" != "service" ]; then
        create_config
        create_data_dir
        setup_path
        
        echo ""
        echo -e "${GREEN}Installation complete!${NC}"
        echo ""
        echo "Quick start:"
        echo "  ${INSTALL_DIR}/mydb-server --config ${CONFIG_DIR}/config.yaml"
        echo ""
        echo "Connect with:"
        echo "  ${INSTALL_DIR}/mydb-cli -h 127.0.0.1 -P 3306 -u root"
    fi
}

main "$@"
