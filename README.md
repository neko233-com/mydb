# MyDB

<div align="center">

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.75+-orange.svg)](https://www.rust-lang.org/)
[![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey.svg)]()

**单机高性能数据库 · MySQL 8.4 协议兼容目标 · MySQL 替代目标**

</div>

MyDB 是一款用 Rust 编写的单机高性能数据库，目标是替代 MySQL 8.4 的常用单机部署。它以 MySQL 协议暴露接口，JDBC、Go、Node.js/TypeScript、JetBrains、VS Code、dbx 和标准 MySQL 客户端均走同一协议入口；内部采用自研 Neko233 Leader/Follower 组提交、Group Commit、WAL 与 Copy-on-Write 存储内核。

> **设计边界**：MyDB 是**单机数据库**，复制拓扑、读写分离、分布式 XA 协调等非单机能力**设计上不支持**。

---

## ✨ 核心特性

| 特性 | 说明 |
|------|------|
| 🔌 **MySQL 兼容** | MySQL 8 协议、CLI/驱动直连；SQL 覆盖以已验收兼容矩阵为准 |
| ⚡ **Leader/Follower 组提交** | 调用方线程协作的严格 FIFO 写，适合玩家状态和游戏事件更新 |
| 💾 **事务支持** | `BEGIN`/`COMMIT`/`ROLLBACK`/`SAVEPOINT`，断线自动回滚 |
| 📊 **Prometheus 监控** | `/metrics` 原生导出连接、查询、锁、WAL、存储等指标 |
| 🔧 **内置 Agent** | HTTP API 提供健康诊断、慢 SQL 分析、SQL 静态检查 |
| 📦 **mydbdump** | 独立 CLI：无表锁一致性快照、Zstd 压缩、校验、增量备份与恢复 |
| 🚀 **高性能** | 零 GC 暂停，内存安全，Rust 实现接近 C++ 性能 |
| 🌐 **跨平台** | Linux (x86_64/aarch64)、macOS (x86_64/aarch64)、Windows (x86_64) |
| ⚙️ **YAML 配置** | 简洁的配置文件格式 |
| 🛡️ **安全** | 支持 TLS、强密码策略、安全文件目录、审计日志 |
| 📦 **一键安装** | 提供 PowerShell 和 Shell 安装脚本 |
| 🔄 **在线迁移** | 独立 `mydb-migrate` 工具支持从 MySQL 8 在线迁移 |

---

## 🏗️ 架构

```
mydb/
├── crates/
│   ├── mydb-server/       # 服务端主体
│   ├── mydb-cli/          # 命令行客户端（兼容 mysql 命令）
│   ├── mydb-wire/         # MySQL 协议兼容层
│   ├── mydb-parser/       # SQL 解析器
│   ├── mydb-storage/      # Neko233 存储引擎（InnoDB 仅为外部兼容别名）
│   ├── mydb-transaction/  # 事务管理与锁
│   ├── mydb-config/       # YAML 配置解析
│   ├── mydb-wal/          # Write-Ahead Log 实现
│   ├── mydb-migrate/      # MySQL 8 在线迁移工具
│   ├── mydb-dump/         # 备份/恢复 CLI（mydbdump）
│   └── mydb-bench/        # 性能基准测试
├── scripts/               # 安装脚本与 Docker 辅助脚本
├── configs/               # 配置文件模板
└── vendor/                # 第三方依赖（patched opensrv-mysql）
```

### 存储引擎：Neko233

- **Leader/Follower FIFO 写入**：首个空闲写者成为 leader 串行 drain 写队列、每组单次 WAL fsync，保证顺序一致性，无专用写线程
- **Leader/Follower Group Commit**：吞吐默认 250μs 收集窗口；`group_commit_window_us=0` 切换为低延迟自然批量
- **WAL 单块写入**：预分配文件（8MB 粒度）+ 64KB 可复用缓冲区，`append_raw` 直写热路径，单 `write_all` 原子追加，CRC32 校验
- **bincode fixint 编码**：WAL 记录使用小端固定长度整数编码，序列化/反序列化比 varint 更快
- **WAL-backed Memtable**：INSERT 追加到内存表（pending_rewrites），不直接写数据页
- **Copy-on-Write Checkpoint**：每 1024 个已提交请求折叠一次，staging→backup→rename 原子替换，仅重建受影响表的索引和缓存
- **幂等 Redo**：崩溃恢复时 WAL 重放幂等，预分配零字节尾部通过 CRC 校验安全截断
- **CRC 校验**：WAL 和数据页均带 CRC，检测损坏并安全拒绝启动
- **提交热路径**：一次 `sync_data()` 顺序 fsync = 持久性保证，锁内仅 write+fsync，无额外 syscall
- **MVCC 基础**：持久化 row-id、事务 commit 序号、RR/SERIALIZABLE 读视图、RC 语句视图、历史版本链与旧版本清理基础已接入；事务读不再复制整库快照
- **索引锁基础**：主键/二级索引的 record、next-key、gap、insert-intention 锁；二级索引锁定读会同步锁定命中的聚簇记录；复合索引左前缀与单列索引 `LIKE` 前缀范围锁，以及 statement-duration 基础 MDL 已覆盖已验收路径；JOIN 锁定读已覆盖最终命中基表聚簇记录、二级索引点锁、列对列比较范围锁和基础 `SKIP LOCKED` 行过滤，完整隐式锁与 InnoDB 边界仍按验收清单推进
- **InnoDB 名称兼容**：`ENGINE=InnoDB` 在 SQL/协议层映射至 Neko233；项目不加载或复用 MySQL InnoDB 源码，`MEMORY` 保持独立语义

---

## 🚀 快速开始

### Docker（推荐用于开发）

运行时镜像基于 `debian:13-slim`，默认限制 0.5 CPU、512 MiB 内存：

```bash
# 1. 创建环境变量文件
cat > .env <<'EOF'
MYDB_ROOT_PASSWORD=$(openssl rand -base64 36)
MYDB_ADMIN_PASSWORD=$(openssl rand -base64 36)
EOF

# 2. 启动服务
docker compose up -d --build

# 3. 连接数据库
mysql --protocol=TCP -h 127.0.0.1 -P 3306 -u root -p
```

**环境变量说明：**

| 变量 | 说明 | 必填 |
|------|------|------|
| `MYDB_ROOT_PASSWORD` | root 用户密码 | 是 |
| `MYDB_ADMIN_PASSWORD` | 管理员密码（HTTP API） | 是 |
| `MYDB_PORT` | SQL 服务端口（默认 3306） | 否 |
| `MYDB_HTTP_PORT` | HTTP 管理端口（默认 4306） | 否 |
| `MYDB_DATA_DIR` | 数据目录 | 否 |
| `MYDB_LOG_LEVEL` | 日志级别（debug/info/warn/error） | 否 |
| `MYDB_GROUP_COMMIT_WINDOW_US` | Group Commit 窗口（微秒） | 否 |
| `MYDB_ROOT_PASSWORD_FILE` | 从文件读取 root 密码（Docker Secrets） | 否 |
| `MYDB_ADMIN_PASSWORD_FILE` | 从文件读取 admin 密码（Docker Secrets） | 否 |

> 💡 同一密钥不可同时设置普通变量和 `_FILE` 变量。

停止服务但保留数据：
```bash
docker compose down
```

### 一键安装脚本

**Linux / macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/neko233-com/mydb/main/scripts/install.sh | bash
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/neko233-com/mydb/main/scripts/install.ps1 | iex
```

Windows 默认安装到 `C:\Server\mydb\mydb-server`，创建 `MyDBServer` 自动启动服务，监听
`0.0.0.0:3306`，配置和数据分别位于 `config\`、`data\`。首次安装会生成强随机 root/admin 密钥到
`config\secrets\root` 与 `config\secrets\admin`，并通过 Windows 服务环境注入；升级幂等保留已有配置、数据和密钥，只替换二进制。远程连接需要 Windows 防火墙允许 TCP 3306。

Linux/macOS 安装默认监听本机 `127.0.0.1:3306`，密钥位于 `~/.config/mydb/secrets/`；需要 systemd/launchd 托管时再执行 `bash scripts/install.sh service`。已有配置不会被覆盖，需管理员自行完成旧配置的密码/TLS 加固。

```powershell
# 本地发布包升级（不重复发布版本）
.\scripts\install.ps1 -PackagePath .\mydb-windows-x86_64.zip
# 若同目录存在 .sha256，安装器会先校验；远程下载始终强制校验 SHA-256。

# 静默安装/升级：不弹 PowerShell 窗口；首次提权仍可能显示 UAC 同意框
cscript //nologo .\scripts\install-silent.vbs -PackagePath .\mydb-windows-x86_64.zip

# 卸载 MySQL/MariaDB；先完成迁移和备份，再按需加 -PurgeData
.\scripts\uninstall-mysql.ps1
```

Docker 开发配置中的 root/root 仅用于本地兼容 smoke；新安装不使用该默认值。公网部署必须使用密钥文件、启用 TLS，并限制防火墙来源。

Linux/macOS 也支持离线包升级：
```bash
PACKAGE_PATH=./mydb-linux-x86_64.tar.gz bash scripts/install.sh all
```
远程包和带 `.sha256` sidecar 的本地包会先校验 SHA-256；未带 sidecar 的本地包只用于受控离线场景并会告警。

### 从源码编译

要求：Rust 1.75+

```bash
# 克隆仓库
git clone https://github.com/neko233-com/mydb.git
cd mydb

# 编译（Release 模式）
cargo build --release

# 正式包只包含 server/cli/mydb/migrate/dump、配置、安装脚本和文档；
# mydb-bench、测试结果与 target/bench 不进入发布包
.\scripts\build-release.ps1 -Version "0.1.12"
# 发布包同时生成同名 `.sha256` 校验文件；发布脚本会拒绝覆盖已存在的 GitHub Release。

# 安装到系统
cargo install --path crates/mydb-server
cargo install --path crates/mydb-cli
cargo install --path crates/mydb-migrate
cargo install --path crates/mydb-dump
```

### 命令行更新

发布包提供 `mydb`（兼容保留 `mydb-cli`）命令。更新只替换二进制，保留现有配置、数据目录和密钥；下载包与 `.sha256` sidecar 会先校验。

```bash
# 检查最新稳定版，不改文件
mydb update --check

# 更新到最新稳定版
mydb update

# 指定版本
mydb update --version v0.1.12
```

Windows 服务更新会在当前 CLI 退出后由后台 helper 完成，日志写入安装目录的 `mydb-update.log`；Linux 更新会自动处理同名 systemd 服务。

---

## ⚙️ 配置

配置文件使用 YAML 格式，默认位置：
- Linux: `/etc/mydb/config.yaml`
- macOS: `/usr/local/etc/mydb/config.yaml`
- Windows: `C:\Server\mydb\mydb-server\config\config.yaml`

### 生产级配置示例

参考 [configs/production.yaml](configs/production.yaml)：

```yaml
server:
  host: "127.0.0.1"          # 生产环境限制到 loopback
  port: 3306
  max_connections: 1000
  thread_count: 4

storage:
  data_dir: "/var/lib/mydb"
  buffer_pool_size: "1G"
  group_commit_window_us: 250   # 吞吐默认；0 = 低延迟自然批量

security:
  authentication: "caching_sha2_password" # MySQL 8.4 默认
  require_secure_transport: true  # 生产环境强制 TLS
  tls_cert: "/etc/mydb/tls/server.crt"
  tls_key: "/etc/mydb/tls/server.key"
  local_infile: false            # 禁用 LOCAL INFILE
  secure_file_priv: "/var/lib/mydb/imports"

logging:
  level: "info"
  file: "/var/log/mydb/mydb.log"
```

### 启动服务

```bash
# 使用默认配置
mydb-server

# 使用自定义配置
mydb-server --config /path/to/config.yaml

# 后台启动（Linux/macOS）
mydb-server --daemon
```

### 连接数据库

```bash
# 使用 mydb-cli
mydb-cli -h 127.0.0.1 -P 3306 -u root -p

# 执行单条 SQL
mydb-cli -h 127.0.0.1 -P 3306 -u root -p -e "SELECT VERSION()"

# 执行 SQL 脚本
mydb-cli -h 127.0.0.1 -P 3306 -u root -p --source schema.sql

# 或使用标准 MySQL 客户端
mysql -h 127.0.0.1 -P 3306 -u root -p
```

### JDBC / JetBrains / VS Code / Go / Node.js

MyDB 暴露标准 MySQL TCP 协议，不要求使用 `mydb-cli`。JetBrains DataGrip/IDEA、VS Code
MySQL 扩展和数据库插件使用 MySQL 数据源：Host `127.0.0.1`、Port `3306`、User `root`。
Docker 开发配置密码为 root；原生新安装请读取 `config/secrets/root`。JDBC URL：

```text
jdbc:mysql://127.0.0.1:3306/game_db_0?useSSL=false&serverTimezone=UTC
```

Go `database/sql`（`github.com/go-sql-driver/mysql`）：

```go
db, err := sql.Open("mysql", "root:root@tcp(127.0.0.1:3306)/game_db_0?charset=utf8mb4&parseTime=true&loc=Local")
```

Node.js / VS Code JavaScript/TypeScript（`mysql2`）：

```js
const db = await mysql.createConnection({
  host: "127.0.0.1", port: 3306, user: "root", password: "root", database: "game_db_0"
});
```

远程客户端将 Host 改为服务器 IP；Windows 安装脚本默认监听全部网卡并创建 TCP 3306 入站规则，Linux/macOS 默认仅监听本机。
生产环境应改为强密码、TLS 或明确的来源 IP 白名单。
启用 `security.enforce_strong_passwords` 后，管理密码只能用于登录换取短期 session token，不能直接作为 Bearer；Prometheus `/metrics` 也需要该 token。CLI Agent 会自动完成登录。

## 🔒 生产部署最佳实践

### 1. 安全配置

```bash
# 创建密钥目录
install -m 0700 -d /run/mydb-secrets
umask 077

# 生成强密码
openssl rand -base64 36 > /run/mydb-secrets/root
openssl rand -base64 36 > /run/mydb-secrets/admin

# 使用密钥文件启动
MYDB_ROOT_PASSWORD_FILE=/run/mydb-secrets/root \
MYDB_ADMIN_PASSWORD_FILE=/run/mydb-secrets/admin \
mydb-server --config /etc/mydb/production.yaml
```

### 2. TLS 加密

配置 `security.tls_cert`、`security.tls_key` 并设置 `require_secure_transport: true`，强制所有连接使用 TLS。

### 3. 备份策略

每次版本升级前后执行恢复演练：
1. 创建全量备份
2. 写入校验数据
3. 创建增量备份
4. 恢复到隔离实例
5. 核对数据完整性

---

## 📊 监控与运维

### Prometheus 指标

```bash
curl http://127.0.0.1:4306/metrics  # 弱密码/开发模式；强密码模式使用登录后的 Bearer token
```

暴露的关键指标：
- 连接数、活跃事务、锁等待
- 查询 QPS、慢查询计数
- WAL fsync 延迟、Group Commit 批次大小
- Checkpoint 耗时、存储页使用量
- 错误率、死锁次数

### HTTP 管理 API

强密码模式先登录取得短期 session token；弱密码/开发模式才允许管理员密码直接作为 Bearer：

```bash
# 强密码模式
TOKEN=$(curl -fsS -X POST -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"'"$(cat /run/mydb-secrets/admin)"'"}' \
  http://127.0.0.1:4306/api/v1/auth/login | jq -r .token)

# 服务状态
curl -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:4306/api/v1/status

# 健康检查
curl -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:4306/api/v1/agent/health

# 慢查询列表
curl -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:4306/api/v1/agent/slow-queries

# 按 SHOW PROCESSLIST 的连接 ID 断开连接并回滚其未提交事务
curl -fsS -X POST -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"connection_id":123}' \
  http://127.0.0.1:4306/api/v1/connections/kill

