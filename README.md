# MyDB

MyDB 是面向单机部署、用于替代 MySQL 8.0.x 的通用事务数据库。对外保持 MySQL 协议、客户端、DDL/DML 和事务使用方式，业务迁移目标是不改 SQL；内部不是 MySQL/InnoDB 分支，而是 Rust 实现的 Neko233 Actor 顺序写、group commit、WAL 与 copy-on-write 存储内核，重点优化游戏行业写多读少、低交互延迟和低资源常驻场景。性能目标是在同资源、同持久化级别下显著高于 MySQL，10x 作为尽力而为的正式验收目标，不以关闭持久化换取数字。

当前已完成项、未完成项、差分证据和最终卸载 MySQL80 的硬门槛统一维护在 [`CheckList.md`](CheckList.md)。只有可复现实测证明的项目才会打勾；未通过完整兼容矩阵和 Ubuntu 24.04 正式性能门禁前，不宣称已经无条件覆盖 MySQL 的全部生产表面。

## 特性

- **标准客户端直连** - MySQL 8 CLI/驱动可通过 `mysql_native_password` 连接
- **Actor 顺序写** - 所有写批次进入有界 FIFO，适合玩家状态和游戏事件更新
- **事务** - `BEGIN` / `COMMIT` / `ROLLBACK` / `SAVEPOINT`，连接断开自动丢弃未提交写集
- **Prometheus** - `/metrics` 原生导出连接、查询、错误、锁、group commit、WAL checkpoint、存储和写队列指标
- **Agent HTTP** - 默认开启健康诊断、慢 SQL、SQL 静态检查
- **mydbdump** - 独立 CLI：无表锁一致性快照、Zstd、校验、增量和恢复
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
│   ├── mydb-storage/      # Neko233 存储引擎（InnoDB 仅为外部兼容别名）
│   ├── mydb-transaction/  # 事务管理
│   └── mydb-config/       # YAML 配置解析
├── scripts/               # 安装脚本
├── configs/               # 配置文件模板
└── tests/                 # 集成测试
```

## 快速开始

### Docker（开发机推荐）

运行时使用官方 `debian:13-slim`；默认限制 0.5 CPU、512 MiB 内存，异常自动重启，数据保存在命名卷：

```bash
cat > .env <<'EOF'
MYDB_ROOT_PASSWORD=change-this-to-a-unique-32-byte-root-secret
MYDB_ADMIN_PASSWORD=change-this-to-a-different-32-byte-admin-secret
EOF
docker compose up -d --build
docker compose ps
mysql --protocol=TCP -h 127.0.0.1 -P 3306 -u root -p
```

Compose 必须显式设置 `MYDB_ROOT_PASSWORD` 和 `MYDB_ADMIN_PASSWORD`，默认强制两个不同且不少于 20 bytes 的 secret；开发烟测才显式关闭该检查。服务还支持 `MYDB_ROOT_PASSWORD_FILE`、`MYDB_ADMIN_PASSWORD_FILE`（Docker/Kubernetes secret 文件）、`MYDB_ENFORCE_STRONG_PASSWORDS`、`MYDB_PORT`、`MYDB_THREAD_COUNT`、`MYDB_HTTP_PORT`、`MYDB_DATA_DIR`、`MYDB_GROUP_COMMIT_WINDOW_US`、`MYDB_LOG_LEVEL`、`MYDB_LOCAL_INFILE`、`MYDB_SECURE_FILE_PRIV`、`MYDB_MAX_LOAD_DATA_SIZE`。同一 secret 不可同时设置普通变量和 `_FILE` 变量。

管理端口只映射到宿主 `127.0.0.1:4306`。停止但保留数据：

```bash
docker compose down
```

Windows Docker Desktop 功能烟测：

```powershell
.\scripts\docker-smoke.ps1
```

Linux / macOS Docker Desktop 功能烟测：

```bash
bash scripts/docker-smoke.sh
```

烟测使用隔离宿主端口 `13316/14316`，通过官方 MySQL 8 CLI 验证 DDL、事务、`LOAD DATA LOCAL INFILE` 字符集/warning、安全目录内服务端 `INFILE`、写入、更新、SIGKILL/WAL 坏尾恢复、Prometheus 和 Agent HTTP。SIGKILL 前会确认未提交脏写已进入事务视图；容器恢复后验证已确认提交仍存在、未提交写已清除，并立即执行新事务验证可写。随后停机向最新 WAL 段追加 5 字节残片，重启必须将文件精确截回注入前的最后有效字节，同时保持数据正确并允许继续写入。镜像运行时为 `debian:13-slim`，支持 Docker Desktop；CI 同时编译测试 Windows、Linux、macOS，Linux 执行完整 Docker 烟测。

Compose 默认使用 `10.233.0.0/24` 独立子网，避免长期开发机的 Docker 默认地址池耗尽；如与 VPN 或现有网络冲突，可在启动前设置 `MYDB_DOCKER_SUBNET` 覆盖。

### 生产基线

从 [`configs/production.yaml`](configs/production.yaml) 开始。模板默认 `utf8mb4`、禁用 `LOAD DATA LOCAL INFILE`、强制独立强 secret，并把 SQL/HTTP 监听限制到 loopback。密码不进入 YAML、镜像层、命令行或日志：

```bash
install -m 0700 -d /run/mydb-secrets
umask 077
openssl rand -base64 36 > /run/mydb-secrets/root
openssl rand -base64 36 > /run/mydb-secrets/admin
MYDB_ROOT_PASSWORD_FILE=/run/mydb-secrets/root \
MYDB_ADMIN_PASSWORD_FILE=/run/mydb-secrets/admin \
mydb-server --config /etc/mydb/production.yaml
```

MyDB 支持原生 TLS：配置 `security.tls_cert`、`security.tls_key` 和 `security.require_secure_transport=true` 后，未加密 SQL 连接会被拒绝。生产仍应将 SQL/HTTP 监听限制在私网或 loopback，并配合防火墙。SQL 用户、角色、`GRANT`/`REVOKE` 和审计日志已持久化；密钥轮换、完整 MySQL 权限/角色语义和危险操作审批仍在验收清单中，不能直接暴露给不可信公网。

每次版本升级前后至少完成一次恢复演练：创建全量备份、写入校验行、创建增量备份、恢复到隔离实例并核对行数与业务校验。备份目录应位于独立持久化卷，并按恢复目标保留完整 full→incremental 父链。

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

# 安装 MySQL 8 在线迁移工具
cargo install --path crates/mydb-migrate

# 安装高性能备份/恢复 CLI（命令名 mydbdump）
cargo install --path crates/mydb-dump
```

### 从 MySQL 8 在线迁移

迁移工具在源端开启一致性快照，流式读取；目标端按批事务写入；逐表校验行数和无序逐行 SHA-256 内容摘要。默认拒绝覆盖已有目标表：

```bash
mydb-migrate \
  --source 'mysql://root:password@127.0.0.1:3306' \
  --target 'mysql://root:root@127.0.0.1:13306' \
  --database game \
  --batch-size 500 \
  --report migration-report.json
```

确认可覆盖目标同名表时显式增加 `--drop-existing`。也可用 `MYSQL_SOURCE_URL`、`MYDB_TARGET_URL` 环境变量，避免密码进入命令历史。迁移保留 SQL `NULL`、空字节、任意 BLOB、数值、日期时间和微秒。

原版 MySQL 8 `mysqldump` 默认 BLOB 输出和 `--hex-blob` 输出均可由原版 `mysql` CLI 直接导入；MyDB 的 `COM_QUERY` 入口保留任意原始字节，不会强制用 UTF-8 解码。大库仍推荐 `mydbdump`，其分表 Zstd、增量链和校验能力更完整。

```bash
mysqldump --single-transaction --quick \
  --set-gtid-purged=OFF --no-tablespaces game > game.sql
mysql --host=127.0.0.1 --port=3306 game < game.sql
```

若希望 dump 文件始终是纯文本，可额外使用 `--hex-blob`；不是 MyDB 导入的硬性要求。

### mydbdump：无表锁全量/增量备份

