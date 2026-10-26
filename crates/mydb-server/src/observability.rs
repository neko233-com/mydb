//! OpenTelemetry OTLP/HTTP export and Prometheus exposition.
//!
//! The exporter runs on its own task and never touches the query hot path. It
//! reads counters that the wire layer already maintains, so enabling it costs
//! one snapshot per interval instead of per-statement instrumentation.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use mydb_config::OtlpSection;
use mydb_wire::{
    AuditLogSnapshot, QueryLatencySnapshot, SlowQueryLogSnapshot, SlowQuerySnapshot,
    StatementDigestSnapshot, WireStatsSnapshot,
};

const OTLP_METRICS_PATH: &str = "/v1/metrics";
const OTLP_TRACES_PATH: &str = "/v1/traces";
/// Upper bound of exported spans per interval. A burst of slow statements
/// must not produce an unbounded payload.
const MAX_EXPORTED_SPANS: usize = 256;
/// Number of statement digests exported as Prometheus labels. Prometheus label
/// cardinality has to stay bounded.
const MAX_PROMETHEUS_DIGESTS: usize = 20;

/// Runtime configuration of the exporter. A configuration reload replaces the
/// whole struct, so the exporter can be retargeted without a restart.
#[derive(Debug, Clone)]
pub struct OtlpSettings {
    pub enabled: bool,
    pub endpoint: String,
    pub interval: Duration,
    pub timeout: Duration,
    pub service_name: String,
    pub headers: BTreeMap<String, String>,
    pub export_traces: bool,
    pub include_sql: bool,
}

impl OtlpSettings {
    pub fn from_section(section: &OtlpSection) -> Self {
        Self {
            enabled: section.enabled,
            endpoint: section.endpoint.trim_end_matches('/').to_string(),
            interval: Duration::from_secs(section.interval_secs.max(1)),
            timeout: Duration::from_secs(section.timeout_secs.max(1)),
            service_name: section.service_name.clone(),
            headers: section.headers.clone(),
            export_traces: section.export_traces,
            include_sql: section.include_sql,
        }
    }
}

#[derive(Debug, Default)]
struct OtlpCounters {
    metric_exports: AtomicU64,
    span_exports: AtomicU64,
    failures: AtomicU64,
    last_success_unix_ms: AtomicU64,
    last_error: Mutex<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct OtlpStatusSnapshot {
    pub enabled: bool,
    pub endpoint: String,
    pub interval_seconds: u64,
    pub service_name: String,
    pub export_traces: bool,
    pub include_sql: bool,
    pub metric_exports: u64,
    pub span_exports: u64,
    pub failures: u64,
    pub last_success_unix_ms: u64,
    pub last_error: Option<String>,
}

/// Counters published in one export interval.
pub struct MonitoringSnapshot<'a> {
    pub wire: &'a WireStatsSnapshot,
    pub storage: &'a mydb_storage::StorageStatsSnapshot,
    pub audit: &'a AuditLogSnapshot,
    pub latency: &'a QueryLatencySnapshot,
    pub slow_queries_total: u64,
    pub digest_entries: usize,
    pub digest_dropped: u64,
}

/// Identity of the exporting server instance.
#[derive(Debug, Clone)]
pub struct ServerIdentity {
    pub host: String,
    pub port: u16,
    pub version: String,
}

#[derive(Clone)]
pub struct OtlpExporter {
    settings: Arc<RwLock<OtlpSettings>>,
    counters: Arc<OtlpCounters>,
    last_exported_slow_query_id: Arc<AtomicU64>,
}

