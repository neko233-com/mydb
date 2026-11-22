# CDC 热重载 + 增量备份增强 + 性能优化设计

日期：2026-11-18
状态：已批准实现（用户指令要求直接推进，不阻塞提问）

## 意图与为什么

MyDB 定位为单机 MySQL 8.4 兼容替代品。本次目标：

1. 记录"后续验证统一使用 podman"的决策到 `AGENTS.md`（已完成）。
2. 增加**自带 CDC（变更数据捕获）机制**：作为相对 MySQL 的额外扩展，**默认关闭**，可通过配置文件热重载**动态开启/关闭**，无需重启。
3. 增强**增量备份**：把当前形同虚设的 `backup.auto_backup_interval` / `backup.max_backups` 变成真正生效的自动备份与保留策略，并让 CDC 游标与增量备份链可互相定位。
4. **深度性能优化**：在同等 1c2g / 相同持久化设置下缩小与 MySQL 的延迟差距（当前吞吐领先，但 p99 写延迟与点读 P50 落后）。

## 目标 / 非目标 / 反目标

- 目标：CDC 事件流（行级 insert/update/delete/ddl）、NDJSON 落盘、重启安全游标、热重载开关、管理 API；自动增量备份与保留；安全且可回归验证的性能优化。
- 非目标：分布式/集群/复制（AGENTS.md 明令禁止）；MySQL binlog 协议兼容；把 CDC 表述为"生产级"。
- 反目标：任何触碰 WAL 热路径的 channel/actor；关闭 `sync_data()`；在 WAL 锁内序列化；硬编码性能比值。

## 约束

- CDC 必须是**读侧**能力：通过 tail WAL 的 `GroupCommit` 记录生成事件，不给 `commit_batch` 增加 `try_send` 或 mpsc（遵守"禁止 Actor / 专用写线程 / mpsc channel"）。默认关闭时零开销。
- 所有 WAL 热路径强制规则（单次 `sync_data`、锁外序列化、无中间 Vec、`bincode_fast`、INSERT→`pending_rewrites`、checkpoint 按请求数）保持不变。
- 性能对比必须同机实际运行、相同配置、相同持久化设置；禁止用历史比值。

## 方案选择

### CDC 数据来源：WAL tail（选中） vs commit 内联钩子 vs binlog 伪协议
- **commit 内联钩子**：在 `commit_batch` 的 `wal.sync()` 后 `try_send`，事件最及时，但向存储提交路径引入 channel，违反 AGENTS.md 并发架构约束；取消安全需要 LSN 去重。放弃。
- **binlog 伪协议**：需实现 MySQL replication 协议，范围过大且与"单机不规划复制"冲突。放弃。
- **WAL tail（选中）**：`GroupCommit` 记录是唯一权威提交标记，payload 含 `committed_unix_ms` 与完整 `Vec<Vec<WriteCommand>>`。读侧独立任务，零热路径成本，天然复用现有 WAL archive 增量备份链路，且 LSN 单调提供重启安全游标。代价是事件有轮询延迟（默认 500ms，可配）。

### 热重载：复用现有 `apply_runtime_config`（选中） vs 新增文件 watcher
- 现有 `POST /api/v1/config/reload` 重读 YAML 并经过 `apply_runtime_config` → `validate_reloadable_config`。`cdc` 段不在拒绝列表里，因此天然可热重载。新增 `notify` 文件监听会引入新依赖与信号复杂度。选中复用。
- 另提供 `POST /api/v1/cdc/enable|disable|reload` 便于"动态开启"，底层都调用 `CdcManager::apply()`。

### 增量备份：激活惰性配置（选中） vs 重写备份格式
- 现有 `export_wal_redo_range` + 元数据链 + PITR 已经健壮。`auto_backup_interval`/`max_backups` 从未被读取。选中：实现后台自动备份调度与保留裁剪，不改备份格式；在元数据中追加 `cdc_cursor_lsn` 以便与 CDC 流对账。

## 设计细节

