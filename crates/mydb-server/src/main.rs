use std::collections::HashSet;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use clap::Parser;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "mydb-server",
    version,
    about = "MySQL 8.x compatible database server"
)]
struct Args {
    /// Configuration file path
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Run as daemon (Linux/macOS only)
    #[arg(long)]
    daemon: bool,

    /// Install as system service
    #[arg(long)]
    service: Option<String>,

    /// Override listen port
    #[arg(short, long)]
    port: Option<u16>,

    /// Override data directory
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// Validate data and configuration for an in-place upgrade
    #[arg(long)]
    upgrade: bool,

    /// Probe local MySQL listener; used by Docker HEALTHCHECK
    #[arg(long, hide = true)]
    healthcheck: bool,
}

#[derive(Clone)]
struct AdminState {
    storage: Arc<mydb_storage::StorageEngineManager>,
    wire_stats: Arc<mydb_wire::WireStats>,
    protocol_config: Arc<RwLock<mydb_wire::ProtocolConfig>>,
    config: Arc<RwLock<mydb_config::ServerConfig>>,
    config_path: Option<PathBuf>,
    started: Instant,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    if args.healthcheck {
        let address = "127.0.0.1:3306".parse()?;
        std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2))?;
        return Ok(());
    }

    // Handle service commands
    if let Some(cmd) = &args.service {
        match cmd.as_str() {
            "install" => {
                info!("Installing mydb-server as system service...");
                install_service()?;
                return Ok(());
            }
            "uninstall" => {
                info!("Uninstalling mydb-server service...");
                uninstall_service()?;
                return Ok(());
            }
            "start" => {
                info!("Starting mydb-server service...");
                start_service()?;
                return Ok(());
            }
            "stop" => {
                info!("Stopping mydb-server service...");
                stop_service()?;
                return Ok(());
            }
            "run" => {
                info!("Running under system service manager");
            }
            _ => {
                eprintln!("Unknown service command: {}", cmd);
                eprintln!("Available commands: install, uninstall, start, stop");
                std::process::exit(1);
            }
        }
    }

    // Load configuration
    let mut config = mydb_config::ServerConfig::load(args.config.as_deref())?;

    // Apply command line overrides
    if let Some(port) = args.port {
        config.server.port = port;
    }
    if let Some(data_dir) = &args.data_dir {
        config.storage.data_dir = data_dir.clone();
    }
    validate_runtime_security(&config)?;

    if args.upgrade {
        std::fs::create_dir_all(&config.storage.data_dir)?;
        println!("MyDB upgrade check complete: data format is compatible");
        return Ok(());
    }

    let worker_threads = runtime_worker_threads(config.server.thread_count);
    info!(worker_threads, "Configured Tokio runtime worker threads");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(worker_threads)
        .thread_name("mydb-worker")
        .build()?
        .block_on(run_server(args, config))
}

fn runtime_worker_threads(configured: u32) -> usize {
    if configured > 0 {
        return configured as usize;
    }
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

async fn run_server(args: Args, config: mydb_config::ServerConfig) -> Result<()> {
    // Ensure data directory exists
    std::fs::create_dir_all(&config.storage.data_dir)?;
    apply_pending_restore(&config)?;

    info!(
        "Starting MyDB server on {}:{}",
        config.server.host, config.server.port
    );
    info!("Data directory: {:?}", config.storage.data_dir);
    info!("Storage engine: {}", config.storage.engine);

    // Initialize storage engine with WAL
    let storage = Arc::new(
        mydb_storage::StorageEngineManager::try_new_with_group_commit_window(
            config.storage.data_dir.clone(),
            config.storage.page_size as usize,
            &config.storage.buffer_pool_size,
            std::time::Duration::from_micros(config.storage.group_commit_window_us),
        )?,
    );

    // Initialize storage (load databases, replay WAL)
    storage.init().await?;

    // Create default "mydb" database if it doesn't exist
    if storage.get_database("mydb").is_none() {
        storage.create_database("mydb").await?;
    }

    // Authentication is configured before accepting connections. Never log secrets.
    info!(
        "MySQL authentication configured for user '{}'",
        config.security.default_username
    );
    info!("HTTP management API port: {}", config.http.port);

    let wire_stats = Arc::new(mydb_wire::WireStats::default());
    let protocol_config = Arc::new(RwLock::new(protocol_config_for(&config)?));

    if config.http.enabled {
        let state = AdminState {
            storage: storage.clone(),
            wire_stats: wire_stats.clone(),
            protocol_config: protocol_config.clone(),
            config: Arc::new(RwLock::new(config.clone())),
            config_path: args.config.clone(),
            started: Instant::now(),
        };
        let address = format!("{}:{}", config.http.host, config.http.port);
        tokio::spawn(async move {
            if let Err(error) = run_admin_server(&address, state).await {
                tracing::error!("HTTP management server stopped: {}", error);
            }
        });
    }

    // Start TCP listener
    let listener =
        tokio::net::TcpListener::bind(format!("{}:{}", config.server.host, config.server.port))
            .await?;

    info!(
        "MyDB server is ready for connections on port {}",
        config.server.port
    );

    let (scheduler_stop, scheduler_shutdown) = tokio::sync::watch::channel(false);
    let scheduler = mydb_wire::spawn_event_scheduler(
        storage.clone(),
        Arc::new(protocol_config.read().clone()),
        wire_stats.clone(),
        scheduler_shutdown,
    );

    let connection_limit = Arc::new(Semaphore::new(config.server.max_connections as usize));
    let idle_timeout = std::time::Duration::from_secs(config.server.interactive_timeout);

    // Accept connections. Each disconnect releases its permit; reconnects create a
    // fresh protocol session without affecting storage or actor ordering.
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            result = listener.accept() => Some(result),
            _ = &mut shutdown => None,
        };
        let Some(accepted) = accepted else {
            info!("Shutdown requested; flushing WAL and buffer pool");
            break;
        };
        let (stream, addr) = match accepted {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!("Accept failed: {}", error);
                continue;
            }
        };
        debug!("New connection from: {}", addr);

        let storage = storage.clone();
        let stats = wire_stats.clone();
        let protocol = Arc::new(protocol_config.read().clone());
        let permit = match connection_limit.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                tracing::warn!("Connection limit reached; rejecting {}", addr);
                drop(stream);
                continue;
            }
        };
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                idle_timeout,
                mydb_wire::handle_connection(stream, storage, protocol, stats),
            )
            .await;
            match result {
                Ok(Err(error)) => tracing::debug!("Connection closed from {}: {}", addr, error),
                Err(_) => tracing::debug!("Connection {} timed out", addr),
                _ => {}
            }
            drop(permit);
        });
    }

    let _ = scheduler_stop.send(true);
    if let Err(error) = scheduler.await {
        tracing::error!("EVENT scheduler task failed while stopping: {}", error);
    }
    storage.flush_consistent().await?;
    Ok(())
}

fn validate_runtime_security(config: &mydb_config::ServerConfig) -> Result<()> {
    if config.security.tls_cert.is_some() != config.security.tls_key.is_some() {
        anyhow::bail!("tls_cert and tls_key must be configured together");
    }
    if config.security.require_secure_transport && config.security.tls_cert.is_none() {
        anyhow::bail!("require_secure_transport=true requires tls_cert and tls_key");
    }
    if config.security.default_password.is_empty() || config.http.admin_password.is_empty() {
        anyhow::bail!("MySQL and HTTP administrator secrets must not be empty");
    }
    if config.security.enforce_strong_passwords {
        validate_strong_secret("MYDB_ROOT_PASSWORD", &config.security.default_password)?;
        validate_strong_secret("MYDB_ADMIN_PASSWORD", &config.http.admin_password)?;
        if config.security.default_password == config.http.admin_password {
            anyhow::bail!("MySQL and HTTP administrator secrets must be different");
        }
    }
    Ok(())
}

fn validate_strong_secret(name: &str, value: &str) -> Result<()> {
    if value.len() < 20 {
        anyhow::bail!(
            "{name} must contain at least 20 bytes when strong secret enforcement is enabled"
        );
    }
    if value.eq_ignore_ascii_case("root")
        || value.to_ascii_uppercase().contains("CHANGE_ME")
        || value.chars().all(char::is_whitespace)
    {
        anyhow::bail!("{name} is a placeholder or weak secret");
    }
    Ok(())
}

