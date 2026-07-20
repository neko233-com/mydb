# MyDB 落地验收清单

> 规则：只有当前源码和可复现实测能证明的项目才打勾。宽泛目标不能由局部 smoke 代替。最后更新：2026-07-17。

## 最终发布门槛

- [ ] MySQL 8 单机全部 DML、事务、错误码和可见外部行为完成兼容矩阵并逐项通过差分
- [ ] 稳定性、崩溃恢复、断线重连和故障注入覆盖生产边界
- [ ] Ubuntu 24.04、linux/amd64、双方相同 `ENGINE=InnoDB`、I/O/CPU/内存同限、总计不超过 60 秒的正式性能验收完成
- [ ] 写吞吐和延迟稳定达到 10x；若客观无法达到，保留原始数据并明确实际结果
- [ ] Windows、Linux、macOS 的原生安装及 Docker 功能均在真实平台通过
- [ ] 全量迁移、校验、切流和回滚演练完成
- [ ] 上述门槛全部通过后再停止并卸载本机 MySQL80

## 内核与存储

- [x] `InnoDB` 映射到自研持久化 Neko233 引擎，`MEMORY` 保持独立非事务语义
- [x] 单写 Actor FIFO、事务批次、group commit、CRC WAL、断尾截断、checkpoint 与恢复
- [x] 同 Actor/主键顺序写、并发计数更新和 UPSERT 不丢写
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

- [x] MySQL text/binary prepared wire protocol，可用官方 MySQL 8 CLI 和 db233-go 原 SQL 连接
- [x] 常用数据库/表/列/索引 DDL、schema-qualified DML、真实 MySQL 8 dump 导入
- [x] `CREATE TABLE [IF NOT EXISTS] ... LIKE ...`：跨 schema 复制列、默认值、自增属性、主键/唯一/普通索引、CHECK 和引擎；不复制行、外键及当前自增计数
- [x] INSERT/REPLACE/INSERT IGNORE/ON DUPLICATE KEY UPDATE/AUTO_INCREMENT；支持 MySQL `INSERT/REPLACE ... SET col=expr`、`VALUES(DEFAULT, scalar_expr)`、`DEFAULT(col)`、`INSERT ... () VALUES ()`/`VALUES ()` 默认行，以及普通/无主键/JOIN UPDATE 和 UPSERT 的 `col=DEFAULT/DEFAULT(col)`；覆盖事务回滚、1364、affected rows 及冲突尝试自增留洞
- [x] MySQL 8.0.19+ `INSERT ... VALUES/SET ... AS new [(alias,...)] ON DUPLICATE KEY UPDATE`：支持 `new.col`、`new.alias`、无限定列别名、表限定旧行值，以及 IF/CASE/CONCAT/COALESCE/GREATEST/LEAST/组合算术等常用冲突标量表达式；赋值严格左到右，覆盖事务回滚、changed-row affected rows 及自增留洞
- [x] UPDATE/UPSERT affected rows 按 MySQL 默认 changed rows 计算：literal/scalar/expression/JOIN/无主键/事务路径覆盖；no-op 在排他锁后判定，不写 WAL、不 fsync、不重写表，duplicate UPSERT 不返回伪 insert id
- [x] INSERT/REPLACE ... SELECT，含 IGNORE/ON DUPLICATE、源表共享锁与事务回滚
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
- [x] JOIN ON 列对列等值/非等值/NULL-safe、括号 AND/OR、多列 USING
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
- [x] 游戏 profile 常用 JSON CRUD：JSON_OBJECT/ARRAY/VALID/TYPE/LENGTH/CONTAINS/SET/REMOVE，覆盖 INSERT/SELECT/WHERE/UPDATE/UPSERT 和事务回滚
- [x] 持久化只读 VIEW：CREATE/CREATE OR REPLACE/DROP/SHOW CREATE/SHOW FULL TABLES；显式列名、普通/JOIN/聚合视图、外层过滤、DDL 隐式提交、重启恢复和禁止写视图
- [x] CREATE TABLE [IF NOT EXISTS] ... AS SELECT：普通/JOIN/聚合/视图来源、结果列类型推导、DDL 隐式提交、建表与首批数据同一 Actor/WAL 原子组、重启恢复
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
- [x] information_schema 只读虚拟表：SCHEMATA/TABLES/COLUMNS/STATISTICS/TABLE_CONSTRAINTS/KEY_COLUMN_USAGE/CHECK_CONSTRAINTS，多行投影、过滤、排序、分组和跨表 JOIN
- [x] mydbdump/mydb-migrate/ORM 风格元数据查询：表/列枚举、COALESCE 引擎、复合索引 GROUP_CONCAT、PK/UNIQUE/FK/CHECK 和临时物理名隐藏
- [x] REFERENTIAL_CONSTRAINTS 与 VIEWS：引用唯一键、UPDATE/DELETE 规则、目标表、视图定义/安全类型/只读状态
- [x] ROUTINES/PARAMETERS 真实存储过程元数据；EVENTS 保持结构化空表，未实现事件时 ORM 探测返回 0 行
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
- [ ] Trigger/Procedure 局部变量完整字符集/排序规则、时区及其余 sql_mode warning；主 condition 的 MySQL 非保证排序差分、所有冷门语句清理细节、routine 权限及 handler 所有边缘条件；FUNCTION/EVENT 对象及完整边缘行为
- [x] 常用 CHECK、命名/复合外键 RESTRICT/CASCADE/SET NULL 及 MySQL 错误码
- [x] 单目标及多目标 UPDATE ... JOIN；DELETE alias-list FROM ... JOIN 与 DELETE FROM alias-list USING ...，主键级锁定物化、事务/WAL/约束路径一致
- [x] 无主键 JOIN UPDATE/DELETE：表锁内使用物理行序号区分完全重复行，顺序表达式观察前序目标变更，整表镜像 WAL 幂等重放
- [x] `LOAD DATA LOCAL INFILE` MySQL 文件传输协议及安全目录内服务端 `INFILE`；字段/行分隔、包围、转义、IGNORE 行、列/用户变量映射、SET 表达式转换、常用字符集转码、BLOB 原字节、1261/1262/1062 warning、strict 1261/1262/1300 原子失败、事务回滚与 affected rows
- [x] 有主键单表的 CASE/函数化 UPDATE SET/WHERE 与 DELETE WHERE，含左到右赋值、ORDER/LIMIT 和事务回滚
- [ ] 复杂互递归 CTE、CYCLE 语义
- [ ] 完整表达式/函数/类型转换/字符集/排序规则/时区语义
- [ ] 完整复杂 HAVING 子查询、排序规则与 ONLY_FULL_GROUP_BY 函数依赖规则
- [ ] `LOAD DATA` PARTITION、MySQL 全部冷门字符集/排序规则及完整 sql_mode warning/error 组合矩阵、完整 DDL/DML 边缘语法
- [ ] 可更新视图、WITH CHECK OPTION 完整约束、触发器、存储过程/函数和事件完整语义
- [ ] MySQL 系统库、权限、角色、审计、复制协议完整语义