impl OtlpExporter {
    pub fn new(section: &OtlpSection) -> Self {
        Self {
            settings: Arc::new(RwLock::new(OtlpSettings::from_section(section))),
            counters: Arc::new(OtlpCounters::default()),
            last_exported_slow_query_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Apply a reloaded configuration. The next interval picks up the new
    /// endpoint, interval or trace settings.
    pub fn apply(&self, section: &OtlpSection) {
        *self.settings.write() = OtlpSettings::from_section(section);
    }

    pub fn status(&self) -> OtlpStatusSnapshot {
        let settings = self.settings.read().clone();
        OtlpStatusSnapshot {
            enabled: settings.enabled,
            endpoint: settings.endpoint.clone(),
            interval_seconds: settings.interval.as_secs(),
            service_name: settings.service_name.clone(),
            export_traces: settings.export_traces,
            include_sql: settings.include_sql,
            metric_exports: self.counters.metric_exports.load(Ordering::Relaxed),
            span_exports: self.counters.span_exports.load(Ordering::Relaxed),
            failures: self.counters.failures.load(Ordering::Relaxed),
            last_success_unix_ms: self.counters.last_success_unix_ms.load(Ordering::Relaxed),
            last_error: self.counters.last_error.lock().clone(),
        }
    }

    async fn export_once(
        &self,
        snapshot: &MonitoringSnapshot<'_>,
        slow_queries: &[SlowQuerySnapshot],
        server: &ServerIdentity,
    ) {
        let settings = self.settings.read().clone();
        if !settings.enabled {
            return;
        }
        let timestamp = unix_nanos();
        let metrics_payload = otlp_metrics_payload(&settings, snapshot, server, timestamp);
        match post_json(
            &format!("{}{}", settings.endpoint, OTLP_METRICS_PATH),
            &settings.headers,
            &metrics_payload,
            settings.timeout,
        )
        .await
        {
            Ok(status) if (200..300).contains(&status) => {
                self.counters.metric_exports.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .last_success_unix_ms
                    .store(unix_millis(), Ordering::Relaxed);
                *self.counters.last_error.lock() = None;
            }
            Ok(status) => {
                self.record_failure(format!("OTLP metrics endpoint returned HTTP {status}"));
            }
            Err(error) => self.record_failure(error.to_string()),
        }

        if !settings.export_traces {
            return;
        }
        let spans = collect_spans(slow_queries, &self.last_exported_slow_query_id);
        if spans.is_empty() {
            return;
        }
        let traces_payload = otlp_traces_payload(&settings, &spans, server);
        match post_json(
            &format!("{}{}", settings.endpoint, OTLP_TRACES_PATH),
            &settings.headers,
            &traces_payload,
            settings.timeout,
        )
        .await
        {
            Ok(status) if (200..300).contains(&status) => {
                self.counters.span_exports.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .last_success_unix_ms
                    .store(unix_millis(), Ordering::Relaxed);
                *self.counters.last_error.lock() = None;
            }
            Ok(status) => {
                self.record_failure(format!("OTLP traces endpoint returned HTTP {status}"));
            }
            Err(error) => self.record_failure(error.to_string()),
        }
    }

    fn record_failure(&self, message: String) {
        self.counters.failures.fetch_add(1, Ordering::Relaxed);
        tracing::warn!("{message}");
        *self.counters.last_error.lock() = Some(message);
    }
}

/// Background exporter loop. The task exits when the server shuts down.
pub async fn run_otlp_exporter(
    exporter: OtlpExporter,
    wire_stats: Arc<mydb_wire::WireStats>,
    storage: Arc<mydb_storage::StorageEngineManager>,
    audit_log: Arc<mydb_wire::AuditLog>,
    server_host: String,
    server_port: u16,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let version = env!("CARGO_PKG_VERSION").to_string();
    loop {
        let interval = exporter.settings.read().interval;
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
        let wire = wire_stats.snapshot();
        let storage_stats = storage.stats();
        let audit = audit_log.snapshot();
        let latency = wire_stats.query_latency();
        let slow_queries = wire_stats.slow_queries();
        let snapshot = MonitoringSnapshot {
            wire: &wire,
            storage: &storage_stats,
            audit: &audit,
            latency: &latency,
            slow_queries_total: wire_stats.slow_queries_total(),
            digest_entries: wire_stats.statement_digest_count(),
            digest_dropped: wire_stats.statement_digest_dropped(),
        };
        let server = ServerIdentity {
            host: server_host.clone(),
            port: server_port,
            version: version.clone(),
        };
        exporter
            .export_once(&snapshot, &slow_queries, &server)
            .await;
    }
}

async fn post_json(
    url: &str,
    headers: &BTreeMap<String, String>,
    payload: &Value,
    timeout: Duration,
) -> anyhow::Result<u16> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| anyhow::anyhow!("failed to build OTLP HTTP client: {error}"))?;
    let mut request = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(payload)?);
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("OTLP export request failed: {}", error.without_url()))?;
    Ok(response.status().as_u16())
}

