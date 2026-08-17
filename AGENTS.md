# MyDB 开发规范

## 项目决策记忆

- MyDB 的产品定位是**单机 MySQL 8.4 兼容数据库**：目标是作为 MySQL 的单机替代品，3306 提供 MySQL 协议，4306 提供 Web SQL IDE/管理 API。
- 不规划分布式、集群、复制或 `mydb-router` 后续架构；不要为分布式抽象牺牲单机性能、可靠性和可维护性。
- 原生安装默认面向内网使用：MySQL 与 Web 管理监听 `0.0.0.0`，分别使用 3306/4306；公网部署必须由防火墙、TLS、强密码和最小权限保护。
- 发布是低频且有意的动作：构建、测试、打包可以自动执行，但创建 GitHub tag/release 和上传二进制必须经过人工确认；禁止为了试验或无意义改动重复发布。
- 对外行为、配置默认值、安装器、Web HTML 文档和 CLI 提示必须同步更新；新增非显然逻辑要写解释性注释，并用 Rust 回归测试锁定行为。

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

## 并发与多核架构

- **禁止 Actor 模型**：存储引擎不使用任何 actor / 专用写线程 / mpsc channel。提交采用 **Leader/Follower 组提交**，运行于调用方（连接）任务之上：首个发现 `commit_state.active == false` 的写者成为 leader，串行 drain 队列、每组一次 WAL `fsync`，队列空才释放 leadership，严格 FIFO。无专用写线程、无 actor 邮箱。
- **多核吞吐**：组提交必须在多核上可并行受益于核心数。当单 leader 的 `fsync` 吞吐上限（≈ 1 / 单次 `fsync` 耗时）低于目标时，按表命名空间分片为多个独立的 Leader/Follower 组（`CommitShard`），每组持有独立 WAL 文件、独立 `fsync`，从而多核并行提交。路由规则：单表 DML/DDL 按 `hash(table_namespace) % shard_count` 固定路由到同一分片，保证单表写入顺序与 DDL-before-DML 顺序；跨表事务路由到其写入集中首个表所属分片（罕见路径，允许在该分片串行）。
- **取消安全（Cancellation Safety）**：leader 运行在调用方任务上，连接断开 / 任务取消必须自恢复。通过 `LeaderGuard`（`Drop`）在取消时重置 `active = false` 并将 in-flight 批次重新入队（`generation` 单调号防止陈旧 leader 误抢 leadership），避免管道永久卡死。任何改 `active` 的路径都必须保证异常 / 取消后能由新 leader 接管。

## 系统性能影响规避

数据库热路径不得受运行平台 / OS 的隐含性能陷阱影响。已确认并必须规避的项：

1. **Windows 定时器分辨率**：`tokio::time::timeout` / `sleep` 受 OS 定时器分辨率约束（Windows 默认 ~15.6ms），会使配置的 250µs 窗口实际等待 ~12ms。组提交窗口等待必须使用 `std::time::Instant`（QPC，sub-µs 精度）+ `tokio::task::yield_now()` 轮询实现，禁止依赖 OS 定时器粒度实现亚毫秒等待。
2. **WAL fsync 成本**：`WalWriter::sync()` 只调一次 `sync_data()`（见性能优先规则第 1 条）；WAL 文件应以写透 / 无缓冲方式打开（`FILE_FLAG_WRITE_THROUGH`，Windows）以让 `FlushFileBuffers` 仅刷已写范围而非整文件脏页，避免 4.4ms 级的隐式刷盘。禁止为降低延迟而关闭持久化（`sync_data`）。
3. **禁止在调用方热路径做阻塞 / 重系统调用**：序列化、堆分配、`clear()` / `rebuild_all_indexes()` 等重操作不得在 WAL 锁持有期间或 leader 关键路径执行；锁必须在任何 `await` 之前释放。
4. **性能对比必须同机、实际运行、相同持久化设置**（见性能优先规则第 8 条）；禁止用历史比值或关闭持久化来“赢”。

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
