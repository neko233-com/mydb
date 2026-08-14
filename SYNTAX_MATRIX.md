# MyDB MySQL 8.4 语法矩阵（验收基线）

> 本文档是 [`CheckList.md`](CheckList.md) 的**全量语法对照基线**：逐条枚举 MySQL 8.4 对外暴露的 SQL 表面，标注 MyDB 当前实现状态。状态以**源码实测**为准（检索 `crates/mydb-wire/src/lib.rs`、`crates/mydb-storage/src/lib.rs`、`vendor/opensrv-mysql`），不是目标描述。
>
> 定位：MyDB 目标是替代 MySQL 8.x 的单机部署，暴露 MySQL 的协议、形式、语法和可见行为。兼容性以源码与同机差分实测为准；InnoDB 语义不因 `ENGINE=InnoDB` 名称映射而自动视为完成。JDBC、Go、Node.js/TypeScript、JetBrains、VS Code、dbx 和 mysql CLI 均直接连接 MyDB 的 3306 端口。
>
> **非单机能力明确不支持（设计决定，非临时延迟）**：binlog 复制拓扑 / GTID、读写分离、Group Replication / Galera、分布式 XA 两阶段协调、跨节点一致性。这些在 [`SYNTAX_MATRIX.md`](SYNTAX_MATRIX.md) 中统一标记为 ❌ 明确不支持，不会纳入范围，也不视为“缺失”。单机本地 XA 支持跨连接 prepared 分支、锁保留、RECOVER、one-phase 与 durable WAL commit marker 故障恢复。
>
> 最后更新：2026-08-14（补齐 `event_scheduler` 全局变量 ON/OFF/DISABLED、DataGrip/GoLand 探测路径、无默认 schema 的 `DATABASE()`、明文 `SHOW STATUS LIKE 'ssl_version'`、SHOW 元数据 LIKE 大小写不敏感、CASE 文本投影中的嵌套 `IN`、持久 row-id、MVCC 读视图/历史版本、基础 statement-duration MDL、主键/单列二级索引 next-key/gap 区间锁、二级索引插入意向锁、RC 记录锁、复合索引全等值/左前缀范围锁、单列索引 `LIKE` 前缀范围锁、无主键基础隐藏行锁和 JOIN 基础 `SKIP LOCKED` 行过滤；新增单基表直接列投影可更新视图和 LOCAL/CASCADED CHECK OPTION、逻辑表级 RANGE/LIST/HASH/KEY 分区及常用表达式函数、`information_schema.PARTITIONS` 行级元数据、`LOAD DATA ... PARTITION` 逻辑分区校验、`PAD SPACE/NO PAD` 与字符串 `_bin` 排序规则边界；新增二级索引锁定读同步锁聚簇记录、JOIN 二级索引点锁、列对列比较范围锁、JOIN ON 常量/常用标量函数/算术表达式、HAVING 未关联标量子查询/`IN (SELECT ...)` 与存储层 UPSERT 唯一键候选镜像校验回归；完整 InnoDB 语义仍按清单逐项验收）。

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
| `GENERATED ALWAYS AS (...)` STORED/VIRTUAL 基础列 | 🟡 Partial | 基础算术/字符串表达式的 INSERT、UPDATE、WHERE、索引过滤、SHOW/`information_schema.COLUMNS` 元数据、显式写入拒绝（3105）与跨列依赖重算已与 MySQL 8.4 差分；完整 generated 表达式类型推导、物化布局与全部 DDL 边界仍 Deferred |
| `CREATE/DROP INDEX`、`CREATE UNIQUE INDEX` | ✅ Verified | |
| `RENAME TABLE`（多项）、`ALTER TABLE RENAME` | ✅ Verified | 原子镜像、跨库 |
| 表/列 `COMMENT` 持久化 | ✅ Verified | CREATE/ALTER TABLE comment 通过 CREATE SQL 持久化；SHOW FULL COLUMNS、SHOW TABLE STATUS、information_schema.tables/columns、SHOW CREATE TABLE 已回归；复杂 MODIFY 边界仍待补 |
| `PARTITION BY`（表级） | 🟡 Partial | 逻辑表级 RANGE/LIST/HASH/KEY、`RANGE/LIST COLUMNS`、常用表达式 `YEAR/MONTH/DAY/QUARTER/TO_DAYS/TO_SECONDS/UNIX_TIMESTAMP/ABS/MOD` 与基础算术的写入校验、重启恢复和 `information_schema.PARTITIONS` 元数据已验证；数据仍保持单表物理布局，子分区、在线重组与物理分区裁剪 Deferred；窗口函数 `PARTITION BY` 为独立语义 |
| `SPATIAL` / `FULLTEXT` 索引 | 🟡 Partial | `IndexKind` 已持久化区分 BTREE/FULLTEXT/SPATIAL；CREATE/ALTER、SHOW INDEX、`information_schema.STATISTICS`、FULLTEXT 布尔检索、基础 `ST_GeomFromText`/`ST_AsText`/`ST_X`/`ST_Y`/`ST_GeometryType` 已与 MySQL 8.4 真实差分覆盖。完整 InnoDB 全文倒排索引/相关性评分、停止词/查询扩展、完整 GIS 类型与空间谓词/优化仍待落地。 |
| `ENGINE=` 其它引擎（除 InnoDB/MEMORY） | ⚪ Absent | 返回错误（InnoDB 为 Neko233 别名） |
| `CREATE TABLESPACE` | ⚪ Absent | 仅权限名占位 |

