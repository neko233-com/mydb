use std::sync::Arc;

use mydb_storage::{BufferPool, Database, RowPredicate, StorageEngineManager, TableEngine};
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

    let db = Database::new("test_db", tmp.path().to_path_buf(), pool, wal_writer);

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
        triggers: Vec::new(),
        next_page_number: 0,
        generation: 0,
        create_sql: None,
        engine: TableEngine::Neko233,
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
async fn snapshot_guard_blocks_writes_at_actor_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = Arc::new(StorageEngineManager::new(
        tmp.path().to_path_buf(),
        16384,
        "1M",
    ));
    manager.init().await.unwrap();
    let guard = manager.snapshot_guard().await;
    let writer = manager.clone();
    let mut pending = tokio::spawn(async move { writer.create_database("after_snapshot").await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut pending)
            .await
            .is_err()
    );
    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(manager.current_lsn() > 0);
}

#[tokio::test]
async fn test_insert_and_scan_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = Arc::new(BufferPool::new(16384, "1M"));

    let wal_dir = tmp.path().join("wal");
    let wal_writer = Arc::new(parking_lot::Mutex::new(
        WalWriter::open(wal_dir, None).unwrap(),
    ));

    let db = Database::new("test_db", tmp.path().to_path_buf(), pool, wal_writer);

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
        triggers: Vec::new(),
        next_page_number: 0,
        generation: 0,
        create_sql: None,
        engine: TableEngine::Neko233,
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
async fn test_db233_batch_upsert_and_insert_ignore() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = StorageEngineManager::new(tmp.path().to_path_buf(), 16384, "1M");
    manager.init().await.unwrap();
    manager.create_database("game").await.unwrap();
    let db = manager.get_database("game").unwrap();
    db.create_table(mydb_storage::TableSchema {
        name: "players".into(),
        columns: vec![
            mydb_storage::Column {
                name: "playerId".into(),
                data_type: mydb_storage::DataType::Varchar(64),
                nullable: false,
                default: None,
                is_primary_key: true,
            },
            mydb_storage::Column {
                name: "name".into(),
                data_type: mydb_storage::DataType::Varchar(64),
                nullable: false,
                default: None,
                is_primary_key: false,
            },
            mydb_storage::Column {
                name: "level".into(),
                data_type: mydb_storage::DataType::Int,
                nullable: false,
                default: None,
                is_primary_key: false,
            },
        ],
        primary_key: Some(vec!["playerId".into()]),
        indexes: vec![],
        triggers: Vec::new(),
        next_page_number: 0,
        generation: 0,
        create_sql: None,
        engine: TableEngine::Neko233,
    })
    .unwrap();

    let row = |id: &str, name: &str, level: &str| {
        let mut row = mydb_storage::Row::new();
        row.push("playerId", id.as_bytes().to_vec());
        row.push("name", name.as_bytes().to_vec());
        row.push("level", level.as_bytes().to_vec());
        row
    };
    assert_eq!(
        db.upsert_rows(
            "players",
            vec![row("p1", "A", "1"), row("p2", "B", "2")],
            &["name".into(), "level".into()],
            false,
        )
        .unwrap(),
        2
    );
    assert_eq!(
        db.upsert_rows(
            "players",
            vec![row("p1", "new", "9"), row("p3", "C", "3")],
            &["name".into(), "level".into()],
            false,
        )
        .unwrap(),
        3
    );
    assert_eq!(
        db.upsert_rows("players", vec![row("p1", "ignored", "99")], &[], true,)
            .unwrap(),
        0
    );
    let rows = db.scan_table("players").unwrap();
    assert_eq!(rows.len(), 3);
    let player = rows
        .iter()
        .find(|item| item.get("playerId") == Some(b"p1"))
        .unwrap();
    assert_eq!(player.get("name"), Some(b"new".as_slice()));
    assert_eq!(player.get("level"), Some(b"9".as_slice()));
}

