#!/usr/bin/env bash
set -euo pipefail

# Reproducible durability benchmark. It requires a Linux Docker daemon on the
# same host namespace because Docker Desktop hides its real block devices in a VM.
if [ "$(id -u)" -ne 0 ]; then
  echo "run as root: loopback filesystems and cgroup block-I/O limits are required" >&2
  exit 2
fi
if [ "$(uname -s)" != Linux ]; then
  echo "performance gate requires a Linux Docker host" >&2
  exit 2
fi

for command in curl docker python3 losetup mkfs.ext4 mount umount mountpoint timeout; do
  command -v "$command" >/dev/null || {
    echo "missing command: $command" >&2
    exit 2
  }
done
if [ "$(docker info --format '{{.Architecture}}')" != x86_64 ]; then
  echo "performance gate requires a linux/amd64 Docker daemon" >&2
  exit 2
fi
test -f /sys/fs/cgroup/cgroup.controllers && grep -qw io /sys/fs/cgroup/cgroup.controllers || {
  echo "cgroup v2 io controller is unavailable" >&2
  exit 2
}

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUN_ROOT=$(mktemp -d /var/tmp/mydb-io-bench.XXXXXX)
MYDB_MOUNT="$RUN_ROOT/mydb"
MYSQL_MOUNT="$RUN_ROOT/mysql"
MYDB_IMAGE_FILE="$RUN_ROOT/mydb.ext4"
MYSQL_IMAGE_FILE="$RUN_ROOT/mysql.ext4"
MYDB_CONTAINER="mydb-io-bench-$$"
MYSQL_CONTAINER="mysql-io-bench-$$"
MYDB_LOOP=""
MYSQL_LOOP=""

FILE_SIZE=${FILE_SIZE:-8G}
WRITE_BPS=${WRITE_BPS:-20mb}
READ_BPS=${READ_BPS:-100mb}
WRITE_IOPS=${WRITE_IOPS:-500}
READ_IOPS=${READ_IOPS:-2000}
BENCH_CPUS=${BENCH_CPUS:-1}
BENCH_MEMORY=${BENCH_MEMORY:-768m}
MYDB_PORT=${MYDB_PORT:-13316}
MYDB_HTTP_PORT=${MYDB_HTTP_PORT:-14316}
MYSQL_PORT=${MYSQL_PORT:-13306}
ACTORS=${ACTORS:-8}
WRITES_PER_ACTOR=${WRITES_PER_ACTOR:-500}
TABLE_COUNT=${TABLE_COUNT:-1}
TRANSACTION_SIZE=${TRANSACTION_SIZE:-10}
READS_PER_ACTOR=${READS_PER_ACTOR:-50}
PAYLOAD_BYTES=${PAYLOAD_BYTES:-256}
ROUNDS=${ROUNDS:-3}
MEASURE_SECONDS=${MEASURE_SECONDS:-60}
WRITE_MODE=${WRITE_MODE:-actor-batch}
RESULTS_DIR=${RESULTS_DIR:-"$ROOT_DIR/target/io-bench"}
SKIP_BUILD=${SKIP_BUILD:-0}

case "$MEASURE_SECONDS" in
  ''|*[!0-9]*) echo "MEASURE_SECONDS must be an integer between 1 and 60" >&2; exit 2 ;;
esac
if [ "$MEASURE_SECONDS" -lt 1 ] || [ "$MEASURE_SECONDS" -gt 60 ]; then
  echo "MEASURE_SECONDS must be between 1 and 60" >&2
  exit 2
fi
for value_name in ROUNDS ACTORS WRITES_PER_ACTOR TABLE_COUNT TRANSACTION_SIZE; do
  eval "value=\${$value_name}"
  case "$value" in
    ''|*[!0-9]*|0) echo "$value_name must be a positive integer" >&2; exit 2 ;;
  esac
done
for value_name in READS_PER_ACTOR PAYLOAD_BYTES; do
  eval "value=\${$value_name}"
  case "$value" in
    ''|*[!0-9]*) echo "$value_name must be a non-negative integer" >&2; exit 2 ;;
  esac
done

cleanup() {
  docker rm -f "$MYDB_CONTAINER" "$MYSQL_CONTAINER" >/dev/null 2>&1 || true
  if mountpoint -q "$MYDB_MOUNT"; then umount "$MYDB_MOUNT" || true; fi
  if mountpoint -q "$MYSQL_MOUNT"; then umount "$MYSQL_MOUNT" || true; fi
  if [ -n "$MYDB_LOOP" ]; then losetup -d "$MYDB_LOOP" >/dev/null 2>&1 || true; fi
  if [ -n "$MYSQL_LOOP" ]; then losetup -d "$MYSQL_LOOP" >/dev/null 2>&1 || true; fi
  case "$RUN_ROOT" in
    /var/tmp/mydb-io-bench.*) rm -rf -- "$RUN_ROOT" ;;
    *) echo "refusing to remove unexpected path: $RUN_ROOT" >&2 ;;
  esac
}
trap cleanup EXIT INT TERM