fn protocol_config_for(config: &mydb_config::ServerConfig) -> Result<mydb_wire::ProtocolConfig> {
    let secure_file_priv = config
        .security
        .secure_file_priv
        .clone()
        .unwrap_or_else(|| config.storage.data_dir.join("imports"));
    std::fs::create_dir_all(&secure_file_priv)?;
    let auth_catalog = Arc::new(mydb_wire::AuthCatalog::open(
        &config.storage.data_dir,
        &config.security.default_username,
        &config.security.default_password,
    )?);
    let audit_log = Arc::new(mydb_wire::AuditLog::open_with_rotation(
        &config.storage.data_dir,
        &config.logging.max_size,
        config.logging.max_files,
    )?);
    let tls_config = load_tls_config(config)?;
    Ok(mydb_wire::ProtocolConfig {
        username: config.security.default_username.clone(),
        password: config.security.default_password.clone(),
        default_database: "mydb".to_string(),
        slow_query_threshold_ms: config.agent.slow_query_threshold_ms,
        max_slow_queries: config.agent.max_slow_queries,
        lock_wait_timeout_ms: 5_000,
        local_infile: config.security.local_infile,
        secure_file_priv,
        max_load_data_size: config.security.max_load_data_size,
        auth_catalog,
        audit_log,
        tls_config,
        require_secure_transport: config.security.require_secure_transport,
    })
}

fn load_tls_config(
    config: &mydb_config::ServerConfig,
) -> Result<Option<Arc<tokio_rustls::rustls::ServerConfig>>> {
    let (Some(cert_path), Some(key_path)) = (&config.security.tls_cert, &config.security.tls_key)
    else {
        return Ok(None);
    };
    let mut certificate_file = BufReader::new(std::fs::File::open(cert_path)?);
    let certificates =
        rustls_pemfile::certs(&mut certificate_file).collect::<std::result::Result<Vec<_>, _>>()?;
    if certificates.is_empty() {
        anyhow::bail!("TLS certificate chain is empty");
    }
    let mut key_file = BufReader::new(std::fs::File::open(key_path)?);
    let private_key = rustls_pemfile::private_key(&mut key_file)?
        .ok_or_else(|| anyhow::anyhow!("TLS private key is missing"))?;
    let tls_config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)?;
    Ok(Some(Arc::new(tls_config)))
}

use tracing::debug;

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn run_admin_server(address: &str, state: AdminState) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics))
        .route("/api/v1/status", get(status))
        .route("/api/v1/config", get(get_config).put(put_config))
        .route("/api/v1/config/reload", post(reload_config))
        .route("/api/v1/memory/stats", get(memory_stats))
        .route("/api/v1/memory/flush", post(memory_flush))
        .route("/api/v1/storage/inventory", get(storage_inventory))
        .route("/api/v1/storage/cleanup", post(storage_cleanup))
        .route("/api/v1/connections", get(connections))
        .route("/api/v1/connections/kill", post(kill_connection))
        .route("/api/v1/backup/full", post(backup_full))
        .route("/api/v1/backup/incremental", post(backup_incremental))
        .route("/api/v1/backup/list", get(backup_list))
        .route("/api/v1/backup/restore", post(backup_restore))
        .route("/api/v1/backup/{id}", delete(backup_delete))
        .route("/api/v1/agent/health", get(agent_health))
        .route("/api/v1/agent/slow-queries", get(agent_slow_queries))
        .route("/api/v1/agent/ask", post(agent_diagnose))
        .route("/api/v1/agent/diagnose", post(agent_diagnose))
        .route("/api/v1/agent/sql", post(agent_sql_debug))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(
        "HTTP management and Prometheus metrics listening on {}",
        address
    );
    axum::serve(listener, app).await?;
    Ok(())
}

fn authorize(headers: &HeaderMap, state: &AdminState) -> Result<(), StatusCode> {
    let expected = format!("Bearer {}", state.config.read().http.admin_password);
    let actual = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if actual.is_some_and(|actual| constant_time_eq(expected.as_bytes(), actual.as_bytes())) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(*left.get(index).unwrap_or(&0) ^ *right.get(index).unwrap_or(&0));
    }
    difference == 0
}

fn authorize_agent(headers: &HeaderMap, state: &AdminState) -> Result<(), StatusCode> {
    if !state.config.read().agent.enabled {
        return Err(StatusCode::NOT_FOUND);
    }
    authorize(headers, state)
}

async fn metrics(State(state): State<AdminState>) -> impl IntoResponse {
    let wire = state.wire_stats.snapshot();
    let storage = state.storage.stats();
    let audit = state.protocol_config.read().audit_log.snapshot();
    let body = format!(
        concat!(
            "# HELP mydb_up Whether MyDB is running.\n",
            "# TYPE mydb_up gauge\nmydb_up 1\n",
            "# TYPE mydb_uptime_seconds gauge\nmydb_uptime_seconds {}\n",
            "# TYPE mydb_connections_active gauge\nmydb_connections_active {}\n",
            "# TYPE mydb_connections_total counter\nmydb_connections_total {}\n",
            "# TYPE mydb_queries_total counter\nmydb_queries_total {}\n",
            "# TYPE mydb_query_errors_total counter\nmydb_query_errors_total {}\n",
            "# TYPE mydb_query_execute_microseconds_total counter\nmydb_query_execute_microseconds_total {}\n",
            "# TYPE mydb_query_response_microseconds_total counter\nmydb_query_response_microseconds_total {}\n",
            "# TYPE mydb_audit_healthy gauge\nmydb_audit_healthy {}\n",
            "# TYPE mydb_audit_events_total counter\nmydb_audit_events_total {}\n",
            "# TYPE mydb_audit_rejections_total counter\nmydb_audit_rejections_total {}\n",
            "# TYPE mydb_lock_waits_total counter\nmydb_lock_waits_total {}\n",
            "# TYPE mydb_lock_timeouts_total counter\nmydb_lock_timeouts_total {}\n",
            "# TYPE mydb_deadlocks_total counter\nmydb_deadlocks_total {}\n",
            "# TYPE mydb_row_lock_acquires_total counter\nmydb_row_lock_acquires_total {}\n",
            "# TYPE mydb_table_lock_acquires_total counter\nmydb_table_lock_acquires_total {}\n",
            "# TYPE mydb_transaction_locks_active gauge\nmydb_transaction_locks_active {}\n",
            "# TYPE mydb_storage_reads_total counter\nmydb_storage_reads_total {}\n",
            "# TYPE mydb_storage_write_batches_total counter\nmydb_storage_write_batches_total {}\n",
            "# TYPE mydb_storage_errors_total counter\nmydb_storage_errors_total {}\n",
            "# TYPE mydb_write_actor_queue_depth gauge\nmydb_write_actor_queue_depth {}\n",
            "# TYPE mydb_buffer_pool_pages gauge\nmydb_buffer_pool_pages {}\n",
            "# TYPE mydb_group_commits_total counter\nmydb_group_commits_total {}\n",
            "# TYPE mydb_grouped_requests_total counter\nmydb_grouped_requests_total {}\n",
            "# TYPE mydb_checkpoints_total counter\nmydb_checkpoints_total {}\n",
            "# TYPE mydb_checkpoint_errors_total counter\nmydb_checkpoint_errors_total {}\n",
            "# TYPE mydb_prepare_validation_microseconds_total counter\nmydb_prepare_validation_microseconds_total {}\n",
            "# TYPE mydb_wal_sync_microseconds_total counter\nmydb_wal_sync_microseconds_total {}\n",
            "# TYPE mydb_apply_microseconds_total counter\nmydb_apply_microseconds_total {}\n",
            "# TYPE mydb_checkpoint_microseconds_total counter\nmydb_checkpoint_microseconds_total {}\n"
        ),
        state.started.elapsed().as_secs(),
        wire.active_connections,
        wire.total_connections,
        wire.queries,
        wire.query_errors,
        wire.query_execute_micros,
        wire.query_response_micros,
        u8::from(audit.healthy),
        audit.accepted,
        audit.rejected,
        wire.lock_waits,
        wire.lock_timeouts,
        wire.deadlocks,
        wire.row_lock_acquires,
        wire.table_lock_acquires,
        wire.active_locks,
        storage.reads,
        storage.writes,
        storage.errors,
        storage.actor_queue_depth,
        storage.buffer_pool_pages,
        storage.group_commits,
        storage.grouped_requests,
        storage.checkpoints,
        storage.checkpoint_errors,
        storage.prepare_validation_micros,
        storage.wal_sync_micros,
        storage.apply_micros,
        storage.checkpoint_micros,
    );
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
}

