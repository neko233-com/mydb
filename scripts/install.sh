#!/usr/bin/env bash
set -euo pipefail

# MyDB Installation Script
# Supports: Linux (x86_64, aarch64), macOS (x86_64, aarch64)

REPO="neko233-com/mydb"
VERSION="${VERSION:-latest}"
PACKAGE_PATH="${PACKAGE_PATH:-}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.mydb}"
CONFIG_DIR="${CONFIG_DIR:-$HOME/.config/mydb}"
DATA_DIR="${DATA_DIR:-$HOME/.mydb/data}"
SECRETS_DIR="${SECRETS_DIR:-$CONFIG_DIR/secrets}"
SERVICE_NAME="${SERVICE_NAME:-mydb}"

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
    local release_base
    if [ "$VERSION" = "latest" ]; then
        release_base="https://github.com/${REPO}/releases/latest/download"
    else
        release_base="https://github.com/${REPO}/releases/download/${VERSION}"
    fi
    local url="${release_base}/${filename}.tar.gz"
    
    info "Downloading ${name}..."
    
    local tmp_dir
    tmp_dir=$(mktemp -d)
    
    local archive_path="${tmp_dir}/${filename}.tar.gz"
    local checksum_path="${tmp_dir}/${filename}.tar.gz.sha256"
    if [ -n "$PACKAGE_PATH" ]; then
        [ -f "$PACKAGE_PATH" ] || error "Local package not found: $PACKAGE_PATH"
        cp "$PACKAGE_PATH" "$archive_path"
        if [ -f "${PACKAGE_PATH}.sha256" ]; then
            cp "${PACKAGE_PATH}.sha256" "$checksum_path"
        else
            warn "Local package has no SHA-256 sidecar: $PACKAGE_PATH"
        fi
    else
        if command -v curl &> /dev/null; then
            curl -fsSL "$url" -o "$archive_path"
            curl -fsSL "${url}.sha256" -o "$checksum_path"
        elif command -v wget &> /dev/null; then
            wget -q "$url" -O "$archive_path"
            wget -q "${url}.sha256" -O "$checksum_path"
        else
            error "Neither curl nor wget found"
        fi
    fi

    if [ -f "$checksum_path" ]; then
        local expected actual
        expected=$(awk 'NF {print $1; exit}' "$checksum_path")
        if command -v sha256sum >/dev/null 2>&1; then
            actual=$(sha256sum "$archive_path" | awk '{print $1}')
        else
            actual=$(shasum -a 256 "$archive_path" | awk '{print $1}')
        fi
        [ "$expected" = "$actual" ] || error "SHA-256 verification failed for ${filename}.tar.gz"
        success "SHA-256 verified: ${filename}.tar.gz"
    fi
    
    # Extract
    tar -xzf "$archive_path" -C "$tmp_dir"
    
    local source_dir="$tmp_dir"
    if [ -d "${tmp_dir}/${filename}" ]; then
        source_dir="${tmp_dir}/${filename}"
    fi

    # Move requested binaries
    mkdir -p "$INSTALL_DIR"
    case "$name" in
        server) mv "${source_dir}/mydb-server" "$INSTALL_DIR/" ;;
        cli)
            mv "${source_dir}/mydb-cli" "$INSTALL_DIR/"
            mv "${source_dir}/mydb" "$INSTALL_DIR/"
            ;;
        migrate) mv "${source_dir}/mydb-migrate" "$INSTALL_DIR/" ;;
        dump) mv "${source_dir}/mydbdump" "$INSTALL_DIR/" ;;
        all)
            mv "${source_dir}/mydb-server" "$INSTALL_DIR/"
            mv "${source_dir}/mydb-cli" "$INSTALL_DIR/"
            mv "${source_dir}/mydb" "$INSTALL_DIR/"
            mv "${source_dir}/mydb-migrate" "$INSTALL_DIR/"
            mv "${source_dir}/mydbdump" "$INSTALL_DIR/"
            ;;
    esac
    chmod +x "$INSTALL_DIR"/mydb "$INSTALL_DIR"/mydb-* 2>/dev/null || true
    
    rm -rf "$tmp_dir"
}

