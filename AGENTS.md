# MyDB 开发规范

## 强制规则

### 性能优先（WAL 热路径）

WAL 写入是每个提交的关键路径，任何修改必须遵守以下约束：

1. **`WalWriter::sync()` 只能调用一次 `sync_data()`**，禁止添加额外的 `set_len`/`fsync`/`sync_all` 调用。预分配零字节尾部由 `recover_valid_tail()` 在启动时 CRC 校验截断，rotate_file 时截断旧文件。
2. **禁止在 WAL 锁持有期间做序列化**：`encode_wal_group_into` 必须在 `wal_writer.lock()` 之外完成。
3. **禁止在 WAL append 热路径分配中间 `Vec<u8>`**：Group Commit 使用 `append_raw` 直写可复用 `write_buf`；序列化使用可复用 `wal_encode_buf`。
4. **bincode 编码使用 `bincode_fast()`（fixint little-endian）**，仅 legacy 格式解析使用 `bincode_compat()`（varint）。
5. **INSERT 必须进入 `pending_rewrites` memtable**，禁止每次 INSERT 直接写数据页和 fsync。
6. **`replace_rows` 禁止调用 `buffer_pool.clear()` 或 `rebuild_all_indexes()`**：必须使用 `clear_namespace` + `replace_table_logical_indexes` + `add_row_page_index` 只重建受影响的表。
7. **Checkpoint 与合批解耦**：按已成功提交请求数触发，不得按提交组数触发；延后 checkpoint 不得改变 WAL durability、崩溃重放、`flush_consistent` 或 shutdown 强制落盘语义。
8. **性能对比必须可复现**：MyDB 与 MySQL 同机实际运行、相同负载与持久化设置；报告记录版本、参数、样本数，禁止硬编码历史比值。

### 数据安全

1. **WAL 记录格式：** `[LSN:8][payload_len:4][payload:payload_len][CRC32:4]`，CRC 覆盖 LSN+payload（不含 payload_len）。
2. **Checkpoint 必须是原子 COW：** staging 目录写入 → marker 文件 → rename(active→backup) → rename(staging→active)。
3. **崩溃恢复幂等：** WAL 重放必须能处理重复 Applied 记录、零字节尾部、torn write。
4. **DDL (`sync_data=true`) 必须调用 `checkpoint_table`**，不能绕过 WAL 直接写磁盘。
5. **`replace_rows` 成功后必须清除 `pending_rewrites`**，防止 UPDATE/DELETE 后读到陈旧内存数据。

### 序列化兼容

- WAL v3 magic: `b"MDG2"` (bincode_fast/fixint)
- WAL v2 magic: `b"MDG1"` / `b"EVT4"` (bincode_compat/varint)，解码时必须做 fallback 兼容
- 新增 WAL 版本必须 bump magic number 并保留旧格式解码路径

## 代码风格

- 不要添加无关注释，代码应自文档化
- 不要在测试中使用 `unwrap()` 处理可能失败的业务逻辑，用 `?` 或 `expect("context")`
- 优先复用现有工具函数，禁止重复实现
- 使用 `parking_lot::Mutex`/`RwLock`（非 `std::sync`）
- Buffer pool 命名空间格式：`{table_name}`（`page_namespace` 方法）

## Git 规范

每次 `git push` 前必须：

1. **运行自动化基准**：`pwsh -File scripts/bench.ps1`（自动执行 clippy → test → release build → 本机无资源限制 MyDB 与实际 MySQL 对比 → 更新 [性能报告.md](性能报告.md)）
2. **更新 README.md**：性能数据摘要、架构变更、新特性必须同步到 README
3. 通过 `cargo clippy --workspace --all-targets -- -D warnings` 零警告
4. 通过 `cargo test --workspace` 全部测试通过
5. Commit message 格式：`<type>(<scope>): <中文描述>`
   - type: `perf`/`fix`/`feat`/`refactor`/`test`/`chore`
   - scope: `wal`/`storage`/`wire`/`parser`/`server`/`cli` 等
   - 示例：`perf(wal): 移除 sync 中的 set_len 减少一次 syscall`
6. 禁止提交临时文件（`*.ps1`、`*.bat`、`*.tmp` 等构建脚本，`scripts/` 下的正式脚本除外）

## 测试覆盖

- WAL 层必须有：append/sync/reopen/rotation/torn-write/corruption 测试
- 存储层必须有：INSERT/UPDATE/DELETE/UPSERT 崩溃恢复测试
- Group commit 必须有：重启后无重复写入测试
- 修改 checkpoint 触发条件必须有：阈值边界、WAL 重放、`flush_consistent` Applied 标记测试
- 修改 WAL 格式必须添加：新旧格式兼容测试

## 默认配置

- 默认账号：`root` / `root`（见 [configs/default.yaml](configs/default.yaml)）
- 默认 Group Commit 窗口：250μs（吞吐默认）；0μs 为低延迟自然批量档。改动必须同时报告单连接延迟与并发吞吐。
- 数据页大小：16KB (`DEFAULT_PAGE_SIZE`)
- WAL 预分配粒度：8MB
- WAL 文件最大大小：64MB
- Checkpoint 间隔：1024 个已成功提交请求；`flush_consistent` 与 shutdown 无条件 checkpoint

## InnoDB 名称兼容边界

- Neko233 是唯一持久化事务存储内核；`MEMORY` 保持独立非事务语义。
- `ENGINE=InnoDB`、`default_storage_engine=InnoDB` 与 `SHOW CREATE TABLE` 中的 `InnoDB` 均为 MySQL SQL/协议兼容名称，实际路由至 Neko233；项目不加载或复用 InnoDB 源码。
- 仅 `InnoDB` 可作为 Neko233 别名。未知 `ENGINE` 必须返回 MySQL unknown-storage-engine 错误，禁止静默重定向。
- 对外兼容承诺以 `CheckList.md` 与 `SYNTAX_MATRIX.md` 已验证项目为准。禁止将名称别名表述为“完整复刻 InnoDB 内核”“已具备全部 InnoDB 行为”或“完全生产级”。
- 仅在全部发布门槛、故障注入、平台验证、性能验收及安全运维验收通过后，才可使用“生产可用/生产级”表述。
