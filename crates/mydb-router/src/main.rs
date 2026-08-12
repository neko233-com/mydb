use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{info, warn};

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
        let candidates = weighted_candidates(&self.config.backends);
        let start = self.next_backend.fetch_add(1, Ordering::Relaxed) % candidates.len();
        let timeout = Duration::from_millis(self.config.connect_timeout_ms);
        let mut last_error = None;
        for offset in 0..candidates.len() {
            let backend = candidates[(start + offset) % candidates.len()].clone();
            let address = format!("{}:{}", backend.host, backend.port);
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

fn weighted_candidates(backends: &[BackendConfig]) -> Vec<BackendConfig> {
    backends
        .iter()
        .flat_map(|backend| std::iter::repeat_n(backend.clone(), backend.weight))
        .collect()
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

    loop {
        let (mut client, peer) = listener.accept().await.context("accept client")?;
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
                    backend = %format!("{}:{}", target.host, target.port),
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
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn weighted_candidates_preserve_backend_weight() {
        let candidates = weighted_candidates(&[
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
        ]);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].host, "a");
        assert_eq!(candidates[1].host, "a");
        assert_eq!(candidates[2].host, "b");
    }
}
