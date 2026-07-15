# MyDB 目标与规划

## 核心定位

MySQL 8.x 开源替代品，专为游戏业务优化。高并发、低延迟、易于部署和运维。

---

## 存储引擎

### Memory 引擎
- 纯内存存储，用于临时表和缓存
- 支持哈希索引
- 会话级别数据隔离
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
- 基于 LSN 的增量备份
- 支持时间点恢复（PITR）
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

---

## 性能目标

| 指标 | MySQL 8.x | MyDB 目标 |
|------|-----------|-----------|
| QPS (单机) | ~50,000 | ~80,000+ |
| 延迟 P99 | ~10ms | ~5ms |
| 内存效率 | 基准 | 提升 30% |
| 启动时间 | ~5s | ~1s |

---

## 兼容性

### 协议兼容
- MySQL 8.x Wire Protocol
- 支持标准 mysql 客户端连接
- 支持所有主流编程语言驱动

### 语法兼容
- DDL: CREATE, ALTER, DROP
- DML: SELECT, INSERT, UPDATE, DELETE
- 事务: BEGIN, COMMIT, ROLLBACK
- 存储过程（未来）

### 配置兼容
- YAML 格式配置
- MySQL 参数兼容映射
- 环境变量支持

---

## 开发阶段

### Phase 1: 核心功能
- [x] MySQL 协议兼容层
- [x] Memory 引擎
- [x] InnoDB 引擎基础
- [ ] 事务管理
- [ ] 锁管理

### Phase 2: 运维工具
- [ ] 一键安装/升级
- [ ] 系统服务集成
- [ ] 增量备份工具
- [ ] 监控指标

### Phase 3: Agent 支持
- [ ] AI Agent CLI
- [ ] 自然语言查询
- [ ] 智能优化建议

### Phase 4: 生产就绪
- [ ] 压力测试
- [ ] 故障注入测试
- [ ] 文档完善
- [ ] 社区建设

---

## 技术栈

| 组件 | 技术 |
|------|------|
| 语言 | Rust |
| 异步运行时 | tokio |
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