# Update only the executable payload. This path intentionally leaves config,
# data, secrets, and service definitions untouched.
update_binaries() {
    local os arch
    os=$(detect_os)
    arch=$(detect_arch)

    if [ -n "${WAIT_FOR_PID:-}" ]; then
        for _ in $(seq 1 240); do
            if ! kill -0 "$WAIT_FOR_PID" 2>/dev/null; then
                break
            fi
            sleep 0.25
        done
        if kill -0 "$WAIT_FOR_PID" 2>/dev/null; then
            error "timed out waiting for the mydb update command to exit"
        fi
    fi

    local service_was_active=false
    if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet "$SERVICE_NAME"; then
        if systemctl stop "$SERVICE_NAME" >/dev/null 2>&1 || sudo systemctl stop "$SERVICE_NAME" >/dev/null 2>&1; then
            service_was_active=true
        else
            error "cannot stop active service $SERVICE_NAME"
        fi
    fi

    download_binary all "$os" "$arch"

    if $service_was_active; then
        systemctl start "$SERVICE_NAME" >/dev/null 2>&1 || sudo systemctl start "$SERVICE_NAME" >/dev/null 2>&1 || error "cannot restart service $SERVICE_NAME"
    fi
    if [ -n "${UPDATE_TEMP_ROOT:-}" ] && [ -d "$UPDATE_TEMP_ROOT" ]; then
        rm -rf "$UPDATE_TEMP_ROOT"
    fi
    success "MyDB binaries updated in $INSTALL_DIR"
}

# Create production-safe secret files. Existing secrets are never rotated by an update.
create_secret() {
    local name=$1
    local path="${SECRETS_DIR}/${name}"
    if [ -f "$path" ]; then
        chmod 600 "$path"
        return
    fi
    command -v openssl >/dev/null 2>&1 || error "openssl is required to generate MyDB secrets"
    umask 077
    openssl rand -base64 36 | tr -d '\r\n' > "$path"
    chmod 600 "$path"
}

create_secrets() {
    mkdir -p "$SECRETS_DIR"
    chmod 700 "$SECRETS_DIR"
    create_secret root
    create_secret admin
    cat > "${SECRETS_DIR}/environment" << EOF
MYDB_ROOT_PASSWORD_FILE=${SECRETS_DIR}/root
MYDB_ADMIN_PASSWORD_FILE=${SECRETS_DIR}/admin
MYDB_ENFORCE_STRONG_PASSWORDS=true
EOF
    chmod 600 "${SECRETS_DIR}/environment"
    success "Strong secrets stored in ${SECRETS_DIR}"
}

# Create config. Secrets are injected through the environment file and never copied into YAML.
create_config() {
    mkdir -p "$CONFIG_DIR"
    if [ ! -f "${CONFIG_DIR}/config.yaml" ]; then
        local tls_cert_value="null"
        local tls_key_value="null"
        local require_tls="false"
        if [ -n "${MYDB_TLS_CERT:-}" ] && [ -n "${MYDB_TLS_KEY:-}" ]; then
            tls_cert_value="\"${MYDB_TLS_CERT}\""
            tls_key_value="\"${MYDB_TLS_KEY}\""
            require_tls="true"
        fi
        cat > "${CONFIG_DIR}/config.yaml" << EOF
server:
  host: "127.0.0.1"
  port: 3306
  max_connections: 512
  thread_count: 0
  connect_timeout: 10
  interactive_timeout: 28800

http:
  host: "127.0.0.1"
  port: 4306
  admin_username: "admin"
  admin_password: "CHANGE_ME_USE_MYDB_ADMIN_PASSWORD_FILE"
  enabled: true

storage:
  data_dir: "${DATA_DIR}"
  engine: "neko233"
  buffer_pool_size: "512M"
  log_file_size: "256M"
  page_size: 16384
  group_commit_window_us: 250

memory:
  max_memory: "1G"
  query_cache_size: "0"
  sort_buffer_size: "4M"

security:
  default_username: "root"
  default_password: "CHANGE_ME_USE_MYDB_ROOT_PASSWORD_FILE"
  authentication: "caching_sha2_password"
  require_secure_transport: ${require_tls}
  tls_cert: ${tls_cert_value}
  tls_key: ${tls_key_value}
  enforce_strong_passwords: true
  local_infile: false
  secure_file_priv: "${DATA_DIR}/imports"
  max_load_data_size: 1073741824

logging:
  level: "info"
  file: ""
  max_size: "100M"
  max_files: 14

character_set:
  server: "utf8mb4"
  connection: "utf8mb4"
  results: "utf8mb4"

agent:
  enabled: true
  slow_query_threshold_ms: 100
  max_slow_queries: 4096
EOF
        success "Config created at ${CONFIG_DIR}/config.yaml"
    fi
}

# Create data directory
create_data_dir() {
    mkdir -p "${DATA_DIR}/imports"
    chmod 700 "$DATA_DIR" "${DATA_DIR}/imports"
    success "Data directory created at ${DATA_DIR}"
}