# 自然语言诊断
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"question":"为什么写入延迟高？"}' \
  http://127.0.0.1:4306/api/v1/agent/ask
```

也可以通过 CLI 访问：
```bash
mydb-cli --admin-password "$(cat /run/mydb-secrets/admin)" agent health
mydb-cli --admin-password "$(cat /run/mydb-secrets/admin)" agent slow
mydb-cli --admin-password "$(cat /run/mydb-secrets/admin)" agent ask "最近有哪些慢 SQL？"
```

### 内置 Web SQL IDE

打开 `http://127.0.0.1:4306/`；使用 `http.admin_username` / `http.admin_password` 登录；完整使用文档：
`http://127.0.0.1:4306/admin/doc`。
登录后可直接执行 SQL、查看结果集、切换数据库、格式化查询和退出会话；执行入口复用
MyDB 的同一套 SQL parser、权限、事务、WAL 与 MVCC，不是旁路模拟器。接口为：

Web 后台默认自动识别浏览器中英文；右上角可手动切换 `Auto`、`中文`、`English`，选择保存在当前浏览器。
登录既支持 HTTP 管理账号，也支持真实 MySQL 用户；后者按该用户的 SQL 权限执行查询。

```bash
# 登录，返回短期 Bearer session token
curl -X POST -H "Content-Type: application/json" \
  -d '{"username":"root","password":"root"}' \
  http://127.0.0.1:4306/api/v1/auth/login

# Web SQL IDE 使用的统一执行入口
curl -X POST -H "Authorization: Bearer <session-token>" \
  -H "Content-Type: application/json" \
  -d '{"sql":"SELECT 1 AS ok","database":null}' \
  http://127.0.0.1:4306/api/v1/sql/query
```