### 1.3 视图 / 触发器 / 例程 / 事件
| 语法 | 状态 | 备注 / 证据 |
|------|------|-------------|
| `CREATE [OR REPLACE] VIEW` / `DROP VIEW [IF EXISTS]` / `SHOW CREATE VIEW` | ✅ Verified | 复杂 JOIN/聚合/表达式视图只读；单基表直接列投影支持 INSERT/UPDATE/DELETE、主键定位、LOCAL/CASCADED CHECK OPTION；持久化/重启恢复已验证 |
| `CREATE TRIGGER`（BEFORE/AFTER，INSERT/UPDATE/DELETE） | ✅ Verified | 完整控制流、跨表 DML、SIGNAL、自增、递归保护 |
| `CREATE PROCEDURE` / `DROP` / `SHOW` / `CALL` | ✅ Verified | IN/OUT/INOUT、控制流、游标、handler、diagnostics、多结果集、prepared OUT |
| `CREATE FUNCTION ... RETURNS ...` | ✅ Verified | 独立 `RoutineKind::Function`，表达式内调用，元数据 `IS_DETERMINISTIC`/`information_schema.routines` |
| `CREATE EVENT` / 调度器 | ✅ Verified | 真实定时器调度（`spawn_event_scheduler`），`information_schema.events` 实际填充（**与旧 CheckList “空表”备注相反，已更正**） |
| `ALTER PROCEDURE/FUNCTION` | ✅ Verified | 仅允许 MySQL 允许项，隐式提交 |
| `SHOW TRIGGERS` / `SHOW CREATE TRIGGER` / `SHOW PROCEDURE/FUNCTION STATUS` | ✅ Verified | |
| Routine 局部变量 charset/collation 字段 | 🟡 Partial | `DECLARE` 的显式 `CHARACTER SET`/`COLLATE` 会随局部变量绑定进入比较、`LIKE`、CASE/IF、DML、`COLLATION()`/`CHARSET()`；已验证 `latin1_bin` 与 MySQL 8.4.11 对齐。例程默认数据库字符集、完整字符集转码与全部排序规则权重仍 Deferred |
| Routine 进入时 `sql_mode` | ✅ Verified | Procedure 调用保存/切换/恢复定义时 `sql_mode`；MySQL 模式切换本身不要求额外 warning；Trigger 局部字符集/排序规则字段仍 Deferred |
| `EVENT DISABLE ON SLAVE` | ⚪ Absent | 明确拒绝（“not supported without replication”） |

---