#[tokio::test]
async fn test_auto_increment_returns_id_and_recovers_sequence() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = StorageEngineManager::new(tmp.path().to_path_buf(), 16384, "1M");
    manager.init().await.unwrap();
    manager.create_database("game").await.unwrap();
    manager
        .execute_write(mydb_storage::WriteCommand::CreateTable {
            database: "game".into(),
            schema: mydb_storage::TableSchema {
                name: "items".into(),
                columns: vec![
                    mydb_storage::Column {
                        name: "id".into(),
                        data_type: mydb_storage::DataType::BigInt,
                        nullable: false,
                        default: None,
                        is_primary_key: true,
                    },
                    mydb_storage::Column {
                        name: "name".into(),
                        data_type: mydb_storage::DataType::Varchar(64),
                        nullable: false,
                        default: None,
                        is_primary_key: false,
                    },
                ],
                primary_key: Some(vec!["id".into()]),
                indexes: vec![],
                triggers: Vec::new(),
                next_page_number: 0,
                generation: 0,
                create_sql: Some(
                    "CREATE TABLE items (id BIGINT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(64) NOT NULL) ENGINE=InnoDB"
                        .into(),
                ),
                engine: TableEngine::Neko233,
            },
        })
        .await
        .unwrap();
    let insert = |name: &str| {
        let mut row = mydb_storage::Row::new();
        row.push("name", name.as_bytes().to_vec());
        mydb_storage::WriteCommand::Insert {
            database: "game".into(),
            table: "items".into(),
            row,
        }
    };
    let result = manager.execute_write(insert("one")).await.unwrap();
    assert_eq!(result.last_insert_id, 1);
    drop(manager);

    let reopened = StorageEngineManager::new(tmp.path().to_path_buf(), 16384, "1M");
    reopened.init().await.unwrap();
    let result = reopened.execute_write(insert("two")).await.unwrap();
    assert_eq!(result.last_insert_id, 2);
    let rows = reopened.scan_table("game", "items").unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.get("id") == Some(b"2")));
}

