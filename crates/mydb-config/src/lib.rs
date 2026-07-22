use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ServerConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub http: HttpSection,
    #[serde(default)]
    pub storage: StorageSection,
    #[serde(default)]
    pub memory: MemorySection,
    #[serde(default)]
    pub security: SecuritySection,
    #[serde(default)]
    pub backup: BackupSection,
    #[serde(default)]
    pub logging: LoggingSection,
    #[serde(default)]
    pub character_set: CharacterSetSection,
    #[serde(default)]
    pub agent: AgentSection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSection {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default)]
    pub thread_count: u32,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,
    #[serde(default = "default_interactive_timeout")]
    pub interactive_timeout: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpSection {
    #[serde(default = "default_http_port")]
    pub port: u16,
    #[serde(default = "default_http_host")]
    pub host: String,
    #[serde(default = "default_admin_username")]
    pub admin_username: String,
    #[serde(default = "default_admin_password")]
    pub admin_password: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSection {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_engine")]
    pub engine: String,
    #[serde(default = "default_buffer_pool_size")]
    pub buffer_pool_size: String,
    #[serde(default = "default_log_file_size")]
    pub log_file_size: String,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Maximum time a write actor waits for more requests before one WAL fsync.
    #[serde(default = "default_group_commit_window_us")]
    pub group_commit_window_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySection {
    #[serde(default = "default_max_memory")]
    pub max_memory: String,
    #[serde(default = "default_query_cache_size")]
    pub query_cache_size: String,
    #[serde(default = "default_sort_buffer_size")]
    pub sort_buffer_size: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecuritySection {
    #[serde(default = "default_root_username")]
    pub default_username: String,
    #[serde(default = "default_root_password")]
    pub default_password: String,
    #[serde(default = "default_authentication")]
    pub authentication: String,
    #[serde(default)]
    pub require_secure_transport: bool,
    #[serde(default)]
    pub tls_cert: Option<PathBuf>,
    #[serde(default)]
    pub tls_key: Option<PathBuf>,
    #[serde(default)]
    pub password_lifetime: u32,
    #[serde(default)]
    pub enforce_strong_passwords: bool,
    #[serde(default = "default_true")]
    pub local_infile: bool,
    #[serde(default)]
    pub secure_file_priv: Option<PathBuf>,
    #[serde(default = "default_max_load_data_size")]
    pub max_load_data_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupSection {
    #[serde(default = "default_backup_dir")]
    pub backup_dir: PathBuf,
    #[serde(default)]
    pub auto_backup_interval: u32,
    #[serde(default = "default_max_backups")]
    pub max_backups: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggingSection {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default = "default_max_size")]
    pub max_size: String,
    #[serde(default = "default_max_files")]
    pub max_files: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CharacterSetSection {
    #[serde(default = "default_charset")]
    pub server: String,
    #[serde(default = "default_charset")]
    pub connection: String,
    #[serde(default = "default_charset")]
    pub results: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_slow_query_threshold_ms")]
    pub slow_query_threshold_ms: u64,
    #[serde(default = "default_max_slow_queries")]
    pub max_slow_queries: usize,
}

// Default values
fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    3306
}
fn default_max_connections() -> u32 {
    1000
}
fn default_connect_timeout() -> u64 {
    10
}
fn default_interactive_timeout() -> u64 {
    28800
}
fn default_http_port() -> u16 {
    4306
}
fn default_http_host() -> String {
    "127.0.0.1".to_string()
}
fn default_admin_username() -> String {
    "root".to_string()
}
fn default_admin_password() -> String {
    "root".to_string()
}
fn default_true() -> bool {
    true
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/mydb")
}
fn default_engine() -> String {
    "neko233".to_string()
}
fn default_buffer_pool_size() -> String {
    "1G".to_string()
}
fn default_log_file_size() -> String {
    "256M".to_string()
}
fn default_page_size() -> u32 {
    16384
}

fn default_group_commit_window_us() -> u64 {
    0
}
fn default_max_memory() -> String {
    "1G".to_string()
}
fn default_query_cache_size() -> String {
    "64M".to_string()
}
fn default_sort_buffer_size() -> String {
    "4M".to_string()
}
fn default_authentication() -> String {
    "mysql_native_password".to_string()
}
fn default_root_username() -> String {
    "root".to_string()
}
fn default_root_password() -> String {
    "root".to_string()
}
fn default_backup_dir() -> PathBuf {
    PathBuf::from("/var/lib/mydb/backups")
}
fn default_max_backups() -> u32 {
    10
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_max_size() -> String {
    "100M".to_string()
}
fn default_max_files() -> u32 {
    10
}
fn default_charset() -> String {
    "utf8mb4".to_string()
}
fn default_slow_query_threshold_ms() -> u64 {
    100
}
fn default_max_slow_queries() -> usize {
    1024
}
fn default_max_load_data_size() -> usize {
    1024 * 1024 * 1024
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            max_connections: default_max_connections(),
            thread_count: 0,
            connect_timeout: default_connect_timeout(),
            interactive_timeout: default_interactive_timeout(),
        }
    }
}

impl Default for HttpSection {
    fn default() -> Self {
        Self {
            port: default_http_port(),
            host: default_http_host(),
            admin_username: default_admin_username(),
            admin_password: default_admin_password(),
            enabled: default_true(),
        }
    }
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            engine: default_engine(),
            buffer_pool_size: default_buffer_pool_size(),
            log_file_size: default_log_file_size(),
            page_size: default_page_size(),
            group_commit_window_us: default_group_commit_window_us(),
        }
    }
}

impl Default for MemorySection {
    fn default() -> Self {
        Self {
            max_memory: default_max_memory(),
            query_cache_size: default_query_cache_size(),
            sort_buffer_size: default_sort_buffer_size(),
        }
    }
}