mkdir -p "$MYDB_MOUNT" "$MYSQL_MOUNT" "$RESULTS_DIR"
rm -f "$RESULTS_DIR"/mydb-*.json "$RESULTS_DIR"/mysql-*.json "$RESULTS_DIR/summary.json"
docker image inspect mysql:8.0 >/dev/null 2>&1 || docker pull mysql:8.0 >/dev/null
MYSQL_UID=$(docker run --rm --platform linux/amd64 --entrypoint id mysql:8.0 -u mysql)
truncate -s "$FILE_SIZE" "$MYDB_IMAGE_FILE"
truncate -s "$FILE_SIZE" "$MYSQL_IMAGE_FILE"
MYDB_LOOP=$(losetup --find --show "$MYDB_IMAGE_FILE")
MYSQL_LOOP=$(losetup --find --show "$MYSQL_IMAGE_FILE")
mkfs.ext4 -q -F "$MYDB_LOOP"
mkfs.ext4 -q -F "$MYSQL_LOOP"
mount -o noatime "$MYDB_LOOP" "$MYDB_MOUNT"
mount -o noatime "$MYSQL_LOOP" "$MYSQL_MOUNT"
chown 10001:10001 "$MYDB_MOUNT"
chown "$MYSQL_UID:$MYSQL_UID" "$MYSQL_MOUNT"
touch "$MYDB_MOUNT/.io-bench-source"

# Refuse Docker Desktop/remote-daemon path translation. Both the bind mount and
# the throttled block node must be from the daemon's own host.
if ! docker run --rm \
  --device-write-bps "$MYDB_LOOP:$WRITE_BPS" \
  --mount "type=bind,source=$MYDB_MOUNT,target=/probe" \
  alpine:3.22 test -f /probe/.io-bench-source; then
  echo "Docker daemon cannot see the host loop device/mount; use native Linux Docker Engine" >&2
  exit 2
fi
rm "$MYDB_MOUNT/.io-bench-source"

cd "$ROOT_DIR"
if [ "$SKIP_BUILD" = 1 ]; then
  for image in mysql:8.0 mydb:io-bench mydb-bench:ubuntu24; do
    docker image inspect "$image" >/dev/null || {
      echo "SKIP_BUILD=1 requires local image: $image" >&2
      exit 2
    }
  done
else
  docker build --platform linux/amd64 \
    --build-arg RUST_IMAGE=rust:1-bookworm \
    --build-arg RUNTIME_IMAGE=ubuntu:24.04 \
    --build-arg TARGET_CACHE_ID=mydb-target-bookworm \
    -t mydb:io-bench .
  docker build --platform linux/amd64 -f scripts/Dockerfile.bench -t mydb-bench:ubuntu24 .
fi

start_mydb() {
  docker run -d --name "$MYDB_CONTAINER" \
    --cpus "$BENCH_CPUS" --memory "$BENCH_MEMORY" --memory-swap "$BENCH_MEMORY" \
    --device-read-bps "$MYDB_LOOP:$READ_BPS" \
    --device-write-bps "$MYDB_LOOP:$WRITE_BPS" \
    --device-read-iops "$MYDB_LOOP:$READ_IOPS" \
    --device-write-iops "$MYDB_LOOP:$WRITE_IOPS" \
    --mount "type=bind,source=$MYDB_MOUNT,target=/var/lib/mydb" \
    -p "127.0.0.1:$MYDB_PORT:3306" \
    -p "127.0.0.1:$MYDB_HTTP_PORT:4306" \
    -e MYDB_ROOT_PASSWORD=root -e MYDB_ADMIN_PASSWORD=root \
    -e MYDB_THREAD_COUNT="$BENCH_CPUS" \
    -e MYDB_ENFORCE_STRONG_PASSWORDS=false \
    mydb:io-bench >/dev/null
}

start_mysql() {
  docker run -d --name "$MYSQL_CONTAINER" \
    --cpus "$BENCH_CPUS" --memory "$BENCH_MEMORY" --memory-swap "$BENCH_MEMORY" \
    --device-read-bps "$MYSQL_LOOP:$READ_BPS" \
    --device-write-bps "$MYSQL_LOOP:$WRITE_BPS" \
    --device-read-iops "$MYSQL_LOOP:$READ_IOPS" \
    --device-write-iops "$MYSQL_LOOP:$WRITE_IOPS" \
    --mount "type=bind,source=$MYSQL_MOUNT,target=/var/lib/mysql" \
    -p "127.0.0.1:$MYSQL_PORT:3306" \
    -e MYSQL_ROOT_PASSWORD=root -e MYSQL_ROOT_HOST=% \
    mysql:8.0 \
    --innodb-buffer-pool-size=256M \
    --innodb-flush-log-at-trx-commit=1 \
    --performance-schema=OFF \
    --skip-log-bin >/dev/null
}

wait_ready() {
  local container=$1
  local kind=$2
  local deadline=$((SECONDS + 240))
  until {
    if [ "$kind" = mydb ]; then
      docker exec "$container" mydb-server --healthcheck >/dev/null 2>&1
    else
      docker exec "$container" mysqladmin --protocol=TCP --host=127.0.0.1 --port=3306 \
        --user=root --password=root ping --silent >/dev/null 2>&1
    fi
  }; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      docker logs "$container" >&2 || true
      echo "$kind did not become ready" >&2
      exit 1
    fi
    sleep 2
  done
}