fn unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn string_attribute(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

fn int_attribute(key: &str, value: i64) -> Value {
    json!({"key": key, "value": {"intValue": value.to_string()}})
}

fn resource_attributes(settings: &OtlpSettings, server: &ServerIdentity) -> Value {
    json!([
        string_attribute("service.name", &settings.service_name),
        string_attribute("service.version", &server.version),
        string_attribute("host.name", &hostname()),
        string_attribute("server.address", &server.host),
        int_attribute("server.port", i64::from(server.port)),
    ])
}

fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "mydb".to_string())
}

fn gauge_metric(name: &str, description: &str, unit: &str, value: u64, timestamp: u64) -> Value {
    json!({
        "name": name,
        "description": description,
        "unit": unit,
        "gauge": {
            "dataPoints": [{
                "asInt": value.to_string(),
                "timeUnixNano": timestamp.to_string(),
            }]
        }
    })
}

fn counter_metric(name: &str, description: &str, unit: &str, value: u64, timestamp: u64) -> Value {
    json!({
        "name": name,
        "description": description,
        "unit": unit,
        "sum": {
            "aggregationTemporality": 2,
            "isMonotonic": true,
            "dataPoints": [{
                "asInt": value.to_string(),
                "timeUnixNano": timestamp.to_string(),
            }]
        }
    })
}

fn latency_histogram_metric(
    name: &str,
    description: &str,
    unit: &str,
    latency: &QueryLatencySnapshot,
    timestamp: u64,
) -> Value {
    let bounds: Vec<f64> = latency
        .bounds_micros
        .iter()
        .map(|bound| *bound as f64)
        .collect();
    let bucket_counts: Vec<String> = latency
        .bucket_counts()
        .iter()
        .map(|count| count.to_string())
        .collect();
    json!({
        "name": name,
        "description": description,
        "unit": unit,
        "histogram": {
            "aggregationTemporality": 2,
            "dataPoints": [{
                "count": latency.count.to_string(),
                "sum": latency.sum_micros as f64,
                "bucketCounts": bucket_counts,
                "explicitBounds": bounds,
                "min": latency.min_micros as f64,
                "max": latency.max_micros as f64,
                "timeUnixNano": timestamp.to_string(),
            }]
        }
    })
}

