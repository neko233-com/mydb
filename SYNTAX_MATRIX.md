# MyDB MySQL 8 语法矩阵（验收基线）

> 本文档是 [`CheckList.md`](CheckList.md) 的**全量语法对照基线**：逐条枚举 MySQL 8.0 对外暴露的 SQL 表面，标注 MyDB 当前实现状态。状态以**源码实测**为准（检索 `crates/mydb-wire/src/lib.rs`、`crates/mydb-storage/src/lib.rs`、`vendor/opensrv-mysql`），不是目标描述。
>
> 定位：**替代 SQLite 的单机高性能数据库，但暴露 MySQL 的形式与语法**。兼容 = 协议、客户端、DDL/DML/TCL 行为与错误码与 MySQL 一致；**不追求内核一致**（无 InnoDB 分支）。设计目标是超高性能——同资源、同持久化级别下显著高于 MySQL/SQLite。下列标注 `Compatible-noop` 的项表示语句被接受并返回 MySQL 形态结果，但内部不执行对应内核动作——这对单机使用透明且符合“兼容 MySQL 一切的单机版”目标。
>
> **非单机能力明确不支持（设计决定，非临时延迟）**：binlog 复制拓扑 / GTID、读写分离、Group Replication / Galera、分布式 XA 两阶段协调、跨节点一致性。这些在 [`SYNTAX_MATRIX.md`](SYNTAX_MATRIX.md) 中统一标记为 ❌ 明确不支持，不会纳入范围，也不视为“缺失”。单机版本地 XA（`XA START/COMMIT` 映射为会话内事务）仍作为兼容表面保留。
>
> 最后更新：2026-07-21（本轮补齐 performance_schema / sys 虚拟库、RENAME USER / SET PASSWORD、ANALYZE/OPTIMIZE/CHECK/REPAIR/CHECKSUM TABLE、FLUSH / CACHE INDEX、复制 SHOW 表面、本地 XA 表面、ALTER DATABASE 选项；并据项目定位将非单机能力从 Deferred 重新归类为“明确不支持”）。

## 状态图例

| 标记 | 含义 |
|------|------|
| ✅ Verified | 源码实现并经测试覆盖，行为与 MySQL 一致 |
| 🟡 Partial | 已实现核心，部分边缘/语义未覆盖（见备注） |
| 🔵 Compatible-noop | 语句被接受并返回 MySQL 形态结果，内部为单机等价/空操作 |
| ⚪ Absent | 尚未实现，返回 `Unsupported SQL statement` 或明确报错 |
| 🔴 Deferred | 单机范围内已知需做、因边缘语义暂未落地（见备注与 CheckList） |
| ❌ Won't support | 明确不在目标内、设计上不做（非单机 / 分布式能力：binlog 复制、读写分离、Group Replication/Galera、分布式 XA 协调等） |

---

## 1. 数据定义（DDL）

### 1.1 数据库 / 模式
| 语法 | 状态 | 备注 / 证据 |
|------|------|-------------|
| `CREATE DATABASE [IF NOT EXISTS] name` | ✅ Verified | `lib.rs` 前缀分发 + `WriteCommand::CreateDatabase` |
| `CREATE DATABASE ... DEFAULT CHARACTER SET / COLLATE` | ✅ Verified | `database_identifier` 仅取库名，选项被忽略（默认 utf8mb4），mysqldump 形态直通 |
| `CREATE SCHEMA ...` | ✅ Verified | 同 CREATE DATABASE |
| `DROP DATABASE [IF EXISTS] name` | ✅ Verified | |
| `ALTER DATABASE [name] ... CHARACTER SET/COLLATE/UPGRADE DATA DIRECTORY NAME/READ ONLY/ENCRYPTION` | 🔵 Compatible-noop | 本轮新增；单机默认 utf8mb4，选项接受为 no-op |
| `SHOW DATABASES` / `SHOW SCHEMAS` | ✅ Verified | |
| `SHOW CREATE DATABASE` | ✅ Verified | |

