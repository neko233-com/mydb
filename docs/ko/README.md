# MyDB 운영 가이드

[中文](../zh/README.md) · [English](../en/README.md) · [日本語](../ja/README.md) · [저장소 개요](../../README.md)

MyDB 는 Rust 로 작성한 단일 노드 데이터베이스입니다. 클라이언트는 MySQL 8.4 프로토콜로 3306 에 접속합니다. Web SQL IDE 와 관리 API 는 4306 을 사용합니다. 스토리지 엔진은 Neko233 입니다. `ENGINE=InnoDB` 는 SQL 호환 이름이며 MySQL InnoDB 소스를 불러오지 않습니다.

복제 토폴로지, GTID, 읽기/쓰기 분리, Group Replication, Galera, 분산 XA 는 제품 범위 밖입니다. 인수 기록은 [CheckList.md](../../CheckList.md), SQL 대조표는 [SYNTAX_MATRIX.md](../../SYNTAX_MATRIX.md) 입니다. 두 문서는 중국어로 유지합니다.

## 빠른 시작

저장소 루트에 비밀번호 파일을 만든 뒤 서버를 시작합니다. 실제 비밀번호는 Git 에 넣지 않습니다.

```bash
cat > .env <<'EOF'
MYDB_ROOT_PASSWORD=replace-with-a-generated-secret
MYDB_ADMIN_PASSWORD=replace-with-another-generated-secret
EOF
docker compose up -d --build
mysql --protocol=TCP -h 127.0.0.1 -P 3306 -u root -p
```

| 변수 | 용도 |
|------|------|
| `MYDB_ROOT_PASSWORD` | SQL root 비밀번호. 필수 |
| `MYDB_ADMIN_PASSWORD` | HTTP 관리 비밀번호. 필수 |
| `MYDB_PORT` | SQL 포트. 기본값 3306 |
| `MYDB_HTTP_PORT` | 관리 포트. 기본값 4306 |
| `MYDB_DATA_DIR` | 데이터 디렉터리 |
| `MYDB_LOG_LEVEL` | `debug` / `info` / `warn` / `error` |
| `MYDB_GROUP_COMMIT_WINDOW_US` | Group Commit 창. 단위는 마이크로초 |
| `MYDB_ROOT_PASSWORD_FILE` | root 비밀번호를 파일에서 읽음 |
| `MYDB_ADMIN_PASSWORD_FILE` | admin 비밀번호를 파일에서 읽음 |

같은 비밀을 일반 변수와 `_FILE` 변수에 동시에 넣지 않습니다. `docker compose down` 은 서버를 멈추고 데이터 볼륨은 남깁니다.

## 설치와 업데이트

Linux / macOS 는 `scripts/install.sh`, Windows 는 `scripts/install.ps1` 을 사용합니다. Windows 기본 디렉터리는 `C:\Server\mydb\mydb-server` 이고 서비스 이름은 `MyDBServer` 입니다. 처음 설치하면 설정 디렉터리의 `secrets` 에 임의의 키를 쓰고, 업그레이드는 바이너리만 바꿉니다.

게시된 바이너리는 `mydb update --check` 로 확인하고 `mydb update` 로 갱신합니다. 설정, 데이터, 키는 유지됩니다.

## 프로덕션

기본 설치는 내부망을 위해 `0.0.0.0:3306` 과 `0.0.0.0:4306` 을 수신합니다. 공개망에서는 방화벽으로 출처를 제한하고, TLS 를 켜고, 키 파일을 사용합니다.

```bash
install -m 0700 -d /run/mydb-secrets
umask 077
openssl rand -base64 36 > /run/mydb-secrets/root
openssl rand -base64 36 > /run/mydb-secrets/admin
MYDB_ROOT_PASSWORD_FILE=/run/mydb-secrets/root \
MYDB_ADMIN_PASSWORD_FILE=/run/mydb-secrets/admin \
mydb-server --config /etc/mydb/production.yaml
```

`security.tls_cert`, `security.tls_key` 를 설정하고 `require_secure_transport: true` 로 둡니다. Docker 연기 테스트의 `root/root` 는 로컬 호환 테스트 전용입니다.

Prometheus 지표는 `http://127.0.0.1:4306/metrics` 입니다. 강한 비밀번호 모드에서는 관리 API 에 로그인한 뒤 Bearer token 을 보냅니다.

## 백업과 이전

`mydbdump` 는 테이블 잠금 없는 일관 스냅샷, 검증, 증분 백업을 수행합니다.

```bash
mydbdump backup --url 'mysql://root:password@127.0.0.1:3306' --database game --output backups/game-full
mydbdump verify --input backups/game-full
mydbdump restore --url 'mysql://root:password@127.0.0.1:3306' --database game --input backups/game-full
```

업그레이드 전후에 격리된 인스턴스에서 복구를 확인합니다. MySQL 에서 옮길 때는 `mydb-migrate` 를 사용합니다. 명령과 HTTP 백업 API 전문은 [README.md](../../README.md) 의 백업 및 이전 절에 있습니다.

## 테스트, 벤치마크, 디스크

로컬 확인:

```bash
cargo test --workspace
.\scripts\docker-smoke.ps1
bash scripts/docker-smoke.sh
.\scripts\docker-fault-injection.ps1
```

Docker Rust 게이트는 `scripts/test-docker.ps1` 과 `scripts/test-docker.sh` 입니다. 끝날 때 볼륨 `mydb-rust-target` 을 지워 debug 와 release 산출물이 디스크에 남지 않게 합니다. 캐시를 남기려면 `-KeepCache` 또는 `KEEP_CACHE=1` 을 사용합니다.

Linux I/O 벤치마크 `scripts/bench-io-limited.sh` 는 MyDB 용과 MySQL 용으로 각각 1G 루프 파일을 사용합니다. 끝나면 `mydb:io-bench`, `mydb-bench:ubuntu24` 와 해당 BuildKit 캐시를 삭제합니다. `mysql:8.0`, `rust`, `debian`, `ubuntu` 기본 이미지는 남깁니다.

이 머신의 벤치마크 잔여물을 지웁니다.

```powershell
.\scripts\clear-bench-disk.ps1
```

```bash
bash scripts/clear-bench-disk.sh
```

스크립트는 반복 실행할 수 있습니다. compose 프로젝트 `mydb-bench` 와 `mydb-smoke`, I/O 및 장애 주입 컨테이너와 볼륨, 위의 벤치마크 이미지, `target/io-bench*` 를 삭제합니다. 이름이 `mydb` 인 업무 컨테이너는 지우지 않으며 Docker 빌드 캐시 전체를 비우지도 않습니다.

성능 수치는 [性能报告.md](../../性能报告.md) 에 기록된 실측값입니다.