---

## 💾 备份与恢复

### 使用 mydbdump

`mydbdump` 提供无表锁一致性快照、Zstd 压缩、表级增量备份：

```bash
# 全量备份
mydbdump backup \
  --url 'mysql://root:password@127.0.0.1:3306' \
  --database game \
  --output backups/game-full

# 验证备份
mydbdump verify --input backups/game-full

# 增量备份
mydbdump backup \
  --url 'mysql://root:password@127.0.0.1:3306' \
  --database game \
  --output backups/game-inc-1 \
  --incremental-from backups/game-full/manifest.json

# 恢复全量
mydbdump restore \
  --url 'mysql://root:root@127.0.0.1:3306' \
  --database game \
  --input backups/game-full

# 应用增量
mydbdump restore \
  --url 'mysql://root:root@127.0.0.1:3306' \
  --database game \
  --input backups/game-inc-1
```

### HTTP 备份 API

```bash
# 强密码模式：先登录取得短期 session token，再调用管理 API。
TOKEN=$(curl -fsS -X POST -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"'"$(cat /run/mydb-secrets/admin)"'"}' \
  http://127.0.0.1:4306/api/v1/auth/login | jq -r .token)

# 全量备份
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:4306/api/v1/backup/full

# LSN 增量备份
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"base_id":"full-..."}' \
  http://127.0.0.1:4306/api/v1/backup/incremental

# 时间点恢复 (PITR；必须显式确认，避免误恢复)
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"id":"incremental-...","point_in_time":"2026-07-15T14:22:53.842Z","confirmation":"RESTORE_BACKUP:incremental-..."}' \
  http://127.0.0.1:4306/api/v1/backup/restore

# 删除备份也必须按备份 ID 显式确认
curl -X DELETE -H "Authorization: Bearer $TOKEN" \
  -H "X-MyDB-Confirm: DELETE_BACKUP:full-..." \
  http://127.0.0.1:4306/api/v1/backup/full-...
```

