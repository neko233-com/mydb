# MyDB

MySQL 8.x 兼容的开源数据库，用 Rust 编写。作为 MySQL 的直接替代品，提供更高的性能和更低的延迟。

## 特性

- **MySQL 8.x 完全兼容** - 支持 MySQL 语法、协议和配置
- **InnoDB 存储引擎兼容** - 事务支持、MVCC、行级锁
- **高性能** - 零 GC 暂停，内存安全，接近 C++ 的性能
- **跨平台** - 支持 Linux (x86_64/aarch64), macOS (x86_64/aarch64), Windows (x86_64)
- **YAML 配置** - 简洁的配置文件格式
- **一键安装** - 提供 PowerShell 和 Shell 安装脚本

## 架构

```
mydb/
├── crates/
│   ├── mydb-server/      # 服务端主体
│   ├── mydb-cli/          # 命令行客户端
│   ├── mydb-wire/         # MySQL 协议兼容层
│   ├── mydb-parser/       # SQL 解析
│   ├── mydb-storage/      # 存储引擎（InnoDB 兼容）
│   ├── mydb-transaction/  # 事务管理
│   └── mydb-config/       # YAML 配置解析
├── scripts/               # 安装脚本
├── configs/               # 配置文件模板
└── tests/                 # 集成测试
```

## 快速开始

### 使用安装脚本

**Linux / macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/neko233-com/mydb/main/scripts/install.sh | bash
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/neko233-com/mydb/main/scripts/install.ps1 | iex
```

### 手动安装

```bash
# 安装 server
cargo install --path crates/mydb-server

# 安装 cli
cargo install --path crates/mydb-cli
```

### 启动服务

```bash
# 使用默认配置启动
mydb-server

# 使用自定义配置
mydb-server --config /path/to/config.yaml

# 后台启动（Linux/macOS）
mydb-server --daemon

# 作为服务运行（需要安装为系统服务）
mydb-server --service install
mydb-server --service start
```

### 连接数据库

```bash
# 使用 mydb-cli（兼容 mysql 命令）
mydb-cli -h 127.0.0.1 -P 3306 -u root -p

# 或者使用标准 mysql 客户端（完全兼容）
mysql -h 127.0.0.1 -P 3306 -u root -p
```

## 配置

配置文件使用 YAML 格式，默认位置：

- Linux: `/etc/mydb/config.yaml`
- macOS: `/usr/local/etc/mydb/config.yaml`
- Windows: `%APPDATA%\mydb\config.yaml`

示例配置：

```yaml
server:
  host: "0.0.0.0"
  port: 3306
  max_connections: 1000
  thread_count: 8

storage:
  data_dir: "/var/lib/mydb"
  engine: "innodb"
  buffer_pool_size: "1G"
  log_file_size: "256M"

security:
  authentication: "mysql_native_password"
  require_secure_transport: false

logging:
  level: "info"
  file: "/var/log/mydb/mydb.log"
```

## 端口

默认端口 **3306**，与 MySQL 完全相同，可以直接替换 MySQL 使用。

## 开发

```bash
# 运行测试
cargo test

# 运行基准测试
cargo bench

# 构建发布版本
cargo build --release

# 代码检查
cargo clippy
cargo fmt --check
```

## 许可证

MIT License
