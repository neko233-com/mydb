# MyDB 落地验收清单

> 规则：只有当前源码和可复现实测能证明的项目才打勾。宽泛目标不能由局部 smoke 代替。最后更新：2026-08-15。

> **范围边界（当前验收口径）**：MyDB 目标是替代 MySQL 8.x 的单机部署，以 MySQL 协议、SQL、事务和可见外部行为为验收面。单机客户端直接连接 3306；复制拓扑、Group Replication / Galera、分布式 XA 两阶段协调仍不宣称已实现。完整 InnoDB 语义必须逐项通过本清单，不能由名称别名代替。

## 最终发布门槛

- [ ] MySQL 8.4 单机全部 DML、事务、错误码和可见外部行为完成兼容矩阵并逐项通过差分
- [ ] 稳定性、崩溃恢复、断线重连和故障注入覆盖生产边界
- [ ] Ubuntu 24.04、linux/amd64、双方相同 `ENGINE=InnoDB`、I/O/CPU/内存同限、总计不超过 60 秒的正式性能验收完成
- [ ] 写吞吐和延迟稳定达到 10x；若客观无法达到，保留原始数据并明确实际结果
- [ ] Windows、Linux、macOS 的原生安装及 Docker 功能均在真实平台通过
- [ ] 全量迁移、校验、切流和回滚演练完成
- [ ] 上述门槛全部通过后再停止并卸载本机 MySQL80

## 内核与存储

- [x] `InnoDB` 映射到自研持久化 Neko233 引擎，`MEMORY` 保持独立非事务语义
- [x] 持久 row-id：新写入稳定分配，旧数据启动迁移补齐，替换/重启保持行身份；row 编码保留 legacy 解码路径
- [x] MVCC 基础：事务 ID、commit 序号、RR/SERIALIZABLE 固定读视图、RC 语句读视图、删除可见历史版本及旧版本清理基础
- [x] Leader/Follower FIFO、事务批次、group commit、CRC WAL、断尾截断、checkpoint 与恢复
- [x] 同主键顺序写、并发计数更新和 UPSERT 不丢写
- [x] 主键/唯一索引、AUTO_INCREMENT、NULL/空字节/BLOB 持久化
- [x] 存储目录感知与只清理未引用 page generation；测试证明不会删除正在引用的数据
- [x] Prometheus 暴露 prepare/WAL sync/apply/checkpoint/锁/错误等指标
- [x] Docker SIGKILL 故障注入：强杀前确认事务脏写存在；自动恢复后已提交数据保留、未提交写丢弃，客户端可重连并提交新事务
- [x] Docker WAL torn-tail 故障注入：停机后向最新 WAL 段追加 5 字节残片；启动精确截回最后有效记录，数据不丢且可继续写
- [x] Docker WAL 中段损坏故障注入：篡改首条 WAL payload 且保留后续字节；启动拒绝恢复并报告 CRC 损坏，不静默丢弃后续记录
- [x] Docker 只读数据目录故障注入：启动安全拒绝，恢复权限后重放成功并可提交新 WAL 写入
- [x] Docker ENOSPC 故障注入：独立 8 MiB tmpfs 数据目录上大 WAL 写返回 MySQL 1105，服务存活且失败事务零行
- [x] Docker 页损坏故障注入：篡改持久化 `pages.dat` 已校验数据字节；启动安全拒绝并报告页校验损坏，不静默少读数据
- [x] 恢复中断边界：模拟页已持久化但 `Applied` 未写入时进程消失；真实重启重新 replay 后无重复行且补写一个 `Applied`
- [ ] 完整故障注入矩阵：宿主断电、磁盘满、只读盘、WAL 中段/页损坏、恢复中再次中断
- [ ] 长时间压力、磁盘空间回收、碎片整理及多 TB 数据验证

## MySQL 协议与 DML