## 事务、锁与连接

- [x] BEGIN/COMMIT/ROLLBACK、autocommit、DDL 隐式提交、事务内读己写
- [x] SAVEPOINT/ROLLBACK TO/RELEASE、重名覆盖、MySQL 1305、自增回滚留洞
- [x] READ UNCOMMITTED/READ COMMITTED/REPEATABLE READ/SERIALIZABLE 常用可见性
- [x] IS/IX/S/X、主键/唯一键行锁、SELECT FOR UPDATE/FOR SHARE、LOCK IN SHARE MODE、NOWAIT 原子获取与 MySQL 3572、主键队列 ORDER BY/LIMIT SKIP LOCKED、wait-for graph 死锁检测、1213/40001 及受害者整事务回滚、锁等待超时
- [x] 断线自动回滚并释放锁；服务重启后客户端可重连和继续新事务
- [x] 事务语句级 CHECK/FK 预校验、级联立即可见、回滚不落 WAL
- [ ] MySQL InnoDB 完整 next-key/gap/意向锁、无主键/复杂 JOIN 的逐行 SKIP LOCKED、多方环与基于回滚成本的受害者选择一致性
- [ ] 全部隔离级别 anomaly、XA、SAVEPOINT 后锁精确释放、锁升级和大事务边界矩阵

## 迁移与备份

- [x] 独立 `crates/mydb-migrate` CLI，可从 MySQL80 迁移并保留 NULL/BLOB/时间值
- [x] 真实 MySQL 8.0.45 dump、循环外键 dump 和 hex BLOB 原样导入
- [x] 独立 `crates/mydb-dump` / `mydbdump` CLI
- [x] 一致性全量、LSN 增量、校验、恢复和 PITR HTTP/CLI 链路
- [x] 备份使用 Actor 边界快照，不锁业务表
- [ ] 大数据量迁移的断点续传、限速、在线增量追平与切流回滚演练
- [ ] 与 mysqldump/mysqlpump/mysqlbinlog 复杂对象及全部选项的兼容矩阵

## Agent HTTP 与运维