---

## 🔄 从 MySQL 迁移

使用 `mydb-migrate` 工具在线迁移：

```bash
mydb-migrate \
  --source 'mysql://root:password@127.0.0.1:3306' \
  --target 'mysql://root:root@127.0.0.1:3306' \
  --database game \
  --batch-size 500 \
  --report migration-report.json
```

特性：
- 源端一致性快照，不锁业务表
- 流式读取，批量写入
- 逐表校验行数和 SHA-256 内容摘要
- 保留 NULL、BLOB、日期时间、微秒精度
- 支持 `MYSQL_SOURCE_URL`/`MYDB_TARGET_URL` 环境变量避免密码泄露

添加 `--drop-existing` 可覆盖目标已有表。

也可以使用标准 `mysqldump`：
```bash
mysqldump --single-transaction --quick --set-gtid-purged=OFF --no-tablespaces game > game.sql
mysql -h 127.0.0.1 -P 3306 game < game.sql
```

---

## 📝 SQL 兼容概览

### DDL

- `CREATE DATABASE`/`DROP DATABASE`
- `CREATE TABLE`（含列定义、主键、索引、外键、CHECK 约束）
- `FULLTEXT`/`SPATIAL` 索引：类型持久化、CREATE/ALTER、SHOW INDEX、`information_schema.STATISTICS`，以及全文 TF-IDF 基础相关性、布尔必选/排除/短语/前缀、基础查询扩展检索和 POINT 空间函数；完整倒排索引/可配置停止词、CJK ngram、GIS 类型仍按兼容矩阵推进
- `CREATE TABLE ... LIKE ...`（跨 schema 复制结构）
- `CREATE TABLE ... AS SELECT ...`（快照建表）
- `ALTER TABLE`（ADD/DROP/MODIFY/CHANGE COLUMN、ADD/DROP INDEX/PRIMARY KEY/FOREIGN KEY/CHECK）
- `CREATE INDEX`/`DROP INDEX`
- 表级逻辑分区：`PARTITION BY RANGE/LIST/HASH/KEY`，含复合 `RANGE COLUMNS`、常用表达式分区函数（`YEAR`/`MONTH`/`DAY`/`QUARTER`/`TO_DAYS`/`TO_SECONDS`/`UNIX_TIMESTAMP`/`ABS`/`MOD`）的写入校验、重启恢复和 `information_schema.PARTITIONS` 元数据；数据保持单表物理布局，子分区/在线重组/物理裁剪仍按兼容矩阵推进
- `CREATE VIEW`/`DROP VIEW`；复杂 JOIN/聚合/表达式视图只读，单基表直接列投影支持 DML 与 `WITH CHECK OPTION`
- `CREATE TEMPORARY TABLE`（连接级临时表）
- `CREATE TRIGGER`/`DROP TRIGGER`（BEFORE/AFTER INSERT/UPDATE/DELETE）
- `CREATE PROCEDURE`/`DROP PROCEDURE`（含 IN/OUT/INOUT、游标、条件处理、诊断）
- `CREATE FUNCTION`/`DROP FUNCTION`
- Routine 局部变量显式 `CHARACTER SET`/`COLLATE`：比较、`LIKE`、CASE/IF、DML、`COLLATION()`/`CHARSET()` 路径已接入；默认数据库字符集与完整排序规则权重仍按兼容矩阵推进
- `CREATE EVENT`/`DROP EVENT`
- `TRUNCATE TABLE`、`RENAME TABLE`