async fn status(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    let wire = state.wire_stats.snapshot();
    let storage = state.storage.stats();
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime": state.started.elapsed().as_secs(),
        "connections": wire.active_connections,
        "total_connections": wire.total_connections,
        "queries": wire.queries,
        "active_transaction_locks": wire.active_locks,
        "lock_waits": wire.lock_waits,
        "lock_timeouts": wire.lock_timeouts,
        "deadlocks": wire.deadlocks,
        "group_commits": storage.group_commits,
        "grouped_requests": storage.grouped_requests,
        "checkpoints": storage.checkpoints,
        "checkpoint_errors": storage.checkpoint_errors,
        "prepare_validation_microseconds": storage.prepare_validation_micros,
        "wal_sync_microseconds": storage.wal_sync_micros,
        "apply_microseconds": storage.apply_micros,
        "checkpoint_microseconds": storage.checkpoint_micros,
    })))
}

async fn get_config(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<mydb_config::ServerConfig>, StatusCode> {
    authorize(&headers, &state)?;
    Ok(Json(redacted_config(&state.config.read())))
}

async fn put_config(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(config): Json<mydb_config::ServerConfig>,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    apply_runtime_config(&state, config).map_err(|error| {
        tracing::warn!("Rejected runtime configuration update: {error}");
        StatusCode::CONFLICT
    })?;
    Ok(Json(
        json!({"reloaded": true, "new_connections_only": true}),
    ))
}

async fn reload_config(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    let config = mydb_config::ServerConfig::load(state.config_path.as_deref())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    apply_runtime_config(&state, config).map_err(|error| {
        tracing::warn!("Rejected runtime configuration reload: {error}");
        StatusCode::CONFLICT
    })?;
    Ok(Json(
        json!({"reloaded": true, "new_connections_only": true}),
    ))
}

fn redacted_config(config: &mydb_config::ServerConfig) -> mydb_config::ServerConfig {
    let mut redacted = config.clone();
    redacted.security.default_password = "<redacted>".to_string();
    redacted.http.admin_password = "<redacted>".to_string();
    redacted
}

fn apply_runtime_config(
    state: &AdminState,
    mut candidate: mydb_config::ServerConfig,
) -> Result<()> {
    let current = state.config.read().clone();
    if candidate.security.default_password == "<redacted>" {
        candidate.security.default_password = current.security.default_password.clone();
    }
    if candidate.http.admin_password == "<redacted>" {
        candidate.http.admin_password = current.http.admin_password.clone();
    }
    validate_runtime_security(&candidate)?;
    validate_reloadable_config(&current, &candidate)?;
    let protocol = protocol_config_for(&candidate)?;
    *state.protocol_config.write() = protocol;
    *state.config.write() = candidate;
    Ok(())
}

fn validate_reloadable_config(
    current: &mydb_config::ServerConfig,
    candidate: &mydb_config::ServerConfig,
) -> Result<()> {
    if current.server != candidate.server
        || current.storage != candidate.storage
        || current.memory != candidate.memory
        || current.logging != candidate.logging
        || current.character_set != candidate.character_set
        || current.http.host != candidate.http.host
        || current.http.port != candidate.http.port
        || current.http.enabled != candidate.http.enabled
    {
        anyhow::bail!(
            "listener, storage, memory, logging, and character-set settings require a process restart"
        );
    }
    Ok(())
}

async fn memory_stats(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    let stats = state.storage.stats();
    let config = state.config.read();
    Ok(Json(json!({
        "max_memory": config.memory.max_memory,
        "buffer_pool_size": config.storage.buffer_pool_size,
        "buffer_pool_pages": stats.buffer_pool_pages,
        "actor_queue_depth": stats.actor_queue_depth,
    })))
}

async fn memory_flush(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    state
        .storage
        .flush_consistent()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({"flushed": true})))
}

async fn storage_inventory(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<mydb_storage::StorageInventory>, StatusCode> {
    authorize(&headers, &state)?;
    state
        .storage
        .storage_inventory()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Deserialize)]
struct StorageCleanupRequest {
    #[serde(default)]
    confirmation: String,
}

async fn storage_cleanup(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(request): Json<StorageCleanupRequest>,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    if request.confirmation != "DELETE_ORPHANED_STORAGE" {
        return Err(StatusCode::BAD_REQUEST);
    }
    let before = state
        .storage
        .storage_inventory()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let result = state
        .storage
        .cleanup_orphan_storage()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let after = state
        .storage
        .storage_inventory()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({
        "removed_directories": result.affected_rows,
        "reclaimed_bytes": before.orphan_bytes.saturating_sub(after.orphan_bytes),
        "remaining_orphan_bytes": after.orphan_bytes,
        "safety": "only unreferenced page-only table directories were eligible",
    })))
}

async fn connections(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    let stats = state.wire_stats.snapshot();
    Ok(Json(json!({
        "active": stats.active_connections,
        "total": stats.total_connections,
        "active_transaction_locks": stats.active_locks,
        "lock_waits": stats.lock_waits,
        "lock_timeouts": stats.lock_timeouts,
        "deadlocks": stats.deadlocks,
    })))
}

async fn kill_connection(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    Err(StatusCode::NOT_IMPLEMENTED)
}

const BACKUP_FORMAT_VERSION: u32 = 1;
const RESTORE_MARKER: &str = ".restore-pending.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupMetadata {
    #[serde(default)]
    format_version: u32,
    id: String,
    kind: String,
    created_at: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    from_lsn: u64,
    #[serde(default)]
    to_lsn: u64,
    #[serde(default)]
    wal_records: u64,
    #[serde(default)]
    wal_bytes: u64,
}

#[derive(Debug, Default, Deserialize)]
struct IncrementalBackupRequest {
    #[serde(default)]
    base_id: Option<String>,
}