`mydbdump` 位于独立目录 `crates/mydb-dump/`。全量备份默认使用单连接 `REPEATABLE READ` 一致性快照，不执行 `LOCK TABLES`。每表独立 Zstd 流，使用批量 INSERT；清单包含 DDL、行内容和压缩文件 SHA-256：

```bash
mydbdump backup \
  --url 'mysql://root:password@127.0.0.1:3306' \
  --database game \
  --output backups/game-full

mydbdump verify --input backups/game-full
```

表级增量会扫描一致性快照，但只落盘变化表；未变化表零数据文件，已删除表记录 tombstone。恢复时先全量，再按顺序应用增量：

```bash
mydbdump backup \
  --url 'mysql://root:password@127.0.0.1:3306' \
  --database game \
  --output backups/game-inc-1 \
  --incremental-from backups/game-full/manifest.json

mydbdump restore \
  --url 'mysql://root:root@127.0.0.1:3306' \
  --database game \
  --input backups/game-full

mydbdump restore \
  --url 'mysql://root:root@127.0.0.1:3306' \
  --database game \
  --input backups/game-inc-1
```

默认拒绝非 InnoDB 表，因为无锁一致性快照无法保证非事务表一致；`--allow-non-transactional` 仅用于明确接受该风险的场景。密码可放在 `MYDBDUMP_URL`。当前增量策略在清单中明确标记为 `table_content_sha256`；不会把重复全量或不完整 WAL 冒充 LSN 增量。

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

# 执行单条 SQL 或脚本
mydb-cli -h 127.0.0.1 -P 3306 -u root -p -D game -e "SELECT COUNT(*) FROM players"
mydb-cli -h 127.0.0.1 -P 3306 -u root -p -D game --source schema.sql

# 或者使用标准 mysql 客户端（完全兼容）
mysql -h 127.0.0.1 -P 3306 -u root -p
```

`mydb-cli` 使用真实 MySQL Wire 协议并输出结果集，不是本地 SQL 占位器。交互模式检测到断线后会为下一条命令重连；为避免重复扣款、发奖等副作用，失败 SQL 不会自动重放。

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
  engine: "neko233" # MySQL 侧 ENGINE=InnoDB 映射到 Neko233
  buffer_pool_size: "1G"
  log_file_size: "256M"
  group_commit_window_us: 250 # 0 = 不等待；并发写入的 WAL fsync 合并窗口

security:
  authentication: "mysql_native_password"
  require_secure_transport: false
  local_infile: true
  secure_file_priv: "/var/lib/mydb/imports"
  max_load_data_size: 1073741824

logging:
  level: "info"
  file: "/var/log/mydb/mydb.log"
```

## 端口

默认端口 **3306**，与 MySQL 完全相同，可以直接替换 MySQL 使用。

## Prometheus 与 Agent HTTP

Prometheus 无认证抓取地址：

```bash
curl http://127.0.0.1:4306/metrics
```

管理和 Agent API 使用管理员密码作为 Bearer Token：

```bash
curl -H "Authorization: Bearer root" http://127.0.0.1:4306/api/v1/status
curl -H "Authorization: Bearer root" http://127.0.0.1:4306/api/v1/agent/health
curl -H "Authorization: Bearer root" http://127.0.0.1:4306/api/v1/agent/slow-queries
curl -X POST -H "Authorization: Bearer root" -H "Content-Type: application/json" \
  -d '{"question":"为什么写入延迟高？"}' \
  http://127.0.0.1:4306/api/v1/agent/ask
curl -X POST -H "Authorization: Bearer root" -H "Content-Type: application/json" \
  -d '{"sql":"SELECT * FROM players ORDER BY score"}' \
  http://127.0.0.1:4306/api/v1/agent/sql
```

同一能力原生集成到轻量 CLI，无外部 AI/云服务依赖：

```bash
mydb-cli --admin-password root agent health
mydb-cli --admin-password root agent slow
mydb-cli --admin-password root agent ask "最近有哪些慢 SQL？"
mydb-cli --admin-password root agent diagnose "why is write latency high?"
mydb-cli --admin-password root agent optimize "SELECT * FROM players ORDER BY score"
```

Agent `ask`/`diagnose` 不依赖外部模型，默认开启；它会把中英文自然语言问题安全归类为健康、慢 SQL、写延迟、锁、连接、存储、备份或 SQL 优化，只读取实时指标和最近慢查询，不执行问题中的任意 SQL。主要管理 API：配置读取/热重载、内存统计/flush、连接统计、全量/原生 LSN 增量备份、恢复链、删除。`mydbdump --incremental-from` 仍提供跨 MySQL/MyDB 的表内容校验增量。

HTTP 原生备份链：

```bash
# 一致性全量快照，响应包含 id / to_lsn
curl -X POST -H "Authorization: Bearer root" \
  http://127.0.0.1:4306/api/v1/backup/full

# 仅归档 (base.to_lsn, current_lsn]；base_id 省略时选择最新 LSN 备份
curl -X POST -H "Authorization: Bearer root" -H "Content-Type: application/json" \
  -d '{"base_id":"full-..."}' \
  http://127.0.0.1:4306/api/v1/backup/incremental

# 校验 full→incremental 父链并暂存恢复；point_in_time 可省略（恢复到链末端）
curl -X POST -H "Authorization: Bearer root" -H "Content-Type: application/json" \
  -d '{"id":"incremental-...","point_in_time":"2026-07-15T14:22:53.842Z"}' \
  http://127.0.0.1:4306/api/v1/backup/restore
docker restart mydb
```

全量复制由 actor 写屏障固定在写组边界。增量包逐记录 CRC 且要求源 WAL LSN 完整连续；源库的 `Applied` checkpoint 标记在增量包中等位转换为无操作 `Checkpoint`，保证从父全量页恢复时所有增量提交都会 redo，同时不重用 LSN。新写 actor 组使用带提交毫秒时间的 `MDG2`，旧 `MDG1` 继续原地读取；RFC3339 PITR 会把最后一个增量段严格截到目标提交 LSN。恢复先写 pending marker，重启后在存储/WAL writer 打开前验证并安装完整父链。已有子增量或 pending restore 引用的备份不能删除。时间点必须不早于父全量；旧 MDG1 历史应先生成新全量，再对其后的 MDG2 增量执行时间点恢复。

存储感知和安全清理：

```bash
curl -H "Authorization: Bearer root" \
  http://127.0.0.1:4306/api/v1/storage/inventory

curl -X POST -H "Authorization: Bearer root" -H "Content-Type: application/json" \
  -d '{"confirmation":"DELETE_ORPHANED_STORAGE"}' \
  http://127.0.0.1:4306/api/v1/storage/cleanup
```

清理只在 actor 顺序写队列内执行，删除“元数据未引用且目录中只有 MyDB page 文件”的孤儿表目录；执行前再次核对活动表。未知目录、备份、WAL、活动数据库和活动表不会被删除。

## 当前 SQL 范围

