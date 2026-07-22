# MyDB

<div align="center">

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.75+-orange.svg)](https://www.rust-lang.org/)
[![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey.svg)]()

**单机高性能数据库 · MySQL 协议兼容 · SQLite 替代者**

</div>

MyDB 是一款用 Rust 编写的单机高性能数据库，定位为 **SQLite 的替代者**。它以 MySQL 协议暴露接口，已验证范围内的现有 MySQL 驱动/工具可直接接入；内部采用自研 Neko233 Actor 顺序写、Group Commit、WAL 与 Copy-on-Write 存储内核，专为游戏行业写多读少、低交互延迟和低资源常驻场景优化。

> **设计边界**：MyDB 是**单机数据库**，复制拓扑、读写分离、分布式 XA 协调等非单机能力**设计上不支持**。

---

## ✨ 核心特性

| 特性 | 说明 |
|------|------|
| 🔌 **MySQL 兼容** | MySQL 8 协议、CLI/驱动直连；SQL 覆盖以已验收兼容矩阵为准 |
| ⚡ **Actor 顺序写** | 所有写批次进入有界 FIFO，适合玩家状态和游戏事件更新 |
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

- **Actor FIFO 写入**：单写 Actor 保证顺序一致性，无锁竞争
- **Leader/Follower Group Commit**：自然批量（fsync 期间请求自动聚合），零延迟默认；`group_commit_window_us` 可配置额外合并窗口（牺牲延迟换吞吐）
- **WAL 单块写入**：预分配文件（8MB 粒度）+ 64KB 可复用缓冲区，`append_raw` 直写热路径，单 `write_all` 原子追加，CRC32 校验
- **bincode fixint 编码**：WAL 记录使用小端固定长度整数编码，序列化/反序列化比 varint 更快
- **WAL-backed Memtable**：INSERT 追加到内存表（pending_rewrites），不直接写数据页
- **Copy-on-Write Checkpoint**：每 64 个提交组执行一次，staging→backup→rename 原子替换，仅重建受影响表的索引和缓存
- **幂等 Redo**：崩溃恢复时 WAL 重放幂等，预分配零字节尾部通过 CRC 校验安全截断
- **CRC 校验**：WAL 和数据页均带 CRC，检测损坏并安全拒绝启动
- **提交热路径**：一次 `sync_data()` 顺序 fsync = 持久性保证，锁内仅 write+fsync，无额外 syscall
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

### 从源码编译

要求：Rust 1.75+

```bash
# 克隆仓库
git clone https://github.com/neko233-com/mydb.git
cd mydb

# 编译（Release 模式）
cargo build --release

# 安装到系统
cargo install --path crates/mydb-server
cargo install --path crates/mydb-cli
cargo install --path crates/mydb-migrate
cargo install --path crates/mydb-dump
```

---

## ⚙️ 配置

配置文件使用 YAML 格式，默认位置：
- Linux: `/etc/mydb/config.yaml`
- macOS: `/usr/local/etc/mydb/config.yaml`
- Windows: `%APPDATA%\mydb\config.yaml`

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
  group_commit_window_us: 0     # 0 = 自然批量（推荐），N 微秒额外合并窗口（牺牲延迟换吞吐量）

security:
  authentication: "mysql_native_password"
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

---

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
curl http://127.0.0.1:4306/metrics
```

暴露的关键指标：
- 连接数、活跃事务、锁等待
- 查询 QPS、慢查询计数
- WAL fsync 延迟、Group Commit 批次大小
- Checkpoint 耗时、存储页使用量
- 错误率、死锁次数

### HTTP 管理 API

使用管理员密码作为 Bearer Token：

```bash
# 服务状态
curl -H "Authorization: Bearer <admin-password>" \
  http://127.0.0.1:4306/api/v1/status

# 健康检查
curl -H "Authorization: Bearer <admin-password>" \
  http://127.0.0.1:4306/api/v1/agent/health

# 慢查询列表
curl -H "Authorization: Bearer <admin-password>" \
  http://127.0.0.1:4306/api/v1/agent/slow-queries

# 自然语言诊断
curl -X POST -H "Authorization: Bearer <admin-password>" \
  -H "Content-Type: application/json" \
  -d '{"question":"为什么写入延迟高？"}' \
  http://127.0.0.1:4306/api/v1/agent/ask
```

也可以通过 CLI 访问：
```bash
mydb-cli --admin-password <admin-password> agent health
mydb-cli --admin-password <admin-password> agent slow
mydb-cli --admin-password <admin-password> agent ask "最近有哪些慢 SQL？"
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
# 全量备份
curl -X POST -H "Authorization: Bearer <admin-password>" \
  http://127.0.0.1:4306/api/v1/backup/full

# LSN 增量备份
curl -X POST -H "Authorization: Bearer <admin-password>" \
  -H "Content-Type: application/json" \
  -d '{"base_id":"full-..."}' \
  http://127.0.0.1:4306/api/v1/backup/incremental

# 时间点恢复 (PITR)
curl -X POST -H "Authorization: Bearer <admin-password>" \
  -H "Content-Type: application/json" \
  -d '{"id":"incremental-...","point_in_time":"2026-07-15T14:22:53.842Z"}' \
  http://127.0.0.1:4306/api/v1/backup/restore
```

---

## 🔄 从 MySQL 迁移

使用 `mydb-migrate` 工具在线迁移：

```bash
mydb-migrate \
  --source 'mysql://root:password@127.0.0.1:3306' \
  --target 'mysql://root:root@127.0.0.1:13306' \
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
- `CREATE TABLE ... LIKE ...`（跨 schema 复制结构）
- `CREATE TABLE ... AS SELECT ...`（快照建表）
- `ALTER TABLE`（ADD/DROP/MODIFY/CHANGE COLUMN、ADD/DROP INDEX/PRIMARY KEY/FOREIGN KEY/CHECK）
- `CREATE INDEX`/`DROP INDEX`
- `CREATE VIEW`/`DROP VIEW`（只读视图）
- `CREATE TEMPORARY TABLE`（连接级临时表）
- `CREATE TRIGGER`/`DROP TRIGGER`（BEFORE/AFTER INSERT/UPDATE/DELETE）
- `CREATE PROCEDURE`/`DROP PROCEDURE`（含 IN/OUT/INOUT、游标、条件处理、诊断）
- `CREATE FUNCTION`/`DROP FUNCTION`
- `CREATE EVENT`/`DROP EVENT`
- `TRUNCATE TABLE`、`RENAME TABLE`

### DML

- `INSERT`/`REPLACE`/`INSERT IGNORE`
- `INSERT ... SET col=expr`（MySQL 语法）
- `INSERT ... ON DUPLICATE KEY UPDATE`（含 `new.col` 别名、`VALUES(col)`）
- `INSERT ... SELECT`
- `UPDATE`（单表、JOIN UPDATE、ORDER BY/LIMIT）
- `DELETE`（单表、多表 DELETE、USING 语法）
- `SELECT`（JOIN、子查询、CTE、窗口函数、GROUP BY、聚合、HAVING、ORDER BY、LIMIT/OFFSET、DISTINCT、SQL_CALC_FOUND_ROWS）
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
- **数值**：ABS、CEIL/FLOOR、ROUND、MOD、POW/SQRT、RAND、PI、三角函数、BIT_COUNT、CONV 等
- **日期时间**：NOW、CURDATE、CURTIME、DATE_ADD/DATE_SUB、DATEDIFF、TIMESTAMPDIFF、DATE_FORMAT、UNIX_TIMESTAMP/FROM_UNIXTIME、CONVERT_TZ（内置 IANA 时区）、WEEK/YEARWEEK、EXTRACT 等
- **JSON**：JSON_EXTRACT、JSON_UNQUOTE、JSON_OBJECT、JSON_ARRAY、JSON_VALID、JSON_TYPE、JSON_LENGTH、JSON_CONTAINS、JSON_SET、JSON_REMOVE
- **其他**：UUID、INET_ATON/INET_NTOA、INET6_ATON/INET6_NTOA、GROUP_CONCAT、IF、CASE、NULLIF、COALESCE、CAST/CONVERT 等

### 系统表

- `information_schema`（SCHEMATA、TABLES、COLUMNS、STATISTICS、TABLE_CONSTRAINTS、KEY_COLUMN_USAGE、CHECK_CONSTRAINTS、REFERENTIAL_CONSTRAINTS、VIEWS、TRIGGERS、ROUTINES、PARAMETERS、EVENTS 等）
- `mysql` 系统库（用户、权限、角色）
- `performance_schema`（常用表）
- `sys` 视图

> 完整的 SQL 兼容矩阵请参阅 [SYNTAX_MATRIX.md](SYNTAX_MATRIX.md)。

---

## 🔧 存储引擎

### Neko233（默认）

- 持久化存储引擎
- 完整 ACID 支持
- Actor 顺序写、Group Commit、WAL、COW Checkpoint
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

> 完整性能报告、测试方法、历史版本数据和发布前验证流程见 [性能报告.md](性能报告.md)。
>
> ⚠️ 以下为开发机 Docker 限速回归数据，不代表物理生产硬件正式验收结论。
>
> MySQL 数字为历史参考值，非本次 Docker 脚本同轮重测结果。
>
> 本轮数据：`44d23f5`，2026-07-22，Docker WSL2（0.5 CPU / 512MiB）。

| 场景 | MyDB | MySQL 8.0.46（历史参考） | 参考比 |
|------|------|--------------|------|
| 单表写 (fsync-per-commit) | 331 ops/s | 3318 ops/s | 0.1x |
| 8 actor / 8表 写 P99 延迟 | 98.4 ms | 495.9 ms | 历史参考 5x |
| 8 actor / 8表 Group Commit | 225 ops/s | 7315 ops/s | 0.03x |
| 读 P50 延迟 | 673 μs | - | - |

MyDB 设计目标是**同资源、同持久化级别下达到 MySQL 10x 写性能**，不以关闭持久化换取数字。

---

## 📋 验收状态

当前开发状态、已完成项、未完成项、差分证据统一维护在 [CheckList.md](CheckList.md)。只有可复现实测证明的项目才会打勾。

**本轮本地门槛（2026-07-22）：**
- ✅ `cargo test --workspace`
- ✅ `cargo clippy --workspace --all-targets -- -D warnings`
- ✅ `cargo build --release -p mydb-server -p mydb-bench`
- ⏳ Docker 崩溃恢复、限额性能与跨平台生产验收：以 [CheckList.md](CheckList.md) 与 [性能报告.md](性能报告.md) 为准

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
