use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
#[cfg(target_os = "windows")]
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tracing::{info, warn};

#[cfg(target_os = "windows")]
use std::sync::OnceLock;

#[cfg(target_os = "windows")]
static WINDOWS_SHUTDOWN: OnceLock<Arc<Notify>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    #[serde(default = "default_listen_host")]
    pub listen_host: String,
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    pub backends: Vec<BackendConfig>,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    #[serde(default = "default_true")]
    pub tcp_nodelay: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackendConfig {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_weight")]
    pub weight: usize,
}

impl RouterConfig {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("read router config {}", path.display()))?;
        let config: Self = serde_yaml::from_str(&content)
            .with_context(|| format!("parse router config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.listen_port == 0 {
            anyhow::bail!("router listen_port must be non-zero");
        }
        if self.backends.is_empty() {
            anyhow::bail!("router requires at least one backend");
        }
        if self.max_connections == 0 {
            anyhow::bail!("router max_connections must be non-zero");
        }
        if self.connect_timeout_ms == 0 {
            anyhow::bail!("router connect_timeout_ms must be non-zero");
        }
        for backend in &self.backends {
            if backend.host.trim().is_empty() || backend.port == 0 {
                anyhow::bail!("router backend must have a host and non-zero port");
            }
            if backend.weight == 0 {
                anyhow::bail!("router backend weight must be non-zero");
            }
        }
        total_backend_weight(&self.backends)?;
        Ok(())
    }
}

struct RouterState {
    config: RouterConfig,
    next_backend: AtomicUsize,
    connections: Arc<Semaphore>,
}

impl RouterState {
    fn new(config: RouterConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            connections: Arc::new(Semaphore::new(config.max_connections)),
            config,
            next_backend: AtomicUsize::new(0),
        })
    }

    async fn connect_backend(&self) -> Result<(TcpStream, BackendConfig)> {
        let total_weight = total_backend_weight(&self.config.backends)?;
        let slot = self.next_backend.fetch_add(1, Ordering::Relaxed) % total_weight;
        let start = weighted_backend_index(&self.config.backends, slot);
        let timeout = Duration::from_millis(self.config.connect_timeout_ms);
        let mut last_error = None;
        for offset in 0..self.config.backends.len() {
            let backend =
                self.config.backends[(start + offset) % self.config.backends.len()].clone();
            let address = backend_address(&backend);
            match tokio::time::timeout(timeout, TcpStream::connect(&address)).await {
                Ok(Ok(stream)) => return Ok((stream, backend)),
                Ok(Err(error)) => last_error = Some(error.to_string()),
                Err(_) => {
                    last_error = Some(format!("connect timeout after {} ms", timeout.as_millis()))
                }
            }
        }
        anyhow::bail!(
            "all MySQL backends unavailable: {}",
            last_error.unwrap_or_default()
        )
    }
}

fn backend_address(backend: &BackendConfig) -> String {
    if backend.host.contains(':') && !backend.host.starts_with('[') {
        format!("[{}]:{}", backend.host, backend.port)
    } else {
        format!("{}:{}", backend.host, backend.port)
    }
}

fn weighted_backend_index(backends: &[BackendConfig], slot: usize) -> usize {
    let mut remaining = slot;
    for (index, backend) in backends.iter().enumerate() {
        if remaining < backend.weight {
            return index;
        }
        remaining -= backend.weight;
    }
    backends.len().saturating_sub(1)
}

fn total_backend_weight(backends: &[BackendConfig]) -> Result<usize> {
    backends.iter().try_fold(0usize, |total, backend| {
        total
            .checked_add(backend.weight)
            .ok_or_else(|| anyhow::anyhow!("router backend weights overflow"))
    })
}

async fn serve(config: RouterConfig) -> Result<()> {
    let state = Arc::new(RouterState::new(config)?);
    let address = format!("{}:{}", state.config.listen_host, state.config.listen_port);
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("bind MySQL router at {address}"))?;
    info!(
        %address,
        backend_count = state.config.backends.len(),
        "MySQL wire router ready"
    );

    serve_listener(listener, state, std::future::pending()).await
}