已实现：数据库和表 DDL、`CREATE TABLE [IF NOT EXISTS] ... LIKE ...`、`ALTER TABLE` 列/索引变更、`CREATE/DROP INDEX`、schema-qualified DML、`INSERT`/`REPLACE`、`INSERT IGNORE`、MySQL 单行 `INSERT/REPLACE ... SET col=expr`（含 `DEFAULT`、IGNORE、ON DUPLICATE、事务和自增留洞）、VALUES 中 `DEFAULT`/`DEFAULT(col)`/常用标量表达式，以及 `INSERT ... () VALUES ()`/`INSERT ... VALUES ()` 默认行和缺默认值错误 1364；普通表、无主键表、JOIN UPDATE 与 ON DUPLICATE UPSERT 均支持 `col=DEFAULT`/`DEFAULT(col)`、嵌套标量表达式、事务回滚和 1364；`INSERT/REPLACE ... SELECT`、`ON DUPLICATE KEY UPDATE col=VALUES(col)` 与 `counter=counter+VALUES(counter)`、`AUTO_INCREMENT`/`LastInsertId`、`SELECT`、任意表数链式 `INNER`/`LEFT`/`RIGHT JOIN`，列对列 ON 支持 `= / != / <> / < / <= / > / >= / <=>` 与括号/`AND`/`OR`（可引用此前任意表，等值路径走哈希）、多列 `USING`、链式 `CROSS JOIN`、`NATURAL INNER/LEFT/RIGHT JOIN` 的公共列匹配/COALESCE/`*` 列序、JOIN 上 `COUNT/SUM/AVG/MAX/MIN` 与多列 `GROUP BY`/常用 `HAVING`、非相关和整谓词相关 `IN/NOT IN`、标量子查询比较与 `EXISTS/NOT EXISTS`（相关值按外层行二进制安全绑定，标量多行返回 MySQL 1242）、`DISTINCT`/`COUNT(DISTINCT col)`、`GROUP_CONCAT([DISTINCT] expr [ORDER BY ...] [SEPARATOR str])`、限定列/别名/`*` 投影；JSON 支持 `JSON_EXTRACT`/`JSON_UNQUOTE`、`JSON_OBJECT`/`JSON_ARRAY`、`JSON_VALID`/`JSON_TYPE`/`JSON_LENGTH`/`JSON_CONTAINS`、`JSON_SET`/`JSON_REMOVE`，可直接用于游戏 profile 的 INSERT、SELECT、WHERE、UPDATE、UPSERT 和事务回滚；另支持 `CASE`/`IF`/`NULLIF`、`CONCAT`/`CONCAT_WS`、`TRIM`/`REPLACE`/`LOCATE`/`INSTR`/`LPAD`/`RPAD`/`REVERSE`/`REPEAT`、`ASCII`/`ORD`/`BIT_LENGTH`/`OCTET_LENGTH`/`CHARACTER_LENGTH`、`SPACE`/`STRCMP`/`SUBSTRING_INDEX`/字符串 `INSERT`/`QUOTE`、迁移与二进制常用的 `BIN`/`OCT`/`HEX`/`UNHEX`/`TO_BASE64`/`FROM_BASE64`/`FORMAT`、内容寻址和迁移校验常用的 `MD5`/`SHA`/`SHA1`/`SHA2`/`CRC32`、游戏标识与网络地址常用的 `UUID`/`UUID_TO_BIN`/`BIN_TO_UUID`/`IS_UUID`、`INET_ATON`/`INET_NTOA`/`INET6_ATON`/`INET6_NTOA`/`IS_IPV4`/`IS_IPV6`/`IS_IPV4_COMPAT`/`IS_IPV4_MAPPED`、权限位掩码和迁移进制常用的 `CONV`/`BIT_COUNT`、游戏坐标和角度计算常用的 `PI`/`DEGREES`/`RADIANS`/`SIN`/`COS`/`TAN`/`COT`/`ASIN`/`ACOS`/`ATAN`/`ATAN2`、活动周期和月末结算常用的 `DAYOFYEAR`/`WEEKDAY`/`QUARTER`/`DAYNAME`/`MONTHNAME`/`LAST_DAY`/`MAKEDATE`、游戏标签与权限常用的 `FIND_IN_SET`/`FIELD`/`ELT`/`MAKE_SET`/`EXPORT_SET`、`POW`/`SQRT`/`MOD`/`SIGN`/`EXP`/`LOG`/`TRUNCATE` 等常用文本与数学函数、常用 `CAST`/`CONVERT`，以及 `NOW`/`CURRENT_TIMESTAMP`/`LOCALTIME`/`LOCALTIMESTAMP`/`CURDATE`/`CURTIME`/`CURRENT_TIME`、`UTC_DATE`/`UTC_TIME`/`UTC_TIMESTAMP`、`UNIX_TIMESTAMP`/`FROM_UNIXTIME`、`DATE_ADD`/`DATE_SUB`/`DATEDIFF`、`TIME`/`MICROSECOND`/`TIME_TO_SEC`/`SEC_TO_TIME`/`TIMEDIFF`、`DATE_FORMAT`/`TIMESTAMPDIFF` 和日期组成提取。TIME 系列支持负时长、超过 24 小时的持续时间和 0–6 位微秒；TIMEDIFF 要求两端同为 TIME 或同为 DATETIME，并按 MySQL TIME 范围收敛结果。`HEX` 区分数值与字节输入，`UNHEX` 兼容奇数位左补零；Base64 输出每 76 字符换行，解码忽略空格、制表、回车和换行；`FORMAT` 支持默认/en_US、de_DE、fr_FR 分组格式。`SHA2` 支持 224/256/384/512 位及 0=256，非法位数返回 NULL；`UUID()` 生成 RFC 4122 version 1，二进制 UUID 支持 MySQL time-part swap；IPv4 使用 4 字节、IPv6 使用 16 字节网络序表示。`CONV` 使用 64 位精度和 2–36 进制，负目标进制输出有符号表示；`BIT_COUNT` 支持数值、十六进制字面量及显式二进制字符串。三角函数使用双精度，ASIN/ACOS 越界返回 NULL，COT(0) 返回范围错误。日历函数覆盖闰年、跨年日序与无效日期 NULL，DAYNAME/MONTHNAME 当前遵循默认英文时间名称。DDL 关键字采用词边界识别，`checksum`、`checkpoint` 等普通列不会被误判为 `CHECK` 约束。字符串放大函数采用 MySQL 默认 64 MiB packet 结果边界，超限返回 NULL，避免不受控分配。`TIMESTAMP`/`DATETIME` 的 `DEFAULT CURRENT_TIMESTAMP[(fsp)]` 和 `DEFAULT NOW[(fsp)]` 在省略列及显式 `DEFAULT(col)` 时动态物化，不再保存函数文本；这些日期表达式可用于 SELECT、INSERT、UPDATE、UPSERT 和事务。聚合参数可使用 CASE、日期、文本和数学表达式，支持 `COUNT(DISTINCT CASE ...)`、`SUM(CASE ...)`，聚合结果可继续嵌套到 `ROUND`/`CONCAT` 等标量函数；`GROUP BY` 支持表达式、投影别名和序号。已用纯 MySQL SQL 覆盖按注册日期分 cohort、次日留存率、DAU 和每日收入统计。标量表达式可在单表和 JOIN 的 WHERE 中组合 `IN/BETWEEN/LIKE/AND/OR/NULL` 三值谓词、`ORDER BY`、`LIMIT ... OFFSET`、`COUNT/SUM/AVG/MAX/MIN`、多列 `GROUP BY ... COUNT(*)`、SELECT `+/-/*//%` 算术、常量/列赋值及单步算术 `UPDATE`、单表 `UPDATE/DELETE ... ORDER BY ... LIMIT`、单目标及多目标 `UPDATE ... JOIN`、`DELETE alias-list FROM ... JOIN`、`DELETE FROM alias-list USING ...`、`DELETE`、`TRUNCATE`、命名/复合外键（`RESTRICT`/`CASCADE`/`SET NULL`）与常用比较/NULL/AND/OR `CHECK`、事务内读己写、`SAVEPOINT`/`ROLLBACK TO`/`RELEASE`、DDL 隐式提交、常用 `SHOW`/`DESCRIBE`、db233-go 所需 `information_schema`、binary prepared statement。CREATE TABLE LIKE 可跨 schema 复制列、默认值、自增属性、主键/唯一/普通索引、CHECK 与引擎，不复制源数据、外键或当前自增计数，并返回 MySQL Note 1050。TRUNCATE 使用 affected rows 0 的持久化空表镜像，执行前隐式提交；即使表原本为空也将 AUTO_INCREMENT 重置为 1，被外部子表引用时返回 MySQL 1701，允许清空子表或在 `FOREIGN_KEY_CHECKS=0` 时清空父表。JOIN 写先 `FOR UPDATE` 锁定参与表并将结果物化为各目标主键级 Actor 命令；INSERT SELECT 对源表加共享锁，均不绕过事务、WAL 与约束。事务 DML 先在单写 Actor 顺序预校验并物化：约束错误在当前语句返回，外键级联立即进入本事务读视图，回滚不落 WAL，自增 ID 保持 MySQL 的回滚与冲突尝试留洞语义，表 COW 重写不会让已预留计数回退。算术 UPDATE/UPSERT 在读取前取得排他锁，按 MySQL 左到右语义计算，再固化为幂等常量 WAL 命令，避免崩溃重放导致计数重复；有限变更切开无键完全重复行、或低频外键级联时使用幂等整表镜像 WAL，精确保持行数并确保 redo 不重复。原版 `mysqldump --hex-blob` 使用的 `0x...`（含奇数位补零）按 MySQL 字节字面量导入，不会保存成文本。`InnoDB` 是持久 Neko233 引擎别名；`ENGINE=MEMORY` 使用独立纯内存行存储，保持 MySQL 的非事务语义（`ROLLBACK` 不撤销写入）和重启后仅保留表结构、清空数据行为。`SHOW CREATE TABLE` 保存 MySQL 原始类型和表选项，避免迁移时丢失 `UNSIGNED`、`DECIMAL`、`JSON`、`ENUM`、`AUTO_INCREMENT`、字符集或排序规则。