- [x] MySQL text/binary prepared wire protocol，可用官方 MySQL 8 CLI 和 db233-go 原 SQL 连接；真实 TCP 回归覆盖基表列类型元数据、`UNSIGNED`、INT/DATE/BLOB 参数及 binary result row
- [x] MySQL 8.4.10 CLI 连接探测：`SELECT @@version_comment LIMIT 1` 后的 `SELECT $$` 语法探针按 1064 返回，3306 可继续执行用户 SQL
- [x] 常用数据库/表/列/索引 DDL、schema-qualified DML、真实 MySQL 8 dump 导入
- [x] `CREATE TABLE [IF NOT EXISTS] ... LIKE ...`：跨 schema 复制列、默认值、自增属性、主键/唯一/普通索引、CHECK 和引擎；不复制行、外键及当前自增计数
- [x] 表/列 `COMMENT`：CREATE/ALTER TABLE、SHOW FULL COLUMNS、SHOW TABLE STATUS、information_schema.tables/columns、SHOW CREATE TABLE 已回归；复杂 `MODIFY COLUMN` 组合仍待补
- [x] INSERT/REPLACE/INSERT IGNORE/ON DUPLICATE KEY UPDATE/AUTO_INCREMENT；支持 MySQL `INSERT/REPLACE ... SET col=expr`、`VALUES(DEFAULT, scalar_expr)`、`DEFAULT(col)`、`INSERT ... () VALUES ()`/`VALUES ()` 默认行，以及普通/无主键/JOIN UPDATE 和 UPSERT 的 `col=DEFAULT/DEFAULT(col)`；覆盖事务回滚、1364、affected rows 及冲突尝试自增留洞
- [x] MySQL 8.0.19+ `INSERT ... VALUES/SET ... AS new [(alias,...)] ON DUPLICATE KEY UPDATE`：支持 `new.col`、`new.alias`、无限定列别名、表限定旧行值，以及 IF/CASE/CONCAT/COALESCE/GREATEST/LEAST/组合算术等常用冲突标量表达式；赋值严格左到右，覆盖事务回滚、changed-row affected rows 及自增留洞
- [x] UPDATE/UPSERT affected rows 按 MySQL 默认 changed rows 计算：literal/scalar/expression/JOIN/无主键/事务路径覆盖；no-op 在排他锁后判定，不写 WAL、不 fsync、不重写表，duplicate UPSERT 不返回伪 insert id
- [x] INSERT/REPLACE ... SELECT，含 IGNORE/ON DUPLICATE、REPEATABLE READ/SERIALIZABLE 源表共享 next-key 锁、READ COMMITTED 一致读及事务回滚
- [x] `TRUNCATE TABLE`：DDL 隐式提交、affected rows 0、空表仍重置 AUTO_INCREMENT、外键 1701、子表及 FOREIGN_KEY_CHECKS=0 语义
- [x] SELECT/UPDATE/DELETE、ORDER BY、LIMIT/OFFSET、DISTINCT、常用谓词与聚合
- [x] 普通表、派生表、JOIN、GROUP BY、UNION/INTERSECT/EXCEPT 的多列/表达式 ORDER BY、别名和序号
- [x] 单列/多列及表达式/别名/序号 GROUP BY；HAVING 和 ORDER BY 支持投影别名、未投影 COUNT/SUM/AVG/MIN/MAX、CASE 条件聚合
- [x] 常用 CASE/IF/NULLIF、字符串/数值/CAST/CONVERT 投影，以及单表/JOIN 函数化 WHERE
- [x] 常用文本/数学/列表/编码/标识/校验函数：TRIM/REPLACE/LOCATE/INSTR/LPAD/RPAD/REVERSE/REPEAT、ASCII/ORD/BIT_LENGTH/OCTET_LENGTH/CHARACTER_LENGTH、SPACE/STRCMP/SUBSTRING_INDEX/字符串 INSERT/QUOTE、BIN/OCT/HEX/UNHEX/TO_BASE64/FROM_BASE64/FORMAT、MD5/SHA/SHA1/SHA2/CRC32、UUID/UUID_TO_BIN/BIN_TO_UUID/IS_UUID、INET_ATON/INET_NTOA/INET6_ATON/INET6_NTOA 与 IP 校验、FIND_IN_SET/FIELD/ELT/MAKE_SET/EXPORT_SET、CONV/BIT_COUNT、PI/DEGREES/RADIANS/SIN/COS/TAN/COT/ASIN/ACOS/ATAN/ATAN2、POW/SQRT/MOD/SIGN/EXP/LN/LOG/LOG2/LOG10/TRUNCATE；覆盖 UTF-8/二进制、迁移摘要、二进制 UUID 主键、IPv4/IPv6、权限位掩码、2–36 进制、游戏坐标向量、定义域、SELECT、WHERE、UPDATE、事务回滚和聚合嵌套，并限制放大结果为 64 MiB
- [x] 常用日期时间表达式：NOW/CURRENT_TIMESTAMP/LOCALTIME/LOCALTIMESTAMP/CURDATE/CURTIME/CURRENT_TIME/SYSDATE、UTC_DATE/UTC_TIME/UTC_TIMESTAMP、UNIX_TIMESTAMP/FROM_UNIXTIME、CONVERT_TZ、DATE_ADD/DATE_SUB/ADDDATE/SUBDATE/DATEDIFF、TIMESTAMP/TIMESTAMPADD、DATE_FORMAT/GET_FORMAT/STR_TO_DATE/TIME_FORMAT、TIME/MICROSECOND/TIME_TO_SEC/SEC_TO_TIME/TIMEDIFF/ADDTIME/SUBTIME/MAKETIME、WEEK/WEEKOFYEAR/YEARWEEK、TO_DAYS/FROM_DAYS/TO_SECONDS、PERIOD_ADD/PERIOD_DIFF、EXTRACT 基础及复合单位、DAYOFYEAR/WEEKDAY/QUARTER/DAYNAME/MONTHNAME/LAST_DAY/MAKEDATE；NOW/local/UTC/UNIX 当前时间使用语句开始快照，SYSDATE 使用实际调用时刻，别名支持 0–6 位 fsp 和 UTC/local 输出；会话 `time_zone` 支持 SYSTEM、`-13:59` 至 `+14:00` 固定偏移及内置 IANA 命名时区，按连接隔离并影响 NOW、SYSDATE、UNIX_TIMESTAMP(datetime)、FROM_UNIXTIME、动态默认值和 ON UPDATE，SET 多赋值保持左到右；CONVERT_TZ 支持固定偏移、UTC/GMT/SYSTEM/IANA、跨日、微秒、DST 跳时 NULL 和回拨多对一；另支持 SQL_TSI_ 单位、月末收敛、日期算术简写、MySQL 月名/微秒格式符、文本日期导入往返、year-0 日序、YYMM/YYYYMM 月周期、紧凑数字日期、周模式 0–7、ISO 跨年 cohort、负时长、跨天小时、微秒、冷却时间构造与加减、闰年/月末/跨年、SELECT/INSERT/UPDATE/UPSERT/事务和动态 CURRENT_TIMESTAMP/NOW 默认值（含 fsp）
- [x] TIMESTAMP/DATETIME ON UPDATE CURRENT_TIMESTAMP/NOW（含 fsp）：按 changed row 刷新、批量逐行精确、显式赋值覆盖、no-op 不刷新，覆盖 literal/scalar/expression/UPSERT/JOIN/事务/重启，WAL 前固化时间
- [x] 分析型常用 SQL：DATE_FORMAT/TIMESTAMPDIFF/日期组成提取，GROUP BY 表达式/别名/序号，COUNT(DISTINCT CASE...)、SUM(CASE...)、聚合结果嵌套 ROUND/CONCAT；注册 cohort、次日留存率、DAU、每日收入用例通过
- [x] 任意表数链式 INNER/LEFT/RIGHT/CROSS JOIN
- [x] JOIN ON 列对列等值/非等值/NULL-safe、常量与常用标量函数/算术表达式、括号 AND/OR、多列 USING；锁定读对非索引表达式使用安全表级回退
- [x] NATURAL INNER/LEFT/RIGHT 的公共列匹配、COALESCE 和 `SELECT *` 列序
- [x] 非相关及复合布尔相关 EXISTS/NOT EXISTS、IN/NOT IN、标量子查询比较
- [x] 标量子查询多行返回 MySQL 错误 1242，相关 NOT IN 覆盖 NULL 三值语义
- [x] FROM 派生表支持过滤、聚合、嵌套、相关子查询及 JOIN 左/右/两侧
- [x] UNION/UNION DISTINCT/UNION ALL 链、全局 ORDER BY/LIMIT、派生表嵌套及错误 1222
- [x] INTERSECT/EXCEPT DISTINCT/ALL、INTERSECT 优先级
- [x] 非递归多 CTE、显式列名、前置 CTE 引用及 CTE JOIN
- [x] 常用递归 CTE：anchor + UNION DISTINCT/ALL、数字序列、树形递归 JOIN、1000 层保护
- [x] 窗口 ROW_NUMBER/RANK/DENSE_RANK/LAG/LEAD/FIRST/LAST/NTH/NTILE/CUME_DIST/PERCENT_RANK、聚合窗口、命名 WINDOW、分组后窗口、ROWS 与常用 RANGE frame
- [x] 常用 JSON_EXTRACT/JSON_UNQUOTE 和标量 IS NULL
- [x] 正则函数：`REGEXP_LIKE`、`REGEXP_INSTR`、`REGEXP_SUBSTR`、`REGEXP_REPLACE` 基础 match_type、位置、occurrence、return_option、NULL 与 Unicode 语义；Rust 回归与 MySQL 8.4 差分覆盖
- [x] 游戏 profile 常用 JSON CRUD：JSON_OBJECT/ARRAY/VALID/TYPE/LENGTH/CONTAINS/CONTAINS_PATH/OVERLAPS/SET/REMOVE，覆盖 INSERT/SELECT/WHERE/UPDATE/UPSERT 和事务回滚；另有 JSON_ARRAY_APPEND/ARRAY_INSERT/MERGE_PATCH/DEPTH/KEYS/PRETTY 的 Rust 回归与 MySQL 8.4 差分
- [x] VIEW：CREATE/CREATE OR REPLACE/DROP/SHOW CREATE/SHOW FULL TABLES；复杂 JOIN/聚合/表达式视图只读，单基表直接列投影支持 INSERT/UPDATE/DELETE 与 LOCAL/CASCADED CHECK OPTION；DDL 隐式提交、重启恢复
- [x] CREATE TABLE [IF NOT EXISTS] ... AS SELECT：普通/JOIN/聚合/视图来源、结果列类型推导、DDL 隐式提交、建表与首批数据同一 WAL 原子组、重启恢复
- [x] RENAME TABLE 多项及 ALTER TABLE RENAME TO/AS：原子 schema+数据镜像，保留普通表、无主键重复行、AUTO_INCREMENT、视图定义、DDL 隐式提交和重启恢复
- [x] 单条 ALTER TABLE 多操作：组合 ADD/DROP/MODIFY COLUMN、ADD/DROP INDEX/UNIQUE KEY，接受 ALGORITHM/LOCK 提示；整批预校验、失败零变更、同一 WAL 原子组、DDL 隐式提交和重启恢复
- [x] ALTER TABLE 列演进：ADD/MODIFY ... FIRST|AFTER、CHANGE COLUMN、RENAME COLUMN；保持旧行字段值、列顺序、主键/自增和索引引用，使用 COW 行重写并可重启恢复
- [x] ALTER TABLE ADD/DROP PRIMARY KEY、RENAME INDEX/KEY；新增主键/唯一索引扫描旧行，在 WAL 前拒绝重复或主键 NULL；失败组合零 WAL，DROP COLUMN 同步 COW 清理旧物理字段
- [x] ALTER TABLE ADD/DROP FOREIGN KEY、ADD/DROP CHECK/CONSTRAINT；普通 ALTER 保留既有 FK/CHECK，新增约束在 WAL 前验证旧数据，依赖列禁止误删/改名，可先删约束后同批演进并重启恢复
- [x] ALTER COLUMN SET/DROP DEFAULT；ADD COLUMN/INDEX IF NOT EXISTS、DROP COLUMN/INDEX IF EXISTS；动态时间默认值保持无引号 SHOW CREATE，ADD COLUMN 默认值通过 COW 对旧行物化
- [x] 会话用户变量：SET @x=/@x:=、SELECT/函数/DML/WHERE/绑定参数使用，连接隔离、NULL/二进制安全、事务回滚不撤销；mysqldump @OLD_*=@@session_var 保存恢复及 SET NAMES
- [x] SQL 级命名 prepared statement：PREPARE ... FROM 字符串/@变量、EXECUTE ... USING @变量、DEALLOCATE/DROP PREPARE；参数数量、NULL/BLOB、事务回滚、模板快照和连接隔离
- [x] LAST_INSERT_ID()/LAST_INSERT_ID(expr)、ROW_COUNT()、FOUND_ROWS() 会话状态；changed-row/no-op、回滚留 ID、连接隔离，以及 UPSERT id=LAST_INSERT_ID(id) 返回既有主键
- [x] SQL_CALC_FOUND_ROWS：忽略顶层 LIMIT/OFFSET 计算完整 WHERE/DISTINCT/GROUP 结果，下一条 FOUND_ROWS() 返回全量；支持命名/协议 prepared 和连接隔离
- [x] 显式列定义 CREATE/DROP TEMPORARY TABLE：连接唯一隐藏物理表、同名永久表遮蔽、完整 CRUD/JOIN/ALTER/TRUNCATE、InnoDB 事务、不隐式提交、断线异步清理及启动安全清理崩溃残留
- [x] CREATE TEMPORARY TABLE ... LIKE/AS SELECT：LIKE 复制结构不复制数据；CTAS 建表与首批行原子提交；连接遮蔽及不隐式提交语义一致
- [x] 临时表 ALTER TABLE ... RENAME：同库元数据改名、跨库原子搬移、未提交写/快照重定向、事务锁保留；RENAME TABLE 按 MySQL 限制不操作临时表
- [x] SHOW TABLES/FULL TABLES 与 information_schema 不暴露临时隐藏物理名；SHOW CREATE/COLUMNS/DESCRIBE 使用连接逻辑名
- [x] SHOW INDEX/INDEXES/KEYS：主键和二级索引逐列元数据、基数、可空性、跨库语法及临时逻辑名
- [x] SHOW TABLE STATUS：FROM/IN、LIKE、WHERE Name 等值过滤；引擎、实际行数、近似数据长度、AUTO_INCREMENT 与视图状态
- [x] information_schema 只读虚拟表：SCHEMATA/TABLES/COLUMNS/STATISTICS/PARTITIONS/TABLE_CONSTRAINTS/KEY_COLUMN_USAGE/CHECK_CONSTRAINTS/APPLICABLE_ROLES，多行投影、过滤、排序、分组和跨表 JOIN；系统库自身 TABLES/COLUMNS 元数据可自描述，表级 RANGE/LIST/HASH/KEY 分区、常用表达式分区函数及重启后的分区元数据已回归
- [x] mydbdump/mydb-migrate/ORM 风格元数据查询：表/列枚举、COALESCE 引擎、复合索引 GROUP_CONCAT、PK/UNIQUE/FK/CHECK 和临时物理名隐藏
- [x] REFERENTIAL_CONSTRAINTS 与 VIEWS：引用唯一键、UPDATE/DELETE 规则、目标表、视图定义/安全类型/只读状态
- [x] ROUTINES/PARAMETERS 真实存储过程元数据；`information_schema.PARAMETERS` 覆盖 MySQL 8.4 的类型长度、数值/时间精度、字符集/排序规则、DTD 与 ROUTINE_TYPE 字段；EVENTS 真实持久化并由 event scheduler 调度，未实现事件时 ORM 探测返回 0 行
- [x] SHOW TABLES/COLUMNS/INDEX/TABLE STATUS 的 FROM/IN、LIKE、复合 WHERE 条件
- [x] BEFORE INSERT Trigger：CREATE/DROP/SHOW/SHOW CREATE/information_schema，SET NEW 多赋值、表达式、普通/多行/IGNORE/REPLACE/UPSERT/INSERT SELECT/LOAD 路径、事务/重启/WAL/表改名/删除
- [x] AFTER INSERT Trigger：BEGIN/END 多条跨表 INSERT、NEW 二进制安全绑定、目标 Trigger 链、全写集预锁、同批事务/WAL、回滚原子性与递归环/深度保护
- [x] BEFORE/AFTER UPDATE/DELETE Trigger：OLD/NEW、BEFORE UPDATE SET NEW、多行与表达式 UPDATE、删除前后跨表 INSERT、事务隔离/回滚原子性、主键精确物化及无主键安全拒绝
- [x] Trigger `SIGNAL SQLSTATE '45000' [SET MESSAGE_TEXT=expr]`：支持 OLD/NEW 与常用标量表达式，语句级原子拒写，事务前序写保留，Wire 返回 MySQL 1644/45000
- [x] Trigger AUTO_INCREMENT：BEFORE INSERT 读取 0，AFTER INSERT 读取最终预留 ID；多行/INSERT SELECT/显式事务/回滚/SIGNAL 保持自增空洞，Trigger 内自增不污染客户端 LAST_INSERT_ID，副作用不计入 affected rows
- [x] 冲突写 Trigger 分支：ON DUPLICATE KEY UPDATE 按实际 INSERT/UPDATE 触发；INSERT IGNORE 对每次尝试执行 BEFORE、仅成功行执行 AFTER；REPLACE 按 BEFORE INSERT→BEFORE/AFTER DELETE→AFTER INSERT，支持多唯一键冲突、INSERT SELECT、LOAD DATA、事务和精确 affected rows
- [x] Trigger body 跨表 UPDATE/DELETE：OLD/NEW 绑定、表达式/WHERE/ORDER/LIMIT、主键逐行幂等物化、递归目标表预锁、目标行事件审计、并发计数无丢写、事务回滚、自表修改拒绝及无主键安全拒绝
- [x] 嵌套 mutation Trigger：A→B→C 行事件递归执行、整张 Trigger 图预锁、嵌套 affected rows 隔离、32 层限制及 A→B→A 环原子拒绝
- [x] BEFORE UPDATE body：SET NEW 与跨表 INSERT/UPDATE/DELETE 组合，普通/表达式/UPSERT 更新统一执行，目标 Trigger 链与事务回滚一致
- [x] BEFORE INSERT body：SET NEW 与跨表 INSERT/UPDATE/DELETE 组合；NEW AUTO_INCREMENT 在副作用中为 0，普通/多行/IGNORE/REPLACE/UPSERT/INSERT SELECT/LOAD、冲突自增空洞、并发预锁、自表拒绝和事务回滚一致
- [x] Trigger IF/ELSEIF/ELSE/END IF：支持嵌套分支、BEFORE SET 后动态 NEW/OLD 条件、条件 DML/SIGNAL；CREATE 与预锁遍历全部分支，运行时只执行命中分支
- [x] Trigger CASE：支持简单 CASE、搜索 CASE、嵌套分支、选择表达式单次求值、BEFORE SET 后动态 NEW/OLD、分支 DML/SIGNAL、全分支校验预锁与事务回滚
- [x] Trigger DECLARE/局部变量基础：多变量声明、DEFAULT、顺序 SET、OLD/NEW、IF/CASE、SET NEW、SIGNAL、跨表 DML、变量/列同名绑定、每行独立状态与事务回滚；CREATE 阶段拒绝声明顺序、重复和未知变量错误
- [x] Trigger LOOP/WHILE/REPEAT：动态局部变量条件、嵌套标签、LEAVE/ITERATE、结束标签校验、循环体 DML/SET NEW、BEFORE/AFTER 时机、全体预锁、事务回滚及一百万次安全上限
- [x] Trigger 嵌套 BEGIN 作用域：作用域栈、内层同名遮蔽、退出恢复、块标签 LEAVE、ITERATE 仅循环、越域变量和同块声明顺序校验、DML 与事务回滚
- [x] Trigger 局部变量常用类型转换：整数/UNSIGNED/BIGINT 边界、DECIMAL scale、浮点、CHAR/VARCHAR 字符截断、BINARY/VARBINARY 补零、BLOB/TEXT、DATE/DATETIME/TIMESTAMP/TIME 基础校验、NULL、越界原子失败及禁止 SET local=DEFAULT
- [x] PROCEDURE/CALL 基础：CREATE/DROP/SHOW/SHOW STATUS、WAL+routines.json 持久化、IN/OUT/INOUT、用户变量回传、复用复合控制流与类型系统、DML 自动提交原子性、显式事务回滚、SIGNAL 回滚、32 层递归限制、ROUTINES/PARAMETERS 元数据及 MySQL 1304/1305
- [x] Procedure SELECT INTO：前置/尾置 INTO、逐语句异步执行、按声明类型赋值、查询结果驱动后续 IF/循环/DML、零行 warning 1329 保持原值、多行 MySQL 1172 与 CALL 写集原子回滚
- [x] Procedure 游标与 NOT FOUND handler 基础：DECLARE/OPEN/FETCH/CLOSE、变量→游标→handler 声明顺序、块级作用域/隐式关闭、OPEN 时事务可见结果物化、只读单向遍历、FETCH 列数与局部变量类型赋值、NOT FOUND/SQLSTATE 02000 CONTINUE handler、SELECT INTO 共用 handler、MySQL 1325/1326/1329
- [x] Procedure condition handler：DECLARE CONDITION（错误码/SQLSTATE）、单/多 condition CONTINUE/EXIT handler、简单/复合 handler body 独立作用域、NOT FOUND/SQLWARNING/SQLEXCEPTION、错误码/SQLSTATE/命名 condition、内层作用域与错误码>SQLSTATE>类别优先级、DML/SIGNAL/SELECT INTO/游标错误统一调度、活动 handler 防自递归、裸 RESIGNAL 与 CALL 原子回滚
- [x] Procedure RESIGNAL/diagnostics：RESIGNAL 原条件或 SQLSTATE/命名 SQLSTATE condition、SET 覆盖 MESSAGE_TEXT/MYSQL_ERRNO/condition items、自定义 u16 错误码与 SQLSTATE 原样写入 Wire；GET CURRENT/STACKED DIAGNOSTICS、NUMBER/ROW_COUNT、完整常用 condition item、局部/参数/用户变量目标、handler 内 CURRENT 刷新与 STACKED 保留、无活动 STACKED 拒绝、condition 越界 1758/35000 condition
- [x] Procedure characteristics/ALTER：CREATE IF NOT EXISTS、COMMENT、LANGUAGE SQL、[NOT] DETERMINISTIC、四类 SQL DATA ACCESS、SQL SECURITY；ALTER 可更新 MySQL 允许项并隐式提交，禁止改 DETERMINISTIC/body/参数；尾部 V2 WAL 变体兼容旧布局，版本化 routines.json 自动升级旧目录，CREATED/LAST_ALTERED/SQL_MODE 重启持久化，CALL 使用例程模式并恢复调用者，SHOW CREATE/SHOW STATUS/ROUTINES 元数据同步
- [x] Procedure 多结果集：过程内普通 SELECT 和嵌套 CALL 按执行顺序返回不同列形结果集，SELECT INTO 不外发；Wire 在中间终止包设置 SERVER_MORE_RESULTS_EXISTS，并追加 MySQL CALL 最终空状态结果；mydb-cli 顺序消费全部结果，未处理错误丢弃暂存结果并保持 CALL 写集原子回滚
- [x] Prepared CALL OUT/INOUT：握手声明 CLIENT_MULTI_RESULTS/CLIENT_PS_MULTI_RESULTS；COM_STMT_EXECUTE 允许 OUT 占位符并忽略其输入值、INOUT 读取绑定初值；普通结果后按声明顺序追加单行参数结果，元数据标记 SERVER_PS_OUT_PARAMS，再发送最终 CALL 状态；BIGINT/UNSIGNED/DECIMAL/FLOAT/DOUBLE/DATE/DATETIME/TIMESTAMP/TIME/文本/BLOB 等按声明类型输出 Binary Wire 元数据和值；失败不传播参数，内部变量无泄漏，真实 mysql Rust 驱动消费后可继续查询
- [x] 连接顶层 GET [CURRENT] DIAGNOSTICS：普通 SQL 自动重建独立会话 area；支持 NUMBER/ROW_COUNT、全部常用 condition items、字面量/用户变量 condition number 与用户变量目标；成功、Note/warning、Wire 错误、越界追加 condition、诊断语句不清空、普通 SELECT 清空和连接隔离；GET STACKED 在非 handler 拒绝，COM_STMT_PREPARE 返回 MySQL 1295
- [x] Diagnostics 多 condition/容量：按产生顺序保存 LOAD DATA 等多 warning，主错误追加在已有 warning 后；`max_error_count` 默认 1024、支持 0..65535/DEFAULT，仅限制 SHOW/GET 可保存 condition，`warning_count/error_count` 保留真实总数并可高于上限；越界 GET 在容量允许时追加 1758，容量已满时只增加总数；`sql_notes=OFF` 不记录 Note，两个 count 变量只读
- [x] Trigger schema 新字段保持旧字段编码顺序；旧二进制 WAL 使用 legacy TableSchema/WriteCommand 解码回退
- [x] FUNCTION 对象（独立 `RoutineKind::Function`，表达式内调用，`DETERMINISTIC`/`CONTAINS SQL`/`SQL SECURITY` 解析并持久化）与 EVENT（真实后台定时器调度，`information_schema.events` 实际填充 24 列——**纠正旧备注“结构化空表”**）完整边缘行为
- [ ] Trigger/Procedure 局部变量完整字符集/排序规则字段；已落地显式 `CHARACTER SET`/`COLLATE` 的绑定、比较、`LIKE`、CASE/IF、DML、`COLLATION()`/`CHARSET()`，并以 MySQL 8.4.11 `latin1_bin` 差分验证；例程默认数据库字符集、完整字符集转码、全部排序规则权重、主 condition 的 MySQL 非保证排序差分、所有冷门语句清理细节、routine 权限及 handler 所有边缘条件仍 **Deferred**，列于 SYNTAX_MATRIX §1.3/§6。Routine 保存并恢复定义时 `sql_mode` 已验证；MySQL 对调用方与 routine 模式切换本身不要求额外 warning。
- [x] 常用 CHECK、命名/复合外键 RESTRICT/CASCADE/SET NULL 及 MySQL 错误码；外键写入检查对被引用父记录建立共享锁，子表 INSERT/UPDATE 不再锁整组关联表
- [x] 单目标及多目标 UPDATE ... JOIN；DELETE alias-list FROM ... JOIN 与 DELETE FROM alias-list USING ...，主键级锁定物化、事务/WAL/约束路径一致
- [x] 无主键 JOIN UPDATE/DELETE：表锁内使用物理行序号区分完全重复行，顺序表达式观察前序目标变更，整表镜像 WAL 幂等重放
- [x] `LOAD DATA LOCAL INFILE` MySQL 文件传输协议及安全目录内服务端 `INFILE`；字段/行分隔、包围、转义、IGNORE 行、列/用户变量映射、SET 表达式转换、常用字符集转码、BLOB 原字节、1261/1262/1062 warning、strict 1261/1262/1300 原子失败、事务回滚与 affected rows
- [x] 有主键单表的 CASE/函数化 UPDATE SET/WHERE 与 DELETE WHERE，含左到右赋值、ORDER/LIMIT 和事务回滚
- [x] 递归 CTE 前向引用/互递归拒绝、`cte_max_recursion_depth` 的 SESSION/GLOBAL 默认传播、递归成员禁止聚合/窗口/GROUP BY/ORDER BY/DISTINCT；MySQL 8.4 不支持 `CYCLE`，保持明确拒绝（SYNTAX_MATRIX §2）
- [x] 完整表达式/函数/类型转换/时区语义；**排序规则比较语义 Partial**：SQL 文本相等/范围/LIKE、写入校验、唯一键与索引候选路径已接入列级 `COLLATE`；`PAD SPACE`/`NO PAD`、字符串 `_bin` 与 BLOB/BINARY 字节语义边界已统一到比较/唯一键/GROUP/JOIN 路径，`SHOW FULL COLUMNS` 与 `information_schema.COLUMNS/TABLES` 返回列/表元数据；accent/locale 完整权重与冷门字符集仍 Deferred（SYNTAX_MATRIX §6）
- [x] HAVING 常用聚合条件、未关联标量子查询/`IN (SELECT ...)`、按分组外层行绑定的关联标量子查询，以及 `AND` 组合的关联 `EXISTS`；更复杂的关联谓词树与排序规则完整权重——**Deferred**（显式 `ONLY_FULL_GROUP_BY` 与主键/非空唯一键函数依赖已验证；SYNTAX_MATRIX §2/§6）
- [x] 冷门 DDL/DML 兼容表面：`ANALYZE`/`OPTIMIZE`/`CHECK`/`REPAIR`/`CHECKSUM TABLE`（MySQL 形态结果集）、`FLUSH`/`CACHE INDEX`（no-op）、`ALTER DATABASE ... CHARACTER SET/COLLATE/UPGRADE DATA DIRECTORY NAME/READ ONLY/ENCRYPTION`、`RENAME USER`、`SET PASSWORD`、`CREATE/ALTER DATABASE` 选项；`LOAD DATA PARTITION (p0,...)` 已按逻辑分区逐行校验并原子拒绝错分区行，完整冷门字符集与 `sql_mode` warning/error 组合矩阵仍 **Deferred**（SYNTAX_MATRIX §7/§6）
- [x] 触发器、存储过程与函数、事件完整语义（创建/持久化/调用/元数据/错误传播/事务原子性）
- [x] 可更新视图、`WITH CHECK OPTION`：单基表直接列投影、主键可见、LOCAL/CASCADED 检查和 DML 回归已验证；复杂 JOIN/聚合/表达式、视图套视图仍 **Deferred**（SYNTAX_MATRIX §1.2）
- [x] MySQL 系统库：`information_schema`（含 `PARTITIONS`/`APPLICABLE_ROLES` 等虚拟表，TABLES/COLUMNS 自描述）、`mysql`（user/db/role_edges/roles_mapping 真实，global_grants/tables_priv/columns_priv/func 虚拟表，`procs_priv` 动态反映例程授权）、`performance_schema`（12 表）、`sys`（12 视图）全部可查询；权限/角色/审计（全局/库级、表级及列级简单 DML 强制，复杂 JOIN 的每个基表及 JOIN/WHERE/GROUP/HAVING/ORDER 投影列级 SELECT 已回归，JOIN UPDATE/DELETE 的逐目标表/逐读取列权限、`INSERT ... SELECT` 的目标写列/来源读列权限、`ON DUPLICATE KEY UPDATE` 的 INSERT/UPDATE/既有行 SELECT 与 `VALUES(col)` 入参边界已回归，`tables_priv`/`columns_priv`/`TABLE_PRIVILEGES`/`COLUMN_PRIVILEGES` 动态展示；全局/库/表/列/例程授权委托会校验被授予权限与同级或更宽范围的 `GRANT OPTION`，角色委托校验 `ADMIN OPTION`，`GRANT ALL` 后各层部分 `REVOKE` 会移除实际权限，跨库/越权委托及 `REVOKE ADMIN OPTION FOR` 已回归，完整授权撤销矩阵仍 Deferred）；复制协议表面 `SHOW MASTER/BINARY LOG STATUS`、`SHOW BINARY LOGS`、`SHOW REPLICAS`、`SHOW REPLICA STATUS`、本地 XA `START/BEGIN/END/PREPARE/COMMIT/ROLLBACK/RECOVER`（跨连接 prepared 分支、锁保留、one-phase）返回 MySQL 形态结果，真正 binlog 复制拓扑/GTID/分布式 XA 外部协调 **Deferred**（SYNTAX_MATRIX §5/§8/§3）