### 1.2 表 / 列 / 索引
| 语法 | 状态 | 备注 / 证据 |
|------|------|-------------|
| `CREATE TABLE [IF NOT EXISTS]` 全量列/类型 | ✅ Verified | |
| `CREATE TABLE ... LIKE ...` | ✅ Verified | 跨 schema 复制列/默认/自增/键/CHECK/引擎 |
| `CREATE TABLE ... AS SELECT` | ✅ Verified | 普通/JOIN/聚合/视图来源，类型推导，原子 DDL |
| `DROP TABLE [IF EXISTS]` / `TEMPORARY` | ✅ Verified | |
| `TRUNCATE TABLE` | ✅ Verified | DDL 隐式提交、affected 0、重置自增、FK 1701 |
| `ALTER TABLE` 单条多操作（ADD/DROP/MODIFY COLUMN、ADD/DROP INDEX/UNIQUE） | ✅ Verified | ALGORITHM/LOCK 提示接受 |
| `ALTER TABLE ... FIRST\|AFTER`、`CHANGE COLUMN`、`RENAME COLUMN` | ✅ Verified | COW 重写、重启恢复 |
| `ALTER TABLE ADD/DROP PRIMARY KEY`、`RENAME INDEX` | ✅ Verified | |
| `ALTER TABLE ADD/DROP FOREIGN KEY`、`ADD/DROP CHECK` | ✅ Verified | 旧数据 WAL 前校验 |
| `ALTER COLUMN SET/DROP DEFAULT`、`ADD/DROP ... IF [NOT] EXISTS` | ✅ Verified | |
| `CREATE/DROP INDEX`、`CREATE UNIQUE INDEX` | ✅ Verified | |
| `RENAME TABLE`（多项）、`ALTER TABLE RENAME` | ✅ Verified | 原子镜像、跨库 |
| 表/列 `COMMENT` 持久化 | 🟡 Partial | 元数据列存在，`COMMENT=` 进入 `create_sql` 文本，未进入 `TableSchema.comment`（读数返回空）——见 CheckList 备注 |
| `PARTITION BY`（表级） | ⚪ Absent | `partition_by` 字段被解析但存储层不执行分区；窗口函数 `PARTITION BY` 为真实 |
| `SPATIAL` / `FULLTEXT` 索引 | ⚪ Absent | |
| `ENGINE=` 其它引擎（除 InnoDB/MEMORY） | ⚪ Absent | 返回错误（InnoDB 为 Neko233 别名） |
| `CREATE TABLESPACE` | ⚪ Absent | 仅权限名占位 |

### 1.3 视图 / 触发器 / 例程 / 事件
| 语法 | 状态 | 备注 / 证据 |
|------|------|-------------|
| `CREATE [OR REPLACE] VIEW` / `DROP VIEW [IF EXISTS]` / `SHOW CREATE VIEW` | ✅ Verified | 只读、持久化、重启恢复、禁止写 |
| `CREATE TRIGGER`（BEFORE/AFTER，INSERT/UPDATE/DELETE） | ✅ Verified | 完整控制流、跨表 DML、SIGNAL、自增、递归保护 |
| `CREATE PROCEDURE` / `DROP` / `SHOW` / `CALL` | ✅ Verified | IN/OUT/INOUT、控制流、游标、handler、diagnostics、多结果集、prepared OUT |
| `CREATE FUNCTION ... RETURNS ...` | ✅ Verified | 独立 `RoutineKind::Function`，表达式内调用，元数据 `IS_DETERMINISTIC`/`information_schema.routines` |
| `CREATE EVENT` / 调度器 | ✅ Verified | 真实定时器调度（`spawn_event_scheduler`），`information_schema.events` 实际填充（**与旧 CheckList “空表”备注相反，已更正**） |
| `ALTER PROCEDURE/FUNCTION` | ✅ Verified | 仅允许 MySQL 允许项，隐式提交 |
| `SHOW TRIGGERS` / `SHOW CREATE TRIGGER` / `SHOW PROCEDURE/FUNCTION STATUS` | ✅ Verified | |
| Routine 局部变量 charset/collation 字段 | 🔴 Deferred | `declare` 仅存 name+data_type，无 collation 字段、无字符集转换（本轮未改，见 CheckList） |
| Routine 进入时 sql_mode 差异 warning | 🔴 Deferred | `sql_mode` 被捕获/还原，但未因差异发射 warning |
| `EVENT DISABLE ON SLAVE` | ⚪ Absent | 明确拒绝（“not supported without replication”） |

