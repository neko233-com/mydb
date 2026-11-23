#!/usr/bin/env bash
# Linux release + musl static gate. Run inside Dockerfile.verify:
#   docker run --rm -v "$PWD:/src:ro" -v mydb-target:/target \
#     -w /src -e CARGO_TARGET_DIR=/target mydb-verify:1.100 \
#     bash /src/scripts/docker-release-verify.sh
set -euo pipefail

cargo build --release --locked \
  -p mydb-server -p mydb-cli -p mydb-migrate -p mydb-dump
echo RELEASE_OK

CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
RUSTFLAGS="-C target-feature=+crt-static -C link-arg=-static" \
  cargo build --release --locked --target x86_64-unknown-linux-musl \
    -p mydb-server -p mydb-cli -p mydb-migrate -p mydb-dump
echo MUSL_BUILD_OK

root="${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release"
for binary in mydb-server mydb-cli mydb mydb-migrate mydbdump; do
  path="$root/$binary"
  file "$path"
  if readelf -l "$path" | grep -q 'INTERP'; then
    echo "$binary has a program interpreter" >&2
    exit 1
  fi
  if readelf -d "$path" 2>/dev/null | grep -q NEEDED; then
    echo "$binary has dynamic dependencies" >&2
    exit 1
  fi
done
echo MUSL_STATIC_OK