时间计算另支持 `ADDTIME`、`SUBTIME` 和 `MAKETIME`：TIME 与 DATETIME 输入保持原形态，支持负时长、跨日、超过 24 小时的持续时间和 0–6 位微秒；MAKETIME 支持小数秒，超出 TIME 范围按 MySQL 上限收敛。`WEEK`、`WEEKOFYEAR`、`YEARWEEK` 支持 MySQL 0–7 周模式、周日/周一周首、0/1 起始周和 ISO 8601 跨年周归属。`TO_DAYS`、`FROM_DAYS`、`TO_SECONDS` 使用 MySQL 从 year 0 开始的日序，不能直接套用从 year 1 开始的宿主日期库序号；`PERIOD_ADD`、`PERIOD_DIFF` 支持 YYMM/YYYYMM 月周期和两位年份展开。`EXTRACT(unit FROM value)` 支持 YEAR/MONTH/WEEK/DAY/HOUR/MINUTE/SECOND/MICROSECOND/QUARTER 及 YEAR_MONTH、DAY_MINUTE、DAY_SECOND、DAY_MICROSECOND 等 MySQL 复合单位；SELECT 仅把顶层 FROM 识别为表来源，不会误判 EXTRACT 内部关键字。`GET_FORMAT` 提供 DATE/TIME/DATETIME/TIMESTAMP 的 USA/EUR/JIS/ISO/INTERNAL 模板；`STR_TO_DATE` 与 `DATE_FORMAT` 可按模板往返日期、时间、日期时间及微秒，`TIME_FORMAT` 保留超过 24 小时的 TIME 小时数。格式转换显式处理 MySQL `%M` 月份全名和 `%f` 六位微秒，避免套用宿主格式符产生静默错值。以上函数可直接用于游戏冷却、活动到期、注册周 cohort、按日归档、赛季周期、事件分区和文本日志导入的 SELECT、WHERE、UPDATE 与事务。

当前时间兼容 `LOCALTIME`/`LOCALTIMESTAMP`、`UTC_DATE`/`UTC_TIME`/`UTC_TIMESTAMP` 和 `SYSDATE`，并为 `CURTIME`、`CURRENT_TIME`、local/UTC 时间戳别名提供 0–6 位小数秒精度。`NOW`/`CURRENT_TIMESTAMP`、local/UTC 别名和无参 `UNIX_TIMESTAMP` 读取同一个语句开始快照，即使语句执行期间等待也保持恒定；`SYSDATE` 返回函数实际执行时刻。它们可直接写入 DATE、TIME、DATETIME、TIMESTAMP 列，并遵守事务回滚；批量 DML 的同一 NOW 表达式不会因逐行求值产生时间漂移。

`CONVERT_TZ(datetime,from_tz,to_tz)` 支持 MySQL 固定偏移范围 `-13:59` 至 `+14:00`，以及 `UTC`、`GMT`、`SYSTEM` 和大小写不敏感的 IANA 命名时区；转换经 UTC 中间值完成，保留 0–6 位微秒并正确处理跨日，可用于 WHERE、UPDATE 和事务。内置 POSIX tzdb 使 Windows、Linux、macOS 不必预装 MySQL 时区表即可使用 `Asia/Shanghai`、`America/New_York` 等名称。DST 春季不存在的墙钟时刻返回 NULL；秋季两个不同 UTC 时刻可映射为同一墙钟值，符合 MySQL 文档描述的多对一转换。无效偏移、未知名称和 NULL 返回 NULL。

会话 `time_zone` 已实际参与时间求值，不再只是可查询变量。每个连接可独立设置 SYSTEM、固定偏移或 IANA 名称；NOW/CURRENT_TIMESTAMP/local 别名、SYSDATE、`UNIX_TIMESTAMP(datetime)`、`FROM_UNIXTIME`、动态时间默认值及 ON UPDATE 均使用当前会话时区，UTC 系列与无参 UNIX_TIMESTAMP 保持绝对时间不变。单条 SET 的多赋值按左到右更新时区，`SET time_zone=DEFAULT` 恢复 SYSTEM；无效名称拒绝并保留原值。带微秒 DATETIME 转 UNIX 时间戳不再丢失小数部分。当前不暴露或允许修改 MySQL `mysql.time_zone*` 系统表，也不提供 leap-second 时区数据变体。

日期算术另支持 `ADDDATE`/`SUBDATE` 的 INTERVAL 同义写法和整数天数简写；`TIMESTAMP(expr[,time])` 统一生成 DATETIME，`TIMESTAMPADD` 支持 MICROSECOND 到 YEAR、`SQL_TSI_` ODBC 单位、负间隔和不存在月日向月末收敛，可直接承接 ORM 排期 SQL，并用于 SELECT、WHERE、UPDATE 与事务。

MySQL 8.0.19+ 现代 UPSERT 支持 `INSERT ... VALUES/SET ... AS new [(alias,...)] ON DUPLICATE KEY UPDATE`，可用 `new.col`、`new.alias`、无限定列别名和表限定旧行值读取 incoming/existing 行。冲突赋值支持 `IF`/`CASE`、`CONCAT`、`COALESCE`、`GREATEST`/`LEAST`、常用字符串/数值/CAST 函数及组合算术，并按 MySQL 左到右规则让后续赋值读取前一列的新值；事务、自增留洞、changed-row affected rows、约束与 WAL 路径和旧 `VALUES(col)` 写法一致。

只读视图支持 `CREATE VIEW`、显式视图列名、`CREATE OR REPLACE VIEW`、`DROP VIEW [IF EXISTS]`、`SHOW CREATE VIEW` 和 `SHOW FULL TABLES` 类型识别。视图定义持久化保存，查询时展开为正常 SELECT 执行，可继续参与外层 WHERE/ORDER BY、JOIN、聚合及另一个视图；DDL 保持隐式提交，重启后定义仍可用，并拒绝 INSERT/UPDATE/DELETE/TRUNCATE 等不可更新视图写入。`CREATE TABLE [IF NOT EXISTS] name AS SELECT ...` 可从普通表、JOIN、聚合、派生表或视图创建快照表，按结果推导常用列类型；建表和首批数据编码为同一个 Actor/WAL 原子组，DDL 隐式提交后即使崩溃也不会留下只有表结构或只有部分快照行的半状态。`RENAME TABLE old TO new [, ...]` 与 `ALTER TABLE old RENAME [TO|AS] new` 使用同一原子 schema/数据镜像批次，保留普通表、无主键重复行、AUTO_INCREMENT 后续序号和只读视图定义，并可跨重启恢复；这不是复制 InnoDB 文件改名流程，而是保持 MySQL 对外可见的原子重命名效果。单条 `ALTER TABLE` 可组合 `ADD/DROP/MODIFY COLUMN`、`ADD/DROP INDEX` 和 `ADD UNIQUE KEY`，并接受 `ALGORITHM`/`LOCK` 兼容提示；全部操作先顺序预校验，再作为同一个 WAL 原子组提交，任一操作失败时不会留下前序半变更，同时保持 DDL 隐式提交和重启恢复。列定义还支持 `ADD/MODIFY ... FIRST|AFTER`、`CHANGE COLUMN` 和 `RENAME COLUMN`；改名同步物理行字段、主键、自增列和普通/唯一索引引用，列顺序与旧行值在 COW 重写及重启后保持一致。键演进支持 `ADD/DROP PRIMARY KEY` 与 `RENAME INDEX/KEY`；新增主键或唯一索引会扫描已有行，在写入 WAL 前拒绝重复值及主键 NULL，因此失败的多操作 DDL 不会留下持久化半状态。`DROP COLUMN` 同步使用 COW 行重写移除旧物理字段，后续 UPDATE/UPSERT 与重启不再读取到废弃列名。

