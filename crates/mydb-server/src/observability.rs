//! OpenTelemetry OTLP/HTTP export and Prometheus exposition.
//!
//! The exporter runs on its own task and never touches the query hot path. It
//! reads counters that the wire layer already maintains, so enabling it costs
//! one snapshot per interval instead of per-statement instrumentation.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use mydb_config::OtlpSection;
use mydb_wire::{
    AuditLogSnapshot, ControlTrace, QueryLatencySnapshot, SessionTrace, SlowQueryLogSnapshot,
    StatementDigestSnapshot, StatementTrace, WireStatsSnapshot,
};

const OTLP_METRICS_PATH: &str = "/v1/metrics";
const OTLP_TRACES_PATH: &str = "/v1/traces";
const OTLP_LOGS_PATH: &str = "/v1/logs";
/// Number of statement digests exported as Prometheus labels. Prometheus label
/// cardinality has to stay bounded.
const MAX_PROMETHEUS_DIGESTS: usize = 20;
/// Upper bound of buffered tracing events and exported log records per cycle.
const MAX_BUFFERED_LOGS: usize = 4096;
const MAX_EXPORTED_LOGS: usize = 256;

#[derive(Clone)]
struct CapturedLog {
    id: u64,
    time_unix_nanos: u64,
    severity_number: i64,
    severity_text: String,
    body: String,
    target: String,
    attributes: BTreeMap<String, String>,
}

/// Bounded buffer feeding the OTLP logs signal. `tracing` stays on its normal
/// synchronous path; only the exporter reads this buffer once per interval.
pub struct OtlpLogBuffer {
    sequence: AtomicU64,
    entries: Mutex<VecDeque<CapturedLog>>,
}

impl OtlpLogBuffer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sequence: AtomicU64::new(0),
            entries: Mutex::new(VecDeque::with_capacity(MAX_BUFFERED_LOGS)),
        })
    }

    fn push(&self, mut log: CapturedLog) {
        log.id = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let mut entries = self.entries.lock();
        if entries.len() == MAX_BUFFERED_LOGS {
            entries.pop_front();
        }
        entries.push_back(log);
    }

    fn since(&self, last_id: u64) -> Vec<CapturedLog> {
        let entries = self.entries.lock();
        let mut pending: Vec<CapturedLog> = entries
            .iter()
            .filter(|entry| entry.id > last_id)
            .cloned()
            .collect();
        if pending.len() > MAX_EXPORTED_LOGS {
            pending.drain(..pending.len() - MAX_EXPORTED_LOGS);
        }
        pending
    }
}

#[derive(Default)]
struct LogVisitor {
    body: Option<String>,
    attributes: BTreeMap<String, String>,
}

impl LogVisitor {
    fn record(&mut self, field: &tracing::field::Field, value: String) {
        if field.name() == "message" {
            self.body = Some(value);
            return;
        }
        if is_sensitive_log_key(field.name()) {
            self.attributes
                .insert(field.name().to_string(), "<redacted>".to_string());
        } else {
            self.attributes.insert(field.name().to_string(), value);
        }
    }
}

impl tracing::field::Visit for LogVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record(field, value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record(field, value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.record(field, value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record(field, value.to_string());
    }
}

fn is_sensitive_log_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["password", "token", "secret", "authorization", "credential"]
        .iter()
        .any(|needle| key.contains(needle))
}

/// `tracing` layer that mirrors events into the OTLP log buffer. Exporter
/// diagnostics are skipped so a failing collector cannot feed itself.
pub struct OtlpLogLayer {
    buffer: Arc<OtlpLogBuffer>,
}

impl OtlpLogLayer {
    pub fn new(buffer: Arc<OtlpLogBuffer>) -> Self {
        Self { buffer }
    }
}

impl<S> tracing_subscriber::Layer<S> for OtlpLogLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = event.metadata();
        if metadata.target().contains("mydb_server::observability") {
            return;
        }
        let mut visitor = LogVisitor::default();
        event.record(&mut visitor);
        let body = visitor.body.unwrap_or_default();
        if body.is_empty() && visitor.attributes.is_empty() {
            return;
        }
        let level = *metadata.level();
        let (severity_text, severity_number) = if level == tracing::Level::ERROR {
            ("ERROR", 17)
        } else if level == tracing::Level::WARN {
            ("WARN", 13)
        } else if level == tracing::Level::INFO {
            ("INFO", 9)
        } else if level == tracing::Level::DEBUG {
            ("DEBUG", 5)
        } else {
            ("TRACE", 1)
        };
        self.buffer.push(CapturedLog {
            id: 0,
            time_unix_nanos: unix_nanos(),
            severity_number,
            severity_text: severity_text.to_string(),
            body,
            target: metadata.target().to_string(),
            attributes: visitor.attributes,
        });
    }
}

/// Install the normal stdout subscriber plus the OTLP log mirror.
pub fn init_logging() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(OtlpLogLayer::new(log_buffer()))
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize logging: {error}"))
}

/// Process-wide log buffer shared by the tracing layer and the exporter, so
/// Windows service mode and normal mode use the same instance.
pub fn log_buffer() -> Arc<OtlpLogBuffer> {
    static LOG_BUFFER: std::sync::OnceLock<Arc<OtlpLogBuffer>> = std::sync::OnceLock::new();
    LOG_BUFFER.get_or_init(OtlpLogBuffer::new).clone()
}

/// Resolved upstream credentials. The password is read once per configuration
/// reload, never once per export, and never appears in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub enum OtlpAuth {
    None,
    Bearer(String),
    Basic { username: String, password: String },
}

impl OtlpAuth {
    pub fn method_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer(_) => "bearer",
            Self::Basic { .. } => "basic",
        }
    }

    fn authorization_header(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Bearer(token) => Some(format!("Bearer {token}")),
            Self::Basic { username, password } => {
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(format!("{username}:{password}"));
                Some(format!("Basic {encoded}"))
            }
        }
    }
}

impl fmt::Debug for OtlpAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Bearer(_) => formatter.write_str("Bearer(<redacted>)"),
            Self::Basic { username, .. } => formatter
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

/// OTLP/HTTP wire encoding. MySQL 9 defaults to `http/protobuf`; the JSON
/// variant stays for local debugging and collectors that only accept JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpProtocol {
    Protobuf,
    Json,
}

impl OtlpProtocol {
    pub fn name(self) -> &'static str {
        match self {
            Self::Protobuf => "http/protobuf",
            Self::Json => "http/json",
        }
    }

    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "http/json" | "json" => Self::Json,
            _ => Self::Protobuf,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpCompression {
    None,
    Gzip,
}

impl OtlpCompression {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gzip => "gzip",
        }
    }

    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "identity" => Self::None,
            _ => Self::Gzip,
        }
    }
}

