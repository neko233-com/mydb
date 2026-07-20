use std::sync::Arc;

use mydb_storage::StorageEngineManager;
use mydb_transaction::{IsolationLevel, TransactionManager};

#[tokio::test]
async fn test_transaction_begin_commit() {
    let data_dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(StorageEngineManager::new(
        data_dir.path().to_path_buf(),
        16384,
        "1M",
    ));
    storage.init().await.unwrap();

    let tx_manager = TransactionManager::new(storage);

    // Begin transaction
    let tx = tx_manager.begin(IsolationLevel::RepeatableRead);
    let tx_id = tx.read().id;

    // Verify transaction is active
    assert_eq!(tx.read().state, mydb_transaction::TransactionState::Active);

    // Commit transaction
    tx_manager.commit(tx_id).unwrap();

    // Verify transaction is committed
    let tx = tx_manager.get_transaction(tx_id).unwrap();
    assert_eq!(
        tx.read().state,
        mydb_transaction::TransactionState::Committed
    );
}

#[tokio::test]
async fn test_transaction_rollback() {
    let data_dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(StorageEngineManager::new(
        data_dir.path().to_path_buf(),
        16384,
        "1M",
    ));
    storage.init().await.unwrap();

    let tx_manager = TransactionManager::new(storage);

    // Begin transaction
    let tx = tx_manager.begin(IsolationLevel::ReadCommitted);
    let tx_id = tx.read().id;

    // Rollback transaction
    tx_manager.rollback(tx_id).unwrap();

    // Verify transaction is aborted
    let tx = tx_manager.get_transaction(tx_id).unwrap();
    assert_eq!(tx.read().state, mydb_transaction::TransactionState::Aborted);
}

#[tokio::test]
async fn test_isolation_levels() {
    let levels = vec![
        IsolationLevel::ReadUncommitted,
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
    ];

    for level in levels {
        let data_dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(StorageEngineManager::new(
            data_dir.path().to_path_buf(),
            16384,
            "1M",
        ));
        storage.init().await.unwrap();

        let tx_manager = TransactionManager::new(storage);
        let tx = tx_manager.begin(level);

        assert_eq!(tx.read().state, mydb_transaction::TransactionState::Active);
    }
}