约束演进支持 `ADD/DROP FOREIGN KEY`、`ADD/DROP CHECK` 与 `DROP CONSTRAINT`。ALTER 的 schema renderer 会从原始建表定义提取并重新附加 FK/CHECK，普通加列、加索引或改键不会再静默丢失已有约束。新增约束先验证父表、引用索引、列定义及全部旧行；孤儿外键、违反 CHECK 的旧值和重复约束名在 WAL 前失败。被 FK/CHECK 使用的列不能直接删除或改名，避免产生失效元数据；可在同一原子 ALTER 中先删除约束，再执行列变更。约束添加、删除、DML 执行与重启恢复使用同一持久化定义。

默认值演进支持 `ALTER COLUMN ... SET DEFAULT` 与 `DROP DEFAULT`，修改后立即作用于省略列 INSERT、显式 `DEFAULT(col)` UPDATE/UPSERT 和事务路径。`CURRENT_TIMESTAMP[(fsp)]`/`NOW[(fsp)]` 继续按动态时间物化，ALTER 重渲染后的 `SHOW CREATE TABLE` 保持函数默认值无字符串引号。`ADD COLUMN/INDEX IF NOT EXISTS` 与 `DROP COLUMN/INDEX IF EXISTS` 可放入同一多操作 ALTER，已存在或已缺失对象按条件跳过，其他真实错误仍使整批在 WAL 前失败。新增列会通过 COW 按新 schema 物化旧行，因此带默认值的列对历史数据立即返回默认值，重启后结果不变。

会话变量支持 `SET @name=expr`、`SET @name:=expr`、单条 SET 左到右多赋值，以及在 SELECT、函数、INSERT、UPDATE、WHERE 和绑定参数执行后的 SQL 中读取。变量值按连接隔离，支持 NULL 和任意字节；用户变量不进入事务写集，因此与 MySQL 一样不被 ROLLBACK 撤销，新连接不会继承旧连接变量。SQL 扫描器跳过字符串和反引号，仅将真实 `@name`/`@@session.name` token 替换为二进制安全字面量，并恢复原始投影列名。mysqldump 常见的 `SET @OLD_SQL_MODE=@@SQL_MODE`、`@OLD_TIME_ZONE`、`@OLD_UNIQUE_CHECKS`、`@OLD_FOREIGN_KEY_CHECKS` 保存/恢复流程使用真实会话值；`SET NAMES` 和常用字符集会话变量也可查询。

除 MySQL binary prepared protocol 外，还支持 SQL 级命名语句：`PREPARE name FROM 'sql'` 或 `FROM @sql`、`EXECUTE name [USING @arg,...]`、`DEALLOCATE PREPARE` 与 `DROP PREPARE`。PREPARE 时复制 SQL 模板，后续修改来源变量不会改变已准备语句；USING 参数在每次执行时读取，严格校验 `?` 数量，并使用与协议 prepared 相同的 NULL/BLOB 安全字面量绑定。命名语句按连接隔离，EXECUTE 结果继续进入普通 SELECT/DML、锁、约束、事务和 WAL 路径；ROLLBACK 只撤销被执行 DML，不释放已准备模板。

写后会话状态支持 `LAST_INSERT_ID()`、`LAST_INSERT_ID(expr)`、`ROW_COUNT()` 与 `FOUND_ROWS()`。自动增长 INSERT 在显式事务中预留 ID 后立即更新连接状态，ROLLBACK 不回退该值；changed-row UPDATE/UPSERT、no-op 和 SELECT 后的 ROW_COUNT 保持 MySQL 常用语义，状态不跨连接传播。经典 `ON DUPLICATE KEY UPDATE id=LAST_INSERT_ID(id)` 会在冲突时返回既有主键，即使赋值最终为 no-op 也更新会话 ID，同时不制造数据 WAL。普通 SELECT 后 FOUND_ROWS 返回实际结果行数；`SELECT SQL_CALC_FOUND_ROWS ... LIMIT/OFFSET` 会使用同一执行器移除顶层 LIMIT 计算完整 WHERE、DISTINCT、GROUP 和集合结果，下一条 `FOUND_ROWS()` 返回全量。当前正确性实现执行一次无 LIMIT 计数查询和一次实际查询，后续执行引擎性能阶段可融合为单遍计数。

`CREATE TEMPORARY TABLE`/`DROP TEMPORARY TABLE` 使用连接 ID 与单调序号生成保留前缀隐藏物理表，同一逻辑名可遮蔽永久表，不同连接可各自创建同名临时表。支持显式列定义、`LIKE` 与 `AS SELECT`：LIKE 复制列、默认值、键、索引和 CHECK 元数据但不复制数据；CTAS 使用同一原子存储批次创建表并写入首批查询结果。INSERT、UPDATE、DELETE、SELECT、JOIN、LOAD DATA、ALTER、索引和 TRUNCATE 复用普通执行器；默认 InnoDB 别名保持事务回滚，MEMORY 仍使用非事务语义。遵循 MySQL 8 限制：`RENAME TABLE` 不操作临时表，使用 `ALTER TABLE ... RENAME`；同库改名只更新连接映射，跨库改名原子搬移已提交镜像并重定向当前事务尚未提交的写和一致性快照。临时 DDL 不隐式提交，也不释放当前事务已持有的行锁；事务内 DROP 会移除该临时表尚未提交的写，但保留同事务其他表写。SHOW 与 information_schema 不暴露隐藏名，SHOW CREATE、SHOW INDEX/KEYS、SHOW COLUMNS 和 DESCRIBE 返回连接逻辑名；SHOW TABLE STATUS 支持数据库、LIKE 与常用 Name 等值过滤，并报告实际行数与近似数据长度。正常断线异步 DROP；异常退出时，存储在 WAL redo 完成且尚无客户端连接的启动阶段，仅清理 `__mydb_tmp_` 保留命名空间，避免删除任何在用普通数据。更复杂 information_schema 投影与元数据过滤仍在清单中。

`information_schema` 使用连接内只读虚拟表并复用普通 SELECT/JOIN/GROUP 执行器，而非为每条探测 SQL 写硬编码分支。已提供 `SCHEMATA`、`TABLES`、`COLUMNS`、`STATISTICS`、`TABLE_CONSTRAINTS`、`KEY_COLUMN_USAGE`、`CHECK_CONSTRAINTS`、`REFERENTIAL_CONSTRAINTS`、`VIEWS`、`TRIGGERS`、`ROUTINES` 与 `PARAMETERS` 的常用 MySQL 8 列，支持 `WHERE`、`LIKE`、`IN`、`COUNT`、`COALESCE`、`ORDER BY`、`GROUP BY`、表别名和跨元数据表 JOIN。外键元数据包含引用唯一键、UPDATE/DELETE 规则和目标表；视图元数据包含定义、安全类型与当前只读状态；触发器元数据包含事件、时机、目标表、顺序、语句与 definer；存储过程元数据包含定义、definer、参数顺序、IN/OUT/INOUT 模式、声明类型、CREATED、LAST_ALTERED 和创建/修改时 SQL_MODE。尚未实现的事件提供列结构正确的空 `EVENTS` 元数据表，使 ORM 能确定对象不存在，而非因探测语句不受支持而中断。表、列、引擎、行数、自增、主键、复合索引、唯一键、外键引用与 CHECK 表达式可供 mydbdump、mydb-migrate 和常见 ORM schema introspection 直接读取；连接临时表隐藏物理名不会进入元数据结果。

