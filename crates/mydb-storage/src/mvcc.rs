use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::Row;

pub type TransactionId = u64;
pub type CommitId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

#[derive(Debug, Clone)]
pub struct ReadView {
    pub creator: TransactionId,
    pub snapshot: CommitId,
    pub active_transactions: Arc<HashSet<TransactionId>>,
}

#[derive(Debug, Clone)]
struct HistoricalVersion {
    commit_id: CommitId,
    row: Row,
}

#[derive(Debug, Clone, Copy)]
struct ActiveTransaction {
    isolation: IsolationLevel,
    snapshot: Option<CommitId>,
}

type RowKey = (String, String, u64);

pub struct MvccManager {
    next_transaction_id: AtomicU64,
    next_commit_id: AtomicU64,
    active_transactions: RwLock<HashMap<TransactionId, ActiveTransaction>>,
    current_commit: RwLock<HashMap<RowKey, CommitId>>,
    history: RwLock<HashMap<RowKey, Vec<HistoricalVersion>>>,
}

impl Default for MvccManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MvccManager {
    pub fn new() -> Self {
        Self {
            next_transaction_id: AtomicU64::new(1),
            next_commit_id: AtomicU64::new(1),
            active_transactions: RwLock::new(HashMap::new()),
            current_commit: RwLock::new(HashMap::new()),
            history: RwLock::new(HashMap::new()),
        }
    }

    pub fn begin(&self, isolation: IsolationLevel) -> TransactionId {
        let id = self.next_transaction_id.fetch_add(1, Ordering::Relaxed);
        self.active_transactions.write().insert(
            id,
            ActiveTransaction {
                isolation,
                snapshot: None,
            },
        );
        id
    }

    pub fn read_view(&self, transaction_id: TransactionId) -> anyhow::Result<ReadView> {
        let mut active = self.active_transactions.write();
        let transaction = active
            .get_mut(&transaction_id)
            .ok_or_else(|| anyhow::anyhow!("transaction {} is not active", transaction_id))?;
        let snapshot = match transaction.isolation {
            IsolationLevel::ReadCommitted | IsolationLevel::ReadUncommitted => self.latest_commit(),
            IsolationLevel::RepeatableRead | IsolationLevel::Serializable => *transaction
                .snapshot
                .get_or_insert_with(|| self.latest_commit()),
        };
        Ok(ReadView {
            creator: transaction_id,
            snapshot,
            active_transactions: Arc::new(active.keys().copied().collect()),
        })
    }

    pub fn end(&self, transaction_id: TransactionId) {
        self.active_transactions.write().remove(&transaction_id);
        self.purge_old_versions();
    }

    pub fn latest_commit(&self) -> CommitId {
        self.next_commit_id
            .load(Ordering::Acquire)
            .saturating_sub(1)
    }

    pub fn allocate_commit_id(&self) -> CommitId {
        self.next_commit_id.fetch_add(1, Ordering::AcqRel)
    }

    pub fn seed_table(&self, database: &str, table: &str, rows: &[Row]) {
        let mut current = self.current_commit.write();
        for row in rows {
            if row.row_id != 0 {
                current
                    .entry((database.to_string(), table.to_string(), row.row_id))
                    .or_insert(0);
            }
        }
    }

    pub fn record_table_commit(
        &self,
        database: &str,
        table: &str,
        before: &[Row],
        after: &[Row],
        commit_id: CommitId,
    ) {
        let before_by_id = before
            .iter()
            .filter(|row| row.row_id != 0)
            .map(|row| (row.row_id, row))
            .collect::<HashMap<_, _>>();
        let after_by_id = after
            .iter()
            .filter(|row| row.row_id != 0)
            .map(|row| (row.row_id, row))
            .collect::<HashMap<_, _>>();
        let mut ids = before_by_id.keys().copied().collect::<HashSet<_>>();
        ids.extend(after_by_id.keys().copied());

        let mut current = self.current_commit.write();
        let mut history = self.history.write();
        for row_id in ids {
            let key = (database.to_string(), table.to_string(), row_id);
            let old = before_by_id.get(&row_id).copied();
            let new = after_by_id.get(&row_id).copied();
            if old == new {
                current.entry(key).or_insert(0);
                continue;
            }
            if let Some(old) = old {
                let old_commit = current.get(&key).copied().unwrap_or(0);
                history
                    .entry(key.clone())
                    .or_default()
                    .push(HistoricalVersion {
                        commit_id: old_commit,
                        row: old.clone(),
                    });
            }
            current.insert(key, commit_id);
        }
    }

    pub fn record_insert_rows(
        &self,
        database: &str,
        table: &str,
        row_ids: &[u64],
        commit_id: CommitId,
    ) {
        let mut current = self.current_commit.write();
        for row_id in row_ids.iter().copied().filter(|row_id| *row_id != 0) {
            current.insert((database.to_string(), table.to_string(), row_id), commit_id);
        }
    }