#[tokio::test]
async fn test_indexed_equality_lookup_is_rebuilt_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let wal_dir = tmp.path().join("wal");
    let wal_writer = Arc::new(parking_lot::Mutex::new(
        WalWriter::open(wal_dir.clone(), None).unwrap(),
    ));
    let db = Database::new(
        "game",
        tmp.path().to_path_buf(),
        Arc::new(BufferPool::new(16384, "1M")),
        wal_writer,
    );
    db.create_table(mydb_storage::TableSchema {
        name: "events".into(),
        columns: vec![
            mydb_storage::Column {
                name: "actor_id".into(),
                data_type: mydb_storage::DataType::BigInt,
                nullable: false,
                default: None,
                is_primary_key: false,
            },
            mydb_storage::Column {
                name: "seq".into(),
                data_type: mydb_storage::DataType::BigInt,
                nullable: false,
                default: None,
                is_primary_key: false,
            },
        ],
        primary_key: Some(vec!["actor_id".into(), "seq".into()]),
        indexes: vec![],
        triggers: Vec::new(),
        next_page_number: 0,
        generation: 0,
        create_sql: None,
        engine: TableEngine::Neko233,
    })
    .unwrap();
    for (actor, seq) in [("7", "1"), ("7", "2"), ("9", "1")] {
        let mut row = mydb_storage::Row::new();
        row.push("actor_id", actor.as_bytes().to_vec());
        row.push("seq", seq.as_bytes().to_vec());
        db.insert_row("events", row).unwrap();
    }
    db.save().await.unwrap();

    let rows = db
        .scan_table_filtered(
            "events",
            Some(&RowPredicate::Eq("actor_id".into(), b"7".to_vec())),
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    let rows = db
        .scan_table_filtered_limit(
            "events",
            Some(&RowPredicate::Eq("actor_id".into(), b"7".to_vec())),
            Some(1),
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(db
        .scan_table_filtered_limit(
            "events",
            Some(&RowPredicate::Eq("actor_id".into(), b"7".to_vec())),
            Some(0),
        )
        .unwrap()
        .is_empty());
    let rows = db
        .scan_table_filtered(
            "events",
            Some(&RowPredicate::In(
                "actor_id".into(),
                vec![b"7".to_vec(), b"9".to_vec(), b"404".to_vec()],
            )),
        )
        .unwrap();
    assert_eq!(rows.len(), 3);
    drop(db);

    let mut reopened = Database::new(
        "game",
        tmp.path().to_path_buf(),
        Arc::new(BufferPool::new(16384, "1M")),
        Arc::new(parking_lot::Mutex::new(
            WalWriter::open(wal_dir, None).unwrap(),
        )),
    );
    reopened.load().await.unwrap();
    let rows = reopened
        .scan_table_filtered(
            "events",
            Some(&RowPredicate::Eq("actor_id".into(), b"7".to_vec())),
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(reopened
        .scan_table_filtered(
            "events",
            Some(&RowPredicate::Eq("actor_id".into(), b"8".to_vec())),
        )
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_copy_on_write_rewrite_rolls_back_or_commits_at_schema_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let wal_dir = tmp.path().join("wal");
    {
        let db = Database::new(
            "game",
            tmp.path().to_path_buf(),
            Arc::new(BufferPool::new(16384, "1M")),
            Arc::new(parking_lot::Mutex::new(
                WalWriter::open(wal_dir.clone(), None).unwrap(),
            )),
        );
        db.create_table(mydb_storage::TableSchema {
            name: "players".into(),
            columns: vec![
                mydb_storage::Column {
                    name: "id".into(),
                    data_type: mydb_storage::DataType::BigInt,
                    nullable: false,
                    default: None,
                    is_primary_key: true,
                },
                mydb_storage::Column {
                    name: "score".into(),
                    data_type: mydb_storage::DataType::BigInt,
                    nullable: false,
                    default: None,
                    is_primary_key: false,
                },
            ],
            primary_key: Some(vec!["id".into()]),
            indexes: vec![],
            triggers: Vec::new(),
            next_page_number: 0,
            generation: 0,
            create_sql: None,
            engine: TableEngine::Neko233,
        })
        .unwrap();
        let mut row = mydb_storage::Row::new();
        row.push("id", b"1".to_vec());
        row.push("score", b"10".to_vec());
        db.insert_row("players", row).unwrap();
        db.save().await.unwrap();

        db.update_rows(
            "players",
            Some(&RowPredicate::Eq("id".into(), b"1".to_vec())),
            &[("score".into(), Some(b"99".to_vec()))],
        )
        .unwrap();
        assert_eq!(
            db.scan_table("players").unwrap()[0].get("score"),
            Some(b"99".as_slice())
        );
        // Simulate power loss before schema.json becomes the commit point.
    }

    let mut recovered = Database::new(
        "game",
        tmp.path().to_path_buf(),
        Arc::new(BufferPool::new(16384, "1M")),
        Arc::new(parking_lot::Mutex::new(
            WalWriter::open(wal_dir.clone(), None).unwrap(),
        )),
    );
    recovered.load().await.unwrap();
    assert_eq!(
        recovered.scan_table("players").unwrap()[0].get("score"),
        Some(b"10".as_slice())
    );

    recovered
        .update_rows(
            "players",
            Some(&RowPredicate::Eq("id".into(), b"1".to_vec())),
            &[("score".into(), Some(b"77".to_vec()))],
        )
        .unwrap();
    recovered.save().await.unwrap();
    drop(recovered);

    let mut committed = Database::new(
        "game",
        tmp.path().to_path_buf(),
        Arc::new(BufferPool::new(16384, "1M")),
        Arc::new(parking_lot::Mutex::new(
            WalWriter::open(wal_dir, None).unwrap(),
        )),
    );
    committed.load().await.unwrap();
    assert_eq!(
        committed.scan_table("players").unwrap()[0].get("score"),
        Some(b"77".as_slice())
    );
}

#[tokio::test]
async fn test_copy_on_write_drop_rolls_back_or_commits_at_schema_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let wal_dir = tmp.path().join("wal");
    {
        let db = Database::new(
            "game",
            tmp.path().to_path_buf(),
            Arc::new(BufferPool::new(16384, "1M")),
            Arc::new(parking_lot::Mutex::new(
                WalWriter::open(wal_dir.clone(), None).unwrap(),
            )),
        );
        db.create_table(mydb_storage::TableSchema {
            name: "players".into(),
            columns: vec![mydb_storage::Column {
                name: "id".into(),
                data_type: mydb_storage::DataType::BigInt,
                nullable: false,
                default: None,
                is_primary_key: true,
            }],
            primary_key: Some(vec!["id".into()]),
            indexes: vec![],
            triggers: Vec::new(),
            next_page_number: 0,
            generation: 0,
            create_sql: None,
            engine: TableEngine::Neko233,
        })
        .unwrap();
        let mut row = mydb_storage::Row::new();
        row.push("id", b"1".to_vec());
        db.insert_row("players", row).unwrap();
        db.save().await.unwrap();
        db.drop_table("players").unwrap();
        // Simulate power loss before the catalog snapshot commits the drop.
    }

    let mut recovered = Database::new(
        "game",
        tmp.path().to_path_buf(),
        Arc::new(BufferPool::new(16384, "1M")),
        Arc::new(parking_lot::Mutex::new(
            WalWriter::open(wal_dir.clone(), None).unwrap(),
        )),
    );
    recovered.load().await.unwrap();
    assert_eq!(recovered.scan_table("players").unwrap().len(), 1);
    recovered.drop_table("players").unwrap();
    recovered.save().await.unwrap();
    drop(recovered);

    let mut committed = Database::new(
        "game",
        tmp.path().to_path_buf(),
        Arc::new(BufferPool::new(16384, "1M")),
        Arc::new(parking_lot::Mutex::new(
            WalWriter::open(wal_dir, None).unwrap(),
        )),
    );
    committed.load().await.unwrap();
    assert!(committed.get_table("players").is_none());
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

#[tokio::test]
async fn test_actor_batch_rejects_duplicate_primary_key_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = StorageEngineManager::new(tmp.path().to_path_buf(), 16384, "1M");
    manager.init().await.unwrap();
    manager.create_database("game").await.unwrap();

    let schema = mydb_storage::TableSchema {
        name: "players".to_string(),
        columns: vec![mydb_storage::Column {
            name: "id".to_string(),
            data_type: mydb_storage::DataType::Int,
            nullable: false,
            default: None,
            is_primary_key: true,
        }],
        primary_key: Some(vec!["id".to_string()]),
        indexes: vec![],
        triggers: Vec::new(),
        next_page_number: 0,
        generation: 0,
        create_sql: None,
        engine: TableEngine::Neko233,
    };
    manager
        .execute_write(mydb_storage::WriteCommand::CreateTable {
            database: "game".to_string(),
            schema,
        })
        .await
        .unwrap();

    let mut first = mydb_storage::Row::new();
    first.push("id", b"1".to_vec());
    let second = first.clone();
    let result = manager
        .execute_batch(vec![
            mydb_storage::WriteCommand::Insert {
                database: "game".to_string(),
                table: "players".to_string(),
                row: first,
            },
            mydb_storage::WriteCommand::Insert {
                database: "game".to_string(),
                table: "players".to_string(),
                row: second,
            },
        ])
        .await;

    assert!(result.is_err());
    assert!(manager.scan_table("game", "players").unwrap().is_empty());
}

#[tokio::test]
async fn test_actor_batch_packs_rows_into_shared_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = StorageEngineManager::new(tmp.path().to_path_buf(), 16384, "1M");
    manager.init().await.unwrap();
    manager.create_database("game").await.unwrap();
    manager
        .execute_write(mydb_storage::WriteCommand::CreateTable {
            database: "game".into(),
            schema: mydb_storage::TableSchema {
                name: "events".into(),
                columns: vec![mydb_storage::Column {
                    name: "id".into(),
                    data_type: mydb_storage::DataType::BigInt,
                    nullable: false,
                    default: None,
                    is_primary_key: true,
                }],
                primary_key: Some(vec!["id".into()]),
                indexes: vec![],
                triggers: Vec::new(),
                next_page_number: 0,
                generation: 0,
                create_sql: None,
                engine: TableEngine::Neko233,
            },
        })
        .await
        .unwrap();
    let commands = (0..100)
        .map(|id| {
            let mut row = mydb_storage::Row::new();
            row.push("id", id.to_string().into_bytes());
            mydb_storage::WriteCommand::Insert {
                database: "game".into(),
                table: "events".into(),
                row,
            }
        })
        .collect();
    manager.execute_batch(commands).await.unwrap();

    assert_eq!(manager.scan_table("game", "events").unwrap().len(), 100);
    let page_files = std::fs::read_dir(tmp.path().join("game/events"))
        .unwrap()
        .count();
    assert!(page_files < 5, "expected packed pages, got {page_files}");
}

#[tokio::test]
async fn test_drop_and_recreate_table_removes_old_rows() {
    let temp = tempfile::tempdir().unwrap();
    let manager = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "16M");
    manager.init().await.unwrap();
    manager.create_database("game").await.unwrap();
    let schema = mydb_storage::TableSchema {
        name: "players".into(),
        columns: vec![mydb_storage::Column {
            name: "id".into(),
            data_type: mydb_storage::DataType::BigInt,
            nullable: false,
            default: None,
            is_primary_key: true,
        }],
        primary_key: Some(vec!["id".into()]),
        indexes: vec![],
        triggers: Vec::new(),
        next_page_number: 0,
        generation: 0,
        create_sql: None,
        engine: TableEngine::Neko233,
    };
    manager
        .execute_write(mydb_storage::WriteCommand::CreateTable {
            database: "game".into(),
            schema: schema.clone(),
        })
        .await
        .unwrap();
    let mut row = mydb_storage::Row::new();
    row.push("id", b"1".to_vec());
    manager
        .execute_write(mydb_storage::WriteCommand::Insert {
            database: "game".into(),
            table: "players".into(),
            row,
        })
        .await
        .unwrap();
    manager
        .execute_write(mydb_storage::WriteCommand::DropTable {
            database: "game".into(),
            table: "players".into(),
        })
        .await
        .unwrap();
    manager
        .execute_write(mydb_storage::WriteCommand::CreateTable {
            database: "game".into(),
            schema,
        })
        .await
        .unwrap();
    assert!(manager.scan_table("game", "players").unwrap().is_empty());
}

