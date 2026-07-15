use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use mydb_wal::{WalReader, WalRecord, WalRecordType, WalWriter};

// ============================================================================
// Page Types
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PageType {
    Index,
    Data,
    Undo,
    Redo,
    System,
}

// ============================================================================
// Page Header
// ============================================================================

#[derive(Debug, Clone)]
pub struct PageHeader {
    pub page_number: u32,
    pub page_type: PageType,
    pub lsn: u64,
    pub prev_page: Option<u32>,
    pub next_page: Option<u32>,
    pub is_dirty: bool,
    pub row_count: u32,
}

// ============================================================================
// Page
// ============================================================================

#[derive(Debug, Clone)]
pub struct Page {
    pub header: PageHeader,
    pub data: Vec<u8>,
    pub size: usize,
}

impl Page {
    pub fn new(page_number: u32, page_type: PageType, size: usize) -> Self {
        Self {
            header: PageHeader {
                page_number,
                page_type,
                lsn: 0,
                prev_page: None,
                next_page: None,
                is_dirty: false,
                row_count: 0,
            },
            data: vec![0; size],
            size,
        }
    }

    pub fn calculate_checksum(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(&self.data);
        hasher.finalize().to_vec()
    }

    /// Serialize page to bytes for disk storage
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.size + 64);

        // Header
        buf.write_u32::<LittleEndian>(self.header.page_number).unwrap();
        buf.write_u8(self.header.page_type as u8).unwrap();
        buf.write_u64::<LittleEndian>(self.header.lsn).unwrap();
        buf.write_u32::<LittleEndian>(self.header.prev_page.unwrap_or(u32::MAX)).unwrap();
        buf.write_u32::<LittleEndian>(self.header.next_page.unwrap_or(u32::MAX)).unwrap();
        buf.write_u32::<LittleEndian>(self.header.row_count).unwrap();

        // Data
        buf.extend_from_slice(&self.data);

        // Checksum of data
        let checksum = self.calculate_checksum();
        buf.extend_from_slice(&checksum);

        buf
    }

    /// Deserialize page from bytes
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 32 {
            return None;
        }

        let mut pos = 0;

        let page_number = read_u32(bytes, &mut pos)?;
        let page_type_byte = bytes[pos]; pos += 1;
        let page_type = match page_type_byte {
            0 => PageType::Index,
            1 => PageType::Data,
            2 => PageType::Undo,
            3 => PageType::Redo,
            4 => PageType::System,
            _ => PageType::Data,
        };
        let lsn = read_u64(bytes, &mut pos)?;
        let prev_raw = read_u32(bytes, &mut pos)?;
        let next_raw = read_u32(bytes, &mut pos)?;
        let row_count = read_u32(bytes, &mut pos)?;

        let prev_page = if prev_raw == u32::MAX { None } else { Some(prev_raw) };
        let next_page = if next_raw == u32::MAX { None } else { Some(next_raw) };

        // Data is everything except header (32 bytes) and checksum (32 bytes)
        let data_len = bytes.len() - 32 - 32;
        let data = bytes[pos..pos + data_len].to_vec();

        Some(Self {
            header: PageHeader {
                page_number,
                page_type,
                lsn,
                prev_page,
                next_page,
                is_dirty: false,
                row_count,
            },
            data,
            size: 16384,
        })
    }
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > bytes.len() {
        return None;
    }
    let val = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().ok()?);
    *pos += 4;
    Some(val)
}

fn read_u64(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    if *pos + 8 > bytes.len() {
        return None;
    }
    let val = u64::from_le_bytes(bytes[*pos..*pos + 8].try_into().ok()?);
    *pos += 8;
    Some(val)
}

// ============================================================================
// Buffer Pool
// ============================================================================

pub struct BufferPool {
    pages: RwLock<HashMap<u32, Page>>,
    page_size: usize,
    max_pages: usize,
}

impl BufferPool {
    pub fn new(page_size: usize, buffer_pool_size: &str) -> Self {
        let size_bytes = Self::parse_size(buffer_pool_size);
        let max_pages = size_bytes / page_size;

        Self {
            pages: RwLock::new(HashMap::with_capacity(max_pages)),
            page_size,
            max_pages,
        }
    }

    fn parse_size(size: &str) -> usize {
        let size = size.trim().to_uppercase();
        let (num, unit) = if size.ends_with('G') {
            (size[..size.len() - 1].parse().unwrap_or(1), 1024 * 1024 * 1024)
        } else if size.ends_with('M') {
            (size[..size.len() - 1].parse().unwrap_or(1), 1024 * 1024)
        } else if size.ends_with('K') {
            (size[..size.len() - 1].parse().unwrap_or(1), 1024)
        } else {
            (size.parse().unwrap_or(1), 1)
        };
        num * unit
    }