## 事务、锁与连接

- [x] BEGIN/COMMIT/ROLLBACK、autocommit、DDL 隐式提交、事务内读己写
- [x] SAVEPOINT/ROLLBACK TO/RELEASE、重名覆盖、MySQL 1305、自增回滚留洞；回滚到保存点保留既有行/间隙锁，并释放保存点后基础主键 INSERT 的隐式记录/索引锁
- [x] READ UNCOMMITTED/READ COMMITTED/REPEATABLE READ/SERIALIZABLE 常用可见性；SERIALIZABLE 普通 SELECT 在 autocommit=0 下获取并持有共享记录/范围锁，回归已覆盖
- [x] IS/IX/S/X、主键/唯一键行锁、SELECT FOR UPDATE/FOR SHARE、LOCK IN SHARE MODE、NOWAIT 原子获取与 MySQL 3572、主键队列 ORDER BY/LIMIT SKIP LOCKED、方向性 gap/insert-intention 冲突、wait-for graph 死锁检测、按事务权重且同权优先当前等待者的 1213/40001 及受害者整事务回滚、锁等待超时
- [x] 断线自动回滚并释放锁；服务重启后客户端可重连和继续新事务
- [x] 事务语句级 CHECK/FK 预校验、级联立即可见、回滚不落 WAL
- [x] 主键及单列二级索引的等值/范围锁：资源化 next-key/gap 区间、空隙 INSERT 阻塞、共享 gap 锁阻塞 insert-intention、DECIMAL/大整数索引范围精确数值排序、事务锁等待与回滚回归；自动提交锁定读语句结束释放、`autocommit=0` 保持至 COMMIT、JOIN 最终命中行记录锁、SERIALIZABLE 普通 SELECT 共享锁、UPDATE 二级索引旧/新键锁已回归
- [x] READ COMMITTED 锁定读与 UPDATE/DELETE 只锁命中记录、不锁普通 gap；RR/SERIALIZABLE 保留范围锁；主键 gap 插入回归
- [x] 二级索引插入意向锁：同一非唯一索引值的不同记录可并发插入，但仍受 next-key/gap X 锁阻塞
- [x] 复合索引全等值与左前缀范围记录/间隙锁；单列字符索引范围按列排序规则而非数值解析比较；无主键表使用稳定行内容+重复序号的隐藏行锁支持基础 `SKIP LOCKED`
- [x] 基础 MDL：普通读持有 statement-duration metadata shared，DML 持有兼容的 metadata intention，DDL 通过 metadata X 锁等待并参与超时/死锁路径
- [x] 单机 MySQL TCP 入口：JDBC/Go/Node.js/JetBrains/VS Code/dbx/mysql CLI 直接连接 3306，共用同一协议路径
- [x] MySQL 账户 host 选择基础语义：`'user'@'host'` 精确主机优先于 `%`/`_` 通配和 IPv4 掩码匹配；握手按实际账户选择 `mysql_native_password` / `caching_sha2_password`
- [x] 基础 JOIN `SKIP LOCKED`：先完成 JOIN/WHERE，再按最终组合原子尝试锁定各基表行；锁等待行被跳过，最终 JOIN 过滤掉的基表行不被误锁
- [x] JOIN 锁定读的最终基表行集合统一排序后一次获取，反向 JOIN 顺序不再造成锁获取顺序差异
- [x] JOIN 锁定读的列对列等值/非等值谓词：按最终命中组合为各端点建立索引点/范围锁，反向比较符同步处理；无索引端点使用表级安全回退，并覆盖匹配范围 INSERT 阻塞回归
- [ ] MySQL InnoDB 完整 next-key/gap/意向锁、多方环与基于回滚成本的受害者选择一致性；当前已实现基础范围锁、死锁图与按事务写集+持有锁数量选择 victim，并验证简单重复键 INSERT 的 duplicate-record S、ON DUPLICATE 唯一二级键 X/非唯一二级键 insert intention、非唯一二级索引等值 SELECT/UPDATE 与 JOIN 锁定读的前置 next-key gap、INSERT SELECT 在 RC 与 RR/SERIALIZABLE 下的源表锁差异、外键子写入的新父键共享记录锁、父键变更/删除对匹配子记录和 FK 范围的定向锁；复杂 JOIN/隐式锁边界仍待补齐
- [ ] 全部隔离级别 anomaly、锁升级和大事务边界矩阵；当前本地 XA 已支持跨连接 prepared 分支、RECOVER、持久 catalog 重载、one-phase、锁转移与提交/回滚，并用同一 durable WAL 的 XA commit marker 处理“数据已提交而 catalog 删除前崩溃”的幂等恢复；无主键/复杂 UPSERT/REPLACE 隐式锁边界及更大故障注入矩阵仍待补齐