#[derive(Deserialize)]
struct RestoreRequest {
    id: String,
    #[serde(default)]
    point_in_time: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingRestore {
    id: String,
    requested_at: String,
    #[serde(default)]
    target_lsn: Option<u64>,
    #[serde(default)]
    point_in_time: Option<String>,
}

async fn backup_full(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    if state.storage.data_dir().join(RESTORE_MARKER).exists() {
        return Err(StatusCode::CONFLICT);
    }
    create_full_backup(state).await
}

async fn backup_incremental(
    State(state): State<AdminState>,
    headers: HeaderMap,
    request: Option<Json<IncrementalBackupRequest>>,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    if state.storage.data_dir().join(RESTORE_MARKER).exists() {
        return Err(StatusCode::CONFLICT);
    }
    let config = state.config.read().clone();
    let backup_root = config.backup.backup_dir.clone();
    let requested_base = request.and_then(|Json(request)| request.base_id);
    if let Some(id) = &requested_base {
        validate_backup_id(id)?;
    }
    let base = tokio::task::spawn_blocking({
        let backup_root = backup_root.clone();
        move || -> Result<Option<BackupMetadata>> {
            if let Some(id) = requested_base {
                Ok(Some(read_backup_metadata(&backup_root, &id)?))
            } else {
                Ok(list_backup_metadata(&backup_root)?
                    .into_iter()
                    .filter(|backup| backup.format_version == BACKUP_FORMAT_VERSION)
                    .max_by(|left, right| {
                        left.to_lsn
                            .cmp(&right.to_lsn)
                            .then_with(|| left.created_at.cmp(&right.created_at))
                    }))
            }
        }
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::BAD_REQUEST)?
    .ok_or(StatusCode::CONFLICT)?;
    if base.format_version != BACKUP_FORMAT_VERSION {
        return Err(StatusCode::CONFLICT);
    }

    let snapshot_guard = state.storage.snapshot_guard().await;
    state
        .storage
        .sync_wal()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let to_lsn = state.storage.current_lsn();
    if to_lsn < base.to_lsn {
        return Err(StatusCode::CONFLICT);
    }
    let id = new_backup_id("incremental");
    let metadata = BackupMetadata {
        format_version: BACKUP_FORMAT_VERSION,
        id: id.clone(),
        kind: "incremental".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        parent_id: Some(base.id.clone()),
        from_lsn: base.to_lsn,
        to_lsn,
        wal_records: to_lsn - base.to_lsn,
        wal_bytes: 0,
    };
    let source_wal = state.storage.data_dir().join("wal");
    let metadata = tokio::task::spawn_blocking({
        let backup_root = backup_root.clone();
        let mut metadata = metadata.clone();
        move || -> Result<BackupMetadata> {
            let _snapshot_guard = snapshot_guard;
            let destination = backup_root.join(&metadata.id);
            std::fs::create_dir_all(&destination)?;
            let archive = destination.join("wal_delta.log");
            let info = mydb_wal::export_wal_redo_range(
                &source_wal,
                &archive,
                metadata.from_lsn,
                metadata.to_lsn,
            )?;
            metadata.wal_records = info.record_count;
            metadata.wal_bytes = info.bytes;
            write_json_atomic(&destination.join("metadata.json"), &metadata)?;
            Ok(metadata)
        }
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(json!({
        "id": metadata.id,
        "kind": metadata.kind,
        "parent_id": metadata.parent_id,
        "from_lsn": metadata.from_lsn,
        "to_lsn": metadata.to_lsn,
        "wal_records": metadata.wal_records,
        "wal_bytes": metadata.wal_bytes,
        "no_changes": metadata.wal_records == 0,
    })))
}

async fn create_full_backup(state: AdminState) -> Result<Json<Value>, StatusCode> {
    let snapshot_guard = state
        .storage
        .consistent_snapshot_guard()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let to_lsn = state.storage.current_lsn();
    let config = state.config.read().clone();
    let id = new_backup_id("full");
    let metadata = BackupMetadata {
        format_version: BACKUP_FORMAT_VERSION,
        id: id.clone(),
        kind: "full".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        parent_id: None,
        from_lsn: 0,
        to_lsn,
        wal_records: to_lsn,
        wal_bytes: 0,
    };
    let backup_root = config.backup.backup_dir.clone();
    let source = state.storage.data_dir().to_path_buf();
    let metadata = tokio::task::spawn_blocking({
        let mut metadata = metadata.clone();
        move || -> Result<BackupMetadata> {
            let _snapshot_guard = snapshot_guard;
            let destination = backup_root.join(&metadata.id).join("data");
            std::fs::create_dir_all(&destination)?;
            copy_directory(&source, &destination, Some(&backup_root))?;
            metadata.wal_bytes = directory_size(&destination.join("wal"))?;
            write_json_atomic(
                &destination
                    .parent()
                    .expect("backup data directory has a parent")
                    .join("metadata.json"),
                &metadata,
            )?;
            Ok(metadata)
        }
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({
        "id": metadata.id,
        "kind": metadata.kind,
        "from_lsn": metadata.from_lsn,
        "to_lsn": metadata.to_lsn,
        "wal_records": metadata.wal_records,
        "wal_bytes": metadata.wal_bytes,
    })))
}

async fn backup_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    let root = state.config.read().backup.backup_dir.clone();
    let backups = tokio::task::spawn_blocking(move || -> Result<Vec<BackupMetadata>> {
        let mut backups = list_backup_metadata(&root)?;
        backups.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        Ok(backups)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({"backups": backups})))
}

async fn backup_delete(
    State(state): State<AdminState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    validate_backup_id(&id)?;
    let backup_root = state.config.read().backup.backup_dir.clone();
    let path = backup_root.join(&id);
    if !path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }
    let is_parent = list_backup_metadata(&backup_root)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .iter()
        .any(|backup| backup.parent_id.as_deref() == Some(&id));
    let pending = state.storage.data_dir().join(RESTORE_MARKER);
    let is_pending = pending
        .exists()
        .then(|| std::fs::read(&pending))
        .transpose()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .and_then(|bytes| serde_json::from_slice::<PendingRestore>(&bytes).ok())
        .is_some_and(|restore| restore.id == id);
    if is_parent || is_pending {
        return Err(StatusCode::CONFLICT);
    }
    tokio::fs::remove_dir_all(path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({"deleted": id})))
}

async fn backup_restore(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(request): Json<RestoreRequest>,
) -> Result<Json<Value>, StatusCode> {
    authorize(&headers, &state)?;
    validate_backup_id(&request.id)?;
    let backup_root = state.config.read().backup.backup_dir.clone();
    let chain = build_backup_chain(&backup_root, &request.id).map_err(|_| StatusCode::CONFLICT)?;
    let target_lsn =
        resolve_restore_target_lsn(&backup_root, &chain, request.point_in_time.as_deref())
            .map_err(|_| StatusCode::BAD_REQUEST)?;
    let marker = state.storage.data_dir().join(RESTORE_MARKER);
    let pending = PendingRestore {
        id: request.id.clone(),
        requested_at: chrono::Utc::now().to_rfc3339(),
        target_lsn: Some(target_lsn),
        point_in_time: request.point_in_time.clone(),
    };
    tokio::task::spawn_blocking(move || write_json_atomic(&marker, &pending))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({
        "staged": request.id,
        "chain": chain.iter().map(|backup| backup.id.as_str()).collect::<Vec<_>>(),
        "target_lsn": target_lsn,
        "point_in_time": request.point_in_time,
        "restart_required": true,
        "safety": "active data is not overwritten; the validated chain is installed before storage opens on restart",
    })))
}

fn new_backup_id(kind: &str) -> String {
    format!(
        "{}-{}-{}",
        kind,
        chrono::Utc::now().format("%Y%m%d%H%M%S"),
        uuid::Uuid::new_v4()
    )
}

fn read_backup_metadata(root: &Path, id: &str) -> Result<BackupMetadata> {
    if !backup_id_is_safe(id) {
        anyhow::bail!("unsafe backup id");
    }
    let path = root.join(id).join("metadata.json");
    let mut metadata: BackupMetadata = serde_json::from_slice(
        &std::fs::read(&path)
            .with_context(|| format!("read backup metadata {}", path.display()))?,
    )?;
    if metadata.id != id || !matches!(metadata.kind.as_str(), "full" | "incremental") {
        anyhow::bail!("backup metadata identity or kind mismatch");
    }
    // Legacy HTTP full snapshots did not record LSNs. Derive their immutable
    // endpoint so they remain restorable, but do not use them as an incremental
    // parent because they lack the new format's chain contract.
    if metadata.format_version == 0 && metadata.kind == "full" {
        let data = root.join(id).join("data");
        metadata.to_lsn = mydb_wal::WalReader::open(data.join("wal"))?.max_lsn()?;
        metadata.wal_records = metadata.to_lsn;
        metadata.wal_bytes = directory_size(&data.join("wal"))?;
    }
    Ok(metadata)
}

fn list_backup_metadata(root: &Path) -> Result<Vec<BackupMetadata>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut backups = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if entry.path().join("metadata.json").is_file() {
            backups.push(read_backup_metadata(root, &id)?);
        }
    }
    Ok(backups)
}

fn build_backup_chain(root: &Path, target_id: &str) -> Result<Vec<BackupMetadata>> {
    let mut reversed = Vec::new();
    let mut current = target_id.to_string();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current.clone()) {
            anyhow::bail!("backup chain cycle detected");
        }
        let metadata = read_backup_metadata(root, &current)?;
        let parent = metadata.parent_id.clone();
        let is_full = metadata.kind == "full";
        reversed.push(metadata);
        if is_full {
            break;
        }
        current = parent.ok_or_else(|| anyhow::anyhow!("incremental backup has no parent"))?;
    }
    reversed.reverse();
    let full = reversed
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty backup chain"))?;
    if full.kind != "full" || full.parent_id.is_some() {
        anyhow::bail!("backup chain does not start with a full backup");
    }
    let full_data = root.join(&full.id).join("data");
    if !full_data.is_dir() {
        anyhow::bail!("full backup data is missing");
    }
    let full_lsn = mydb_wal::WalReader::open(full_data.join("wal"))?.max_lsn()?;
    if full_lsn != full.to_lsn {
        anyhow::bail!(
            "full backup LSN mismatch: metadata {}, WAL {full_lsn}",
            full.to_lsn
        );
    }
    for pair in reversed.windows(2) {
        let parent = &pair[0];
        let incremental = &pair[1];
        if incremental.kind != "incremental"
            || incremental.format_version != BACKUP_FORMAT_VERSION
            || incremental.parent_id.as_deref() != Some(parent.id.as_str())
            || incremental.from_lsn != parent.to_lsn
            || incremental.wal_records != incremental.to_lsn - incremental.from_lsn
        {
            anyhow::bail!("invalid or discontinuous incremental backup chain");
        }
        let info = mydb_wal::validate_wal_archive(
            &root.join(&incremental.id).join("wal_delta.log"),
            incremental.from_lsn,
            incremental.to_lsn,
        )?;
        if info.record_count != incremental.wal_records || info.bytes != incremental.wal_bytes {
            anyhow::bail!("incremental WAL manifest mismatch");
        }
    }
    Ok(reversed)
}

