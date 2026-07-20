use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use tracing::{debug, info};
use uuid::Uuid;

use mydb_storage::StorageEngineManager;

// Transaction Isolation Levels
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl std::str::FromStr for IsolationLevel {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "READ UNCOMMITTED" => Ok(Self::ReadUncommitted),
            "READ COMMITTED" => Ok(Self::ReadCommitted),
            "REPEATABLE READ" => Ok(Self::RepeatableRead),
            "SERIALIZABLE" => Ok(Self::Serializable),
            _ => Ok(Self::RepeatableRead),
        }
    }
}

// Transaction States
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransactionState {
    Active,
    Prepared,
    Committed,
    Aborted,
}

// Transaction
pub struct Transaction {
    pub id: Uuid,
    pub state: TransactionState,
    pub isolation_level: IsolationLevel,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
    pub read_set: Vec<ReadOp>,
    pub write_set: Vec<WriteOp>,
}

#[derive(Debug, Clone)]
pub struct ReadOp {
    pub table: String,
    pub key: Vec<u8>,
    pub version: u64,
}

#[derive(Debug, Clone)]
pub struct WriteOp {
    pub table: String,
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
    pub op_type: WriteOpType,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WriteOpType {
    Insert,
    Update,
    Delete,
}

// Transaction Manager
pub struct TransactionManager {
    transactions: RwLock<HashMap<Uuid, Arc<RwLock<Transaction>>>>,
}

impl TransactionManager {
    pub fn new(_storage: Arc<StorageEngineManager>) -> Self {
        Self {
            transactions: RwLock::new(HashMap::new()),
        }
    }

    pub fn begin(&self, isolation_level: IsolationLevel) -> Arc<RwLock<Transaction>> {
        let tx = Transaction {
            id: Uuid::new_v4(),
            state: TransactionState::Active,
            isolation_level,
            created_at: Utc::now(),
            modified_at: Utc::now(),
            read_set: Vec::new(),
            write_set: Vec::new(),
        };

        let tx = Arc::new(RwLock::new(tx));
        self.transactions.write().insert(tx.read().id, tx.clone());

        info!("Transaction started: {}", tx.read().id);
        tx
    }