impl Default for SecuritySection {
    fn default() -> Self {
        Self {
            default_username: default_root_username(),
            default_password: default_root_password(),
            authentication: default_authentication(),
            require_secure_transport: false,
            tls_cert: None,
            tls_key: None,
            password_lifetime: 0,
            enforce_strong_passwords: false,
            local_infile: true,
            secure_file_priv: None,
            max_load_data_size: default_max_load_data_size(),
        }
    }
}

impl Default for BackupSection {
    fn default() -> Self {
        Self {
            backup_dir: default_backup_dir(),
            auto_backup_interval: 0,
            max_backups: default_max_backups(),
        }
    }
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: None,
            max_size: default_max_size(),
            max_files: default_max_files(),
        }
    }
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            enabled: true,
            slow_query_threshold_ms: default_slow_query_threshold_ms(),
            max_slow_queries: default_max_slow_queries(),
        }
    }
}

impl ServerConfig {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::default_config_path()?,
        };

        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            let mut config: ServerConfig = serde_yaml::from_str(&content)?;
            config.apply_env()?;
            Ok(config)
        } else {
            // Return default config
            let mut config = Self::default();
            config.apply_env()?;
            Ok(config)
        }
    }

    fn apply_env(&mut self) -> Result<()> {
        if let Ok(value) = std::env::var("MYDB_PORT") {
            self.server.port = value.parse()?;
        }
        if let Ok(value) = std::env::var("MYDB_THREAD_COUNT") {
            self.server.thread_count = value.parse()?;
        }
        if let Ok(value) = std::env::var("MYDB_HTTP_PORT") {
            self.http.port = value.parse()?;
        }
        if let Ok(value) = std::env::var("MYDB_DATA_DIR") {
            self.storage.data_dir = value.into();
        }
        if let Ok(value) = std::env::var("MYDB_GROUP_COMMIT_WINDOW_US") {
            self.storage.group_commit_window_us = value.parse()?;
        }
        if let Some(value) = secret_from_env("MYDB_ROOT_PASSWORD")? {
            self.security.default_password = value;
        }
        if let Some(value) = secret_from_env("MYDB_ADMIN_PASSWORD")? {
            self.http.admin_password = value;
        }
        if let Ok(value) = std::env::var("MYDB_LOG_LEVEL") {
            self.logging.level = value;
        }
        if let Ok(value) = std::env::var("MYDB_LOCAL_INFILE") {
            self.security.local_infile = matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            );
        }
        if let Ok(value) = std::env::var("MYDB_SECURE_FILE_PRIV") {
            self.security.secure_file_priv = Some(value.into());
        }
        if let Ok(value) = std::env::var("MYDB_MAX_LOAD_DATA_SIZE") {
            self.security.max_load_data_size = value.parse()?;
        }
        if let Ok(value) = std::env::var("MYDB_ENFORCE_STRONG_PASSWORDS") {
            self.security.enforce_strong_passwords = matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            );
        }
        Ok(())
    }

    fn default_config_path() -> Result<PathBuf> {
        #[cfg(target_os = "linux")]
        let path = PathBuf::from("/etc/mydb/config.yaml");

        #[cfg(target_os = "macos")]
        let path = PathBuf::from("/usr/local/etc/mydb/config.yaml");

        #[cfg(target_os = "windows")]
        let path = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("mydb")
            .join("config.yaml");

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        let path = PathBuf::from("config.yaml");

        Ok(path)
    }
}

fn secret_from_env(name: &str) -> Result<Option<String>> {
    let value = std::env::var(name).ok();
    let file_name = format!("{name}_FILE");
    let file = std::env::var(&file_name).ok();
    secret_from_value_or_file(name, value, file)
}

fn secret_from_value_or_file(
    name: &str,
    value: Option<String>,
    file: Option<String>,
) -> Result<Option<String>> {
    match (value, file) {
        (Some(_), Some(_)) => anyhow::bail!("set either {name} or {name}_FILE, not both"),
        (Some(value), None) if value.is_empty() => anyhow::bail!("{name} must not be empty"),
        (Some(value), None) => Ok(Some(value)),
        (None, Some(path)) => {
            let value = std::fs::read_to_string(&path)
                .map_err(|error| anyhow::anyhow!("read {name}_FILE {}: {error}", path))?;
            // Docker and Kubernetes secret files conventionally carry one terminal line ending.
            let value = value.strip_suffix('\n').unwrap_or(&value);
            let value = value.strip_suffix('\r').unwrap_or(value).to_owned();
            if value.is_empty() {
                anyhow::bail!("{name}_FILE must not contain an empty secret");
            }
            Ok(Some(value))
        }
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_file_strips_one_terminal_newline_only() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "correct horse battery staple\n").unwrap();
        let value = secret_from_value_or_file(
            "MYDB_ROOT_PASSWORD",
            None,
            Some(file.path().display().to_string()),
        )
        .unwrap();
        assert_eq!(value.as_deref(), Some("correct horse battery staple"));
    }

    #[test]
    fn value_and_file_cannot_both_define_one_secret() {
        assert!(secret_from_value_or_file(
            "MYDB_ROOT_PASSWORD",
            Some("one".to_string()),
            Some("two".to_string()),
        )
        .is_err());
    }

    #[test]
    fn production_template_is_valid_server_config() {
        let config: ServerConfig =
            serde_yaml::from_str(include_str!("../../../configs/production.yaml")).unwrap();
        assert!(config.security.enforce_strong_passwords);
        assert_eq!(config.character_set.server, "utf8mb4");
        assert!(!config.security.local_infile);
    }
}