async fn serve_listener<F>(
    listener: TcpListener,
    state: Arc<RouterState>,
    shutdown: F,
) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send,
{
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (mut client, peer) = accepted.context("accept client")?;
                if state.config.tcp_nodelay {
                    client.set_nodelay(true).ok();
                }
                let state = state.clone();
                let permit = match state.connections.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        warn!(%peer, "MySQL router connection limit reached");
                        continue;
                    }
                };
                tokio::spawn(async move {
                    let _permit = permit;
                    let result = async {
                        let (mut backend, target) = state.connect_backend().await?;
                        if state.config.tcp_nodelay {
                            backend.set_nodelay(true).ok();
                        }
                        info!(
                            %peer,
                            backend = %backend_address(&target),
                            "MySQL client routed"
                        );
                        copy_bidirectional(&mut client, &mut backend)
                            .await
                            .context("proxy MySQL connection")?;
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;
                    if let Err(error) = result {
                        warn!(%peer, error = %error, "MySQL router connection ended with error");
                    }
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Parser)]
#[command(name = "mydb-router", about = "Transparent MySQL 8.x wire router")]
struct Args {
    #[arg(short, long, default_value = "configs/router.yaml")]
    config: PathBuf,
    #[arg(long)]
    listen_host: Option<String>,
    #[arg(long)]
    listen_port: Option<u16>,
    #[arg(long = "backend", value_name = "HOST:PORT")]
    backends: Vec<String>,

    /// Install, uninstall, start, stop, or run as the MyDBRouter service.
    #[arg(long, hide = true)]
    service: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    #[cfg(target_os = "windows")]
    if args.service.as_deref() == Some("run") {
        return run_windows_service();
    }

    if let Some(command) = args.service.as_deref() {
        match command {
            "install" => install_service(Some(args.config.as_path()))?,
            "uninstall" => uninstall_service()?,
            "start" => start_service()?,
            "stop" => stop_service()?,
            "run" => unreachable!(),
            other => anyhow::bail!(
                "unknown service command '{other}', expected install, uninstall, start, or stop"
            ),
        }
        return Ok(());
    }

    let mut config = RouterConfig::load(&args.config)?;
    if let Some(host) = args.listen_host {
        config.listen_host = host;
    }
    if let Some(port) = args.listen_port {
        config.listen_port = port;
    }
    if !args.backends.is_empty() {
        config.backends = args
            .backends
            .iter()
            .map(|value| parse_backend(value))
            .collect::<Result<Vec<_>>>()?;
    }
    config.validate()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(config))
}

fn parse_backend(value: &str) -> Result<BackendConfig> {
    let (host, port) = value
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("backend must be HOST:PORT"))?;
    Ok(BackendConfig {
        host: host.trim_matches(['[', ']']).to_string(),
        port: port.parse().context("parse backend port")?,
        weight: 1,
    })
}

fn default_listen_host() -> String {
    "0.0.0.0".to_string()
}
fn default_listen_port() -> u16 {
    13306
}
fn default_connect_timeout_ms() -> u64 {
    2_000
}
fn default_max_connections() -> usize {
    4_096
}
fn default_true() -> bool {
    true
}
fn default_weight() -> usize {
    1
}

#[cfg(target_os = "windows")]
windows_service::define_windows_service!(ffi_service_main, windows_service_main);

#[cfg(target_os = "windows")]
fn run_windows_service() -> Result<()> {
    let _ = WINDOWS_SHUTDOWN.set(Arc::new(Notify::new()));
    windows_service::service_dispatcher::start("MyDBRouter", ffi_service_main)
        .map_err(|error| anyhow::anyhow!("Windows service dispatcher failed: {error}"))
}

#[cfg(target_os = "windows")]
fn windows_service_main(_arguments: Vec<std::ffi::OsString>) {
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    let result = (|| -> Result<()> {
        let shutdown = WINDOWS_SHUTDOWN
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Windows router shutdown channel is missing"))?;
        let event_handler = {
            let shutdown = shutdown.clone();
            move |event| -> ServiceControlHandlerResult {
                match event {
                    ServiceControl::Stop | ServiceControl::Shutdown => {
                        shutdown.notify_waiters();
                        ServiceControlHandlerResult::NoError
                    }
                    ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                    _ => ServiceControlHandlerResult::NotImplemented,
                }
            }
        };
        let status_handle = service_control_handler::register("MyDBRouter", event_handler)?;
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        let args = Args::try_parse_from(std::env::args_os())?;
        let mut config = RouterConfig::load(&args.config)?;
        if let Some(host) = args.listen_host {
            config.listen_host = host;
        }
        if let Some(port) = args.listen_port {
            config.listen_port = port;
        }
        if !args.backends.is_empty() {
            config.backends = args
                .backends
                .iter()
                .map(|value| parse_backend(value))
                .collect::<Result<Vec<_>>>()?;
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(serve_until_shutdown(config, shutdown.notified()));
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: if result.is_ok() {
                ServiceExitCode::Win32(0)
            } else {
                ServiceExitCode::Win32(1)
            },
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;
        result
    })();

    if let Err(error) = result {
        tracing::error!("MyDB router Windows service stopped: {error}");
    }
}

async fn serve_until_shutdown<F>(config: RouterConfig, shutdown: F) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send,
{
    let state = Arc::new(RouterState::new(config)?);
    let address = format!("{}:{}", state.config.listen_host, state.config.listen_port);
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("bind MySQL router at {address}"))?;
    info!(%address, backend_count = state.config.backends.len(), "MySQL wire router ready");
    serve_listener(listener, state, shutdown).await
}