---

## 2. 数据操作（DML）

| 语法 | 状态 | 备注 |
|------|------|------|
| `SELECT`（投影/别名/`*`/限定列） | ✅ Verified | |
| `INSERT` / `REPLACE` / `INSERT IGNORE` / `ON DUPLICATE KEY UPDATE` | ✅ Verified | 含 MySQL 8.0.19 `... AS new` 现代 UPSERT |
| `INSERT/REPLACE ... SET col=expr`、值中 `DEFAULT`/`DEFAULT(col)`/1364 | ✅ Verified | |
| `INSERT/REPLACE ... SELECT` | ✅ Verified | 含 IGNORE/ON DUPLICATE |
| `UPDATE` / 单目标 & 多目标 `JOIN UPDATE` | ✅ Verified | changed-row affected、no-op 不写 WAL |
| `DELETE` / `JOIN DELETE`（alias-list USING/FROM） | ✅ Verified | 无主键重复行物理序号区分 |
| `LOAD DATA [LOCAL] INFILE` | ✅ Verified | 协议/安全目录、字符集转码、1261/1262/1062/1300 |
| `SELECT` 谓词/聚合/`DISTINCT`/`COUNT(DISTINCT)` | ✅ Verified | |
| `JOIN` INNER/LEFT/RIGHT/CROSS/NATURAL、ON 等值/非等值/NULL-safe、USING | ✅ Verified | JOIN ON 常量/函数表达式 🔴 Deferred |
| 派生表、子查询（相关/非相关 IN/NOT IN/EXISTS/标量） | ✅ Verified | |
| `UNION/INTERSECT/EXCEPT DISTINCT/ALL` | ✅ Verified | |
| 非递归 & 常用递归 CTE | ✅ Verified | 复杂互递归 CTE / CYCLE 语义 🔴 Deferred |
| 窗口函数（ROW_NUMBER…NTILE/CUME_DIST、命名 WINDOW、ROWS/RANGE frame） | ✅ Verified | |
| `GROUP BY` 表达式/别名/序号、`HAVING` | ✅ Verified | 完整复杂 HAVING 子查询、ONLY_FULL_GROUP_BY 函数依赖 🔴 Deferred |
| JSON（`JSON_EXTRACT/UNQUOTE/OBJECT/ARRAY/VALID/TYPE/LENGTH/CONTAINS/SET/REMOVE`） | ✅ Verified | |
| 常用字符串/数值/日期/网络/摘要/进制/三角/UUID 函数 | ✅ Verified | 见 README “当前 SQL 范围” |

---

## 3. 事务（TCL）

| 语法 | 状态 | 备注 |
|------|------|------|
| `BEGIN` / `START TRANSACTION` / `COMMIT` / `ROLLBACK` | ✅ Verified | autocommit、DDL 隐式提交、读己写 |
| `SAVEPOINT` / `ROLLBACK TO` / `RELEASE` | ✅ Verified | 重名覆盖、1305、自增回滚留洞 |
| 隔离级别 RU/RC/RR/SERIALIZABLE 常用可见性 | ✅ Verified | |
| 锁 IS/IX/S/X、行锁、`FOR UPDATE`/`FOR SHARE`/`LOCK IN SHARE MODE`、NOWAIT/3572、SKIP LOCKED、死锁 1213 | ✅ Verified | 完整 next-key/gap、无主键 SKIP LOCKED、多方环成本化受害者 🔴 Deferred |
| `XA START/BEGIN` / `XA END` / `XA PREPARE` / `XA COMMIT` / `XA ROLLBACK` / `XA RECOVER` | 🔵 Compatible-noop | 本轮新增；映射为单机会话内事务（XA START→开事务，XA COMMIT/ROLLBACK→提交/回滚，XA RECOVER→空）。无两阶段外部协调（分布式 XA 协调 ❌ 明确不支持），符合单机定位 |