fn otlp_metrics_payload(
    settings: &OtlpSettings,
    snapshot: &MonitoringSnapshot<'_>,
    server: &ServerIdentity,
    timestamp: u64,
) -> Value {
    let wire = snapshot.wire;
    let storage = snapshot.storage;
    let audit = snapshot.audit;
    let latency = snapshot.latency;
    let metrics = vec![
        gauge_metric(
            "mydb.connections.active",
            "Currently open MySQL protocol connections.",
            "1",
            wire.active_connections as u64,
            timestamp,
        ),
        gauge_metric(
            "mydb.locks.active",
            "Locks currently held by running transactions.",
            "1",
            wire.active_locks as u64,
            timestamp,
        ),
        gauge_metric(
            "mydb.storage.buffer_pool.pages",
            "Resident pages in the buffer pool.",
            "1",
            storage.buffer_pool_pages as u64,
            timestamp,
        ),
        gauge_metric(
            "mydb.write.commit_queue.depth",
            "Requests waiting for the next group commit.",
            "1",
            storage.commit_queue_depth as u64,
            timestamp,
        ),
        counter_metric(
            "mydb.connections.total",
            "Accepted MySQL protocol connections.",
            "1",
            wire.total_connections,
            timestamp,
        ),
        counter_metric(
            "mydb.queries.total",
            "Executed statements.",
            "1",
            wire.queries,
            timestamp,
        ),
        counter_metric(
            "mydb.queries.errors",
            "Statements that returned an error.",
            "1",
            wire.query_errors,
            timestamp,
        ),
        counter_metric(
            "mydb.queries.slow",
            "Statements slower than long_query_time or failed.",
            "1",
            snapshot.slow_queries_total,
            timestamp,
        ),
        counter_metric(
            "mydb.locks.waits",
            "Lock waits observed.",
            "1",
            wire.lock_waits,
            timestamp,
        ),
        counter_metric(
            "mydb.locks.timeouts",
            "Lock wait timeouts.",
            "1",
            wire.lock_timeouts,
            timestamp,
        ),
        counter_metric(
            "mydb.locks.deadlocks",
            "Deadlocks detected.",
            "1",
            wire.deadlocks,
            timestamp,
        ),
        counter_metric(
            "mydb.storage.reads",
            "Page reads served by the storage engine.",
            "1",
            storage.reads,
            timestamp,
        ),
        counter_metric(
            "mydb.storage.writes",
            "Write batches applied by the storage engine.",
            "1",
            storage.writes,
            timestamp,
        ),
        counter_metric(
            "mydb.storage.errors",
            "Storage engine errors.",
            "1",
            storage.errors,
            timestamp,
        ),
        counter_metric(
            "mydb.commit.group_commits",
            "Group commits performed.",
            "1",
            storage.group_commits,
            timestamp,
        ),
        counter_metric(
            "mydb.commit.grouped_requests",
            "Requests batched into group commits.",
            "1",
            storage.grouped_requests,
            timestamp,
        ),
        counter_metric(
            "mydb.checkpoint.total",
            "Checkpoints performed.",
            "1",
            storage.checkpoints,
            timestamp,
        ),
        counter_metric(
            "mydb.checkpoint.errors",
            "Failed checkpoints.",
            "1",
            storage.checkpoint_errors,
            timestamp,
        ),
        counter_metric(
            "mydb.wal.sync.microseconds",
            "Time spent inside WAL fsync.",
            "us",
            storage.wal_sync_micros,
            timestamp,
        ),
        counter_metric(
            "mydb.checkpoint.microseconds",
            "Time spent inside checkpointing.",
            "us",
            storage.checkpoint_micros,
            timestamp,
        ),
        counter_metric(
            "mydb.audit.events",
            "Accepted audit events.",
            "1",
            audit.accepted,
            timestamp,
        ),
        counter_metric(
            "mydb.audit.rejections",
            "Rejected audit events.",
            "1",
            audit.rejected,
            timestamp,
        ),
        counter_metric(
            "mydb.statement.digests",
            "Tracked statement digests.",
            "1",
            snapshot.digest_entries as u64,
            timestamp,
        ),
        counter_metric(
            "mydb.statement.digests.dropped",
            "Statement digests evicted because the registry was full.",
            "1",
            snapshot.digest_dropped,
            timestamp,
        ),
        latency_histogram_metric(
            "mydb.query.latency",
            "Statement latency distribution.",
            "us",
            latency,
            timestamp,
        ),
    ];
    json!({
        "resourceMetrics": [{
            "resource": {
                "attributes": resource_attributes(settings, server)
            },
            "scopeMetrics": [{
                "scope": {"name": "mydb", "version": server.version},
                "metrics": metrics,
            }]
        }]
    })
}

/// Slow statements recorded since the previous export become client spans.
/// The watermark advances even when the export fails, so a broken collector
/// cannot make the exporter resend the same spans forever.
fn collect_spans(
    slow_queries: &[SlowQuerySnapshot],
    last_exported_id: &AtomicU64,
) -> Vec<SlowQuerySnapshot> {
    let since = last_exported_id.load(Ordering::Relaxed);
    let mut pending: Vec<SlowQuerySnapshot> = slow_queries
        .iter()
        .filter(|snapshot| snapshot.id > since)
        .cloned()
        .collect();
    if pending.len() > MAX_EXPORTED_SPANS {
        pending.drain(..pending.len() - MAX_EXPORTED_SPANS);
    }
    if let Some(last) = pending.last() {
        last_exported_id.store(last.id, Ordering::Relaxed);
    }
    pending
}

