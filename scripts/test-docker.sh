#!/usr/bin/env bash
set -euo pipefail

MEMORY_LIMIT=${MYDB_TEST_MEMORY:-2g}
CPU_LIMIT=${MYDB_TEST_CPUS:-2}
BUILD_JOBS=${MYDB_TEST_JOBS:-1}
DISK_GIB=${MYDB_TEST_DISK_GIB:-10}
MIN_FREE_GIB=${MYDB_TEST_MIN_FREE_GIB:-6}
TIMEOUT_MINUTES=${MYDB_TEST_TIMEOUT_MINUTES:-45}
SKIP_RELEASE_BUILD=${SKIP_RELEASE_BUILD:-0}
ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
RUST_IMAGE=${MYDB_RUST_TEST_IMAGE:-rust:1-trixie}
RUN_ID="$(date +%s)-$$-$RANDOM"
CONTAINER="mydb-quality-$RUN_ID"
TARGET_VOLUME="mydb-quality-target-$RUN_ID"
REGISTRY_VOLUME="mydb-quality-registry-$RUN_ID"
GIT_VOLUME="mydb-quality-git-$RUN_ID"
OUTPUT="$ROOT_DIR/target/docker-gate-$RUN_ID"
created_volumes=()
container_created=0
peak_bytes=0

for value in "$CPU_LIMIT" "$BUILD_JOBS"; do
  [[ $value =~ ^[1-8]$ ]] || { echo 'CPU/jobs must be integers between 1 and 8' >&2; exit 2; }
done
for value in "$DISK_GIB" "$MIN_FREE_GIB" "$TIMEOUT_MINUTES"; do
  [[ $value =~ ^[1-9][0-9]*$ ]] || { echo 'Budgets must be positive decimal integers' >&2; exit 2; }
done
(( DISK_GIB <= 64 && MIN_FREE_GIB <= 128 && TIMEOUT_MINUTES <= 180 )) || { echo 'Budget exceeds the supported limit' >&2; exit 2; }
command -v docker >/dev/null || { echo 'docker is required' >&2; exit 2; }
docker info >/dev/null
mkdir -p "$OUTPUT"