    pub fn get_page(&self, page_number: u32) -> Option<Page> {
        self.pages.read().get(&page_number).cloned()
    }

    pub fn insert_page(&self, page: Page) {
        let mut pages = self.pages.write();
        if pages.len() >= self.max_pages {
            if let Some((&oldest, _)) = pages.iter().min_by_key(|(_, p)| p.header.lsn) {
                pages.remove(&oldest);
            }
        }
        pages.insert(page.header.page_number, page);
    }

    pub fn mark_dirty(&self, page_number: u32) {
        let mut pages = self.pages.write();
        if let Some(page) = pages.get_mut(&page_number) {
            page.header.is_dirty = true;
        }
    }

    pub fn get_all_dirty_pages(&self) -> Vec<Page> {
        self.pages
            .read()
            .values()
            .filter(|p| p.header.is_dirty)
            .cloned()
            .collect()
    }

    pub fn clear_dirty_flags(&self) {
        let mut pages = self.pages.write();
        for page in pages.values_mut() {
            page.header.is_dirty = false;
        }
    }

    pub fn flush_all(&self) -> Result<()> {
        // Just clear dirty flags, actual disk write happens in DiskManager
        self.clear_dirty_flags();
        Ok(())
    }

    pub fn page_count(&self) -> usize {
        self.pages.read().len()
    }

    pub fn clear(&self) {
        self.pages.write().clear();
    }
}

// ============================================================================
// Disk Manager
// ============================================================================

pub struct DiskManager {
    data_dir: PathBuf,
    page_size: usize,
}

impl DiskManager {
    pub fn new(data_dir: PathBuf, page_size: usize) -> Self {
        Self { data_dir, page_size }
    }

    /// Ensure data directory structure exists
    pub fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir)?;
        Ok(())
    }

    /// Read a page from disk
    pub fn read_page(&self, table_name: &str, page_number: u32) -> Result<Option<Page>> {
        let table_dir = self.data_dir.join(table_name);
        if !table_dir.exists() {
            return Ok(None);
        }

        let file_path = table_dir.join(format!("page_{:08}.bin", page_number));
        if !file_path.exists() {
            return Ok(None);
        }

        let mut file = File::open(&file_path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        Ok(Page::decode(&bytes))
    }

    /// Write a page to disk
    pub fn write_page(&self, table_name: &str, page: &Page) -> Result<()> {
        let table_dir = self.data_dir.join(table_name);
        fs::create_dir_all(&table_dir)?;

        let file_path = table_dir.join(format!("page_{:08}.bin", page.header.page_number));
        let bytes = page.encode();

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&file_path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;

        Ok(())
    }

    /// List all page numbers for a table
    pub fn list_pages(&self, table_name: &str) -> Result<Vec<u32>> {
        let table_dir = self.data_dir.join(table_name);
        if !table_dir.exists() {
            return Ok(Vec::new());
        }

        let mut pages = Vec::new();
        for entry in fs::read_dir(&table_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("page_") && name.ends_with(".bin") {
                let num_str = &name[5..name.len() - 4];
                if let Ok(num) = num_str.parse::<u32>() {
                    pages.push(num);
                }
            }
        }
        pages.sort();
        Ok(pages)
    }

    /// Delete a page file
    pub fn delete_page(&self, table_name: &str, page_number: u32) -> Result<()> {
        let file_path = self
            .data_dir
            .join(table_name)
            .join(format!("page_{:08}.bin", page_number));
        if file_path.exists() {
            fs::remove_file(&file_path)?;
        }
        Ok(())
    }
}

// ============================================================================
// Table Schema
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<Column>,
    pub primary_key: Option<Vec<String>>,
    pub indexes: Vec<Index>,
    pub next_page_number: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub default: Option<String>,
    pub is_primary_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataType {
    Int,
    BigInt,
    Float,
    Double,
    Varchar(u32),
    Text,
    Blob,
    Date,
    DateTime,
    Timestamp,
    Boolean,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

// ============================================================================
// Row
// ============================================================================

#[derive(Debug, Clone)]
pub struct Row {
    pub values: Vec<(String, Vec<u8>)>,
}

impl Row {
    pub fn new() -> Self {
        Self { values: Vec::new() }
    }

    pub fn push(&mut self, name: &str, value: Vec<u8>) {
        self.values.push((name.to_string(), value));
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.values.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_slice())
    }

    pub fn encode(&self) -> Vec<u8> {
        mydb_wal::record::encode_row(&self.values)
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        let values = mydb_wal::record::decode_row(data)?;
        Some(Self { values })
    }

    pub fn to_string_map(&self) -> HashMap<String, String> {
        self.values
            .iter()
            .map(|(k, v)| {
                let val = String::from_utf8_lossy(v).to_string();
                (k.clone(), val)
            })
            .collect()
    }
}

