use std::sync::Arc;

use mydb_storage::{BufferPool, Database, StorageEngineManager};
use mydb_wal::WalWriter;

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
    let tmp = tempfile::tempdir().unwrap();
    let pool = Arc::new(BufferPool::new(16384, "1M"));

    // Create WAL writer
    let wal_dir = tmp.path().join("wal");
    let wal_writer = Arc::new(parking_lot::Mutex::new(
        WalWriter::open(wal_dir, None).unwrap(),
    ));

    let mut db = Database::new("test_db", tmp.path().to_path_buf(), pool, wal_writer);

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
        next_page_number: 0,
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
    let tmp = tempfile::tempdir().unwrap();
    let manager = StorageEngineManager::new(tmp.path().to_path_buf(), 16384, "1M");

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

#[tokio::test]
async fn test_insert_and_scan_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = Arc::new(BufferPool::new(16384, "1M"));

    let wal_dir = tmp.path().join("wal");
    let wal_writer = Arc::new(parking_lot::Mutex::new(
        WalWriter::open(wal_dir, None).unwrap(),
    ));

    let mut db = Database::new("test_db", tmp.path().to_path_buf(), pool, wal_writer);

    // Create table
    let schema = mydb_storage::TableSchema {
        name: "users".to_string(),
        columns: vec![mydb_storage::Column {
            name: "id".to_string(),
            data_type: mydb_storage::DataType::Int,
            nullable: false,
            default: None,
            is_primary_key: true,
        }],
        primary_key: Some(vec!["id".to_string()]),
        indexes: vec![],
        next_page_number: 0,
    };

    db.create_table(schema).unwrap();

    // Insert rows
    let mut row1 = mydb_storage::Row::new();
    row1.push("id", 1i32.to_le_bytes().to_vec());
    db.insert_row("users", row1).unwrap();

    let mut row2 = mydb_storage::Row::new();
    row2.push("id", 2i32.to_le_bytes().to_vec());
    db.insert_row("users", row2).unwrap();

    // Scan table
    let rows = db.scan_table("users").unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn test_page_encode_decode() {
    let mut page = mydb_storage::Page::new(42, mydb_storage::PageType::Data, 1024);
    page.header.lsn = 100;
    page.header.row_count = 5;
    page.data[..11].copy_from_slice(b"hello world");

    let encoded = page.encode();
    let decoded = mydb_storage::Page::decode(&encoded).unwrap();

    assert_eq!(decoded.header.page_number, 42);
    assert_eq!(decoded.header.lsn, 100);
    assert_eq!(decoded.header.row_count, 5);
    assert_eq!(&decoded.data[..11], b"hello world");
}