---

## 4. 账号 / 权限（DCL）

| 语法 | 状态 | 备注 |
|------|------|------|
| `CREATE USER [IF NOT EXISTS]` / `ALTER USER ... IDENTIFIED BY` | ✅ Verified | 持久化（`auth_catalog.mutate`） |
| `DROP USER [IF EXISTS]` | ✅ Verified | |
| `CREATE ROLE` / `DROP ROLE` / `GRANT` / `REVOKE` | ✅ Verified | 全局/库/例程权限、角色 |
| `RENAME USER old TO new` | ✅ Verified | 本轮新增（`AuthCatalog::rename_user`） |
| `SET PASSWORD [FOR user] = '...'` | ✅ Verified | 本轮新增，路由到 `alter_user_passwords` |
| `SHOW GRANTS` | ✅ Verified | |
| 表级 / 列级权限（`tables_priv`/`columns_priv`） | 🟡 Partial | 虚拟表存在且返回空行，但**不强制**表/列级校验；全局/库级权限强制 |
| `mysql.user` / `mysql.db` / `mysql.role_edges` 虚拟表 | ✅ Verified | 真实填充 |
| `mysql.global_grants` / `default_roles` / `tables_priv` / `columns_priv` / `procs_priv` / `func` 虚拟表 | ✅ Verified | 动态全局权限未实现，`global_grants` 故意为空 |
| 审计日志 | ✅ Verified | `AuditLog` 轮转 worker + metrics |
| 复制相关权限（REPLICATION SLAVE/CLIENT） | 🔵 Compatible-noop | 仅权限名声明，无复制拓扑 |

---

## 5. 系统库（System Schemas）

| 库 | 状态 | 备注 |
|----|------|------|
| `information_schema` | ✅ Verified | ~21 张虚拟表：SCHEMATA/TABLES/COLUMNS/STATISTICS/TABLE_CONSTRAINTS/KEY_COLUMN_USAGE/CHECK_CONSTRAINTS/REFERENTIAL_CONSTRAINTS/VIEWS/TRIGGERS/ROUTINES/PARAMETERS/EVENTS/CHARACTER_SETS/COLLATIONS/ENGINES/USER_PRIVILEGES/SCHEMA_PRIVILEGES/TABLE_PRIVILEGES/COLUMN_PRIVILEGES 等 |
| `mysql` | ✅ Verified | user/db/role_edges 真实；global_grants/default_roles/tables_priv/columns_priv/procs_priv/func 为虚拟表 |
| `performance_schema` | ✅ Verified | 本轮新增虚拟表：GLOBAL_STATUS/SESSION_STATUS/GLOBAL_VARIABLES/SESSION_VARIABLES/PROCESSLIST/STATUS_BY_HOST/USER/THREAD/EVENTS_STATEMENTS_SUMMARY_BY_DIGEST/MUTEX_INSTANCES/FILE_INSTANCES/EVENTS_WAITS_SUMMARY_GLOBAL_BY_EVENT_NAME |
| `sys` | ✅ Verified | 本轮新增虚拟视图：processlist/x$processlist/metrics/x$metrics/session/x$session/statement_analysis/x$statement_analysis/sys_config/host_summary/user_summary/schema_table_statistics |
| 缺：`information_schema.PARTITIONS` / `PROCESSLIST` 实时行 / `GLOBAL/SESSION STATUS` 镜像 | 🟡 Partial | SHOW STATUS 走独立路径；虚拟表为结构占位 |

---

## 6. 字符集 / 排序规则 / 时区

