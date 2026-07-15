use std::path::PathBuf;

use anyhow::Result;
use mydb_config::ServerConfig;

#[tokio::test]
async fn test_default_config() {
    let config = ServerConfig::default();
    
    assert_eq!(config.server.host, "0.0.0.0");
    assert_eq!(config.server.port, 3306);
    assert_eq!(config.server.max_connections, 1000);
    assert_eq!(config.storage.engine, "innodb");
    assert_eq!(config.storage.buffer_pool_size, "1G");
    assert_eq!(config.logging.level, "info");
}

#[tokio::test]
async fn test_config_yaml_parse() {
    let yaml = r#"
server:
  host: "127.0.0.1"
  port: 3307
  max_connections: 500

storage:
  data_dir: "/tmp/mydb"
  engine: "innodb"
  buffer_pool_size: "512M"

logging:
  level: "debug"
"#;
    
    let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
    
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 3307);
    assert_eq!(config.server.max_connections, 500);
    assert_eq!(config.storage.data_dir, PathBuf::from("/tmp/mydb"));
    assert_eq!(config.logging.level, "debug");
}

#[tokio::test]
async fn test_config_load_nonexistent() {
    let result = ServerConfig::load(Some(PathBuf::from("/nonexistent/config.yaml").as_path()));
    // Should return default config when file doesn't exist
    assert!(result.is_ok());
}
