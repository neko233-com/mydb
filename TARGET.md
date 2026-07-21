# MyDB 目标与规划

## 核心定位

**替代 SQLite 的单机高性能数据库，但以 MySQL 的形式与语法暴露接口，追求超高性能。**

- 对业务：直接用 MySQL 协议、客户端、DDL/DML 与事务，无需改写 SQL；可作为 SQLite 的高性能、可联网替代。
- 对内：Rust 实现的 Neko233 引擎（Actor 顺序写、group commit、WAL、copy-on-write），目标在同资源、同持久化级别下显著高于 MySQL/SQLite。
- **非单机能力明确不支持（设计决定）**：binlog 复制拓扑 / GTID、读写分离、Group Replication / Galera、分布式 XA 两阶段协调、跨节点一致性。单机版本地 XA 与复制 SHOW 表面作为兼容 no-op 保留。
- 专为游戏行业写多读少、低交互延迟、低资源常驻场景优化。

执行状态以 [`CheckList.md`](CheckList.md) 为准；目标描述不等于已经实现。非单机能力不在 CheckList 范围内。

---

## 存储引擎

### Memory 引擎
- 纯内存共享表，用于临时状态和缓存
- 支持哈希索引
- 与 MySQL 一致为非事务引擎：`ROLLBACK` 不撤销已执行写入
- 表结构持久化；服务重启后行数据清空，WAL redo 不复活
- 适用于：玩家会话数据、临时计算结果

### InnoDB 引擎
- 完整事务支持（ACID）
- MVCC 多版本并发控制
- 行级锁、Gap Lock
- Buffer Pool 管理
- WAL（Write-Ahead Logging）
- 适用于：玩家档案、装备数据、交易记录

---

## 部署与运维

### 一键安装
- Linux/macOS: `curl | bash` 一键安装
- Windows: `irm | iex` PowerShell 一键安装
- 支持 `server` / `cli` / `all` 组件选择
- 自动检测架构（x86_64 / aarch64）

### 一键升级
- `mydb-server --upgrade` 自动升级
- 滚动更新，零停机
- 版本回退支持
- 数据兼容性保证

### 系统服务
- Linux: systemd 服务
- macOS: launchctl 服务
- Windows: Windows Service
- 开机自启动
- 自动重启

---

## HTTP 管理 API

### 概述
内置 HTTP 管理接口，用于内网运维操作。所有请求需要携带管理员密码。

### 默认配置
- 端口: `4306`（HTTP 管理端口）
- MySQL 端口: `3306`（与 MySQL 相同）
- 默认账号: `root`
- 默认密码: `root`
- 权限: 全权限
- 仅监听内网: `127.0.0.1`

### API 端点

#### 认证
所有请求需携带 `Authorization` 头：
```
Authorization: Bearer root
```

#### 状态查询
```
GET /api/v1/status
Response: { "version": "0.1.0", "uptime": 12345, "connections": 10 }
```

#### 配置管理
```
GET /api/v1/config              # 获取当前配置
PUT /api/v1/config              # 热重载配置
POST /api/v1/config/reload      # 重新加载配置文件
```

#### 备份管理
```
POST /api/v1/backup/full        # 全量备份
POST /api/v1/backup/incremental # 增量备份
GET  /api/v1/backup/list        # 查看备份列表
POST /api/v1/backup/restore     # 恢复备份
DELETE /api/v1/backup/:id       # 删除备份
```

#### 内存管理
```
GET  /api/v1/memory/stats       # 内存使用统计
POST /api/v1/memory/flush       # 刷新 Buffer Pool
```

#### 连接管理
```
GET  /api/v1/connections        # 查看活跃连接
POST /api/v1/connections/kill   # 终止连接
```

### 安全注意
- 仅限内网使用，不要暴露到公网
- 生产环境务必修改默认密码
- 建议配合防火墙限制访问

---

## Agent CLI

### 原生支持
- 内置 AI Agent 接口
- 支持自然语言查询
- 自动优化建议
- 慢查询分析

### 命令行工具
```
mydb-cli
├── 连接管理
├── 数据库操作
├── 性能监控
├── 备份恢复
└── Agent 交互
```

---

## 备份与恢复

### 增量备份
- 基于 LSN 的增量备份（已支持 HTTP full→incremental 连续链）
- 支持时间点恢复（RFC3339 → MDG2 提交时间 → 增量段目标 LSN）
- 备份到本地/远程存储
- 自动备份调度

