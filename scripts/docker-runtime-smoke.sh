#!/usr/bin/env bash
# Runtime smoke + OLAP scan measurement inside one Linux container.
# Requires scripts/docker-release-verify.sh to have produced /target/release.
set -euo pipefail

root="${CARGO_TARGET_DIR:-/target}/release"
work="/tmp/mydb-runtime-smoke"
rm -rf "$work"
mkdir -p "$work/data" "$work/backups" "$work/imports"

config="$work/config.yaml"
sed \
  -e "s#data_dir: \"/var/lib/mydb/data\"#data_dir: \"$work/data\"#" \
  -e "s#backup_dir: \"/var/lib/mydb/backups\"#backup_dir: \"$work/backups\"#" \
  -e "s#secure_file_priv: \"/var/lib/mydb/imports\"#secure_file_priv: \"$work/imports\"#" \
  -e "s#thread_count: 1#thread_count: 2#" \
  /src/configs/docker.yaml >"$config"

start_server() {
  local scan_threads="${1:-}"
  if [ -n "$scan_threads" ]; then
    MYDB_OLAP_SCAN_THREADS="$scan_threads" "$root/mydb-server" --config "$config" \
      >"$work/server.log" 2>&1 &
  else
    "$root/mydb-server" --config "$config" >"$work/server.log" 2>&1 &
  fi
  server_pid=$!
  for _ in $(seq 1 120); do
    if python3 - <<'PY' >/dev/null 2>&1
import urllib.request
urllib.request.urlopen('http://127.0.0.1:4306/healthz', timeout=1)
PY
    then
      return 0
    fi
    if ! kill -0 "$server_pid" 2>/dev/null; then
      echo "server exited during startup" >&2
      cat "$work/server.log" >&2
      exit 1
    fi
    sleep 0.5
  done
  echo "server did not become healthy" >&2
  cat "$work/server.log" >&2
  exit 1
}

stop_server() {
  kill -TERM "$server_pid" 2>/dev/null || true
  wait "$server_pid" 2>/dev/null || true
}
trap 'stop_server 2>/dev/null || true' EXIT

cli() {
  "$root/mydb-cli" -h 127.0.0.1 -P 3306 -u root -p root "$@"
}

start_server
cli -e "CREATE DATABASE smoke"
cli -D smoke -e "CREATE TABLE players (id BIGINT AUTO_INCREMENT PRIMARY KEY, actor VARCHAR(32) UNIQUE, score BIGINT)"
cli -D smoke -e "START TRANSACTION; INSERT INTO players(actor,score) VALUES ('a',10),('b',20); COMMIT"
cli -D smoke -e "UPDATE players SET score=score+5 WHERE actor='a'"
cli -D smoke -e "CREATE TABLE olap (id BIGINT PRIMARY KEY, v BIGINT, pad VARCHAR(16))"
python3 - "$work/imports/olap.csv" <<'PY'
import sys
path = sys.argv[1]
with open(path, 'w', encoding='ascii') as handle:
    for i in range(1, 10001):
        handle.write(f"{i},{i % 1000},pad{i % 100:02d}\n")
PY
cli -D smoke -e "LOAD DATA INFILE '$work/imports/olap.csv' INTO TABLE olap FIELDS TERMINATED BY ','"
count_raw=$(cli -D smoke -e "SELECT COUNT(*) FROM olap")
row_count=$(printf '%s' "$count_raw" | sed -n '2p' | tr -dc '0-9')
echo "row_count parsed=$row_count"
test "$row_count" = "10000"

query="SELECT COUNT(*),SUM(v),MIN(id),MAX(id) FROM olap WHERE v BETWEEN 100 AND 900 AND id > 0"
warmup=$(cli -D smoke -e "$query")

measure() {
  local label="$1"
  local total_ns=0
  local out
  for _ in 1 2 3 4 5; do
    start=$(date +%s%N)
    out=$(cli -D smoke -e "$query")
    end=$(date +%s%N)
    total_ns=$((total_ns + end - start))
  done
  echo "$label total_ms=$((total_ns / 1000000)) avg_ms=$((total_ns / 5000000))"
  printf '%s' "$out" >"$work/$label.out"
}

measure parallel
test "$warmup" = "$(cat "$work/parallel.out")"
stop_server

start_server 1
serial_warmup=$(cli -D smoke -e "$query")
measure serial
test "$serial_warmup" = "$(cat "$work/serial.out")"
test "$(cat "$work/parallel.out")" = "$(cat "$work/serial.out")"
echo "OLAP_RESULT_CONSISTENT"

# Committed data must survive a restart plus an unclean kill.
cli -D smoke -e "INSERT INTO players(actor,score) VALUES ('c',30)"
stop_server
start_server
players=$(cli -D smoke -e "SELECT COUNT(*),SUM(score) FROM players" | sed -n '2p' | tr -dc '0-9')
test "$players" = "365"
kill -KILL "$server_pid"
wait "$server_pid" 2>/dev/null || true
start_server
players=$(cli -D smoke -e "SELECT COUNT(*),SUM(score) FROM players" | sed -n '2p' | tr -dc '0-9')
test "$players" = "365"
echo "RUNTIME_SMOKE_OK"