    pub fn visible_table(
        &self,
        database: &str,
        table: &str,
        current_rows: &[Row],
        view: &ReadView,
    ) -> Vec<Row> {
        let current_commits = self.current_commit.read();
        let history = self.history.read();
        let current_ids = current_rows
            .iter()
            .filter_map(|row| (row.row_id != 0).then_some(row.row_id))
            .collect::<HashSet<_>>();
        let mut visible = Vec::with_capacity(current_rows.len());
        for row in current_rows {
            let key = (database.to_string(), table.to_string(), row.row_id);
            let commit_id = current_commits.get(&key).copied().unwrap_or(0);
            if commit_id <= view.snapshot {
                visible.push(row.clone());
                continue;
            }
            if let Some(previous) = history
                .get(&key)
                .and_then(|versions| latest_version_at(versions, view.snapshot))
            {
                visible.push(previous.row.clone());
            }
        }
        for (key, versions) in history.iter() {
            if key.0 != database || key.1 != table || current_ids.contains(&key.2) {
                continue;
            }
            let current_commit = current_commits.get(key).copied().unwrap_or(0);
            if current_commit <= view.snapshot {
                continue;
            }
            if let Some(previous) = latest_version_at(versions, view.snapshot) {
                visible.push(previous.row.clone());
            }
        }
        visible
    }

    pub fn history_len(&self) -> usize {
        self.history.read().values().map(Vec::len).sum()
    }

    pub fn purge_old_versions(&self) {
        let active = self.active_transactions.read();
        let Some(min_snapshot) = active
            .values()
            .filter_map(|transaction| transaction.snapshot)
            .min()
        else {
            drop(active);
            self.history.write().clear();
            return;
        };
        drop(active);
        let mut history = self.history.write();
        for versions in history.values_mut() {
            if versions.len() < 2 {
                continue;
            }
            let keep_from = versions
                .iter()
                .rposition(|version| version.commit_id <= min_snapshot)
                .unwrap_or(0);
            if keep_from > 0 {
                versions.drain(..keep_from);
            }
        }
        history.retain(|_, versions| !versions.is_empty());
    }
}

fn latest_version_at(
    versions: &[HistoricalVersion],
    snapshot: CommitId,
) -> Option<&HistoricalVersion> {
    versions
        .iter()
        .filter(|version| version.commit_id <= snapshot)
        .max_by_key(|version| version.commit_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, value: &str) -> Row {
        let mut row = Row::new();
        row.row_id = id.parse().expect("row id");
        row.push("id", id.as_bytes().to_vec());
        row.push("value", value.as_bytes().to_vec());
        row
    }

    #[test]
    fn repeatable_read_uses_version_chain_and_purge_keeps_active_view() {
        let manager = MvccManager::new();
        let first = row("1", "before");
        manager.seed_table("db", "t", std::slice::from_ref(&first));
        let transaction = manager.begin(IsolationLevel::RepeatableRead);
        let view = manager.read_view(transaction).expect("read view");
        let second = row("1", "after");
        let commit = manager.allocate_commit_id();
        manager.record_table_commit(
            "db",
            "t",
            std::slice::from_ref(&first),
            std::slice::from_ref(&second),
            commit,
        );

        let visible = manager.visible_table("db", "t", &[second], &view);
        assert_eq!(visible[0].get("value"), Some(b"before".as_slice()));
        assert_eq!(manager.history_len(), 1);
        manager.end(transaction);
        assert_eq!(manager.history_len(), 0);
    }

    #[test]
    fn deleted_rows_remain_visible_to_an_older_read_view() {
        let manager = MvccManager::new();
        let first = row("1", "before");
        manager.seed_table("db", "t", std::slice::from_ref(&first));
        let transaction = manager.begin(IsolationLevel::RepeatableRead);
        let view = manager.read_view(transaction).expect("read view");
        let commit = manager.allocate_commit_id();
        manager.record_table_commit("db", "t", &[first], &[], commit);

        let visible = manager.visible_table("db", "t", &[], &view);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].get("id"), Some(b"1".as_slice()));
    }

    #[test]
    fn inserted_rows_are_hidden_from_an_older_read_view() {
        let manager = MvccManager::new();
        let transaction = manager.begin(IsolationLevel::RepeatableRead);
        let view = manager.read_view(transaction).expect("read view");
        let inserted = row("1", "after");
        let commit = manager.allocate_commit_id();
        manager.record_insert_rows("db", "t", &[inserted.row_id], commit);

        assert!(manager
            .visible_table("db", "t", &[inserted], &view)
            .is_empty());
        manager.end(transaction);
    }
}
