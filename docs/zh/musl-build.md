# Linux musl 静态构建规范

MyDB 的 Linux 发布产物必须使用 `x86_64-unknown-linux-musl` 静态链接，避免依赖目标机的 glibc 版本。Windows 开发机继续使用 `stable-x86_64-pc-windows-gnullvm`，不用于生成 Linux 产物。

## 本机构建

```bash
sudo apt-get update
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl

export CC_x86_64_unknown_linux_musl=musl-gcc
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc

cargo build --release --locked --target x86_64-unknown-linux-musl \
  -p mydb-server -p mydb-cli -p mydb-migrate -p mydb-dump
```

## 验收

```bash
root=target/x86_64-unknown-linux-musl/release
for bin in mydb-server mydb-cli mydb mydb-migrate mydbdump; do
  file "$root/$bin" | grep -q 'statically linked'
  ! readelf -d "$root/$bin" | grep -q NEEDED
done
```

`file` 必须显示 `statically linked`，`readelf -d` 不得出现 `NEEDED`。任一失败即阻断发布。

## 依赖体检

新增依赖前执行：

```bash
cargo tree --target x86_64-unknown-linux-musl -e normal
```

- 优先纯 Rust 实现；TLS 统一 `rustls`。
- 禁止新增 `openssl-sys`、`libz-sys`、`libsqlite3-sys` 等系统 C 库绑定。
- `zstd-sys`、`ring` 等确需 C/汇编的 crate 必须静态编入二进制，目标机不得要求安装 `.so`/`.dll`；新增时在提交说明中记录原因与体积影响。

## CI

`.github/workflows/build-linux-musl.yml` 在 `workflow_dispatch` 和 `v*` tag 上构建，执行静态链接校验并上传 `mydb-<version>-linux-x86_64-musl.tar.gz` 与 `.sha256`。该工作流只产出候选物，创建 GitHub Release 仍需人工确认。

## 容器

新增或修改 Dockerfile 时，运行层优先 Alpine/scratch 等 musl 基础镜像，不默认使用 Debian/glibc；必须用 `podman` 实际构建并通过 `/healthz`、`/readyz` 后才可提交。
