//! Change Data Capture for MyDB.
//!
//! CDC is a read-side extension: it tails committed `GroupCommit` WAL records
//! and writes row-level NDJSON events. It never touches the write hot path, so
//! leaving it disabled (the default) costs nothing. Every setting can be
//! hot-reloaded through the admin API, including enabling it at runtime.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use base64::Engine;
use mydb_config::{CdcFilters, CdcSection};
use mydb_wal::{WalChange, WalChangeOp, WalReader, WalRecordType};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

const EVENT_VERSION: u8 = 1;
const CURSOR_FILE: &str = "cursor.json";
const CHECKPOINT_FILE: &str = "checkpoint.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdcEvent {
    pub v: u8,
    pub lsn: u64,
    pub tx_id: u64,
    pub committed_unix_ms: u64,
    pub changes: Vec<CdcChangeEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdcChangeEvent {
    pub op: String,
    pub database: String,
    pub table: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub before: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub after: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ddl_sql: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdcStatus {
    pub enabled: bool,
    pub dir: String,
    pub last_lsn: u64,
    /// Last LSN a downstream consumer confirmed. Kept separate from
    /// `cursor.json` so a TiDB/warehouse loader can acknowledge progress
    /// without moving the producer's tail position.
    pub checkpoint_lsn: u64,
    pub events_emitted: u64,
    pub changes_emitted: u64,
    pub files_rotated: u64,
    pub current_file: Option<String>,
    pub last_error: Option<String>,
}

impl CdcStatus {
    /// LSN distance between the producer and the confirmed consumer.
    pub fn lag_lsn(&self) -> u64 {
        self.last_lsn.saturating_sub(self.checkpoint_lsn)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Cursor {
    #[serde(default)]
    last_lsn: u64,
}

struct CdcRuntime {
    dir: PathBuf,
    prefix: String,
    max_file_size: u64,
    max_files: u32,
    flush_every: u64,
    include_before: bool,
    include_ddl: bool,
    include_row_images: bool,
    filters: CdcFilters,
    file: Option<File>,
    file_index: u64,
    file_bytes: u64,
    /// Persisted cursor: only advances when a batched line is durably written.
    last_lsn: u64,
    /// Consumer-confirmed LSN, persisted in `checkpoint.json`.
    checkpoint_lsn: u64,
    /// In-memory scan position; advances for every scanned group so a partial
    /// batch is not re-scanned within the same process.
    last_scanned_lsn: u64,
    events_emitted: u64,
    changes_emitted: u64,
    files_rotated: u64,
    pending: u64,
    pending_changes: Vec<CdcChangeEvent>,
    last_error: Option<String>,
}

/// Owns the hot-reloadable CDC configuration and the live output sink.
pub struct CdcManager {
    config: ArcSwap<CdcSection>,
    data_dir: PathBuf,
    runtime: Mutex<CdcRuntime>,
}

impl CdcManager {
    pub fn new(data_dir: PathBuf, section: CdcSection) -> Self {
        let manager = Self {
            config: ArcSwap::from(Arc::new(section.clone())),
            data_dir,
            runtime: Mutex::new(CdcRuntime {
                dir: PathBuf::new(),
                prefix: "events".to_string(),
                max_file_size: parse_size("128M"),
                max_files: 10,
                flush_every: 1,
                include_before: true,
                include_ddl: true,
                include_row_images: true,
                filters: CdcFilters::default(),
                file: None,
                file_index: 0,
                file_bytes: 0,
                last_lsn: 0,
                checkpoint_lsn: 0,
                last_scanned_lsn: 0,
                events_emitted: 0,
                changes_emitted: 0,
                files_rotated: 0,
                pending: 0,
                pending_changes: Vec::new(),
                last_error: None,
            }),
        };
        if let Err(error) = manager.apply(section) {
            warn!(error = %error, "initial CDC setup failed; CDC stays disabled");
        }
        manager
    }

    /// Apply a full CDC section. Called at boot and on every config reload.
    pub fn apply(&self, section: CdcSection) -> Result<()> {
        self.config.store(Arc::new(section.clone()));
        let mut runtime = self.runtime.lock();
        let dir = section
            .dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("cdc"));
        if !section.enabled {
            runtime.file = None;
            runtime.last_error = None;
            return Ok(());
        }
        fs::create_dir_all(&dir)
            .with_context(|| format!("create CDC directory {}", dir.display()))?;
        let max_file_size = parse_size(&section.max_file_size);
        if max_file_size == 0 {
            return Err(anyhow!("cdc.max_file_size must be greater than zero"));
        }
        let cursor = read_cursor(&dir.join(CURSOR_FILE))?;
        let checkpoint = read_cursor(&dir.join(CHECKPOINT_FILE))?;
        let index = latest_file_index(&dir, &section.file_prefix)?;

        let reopen = !section.enabled
            || runtime.file.is_none()
            || runtime.dir != dir
            || runtime.prefix != section.file_prefix;
        if reopen {
            runtime.file = None;
            runtime.file_index = index;
            runtime.file_bytes = 0;
        }
        runtime.dir = dir;
        runtime.prefix = section.file_prefix.clone();
        runtime.max_file_size = max_file_size;
        runtime.max_files = section.max_files;
        runtime.flush_every = section.flush_every.max(1);
        runtime.include_before = section.include_before;
        runtime.include_ddl = section.include_ddl;
        runtime.include_row_images = section.include_row_images;
        runtime.filters = section.filters.clone();
        runtime.last_lsn = cursor.last_lsn;
        runtime.checkpoint_lsn = checkpoint.last_lsn;
        runtime.last_scanned_lsn = cursor.last_lsn;
        runtime.pending = 0;
        runtime.pending_changes.clear();
        runtime.last_error = None;
        if runtime.file.is_none() {
            let path = runtime
                .dir
                .join(event_file_name(&runtime.prefix, runtime.file_index));
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("open CDC event file {}", path.display()))?;
            runtime.file_bytes = file
                .metadata()
                .map(|meta| meta.len())
                .unwrap_or(runtime.file_bytes);
            runtime.file = Some(file);
        }
        Ok(())
    }

    pub fn is_enabled(&self) -> bool {
        self.config.load().enabled
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.config.load().poll_interval_ms.max(10))
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        let mut section = (*self.config.load_full()).clone();
        section.enabled = enabled;
        self.apply(section)
    }

    /// Re-read the YAML file and apply only its CDC section. This is the
    /// "hot reload from config file" path used by the admin API.
    pub fn reload_from(&self, path: Option<&Path>) -> Result<()> {
        let section = mydb_config::ServerConfig::load(path)?.cdc;
        self.apply(section)
    }

    pub fn status(&self) -> CdcStatus {
        let runtime = self.runtime.lock();
        CdcStatus {
            enabled: self.config.load().enabled,
            dir: runtime.dir.to_string_lossy().to_string(),
            last_lsn: runtime.last_lsn,
            checkpoint_lsn: runtime.checkpoint_lsn,
            events_emitted: runtime.events_emitted,
            changes_emitted: runtime.changes_emitted,
            files_rotated: runtime.files_rotated,
            current_file: runtime
                .file
                .as_ref()
                .map(|_| event_file_name(&runtime.prefix, runtime.file_index)),
            last_error: runtime.last_error.clone(),
        }
    }

    fn record_error(&self, error: anyhow::Error) {
        let message = format!("{error:#}");
        warn!(error = %message, "CDC tailer iteration failed");
        self.runtime.lock().last_error = Some(message);
    }

    /// Confirm that a downstream consumer durably applied events up to `lsn`.
    /// The producer cursor is untouched: replaying from `cursor.json` still
    /// yields every event, while this checkpoint drives lag reporting and lets
    /// a TiDB loader resume without guessing.
    pub fn set_checkpoint(&self, lsn: u64) -> Result<u64> {
        let mut runtime = self.runtime.lock();
        if runtime.dir.as_os_str().is_empty() {
            return Err(anyhow!("CDC is not enabled; no checkpoint directory"));
        }
        if lsn > runtime.last_lsn {
            return Err(anyhow!(
                "checkpoint LSN {lsn} is ahead of producer LSN {}",
                runtime.last_lsn
            ));
        }
        if lsn < runtime.checkpoint_lsn {
            return Err(anyhow!(
                "checkpoint LSN {lsn} would move backwards from {}",
                runtime.checkpoint_lsn
            ));
        }
        write_cursor(&runtime.dir.join(CHECKPOINT_FILE), lsn)?;
        runtime.checkpoint_lsn = lsn;
        Ok(lsn)
    }

    /// Scan the WAL once and append every newly committed group after the
    /// persisted cursor. Runs on a blocking thread from the tailer task.
    pub fn scan_once(&self, wal_dir: &Path) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        let cursor = self.runtime.lock().last_scanned_lsn;
        let reader = WalReader::open_silent(wal_dir.to_path_buf())?;
        let mut groups: Vec<(u64, u64, u64, Vec<u8>)> = Vec::new();
        reader.replay_silent(|record| {
            if record.record_type == WalRecordType::GroupCommit && record.lsn > cursor {
                let committed_unix_ms =
                    mydb_storage::wal_group_commit_unix_ms(&record).unwrap_or(0);
                groups.push((record.lsn, record.tx_id, committed_unix_ms, record.data));
            }
        })?;
        groups.sort_by_key(|group| group.0);
        for (lsn, tx_id, committed_unix_ms, payload) in groups {
            if lsn <= self.runtime.lock().last_scanned_lsn {
                continue;
            }
            let changes = mydb_storage::decode_wal_group_changes(&payload)?;
            self.append_group(lsn, tx_id, committed_unix_ms, &changes)?;
        }
        Ok(())
    }

    fn append_group(
        &self,
        lsn: u64,
        tx_id: u64,
        committed_unix_ms: u64,
        changes: &[WalChange],
    ) -> Result<()> {
        let mut runtime = self.runtime.lock();
        if !self.config.load().enabled {
            return Ok(());
        }
        for change in changes {
            let filtered = self.filter_change(change, &runtime);
            if let Some(event) = filtered {
                runtime.pending_changes.push(event);
            }
        }
        runtime.pending += 1;
        if lsn > runtime.last_scanned_lsn {
            runtime.last_scanned_lsn = lsn;
        }
        if runtime.pending < runtime.flush_every {
            return Ok(());
        }
        if !runtime.pending_changes.is_empty() {
            let event = CdcEvent {
                v: EVENT_VERSION,
                lsn,
                tx_id,
                committed_unix_ms,
                changes: std::mem::take(&mut runtime.pending_changes),
            };
            let changes_count = event.changes.len() as u64;
            let mut line = serde_json::to_vec(&event)?;
            line.push(b'\n');
            self.write_line(&mut runtime, &line)?;
            runtime.events_emitted += 1;
            runtime.changes_emitted += changes_count;
        }
        runtime.pending = 0;
        if lsn > runtime.last_lsn {
            runtime.last_lsn = lsn;
            write_cursor(&runtime.dir.join(CURSOR_FILE), lsn)?;
        }
        Ok(())
    }

    fn filter_change(&self, change: &WalChange, runtime: &CdcRuntime) -> Option<CdcChangeEvent> {
        if !runtime.filters.databases.is_empty()
            && !runtime
                .filters
                .databases
                .iter()
                .any(|db| db == &change.database)
        {
            return None;
        }
        if !runtime.filters.tables.is_empty()
            && !runtime.filters.tables.iter().any(|table| {
                table == &change.table || table == &format!("{}.{}", change.database, change.table)
            })
        {
            return None;
        }
        if change.operation == WalChangeOp::Ddl && !runtime.include_ddl {
            return None;
        }
        let mut before = BTreeMap::new();
        let mut after = BTreeMap::new();
        if runtime.include_row_images {
            if runtime.include_before {
                for (name, value) in &change.before {
                    before.insert(name.clone(), encode_value(value));
                }
            }
            for (name, value) in &change.after {
                after.insert(name.clone(), encode_value(value));
            }
        }
        Some(CdcChangeEvent {
            op: change.operation.as_str().to_string(),
            database: change.database.clone(),
            table: change.table.clone(),
            before,
            after,
            ddl_sql: change.ddl_sql.clone(),
        })
    }

    fn write_line(&self, runtime: &mut CdcRuntime, line: &[u8]) -> Result<()> {
        let needs_rotation = runtime
            .file
            .as_ref()
            .is_some_and(|_| runtime.file_bytes + line.len() as u64 > runtime.max_file_size);
        if needs_rotation {
            runtime.file = None;
            runtime.file_index += 1;
            runtime.files_rotated += 1;
            prune_files(&runtime.dir, &runtime.prefix, runtime.max_files)?;
        }
        if runtime.file.is_none() {
            let path = runtime
                .dir
                .join(event_file_name(&runtime.prefix, runtime.file_index));
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("open CDC event file {}", path.display()))?;
            runtime.file = Some(file);
            runtime.file_bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        }
        let file = runtime
            .file
            .as_mut()
            .ok_or_else(|| anyhow!("CDC output file is not open"))?;
        file.write_all(line)?;
        file.flush()?;
        runtime.file_bytes += line.len() as u64;
        Ok(())
    }

    /// Read persisted events newer than `since_lsn`, oldest first.
    pub fn read_changes(&self, since_lsn: u64, limit: usize) -> Result<Vec<CdcEvent>> {
        let runtime = self.runtime.lock();
        if runtime.dir.as_os_str().is_empty() || !runtime.dir.exists() {
            return Ok(Vec::new());
        }
        let mut files: Vec<PathBuf> = fs::read_dir(&runtime.dir)?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&runtime.prefix) && name.ends_with(".log"))
            })
            .collect();
        files.sort();
        let mut events = Vec::new();
        for path in files {
            let reader = BufReader::new(File::open(&path)?);
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let event: CdcEvent = serde_json::from_str(&line)?;
                if event.lsn > since_lsn {
                    events.push(event);
                    if events.len() >= limit {
                        return Ok(events);
                    }
                }
            }
        }
        events.sort_by_key(|event| event.lsn);
        Ok(events)
    }
}