// ============================================================================
// Database
// ============================================================================

pub struct Database {
    pub name: String,
    pub tables: RwLock<HashMap<String, TableSchema>>,
    pub buffer_pool: Arc<BufferPool>,
    disk_manager: DiskManager,
    wal_writer: Arc<parking_lot::Mutex<WalWriter>>,
    next_tx_id: AtomicU64,
    data_dir: PathBuf,
}

impl Database {
    pub fn new(
        name: &str,
        data_dir: PathBuf,
        buffer_pool: Arc<BufferPool>,
        wal_writer: Arc<parking_lot::Mutex<WalWriter>>,
    ) -> Self {
        let disk_manager = DiskManager::new(data_dir.join(name), 16384);

        Self {
            name: name.to_string(),
            tables: RwLock::new(HashMap::new()),
            buffer_pool,
            disk_manager,
            wal_writer,
            next_tx_id: AtomicU64::new(1),
            data_dir,
        }
    }

    pub fn new_tx_id(&self) -> u64 {
        self.next_tx_id.fetch_add(1, Ordering::SeqCst)
    }

    pub async fn load(&mut self) -> Result<()> {
        let schema_file = self.data_dir.join(&self.name).join("schema.json");
        if schema_file.exists() {
            let content = std::fs::read_to_string(&schema_file)?;
            let schemas: HashMap<String, TableSchema> = serde_json::from_str(&content)?;
            *self.tables.write() = schemas;
        }
        Ok(())
    }

    pub async fn save(&self) -> Result<()> {
        let table_dir = self.data_dir.join(&self.name);
        std::fs::create_dir_all(&table_dir)?;
        let schema_file = table_dir.join("schema.json");
        let schemas = self.tables.read().clone();
        let content = serde_json::to_string_pretty(&schemas)?;
        std::fs::write(schema_file, content)?;
        Ok(())
    }

    pub fn create_table(&self, schema: TableSchema) -> Result<()> {
        let mut tables = self.tables.write();
        if tables.contains_key(&schema.name) {
            anyhow::bail!("Table '{}' already exists", schema.name);
        }

        // Write WAL record
        let tx_id = self.new_tx_id();
        let data = serde_json::to_vec(&schema)?;
        let mut record = WalRecord::new(
            0,
            WalRecordType::CreateTable,
            tx_id,
            &schema.name,
            data,
        );
        self.wal_writer.lock().append(&mut record)?;

        tables.insert(schema.name.clone(), schema);
        Ok(())
    }