## 2. 数据操作（DML）

| 语法 | 状态 | 备注 |
|------|------|------|
| `SELECT`（投影/别名/`*`/限定列） | ✅ Verified | |
| `INSERT` / `REPLACE` / `INSERT IGNORE` / `ON DUPLICATE KEY UPDATE` | ✅ Verified | 含 MySQL 8.0.19 `... AS new` 现代 UPSERT；`INSERT ... SELECT` 在 READ COMMITTED 使用源表一致读，在 REPEATABLE READ/SERIALIZABLE 使用源表共享 next-key 读锁 |
| `INSERT/REPLACE ... SET col=expr`、值中 `DEFAULT`/`DEFAULT(col)`/1364 | ✅ Verified | |
| `INSERT/REPLACE ... SELECT` | ✅ Verified | 含 IGNORE/ON DUPLICATE |
| `UPDATE` / 单目标 & 多目标 `JOIN UPDATE` | ✅ Verified | changed-row affected、no-op 不写 WAL |
| `DELETE` / `JOIN DELETE`（alias-list USING/FROM） | ✅ Verified | 无主键重复行物理序号区分 |
| `LOAD DATA [LOCAL] INFILE` | ✅ Verified | 协议/安全目录、字符集转码、1261/1262/1062/1300、`PARTITION (p0,...)` 分区名与逐行归属校验 |
| `SELECT` 谓词/聚合/`DISTINCT`/`COUNT(DISTINCT)` | ✅ Verified | |
| `JOIN` INNER/LEFT/RIGHT/CROSS/NATURAL、ON 等值/非等值/NULL-safe、常量/常用标量函数与算术表达式、USING | ✅ Verified | 复杂子查询谓词与完整优化器差异仍按子查询/执行计划条目验收 |
| 派生表、子查询（相关/非相关 IN/NOT IN/EXISTS/标量） | ✅ Verified | |
| `UNION/INTERSECT/EXCEPT DISTINCT/ALL` | ✅ Verified | |
| 非递归 & 常用递归 CTE | ✅ Verified | 前向引用/互递归按 MySQL 8.4 明确拒绝；`cte_max_recursion_depth` 的 SESSION/GLOBAL 默认传播、递归成员禁止聚合/窗口/GROUP BY/ORDER BY/DISTINCT 已验证；MySQL 8.4 无 `CYCLE` 语法 |
| 窗口函数（ROW_NUMBER…NTILE/CUME_DIST、命名 WINDOW、ROWS/RANGE frame） | ✅ Verified | |
| `GROUP BY` 表达式/别名/序号、`HAVING` | ✅ Verified | 未关联标量子查询、`IN (SELECT ...)`、按分组外层行绑定的关联标量子查询及 `AND` 组合的关联 `EXISTS` 已覆盖；更复杂关联谓词树 🔴 Deferred；显式 `ONLY_FULL_GROUP_BY` 与主键/非空唯一键函数依赖已覆盖 |
| JSON（`JSON_EXTRACT/UNQUOTE/OBJECT/ARRAY/VALID/TYPE/LENGTH/CONTAINS/CONTAINS_PATH/OVERLAPS/SET/REMOVE`） | ✅ Verified | |
| 常用字符串/数值/日期/网络/摘要/进制/三角/UUID 函数 | ✅ Verified | 见 README “当前 SQL 范围” |

---

## 3. 事务（TCL）

