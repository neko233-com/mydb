use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSection,
    pub storage: StorageSection,
    pub security: SecuritySection,
    pub logging: LoggingSection,
    #[serde(default)]
    pub character_set: CharacterSetSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecuritySection {
    #[serde(default = "default_authentication")]
    pub authentication: String,
    #[serde(default)]
    pub require_secure_transport: bool,
    #[serde(default)]
    pub password_lifetime: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CharacterSetSection {
    #[serde(default = "default_charset")]
    pub server: String,
    #[serde(default = "default_charset")]
    pub connection: String,
    #[serde(default = "default_charset")]
    pub results: String,
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
fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/mydb")
}
fn default_engine() -> String {
    "innodb".to_string()
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
fn default_authentication() -> String {
    "mysql_native_password".to_string()
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

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            engine: default_engine(),
            buffer_pool_size: default_buffer_pool_size(),
            log_file_size: default_log_file_size(),
            page_size: default_page_size(),
        }
    }
}

impl Default for SecuritySection {
    fn default() -> Self {
        Self {
            authentication: default_authentication(),
            require_secure_transport: false,
            password_lifetime: 0,
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

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSection::default(),
            storage: StorageSection::default(),
            security: SecuritySection::default(),
            logging: LoggingSection::default(),
            character_set: CharacterSetSection::default(),
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
            let config: ServerConfig = serde_yaml::from_str(&content)?;
            Ok(config)
        } else {
            // Return default config
            Ok(Self::default())
        }
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
