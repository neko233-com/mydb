# mydbdump

独立的 MySQL 8 / MyDB 逻辑备份、增量、恢复和离线校验 CLI。用户命令为 `mydbdump`。

## 设计依据

- MySQL 的无锁事务表备份基线：`REPEATABLE READ` + `START TRANSACTION WITH CONSISTENT SNAPSHOT`，对应 mysqldump `--single-transaction --quick`。
- MyDumper 的性能思路：按表拆分、批量行、压缩和清单校验；多连接快照若不使用 FTWRL/GTID 同步，不能假装全局一致。
- Percona XtraBackup 的增量原则：增量必须有明确基线和可验证边界。MyDB WAL 尚未具备完整重放语义前，使用 `table_content_sha256`，不冒充 LSN 增量。

参考：

- https://dev.mysql.com/doc/refman/8.0/en/mysqldump.html
- https://dev.mysql.com/doc/refman/8.0/en/point-in-time-recovery.html
- https://docs.percona.com/percona-xtrabackup/8.0/create-incremental-backup.html
- https://mydumper.github.io/mydumper/docs/html/locks.html

## 保证

- 默认不执行 `LOCK TABLES`。
- 仅事务表时，单连接一致性快照覆盖所有导出表。
- 非 InnoDB 默认失败；显式 `--allow-non-transactional` 才继续，并在清单标记非一致。
- SQL 字节统一输出为十六进制字面量，NULL、空值和任意 BLOB 不混淆。
- 每表 Zstd 流；批量 INSERT；不把全库拼成一个巨型文件。
- 三层校验：DDL SHA-256、无序逐行内容 SHA-256、压缩文件 SHA-256。
- 增量记录变化表、未变化表和删除表 tombstone；恢复顺序明确。

## 命令

```bash
mydbdump backup --url mysql://root:pass@127.0.0.1:3306 \
  --database game --output backup/full

mydbdump backup --url mysql://root:pass@127.0.0.1:3306 \
  --database game --output backup/inc-1 \
  --incremental-from backup/full/manifest.json

mydbdump verify --input backup/full

mydbdump restore --url mysql://root:pass@127.0.0.1:3306 \
  --database game --input backup/full
mydbdump restore --url mysql://root:pass@127.0.0.1:3306 \
  --database game --input backup/inc-1
```

用 `MYDBDUMP_URL` 代替 `--url` 可避免密码进入 shell 历史。

## 当前增量边界

`table_content_sha256` 会扫描源表，但只为变化表写压缩数据。因此它减少备份存储和恢复流量，不承诺减少源端扫描量。原生页/LSN 增量必须等 WAL 能完整重放数据库、DDL、事务提交顺序后再启用。