## 迁移与备份

- [x] 独立 `crates/mydb-migrate` CLI，可从 MySQL80 迁移并保留 NULL/BLOB/时间值
- [x] 真实 MySQL 8.0.45 dump、循环外键 dump 和 hex BLOB 原样导入
- [x] 独立 `crates/mydb-dump` / `mydbdump` CLI
- [x] 一致性全量、LSN 增量、校验、恢复和 PITR HTTP/CLI 链路
- [x] 备份使用组提交边界快照，不锁业务表
- [ ] 大数据量迁移的断点续传、限速、在线增量追平与切流回滚演练
- [ ] 与 mysqldump/mysqlpump/mysqlbinlog 复杂对象及全部选项的兼容矩阵

## Agent HTTP 与运维

- [x] Agent HTTP 默认开启，提供 health、自然语言诊断、slow SQL、锁/WAL/checkpoint 状态
- [x] HTTP 全量/增量备份、PITR 恢复 staging 和重启安装
- [x] 原生 CLI 可访问 Agent API，Prometheus `/metrics` 默认可用
- [x] 管理端口与 SQL 端口分离，支持 bearer/admin 密码
- [ ] 完整生产鉴权、TLS、密钥轮换、权限审计与危险操作审批；当前已落地强密码/TLS 配置校验、HTTP 登录失败限流、管理 API 审计，以及备份删除/恢复的 ID 绑定显式确认；密钥轮换持久化、细粒度管理角色和审批留痕仍待补齐
- [ ] slow SQL 执行计划、索引建议、跨时间段根因分析和告警集成