`SHOW TABLES`/`SHOW FULL TABLES`、`SHOW [FULL] COLUMNS/FIELDS`、`SHOW INDEX/INDEXES/KEYS` 与 `SHOW TABLE STATUS` 支持 MySQL 常用 `FROM/IN`、`LIKE` 和复合 `WHERE` 过滤。WHERE 复用标量条件执行器，可组合 `AND`、`OR`、`IN`、`LIKE`、比较和 NULL 判断；动态 `Tables_in_<db>`、`Table_type`、`Field`、`Key_name`、`Engine` 等结果列可直接参与过滤。

Trigger 支持 `CREATE TRIGGER`、`DROP TRIGGER [IF EXISTS]`、`SHOW TRIGGERS`、`SHOW CREATE TRIGGER` 与 `information_schema.TRIGGERS`。`BEFORE INSERT/UPDATE` 均可在 BEGIN/END 中按顺序组合 SET NEW 与跨表 INSERT/UPDATE/DELETE；UPDATE 表达式同时读取 OLD 与当前 NEW，INSERT 副作用读取 SET 后的 NEW，但 AUTO_INCREMENT 在 BEFORE 阶段保持 MySQL 值 0。流程控制支持嵌套 `IF ... THEN / ELSEIF / ELSE / END IF`、简单 `CASE expr WHEN ...`、搜索 `CASE WHEN condition ... END CASE`、`WHILE ... DO`、`REPEAT ... UNTIL` 和带标签 `LOOP`；循环支持嵌套、结束标签校验、`LEAVE` 与 `ITERATE`，并有每行一百万次迭代安全上限，避免错误 Trigger 永久占用执行 Actor。BEFORE 条件在每一步使用最新 NEW/OLD，条件分支可执行 SET、DML 或 SIGNAL；CASE 选择表达式只求值一次，嵌套 CASE 与 SET 后动态分支保持 MySQL 行级语义；CREATE 校验和递归预锁遍历全部分支及循环体，运行时只展开实际执行路径。BEGIN 顶部支持 `DECLARE name [, ...] type [DEFAULT expr]` 与 `SET local=expr`，变量按每次行级触发独立初始化，可顺序读取前次赋值、OLD/NEW 和其他已声明变量，并可用于 IF/CASE/循环、SET NEW、SIGNAL 及跨表 INSERT/UPDATE/DELETE；变量与目标列同名时保护 DML 表名、列清单和赋值左值，表达式仍按局部变量绑定。局部变量与 Procedure 参数共用类型转换器，已覆盖整数/无符号范围、DECIMAL scale、浮点、文本/二进制定长、DATE、DATETIME/TIMESTAMP/TIME(0..6)、ENUM 和 SET；时间小数默认按声明 FSP 舍入并支持跨秒/跨日进位，`TIME_TRUNCATE_FRACTIONAL` 模式改为截断，CALL 参数在切换例程 SQL_MODE 前按调用者模式转换。ENUM 按声明成员或数字索引归一化，SET 支持逗号成员、去重、声明顺序输出和数字位掩码。嵌套 `BEGIN ... END` 使用独立作用域栈，允许内层同名变量遮蔽，退出块后恢复外层值；块可带标签并由 LEAVE 提前退出，ITERATE 只允许指向循环标签。每个复合块内的声明必须位于可执行语句前，重复声明、越域变量、无效标签和结束标签不匹配在 CREATE TRIGGER 阶段拒绝。普通/多行 INSERT、IGNORE、REPLACE、ON DUPLICATE、INSERT SELECT 与 LOAD DATA 使用分段 provenance 展开：BEFORE 副作用、源行事件、AFTER 副作用各执行一次，不会因冲突分支重复或漏触发。`AFTER INSERT/UPDATE` 和 `BEFORE/AFTER DELETE` 也可执行跨表 DML。跨表 UPDATE/DELETE 支持常用标量表达式、WHERE、ORDER BY 与 LIMIT，先递归扫描整张 Trigger 图并按全局顺序取得目标表排他锁，再读取事务可见镜像并物化为主键精确、崩溃重放幂等的常量命令；并发触发同一游戏计数器不会丢写。A→B→C mutation 行事件会递归执行，嵌套副作用不污染客户端 affected rows；活动表栈与 32 层限制令 A→B→A 环在落 WAL 前原子失败。目标表已有对应 UPDATE/DELETE Trigger 时继续执行其行事件和审计 INSERT；自表修改按 MySQL 运行时错误拒绝，无主键 mutation 目标在 CREATE TRIGGER 时拒绝。任一支持时机可执行 `SIGNAL SQLSTATE '45000' [SET MESSAGE_TEXT=expr]`，MESSAGE_TEXT 可读取 OLD/NEW、局部变量和常用标量函数；拒写不进入 WAL，不留下当前语句的原行或副作用半状态，显式事务此前成功语句仍可继续提交，Wire 返回 MySQL 1644 与 SQLSTATE 45000。AUTO_INCREMENT 在 BEFORE 完成后预留并物化，因此 AFTER INSERT、跨表审计和 SIGNAL 消息可读取最终 ID；失败与回滚保留自增空洞，多行和 INSERT SELECT 返回首个 ID，Trigger 内目标表生成的 ID 不污染客户端 LAST_INSERT_ID。Trigger/FK 副作用保持原子但不增加客户端 affected rows。冲突写不按 SQL 外形猜事件：ON DUPLICATE KEY UPDATE 的新行执行 AFTER INSERT，冲突行执行 BEFORE/AFTER UPDATE；INSERT IGNORE 对每次尝试执行 BEFORE INSERT，仅成功行执行 AFTER INSERT；REPLACE 按 BEFORE INSERT、每个冲突旧行的 BEFORE/AFTER DELETE、AFTER INSERT 顺序执行，一个新行可同时淘汰多个唯一键冲突。REPLACE 使用幂等最终表镜像进入同一 WAL 原子批次，避免崩溃重放重复删除或插入。原行和全部副作用进入同一事务/WAL 批次；回滚不会遗留审计半状态。触发器定义随表 schema 进入原子 DDL WAL，重启恢复、DDL 隐式提交、表改名携带和 DROP TABLE 自动删除均已覆盖；旧无 trigger 字段的二进制 WAL 使用旧布局回退解码。尚未支持完整字符集/排序规则/时区转换、CALL，以及自定义非 45000 condition。

局部变量在 DECLARE 默认值和后续 SET 时按声明类型转换。已覆盖有符号/无符号整数及精确 BIGINT 边界、DECIMAL scale、FLOAT/DOUBLE、CHAR/VARCHAR 字符长度、BINARY/VARBINARY 字节长度与补零、常用 BLOB/TEXT，以及 DATE、DATETIME/TIMESTAMP、TIME 的基础格式校验；NULL 保持 NULL。整数越界和无效日期时间在原行与副作用落 WAL 前失败。遵循 MySQL 8，`SET local=DEFAULT` 在 CREATE TRIGGER 阶段作为语法错误拒绝。ENUM/SET、完整字符集/排序规则、全部 FSP/时区和 sql_mode warning 组合仍待补齐。

存储过程支持 `CREATE [DEFINER=...] PROCEDURE`、`DROP PROCEDURE [IF EXISTS]`、`SHOW CREATE PROCEDURE`、`SHOW PROCEDURE STATUS` 与 `CALL`。定义和 IN/OUT/INOUT 参数元数据经写 Actor、WAL 与独立 `routines.json` 原子持久化，不改变旧 `schema.json` 格式；重启后仍可调用。过程体使用逐语句异步解释器，复用 Trigger 已验证的 DECLARE 类型、嵌套 BEGIN 作用域、IF/CASE、LOOP/WHILE/REPEAT、LEAVE/ITERATE、SIGNAL 和 DML；OUT/INOUT 通过调用方用户变量返回。支持 `SELECT expr INTO local FROM ...` 与尾置 `SELECT ... FROM ... INTO local`，查询结果立即按局部变量声明类型写入并可驱动后续条件、循环和 DML；零行保持目标原值并返回 MySQL warning 1329，若当前块声明了匹配 handler 则执行后继续或退出声明块；多行返回 MySQL 1172 并撤销本次 CALL 写。游标支持 `DECLARE cursor CURSOR FOR SELECT`、`OPEN`、`FETCH [FROM] cursor INTO locals` 和 `CLOSE`，严格校验“变量/条件→游标→handler→可执行语句”的声明顺序；OPEN 时绑定局部变量并物化事务可见查询结果，FETCH 只读单向遍历，耗尽触发块级 NOT FOUND handler，离开 BEGIN 块自动关闭，重复 OPEN/未 OPEN 的 FETCH/CLOSE 映射 MySQL 1325/1326。