### 工具命令
```bash
# 全量备份
mydb-backup --full --output /backup/full

# 增量备份
mydb-backup --incremental --output /backup/incr

# 恢复到指定时间点
mydb-restore --point-in-time "2024-01-01 12:00:00"

# 查看备份列表
mydb-backup --list
```

### HTTP 备份
```bash
# 通过 HTTP API 触发备份
curl -X POST http://127.0.0.1:9036/api/v1/backup/full \
  -H "Authorization: Bearer root"

# 查看备份列表
curl http://127.0.0.1:9036/api/v1/backup/list \
  -H "Authorization: Bearer root"
```

---

## 内存管理

### 默认限制
- 默认内存上限: **1GB**
- 可通过配置调整
- 支持热重载

### 配置项
```yaml
memory:
  # 内存上限
  max_memory: "1G"
  # Buffer Pool 大小（不超过 max_memory 的 80%）
  buffer_pool_size: "800M"
  # 查询缓存
  query_cache_size: "64M"
  # 排序缓冲
  sort_buffer_size: "4M"
```

### 监控
- 实时内存使用率
- Buffer Pool 命中率
- 内存分配统计

---

## 性能目标

| 指标 | MySQL 8.0 | MyDB 目标 |
|------|-----------|-----------|
| 游戏 actor 写吞吐 | 1.0x | >= 10.0x |
| 写事务 P99 | 1.0x | <= 0.1x |
| 内存效率 | 基准 | 提升 30% |
| 启动时间 | ~5s | ~1s |

性能验收只比较双方完全相同的 `ENGINE=InnoDB` 业务 DDL/DML，不约束 MyDB 内部实现。固定 Docker Ubuntu 24.04、linux/amd64；两个数据库使用独立同规格 ext4 loopback 盘和相同 BPS/IOPS、CPU、内存限制；预热与正式采样合计最多 60 秒，交替多轮并保留原始样本、中位数和 p50/p95/p99。当前尚未稳定达到 10x，不能据此卸载 MySQL80。

---

## 兼容性

### 协议兼容
- MySQL 8.x Wire Protocol
- 支持标准 mysql 客户端连接
- 支持所有主流编程语言驱动

### 语法兼容
- DDL: CREATE, ALTER, DROP
- DML: SELECT DISTINCT，任意表数链式 INNER/LEFT/RIGHT JOIN（列对列等值/非等值、AND/OR、NULL-safe、多列 USING）、CROSS/NATURAL JOIN、JOIN 聚合/多列 GROUP BY/HAVING、非相关与复合布尔相关 IN/NOT IN/标量/EXISTS 子查询、嵌套/聚合/JOIN 派生表、非递归与常用递归 CTE、UNION/INTERSECT/EXCEPT DISTINCT/ALL、排名/导航/分布/聚合窗口函数，多列/表达式 ORDER BY 与 LIMIT OFFSET，常用 CASE/字符串/数值/CAST/JSON_EXTRACT/JSON_UNQUOTE，ASCII/ORD/长度/截取/插入/QUOTE、BIN/OCT/HEX/UNHEX/Base64/FORMAT、MD5/SHA/SHA1/SHA2/CRC32 摘要校验、UUID 二进制转换、IPv4/IPv6 网络地址函数、CONV/BIT_COUNT 进制位掩码、PI/角度/三角函数、NOW/CURRENT_TIMESTAMP/LOCALTIME/LOCALTIMESTAMP/CURDATE/CURTIME/CURRENT_TIME/SYSDATE 与 UTC_DATE/UTC_TIME/UTC_TIMESTAMP 当前时间函数（区分语句快照和调用时刻）、连接级 SYSTEM/固定偏移/IANA time_zone 语义、CONVERT_TZ 固定偏移/UTC/GMT/SYSTEM/IANA/DST 时区转换、TIME/MICROSECOND/TIME_TO_SEC/SEC_TO_TIME/TIMEDIFF/ADDTIME/SUBTIME/MAKETIME 时长函数、DATE_ADD/DATE_SUB/ADDDATE/SUBDATE/TIMESTAMP/TIMESTAMPADD 日期算术、DATE_FORMAT/GET_FORMAT/STR_TO_DATE/TIME_FORMAT 格式往返、WEEK/WEEKOFYEAR/YEARWEEK 周期函数、TO_DAYS/FROM_DAYS/TO_SECONDS 日序函数、PERIOD_ADD/PERIOD_DIFF 月周期函数、EXTRACT 基础与复合时间单位及 DAYOFYEAR/WEEKDAY/QUARTER/DAYNAME/MONTHNAME/LAST_DAY/MAKEDATE 日历函数，以及 FIND_IN_SET/FIELD/ELT/MAKE_SET/EXPORT_SET 标签权限函数，INSERT/REPLACE SELECT、UPDATE、DELETE、单目标 JOIN UPDATE/DELETE，有主键单表的函数化 UPDATE/DELETE 与 ORDER BY LIMIT；常用复合外键 RESTRICT/CASCADE/SET NULL 与 CHECK 约束，事务语句级预校验与级联读己写
- 事务: BEGIN, COMMIT, ROLLBACK, SAVEPOINT, ROLLBACK TO, RELEASE SAVEPOINT
- 存储过程与诊断区：CREATE/DROP/SHOW/CALL、IN/OUT/INOUT、复合控制流、事务原子性、SELECT INTO、只读单向游标、DECLARE CONDITION、简单/复合 CONTINUE/EXIT handler、NOT FOUND/SQLWARNING/SQLEXCEPTION、条件优先级、RESIGNAL condition/SET、Procedure 内 CURRENT/STACKED GET DIAGNOSTICS、连接顶层 CURRENT diagnostics area、max_error_count/sql_notes/真实 warning-error 总数、CREATE/ALTER characteristics、CREATED/LAST_ALTERED 与创建/修改时 SQL_MODE 持久化执行快照、Trigger/Procedure/参数 ENUM/SET 成员归一化与数字索引/位掩码转换、TIME/DATETIME/TIMESTAMP FSP 默认舍入和 TIME_TRUNCATE_FRACTIONAL 截断、普通 SELECT/嵌套 CALL 顺序多结果集和最终状态结果，以及 prepared CALL OUT/INOUT 单行结果、SERVER_PS_OUT_PARAMS 和常用声明类型 Binary Wire；主 condition 非保证排序差分、冷门语句清理边缘与 routine 权限仍在补齐