/// Runtime configuration of the exporter. A configuration reload replaces the
/// whole struct, so the exporter can be retargeted without a restart.
#[derive(Clone)]
pub struct OtlpSettings {
    pub enabled: bool,
    pub protocol: OtlpProtocol,
    pub compression: OtlpCompression,
    pub endpoint: String,
    pub interval: Duration,
    pub timeout: Duration,
    pub service_name: String,
    pub headers: BTreeMap<String, String>,
    pub auth: OtlpAuth,
    pub export_traces: bool,
    pub export_logs: bool,
    pub include_sql: bool,
}

impl fmt::Debug for OtlpSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OtlpSettings")
            .field("enabled", &self.enabled)
            .field("protocol", &self.protocol.name())
            .field("compression", &self.compression.name())
            .field("endpoint", &self.endpoint)
            .field("interval", &self.interval)
            .field("timeout", &self.timeout)
            .field("service_name", &self.service_name)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("auth", &self.auth)
            .field("export_traces", &self.export_traces)
            .field("export_logs", &self.export_logs)
            .field("include_sql", &self.include_sql)
            .finish()
    }
}

impl OtlpSettings {
    pub fn from_section(section: &OtlpSection) -> Self {
        let auth = resolve_auth(section);
        Self {
            enabled: section.enabled,
            protocol: OtlpProtocol::parse(&section.protocol),
            compression: OtlpCompression::parse(&section.compression),
            endpoint: section.endpoint.trim_end_matches('/').to_string(),
            interval: Duration::from_secs(section.interval_secs.max(1)),
            timeout: Duration::from_secs(section.timeout_secs.max(1)),
            service_name: section.service_name.clone(),
            headers: section.headers.clone(),
            auth,
            export_traces: section.export_traces,
            export_logs: section.export_logs,
            include_sql: section.include_sql,
        }
    }
}

fn resolve_auth(section: &OtlpSection) -> OtlpAuth {
    let method = section.auth.normalized_method().to_ascii_lowercase();
    match method.as_str() {
        "basic" => {
            let mut password = section.auth.password.clone();
            if password.is_empty() {
                if let Some(path) = section.auth.password_file.as_ref() {
                    match std::fs::read_to_string(path) {
                        Ok(content) => password = content.trim_end_matches(['\r', '\n']).to_string(),
                        Err(error) => tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            "failed to read OTLP password file"
                        ),
                    }
                }
            }
            OtlpAuth::Basic {
                username: section.auth.username.clone(),
                password,
            }
        }
        "bearer" => OtlpAuth::Bearer(section.auth.token.clone()),
        _ => OtlpAuth::None,
    }
}

#[derive(Debug, Default)]
struct OtlpCounters {
    metric_exports: AtomicU64,
    span_exports: AtomicU64,
    log_exports: AtomicU64,
    failures: AtomicU64,
    last_success_unix_ms: AtomicU64,
    last_error: Mutex<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct OtlpStatusSnapshot {
    pub enabled: bool,
    pub protocol: &'static str,
    pub compression: &'static str,
    pub endpoint: String,
    pub interval_seconds: u64,
    pub service_name: String,
    pub auth_method: String,
    pub export_traces: bool,
    pub export_logs: bool,
    pub include_sql: bool,
    pub metric_exports: u64,
    pub span_exports: u64,
    pub log_exports: u64,
    pub failures: u64,
    pub last_success_unix_ms: u64,
    pub last_error: Option<String>,
}

impl OtlpStatusSnapshot {
    /// Safe for the admin API: never contains a password, token or header
    /// value, only the authentication method that an operator selected.
    pub fn to_json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "protocol": self.protocol,
            "compression": self.compression,
            "endpoint": self.endpoint,
            "interval_seconds": self.interval_seconds,
            "service_name": self.service_name,
            "auth_method": self.auth_method,
            "export_traces": self.export_traces,
            "export_logs": self.export_logs,
            "include_sql": self.include_sql,
            "metric_exports": self.metric_exports,
            "span_exports": self.span_exports,
            "log_exports": self.log_exports,
            "failures": self.failures,
            "last_success_unix_ms": self.last_success_unix_ms,
            "last_error": self.last_error,
        })
    }
}

/// Counters published in one export interval.
pub struct MonitoringSnapshot<'a> {
    pub wire: &'a WireStatsSnapshot,
    pub storage: &'a mydb_storage::StorageStatsSnapshot,
    pub audit: &'a AuditLogSnapshot,
    pub latency: &'a QueryLatencySnapshot,
    pub cdc: Option<&'a mydb_cdc::CdcStatus>,
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
    last_exported_statement_trace_id: Arc<AtomicU64>,
    last_exported_session_trace_id: Arc<AtomicU64>,
    last_exported_control_trace_id: Arc<AtomicU64>,
    last_exported_log_id: Arc<AtomicU64>,
    log_buffer: Arc<OtlpLogBuffer>,
}

impl OtlpExporter {
    pub fn new(section: &OtlpSection) -> Self {
        Self {
            settings: Arc::new(RwLock::new(OtlpSettings::from_section(section))),
            counters: Arc::new(OtlpCounters::default()),
            last_exported_statement_trace_id: Arc::new(AtomicU64::new(0)),
            last_exported_session_trace_id: Arc::new(AtomicU64::new(0)),
            last_exported_control_trace_id: Arc::new(AtomicU64::new(0)),
            last_exported_log_id: Arc::new(AtomicU64::new(0)),
            log_buffer: log_buffer(),
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
            protocol: settings.protocol.name(),
            compression: settings.compression.name(),
            endpoint: settings.endpoint.clone(),
            interval_seconds: settings.interval.as_secs(),
            service_name: settings.service_name.clone(),
            auth_method: settings.auth.method_name().to_string(),
            export_traces: settings.export_traces,
            export_logs: settings.export_logs,
            include_sql: settings.include_sql,
            metric_exports: self.counters.metric_exports.load(Ordering::Relaxed),
            span_exports: self.counters.span_exports.load(Ordering::Relaxed),
            log_exports: self.counters.log_exports.load(Ordering::Relaxed),
            failures: self.counters.failures.load(Ordering::Relaxed),
            last_success_unix_ms: self.counters.last_success_unix_ms.load(Ordering::Relaxed),
            last_error: self.counters.last_error.lock().clone(),
        }
    }