fn otlp_traces_payload(
    settings: &OtlpSettings,
    spans: &[SlowQuerySnapshot],
    server: &ServerIdentity,
) -> Value {
    let otlp_spans: Vec<Value> = spans
        .iter()
        .map(|snapshot| {
            let start_nanos = snapshot.unix_ms.saturating_mul(1_000_000);
            let end_nanos = start_nanos.saturating_add(snapshot.duration_ms.saturating_mul(1_000_000));
            let mut attributes = vec![
                string_attribute("db.system", "mysql"),
                string_attribute("db.operation", &snapshot.statement_type),
                string_attribute("mydb.statement.digest", &format!("{:x}", Sha256::digest(snapshot.query_digest.as_bytes()))),
                string_attribute("mydb.query.outcome", &snapshot.outcome),
                int_attribute(
                    "mydb.query.duration_ms",
                    snapshot.duration_ms as i64,
                ),
                int_attribute("db.mysql.connection_id", i64::from(snapshot.connection_id)),
            ];
            if settings.include_sql {
                attributes.push(string_attribute("db.statement", &snapshot.sql));
                attributes.push(string_attribute("db.name", &snapshot.database));
            }
            if let Some(rows) = snapshot.rows_returned {
                attributes.push(int_attribute("db.response.returned_rows", rows as i64));
            }
            if let Some(affected) = snapshot.affected_rows {
                attributes.push(int_attribute("db.response.affected_rows", affected as i64));
            }
            if let Some(error) = snapshot.error.as_ref().filter(|_| settings.include_sql) {
                attributes.push(string_attribute("error.message", error));
            }
            let status = if snapshot.error.is_some() {
                json!({"code": 2, "message": if settings.include_sql { snapshot.error.clone().unwrap_or_default() } else { "SQL execution failed".to_string() }})
            } else {
                json!({"code": 1})
            };
            json!({
                "traceId": random_trace_id(),
                "spanId": random_span_id(),
                "name": format!("mysql {}", snapshot.statement_type),
                "kind": 3,
                "startTimeUnixNano": start_nanos.to_string(),
                "endTimeUnixNano": end_nanos.to_string(),
                "attributes": attributes,
                "status": status,
            })
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": resource_attributes(settings, server)
            },
            "scopeSpans": [{
                "scope": {"name": "mydb", "version": server.version},
                "spans": otlp_spans,
            }]
        }]
    })
}