### DML

- `INSERT`/`REPLACE`/`INSERT IGNORE`
- `INSERT ... SET col=expr`（MySQL 语法）
- `INSERT ... ON DUPLICATE KEY UPDATE`（含 `new.col` 别名、`VALUES(col)`）
- `INSERT ... SELECT`
- `UPDATE`（单表、JOIN UPDATE、ORDER BY/LIMIT）
- `DELETE`（单表、多表 DELETE、USING 语法）
- `JOIN ... ON` 支持列对列等值/范围、常量、常用标量函数与算术表达式；锁定读对非索引表达式使用安全表级回退
- `SELECT`（JOIN、子查询、CTE、窗口函数、GROUP BY、聚合、HAVING（含常用未关联与分组相关标量子查询）、ORDER BY、LIMIT/OFFSET、DISTINCT、SQL_CALC_FOUND_ROWS）
- `MATCH(...) AGAINST(...)` 基础自然语言/布尔检索；`ST_GeomFromText`、`ST_AsText`、`ST_X`、`ST_Y`、`ST_GeometryType`
- `LOAD DATA [LOCAL] INFILE`
- `PREPARE`/`EXECUTE`/`DEALLOCATE PREPARE`（SQL 级命名预处理语句）

### 事务

- `BEGIN`/`START TRANSACTION`/`COMMIT`/`ROLLBACK`
- `SAVEPOINT`/`ROLLBACK TO SAVEPOINT`/`RELEASE SAVEPOINT`
- 隔离级别：`READ UNCOMMITTED`/`READ COMMITTED`/`REPEATABLE READ`/`SERIALIZABLE`
- `SELECT ... FOR UPDATE`/`SELECT ... FOR SHARE`
- `NOWAIT`/`SKIP LOCKED`
- 死锁检测与受害者回滚