#[tokio::test]
async fn test_storage_cleanup_only_removes_unreferenced_page_directories() {
    let temp = tempfile::tempdir().unwrap();
    let manager = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "16M");
    manager.init().await.unwrap();
    manager.create_database("game").await.unwrap();
    let schema = mydb_storage::TableSchema {
        name: "orphaned".into(),
        columns: vec![mydb_storage::Column {
            name: "id".into(),
            data_type: mydb_storage::DataType::BigInt,
            nullable: false,
            default: None,
            is_primary_key: true,
        }],
        primary_key: Some(vec!["id".into()]),
        indexes: vec![],
        triggers: Vec::new(),
        next_page_number: 0,
        generation: 0,
        create_sql: None,
        engine: TableEngine::Neko233,
    };
    manager
        .execute_write(mydb_storage::WriteCommand::CreateTable {
            database: "game".into(),
            schema,
        })
        .await
        .unwrap();
    let mut row = mydb_storage::Row::new();
    row.push("id", b"1".to_vec());
    manager
        .execute_write(mydb_storage::WriteCommand::Insert {
            database: "game".into(),
            table: "orphaned".into(),
            row,
        })
        .await
        .unwrap();

    let database = manager.get_database("game").unwrap();
    database.tables.write().remove("orphaned");
    let unmanaged = temp.path().join("game").join("unmanaged");
    std::fs::create_dir_all(&unmanaged).unwrap();
    std::fs::write(unmanaged.join("do-not-delete.txt"), b"keep").unwrap();

    let before = manager.storage_inventory().unwrap();
    assert_eq!(before.orphan_storage.len(), 1);
    let cleaned = manager.cleanup_orphan_storage().await.unwrap();
    assert_eq!(cleaned.affected_rows, 1);
    assert!(!temp.path().join("game").join("orphaned").exists());
    assert!(unmanaged.join("do-not-delete.txt").exists());
    assert!(manager
        .storage_inventory()
        .unwrap()
        .orphan_storage
        .is_empty());
}