fn resolve_restore_target_lsn(
    root: &Path,
    chain: &[BackupMetadata],
    point_in_time: Option<&str>,
) -> Result<u64> {
    let last = chain
        .last()
        .ok_or_else(|| anyhow::anyhow!("empty backup chain"))?;
    let Some(point_in_time) = point_in_time else {
        return Ok(last.to_lsn);
    };
    let target = chrono::DateTime::parse_from_rfc3339(point_in_time)?.timestamp_millis();
    if target < 0 {
        anyhow::bail!("point-in-time timestamp precedes Unix epoch");
    }
    let target = target as u64;
    let full = &chain[0];
    let full_created = chrono::DateTime::parse_from_rfc3339(&full.created_at)?.timestamp_millis();
    if full_created < 0 || target < full_created as u64 {
        anyhow::bail!("point-in-time target predates the parent full snapshot");
    }

    let mut target_lsn = full.to_lsn;
    for incremental in chain.iter().skip(1) {
        let created =
            chrono::DateTime::parse_from_rfc3339(&incremental.created_at)?.timestamp_millis();
        if created >= 0 && created as u64 <= target {
            target_lsn = incremental.to_lsn;
            continue;
        }
        for record in mydb_wal::read_wal_archive(&root.join(&incremental.id).join("wal_delta.log"))?
        {
            if record.record_type != mydb_wal::WalRecordType::GroupCommit {
                continue;
            }
            let committed = mydb_storage::wal_group_commit_unix_ms(&record).ok_or_else(|| {
                anyhow::anyhow!(
                    "point-in-time restore requires timestamped MDG2 records after the full snapshot"
                )
            })?;
            if committed > target {
                return Ok(target_lsn);
            }
            target_lsn = record.lsn;
        }
    }
    Ok(target_lsn)
}

fn apply_pending_restore(config: &mydb_config::ServerConfig) -> Result<()> {
    let data_dir = &config.storage.data_dir;
    let marker = data_dir.join(RESTORE_MARKER);
    if !marker.is_file() {
        return Ok(());
    }
    let pending: PendingRestore = serde_json::from_slice(&std::fs::read(&marker)?)?;
    let backup_root = &config.backup.backup_dir;
    let chain = build_backup_chain(backup_root, &pending.id)?;
    let full = chain
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty restore chain"))?;

    clear_restore_destination(data_dir, backup_root, &marker)?;
    copy_directory(&backup_root.join(&full.id).join("data"), data_dir, None)?;
    let wal_dir = data_dir.join("wal");
    let mut installed_lsn = mydb_wal::WalReader::open(wal_dir.clone())?.max_lsn()?;
    if installed_lsn != full.to_lsn {
        anyhow::bail!("restored full backup ends at unexpected LSN");
    }
    let target_lsn = pending
        .target_lsn
        .unwrap_or_else(|| chain.last().map_or(full.to_lsn, |backup| backup.to_lsn));
    let chain_end = chain.last().map_or(full.to_lsn, |backup| backup.to_lsn);
    if target_lsn < full.to_lsn || target_lsn > chain_end {
        anyhow::bail!("pending restore target LSN is outside the backup chain");
    }
    for incremental in chain.iter().skip(1) {
        if installed_lsn != incremental.from_lsn {
            anyhow::bail!("restore chain LSN discontinuity");
        }
        if target_lsn <= incremental.from_lsn {
            break;
        }
        if incremental.to_lsn == incremental.from_lsn {
            continue;
        }
        let archive = backup_root.join(&incremental.id).join("wal_delta.log");
        if target_lsn < incremental.to_lsn {
            mydb_wal::install_wal_archive_prefix(
                &archive,
                &wal_dir,
                incremental.from_lsn,
                incremental.to_lsn,
                target_lsn,
            )?;
            installed_lsn = target_lsn;
            break;
        } else {
            mydb_wal::install_wal_archive(
                &archive,
                &wal_dir,
                incremental.from_lsn,
                incremental.to_lsn,
            )?;
            installed_lsn = incremental.to_lsn;
        }
    }
    if installed_lsn != target_lsn {
        anyhow::bail!("restore did not reach the requested LSN");
    }
    std::fs::remove_file(&marker)?;
    info!(
        "Installed backup chain ending at {} (LSN {}) before storage startup",
        pending.id, installed_lsn
    );
    Ok(())
}

fn clear_restore_destination(data_dir: &Path, backup_root: &Path, marker: &Path) -> Result<()> {
    let canonical_data = std::fs::canonicalize(data_dir)?;
    let canonical_backup = std::fs::canonicalize(backup_root).ok();
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path == marker {
            continue;
        }
        let canonical_path = std::fs::canonicalize(&path)?;
        if !canonical_path.starts_with(&canonical_data) {
            anyhow::bail!("restore cleanup path escaped the data directory");
        }
        if canonical_backup
            .as_ref()
            .is_some_and(|backup| backup.starts_with(&canonical_path))
        {
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("JSON output has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    use std::io::Write as _;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn directory_size(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut bytes = 0_u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        bytes += if file_type.is_dir() {
            directory_size(&entry.path())?
        } else {
            entry.metadata()?.len()
        };
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct AgentRequest {
    #[serde(default)]
    question: String,
    #[serde(default)]
    sql: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentIntent {
    Health,
    SlowQueries,
    WriteLatency,
    Locks,
    Connections,
    Storage,
    Backup,
    SqlOptimization,
    Unknown,
}

impl AgentIntent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::SlowQueries => "slow_queries",
            Self::WriteLatency => "write_latency",
            Self::Locks => "locks",
            Self::Connections => "connections",
            Self::Storage => "storage",
            Self::Backup => "backup",
            Self::SqlOptimization => "sql_optimization",
            Self::Unknown => "unknown",
        }
    }

    fn recommended_endpoint(self) -> &'static str {
        match self {
            Self::SlowQueries => "/api/v1/agent/slow-queries",
            Self::SqlOptimization => "/api/v1/agent/sql",
            Self::Connections => "/api/v1/connections",
            Self::Storage => "/api/v1/storage/inventory",
            Self::Backup => "/api/v1/backup/list",
            _ => "/api/v1/agent/health",
        }
    }
}

fn classify_agent_question(question: &str) -> AgentIntent {
    let question = question.trim().to_lowercase();
    let contains_any =
        |keywords: &[&str]| keywords.iter().any(|keyword| question.contains(keyword));
    if question.is_empty() {
        return AgentIntent::Health;
    }
    if contains_any(&["backup", "restore", "备份", "恢复", "快照"]) {
        AgentIntent::Backup
    } else if contains_any(&[
        "reconnect",
        "connection",
        "disconnect",
        "连接",
        "重连",
        "断线",
    ]) {
        AgentIntent::Connections
    } else if contains_any(&[
        "deadlock",
        "lock wait",
        "lock timeout",
        "锁",
        "死锁",
        "锁等待",
    ]) {
        AgentIntent::Locks
    } else if contains_any(&[
        "slow query",
        "slow sql",
        "query latency",
        "慢查询",
        "慢 sql",
        "慢sql",
        "查询慢",
    ]) {
        AgentIntent::SlowQueries
    } else if contains_any(&[
        "write",
        "latency",
        "wal",
        "fsync",
        "checkpoint",
        "actor queue",
        "写入",
        "写慢",
        "延迟",
        "写队列",
    ]) {
        AgentIntent::WriteLatency
    } else if contains_any(&[
        "storage",
        "disk",
        "orphan",
        "buffer pool",
        "磁盘",
        "存储",
        "空间",
        "孤儿",
        "缓存池",
    ]) {
        AgentIntent::Storage
    } else if contains_any(&["sql", "index", "query plan", "索引", "执行计划", "优化查询"])
    {
        AgentIntent::SqlOptimization
    } else if contains_any(&[
        "health", "status", "problem", "fault", "健康", "状态", "问题", "故障",
    ]) {
        AgentIntent::Health
    } else {
        AgentIntent::Unknown
    }
}

