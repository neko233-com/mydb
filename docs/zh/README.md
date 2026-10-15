# MyDB 操作指南

[English](../en/README.md) · [日本語](../ja/README.md) · [한국어](../ko/README.md) · [仓库总览](../../README.md)

MyDB 是用 Rust 编写的单机数据库。客户端使用 MySQL 8.4 协议连接 3306。Web SQL IDE 和管理 API 使用 4306。存储引擎是 Neko233。`ENGINE=InnoDB` 只是 SQL 里的兼容别名，不会加载 MySQL InnoDB 源码。

复制拓扑、GTID、读写分离、Group Replication、Galera 和分布式 XA 不在产品范围内。验收记录见 [CheckList.md](../../CheckList.md)，语法对照见 [SYNTAX_MATRIX.md](../../SYNTAX_MATRIX.md)。

## 快速开始

在仓库根目录准备密码文件后启动。不要把真实密码写进 Git。

```bash
cat > .env <<'EOF'
MYDB_ROOT_PASSWORD=replace-with-a-generated-secret
MYDB_ADMIN_PASSWORD=replace-with-another-generated-secret
EOF
docker compose up -d --build
mysql --protocol=TCP -h 127.0.0.1 -P 3306 -u root -p
```

常用环境变量：

| 变量 | 作用 |
|------|------|
| `MYDB_ROOT_PASSWORD` | SQL root 密码，必填 |
| `MYDB_ADMIN_PASSWORD` | HTTP 管理密码，必填 |
| `MYDB_PORT` | SQL 端口，默认 3306 |
| `MYDB_HTTP_PORT` | 管理端口，默认 4306 |
| `MYDB_DATA_DIR` | 数据目录 |
| `MYDB_LOG_LEVEL` | `debug` / `info` / `warn` / `error` |
| `MYDB_GROUP_COMMIT_WINDOW_US` | Group Commit 窗口，单位微秒 |
| `MYDB_ROOT_PASSWORD_FILE` | 从文件读取 root 密码 |
| `MYDB_ADMIN_PASSWORD_FILE` | 从文件读取 admin 密码 |

同一个密钥不要同时设置普通变量和对应的 `_FILE` 变量。`docker compose down` 停止服务并保留数据卷。

## 安装与更新

Linux / macOS 使用 `scripts/install.sh`。Windows 使用 `scripts/install.ps1`，默认目录是 `C:\Server\mydb\mydb-server`，服务名是 `MyDBServer`。首次安装把随机密钥写到配置目录的 `secrets`，升级只替换二进制。

已发布的二进制可以用 `mydb update --check` 查看版本，用 `mydb update` 更新。更新保留配置、数据和密钥。

## 生产部署

原生安装默认监听 `0.0.0.0:3306` 和 `0.0.0.0:4306`，面向内网。放到公网时由防火墙限制来源，打开 TLS，并使用密钥文件：

```bash
install -m 0700 -d /run/mydb-secrets
umask 077
openssl rand -base64 36 > /run/mydb-secrets/root
openssl rand -base64 36 > /run/mydb-secrets/admin
MYDB_ROOT_PASSWORD_FILE=/run/mydb-secrets/root \
MYDB_ADMIN_PASSWORD_FILE=/run/mydb-secrets/admin \
mydb-server --config /etc/mydb/production.yaml
```

配置 `security.tls_cert`、`security.tls_key`，并设置 `require_secure_transport: true`。Docker 烟测里的 `root/root` 只属于本地兼容测试。

Prometheus 指标在 `http://127.0.0.1:4306/metrics`。强密码模式下先登录管理 API，再带 Bearer token 访问。

## 备份与迁移

`mydbdump` 做无表锁一致性快照、校验和增量备份：

```bash
mydbdump backup --url 'mysql://root:password@127.0.0.1:3306' --database game --output backups/game-full
mydbdump verify --input backups/game-full
mydbdump restore --url 'mysql://root:password@127.0.0.1:3306' --database game --input backups/game-full
```

升级前后在隔离实例上演练恢复。从 MySQL 迁入使用 `mydb-migrate`。命令和 HTTP 备份 API 的完整示例在仓库 [README.md](../../README.md) 的备份与迁移章节。

## 测试、基准与磁盘

本地检查：

```bash
cargo test --workspace
.\scripts\docker-smoke.ps1
bash scripts/docker-smoke.sh
.\scripts\docker-fault-injection.ps1
```

Docker Rust 门禁是 `scripts/test-docker.ps1` 和 `scripts/test-docker.sh`。每次结束删除卷 `mydb-rust-target`，避免 debug 与 release 产物一直占用磁盘。保留编译缓存时加 `-KeepCache` 或 `KEEP_CACHE=1`。

Linux I/O 基准 `scripts/bench-io-limited.sh` 的回环文件默认各 1G。结束时删除 `mydb:io-bench`、`mydb-bench:ubuntu24` 和对应的 BuildKit 编译缓存。基础镜像 `mysql:8.0`、`rust`、`debian`、`ubuntu` 保留。

清理本机压测残留：

```powershell
.\scripts\clear-bench-disk.ps1
```

```bash
bash scripts/clear-bench-disk.sh
```

脚本可以重复执行。它删除 compose 项目 `mydb-bench` 与 `mydb-smoke`、I/O 和故障注入留下的容器与卷、上述压测镜像，以及 `target/io-bench*`。它不删除名为 `mydb` 的业务容器，也不清空全部 Docker 构建缓存。

性能数字以 [性能报告.md](../../性能报告.md) 里当次实测为准。