Condition handling 支持 `DECLARE name CONDITION FOR` MySQL 错误码或 SQLSTATE，以及简单/复合 `DECLARE CONTINUE|EXIT HANDLER FOR`；一个 handler 可列出多个条件，条件可使用错误码、SQLSTATE、命名 condition、`NOT FOUND`、`SQLWARNING` 或 `SQLEXCEPTION`。同块多个匹配项按 MySQL 的“错误码优先于 SQLSTATE，SQLSTATE 优先于类别”选择；内层块优先于外层块。复合 handler body 使用独立局部作用域，CONTINUE 从触发条件的下一语句继续，EXIT 离开 handler 所在 BEGIN 块；活动 handler 不会递归捕获自身。`RESIGNAL` 支持保持原条件、改用 SQLSTATE/基于 SQLSTATE 的命名 condition，并可通过 SET 覆盖 MESSAGE_TEXT、MYSQL_ERRNO 及 MySQL condition information items；覆盖后继续向外层 handler 或客户端传播。Wire 错误包使用动态 `u16 + SQLSTATE` 编码，不把自定义 MYSQL_ERRNO 降级成固定枚举错误。DML、SIGNAL、SELECT INTO 和游标状态/查询错误均进入统一 condition 调度。

Procedure 内支持 `GET [CURRENT|STACKED] DIAGNOSTICS`。Statement area 可读取 `NUMBER` 与 `ROW_COUNT`；condition area 可读取 `RETURNED_SQLSTATE`、`MESSAGE_TEXT`、`MYSQL_ERRNO`、origin、constraint/schema/table/column/cursor 等 MySQL 项目，目标可为局部变量、IN/OUT/INOUT 参数或用户变量。CURRENT 会随 handler 内 SET/DML 等普通语句刷新；STACKED 在 handler 存续期间固定保存激活条件，因此可在错误映射逻辑执行后可靠读取原错误。无活动 handler 时访问 STACKED 会失败，越界 condition number 追加 MySQL 1758/35000 condition 并保持目标原值。

连接顶层也支持 MySQL 扩展 `GET [CURRENT] DIAGNOSTICS`。每个连接持有独立 diagnostics area；普通非诊断 SQL 开始时清理旧 condition，并在结束时写入 affected row count、warning/Note 或主错误，`GET DIAGNOSTICS`、`SHOW WARNINGS`、`SHOW ERRORS` 本身不清空。可先用 `GET DIAGNOSTICS @n=NUMBER,@rows=ROW_COUNT` 获取 statement information，再用字面量或用户变量 condition number 读取 SQLSTATE、errno、message 和其余 condition items。多 condition 按实际产生顺序保存；若语句先产生 warning 后失败，主错误追加在已保存 warning 后，因此应用应按 MySQL 建议先读取 NUMBER，再检查所需 condition。`max_error_count` 默认为 1024，可在 0..65535 内按会话设置或恢复 DEFAULT；它只限制 SHOW/GET 保存的 condition 数量，`warning_count`、`error_count` 和 OK packet warning count 保持真实总数并可高于保存上限。越界 GET 在容量允许时追加 Invalid condition number；容量已满时 condition 列表不增长，但总数仍增长。`sql_notes=OFF` 时 Note 不进入计数和 area，`warning_count/error_count` 为只读变量。真实 mysql Rust 驱动已验证在收到 1062 Wire 错误后无需重连即可读取 diagnostics；顶层 `GET STACKED` 按 MySQL 拒绝，GET DIAGNOSTICS 作为 prepared statement 在 prepare 阶段返回 1295。

`CREATE PROCEDURE [IF NOT EXISTS]` 支持 `COMMENT`、`LANGUAGE SQL`、`[NOT] DETERMINISTIC`、`CONTAINS SQL/NO SQL/READS SQL DATA/MODIFIES SQL DATA` 与 `SQL SECURITY DEFINER/INVOKER`。`ALTER PROCEDURE` 可原子更新 MySQL 允许修改的 COMMENT、LANGUAGE SQL、SQL DATA ACCESS 和 SQL SECURITY，不允许借 ALTER 修改参数、body 或 DETERMINISTIC；执行前按 MySQL DDL 规则隐式提交。CREATE/ALTER 使用新增尾部 V2 WAL 变体携带 CREATED、LAST_ALTERED 和 SQL_MODE，不改变旧 ProcedureDefinition/WAL 二进制布局；`routines.json` 使用带版本的单文件目录快照，旧无版本格式启动时自动补齐元数据。CALL 在参数求值后临时切换到例程创建或最后修改时的 SQL_MODE，正常返回、condition 失败和嵌套调用结束后均恢复调用者会话；SHOW PROCEDURE STATUS 与 information_schema.ROUTINES 返回持久化时间和模式。ALTER 后 SHOW CREATE 会重建可直接执行的 definer、参数、characteristics 与原 body。过程内不带 INTO 的 SELECT 和嵌套 CALL 会按实际执行顺序形成多个不同列形的结果集；Wire 为每个中间结果设置 `SERVER_MORE_RESULTS_EXISTS`，并按 MySQL CALL 规则追加最终空状态结果，`mydb-cli` 会逐个读取和显示。Prepared CALL 通过握手协商 `CLIENT_PS_MULTI_RESULTS`，允许 `CALL p(?,?,?)` 直接绑定 IN/OUT/INOUT：OUT 的输入值按 MySQL 忽略，INOUT 使用绑定初值；过程普通结果之后追加按参数声明顺序排列的单行 OUT/INOUT 结果，其元数据标记 `SERVER_PS_OUT_PARAMS`，随后发送最终 CALL 状态。OUT 结果在 Binary Wire 边界按声明类型编码；已覆盖有符号/无符号整数、DECIMAL、FLOAT/DOUBLE、DATE、DATETIME/TIMESTAMP、正负 TIME、文本、BLOB、JSON、ENUM 和 SET 的元数据映射，真实 mysql Rust 驱动可直接得到 `Int/UInt/Double/Date/Time/Bytes` 等对应值并继续执行下一条查询。结果在 CALL 成功前暂存，后续未处理 condition 不会向客户端泄露半套结果或传播失败前的 OUT 值。自动提交连接把一次 CALL 的全部 DML 作为一个内部事务提交，失败时撤销本次过程全部写和副作用；显式事务内则并入调用方事务并可整体回滚。递归调用限制为 32 层。尚未支持完整多 condition 排序、所有语句对 CURRENT area 的细微清理规则、routine 权限系统，以及 OUT 参数的所有可赋值表达式形式。

`TIMESTAMP`/`DATETIME ... ON UPDATE CURRENT_TIMESTAMP[(fsp)]` 与 `ON UPDATE NOW[(fsp)]` 已按行实现。只有真实发生字段变化的行才刷新时间；整条 no-op UPDATE 不刷新，批量 UPDATE 中未变化的行也不刷新，显式赋值自动时间列时以业务值为准。literal/scalar/expression、UPSERT、JOIN 和事务路径均在持有排他锁后将时间固化为 WAL 常量，因此回滚、崩溃重放与重启不会使用新的系统时间破坏幂等。

UPDATE/UPSERT 默认按 MySQL changed rows 返回 affected rows，不把匹配但字节未变化的行计入结果。literal、scalar、expression、JOIN、无主键重复行和显式事务都在取得排他锁后比较事务可见行，保证并发写入下判断不陈旧；no-op 直接返回，不生成 WAL、不触发 fsync 或表 COW 重写。真实变化继续进入 FIFO actor、约束预校验、group commit 和崩溃恢复链。此设计同时降低游戏业务常见幂等写、状态重复上报和重试请求的延迟及磁盘写放大。

