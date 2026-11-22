# OpenTelemetry 接入（对齐 MySQL 9）

MyDB 原生导出 OTLP/HTTP JSON，覆盖 metrics、traces、logs 三种信号。实现参考 MySQL 9 的 Telemetry 组件（`component_telemetry`），但保持单机、零热路径分配。

## 端点与配置

| 信号 | 路径 | 默认 |
|---|---|---|
| Metrics | `POST /v1/metrics` | 开启 |
| Traces | `POST /v1/traces` | 开启（仅慢查询/错误语句） |
| Logs | `POST /v1/logs` | 关闭（日志可能含业务数据） |

```yaml
observability:
  otlp:
    enabled: true
    endpoint: "https://collector.example.com:4318"
    interval_secs: 15
    timeout_secs: 5
    service_name: "mydb-prod"
    export_traces: true
    export_logs: false
    include_sql: false
    auth:
      method: "basic"          # none | basic | bearer
      username: "collector"
      password_file: "/run/secrets/otlp-password"
```

环境变量：`MYDB_OTLP_ENABLED`、`MYDB_OTLP_ENDPOINT`、`MYDB_OTLP_TRACES`、`MYDB_OTLP_LOGS`、`MYDB_OTLP_AUTH_METHOD`、`MYDB_OTLP_USERNAME`、`MYDB_OTLP_PASSWORD`/`_FILE`、`MYDB_OTLP_TOKEN`。

非 loopback endpoint 必须使用 HTTPS；证书校验由系统根证书完成，暂不支持自定义 CA/mTLS。

## 与 MySQL 9 的对照

| 能力 | MySQL 9 | MyDB 当前 |
|---|---|---|
| Metrics | Server meters + 可选指标 | `mydb.*` 指标 + Prometheus 同源 |
| Statement span | 所有语句，名称 `stmt` | 慢查询/错误语句，名称 `stmt`，含 `mysql.*` 与语义属性 |
| Session span | 连接关闭时导出 | 未实现 |
| Control span | 配置变更时导出 | 未实现 |
| Logs | 9.1+ 支持 `/v1/logs` | 支持，默认关闭 |
| Trace context | 接受 `traceparent` 查询属性 | 未实现（MySQL 协议层透传待补） |
| 传输 | OTLP/HTTP protobuf | OTLP/HTTP JSON |
| 鉴权 | headers/TLS | Basic、Bearer、headers，显式 Authorization 优先 |

## 隐私

- `include_sql: false` 时只导出 digest，不导出 SQL 原文、库名、错误原文。
- `export_logs: true` 时日志正文会进入 collector；密码、token、secret、authorization、credential 字段强制替换为 `<redacted>`，但业务日志正文仍可能含敏感数据，生产默认关闭。
- 管理 API `GET /api/v1/observability/otlp` 只返回状态与计数，不返回凭据。

## 验收

```bash
curl -u admin:token http://127.0.0.1:4306/api/v1/observability/otlp
```

`export_logs` 打开后，collector 应收到 `resourceLogs[].scopeLogs[].logRecords`，并与 `/v1/traces` 的 `stmt` span 使用同一 `service.name`/`service.instance.id`。