/// Background read-side tailer. It only wakes on the configured poll interval
/// and skips work entirely while CDC is disabled.
pub async fn run_cdc_tailer(
    manager: Arc<CdcManager>,
    wal_dir: PathBuf,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(manager.poll_interval()) => {}
        }
        if !manager.is_enabled() {
            continue;
        }
        let scan_manager = manager.clone();
        let scan_dir = wal_dir.clone();
        let result = tokio::task::spawn_blocking(move || scan_manager.scan_once(&scan_dir)).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => manager.record_error(error),
            Err(error) => manager.record_error(anyhow!("CDC tailer task failed: {error}")),
        }
    }
    debug!("CDC tailer stopped");
}

fn encode_value(value: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(value)
}

fn parse_size(text: &str) -> u64 {
    let text = text.trim();
    let (digits, multiplier) = match text.chars().last() {
        Some('M') | Some('m') => (&text[..text.len() - 1], 1024 * 1024),
        Some('G') | Some('g') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        Some('K') | Some('k') => (&text[..text.len() - 1], 1024),
        _ => (text, 1),
    };
    digits
        .trim()
        .parse::<u64>()
        .unwrap_or(128 * 1024 * 1024)
        .saturating_mul(multiplier)
}

fn event_file_name(prefix: &str, index: u64) -> String {
    format!("{prefix}_{index:06}.log")
}

