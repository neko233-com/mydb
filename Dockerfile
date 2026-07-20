ARG RUST_IMAGE=rust:1-trixie
ARG RUNTIME_IMAGE=debian:13-slim
ARG TARGET_CACHE_ID=mydb-target-trixie
FROM ${RUST_IMAGE} AS builder
ARG TARGET_CACHE_ID
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY vendor ./vendor
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=${TARGET_CACHE_ID},target=/src/target \
    cargo build --release -p mydb-server -p mydb-cli -p mydb-migrate -p mydb-dump && \
    cp /src/target/release/mydb-server /tmp/mydb-server && \
    cp /src/target/release/mydb-cli /tmp/mydb-cli && \
    cp /src/target/release/mydb-migrate /tmp/mydb-migrate && \
    cp /src/target/release/mydbdump /tmp/mydbdump

FROM ${RUNTIME_IMAGE}
LABEL org.opencontainers.image.title="MyDB" \
      org.opencontainers.image.description="MySQL-compatible actor-ordered game database"

RUN groupadd --system --gid 10001 mydb && \
    useradd --system --uid 10001 --gid mydb --home-dir /var/lib/mydb --shell /usr/sbin/nologin mydb && \
    mkdir -p /etc/mydb /var/lib/mydb/backups && \
    chown -R mydb:mydb /var/lib/mydb

COPY --from=builder /tmp/mydb-server /usr/local/bin/mydb-server
COPY --from=builder /tmp/mydb-cli /usr/local/bin/mydb-cli
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