### 函数

- **字符串**：CONCAT、SUBSTRING、TRIM、REPLACE、LPAD/RPAD、UPPER/LOWER、HEX/UNHEX、Base64、MD5、SHA1、SHA2、CRC32、REGEXP 等
- **数值**：ABS、CEIL/FLOOR、ROUND、MOD、POW/SQRT、RAND、PI、三角函数、BIT_COUNT、BIT_AND、BIT_OR、BIT_XOR、CONV 等
- **日期时间**：NOW、CURDATE、CURTIME、DATE_ADD/DATE_SUB、DATEDIFF、TIMESTAMPDIFF、DATE_FORMAT、UNIX_TIMESTAMP/FROM_UNIXTIME、CONVERT_TZ（内置 IANA 时区）、WEEK/YEARWEEK、EXTRACT 等
- **JSON**：JSON_EXTRACT、JSON_UNQUOTE、JSON_OBJECT、JSON_ARRAY、JSON_VALID、JSON_TYPE、JSON_LENGTH、JSON_CONTAINS、JSON_CONTAINS_PATH、JSON_OVERLAPS、JSON_SET、JSON_REMOVE、JSON_ARRAY_APPEND、JSON_ARRAY_INSERT、JSON_MERGE_PATCH、JSON_DEPTH、JSON_KEYS、JSON_PRETTY
- **其他**：UUID、INET_ATON/INET_NTOA、INET6_ATON/INET6_NTOA、GROUP_CONCAT、IF、CASE、NULLIF、COALESCE、CAST/CONVERT 等