Neko233 使用 actor FIFO 写入、group commit、WAL-backed 更新 memtable、跨事务共享页打包、索引列页定位、表级 copy-on-write checkpoint 和原子 schema generation 提交。同一 fsync 组的事务编码为一条带 CRC 的二进制 `GroupCommit`：完整记录本身就是提交标记，避免每事务重复写 `Batch + Commit`；fsync 后在内存中按 actor 顺序可见，默认每 64 个写组才合并 checkpoint 数据页并记录 `Applied`。启动会先校验 WAL CRC 并截断断电残留的半条/坏尾，再 redo 未 Applied 写组；部分应用按主键/唯一键幂等补齐，被后续 DROP 覆盖的旧 DML 不会错误复活。旧二进制 `Batch + Commit` 和 JSON WAL 仍可原地读取。Prometheus 与 Agent HTTP 原生暴露 prepare/validation、WAL sync、apply、checkpoint、锁等待、超时和死锁累计指标，便于定位慢写。默认 `REPEATABLE READ` 提供稳定快照，同时支持 `READ UNCOMMITTED` 跨连接脏读（回滚/断线立即移除）、`READ COMMITTED`、`SERIALIZABLE`、IS/IX/S/X 分层锁、主键/唯一键行锁、`SELECT ... FOR UPDATE`、`SELECT ... FOR SHARE`、`LOCK IN SHARE MODE`、`NOWAIT` 原子锁获取及 MySQL 3572、`innodb_lock_wait_timeout`、断线自动回滚。主键任务队列支持按 WHERE/ORDER BY 扫描、逐行 try-lock、跳过冲突后再应用 OFFSET/LIMIT 的 `SKIP LOCKED`，共享锁与排他锁兼容关系和 MySQL 一致。锁等待使用 wait-for graph 检测环；形成死锁的受害者返回 MySQL 1213/40001，清空未提交写集并释放全部事务锁，存活事务继续执行。范围或非主键写使用保守表锁，避免 phantom/gap 正确性漏洞；无主键或复杂 JOIN 的逐行 SKIP LOCKED、完整 next-key/gap lock、多方环成本化受害者选择尚未实现。

已支持单源/嵌套 `FROM (SELECT ...) alias` 派生表，并可过滤、聚合、参与链式 JOIN 和继续执行相关子查询；`UNION`/`INTERSECT`/`EXCEPT` 支持 DISTINCT/ALL、MySQL 优先级、全局 ORDER BY/LIMIT 和派生表嵌套，列数不一致返回 MySQL 1222；多 CTE 支持显式列名、引用前置 CTE、参与 JOIN，以及多 anchor/递归臂 + UNION DISTINCT/ALL 的数字序列和树形递归 JOIN。窗口支持 `ROW_NUMBER/RANK/DENSE_RANK/LAG/LEAD/FIRST_VALUE/LAST_VALUE/NTH_VALUE/NTILE/CUME_DIST/PERCENT_RANK`、窗口 `COUNT/SUM/AVG/MIN/MAX`、命名 WINDOW 及继承、PARTITION、多列 ORDER、分组后窗口、MySQL 默认 RANGE peer、常用显式 RANGE（含单值数值偏移）与 ROWS frame，并可嵌套进派生表。未实现：JOIN ON 的常量/函数表达式、互递归复杂 CTE、完整表达式执行器、细粒度有序 Gap Lock（当前范围写安全回退表锁）、完整 CHECK 表达式与外键所有边缘动作、Trigger body 的 UPDATE/DELETE/CALL 与流程控制、存储过程、复制、MySQL 系统库完整语义。迁移生产 MySQL 前必须先跑业务 SQL 双库结果比较和故障注入；不能仅凭 Wire Protocol 连通判定完全兼容，也不会要求业务为 MyDB 修改 DDL/DML。当前已用 db233-go 完整集成测试验证其业务 DDL/DML 无需改写。

`LOAD DATA [LOCAL] INFILE` 已支持 MySQL 文件传输协议、`REPLACE`/`IGNORE`、字段/行分隔符、可选包围符、转义符、跳过头行、直接列/用户变量映射及 SET 标量表达式转换；`CHARACTER SET` 支持 binary/ascii/utf8mb3/utf8mb4/latin1/GBK/Big5/Shift-JIS/EUC-KR/UTF-16 常用别名，文本转成内部 UTF-8，BLOB/VARBINARY 保留输入原字节。LOCAL/IGNORE 的缺字段、超字段、唯一键冲突分别产生 MySQL 1261/1262/1062，OK packet warning count、`SHOW WARNINGS`、`SHOW COUNT(*) WARNINGS` 和 `@@warning_count` 可供原版客户端读取。非 LOCAL/IGNORE 的缺字段和超字段分别返回原生 1261/1262；非法字符序列即使使用 LOCAL/IGNORE 也按 MySQL 返回 1300。错误发生前已解码的行不会部分提交，整条 LOAD 保持语句原子性。整批进入正常事务、WAL 与约束路径，显式事务内也返回按唯一键冲突计算的 affected rows。LOCAL 可用 `security.local_infile` 禁用；服务端文件只允许位于 `security.secure_file_priv` 的规范化路径内，并受 `max_load_data_size` 限制。尚未实现 PARTITION、MySQL 全部冷门字符集/排序规则及完整 sql_mode warning/error 组合矩阵。

JOIN UPDATE/DELETE 同时支持有主键和无主键目标。有主键目标物化为主键 UPDATE/DELETE；无主键目标在 `FOR UPDATE` 表锁内使用查询快照物理行序号区分字节完全相同的重复行，按 MySQL 匹配顺序让后续表达式观察前序目标变更，最后生成带 affected rows 的幂等 `ReplaceRows` WAL 镜像。外连接额外携带内部存在标记，不会把未匹配 NULL 占位误认成真实全 NULL 行。

## 开发

```bash
# 运行测试
cargo test

# 运行 actor 游戏负载基准
cargo run --release -p mydb-bench -- \
  --url mysql://root:root@127.0.0.1:3306 \
  --write-mode actor-batch

# 公平限制 MyDB/MySQL80 各自块设备写速率、IOPS，交替跑 3 轮
# Docker Ubuntu 24.04 linux/amd64；预热 + 正式采样硬截止 60 秒
sudo WRITE_BPS=20mb WRITE_IOPS=500 ROUNDS=3 MEASURE_SECONDS=60 \
  bash scripts/bench-io-limited.sh

# Windows Docker Desktop 快速验证：自动发现 VM 数据盘并施加同额 cgroup 限速
pwsh -File scripts/bench-io-limited.ps1 -MeasureSeconds 60 -Rounds 3

# 构建发布版本
cargo build --release

# 代码检查
cargo clippy
cargo fmt --check
```

`bench-io-limited.sh` 的正式性能验收平台固定为 Docker Ubuntu 24.04、linux/amd64；MySQL 对照端使用官方 `mysql:8.0`，避免 Ubuntu apt 默认值改变基线。它为两个数据库分别创建同尺寸 ext4 loopback 盘，同时固定 CPU、内存、读写带宽和 IOPS，开启提交 fsync，先预热再交替执行并输出中位数及 p50/p95/p99；从预热开始到全部采样硬截止 60 秒，脚本拒绝小于 1 或大于 60 的采样预算。脚本会验证 Docker daemon 确实看得到该块设备和 bind mount。Windows Docker Desktop 可用 PowerShell 快速门禁：它探测 Linux VM 中承载 Docker volume 的真实块设备，对两个容器施加相同 cgroup 限制；由于共享物理盘，它用于开发机回归，正式结论仍采用独立 loop 盘。Windows、macOS 仍运行相同 Docker 功能 smoke，但不纳入正式性能结论。两套性能脚本只执行双方完全相同的 `ENGINE=InnoDB` DDL/DML；限制磁盘可减少设备突发差异，但仍应关闭其他重负载，保留原始多轮样本，不能把单轮数据当作 10x 结论。

## 许可证

MIT License