#[tokio::test]
async fn test_unique_index_is_enforced_atomically_and_allows_multiple_nulls() {
    let temp = tempfile::tempdir().unwrap();
    let manager = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "16M");
    manager.init().await.unwrap();
    manager.create_database("game").await.unwrap();
    manager
        .execute_write(mydb_storage::WriteCommand::CreateTable {
            database: "game".into(),
            schema: mydb_storage::TableSchema {
                name: "accounts".into(),
                columns: vec![
                    mydb_storage::Column {
                        name: "id".into(),
                        data_type: mydb_storage::DataType::BigInt,
                        nullable: false,
                        default: None,
                        is_primary_key: true,
                    },
                    mydb_storage::Column {
                        name: "email".into(),
                        data_type: mydb_storage::DataType::Varchar(255),
                        nullable: true,
                        default: None,
                        is_primary_key: false,
                    },
                ],
                primary_key: Some(vec!["id".into()]),
                indexes: vec![mydb_storage::Index {
                    name: "email_unique".into(),
                    columns: vec!["email".into()],
                    unique: true,
                }],
                triggers: Vec::new(),
                next_page_number: 0,
                generation: 0,
                create_sql: None,
                engine: TableEngine::Neko233,
            },
        })
        .await
        .unwrap();

    let make_row = |id: &str, email: Option<&str>| {
        let mut row = mydb_storage::Row::new();
        row.push("id", id.as_bytes().to_vec());
        if let Some(email) = email {
            row.push("email", email.as_bytes().to_vec());
        } else {
            row.push_null("email");
        }
        row
    };
    let duplicate = manager
        .execute_batch(vec![
            mydb_storage::WriteCommand::Insert {
                database: "game".into(),
                table: "accounts".into(),
                row: make_row("1", Some("same@example.test")),
            },
            mydb_storage::WriteCommand::Insert {
                database: "game".into(),
                table: "accounts".into(),
                row: make_row("2", Some("same@example.test")),
            },
        ])
        .await;
    assert!(duplicate.is_err());
    assert!(manager.scan_table("game", "accounts").unwrap().is_empty());

    manager
        .execute_batch(vec![
            mydb_storage::WriteCommand::Insert {
                database: "game".into(),
                table: "accounts".into(),
                row: make_row("1", None),
            },
            mydb_storage::WriteCommand::Insert {
                database: "game".into(),
                table: "accounts".into(),
                row: make_row("2", None),
            },
        ])
        .await
        .unwrap();
    assert_eq!(manager.scan_table("game", "accounts").unwrap().len(), 2);
}