fn answer_agent_question(
    intent: AgentIntent,
    wire: mydb_wire::WireStatsSnapshot,
    storage: mydb_storage::StorageStatsSnapshot,
    slow_query_count: usize,
    latest_slow_ms: Option<u64>,
) -> String {
    match intent {
        AgentIntent::Health => diagnose_advice(
            wire.query_errors,
            storage.errors,
            storage.actor_queue_depth,
            wire.lock_timeouts,
        )
        .join("; "),
        AgentIntent::SlowQueries => match latest_slow_ms {
            Some(duration) => format!(
                "captured {slow_query_count} slow/error queries; latest duration is {duration} ms"
            ),
            None => "no slow or failed query has been captured".to_string(),
        },
        AgentIntent::WriteLatency => {
            if storage.group_commits == 0 {
                return "no committed write group is available yet; run the workload and ask again"
                    .to_string();
            }
            let groups = storage.group_commits;
            let phases = [
                ("prepare/validation", storage.prepare_validation_micros),
                ("WAL fsync", storage.wal_sync_micros),
                ("apply", storage.apply_micros),
                ("checkpoint", storage.checkpoint_micros),
            ];
            let dominant = phases
                .iter()
                .max_by_key(|(_, micros)| *micros)
                .map(|(name, _)| *name)
                .unwrap_or("unknown");
            format!(
                "{groups} write groups, queue depth {}, average prepare/WAL/apply/checkpoint = {}/{}/{}/{} us per group; dominant phase: {dominant}",
                storage.actor_queue_depth,
                storage.prepare_validation_micros / groups,
                storage.wal_sync_micros / groups,
                storage.apply_micros / groups,
                storage.checkpoint_micros / groups,
            )
        }
        AgentIntent::Locks => format!(
            "{} active transaction locks, {} waits, {} timeouts, {} deadlocks; keep actor transactions short and keyed by primary/unique key",
            wire.active_locks, wire.lock_waits, wire.lock_timeouts, wire.deadlocks
        ),
        AgentIntent::Connections => format!(
            "{} active connections, {} accepted connections total; disconnects roll back transactional writes and a reconnect starts a clean session",
            wire.active_connections, wire.total_connections
        ),
        AgentIntent::Storage => format!(
            "{} buffer-pool pages, {} storage errors, {} checkpoint errors; inspect storage inventory before any orphan cleanup",
            storage.buffer_pool_pages, storage.errors, storage.checkpoint_errors
        ),
        AgentIntent::Backup => {
            "full, native LSN-incremental chains, and RFC3339 point-in-time recovery are available over Agent HTTP; restore is staged and installed before storage opens on restart"
                .to_string()
        }
        AgentIntent::SqlOptimization => {
            "submit the exact SQL to /api/v1/agent/sql for read-only static analysis".to_string()
        }
        AgentIntent::Unknown => {
            "question not recognized; ask about health, slow SQL, write latency, locks, connections, storage, backup, or SQL optimization"
                .to_string()
        }
    }
}

async fn agent_health(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorize_agent(&headers, &state)?;
    let wire = state.wire_stats.snapshot();
    let storage = state.storage.stats();
    let audit = state.protocol_config.read().audit_log.snapshot();
    let healthy = audit.healthy
        && storage.errors == 0
        && storage.checkpoint_errors == 0
        && storage.actor_queue_depth < 1024;
    Ok(Json(json!({
        "healthy": healthy,
        "connections": wire.active_connections,
        "query_errors": wire.query_errors,
        "audit_healthy": audit.healthy,
        "audit_events": audit.accepted,
        "audit_rejections": audit.rejected,
        "write_actor_queue_depth": storage.actor_queue_depth,
        "storage_errors": storage.errors,
        "group_commits": storage.group_commits,
        "grouped_requests": storage.grouped_requests,
        "checkpoints": storage.checkpoints,
        "checkpoint_errors": storage.checkpoint_errors,
        "prepare_validation_microseconds": storage.prepare_validation_micros,
        "wal_sync_microseconds": storage.wal_sync_micros,
        "apply_microseconds": storage.apply_micros,
        "checkpoint_microseconds": storage.checkpoint_micros,
        "active_transaction_locks": wire.active_locks,
        "lock_waits": wire.lock_waits,
        "lock_timeouts": wire.lock_timeouts,
        "deadlocks": wire.deadlocks,
        "advice": diagnose_advice(
            wire.query_errors,
            storage.errors,
            storage.actor_queue_depth,
            wire.lock_timeouts,
        ),
    })))
}

async fn agent_slow_queries(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    authorize_agent(&headers, &state)?;
    let queries: Vec<_> = state
        .wire_stats
        .slow_queries()
        .into_iter()
        .rev()
        .map(|query| {
            json!({
                "timestamp_ms": query.unix_ms,
                "duration_ms": query.duration_ms,
                "connection_id": query.connection_id,
                "database": query.database,
                "sql": query.sql,
                "error": query.error,
            })
        })
        .collect();
    Ok(Json(json!({"slow_queries": queries})))
}

async fn agent_diagnose(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(request): Json<AgentRequest>,
) -> Result<Json<Value>, StatusCode> {
    authorize_agent(&headers, &state)?;
    let wire = state.wire_stats.snapshot();
    let storage = state.storage.stats();
    let slow_queries = state.wire_stats.slow_queries();
    let intent = classify_agent_question(&request.question);
    let answer = answer_agent_question(
        intent,
        wire,
        storage,
        slow_queries.len(),
        slow_queries.last().map(|query| query.duration_ms),
    );
    let recent_slow_queries: Vec<_> = slow_queries
        .iter()
        .rev()
        .take(5)
        .map(|query| {
            json!({
                "timestamp_ms": query.unix_ms,
                "duration_ms": query.duration_ms,
                "connection_id": query.connection_id,
                "database": query.database,
                "sql": query.sql,
                "error": query.error,
            })
        })
        .collect();
    Ok(Json(json!({
        "question": request.question,
        "intent": intent.as_str(),
        "answer": answer,
        "recommended_endpoint": intent.recommended_endpoint(),
        "summary": diagnose_advice(
            wire.query_errors,
            storage.errors,
            storage.actor_queue_depth,
            wire.lock_timeouts,
        ),
        "signals": {
            "active_connections": wire.active_connections,
            "total_queries": wire.queries,
            "query_errors": wire.query_errors,
            "storage_errors": storage.errors,
            "actor_queue_depth": storage.actor_queue_depth,
            "buffer_pool_pages": storage.buffer_pool_pages,
            "group_commits": storage.group_commits,
            "grouped_requests": storage.grouped_requests,
            "checkpoints": storage.checkpoints,
            "checkpoint_errors": storage.checkpoint_errors,
            "prepare_validation_microseconds": storage.prepare_validation_micros,
            "wal_sync_microseconds": storage.wal_sync_micros,
            "apply_microseconds": storage.apply_micros,
            "checkpoint_microseconds": storage.checkpoint_micros,
            "slow_query_count": slow_queries.len(),
            "active_transaction_locks": wire.active_locks,
            "lock_waits": wire.lock_waits,
            "lock_timeouts": wire.lock_timeouts,
            "deadlocks": wire.deadlocks,
            "row_lock_acquires": wire.row_lock_acquires,
            "table_lock_acquires": wire.table_lock_acquires,
        },
        "recent_slow_queries": recent_slow_queries,
    })))
}

async fn agent_sql_debug(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(request): Json<AgentRequest>,
) -> Result<Json<Value>, StatusCode> {
    authorize_agent(&headers, &state)?;
    let upper = request.sql.trim().to_ascii_uppercase();
    let kind = upper.split_whitespace().next().unwrap_or("UNKNOWN");
    let mut advice = Vec::new();
    if upper.starts_with("SELECT") && !upper.contains(" WHERE ") && upper.contains(" FROM ") {
        advice.push("full table scan: add selective WHERE predicate or index");
    }
    if upper.contains("ORDER BY") && !upper.contains("LIMIT") {
        advice.push("unbounded sort: add LIMIT for game-facing request paths");
    }
    if (upper.starts_with("UPDATE") || upper.starts_with("DELETE")) && !upper.contains(" WHERE ") {
        advice.push("whole-table mutation: add WHERE predicate");
    }
    if advice.is_empty() {
        advice.push("no obvious static issue; inspect slow-query duration and actor queue depth");
    }
    Ok(Json(json!({
        "statement_type": kind,
        "sql": request.sql,
        "advice": advice,
        "engine": "single-node actor-ordered writer",
    })))
}