- [x] Agent HTTP 默认开启，提供 health、自然语言诊断、slow SQL、锁/WAL/checkpoint 状态
- [x] HTTP 全量/增量备份、PITR 恢复 staging 和重启安装
- [x] 原生 CLI 可访问 Agent API，Prometheus `/metrics` 默认可用
- [x] 管理端口与 SQL 端口分离，支持 bearer/admin 密码
- [ ] 完整生产鉴权、TLS、密钥轮换、权限审计与危险操作审批
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
- [ ] 发布产物签名、校验和、升级/降级和卸载流程验证

## 当前可复现证据

- [x] `cargo test --workspace`：204 项通过
- [x] `cargo test -p mydb-wire`：133 项通过（含 IANA 命名时区、大小写名称、上海/纽约、DST 跳时/回拨、连接隔离、动态默认值、事务及函数比较投影，会话 time_zone 固定偏移/SYSTEM、连接隔离、SET 左到右、NOW/SYSDATE、UNIX 微秒往返、动态默认值、ON UPDATE、事务，CONVERT_TZ 固定偏移、UTC/GMT/SYSTEM、跨日、微秒、无效时区、WHERE/UPDATE/事务，NOW/CURRENT_TIMESTAMP/local/UTC/UNIX 的语句开始快照、SYSDATE 调用时刻、跨 SLEEP 与批量 UPDATE 一致性，UTC_DATE/UTC_TIME/UTC_TIMESTAMP、LOCALTIME/LOCALTIMESTAMP、CURTIME/CURRENT_TIME 的 UTC/local、fsp、DML 和事务，ADDDATE/SUBDATE/TIMESTAMP/TIMESTAMPADD 的别名、天数简写、SQL_TSI_、月末、微秒和排期 DML/事务，GET_FORMAT/STR_TO_DATE/TIME_FORMAT 的官方格式、月名/微秒差异、文本导入 DML/事务，EXTRACT 基础/复合单位、函数内 FROM 顶层解析、事件分区 DML/事务，TO_DAYS/FROM_DAYS/TO_SECONDS 与 PERIOD_ADD/PERIOD_DIFF 的 year-0 日序、紧凑数字日期、归档/赛季 DML/事务，WEEK/WEEKOFYEAR/YEARWEEK 的 0–7 模式、ISO 跨年、注册周 cohort DML/事务，ADDTIME/SUBTIME/MAKETIME 的跨日 DATETIME、负时长、微秒、游戏冷却 DML/事务，TIME/MICROSECOND/TIME_TO_SEC/SEC_TO_TIME/TIMEDIFF 的负时长、跨天、微秒、DATETIME 差值及 DML/事务，DAYOFYEAR/WEEKDAY/QUARTER/DAYNAME/MONTHNAME/LAST_DAY/MAKEDATE 的闰年、跨年、月末结算 DML/事务，CONV/BIT_COUNT 的 64 位进制、显式二进制位计数、权限掩码 DML/事务，PI/角度/三角函数的定义域、游戏向量 DML/事务，MD5/SHA/SHA1/SHA2/CRC32 的文本/二进制迁移摘要、DML/事务和 CHECK 关键字边界，UUID v1/二进制 swap/校验、IPv4/IPv6 二进制往返与 DML/事务，BIN/OCT/HEX/UNHEX/Base64/FORMAT 的迁移编码、换行/空白、locale、DML/事务回滚，字符串工具函数 UTF-8/二进制/DML/64MiB 内存边界、FIND_IN_SET/FIELD/ELT/MAKE_SET/EXPORT_SET 的 SELECT/WHERE/UPDATE/回滚、存储程序 TIME/DATETIME/TIMESTAMP FSP 舍入/截断/进位与调用者-例程 SQL_MODE 边界、Trigger/Procedure/参数 ENUM/SET 成员与数字索引/位掩码转换、PROCEDURE CREATED/LAST_ALTERED/SQL_MODE 快照与恢复、diagnostics 多 condition/max_error_count/sql_notes、连接顶层 GET DIAGNOSTICS 真实驱动与 prepared 1295、Prepared CALL OUT/INOUT 声明类型 Binary Wire、PROCEDURE/CALL 多结果集/游标/condition handler/diagnostics/ALTER characteristics、Trigger 复合控制流与常用局部变量类型转换、连接级临时表、SQL_CALC_FOUND_ROWS、会话写后状态、日期/自动更新时间、用户/系统变量、协议/SQL prepared、注册留存、DAU、收入、数学、文本、JSON CRUD、视图及 ALTER 演进重启）
- [x] `cargo test -p mydb-storage -p mydb-wire`：173 项通过（23 个 storage 单元测试、17 个 storage 集成测试、133 个 wire 测试）
- [x] 最新并发回归：`cargo test -p mydb-storage` 32 个单元测试、17 个集成测试通过；`cargo test -p mydb-wire` 140 项通过；并发不同表 INSERT/UPDATE/UPSERT 合并为一个 WAL fsync，物理 apply 保持 Actor 顺序；真实 Actor group 重启后双表数据完整且无重复
- [x] vendored `opensrv-mysql`：110 项通过，覆盖自定义错误码/SQLSTATE、多结果 SERVER_MORE_RESULTS_EXISTS、握手多结果能力和 Prepared CALL SERVER_PS_OUT_PARAMS 状态位
- [x] `cargo clippy --workspace --all-targets -- -D warnings`：通过
- [x] MySQL 8.0.45/8.0.46 差分：真实 dump、changed-row affected counts/no-op UPSERT insert id、INSERT/REPLACE SET、INSERT VALUES 默认行/表达式/DEFAULT(col)/1364、UPDATE/UPSERT/JOIN DEFAULT、MySQL 8 行/列别名 UPSERT、复杂冲突标量表达式和左到右赋值、CREATE TABLE LIKE、TRUNCATE 隐式提交/自增/FK 1701、LOAD DATA 用户变量/SET/latin1/BLOB/1261/1262/1062 warning/strict 1261/1262/1300 原子失败、FOR SHARE/NOWAIT 3572/主键队列 SKIP LOCKED/双事务死锁 1213、FK/CHECK/事务/SAVEPOINT、JOIN/NATURAL/USING、有键/无键重复行单/多目标 JOIN UPDATE/DELETE、相关/派生/CTE 子查询、set operators、多列 GROUP BY、窗口、多列/表达式 ORDER BY、常用 CASE/字符串/数值/CAST 投影/WHERE/UPDATE/DELETE
- [x] `scripts/docker-smoke.ps1`：通过，含 changed-row affected counts/no-op WAL avoidance、INSERT/REPLACE SET、INSERT VALUES 默认行/表达式/1364、UPDATE/UPSERT/JOIN DEFAULT、MySQL 8 行/列别名 UPSERT、复杂冲突标量表达式/左到右赋值、SIGKILL committed/uncommitted 恢复、WAL 坏尾精确截断、CREATE TABLE LIKE、TRUNCATE 自增/FK、双连接 FOR SHARE/NOWAIT/SKIP LOCKED/死锁受害者回滚、真实 `LOAD DATA LOCAL INFILE` 协议、字符集/warning/strict error 诊断、语句原子性及 `secure_file_priv` 边界
- [x] `NO_BUILD=1 bash scripts/docker-smoke.sh`：通过（当前脚本与 PowerShell 同覆盖）
- [x] Windows Docker Desktop Ubuntu 24.04 开发基准门禁：20 秒预算、1 轮、同为 `ENGINE=InnoDB`、20 MB/s/500 IOPS、fsync-on-commit；2026-07-20 最新原始样本 `target/io-bench-desktop-header-check/` 为 MyDB 2262.9 ops/s、MySQL 8.0.46 3318.4 ops/s、0.682x，MyDB 写 P99 40.5 ms、MySQL 495.9 ms。仅证明限速工具链与回归数据，不作为正式性能结论
- [x] 2026-07-20 8 表/4 CPU 限速单轮：`target/io-bench-current-multitable-windowed/`，MyDB 1802.7 ops/s、MySQL 3417.4 ops/s、0.527x；WAL 305 次 fsync 覆盖 821 请求（2.69 请求/组），比无窗口专用 Actor 的 2.17 请求/组提升。单轮仅作回归证据，不作为正式性能结论
- [x] 2026-07-20 8 表/4 CPU 限速 3 轮：`target/io-bench-multitable-async-audit-3r/`，MyDB 7017.2 ops/s、MySQL 7315.1 ops/s、0.959x；WAL 269 次 fsync 覆盖 1641 请求（6.10 请求/组）。异步批量审计移出 SQL 临界路径；读主导样本 `target/io-bench-read-async-audit/` 读 P50 为 210 us。开发机 Docker 回归证据，不代表物理生产硬件验收
- [x] db233-go `go test -count=1 ./...`：通过且仓库无改动
- [x] 默认 MyDB 容器：healthy、`unless-stopped`、0.5 CPU、512 MiB
- [x] 本机 MySQL80：仍为 Running/Automatic，符合“最终验收前不得卸载”
- [ ] 正式 Ubuntu 24.04 物理 linux/amd64 性能结果稳定达到目标；当前证据不足