run_one() {
  local engine=$1
  local port=$2
  local round=$3
  local elapsed=$((SECONDS - BENCH_STARTED))
  local remaining=$((MEASURE_SECONDS - elapsed))
  if [ "$remaining" -le 0 ]; then
    echo "performance sampling exceeded ${MEASURE_SECONDS}s" >&2
    exit 1
  fi
  timeout --signal=TERM "${remaining}s" docker run --rm --network host \
    --platform linux/amd64 mydb-bench:ubuntu24 \
    --url "mysql://root:root@127.0.0.1:$port" \
    --actors "$ACTORS" \
    --writes-per-actor "$WRITES_PER_ACTOR" \
    --table-count "$TABLE_COUNT" \
    --transaction-size "$TRANSACTION_SIZE" \
    --reads-per-actor "$READS_PER_ACTOR" \
    --payload-bytes "$PAYLOAD_BYTES" \
    --write-mode "$WRITE_MODE" \
    >"$RESULTS_DIR/${engine}-${round}.json"
}

start_mydb
start_mysql
wait_ready "$MYDB_CONTAINER" mydb
wait_ready "$MYSQL_CONTAINER" mysql
MYDB_VERSION=$(docker exec "$MYDB_CONTAINER" mydb-server --version)
MYSQL_VERSION=$(docker exec "$MYSQL_CONTAINER" mysqld --version)
MYDB_IMAGE_ID=$(docker inspect --format '{{.Image}}' "$MYDB_CONTAINER")
MYSQL_IMAGE_ID=$(docker inspect --format '{{.Image}}' "$MYSQL_CONTAINER")

# Warmup plus all measured rounds have one hard one-minute wall-clock budget.
BENCH_STARTED=$SECONDS
run_one mydb "$MYDB_PORT" warmup
run_one mysql "$MYSQL_PORT" warmup
rm "$RESULTS_DIR/mydb-warmup.json" "$RESULTS_DIR/mysql-warmup.json"

for ((round = 1; round <= ROUNDS; round++)); do
  # Alternate order so thermal/background drift does not always favor one engine.
  if ((round % 2 == 1)); then
    run_one mydb "$MYDB_PORT" "$round"
    run_one mysql "$MYSQL_PORT" "$round"
  else
    run_one mysql "$MYSQL_PORT" "$round"
    run_one mydb "$MYDB_PORT" "$round"
  fi
done
SAMPLING_ELAPSED=$((SECONDS - BENCH_STARTED))
curl --fail --silent "http://127.0.0.1:$MYDB_HTTP_PORT/metrics" \
  >"$RESULTS_DIR/mydb-metrics.prom"

python3 - "$RESULTS_DIR" "$ROUNDS" "$WRITE_BPS" "$WRITE_IOPS" \
  "$MEASURE_SECONDS" "$SAMPLING_ELAPSED" "$MYDB_VERSION" "$MYSQL_VERSION" \
  "$MYDB_IMAGE_ID" "$MYSQL_IMAGE_ID" <<'PY' | tee "$RESULTS_DIR/summary.json"
import glob
import json
import statistics
import sys

(root, rounds, write_bps, write_iops, measure_seconds, sampling_elapsed,
 mydb_version, mysql_version, mydb_image, mysql_image) = sys.argv[1:]

def load(engine):
    rows = []
    for path in sorted(glob.glob(f"{root}/{engine}-*.json")):
        with open(path, encoding="utf-8") as handle:
            rows.append(json.load(handle))
    if len(rows) != int(rounds):
        raise SystemExit(f"expected {rounds} {engine} results, found {len(rows)}")
    return rows

def summarize(rows):
    fields = (
        "operations_per_second",
        "write_transactions_p50_us",
        "write_transactions_p95_us",
        "write_transactions_p99_us",
    )
    return {field: statistics.median(row[field] for row in rows) for field in fields}

mydb = summarize(load("mydb"))
mysql = summarize(load("mysql"))
print(json.dumps({
    "durability": "fsync-on-commit",
    "write_bps_per_engine": write_bps,
    "write_iops_per_engine": int(write_iops),
    "sampling_budget_seconds": int(measure_seconds),
    "sampling_elapsed_seconds": int(sampling_elapsed),
    "platform": "Docker Ubuntu 24.04 linux/amd64 (MySQL official 8.0 baseline)",
    "sql_engine": "InnoDB on both servers",
    "mydb_version": mydb_version,
    "mysql80_version": mysql_version,
    "mydb_image_id": mydb_image,
    "mysql80_image_id": mysql_image,
    "rounds": int(rounds),
    "mydb_median": mydb,
    "mysql80_median": mysql,
    "mydb_over_mysql_ops_ratio": mydb["operations_per_second"] / mysql["operations_per_second"],
}, indent=2))
PY

echo "I/O-limited benchmark passed; raw samples and summary: $RESULTS_DIR"
