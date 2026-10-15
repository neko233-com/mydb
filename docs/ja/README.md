# MyDB 運用ガイド

[中文](../zh/README.md) · [English](../en/README.md) · [한국어](../ko/README.md) · [リポジトリ概要](../../README.md)

MyDB は Rust で書かれた単一ノードのデータベースです。クライアントは MySQL 8.4 プロトコルで 3306 に接続します。Web SQL IDE と管理 API は 4306 を使います。ストレージエンジンは Neko233 です。`ENGINE=InnoDB` は SQL 上の互換名であり、MySQL InnoDB のソースは読み込みません。

レプリケーション、GTID、読み書き分離、Group Replication、Galera、分散 XA は対象外です。受入記録は [CheckList.md](../../CheckList.md)、SQL 対応表は [SYNTAX_MATRIX.md](../../SYNTAX_MATRIX.md) です。どちらも中国語のままです。

## クイックスタート

リポジトリ直下にパスワードファイルを作り、サーバを起動します。実際のパスワードは Git に入れません。

```bash
cat > .env <<'EOF'
MYDB_ROOT_PASSWORD=replace-with-a-generated-secret
MYDB_ADMIN_PASSWORD=replace-with-another-generated-secret
EOF
docker compose up -d --build
mysql --protocol=TCP -h 127.0.0.1 -P 3306 -u root -p
```

| 変数 | 用途 |
|------|------|
| `MYDB_ROOT_PASSWORD` | SQL の root パスワード。必須 |
| `MYDB_ADMIN_PASSWORD` | HTTP 管理パスワード。必須 |
| `MYDB_PORT` | SQL ポート。既定は 3306 |
| `MYDB_HTTP_PORT` | 管理ポート。既定は 4306 |
| `MYDB_DATA_DIR` | データディレクトリ |
| `MYDB_LOG_LEVEL` | `debug` / `info` / `warn` / `error` |
| `MYDB_GROUP_COMMIT_WINDOW_US` | Group Commit の窓。単位はマイクロ秒 |
| `MYDB_ROOT_PASSWORD_FILE` | root パスワードをファイルから読む |
| `MYDB_ADMIN_PASSWORD_FILE` | admin パスワードをファイルから読む |

同じ秘密を通常の変数と `_FILE` 変数の両方に設定しません。`docker compose down` はサーバを止め、データボリュームは残します。

## インストールと更新

Linux / macOS は `scripts/install.sh`、Windows は `scripts/install.ps1` を使います。Windows の既定ディレクトリは `C:\Server\mydb\mydb-server`、サービス名は `MyDBServer` です。初回は設定ディレクトリの `secrets` にランダムな鍵を書き、更新はバイナリだけを置き換えます。

公開済みバイナリは `mydb update --check` で確認し、`mydb update` で更新します。設定、データ、鍵は残ります。

## 本番配置

ネイティブインストールは社内ネットワーク向けに `0.0.0.0:3306` と `0.0.0.0:4306` を待ち受けます。公開ネットワークではファイアウォールで送信元を制限し、TLS を有効にし、鍵ファイルを使います。

```bash
install -m 0700 -d /run/mydb-secrets
umask 077
openssl rand -base64 36 > /run/mydb-secrets/root
openssl rand -base64 36 > /run/mydb-secrets/admin
MYDB_ROOT_PASSWORD_FILE=/run/mydb-secrets/root \
MYDB_ADMIN_PASSWORD_FILE=/run/mydb-secrets/admin \
mydb-server --config /etc/mydb/production.yaml
```

`security.tls_cert`、`security.tls_key` を設定し、`require_secure_transport: true` にします。Docker の疎通試験で使う `root/root` はローカル互換試験専用です。

Prometheus 指標は `http://127.0.0.1:4306/metrics` です。強いパスワードでは管理 API にログインしてから Bearer token を付けます。

## バックアップと移行

`mydbdump` はテーブルロックなしの一貫スナップショット、検証、増分バックアップを行います。

```bash
mydbdump backup --url 'mysql://root:password@127.0.0.1:3306' --database game --output backups/game-full
mydbdump verify --input backups/game-full
mydbdump restore --url 'mysql://root:password@127.0.0.1:3306' --database game --input backups/game-full
```

アップグレードの前後に、隔離したインスタンスで復元を確認します。MySQL からの移行は `mydb-migrate` を使います。コマンドと HTTP バックアップ API の全文は [README.md](../../README.md) のバックアップと移行の節にあります。

## テスト、ベンチマーク、ディスク

ローカル確認:

```bash
cargo test --workspace
.\scripts\docker-smoke.ps1
bash scripts/docker-smoke.sh
.\scripts\docker-fault-injection.ps1
```

Docker 上の Rust ゲートは `scripts/test-docker.ps1` と `scripts/test-docker.sh` です。終了時にボリューム `mydb-rust-target` を削除し、debug と release の生成物を残しません。キャッシュを残すときは `-KeepCache` または `KEEP_CACHE=1` を指定します。

Linux の I/O ベンチマーク `scripts/bench-io-limited.sh` は、MyDB 用と MySQL 用にそれぞれ 1G のループファイルを使います。終了時に `mydb:io-bench`、`mydb-bench:ubuntu24`、対応する BuildKit キャッシュを削除します。`mysql:8.0`、`rust`、`debian`、`ubuntu` のベースイメージは残します。

このマシンのベンチマーク残骸を消す:

```powershell
.\scripts\clear-bench-disk.ps1
```

```bash
bash scripts/clear-bench-disk.sh
```

スクリプトは繰り返し実行できます。compose プロジェクト `mydb-bench` と `mydb-smoke`、I/O 試験と障害注入のコンテナとボリューム、上記のベンチマークイメージ、`target/io-bench*` を削除します。名前が `mydb` の業務コンテナは削除せず、Docker のビルドキャッシュ全体も消しません。

性能値は [性能报告.md](../../性能报告.md) に記録された実測です。
