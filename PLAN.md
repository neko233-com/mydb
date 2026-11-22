# PLAN.md — v0.1.38 生产级落地计划

> 本文是当前迭代的唯一执行计划。完成一项勾选一项；未完成的项必须保留原因，不得用“已支持/生产级”表述掩盖。
> 目标定位不变：**单机 MySQL 8.0~8.4 协议与语法兼容数据库**，不引入分布式、集群、复制或 router 抽象。

## 0. 迭代目标（用户诉求拆解）

| # | 诉求 | 可验收结果 |
|---|------|-----------|
| G1 | 更新到最新 | `cargo update` 后锁文件为当前最新兼容版本；工具链在本机可复现构建 |
| G2 | OpenTelemetry 原生支持 + 密码搭配 | OTLP/HTTP（metrics + traces）为标准 OTLP JSON；支持 Bearer/Basic（用户名+密码）鉴权；密钥可来自环境变量或文件；支持热开关；连接器状态可观测 |
| G3 | 100% 高性能 | 不回归既有 WAL/组提交热路径约束；新增优化必须有回归测试与可观测计数 |
| G4 | 100% 生产级 | 健康/就绪探针、静态配置校验、优雅停机、错误路径可观测、文档与脚本同步；以门槛清单为准，不空喊 |
| G5 | CDC 自带机制，便于迁移 TiDB | CDC 事件流（NDJSON）可查询、可断点、可对账；提供快照 + CDC 游标的 TiDB 迁移路径与工具说明 |
| G6 | 原生 OLTP 与 OLAP 都快 | 行存 OLTP 路径不回退；新增页级 zone map（min/max/null）让分析型范围扫描跳过无关页，并暴露 `pages_pruned` 指标 |
| G7 | PLAN.md 驱动 + 可 git push | 本文件逐项勾选；clippy/test/release 构建通过后提交并推送 main |

非目标：TiKV/TiDB 分布式事务、MySQL binlog 协议复制、GTID、读写分离、跨节点一致性。

## 1. 基线（G1）

- [x] 1.1 修复本机构建工具链（MSVC 安装缺 `vcruntime.h`/`msvcrt.lib`，改用 `stable-x86_64-pc-windows-gnullvm` + llvm-mingw）
- [x] 1.2 `cargo update` 更新锁文件；记录无法升级的 crate 及原因
- [x] 1.3 `cargo +gnullvm check --workspace --all-targets` 通过
- [x] 1.4 `cargo test --workspace` 全绿（storage 66 + storage_test 17 + wire 279 + server 28 + 其他 workspace 测试）

## 2. OpenTelemetry 生产化（G2）

现状：`crates/mydb-server/src/observability.rs` 已以 OTLP/HTTP JSON 导出 metrics 与慢 SQL spans，`headers` 支持自定义请求头，`/metrics` Prometheus 常驻。

- [x] 2.1 配置新增 `observability.otlp.auth`：`none | bearer | basic`；basic 使用 `username + password`，password 可来自 `MYDB_OTLP_PASSWORD` 或 `password_file`
- [x] 2.2 请求头构造集中化，优先级：显式 `headers.Authorization` > auth 配置；Basic 使用标准 base64，禁止把密码写入日志或 GET 配置响应
- [x] 2.3 OTLP 端点安全校验：任何凭据（header/auth）在非 loopback 时必须 HTTPS；用户名与密码必须成对出现；`password_file` 与 `password` 互斥
- [x] 2.4 导出器常驻任务：默认关闭时零快照开销；`otlp.enabled` 改为可热开关（不再要求重启）
- [x] 2.5 状态可观测：`GET /api/v1/observability/otlp` 返回开关、端点、鉴权类型、成功/失败计数、最近错误（不含密钥）
- [x] 2.6 OTLP 指标补充 CDC 与 OLAP：`mydb.cdc.*`、`mydb.olap.*`、导出器失败计数
- [x] 2.7 测试：鉴权头生成、配置校验、脱敏、热开关状态、OTLP payload 结构
- [x] 2.8 对齐 MySQL 9 Telemetry：`stmt` Internal span + `mysql.*` 属性；新增 OTLP `/v1/logs`（`export_logs` 默认关闭、敏感字段脱敏）；运行时冒烟验证 `/v1/metrics` 与 `/v1/logs` 均到达采集器；对照与缺口写入 `docs/zh/opentelemetry.md`

## 3. OLAP 页级 zone map（G6）

现状：存储为 16KB 行存页；过滤扫描对非索引谓词退化为全表解码。