### 系统表

- `information_schema`（SCHEMATA、TABLES、COLUMNS、STATISTICS、TABLE_CONSTRAINTS、KEY_COLUMN_USAGE、CHECK_CONSTRAINTS、REFERENTIAL_CONSTRAINTS、VIEWS、TRIGGERS、ROUTINES、PARAMETERS、EVENTS、APPLICABLE_ROLES 等）；系统库自身的 TABLES/COLUMNS 元数据也可自描述，方便 DataGrip/JDBC/ORM 二次探测
- `mysql` 系统库（用户、权限、角色、`role_edges` 与 MariaDB/GoLand 兼容的 `roles_mapping`）
- `performance_schema`（常用表）
- `sys` 视图

> 完整的 SQL 兼容矩阵请参阅 [SYNTAX_MATRIX.md](SYNTAX_MATRIX.md)。

---

## 🔧 存储引擎

### Neko233（默认）

- 持久化存储引擎
- 完整 ACID 支持
- Leader/Follower 组提交、Group Commit、WAL、COW Checkpoint
- 主键/唯一索引、外键、CHECK 约束
- `ENGINE=InnoDB` 是外部兼容别名；未知引擎返回 MySQL 1286，实际兼容范围以 [CheckList.md](CheckList.md) 已验收项为准
- 崩溃恢复回归覆盖 WAL 预分配零尾、torn write 与中段损坏；完整故障注入/平台验收仍是发布门槛

### MEMORY

- 纯内存非事务存储
- `ROLLBACK` 不撤销写入
- 重启后仅保留表结构，清空数据
- 适用于临时缓存场景

---

## 🧪 开发与测试

```bash
# 编译检查
cargo check --workspace

# Clippy 代码检查
cargo clippy --workspace --all-targets -- -D warnings

# 运行所有测试
cargo test --workspace

# Docker 烟测（Windows PowerShell）
.\scripts\docker-smoke.ps1

# Docker 烟测（Linux/macOS Bash）
bash scripts/docker-smoke.sh
```

---

## 📊 性能

> 完整测试方法、运行指标和发布前验证见 [性能报告.md](性能报告.md)。下表为 2026-08-14 Docker `linux/amd64` 受控实测；MyDB 与 MySQL 8.4.11 使用相同 CPU/内存限制和持久化设置，性能阶段不预热、1 次采样且限时 60 秒。
>
> 本轮数据：MyDB/MySQL 均为 2 vCPU、2 GiB；MySQL 8.4.11 使用 `innodb_flush_log_at_trx_commit=1`、`sync_binlog=1`，MyDB 使用默认 250μs Group Commit 与每 1024 个已提交请求 checkpoint。

