use std::path::PathBuf;

use mydb_config::ServerConfig;

#[tokio::test]
async fn test_default_config() {
    let config = ServerConfig::default();

    assert_eq!(config.server.host, "0.0.0.0");
    assert_eq!(config.server.port, 3306);
    assert_eq!(config.server.max_connections, 1000);
    assert_eq!(config.storage.engine, "neko233");
    assert_eq!(config.storage.buffer_pool_size, "1G");
    assert_eq!(config.storage.group_commit_window_us, 250);
    assert_eq!(config.logging.level, "info");
    assert!(config.security.local_infile);
    assert_eq!(config.security.max_load_data_size, 1024 * 1024 * 1024);
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
  group_commit_window_us: 500

logging:
  level: "debug"
"#;

    let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();

    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 3307);
    assert_eq!(config.server.max_connections, 500);
    assert_eq!(config.storage.data_dir, PathBuf::from("/tmp/mydb"));
    assert_eq!(config.storage.group_commit_window_us, 500);
    assert_eq!(config.logging.level, "debug");
}

#[tokio::test]
async fn test_config_load_nonexistent() {
    let result = ServerConfig::load(Some(PathBuf::from("/nonexistent/config.yaml").as_path()));
    // Should return default config when file doesn't exist
    assert!(result.is_ok());
}