fn random_trace_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn random_span_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// Prometheus exposition of the monitoring state that has no counterpart in
/// the legacy metric list: latency histogram, statement digests, slow query
/// log health and exporter status.
pub fn prometheus_monitoring_metrics(
    latency: &QueryLatencySnapshot,
    digests: &[StatementDigestSnapshot],
    slow_log: &SlowQueryLogSnapshot,
    otlp: &OtlpStatusSnapshot,
    digest_entries: usize,
    digest_dropped: u64,
) -> String {
    let mut body = String::new();
    body.push_str("# HELP mydb_query_latency_microseconds Statement latency distribution.\n");
    body.push_str("# TYPE mydb_query_latency_microseconds histogram\n");
    for (index, bound) in latency.bounds_micros.iter().enumerate() {
        body.push_str(&format!(
            "mydb_query_latency_microseconds_bucket{{le=\"{}\"}} {}\n",
            bound, latency.cumulative_counts[index]
        ));
    }
    body.push_str(&format!(
        "mydb_query_latency_microseconds_bucket{{le=\"+Inf\"}} {}\n",
        latency.cumulative_counts[latency.cumulative_counts.len() - 1]
    ));
    body.push_str(&format!(
        "mydb_query_latency_microseconds_sum {}\n",
        latency.sum_micros
    ));
    body.push_str(&format!(
        "mydb_query_latency_microseconds_count {}\n",
        latency.count
    ));
    body.push_str(
        "# HELP mydb_statement_executions_total Executions per normalized statement digest.\n",
    );
    body.push_str("# TYPE mydb_statement_executions_total counter\n");
    for digest in digests.iter().take(MAX_PROMETHEUS_DIGESTS) {
        body.push_str(&format!(
            "mydb_statement_executions_total{{statement_type=\"{}\",digest=\"{}\"}} {}\n",
            escape_prometheus_label(&digest.statement_type),
            escape_prometheus_label(&digest.digest),
            digest.count_star
        ));
    }
    body.push_str("# HELP mydb_statement_digest_entries Tracked statement digests.\n");
    body.push_str("# TYPE mydb_statement_digest_entries gauge\n");
    body.push_str(&format!("mydb_statement_digest_entries {digest_entries}\n"));
    body.push_str(
        "# HELP mydb_statement_digest_dropped_total Digests evicted when the registry was full.\n",
    );
    body.push_str("# TYPE mydb_statement_digest_dropped_total counter\n");
    body.push_str(&format!(
        "mydb_statement_digest_dropped_total {digest_dropped}\n"
    ));
    body.push_str(
        "# HELP mydb_slow_query_log_enabled Whether the slow query log writer is active.\n",
    );
    body.push_str("# TYPE mydb_slow_query_log_enabled gauge\n");
    body.push_str(&format!(
        "mydb_slow_query_log_enabled {}\n",
        u8::from(slow_log.enabled)
    ));
    body.push_str("# HELP mydb_slow_query_log_written_total Slow query log records written.\n");
    body.push_str("# TYPE mydb_slow_query_log_written_total counter\n");
    body.push_str(&format!(
        "mydb_slow_query_log_written_total {}\n",
        slow_log.written
    ));
    body.push_str("# HELP mydb_slow_query_log_dropped_total Slow query log records dropped.\n");
    body.push_str("# TYPE mydb_slow_query_log_dropped_total counter\n");
    body.push_str(&format!(
        "mydb_slow_query_log_dropped_total {}\n",
        slow_log.dropped
    ));
    body.push_str("# HELP mydb_otlp_enabled Whether the OTLP exporter is enabled.\n");
    body.push_str("# TYPE mydb_otlp_enabled gauge\n");
    body.push_str(&format!("mydb_otlp_enabled {}\n", u8::from(otlp.enabled)));
    body.push_str("# HELP mydb_otlp_metric_exports_total Successful OTLP metric exports.\n");
    body.push_str("# TYPE mydb_otlp_metric_exports_total counter\n");
    body.push_str(&format!(
        "mydb_otlp_metric_exports_total {}\n",
        otlp.metric_exports
    ));
    body.push_str("# HELP mydb_otlp_span_exports_total Successful OTLP trace exports.\n");
    body.push_str("# TYPE mydb_otlp_span_exports_total counter\n");
    body.push_str(&format!(
        "mydb_otlp_span_exports_total {}\n",
        otlp.span_exports
    ));
    body.push_str("# HELP mydb_otlp_export_failures_total Failed OTLP exports.\n");
    body.push_str("# TYPE mydb_otlp_export_failures_total counter\n");
    body.push_str(&format!(
        "mydb_otlp_export_failures_total {}\n",
        otlp.failures
    ));
    body.push_str("# HELP mydb_otlp_last_success_timestamp_seconds Unix time of the last successful export.\n");
    body.push_str("# TYPE mydb_otlp_last_success_timestamp_seconds gauge\n");
    body.push_str(&format!(
        "mydb_otlp_last_success_timestamp_seconds {}\n",
        otlp.last_success_unix_ms / 1_000
    ));
    body
}