## 安装、Docker 与平台

- [x] Debian 13 slim 运行镜像；当前 amd64 镜像约 41.7 MB
- [x] Compose 默认 `unless-stopped`，0.5 CPU、512 MiB，开发机端口 `13316/14316`
- [x] Compose 使用可配置独立 `/24` 子网，默认 Docker 地址池耗尽时仍可启动
- [x] Windows Docker Desktop PowerShell 完整 smoke 通过
- [x] Linux Bash 路径完整 smoke 通过，使用官方 `mysql:8.0` CLI
- [x] linux/arm64 镜像曾在 QEMU 下完成完整 smoke
- [x] CI 配置 Windows/Linux/macOS 原生 Rust 编译测试，Ubuntu 24.04 Docker smoke
- [ ] 当前最终提交在真实 macOS Docker Desktop 上完成 smoke
- [ ] Windows 安装脚本、Linux systemd、macOS launchctl 在干净真实机器端到端通过
- [x] Windows/Bash 发布脚本为压缩包生成并上传 SHA-256 sidecar；签名、透明密钥和升级/降级演练仍待完成
- [x] `mydb update` / `mydb-cli update` 基础跨平台在线更新：v0.1.18→v0.1.19 Release 资产下载、SHA-256 校验、五个二进制事务替换、配置/密钥保留、Windows helper、Linux Debian CLI 更新和 Linux 安装脚本注入失败回滚均已验证；v0.1.20 修正 `--check` 通过 Release 页面解析最新 tag 并识别当前版本；v0.1.22 的 Windows 服务更新 helper 在服务运行且 CLI 非管理员时自动请求 UAC；真实 Windows 服务/Linux systemd 切换、签名、升级/降级仍待补齐
- [ ] 发布产物签名、升级/降级和卸载流程验证