### 配置兼容
- YAML 格式配置
- MySQL 参数兼容映射
- 环境变量支持

---

## 开发阶段

### Phase 1: 核心功能
- [x] MySQL 协议兼容层
- [x] Memory 引擎（共享、非事务、重启清空、redo 不复活）
- [x] InnoDB 引擎基础
- [x] 事务管理（批事务 WAL、redo、RU 跨连接脏读、RC/RR/Serializable）
- [x] 锁管理（IS/IX/S/X、主键/唯一键行锁；范围写表锁回退，有序 Gap Lock 待实现）

### Phase 2: 运维工具
- [x] 一键安装/升级
- [x] 系统服务集成
- [x] 增量备份工具（HTTP 原生 LSN 连续链；`mydbdump` 提供跨库表内容校验增量）
- [x] 监控指标

### Phase 3: HTTP 管理
- [x] HTTP API 框架
- [x] 认证与授权
- [x] 配置热重载
- [x] 备份管理接口（actor 屏障全量、CRC/LSN 增量、父链校验、RFC3339 PITR、重启前安全恢复）
- [x] 内存监控接口

### Phase 4: Agent 支持
- [x] AI Agent CLI（本地 health/slow/diagnose/SQL optimize；无外部模型依赖）
- [x] 中英文自然语言运维查询（内置离线意图分类，只读实时信号/慢 SQL，不执行任意 SQL）
- [x] 慢 SQL、健康诊断和静态 SQL 优化建议

### Phase 5: 生产就绪
- [x] 压力测试工具
- [x] 故障注入测试
- [x] 文档完善
- [ ] 社区建设

---

## 技术栈

| 组件 | 技术 |
|------|------|
| 语言 | Rust |
| 异步运行时 | tokio |
| HTTP 框架 | axum |
| SQL 解析 | sqlparser-rs |
| 序列化 | serde |
| 日志 | tracing |
| 配置 | serde_yaml |
| 测试 | cargo test |
| CI/CD | GitHub Actions |

---

## 参考项目

- [TiKV](https://github.com/tikv/tikv) - 分布式 KV 存储
- [DataFusion](https://github.com/apache/datafusion) - SQL 查询引擎
- [rust-mysql-common](https://github.com/blackbeam/rust-mysql-common) - MySQL 协议库
- [redis-rs](https://github.com/mitsuhiko/redis-rs) - Redis 客户端（HTTP API 参考）