| 语法 | 状态 | 备注 |
|------|------|------|
| `BEGIN` / `START TRANSACTION` / `COMMIT` / `ROLLBACK` | ✅ Verified | autocommit、DDL 隐式提交、读己写 |
| `SAVEPOINT` / `ROLLBACK TO` / `RELEASE` | ✅ Verified | 重名覆盖、1305、自增回滚留洞；回滚后保留既有基础行/间隙锁，释放保存点后基础主键 INSERT 的隐式记录/索引锁 |
| 隔离级别 RU/RC/RR/SERIALIZABLE 常用可见性 | ✅ Verified | |
| 锁 IS/IX/S/X/insert-intention、基础 MDL、行锁、`FOR UPDATE`/`FOR SHARE`/`LOCK IN SHARE MODE`、NOWAIT/3572、SKIP LOCKED、死锁 1213 | 🟡 Partial | 持久 row-id、statement-duration 基础 MDL、主键/单列二级索引 next-key/gap、非唯一二级索引等值 SELECT/UPDATE 的前置 next-key gap、共享 gap 锁阻塞二级索引插入意向、复合索引全等值/左前缀范围锁、单列索引 `LIKE` 前缀范围锁、DECIMAL 与大整数二级索引按精确数值顺序比较、字符串索引按排序规则字节顺序比较、RC 实际记录锁、无主键基础隐藏行锁、自动提交锁定读边界、SERIALIZABLE 普通 SELECT 共享锁、JOIN 最终命中行记录锁、JOIN 列对列比较的点/范围锁、UPDATE 二级索引旧/新键锁、简单重复键 INSERT 的 duplicate-record S、ON DUPLICATE 唯一二级键 X/非唯一二级键 insert intention、外键子表 INSERT/UPDATE 的新父键共享记录锁、父键变更/删除对匹配子记录和 FK 范围的定向锁、JOIN 最终基表写集统一排序与按最终命中逐行 `SKIP LOCKED`、方向性 gap 冲突与同权 deadlock victim 发起者优先已落地；复杂隐式锁边界与完整 InnoDB 矩阵 🔴 Deferred |
| `XA START/BEGIN` / `XA END` / `XA PREPARE` / `XA COMMIT` / `XA ROLLBACK` / `XA RECOVER` | 🟡 Partial | 单机本地 XA 已支持 XID、状态校验、跨连接 prepared 分支、RECOVER、one-phase、prepared catalog 重载、锁转移与提交/回滚，并用同一 durable WAL 的 XA commit marker 处理 catalog 删除前崩溃的幂等恢复；分布式协调明确不支持 |

---

## 4. 账号 / 权限（DCL）

| 语法 | 状态 | 备注 |
|------|------|------|
| `CREATE USER [IF NOT EXISTS]` / `ALTER USER ... IDENTIFIED BY` | ✅ Verified | 持久化（`auth_catalog.mutate`）；账户按 `'user'@'host'` 保存，精确 host 优先于通配 host |
| `DROP USER [IF EXISTS]` | ✅ Verified | |
| `CREATE ROLE` / `DROP ROLE` / `GRANT` / `REVOKE` | ✅ Verified | 全局/库/例程权限、角色 |
| `RENAME USER old TO new` | ✅ Verified | 本轮新增（`AuthCatalog::rename_user`） |
| `SET PASSWORD [FOR user] = '...'` | ✅ Verified | 本轮新增，路由到 `alter_user_passwords` |
| `SHOW GRANTS` | ✅ Verified | |
| 表级 / 列级权限（`tables_priv`/`columns_priv`） | 🟡 Partial | 表级与 `GRANT SELECT/INSERT/UPDATE/REFERENCES (列)` 已持久化、角色继承、`SHOW GRANTS`/虚拟表展示；复杂 JOIN 的每个基表及 JOIN/WHERE/GROUP/HAVING/ORDER 投影列级 SELECT、JOIN UPDATE/DELETE 的逐目标表/逐读取列授权、`INSERT ... SELECT` 目标写列/来源读列授权、`ON DUPLICATE KEY UPDATE` 的 INSERT/UPDATE/既有行 SELECT 与 `VALUES(col)` 入参边界已验证；全局/库/表/列/例程授权委托现按被授予权限与同级或更宽范围 `GRANT OPTION` 校验，角色委托按 `ADMIN OPTION` 校验并支持 `REVOKE ADMIN OPTION FOR`；`GRANT ALL` 后各层部分 `REVOKE` 会展开并移除实际权限，跨库/越权委托已回归；完整授权撤销矩阵仍待补齐 |
| `mysql.user` / `mysql.db` / `mysql.role_edges` / `mysql.roles_mapping` 虚拟表 | ✅ Verified | 真实填充；兼容 MySQL 与 MariaDB/GoLand 角色元数据查询 |
| `mysql.global_grants` / `default_roles` / `tables_priv` / `columns_priv` / `procs_priv` / `func` 虚拟表 | ✅ Verified | `procs_priv` 反映用户/角色的例程权限；动态全局权限未实现，`global_grants` 故意为空 |
| 审计日志 | ✅ Verified | `AuditLog` 轮转 worker + metrics |
| 复制相关权限（REPLICATION SLAVE/CLIENT） | 🔵 Compatible-noop | 仅权限名声明，无复制拓扑 |

