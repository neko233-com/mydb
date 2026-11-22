# MyDB → TiDB 迁移手册（单机 → 分布式）

> 本文描述**当前已落地**的迁移路径；MyDB 仍然是单机数据库，不实现 TiKV/PD、GTID 或 MySQL binlog 复制。
> CDC 是 MyDB 的扩展变更流，不是 MySQL 复制协议；迁移正确性依赖**幂等回放**与**写入静默窗口**。

## 1. 适用边界

| 项 | 说明 |
|---|---|
| 数据规模 | 单机 MyDB 能承载的库；迁移过程不要求 MyDB 集群 |
| 协议 | MyDB 兼容 MySQL 8.0~8.4；TiDB 以 MySQL 协议接收 DDL/DML |
| 一致性 | 快照 + CDC 追平；切换前需短暂停写并排空 CDC |
| 不保证 | 跨表事务的 exactly-once、DDL 自动重放、GTID、binlog 订阅 |
| 回滚 | TiDB 侧写入后回滚需业务侧补偿；保留 MyDB 快照备份 |

## 2. 迁移拓扑

```
MyDB (单机)  ──mydb-migrate──▶  TiDB (MySQL 协议)
     │
     └── CDC NDJSON (cursor.json / checkpoint.json) ──▶ 幂等 Loader ──▶ TiDB
```

## 3. 前置检查

1. `cdc.enabled: true` 或调用 `POST /api/v1/cdc/enable`，确认：

```bash
curl -H "Authorization: Bearer $MYDB_ADMIN_TOKEN" \
  http://127.0.0.1:8080/api/v1/cdc/status
```

返回包含 `last_lsn`、`checkpoint_lsn`、`lag_lsn`、`dir`。
2. TiDB 已建库，账号具备 `CREATE/INSERT/UPDATE/DELETE/ALTER` 权限。
3. MyDB 与 TiDB 的 `sql_mode`、字符集、排序规则一致；建议 `utf8mb4`
   与 MyDB 表定义一致，避免大小写/排序差异导致校验失败。
4. 记录快照起点 `L0 = last_lsn`。**L0 之后的所有变更都必须被 CDC 覆盖**；
   不要清空 `cdc.dir`，不要删除 `cursor.json`。

## 4. 第一步：结构与数据快照

`mydb-migrate` 以 MySQL 协议对称工作：`--source` 指向 MyDB，`--target` 指向 TiDB。

```bash
mydb-migrate \
  --source "mysql://root:pass@127.0.0.1:3306/app" \
  --target "mysql://root:pass@tidb-host:4000/app" \
  --database app \
  --batch-size 500 \
  --report tidb-migration-report.json
```

- 默认先建表、再按批复制数据，并逐表校验行数与校验和。
- 大表可拆分：`--schema-only` 先建结构；业务低峰用 `--data-only` 复制数据。
- 若目标已有表，明确加 `--drop-existing`（会删表，先确认目标库为空）。
- 报告 `verified: true` 才进入下一步。

> 快照期间允许写入：CDC 从 `L0` 起会记录这些变更。但**快照读到的行可能晚于 L0**，
> 因此后续 CDC 回放必须使用 **upsert + 按主键 delete** 的幂等语义，不能盲目 INSERT。

## 5. 第二步：CDC 追平

### 5.1 拉取变更

```bash
curl -H "Authorization: Bearer $MYDB_ADMIN_TOKEN" \
  "http://127.0.0.1:8080/api/v1/cdc/changes?since_lsn=$L0&limit=1000"
```

响应：

```json
{"events":[{
  "v":1,"lsn":123,"tx_id":45,"committed_unix_ms":1700000000000,
  "changes":[{
    "op":"insert|update|delete|ddl",
    "database":"app","table":"orders",
    "before":{"id":"MTIz"},
    "after":{"id":"MTIz","amount":"MTAw"},
    "ddl_sql":null
  }]
}]}
```

- `before` / `after` 的列值是 **base64**；空 map 表示该侧不存在。
- `ddl` 事件可能只带 `ddl_sql`；建议在业务停写窗口单独执行 DDL，避免结构漂移。
- 每次最多返回 1000 条事件；按 `lsn` 单调推进，不要跳读。

### 5.2 Loader 要求

| 事件 | TiDB 动作 |
|---|---|
| insert | `INSERT ... ON DUPLICATE KEY UPDATE`（幂等 upsert） |
| update | 按 `after` 的主键 upsert；若主键变更，先按 `before` 主键删除再插入 |
| delete | 按 `before` 主键 `DELETE` |
| ddl | 记录待人工/受控执行，不自动并发执行 |

要点：
- 一批事件在 TiDB **单事务提交成功后**，才推进 checkpoint。
- 失败可整批重放；upsert/delete 必须幂等。
- 不依赖事件顺序之外的并发；同一个 `tx_id` 内按原顺序应用。

### 5.3 确认断点

每批提交后：

```bash
curl -X POST -H "Authorization: Bearer $MYDB_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"lsn":123}' \
  http://127.0.0.1:8080/api/v1/cdc/checkpoint
```

- `checkpoint_lsn` 只能前进，不能回退；超过生产者 `last_lsn` 会被拒绝。
- `lag_lsn = last_lsn - checkpoint_lsn`，追平目标为 `0`。
- 生产者的 `cursor.json` 不会被 checkpoint 改动；重新读取仍可从原位置回放。

## 6. 第三步：切换

1. 业务停写（或只读），等待 `lag_lsn == 0` 且最后一批已提交 TiDB。
2. 对比关键表：行数、主键校验和、抽样业务字段。
3. 切换连接串到 TiDB；保留 MyDB 数据目录与最近一次全量备份。
4. 观察 TiDB 慢查询与写入热点；MyDB 侧停止 CDC（`POST /api/v1/cdc/disable`）。

## 7. 校验 SQL 示例

```sql
-- 行数
SELECT COUNT(*) FROM app.orders;
-- 主键范围校验和（示例，按业务替换）
SELECT COUNT(*), MIN(id), MAX(id), SUM(CRC32(CONCAT_WS('#', id, amount))) FROM app.orders;
```

`mydb-migrate` 报告中的 `source_checksum` / `target_checksum` 可直接对比；
不一致时先查字符集、空格补齐（PAD SPACE）、浮点格式化与 NULL 表示。

## 8. 已知限制（必须写进变更评审）

- MyDB 是单机数据库；迁 TiDB 不会自动获得 MyDB 的 WAL/组提交语义。
- CDC 不是 binlog，不能接原生 MySQL 复制消费者；需要按本文 NDJSON 协议自建 Loader。
- `checkpoint.json` 只记录消费确认，不做数据校验；校验以 `mydb-migrate` 报告和切换前对账为准。
- 大事务会放大单条 CDC 事件体积；建议单事务行数受控（例如 ≤ 5000 行）。
- DDL 与 DML 混跑时结构可能漂移；生产切换应在停写窗口完成最终 DDL 对齐。