    async fn export_once(
        &self,
        snapshot: &MonitoringSnapshot<'_>,
        wire_stats: &mydb_wire::WireStats,
        server: &ServerIdentity,
    ) {
        let settings = self.settings.read().clone();
        if !settings.enabled {
            return;
        }
        let timestamp = unix_nanos();
        let metrics_payload = otlp_metrics_payload(&settings, snapshot, server, timestamp);
        match post_otlp(
            &format!("{}{}", settings.endpoint, OTLP_METRICS_PATH),
            &settings.headers,
            &settings.auth,
            settings.protocol,
            settings.compression,
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

        if settings.export_traces {
            let statements =
                wire_stats.statement_traces_since(self.last_exported_statement_trace_id.load(Ordering::Relaxed));
            let sessions =
                wire_stats.session_traces_since(self.last_exported_session_trace_id.load(Ordering::Relaxed));
            let controls =
                wire_stats.control_traces_since(self.last_exported_control_trace_id.load(Ordering::Relaxed));
            if let Some(last) = statements.last() {
                self.last_exported_statement_trace_id
                    .store(last.id, Ordering::Relaxed);
            }
            if let Some(last) = sessions.last() {
                self.last_exported_session_trace_id
                    .store(last.id, Ordering::Relaxed);
            }
            if let Some(last) = controls.last() {
                self.last_exported_control_trace_id
                    .store(last.id, Ordering::Relaxed);
            }
            if !statements.is_empty() || !sessions.is_empty() || !controls.is_empty() {
                let traces_payload =
                    otlp_traces_payload(&settings, &statements, &sessions, &controls, server);
                match post_otlp(
                    &format!("{}{}", settings.endpoint, OTLP_TRACES_PATH),
                    &settings.headers,
                    &settings.auth,
                    settings.protocol,
                    settings.compression,
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
                        self.record_failure(format!(
                            "OTLP traces endpoint returned HTTP {status}"
                        ));
                    }
                    Err(error) => self.record_failure(error.to_string()),
                }
            }
        }

        if settings.export_logs {
            let logs = self
                .log_buffer
                .since(self.last_exported_log_id.load(Ordering::Relaxed));
            if !logs.is_empty() {
                if let Some(last) = logs.last() {
                    self.last_exported_log_id
                        .store(last.id, Ordering::Relaxed);
                }
                let logs_payload = otlp_logs_payload(&settings, &logs, server);
                match post_otlp(
                    &format!("{}{}", settings.endpoint, OTLP_LOGS_PATH),
                    &settings.headers,
                    &settings.auth,
                    settings.protocol,
                    settings.compression,
                    &logs_payload,
                    settings.timeout,
                )
                .await
                {
                    Ok(status) if (200..300).contains(&status) => {
                        self.counters.log_exports.fetch_add(1, Ordering::Relaxed);
                        self.counters
                            .last_success_unix_ms
                            .store(unix_millis(), Ordering::Relaxed);
                        *self.counters.last_error.lock() = None;
                    }
                    Ok(status) => {
                        self.record_failure(format!(
                            "OTLP logs endpoint returned HTTP {status}"
                        ));
                    }
                    Err(error) => self.record_failure(error.to_string()),
                }
            }
        }
    }

    fn record_failure(&self, message: String) {
        self.counters.failures.fetch_add(1, Ordering::Relaxed);
        tracing::warn!("{message}");
        *self.counters.last_error.lock() = Some(message);
    }
}

/// Dependencies of the resident OTLP exporter loop, grouped to keep the
/// signature stable as more exporters are added.
pub struct OtlpExporterContext {
    pub exporter: OtlpExporter,
    pub wire_stats: Arc<mydb_wire::WireStats>,
    pub storage: Arc<mydb_storage::StorageEngineManager>,
    pub audit_log: Arc<mydb_wire::AuditLog>,
    pub cdc: Arc<mydb_cdc::CdcManager>,
    pub server_host: String,
    pub server_port: u16,
}

/// Background exporter loop. It stays resident even while OTLP is disabled so
/// an admin reload can turn it on without a process restart. Disabled cycles
/// do not take a stats snapshot, keeping the idle cost to one timer tick.
pub async fn run_otlp_exporter(
    context: OtlpExporterContext,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let OtlpExporterContext {
        exporter,
        wire_stats,
        storage,
        audit_log,
        cdc,
        server_host,
        server_port,
    } = context;
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
        let active = {
            let settings = exporter.settings.read();
            settings.enabled
        };
        wire_stats.set_statement_tracing(active && exporter.settings.read().export_traces);
        if !active {
            continue;
        }
        let cdc_status = cdc.is_enabled().then(|| cdc.status());
        let wire = wire_stats.snapshot();
        let storage_stats = storage.stats();
        let audit = audit_log.snapshot();
        let latency = wire_stats.query_latency();
        let snapshot = MonitoringSnapshot {
            wire: &wire,
            storage: &storage_stats,
            audit: &audit,
            latency: &latency,
            cdc: cdc_status.as_ref(),
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
            .export_once(&snapshot, &wire_stats, &server)
            .await;
    }
}

async fn post_otlp(
    url: &str,
    headers: &BTreeMap<String, String>,
    auth: &OtlpAuth,
    protocol: OtlpProtocol,
    compression: OtlpCompression,
    payload: &Value,
    timeout: Duration,
) -> anyhow::Result<u16> {
    let body = match protocol {
        OtlpProtocol::Json => serde_json::to_vec(payload)?,
        OtlpProtocol::Protobuf => encode_otlp_protobuf(url, payload)?,
    };
    let body = match compression {
        OtlpCompression::Gzip => {
            use std::io::Write as _;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&body)?;
            encoder.finish()?
        }
        OtlpCompression::None => body,
    };
    let content_type = match protocol {
        OtlpProtocol::Json => "application/json",
        OtlpProtocol::Protobuf => "application/x-protobuf",
    };
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| anyhow::anyhow!("failed to build OTLP HTTP client: {error}"))?;
    let mut request = client
        .post(url)
        .header("Content-Type", content_type)
        .body(body);
    if compression == OtlpCompression::Gzip {
        request = request.header("Content-Encoding", "gzip");
    }
    let explicit_authorization = headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"));
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    if !explicit_authorization {
        if let Some(value) = auth.authorization_header() {
            request = request.header("Authorization", value);
        }
    }
    let response = request
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("OTLP export request failed: {}", error.without_url()))?;
    Ok(response.status().as_u16())
}

/// Encode the OTLP payload built for JSON into the protobuf wire format
/// advertised by MySQL 9's default `http/protobuf` transport. The schema uses
/// the stable field numbers from the OpenTelemetry proto definitions.
fn encode_otlp_protobuf(url: &str, payload: &Value) -> anyhow::Result<Vec<u8>> {
    if url.ends_with(OTLP_TRACES_PATH) {
        Ok(encode_trace_request(payload))
    } else if url.ends_with(OTLP_METRICS_PATH) {
        Ok(encode_metrics_request(payload))
    } else if url.ends_with(OTLP_LOGS_PATH) {
        Ok(encode_logs_request(payload))
    } else {
        anyhow::bail!("unknown OTLP signal path: {url}")
    }
}

fn pb_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn pb_key(field: u32, wire: u8, output: &mut Vec<u8>) {
    pb_varint(u64::from(field) << 3 | u64::from(wire), output);
}

fn pb_varint_field(field: u32, value: u64, output: &mut Vec<u8>) {
    pb_key(field, 0, output);
    pb_varint(value, output);
}