cleanup() {
  result=$?
  trap - EXIT
  set +e
  cleanup_ok=1
  if [[ $container_created == 1 ]]; then
    docker logs "$CONTAINER" >"$OUTPUT/cargo.log" 2>&1
    docker rm --force "$CONTAINER" >"$OUTPUT/cleanup.log" 2>&1 || cleanup_ok=0
    tail -n 25 "$OUTPUT/cargo.log"
  fi
  if [[ ${KEEP_CACHE:-0} != 1 ]]; then
    for volume in "${created_volumes[@]-}"; do
      [[ -n $volume ]] || continue
      docker volume rm "$volume" >>"$OUTPUT/cleanup.log" 2>&1 || cleanup_ok=0
    done
  fi
  if [[ $cleanup_ok != 1 && $result == 0 ]]; then result=1; fi
  printf 'exit_code=%s\ncleanup_succeeded=%s\npeak_observed_disk_bytes=%s\ndisk_budget_gib=%s\nminimum_free_gib=%s\ntimeout_minutes=%s\nimage=%s\n' \
    "$result" "$cleanup_ok" "$peak_bytes" "$DISK_GIB" "$MIN_FREE_GIB" "$TIMEOUT_MINUTES" "$RUST_IMAGE" >"$OUTPUT/receipt.txt"
  echo "Bounded gate receipt: $OUTPUT"
  exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

available_kib=$(docker run --rm --memory 128m --cpus 1 --network none --read-only --entrypoint sh "$RUST_IMAGE" -c 'df -Pk /' | awk 'END {print $4}')
[[ $available_kib =~ ^[0-9]+$ ]] || { echo 'Cannot inspect Docker disk capacity' >&2; exit 1; }
(( available_kib * 1024 >= (DISK_GIB + MIN_FREE_GIB) * 1073741824 )) || { echo 'Insufficient Docker disk headroom for the bounded quality gate' >&2; exit 1; }
for volume in "$TARGET_VOLUME" "$REGISTRY_VOLUME" "$GIT_VOLUME"; do
  docker volume create --label "com.neko233.mydb.quality-run=$RUN_ID" "$volume" >/dev/null
  created_volumes+=("$volume")
done

commands=(
  'set -eu'
  "export CARGO_BUILD_JOBS=$BUILD_JOBS"
  'export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0'
  'rustup component add rustfmt clippy'
  'cargo fmt --all -- --check'
  'cargo clippy --workspace --all-targets --locked -- -D warnings'
  'cargo test --workspace --locked -- --test-threads=1'
)
if [[ $SKIP_RELEASE_BUILD != 1 ]]; then
  commands+=('cargo build --release --locked -p mydb-server -p mydb-cli -p mydb-migrate -p mydb-dump -p mydb-bench')
fi
printf -v command ' && %s' "${commands[@]}"
command=${command:4}
echo "Docker Rust gate: image=$RUST_IMAGE memory=$MEMORY_LIMIT cpus=$CPU_LIMIT jobs=$BUILD_JOBS disk=${DISK_GIB}GiB timeout=${TIMEOUT_MINUTES}m"
docker create --init --name "$CONTAINER" --label "com.neko233.mydb.quality-run=$RUN_ID" \
  --cpus "$CPU_LIMIT" --memory "$MEMORY_LIMIT" --memory-swap "$MEMORY_LIMIT" \
  --pids-limit 256 --log-driver json-file --log-opt max-size=1m --log-opt max-file=3 \
  --security-opt no-new-privileges \
  --mount "type=bind,source=$ROOT_DIR,target=/workspace,readonly" \
  --mount "type=volume,source=$TARGET_VOLUME,target=/workspace/target" \
  --mount "type=volume,source=$REGISTRY_VOLUME,target=/usr/local/cargo/registry" \
  --mount "type=volume,source=$GIT_VOLUME,target=/usr/local/cargo/git" \
  --workdir /workspace "$RUST_IMAGE" sh -c "$command" >/dev/null
container_created=1
docker start "$CONTAINER" >/dev/null
deadline=$(( $(date +%s) + TIMEOUT_MINUTES * 60 ))
while [[ $(docker inspect --format '{{.State.Running}}' "$CONTAINER") == true ]]; do
  usage=$(docker exec "$CONTAINER" sh -c 'du -sk /workspace/target /usr/local/cargo/registry /usr/local/cargo/git 2>/dev/null' || true)
  if [[ $(printf '%s\n' "$usage" | wc -l | tr -d ' ') == 3 ]]; then
    volume_bytes=$(printf '%s\n' "$usage" | awk '{sum += $1} END {printf "%.0f", sum * 1024}')
    writable_bytes=$(docker inspect --size --format '{{.SizeRw}}' "$CONTAINER")
    bytes=$(( volume_bytes + writable_bytes ))
    if (( bytes > peak_bytes )); then peak_bytes=$bytes; fi
    (( bytes <= DISK_GIB * 1073741824 )) || { echo 'Docker Rust gate exceeded its disk budget' >&2; exit 1; }
    available_kib=$(docker exec "$CONTAINER" sh -c 'df -Pk /' | awk 'END {print $4}' || true)
    if [[ $available_kib =~ ^[0-9]+$ ]]; then
      (( available_kib * 1024 >= MIN_FREE_GIB * 1073741824 )) || { echo 'Docker Rust gate reached the reserved disk headroom' >&2; exit 1; }
    fi
  fi
  (( $(date +%s) < deadline )) || { echo 'Docker Rust gate exceeded its timeout' >&2; exit 1; }
  sleep 5
done
[[ $(docker inspect --format '{{.State.ExitCode}}' "$CONTAINER") == 0 ]] || { echo 'Docker Rust gate failed' >&2; exit 1; }
echo 'Docker Rust gate passed'