fn install_service(config: Option<&std::path::Path>) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let executable = std::env::current_exe()?;
        let config = config
            .map(std::path::Path::to_path_buf)
            .unwrap_or(std::env::current_dir()?.join("router.yaml"));
        let bin_path = format!(
            "\"{}\" --service run --config \"{}\"",
            executable.display(),
            config.display()
        );
        run_service_command([
            "create",
            "MyDBRouter",
            &format!("binPath= {bin_path}"),
            "start= auto",
            "DisplayName= MyDB Router",
        ])?;
        println!("MyDBRouter service installed");
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = config;
        anyhow::bail!("router service installation is currently supported on Windows only")
    }
}

fn uninstall_service() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let _ = run_service_command(["stop", "MyDBRouter"]);
        run_service_command(["delete", "MyDBRouter"])?;
        println!("MyDBRouter service uninstalled");
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    anyhow::bail!("router service management is currently supported on Windows only")
}

fn start_service() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        run_service_command(["start", "MyDBRouter"])?;
        println!("MyDBRouter service started");
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    anyhow::bail!("router service management is currently supported on Windows only")
}

fn stop_service() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        run_service_command(["stop", "MyDBRouter"])?;
        println!("MyDBRouter service stopped");
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    anyhow::bail!("router service management is currently supported on Windows only")
}

#[cfg(target_os = "windows")]
fn run_service_command<const N: usize>(arguments: [&str; N]) -> Result<()> {
    let status = std::process::Command::new("sc.exe")
        .args(arguments)
        .status()
        .context("run sc.exe")?;
    if !status.success() {
        anyhow::bail!("sc.exe failed with status {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    #[test]
    fn config_requires_a_backend() {
        let config = RouterConfig {
            listen_host: "0.0.0.0".to_string(),
            listen_port: 13306,
            backends: Vec::new(),
            connect_timeout_ms: 100,
            max_connections: 1,
            tcp_nodelay: true,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn weighted_backend_index_preserves_backend_weight() {
        let backends = [
            BackendConfig {
                host: "a".to_string(),
                port: 3306,
                weight: 2,
            },
            BackendConfig {
                host: "b".to_string(),
                port: 3306,
                weight: 1,
            },
        ];
        assert_eq!(weighted_backend_index(&backends, 0), 0);
        assert_eq!(weighted_backend_index(&backends, 1), 0);
        assert_eq!(weighted_backend_index(&backends, 2), 1);
    }

    #[test]
    fn ipv6_backend_address_is_bracketed() {
        let backend = BackendConfig {
            host: "::1".to_string(),
            port: 3306,
            weight: 1,
        };
        assert_eq!(backend_address(&backend), "[::1]:3306");
    }

    #[tokio::test]
    async fn router_proxies_bytes_and_falls_back_to_next_backend() {
        let backend_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("backend bind");
        let backend_port = backend_listener
            .local_addr()
            .expect("backend address")
            .port();
        let backend_task = tokio::spawn(async move {
            let (mut socket, _) = backend_listener.accept().await.expect("backend accept");
            let mut payload = [0u8; 4];
            socket.read_exact(&mut payload).await.expect("backend read");
            socket.write_all(&payload).await.expect("backend write");
        });

        let config = RouterConfig {
            listen_host: "127.0.0.1".to_string(),
            listen_port: 13306,
            backends: vec![
                BackendConfig {
                    host: "127.0.0.1".to_string(),
                    port: 1,
                    weight: 1,
                },
                BackendConfig {
                    host: "127.0.0.1".to_string(),
                    port: backend_port,
                    weight: 1,
                },
            ],
            connect_timeout_ms: 100,
            max_connections: 4,
            tcp_nodelay: true,
        };
        let state = Arc::new(RouterState::new(config).expect("router config"));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("router bind");
        let router_address = listener.local_addr().expect("router address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let router_task = tokio::spawn(serve_listener(listener, state, async move {
            let _ = shutdown_rx.await;
        }));

        let mut client = TcpStream::connect(router_address)
            .await
            .expect("router connect");
        client.write_all(b"ping").await.expect("client write");
        let mut response = [0u8; 4];
        client.read_exact(&mut response).await.expect("client read");
        assert_eq!(&response, b"ping");

        let _ = shutdown_tx.send(());
        router_task
            .await
            .expect("router task")
            .expect("router exit");
        backend_task.await.expect("backend task");
    }
}