## 当前可复现证据

- [x] `cargo test --workspace --locked -- --test-threads=1`：通过（wire 269 个单测、storage 61 个单测、17 个集成测、WAL 18 个单测及其余 workspace 测试）
- [x] `cargo test -p mydb-storage -p mydb-wire --locked`：通过（wire 269 个单测、storage 61 个单测）
- [x] 最新并发回归：持久 row-id、MVCC 读视图、删除历史版本、精确 DECIMAL/大整数索引范围、方向性 gap/insert-intention、基础 MDL 与已有并发写回归通过；并发不同表写入保持 FIFO/WAL 组提交语义
- [x] vendored `opensrv-mysql`：110 项通过，覆盖自定义错误码/SQLSTATE、多结果 SERVER_MORE_RESULTS_EXISTS、握手多结果能力和 Prepared CALL SERVER_PS_OUT_PARAMS 状态位
- [x] `cargo clippy --workspace --all-targets -- -D warnings`：通过
- [x] MySQL 8.0.45/8.0.46 差分：真实 dump、changed-row affected counts/no-op UPSERT insert id、INSERT/REPLACE SET、INSERT VALUES 默认行/表达式/DEFAULT(col)/1364、UPDATE/UPSERT/JOIN DEFAULT、MySQL 8 行/列别名 UPSERT、复杂冲突标量表达式和左到右赋值、CREATE TABLE LIKE、TRUNCATE 隐式提交/自增/FK 1701、LOAD DATA 用户变量/SET/latin1/BLOB/1261/1262/1062 warning/strict 1261/1262/1300 原子失败、FOR SHARE/NOWAIT 3572/主键队列 SKIP LOCKED/双事务死锁 1213、FK/CHECK/事务/SAVEPOINT、JOIN/NATURAL/USING、有键/无键重复行单/多目标 JOIN UPDATE/DELETE、相关/派生/CTE 子查询、set operators、多列 GROUP BY、窗口、多列/表达式 ORDER BY、常用 CASE/字符串/数值/CAST 投影/WHERE/UPDATE/DELETE
- [x] 本轮 SQL 回归：JOIN ON 常量/算术/常用标量函数；分组 HAVING 未关联标量子查询及 `IN (SELECT ...)`；锁定读非索引 JOIN 表达式安全回退
- [x] `scripts/docker-smoke.ps1`：通过，含 changed-row affected counts/no-op WAL avoidance、INSERT/REPLACE SET、INSERT VALUES 默认行/表达式/1364、UPDATE/UPSERT/JOIN DEFAULT、MySQL 8 行/列别名 UPSERT、复杂冲突标量表达式/左到右赋值、SIGKILL committed/uncommitted 恢复、WAL 坏尾精确截断、CREATE TABLE LIKE、TRUNCATE 自增/FK、双连接 FOR SHARE/NOWAIT/SKIP LOCKED/死锁受害者回滚、真实 `LOAD DATA LOCAL INFILE` 协议、字符集/warning/strict error 诊断、语句原子性及 `secure_file_priv` 边界
- [x] `NO_BUILD=1 bash scripts/docker-smoke.sh`：通过（当前脚本与 PowerShell 同覆盖）
- [x] Windows Docker Desktop Ubuntu 24.04 开发基准门禁：20 秒预算、1 轮、同为 `ENGINE=InnoDB`、20 MB/s/500 IOPS、fsync-on-commit；2026-07-20 最新原始样本 `target/io-bench-desktop-header-check/` 为 MyDB 2262.9 ops/s、MySQL 8.0.46 3318.4 ops/s、0.682x，MyDB 写 P99 40.5 ms、MySQL 495.9 ms。仅证明限速工具链与回归数据，不作为正式性能结论
- [x] 2026-07-20 8 表/4 CPU 限速单轮：`target/io-bench-current-multitable-windowed/`，MyDB 1802.7 ops/s、MySQL 3417.4 ops/s、0.527x；WAL 305 次 fsync 覆盖 821 请求（2.69 请求/组），比无窗口专用写线程的 2.17 请求/组提升。单轮仅作回归证据，不作为正式性能结论
- [x] 2026-07-20 8 表/4 CPU 限速 3 轮：`target/io-bench-multitable-async-audit-3r/`，MyDB 7017.2 ops/s、MySQL 7315.1 ops/s、0.959x；WAL 269 次 fsync 覆盖 1641 请求（6.10 请求/组）。异步批量审计移出 SQL 临界路径；读主导样本 `target/io-bench-read-async-audit/` 读 P50 为 210 us。开发机 Docker 回归证据，不代表物理生产硬件验收
- [x] db233-go `go test -count=1 ./...`：通过且仓库无改动
- [x] 默认 MyDB 容器：healthy、`unless-stopped`、0.5 CPU、512 MiB
- [x] MySQL Connector/J 9.1.0：当前 release 服务 3306 4/4 通过，含无默认库、认证、CRUD、UTF-8、DataGrip/IDEA 常用连接属性，以及 typed prepared metadata/DATE/BLOB binary row 回归；覆盖 JDBC 路径
- [x] Go `database/sql` `github.com/go-sql-driver/mysql` v1.10.0：当前 release 服务 3306 回归通过 Ping、建库/建表、中文、DATE、普通查询与预处理查询
- [x] MySQL 8.4 真实差分基线：`scripts/mysql84-diff.ps1` 同机 Docker MySQL 8.4（Windows `lower_case_table_names=1` 基线）与隔离 MyDB 逐结果集比较，上一阶段共 117 项通过；JSON 成本与 InnoDB 物理估算字段按语义/结构校验，不硬比引擎估算值
- [x] Node.js `mysql2` v3.23.3：当前 release 服务 3306 通过普通查询与预处理查询；覆盖 VS Code JavaScript/TypeScript 连接路径
- [x] 2026-08-15 REGEXP 差分阶段：`REGEXP_INSTR`、`REGEXP_SUBSTR`、`REGEXP_REPLACE` 的位置/occurrence/return_option/NULL/Unicode 与 MySQL 8.4 对齐，该阶段总差分 118/118
- [x] 2026-08-15 JSON 聚合/路径差分阶段：`JSON_ARRAYAGG`/`JSON_OBJECTAGG` SQL NULL、JSON 值、空集合、重复 key 覆盖及窗口累计聚合，JSON `.*`/`[*]`/`**`、数组范围、`last` 动态下标与基础/按前置表行隐式关联的 `JSON_TABLE`（标量、嵌套、序号、存在性、默认/错误行为）与 MySQL 8.4 对齐；该阶段 `scripts/mysql84-diff.ps1` 为 117/117 cases
- [x] Windows 当前构建：`MyDBServer` Automatic 服务停止/启动循环通过；监听 `0.0.0.0:3306`，LAN 地址连接成功，防火墙入站规则启用
- [x] Windows 物理服务现状：`MyDBServer` Automatic、Running；3306 返回 `8.4.0-mydb-0.1.0` 与 `event_scheduler=ON`；本轮最新 release 已在隔离 13306 完成同等差分
- [ ] Windows 物理服务已切换到本轮最新 release：当前 shell 无法重启 LocalSystem `MyDBServer`，3306/4306 仍保持稳定开发实例；本轮最新源码已在隔离 13306 完成验证，待有权限窗口切换 3306
- [x] 本机全量切流：9 个业务库迁移并重启校验；`sakila.staff` 超大 BLOB 通过 16KB 页外溢存储保留；MySQL80 服务、程序、进程和数据目录已卸载清理，SQL 备份保留在 `C:\Server\mydb\mysql-backup-20260811\all-databases.sql`
- [x] 2026-08-15 阶段性源码真实差分：同机 Docker `mysql:8.4` 与隔离 MyDB 对比，117/117 cases 通过；新增基础 JSON_TABLE 标量/嵌套/序号/存在性/默认与错误行为及按前置表行隐式关联 JSON_TABLE、POINT/LINESTRING/POLYGON/MULTI*/GEOMETRYCOLLECTION 空间构造器/度量/访问器/SRID 轴序/谓词，另含 JSON_SEARCH、JSON 浅层/递归通配路径、数组范围/`last` 下标、JSON_ARRAYAGG/JSON_OBJECTAGG 聚合与窗口、数组追加/插入、RFC 7396 合并、深度/键/美化、重叠、BIT 聚合、常量聚合投影、全文 TF-IDF 基础评分/布尔前缀/短语/查询扩展、停止词/短词边界、未知线程 KILL 错误、角色授权、ai_ci、EXPLAIN、状态接口、生成列 INSERT/UPDATE/UPSERT/INSERT SELECT 显式写入错误码/消息；物理 3306 服务未强制替换
- [x] 2026-08-15 同机持久化基准（v0.1.21 发布候选，提交 7bdcb65）：MyDB 与 MySQL 8.4.11 使用相同 Docker 资源和持久化设置；性能阶段无预热、17.6/60 秒、1 次采样，单表写 582/276 ops/s、4 actor P99 10.1/92.9 ms、并发吞吐 343/153 ops/s、读 P50 290/132 μs；原始结果见 `性能报告.md`，不硬编码历史比值
- [x] 2026-08-15 v0.1.23 索引点查优化后同机持久化基准：MyDB 与 MySQL 8.4.11 使用相同 Docker 资源和持久化设置；性能阶段无预热、22.3/60 秒、1 次采样，单表写 218/78 ops/s、4 actor P99 28.2/58.8 ms、并发吞吐 251/155 ops/s、读 P50 400/130 μs；原始结果见 `性能报告.md`，不硬编码历史比值
- [x] 2026-08-15 v0.1.24 多列 `COUNT(DISTINCT ...)` 修复后：Rust workspace 测试、clippy、`scripts/mysql84-diff.ps1` 119/119 与完整 `scripts/docker-smoke.ps1` 均通过；Docker 使用隔离 13316/14316 端口，未触碰物理 3306/4306
- [x] 2026-08-15 v0.1.24 同条件持久化基准：MyDB 与 MySQL 8.4.11 使用相同 Docker 资源和持久化设置；性能阶段无预热、22.1/60 秒、1 次采样，单表写 203/78 ops/s、4 actor P99 28.7/57.3 ms、并发吞吐 330/144 ops/s、读 P50 403/132 μs；原始结果见 `性能报告.md`，不硬编码历史比值
- [x] 2026-08-14 Windows 隔离安装回归：管理员本地发布包安装 exit 0，生成 ACL 受限 root/admin 强密钥并创建配置/数据目录；新包仅含 server/cli/migrate/dump，不含 mydb-router
- [x] 2026-08-14 安装包完整性回归：Linux 容器本地 tar.gz 与 Windows 本地 zip 均完成 `.sha256` 校验、强密钥配置和无 router 文件检查；远程安装路径强制下载 sidecar
- [ ] 正式 Ubuntu 24.04 物理 linux/amd64 性能结果稳定达到目标；当前证据不足