fn escape_prometheus_label(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use mydb_wire::QUERY_LATENCY_BOUNDS_MICROS;

    fn settings() -> OtlpSettings {
        OtlpSettings {
            enabled: true,
            endpoint: "http://127.0.0.1:4318".to_string(),
            interval: Duration::from_secs(15),
            timeout: Duration::from_secs(5),
            service_name: "mydb".to_string(),
            headers: BTreeMap::new(),
            export_traces: true,
            include_sql: false,
        }
    }

    fn server_identity() -> ServerIdentity {
        ServerIdentity {
            host: "0.0.0.0".to_string(),
            port: 3306,
            version: "0.1.36".to_string(),
        }
    }

    fn latency_snapshot(counts: [u64; 12], sum: u64, max: u64) -> QueryLatencySnapshot {
        QueryLatencySnapshot {
            bounds_micros: QUERY_LATENCY_BOUNDS_MICROS,
            cumulative_counts: counts,
            count: counts[11],
            sum_micros: sum,
            min_micros: 1,
            max_micros: max,
        }
    }

    #[test]
    fn bucket_counts_are_differences_of_cumulative_counts() {
        let snapshot = latency_snapshot([1, 3, 4, 4, 4, 4, 4, 4, 4, 4, 4, 5], 120, 900);
        assert_eq!(
            snapshot.bucket_counts(),
            [1, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
    }

    #[test]
    fn metrics_payload_uses_otlp_json_shape() {
        let wire = mydb_wire::WireStatsSnapshot {
            active_connections: 3,
            total_connections: 9,
            queries: 42,
            query_errors: 1,
            query_execute_micros: 100,
            query_response_micros: 120,
            lock_waits: 0,
            lock_timeouts: 0,
            deadlocks: 0,
            row_lock_acquires: 0,
            table_lock_acquires: 0,
            active_locks: 0,
        };
        let storage = mydb_storage::StorageStatsSnapshot {
            uptime_seconds: 10,
            reads: 5,
            writes: 4,
            errors: 0,
            commit_queue_depth: 0,
            buffer_pool_pages: 128,
            group_commits: 2,
            grouped_requests: 6,
            checkpoints: 1,
            checkpoint_errors: 0,
            prepare_validation_micros: 0,
            wal_sync_micros: 1000,
            apply_micros: 10,
            checkpoint_micros: 20,
        };
        let audit = AuditLogSnapshot {
            healthy: true,
            accepted: 7,
            rejected: 0,
        };
        let latency = latency_snapshot([0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1], 250, 900);
        let snapshot = MonitoringSnapshot {
            wire: &wire,
            storage: &storage,
            audit: &audit,
            latency: &latency,
            slow_queries_total: 3,
            digest_entries: 12,
            digest_dropped: 0,
        };
        let server = ServerIdentity {
            host: "0.0.0.0".to_string(),
            port: 3306,
            version: "0.1.36".to_string(),
        };
        let payload =
            otlp_metrics_payload(&settings(), &snapshot, &server, 1_700_000_000_000_000_000);
        let resource_metrics = &payload["resourceMetrics"][0];
        assert_eq!(
            resource_metrics["scopeMetrics"][0]["scope"]["name"],
            json!("mydb")
        );
        let metrics = resource_metrics["scopeMetrics"][0]["metrics"]
            .as_array()
            .expect("metrics array");
        let names: Vec<&str> = metrics
            .iter()
            .map(|metric| metric["name"].as_str().expect("metric name"))
            .collect();
        assert!(names.contains(&"mydb.queries.total"));
        assert!(names.contains(&"mydb.query.latency"));
        let queries = metrics
            .iter()
            .find(|metric| metric["name"] == json!("mydb.queries.total"))
            .expect("query counter");
        assert_eq!(queries["sum"]["aggregationTemporality"], json!(2));
        assert_eq!(queries["sum"]["isMonotonic"], json!(true));
        assert_eq!(queries["sum"]["dataPoints"][0]["asInt"], json!("42"));
        let histogram = metrics
            .iter()
            .find(|metric| metric["name"] == json!("mydb.query.latency"))
            .expect("latency histogram");
        let data_point = &histogram["histogram"]["dataPoints"][0];
        assert_eq!(data_point["count"], json!("1"));
        assert_eq!(
            data_point["bucketCounts"].as_array().map(Vec::len),
            Some(QUERY_LATENCY_BOUNDS_MICROS.len() + 1)
        );
        assert_eq!(
            data_point["explicitBounds"].as_array().map(Vec::len),
            Some(QUERY_LATENCY_BOUNDS_MICROS.len())
        );
    }

    #[test]
    fn traces_payload_exports_slow_statements_as_client_spans() {
        let snapshot = SlowQuerySnapshot {
            id: 7,
            unix_ms: 1_700_000_000_000,
            duration_ms: 250,
            connection_id: 3,
            database: "shop".to_string(),
            statement_type: "SELECT".to_string(),
            query_digest: "select * from `t` where id = ?".to_string(),
            sql: "select * from t where id = 42".to_string(),
            error: None,
            outcome: "rows".to_string(),
            execution_micros: 240_000,
            explain_micros: 0,
            rows_returned: Some(10),
            affected_rows: None,
            explain_json: None,
        };
        let payload = otlp_traces_payload(
            &settings(),
            std::slice::from_ref(&snapshot),
            &server_identity(),
        );
        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["kind"], json!(3));
        assert_eq!(span["status"]["code"], json!(1));
        assert_eq!(span["traceId"].as_str().map(str::len), Some(32));
        assert_eq!(span["spanId"].as_str().map(str::len), Some(16));
        let attributes = span["attributes"].as_array().expect("attributes");
        assert!(attributes
            .iter()
            .any(|attribute| attribute["key"] == json!("db.system")
                && attribute["value"]["stringValue"] == json!("mysql")));
        assert!(!attributes
            .iter()
            .any(|attribute| attribute["key"] == json!("db.statement")));
    }

    #[test]
    fn traces_payload_includes_sql_only_when_configured() {
        let snapshot = SlowQuerySnapshot {
            id: 9,
            unix_ms: 1_700_000_000_000,
            duration_ms: 10,
            connection_id: 1,
            database: "shop".to_string(),
            statement_type: "UPDATE".to_string(),
            query_digest: "update `t` set a = ?".to_string(),
            sql: "update t set a = 1".to_string(),
            error: Some("deadlock".to_string()),
            outcome: "error".to_string(),
            execution_micros: 10_000,
            explain_micros: 0,
            rows_returned: None,
            affected_rows: Some(1),
            explain_json: None,
        };
        let mut enabled = settings();
        let private_payload = otlp_traces_payload(
            &enabled,
            std::slice::from_ref(&snapshot),
            &server_identity(),
        );
        let encoded = private_payload.to_string();
        assert!(!encoded.contains(&snapshot.sql));
        assert!(!encoded.contains(&snapshot.database));
        assert!(!encoded.contains("deadlock"));
        enabled.include_sql = true;
        let payload = otlp_traces_payload(
            &enabled,
            std::slice::from_ref(&snapshot),
            &server_identity(),
        );
        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["status"]["code"], json!(2));
        let attributes = span["attributes"].as_array().expect("attributes");
        assert!(attributes
            .iter()
            .any(|attribute| attribute["key"] == json!("db.statement")
                && attribute["value"]["stringValue"] == json!("update t set a = 1")));
    }

    #[test]
    fn collect_spans_only_returns_new_slow_queries() {
        let last_id = AtomicU64::new(0);
        let mut first = SlowQuerySnapshot {
            id: 1,
            unix_ms: 1,
            duration_ms: 1,
            connection_id: 1,
            database: "db".to_string(),
            statement_type: "SELECT".to_string(),
            query_digest: "select ?".to_string(),
            sql: "select 1".to_string(),
            error: None,
            outcome: "rows".to_string(),
            execution_micros: 1,
            explain_micros: 0,
            rows_returned: None,
            affected_rows: None,
            explain_json: None,
        };
        let mut second = first.clone();
        second.id = 2;
        first.id = 1;
        let spans = collect_spans(&[first.clone(), second.clone()], &last_id);
        assert_eq!(spans.len(), 2);
        assert_eq!(last_id.load(Ordering::Relaxed), 2);
        let spans = collect_spans(&[first, second], &last_id);
        assert!(spans.is_empty());
        assert_eq!(last_id.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn prometheus_labels_are_escaped() {
        assert_eq!(escape_prometheus_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
