#!/usr/bin/env bash
set -euo pipefail

MEMORY_LIMIT=${MYDB_TEST_MEMORY:-2g}
CPU_LIMIT=${MYDB_TEST_CPUS:-2}
BUILD_JOBS=${MYDB_TEST_JOBS:-2}
SKIP_RELEASE_BUILD=${SKIP_RELEASE_BUILD:-0}
ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUST_IMAGE=${MYDB_RUST_TEST_IMAGE:-rust:1-trixie}
TARGET_VOLUME=${MYDB_TEST_TARGET_VOLUME:-mydb-rust-target}
REGISTRY_VOLUME=${MYDB_TEST_REGISTRY_VOLUME:-mydb-rust-registry}
GIT_VOLUME=${MYDB_TEST_GIT_VOLUME:-mydb-rust-git}

command -v docker >/dev/null || { echo "docker is required" >&2; exit 2; }
docker info >/dev/null
docker volume create "$TARGET_VOLUME" >/dev/null
docker volume create "$REGISTRY_VOLUME" >/dev/null
docker volume create "$GIT_VOLUME" >/dev/null

commands=(
  "set -eu"
  "export CARGO_BUILD_JOBS=$BUILD_JOBS"
  "export CARGO_INCREMENTAL=0"
  "rustup component add rustfmt clippy"
  "cargo fmt --all -- --check"
  "cargo clippy --workspace --all-targets --locked -- -D warnings"
  "cargo test --workspace --locked -- --test-threads=1"
)
if [ "$SKIP_RELEASE_BUILD" != 1 ]; then
  commands+=("cargo build --release --locked -p mydb-server -p mydb-bench")
fi

printf -v command ' && %s' "${commands[@]}"
command=${command:4}
echo "Docker Rust gate: image=$RUST_IMAGE memory=$MEMORY_LIMIT cpus=$CPU_LIMIT jobs=$BUILD_JOBS"
docker run --rm --init \
  --cpus "$CPU_LIMIT" --memory "$MEMORY_LIMIT" --memory-swap "$MEMORY_LIMIT" \
  --mount "type=bind,source=$ROOT_DIR,target=/workspace" \
  --mount "type=volume,source=$TARGET_VOLUME,target=/workspace/target" \
  --mount "type=volume,source=$REGISTRY_VOLUME,target=/usr/local/cargo/registry" \
  --mount "type=volume,source=$GIT_VOLUME,target=/usr/local/cargo/git" \
  --workdir /workspace \
  "$RUST_IMAGE" sh -c "$command"
echo "Docker Rust gate passed"