fn pb_fixed64_field(field: u32, value: u64, output: &mut Vec<u8>) {
    pb_key(field, 1, output);
    output.extend_from_slice(&value.to_le_bytes());
}

fn pb_bytes_field(field: u32, value: &[u8], output: &mut Vec<u8>) {
    pb_key(field, 2, output);
    pb_varint(value.len() as u64, output);
    output.extend_from_slice(value);
}

fn pb_string_field(field: u32, value: &str, output: &mut Vec<u8>) {
    pb_bytes_field(field, value.as_bytes(), output);
}

fn pb_message_field(field: u32, message: &[u8], output: &mut Vec<u8>) {
    pb_bytes_field(field, message, output);
}

fn hex_to_bytes(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks(2) {
        bytes.push(u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?);
    }
    Some(bytes)
}

fn encode_any_value(value: &Value, output: &mut Vec<u8>) {
    if let Some(text) = value.get("stringValue").and_then(Value::as_str) {
        pb_string_field(1, text, output);
    } else if let Some(flag) = value.get("boolValue").and_then(Value::as_bool) {
        pb_varint_field(2, u64::from(flag), output);
    } else if let Some(text) = value.get("intValue").and_then(Value::as_str) {
        if let Ok(number) = text.parse::<i64>() {
            pb_varint_field(3, number as u64, output);
        }
    } else if let Some(number) = value.get("doubleValue").and_then(Value::as_f64) {
        pb_fixed64_field(4, number.to_bits(), output);
    }
}

fn encode_attributes(attributes: &[Value], output: &mut Vec<u8>, field: u32) {
    for attribute in attributes {
        let Some(key) = attribute.get("key").and_then(Value::as_str) else {
            continue;
        };
        let mut message = Vec::new();
        pb_string_field(1, key, &mut message);
        if let Some(value) = attribute.get("value") {
            let mut any = Vec::new();
            encode_any_value(value, &mut any);
            if !any.is_empty() {
                pb_message_field(2, &any, &mut message);
            }
        }
        pb_message_field(field, &message, output);
    }
}

fn encode_resource(resource: &Value, output: &mut Vec<u8>) {
    if let Some(attributes) = resource.get("attributes").and_then(Value::as_array) {
        encode_attributes(attributes, output, 1);
    }
}

fn encode_scope(scope: &Value, output: &mut Vec<u8>) {
    if let Some(name) = scope.get("name").and_then(Value::as_str) {
        pb_string_field(1, name, output);
    }
    if let Some(version) = scope.get("version").and_then(Value::as_str) {
        pb_string_field(2, version, output);
    }
}

fn encode_status(status: &Value, output: &mut Vec<u8>) {
    if let Some(message) = status.get("message").and_then(Value::as_str) {
        pb_string_field(2, message, output);
    }
    if let Some(code) = status.get("code").and_then(Value::as_i64) {
        pb_varint_field(3, code as u64, output);
    }
}

fn encode_span(span: &Value, output: &mut Vec<u8>) {
    if let Some(trace) = span
        .get("traceId")
        .and_then(Value::as_str)
        .and_then(hex_to_bytes)
    {
        pb_bytes_field(1, &trace, output);
    }
    if let Some(span_id) = span
        .get("spanId")
        .and_then(Value::as_str)
        .and_then(hex_to_bytes)
    {
        pb_bytes_field(2, &span_id, output);
    }
    if let Some(parent) = span
        .get("parentSpanId")
        .and_then(Value::as_str)
        .and_then(hex_to_bytes)
    {
        if !parent.is_empty() {
            pb_bytes_field(4, &parent, output);
        }
    }
    if let Some(name) = span.get("name").and_then(Value::as_str) {
        pb_string_field(5, name, output);
    }
    if let Some(kind) = span.get("kind").and_then(Value::as_i64) {
        pb_varint_field(6, kind as u64, output);
    }
    let start = span
        .get("startTimeUnixNano")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let end = span
        .get("endTimeUnixNano")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(start);
    pb_fixed64_field(7, start, output);
    pb_fixed64_field(8, end, output);
    if let Some(attributes) = span.get("attributes").and_then(Value::as_array) {
        encode_attributes(attributes, output, 9);
    }
    if let Some(status) = span.get("status") {
        let mut message = Vec::new();
        encode_status(status, &mut message);
        if !message.is_empty() {
            pb_message_field(15, &message, output);
        }
    }
}

fn encode_trace_request(payload: &Value) -> Vec<u8> {
    let mut request = Vec::new();
    let Some(resource_spans) = payload.get("resourceSpans").and_then(Value::as_array) else {
        return request;
    };
    for resource_span in resource_spans {
        let mut message = Vec::new();
        if let Some(resource) = resource_span.get("resource") {
            let mut resource_message = Vec::new();
            encode_resource(resource, &mut resource_message);
            pb_message_field(1, &resource_message, &mut message);
        }
        if let Some(scope_spans) = resource_span.get("scopeSpans").and_then(Value::as_array) {
            for scope_span in scope_spans {
                let mut scope_message = Vec::new();
                if let Some(scope) = scope_span.get("scope") {
                    let mut instrument_scope = Vec::new();
                    encode_scope(scope, &mut instrument_scope);
                    pb_message_field(1, &instrument_scope, &mut scope_message);
                }
                if let Some(spans) = scope_span.get("spans").and_then(Value::as_array) {
                    for span in spans {
                        let mut span_message = Vec::new();
                        encode_span(span, &mut span_message);
                        pb_message_field(2, &span_message, &mut scope_message);
                    }
                }
                pb_message_field(2, &scope_message, &mut message);
            }
        }
        pb_message_field(1, &message, &mut request);
    }
    request
}

fn encode_number_data_point(point: &Value, output: &mut Vec<u8>) {
    if let Some(attributes) = point.get("attributes").and_then(Value::as_array) {
        encode_attributes(attributes, output, 7);
    }
    if let Some(time) = point
        .get("timeUnixNano")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
    {
        pb_fixed64_field(3, time, output);
    }
    if let Some(value) = point
        .get("asInt")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<i64>().ok())
    {
        pb_fixed64_field(6, value as u64, output);
    } else if let Some(value) = point.get("asDouble").and_then(Value::as_f64) {
        pb_fixed64_field(4, value.to_bits(), output);
    }
}