- [x] 3.1 `PageZoneMap`：按页、按列记录非 NULL 的 min/max 与 NULL 计数；按页 checksum 做缓存键和失效
- [x] 3.2 谓词裁剪：`Eq/In/Less/LessOrEq/Greater/GreaterOrEq/Between/And/Or/IsNull/IsNotNull` 保守裁剪；字符串仅在二进制排序规则下裁剪，其余一律不裁剪
- [x] 3.3 接入 `scan_table_filtered_limit` 的非索引与溢出行路径；统计 `pages_scanned/pages_pruned/rows_pruned`
- [x] 3.4 指标：Prometheus 与 OTLP 暴露 `mydb_olap_pages_pruned_total` 等
- [x] 3.5 测试：数值/字符串/NULL/AND-OR 裁剪正确性；更新后 checksum 失效；结果与全量扫描逐行一致
- [x] 3.6 明确记录未做项：列裁剪、向量化聚合、并行页扫描留待下一迭代（不得宣称 OLAP 已“全面领先”）

## 4. CDC → TiDB 迁移支撑（G5）

现状：`mydb-cdc` 读侧 tail WAL，输出 NDJSON，游标 `cursor.json`；`mydb-migrate` 可在 MySQL 协议两端复制表结构与数据。

- [x] 4.1 消费端断点：`GET/PUT /api/v1/cdc/checkpoint`（独立于生产者 `cursor.json`，写入 `checkpoint.json`）
- [x] 4.2 `GET /api/v1/cdc/status` 增加 `checkpoint_lsn` 与 `lag_lsn`（生产者 LSN - 消费确认 LSN）
- [x] 4.3 迁移路径文档 `docs/zh/tidb-migration.md`：快照（mydb-migrate / mydb-dump）→ 记录 LSN → CDC 追平 → 校验；英文摘要进 README
- [x] 4.4 测试：checkpoint 持久化、lag 计算、越界确认拒绝

## 5. 生产级运维（G4）

- [x] 5.1 无鉴权探针：`GET /healthz`（进程存活）、`GET /readyz`（存储已初始化、HTTP/MySQL 监听状态）；不泄露内部细节
- [x] 5.2 启动静态校验：OTLP 鉴权、CDC 目录/大小、备份保留参数；错误信息可定位但不含密钥
- [x] 5.3 管理 API 请求体上限与超时（避免大 JSON 拖垮单机）
- [x] 5.4 配置模板同步 `configs/*.yaml`（otel auth、cdc checkpoint、olap 开关）
- [x] 5.5 更新 README / TARGET / AGENTS / 四语操作文档 / 性能报告（如实记录未重跑基准的原因与状态）

## 6. 验收门槛（发布前必须全部通过）

- [x] 6.1 `cargo +stable-x86_64-pc-windows-gnullvm clippy --workspace --all-targets -- -D warnings`（通过，仅 cargo manifest 未使用依赖提示）
- [x] 6.2 `cargo +stable-x86_64-pc-windows-gnullvm test --workspace`（全绿）
- [x] 6.3 `cargo +stable-x86_64-pc-windows-gnullvm build --release --locked -p mydb-server -p mydb-cli -p mydb-migrate -p mydb-dump`（通过）
- [x] 6.4 本机启动冒烟：release 二进制、`/healthz`、`/readyz`、`/metrics`、OTLP 状态、OTLP `/v1/metrics`+`/v1/logs`、SQL 建表/写入/范围扫描、OLAP 裁剪、权限隔离（`scripts/verify-monitoring-runtime.py` 通过）
- [x] 6.5 本机无 MySQL/podman，未重跑同机基准；已如实标注“未重跑”，不写新倍数
- [x] 6.6 版本号 `0.1.38`，提交信息符合 `<type>(<scope>): <中文描述>`，推送 `main`；GitHub Release `v0.1.38` 已发布（Windows zip + Linux musl tar，校验和已上传）
- [x] 6.7 Linux musl 静态构建规范与 CI：`AGENTS.md`、`docs/zh/musl-build.md`、`.github/workflows/build-linux-musl.yml`；GitHub Actions run `45060434795` 构建、静态校验与产物上传通过

## 7. 风险与回退

- 页级 zone map 只做“跳过不可能命中的页”，命中判断必须保守；任何不确定情况返回“可能命中”，绝不漏行。
- OTLP 导出在独立任务中执行，不进入查询/提交热路径；关闭时不采集快照。
- CDC 仍是额外扩展，不是 MySQL binlog；文档中不得表述为复制兼容。
- 性能结论以同机实测为准；没有实测就只记录优化点与可观测计数。