fn latest_file_index(dir: &Path, prefix: &str) -> Result<u64> {
    let mut latest = None;
    for entry in fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(index) = name
            .strip_prefix(&format!("{prefix}_"))
            .and_then(|rest| rest.strip_suffix(".log"))
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        latest = Some(latest.map_or(index, |current: u64| current.max(index)));
    }
    Ok(latest.map_or(0, |index| index + 1))
}

fn prune_files(dir: &Path, prefix: &str, max_files: u32) -> Result<()> {
    if max_files == 0 {
        return Ok(());
    }
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix) && name.ends_with(".log"))
        })
        .collect();
    files.sort();
    while files.len() > max_files as usize {
        let oldest = files.remove(0);
        fs::remove_file(oldest)?;
    }
    Ok(())
}

fn read_cursor(path: &Path) -> Result<Cursor> {
    if !path.exists() {
        return Ok(Cursor::default());
    }
    let content = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

fn write_cursor(path: &Path, last_lsn: u64) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let mut file = File::create(&temporary)?;
    serde_json::to_writer(&mut file, &Cursor { last_lsn })?;
    file.flush()?;
    drop(file);
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section() -> CdcSection {
        CdcSection {
            enabled: true,
            dir: None,
            file_prefix: "events".to_string(),
            max_file_size: "1K".to_string(),
            max_files: 3,
            poll_interval_ms: 10,
            flush_every: 1,
            include_before: true,
            include_ddl: true,
            include_row_images: true,
            filters: CdcFilters::default(),
        }
    }

    fn change(op: WalChangeOp, table: &str) -> WalChange {
        WalChange {
            operation: op,
            database: "db".to_string(),
            table: table.to_string(),
            before: vec![("id".to_string(), 1u64.to_le_bytes().to_vec())],
            after: vec![("name".to_string(), b"alice".to_vec())],
            ddl_sql: None,
        }
    }

    #[test]
    fn disabled_by_default_and_zero_output() {
        let dir = tempfile::tempdir().unwrap();
        let manager = CdcManager::new(dir.path().to_path_buf(), CdcSection::default());
        assert!(!manager.is_enabled());
        assert_eq!(manager.status().last_lsn, 0);
        assert!(!dir.path().join("cdc").exists());
    }

    #[test]
    fn emits_ndjson_and_persists_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let manager = CdcManager::new(dir.path().to_path_buf(), section());
        manager
            .append_group(7, 7, 1234, &[change(WalChangeOp::Insert, "t1")])
            .unwrap();
        let status = manager.status();
        assert_eq!(status.last_lsn, 7);
        assert_eq!(status.events_emitted, 1);
        assert_eq!(status.changes_emitted, 1);

        let events = manager.read_changes(0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].lsn, 7);
        assert_eq!(events[0].changes[0].op, "insert");
        assert_eq!(events[0].changes[0].after.get("name").unwrap(), "YWxpY2U=");

        let cursor = read_cursor(&dir.path().join("cdc").join(CURSOR_FILE)).unwrap();
        assert_eq!(cursor.last_lsn, 7);
    }

    #[test]
    fn checkpoint_persists_and_reports_lag() {
        let dir = tempfile::tempdir().unwrap();
        let manager = CdcManager::new(dir.path().to_path_buf(), section());
        manager
            .append_group(5, 5, 1, &[change(WalChangeOp::Insert, "t1")])
            .unwrap();
        assert_eq!(manager.status().lag_lsn(), 5);
        manager.set_checkpoint(5).unwrap();
        assert_eq!(manager.status().checkpoint_lsn, 5);
        assert_eq!(manager.status().lag_lsn(), 0);
        assert!(manager.set_checkpoint(6).is_err());
        assert!(manager.set_checkpoint(4).is_err());

        let reopened = CdcManager::new(dir.path().to_path_buf(), section());
        assert_eq!(reopened.status().checkpoint_lsn, 5);
        assert_eq!(reopened.set_checkpoint(5).unwrap(), 5);
    }

    #[test]
    fn hot_reload_disables_and_enables_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let manager = CdcManager::new(dir.path().to_path_buf(), section());
        assert!(manager.is_enabled());
        manager.set_enabled(false).unwrap();
        assert!(!manager.is_enabled());
        manager
            .append_group(9, 9, 1, &[change(WalChangeOp::Insert, "t1")])
            .unwrap();
        assert_eq!(manager.status().events_emitted, 0);
        manager.set_enabled(true).unwrap();
        assert!(manager.is_enabled());
    }

    #[test]
    fn flush_every_batches_changes_without_loss() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = section();
        config.flush_every = 2;
        let manager = CdcManager::new(dir.path().to_path_buf(), config);
        manager
            .append_group(1, 1, 1, &[change(WalChangeOp::Insert, "t1")])
            .unwrap();
        manager
            .append_group(2, 2, 2, &[change(WalChangeOp::Delete, "t1")])
            .unwrap();
        let events = manager.read_changes(0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].lsn, 2);
        assert_eq!(events[0].changes.len(), 2);
        assert_eq!(manager.status().last_lsn, 2);
    }

    #[test]
    fn filters_and_flush_every_are_applied() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = section();
        config.flush_every = 2;
        config.filters.tables = vec!["db.keep".to_string()];
        let manager = CdcManager::new(dir.path().to_path_buf(), config);
        manager
            .append_group(1, 1, 1, &[change(WalChangeOp::Insert, "drop")])
            .unwrap();
        assert_eq!(manager.status().events_emitted, 0);
        manager
            .append_group(2, 2, 1, &[change(WalChangeOp::Insert, "keep")])
            .unwrap();
        let events = manager.read_changes(0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].changes[0].table, "keep");
    }
}