fn encode_metric(metric: &Value, output: &mut Vec<u8>) {
    if let Some(name) = metric.get("name").and_then(Value::as_str) {
        pb_string_field(1, name, output);
    }
    if let Some(description) = metric.get("description").and_then(Value::as_str) {
        pb_string_field(2, description, output);
    }
    if let Some(unit) = metric.get("unit").and_then(Value::as_str) {
        pb_string_field(3, unit, output);
    }
    if let Some(gauge) = metric.get("gauge") {
        let mut message = Vec::new();
        if let Some(points) = gauge.get("dataPoints").and_then(Value::as_array) {
            for point in points {
                let mut point_message = Vec::new();
                encode_number_data_point(point, &mut point_message);
                pb_message_field(1, &point_message, &mut message);
            }
        }
        pb_message_field(5, &message, output);
    }
    if let Some(sum) = metric.get("sum") {
        let mut message = Vec::new();
        if let Some(points) = sum.get("dataPoints").and_then(Value::as_array) {
            for point in points {
                let mut point_message = Vec::new();
                encode_number_data_point(point, &mut point_message);
                pb_message_field(1, &point_message, &mut message);
            }
        }
        if let Some(temporality) = sum.get("aggregationTemporality").and_then(Value::as_i64) {
            pb_varint_field(2, temporality as u64, &mut message);
        }
        if let Some(monotonic) = sum.get("isMonotonic").and_then(Value::as_bool) {
            pb_varint_field(3, u64::from(monotonic), &mut message);
        }
        pb_message_field(7, &message, output);
    }
}

fn encode_metrics_request(payload: &Value) -> Vec<u8> {
    let mut request = Vec::new();
    let Some(resource_metrics) = payload.get("resourceMetrics").and_then(Value::as_array) else {
        return request;
    };
    for resource_metric in resource_metrics {
        let mut message = Vec::new();
        if let Some(resource) = resource_metric.get("resource") {
            let mut resource_message = Vec::new();
            encode_resource(resource, &mut resource_message);
            pb_message_field(1, &resource_message, &mut message);
        }
        if let Some(scope_metrics) = resource_metric.get("scopeMetrics").and_then(Value::as_array) {
            for scope_metric in scope_metrics {
                let mut scope_message = Vec::new();
                if let Some(scope) = scope_metric.get("scope") {
                    let mut instrument_scope = Vec::new();
                    encode_scope(scope, &mut instrument_scope);
                    pb_message_field(1, &instrument_scope, &mut scope_message);
                }
                if let Some(metrics) = scope_metric.get("metrics").and_then(Value::as_array) {
                    for metric in metrics {
                        let mut metric_message = Vec::new();
                        encode_metric(metric, &mut metric_message);
                        pb_message_field(2, &metric_message, &mut scope_message);
                    }
                }
                pb_message_field(2, &scope_message, &mut message);
            }
        }
        pb_message_field(1, &message, &mut request);
    }
    request
}

fn encode_log_record(record: &Value, output: &mut Vec<u8>) {
    if let Some(time) = record
        .get("timeUnixNano")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
    {
        pb_fixed64_field(1, time, output);
    }
    if let Some(severity) = record.get("severityNumber").and_then(Value::as_i64) {
        pb_varint_field(2, severity as u64, output);
    }
    if let Some(text) = record.get("severityText").and_then(Value::as_str) {
        pb_string_field(3, text, output);
    }
    if let Some(body) = record.get("body") {
        let mut body_message = Vec::new();
        encode_any_value(body, &mut body_message);
        if !body_message.is_empty() {
            pb_message_field(5, &body_message, output);
        }
    }
    if let Some(attributes) = record.get("attributes").and_then(Value::as_array) {
        encode_attributes(attributes, output, 6);
    }
}