| 场景 | MyDB | MySQL 8.4.11 | MyDB / MySQL |
|------|------|--------------|--------------|
| 单表写（fsync-per-commit） | 211 ops/s | 82 ops/s | 2.58x |
| 4 actor / 4表 写 P99 延迟 | 34.9 ms | 55.4 ms | 1.59x（低更好） |
| 4 actor / 4表 Group Commit | 325 ops/s | 144 ops/s | 2.25x |
| 读 P50 延迟 | 388 μs | 131 μs | - |

性能优化不以关闭 WAL 持久化或弱化恢复语义换取数字。默认 250μs Group Commit 窗口优先并发吞吐，checkpoint 按 1024 个已提交请求触发；不声明未经实测证明的固定倍数。

---

## 📋 验收状态

当前开发状态、已完成项、未完成项、差分证据统一维护在 [CheckList.md](CheckList.md)。只有可复现实测证明的项目才会打勾。

### v0.1.12 发布候选

本版面向单机 MySQL 8.4 常用生产工作负载：3306 提供 MySQL 协议，4306 提供登录保护的 Web SQL IDE/管理 API；补齐常用 JSON 数组追加/插入、RFC 7396 合并、深度/键枚举/美化输出，并保留全文自然语言 TF-IDF 基础相关性、布尔短语/前缀、基础查询扩展、`mydb update` 跨平台事务替换/失败回滚和非 ASCII tar 文件名校验。发布不宣称复制/集群、完整 InnoDB 全部锁边界、全部冷门字符集或全部 MySQL 错误码已完成；逐项状态见 [CheckList.md](CheckList.md) 和 [SYNTAX_MATRIX.md](SYNTAX_MATRIX.md)。

**本轮本地门槛（2026-08-14）：**
- ✅ `cargo test --workspace --locked`：Docker Linux 门禁通过；wire 269（含权限委派/角色 ADMIN OPTION/ALL 部分撤销），其他 workspace 测试与文档测试全部通过
- ✅ `cargo clippy --workspace --all-targets -- -D warnings`
- ✅ `cargo build --release -p mydb-server -p mydb-cli -p mydb-migrate -p mydb-dump`
- ✅ `mydb update --check`：GitHub Release 资产下载、SHA-256 校验和包结构验证通过；更新仅替换二进制并保留配置/数据/密钥
- ✅ MySQL 8.4 CLI 3306 连接、`event_scheduler`/版本探测
- ✅ Connector/J 9.1.0、Node mysql2 3.23.3 已完成 3306 普通/预处理查询 smoke；Go `database/sql` + go-sql-driver/mysql 1.10.0 已完成当前 release 3306 服务的 Ping、中文、DATE、普通/预处理查询回归
- ✅ MySQL `'user'@'host'` 基础账户匹配：精确主机优先于通配主机，握手按账户插件选择认证方式
- ✅ `scripts/bench.ps1`：Docker Linux Rust gate、release build 与同条件 MySQL 8.4 持久化基准通过；性能阶段无预热、60 秒硬截止（报告见 [性能报告.md](性能报告.md)）
- ✅ `scripts/mysql84-diff.ps1`：当前源码隔离端口与同机 Docker MySQL 8.4，105/105 差分通过；新增 JSON 数组追加/插入、合并、深度/键/美化输出及全文相关性、布尔短语/前缀、停止词/短词边界与查询扩展覆盖
- ⏳ Ubuntu 24.04 物理性能、macOS 原生验收、宿主断电/恢复中断、大数据压力与生产安全运维验收：以 [CheckList.md](CheckList.md) 与 [性能报告.md](性能报告.md) 为准

---

## 🤝 贡献

欢迎提交 Issue 和 Pull Request！

---

## 📄 许可证

Copyright © 2026 neko233

本项目采用 [MIT License](LICENSE) 开源。

---

## 🙏 致谢

- [opensrv-mysql](https://github.com/polkasign/opensrv-mysql) - MySQL 协议实现（MyDB 使用 vendor 并打补丁版本）
- MySQL 是 Oracle Corporation 的注册商标