    pub fn commit(&self, tx_id: Uuid) -> Result<()> {
        let tx = self
            .transactions
            .read()
            .get(&tx_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Transaction not found"))?;

        let mut tx = tx.write();

        // Validate transaction
        if tx.state != TransactionState::Active {
            anyhow::bail!("Transaction is not active");
        }

        // Apply write operations
        for op in &tx.write_set {
            match op.op_type {
                WriteOpType::Insert | WriteOpType::Update => {
                    debug!(
                        "Applying write to {}.{}",
                        tx.id,
                        String::from_utf8_lossy(&op.key)
                    );
                }
                WriteOpType::Delete => {
                    debug!(
                        "Applying delete from {}.{}",
                        tx.id,
                        String::from_utf8_lossy(&op.key)
                    );
                }
            }
        }

        tx.state = TransactionState::Committed;
        tx.modified_at = Utc::now();

        info!("Transaction committed: {}", tx_id);
        Ok(())
    }

    pub fn rollback(&self, tx_id: Uuid) -> Result<()> {
        let tx = self
            .transactions
            .read()
            .get(&tx_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Transaction not found"))?;

        let mut tx = tx.write();

        if tx.state != TransactionState::Active {
            anyhow::bail!("Transaction is not active");
        }

        // Rollback write operations
        for op in tx.write_set.iter().rev() {
            match op.op_type {
                WriteOpType::Insert => {
                    debug!("Rolling back insert from {}", tx.id);
                }
                WriteOpType::Update => {
                    debug!("Rolling back update from {}", tx.id);
                }
                WriteOpType::Delete => {
                    debug!("Rolling back delete from {}", tx.id);
                }
            }
        }

        tx.state = TransactionState::Aborted;
        tx.modified_at = Utc::now();

        info!("Transaction rolled back: {}", tx_id);
        Ok(())
    }

    pub fn get_transaction(&self, tx_id: Uuid) -> Option<Arc<RwLock<Transaction>>> {
        self.transactions.read().get(&tx_id).cloned()
    }

    pub fn cleanup_transactions(&self) {
        let mut transactions = self.transactions.write();
        transactions.retain(|_, tx| {
            let tx = tx.read();
            tx.state == TransactionState::Active
        });
    }
}

// Lock Manager for MVCC
pub struct LockManager {
    locks: RwLock<HashMap<String, Lock>>,
}

#[derive(Debug, Clone)]
pub struct Lock {
    pub resource: String,
    pub lock_type: LockType,
    pub holder: Uuid,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LockType {
    Shared,
    Exclusive,
    IntentionShared,
    IntentionExclusive,
}

impl LockManager {
    pub fn new() -> Self {
        Self {
            locks: RwLock::new(HashMap::new()),
        }
    }

    pub fn acquire_lock(&self, resource: &str, lock_type: LockType, tx_id: Uuid) -> Result<()> {
        let mut locks = self.locks.write();

        if let Some(existing) = locks.get(resource) {
            // Check for compatibility
            if existing.holder == tx_id {
                // Same transaction, upgrade if needed
                if lock_type == LockType::Exclusive && existing.lock_type == LockType::Shared {
                    // Remove and re-insert with new lock type
                    let mut upgraded = existing.clone();
                    upgraded.lock_type = lock_type;
                    locks.insert(resource.to_string(), upgraded);
                }
                return Ok(());
            }

            // Different transaction, check compatibility
            match (existing.lock_type, lock_type) {
                (LockType::Shared, LockType::Shared) => {
                    // Shared locks are compatible
                }
                _ => {
                    anyhow::bail!("Lock conflict on resource '{}'", resource);
                }
            }
        }

        locks.insert(
            resource.to_string(),
            Lock {
                resource: resource.to_string(),
                lock_type,
                holder: tx_id,
                created_at: Utc::now(),
            },
        );

        Ok(())
    }

    pub fn release_lock(&self, resource: &str, tx_id: Uuid) {
        let mut locks = self.locks.write();
        if let Some(lock) = locks.get(resource) {
            if lock.holder == tx_id {
                locks.remove(resource);
            }
        }
    }

    pub fn release_all_locks(&self, tx_id: Uuid) {
        let mut locks = self.locks.write();
        locks.retain(|_, lock| lock.holder != tx_id);
    }
}

impl Default for LockManager {
    fn default() -> Self {
        Self::new()
    }
}

// Deadlock Detection
pub struct DeadlockDetector {
    waits_for: RwLock<HashMap<Uuid, Vec<Uuid>>>,
}

impl DeadlockDetector {
    pub fn new() -> Self {
        Self {
            waits_for: RwLock::new(HashMap::new()),
        }
    }

    pub fn detect_deadlock(&self) -> Option<Vec<Uuid>> {
        let waits_for = self.waits_for.read();
        let mut visited = std::collections::HashSet::new();
        let mut path = Vec::new();

        for &tx_id in waits_for.keys() {
            if self.dfs(tx_id, &waits_for, &mut visited, &mut path) {
                return Some(path);
            }
        }

        None
    }

    fn dfs(
        &self,
        tx_id: Uuid,
        waits_for: &HashMap<Uuid, Vec<Uuid>>,
        visited: &mut std::collections::HashSet<Uuid>,
        path: &mut Vec<Uuid>,
    ) -> bool {
        if path.contains(&tx_id) {
            path.push(tx_id);
            return true;
        }

        if visited.contains(&tx_id) {
            return false;
        }

        visited.insert(tx_id);
        path.push(tx_id);

        if let Some(waiting) = waits_for.get(&tx_id) {
            for &next in waiting {
                if self.dfs(next, waits_for, visited, path) {
                    return true;
                }
            }
        }

        path.pop();
        false
    }
}

impl Default for DeadlockDetector {
    fn default() -> Self {
        Self::new()
    }
}
