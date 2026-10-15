# MyDB operator guide

[中文](../zh/README.md) · [日本語](../ja/README.md) · [한국어](../ko/README.md) · [Repository overview](../../README.md)

MyDB is a single-node database written in Rust. Clients use the MySQL 8.4 protocol on port 3306. The Web SQL IDE and admin API use port 4306. The storage engine is Neko233. `ENGINE=InnoDB` is a SQL compatibility alias and does not load the MySQL InnoDB source.

Replication topologies, GTID, read/write splitting, Group Replication, Galera, and distributed XA are outside the product. The acceptance record is [CheckList.md](../../CheckList.md). The SQL surface is [SYNTAX_MATRIX.md](../../SYNTAX_MATRIX.md). Both remain in Chinese.

## Quick start

Create a password file in the repository root, then start the server. Do not commit real passwords.

```bash
cat > .env <<'EOF'
MYDB_ROOT_PASSWORD=replace-with-a-generated-secret
MYDB_ADMIN_PASSWORD=replace-with-another-generated-secret
EOF
docker compose up -d --build
mysql --protocol=TCP -h 127.0.0.1 -P 3306 -u root -p
```

| Variable | Purpose |
|----------|---------|
| `MYDB_ROOT_PASSWORD` | SQL root password, required |
| `MYDB_ADMIN_PASSWORD` | HTTP admin password, required |
| `MYDB_PORT` | SQL port, default 3306 |
| `MYDB_HTTP_PORT` | Admin port, default 4306 |
| `MYDB_DATA_DIR` | Data directory |
| `MYDB_LOG_LEVEL` | `debug`, `info`, `warn`, or `error` |
| `MYDB_GROUP_COMMIT_WINDOW_US` | Group commit window in microseconds |
| `MYDB_ROOT_PASSWORD_FILE` | Read the root password from a file |
| `MYDB_ADMIN_PASSWORD_FILE` | Read the admin password from a file |

Do not set a secret in both the plain variable and its `_FILE` variable. `docker compose down` stops the server and keeps the data volume.

## Install and update

Linux and macOS use `scripts/install.sh`. Windows uses `scripts/install.ps1`, installs to `C:\Server\mydb\mydb-server`, and registers the `MyDBServer` service. The first install writes random secrets under the config `secrets` directory. An upgrade replaces binaries and keeps config, data, and secrets.

Published binaries can check with `mydb update --check` and update with `mydb update`.

## Production

A native install listens on `0.0.0.0:3306` and `0.0.0.0:4306` for a private network. On a public network, restrict the firewall, enable TLS, and load secrets from files:

```bash
install -m 0700 -d /run/mydb-secrets
umask 077
openssl rand -base64 36 > /run/mydb-secrets/root
openssl rand -base64 36 > /run/mydb-secrets/admin
MYDB_ROOT_PASSWORD_FILE=/run/mydb-secrets/root \
MYDB_ADMIN_PASSWORD_FILE=/run/mydb-secrets/admin \
mydb-server --config /etc/mydb/production.yaml
```

Set `security.tls_cert`, `security.tls_key`, and `require_secure_transport: true`. The `root/root` pair in Docker smoke tests is only for local compatibility tests.

Prometheus metrics are at `http://127.0.0.1:4306/metrics`. In strong-password mode, log in to the admin API and send the Bearer token.

## Backup and migration

`mydbdump` takes a consistent snapshot without a table lock, verifies it, and can take an incremental backup:

```bash
mydbdump backup --url 'mysql://root:password@127.0.0.1:3306' --database game --output backups/game-full
mydbdump verify --input backups/game-full
mydbdump restore --url 'mysql://root:password@127.0.0.1:3306' --database game --input backups/game-full
```

Rehearse restore on an isolated instance before and after an upgrade. Use `mydb-migrate` to move a database from MySQL. Full command examples and the HTTP backup API are in the backup and migration sections of [README.md](../../README.md).

## Tests, benchmarks, and disk

Local checks:

```bash
cargo test --workspace
.\scripts\docker-smoke.ps1
bash scripts/docker-smoke.sh
.\scripts\docker-fault-injection.ps1
```

The Docker Rust gate is `scripts/test-docker.ps1` and `scripts/test-docker.sh`. Each run deletes the `mydb-rust-target` volume so debug and release artifacts do not stay on disk. Pass `-KeepCache` or `KEEP_CACHE=1` to keep that cache.

The Linux I/O benchmark `scripts/bench-io-limited.sh` uses a 1G loop file for MyDB and another 1G file for MySQL. It then deletes `mydb:io-bench`, `mydb-bench:ubuntu24`, and the matching BuildKit caches. Base images `mysql:8.0`, `rust`, `debian`, and `ubuntu` stay.

Clean this machine:

```powershell
.\scripts\clear-bench-disk.ps1
```

```bash
bash scripts/clear-bench-disk.sh
```

The script is safe to run again. It removes compose projects `mydb-bench` and `mydb-smoke`, I/O and fault-injection containers and volumes, the benchmark images above, and `target/io-bench*`. It leaves a business container named `mydb` in place and does not prune the entire Docker build cache.

Performance numbers belong to the measured run in [性能报告.md](../../性能报告.md).