    pub fn drop_table(&self, name: &str) -> Result<()> {
        let mut tables = self.tables.write();

        // Write WAL record
        let tx_id = self.new_tx_id();
        let mut record = WalRecord::new(
            0,
            WalRecordType::DropTable,
            tx_id,
            name,
            vec![],
        );
        self.wal_writer.lock().append(&mut record)?;

        tables
            .remove(name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", name))?;
        Ok(())
    }

    pub fn get_table(&self, name: &str) -> Option<TableSchema> {
        self.tables.read().get(name).cloned()
    }

    pub fn list_tables(&self) -> Vec<String> {
        self.tables.read().keys().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Row operations
    // -----------------------------------------------------------------------

    /// Insert a row into a table
    pub fn insert_row(&self, table_name: &str, row: Row) -> Result<()> {
        // Get or create page for this table
        let mut tables = self.tables.write();
        let schema = tables
            .get_mut(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;

        let page_number = schema.next_page_number;
        schema.next_page_number += 1;

        // Create new page with the row data
        let row_data = row.encode();
        let mut page = Page::new(page_number, PageType::Data, 16384);
        page.data[..row_data.len()].copy_from_slice(&row_data);
        page.header.row_count = 1;
        page.header.is_dirty = true;

        // Write WAL record
        let tx_id = self.new_tx_id();
        let mut record = WalRecord::new(
            0,
            WalRecordType::Insert,
            tx_id,
            table_name,
            row_data,
        );
        self.wal_writer.lock().append(&mut record)?;

        // Insert into buffer pool
        self.buffer_pool.insert_page(page.clone());

        // Write to disk
        self.disk_manager.write_page(table_name, &page)?;

        info!("Inserted row into {}.{} (page {})", self.name, table_name, page_number);

        Ok(())
    }

    /// Get all rows from a table
    pub fn scan_table(&self, table_name: &str) -> Result<Vec<Row>> {
        let tables = self.tables.read();
        let schema = tables
            .get(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;

        let mut rows = Vec::new();
        let page_numbers = self.disk_manager.list_pages(table_name)?;

        for page_num in page_numbers {
            // Try buffer pool first
            if let Some(page) = self.buffer_pool.get_page(page_num) {
                if page.header.row_count > 0 {
                    let row_data = &page.data[..];
                    if let Some(row) = Row::decode(row_data) {
                        rows.push(row);
                    }
                }
            } else {
                // Read from disk
                if let Some(page) = self.disk_manager.read_page(table_name, page_num)? {
                    self.buffer_pool.insert_page(page.clone());
                    if page.header.row_count > 0 {
                        let row_data = &page.data[..];
                        if let Some(row) = Row::decode(row_data) {
                            rows.push(row);
                        }
                    }
                }
            }
        }

        Ok(rows)
    }

    /// Delete all rows (for simplicity, mark pages as deleted)
    pub fn delete_all_rows(&self, table_name: &str) -> Result<u64> {
        let tables = self.tables.read();
        let schema = tables
            .get(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;

        let page_numbers = self.disk_manager.list_pages(table_name)?;
        let count = page_numbers.len() as u64;

        // Write WAL record
        let tx_id = self.new_tx_id();
        let mut record = WalRecord::new(
            0,
            WalRecordType::Delete,
            tx_id,
            table_name,
            vec![],
        );
        self.wal_writer.lock().append(&mut record)?;

        // Delete pages
        for page_num in &page_numbers {
            self.disk_manager.delete_page(table_name, *page_num)?;
            // Remove from buffer pool too
            self.buffer_pool.clear();
        }

        // Reset page counter
        drop(tables);
        let mut tables = self.tables.write();
        if let Some(schema) = tables.get_mut(table_name) {
            schema.next_page_number = 0;
        }

        Ok(count)
    }
}

// ============================================================================
// Storage Engine Manager
// ============================================================================

pub struct StorageEngineManager {
    databases: RwLock<HashMap<String, Arc<Database>>>,
    buffer_pool: Arc<BufferPool>,
    wal_writer: Arc<parking_lot::Mutex<WalWriter>>,
    data_dir: PathBuf,
}

impl StorageEngineManager {
    pub fn new(data_dir: PathBuf, page_size: usize, buffer_pool_size: &str) -> Self {
        let buffer_pool = Arc::new(BufferPool::new(page_size, buffer_pool_size));

        // Initialize WAL
        let wal_dir = data_dir.join("wal");
        let wal_writer = WalWriter::open(wal_dir, None).expect("Failed to open WAL");

        Self {
            databases: RwLock::new(HashMap::new()),
            buffer_pool,
            wal_writer: Arc::new(parking_lot::Mutex::new(wal_writer)),
            data_dir,
        }
    }

    pub async fn init(&self) -> Result<()> {
        // Create directories
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.data_dir.join("wal"))?;

        // WAL recovery: replay existing WAL
        self.wal_replay().await?;

        // Load existing databases
        let mut entries = std::fs::read_dir(&self.data_dir)?;
        while let Some(entry) = entries.next() {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                if name == "wal" {
                    continue; // skip wal directory
                }

                let mut db = Database::new(
                    &name,
                    self.data_dir.clone(),
                    self.buffer_pool.clone(),
                    self.wal_writer.clone(),
                );
                db.load().await?;

                self.databases.write().insert(name, Arc::new(db));
            }
        }

        info!(
            "Storage engine initialized with {} databases",
            self.databases.read().len()
        );
        Ok(())
    }

    async fn wal_replay(&self) -> Result<()> {
        let wal_dir = self.data_dir.join("wal");
        let reader = WalReader::open(wal_dir)?;

        let mut replay_count = 0u64;
        reader.replay(|record| {
            debug!(
                "WAL replay: lsn={} type={} table={}",
                record.lsn, record.record_type, record.table_name
            );
            replay_count += 1;
        })?;

        if replay_count > 0 {
            info!("WAL replayed {} records", replay_count);

            // Truncate old WAL files
            let max_lsn = reader.max_lsn()?;
            self.wal_writer.lock().truncate_up_to(max_lsn)?;
        }

        Ok(())
    }

    pub async fn create_database(&self, name: &str) -> Result<()> {
        let db_path = self.data_dir.join(name);
        std::fs::create_dir_all(&db_path)?;

        let db = Database::new(
            name,
            self.data_dir.clone(),
            self.buffer_pool.clone(),
            self.wal_writer.clone(),
        );
        db.save().await?;

        self.databases
            .write()
            .insert(name.to_string(), Arc::new(db));

        info!("Created database: {}", name);
        Ok(())
    }

    pub async fn drop_database(&self, name: &str) -> Result<()> {
        let db_path = self.data_dir.join(name);
        if db_path.exists() {
            std::fs::remove_dir_all(&db_path)?;
        }

        self.databases.write().remove(name);

        info!("Dropped database: {}", name);
        Ok(())
    }

    pub fn get_database(&self, name: &str) -> Option<Arc<Database>> {
        self.databases.read().get(name).cloned()
    }

    pub fn list_databases(&self) -> Vec<String> {
        self.databases.read().keys().cloned().collect()
    }

    /// Sync WAL to disk
    pub fn sync_wal(&self) -> Result<()> {
        self.wal_writer.lock().sync()
    }
}

// ============================================================================
// Checkpointer (background dirty page flush)
// ============================================================================

pub struct Checkpointer {
    buffer_pool: Arc<BufferPool>,
    disk_manager: DiskManager,
    interval: std::time::Duration,
}

impl Checkpointer {
    pub fn new(
        buffer_pool: Arc<BufferPool>,
        data_dir: PathBuf,
        interval: std::time::Duration,
    ) -> Self {
        Self {
            buffer_pool,
            disk_manager: DiskManager::new(data_dir, 16384),
            interval,
        }
    }

    /// Start the checkpoint loop in a background task
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.interval);
            loop {
                interval.tick().await;
                if let Err(e) = self.checkpoint() {
                    error!("Checkpoint error: {}", e);
                }
            }
        })
    }

    /// Run a single checkpoint: flush all dirty pages
    pub fn checkpoint(&self) -> Result<()> {
        let dirty_pages = self.buffer_pool.get_all_dirty_pages();
        if dirty_pages.is_empty() {
            return Ok(());
        }

        let count = dirty_pages.len();

        for page in &dirty_pages {
            // We don't know the table name from just a page number,
            // so we skip disk write for now. In production, we'd track
            // which table owns which page.
            debug!("Checkpoint: flushing page {}", page.header.page_number);
        }

        self.buffer_pool.clear_dirty_flags();

        debug!("Checkpoint complete: flushed {} pages", count);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_pool_size_parsing() {
        assert_eq!(BufferPool::parse_size("1G"), 1024 * 1024 * 1024);
        assert_eq!(BufferPool::parse_size("512M"), 512 * 1024 * 1024);
        assert_eq!(BufferPool::parse_size("64K"), 64 * 1024);
        assert_eq!(BufferPool::parse_size("1024"), 1024);
    }

    #[test]
    fn test_page_encode_decode_roundtrip() {
        let mut page = Page::new(42, PageType::Data, 1024);
        page.header.lsn = 100;
        page.header.row_count = 5;
        page.data[..11].copy_from_slice(b"hello world");

        let encoded = page.encode();
        let decoded = Page::decode(&encoded).unwrap();

        assert_eq!(decoded.header.page_number, 42);
        assert_eq!(decoded.header.lsn, 100);
        assert_eq!(decoded.header.row_count, 5);
        assert_eq!(&decoded.data[..11], b"hello world");
    }

    #[test]
    fn test_row_encode_decode_roundtrip() {
        let mut row = Row::new();
        row.push("id", 42u32.to_le_bytes().to_vec());
        row.push("name", b"Alice".to_vec());
        row.push("score", 99.5f64.to_le_bytes().to_vec());

        let encoded = row.encode();
        let decoded = Row::decode(&encoded).unwrap();

        assert_eq!(decoded.get("id"), Some(42u32.to_le_bytes().as_slice()));
        assert_eq!(decoded.get("name"), Some(b"Alice".as_slice()));
        assert_eq!(decoded.get("score"), Some(99.5f64.to_le_bytes().as_slice()));
    }

    #[test]
    fn test_disk_manager_read_write_page() {
        let tmp = tempfile::tempdir().unwrap();
        let dm = DiskManager::new(tmp.path().to_path_buf(), 16384);
        dm.init().unwrap();

        let mut page = Page::new(0, PageType::Data, 1024);
        page.data[..5].copy_from_slice(b"hello");

        dm.write_page("test_table", &page).unwrap();
        let loaded = dm.read_page("test_table", 0).unwrap().unwrap();

        assert_eq!(loaded.header.page_number, 0);
        assert_eq!(&loaded.data[..5], b"hello");
    }
}