| 项 | 状态 | 备注 |
|----|------|------|
| 会话/库 `CHARACTER SET` 与 `COLLATE` 变量（utf8mb4_0900_ai_ci 等） | ✅ Verified | 变量可读写、`SHOW CHARSET`/`SHOW COLLATION` 有真实行 |
| `information_schema.character_sets` / `collations` | ✅ Verified | |
| `SET NAMES` / `SET CHARACTER SET` | ✅ Verified | |
| `LOAD DATA ... CHARACTER SET` | ✅ Verified | binary/ascii/utf8mb3/utf8mb4/latin1/GBK/Big5/Shift-JIS/EUC-KR/UTF-16 |
| 会话 `time_zone`（SYSTEM/固定偏移/IANA）、`CONVERT_TZ`、当前时间函数 | ✅ Verified | 见 README |
| **排序规则实际比较语义**（ORDER BY / 相等 / 唯一键排序） | 🔴 Deferred | 当前 `compare_row_value` 对非数值走裸字节比较，全部退化为 `*_bin` 语义；`_general_ci`/`_unicode_ci`/`_ai_ci` 的大小写/口音不敏感**尚未接入比较与索引键路径**。单机定位下对 ASCII/CJK 通常无感，但跨 locale 文本排序与唯一冲突判定与 MySQL 不完全一致。列级 collation 元数据不持久化。 |
| 全部冷门字符集（ucs2/utf16le/utf32/dec8/…） | ⚪ Absent | 仅常用别名覆盖 |

---

## 7. 管理 / 维护语句

| 语法 | 状态 | 备注 |
|------|------|------|
| `ANALYZE TABLE` | 🔵 Compatible-noop | 本轮新增；返回 MySQL 形态 `Table/Op/Msg_type/Msg_text` 行 |
| `OPTIMIZE TABLE` | 🔵 Compatible-noop | 同上（op=optimize） |
| `CHECK TABLE` | 🔵 Compatible-noop | 同上（op=check，status=OK） |
| `REPAIR TABLE` | 🔵 Compatible-noop | 同上（op=repair） |
| `CHECKSUM TABLE` | 🔵 Compatible-noop | 本轮新增；返回 `Table/Checksum`（单机无损坏，固定 0） |
| `FLUSH PRIVILEGES/STATUS/TABLES/HOSTS/LOG` | 🔵 Compatible-noop | 本轮新增；接受为 no-op（权限已实时生效） |
| `CACHE INDEX` | 🔵 Compatible-noop | 本轮新增；接受为 no-op |
| `SHOW TABLE STATUS` / `SHOW INDEX` / `SHOW COLUMNS` / `SHOW FULL TABLES` | ✅ Verified | FROM/IN/LIKE/WHERE |
| `SHOW PROCESSLIST` / `SHOW STATUS` / `SHOW VARIABLES` | ✅ Verified | |
| `SHOW ENGINES` / `SHOW CHARSET` / `SHOW COLLATION` | ✅ Verified | |
| `SHOW WARNINGS` / `SHOW ERRORS` / `SHOW COUNT(*) WARNINGS` | ✅ Verified | |
| `KILL` / `SHOW GRANTS` | ✅ Verified | |

---

## 8. 复制表面（单机兼容）

| 语法 | 状态 | 备注 |
|------|------|------|
| `SHOW MASTER STATUS` / `SHOW BINARY LOG STATUS` | 🔵 Compatible-noop | 本轮新增；返回单行空/0（无 binlog） |
| `SHOW BINARY LOGS` | 🔵 Compatible-noop | 本轮新增；空行 |
| `SHOW REPLICAS` / `SHOW SLAVE HOSTS` | 🔵 Compatible-noop | 本轮新增；空行 |
| `SHOW REPLICA STATUS` / `SHOW SLAVE STATUS` | 🔵 Compatible-noop | 本轮新增；返回标准 67 列全 NULL 单行 |
| `CHANGE MASTER/REPLICATION` / `START/STOP/RESET SLAVE/REPLICA` / `RESET MASTER` | ❌ Won't support | 明确报错：“Replication is not supported in single-node MyDB”。binlog+GTID 复制与读写分离属非单机能力，设计上不支持 |
| 二进制日志 / GTID | ❌ Won't support | 无 binlog 组件，且不作为目标；单机持久化由 Neko233 WAL 负责 |