# Install service (optional)
install_service() {
    local os=$(detect_os)
    
    if [ "$os" = "linux" ]; then
        info "Installing systemd service..."
        local user_name
        local group_name
        user_name=$(id -un)
        group_name=$(id -gn)
        
        sudo tee "/etc/systemd/system/${SERVICE_NAME}.service" > /dev/null << EOF
[Unit]
Description=MyDB Server
After=network.target

[Service]
Type=simple
User=${user_name}
Group=${group_name}
WorkingDirectory=${DATA_DIR}
EnvironmentFile=${SECRETS_DIR}/environment
ExecStart=${INSTALL_DIR}/mydb-server --config ${CONFIG_DIR}/config.yaml
Restart=on-failure
RestartSec=5
LimitNOFILE=65536
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=${DATA_DIR}
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
EOF

        sudo systemctl daemon-reload
        success "Systemd service installed"
        info "Enable with: sudo systemctl enable ${SERVICE_NAME}"
        info "Start with: sudo systemctl start ${SERVICE_NAME}"
        
    elif [ "$os" = "macos" ]; then
        local plist_dir="$HOME/Library/LaunchAgents"
        local plist_path="${plist_dir}/com.neko233.${SERVICE_NAME}.plist"
        mkdir -p "$plist_dir"
        cat > "$plist_path" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.neko233.${SERVICE_NAME}</string>
<key>ProgramArguments</key><array><string>${INSTALL_DIR}/mydb-server</string><string>--config</string><string>${CONFIG_DIR}/config.yaml</string></array>
<key>EnvironmentVariables</key><dict>
<key>MYDB_ROOT_PASSWORD_FILE</key><string>${SECRETS_DIR}/root</string>
<key>MYDB_ADMIN_PASSWORD_FILE</key><string>${SECRETS_DIR}/admin</string>
<key>MYDB_ENFORCE_STRONG_PASSWORDS</key><string>true</string>
</dict>
<key>WorkingDirectory</key><string>${DATA_DIR}</string>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
</dict></plist>
EOF
        launchctl bootout "gui/$(id -u)/com.neko233.${SERVICE_NAME}" 2>/dev/null || true
        launchctl bootstrap "gui/$(id -u)" "$plist_path"
        success "launchd service installed: ${plist_path}"
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

    # Remove files left by pre-single-node releases. The current package has
    # no router component; MyDB clients connect directly to TCP 3306.
    rm -f "$INSTALL_DIR/mydb-router" "$INSTALL_DIR/router.yaml"
    
    info "OS: ${os}"
    info "Architecture: ${arch}"
    
    case "$component" in
        update)
            update_binaries
            ;;
        server)
            download_binary "server" "$os" "$arch"
            success "Server installed to ${INSTALL_DIR}/mydb-server"
            ;;
        cli)
            download_binary "cli" "$os" "$arch"
            success "CLI installed to ${INSTALL_DIR}/mydb and ${INSTALL_DIR}/mydb-cli"
            ;;
        migrate)
            download_binary "migrate" "$os" "$arch"
            success "Migration CLI installed to ${INSTALL_DIR}/mydb-migrate"
            ;;
        dump)
            download_binary "dump" "$os" "$arch"
            success "Backup CLI installed to ${INSTALL_DIR}/mydbdump"
            ;;
        all)
            download_binary "all" "$os" "$arch"
            success "Server, CLI (mydb/mydb-cli), and migration tool installed to ${INSTALL_DIR}"
            ;;
        service)
            install_service
            ;;
        *)
            echo "Usage: $0 [update|server|cli|migrate|dump|all|service]"
            exit 1
            ;;
    esac
    
    if [ "$component" = "update" ]; then
        return
    fi
    if [ "$component" != "service" ]; then
        create_secrets
        create_config
        create_data_dir
        setup_path
        
        echo ""
        echo -e "${GREEN}Installation complete!${NC}"
        echo ""
        echo "Quick start:"
        echo "  MYDB_ROOT_PASSWORD_FILE=${SECRETS_DIR}/root MYDB_ADMIN_PASSWORD_FILE=${SECRETS_DIR}/admin MYDB_ENFORCE_STRONG_PASSWORDS=true ${INSTALL_DIR}/mydb-server --config ${CONFIG_DIR}/config.yaml"
        echo ""
        echo "Connect with:"
        echo "  ${INSTALL_DIR}/mydb-cli -h 127.0.0.1 -P 3306 -u root --password \$(cat ${SECRETS_DIR}/root)"
        echo "  ${INSTALL_DIR}/mydb-migrate --help"
        echo "  ${INSTALL_DIR}/mydbdump --help"
    else
        create_secrets
        create_config
        create_data_dir
        install_service
    fi
}

main "$@"