---

## 5. 系统库（System Schemas）

| 库 | 状态 | 备注 |
|----|------|------|
| `information_schema` | ✅ Verified | 虚拟表：SCHEMATA/TABLES/COLUMNS/STATISTICS/PARTITIONS/TABLE_CONSTRAINTS/KEY_COLUMN_USAGE/CHECK_CONSTRAINTS/REFERENTIAL_CONSTRAINTS/VIEWS/TRIGGERS/ROUTINES/PARAMETERS/EVENTS/APPLICABLE_ROLES/CHARACTER_SETS/COLLATIONS/ENGINES/USER_PRIVILEGES/SCHEMA_PRIVILEGES/TABLE_PRIVILEGES/COLUMN_PRIVILEGES 等；系统库自身 TABLES/COLUMNS 元数据可自描述，PARAMETERS 已覆盖 MySQL 8.4 类型长度、精度、字符集/排序规则、DTD、ROUTINE_TYPE |
| `mysql` | ✅ Verified | user/db/role_edges/roles_mapping 真实填充；global_grants/default_roles/tables_priv/columns_priv/procs_priv/func 为虚拟表，其中 procs_priv 动态反映例程授权 |
| `performance_schema` | ✅ Verified | 本轮新增虚拟表：GLOBAL_STATUS/SESSION_STATUS/GLOBAL_VARIABLES/SESSION_VARIABLES/PROCESSLIST/STATUS_BY_HOST/USER/THREAD/EVENTS_STATEMENTS_SUMMARY_BY_DIGEST/MUTEX_INSTANCES/FILE_INSTANCES/EVENTS_WAITS_SUMMARY_GLOBAL_BY_EVENT_NAME |
| `sys` | ✅ Verified | 本轮新增虚拟视图：processlist/x$processlist/metrics/x$metrics/session/x$session/statement_analysis/x$statement_analysis/sys_config/host_summary/user_summary/schema_table_statistics |
| `PROCESSLIST` 实时行 / `GLOBAL/SESSION STATUS` 完整镜像 | 🟡 Partial | `information_schema.PARTITIONS` 已覆盖非分区表基础行；performance_schema 基础 status/variables 已填充，SHOW STATUS 走独立路径；跨会话 PROCESSLIST 基础实时行已覆盖，完整 session 镜像与全部 MySQL 元数据列仍待补齐 |

---

## 6. 字符集 / 排序规则 / 时区