---

## 9. 错误码 / 警告

| 项 | 状态 | 备注 |
|----|------|------|
| 中央错误码模块（`ErrorKind` 枚举，vendor/opensrv-mysql） | ✅ Verified | 覆盖 1062/1364/1644/1213/3572/1242/1222/1172/1329/1300/1261/1262/1295/1305/1758 等常用码 |
| `GET [CURRENT/STACKED] DIAGNOSTICS`、顶层 diagnostics | ✅ Verified | max_error_count/sql_notes/真实 warning-error 总数 |
| 完整 MySQL ~5000 错误码逐一映射 | 🟡 Partial | 等于 vendored opensrv-mysql 子集；具体 `ErrorKind` 引用与 warning 回传路径仍按需求扩展 |
| 冷门语句清理边缘 / 主 condition 非保证排序差分 | 🔴 Deferred | 见 CheckList |

---

## 10. 客户端协议 / 运维 / 迁移

| 项 | 状态 | 备注 |
|----|------|------|
| MySQL text/binary 协议、官方 CLI/驱动连接 | ✅ Verified | |
| prepared statement（协议级 + SQL 级 `PREPARE/EXECUTE`） | ✅ Verified | |
| `LAST_INSERT_ID()` / `ROW_COUNT()` / `FOUND_ROWS()` / `SQL_CALC_FOUND_ROWS` | ✅ Verified | |
| 会话用户变量 `@x` | ✅ Verified | |
| Prometheus / Agent HTTP / 原生 CLI | ✅ Verified | |
| `mydbdump` / `mydb-migrate` 迁移与备份（全量/增量/PITR） | ✅ Verified | |
| Docker / Compose / 原生安装脚本 | ✅ Verified | macOS Docker、Windows/Linux 安装脚本端到端待补（CheckList） |
| 故障注入矩阵（SIGKILL/WAL 坏尾/页损坏/只读/ENOSPC） | ✅ Verified | 完整矩阵（宿主断电、磁盘满、恢复中再次中断）🔴 Deferred |

---

## 结论

- **已实现并经测试**：DDL 全量、DML 全量、事务与锁常用面、存储函数/事件调度、账号/角色/审计、information_schema+mysql 虚拟库、时区、错误码、协议与迁移。
- **本轮补齐（兼容 no-op / 虚拟表 / 表面）**：performance_schema、sys、RENAME USER、SET PASSWORD、ALTER DATABASE 选项、ANALYZE/OPTIMIZE/CHECK/REPAIR/CHECKSUM TABLE、FLUSH、CACHE INDEX、复制 SHOW 表面、本地 XA 表面。
- **明确不支持（设计决定，非临时延迟 ❌）**：binlog 复制拓扑 / GTID、读写分离、Group Replication / Galera、分布式 XA 两阶段协调、跨节点一致性。这些是**非单机能力**，与 MyDB“替代 SQLite 的单机高性能数据库”定位相悖，不会实现，也不计入“缺失”。复制 SHOW 表面（空结果）与本地 XA（会话内事务）仍作为兼容表面保留。
- **范围外（单机定位下的边缘语义 🔴 Deferred）**：完整 next-key/gap 锁与多连接成本化死锁、表/列级权限强制、排序规则真实比较语义（`*_general_ci`/`*_ai_ci`）、JOIN ON 常量/函数表达式、复杂互递归 CTE、ONLY_FULL_GROUP_BY 函数依赖、routine 局部变量 charset/collation 与进 routine 时 sql_mode 差异 warning、冷门语句清理边缘。这些在 CheckList 标注为 Deferred，不影响“兼容 MySQL 一切的单机版”目标。