### 配置（`mydb-config`）
新增 `cdc: CdcSection`：
- `enabled: false`（默认关闭）
- `dir: null` → `<data_dir>/cdc`
- `file_prefix: "events"`, `max_file_size: "128M"`, `max_files: 10`
- `poll_interval_ms: 500`, `flush_every: 1`
- `include_before: true`, `include_ddl: true`, `include_row_images: true`
- `filters: { databases: [], tables: [] }`（空=全部）
- 新增 env 覆盖 `MYDB_CDC_ENABLED`。

### 事件格式（NDJSON，每行一个提交组）
```
{"v":1,"lsn":123,"tx_id":123,"committed_unix_ms":...,"changes":[{"op":"insert","database":"db","table":"t","before":{},"after":{"id":"base64...","name":"..."}}]}
```
行值一律 base64（避免非 UTF-8），列名原样。`before` 来自 `RowPredicate` 的等值条件，`after` 来自 `Row.values` / assignments。DDL 记录 `op:"ddl"` + `ddl_sql`。

### CDC crate 与游标
- `mydb-wal` 新增中性 `WalChange` / `WalChangeOp`（不依赖 storage 类型）。
- `mydb-storage` 暴露 `pub fn decode_wal_group_changes(payload) -> Result<Vec<WalChange>>`，复用私有 `decode_wal_group`。
- `mydb-cdc`：`CdcManager`（`ArcSwap<CdcSection>` + 互斥运行时状态：当前文件、游标 LSN、计数、错误），`append_group` 写 NDJSON 并轮转，`status()` 返回统计；`run_cdc_tailer` 后台任务用 `spawn_blocking` 扫 WAL、按 LSN 排序、去重（游标 > lsn）后提交。
- 游标持久化 `<cdc_dir>/cursor.json`，记录 `last_lsn`，重启后从该 LSN 之后继续；torn/重复记录由 LSN 去重吸收。

### 管理 API
- `GET /api/v1/cdc/status`
- `POST /api/v1/cdc/enable` / `disable` / `reload`
- `GET /api/v1/cdc/changes?since_lsn=&limit=` 从 NDJSON 读取，供增量消费者使用。
- 均复用 `authorize()`。

### 增量备份增强
- 后台任务每轮读 `state.config`，按 `auto_backup_interval`（小时）触发：优先增量（存在基备份时），否则全量。
- 保留策略：按 `created_at` 排序，保留最近 `max_backups`，删除时跳过仍被保留备份依赖的父备份。
- 元数据追加 `cdc_cursor_lsn`（可选字段，缺省 0，保持旧备份可读）。

### 性能优化（低风险、可回归）
1. **已做**：删除 `CommitShard::send_write_batch` 中无人 await 的 `commit_notify.notify_one()`（确认 `collect_batch` 用 `Instant` + `yield_now` 轮询，无 waiter），每次写省一次原子通知；同时移除对应的 `Notify` 字段与导入。
2. **已做**：BufferPool 页索引由 `HashMap<(String,u32), Page>` 改为 `HashMap<String, HashMap<u32, Page>>`。页查找不再分配 `String` 键；`clear_namespace` 由 O(n) `retain` 变成 O(1) `remove`；淘汰仍为跨命名空间全局最小 LSN。新增 `buffer_pool_namespaces_are_isolated_and_eviction_is_bounded` 回归测试。
3. **未做**：`commit_batch` 中 `candidate = cumulative.clone()` 的 O(n²) 克隆仅出现在非 insert 且相互依赖的分组，INSERT-only 基准负载不触发，收益无法在当前环境验证，风险高于收益，暂不改动。
4. 验证：`cargo clippy --workspace --all-targets -- -D warnings` 与 `cargo test --workspace` 全绿。**基准未重跑**：环境无 podman/Docker/MySQL，无法满足可复现同机对比；性能报告已如实注明，不声明新倍数，也不宣称 1c2g 下完全超越 MySQL。

## 测试计划
- config：默认 `cdc.enabled == false`，YAML 解析、env 覆盖、模板可解析。
- storage：`decode_wal_group_changes` 对 insert/update/delete/ddl 的映射往返。
- cdc：NDJSON 输出格式、`cursor.json` 重启续读、轮转、过滤、enable/disable。
- server：热重载配置可切换 cdc.enabled；enable/disable 端点鉴权。
- WAL：既有 append/reopen/rotation/torn-write/corruption 保持通过。