| 项 | 状态 | 备注 |
|----|------|------|
| 会话/库 `CHARACTER SET` 与 `COLLATE` 变量（utf8mb4_0900_ai_ci 等） | ✅ Verified | 变量可读写、`SHOW CHARSET`/`SHOW COLLATION` 有真实行 |
| `information_schema.character_sets` / `collations` | ✅ Verified | |
| `SET NAMES` / `SET CHARACTER SET` | ✅ Verified | |
| `LOAD DATA ... CHARACTER SET` | ✅ Verified | binary/ascii/utf8mb3/utf8mb4/latin1/GBK/Big5/Shift-JIS/EUC-KR/UTF-16 |
| 会话 `time_zone`（SYSTEM/固定偏移/IANA）、`CONVERT_TZ`、当前时间函数 | ✅ Verified | 见 README |
| **排序规则实际比较语义**（ORDER BY / 相等 / 唯一键排序） | 🟡 Partial | SQL 文本相等/范围/LIKE、写入校验、唯一键与索引候选路径已接入列级 `COLLATE`；`PAD SPACE`/`NO PAD`、字符串 `_bin`、`utf8mb4_0900_as_cs`/`as_ci` 与 BLOB/BINARY 字节语义边界已统一到比较/唯一键/GROUP/JOIN 路径；`SHOW FULL COLUMNS` 与 `information_schema.COLUMNS/TABLES` 返回列/表元数据。accent/locale 完整权重与冷门字符集仍 Deferred，不能宣称完整 MySQL collation。 |
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
| `KILL [CONNECTION\|QUERY]` / `SHOW GRANTS` | ✅ Verified | `CONNECTION` 通知连接任务退出并清理事务/锁；`QUERY` 中断可取消的活动等待（含 `SLEEP`）并返回 1317；权限与 1094/1095 错误已覆盖 |

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
| prepared statement（协议级 + SQL 级 `PREPARE/EXECUTE`） | ✅ Verified | 真实 TCP 回归覆盖基表列类型元数据、`UNSIGNED`、INT/DATE/BLOB 参数及 binary result row 解码；复杂表达式仍按表达式推断类型 |
| `LAST_INSERT_ID()` / `ROW_COUNT()` / `FOUND_ROWS()` / `SQL_CALC_FOUND_ROWS` | ✅ Verified | |
| 会话用户变量 `@x` | ✅ Verified | |
| Prometheus / Agent HTTP / 原生 CLI | ✅ Verified | |
| `mydbdump` / `mydb-migrate` 迁移与备份（全量/增量/PITR） | ✅ Verified | |
| Docker / Compose / 原生安装脚本 | ✅ Verified | macOS Docker、Windows/Linux 安装脚本端到端待补（CheckList） |
| 故障注入矩阵（SIGKILL/WAL 坏尾/页损坏/只读/ENOSPC） | ✅ Verified | 完整矩阵（宿主断电、磁盘满、恢复中再次中断）🔴 Deferred |

---

## 结论

- **已实现并经测试**：DDL 全量、DML 全量、事务与锁常用面、存储函数/事件调度、账号/角色/审计、information_schema+mysql 虚拟库、时区、错误码、协议与迁移。
- **本轮补齐（兼容 no-op / 虚拟表 / 表面）**：performance_schema、sys、RENAME USER、SET PASSWORD、ALTER DATABASE 选项、ANALYZE/OPTIMIZE/CHECK/REPAIR/CHECKSUM TABLE、FLUSH、CACHE INDEX、复制 SHOW 表面；本地 XA 已从兼容 no-op 提升为跨连接 prepared 分支与锁生命周期语义。
- **明确不支持（设计决定，非临时延迟 ❌）**：binlog 复制拓扑 / GTID、读写分离、Group Replication / Galera、分布式 XA 两阶段协调、跨节点一致性。这些是**非单机能力**，与 MyDB“替代 MySQL 的单机数据库”定位相悖，不会实现，也不计入“缺失”。复制 SHOW 表面（空结果）与本地 XA（会话内事务）仍作为兼容表面保留。
- **尚未完成（单机范围内的语义 🔴 Deferred）**：完整 next-key/gap 锁与全部隐式锁边界、复杂 UPSERT 表达式的全语法逐列权限、完整授权撤销矩阵、排序规则在所有 `ORDER BY`/JOIN/GROUP BY 路径的真实权重（`*_general_ci`/`*_ai_ci`）、更复杂关联 HAVING 谓词树、routine 局部变量 charset/collation、冷门语句清理边缘。基础多方死锁与成本化 victim、分组相关标量 HAVING、`AND`+关联 EXISTS、复杂 JOIN 的 SELECT 基表/列授权、JOIN DML 与 `INSERT ... SELECT` 的核心列授权、角色 `ADMIN OPTION`、递归 CTE 的 MySQL 拒绝边界已完成。它们仍是“完整 MySQL 8.4 对外表现”目标的未完成项。
