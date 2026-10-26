# Metrics233 与 Grafana 安全性能监控

MyDB 原生提供经过认证的 Prometheus `/metrics`，不需要 exporter sidecar。
MetricsPanel233 的 **Collectors → Quick connect → MyDB** 可实际探测并预览采集器、
性能仪表盘和持续告警，确认后一次保存。Grafana 通过 Prometheus 数据源查询历史数据，
不能把 MyDB 的原始 `/metrics` 直接当作 Grafana 查询接口。
MyDB 类型仅接受 Bearer 采集凭据，禁用数据库用户名/密码转发；非 loopback 地址必须使用 HTTPS。

```yaml
observability:
  metrics_token_file: /run/secrets/mydb-metrics-token
```

凭据文件为 32–8192 字节可打印非空白 ASCII，允许末尾换行。限制文件权限，仅服务身份
和运维人员可读；也可用 `MYDB_METRICS_TOKEN_FILE` 指定路径。轮换需重启服务。
读取最多 8195 字节，超过文件大小限制即拒绝启动，避免错误配置导致无界内存占用。
监控端的 Bearer token 填文件内容；此凭据只能读 `/metrics`，不能用于 SQL、管理 API、
配置或备份。不要使用数据库 root 密码或管理账号 session 作为长效采集凭据。

HTTP 管理权限来自 `http.admin_username/admin_password`，登录响应的 `administrator` 字段
明确标识该角色。普通 SQL 用户登录仅保留数据库授权，不能访问配置、备份、服务状态、
Agent 或全局指标，管理请求返回 403；未登录或会话过期返回 401。服务管理员与 SQL 用户
可以退出各自会话；配置重载使已有会话失效。不要把 SQL 登录成功当作管理权限证明。

## 公网 HTTPS

MyDB 管理 HTTP 保持监听 `127.0.0.1:4306`。用可信证书的反向代理仅转发 `/metrics`，
其他路径返回 404。Caddy 示例：

```caddyfile
db-metrics.example.com {
    @metrics {
        path /metrics
        method GET HEAD
    }
    handle @metrics {
        reverse_proxy 127.0.0.1:4306
    }
    handle {
        respond 404
    }
}
```

后端校验 Bearer 凭据；HTTP 明文仅用于同机 loopback，不用于公网凭据传输。
不公开 3306、4306 或整个管理站点。域名、证书、代理部署由实际环境单独配置。

标准 Prometheus 采集配置：

```yaml
scrape_configs:
  - job_name: mydb
    scrape_interval: 15s
    scheme: https
    metrics_path: /metrics
    authorization:
      type: Bearer
      credentials_file: /run/secrets/mydb-metrics-token
    static_configs:
      - targets: [db-metrics.example.com]
```

Grafana 使用该 Prometheus，导入 `examples/monitoring/grafana-mydb.json`。
也可把 Grafana 的 Prometheus datasource URL 设置为
`https://metrics.example.com/prometheus`，使用 MetricsPanel233 local 模式的 Viewer
账号与 HTTP Basic；账号保存在 Grafana secure settings，不使用管理 token。
MyDB 的采集凭据、MetricsPanel233 的上报凭据、Grafana 的只读查询身份相互独立。

## 指标口径与预警

固定指标覆盖连接、查询/错误/慢查询速率、P95/P99 延迟、锁等待/死锁、提交队列、组提交、
WAL 刷盘、存储/检查点错误与审计健康。Counter 使用速率，Gauge 显示当前值；缺样和
采集失败分开处理。安全指标包括登录失败累计次数、TLS 和强密码策略开关、LOCAL INFILE。
强密码策略开关不能证明所有历史账号都已通过密码检查。

TLS 配置指标描述 MySQL 协议监听器；不能用它推断外层 HTTPS 代理是否配置正确。
启用/禁用 OTLP 需要重启；已启用时可按现有配置重载机制调整目标与间隔。
带认证头的 OTLP 推送仅允许 HTTPS，loopback HTTP 除外，URL 不接受内嵌凭据。

公开指标不包含密码、SQL 文本或用户名，statement digest 仅导出 SHA-256 标识且最多
20 项。OTLP 的 `include_sql: false` 默认省略原 SQL、库名和原始错误文本；本地慢查询日志
仍可能含 SQL，需按数据库敏感日志保护。`include_sql: true` 是显式敏感信息导出。

MetricsPanel233 可生成持续告警：采集失败、连接容量、审计异常、持久化错误、持续登录
失败和安全配置风险。缺样保持先前状态，恢复确认 1 分钟、通知冷却 1 小时。未配置联系点
时只有本地告警事件；生成规则不代表已发送通知。公网访问、实际 Grafana 展示和通知送达
分别验收。具体版本、平台及公开发布结果以对应 Release、CI 和验收回执为准。