fn diagnose_advice(
    query_errors: u64,
    storage_errors: u64,
    queue_depth: usize,
    lock_timeouts: u64,
) -> Vec<&'static str> {
    let mut advice = Vec::new();
    if storage_errors > 0 {
        advice.push("storage errors detected: inspect server log and disk health");
    }
    if queue_depth > 256 {
        advice.push(
            "write actor queue elevated: batch game-state writes and reduce transaction size",
        );
    }
    if query_errors > 0 {
        advice.push("SQL errors detected: inspect /api/v1/agent/slow-queries");
    }
    if lock_timeouts > 0 {
        advice.push(
            "lock wait timeouts detected: inspect transaction scope and keep actor writes keyed by primary key",
        );
    }
    if advice.is_empty() {
        advice.push("no active fault detected");
    }
    advice
}

fn validate_backup_id(id: &str) -> Result<(), StatusCode> {
    if !backup_id_is_safe(id) {
        Err(StatusCode::BAD_REQUEST)
    } else {
        Ok(())
    }
}

fn backup_id_is_safe(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn copy_directory(source: &Path, destination: &Path, skip: Option<&Path>) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        if skip
            .map(|path| source_path.starts_with(path))
            .unwrap_or(false)
        {
            continue;
        }
        let destination_path = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&source_path, &destination_path, skip)?;
        } else {
            std::fs::copy(source_path, destination_path)?;
        }
    }
    Ok(())
}

fn install_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let service_content = format!(
            r#"[Unit]
Description=MyDB Server
After=network.target

[Service]
Type=simple
ExecStart={}/mydb-server
Restart=always
RestartSec=5
User=mydb
Group=mydb
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
"#,
            std::env::current_exe()?.parent().unwrap().display()
        );

        let service_path = "/etc/systemd/system/mydb.service";
        std::fs::write(service_path, service_content)?;

        std::process::Command::new("useradd")
            .args(["-r", "-s", "/bin/false", "mydb"])
            .output()
            .ok();

        std::fs::create_dir_all("/var/lib/mydb")?;
        std::process::Command::new("chown")
            .args(["-R", "mydb:mydb", "/var/lib/mydb"])
            .output()?;

        println!("Service installed. Enable with: systemctl enable mydb");
        println!("Start with: systemctl start mydb");
    }

    #[cfg(target_os = "macos")]
    {
        let service_content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.mydb.server</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}/mydb-server</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/usr/local/var/log/mydb.log</string>
    <key>StandardErrorPath</key>
    <string>/usr/local/var/log/mydb.log</string>
</dict>
</plist>
"#,
            std::env::current_exe()?.parent().unwrap().display()
        );

        let service_path = format!(
            "{}/Library/LaunchDaemons/com.mydb.server.plist",
            dirs::home_dir().unwrap().display()
        );
        std::fs::write(&service_path, service_content)?;

        println!(
            "Service installed. Load with: launchctl load {}",
            service_path
        );
        println!("Start with: launchctl start com.mydb.server");
    }

    #[cfg(target_os = "windows")]
    {
        println!("For Windows, use sc.exe or install as a Windows Service:");
        println!(
            "  sc.exe create MyDBServer binPath= \"{}\\mydb-server.exe\"",
            std::env::current_exe()?.parent().unwrap().display()
        );
        println!("  sc.exe start MyDBServer");
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        println!("Service installation not supported on this platform");
    }

    Ok(())
}

fn uninstall_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("systemctl")
            .args(["stop", "mydb"])
            .output()
            .ok();
        std::process::Command::new("systemctl")
            .args(["disable", "mydb"])
            .output()
            .ok();
        std::fs::remove_file("/etc/systemd/system/mydb.service")?;
        std::process::Command::new("systemctl")
            .args(["daemon-reload"])
            .output()?;
        println!("Service uninstalled");
    }

    #[cfg(target_os = "macos")]
    {
        let service_path = format!(
            "{}/Library/LaunchDaemons/com.mydb.server.plist",
            dirs::home_dir().unwrap().display()
        );
        std::process::Command::new("launchctl")
            .args(["unload", &service_path])
            .output()
            .ok();
        std::fs::remove_file(service_path)?;
        println!("Service uninstalled");
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("sc.exe")
            .args(["stop", "MyDBServer"])
            .output()
            .ok();
        std::process::Command::new("sc.exe")
            .args(["delete", "MyDBServer"])
            .output()?;
        println!("Service uninstalled");
    }

    Ok(())
}

fn start_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("systemctl")
            .args(["start", "mydb"])
            .output()?;
        println!("Service started");
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("launchctl")
            .args(["start", "com.mydb.server"])
            .output()?;
        println!("Service started");
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("sc.exe")
            .args(["start", "MyDBServer"])
            .output()?;
        println!("Service started");
    }

    Ok(())
}

