#!/usr/bin/env bash
# Remove MyDB benchmark and test leftovers. Safe to run repeatedly.
# Does not remove rust, mysql, debian, golang, or ubuntu base images.
set -u

KEEP_CACHE=${KEEP_CACHE:-0}
INCLUDE_DEV_IMAGE=${INCLUDE_DEV_IMAGE:-0}
ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

remove_exact_container() {
  if docker ps --all --format '{{.Names}}' | grep -qx "$1"; then
    docker rm --force "$1" >/dev/null 2>&1 || true
    echo "removed container $1"
  fi
}

remove_prefix_containers() {
  docker ps --all --format '{{.Names}}' | while IFS= read -r name; do
    case "$name" in
      "$1"*) docker rm --force "$name" >/dev/null 2>&1 || true; echo "removed container $name" ;;
    esac
  done
}

remove_exact_volume() {
  if docker volume ls --format '{{.Name}}' | grep -qx "$1"; then
    docker volume rm --force "$1" >/dev/null 2>&1 || true
    echo "removed volume $1"
  fi
}

remove_prefix_volumes() {
  docker volume ls --format '{{.Name}}' | while IFS= read -r name; do
    case "$name" in
      "$1"*) docker volume rm --force "$name" >/dev/null 2>&1 || true; echo "removed volume $name" ;;
    esac
  done
}

remove_exact_image() {
  if docker image inspect "$1" >/dev/null 2>&1; then
    docker image rm --force "$1" >/dev/null 2>&1 || true
    echo "removed image $1"
  fi
}

if docker info >/dev/null 2>&1; then
  for project in mydb-bench mydb-smoke; do
    docker compose -p "$project" down --volumes --remove-orphans >/dev/null 2>&1 || true
    echo "compose project $project is down"
  done

  for name in mydb-io-desktop mysql80-io-desktop mydb-page-corrupt mydb-bench mysql-bench; do
    remove_exact_container "$name"
  done
  for prefix in mydb-io-bench- mysql-io-bench- mydb-fault- mydb-enospc- mydb-bench-; do
    remove_prefix_containers "$prefix"
  done

  for name in mydb-io-desktop-data mysql80-io-desktop-data mydb-smoke-page-corrupt-data; do
    remove_exact_volume "$name"
  done
  if [ "$KEEP_CACHE" != "1" ]; then
    for name in mydb-rust-target mydb-rust-registry mydb-rust-git; do
      remove_exact_volume "$name"
    done
  fi
  remove_prefix_volumes "mydb-fault-"

  for reference in mydb:io-bench mydb:bench mydb-bench:bench mydb-bench:ubuntu24; do
    remove_exact_image "$reference"
  done
  if [ "$INCLUDE_DEV_IMAGE" = "1" ]; then
    remove_exact_image "mydb:dev"
  fi

  for cache_id in mydb-target-trixie mydb-target-bookworm mydb-bench-bookworm-target; do
    docker buildx prune --force --filter "id=$cache_id" >/dev/null 2>&1 || true
  done
else
  echo "Docker is not available; only local result directories will be removed"
fi

for relative in target/io-bench target/io-bench-desktop; do
  path="$ROOT_DIR/$relative"
  if [ -d "$path" ]; then
    rm -rf -- "$path"
    echo "removed $relative"
  fi
done
if [ -d "$ROOT_DIR/target" ]; then
  find "$ROOT_DIR/target" -mindepth 1 -maxdepth 1 -type d -name 'io-bench*' -exec rm -rf -- {} +
fi

echo "MyDB bench disk cleanup finished"