fn encode_logs_request(payload: &Value) -> Vec<u8> {
    let mut request = Vec::new();
    let Some(resource_logs) = payload.get("resourceLogs").and_then(Value::as_array) else {
        return request;
    };
    for resource_log in resource_logs {
        let mut message = Vec::new();
        if let Some(resource) = resource_log.get("resource") {
            let mut resource_message = Vec::new();
            encode_resource(resource, &mut resource_message);
            pb_message_field(1, &resource_message, &mut message);
        }
        if let Some(scope_logs) = resource_log.get("scopeLogs").and_then(Value::as_array) {
            for scope_log in scope_logs {
                let mut scope_message = Vec::new();
                if let Some(scope) = scope_log.get("scope") {
                    let mut instrument_scope = Vec::new();
                    encode_scope(scope, &mut instrument_scope);
                    pb_message_field(1, &instrument_scope, &mut scope_message);
                }
                if let Some(records) = scope_log.get("logRecords").and_then(Value::as_array) {
                    for record in records {
                        let mut record_message = Vec::new();
                        encode_log_record(record, &mut record_message);
                        pb_message_field(2, &record_message, &mut scope_message);
                    }
                }
                pb_message_field(2, &scope_message, &mut message);
            }
        }
        pb_message_field(1, &message, &mut request);
    }
    request
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

fn bool_attribute(key: &str, value: bool) -> Value {
    json!({"key": key, "value": {"boolValue": value}})
}

fn resource_attributes(settings: &OtlpSettings, server: &ServerIdentity) -> Value {
    json!([
        string_attribute("service.name", &settings.service_name),
        string_attribute("service.version", &server.version),
        string_attribute("service.instance.id", &format!("{}:{}", hostname(), server.port)),
        string_attribute("host.name", &hostname()),
        string_attribute("server.address", &server.host),
        int_attribute("server.port", i64::from(server.port)),
        string_attribute("telemetry.sdk.name", "mydb-otlp"),
        string_attribute("telemetry.sdk.language", "rust"),
        string_attribute("telemetry.sdk.version", &server.version),
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

fn cdc_gauge_metric(
    cdc: Option<&mydb_cdc::CdcStatus>,
    suffix: &str,
    value: impl Fn(&mydb_cdc::CdcStatus) -> u64,
    timestamp: u64,
) -> Value {
    gauge_metric(
        &format!("mydb.cdc.{suffix}"),
        "CDC stream state. Zero when CDC is disabled.",
        "1",
        cdc.map(value).unwrap_or(0),
        timestamp,
    )
}

fn cdc_counter_metric(
    cdc: Option<&mydb_cdc::CdcStatus>,
    suffix: &str,
    value: impl Fn(&mydb_cdc::CdcStatus) -> u64,
    timestamp: u64,
) -> Value {
    counter_metric(
        &format!("mydb.cdc.{suffix}"),
        "CDC events emitted since process start. Zero when CDC is disabled.",
        "1",
        cdc.map(value).unwrap_or(0),
        timestamp,
    )
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
        counter_metric(
            "mydb.olap.pages.scanned",
            "Pages decoded by filtered scans.",
            "1",
            storage.olap_pages_scanned,
            timestamp,
        ),
        counter_metric(
            "mydb.olap.pages.pruned",
            "Pages skipped by zone-map pruning.",
            "1",
            storage.olap_pages_pruned,
            timestamp,
        ),
        counter_metric(
            "mydb.olap.rows.pruned",
            "Rows skipped without decoding because their page was pruned.",
            "1",
            storage.olap_rows_pruned,
            timestamp,
        ),
        cdc_gauge_metric(snapshot.cdc, "enabled", |status| u64::from(status.enabled), timestamp),
        cdc_gauge_metric(snapshot.cdc, "last_lsn", |status| status.last_lsn, timestamp),
        cdc_gauge_metric(snapshot.cdc, "checkpoint_lsn", |status| status.checkpoint_lsn, timestamp),
        cdc_gauge_metric(snapshot.cdc, "lag_lsn", |status| status.lag_lsn(), timestamp),
        cdc_counter_metric(snapshot.cdc, "events_total", |status| status.events_emitted, timestamp),
        cdc_counter_metric(snapshot.cdc, "changes_total", |status| status.changes_emitted, timestamp),
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

fn otlp_traces_payload(
    settings: &OtlpSettings,
    statements: &[StatementTrace],
    sessions: &[SessionTrace],
    controls: &[ControlTrace],
    server: &ServerIdentity,
) -> Value {
    let mut otlp_spans: Vec<Value> = Vec::new();
    for control in controls {
        let attributes = vec![
            bool_attribute("trace_enabled", control.trace_enabled),
            bool_attribute("metrics_enabled", control.metrics_enabled),
            bool_attribute("logs_enabled", control.logs_enabled),
            string_attribute("details", &control.details),
        ];
        otlp_spans.push(json!({
            "traceId": control.trace_id,
            "spanId": control.span_id,
            "name": "Control",
            "kind": 1,
            "startTimeUnixNano": control.time_unix_nanos.to_string(),
            "endTimeUnixNano": control.time_unix_nanos.to_string(),
            "attributes": attributes,
            "status": {"code": 1},
        }));
    }
    for session in sessions {
        let mut attributes = vec![
            int_attribute("mysql.processlist_id", i64::from(session.processlist_id)),
            int_attribute("mysql.thread_id", i64::from(session.thread_id)),
            string_attribute("mysql.user", &session.user),
            string_attribute("mysql.host", &session.host),
            string_attribute("mysql.group", ""),
            string_attribute("mysql.session_attr.host", &session.host),
        ];
        if let Some(database) = session.database.as_deref() {
            attributes.push(string_attribute("mysql.session_attr.db", database));
        }
        otlp_spans.push(json!({
            "traceId": session.trace_id,
            "spanId": session.span_id,
            "name": "session",
            "kind": 1,
            "startTimeUnixNano": session.start_unix_nanos.to_string(),
            "endTimeUnixNano": session.end_unix_nanos.to_string(),
            "attributes": attributes,
            "status": {"code": 1},
        }));
    }
    for statement in statements {
        let mut attributes = vec![
            string_attribute("db.system", "mysql"),
            string_attribute("db.system.name", "mysql"),
            string_attribute("db.operation", &statement.statement_type),
            string_attribute("db.operation.name", &statement.statement_type),
            string_attribute(
                "mydb.statement.digest",
                &format!("{:x}", Sha256::digest(statement.digest.as_bytes())),
            ),
            string_attribute("mydb.query.outcome", &statement.outcome),
            int_attribute("mydb.query.duration_ms", statement.duration_micros as i64 / 1_000),
            int_attribute("db.mysql.connection_id", i64::from(statement.connection_id)),
            string_attribute(
                "mysql.event_name",
                &format!(
                    "statement/sql/{}",
                    statement.statement_type.to_ascii_lowercase()
                ),
            ),
            int_attribute("mysql.lock_time", 0),
            string_attribute("mysql.digest_text", &statement.digest),
            int_attribute("mysql.sql_errno", i64::from(statement.sql_errno)),
            string_attribute(
                "mysql.sqlstate",
                statement.sqlstate.as_deref().unwrap_or("00000"),
            ),
            int_attribute("mysql.error_count", i64::from(statement.error.is_some())),
            int_attribute("mysql.warning_count", statement.warning_count as i64),
            int_attribute("mysql.rows_examined", 0),
            int_attribute("mysql.created_tmp_disk_tables", 0),
            int_attribute("mysql.created_tmp_tables", 0),
            int_attribute("mysql.select_full_join", 0),
            int_attribute("mysql.select_full_range_join", 0),
            int_attribute("mysql.select_range", 0),
            int_attribute("mysql.select_range_check", 0),
            int_attribute("mysql.select_scan", 0),
            int_attribute("mysql.sort_merge_passes", 0),
            int_attribute("mysql.sort_range", 0),
            int_attribute("mysql.sort_rows", 0),
            int_attribute("mysql.sort_scan", 0),
            int_attribute("mysql.no_index_used", 0),
            int_attribute("mysql.no_good_index_used", 0),
            int_attribute("mysql.max_controlled_memory", 0),
            int_attribute("mysql.max_total_memory", 0),
            int_attribute("mysql.cpu_time", statement.duration_micros as i64),
        ];
        if let Some(rows) = statement.rows_sent {
            attributes.push(int_attribute("db.response.returned_rows", rows as i64));
            attributes.push(int_attribute("mysql.rows_sent", rows as i64));
        }
        if let Some(affected) = statement.rows_affected {
            attributes.push(int_attribute("db.response.affected_rows", affected as i64));
            attributes.push(int_attribute("mysql.rows_affected", affected as i64));
        }
        if let Some(error) = statement.error.as_ref() {
            attributes.push(string_attribute(
                "mysql.message_text",
                if settings.include_sql {
                    error
                } else {
                    "SQL execution failed"
                },
            ));
            attributes.push(string_attribute(
                "error.message",
                if settings.include_sql {
                    error
                } else {
                    "SQL execution failed"
                },
            ));
        }
        if settings.include_sql {
            attributes.push(string_attribute("db.statement", &statement.sql));
            attributes.push(string_attribute("db.name", &statement.database));
            attributes.push(string_attribute("mysql.sql_text", &statement.sql));
            attributes.push(string_attribute("db.namespace", &statement.database));
            attributes.push(string_attribute("mysql.current_schema", &statement.database));
        }
        let status = if statement.error.is_some() {
            json!({"code": 2, "message": if settings.include_sql { statement.error.clone().unwrap_or_default() } else { "SQL execution failed".to_string() }})
        } else {
            json!({"code": 1})
        };
        otlp_spans.push(json!({
            "traceId": statement.trace_id,
            "spanId": statement.span_id,
            "parentSpanId": statement.parent_span_id,
            "name": "stmt",
            "kind": 1,
            "startTimeUnixNano": statement.start_unix_nanos.to_string(),
            "endTimeUnixNano": statement.end_unix_nanos.to_string(),
            "attributes": attributes,
            "status": status,
        }));
    }
    otlp_spans.sort_by_key(|span| {
        span.get("startTimeUnixNano")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    });
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

/// Mirror of MySQL 9's OTLP logs signal: one record per captured `tracing`
/// event, with the target and structured fields preserved as attributes.
fn otlp_logs_payload(settings: &OtlpSettings, logs: &[CapturedLog], server: &ServerIdentity) -> Value {
    let records: Vec<Value> = logs
        .iter()
        .map(|log| {
            let mut attributes = vec![string_attribute("code.target", &log.target)];
            for (key, value) in &log.attributes {
                attributes.push(string_attribute(key, value));
            }
            json!({
                "timeUnixNano": log.time_unix_nanos.to_string(),
                "severityNumber": log.severity_number,
                "severityText": log.severity_text,
                "body": {"stringValue": log.body},
                "attributes": attributes,
            })
        })
        .collect();
    json!({
        "resourceLogs": [{
            "resource": {
                "attributes": resource_attributes(settings, server)
            },
            "scopeLogs": [{
                "scope": {"name": "mydb", "version": server.version},
                "logRecords": records,
            }]
        }]
    })
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

/// Zone-map and CDC counters that complement the legacy `/metrics` block.
/// They are emitted separately so the original metric list keeps its exact
/// names and ordering for existing dashboards.
pub fn prometheus_olap_cdc_metrics(
    storage: &mydb_storage::StorageStatsSnapshot,
    cdc: Option<&mydb_cdc::CdcStatus>,
) -> String {
    let mut body = String::new();
    body.push_str("# HELP mydb_olap_pages_scanned_total Pages decoded by filtered scans.\n");
    body.push_str("# TYPE mydb_olap_pages_scanned_total counter\n");
    body.push_str(&format!(
        "mydb_olap_pages_scanned_total {}\n",
        storage.olap_pages_scanned
    ));
    body.push_str("# HELP mydb_olap_pages_pruned_total Pages skipped by zone-map pruning.\n");
    body.push_str("# TYPE mydb_olap_pages_pruned_total counter\n");
    body.push_str(&format!(
        "mydb_olap_pages_pruned_total {}\n",
        storage.olap_pages_pruned
    ));
    body.push_str("# HELP mydb_olap_rows_pruned_total Rows skipped without decoding.\n");
    body.push_str("# TYPE mydb_olap_rows_pruned_total counter\n");
    body.push_str(&format!(
        "mydb_olap_rows_pruned_total {}\n",
        storage.olap_rows_pruned
    ));
    body.push_str("# HELP mydb_cdc_enabled Whether CDC is enabled.\n");
    body.push_str("# TYPE mydb_cdc_enabled gauge\n");
    body.push_str(&format!(
        "mydb_cdc_enabled {}\n",
        u8::from(cdc.is_some_and(|status| status.enabled))
    ));
    if let Some(status) = cdc {
        body.push_str("# HELP mydb_cdc_last_lsn Last committed LSN emitted to the CDC stream.\n");
        body.push_str("# TYPE mydb_cdc_last_lsn gauge\n");
        body.push_str(&format!("mydb_cdc_last_lsn {}\n", status.last_lsn));
        body.push_str("# HELP mydb_cdc_checkpoint_lsn Consumer-confirmed LSN.\n");
        body.push_str("# TYPE mydb_cdc_checkpoint_lsn gauge\n");
        body.push_str(&format!(
            "mydb_cdc_checkpoint_lsn {}\n",
            status.checkpoint_lsn
        ));
        body.push_str("# HELP mydb_cdc_lag_lsn LSN distance between producer and consumer.\n");
        body.push_str("# TYPE mydb_cdc_lag_lsn gauge\n");
        body.push_str(&format!("mydb_cdc_lag_lsn {}\n", status.lag_lsn()));
        body.push_str("# HELP mydb_cdc_events_total CDC commit events written.\n");
        body.push_str("# TYPE mydb_cdc_events_total counter\n");
        body.push_str(&format!(
            "mydb_cdc_events_total {}\n",
            status.events_emitted
        ));
        body.push_str("# HELP mydb_cdc_changes_total CDC row/DDL changes written.\n");
        body.push_str("# TYPE mydb_cdc_changes_total counter\n");
        body.push_str(&format!(
            "mydb_cdc_changes_total {}\n",
            status.changes_emitted
        ));
    }
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
            protocol: OtlpProtocol::Protobuf,
            compression: OtlpCompression::Gzip,
            endpoint: "http://127.0.0.1:4318".to_string(),
            interval: Duration::from_secs(15),
            timeout: Duration::from_secs(5),
            service_name: "mydb".to_string(),
            headers: BTreeMap::new(),
            auth: OtlpAuth::None,
            export_traces: true,
            export_logs: false,
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
            olap_pages_scanned: 0,
            olap_pages_pruned: 0,
            olap_rows_pruned: 0,
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
            cdc: None,
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

    fn statement_trace() -> StatementTrace {
        StatementTrace {
            id: 7,
            trace_id: "0af7651916cd43dd8448eb211c80319c".to_string(),
            parent_span_id: "b7ad6b7169203331".to_string(),
            span_id: "0000000000000007".to_string(),
            start_unix_nanos: 1_700_000_000_000_000_000,
            end_unix_nanos: 1_700_000_000_250_000_000,
            duration_micros: 250_000,
            connection_id: 3,
            user: "app".to_string(),
            database: "shop".to_string(),
            statement_type: "SELECT".to_string(),
            digest: "select * from `t` where id = ?".to_string(),
            sql: "select * from t where id = 42".to_string(),
            outcome: "rows".to_string(),
            rows_sent: Some(10),
            rows_affected: None,
            error: None,
            sql_errno: 0,
            sqlstate: None,
            warning_count: 0,
        }
    }

    #[test]
    fn traces_payload_exports_mysql9_spans() {
        let session = SessionTrace {
            id: 2,
            trace_id: "0af7651916cd43dd8448eb211c80319d".to_string(),
            span_id: "b7ad6b7169203332".to_string(),
            start_unix_nanos: 1_699_999_000_000_000_000,
            end_unix_nanos: 1_700_000_001_000_000_000,
            processlist_id: 3,
            thread_id: 3,
            user: "app".to_string(),
            host: "127.0.0.1".to_string(),
            database: Some("shop".to_string()),
        };
        let control = ControlTrace {
            id: 1,
            trace_id: "0af7651916cd43dd8448eb211c80319e".to_string(),
            span_id: "b7ad6b7169203333".to_string(),
            time_unix_nanos: 1_700_000_000_000_000_000,
            trace_enabled: true,
            metrics_enabled: true,
            logs_enabled: false,
            details: "startup".to_string(),
        };
        let payload = otlp_traces_payload(
            &settings(),
            std::slice::from_ref(&statement_trace()),
            &[session],
            &[control],
            &server_identity(),
        );
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .expect("spans");
        assert_eq!(spans.len(), 3);
        let names: Vec<&str> = spans
            .iter()
            .filter_map(|span| span["name"].as_str())
            .collect();
        assert!(names.contains(&"Control"));
        assert!(names.contains(&"session"));
        assert!(names.contains(&"stmt"));
        let stmt = spans
            .iter()
            .find(|span| span["name"] == json!("stmt"))
            .expect("stmt");
        assert_eq!(stmt["kind"], json!(1));
        assert_eq!(stmt["status"]["code"], json!(1));
        assert_eq!(stmt["traceId"].as_str().map(str::len), Some(32));
        assert_eq!(stmt["spanId"].as_str().map(str::len), Some(16));
        let attributes = stmt["attributes"].as_array().expect("attributes");
        assert!(attributes
            .iter()
            .any(|attribute| attribute["key"] == json!("db.system")
                && attribute["value"]["stringValue"] == json!("mysql")));
        assert!(attributes.iter().any(|attribute| {
            attribute["key"] == json!("mysql.cpu_time")
                && attribute["value"]["intValue"] == json!("250000")
        }));
        assert!(attributes
            .iter()
            .any(|attribute| attribute["key"] == json!("mysql.rows_sent")
                && attribute["value"]["intValue"] == json!("10")));
        assert!(!attributes
            .iter()
            .any(|attribute| attribute["key"] == json!("db.statement")));
        let session_span = spans
            .iter()
            .find(|span| span["name"] == json!("session"))
            .expect("session");
        let session_attributes = session_span["attributes"].as_array().expect("attributes");
        assert!(session_attributes
            .iter()
            .any(|attribute| attribute["key"] == json!("mysql.processlist_id")));
        let control_span = spans
            .iter()
            .find(|span| span["name"] == json!("Control"))
            .expect("control");
        let control_attributes = control_span["attributes"].as_array().expect("attributes");
        assert!(control_attributes
            .iter()
            .any(|attribute| attribute["key"] == json!("trace_enabled")
                && attribute["value"]["boolValue"] == json!(true)));
    }

    #[test]
    fn protobuf_encoder_emits_otlp_trace_request() {
        let payload = otlp_traces_payload(
            &settings(),
            std::slice::from_ref(&statement_trace()),
            &[],
            &[],
            &server_identity(),
        );
        let encoded = encode_otlp_protobuf("/v1/traces", &payload).expect("protobuf traces");
        assert!(!encoded.is_empty());
        // ExportTraceServiceRequest.resource_spans is field 1, length-delimited.
        assert_eq!(encoded[0], 0x0a);
    }

    #[test]
    fn traces_payload_includes_sql_only_when_configured() {
        let mut snapshot = statement_trace();
        snapshot.id = 9;
        snapshot.statement_type = "UPDATE".to_string();
        snapshot.digest = "update `t` set a = ?".to_string();
        snapshot.sql = "update t set a = 1".to_string();
        snapshot.error = Some("deadlock".to_string());
        snapshot.sql_errno = 1213;
        snapshot.sqlstate = Some("40001".to_string());
        snapshot.outcome = "error".to_string();
        snapshot.rows_sent = None;
        snapshot.rows_affected = Some(1);
        let mut enabled = settings();
        let private_payload = otlp_traces_payload(
            &enabled,
            std::slice::from_ref(&snapshot),
            &[],
            &[],
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
            &[],
            &[],
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
    fn prometheus_labels_are_escaped() {
        assert_eq!(escape_prometheus_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn otlp_basic_auth_uses_standard_base64_and_redacts_debug() {
        let auth = OtlpAuth::Basic {
            username: "user".to_string(),
            password: "p@ss".to_string(),
        };
        assert_eq!(
            auth.authorization_header().as_deref(),
            Some("Basic dXNlcjpwQHNz")
        );
        let debug = format!("{auth:?}");
        assert!(!debug.contains("p@ss"));
        assert!(debug.contains("<redacted>"));
        assert_eq!(auth.method_name(), "basic");
    }

    #[test]
    fn otlp_bearer_auth_header_and_empty_auth() {
        assert_eq!(
            OtlpAuth::Bearer("token-value".to_string())
                .authorization_header()
                .as_deref(),
            Some("Bearer token-value")
        );
        assert_eq!(OtlpAuth::None.authorization_header(), None);
        assert_eq!(OtlpAuth::None.method_name(), "none");
    }

    #[test]
    fn otlp_settings_from_section_resolves_password_file_without_exposing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("otlp-password");
        std::fs::write(&path, "file-secret\n").unwrap();
        let mut section = mydb_config::OtlpSection::default();
        section.auth.method = "basic".into();
        section.auth.username = "collector".into();
        section.auth.password_file = Some(path);
        let settings = OtlpSettings::from_section(&section);
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("collector:file-secret")
        );
        assert_eq!(
            settings.auth.authorization_header().as_deref(),
            Some(expected.as_str())
        );
        assert!(!format!("{:?}", settings.auth).contains("file-secret"));
    }

    #[test]
    fn logs_payload_uses_otlp_log_shape_and_bounded_fields() {
        let log = CapturedLog {
            id: 1,
            time_unix_nanos: 1_700_000_000_000_000_000,
            severity_number: 13,
            severity_text: "WARN".to_string(),
            body: "test warning".to_string(),
            target: "mydb_server::test".to_string(),
            attributes: BTreeMap::from([("retry".to_string(), "2".to_string())]),
        };
        let payload = otlp_logs_payload(&settings(), &[log], &server_identity());
        let record = &payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(record["severityNumber"], json!(13));
        assert_eq!(record["severityText"], json!("WARN"));
        assert_eq!(record["body"]["stringValue"], json!("test warning"));
        let attributes = record["attributes"].as_array().expect("attributes");
        assert!(attributes.iter().any(|attribute| {
            attribute["key"] == json!("code.target")
                && attribute["value"]["stringValue"] == json!("mydb_server::test")
        }));
        assert!(attributes.iter().any(|attribute| {
            attribute["key"] == json!("retry")
                && attribute["value"]["stringValue"] == json!("2")
        }));
    }

    #[test]
    fn sensitive_log_keys_are_redacted() {
        assert!(is_sensitive_log_key("password"));
        assert!(is_sensitive_log_key("authorization"));
        assert!(is_sensitive_log_key("MYDB_OTLP_TOKEN"));
        assert!(!is_sensitive_log_key("rows"));
    }

    #[test]
    fn log_buffer_is_bounded_and_reports_only_new_records() {
        let buffer = OtlpLogBuffer::new();
        for id in 0..(MAX_BUFFERED_LOGS + 10) {
            buffer.push(CapturedLog {
                id: 0,
                time_unix_nanos: 1,
                severity_number: 9,
                severity_text: "INFO".to_string(),
                body: format!("log {id}"),
                target: "test".to_string(),
                attributes: BTreeMap::new(),
            });
        }
        let pending = buffer.since(0);
        assert_eq!(pending.len(), MAX_EXPORTED_LOGS);
        assert_eq!(pending.last().map(|log| log.id), Some((MAX_BUFFERED_LOGS + 10) as u64));
        assert!(buffer.since(pending.last().expect("last log").id).is_empty());
    }
}