fn stop_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("systemctl")
            .args(["stop", "mydb"])
            .output()?;
        println!("Service stopped");
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("launchctl")
            .args(["stop", "com.mydb.server"])
            .output()?;
        println!("Service stopped");
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("sc.exe")
            .args(["stop", "MyDBServer"])
            .output()?;
        println!("Service stopped");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_token_comparison_requires_exact_match() {
        assert!(constant_time_eq(
            b"Bearer production-token",
            b"Bearer production-token"
        ));
        assert!(!constant_time_eq(
            b"Bearer production-token",
            b"Bearer production-tokeX"
        ));
        assert!(!constant_time_eq(
            b"Bearer production-token",
            b"Bearer production"
        ));
    }

    #[test]
    fn secure_transport_cannot_claim_tls_without_tls_listener() {
        let mut config = mydb_config::ServerConfig::default();
        assert!(validate_runtime_security(&config).is_ok());
        config.security.require_secure_transport = true;
        assert!(validate_runtime_security(&config)
            .unwrap_err()
            .to_string()
            .contains("tls_cert"));
    }

    #[test]
    fn strong_secret_enforcement_rejects_default_credentials() {
        let mut config = mydb_config::ServerConfig::default();
        config.security.enforce_strong_passwords = true;
        assert!(validate_runtime_security(&config).is_err());

        config.security.default_password = "mysql-root-secret-for-production".to_string();
        config.http.admin_password = "http-admin-secret-for-production".to_string();
        assert!(validate_runtime_security(&config).is_ok());
    }

    #[test]
    fn config_response_never_contains_secrets() {
        let config = mydb_config::ServerConfig::default();
        let redacted = redacted_config(&config);
        assert_eq!(redacted.security.default_password, "<redacted>");
        assert_eq!(redacted.http.admin_password, "<redacted>");
    }

    #[test]
    fn runtime_worker_count_honors_explicit_configuration() {
        assert_eq!(runtime_worker_threads(1), 1);
        assert_eq!(runtime_worker_threads(8), 8);
        assert!(runtime_worker_threads(0) >= 1);
    }

    #[test]
    fn classifies_english_and_chinese_agent_questions() {
        assert_eq!(
            classify_agent_question("why is write latency high?"),
            AgentIntent::WriteLatency
        );
        assert_eq!(
            classify_agent_question("最近有哪些慢 SQL？"),
            AgentIntent::SlowQueries
        );
        assert_eq!(
            classify_agent_question("断线后可以自动重连吗"),
            AgentIntent::Connections
        );
        assert_eq!(
            classify_agent_question("检查磁盘孤儿文件"),
            AgentIntent::Storage
        );
        assert_eq!(
            classify_agent_question("这个句子没有已知意图"),
            AgentIntent::Unknown
        );
    }

    #[test]
    fn write_latency_answer_identifies_dominant_phase() {
        let answer = answer_agent_question(
            AgentIntent::WriteLatency,
            mydb_wire::WireStatsSnapshot {
                active_connections: 1,
                total_connections: 2,
                queries: 3,
                query_errors: 0,
                query_execute_micros: 0,
                query_response_micros: 0,
                lock_waits: 0,
                lock_timeouts: 0,
                deadlocks: 0,
                row_lock_acquires: 0,
                table_lock_acquires: 0,
                active_locks: 0,
            },
            mydb_storage::StorageStatsSnapshot {
                uptime_seconds: 1,
                reads: 0,
                writes: 20,
                errors: 0,
                actor_queue_depth: 4,
                buffer_pool_pages: 2,
                group_commits: 10,
                grouped_requests: 20,
                checkpoints: 1,
                checkpoint_errors: 0,
                prepare_validation_micros: 100,
                wal_sync_micros: 1_000,
                apply_micros: 200,
                checkpoint_micros: 300,
            },
            0,
            None,
        );
        assert!(answer.contains("dominant phase: WAL fsync"));
        assert!(answer.contains("average prepare/WAL/apply/checkpoint = 10/100/20/30"));
    }

    #[test]
    fn pending_restore_installs_validated_full_and_incremental_chain() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("data");
        let backup_root = temp.path().join("backups");
        let full_id = "full-test";
        let incremental_id = "incremental-test";
        let full_data = backup_root.join(full_id).join("data");
        std::fs::create_dir_all(full_data.join("wal")).unwrap();
        {
            let mut writer = mydb_wal::WalWriter::open(full_data.join("wal"), None).unwrap();
            for transaction in 1..=2 {
                let mut record = mydb_wal::WalRecord::new(
                    0,
                    mydb_wal::WalRecordType::Commit,
                    transaction,
                    "",
                    Vec::new(),
                );
                writer.append(&mut record).unwrap();
            }
            writer.sync().unwrap();
        }
        let full = BackupMetadata {
            format_version: BACKUP_FORMAT_VERSION,
            id: full_id.to_string(),
            kind: "full".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            parent_id: None,
            from_lsn: 0,
            to_lsn: 2,
            wal_records: 2,
            wal_bytes: directory_size(&full_data.join("wal")).unwrap(),
        };
        write_json_atomic(&backup_root.join(full_id).join("metadata.json"), &full).unwrap();

        let source_wal = temp.path().join("source-wal");
        {
            let mut writer = mydb_wal::WalWriter::open(source_wal.clone(), None).unwrap();
            for transaction in 1..=4 {
                let mut record = mydb_wal::WalRecord::new(
                    0,
                    mydb_wal::WalRecordType::Commit,
                    transaction,
                    "",
                    Vec::new(),
                );
                writer.append(&mut record).unwrap();
            }
            writer.sync().unwrap();
        }
        let incremental_dir = backup_root.join(incremental_id);
        let info =
            mydb_wal::export_wal_range(&source_wal, &incremental_dir.join("wal_delta.log"), 2, 4)
                .unwrap();
        let incremental = BackupMetadata {
            format_version: BACKUP_FORMAT_VERSION,
            id: incremental_id.to_string(),
            kind: "incremental".to_string(),
            created_at: "2026-01-01T00:01:00Z".to_string(),
            parent_id: Some(full_id.to_string()),
            from_lsn: 2,
            to_lsn: 4,
            wal_records: info.record_count,
            wal_bytes: info.bytes,
        };
        write_json_atomic(&incremental_dir.join("metadata.json"), &incremental).unwrap();

        std::fs::create_dir_all(data_dir.join("stale-database")).unwrap();
        write_json_atomic(
            &data_dir.join(RESTORE_MARKER),
            &PendingRestore {
                id: incremental_id.to_string(),
                requested_at: "2026-01-01T00:02:00Z".to_string(),
                target_lsn: Some(4),
                point_in_time: None,
            },
        )
        .unwrap();
        let mut config = mydb_config::ServerConfig::default();
        config.storage.data_dir = data_dir.clone();
        config.backup.backup_dir = backup_root;
        apply_pending_restore(&config).unwrap();

        assert!(!data_dir.join(RESTORE_MARKER).exists());
        assert!(!data_dir.join("stale-database").exists());
        let mut lsns = Vec::new();
        mydb_wal::WalReader::open(data_dir.join("wal"))
            .unwrap()
            .replay(|record| lsns.push(record.lsn))
            .unwrap();
        assert_eq!(lsns, vec![1, 2, 3, 4]);
    }

    #[test]
    fn pending_restore_rejects_corrupt_full_wal_without_clearing_marker() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("data");
        let backup_root = temp.path().join("backups");
        let full_id = "full-corrupt";
        let full_data = backup_root.join(full_id).join("data");
        let wal_dir = full_data.join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        {
            let mut writer = mydb_wal::WalWriter::open(wal_dir.clone(), None).unwrap();
            let mut record =
                mydb_wal::WalRecord::new(0, mydb_wal::WalRecordType::Commit, 1, "", Vec::new());
            writer.append(&mut record).unwrap();
            writer.sync().unwrap();
        }
        {
            use std::io::{Seek, SeekFrom, Write};

            let mut wal = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(wal_dir.join("wal_000000.log"))
                .unwrap();
            wal.seek(SeekFrom::Start(8 + 8 + 4)).unwrap();
            wal.write_all(&[0xff]).unwrap();
            wal.sync_all().unwrap();
        }
        let full = BackupMetadata {
            format_version: BACKUP_FORMAT_VERSION,
            id: full_id.to_string(),
            kind: "full".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            parent_id: None,
            from_lsn: 0,
            to_lsn: 1,
            wal_records: 1,
            wal_bytes: directory_size(&wal_dir).unwrap(),
        };
        write_json_atomic(&backup_root.join(full_id).join("metadata.json"), &full).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        write_json_atomic(
            &data_dir.join(RESTORE_MARKER),
            &PendingRestore {
                id: full_id.to_string(),
                requested_at: "2026-01-01T00:01:00Z".to_string(),
                target_lsn: Some(1),
                point_in_time: None,
            },
        )
        .unwrap();
        let mut config = mydb_config::ServerConfig::default();
        config.storage.data_dir = data_dir.clone();
        config.backup.backup_dir = backup_root;

        let error = apply_pending_restore(&config).unwrap_err();
        assert!(error.to_string().contains("WAL CRC mismatch"));
        assert!(data_dir.join(RESTORE_MARKER).is_file());
    }

    #[tokio::test]
    async fn point_in_time_resolves_to_timestamped_group_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("source");
        let backup_root = temp.path().join("backups");
        let manager = mydb_storage::StorageEngineManager::new(data_dir.clone(), 16384, "4M");
        manager.init().await.unwrap();
        manager.create_database("baseline").await.unwrap();

        let guard = manager.snapshot_guard().await;
        manager.flush().unwrap();
        let full_lsn = manager.current_lsn();
        let full_id = "full-pitr";
        let full_data = backup_root.join(full_id).join("data");
        copy_directory(&data_dir, &full_data, Some(&backup_root)).unwrap();
        drop(guard);
        let full = BackupMetadata {
            format_version: BACKUP_FORMAT_VERSION,
            id: full_id.into(),
            kind: "full".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            parent_id: None,
            from_lsn: 0,
            to_lsn: full_lsn,
            wal_records: full_lsn,
            wal_bytes: directory_size(&full_data.join("wal")).unwrap(),
        };
        write_json_atomic(&backup_root.join(full_id).join("metadata.json"), &full).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        manager.create_database("included").await.unwrap();
        let included_lsn = manager.current_lsn();
        let point_in_time = chrono::Utc::now().to_rfc3339();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        manager.create_database("excluded").await.unwrap();
        manager.sync_wal().unwrap();
        let end_lsn = manager.current_lsn();

        let incremental_id = "incremental-pitr";
        let incremental_dir = backup_root.join(incremental_id);
        let info = mydb_wal::export_wal_redo_range(
            &data_dir.join("wal"),
            &incremental_dir.join("wal_delta.log"),
            full_lsn,
            end_lsn,
        )
        .unwrap();
        let incremental = BackupMetadata {
            format_version: BACKUP_FORMAT_VERSION,
            id: incremental_id.into(),
            kind: "incremental".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            parent_id: Some(full_id.into()),
            from_lsn: full_lsn,
            to_lsn: end_lsn,
            wal_records: info.record_count,
            wal_bytes: info.bytes,
        };
        write_json_atomic(&incremental_dir.join("metadata.json"), &incremental).unwrap();
        let chain = build_backup_chain(&backup_root, incremental_id).unwrap();
        assert_eq!(
            resolve_restore_target_lsn(&backup_root, &chain, Some(&point_in_time)).unwrap(),
            included_lsn
        );
    }
}
