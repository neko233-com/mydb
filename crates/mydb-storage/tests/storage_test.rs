use std::sync::Arc;

use mydb_storage::{BufferPool, Database, StorageEngineManager};

#[tokio::test]
async fn test_buffer_pool_creation() {
    let pool = BufferPool::new(16384, "1G");
    assert!(pool.get_page(0).is_none());
}

#[tokio::test]
async fn test_buffer_pool_insert_get() {
    let pool = BufferPool::new(16384, "1M");
    
    let page = mydb_storage::Page::new(0, mydb_storage::PageType::Data, 16384);
    pool.insert_page(page);
    
    let retrieved = pool.get_page(0);
    assert!(retrieved.is_some());
}

#[tokio::test]
async fn test_database_operations() {
    let pool = Arc::new(BufferPool::new(16384, "1M"));
    let data_dir = tempfile::tempdir().unwrap();
    
    let mut db = Database::new("test_db", data_dir.path().to_path_buf(), pool);
    
    // Create table
    let schema = mydb_storage::TableSchema {
        name: "users".to_string(),
        columns: vec![
            mydb_storage::Column {
                name: "id".to_string(),
                data_type: mydb_storage::DataType::Int,
                nullable: false,
                default: None,
                is_primary_key: true,
            },
            mydb_storage::Column {
                name: "name".to_string(),
                data_type: mydb_storage::DataType::Varchar(100),
                nullable: false,
                default: None,
                is_primary_key: false,
            },
        ],
        primary_key: Some(vec!["id".to_string()]),
        indexes: vec![],
    };
    
    db.create_table(schema).unwrap();
    
    // List tables
    let tables = db.list_tables();
    assert_eq!(tables.len(), 1);
    assert!(tables.contains(&"users".to_string()));
    
    // Get table
    let table = db.get_table("users").unwrap();
    assert_eq!(table.name, "users");
    assert_eq!(table.columns.len(), 2);
}

#[tokio::test]
async fn test_storage_engine_manager() {
    let data_dir = tempfile::tempdir().unwrap();
    let manager = StorageEngineManager::new(data_dir.path().to_path_buf(), 16384, "1M");
    
    manager.init().await.unwrap();
    
    // Create database
    manager.create_database("test_db").await.unwrap();
    
    // List databases
    let databases = manager.list_databases();
    assert!(databases.contains(&"test_db".to_string()));
    
    // Get database
    let db = manager.get_database("test_db").unwrap();
    assert_eq!(db.name, "test_db");
}
