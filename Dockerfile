ARG RUST_IMAGE=rust:1-bookworm@sha256:e12566d5abc17a0da741628782cdb504b68b8961f5bfcf677936bf3e8b32de29
ARG RUNTIME_IMAGE=debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
ARG TARGET_CACHE_ID=mydb-target-bookworm
FROM ${RUST_IMAGE} AS builder
ARG TARGET_CACHE_ID
ARG CARGO_BUILD_JOBS=1
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} CARGO_INCREMENTAL=0
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY vendor ./vendor
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=${TARGET_CACHE_ID},target=/src/target \
    cargo build --release --locked -p mydb-server -p mydb-migrate -p mydb-dump && \
    cargo build --release --locked -p mydb-cli --bins && \
    cp /src/target/release/mydb-server /tmp/mydb-server && \
    cp /src/target/release/mydb-cli /tmp/mydb-cli && \
    cp /src/target/release/mydb /tmp/mydb && \
    cp /src/target/release/mydb-migrate /tmp/mydb-migrate && \
    cp /src/target/release/mydbdump /tmp/mydbdump

FROM ${RUNTIME_IMAGE}
LABEL org.opencontainers.image.title="MyDB" \
      org.opencontainers.image.description="Single-node durable MySQL-compatible database"

RUN groupadd --system --gid 10001 mydb && \
    useradd --system --uid 10001 --gid mydb --home-dir /var/lib/mydb --shell /usr/sbin/nologin mydb && \
    mkdir -p /etc/mydb /var/lib/mydb/backups && \
    chown -R mydb:mydb /var/lib/mydb

COPY --from=builder /tmp/mydb-server /usr/local/bin/mydb-server
COPY --from=builder /tmp/mydb-cli /usr/local/bin/mydb-cli
COPY --from=builder /tmp/mydb /usr/local/bin/mydb
COPY --from=builder /tmp/mydb-migrate /usr/local/bin/mydb-migrate
COPY --from=builder /tmp/mydbdump /usr/local/bin/mydbdump
COPY configs/docker.yaml /etc/mydb/config.yaml

USER mydb
VOLUME ["/var/lib/mydb"]
EXPOSE 3306 4306
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=5 \
    CMD ["mydb-server", "--healthcheck"]
ENTRYPOINT ["mydb-server"]
CMD ["--config", "/etc/mydb/config.yaml"]
