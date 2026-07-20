use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use byteorder::{LittleEndian, WriteBytesExt};
use chrono::{Local, Timelike};
use parking_lot::RwLock;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

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

const PAGE_HEADER_SIZE: usize = 25;
const PAGE_CHECKSUM_SIZE: usize = 32;
const PAGE_FORMAT_HEADER_CHECKSUM: u8 = 0x80;

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

    fn encoded_header(&self) -> Vec<u8> {
        let mut header = Vec::with_capacity(PAGE_HEADER_SIZE);
        header
            .write_u32::<LittleEndian>(self.header.page_number)
            .unwrap();
        header.write_u8(self.header.page_type as u8).unwrap();
        header.write_u64::<LittleEndian>(self.header.lsn).unwrap();
        header
            .write_u32::<LittleEndian>(self.header.prev_page.unwrap_or(u32::MAX))
            .unwrap();
        header
            .write_u32::<LittleEndian>(self.header.next_page.unwrap_or(u32::MAX))
            .unwrap();
        header
            .write_u32::<LittleEndian>(self.header.row_count)
            .unwrap();
        debug_assert_eq!(header.len(), PAGE_HEADER_SIZE);
        header
    }

    pub fn calculate_checksum(&self) -> Vec<u8> {
        let header = self.encoded_header();
        page_checksum(&header, &self.data, false)
    }

    /// Serialize page to bytes for disk storage.
    ///
    /// New pages mark the type byte and checksum metadata plus row bytes. Old
    /// pages used a checksum of row bytes alone and remain readable.
    pub fn encode(&self) -> Vec<u8> {
        let header = self.encoded_header();
        let mut buf = Vec::with_capacity(self.size + PAGE_HEADER_SIZE + PAGE_CHECKSUM_SIZE);
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&self.data);
        buf.extend_from_slice(&page_checksum(&header, &self.data, false));
        buf
    }

    /// Deserialize page from bytes
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < PAGE_HEADER_SIZE + PAGE_CHECKSUM_SIZE {
            return None;
        }

        let mut pos = 0;

        let page_number = read_u32(bytes, &mut pos)?;
        let page_type_byte = bytes[pos];
        pos += 1;
        let header_checksum = page_type_byte & PAGE_FORMAT_HEADER_CHECKSUM != 0;
        let page_type = match page_type_byte & !PAGE_FORMAT_HEADER_CHECKSUM {
            0 => PageType::Index,
            1 => PageType::Data,
            2 => PageType::Undo,
            3 => PageType::Redo,
            4 => PageType::System,
            _ => return None,
        };
        let lsn = read_u64(bytes, &mut pos)?;
        let prev_raw = read_u32(bytes, &mut pos)?;
        let next_raw = read_u32(bytes, &mut pos)?;
        let row_count = read_u32(bytes, &mut pos)?;

        let prev_page = if prev_raw == u32::MAX {
            None
        } else {
            Some(prev_raw)
        };
        let next_page = if next_raw == u32::MAX {
            None
        } else {
            Some(next_raw)
        };

        let data_len = bytes.len() - PAGE_HEADER_SIZE - PAGE_CHECKSUM_SIZE;
        let data = bytes[pos..pos + data_len].to_vec();
        let expected = &bytes[pos + data_len..];
        if page_checksum(&bytes[..PAGE_HEADER_SIZE], &data, header_checksum).as_slice() != expected
        {
            return None;
        }

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
            size: data_len,
        })
    }
}

fn page_checksum(header: &[u8], data: &[u8], include_header: bool) -> Vec<u8> {
    let mut hasher = Sha256::new();
    if include_header {
        hasher.update(header);
    }
    hasher.update(data);
    hasher.finalize().to_vec()
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
    pages: RwLock<HashMap<(String, u32), Page>>,
    max_pages: usize,
}

impl BufferPool {
    pub fn new(page_size: usize, buffer_pool_size: &str) -> Self {
        let size_bytes = Self::parse_size(buffer_pool_size);
        let max_pages = size_bytes / page_size;

        Self {
            pages: RwLock::new(HashMap::with_capacity(max_pages)),
            max_pages,
        }
    }

    fn parse_size(size: &str) -> usize {
        let size = size.trim().to_uppercase();
        let (num, unit) = if size.ends_with('G') {
            (
                size[..size.len() - 1].parse().unwrap_or(1),
                1024 * 1024 * 1024,
            )
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
        self.get_table_page("", page_number)
    }

    pub fn insert_page(&self, page: Page) {
        self.insert_table_page("", page);
    }

    pub fn get_table_page(&self, namespace: &str, page_number: u32) -> Option<Page> {
        self.pages
            .read()
            .get(&(namespace.to_string(), page_number))
            .cloned()
    }

    pub fn insert_table_page(&self, namespace: &str, page: Page) {
        let mut pages = self.pages.write();
        if pages.len() >= self.max_pages {
            if let Some((oldest, _)) = pages.iter().min_by_key(|(_, page)| page.header.lsn) {
                let oldest = oldest.clone();
                pages.remove(&oldest);
            }
        }
        pages.insert((namespace.to_string(), page.header.page_number), page);
    }

    pub fn mark_dirty(&self, page_number: u32) {
        let mut pages = self.pages.write();
        if let Some(page) = pages.get_mut(&(String::new(), page_number)) {
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
        Self {
            data_dir,
            page_size,
        }
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

        let segment_path = table_dir.join("pages.dat");
        if segment_path.exists() {
            let mut file = File::open(segment_path)?;
            let record_size = self.page_size + PAGE_HEADER_SIZE + PAGE_CHECKSUM_SIZE;
            let offset = u64::from(page_number) * record_size as u64;
            let length = file.metadata()?.len();
            if length % record_size as u64 != 0 {
                anyhow::bail!("Page segment for table '{}' is truncated", table_name);
            }
            if length >= offset + record_size as u64 {
                file.seek(SeekFrom::Start(offset))?;
                let mut bytes = vec![0; record_size];
                file.read_exact(&mut bytes)?;
                if bytes.iter().all(|byte| *byte == 0) {
                    return Ok(None);
                }
                let page = Page::decode(&bytes).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Page corruption in table '{}' page {} (checksum or format invalid)",
                        table_name,
                        page_number
                    )
                })?;
                if page.header.page_number != page_number {
                    anyhow::bail!(
                        "Page corruption in table '{}' page {} (stored page number {})",
                        table_name,
                        page_number,
                        page.header.page_number
                    );
                }
                return Ok(Some(page));
            }
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
        self.write_pages(table_name, std::slice::from_ref(page))
    }

    /// Write a page batch into one segment and issue one durable flush.
    pub fn write_pages(&self, table_name: &str, pages: &[Page]) -> Result<()> {
        self.write_pages_inner(table_name, pages, true)
    }

    fn write_pages_buffered(&self, table_name: &str, pages: &[Page]) -> Result<()> {
        self.write_pages_inner(table_name, pages, false)
    }

    fn write_pages_inner(&self, table_name: &str, pages: &[Page], sync: bool) -> Result<()> {
        if pages.is_empty() {
            fs::create_dir_all(self.data_dir.join(table_name))?;
            return Ok(());
        }
        let table_dir = self.data_dir.join(table_name);
        fs::create_dir_all(&table_dir)?;
        let file_path = table_dir.join("pages.dat");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&file_path)?;
        let record_size = self.page_size + PAGE_HEADER_SIZE + PAGE_CHECKSUM_SIZE;
        for page in pages {
            let bytes = page.encode();
            if bytes.len() != record_size {
                anyhow::bail!("Page {} has invalid encoded size", page.header.page_number);
            }
            file.seek(SeekFrom::Start(
                u64::from(page.header.page_number) * record_size as u64,
            ))?;
            file.write_all(&bytes)?;
        }
        if sync {
            file.sync_data()?;
        }

        Ok(())
    }

    fn sync_table(&self, table_name: &str) -> Result<()> {
        let path = self.data_dir.join(table_name).join("pages.dat");
        if path.exists() {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?
                .sync_data()?;
        }
        Ok(())
    }

    /// List all page numbers for a table
    pub fn list_pages(&self, table_name: &str) -> Result<Vec<u32>> {
        let table_dir = self.data_dir.join(table_name);
        if !table_dir.exists() {
            return Ok(Vec::new());
        }

        let segment_path = table_dir.join("pages.dat");
        if segment_path.exists() {
            let record_size = self.page_size + PAGE_HEADER_SIZE + PAGE_CHECKSUM_SIZE;
            let length = fs::metadata(segment_path)?.len();
            if length % record_size as u64 != 0 {
                anyhow::bail!("Page segment for table '{}' is truncated", table_name);
            }
            let count = length / record_size as u64;
            return Ok((0..count as u32).collect());
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
        let segment_path = self.data_dir.join(table_name).join("pages.dat");
        if segment_path.exists() {
            let record_size = self.page_size + PAGE_HEADER_SIZE + PAGE_CHECKSUM_SIZE;
            let mut file = OpenOptions::new().write(true).open(segment_path)?;
            file.seek(SeekFrom::Start(u64::from(page_number) * record_size as u64))?;
            file.write_all(&vec![0; record_size])?;
            file.sync_data()?;
            return Ok(());
        }
        let file_path = self
            .data_dir
            .join(table_name)
            .join(format!("page_{:08}.bin", page_number));
        if file_path.exists() {
            fs::remove_file(&file_path)?;
        }
        Ok(())
    }

    pub fn delete_table(&self, table_name: &str) -> Result<()> {
        let table_dir = self.data_dir.join(table_name);
        if table_dir.exists() {
            fs::remove_dir_all(table_dir)?;
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
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub create_sql: Option<String>,
    #[serde(default)]
    pub engine: TableEngine,
    // Appended after every legacy field so older bincode WAL payloads keep
    // their original field order. Missing values deserialize as no triggers.
    #[serde(default)]
    pub triggers: Vec<TriggerDefinition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TableEngine {
    #[default]
    Neko233,
    Memory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TriggerTiming {
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerDefinition {
    pub name: String,
    pub timing: TriggerTiming,
    pub event: TriggerEvent,
    pub body: String,
    pub definer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcedureParameterMode {
    In,
    Out,
    InOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureParameter {
    pub name: String,
    pub mode: ProcedureParameterMode,
    pub data_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureDefinition {
    pub name: String,
    pub parameters: Vec<ProcedureParameter>,
    pub body: String,
    pub definer: String,
    pub create_sql: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureMetadata {
    pub created: String,
    pub last_altered: String,
    pub sql_mode: String,
}

impl ProcedureMetadata {
    pub fn new(sql_mode: String) -> Self {
        let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        Self {
            created: now.clone(),
            last_altered: now,
            sql_mode,
        }
    }

    pub fn altered(&self, sql_mode: String) -> Self {
        Self {
            created: self.created.clone(),
            last_altered: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            sql_mode,
        }
    }
}

pub fn table_engine_from_sql(sql: &str) -> TableEngine {
    let options = sql
        .rfind(')')
        .map(|index| &sql[index + 1..])
        .unwrap_or(sql)
        .replace('=', " ");
    let tokens = options.split_whitespace().collect::<Vec<_>>();
    if tokens.windows(2).any(|pair| {
        pair[0].eq_ignore_ascii_case("ENGINE") && pair[1].eq_ignore_ascii_case("MEMORY")
    }) {
        TableEngine::Memory
    } else {
        TableEngine::Neko233
    }
}

/// MySQL's MEMORY engine keeps table metadata but loses every row on restart.
/// InnoDB and Neko233 are aliases for the durable Neko233 storage path.
pub fn is_memory_schema(schema: &TableSchema) -> bool {
    schema.engine == TableEngine::Memory
        || schema
            .create_sql
            .as_deref()
            .is_some_and(|sql| table_engine_from_sql(sql) == TableEngine::Memory)
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
    Raw(String),
}

pub fn is_current_timestamp_default(value: &str) -> bool {
    let upper = value.trim().to_ascii_uppercase();
    upper == "CURRENT_TIMESTAMP"
        || upper == "CURRENT_TIMESTAMP()"
        || upper == "NOW()"
        || ["CURRENT_TIMESTAMP(", "NOW("].into_iter().any(|prefix| {
            upper
                .strip_prefix(prefix)
                .and_then(|value| value.strip_suffix(')'))
                .is_some_and(|value| value.trim().parse::<u32>().is_ok_and(|fsp| fsp <= 6))
        })
}

fn current_timestamp_default(value: &str) -> Option<Vec<u8>> {
    if !is_current_timestamp_default(value) {
        return None;
    }
    let upper = value.trim().to_ascii_uppercase();
    let fsp = upper
        .strip_prefix("CURRENT_TIMESTAMP(")
        .or_else(|| upper.strip_prefix("NOW("))
        .and_then(|value| value.strip_suffix(')'))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0)
        .min(6);
    let now = Local::now();
    let mut output = now.format("%Y-%m-%d %H:%M:%S").to_string();
    if fsp > 0 {
        let fraction = format!("{:06}", now.nanosecond() / 1_000);
        output.push('.');
        output.push_str(&fraction[..fsp]);
    }
    Some(output.into_bytes())
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub values: Vec<(String, Vec<u8>)>,
    null_columns: HashSet<String>,
}

impl Row {
    pub fn new() -> Self {
        Self {
            values: Vec::new(),
            null_columns: HashSet::new(),
        }
    }

    pub fn push(&mut self, name: &str, value: Vec<u8>) {
        self.null_columns
            .retain(|column| !column.eq_ignore_ascii_case(name));
        self.values.push((name.to_string(), value));
    }

    pub fn push_null(&mut self, name: &str) {
        self.values.push((name.to_string(), Vec::new()));
        self.null_columns.insert(name.to_string());
    }

    pub fn contains(&self, name: &str) -> bool {
        self.values
            .iter()
            .any(|(column, _)| column.eq_ignore_ascii_case(name))
    }

    pub fn is_null(&self, name: &str) -> bool {
        self.null_columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(name))
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.values
            .iter()
            .find(|(column, _)| column.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }

    pub fn encode(&self) -> Vec<u8> {
        mydb_wal::record::encode_nullable_row(&self.values, &self.null_columns)
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        let (values, null_columns) = mydb_wal::record::decode_nullable_row(data)?;
        Some(Self {
            values,
            null_columns,
        })
    }

    pub fn to_string_map(&self) -> HashMap<String, String> {
        self.values
            .iter()
            .map(|(k, v)| {
                let val = if self.is_null(k) {
                    "NULL".to_string()
                } else {
                    String::from_utf8_lossy(v).to_string()
                };
                (k.clone(), val)
            })
            .collect()
    }

    pub fn set(&mut self, name: &str, value: Vec<u8>) {
        self.null_columns
            .retain(|column| !column.eq_ignore_ascii_case(name));
        if let Some((_, current)) = self
            .values
            .iter_mut()
            .find(|(column, _)| column.eq_ignore_ascii_case(name))
        {
            *current = value;
        } else {
            self.push(name, value);
        }
    }

    pub fn set_null(&mut self, name: &str) {
        let stored_name = if let Some((column, current)) = self
            .values
            .iter_mut()
            .find(|(column, _)| column.eq_ignore_ascii_case(name))
        {
            current.clear();
            column.clone()
        } else {
            self.values.push((name.to_string(), Vec::new()));
            name.to_string()
        };
        self.null_columns
            .retain(|column| !column.eq_ignore_ascii_case(name));
        self.null_columns.insert(stored_name);
    }

    fn rename_column(&mut self, old_name: &str, new_name: &str) {
        if old_name.eq_ignore_ascii_case(new_name) {
            return;
        }
        if let Some((name, _)) = self
            .values
            .iter_mut()
            .find(|(column, _)| column.eq_ignore_ascii_case(old_name))
        {
            *name = new_name.to_string();
        }
        let was_null = self
            .null_columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(old_name));
        self.null_columns
            .retain(|column| !column.eq_ignore_ascii_case(old_name));
        if was_null {
            self.null_columns.insert(new_name.to_string());
        }
    }

    fn remove_column(&mut self, name: &str) {
        self.values
            .retain(|(column, _)| !column.eq_ignore_ascii_case(name));
        self.null_columns
            .retain(|column| !column.eq_ignore_ascii_case(name));
    }
}

impl Default for Row {
    fn default() -> Self {
        Self::new()
    }
}

const ROW_PAGE_MAGIC: &[u8; 4] = b"RWS1";
const ROW_PAGE_HEADER_SIZE: usize = 8;
type ConstraintKeySets = HashMap<(String, String), HashSet<Vec<u8>>>;
type RowPageIndex = HashMap<(String, String, Vec<u8>), HashSet<u32>>;
type RowValueIndex = HashMap<(String, String, Vec<u8>), Vec<usize>>;

fn pack_row_pages(first_page_number: u32, rows: &[Row]) -> Result<Vec<Page>> {
    let encoded = rows.iter().map(Row::encode).collect::<Vec<_>>();
    let mut pages = Vec::new();
    let mut index = 0;
    while index < encoded.len() {
        let mut page = Page::new(
            first_page_number + pages.len() as u32,
            PageType::Data,
            16384,
        );
        page.data[..4].copy_from_slice(ROW_PAGE_MAGIC);
        let mut cursor = ROW_PAGE_HEADER_SIZE;
        let start = index;
        while index < encoded.len() {
            let row = &encoded[index];
            if row.len() + 4 > page.data.len() - ROW_PAGE_HEADER_SIZE {
                anyhow::bail!("Row size {} exceeds page capacity", row.len());
            }
            if cursor + 4 + row.len() > page.data.len() {
                break;
            }
            page.data[cursor..cursor + 4].copy_from_slice(&(row.len() as u32).to_le_bytes());
            cursor += 4;
            page.data[cursor..cursor + row.len()].copy_from_slice(row);
            cursor += row.len();
            index += 1;
        }
        page.header.row_count = (index - start) as u32;
        page.data[4..8].copy_from_slice(&page.header.row_count.to_le_bytes());
        page.header.is_dirty = true;
        pages.push(page);
    }
    Ok(pages)
}

fn unpack_page_rows(page: &Page) -> Vec<Row> {
    if page.header.row_count == 0 {
        return Vec::new();
    }
    if !page.data.starts_with(ROW_PAGE_MAGIC) {
        return Row::decode(&page.data).into_iter().collect();
    }
    let count = u32::from_le_bytes(page.data[4..8].try_into().unwrap()) as usize;
    let mut rows = Vec::with_capacity(count);
    let mut cursor = ROW_PAGE_HEADER_SIZE;
    for _ in 0..count {
        if cursor + 4 > page.data.len() {
            break;
        }
        let length = u32::from_le_bytes(page.data[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;
        if cursor + length > page.data.len() {
            break;
        }
        if let Some(row) = Row::decode(&page.data[cursor..cursor + length]) {
            rows.push(row);
        }
        cursor += length;
    }
    rows
}

// ============================================================================
// Database
// ============================================================================

pub struct Database {
    pub name: String,
    pub tables: RwLock<HashMap<String, TableSchema>>,
    pub procedures: RwLock<HashMap<String, ProcedureDefinition>>,
    procedure_metadata: RwLock<HashMap<String, ProcedureMetadata>>,
    pub buffer_pool: Arc<BufferPool>,
    disk_manager: DiskManager,
    data_dir: PathBuf,
    constraint_keys: RwLock<ConstraintKeySets>,
    row_page_index: RwLock<RowPageIndex>,
    // Indexed values point into current rewrite overlays. Disk-resident rows
    // use page indexes; overlay rows use offsets to avoid duplicating BLOBs.
    row_value_index: RwLock<RowValueIndex>,
    auto_increment_next: RwLock<HashMap<String, u64>>,
    /// Full current table images for update-heavy tables since the last
    /// checkpoint. WAL is durable; the actor folds many rewrites into one COW.
    pending_rewrites: RwLock<HashMap<String, Vec<Row>>>,
    /// Volatile canonical rows for ENGINE=MEMORY. Metadata is durable, rows are
    /// deliberately neither paged nor replayed from WAL after restart.
    memory_rows: RwLock<HashMap<String, Vec<Row>>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RoutineCatalogV2 {
    format_version: u32,
    procedures: HashMap<String, ProcedureDefinition>,
    metadata: HashMap<String, ProcedureMetadata>,
}

fn routine_file_timestamp(path: &Path) -> String {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .map(chrono::DateTime::<Local>::from)
        .unwrap_or_else(|_| Local::now())
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

#[derive(Debug, Serialize, Deserialize)]
struct TableRewriteMarker {
    table: String,
    old_next_page_number: u32,
    new_next_page_number: u32,
    old_generation: u64,
    new_generation: u64,
    staging_dir: String,
    backup_dir: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct TableDropMarker {
    table: String,
    backup_dir: String,
}

impl Database {
    pub fn new(
        name: &str,
        data_dir: PathBuf,
        buffer_pool: Arc<BufferPool>,
        _wal_writer: Arc<parking_lot::Mutex<WalWriter>>,
    ) -> Self {
        let disk_manager = DiskManager::new(data_dir.join(name), 16384);

        Self {
            name: name.to_string(),
            tables: RwLock::new(HashMap::new()),
            procedures: RwLock::new(HashMap::new()),
            procedure_metadata: RwLock::new(HashMap::new()),
            buffer_pool,
            disk_manager,
            data_dir,
            constraint_keys: RwLock::new(HashMap::new()),
            row_page_index: RwLock::new(HashMap::new()),
            row_value_index: RwLock::new(HashMap::new()),
            auto_increment_next: RwLock::new(HashMap::new()),
            pending_rewrites: RwLock::new(HashMap::new()),
            memory_rows: RwLock::new(HashMap::new()),
        }
    }

    pub async fn load(&mut self) -> Result<()> {
        let database_dir = self.data_dir.join(&self.name);
        recover_schema_snapshot(&database_dir)?;
        let schema_file = database_dir.join("schema.json");
        if schema_file.exists() {
            let content = std::fs::read_to_string(&schema_file)?;
            let mut schemas: HashMap<String, TableSchema> = serde_json::from_str(&content)?;
            recover_table_drops(&database_dir, &schemas)?;
            recover_table_rewrites(&database_dir, &schemas)?;
            // Append-only inserts need not rewrite and fsync schema.json on
            // every group commit. Reconstruct the allocation cursor from the
            // durable segment length before rebuilding indexes.
            for (table_name, schema) in &mut schemas {
                if is_memory_schema(schema) {
                    schema.engine = TableEngine::Memory;
                    // Upgrade safety: older MyDB versions incorrectly paged a
                    // MEMORY table. MySQL semantics require those rows to be
                    // gone after every server restart.
                    self.disk_manager.delete_table(table_name)?;
                    schema.next_page_number = 0;
                    self.memory_rows
                        .write()
                        .insert(table_name.clone(), Vec::new());
                } else {
                    schema.next_page_number = self
                        .disk_manager
                        .list_pages(table_name)?
                        .into_iter()
                        .max()
                        .map_or(0, |page| page.saturating_add(1));
                }
            }
            *self.tables.write() = schemas;
            self.rebuild_all_indexes()?;
        }
        let routines_file = database_dir.join("routines.json");
        if routines_file.exists() {
            let content = std::fs::read_to_string(&routines_file)?;
            let value: serde_json::Value = serde_json::from_str(&content)?;
            if value
                .get("format_version")
                .and_then(serde_json::Value::as_u64)
                == Some(2)
            {
                let catalog: RoutineCatalogV2 = serde_json::from_value(value)?;
                *self.procedures.write() = catalog.procedures;
                *self.procedure_metadata.write() = catalog.metadata;
            } else {
                let procedures: HashMap<String, ProcedureDefinition> =
                    serde_json::from_value(value)?;
                let fallback = routine_file_timestamp(&routines_file);
                *self.procedure_metadata.write() = procedures
                    .keys()
                    .map(|name| {
                        (
                            name.clone(),
                            ProcedureMetadata {
                                created: fallback.clone(),
                                last_altered: fallback.clone(),
                                sql_mode: String::new(),
                            },
                        )
                    })
                    .collect();
                *self.procedures.write() = procedures;
            }
        }
        Ok(())
    }

    pub async fn save(&self) -> Result<()> {
        self.save_sync()
    }

    fn save_sync(&self) -> Result<()> {
        let table_dir = self.data_dir.join(&self.name);
        std::fs::create_dir_all(&table_dir)?;
        let schema_file = table_dir.join("schema.json");
        let schemas = self.tables.read().clone();
        let content = serde_json::to_string_pretty(&schemas)?;
        write_schema_snapshot(&schema_file, content.as_bytes())?;
        let routines_file = table_dir.join("routines.json");
        let routines = serde_json::to_string_pretty(&RoutineCatalogV2 {
            format_version: 2,
            procedures: self.procedures.read().clone(),
            metadata: self.procedure_metadata.read().clone(),
        })?;
        write_schema_snapshot(&routines_file, routines.as_bytes())?;
        finalize_table_rewrites(&table_dir, &schemas)?;
        finalize_table_drops(&table_dir, &schemas)?;
        Ok(())
    }

    pub fn create_table(&self, schema: TableSchema) -> Result<()> {
        let mut tables = self.tables.write();
        if tables.contains_key(&schema.name) {
            anyhow::bail!("Table '{}' already exists", schema.name);
        }

        let table_name = schema.name.clone();
        let memory = is_memory_schema(&schema);
        let constraints = key_constraints(&schema);
        if auto_increment_column(&schema).is_some() {
            self.auto_increment_next
                .write()
                .insert(table_name.clone(), 1);
        }
        tables.insert(table_name.clone(), schema);
        drop(tables);
        if memory {
            self.memory_rows
                .write()
                .insert(table_name.clone(), Vec::new());
        }
        let mut keys = self.constraint_keys.write();
        for (key_name, _, _) in constraints {
            keys.insert((table_name.clone(), key_name), HashSet::new());
        }
        Ok(())
    }

    pub fn drop_table(&self, name: &str) -> Result<()> {
        let mut tables = self.tables.write();

        if !tables.contains_key(name) {
            anyhow::bail!("Table '{}' does not exist", name);
        }

        let database_dir = self.data_dir.join(&self.name);
        let safe_table = rewrite_component(name)?;
        let marker = TableDropMarker {
            table: name.to_string(),
            backup_dir: format!(".drop-{safe_table}.backup"),
        };
        let marker_path = database_dir.join(format!(".drop-{safe_table}.json"));
        let active = database_dir.join(name);
        let backup = database_dir.join(&marker.backup_dir);
        if marker_path.exists() || backup.exists() {
            anyhow::bail!("Table '{}' has an unfinished drop", name);
        }
        write_json_sync(&marker_path, &marker)?;
        if active.exists() {
            fs::rename(active, backup)?;
        }
        self.buffer_pool.clear();
        tables.remove(name);
        self.constraint_keys
            .write()
            .retain(|(table, _), _| table != name);
        self.row_page_index
            .write()
            .retain(|(table, _, _), _| table != name);
        self.row_value_index
            .write()
            .retain(|(table, _, _), _| table != name);
        self.auto_increment_next.write().remove(name);
        self.pending_rewrites.write().remove(name);
        self.memory_rows.write().remove(name);
        Ok(())
    }

    pub fn get_table(&self, name: &str) -> Option<TableSchema> {
        self.tables.read().get(name).cloned()
    }

    pub fn list_tables(&self) -> Vec<String> {
        self.tables.read().keys().cloned().collect()
    }

    pub fn create_procedure(&self, procedure: ProcedureDefinition) -> Result<()> {
        self.create_procedure_with_metadata(procedure, ProcedureMetadata::new(String::new()))
    }

    pub fn create_procedure_with_metadata(
        &self,
        procedure: ProcedureDefinition,
        metadata: ProcedureMetadata,
    ) -> Result<()> {
        let mut procedures = self.procedures.write();
        if procedures
            .keys()
            .any(|name| name.eq_ignore_ascii_case(&procedure.name))
        {
            anyhow::bail!("PROCEDURE {} already exists", procedure.name);
        }
        let name = procedure.name.clone();
        procedures.insert(name.clone(), procedure);
        self.procedure_metadata.write().insert(name, metadata);
        Ok(())
    }

    pub fn drop_procedure(&self, name: &str) -> Result<()> {
        let mut procedures = self.procedures.write();
        let stored = procedures
            .keys()
            .find(|stored| stored.eq_ignore_ascii_case(name))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("PROCEDURE {} does not exist", name))?;
        procedures.remove(&stored);
        self.procedure_metadata.write().remove(&stored);
        Ok(())
    }

    pub fn alter_procedure(&self, name: &str, create_sql: String) -> Result<()> {
        let metadata = self
            .get_procedure_metadata(name)
            .unwrap_or_else(|| ProcedureMetadata::new(String::new()))
            .altered(String::new());
        self.alter_procedure_with_metadata(name, create_sql, metadata)
    }

    pub fn alter_procedure_with_metadata(
        &self,
        name: &str,
        create_sql: String,
        metadata: ProcedureMetadata,
    ) -> Result<()> {
        let mut procedures = self.procedures.write();
        let stored = procedures
            .keys()
            .find(|stored| stored.eq_ignore_ascii_case(name))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("PROCEDURE {} does not exist", name))?;
        procedures
            .get_mut(&stored)
            .expect("resolved procedure")
            .create_sql = create_sql;
        self.procedure_metadata.write().insert(stored, metadata);
        Ok(())
    }

    pub fn get_procedure(&self, name: &str) -> Option<ProcedureDefinition> {
        self.procedures
            .read()
            .iter()
            .find(|(stored, _)| stored.eq_ignore_ascii_case(name))
            .map(|(_, procedure)| procedure.clone())
    }

    pub fn list_procedures(&self) -> Vec<ProcedureDefinition> {
        self.procedures.read().values().cloned().collect()
    }

    pub fn get_procedure_metadata(&self, name: &str) -> Option<ProcedureMetadata> {
        self.procedure_metadata
            .read()
            .iter()
            .find(|(stored, _)| stored.eq_ignore_ascii_case(name))
            .map(|(_, metadata)| metadata.clone())
    }

    pub fn is_memory_table(&self, name: &str) -> bool {
        self.tables.read().get(name).is_some_and(is_memory_schema)
    }

    // -----------------------------------------------------------------------
    // Row operations
    // -----------------------------------------------------------------------

    /// Insert a row into a table
    pub fn insert_row(&self, table_name: &str, row: Row) -> Result<()> {
        let schema_snapshot = self
            .get_table(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;
        let row = materialize_row(row, &schema_snapshot)?;
        let stored_keys = self.constraint_keys.read();
        for (key_name, columns, nullable) in key_constraints(&schema_snapshot) {
            let Some(key) = encode_key(&row, &columns, nullable) else {
                continue;
            };
            if stored_keys
                .get(&(table_name.to_string(), key_name.clone()))
                .is_some_and(|keys| keys.contains(&key))
            {
                anyhow::bail!("Duplicate entry for key '{}'", key_name);
            }
        }
        drop(stored_keys);

        self.insert_rows_validated(table_name, vec![row], true)
            .map(|_| ())
    }

    fn insert_rows_validated(
        &self,
        table_name: &str,
        rows: Vec<Row>,
        sync_data: bool,
    ) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        let schema_snapshot = self
            .get_table(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;
        let (rows, last_insert_id, next_auto_increment) =
            self.assign_auto_increment_rows(table_name, rows, &schema_snapshot)?;
        let rows = rows
            .into_iter()
            .map(|row| materialize_row(row, &schema_snapshot))
            .collect::<Result<Vec<_>>>()?;

        if self.is_memory_table(table_name) {
            let mut memory = self.memory_rows.write();
            let current = memory.entry(table_name.to_string()).or_default();
            let first_row = current.len();
            current.extend(rows.iter().cloned());
            drop(memory);
            self.add_constraint_keys(table_name, &schema_snapshot, &rows);
            self.add_rows_to_value_index(table_name, &schema_snapshot, first_row, &rows);
            if let Some(next) = next_auto_increment {
                self.auto_increment_next
                    .write()
                    .insert(table_name.to_string(), next);
            }
            return Ok(last_insert_id);
        }

        let mut pending = self.pending_rewrites.write();
        if let Some(current) = pending.get_mut(table_name) {
            let first_row = current.len();
            current.extend(rows.iter().cloned());
            drop(pending);
            self.add_constraint_keys(table_name, &schema_snapshot, &rows);
            self.add_rows_to_value_index(table_name, &schema_snapshot, first_row, &rows);
            if let Some(next) = next_auto_increment {
                self.auto_increment_next
                    .write()
                    .insert(table_name.to_string(), next);
            }
            return Ok(last_insert_id);
        }
        drop(pending);

        let first_page_number = self
            .tables
            .read()
            .get(table_name)
            .map(|schema| schema.next_page_number)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;
        let pages = pack_row_pages(first_page_number, &rows)?;
        let page_count = pages.len() as u32;

        if sync_data {
            self.disk_manager.write_pages(table_name, &pages)?;
        } else {
            self.disk_manager.write_pages_buffered(table_name, &pages)?;
        }
        for page in pages {
            self.add_row_page_index(table_name, &schema_snapshot, &page);
            self.buffer_pool
                .insert_table_page(&self.page_namespace(table_name), page);
        }
        if let Some(schema) = self.tables.write().get_mut(table_name) {
            schema.next_page_number = first_page_number + page_count;
        }
        self.add_constraint_keys(table_name, &schema_snapshot, &rows);
        if let Some(next) = next_auto_increment {
            self.auto_increment_next
                .write()
                .insert(table_name.to_string(), next);
        }

        debug!(
            "Inserted {} rows into {}.{}",
            rows.len(),
            self.name,
            table_name
        );

        Ok(last_insert_id)
    }

    /// Get all rows from a table
    pub fn scan_table(&self, table_name: &str) -> Result<Vec<Row>> {
        if let Some(rows) = self.memory_rows.read().get(table_name) {
            return Ok(rows.clone());
        }
        if let Some(rows) = self.pending_rewrites.read().get(table_name) {
            return Ok(rows.clone());
        }
        let next_page_number = self
            .tables
            .read()
            .get(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?
            .next_page_number;

        let mut rows = Vec::new();
        for page_num in 0..next_page_number {
            let namespace = self.page_namespace(table_name);
            let page = if let Some(page) = self.buffer_pool.get_table_page(&namespace, page_num) {
                Some(page)
            } else {
                let page = self.disk_manager.read_page(table_name, page_num)?;
                if let Some(page) = &page {
                    self.buffer_pool.insert_table_page(&namespace, page.clone());
                }
                page
            };
            if let Some(page) = page {
                rows.extend(unpack_page_rows(&page));
            }
        }

        Ok(rows)
    }

    /// Scan only pages that can contain indexed equality/IN values. Falls back
    /// to a full scan when the predicate column is not indexed.
    pub fn scan_table_filtered(
        &self,
        table_name: &str,
        filter: Option<&RowPredicate>,
    ) -> Result<Vec<Row>> {
        self.scan_table_filtered_limit(table_name, filter, None)
    }

    /// Indexed predicate scan with an optional safe LIMIT pushdown.
    pub fn scan_table_filtered_limit(
        &self,
        table_name: &str,
        filter: Option<&RowPredicate>,
        limit: Option<usize>,
    ) -> Result<Vec<Row>> {
        if limit == Some(0) {
            return Ok(Vec::new());
        }
        let (column, values): (&String, Vec<&Vec<u8>>) = match filter {
            Some(RowPredicate::Eq(column, value)) => (column, vec![value]),
            Some(RowPredicate::In(column, values)) => (column, values.iter().collect()),
            _ => {
                return Ok(self
                    .scan_table(table_name)?
                    .into_iter()
                    .filter(|row| filter.is_none_or(|predicate| predicate.matches(row)))
                    .take(limit.unwrap_or(usize::MAX))
                    .collect());
            }
        };
        let schema = self
            .get_table(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;
        if !indexed_columns(&schema).contains(column) {
            return Ok(self
                .scan_table(table_name)?
                .into_iter()
                .filter(|row| filter.is_none_or(|predicate| predicate.matches(row)))
                .take(limit.unwrap_or(usize::MAX))
                .collect());
        }
        let memory = self.memory_rows.read();
        let pending = self.pending_rewrites.read();
        if let Some(overlay_rows) = memory.get(table_name).or_else(|| pending.get(table_name)) {
            let value_index = self.row_value_index.read();
            let mut indexed_rows = Vec::new();
            for value in &values {
                if let Some(positions) =
                    value_index.get(&(table_name.to_string(), column.clone(), (*value).clone()))
                {
                    for position in positions {
                        let Some(row) = overlay_rows.get(*position) else {
                            continue;
                        };
                        if filter.is_some_and(|predicate| !predicate.matches(row)) {
                            continue;
                        }
                        indexed_rows.push(row.clone());
                        if limit.is_some_and(|limit| indexed_rows.len() >= limit) {
                            break;
                        }
                    }
                }
                if limit.is_some_and(|limit| indexed_rows.len() >= limit) {
                    break;
                }
            }
            return Ok(indexed_rows);
        }
        drop(pending);
        drop(memory);
        let index = self.row_page_index.read();
        let mut page_numbers = HashSet::new();
        for value in values {
            page_numbers.extend(
                index
                    .get(&(table_name.to_string(), column.clone(), value.clone()))
                    .into_iter()
                    .flatten()
                    .copied(),
            );
        }
        let mut page_numbers: Vec<_> = page_numbers.into_iter().collect();
        page_numbers.sort_unstable();
        let namespace = self.page_namespace(table_name);
        let mut rows = Vec::new();
        'pages: for page_num in page_numbers {
            let page = if let Some(page) = self.buffer_pool.get_table_page(&namespace, page_num) {
                Some(page)
            } else {
                let page = self.disk_manager.read_page(table_name, page_num)?;
                if let Some(page) = &page {
                    self.buffer_pool.insert_table_page(&namespace, page.clone());
                }
                page
            };
            if let Some(page) = page {
                for row in unpack_page_rows(&page)
                    .into_iter()
                    .filter(|row| filter.is_none_or(|predicate| predicate.matches(row)))
                {
                    rows.push(row);
                    if limit.is_some_and(|limit| rows.len() >= limit) {
                        break 'pages;
                    }
                }
            }
        }
        Ok(rows)
    }

    /// Delete all rows (for simplicity, mark pages as deleted)
    pub fn delete_all_rows(&self, table_name: &str) -> Result<u64> {
        let count = self.scan_table(table_name)?.len() as u64;
        self.replace_rows(table_name, Vec::new())?;
        Ok(count)
    }

    pub fn update_rows(
        &self,
        table_name: &str,
        filter: Option<&RowPredicate>,
        assignments: &[(String, Option<Vec<u8>>)],
    ) -> Result<u64> {
        self.update_rows_mode(table_name, filter, assignments, false)
    }

    fn update_rows_mode(
        &self,
        table_name: &str,
        filter: Option<&RowPredicate>,
        assignments: &[(String, Option<Vec<u8>>)],
        defer_rewrite: bool,
    ) -> Result<u64> {
        let mut rows = self.scan_table(table_name)?;
        let mut affected = 0;
        for row in &mut rows {
            if row_matches(row, filter) {
                let before = row.clone();
                for (column, value) in assignments {
                    if let Some(value) = value {
                        row.set(column, value.clone());
                    } else {
                        row.set_null(column);
                    }
                }
                if *row != before {
                    affected += 1;
                }
            }
        }
        if affected > 0 {
            if defer_rewrite {
                self.stage_rows(table_name, rows)?;
            } else {
                self.replace_rows(table_name, rows)?;
            }
        }
        Ok(affected)
    }

    fn update_rows_expression_mode(
        &self,
        table_name: &str,
        filter: Option<&RowPredicate>,
        assignments: &[ExpressionAssignment],
        defer_rewrite: bool,
    ) -> Result<u64> {
        let schema = self
            .get_table(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;
        let mut rows = self.scan_table(table_name)?;
        let mut affected = 0;
        for row in &mut rows {
            if !row_matches(row, filter) {
                continue;
            }
            let before = row.clone();
            apply_expression_assignments(row, &schema, assignments)?;
            if *row != before {
                affected += 1;
            }
        }
        if affected > 0 {
            if defer_rewrite {
                self.stage_rows(table_name, rows)?;
            } else {
                self.replace_rows(table_name, rows)?;
            }
        }
        Ok(affected)
    }

    pub fn upsert_rows(
        &self,
        table_name: &str,
        incoming: Vec<Row>,
        update_columns: &[String],
        ignore: bool,
    ) -> Result<u64> {
        self.upsert_rows_mode(table_name, incoming, update_columns, ignore, false)
    }

    fn upsert_rows_mode(
        &self,
        table_name: &str,
        incoming: Vec<Row>,
        update_columns: &[String],
        ignore: bool,
        defer_rewrite: bool,
    ) -> Result<u64> {
        let schema = self
            .get_table(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;
        let (incoming, _, next_auto_increment) =
            self.assign_auto_increment_rows(table_name, incoming, &schema)?;
        let incoming = incoming
            .into_iter()
            .map(|row| materialize_row(row, &schema))
            .collect::<Result<Vec<_>>>()?;
        let constraints = key_constraints(&schema);
        let mut existing = self.scan_table(table_name)?;
        let mut inserts = Vec::new();
        let mut affected = 0_u64;
        let mut changed_existing = false;
        for row in incoming {
            let conflict = existing.iter().position(|current| {
                constraints.iter().any(|(_, columns, nullable)| {
                    let incoming_key = encode_key(&row, columns, *nullable);
                    incoming_key.is_some()
                        && incoming_key == encode_key(current, columns, *nullable)
                })
            });
            if let Some(index) = conflict {
                if ignore {
                    continue;
                }
                let before = existing[index].clone();
                for column in update_columns {
                    validate_column_exists(&schema, column)?;
                    if row.is_null(column) {
                        existing[index].set_null(column);
                    } else if let Some(value) = row.get(column) {
                        existing[index].set(column, value.to_vec());
                    }
                }
                if existing[index] != before {
                    affected += 2;
                    changed_existing = true;
                }
            } else {
                existing.push(row.clone());
                inserts.push(row);
                affected += 1;
            }
        }
        if changed_existing {
            if defer_rewrite {
                self.stage_rows(table_name, existing)?;
            } else {
                self.replace_rows(table_name, existing)?;
            }
        } else if !inserts.is_empty() {
            self.insert_rows_validated(table_name, inserts, !defer_rewrite)?;
        }
        if let Some(next) = next_auto_increment {
            self.auto_increment_next
                .write()
                .entry(table_name.to_string())
                .and_modify(|current| *current = (*current).max(next))
                .or_insert(next);
        }
        Ok(affected)
    }

    fn assign_auto_increment_rows(
        &self,
        table_name: &str,
        mut rows: Vec<Row>,
        schema: &TableSchema,
    ) -> Result<(Vec<Row>, u64, Option<u64>)> {
        let Some(column) = auto_increment_column(schema) else {
            return Ok((rows, 0, None));
        };
        let mut next = self
            .auto_increment_next
            .read()
            .get(table_name)
            .copied()
            .unwrap_or(1);
        let mut last_insert_id = 0;
        for row in &mut rows {
            let generate = !row.contains(&column)
                || row.is_null(&column)
                || row.get(&column).is_some_and(|value| value == b"0");
            if generate {
                if last_insert_id == 0 {
                    last_insert_id = next;
                }
                row.set(&column, next.to_string().into_bytes());
                next = next
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("AUTO_INCREMENT overflow"))?;
            } else if let Some(value) = row.get(&column) {
                let explicit = std::str::from_utf8(value)?.parse::<u64>()?;
                next = next.max(explicit.saturating_add(1));
            }
        }
        Ok((rows, last_insert_id, Some(next)))
    }

    /// Materialize a consecutive actor batch with one schema lookup and one
    /// AUTO_INCREMENT reservation instead of locking the same metadata per row.
    fn prepare_rows_for_wal(&self, table_name: &str, rows: Vec<Row>) -> Result<(Vec<Row>, u64)> {
        let schema = self
            .get_table(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;
        let (rows, last_insert_id, next) =
            self.assign_auto_increment_rows(table_name, rows, &schema)?;
        if let Some(next) = next {
            self.auto_increment_next
                .write()
                .entry(table_name.to_string())
                .and_modify(|current| *current = (*current).max(next))
                .or_insert(next);
        }
        let rows = rows
            .into_iter()
            .map(|row| materialize_row(row, &schema))
            .collect::<Result<Vec<_>>>()?;
        Ok((rows, last_insert_id))
    }

    pub fn materialize_insert_rows(
        &self,
        table_name: &str,
        rows: Vec<Row>,
    ) -> Result<(Vec<Row>, u64)> {
        self.prepare_rows_for_wal(table_name, rows)
    }

    pub fn delete_rows(&self, table_name: &str, filter: Option<&RowPredicate>) -> Result<u64> {
        self.delete_rows_mode(table_name, filter, false)
    }

    fn delete_rows_mode(
        &self,
        table_name: &str,
        filter: Option<&RowPredicate>,
        defer_rewrite: bool,
    ) -> Result<u64> {
        if filter.is_none() {
            let count = self.scan_table(table_name)?.len() as u64;
            if defer_rewrite {
                self.stage_rows(table_name, Vec::new())?;
            } else {
                self.replace_rows(table_name, Vec::new())?;
            }
            return Ok(count);
        }
        let rows = self.scan_table(table_name)?;
        let before = rows.len();
        let retained: Vec<_> = rows
            .into_iter()
            .filter(|row| !row_matches(row, filter))
            .collect();
        let affected = (before - retained.len()) as u64;
        if affected > 0 {
            if defer_rewrite {
                self.stage_rows(table_name, retained)?;
            } else {
                self.replace_rows(table_name, retained)?;
            }
        }
        Ok(affected)
    }

    fn stage_rows(&self, table_name: &str, rows: Vec<Row>) -> Result<()> {
        let rows = self.prepare_replacement_rows(table_name, rows)?;
        let schema = self
            .get_table(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;
        if self.is_memory_table(table_name) {
            self.memory_rows
                .write()
                .insert(table_name.to_string(), rows.clone());
        } else {
            self.pending_rewrites
                .write()
                .insert(table_name.to_string(), rows.clone());
        }
        self.replace_table_logical_indexes(table_name, &schema, &rows);
        Ok(())
    }

    fn replace_rows(&self, table_name: &str, rows: Vec<Row>) -> Result<()> {
        let rows = self.prepare_replacement_rows(table_name, rows)?;
        let schema = self
            .get_table(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;
        if is_memory_schema(&schema) {
            self.memory_rows
                .write()
                .insert(table_name.to_string(), rows.clone());
            self.replace_table_logical_indexes(table_name, &schema, &rows);
            return Ok(());
        }
        let pages = pack_row_pages(0, &rows)?;
        let database_dir = self.data_dir.join(&self.name);
        let safe_table = rewrite_component(table_name)?;
        let marker = TableRewriteMarker {
            table: table_name.to_string(),
            old_next_page_number: schema.next_page_number,
            new_next_page_number: pages.len() as u32,
            old_generation: schema.generation,
            new_generation: schema.generation.saturating_add(1),
            staging_dir: format!(".rewrite-{safe_table}.staging"),
            backup_dir: format!(".rewrite-{safe_table}.backup"),
        };
        let marker_path = database_dir.join(format!(".rewrite-{safe_table}.json"));
        let staging = database_dir.join(&marker.staging_dir);
        let backup = database_dir.join(&marker.backup_dir);
        let active = database_dir.join(table_name);
        if marker_path.exists() || staging.exists() || backup.exists() {
            anyhow::bail!("Table '{}' has an unfinished rewrite", table_name);
        }
        fs::create_dir_all(&staging)?;
        self.disk_manager.write_pages(&marker.staging_dir, &pages)?;
        write_json_sync(&marker_path, &marker)?;
        if active.exists() {
            fs::rename(&active, &backup)?;
        }
        if let Err(error) = fs::rename(&staging, &active) {
            if backup.exists() && !active.exists() {
                let _ = fs::rename(&backup, &active);
            }
            return Err(error.into());
        }

        if let Some(table) = self.tables.write().get_mut(table_name) {
            table.next_page_number = pages.len() as u32;
            table.generation = marker.new_generation;
        }
        self.buffer_pool.clear();
        self.rebuild_all_indexes()?;
        Ok(())
    }

    fn prepare_replacement_rows(&self, table_name: &str, rows: Vec<Row>) -> Result<Vec<Row>> {
        let schema = self
            .get_table(table_name)
            .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table_name))?;
        let rows = rows
            .into_iter()
            .map(|row| materialize_row(row, &schema))
            .collect::<Result<Vec<_>>>()?;
        for (key_name, columns, nullable) in key_constraints(&schema) {
            let mut keys = HashSet::new();
            for key in rows
                .iter()
                .filter_map(|row| encode_key(row, &columns, nullable))
            {
                if !keys.insert(key) {
                    anyhow::bail!("Duplicate entry for key '{}'", key_name);
                }
            }
        }
        Ok(rows)
    }

    fn orphan_storage(&self) -> Result<Vec<OrphanStorage>> {
        let active: HashSet<_> = self.tables.read().keys().cloned().collect();
        let database_dir = self.data_dir.join(&self.name);
        let mut orphaned = Vec::new();
        if !database_dir.exists() {
            return Ok(orphaned);
        }
        for entry in fs::read_dir(database_dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let table = entry.file_name().to_string_lossy().to_string();
            if active.contains(&table) || !is_page_only_directory(&entry.path())? {
                continue;
            }
            orphaned.push(OrphanStorage {
                database: self.name.clone(),
                table,
                bytes: directory_size(&entry.path())?,
            });
        }
        orphaned.sort_by(|a, b| a.table.cmp(&b.table));
        Ok(orphaned)
    }

    fn cleanup_orphan_storage(&self) -> Result<u64> {
        let candidates = self.orphan_storage()?;
        let mut removed = 0;
        for candidate in candidates {
            // Recheck metadata immediately before deletion while running inside
            // the FIFO write actor. Active table directories are never removed.
            if self.tables.read().contains_key(&candidate.table) {
                continue;
            }
            let path = self.data_dir.join(&self.name).join(&candidate.table);
            if path.parent() != Some(self.data_dir.join(&self.name).as_path())
                || !is_page_only_directory(&path)?
            {
                continue;
            }
            fs::remove_dir_all(path)?;
            removed += 1;
        }
        if removed > 0 {
            self.buffer_pool.clear();
        }
        Ok(removed)
    }

    fn page_namespace(&self, table_name: &str) -> String {
        format!("{}/{}", self.name, table_name)
    }

    fn sync_table(&self, table_name: &str) -> Result<()> {
        self.disk_manager.sync_table(table_name)
    }

    fn checkpoint_table(&self, table_name: &str) -> Result<()> {
        if self.is_memory_table(table_name) {
            return Ok(());
        }
        let pending = self.pending_rewrites.read().get(table_name).cloned();
        let Some(rows) = pending else {
            return self.sync_table(table_name);
        };
        // Keep the overlay visible while COW writes and atomically swaps the
        // durable table. On failure, reads continue from the WAL-backed overlay.
        self.replace_rows(table_name, rows)?;
        self.pending_rewrites.write().remove(table_name);
        // replace_rows rebuilt logical constraints from the overlay; rebuild
        // once more from the newly swapped pages to restore page indexes.
        self.rebuild_all_indexes()?;
        self.save_sync()
    }

    fn constraint_key_exists(&self, table_name: &str, key_name: &str, key: &[u8]) -> bool {
        self.constraint_keys
            .read()
            .get(&(table_name.to_string(), key_name.to_string()))
            .is_some_and(|keys| keys.contains(key))
    }

    fn rebuild_all_indexes(&self) -> Result<()> {
        let schemas = self.tables.read().clone();
        let pending = self.pending_rewrites.read().clone();
        let memory = self.memory_rows.read().clone();
        let current_auto_increment = self.auto_increment_next.read().clone();
        let mut rebuilt = HashMap::new();
        let mut page_index: RowPageIndex = HashMap::new();
        let mut value_index: RowValueIndex = HashMap::new();
        let mut auto_increment_next = HashMap::new();
        for (table_name, schema) in schemas {
            let (rows, has_overlay) = if let Some(rows) = memory.get(&table_name) {
                (rows.clone(), true)
            } else if let Some(rows) = pending.get(&table_name) {
                (rows.clone(), true)
            } else {
                let mut rows = Vec::new();
                for page_number in self.disk_manager.list_pages(&table_name)? {
                    let Some(page) = self.disk_manager.read_page(&table_name, page_number)? else {
                        continue;
                    };
                    let page_rows = unpack_page_rows(&page);
                    add_rows_to_page_index(
                        &mut page_index,
                        &table_name,
                        &schema,
                        page_number,
                        &page_rows,
                    );
                    rows.extend(page_rows);
                    self.buffer_pool
                        .insert_table_page(&self.page_namespace(&table_name), page);
                }
                (rows, false)
            };
            for (key_name, columns, nullable) in key_constraints(&schema) {
                let keys = rows
                    .iter()
                    .filter_map(|row| encode_key(row, &columns, nullable))
                    .collect();
                rebuilt.insert((table_name.clone(), key_name), keys);
            }
            if has_overlay {
                add_rows_to_value_index(&mut value_index, &table_name, &schema, 0, &rows);
            }
            if let Some(column) = auto_increment_column(&schema) {
                let next = rows
                    .iter()
                    .filter_map(|row| row.get(&column))
                    .filter_map(|value| std::str::from_utf8(value).ok())
                    .filter_map(|value| value.parse::<u64>().ok())
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                auto_increment_next.insert(
                    table_name.clone(),
                    next.max(
                        current_auto_increment
                            .get(&table_name)
                            .copied()
                            .unwrap_or(1),
                    )
                    .max(1),
                );
            }
        }
        *self.constraint_keys.write() = rebuilt;
        *self.row_page_index.write() = page_index;
        *self.row_value_index.write() = value_index;
        *self.auto_increment_next.write() = auto_increment_next;
        Ok(())
    }

    fn add_constraint_keys(&self, table_name: &str, schema: &TableSchema, rows: &[Row]) {
        let mut stored = self.constraint_keys.write();
        for (key_name, columns, nullable) in key_constraints(schema) {
            let keys = stored
                .entry((table_name.to_string(), key_name))
                .or_default();
            keys.extend(
                rows.iter()
                    .filter_map(|row| encode_key(row, &columns, nullable)),
            );
        }
    }

    fn replace_table_logical_indexes(&self, table_name: &str, schema: &TableSchema, rows: &[Row]) {
        let mut stored = self.constraint_keys.write();
        stored.retain(|(table, _), _| table != table_name);
        for (key_name, columns, nullable) in key_constraints(schema) {
            stored.insert(
                (table_name.to_string(), key_name),
                rows.iter()
                    .filter_map(|row| encode_key(row, &columns, nullable))
                    .collect(),
            );
        }
        drop(stored);
        self.row_page_index
            .write()
            .retain(|(table, _, _), _| table != table_name);
        self.replace_table_value_index(table_name, schema, rows);
        if let Some(column) = auto_increment_column(schema) {
            let row_next = rows
                .iter()
                .filter_map(|row| row.get(&column))
                .filter_map(|value| std::str::from_utf8(value).ok())
                .filter_map(|value| value.parse::<u64>().ok())
                .max()
                .unwrap_or(0)
                .saturating_add(1)
                .max(1);
            self.auto_increment_next
                .write()
                .entry(table_name.to_string())
                .and_modify(|next| *next = (*next).max(row_next))
                .or_insert(row_next);
        } else {
            self.auto_increment_next.write().remove(table_name);
        }
    }

    fn reset_auto_increment(&self, table_name: &str) {
        if self
            .get_table(table_name)
            .is_some_and(|schema| auto_increment_column(&schema).is_some())
        {
            self.auto_increment_next
                .write()
                .insert(table_name.to_string(), 1);
        }
    }

    fn add_row_page_index(&self, table_name: &str, schema: &TableSchema, page: &Page) {
        add_rows_to_page_index(
            &mut self.row_page_index.write(),
            table_name,
            schema,
            page.header.page_number,
            &unpack_page_rows(page),
        );
    }

    fn add_rows_to_value_index(
        &self,
        table_name: &str,
        schema: &TableSchema,
        first_row: usize,
        rows: &[Row],
    ) {
        add_rows_to_value_index(
            &mut self.row_value_index.write(),
            table_name,
            schema,
            first_row,
            rows,
        );
    }

    fn replace_table_value_index(&self, table_name: &str, schema: &TableSchema, rows: &[Row]) {
        let mut index = self.row_value_index.write();
        index.retain(|(table, _, _), _| table != table_name);
        add_rows_to_value_index(&mut index, table_name, schema, 0, rows);
    }
}

pub fn apply_expression_assignments(
    row: &mut Row,
    schema: &TableSchema,
    assignments: &[ExpressionAssignment],
) -> Result<()> {
    for assignment in assignments {
        let target = schema
            .columns
            .iter()
            .find(|column| column.name == assignment.target_column)
            .ok_or_else(|| anyhow::anyhow!("Unknown column '{}'", assignment.target_column))?;
        let value = evaluate_update_value(row, schema, target, &assignment.value)?;
        match value {
            Some(value) => row.set(&assignment.target_column, value),
            None if target.nullable => row.set_null(&assignment.target_column),
            None => anyhow::bail!("Column '{}' cannot be null", target.name),
        }
    }
    Ok(())
}

fn evaluate_update_value(
    row: &Row,
    schema: &TableSchema,
    target: &Column,
    expression: &UpdateValueExpression,
) -> Result<Option<Vec<u8>>> {
    let UpdateValueExpression::Numeric {
        source_column,
        operator,
        operand,
    } = expression
    else {
        return Ok(match expression {
            UpdateValueExpression::Literal(value) => value.clone(),
            UpdateValueExpression::Column(source_column) => {
                if row.is_null(source_column) || !row.contains(source_column) {
                    None
                } else {
                    Some(
                        row.get(source_column)
                            .ok_or_else(|| anyhow::anyhow!("Unknown column '{}'", source_column))?
                            .to_vec(),
                    )
                }
            }
            UpdateValueExpression::Default(source_column) => {
                column_default_value(schema, source_column)?
            }
            UpdateValueExpression::Numeric { .. } => unreachable!(),
        });
    };
    if row.is_null(source_column) || !row.contains(source_column) {
        return Ok(None);
    }
    let source = row
        .get(source_column)
        .ok_or_else(|| anyhow::anyhow!("Unknown column '{}'", source_column))?;
    let source = std::str::from_utf8(source)?.parse::<Decimal>()?;
    let operand = std::str::from_utf8(operand)?.parse::<Decimal>()?;
    let result = match operator {
        NumericOperator::Add => source.checked_add(operand),
        NumericOperator::Subtract => source.checked_sub(operand),
        NumericOperator::Multiply => source.checked_mul(operand),
        NumericOperator::Divide if operand.is_zero() => anyhow::bail!("Division by 0"),
        NumericOperator::Divide => source.checked_div(operand),
    }
    .ok_or_else(|| anyhow::anyhow!("Numeric value out of range"))?;
    Ok(Some(
        format_numeric_assignment(result, &target.data_type)?.into_bytes(),
    ))
}

fn column_default_value(schema: &TableSchema, column_name: &str) -> Result<Option<Vec<u8>>> {
    let column = schema
        .columns
        .iter()
        .find(|column| column.name.eq_ignore_ascii_case(column_name))
        .ok_or_else(|| anyhow::anyhow!("Unknown column '{}'", column_name))?;
    if auto_increment_column(schema)
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(&column.name))
    {
        return Ok(Some(b"0".to_vec()));
    }
    match column.default.as_deref() {
        Some(default) if default.eq_ignore_ascii_case("NULL") => Ok(None),
        Some(default) if is_current_timestamp_default(default) => {
            Ok(current_timestamp_default(default))
        }
        Some(default) => Ok(Some(default.as_bytes().to_vec())),
        None if column.nullable => Ok(None),
        None => anyhow::bail!("Field '{}' doesn't have a default value", column.name),
    }
}

fn format_numeric_assignment(mut value: Decimal, data_type: &DataType) -> Result<String> {
    match data_type {
        DataType::Int | DataType::BigInt | DataType::Boolean => Ok(value
            .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
            .normalize()
            .to_string()),
        DataType::Float | DataType::Double => Ok(value.normalize().to_string()),
        DataType::Raw(raw)
            if raw.trim().to_ascii_uppercase().starts_with("DECIMAL(")
                || raw.trim().to_ascii_uppercase().starts_with("NUMERIC(") =>
        {
            let scale = decimal_type_scale(raw).unwrap_or(0);
            value = value.round_dp_with_strategy(scale, RoundingStrategy::MidpointAwayFromZero);
            value.rescale(scale);
            Ok(value.to_string())
        }
        DataType::Raw(raw) => anyhow::bail!("Data type '{}' is not numeric", raw),
        _ => anyhow::bail!("Column data type is not numeric"),
    }
}

fn is_numeric_data_type(data_type: &DataType) -> bool {
    match data_type {
        DataType::Int
        | DataType::BigInt
        | DataType::Float
        | DataType::Double
        | DataType::Boolean => true,
        DataType::Raw(raw) => {
            let raw = raw.trim().to_ascii_uppercase();
            raw.starts_with("DECIMAL(") || raw.starts_with("NUMERIC(")
        }
        _ => false,
    }
}

fn decimal_type_scale(data_type: &str) -> Option<u32> {
    let open = data_type.find('(')?;
    let close = data_type[open + 1..].find(')')? + open + 1;
    data_type[open + 1..close]
        .split(',')
        .nth(1)
        .map(str::trim)?
        .parse()
        .ok()
}

fn rewrite_component(table_name: &str) -> Result<String> {
    if table_name.is_empty()
        || table_name == "."
        || table_name == ".."
        || table_name.contains(['/', '\\'])
    {
        anyhow::bail!("Unsafe table name '{}'", table_name);
    }
    Ok(table_name
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn write_json_sync<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_schema_snapshot(schema_file: &Path, content: &[u8]) -> Result<()> {
    let database_dir = schema_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Schema path has no parent"))?;
    let temporary = database_dir.join("schema.json.tmp");
    let backup = database_dir.join("schema.json.bak");
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);

    if backup.exists() {
        fs::remove_file(&backup)?;
    }
    if schema_file.exists() {
        fs::rename(schema_file, &backup)?;
    }
    if let Err(error) = fs::rename(&temporary, schema_file) {
        if backup.exists() && !schema_file.exists() {
            let _ = fs::rename(&backup, schema_file);
        }
        return Err(error.into());
    }
    if backup.exists() {
        fs::remove_file(backup)?;
    }
    Ok(())
}

fn recover_schema_snapshot(database_dir: &Path) -> Result<()> {
    let schema = database_dir.join("schema.json");
    let temporary = database_dir.join("schema.json.tmp");
    let backup = database_dir.join("schema.json.bak");
    if schema.exists() {
        if temporary.exists() {
            fs::remove_file(temporary)?;
        }
        if backup.exists() {
            fs::remove_file(backup)?;
        }
    } else if backup.exists() {
        fs::rename(backup, &schema)?;
        if temporary.exists() {
            fs::remove_file(temporary)?;
        }
    } else if temporary.exists() {
        fs::rename(temporary, schema)?;
    }
    Ok(())
}

fn rewrite_markers(database_dir: &Path) -> Result<Vec<(PathBuf, TableRewriteMarker)>> {
    let mut markers = Vec::new();
    if !database_dir.exists() {
        return Ok(markers);
    }
    for entry in fs::read_dir(database_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(".rewrite-") || !name.ends_with(".json") {
            continue;
        }
        let marker: TableRewriteMarker = serde_json::from_slice(&fs::read(entry.path())?)?;
        validate_rewrite_marker(&marker)?;
        markers.push((entry.path(), marker));
    }
    Ok(markers)
}

fn validate_rewrite_marker(marker: &TableRewriteMarker) -> Result<()> {
    rewrite_component(&marker.table)?;
    for component in [&marker.staging_dir, &marker.backup_dir] {
        if !component.starts_with(".rewrite-") || Path::new(component).components().count() != 1 {
            anyhow::bail!("Unsafe rewrite path '{}'", component);
        }
    }
    Ok(())
}

fn drop_markers(database_dir: &Path) -> Result<Vec<(PathBuf, TableDropMarker)>> {
    let mut markers = Vec::new();
    if !database_dir.exists() {
        return Ok(markers);
    }
    for entry in fs::read_dir(database_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(".drop-") || !name.ends_with(".json") {
            continue;
        }
        let marker: TableDropMarker = serde_json::from_slice(&fs::read(entry.path())?)?;
        rewrite_component(&marker.table)?;
        if !marker.backup_dir.starts_with(".drop-")
            || Path::new(&marker.backup_dir).components().count() != 1
        {
            anyhow::bail!("Unsafe drop path '{}'", marker.backup_dir);
        }
        markers.push((entry.path(), marker));
    }
    Ok(markers)
}

fn recover_table_drops(database_dir: &Path, schemas: &HashMap<String, TableSchema>) -> Result<()> {
    for (marker_path, marker) in drop_markers(database_dir)? {
        let active = database_dir.join(&marker.table);
        let backup = database_dir.join(&marker.backup_dir);
        if schemas.contains_key(&marker.table) {
            if !active.exists() && backup.exists() {
                fs::rename(backup, active)?;
            }
        } else if backup.exists() {
            fs::remove_dir_all(backup)?;
        }
        fs::remove_file(marker_path)?;
    }
    Ok(())
}

fn finalize_table_drops(database_dir: &Path, schemas: &HashMap<String, TableSchema>) -> Result<()> {
    for (marker_path, marker) in drop_markers(database_dir)? {
        if schemas.contains_key(&marker.table) {
            continue;
        }
        let backup = database_dir.join(marker.backup_dir);
        if backup.exists() {
            fs::remove_dir_all(backup)?;
        }
        fs::remove_file(marker_path)?;
    }
    Ok(())
}

fn recover_table_rewrites(
    database_dir: &Path,
    schemas: &HashMap<String, TableSchema>,
) -> Result<()> {
    for (marker_path, marker) in rewrite_markers(database_dir)? {
        let active = database_dir.join(&marker.table);
        let staging = database_dir.join(&marker.staging_dir);
        let backup = database_dir.join(&marker.backup_dir);
        let committed = schemas
            .get(&marker.table)
            .is_some_and(|schema| schema.generation == marker.new_generation);
        if committed {
            if !active.exists() && staging.exists() {
                fs::rename(&staging, &active)?;
            }
            if !active.exists() {
                anyhow::bail!(
                    "Committed rewrite for '{}' has no active data",
                    marker.table
                );
            }
            if backup.exists() {
                fs::remove_dir_all(&backup)?;
            }
            if staging.exists() {
                fs::remove_dir_all(&staging)?;
            }
        } else {
            if backup.exists() {
                if active.exists() {
                    fs::remove_dir_all(&active)?;
                }
                fs::rename(&backup, &active)?;
            }
            if staging.exists() {
                fs::remove_dir_all(&staging)?;
            }
        }
        fs::remove_file(marker_path)?;
    }
    Ok(())
}

fn finalize_table_rewrites(
    database_dir: &Path,
    schemas: &HashMap<String, TableSchema>,
) -> Result<()> {
    for (marker_path, marker) in rewrite_markers(database_dir)? {
        let committed = schemas
            .get(&marker.table)
            .is_some_and(|schema| schema.generation == marker.new_generation);
        if !committed {
            continue;
        }
        let backup = database_dir.join(&marker.backup_dir);
        let staging = database_dir.join(&marker.staging_dir);
        if backup.exists() {
            fs::remove_dir_all(backup)?;
        }
        if staging.exists() {
            fs::remove_dir_all(staging)?;
        }
        fs::remove_file(marker_path)?;
    }
    Ok(())
}

fn is_page_only_directory(path: &Path) -> Result<bool> {
    let mut storage_files = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();
        let legacy_page = name.starts_with("page_") && name.ends_with(".bin");
        if !file_type.is_file() || file_type.is_symlink() || (!legacy_page && name != "pages.dat") {
            return Ok(false);
        }
        storage_files += 1;
    }
    Ok(storage_files > 0)
}

fn directory_size(path: &Path) -> Result<u64> {
    if path.is_file() {
        return Ok(fs::metadata(path)?.len());
    }
    if !path.exists() {
        return Ok(0);
    }
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        bytes += if file_type.is_dir() {
            directory_size(&entry.path())?
        } else {
            entry.metadata()?.len()
        };
    }
    Ok(bytes)
}

fn row_matches(row: &Row, filter: Option<&RowPredicate>) -> bool {
    filter
        .map(|predicate| predicate.matches(row))
        .unwrap_or(true)
}

// ============================================================================
// Storage Engine Manager
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WriteCommand {
    CreateDatabase(String),
    DropDatabase(String),
    CreateTable {
        database: String,
        schema: TableSchema,
    },
    DropTable {
        database: String,
        table: String,
    },
    AlterTable {
        database: String,
        table: String,
        operation: AlterTableOperation,
    },
    Insert {
        database: String,
        table: String,
        row: Row,
    },
    Upsert {
        database: String,
        table: String,
        row: Row,
        update_columns: Vec<String>,
        ignore: bool,
    },
    Update {
        database: String,
        table: String,
        filter: Option<RowPredicate>,
        assignments: Vec<(String, Option<Vec<u8>>)>,
    },
    Delete {
        database: String,
        table: String,
        filter: Option<RowPredicate>,
    },
    CleanupOrphanStorage,
    // Appended after every legacy variant so old bincode WAL discriminants stay
    // unchanged during in-place upgrades.
    ExpressionUpdate {
        database: String,
        table: String,
        filter: Option<RowPredicate>,
        assignments: Vec<ExpressionAssignment>,
    },
    /// Idempotent full table image used only when a limited mutation must
    /// distinguish physically duplicated rows that have no unique key.
    ReplaceRows {
        database: String,
        table: String,
        rows: Vec<Row>,
        affected_rows: u64,
    },
    /// Per-request actor marker used by MySQL dump sessions. It is removed
    /// during prepare and is therefore never persisted in WAL.
    ForeignKeyChecksDisabled,
    CreateProcedure {
        database: String,
        procedure: ProcedureDefinition,
    },
    DropProcedure {
        database: String,
        procedure: String,
    },
    // 仅更新可 ALTER 的 characteristics；追加变体以保持旧 WAL discriminant 稳定。
    AlterProcedure {
        database: String,
        procedure: String,
        create_sql: String,
    },
    // V2 变体携带例程元数据；旧变体保留，确保历史 WAL 可继续解码。
    CreateProcedureV2 {
        database: String,
        procedure: ProcedureDefinition,
        metadata: ProcedureMetadata,
    },
    AlterProcedureV2 {
        database: String,
        procedure: String,
        create_sql: String,
        metadata: ProcedureMetadata,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NumericOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateValueExpression {
    Literal(Option<Vec<u8>>),
    Column(String),
    Default(String),
    Numeric {
        source_column: String,
        operator: NumericOperator,
        operand: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpressionAssignment {
    pub target_column: String,
    pub value: UpdateValueExpression,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowPredicate {
    Eq(String, Vec<u8>),
    NotEq(String, Vec<u8>),
    Less(String, Vec<u8>),
    LessOrEq(String, Vec<u8>),
    Greater(String, Vec<u8>),
    GreaterOrEq(String, Vec<u8>),
    In(String, Vec<Vec<u8>>),
    Between(String, Vec<u8>, Vec<u8>),
    Like(String, Vec<u8>),
    And(Box<RowPredicate>, Box<RowPredicate>),
    Or(Box<RowPredicate>, Box<RowPredicate>),
    IsNull(String),
    IsNotNull(String),
    Never,
}

impl RowPredicate {
    pub fn matches(&self, row: &Row) -> bool {
        match self {
            Self::Eq(column, value) => {
                !row.is_null(column) && row.get(column).is_some_and(|actual| actual == value)
            }
            Self::NotEq(column, value) => {
                !row.is_null(column) && row.get(column).is_some_and(|actual| actual != value)
            }
            Self::Less(column, value) => {
                compare_row_value(row, column, value).is_some_and(|ordering| ordering.is_lt())
            }
            Self::LessOrEq(column, value) => {
                compare_row_value(row, column, value).is_some_and(|ordering| ordering.is_le())
            }
            Self::Greater(column, value) => {
                compare_row_value(row, column, value).is_some_and(|ordering| ordering.is_gt())
            }
            Self::GreaterOrEq(column, value) => {
                compare_row_value(row, column, value).is_some_and(|ordering| ordering.is_ge())
            }
            Self::In(column, values) => {
                !row.is_null(column)
                    && row
                        .get(column)
                        .is_some_and(|actual| values.iter().any(|value| actual == value))
            }
            Self::Between(column, lower, upper) => {
                compare_row_value(row, column, lower).is_some_and(|ordering| ordering.is_ge())
                    && compare_row_value(row, column, upper)
                        .is_some_and(|ordering| ordering.is_le())
            }
            Self::Like(column, pattern) => {
                !row.is_null(column)
                    && row
                        .get(column)
                        .is_some_and(|value| mysql_like_matches(value, pattern))
            }
            Self::And(left, right) => left.matches(row) && right.matches(row),
            Self::Or(left, right) => left.matches(row) || right.matches(row),
            Self::IsNull(column) => row.contains(column) && row.is_null(column),
            Self::IsNotNull(column) => row.contains(column) && !row.is_null(column),
            Self::Never => false,
        }
    }

    pub fn columns(&self) -> Vec<&str> {
        match self {
            Self::Eq(column, _)
            | Self::NotEq(column, _)
            | Self::Less(column, _)
            | Self::LessOrEq(column, _)
            | Self::Greater(column, _)
            | Self::GreaterOrEq(column, _)
            | Self::In(column, _)
            | Self::Between(column, _, _)
            | Self::Like(column, _)
            | Self::IsNull(column)
            | Self::IsNotNull(column) => vec![column],
            Self::And(left, right) | Self::Or(left, right) => {
                let mut columns = left.columns();
                columns.extend(right.columns());
                columns
            }
            Self::Never => Vec::new(),
        }
    }
}

fn compare_row_value(row: &Row, column: &str, expected: &[u8]) -> Option<std::cmp::Ordering> {
    if row.is_null(column) {
        return None;
    }
    let actual = row.get(column)?;
    let numeric = std::str::from_utf8(actual)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .zip(
            std::str::from_utf8(expected)
                .ok()
                .and_then(|value| value.parse::<f64>().ok()),
        );
    numeric
        .and_then(|(left, right)| left.partial_cmp(&right))
        .or_else(|| Some(actual.cmp(expected)))
}

fn mysql_like_matches(value: &[u8], pattern: &[u8]) -> bool {
    fn matches(value: &[u8], pattern: &[u8], value_at: usize, pattern_at: usize) -> bool {
        if pattern_at == pattern.len() {
            return value_at == value.len();
        }
        match pattern[pattern_at] {
            b'%' => {
                matches(value, pattern, value_at, pattern_at + 1)
                    || (value_at < value.len() && matches(value, pattern, value_at + 1, pattern_at))
            }
            b'_' => value_at < value.len() && matches(value, pattern, value_at + 1, pattern_at + 1),
            b'\\' if pattern_at + 1 < pattern.len() => {
                value_at < value.len()
                    && value[value_at] == pattern[pattern_at + 1]
                    && matches(value, pattern, value_at + 1, pattern_at + 2)
            }
            byte => {
                value_at < value.len()
                    && value[value_at] == byte
                    && matches(value, pattern, value_at + 1, pattern_at + 1)
            }
        }
    }
    matches(value, pattern, 0, 0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlterTableOperation {
    IfExists(Box<AlterTableOperation>),
    IfNotExists(Box<AlterTableOperation>),
    AddColumn(Column),
    AddColumnAt {
        column: Column,
        position: ColumnPosition,
    },
    DropColumn(String),
    ModifyColumn(Column),
    ModifyColumnAt {
        column: Column,
        position: ColumnPosition,
    },
    ChangeColumn {
        old_name: String,
        column: Column,
        position: Option<ColumnPosition>,
    },
    RenameColumn {
        old_name: String,
        new_name: String,
    },
    AddPrimaryKey(Vec<String>),
    DropPrimaryKey,
    AddIndex(Index),
    DropIndex(String),
    RenameIndex {
        old_name: String,
        new_name: String,
    },
    AddForeignKey {
        name: String,
        definition: String,
    },
    DropForeignKey(String),
    AddCheck {
        name: String,
        definition: String,
    },
    DropCheck(String),
    SetColumnDefault {
        column: String,
        default: Option<String>,
    },
    AddTrigger(TriggerDefinition),
    DropTrigger(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnPosition {
    First,
    After(String),
}

enum AlterRowMutation {
    Rename { old_name: String, new_name: String },
    Drop(String),
    Materialize,
}

fn conditional_target_exists(
    schema: &TableSchema,
    operation: &AlterTableOperation,
) -> Result<bool> {
    match operation {
        AlterTableOperation::IfExists(operation) | AlterTableOperation::IfNotExists(operation) => {
            conditional_target_exists(schema, operation)
        }
        AlterTableOperation::AddColumn(column) => {
            Ok(schema.columns.iter().any(|item| item.name == column.name))
        }
        AlterTableOperation::AddColumnAt { column, .. } => {
            Ok(schema.columns.iter().any(|item| item.name == column.name))
        }
        AlterTableOperation::DropColumn(column)
        | AlterTableOperation::SetColumnDefault { column, .. } => {
            Ok(schema.columns.iter().any(|item| item.name == *column))
        }
        AlterTableOperation::AddIndex(index) => {
            Ok(schema.indexes.iter().any(|item| item.name == index.name))
        }
        AlterTableOperation::DropIndex(index) => {
            Ok(schema.indexes.iter().any(|item| item.name == *index))
        }
        _ => anyhow::bail!("Conditional ALTER is not supported for this operation"),
    }
}

fn alter_row_mutation(operation: &AlterTableOperation) -> Option<AlterRowMutation> {
    match operation {
        AlterTableOperation::IfExists(operation) | AlterTableOperation::IfNotExists(operation) => {
            alter_row_mutation(operation)
        }
        AlterTableOperation::ChangeColumn {
            old_name, column, ..
        } if old_name != &column.name => Some(AlterRowMutation::Rename {
            old_name: old_name.clone(),
            new_name: column.name.clone(),
        }),
        AlterTableOperation::RenameColumn { old_name, new_name } if old_name != new_name => {
            Some(AlterRowMutation::Rename {
                old_name: old_name.clone(),
                new_name: new_name.clone(),
            })
        }
        AlterTableOperation::DropColumn(column) => Some(AlterRowMutation::Drop(column.clone())),
        AlterTableOperation::AddColumn(_) | AlterTableOperation::AddColumnAt { .. } => {
            Some(AlterRowMutation::Materialize)
        }
        _ => None,
    }
}

fn apply_alter_row_mutation(rows: &mut [Row], mutation: &AlterRowMutation) {
    for row in rows {
        match mutation {
            AlterRowMutation::Rename { old_name, new_name } => {
                row.rename_column(old_name, new_name);
            }
            AlterRowMutation::Drop(column) => row.remove_column(column),
            AlterRowMutation::Materialize => {}
        }
    }
}

fn new_key_constraint(operation: &AlterTableOperation) -> Option<(&str, &[String], bool)> {
    match operation {
        AlterTableOperation::IfExists(operation) | AlterTableOperation::IfNotExists(operation) => {
            new_key_constraint(operation)
        }
        AlterTableOperation::AddPrimaryKey(columns) => Some(("PRIMARY", columns, false)),
        AlterTableOperation::AddIndex(index) if index.unique => {
            Some((&index.name, &index.columns, true))
        }
        _ => None,
    }
}

fn validate_rows_for_new_key(
    rows: &[Row],
    columns: &[String],
    key_name: &str,
    nullable: bool,
) -> Result<()> {
    let mut keys = HashSet::new();
    for row in rows {
        let missing = columns
            .iter()
            .find(|column| row.is_null(column) || row.get(column).is_none());
        if let Some(column) = missing {
            if nullable {
                continue;
            }
            anyhow::bail!("Invalid use of NULL value in column '{}'", column);
        }
        let key = encode_key(row, columns, nullable)
            .ok_or_else(|| anyhow::anyhow!("Unable to encode key '{}'", key_name))?;
        if !keys.insert(key) {
            anyhow::bail!("Duplicate entry for key '{}'", key_name);
        }
    }
    Ok(())
}

fn column_insert_index(schema: &TableSchema, position: &ColumnPosition) -> Result<usize> {
    match position {
        ColumnPosition::First => Ok(0),
        ColumnPosition::After(reference) => schema
            .columns
            .iter()
            .position(|column| column.name == *reference)
            .map(|index| index + 1)
            .ok_or_else(|| anyhow::anyhow!("Unknown column '{}' in 'after clause'", reference)),
    }
}

fn replace_column_references(schema: &mut TableSchema, old_name: &str, new_name: &str) {
    if let Some(primary_key) = &mut schema.primary_key {
        for column in primary_key {
            if column == old_name {
                *column = new_name.to_string();
            }
        }
    }
    for index in &mut schema.indexes {
        for column in &mut index.columns {
            if column == old_name {
                *column = new_name.to_string();
            }
        }
    }
}

fn apply_alter_schema(
    schema: &mut TableSchema,
    operation: &AlterTableOperation,
) -> Result<Option<(String, String)>> {
    match operation {
        AlterTableOperation::IfExists(operation) => {
            if !conditional_target_exists(schema, operation)? {
                return Ok(None);
            }
            return apply_alter_schema(schema, operation);
        }
        AlterTableOperation::IfNotExists(operation) => {
            if conditional_target_exists(schema, operation)? {
                return Ok(None);
            }
            return apply_alter_schema(schema, operation);
        }
        _ => {}
    }
    let original_auto_increment = auto_increment_column(schema);
    let mut constraints = table_constraint_definitions(schema);
    let mut renamed_column = None;
    match operation {
        AlterTableOperation::IfExists(_) | AlterTableOperation::IfNotExists(_) => unreachable!(),
        AlterTableOperation::AddColumn(column) => {
            if schema.columns.iter().any(|item| item.name == column.name) {
                anyhow::bail!("Duplicate column '{}'", column.name);
            }
            schema.columns.push(column.clone());
        }
        AlterTableOperation::AddColumnAt { column, position } => {
            if schema.columns.iter().any(|item| item.name == column.name) {
                anyhow::bail!("Duplicate column '{}'", column.name);
            }
            let index = column_insert_index(schema, position)?;
            schema.columns.insert(index, column.clone());
        }
        AlterTableOperation::DropColumn(column) => {
            let before = schema.columns.len();
            schema.columns.retain(|item| item.name != *column);
            if schema.columns.len() == before {
                anyhow::bail!("Unknown column '{}'", column);
            }
        }
        AlterTableOperation::ModifyColumn(column) => {
            let item = schema
                .columns
                .iter_mut()
                .find(|item| item.name == column.name)
                .ok_or_else(|| anyhow::anyhow!("Unknown column '{}'", column.name))?;
            *item = column.clone();
        }
        AlterTableOperation::ModifyColumnAt { column, position } => {
            let index = schema
                .columns
                .iter()
                .position(|item| item.name == column.name)
                .ok_or_else(|| anyhow::anyhow!("Unknown column '{}'", column.name))?;
            schema.columns.remove(index);
            let target = column_insert_index(schema, position)?;
            schema.columns.insert(target, column.clone());
        }
        AlterTableOperation::ChangeColumn {
            old_name,
            column,
            position,
        } => {
            let index = schema
                .columns
                .iter()
                .position(|item| item.name == *old_name)
                .ok_or_else(|| anyhow::anyhow!("Unknown column '{}'", old_name))?;
            if old_name != &column.name
                && schema.columns.iter().any(|item| item.name == column.name)
            {
                anyhow::bail!("Duplicate column '{}'", column.name);
            }
            schema.columns.remove(index);
            let target = match position {
                Some(position) => column_insert_index(schema, position)?,
                None => index.min(schema.columns.len()),
            };
            schema.columns.insert(target, column.clone());
            replace_column_references(schema, old_name, &column.name);
            if old_name != &column.name {
                renamed_column = Some((old_name.clone(), column.name.clone()));
            }
        }
        AlterTableOperation::RenameColumn { old_name, new_name } => {
            if old_name != new_name && schema.columns.iter().any(|item| item.name == *new_name) {
                anyhow::bail!("Duplicate column '{}'", new_name);
            }
            let item = schema
                .columns
                .iter_mut()
                .find(|item| item.name == *old_name)
                .ok_or_else(|| anyhow::anyhow!("Unknown column '{}'", old_name))?;
            item.name = new_name.clone();
            replace_column_references(schema, old_name, new_name);
            if old_name != new_name {
                renamed_column = Some((old_name.clone(), new_name.clone()));
            }
        }
        AlterTableOperation::AddPrimaryKey(columns) => {
            if !primary_key_columns(schema).is_empty() {
                anyhow::bail!("Multiple primary key defined");
            }
            if columns.is_empty() {
                anyhow::bail!("A primary key must include at least one column");
            }
            for column in columns {
                validate_column_exists(schema, column)?;
            }
            for column in &mut schema.columns {
                column.is_primary_key = false;
                if columns.contains(&column.name) {
                    column.nullable = false;
                }
            }
            schema.primary_key = Some(columns.clone());
        }
        AlterTableOperation::DropPrimaryKey => {
            if primary_key_columns(schema).is_empty() {
                anyhow::bail!("Can't DROP 'PRIMARY'; check that column/key exists");
            }
            schema.primary_key = None;
            for column in &mut schema.columns {
                column.is_primary_key = false;
            }
        }
        AlterTableOperation::AddIndex(index) => {
            if schema.indexes.iter().any(|item| item.name == index.name) {
                anyhow::bail!("Duplicate key name '{}'", index.name);
            }
            for column in &index.columns {
                validate_column_exists(schema, column)?;
            }
            schema.indexes.push(index.clone());
        }
        AlterTableOperation::DropIndex(index) => {
            let before = schema.indexes.len();
            schema.indexes.retain(|item| item.name != *index);
            if schema.indexes.len() == before {
                anyhow::bail!("Can't DROP '{}'; check that index exists", index);
            }
        }
        AlterTableOperation::RenameIndex { old_name, new_name } => {
            if old_name != new_name && schema.indexes.iter().any(|item| item.name == *new_name) {
                anyhow::bail!("Duplicate key name '{}'", new_name);
            }
            let position = schema
                .indexes
                .iter()
                .position(|item| item.name == *old_name)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Key '{}' doesn't exist in table '{}'",
                        old_name,
                        schema.name
                    )
                })?;
            schema.indexes[position].name = new_name.clone();
        }
        AlterTableOperation::AddForeignKey { name, definition } => {
            if constraints.iter().any(|constraint| {
                parse_foreign_key_definition(&schema.name, constraint)
                    .is_some_and(|foreign_key| foreign_key.name == *name)
            }) {
                anyhow::bail!("Duplicate foreign key constraint name '{}'", name);
            }
            let foreign_key = parse_foreign_key_definition(&schema.name, definition)
                .ok_or_else(|| anyhow::anyhow!("Invalid FOREIGN KEY definition"))?;
            for column in &foreign_key.child_columns {
                validate_column_exists(schema, column)?;
            }
            constraints.push(definition.clone());
        }
        AlterTableOperation::DropForeignKey(name) => {
            let before = constraints.len();
            constraints.retain(|constraint| {
                parse_foreign_key_definition(&schema.name, constraint)
                    .is_none_or(|foreign_key| foreign_key.name != *name)
            });
            if constraints.len() == before {
                anyhow::bail!("Can't DROP '{}'; check that column/key exists", name);
            }
        }
        AlterTableOperation::AddCheck { name, definition } => {
            if constraints.iter().enumerate().any(|(index, constraint)| {
                check_constraint_name(&schema.name, constraint, index) == Some(name.clone())
            }) {
                anyhow::bail!("Duplicate check constraint name '{}'", name);
            }
            if check_constraint_name(&schema.name, definition, constraints.len()).is_none() {
                anyhow::bail!("Invalid CHECK definition");
            }
            constraints.push(definition.clone());
        }
        AlterTableOperation::DropCheck(name) => {
            let mut index = 0;
            let before = constraints.len();
            constraints.retain(|constraint| {
                let keep =
                    check_constraint_name(&schema.name, constraint, index).as_deref() != Some(name);
                index += 1;
                keep
            });
            if constraints.len() == before {
                anyhow::bail!(
                    "Check constraint '{}' is not found in table '{}'",
                    name,
                    schema.name
                );
            }
        }
        AlterTableOperation::SetColumnDefault { column, default } => {
            let target = schema
                .columns
                .iter_mut()
                .find(|item| item.name == *column)
                .ok_or_else(|| anyhow::anyhow!("Unknown column '{}'", column))?;
            target.default = default.clone();
        }
        AlterTableOperation::AddTrigger(trigger) => {
            if schema
                .triggers
                .iter()
                .any(|item| item.name.eq_ignore_ascii_case(&trigger.name))
            {
                anyhow::bail!("Trigger '{}' already exists", trigger.name);
            }
            schema.triggers.push(trigger.clone());
        }
        AlterTableOperation::DropTrigger(name) => {
            let before = schema.triggers.len();
            schema
                .triggers
                .retain(|trigger| !trigger.name.eq_ignore_ascii_case(name));
            if schema.triggers.len() == before {
                anyhow::bail!("Trigger '{}' does not exist", name);
            }
        }
    }
    let auto_increment = original_auto_increment.and_then(|name| {
        let renamed = renamed_column
            .as_ref()
            .filter(|(old_name, _)| old_name == &name)
            .map(|(_, new_name)| new_name.clone())
            .unwrap_or(name);
        schema
            .columns
            .iter()
            .any(|column| column.name == renamed)
            .then_some(renamed)
    });
    schema.create_sql = Some(render_table_schema_sql(
        schema,
        auto_increment.as_deref(),
        &constraints,
    ));
    Ok(renamed_column)
}

fn column_matches_at(
    schema: &TableSchema,
    column: &Column,
    position: Option<&ColumnPosition>,
) -> bool {
    let Some(index) = schema
        .columns
        .iter()
        .position(|item| item.name == column.name)
    else {
        return false;
    };
    if serde_json::to_vec(&schema.columns[index]).ok() != serde_json::to_vec(column).ok() {
        return false;
    }
    match position {
        None => true,
        Some(ColumnPosition::First) => index == 0,
        Some(ColumnPosition::After(reference)) => {
            index > 0 && schema.columns[index - 1].name == *reference
        }
    }
}

fn alter_operation_applied(schema: &TableSchema, operation: &AlterTableOperation) -> bool {
    match operation {
        AlterTableOperation::IfExists(operation) | AlterTableOperation::IfNotExists(operation) => {
            alter_operation_applied(schema, operation)
        }
        AlterTableOperation::AddColumn(column) | AlterTableOperation::ModifyColumn(column) => {
            column_matches_at(schema, column, None)
        }
        AlterTableOperation::AddColumnAt { column, position }
        | AlterTableOperation::ModifyColumnAt { column, position } => {
            column_matches_at(schema, column, Some(position))
        }
        AlterTableOperation::ChangeColumn {
            old_name,
            column,
            position,
        } => {
            (old_name == &column.name || !schema.columns.iter().any(|item| item.name == *old_name))
                && column_matches_at(schema, column, position.as_ref())
        }
        AlterTableOperation::RenameColumn { old_name, new_name } => {
            (old_name == new_name && schema.columns.iter().any(|item| item.name == *old_name))
                || (!schema.columns.iter().any(|item| item.name == *old_name)
                    && schema.columns.iter().any(|item| item.name == *new_name))
        }
        AlterTableOperation::AddPrimaryKey(columns) => primary_key_columns(schema) == *columns,
        AlterTableOperation::DropPrimaryKey => primary_key_columns(schema).is_empty(),
        AlterTableOperation::DropColumn(column) => {
            !schema.columns.iter().any(|item| item.name == *column)
        }
        AlterTableOperation::AddIndex(index) => schema
            .indexes
            .iter()
            .find(|item| item.name == index.name)
            .is_some_and(|item| serde_json::to_vec(item).ok() == serde_json::to_vec(index).ok()),
        AlterTableOperation::DropIndex(index) => {
            !schema.indexes.iter().any(|item| item.name == *index)
        }
        AlterTableOperation::RenameIndex { old_name, new_name } => {
            (old_name == new_name && schema.indexes.iter().any(|item| item.name == *old_name))
                || (!schema.indexes.iter().any(|item| item.name == *old_name)
                    && schema.indexes.iter().any(|item| item.name == *new_name))
        }
        AlterTableOperation::AddForeignKey { name, .. } => table_foreign_keys(&schema.name, schema)
            .iter()
            .any(|foreign_key| foreign_key.name == *name),
        AlterTableOperation::DropForeignKey(name) => !table_foreign_keys(&schema.name, schema)
            .iter()
            .any(|foreign_key| foreign_key.name == *name),
        AlterTableOperation::AddCheck { name, .. } => table_check_constraints(schema)
            .iter()
            .any(|(check_name, _)| check_name == name),
        AlterTableOperation::DropCheck(name) => !table_check_constraints(schema)
            .iter()
            .any(|(check_name, _)| check_name == name),
        AlterTableOperation::SetColumnDefault { column, default } => schema
            .columns
            .iter()
            .find(|item| item.name == *column)
            .is_some_and(|item| item.default == *default),
        AlterTableOperation::AddTrigger(trigger) => schema
            .triggers
            .iter()
            .any(|item| item.name.eq_ignore_ascii_case(&trigger.name)),
        AlterTableOperation::DropTrigger(name) => !schema
            .triggers
            .iter()
            .any(|item| item.name.eq_ignore_ascii_case(name)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteResult {
    pub affected_rows: u64,
    pub last_insert_id: u64,
}

struct WriteRequest {
    commands: Vec<WriteCommand>,
    prepared: bool,
    reply: oneshot::Sender<Result<WriteResult>>,
}

struct PrepareRequest {
    commands: Vec<WriteCommand>,
    prepared_prefix_len: usize,
    reply: oneshot::Sender<Result<PreparedTransactionBatch>>,
}

enum ActorRequest {
    Write(WriteRequest),
    Prepare(PrepareRequest),
}

#[derive(Debug)]
pub struct PreparedTransactionBatch {
    pub commands: Vec<WriteCommand>,
    pub last_insert_id: u64,
}

const WAL_BATCH_VERSION: u16 = 2;
const WAL_BATCH_BINARY_MAGIC: &[u8; 4] = b"MDB2";
const WAL_GROUP_VERSION: u16 = 2;
const WAL_GROUP_BINARY_MAGIC_V1: &[u8; 4] = b"MDG1";
const WAL_GROUP_BINARY_MAGIC_V2: &[u8; 4] = b"MDG2";

#[derive(Debug, Serialize, Deserialize)]
struct WalBatch {
    version: u16,
    commands: Vec<WriteCommand>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyTableSchema {
    name: String,
    columns: Vec<Column>,
    primary_key: Option<Vec<String>>,
    indexes: Vec<Index>,
    next_page_number: u32,
    generation: u64,
    create_sql: Option<String>,
    engine: TableEngine,
}

impl From<LegacyTableSchema> for TableSchema {
    fn from(value: LegacyTableSchema) -> Self {
        Self {
            name: value.name,
            columns: value.columns,
            primary_key: value.primary_key,
            indexes: value.indexes,
            next_page_number: value.next_page_number,
            generation: value.generation,
            create_sql: value.create_sql,
            engine: value.engine,
            triggers: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum LegacyWriteCommand {
    CreateDatabase(String),
    DropDatabase(String),
    CreateTable {
        database: String,
        schema: LegacyTableSchema,
    },
    DropTable {
        database: String,
        table: String,
    },
    AlterTable {
        database: String,
        table: String,
        operation: AlterTableOperation,
    },
    Insert {
        database: String,
        table: String,
        row: Row,
    },
    Upsert {
        database: String,
        table: String,
        row: Row,
        update_columns: Vec<String>,
        ignore: bool,
    },
    Update {
        database: String,
        table: String,
        filter: Option<RowPredicate>,
        assignments: Vec<(String, Option<Vec<u8>>)>,
    },
    Delete {
        database: String,
        table: String,
        filter: Option<RowPredicate>,
    },
    CleanupOrphanStorage,
    ExpressionUpdate {
        database: String,
        table: String,
        filter: Option<RowPredicate>,
        assignments: Vec<ExpressionAssignment>,
    },
    ReplaceRows {
        database: String,
        table: String,
        rows: Vec<Row>,
        affected_rows: u64,
    },
    ForeignKeyChecksDisabled,
}

impl From<LegacyWriteCommand> for WriteCommand {
    fn from(value: LegacyWriteCommand) -> Self {
        match value {
            LegacyWriteCommand::CreateDatabase(name) => Self::CreateDatabase(name),
            LegacyWriteCommand::DropDatabase(name) => Self::DropDatabase(name),
            LegacyWriteCommand::CreateTable { database, schema } => Self::CreateTable {
                database,
                schema: schema.into(),
            },
            LegacyWriteCommand::DropTable { database, table } => {
                Self::DropTable { database, table }
            }
            LegacyWriteCommand::AlterTable {
                database,
                table,
                operation,
            } => Self::AlterTable {
                database,
                table,
                operation,
            },
            LegacyWriteCommand::Insert {
                database,
                table,
                row,
            } => Self::Insert {
                database,
                table,
                row,
            },
            LegacyWriteCommand::Upsert {
                database,
                table,
                row,
                update_columns,
                ignore,
            } => Self::Upsert {
                database,
                table,
                row,
                update_columns,
                ignore,
            },
            LegacyWriteCommand::Update {
                database,
                table,
                filter,
                assignments,
            } => Self::Update {
                database,
                table,
                filter,
                assignments,
            },
            LegacyWriteCommand::Delete {
                database,
                table,
                filter,
            } => Self::Delete {
                database,
                table,
                filter,
            },
            LegacyWriteCommand::CleanupOrphanStorage => Self::CleanupOrphanStorage,
            LegacyWriteCommand::ExpressionUpdate {
                database,
                table,
                filter,
                assignments,
            } => Self::ExpressionUpdate {
                database,
                table,
                filter,
                assignments,
            },
            LegacyWriteCommand::ReplaceRows {
                database,
                table,
                rows,
                affected_rows,
            } => Self::ReplaceRows {
                database,
                table,
                rows,
                affected_rows,
            },
            LegacyWriteCommand::ForeignKeyChecksDisabled => Self::ForeignKeyChecksDisabled,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyWalBatch {
    version: u16,
    commands: Vec<LegacyWriteCommand>,
}

#[cfg(test)]
#[derive(Serialize)]
struct WalBatchRef<'a> {
    version: u16,
    commands: &'a [WriteCommand],
}

#[derive(Debug)]
struct WalGroup {
    committed_unix_ms: Option<u64>,
    transactions: Vec<Vec<WriteCommand>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WalGroupV1 {
    version: u16,
    transactions: Vec<Vec<WriteCommand>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyWalGroupV1 {
    version: u16,
    transactions: Vec<Vec<LegacyWriteCommand>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WalGroupV2 {
    version: u16,
    committed_unix_ms: u64,
    transactions: Vec<Vec<WriteCommand>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyWalGroupV2 {
    version: u16,
    committed_unix_ms: u64,
    transactions: Vec<Vec<LegacyWriteCommand>>,
}

#[derive(Serialize)]
struct WalGroupRefV2<'a> {
    version: u16,
    committed_unix_ms: u64,
    transactions: &'a [&'a [WriteCommand]],
}

#[cfg(test)]
fn encode_wal_batch(batch: &WalBatch) -> Result<Vec<u8>> {
    encode_wal_commands(batch.version, &batch.commands)
}

#[cfg(test)]
fn encode_wal_commands(version: u16, commands: &[WriteCommand]) -> Result<Vec<u8>> {
    let encoded = bincode::serialize(&WalBatchRef { version, commands })?;
    let mut payload = Vec::with_capacity(WAL_BATCH_BINARY_MAGIC.len() + encoded.len());
    payload.extend_from_slice(WAL_BATCH_BINARY_MAGIC);
    payload.extend_from_slice(&encoded);
    Ok(payload)
}

fn decode_wal_batch(payload: &[u8]) -> Result<WalBatch> {
    let batch: WalBatch = if let Some(encoded) = payload.strip_prefix(WAL_BATCH_BINARY_MAGIC) {
        match bincode::deserialize(encoded) {
            Ok(batch) => batch,
            Err(current_error) => {
                let legacy: LegacyWalBatch = bincode::deserialize(encoded).map_err(|legacy_error| {
                    anyhow::anyhow!(
                        "cannot decode current WAL batch ({current_error}) or legacy WAL batch ({legacy_error})"
                    )
                })?;
                WalBatch {
                    version: legacy.version,
                    commands: legacy.commands.into_iter().map(Into::into).collect(),
                }
            }
        }
    } else {
        // Version 1 used JSON. Keep it readable for in-place upgrades.
        serde_json::from_slice(payload)?
    };
    if !matches!(batch.version, 1 | WAL_BATCH_VERSION) {
        anyhow::bail!("unsupported WAL batch version {}", batch.version);
    }
    Ok(batch)
}

fn encode_wal_group(transactions: &[&[WriteCommand]]) -> Result<Vec<u8>> {
    let committed_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let encoded = bincode::serialize(&WalGroupRefV2 {
        version: WAL_GROUP_VERSION,
        committed_unix_ms,
        transactions,
    })?;
    let mut payload = Vec::with_capacity(WAL_GROUP_BINARY_MAGIC_V2.len() + encoded.len());
    payload.extend_from_slice(WAL_GROUP_BINARY_MAGIC_V2);
    payload.extend_from_slice(&encoded);
    Ok(payload)
}

fn decode_wal_group(payload: &[u8]) -> Result<WalGroup> {
    if let Some(encoded) = payload.strip_prefix(WAL_GROUP_BINARY_MAGIC_V2) {
        let group: WalGroupV2 = match bincode::deserialize(encoded) {
            Ok(group) => group,
            Err(current_error) => {
                let legacy: LegacyWalGroupV2 =
                    bincode::deserialize(encoded).map_err(|legacy_error| {
                        anyhow::anyhow!(
                            "cannot decode current WAL group ({current_error}) or legacy WAL group ({legacy_error})"
                        )
                    })?;
                WalGroupV2 {
                    version: legacy.version,
                    committed_unix_ms: legacy.committed_unix_ms,
                    transactions: legacy
                        .transactions
                        .into_iter()
                        .map(|commands| commands.into_iter().map(Into::into).collect())
                        .collect(),
                }
            }
        };
        if group.version != WAL_GROUP_VERSION {
            anyhow::bail!("unsupported WAL group version {}", group.version);
        }
        return Ok(WalGroup {
            committed_unix_ms: Some(group.committed_unix_ms),
            transactions: group.transactions,
        });
    }
    if let Some(encoded) = payload.strip_prefix(WAL_GROUP_BINARY_MAGIC_V1) {
        let group: WalGroupV1 = match bincode::deserialize(encoded) {
            Ok(group) => group,
            Err(current_error) => {
                let legacy: LegacyWalGroupV1 =
                    bincode::deserialize(encoded).map_err(|legacy_error| {
                        anyhow::anyhow!(
                            "cannot decode current legacy WAL group ({current_error}) or pre-trigger WAL group ({legacy_error})"
                        )
                    })?;
                WalGroupV1 {
                    version: legacy.version,
                    transactions: legacy
                        .transactions
                        .into_iter()
                        .map(|commands| commands.into_iter().map(Into::into).collect())
                        .collect(),
                }
            }
        };
        if group.version != 1 {
            anyhow::bail!("unsupported legacy WAL group version {}", group.version);
        }
        return Ok(WalGroup {
            committed_unix_ms: None,
            transactions: group.transactions,
        });
    }
    anyhow::bail!("invalid WAL group magic")
}

/// Commit timestamp carried by MDG2 actor groups. Legacy MDG1 records return
/// `None` and remain fully replayable.
pub fn wal_group_commit_unix_ms(record: &WalRecord) -> Option<u64> {
    (record.record_type == WalRecordType::GroupCommit)
        .then(|| decode_wal_group(&record.data).ok()?.committed_unix_ms)
        .flatten()
}

struct PreparedWrite {
    tx_id: u64,
    commands: Vec<WriteCommand>,
    last_insert_id: u64,
    reply: oneshot::Sender<Result<WriteResult>>,
}

struct PendingWrite {
    commands: Vec<WriteCommand>,
    prepared: bool,
    last_insert_id: u64,
    reply: oneshot::Sender<Result<WriteResult>>,
}

#[derive(Default)]
struct ActorCheckpointState {
    tables: HashSet<(String, String)>,
    transactions: Vec<u64>,
    groups_since_checkpoint: usize,
}

impl ActorCheckpointState {
    fn clear(&mut self) {
        self.tables.clear();
        self.transactions.clear();
        self.groups_since_checkpoint = 0;
    }
}

struct ActorCoordination {
    snapshot_barrier: Arc<tokio::sync::RwLock<()>>,
    checkpoint_state: parking_lot::Mutex<ActorCheckpointState>,
    group_commit_window: Duration,
}

pub struct StorageStats {
    started: Instant,
    reads: AtomicU64,
    writes: AtomicU64,
    errors: AtomicU64,
    queue_depth: AtomicUsize,
    group_commits: AtomicU64,
    grouped_requests: AtomicU64,
    checkpoints: AtomicU64,
    checkpoint_errors: AtomicU64,
    prepare_validation_micros: AtomicU64,
    wal_sync_micros: AtomicU64,
    apply_micros: AtomicU64,
    checkpoint_micros: AtomicU64,
}

impl Default for StorageStats {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            queue_depth: AtomicUsize::new(0),
            group_commits: AtomicU64::new(0),
            grouped_requests: AtomicU64::new(0),
            checkpoints: AtomicU64::new(0),
            checkpoint_errors: AtomicU64::new(0),
            prepare_validation_micros: AtomicU64::new(0),
            wal_sync_micros: AtomicU64::new(0),
            apply_micros: AtomicU64::new(0),
            checkpoint_micros: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StorageStatsSnapshot {
    pub uptime_seconds: u64,
    pub reads: u64,
    pub writes: u64,
    pub errors: u64,
    pub actor_queue_depth: usize,
    pub buffer_pool_pages: usize,
    pub group_commits: u64,
    pub grouped_requests: u64,
    pub checkpoints: u64,
    pub checkpoint_errors: u64,
    pub prepare_validation_micros: u64,
    pub wal_sync_micros: u64,
    pub apply_micros: u64,
    pub checkpoint_micros: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrphanStorage {
    pub database: String,
    pub table: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DatabaseStorageInventory {
    pub database: String,
    pub bytes: u64,
    pub active_tables: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageInventory {
    pub data_dir: String,
    pub total_bytes: u64,
    pub wal_bytes: u64,
    pub active_bytes: u64,
    pub orphan_bytes: u64,
    pub databases: Vec<DatabaseStorageInventory>,
    pub orphan_storage: Vec<OrphanStorage>,
}

pub struct StorageEngineManager {
    databases: Arc<RwLock<HashMap<String, Arc<Database>>>>,
    buffer_pool: Arc<BufferPool>,
    wal_writer: Arc<parking_lot::Mutex<WalWriter>>,
    data_dir: PathBuf,
    writer_tx: mpsc::Sender<ActorRequest>,
    stats: Arc<StorageStats>,
    actor_coordination: Arc<ActorCoordination>,
}

impl StorageEngineManager {
    pub fn new(data_dir: PathBuf, page_size: usize, buffer_pool_size: &str) -> Self {
        Self::try_new(data_dir, page_size, buffer_pool_size).expect("Failed to open WAL")
    }

    pub fn try_new(data_dir: PathBuf, page_size: usize, buffer_pool_size: &str) -> Result<Self> {
        // Keep embedding callers on the historical immediate-commit path.
        // mydb-server supplies its configured collection window explicitly.
        Self::try_new_with_actor_mode(data_dir, page_size, buffer_pool_size, Duration::ZERO, false)
    }

    pub fn try_new_with_group_commit_window(
        data_dir: PathBuf,
        page_size: usize,
        buffer_pool_size: &str,
        group_commit_window: Duration,
    ) -> Result<Self> {
        Self::try_new_with_actor_mode(
            data_dir,
            page_size,
            buffer_pool_size,
            group_commit_window,
            true,
        )
    }

    fn try_new_with_actor_mode(
        data_dir: PathBuf,
        page_size: usize,
        buffer_pool_size: &str,
        group_commit_window: Duration,
        dedicated_actor: bool,
    ) -> Result<Self> {
        let buffer_pool = Arc::new(BufferPool::new(page_size, buffer_pool_size));

        // Initialize WAL
        let wal_dir = data_dir.join("wal");
        let wal_writer = Arc::new(parking_lot::Mutex::new(WalWriter::open(wal_dir, None)?));

        let databases = Arc::new(RwLock::new(HashMap::new()));
        let stats = Arc::new(StorageStats::default());
        let actor_coordination = Arc::new(ActorCoordination {
            snapshot_barrier: Arc::new(tokio::sync::RwLock::new(())),
            checkpoint_state: parking_lot::Mutex::new(ActorCheckpointState::default()),
            group_commit_window,
        });
        let (writer_tx, writer_rx) = mpsc::channel(8192);
        let actor_databases = databases.clone();
        let actor_buffer_pool = buffer_pool.clone();
        let actor_wal_writer = wal_writer.clone();
        let actor_data_dir = data_dir.clone();
        let actor_stats = stats.clone();
        let actor_actor_coordination = actor_coordination.clone();
        let actor = write_actor(
            writer_rx,
            actor_databases,
            actor_buffer_pool,
            actor_wal_writer,
            actor_data_dir,
            actor_stats,
            actor_actor_coordination,
        );
        if dedicated_actor {
            let actor_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            std::thread::Builder::new()
                .name("mydb-write-actor".into())
                .spawn(move || actor_runtime.block_on(actor))?;
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(actor);
        } else {
            let actor_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            std::thread::Builder::new()
                .name("mydb-write-actor".into())
                .spawn(move || actor_runtime.block_on(actor))?;
        }

        Ok(Self {
            databases,
            buffer_pool,
            wal_writer,
            data_dir,
            writer_tx,
            stats,
            actor_coordination,
        })
    }

    pub async fn init(&self) -> Result<()> {
        // Create directories
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.data_dir.join("wal"))?;

        // Load the durable table/catalog state before applying committed WAL redo.
        let entries = std::fs::read_dir(&self.data_dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if name == "wal" || !path.join("schema.json").is_file() {
                    continue;
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

        self.wal_replay().await?;
        self.cleanup_stale_temporary_tables().await?;

        info!(
            "Storage engine initialized with {} databases",
            self.databases.read().len()
        );
        Ok(())
    }

    async fn cleanup_stale_temporary_tables(&self) -> Result<()> {
        let databases = self.databases.read().values().cloned().collect::<Vec<_>>();
        let mut removed = 0_u64;
        for database in databases {
            let stale = database
                .list_tables()
                .into_iter()
                .filter(|table| table.starts_with("__mydb_tmp_"))
                .collect::<Vec<_>>();
            if stale.is_empty() {
                continue;
            }
            for table in stale {
                database.drop_table(&table)?;
                removed += 1;
            }
            database.save().await?;
        }
        if removed > 0 {
            info!("Removed {} stale temporary tables during startup", removed);
        }
        Ok(())
    }

    async fn wal_replay(&self) -> Result<()> {
        let wal_dir = self.data_dir.join("wal");
        let reader = WalReader::open(wal_dir)?;

        let mut records = Vec::new();
        reader.replay(|record| records.push(record))?;
        let committed: HashSet<u64> = records
            .iter()
            .filter(|record| record.record_type == WalRecordType::Commit)
            .map(|record| record.tx_id)
            .collect();
        let applied: HashSet<u64> = records
            .iter()
            .filter(|record| record.record_type == WalRecordType::Applied)
            .map(|record| record.tx_id)
            .collect();
        let mut batches = Vec::new();
        for record in records {
            if applied.contains(&record.tx_id) {
                continue;
            }
            let commands = match record.record_type {
                WalRecordType::Batch if committed.contains(&record.tx_id) => {
                    decode_wal_batch(&record.data)
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "cannot decode WAL transaction {}: {error}",
                                record.tx_id
                            )
                        })?
                        .commands
                }
                WalRecordType::GroupCommit => decode_wal_group(&record.data)
                    .map_err(|error| {
                        anyhow::anyhow!("cannot decode WAL group {}: {error}", record.tx_id)
                    })?
                    .transactions
                    .into_iter()
                    .flatten()
                    .collect(),
                _ => continue,
            };
            batches.push((record.lsn, record.tx_id, commands));
        }
        batches.sort_by_key(|(lsn, _, _)| *lsn);

        let mut recovered = 0u64;
        for (_, tx_id, commands) in batches {
            let commands = normalize_replay_commands(commands, &self.databases)?;
            if !commands.is_empty() {
                let tables = commands
                    .iter()
                    .filter_map(|command| match command {
                        WriteCommand::Insert {
                            database, table, ..
                        }
                        | WriteCommand::Upsert {
                            database, table, ..
                        }
                        | WriteCommand::Update {
                            database, table, ..
                        }
                        | WriteCommand::ExpressionUpdate {
                            database, table, ..
                        }
                        | WriteCommand::ReplaceRows {
                            database, table, ..
                        }
                        | WriteCommand::Delete {
                            database, table, ..
                        } => Some((database.clone(), table.clone())),
                        _ => None,
                    })
                    .collect::<HashSet<_>>();
                apply_write_batch(
                    commands,
                    &self.databases,
                    &self.buffer_pool,
                    &self.wal_writer,
                    &self.data_dir,
                    true,
                    true,
                )
                .await?;
                for (database, table) in tables {
                    if let Some(db) = self.databases.read().get(&database).cloned() {
                        if db.get_table(&table).is_some() {
                            db.checkpoint_table(&table)?;
                        }
                    }
                }
            }
            let mut applied_record =
                WalRecord::new(0, WalRecordType::Applied, tx_id, "", Vec::new());
            self.wal_writer.lock().append(&mut applied_record)?;
            recovered += 1;
        }
        if recovered > 0 {
            self.wal_writer.lock().sync()?;
            info!("WAL redid {} committed transaction(s)", recovered);
        }
        Ok(())
    }

    pub async fn create_database(&self, name: &str) -> Result<()> {
        self.execute_write(WriteCommand::CreateDatabase(name.to_string()))
            .await?;
        Ok(())
    }

    pub async fn drop_database(&self, name: &str) -> Result<()> {
        self.execute_write(WriteCommand::DropDatabase(name.to_string()))
            .await?;
        Ok(())
    }

    pub async fn execute_write(&self, command: WriteCommand) -> Result<WriteResult> {
        self.execute_batch(vec![command]).await
    }

    pub async fn execute_batch(&self, commands: Vec<WriteCommand>) -> Result<WriteResult> {
        self.send_write_batch(commands, false).await
    }

    pub async fn execute_prepared_batch(&self, commands: Vec<WriteCommand>) -> Result<WriteResult> {
        self.send_write_batch(commands, true).await
    }

    async fn send_write_batch(
        &self,
        commands: Vec<WriteCommand>,
        prepared: bool,
    ) -> Result<WriteResult> {
        let (reply, result) = oneshot::channel();
        self.stats.queue_depth.fetch_add(1, Ordering::Relaxed);
        if self
            .writer_tx
            .send(ActorRequest::Write(WriteRequest {
                commands,
                prepared,
                reply,
            }))
            .await
            .is_err()
        {
            self.stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
            anyhow::bail!("write actor stopped");
        }
        result
            .await
            .map_err(|_| anyhow::anyhow!("write actor stopped"))?
    }

    pub async fn prepare_transaction_batch(
        &self,
        commands: Vec<WriteCommand>,
        prepared_prefix_len: usize,
    ) -> Result<PreparedTransactionBatch> {
        let (reply, result) = oneshot::channel();
        self.stats.queue_depth.fetch_add(1, Ordering::Relaxed);
        if self
            .writer_tx
            .send(ActorRequest::Prepare(PrepareRequest {
                commands,
                prepared_prefix_len,
                reply,
            }))
            .await
            .is_err()
        {
            self.stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
            anyhow::bail!("write actor stopped");
        }
        result
            .await
            .map_err(|_| anyhow::anyhow!("write actor stopped"))?
    }

    pub fn get_database(&self, name: &str) -> Option<Arc<Database>> {
        self.databases.read().get(name).cloned()
    }

    pub fn is_memory_table(&self, database: &str, table: &str) -> bool {
        self.get_database(database)
            .is_some_and(|database| database.is_memory_table(table))
    }

    pub fn list_databases(&self) -> Vec<String> {
        let mut names: Vec<_> = self.databases.read().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn referential_tables_for_commands(
        &self,
        commands: &[WriteCommand],
    ) -> Result<Vec<(String, String)>> {
        let mut touched = HashMap::<String, HashSet<String>>::new();
        for command in commands {
            if let Some((database, table)) = command_table(command) {
                touched
                    .entry(database.to_string())
                    .or_default()
                    .insert(table.to_string());
            }
        }
        let mut related = HashSet::new();
        for (database_name, touched_tables) in touched {
            let database = self
                .get_database(&database_name)
                .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database_name))?;
            let foreign_keys = database
                .tables
                .read()
                .iter()
                .flat_map(|(table, schema)| table_foreign_keys(table, schema))
                .collect::<Vec<_>>();
            if foreign_keys.iter().any(|key| {
                touched_tables.contains(&key.child_table)
                    || touched_tables.contains(&key.parent_table)
            }) {
                for key in foreign_keys {
                    related.insert((database_name.clone(), key.child_table));
                    related.insert((database_name.clone(), key.parent_table));
                }
            }
        }
        let mut related = related.into_iter().collect::<Vec<_>>();
        related.sort();
        Ok(related)
    }

    pub fn validate_truncate_table(
        &self,
        database_name: &str,
        table_name: &str,
        foreign_key_checks: bool,
    ) -> Result<()> {
        let database = self
            .get_database(database_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database_name))?;
        if database.get_table(table_name).is_none() {
            anyhow::bail!("Table '{}.{}' doesn't exist", database_name, table_name);
        }
        if !foreign_key_checks {
            return Ok(());
        }
        let tables = database.tables.read();
        for (child_table, schema) in tables.iter() {
            if child_table == table_name {
                continue;
            }
            if let Some(foreign_key) = table_foreign_keys(child_table, schema)
                .into_iter()
                .find(|foreign_key| foreign_key.parent_table == table_name)
            {
                anyhow::bail!(
                    "Cannot truncate a table referenced in a foreign key constraint (`{}`.`{}`, CONSTRAINT `{}`)",
                    database_name,
                    child_table,
                    foreign_key.name
                );
            }
        }
        Ok(())
    }

    pub fn scan_table(&self, database: &str, table: &str) -> Result<Vec<Row>> {
        self.stats.reads.fetch_add(1, Ordering::Relaxed);
        self.get_database(database)
            .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?
            .scan_table(table)
    }

    pub fn scan_table_filtered(
        &self,
        database: &str,
        table: &str,
        filter: Option<&RowPredicate>,
    ) -> Result<Vec<Row>> {
        self.scan_table_filtered_limit(database, table, filter, None)
    }

    pub fn scan_table_filtered_limit(
        &self,
        database: &str,
        table: &str,
        filter: Option<&RowPredicate>,
        limit: Option<usize>,
    ) -> Result<Vec<Row>> {
        self.stats.reads.fetch_add(1, Ordering::Relaxed);
        self.get_database(database)
            .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?
            .scan_table_filtered_limit(table, filter, limit)
    }

    /// Sync WAL to disk
    pub fn sync_wal(&self) -> Result<()> {
        self.wal_writer.lock().sync()
    }

    /// Stop the single writer at an actor-group boundary. Holding the returned
    /// guard makes a filesystem snapshot stable while reads remain available.
    pub async fn snapshot_guard(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.actor_coordination
            .snapshot_barrier
            .clone()
            .write_owned()
            .await
    }

    pub fn current_lsn(&self) -> u64 {
        self.wal_writer.lock().next_lsn().saturating_sub(1)
    }

    pub fn flush(&self) -> Result<()> {
        self.sync_wal()?;
        for database in self.databases.read().values() {
            for table in database.list_tables() {
                database.checkpoint_table(&table)?;
            }
        }
        self.buffer_pool.flush_all()
    }

    /// Checkpoint all completed actor groups and return the still-held write
    /// barrier so a caller can copy one page/WAL-consistent filesystem image.
    pub async fn consistent_snapshot_guard(
        &self,
    ) -> Result<tokio::sync::OwnedRwLockWriteGuard<()>> {
        let guard = self.snapshot_guard().await;
        let checkpoint_started = Instant::now();
        let checkpointed = {
            let mut state = self.actor_coordination.checkpoint_state.lock();
            let checkpointed = !state.transactions.is_empty();
            checkpoint_actor_state(
                &self.databases,
                &self.wal_writer,
                &state.tables,
                &state.transactions,
            )?;
            state.clear();
            checkpointed
        };
        if checkpointed {
            self.stats.checkpoints.fetch_add(1, Ordering::Relaxed);
            self.stats.checkpoint_micros.fetch_add(
                checkpoint_started.elapsed().as_micros() as u64,
                Ordering::Relaxed,
            );
        }
        self.flush()?;
        Ok(guard)
    }

    /// Wait for the actor's current commit/checkpoint group, then flush while
    /// preventing a new group from racing the filesystem snapshot.
    pub async fn flush_consistent(&self) -> Result<()> {
        let _guard = self.consistent_snapshot_guard().await?;
        Ok(())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn stats(&self) -> StorageStatsSnapshot {
        StorageStatsSnapshot {
            uptime_seconds: self.stats.started.elapsed().as_secs(),
            reads: self.stats.reads.load(Ordering::Relaxed),
            writes: self.stats.writes.load(Ordering::Relaxed),
            errors: self.stats.errors.load(Ordering::Relaxed),
            actor_queue_depth: self.stats.queue_depth.load(Ordering::Relaxed),
            buffer_pool_pages: self.buffer_pool.page_count(),
            group_commits: self.stats.group_commits.load(Ordering::Relaxed),
            grouped_requests: self.stats.grouped_requests.load(Ordering::Relaxed),
            checkpoints: self.stats.checkpoints.load(Ordering::Relaxed),
            checkpoint_errors: self.stats.checkpoint_errors.load(Ordering::Relaxed),
            prepare_validation_micros: self.stats.prepare_validation_micros.load(Ordering::Relaxed),
            wal_sync_micros: self.stats.wal_sync_micros.load(Ordering::Relaxed),
            apply_micros: self.stats.apply_micros.load(Ordering::Relaxed),
            checkpoint_micros: self.stats.checkpoint_micros.load(Ordering::Relaxed),
        }
    }

    pub fn storage_inventory(&self) -> Result<StorageInventory> {
        let total_bytes = directory_size(&self.data_dir)?;
        let wal_bytes = directory_size(&self.data_dir.join("wal"))?;
        let databases = self.databases.read().values().cloned().collect::<Vec<_>>();
        let mut database_inventory = Vec::with_capacity(databases.len());
        let mut orphan_storage = Vec::new();
        let mut active_bytes = wal_bytes;
        for database in databases {
            let database_dir = self.data_dir.join(&database.name);
            let bytes = directory_size(&database_dir)?;
            let mut active_tables = database.list_tables();
            active_tables.sort();
            let orphaned = database.orphan_storage()?;
            let orphan_bytes = orphaned.iter().map(|item| item.bytes).sum::<u64>();
            active_bytes += bytes.saturating_sub(orphan_bytes);
            orphan_storage.extend(orphaned);
            database_inventory.push(DatabaseStorageInventory {
                database: database.name.clone(),
                bytes,
                active_tables,
            });
        }
        database_inventory.sort_by(|a, b| a.database.cmp(&b.database));
        orphan_storage.sort_by(|a, b| {
            a.database
                .cmp(&b.database)
                .then_with(|| a.table.cmp(&b.table))
        });
        let orphan_bytes = orphan_storage.iter().map(|item| item.bytes).sum();
        Ok(StorageInventory {
            data_dir: self.data_dir.display().to_string(),
            total_bytes,
            wal_bytes,
            active_bytes,
            orphan_bytes,
            databases: database_inventory,
            orphan_storage,
        })
    }

    pub async fn cleanup_orphan_storage(&self) -> Result<WriteResult> {
        self.execute_write(WriteCommand::CleanupOrphanStorage).await
    }
}

fn prepare_transaction_request(
    request: PrepareRequest,
    databases: &Arc<RwLock<HashMap<String, Arc<Database>>>>,
    stats: &StorageStats,
) {
    let result = (|| {
        let (commands, last_insert_id) = prepare_write_commands_with_prefix(
            request.commands,
            request.prepared_prefix_len,
            databases,
        )?;
        validate_write_batch(&commands, databases)?;
        Ok(PreparedTransactionBatch {
            commands,
            last_insert_id,
        })
    })();
    stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
    if result.is_err() {
        stats.errors.fetch_add(1, Ordering::Relaxed);
    }
    let _ = request.reply.send(result);
}

async fn write_actor(
    mut receiver: mpsc::Receiver<ActorRequest>,
    databases: Arc<RwLock<HashMap<String, Arc<Database>>>>,
    buffer_pool: Arc<BufferPool>,
    wal_writer: Arc<parking_lot::Mutex<WalWriter>>,
    data_dir: PathBuf,
    stats: Arc<StorageStats>,
    actor_coordination: Arc<ActorCoordination>,
) {
    const MAX_GROUP_COMMIT_REQUESTS: usize = 128;
    // WAL fsync is the durability boundary. Folding the WAL-backed memtable
    // less often keeps foreground actor writes from repeatedly paying the COW
    // checkpoint cost while bounding restart replay to a small number of groups.
    const CHECKPOINT_GROUP_INTERVAL: usize = 64;
    let mut deferred = None;
    loop {
        let request = match deferred.take() {
            Some(request) => Some(request),
            None => receiver.recv().await,
        };
        let Some(request) = request else {
            break;
        };
        let first = match request {
            ActorRequest::Write(request) => request,
            ActorRequest::Prepare(request) => {
                prepare_transaction_request(request, &databases, &stats);
                continue;
            }
        };
        // A backup takes the exclusive side only at group boundaries. Tokio's
        // fair lock prevents a continuous write stream from starving it.
        let _snapshot_read_guard = actor_coordination
            .snapshot_barrier
            .clone()
            .read_owned()
            .await;
        let mut pending = VecDeque::with_capacity(MAX_GROUP_COMMIT_REQUESTS);
        pending.push_back(PendingWrite {
            commands: first.commands,
            prepared: first.prepared,
            last_insert_id: 0,
            reply: first.reply,
        });
        while pending.len() < MAX_GROUP_COMMIT_REQUESTS {
            match receiver.try_recv() {
                Ok(ActorRequest::Write(request)) => pending.push_back(PendingWrite {
                    commands: request.commands,
                    prepared: request.prepared,
                    last_insert_id: 0,
                    reply: request.reply,
                }),
                Ok(request @ ActorRequest::Prepare(_)) => {
                    deferred = Some(request);
                    break;
                }
                Err(_) => break,
            }
        }
        // The actor runs on its own runtime, so ready network tasks can arrive
        // after the initial nonblocking drain. Wait a bounded interval to turn
        // concurrent commits into one durable WAL fsync. A zero window keeps
        // the legacy immediate-commit behavior for latency-sensitive setups.
        if !actor_coordination.group_commit_window.is_zero() {
            let group_deadline =
                tokio::time::Instant::now() + actor_coordination.group_commit_window;
            while pending.len() < MAX_GROUP_COMMIT_REQUESTS && deferred.is_none() {
                match tokio::time::timeout_at(group_deadline, receiver.recv()).await {
                    Ok(Some(ActorRequest::Write(request))) => pending.push_back(PendingWrite {
                        commands: request.commands,
                        prepared: request.prepared,
                        last_insert_id: 0,
                        reply: request.reply,
                    }),
                    Ok(Some(request @ ActorRequest::Prepare(_))) => {
                        deferred = Some(request);
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        }
        while !pending.is_empty() {
            let prepare_started = Instant::now();
            let mut staged: Vec<PreparedWrite> = Vec::new();
            let mut cumulative = Vec::new();
            let mut insert_group_keys = HashMap::new();

            while let Some(mut request) = pending.pop_front() {
                if !request.prepared {
                    let original_last_insert_id = request.last_insert_id;
                    match prepare_write_commands(std::mem::take(&mut request.commands), &databases)
                    {
                        Ok((commands, generated_id)) => {
                            request.commands = commands;
                            request.last_insert_id = if original_last_insert_id != 0 {
                                original_last_insert_id
                            } else {
                                generated_id
                            };
                        }
                        Err(error) if !staged.is_empty() => {
                            pending.push_front(request);
                            debug!("ending WAL group before dependent write: {}", error);
                            break;
                        }
                        Err(error) => {
                            stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
                            record_write_result(&stats, &Err(anyhow::anyhow!(error.to_string())));
                            let _ = request.reply.send(Err(error));
                            continue;
                        }
                    }
                }

                let request_is_memory = commands_are_memory_dml(&request.commands, &databases);
                if let Some(first_staged) = staged.first() {
                    let staged_is_memory =
                        commands_are_memory_dml(&first_staged.commands, &databases);
                    if request_is_memory != staged_is_memory {
                        pending.push_front(request);
                        break;
                    }
                }

                let request_is_insert = request
                    .commands
                    .iter()
                    .all(|command| matches!(command, WriteCommand::Insert { .. }));
                let staged_are_inserts = staged.iter().all(|request| {
                    request
                        .commands
                        .iter()
                        .all(|command| matches!(command, WriteCommand::Insert { .. }))
                });
                if !staged.is_empty() && request_is_insert != staged_are_inserts {
                    pending.push_front(request);
                    break;
                }

                let request_is_independent = request.commands.iter().all(|command| {
                    matches!(
                        command,
                        WriteCommand::Upsert { .. }
                            | WriteCommand::Update { .. }
                            | WriteCommand::ExpressionUpdate { .. }
                            | WriteCommand::ReplaceRows { .. }
                            | WriteCommand::Delete { .. }
                    )
                });
                let staged_are_independent = cumulative.is_empty()
                    && staged.iter().all(|request: &PreparedWrite| {
                        request.commands.iter().all(|command| {
                            matches!(
                                command,
                                WriteCommand::Upsert { .. }
                                    | WriteCommand::Update { .. }
                                    | WriteCommand::ExpressionUpdate { .. }
                                    | WriteCommand::ReplaceRows { .. }
                                    | WriteCommand::Delete { .. }
                            )
                        })
                    });
                if !request_is_independent && !staged.is_empty() && staged_are_independent {
                    pending.push_front(request);
                    break;
                }
                let mut candidate = Vec::new();
                let validation = if request_is_insert {
                    validate_insert_commands_incremental(
                        &request.commands,
                        &databases,
                        &mut insert_group_keys,
                    )
                } else if request_is_independent && staged_are_independent {
                    validate_write_batch(&request.commands, &databases)
                } else {
                    candidate = cumulative.clone();
                    candidate.extend(request.commands.iter().cloned());
                    validate_write_batch(&candidate, &databases)
                };
                if let Err(error) = validation {
                    if staged.is_empty() {
                        stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
                        record_write_result(&stats, &Err(anyhow::anyhow!(error.to_string())));
                        let _ = request.reply.send(Err(error));
                        continue;
                    }
                    pending.push_front(request);
                    break;
                }
                if !candidate.is_empty() {
                    cumulative = candidate;
                }
                staged.push(PreparedWrite {
                    tx_id: 0,
                    commands: request.commands,
                    last_insert_id: request.last_insert_id,
                    reply: request.reply,
                });
            }

            stats.prepare_validation_micros.fetch_add(
                prepare_started.elapsed().as_micros() as u64,
                Ordering::Relaxed,
            );

            if staged.is_empty() {
                continue;
            }

            if commands_are_memory_dml(&staged[0].commands, &databases) {
                // MEMORY is deliberately nontransactional and non-durable in
                // MySQL. Preserve actor order, but skip WAL/fsync/checkpoint.
                let apply_started = Instant::now();
                let mut earlier_apply_failed = false;
                for request in staged {
                    let mut result = if earlier_apply_failed {
                        Err(anyhow::anyhow!("earlier MEMORY statement failed to apply"))
                    } else {
                        apply_write_batch(
                            request.commands,
                            &databases,
                            &buffer_pool,
                            &wal_writer,
                            &data_dir,
                            false,
                            false,
                        )
                        .await
                    };
                    if let Ok(value) = &mut result {
                        if value.last_insert_id == 0 {
                            value.last_insert_id = request.last_insert_id;
                        }
                    }
                    if result.is_err() {
                        earlier_apply_failed = true;
                    }
                    stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
                    record_write_result(&stats, &result);
                    let _ = request.reply.send(result);
                }
                stats.apply_micros.fetch_add(
                    apply_started.elapsed().as_micros() as u64,
                    Ordering::Relaxed,
                );
                continue;
            }
            stats.group_commits.fetch_add(1, Ordering::Relaxed);
            stats
                .grouped_requests
                .fetch_add(staged.len() as u64, Ordering::Relaxed);

            // Phase 1: one checksummed WAL record and one fsync cover the whole
            // actor group. A complete record is the commit marker, eliminating
            // per-transaction Batch+Commit records without weakening durability.
            let wal_started = Instant::now();
            let wal_result = (|| -> Result<u64> {
                let mut wal = wal_writer.lock();
                let group_id = wal.next_lsn();
                for request in &mut staged {
                    request.tx_id = group_id;
                }
                let transactions = staged
                    .iter()
                    .map(|request| request.commands.as_slice())
                    .collect::<Vec<_>>();
                let payload = encode_wal_group(&transactions)?;
                let mut group =
                    WalRecord::new(0, WalRecordType::GroupCommit, group_id, "", payload);
                let lsn = wal.append(&mut group)?;
                debug_assert_eq!(lsn, group_id);
                wal.sync()?;
                Ok(group_id)
            })();
            stats
                .wal_sync_micros
                .fetch_add(wal_started.elapsed().as_micros() as u64, Ordering::Relaxed);
            let group_id = match wal_result {
                Ok(group_id) => group_id,
                Err(error) => {
                    let message = error.to_string();
                    for request in staged {
                        stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
                        let result = Err(anyhow::anyhow!("WAL group commit failed: {message}"));
                        record_write_result(&stats, &result);
                        let _ = request.reply.send(result);
                    }
                    continue;
                }
            };

            // Phase 2: apply in actor order, then flush table and catalog state.
            let apply_started = Instant::now();
            let mut completed = Vec::with_capacity(staged.len());
            let mut deferred_tables = HashSet::new();
            let pure_insert_group = staged.iter().all(|request| {
                request
                    .commands
                    .iter()
                    .all(|command| matches!(command, WriteCommand::Insert { .. }))
            });
            if pure_insert_group {
                // Every transaction already has an independent durable Batch
                // and Commit record. Apply the whole insert group together so
                // adjacent actor transactions share packed data pages.
                let mut commands = Vec::new();
                let mut requests = Vec::with_capacity(staged.len());
                for request in staged {
                    for command in &request.commands {
                        if let WriteCommand::Insert {
                            database, table, ..
                        } = command
                        {
                            deferred_tables.insert((database.clone(), table.clone()));
                        }
                    }
                    let affected_rows = request.commands.len() as u64;
                    commands.extend(request.commands);
                    requests.push((
                        request.tx_id,
                        request.reply,
                        request.last_insert_id,
                        affected_rows,
                    ));
                }
                match apply_write_batch(
                    commands,
                    &databases,
                    &buffer_pool,
                    &wal_writer,
                    &data_dir,
                    true,
                    false,
                )
                .await
                {
                    Ok(_) => {
                        for (tx_id, reply, last_insert_id, affected_rows) in requests {
                            completed.push((
                                tx_id,
                                reply,
                                Ok(WriteResult {
                                    affected_rows,
                                    last_insert_id,
                                }),
                            ));
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        for (tx_id, reply, _, _) in requests {
                            completed.push((tx_id, reply, Err(anyhow::anyhow!(message.clone()))));
                        }
                    }
                }
            } else {
                let mut earlier_apply_failed = false;
                for request in staged {
                    let deferred = request.commands.iter().all(|command| {
                        matches!(
                            command,
                            WriteCommand::Insert { .. }
                                | WriteCommand::Upsert { .. }
                                | WriteCommand::Update { .. }
                                | WriteCommand::ExpressionUpdate { .. }
                                | WriteCommand::ReplaceRows { .. }
                                | WriteCommand::Delete { .. }
                        )
                    });
                    if deferred {
                        for command in &request.commands {
                            match command {
                                WriteCommand::Insert {
                                    database, table, ..
                                }
                                | WriteCommand::Upsert {
                                    database, table, ..
                                }
                                | WriteCommand::Update {
                                    database, table, ..
                                }
                                | WriteCommand::ExpressionUpdate {
                                    database, table, ..
                                }
                                | WriteCommand::ReplaceRows {
                                    database, table, ..
                                }
                                | WriteCommand::Delete {
                                    database, table, ..
                                } => {
                                    deferred_tables.insert((database.clone(), table.clone()));
                                }
                                _ => {}
                            }
                        }
                    }
                    let mut result = if earlier_apply_failed {
                        Err(anyhow::anyhow!(
                            "earlier committed transaction failed to apply; restart recovery required"
                        ))
                    } else {
                        apply_write_batch(
                            request.commands,
                            &databases,
                            &buffer_pool,
                            &wal_writer,
                            &data_dir,
                            deferred,
                            false,
                        )
                        .await
                    };
                    if let Ok(value) = &mut result {
                        if value.last_insert_id == 0 {
                            value.last_insert_id = request.last_insert_id;
                        }
                    } else {
                        earlier_apply_failed = true;
                    }
                    completed.push((request.tx_id, request.reply, result));
                }
            }

            stats.apply_micros.fetch_add(
                apply_started.elapsed().as_micros() as u64,
                Ordering::Relaxed,
            );

            let group_succeeded = completed.iter().all(|(_, _, result)| result.is_ok());
            for (_, reply, result) in completed {
                stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
                record_write_result(&stats, &result);
                let _ = reply.send(result);
            }
            let mut checkpoint = actor_coordination.checkpoint_state.lock();
            checkpoint.tables.extend(deferred_tables);
            if group_succeeded {
                checkpoint.transactions.push(group_id);
            }
            checkpoint.groups_since_checkpoint += 1;
            if checkpoint.groups_since_checkpoint >= CHECKPOINT_GROUP_INTERVAL {
                let checkpoint_started = Instant::now();
                match checkpoint_actor_state(
                    &databases,
                    &wal_writer,
                    &checkpoint.tables,
                    &checkpoint.transactions,
                ) {
                    Ok(()) => {
                        stats.checkpoints.fetch_add(1, Ordering::Relaxed);
                        checkpoint.clear();
                    }
                    Err(error) => {
                        stats.checkpoint_errors.fetch_add(1, Ordering::Relaxed);
                        error!("actor checkpoint failed; WAL remains authoritative: {error}")
                    }
                }
                stats.checkpoint_micros.fetch_add(
                    checkpoint_started.elapsed().as_micros() as u64,
                    Ordering::Relaxed,
                );
            }
        }
    }
    let checkpoint = actor_coordination.checkpoint_state.lock();
    if let Err(error) = checkpoint_actor_state(
        &databases,
        &wal_writer,
        &checkpoint.tables,
        &checkpoint.transactions,
    ) {
        error!("final actor checkpoint failed; WAL remains authoritative: {error}");
    }
}

fn commands_are_memory_dml(
    commands: &[WriteCommand],
    databases: &Arc<RwLock<HashMap<String, Arc<Database>>>>,
) -> bool {
    !commands.is_empty()
        && commands.iter().all(|command| {
            let (database, table) = match command {
                WriteCommand::Insert {
                    database, table, ..
                }
                | WriteCommand::Upsert {
                    database, table, ..
                }
                | WriteCommand::Update {
                    database, table, ..
                }
                | WriteCommand::ExpressionUpdate {
                    database, table, ..
                }
                | WriteCommand::ReplaceRows {
                    database, table, ..
                }
                | WriteCommand::Delete {
                    database, table, ..
                } => (database, table),
                _ => return false,
            };
            databases
                .read()
                .get(database)
                .is_some_and(|db| db.is_memory_table(table))
        })
}

type GroupConstraintKeys = HashMap<(String, String, String), HashSet<Vec<u8>>>;

fn validate_insert_commands_incremental(
    commands: &[WriteCommand],
    databases: &Arc<RwLock<HashMap<String, Arc<Database>>>>,
    group_keys: &mut GroupConstraintKeys,
) -> Result<()> {
    let mut tables =
        HashMap::<(String, String), (Arc<Database>, Vec<(String, Vec<String>, bool)>)>::new();
    let mut candidate = GroupConstraintKeys::new();
    for command in commands {
        let WriteCommand::Insert {
            database,
            table,
            row,
        } = command
        else {
            anyhow::bail!("incremental INSERT validation received non-INSERT command");
        };
        let table_key = (database.clone(), table.clone());
        if !tables.contains_key(&table_key) {
            let db = databases
                .read()
                .get(database)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?;
            let schema = db
                .get_table(table)
                .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
            tables.insert(table_key.clone(), (db, key_constraints(&schema)));
        }
        let (db, constraints) = tables.get(&table_key).unwrap();
        for (key_name, columns, nullable) in constraints {
            let Some(key) = encode_key(row, columns, *nullable) else {
                continue;
            };
            let constraint = (database.clone(), table.clone(), key_name.clone());
            let already_staged = group_keys
                .get(&constraint)
                .is_some_and(|keys| keys.contains(&key));
            let duplicate_in_request = !candidate
                .entry(constraint.clone())
                .or_default()
                .insert(key.clone());
            if already_staged
                || duplicate_in_request
                || db.constraint_key_exists(table, key_name, &key)
            {
                anyhow::bail!("Duplicate entry for key '{}'", key_name);
            }
        }
    }
    for (constraint, keys) in candidate {
        group_keys.entry(constraint).or_default().extend(keys);
    }
    Ok(())
}

fn checkpoint_actor_state(
    databases: &Arc<RwLock<HashMap<String, Arc<Database>>>>,
    wal_writer: &Arc<parking_lot::Mutex<WalWriter>>,
    tables: &HashSet<(String, String)>,
    transactions: &[u64],
) -> Result<()> {
    if transactions.is_empty() {
        return Ok(());
    }
    for (database, table) in tables {
        if let Some(db) = databases.read().get(database).cloned() {
            db.checkpoint_table(table)?;
        }
    }
    let mut wal = wal_writer.lock();
    for tx_id in transactions {
        let mut applied = WalRecord::new(0, WalRecordType::Applied, *tx_id, "", Vec::new());
        wal.append(&mut applied)?;
    }
    wal.sync()
}

fn record_write_result(stats: &StorageStats, result: &Result<WriteResult>) {
    match result {
        Ok(_) => {
            stats.writes.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            error!("write actor error: {}", error);
        }
    }
}

fn prepare_write_commands(
    commands: Vec<WriteCommand>,
    databases: &Arc<RwLock<HashMap<String, Arc<Database>>>>,
) -> Result<(Vec<WriteCommand>, u64)> {
    prepare_write_commands_with_prefix(commands, 0, databases)
}

fn prepare_write_commands_with_prefix(
    commands: Vec<WriteCommand>,
    prepared_prefix_len: usize,
    databases: &Arc<RwLock<HashMap<String, Arc<Database>>>>,
) -> Result<(Vec<WriteCommand>, u64)> {
    let mut prepared = Vec::with_capacity(commands.len());
    let mut last_insert_id = 0;
    let mut foreign_key_checks = true;
    let mut created_tables = HashMap::<(String, String), TableSchema>::new();
    let mut commands = commands.into_iter().peekable();
    while let Some(command) = commands.next() {
        match command {
            WriteCommand::CreateTable { database, schema } => {
                created_tables.insert((database.clone(), schema.name.clone()), schema.clone());
                prepared.push(WriteCommand::CreateTable { database, schema });
            }
            WriteCommand::Insert {
                database,
                table,
                row,
            } => {
                let mut rows = vec![row];
                while matches!(
                    commands.peek(),
                    Some(WriteCommand::Insert {
                        database: next_database,
                        table: next_table,
                        ..
                    }) if next_database == &database && next_table == &table
                ) {
                    if let Some(WriteCommand::Insert { row, .. }) = commands.next() {
                        rows.push(row);
                    }
                }
                let (rows, generated_id) =
                    if let Some(schema) = created_tables.get(&(database.clone(), table.clone())) {
                        (
                            rows.into_iter()
                                .map(|row| materialize_row(row, schema))
                                .collect::<Result<Vec<_>>>()?,
                            0,
                        )
                    } else {
                        let db =
                            databases.read().get(&database).cloned().ok_or_else(|| {
                                anyhow::anyhow!("Unknown database '{}'", database)
                            })?;
                        db.prepare_rows_for_wal(&table, rows)?
                    };
                if last_insert_id == 0 {
                    last_insert_id = generated_id;
                }
                prepared.extend(rows.into_iter().map(|row| WriteCommand::Insert {
                    database: database.clone(),
                    table: table.clone(),
                    row,
                }));
            }
            WriteCommand::Upsert {
                database,
                table,
                row,
                update_columns,
                ignore,
            } => {
                let mut entries = vec![(row, update_columns, ignore)];
                while matches!(
                    commands.peek(),
                    Some(WriteCommand::Upsert {
                        database: next_database,
                        table: next_table,
                        ..
                    }) if next_database == &database && next_table == &table
                ) {
                    if let Some(WriteCommand::Upsert {
                        row,
                        update_columns,
                        ignore,
                        ..
                    }) = commands.next()
                    {
                        entries.push((row, update_columns, ignore));
                    }
                }
                let db = databases
                    .read()
                    .get(&database)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?;
                let rows = entries
                    .iter()
                    .map(|(row, _, _)| row.clone())
                    .collect::<Vec<_>>();
                let (rows, generated_id) = db.prepare_rows_for_wal(&table, rows)?;
                if last_insert_id == 0
                    && generated_id != 0
                    && entries.iter().any(|(_, _, ignore)| !ignore)
                {
                    last_insert_id = generated_id;
                }
                prepared.extend(rows.into_iter().zip(entries).map(
                    |(row, (_, update_columns, ignore))| WriteCommand::Upsert {
                        database: database.clone(),
                        table: table.clone(),
                        row,
                        update_columns,
                        ignore,
                    },
                ));
            }
            WriteCommand::ExpressionUpdate { .. } => {
                anyhow::bail!(
                    "Expression UPDATE must be materialized under a wire lock before WAL append"
                );
            }
            WriteCommand::ForeignKeyChecksDisabled => foreign_key_checks = false,
            command => prepared.push(command),
        }
    }
    Ok((
        if foreign_key_checks {
            prepare_referential_commands(prepared, prepared_prefix_len, databases)?
        } else {
            prepared
        },
        last_insert_id,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferentialAction {
    Restrict,
    Cascade,
    SetNull,
}

#[derive(Debug, Clone)]
struct ForeignKeySpec {
    name: String,
    child_table: String,
    child_columns: Vec<String>,
    parent_table: String,
    parent_columns: Vec<String>,
    on_delete: ReferentialAction,
    on_update: ReferentialAction,
}

fn prepare_referential_commands(
    commands: Vec<WriteCommand>,
    prepared_prefix_len: usize,
    databases: &Arc<RwLock<HashMap<String, Arc<Database>>>>,
) -> Result<Vec<WriteCommand>> {
    let mut database_names = HashSet::new();
    for command in &commands {
        if let Some((database, _)) = command_table(command) {
            database_names.insert(database.to_string());
        }
    }
    let mut output = commands;
    for database_name in database_names {
        let database = databases
            .read()
            .get(&database_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database_name))?;
        let schemas = database.tables.read().clone();
        let foreign_keys = schemas
            .iter()
            .flat_map(|(table, schema)| table_foreign_keys(table, schema))
            .collect::<Vec<_>>();
        if foreign_keys.is_empty() {
            continue;
        }
        let related = foreign_keys
            .iter()
            .flat_map(|key| [key.child_table.clone(), key.parent_table.clone()])
            .collect::<HashSet<_>>();
        if !output.iter().any(|command| {
            command_table(command).is_some_and(|(database, table)| {
                database == database_name && related.contains(table)
            })
        }) {
            continue;
        }

        let mut images = HashMap::new();
        for table in &related {
            images.insert(table.clone(), database.scan_table(table)?);
        }
        let mut initial_images = images.clone();
        let mut current_statement_started = prepared_prefix_len == 0;
        let mut changed = HashSet::new();
        let mut affected = 0_u64;
        let mut retained = Vec::new();
        for (command_index, command) in output.into_iter().enumerate() {
            let related_command = command_table(&command).is_some_and(|(database, table)| {
                database == database_name && related.contains(table)
            });
            if !related_command {
                retained.push(command);
                continue;
            }
            if !current_statement_started && command_index >= prepared_prefix_len {
                initial_images = images.clone();
                current_statement_started = true;
            }
            let (command_affected, changes) =
                apply_referential_source_command(&command, &schemas, &mut images)?;
            affected += command_affected;
            if command_affected > 0 {
                let (_, table) = command_table(&command).expect("related command has a table");
                changed.insert(table.to_string());
            }
            if command_index < prepared_prefix_len {
                continue;
            }
            for (table, _, _) in &changes {
                changed.insert(table.clone());
            }
            apply_referential_actions(changes, &foreign_keys, &schemas, &mut images, &mut changed)?;
            validate_foreign_key_images(&foreign_keys, &images, &initial_images)?;
        }
        let mut changed = changed.into_iter().collect::<Vec<_>>();
        changed.sort();
        for (index, table) in changed.into_iter().enumerate() {
            retained.push(WriteCommand::ReplaceRows {
                database: database_name.clone(),
                table: table.clone(),
                rows: images.remove(&table).unwrap_or_default(),
                affected_rows: if index == 0 { affected } else { 0 },
            });
        }
        output = retained;
    }
    Ok(output)
}

fn command_table(command: &WriteCommand) -> Option<(&str, &str)> {
    match command {
        WriteCommand::Insert {
            database, table, ..
        }
        | WriteCommand::Upsert {
            database, table, ..
        }
        | WriteCommand::Update {
            database, table, ..
        }
        | WriteCommand::ExpressionUpdate {
            database, table, ..
        }
        | WriteCommand::ReplaceRows {
            database, table, ..
        }
        | WriteCommand::Delete {
            database, table, ..
        } => Some((database, table)),
        _ => None,
    }
}

type ReferentialRowChange = (String, Row, Option<Row>);

fn apply_referential_source_command(
    command: &WriteCommand,
    schemas: &HashMap<String, TableSchema>,
    images: &mut HashMap<String, Vec<Row>>,
) -> Result<(u64, Vec<ReferentialRowChange>)> {
    let (_, table) = command_table(command)
        .ok_or_else(|| anyhow::anyhow!("referential preparation requires DML"))?;
    let schema = schemas
        .get(table)
        .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
    let rows = images
        .get_mut(table)
        .ok_or_else(|| anyhow::anyhow!("missing referential row image for '{}'", table))?;
    let mut changes = Vec::new();
    let affected = match command {
        WriteCommand::Insert { row, .. } => {
            let row = materialize_row(row.clone(), schema)?;
            rows.push(row);
            1
        }
        WriteCommand::Upsert {
            row,
            update_columns,
            ignore,
            ..
        } => {
            let incoming = materialize_row(row.clone(), schema)?;
            if let Some(index) = rows
                .iter()
                .position(|current| rows_have_constraint_conflict(current, &incoming, schema))
            {
                if *ignore {
                    0
                } else {
                    let old = rows[index].clone();
                    let mut updated = old.clone();
                    for column in update_columns {
                        if incoming.is_null(column) {
                            updated.set_null(column);
                        } else if let Some(value) = incoming.get(column) {
                            updated.set(column, value.to_vec());
                        }
                    }
                    let updated = materialize_row(updated, schema)?;
                    if updated != old {
                        rows[index] = updated.clone();
                        changes.push((table.to_string(), old, Some(updated)));
                        2
                    } else {
                        0
                    }
                }
            } else {
                rows.push(incoming);
                1
            }
        }
        WriteCommand::Update {
            filter,
            assignments,
            ..
        } => {
            let mut matched = 0;
            for row in rows
                .iter_mut()
                .filter(|row| row_matches(row, filter.as_ref()))
            {
                let old = row.clone();
                for (column, value) in assignments {
                    if let Some(value) = value {
                        row.set(column, value.clone());
                    } else {
                        row.set_null(column);
                    }
                }
                *row = materialize_row(row.clone(), schema)?;
                changes.push((table.to_string(), old, Some(row.clone())));
                matched += 1;
            }
            matched
        }
        WriteCommand::ReplaceRows {
            rows: replacement,
            affected_rows,
            ..
        } => {
            let old = std::mem::take(rows);
            *rows = replacement
                .iter()
                .cloned()
                .map(|row| materialize_row(row, schema))
                .collect::<Result<Vec<_>>>()?;
            for removed in old
                .into_iter()
                .filter(|old| !rows.iter().any(|current| current == old))
            {
                changes.push((table.to_string(), removed, None));
            }
            *affected_rows
        }
        WriteCommand::Delete { filter, .. } => {
            let old = std::mem::take(rows);
            let mut deleted = 0;
            for row in old {
                if row_matches(&row, filter.as_ref()) {
                    changes.push((table.to_string(), row, None));
                    deleted += 1;
                } else {
                    rows.push(row);
                }
            }
            deleted
        }
        WriteCommand::ExpressionUpdate { .. } => {
            anyhow::bail!("Expression UPDATE must be materialized before foreign key validation")
        }
        _ => anyhow::bail!("unsupported referential DML command"),
    };
    Ok((affected, changes))
}

fn apply_referential_actions(
    changes: Vec<ReferentialRowChange>,
    foreign_keys: &[ForeignKeySpec],
    schemas: &HashMap<String, TableSchema>,
    images: &mut HashMap<String, Vec<Row>>,
    changed: &mut HashSet<String>,
) -> Result<()> {
    let mut pending = VecDeque::from(changes);
    let mut processed = 0_usize;
    while let Some((parent_table, old_parent, new_parent)) = pending.pop_front() {
        processed += 1;
        if processed > 100_000 {
            anyhow::bail!("foreign key cascade exceeded safe row limit");
        }
        for foreign_key in foreign_keys
            .iter()
            .filter(|foreign_key| foreign_key.parent_table == parent_table)
        {
            let Some(old_key) = foreign_key_row_key(&old_parent, &foreign_key.parent_columns)
            else {
                continue;
            };
            let new_key = new_parent
                .as_ref()
                .and_then(|row| foreign_key_row_key(row, &foreign_key.parent_columns));
            if new_key.as_ref() == Some(&old_key) {
                continue;
            }
            let action = if new_parent.is_some() {
                foreign_key.on_update
            } else {
                foreign_key.on_delete
            };
            let child_schema = schemas
                .get(&foreign_key.child_table)
                .ok_or_else(|| anyhow::anyhow!("foreign key child table is missing"))?;
            let child_rows = images
                .get_mut(&foreign_key.child_table)
                .ok_or_else(|| anyhow::anyhow!("foreign key child row image is missing"))?;
            let matching = child_rows
                .iter()
                .enumerate()
                .filter_map(|(index, row)| {
                    (foreign_key_row_key(row, &foreign_key.child_columns).as_ref()
                        == Some(&old_key))
                    .then_some(index)
                })
                .collect::<Vec<_>>();
            if matching.is_empty() {
                continue;
            }
            if action == ReferentialAction::Restrict {
                anyhow::bail!(
                    "Cannot delete or update a parent row: a foreign key constraint fails ('{}')",
                    foreign_key.name
                );
            }
            changed.insert(foreign_key.child_table.clone());
            for index in matching.into_iter().rev() {
                let old_child = child_rows[index].clone();
                if action == ReferentialAction::Cascade && new_parent.is_none() {
                    child_rows.remove(index);
                    pending.push_back((foreign_key.child_table.clone(), old_child, None));
                    continue;
                }
                let mut updated = old_child.clone();
                match action {
                    ReferentialAction::Cascade => {
                        let new_key = new_key.as_ref().ok_or_else(|| {
                            anyhow::anyhow!("ON UPDATE CASCADE parent key cannot be null")
                        })?;
                        for (column, value) in foreign_key.child_columns.iter().zip(new_key.iter())
                        {
                            updated.set(column, value.clone());
                        }
                    }
                    ReferentialAction::SetNull => {
                        for column in &foreign_key.child_columns {
                            updated.set_null(column);
                        }
                    }
                    ReferentialAction::Restrict => unreachable!(),
                }
                let updated = materialize_row(updated, child_schema)?;
                child_rows[index] = updated.clone();
                pending.push_back((foreign_key.child_table.clone(), old_child, Some(updated)));
            }
        }
    }
    Ok(())
}

fn validate_foreign_key_images(
    foreign_keys: &[ForeignKeySpec],
    images: &HashMap<String, Vec<Row>>,
    initial_images: &HashMap<String, Vec<Row>>,
) -> Result<()> {
    for foreign_key in foreign_keys {
        let child_rows = images
            .get(&foreign_key.child_table)
            .ok_or_else(|| anyhow::anyhow!("foreign key child row image is missing"))?;
        let parent_rows = images
            .get(&foreign_key.parent_table)
            .ok_or_else(|| anyhow::anyhow!("foreign key parent row image is missing"))?;
        for child in child_rows {
            let Some(child_key) = foreign_key_row_key(child, &foreign_key.child_columns) else {
                continue;
            };
            if !parent_rows.iter().any(|parent| {
                foreign_key_row_key(parent, &foreign_key.parent_columns).as_ref()
                    == Some(&child_key)
            }) {
                let grandfathered = initial_images
                    .get(&foreign_key.child_table)
                    .is_some_and(|rows| rows.iter().any(|initial| initial == child))
                    && initial_images
                        .get(&foreign_key.parent_table)
                        .is_some_and(|parents| {
                            !parents.iter().any(|parent| {
                                foreign_key_row_key(parent, &foreign_key.parent_columns).as_ref()
                                    == Some(&child_key)
                            })
                        });
                if grandfathered {
                    continue;
                }
                anyhow::bail!(
                    "Cannot add or update a child row: a foreign key constraint fails ('{}')",
                    foreign_key.name
                );
            }
        }
    }
    Ok(())
}

fn foreign_key_row_key(row: &Row, columns: &[String]) -> Option<Vec<Vec<u8>>> {
    columns
        .iter()
        .map(|column| {
            if row.is_null(column) {
                None
            } else {
                row.get(column).map(|value| value.to_vec())
            }
        })
        .collect()
}

fn table_foreign_keys(table: &str, schema: &TableSchema) -> Vec<ForeignKeySpec> {
    let Some(sql) = schema.create_sql.as_deref() else {
        return Vec::new();
    };
    let Some(open) = sql.find('(') else {
        return Vec::new();
    };
    let Some(close) = sql.rfind(')') else {
        return Vec::new();
    };
    split_schema_definitions(&sql[open + 1..close])
        .into_iter()
        .filter_map(|definition| parse_foreign_key_definition(table, definition))
        .collect()
}

fn parse_foreign_key_definition(table: &str, definition: &str) -> Option<ForeignKeySpec> {
    let upper = definition.to_ascii_uppercase();
    let foreign = upper.find("FOREIGN KEY")?;
    let child_open = definition[foreign..].find('(')? + foreign;
    let child_close = definition[child_open..].find(')')? + child_open;
    let references = upper.find("REFERENCES")?;
    let parent_start = references + "REFERENCES".len();
    let parent_open = definition[parent_start..].find('(')? + parent_start;
    let parent_close = definition[parent_open..].find(')')? + parent_open;
    let parent_table = definition[parent_start..parent_open]
        .trim()
        .split('.')
        .next_back()?
        .trim_matches('`')
        .to_string();
    let columns = |value: &str| {
        split_schema_definitions(value)
            .into_iter()
            .map(|column| column.trim().trim_matches('`').to_string())
            .collect::<Vec<_>>()
    };
    let name = if upper[..foreign].trim().starts_with("CONSTRAINT") {
        definition[..foreign]
            .split_whitespace()
            .nth(1)
            .map(|value| value.trim_matches('`').to_string())
            .unwrap_or_else(|| format!("{}_ibfk_1", table))
    } else {
        format!("{}_ibfk_1", table)
    };
    Some(ForeignKeySpec {
        name,
        child_table: table.to_string(),
        child_columns: columns(&definition[child_open + 1..child_close]),
        parent_table,
        parent_columns: columns(&definition[parent_open + 1..parent_close]),
        on_delete: parse_referential_action(&upper, " ON DELETE "),
        on_update: parse_referential_action(&upper, " ON UPDATE "),
    })
}

fn parse_referential_action(definition_upper: &str, clause: &str) -> ReferentialAction {
    let Some(position) = definition_upper.find(clause) else {
        return ReferentialAction::Restrict;
    };
    let value = definition_upper[position + clause.len()..].trim_start();
    if value.starts_with("CASCADE") {
        ReferentialAction::Cascade
    } else if value.starts_with("SET NULL") {
        ReferentialAction::SetNull
    } else {
        ReferentialAction::Restrict
    }
}

fn normalize_replay_commands(
    commands: Vec<WriteCommand>,
    databases: &Arc<RwLock<HashMap<String, Arc<Database>>>>,
) -> Result<Vec<WriteCommand>> {
    let actual = databases.read().clone();
    let mut catalog = actual
        .iter()
        .map(|(name, database)| (name.clone(), database.tables.read().clone()))
        .collect::<HashMap<_, _>>();
    let mut procedures = actual
        .iter()
        .map(|(name, database)| (name.clone(), database.procedures.read().clone()))
        .collect::<HashMap<_, _>>();
    let mut normalized = Vec::with_capacity(commands.len());
    for command in commands {
        match command {
            WriteCommand::CreateDatabase(name) => {
                if !catalog.contains_key(&name) {
                    catalog.insert(name.clone(), HashMap::new());
                    procedures.insert(name.clone(), HashMap::new());
                    normalized.push(WriteCommand::CreateDatabase(name));
                }
            }
            WriteCommand::DropDatabase(name) => {
                if catalog.remove(&name).is_some() {
                    procedures.remove(&name);
                    normalized.push(WriteCommand::DropDatabase(name));
                }
            }
            WriteCommand::CreateTable { database, schema } => {
                let Some(tables) = catalog.get_mut(&database) else {
                    // A later durable DROP DATABASE supersedes this old DDL.
                    continue;
                };
                if !tables.contains_key(&schema.name) {
                    tables.insert(schema.name.clone(), schema.clone());
                    normalized.push(WriteCommand::CreateTable { database, schema });
                }
            }
            WriteCommand::DropTable { database, table } => {
                if catalog
                    .get_mut(&database)
                    .is_some_and(|tables| tables.remove(&table).is_some())
                {
                    normalized.push(WriteCommand::DropTable { database, table });
                }
            }
            WriteCommand::AlterTable {
                database,
                table,
                operation,
            } => {
                let Some(schema) = catalog
                    .get_mut(&database)
                    .and_then(|tables| tables.get_mut(&table))
                else {
                    // The table was durably removed after this old operation.
                    continue;
                };
                let already_applied = alter_operation_applied(schema, &operation);
                if !already_applied {
                    apply_alter_schema(schema, &operation)?;
                    normalized.push(WriteCommand::AlterTable {
                        database,
                        table,
                        operation,
                    });
                }
            }
            WriteCommand::Insert {
                database,
                table,
                row,
            } => {
                let Some(schema) = catalog
                    .get(&database)
                    .and_then(|tables| tables.get(&table))
                    .cloned()
                else {
                    // The row belonged to a table removed by a later DDL.
                    continue;
                };
                if is_memory_schema(&schema) {
                    continue;
                }
                let exists = actual
                    .get(&database)
                    .and_then(|db| db.scan_table(&table).ok())
                    .is_some_and(|rows| {
                        rows.iter()
                            .any(|current| rows_have_constraint_conflict(current, &row, &schema))
                    });
                if !exists {
                    normalized.push(WriteCommand::Insert {
                        database,
                        table,
                        row,
                    });
                }
            }
            WriteCommand::Upsert {
                database,
                table,
                row,
                update_columns,
                ignore,
            } => {
                if catalog.get(&database).is_some_and(|tables| {
                    tables
                        .get(&table)
                        .is_some_and(|schema| !is_memory_schema(schema))
                }) {
                    normalized.push(WriteCommand::Upsert {
                        database,
                        table,
                        row,
                        update_columns,
                        ignore,
                    });
                }
            }
            WriteCommand::Update {
                database,
                table,
                filter,
                assignments,
            } => {
                if catalog.get(&database).is_some_and(|tables| {
                    tables
                        .get(&table)
                        .is_some_and(|schema| !is_memory_schema(schema))
                }) {
                    normalized.push(WriteCommand::Update {
                        database,
                        table,
                        filter,
                        assignments,
                    });
                }
            }
            WriteCommand::ExpressionUpdate {
                database,
                table,
                filter,
                assignments,
            } => {
                if catalog.get(&database).is_some_and(|tables| {
                    tables
                        .get(&table)
                        .is_some_and(|schema| !is_memory_schema(schema))
                }) {
                    normalized.push(WriteCommand::ExpressionUpdate {
                        database,
                        table,
                        filter,
                        assignments,
                    });
                }
            }
            WriteCommand::ReplaceRows {
                database,
                table,
                rows,
                affected_rows,
            } => {
                if catalog.get(&database).is_some_and(|tables| {
                    tables
                        .get(&table)
                        .is_some_and(|schema| !is_memory_schema(schema))
                }) {
                    normalized.push(WriteCommand::ReplaceRows {
                        database,
                        table,
                        rows,
                        affected_rows,
                    });
                }
            }
            WriteCommand::Delete {
                database,
                table,
                filter,
            } => {
                if catalog.get(&database).is_some_and(|tables| {
                    tables
                        .get(&table)
                        .is_some_and(|schema| !is_memory_schema(schema))
                }) {
                    normalized.push(WriteCommand::Delete {
                        database,
                        table,
                        filter,
                    });
                }
            }
            WriteCommand::CleanupOrphanStorage => {
                normalized.push(WriteCommand::CleanupOrphanStorage)
            }
            WriteCommand::ForeignKeyChecksDisabled => {}
            WriteCommand::CreateProcedure {
                database,
                procedure,
            } => {
                let Some(items) = procedures.get_mut(&database) else {
                    continue;
                };
                if !items
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case(&procedure.name))
                {
                    items.insert(procedure.name.clone(), procedure.clone());
                    normalized.push(WriteCommand::CreateProcedure {
                        database,
                        procedure,
                    });
                }
            }
            WriteCommand::DropProcedure {
                database,
                procedure,
            } => {
                let Some(items) = procedures.get_mut(&database) else {
                    continue;
                };
                let stored = items
                    .keys()
                    .find(|name| name.eq_ignore_ascii_case(&procedure))
                    .cloned();
                if let Some(stored) = stored {
                    items.remove(&stored);
                    normalized.push(WriteCommand::DropProcedure {
                        database,
                        procedure,
                    });
                }
            }
            WriteCommand::AlterProcedure {
                database,
                procedure,
                create_sql,
            } => {
                let Some(items) = procedures.get_mut(&database) else {
                    continue;
                };
                let stored = items
                    .keys()
                    .find(|name| name.eq_ignore_ascii_case(&procedure))
                    .cloned();
                if let Some(stored) = stored {
                    items
                        .get_mut(&stored)
                        .expect("resolved procedure")
                        .create_sql = create_sql.clone();
                    normalized.push(WriteCommand::AlterProcedure {
                        database,
                        procedure,
                        create_sql,
                    });
                }
            }
            WriteCommand::CreateProcedureV2 {
                database,
                procedure,
                metadata,
            } => {
                let Some(items) = procedures.get_mut(&database) else {
                    continue;
                };
                if !items
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case(&procedure.name))
                {
                    items.insert(procedure.name.clone(), procedure.clone());
                    normalized.push(WriteCommand::CreateProcedureV2 {
                        database,
                        procedure,
                        metadata,
                    });
                }
            }
            WriteCommand::AlterProcedureV2 {
                database,
                procedure,
                create_sql,
                metadata,
            } => {
                let Some(items) = procedures.get_mut(&database) else {
                    continue;
                };
                let stored = items
                    .keys()
                    .find(|name| name.eq_ignore_ascii_case(&procedure))
                    .cloned();
                if let Some(stored) = stored {
                    items
                        .get_mut(&stored)
                        .expect("resolved procedure")
                        .create_sql = create_sql.clone();
                    normalized.push(WriteCommand::AlterProcedureV2 {
                        database,
                        procedure,
                        create_sql,
                        metadata,
                    });
                }
            }
        }
    }
    Ok(normalized)
}

fn rows_have_constraint_conflict(left: &Row, right: &Row, schema: &TableSchema) -> bool {
    let constraints = key_constraints(schema);
    if constraints.is_empty() {
        return left == right;
    }
    constraints.iter().any(|(_, columns, nullable)| {
        let left_key = encode_key(left, columns, *nullable);
        left_key.is_some() && left_key == encode_key(right, columns, *nullable)
    })
}

async fn apply_write_batch(
    commands: Vec<WriteCommand>,
    databases: &Arc<RwLock<HashMap<String, Arc<Database>>>>,
    buffer_pool: &Arc<BufferPool>,
    wal_writer: &Arc<parking_lot::Mutex<WalWriter>>,
    data_dir: &Path,
    defer_catalog_save: bool,
    validate: bool,
) -> Result<WriteResult> {
    if validate {
        validate_write_batch(&commands, databases)?;
    }
    let mut affected_rows = 0;
    let mut last_insert_id = 0;
    let mut dirty_databases = HashSet::new();
    let mut commands = commands.into_iter().peekable();
    while let Some(command) = commands.next() {
        match command {
            WriteCommand::CreateDatabase(name) => {
                let path = data_dir.join(&name);
                std::fs::create_dir_all(&path)?;
                let db = Database::new(
                    &name,
                    data_dir.to_path_buf(),
                    buffer_pool.clone(),
                    wal_writer.clone(),
                );
                db.save().await?;
                databases.write().insert(name.clone(), Arc::new(db));
                info!("Created database: {}", name);
            }
            WriteCommand::DropDatabase(name) => {
                let path = data_dir.join(&name);
                if path.exists() {
                    std::fs::remove_dir_all(path)?;
                }
                databases.write().remove(&name);
                info!("Dropped database: {}", name);
            }
            WriteCommand::CreateTable { database, schema } => {
                let db = databases.read().get(&database).cloned().unwrap();
                db.create_table(schema)?;
                db.save().await?;
            }
            WriteCommand::DropTable { database, table } => {
                let db = databases.read().get(&database).cloned().unwrap();
                db.drop_table(&table)?;
                db.save().await?;
            }
            WriteCommand::AlterTable {
                database,
                table,
                operation,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                if let Some((key_name, columns, nullable)) = new_key_constraint(&operation) {
                    let mut next_schema = db
                        .get_table(&table)
                        .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
                    apply_alter_schema(&mut next_schema, &operation)?;
                    let rows = db
                        .scan_table(&table)?
                        .into_iter()
                        .map(|row| materialize_row(row, &next_schema))
                        .collect::<Result<Vec<_>>>()?;
                    validate_rows_for_new_key(&rows, columns, key_name, nullable)?;
                }
                let row_mutation = alter_row_mutation(&operation);
                let mut rewritten_rows = row_mutation
                    .as_ref()
                    .map(|_| db.scan_table(&table))
                    .transpose()?;
                {
                    let mut tables = db.tables.write();
                    let schema = tables
                        .get_mut(&table)
                        .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
                    apply_alter_schema(schema, &operation)?;
                }
                if let (Some(mutation), Some(rows)) =
                    (row_mutation.as_ref(), rewritten_rows.as_mut())
                {
                    apply_alter_row_mutation(rows, mutation);
                    db.replace_rows(&table, std::mem::take(rows))?;
                }
                db.rebuild_all_indexes()?;
                db.save().await?;
            }
            WriteCommand::Insert {
                database,
                table,
                row,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                let mut rows = vec![row];
                while matches!(
                    commands.peek(),
                    Some(WriteCommand::Insert {
                        database: next_database,
                        table: next_table,
                        ..
                    }) if next_database == &database && next_table == &table
                ) {
                    if let Some(WriteCommand::Insert { row, .. }) = commands.next() {
                        rows.push(row);
                    }
                }
                affected_rows += rows.len() as u64;
                // The transaction Batch+Commit record is already durable before
                // apply. Do not append one redundant legacy Insert record per row.
                let insert_id = db.insert_rows_validated(&table, rows, !defer_catalog_save)?;
                if insert_id != 0 && last_insert_id == 0 {
                    last_insert_id = insert_id;
                }
                if !db.is_memory_table(&table) {
                    dirty_databases.insert(database);
                }
            }
            WriteCommand::Upsert {
                database,
                table,
                row,
                update_columns,
                ignore,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                let mut rows = vec![row];
                while matches!(
                    commands.peek(),
                    Some(WriteCommand::Upsert {
                        database: next_database,
                        table: next_table,
                        update_columns: next_columns,
                        ignore: next_ignore,
                        ..
                    }) if next_database == &database
                        && next_table == &table
                        && next_columns == &update_columns
                        && next_ignore == &ignore
                ) {
                    if let Some(WriteCommand::Upsert { row, .. }) = commands.next() {
                        rows.push(row);
                    }
                }
                affected_rows +=
                    db.upsert_rows_mode(&table, rows, &update_columns, ignore, defer_catalog_save)?;
                if !db.is_memory_table(&table) {
                    dirty_databases.insert(database);
                }
            }
            WriteCommand::Update {
                database,
                table,
                filter,
                assignments,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                affected_rows +=
                    db.update_rows_mode(&table, filter.as_ref(), &assignments, defer_catalog_save)?;
                if !db.is_memory_table(&table) {
                    dirty_databases.insert(database);
                }
            }
            WriteCommand::ExpressionUpdate {
                database,
                table,
                filter,
                assignments,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                affected_rows += db.update_rows_expression_mode(
                    &table,
                    filter.as_ref(),
                    &assignments,
                    defer_catalog_save,
                )?;
                if !db.is_memory_table(&table) {
                    dirty_databases.insert(database);
                }
            }
            WriteCommand::ReplaceRows {
                database,
                table,
                rows,
                affected_rows: command_affected_rows,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                let resets_auto_increment = rows.is_empty() && command_affected_rows == 0;
                if defer_catalog_save {
                    db.stage_rows(&table, rows)?;
                } else {
                    db.replace_rows(&table, rows)?;
                }
                if resets_auto_increment {
                    db.reset_auto_increment(&table);
                }
                affected_rows += command_affected_rows;
                if !db.is_memory_table(&table) {
                    dirty_databases.insert(database);
                }
            }
            WriteCommand::Delete {
                database,
                table,
                filter,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                affected_rows +=
                    db.delete_rows_mode(&table, filter.as_ref(), defer_catalog_save)?;
                if !db.is_memory_table(&table) {
                    dirty_databases.insert(database);
                }
            }
            WriteCommand::CleanupOrphanStorage => {
                let items = databases.read().values().cloned().collect::<Vec<_>>();
                for database in items {
                    affected_rows += database.cleanup_orphan_storage()?;
                }
            }
            WriteCommand::ForeignKeyChecksDisabled => {
                anyhow::bail!("foreign key session marker reached storage apply")
            }
            WriteCommand::CreateProcedure {
                database,
                procedure,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                db.create_procedure(procedure)?;
                db.save().await?;
            }
            WriteCommand::DropProcedure {
                database,
                procedure,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                db.drop_procedure(&procedure)?;
                db.save().await?;
            }
            WriteCommand::AlterProcedure {
                database,
                procedure,
                create_sql,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                db.alter_procedure(&procedure, create_sql)?;
                db.save().await?;
            }
            WriteCommand::CreateProcedureV2 {
                database,
                procedure,
                metadata,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                db.create_procedure_with_metadata(procedure, metadata)?;
                db.save().await?;
            }
            WriteCommand::AlterProcedureV2 {
                database,
                procedure,
                create_sql,
                metadata,
            } => {
                let db = databases.read().get(&database).cloned().unwrap();
                db.alter_procedure_with_metadata(&procedure, create_sql, metadata)?;
                db.save().await?;
            }
        }
    }
    if !defer_catalog_save {
        for database in dirty_databases {
            let db = { databases.read().get(&database).cloned() };
            if let Some(db) = db {
                db.save().await?;
            }
        }
    }
    Ok(WriteResult {
        affected_rows,
        last_insert_id,
    })
}

fn alter_target_column(operation: &AlterTableOperation) -> Option<&str> {
    match operation {
        AlterTableOperation::IfExists(operation) | AlterTableOperation::IfNotExists(operation) => {
            alter_target_column(operation)
        }
        AlterTableOperation::DropColumn(column) => Some(column),
        AlterTableOperation::ChangeColumn {
            old_name, column, ..
        } if old_name != &column.name => Some(old_name),
        AlterTableOperation::RenameColumn { old_name, new_name } if old_name != new_name => {
            Some(old_name)
        }
        _ => None,
    }
}

fn expression_mentions_column(expression: &str, column: &str) -> bool {
    expression
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|token| token.eq_ignore_ascii_case(column))
}

fn validate_alter_column_dependencies(
    catalog: &HashMap<String, HashMap<String, TableSchema>>,
    database: &str,
    table: &str,
    operation: &AlterTableOperation,
) -> Result<()> {
    let Some(column) = alter_target_column(operation) else {
        return Ok(());
    };
    let tables = catalog
        .get(database)
        .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?;
    for (child_table, schema) in tables {
        for foreign_key in table_foreign_keys(child_table, schema) {
            let child_dependency =
                child_table == table && foreign_key.child_columns.iter().any(|item| item == column);
            let parent_dependency = foreign_key.parent_table == table
                && foreign_key.parent_columns.iter().any(|item| item == column);
            if child_dependency || parent_dependency {
                anyhow::bail!(
                    "Column '{}' is used by foreign key constraint '{}'",
                    column,
                    foreign_key.name
                );
            }
        }
    }
    if let Some(schema) = tables.get(table) {
        for (name, expression) in table_check_constraints(schema) {
            if expression_mentions_column(&expression, column) {
                anyhow::bail!("Column '{}' is used by check constraint '{}'", column, name);
            }
        }
    }
    Ok(())
}

struct AlterValidationContext<'a> {
    alter_rows: &'a mut HashMap<(String, String), Vec<Row>>,
    existing: &'a HashMap<String, Arc<Database>>,
    reset_databases: &'a HashSet<String>,
    reset_tables: &'a HashSet<(String, String)>,
    database: &'a str,
}

fn validate_added_check(
    context: &mut AlterValidationContext<'_>,
    table: &str,
    schema: &TableSchema,
) -> Result<()> {
    let mut null_row = Row::new();
    for column in &schema.columns {
        null_row.push_null(&column.name);
    }
    for (_, expression) in table_check_constraints(schema) {
        evaluate_check_expression(&expression, &null_row, schema)?;
    }
    let rows = validation_alter_rows_mut(
        context.alter_rows,
        context.existing,
        context.reset_databases,
        context.reset_tables,
        context.database,
        table,
    )?
    .iter()
    .cloned()
    .map(|row| materialize_row(row, schema))
    .collect::<Result<Vec<_>>>()?;
    for row in rows {
        validate_check_constraints(&row, schema)?;
    }
    Ok(())
}

fn validate_added_foreign_key(
    catalog: &HashMap<String, HashMap<String, TableSchema>>,
    context: &mut AlterValidationContext<'_>,
    table: &str,
    name: &str,
) -> Result<()> {
    let tables = catalog
        .get(context.database)
        .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", context.database))?;
    let duplicate_names = tables
        .iter()
        .flat_map(|(table_name, schema)| table_foreign_keys(table_name, schema))
        .filter(|foreign_key| foreign_key.name == name)
        .count();
    if duplicate_names > 1 {
        anyhow::bail!("Duplicate foreign key constraint name '{}'", name);
    }
    let child_schema = tables
        .get(table)
        .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
    let foreign_key = table_foreign_keys(table, child_schema)
        .into_iter()
        .find(|foreign_key| foreign_key.name == name)
        .ok_or_else(|| anyhow::anyhow!("Foreign key '{}' is not defined", name))?;
    let parent_schema = tables
        .get(&foreign_key.parent_table)
        .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", foreign_key.parent_table))?;
    for column in &foreign_key.child_columns {
        validate_column_exists(child_schema, column)?;
    }
    for column in &foreign_key.parent_columns {
        validate_column_exists(parent_schema, column)?;
    }
    let parent_is_indexed = primary_key_columns(parent_schema) == foreign_key.parent_columns
        || parent_schema
            .indexes
            .iter()
            .any(|index| index.columns == foreign_key.parent_columns);
    if !parent_is_indexed {
        anyhow::bail!(
            "Missing index for constraint '{}' in referenced table '{}'",
            name,
            foreign_key.parent_table
        );
    }
    let child_rows = validation_alter_rows_mut(
        context.alter_rows,
        context.existing,
        context.reset_databases,
        context.reset_tables,
        context.database,
        table,
    )?
    .iter()
    .cloned()
    .map(|row| materialize_row(row, child_schema))
    .collect::<Result<Vec<_>>>()?;
    let parent_rows = if foreign_key.parent_table == table {
        child_rows.clone()
    } else {
        validation_alter_rows_mut(
            context.alter_rows,
            context.existing,
            context.reset_databases,
            context.reset_tables,
            context.database,
            &foreign_key.parent_table,
        )?
        .iter()
        .cloned()
        .map(|row| materialize_row(row, parent_schema))
        .collect::<Result<Vec<_>>>()?
    };
    for child in &child_rows {
        let Some(child_key) = foreign_key_row_key(child, &foreign_key.child_columns) else {
            continue;
        };
        let found = parent_rows.iter().any(|parent| {
            foreign_key_row_key(parent, &foreign_key.parent_columns).as_ref() == Some(&child_key)
        });
        if !found {
            anyhow::bail!(
                "Cannot add or update a child row: a foreign key constraint fails (`{}`)",
                name
            );
        }
    }
    Ok(())
}

fn validate_write_batch(
    commands: &[WriteCommand],
    databases: &Arc<RwLock<HashMap<String, Arc<Database>>>>,
) -> Result<()> {
    let guard = databases.read();
    let existing: HashMap<_, _> = guard
        .iter()
        .map(|(name, db)| (name.clone(), db.clone()))
        .collect();
    drop(guard);

    let mut catalog: HashMap<String, HashMap<String, TableSchema>> = HashMap::new();
    let mut procedures: HashMap<String, HashMap<String, ProcedureDefinition>> = HashMap::new();
    // Keep only keys introduced by this candidate batch. Existing keys stay in
    // the database-owned sets and are checked in place, avoiding an O(database)
    // clone for every actor request as tables grow.
    let mut constraint_keys: HashMap<(String, String, String), HashSet<Vec<u8>>> = HashMap::new();
    let mut check_rows: HashMap<(String, String), Vec<Row>> = HashMap::new();
    let mut alter_rows: HashMap<(String, String), Vec<Row>> = HashMap::new();
    let mut reset_databases = HashSet::new();
    let mut reset_tables = HashSet::new();
    for (database_name, database) in &existing {
        let schemas = database.tables.read().clone();
        catalog.insert(database_name.clone(), schemas);
        procedures.insert(database_name.clone(), database.procedures.read().clone());
    }

    for command in commands {
        match command {
            WriteCommand::CreateDatabase(name) => {
                if catalog.contains_key(name) {
                    anyhow::bail!("Database '{}' already exists", name);
                }
                catalog.insert(name.clone(), Default::default());
                procedures.insert(name.clone(), Default::default());
                reset_databases.insert(name.clone());
            }
            WriteCommand::DropDatabase(name) => {
                if catalog.remove(name).is_none() {
                    anyhow::bail!("Unknown database '{}'", name);
                }
                procedures.remove(name);
                constraint_keys.retain(|(database, _, _), _| database != name);
                reset_databases.insert(name.clone());
            }
            WriteCommand::CreateTable { database, schema } => {
                let tables = catalog
                    .get_mut(database)
                    .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?;
                if tables.contains_key(&schema.name) {
                    anyhow::bail!("Table '{}' already exists", schema.name);
                }
                for (key_name, _, _) in key_constraints(schema) {
                    constraint_keys.insert(
                        (database.clone(), schema.name.clone(), key_name),
                        Default::default(),
                    );
                }
                reset_tables.insert((database.clone(), schema.name.clone()));
                if !table_check_constraints(schema).is_empty() {
                    check_rows.insert((database.clone(), schema.name.clone()), Vec::new());
                }
                tables.insert(schema.name.clone(), schema.clone());
            }
            WriteCommand::DropTable { database, table } => {
                let tables = catalog
                    .get_mut(database)
                    .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?;
                if tables.remove(table).is_none() {
                    anyhow::bail!("Table '{}' does not exist", table);
                }
                constraint_keys.retain(|(db, item, _), _| db != database || item != table);
                check_rows.remove(&(database.clone(), table.clone()));
                reset_tables.insert((database.clone(), table.clone()));
            }
            WriteCommand::AlterTable {
                database,
                table,
                operation,
            } => {
                validate_alter_column_dependencies(&catalog, database, table, operation)?;
                let row_mutation = alter_row_mutation(operation);
                if let Some(mutation) = &row_mutation {
                    let rows = validation_alter_rows_mut(
                        &mut alter_rows,
                        &existing,
                        &reset_databases,
                        &reset_tables,
                        database,
                        table,
                    )?;
                    apply_alter_row_mutation(rows, mutation);
                }
                {
                    let schema = catalog
                        .get_mut(database)
                        .and_then(|tables| tables.get_mut(table))
                        .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
                    apply_alter_schema(schema, operation)?;
                }
                let schema = catalog
                    .get(database)
                    .and_then(|tables| tables.get(table))
                    .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
                if let Some((key_name, columns, nullable)) = new_key_constraint(operation) {
                    let rows = validation_alter_rows_mut(
                        &mut alter_rows,
                        &existing,
                        &reset_databases,
                        &reset_tables,
                        database,
                        table,
                    )?
                    .iter()
                    .cloned()
                    .map(|row| materialize_row(row, schema))
                    .collect::<Result<Vec<_>>>()?;
                    validate_rows_for_new_key(&rows, columns, key_name, nullable)?;
                }
                let mut validation_context = AlterValidationContext {
                    alter_rows: &mut alter_rows,
                    existing: &existing,
                    reset_databases: &reset_databases,
                    reset_tables: &reset_tables,
                    database,
                };
                if matches!(operation, AlterTableOperation::AddCheck { .. }) {
                    validate_added_check(&mut validation_context, table, schema)?;
                }
                if let AlterTableOperation::AddForeignKey { name, .. } = operation {
                    validate_added_foreign_key(&catalog, &mut validation_context, table, name)?;
                }
            }
            WriteCommand::Insert {
                database,
                table,
                row,
            } => {
                let schema = catalog
                    .get(database)
                    .and_then(|tables| tables.get(table))
                    .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
                let materialized = materialize_row(row.clone(), schema)?;
                if !table_check_constraints(schema).is_empty() {
                    validation_check_rows_mut(
                        &mut check_rows,
                        &existing,
                        &reset_databases,
                        &reset_tables,
                        database,
                        table,
                    )?
                    .push(materialized.clone());
                }
                for (key_name, columns, nullable) in key_constraints(schema) {
                    let Some(key) = encode_key(&materialized, &columns, nullable) else {
                        continue;
                    };
                    let exists_in_storage = !reset_databases.contains(database)
                        && !reset_tables.contains(&(database.clone(), table.clone()))
                        && existing
                            .get(database)
                            .is_some_and(|db| db.constraint_key_exists(table, &key_name, &key));
                    let keys = constraint_keys
                        .entry((database.clone(), table.clone(), key_name.clone()))
                        .or_default();
                    if exists_in_storage || !keys.insert(key) {
                        anyhow::bail!("Duplicate entry for key '{}'", key_name);
                    }
                }
            }
            WriteCommand::Upsert {
                database,
                table,
                row,
                update_columns,
                ..
            } => {
                let schema = catalog
                    .get(database)
                    .and_then(|tables| tables.get(table))
                    .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
                let materialized = materialize_row(row.clone(), schema)?;
                for column in update_columns {
                    validate_column_exists(schema, column)?;
                }
                if !table_check_constraints(schema).is_empty() {
                    let rows = validation_check_rows_mut(
                        &mut check_rows,
                        &existing,
                        &reset_databases,
                        &reset_tables,
                        database,
                        table,
                    )?;
                    if let Some(index) = rows.iter().position(|current| {
                        rows_have_constraint_conflict(current, &materialized, schema)
                    }) {
                        let mut updated = rows[index].clone();
                        for column in update_columns {
                            if materialized.is_null(column) {
                                updated.set_null(column);
                            } else if let Some(value) = materialized.get(column) {
                                updated.set(column, value.to_vec());
                            }
                        }
                        rows[index] = materialize_row(updated, schema)?;
                    } else {
                        rows.push(materialized);
                    }
                }
            }
            WriteCommand::Update {
                database,
                table,
                filter,
                assignments,
            } => {
                let schema = catalog
                    .get(database)
                    .and_then(|tables| tables.get(table))
                    .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
                for (column, value) in assignments {
                    validate_column_exists(schema, column)?;
                    if value.is_none()
                        && schema
                            .columns
                            .iter()
                            .any(|item| item.name == *column && !item.nullable)
                    {
                        anyhow::bail!("Column '{}' cannot be null", column);
                    }
                }
                if let Some(filter) = filter {
                    for column in filter.columns() {
                        validate_column_exists(schema, column)?;
                    }
                }
                if !table_check_constraints(schema).is_empty() {
                    let rows = validation_check_rows_mut(
                        &mut check_rows,
                        &existing,
                        &reset_databases,
                        &reset_tables,
                        database,
                        table,
                    )?;
                    for row in rows
                        .iter_mut()
                        .filter(|row| row_matches(row, filter.as_ref()))
                    {
                        for (column, value) in assignments {
                            if let Some(value) = value {
                                row.set(column, value.clone());
                            } else {
                                row.set_null(column);
                            }
                        }
                        *row = materialize_row(row.clone(), schema)?;
                    }
                }
            }
            WriteCommand::ExpressionUpdate {
                database,
                table,
                filter,
                assignments,
            } => {
                let schema = catalog
                    .get(database)
                    .and_then(|tables| tables.get(table))
                    .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
                for assignment in assignments {
                    let target = schema
                        .columns
                        .iter()
                        .find(|column| column.name == assignment.target_column)
                        .ok_or_else(|| {
                            anyhow::anyhow!("Unknown column '{}'", assignment.target_column)
                        })?;
                    match &assignment.value {
                        UpdateValueExpression::Literal(value) => {
                            if value.is_none() && !target.nullable {
                                anyhow::bail!("Column '{}' cannot be null", target.name);
                            }
                        }
                        UpdateValueExpression::Column(source_column) => {
                            validate_column_exists(schema, source_column)?;
                        }
                        UpdateValueExpression::Default(source_column) => {
                            column_default_value(schema, source_column)?;
                        }
                        UpdateValueExpression::Numeric {
                            source_column,
                            operand,
                            ..
                        } => {
                            let source = schema
                                .columns
                                .iter()
                                .find(|column| column.name == *source_column)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("Unknown column '{}'", source_column)
                                })?;
                            if !is_numeric_data_type(&source.data_type)
                                || !is_numeric_data_type(&target.data_type)
                            {
                                anyhow::bail!("Numeric UPDATE requires numeric columns");
                            }
                            std::str::from_utf8(operand)?.parse::<Decimal>()?;
                        }
                    }
                }
                if let Some(filter) = filter {
                    for column in filter.columns() {
                        validate_column_exists(schema, column)?;
                    }
                }
                if !table_check_constraints(schema).is_empty() {
                    let rows = validation_check_rows_mut(
                        &mut check_rows,
                        &existing,
                        &reset_databases,
                        &reset_tables,
                        database,
                        table,
                    )?;
                    for row in rows
                        .iter_mut()
                        .filter(|row| row_matches(row, filter.as_ref()))
                    {
                        apply_expression_assignments(row, schema, assignments)?;
                        *row = materialize_row(row.clone(), schema)?;
                    }
                }
            }
            WriteCommand::ReplaceRows {
                database,
                table,
                rows,
                ..
            } => {
                let schema = catalog
                    .get(database)
                    .and_then(|tables| tables.get(table))
                    .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
                constraint_keys.retain(|(db, item, _), _| db != database || item != table);
                reset_tables.insert((database.clone(), table.clone()));
                if !table_check_constraints(schema).is_empty() {
                    check_rows.insert((database.clone(), table.clone()), Vec::new());
                }
                for row in rows {
                    let materialized = materialize_row(row.clone(), schema)?;
                    if !table_check_constraints(schema).is_empty() {
                        check_rows
                            .get_mut(&(database.clone(), table.clone()))
                            .expect("check row image was initialized")
                            .push(materialized.clone());
                    }
                    for (key_name, columns, nullable) in key_constraints(schema) {
                        let Some(key) = encode_key(&materialized, &columns, nullable) else {
                            continue;
                        };
                        let keys = constraint_keys
                            .entry((database.clone(), table.clone(), key_name.clone()))
                            .or_default();
                        if !keys.insert(key) {
                            anyhow::bail!("Duplicate entry for key '{}'", key_name);
                        }
                    }
                }
            }
            WriteCommand::Delete {
                database,
                table,
                filter,
            } => {
                let schema = catalog
                    .get(database)
                    .and_then(|tables| tables.get(table))
                    .ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", table))?;
                if let Some(filter) = filter {
                    for column in filter.columns() {
                        validate_column_exists(schema, column)?;
                    }
                }
                if !table_check_constraints(schema).is_empty() {
                    validation_check_rows_mut(
                        &mut check_rows,
                        &existing,
                        &reset_databases,
                        &reset_tables,
                        database,
                        table,
                    )?
                    .retain(|row| !row_matches(row, filter.as_ref()));
                }
            }
            WriteCommand::CleanupOrphanStorage => {}
            WriteCommand::ForeignKeyChecksDisabled => {
                anyhow::bail!("foreign key session marker reached storage validation")
            }
            WriteCommand::CreateProcedure {
                database,
                procedure,
            } => {
                let items = procedures
                    .get_mut(database)
                    .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?;
                if items
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case(&procedure.name))
                {
                    anyhow::bail!("PROCEDURE {} already exists", procedure.name);
                }
                items.insert(procedure.name.clone(), procedure.clone());
            }
            WriteCommand::DropProcedure {
                database,
                procedure,
            } => {
                let items = procedures
                    .get_mut(database)
                    .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?;
                let stored = items
                    .keys()
                    .find(|name| name.eq_ignore_ascii_case(procedure))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("PROCEDURE {} does not exist", procedure))?;
                items.remove(&stored);
            }
            WriteCommand::AlterProcedure {
                database,
                procedure,
                create_sql,
            } => {
                let items = procedures
                    .get_mut(database)
                    .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?;
                let stored = items
                    .keys()
                    .find(|name| name.eq_ignore_ascii_case(procedure))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("PROCEDURE {} does not exist", procedure))?;
                items
                    .get_mut(&stored)
                    .expect("resolved procedure")
                    .create_sql = create_sql.clone();
            }
            WriteCommand::CreateProcedureV2 {
                database,
                procedure,
                ..
            } => {
                let items = procedures
                    .get_mut(database)
                    .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?;
                if items
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case(&procedure.name))
                {
                    anyhow::bail!("PROCEDURE {} already exists", procedure.name);
                }
                items.insert(procedure.name.clone(), procedure.clone());
            }
            WriteCommand::AlterProcedureV2 {
                database,
                procedure,
                create_sql,
                ..
            } => {
                let items = procedures
                    .get_mut(database)
                    .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?;
                let stored = items
                    .keys()
                    .find(|name| name.eq_ignore_ascii_case(procedure))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("PROCEDURE {} does not exist", procedure))?;
                items
                    .get_mut(&stored)
                    .expect("resolved procedure")
                    .create_sql = create_sql.clone();
            }
        }
    }
    Ok(())
}

fn validation_check_rows_mut<'a>(
    check_rows: &'a mut HashMap<(String, String), Vec<Row>>,
    existing: &HashMap<String, Arc<Database>>,
    reset_databases: &HashSet<String>,
    reset_tables: &HashSet<(String, String)>,
    database: &str,
    table: &str,
) -> Result<&'a mut Vec<Row>> {
    let key = (database.to_string(), table.to_string());
    if !check_rows.contains_key(&key) {
        let rows = if reset_databases.contains(database) || reset_tables.contains(&key) {
            Vec::new()
        } else {
            existing
                .get(database)
                .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?
                .scan_table(table)?
        };
        check_rows.insert(key.clone(), rows);
    }
    Ok(check_rows
        .get_mut(&key)
        .expect("check row image was initialized"))
}

fn validation_alter_rows_mut<'a>(
    alter_rows: &'a mut HashMap<(String, String), Vec<Row>>,
    existing: &HashMap<String, Arc<Database>>,
    reset_databases: &HashSet<String>,
    reset_tables: &HashSet<(String, String)>,
    database: &str,
    table: &str,
) -> Result<&'a mut Vec<Row>> {
    let key = (database.to_string(), table.to_string());
    if !alter_rows.contains_key(&key) {
        let rows = if reset_databases.contains(database) || reset_tables.contains(&key) {
            Vec::new()
        } else {
            existing
                .get(database)
                .ok_or_else(|| anyhow::anyhow!("Unknown database '{}'", database))?
                .scan_table(table)?
        };
        alter_rows.insert(key.clone(), rows);
    }
    Ok(alter_rows
        .get_mut(&key)
        .expect("alter row image was initialized"))
}

fn materialize_row(mut row: Row, schema: &TableSchema) -> Result<Row> {
    let auto_increment = auto_increment_column(schema);
    for (name, _) in &row.values {
        validate_column_exists(schema, name)?;
    }
    for column in &schema.columns {
        if !row.contains(&column.name) {
            if auto_increment.as_deref() == Some(column.name.as_str()) {
                continue;
            } else if let Some(default) = &column.default {
                if default.eq_ignore_ascii_case("NULL") {
                    row.push_null(&column.name);
                } else if let Some(default) = current_timestamp_default(default) {
                    row.push(&column.name, default);
                } else {
                    row.push(&column.name, default.as_bytes().to_vec());
                }
            } else if column.nullable {
                row.push_null(&column.name);
            } else {
                anyhow::bail!("Field '{}' doesn't have a default value", column.name);
            }
        } else if row.is_null(&column.name) && !column.nullable {
            anyhow::bail!("Column '{}' cannot be null", column.name);
        }
    }
    validate_check_constraints(&row, schema)?;
    Ok(row)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckTruth {
    True,
    False,
    Unknown,
}

fn validate_check_constraints(row: &Row, schema: &TableSchema) -> Result<()> {
    for (name, expression) in table_check_constraints(schema) {
        if evaluate_check_expression(&expression, row, schema)? == CheckTruth::False {
            anyhow::bail!("Check constraint '{}' is violated.", name);
        }
    }
    Ok(())
}

fn check_constraint_name(table: &str, definition: &str, index: usize) -> Option<String> {
    let upper = definition.to_ascii_uppercase();
    let check = upper.find("CHECK")?;
    let prefix = definition[..check].trim();
    if prefix.to_ascii_uppercase().starts_with("CONSTRAINT ") {
        prefix
            .split_whitespace()
            .nth(1)
            .map(|value| value.trim_matches('`').to_string())
    } else {
        Some(format!("{}_chk_{}", table, index + 1))
    }
}

fn table_check_constraints(schema: &TableSchema) -> Vec<(String, String)> {
    let Some(sql) = schema.create_sql.as_deref() else {
        return Vec::new();
    };
    let Some(open) = sql.find('(') else {
        return Vec::new();
    };
    let Some(close) = sql.rfind(')') else {
        return Vec::new();
    };
    split_schema_definitions(&sql[open + 1..close])
        .into_iter()
        .enumerate()
        .filter_map(|(index, definition)| {
            let upper = definition.to_ascii_uppercase();
            let check = upper.find("CHECK")?;
            let expression_start = definition[check + 5..].find('(')? + check + 6;
            let expression_end = definition.rfind(')')?;
            if expression_end < expression_start {
                return None;
            }
            let name = check_constraint_name(&schema.name, definition, index)?;
            Some((
                name,
                definition[expression_start..expression_end]
                    .trim()
                    .to_string(),
            ))
        })
        .collect()
}

fn evaluate_check_expression(
    expression: &str,
    row: &Row,
    schema: &TableSchema,
) -> Result<CheckTruth> {
    let expression = trim_check_parentheses(expression.trim());
    if let Some(position) = find_check_keyword(expression, " OR ") {
        let left = evaluate_check_expression(&expression[..position], row, schema)?;
        let right = evaluate_check_expression(&expression[position + 4..], row, schema)?;
        return Ok(match (left, right) {
            (CheckTruth::True, _) | (_, CheckTruth::True) => CheckTruth::True,
            (CheckTruth::Unknown, _) | (_, CheckTruth::Unknown) => CheckTruth::Unknown,
            _ => CheckTruth::False,
        });
    }
    if let Some(position) = find_check_keyword(expression, " AND ") {
        let left = evaluate_check_expression(&expression[..position], row, schema)?;
        let right = evaluate_check_expression(&expression[position + 5..], row, schema)?;
        return Ok(match (left, right) {
            (CheckTruth::False, _) | (_, CheckTruth::False) => CheckTruth::False,
            (CheckTruth::Unknown, _) | (_, CheckTruth::Unknown) => CheckTruth::Unknown,
            _ => CheckTruth::True,
        });
    }

    let upper = expression.to_ascii_uppercase();
    if let Some(column) = upper.strip_suffix(" IS NOT NULL") {
        let name = expression[..column.len()].trim().trim_matches('`');
        return Ok(if row.is_null(name) {
            CheckTruth::False
        } else {
            CheckTruth::True
        });
    }
    if let Some(column) = upper.strip_suffix(" IS NULL") {
        let name = expression[..column.len()].trim().trim_matches('`');
        return Ok(if row.is_null(name) {
            CheckTruth::True
        } else {
            CheckTruth::False
        });
    }

    for operator in [">=", "<=", "<>", "!=", ">", "<", "="] {
        let Some(position) = find_check_operator(expression, operator) else {
            continue;
        };
        let column = expression[..position].trim().trim_matches('`');
        validate_column_exists(schema, column)?;
        if row.is_null(column) {
            return Ok(CheckTruth::Unknown);
        }
        let actual = row
            .get(column)
            .ok_or_else(|| anyhow::anyhow!("Unknown column '{}' in check constraint", column))?;
        let expected = parse_check_literal(&expression[position + operator.len()..]);
        let ordering = compare_check_values(actual, &expected);
        let matches = match operator {
            ">=" => ordering.is_ge(),
            "<=" => ordering.is_le(),
            "<>" | "!=" => !ordering.is_eq(),
            ">" => ordering.is_gt(),
            "<" => ordering.is_lt(),
            "=" => ordering.is_eq(),
            _ => unreachable!(),
        };
        return Ok(if matches {
            CheckTruth::True
        } else {
            CheckTruth::False
        });
    }
    anyhow::bail!("Unsupported check constraint expression '{}'", expression)
}

fn trim_check_parentheses(mut value: &str) -> &str {
    loop {
        if !value.starts_with('(') || !value.ends_with(')') {
            return value;
        }
        let mut depth = 0_u32;
        let mut quote = None;
        let mut closes_at_end = false;
        for (index, ch) in value.char_indices() {
            if let Some(active) = quote {
                if ch == active {
                    quote = None;
                }
                continue;
            }
            match ch {
                '\'' | '"' => quote = Some(ch),
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        closes_at_end = index == value.len() - 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !closes_at_end {
            return value;
        }
        value = value[1..value.len() - 1].trim();
    }
}

fn find_check_keyword(value: &str, keyword: &str) -> Option<usize> {
    let upper = value.to_ascii_uppercase();
    let mut depth = 0_u32;
    let mut quote = None;
    for (index, ch) in value.char_indices() {
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 && upper[index..].starts_with(keyword) => return Some(index),
            _ => {}
        }
    }
    None
}

fn find_check_operator(value: &str, operator: &str) -> Option<usize> {
    let mut depth = 0_u32;
    let mut quote = None;
    for (index, ch) in value.char_indices() {
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 && value[index..].starts_with(operator) => return Some(index),
            _ => {}
        }
    }
    None
}

fn parse_check_literal(value: &str) -> Vec<u8> {
    let value = value.trim();
    if value.len() >= 2
        && matches!(value.as_bytes()[0], b'\'' | b'"')
        && value.as_bytes().last() == value.as_bytes().first()
    {
        value.as_bytes()[1..value.len() - 1].to_vec()
    } else {
        value.as_bytes().to_vec()
    }
}

fn compare_check_values(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    let numeric = std::str::from_utf8(left)
        .ok()
        .and_then(|value| value.parse::<Decimal>().ok())
        .zip(
            std::str::from_utf8(right)
                .ok()
                .and_then(|value| value.parse::<Decimal>().ok()),
        );
    numeric.map_or_else(|| left.cmp(right), |(left, right)| left.cmp(&right))
}

fn auto_increment_column(schema: &TableSchema) -> Option<String> {
    let sql = schema.create_sql.as_deref()?;
    let open = sql.find('(')?;
    let close = sql.rfind(')')?;
    split_schema_definitions(&sql[open + 1..close])
        .into_iter()
        .find_map(|definition| {
            let upper = definition.to_ascii_uppercase();
            if !upper.contains("AUTO_INCREMENT") {
                return None;
            }
            let name = definition.split_whitespace().next()?.trim_matches('`');
            schema
                .columns
                .iter()
                .any(|column| column.name == name)
                .then(|| name.to_string())
        })
}

fn table_constraint_definitions(schema: &TableSchema) -> Vec<String> {
    let Some(sql) = schema.create_sql.as_deref() else {
        return Vec::new();
    };
    let Some(open) = sql.find('(') else {
        return Vec::new();
    };
    let Some(close) = sql.rfind(')') else {
        return Vec::new();
    };
    split_schema_definitions(&sql[open + 1..close])
        .into_iter()
        .filter(|definition| {
            let upper = definition.to_ascii_uppercase();
            upper.starts_with("FOREIGN KEY")
                || upper.contains(" FOREIGN KEY")
                || upper.starts_with("CHECK")
                || (upper.starts_with("CONSTRAINT ") && upper.contains(" CHECK"))
        })
        .map(str::to_string)
        .collect()
}

fn render_table_schema_sql(
    schema: &TableSchema,
    auto_increment: Option<&str>,
    constraints: &[String],
) -> String {
    let mut definitions = schema
        .columns
        .iter()
        .map(|column| {
            let default = column.default.as_ref().map_or_else(String::new, |value| {
                if value.eq_ignore_ascii_case("NULL") {
                    " DEFAULT NULL".to_string()
                } else if is_current_timestamp_default(value)
                    || (matches!(
                        column.data_type,
                        DataType::Int
                            | DataType::BigInt
                            | DataType::Float
                            | DataType::Double
                            | DataType::Boolean
                    ) && value.parse::<f64>().is_ok())
                {
                    format!(" DEFAULT {value}")
                } else {
                    format!(
                        " DEFAULT '{}'",
                        value.replace('\\', "\\\\").replace('\'', "''")
                    )
                }
            });
            format!(
                "  `{}` {}{}{}{}{}",
                column.name.replace('`', "``"),
                storage_data_type_sql(&column.data_type),
                if column.nullable { "" } else { " NOT NULL" },
                if auto_increment == Some(column.name.as_str()) {
                    " AUTO_INCREMENT"
                } else {
                    ""
                },
                if column.is_primary_key {
                    " PRIMARY KEY"
                } else {
                    ""
                },
                default,
            )
        })
        .collect::<Vec<_>>();
    if let Some(primary_key) = &schema.primary_key {
        if !schema.columns.iter().any(|column| column.is_primary_key) {
            definitions.push(format!(
                "  PRIMARY KEY ({})",
                primary_key
                    .iter()
                    .map(|column| format!("`{}`", column.replace('`', "``")))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
    }
    definitions.extend(schema.indexes.iter().map(|index| {
        format!(
            "  {}KEY `{}` ({})",
            if index.unique { "UNIQUE " } else { "" },
            index.name.replace('`', "``"),
            index
                .columns
                .iter()
                .map(|column| format!("`{}`", column.replace('`', "``")))
                .collect::<Vec<_>>()
                .join(",")
        )
    }));
    definitions.extend(
        constraints
            .iter()
            .map(|constraint| format!("  {}", constraint.trim())),
    );
    format!(
        "CREATE TABLE `{}` (\n{}\n) ENGINE={} DEFAULT CHARSET=utf8mb4",
        schema.name.replace('`', "``"),
        definitions.join(",\n"),
        if is_memory_schema(schema) {
            "MEMORY"
        } else {
            "InnoDB"
        }
    )
}

fn storage_data_type_sql(value: &DataType) -> String {
    match value {
        DataType::Int => "int".into(),
        DataType::BigInt => "bigint".into(),
        DataType::Float => "float".into(),
        DataType::Double => "double".into(),
        DataType::Varchar(length) => format!("varchar({length})"),
        DataType::Text => "text".into(),
        DataType::Blob => "blob".into(),
        DataType::Date => "date".into(),
        DataType::DateTime => "datetime".into(),
        DataType::Timestamp => "timestamp".into(),
        DataType::Boolean => "tinyint(1)".into(),
        DataType::Raw(value) => value.clone(),
    }
}

fn split_schema_definitions(value: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut depth = 0_u32;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(active) = quote {
            if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                result.push(value[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    result.push(value[start..].trim());
    result
}

fn validate_column_exists(schema: &TableSchema, column: &str) -> Result<()> {
    if schema.columns.iter().any(|item| item.name == column) {
        Ok(())
    } else {
        anyhow::bail!("Unknown column '{}'", column)
    }
}

fn primary_key_columns(schema: &TableSchema) -> Vec<String> {
    schema.primary_key.clone().unwrap_or_else(|| {
        schema
            .columns
            .iter()
            .filter(|column| column.is_primary_key)
            .map(|column| column.name.clone())
            .collect()
    })
}

fn key_constraints(schema: &TableSchema) -> Vec<(String, Vec<String>, bool)> {
    let mut constraints = Vec::new();
    let primary = primary_key_columns(schema);
    if !primary.is_empty() {
        constraints.push(("PRIMARY".to_string(), primary, false));
    }
    constraints.extend(
        schema
            .indexes
            .iter()
            .filter(|index| index.unique)
            .map(|index| (index.name.clone(), index.columns.clone(), true)),
    );
    constraints
}

fn indexed_columns(schema: &TableSchema) -> HashSet<String> {
    let mut columns: HashSet<_> = primary_key_columns(schema).into_iter().collect();
    for index in &schema.indexes {
        columns.extend(index.columns.iter().cloned());
    }
    columns
}

fn add_rows_to_page_index(
    index: &mut RowPageIndex,
    table_name: &str,
    schema: &TableSchema,
    page_number: u32,
    rows: &[Row],
) {
    for column in indexed_columns(schema) {
        for row in rows {
            if row.is_null(&column) {
                continue;
            }
            if let Some(value) = row.get(&column) {
                index
                    .entry((table_name.to_string(), column.clone(), value.to_vec()))
                    .or_default()
                    .insert(page_number);
            }
        }
    }
}

fn add_rows_to_value_index(
    index: &mut RowValueIndex,
    table_name: &str,
    schema: &TableSchema,
    first_row: usize,
    rows: &[Row],
) {
    for column in indexed_columns(schema) {
        for (offset, row) in rows.iter().enumerate() {
            if row.is_null(&column) {
                continue;
            }
            if let Some(value) = row.get(&column) {
                index
                    .entry((table_name.to_string(), column.clone(), value.to_vec()))
                    .or_default()
                    .push(first_row + offset);
            }
        }
    }
}

fn encode_key(row: &Row, columns: &[String], _nullable: bool) -> Option<Vec<u8>> {
    if columns.is_empty() {
        return None;
    }
    let mut encoded = Vec::new();
    for column in columns {
        if row.is_null(column) {
            return None;
        }
        let value = row.get(column)?;
        encoded.extend_from_slice(&(value.len() as u32).to_le_bytes());
        encoded.extend_from_slice(value);
    }
    Some(encoded)
}

// ============================================================================
// Checkpointer (background dirty page flush)
// ============================================================================

pub struct Checkpointer {
    buffer_pool: Arc<BufferPool>,
    interval: std::time::Duration,
}

impl Checkpointer {
    pub fn new(
        buffer_pool: Arc<BufferPool>,
        _data_dir: PathBuf,
        interval: std::time::Duration,
    ) -> Self {
        Self {
            buffer_pool,
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

    fn recovery_schema(name: &str) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            columns: vec![
                Column {
                    name: "id".into(),
                    data_type: DataType::BigInt,
                    nullable: false,
                    default: None,
                    is_primary_key: true,
                },
                Column {
                    name: "value".into(),
                    data_type: DataType::Varchar(64),
                    nullable: true,
                    default: None,
                    is_primary_key: false,
                },
            ],
            primary_key: Some(vec!["id".into()]),
            indexes: Vec::new(),
            triggers: Vec::new(),
            next_page_number: 0,
            generation: 0,
            create_sql: None,
            engine: TableEngine::Neko233,
        }
    }

    async fn recovery_manager() -> (tempfile::TempDir, StorageEngineManager) {
        let temp = tempfile::tempdir().unwrap();
        let manager = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "4M");
        manager.init().await.unwrap();
        manager.create_database("game").await.unwrap();
        for table in ["a", "b"] {
            manager
                .execute_write(WriteCommand::CreateTable {
                    database: "game".into(),
                    schema: recovery_schema(table),
                })
                .await
                .unwrap();
        }
        (temp, manager)
    }

    async fn recovery_manager_with_group_commit_window(
        group_commit_window: Duration,
    ) -> (tempfile::TempDir, StorageEngineManager) {
        let temp = tempfile::tempdir().unwrap();
        let manager = StorageEngineManager::try_new_with_group_commit_window(
            temp.path().to_path_buf(),
            16384,
            "4M",
            group_commit_window,
        )
        .unwrap();
        manager.init().await.unwrap();
        manager.create_database("game").await.unwrap();
        for table in ["a", "b"] {
            manager
                .execute_write(WriteCommand::CreateTable {
                    database: "game".into(),
                    schema: recovery_schema(table),
                })
                .await
                .unwrap();
        }
        (temp, manager)
    }

    fn append_fault_batch(
        manager: &StorageEngineManager,
        commands: Vec<WriteCommand>,
        commit: bool,
    ) -> u64 {
        let mut wal = manager.wal_writer.lock();
        let tx_id = wal.next_lsn();
        let mut batch = WalRecord::new(
            0,
            WalRecordType::Batch,
            tx_id,
            "",
            encode_wal_batch(&WalBatch {
                version: WAL_BATCH_VERSION,
                commands,
            })
            .unwrap(),
        );
        wal.append(&mut batch).unwrap();
        if commit {
            let mut record = WalRecord::new(0, WalRecordType::Commit, tx_id, "", Vec::new());
            wal.append(&mut record).unwrap();
        }
        wal.sync().unwrap();
        tx_id
    }

    fn append_fault_group(
        manager: &StorageEngineManager,
        transactions: Vec<Vec<WriteCommand>>,
    ) -> u64 {
        let mut wal = manager.wal_writer.lock();
        let tx_id = wal.next_lsn();
        let transaction_refs = transactions.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mut group = WalRecord::new(
            0,
            WalRecordType::GroupCommit,
            tx_id,
            "",
            encode_wal_group(&transaction_refs).unwrap(),
        );
        wal.append(&mut group).unwrap();
        wal.sync().unwrap();
        tx_id
    }

    fn insert_command(table: &str, id: &str) -> WriteCommand {
        let mut row = Row::new();
        row.push("id", id.as_bytes().to_vec());
        WriteCommand::Insert {
            database: "game".into(),
            table: table.into(),
            row,
        }
    }

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
        let mut decoded = Row::decode(&encoded).unwrap();

        assert_eq!(decoded.get("ID"), Some(42u32.to_le_bytes().as_slice()));
        assert_eq!(decoded.get("name"), Some(b"Alice".as_slice()));
        assert_eq!(decoded.get("score"), Some(99.5f64.to_le_bytes().as_slice()));
        decoded.set("NAME", b"Bob".to_vec());
        assert_eq!(decoded.get("name"), Some(b"Bob".as_slice()));
        assert_eq!(
            decoded
                .values
                .iter()
                .filter(|(column, _)| column.eq_ignore_ascii_case("name"))
                .count(),
            1
        );
        decoded.set_null("SCORE");
        assert!(decoded.is_null("score"));
        decoded.set("score", b"100".to_vec());
        assert!(!decoded.is_null("SCORE"));
    }

    #[test]
    fn table_schema_reads_legacy_bincode_without_trigger_field() {
        let encoded = bincode::serialize(&LegacyWalBatch {
            version: WAL_BATCH_VERSION,
            commands: vec![LegacyWriteCommand::CreateTable {
                database: "game".into(),
                schema: LegacyTableSchema {
                    name: "legacy".into(),
                    columns: vec![Column {
                        name: "id".into(),
                        data_type: DataType::BigInt,
                        nullable: false,
                        default: None,
                        is_primary_key: true,
                    }],
                    primary_key: Some(vec!["id".into()]),
                    indexes: Vec::new(),
                    next_page_number: 0,
                    generation: 0,
                    create_sql: Some("CREATE TABLE legacy(id BIGINT PRIMARY KEY)".into()),
                    engine: TableEngine::Neko233,
                },
            }],
        })
        .unwrap();
        let mut payload = WAL_BATCH_BINARY_MAGIC.to_vec();
        payload.extend(encoded);
        let decoded = decode_wal_batch(&payload).unwrap();
        let WriteCommand::CreateTable { schema, .. } = &decoded.commands[0] else {
            panic!("expected legacy CREATE TABLE")
        };
        assert_eq!(schema.name, "legacy");
        assert!(schema.triggers.is_empty());

        let encoded = bincode::serialize(&LegacyWalGroupV2 {
            version: WAL_GROUP_VERSION,
            committed_unix_ms: 7,
            transactions: vec![vec![LegacyWriteCommand::CreateTable {
                database: "game".into(),
                schema: LegacyTableSchema {
                    name: "legacy_group".into(),
                    columns: Vec::new(),
                    primary_key: None,
                    indexes: Vec::new(),
                    next_page_number: 0,
                    generation: 0,
                    create_sql: None,
                    engine: TableEngine::Neko233,
                },
            }]],
        })
        .unwrap();
        let mut payload = WAL_GROUP_BINARY_MAGIC_V2.to_vec();
        payload.extend(encoded);
        let decoded = decode_wal_group(&payload).unwrap();
        let WriteCommand::CreateTable { schema, .. } = &decoded.transactions[0][0] else {
            panic!("expected legacy grouped CREATE TABLE")
        };
        assert_eq!(schema.name, "legacy_group");
        assert!(schema.triggers.is_empty());
    }

    #[test]
    fn test_disk_manager_read_write_page() {
        let tmp = tempfile::tempdir().unwrap();
        let dm = DiskManager::new(tmp.path().to_path_buf(), 1024);
        dm.init().unwrap();

        let mut page = Page::new(0, PageType::Data, 1024);
        page.data[..5].copy_from_slice(b"hello");

        dm.write_page("test_table", &page).unwrap();
        let loaded = dm.read_page("test_table", 0).unwrap().unwrap();

        assert_eq!(loaded.header.page_number, 0);
        assert_eq!(&loaded.data[..5], b"hello");
    }

    #[test]
    fn disk_manager_rejects_corrupt_or_truncated_page_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let dm = DiskManager::new(tmp.path().to_path_buf(), 1024);
        dm.init().unwrap();
        dm.write_page("test_table", &Page::new(0, PageType::Data, 1024))
            .unwrap();

        let segment = tmp.path().join("test_table/pages.dat");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&segment)
            .unwrap();
        file.seek(SeekFrom::Start(PAGE_HEADER_SIZE as u64)).unwrap();
        file.write_all(&[0xff]).unwrap();
        file.sync_all().unwrap();
        let error = dm.read_page("test_table", 0).unwrap_err();
        assert!(error.to_string().contains("Page corruption"));

        let mut file = OpenOptions::new().append(true).open(segment).unwrap();
        file.write_all(&[0]).unwrap();
        file.sync_all().unwrap();
        let error = dm.list_pages("test_table").unwrap_err();
        assert!(error.to_string().contains("truncated"));
    }

    #[tokio::test]
    async fn storage_startup_refuses_corrupt_persisted_pages() {
        let temp = tempfile::tempdir().unwrap();
        let manager = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "4M");
        manager.init().await.unwrap();
        manager.create_database("game").await.unwrap();
        manager
            .execute_write(WriteCommand::CreateTable {
                database: "game".into(),
                schema: recovery_schema("corrupt_pages"),
            })
            .await
            .unwrap();
        manager
            .execute_write(insert_command("corrupt_pages", "1"))
            .await
            .unwrap();
        drop(manager);

        let segment = temp.path().join("game/corrupt_pages/pages.dat");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(segment)
            .unwrap();
        file.seek(SeekFrom::Start(PAGE_HEADER_SIZE as u64)).unwrap();
        file.write_all(&[0xff]).unwrap();
        file.sync_all().unwrap();

        let restarted =
            StorageEngineManager::try_new(temp.path().to_path_buf(), 16384, "4M").unwrap();
        let error = restarted.init().await.unwrap_err();
        assert!(error.to_string().contains("Page corruption"));
    }

    #[test]
    fn wal_batch_binary_codec_reads_legacy_json() {
        let legacy = serde_json::to_vec(&WalBatch {
            version: 1,
            commands: vec![insert_command("a", "7")],
        })
        .unwrap();
        let decoded = decode_wal_batch(&legacy).unwrap();
        assert_eq!(decoded.version, 1);

        let current = encode_wal_batch(&WalBatch {
            version: WAL_BATCH_VERSION,
            commands: vec![insert_command("a", "8")],
        })
        .unwrap();
        assert!(current.starts_with(WAL_BATCH_BINARY_MAGIC));
        assert_eq!(decode_wal_batch(&current).unwrap().version, 2);
    }

    #[test]
    fn wal_group_v2_timestamp_keeps_v1_read_compatibility() {
        let legacy_encoded = bincode::serialize(&WalGroupV1 {
            version: 1,
            transactions: vec![vec![insert_command("a", "1")]],
        })
        .unwrap();
        let mut legacy = WAL_GROUP_BINARY_MAGIC_V1.to_vec();
        legacy.extend_from_slice(&legacy_encoded);
        let decoded = decode_wal_group(&legacy).unwrap();
        assert_eq!(decoded.transactions.len(), 1);
        assert_eq!(decoded.committed_unix_ms, None);

        let current = encode_wal_group(&[&[insert_command("a", "2")]]).unwrap();
        assert!(current.starts_with(WAL_GROUP_BINARY_MAGIC_V2));
        assert!(decode_wal_group(&current)
            .unwrap()
            .committed_unix_ms
            .is_some());
    }

    #[tokio::test]
    async fn committed_batch_is_redone_once_and_marked_applied() {
        let (_temp, manager) = recovery_manager().await;
        append_fault_batch(&manager, vec![insert_command("a", "1")], true);

        manager.wal_replay().await.unwrap();
        manager.wal_replay().await.unwrap();

        let rows = manager.scan_table("game", "a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("id"), Some(b"1".as_slice()));
    }

    #[tokio::test]
    async fn atomic_group_commit_is_redone_once_and_marked_applied() {
        let (_temp, manager) = recovery_manager().await;
        append_fault_group(
            &manager,
            vec![
                vec![insert_command("a", "1")],
                vec![insert_command("b", "2")],
            ],
        );

        manager.wal_replay().await.unwrap();
        manager.wal_replay().await.unwrap();

        assert_eq!(manager.scan_table("game", "a").unwrap().len(), 1);
        assert_eq!(manager.scan_table("game", "b").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn replay_checkpoints_multiple_rewrites_of_one_table_once() {
        let (temp, manager) = recovery_manager().await;
        manager.wal_replay().await.unwrap();
        append_fault_group(&manager, vec![vec![insert_command("a", "1")]]);
        manager.wal_replay().await.unwrap();

        let update = |value: &[u8]| WriteCommand::Update {
            database: "game".into(),
            table: "a".into(),
            filter: Some(RowPredicate::Eq("id".into(), b"1".to_vec())),
            assignments: vec![("value".into(), Some(value.to_vec()))],
        };
        append_fault_group(&manager, vec![vec![update(b"first"), update(b"second")]]);

        manager.wal_replay().await.unwrap();
        manager.wal_replay().await.unwrap();

        let rows = manager.scan_table("game", "a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("value"), Some(b"second".as_slice()));
        let database_dir = temp.path().join("game");
        assert!(!database_dir.join(".rewrite-a.json").exists());
        assert!(!database_dir.join(".rewrite-a.staging").exists());
        assert!(!database_dir.join(".rewrite-a.backup").exists());
    }

    #[tokio::test]
    async fn replace_rows_wal_replay_is_idempotent_for_keyless_limit_fallback() {
        let (_temp, manager) = recovery_manager().await;
        manager.wal_replay().await.unwrap();
        append_fault_group(&manager, vec![vec![insert_command("a", "1")]]);
        manager.wal_replay().await.unwrap();

        let mut replacement = Row::new();
        replacement.push("id", b"2".to_vec());
        replacement.push("value", b"replacement".to_vec());
        append_fault_group(
            &manager,
            vec![vec![WriteCommand::ReplaceRows {
                database: "game".into(),
                table: "a".into(),
                rows: vec![replacement],
                affected_rows: 1,
            }]],
        );

        manager.wal_replay().await.unwrap();
        manager.wal_replay().await.unwrap();
        let rows = manager.scan_table("game", "a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("id"), Some(b"2".as_slice()));
        assert_eq!(rows[0].get("value"), Some(b"replacement".as_slice()));
    }

    #[tokio::test]
    async fn incremental_insert_validation_rejects_cross_request_duplicates() {
        let (_temp, manager) = recovery_manager().await;
        let mut group_keys = GroupConstraintKeys::new();
        let first = vec![insert_command("a", "1")];
        validate_insert_commands_incremental(&first, &manager.databases, &mut group_keys).unwrap();

        let duplicate = vec![insert_command("a", "1")];
        assert!(validate_insert_commands_incremental(
            &duplicate,
            &manager.databases,
            &mut group_keys
        )
        .is_err());
        let independent = vec![insert_command("a", "2")];
        validate_insert_commands_incremental(&independent, &manager.databases, &mut group_keys)
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_groups_distinct_table_insert_commits() {
        let (_temp, manager) = recovery_manager().await;
        let groups_before = manager.stats().group_commits;
        let (left, right) = tokio::join!(
            manager.execute_write(insert_command("a", "1")),
            manager.execute_write(insert_command("b", "2")),
        );
        left.unwrap();
        right.unwrap();
        assert_eq!(manager.stats().group_commits, groups_before + 1);
        assert_eq!(manager.scan_table("game", "a").unwrap().len(), 1);
        assert_eq!(manager.scan_table("game", "b").unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_grouped_commits_survive_restart_without_duplicates() {
        let (temp, manager) = recovery_manager().await;
        let groups_before = manager.stats().group_commits;
        let (left, right) = tokio::join!(
            manager.execute_write(insert_command("a", "1")),
            manager.execute_write(insert_command("b", "2")),
        );
        left.unwrap();
        right.unwrap();
        assert_eq!(manager.stats().group_commits, groups_before + 1);
        drop(manager);
        tokio::task::yield_now().await;

        let restarted = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "4M");
        restarted.init().await.unwrap();
        let left = restarted.scan_table("game", "a").unwrap();
        let right = restarted.scan_table("game", "b").unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(right.len(), 1);
        assert_eq!(left[0].get("id"), Some(b"1".as_slice()));
        assert_eq!(right[0].get("id"), Some(b"2".as_slice()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_groups_write_arriving_within_commit_window() {
        let (_temp, manager) =
            recovery_manager_with_group_commit_window(Duration::from_millis(50)).await;
        let manager = Arc::new(manager);
        let groups_before = manager.stats().group_commits;
        let requests_before = manager.stats().grouped_requests;
        let first_manager = manager.clone();
        let first =
            tokio::spawn(
                async move { first_manager.execute_write(insert_command("a", "1")).await },
            );

        tokio::time::sleep(Duration::from_millis(5)).await;
        manager
            .execute_write(insert_command("b", "2"))
            .await
            .unwrap();
        first.await.unwrap().unwrap();

        assert_eq!(manager.stats().group_commits, groups_before + 1);
        assert_eq!(manager.stats().grouped_requests, requests_before + 2);
    }

    #[tokio::test]
    async fn indexed_read_uses_current_rewrite_overlay_without_stale_rows() {
        let (_temp, manager) = recovery_manager().await;
        manager
            .execute_write(insert_command("a", "1"))
            .await
            .unwrap();
        manager
            .execute_write(WriteCommand::Update {
                database: "game".into(),
                table: "a".into(),
                filter: Some(RowPredicate::Eq("id".into(), b"1".to_vec())),
                assignments: vec![("value".into(), Some(b"latest".to_vec()))],
            })
            .await
            .unwrap();

        let database = manager.get_database("game").unwrap();
        assert!(database.pending_rewrites.read().contains_key("a"));
        let rows = database
            .scan_table_filtered_limit(
                "a",
                Some(&RowPredicate::Eq("id".into(), b"1".to_vec())),
                Some(1),
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("value"), Some(b"latest".as_slice()));
        assert_eq!(
            database
                .row_value_index
                .read()
                .get(&("a".into(), "id".into(), b"1".to_vec()))
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_groups_distinct_table_update_commits() {
        let (_temp, manager) = recovery_manager().await;
        manager
            .execute_write(insert_command("a", "1"))
            .await
            .unwrap();
        manager
            .execute_write(insert_command("b", "2"))
            .await
            .unwrap();
        let groups_before = manager.stats().group_commits;
        let (left, right) = tokio::join!(
            manager.execute_write(WriteCommand::Update {
                database: "game".into(),
                table: "a".into(),
                filter: Some(RowPredicate::Eq("id".into(), b"1".to_vec())),
                assignments: vec![("value".into(), Some(b"left".to_vec()))],
            }),
            manager.execute_write(WriteCommand::Update {
                database: "game".into(),
                table: "b".into(),
                filter: Some(RowPredicate::Eq("id".into(), b"2".to_vec())),
                assignments: vec![("value".into(), Some(b"right".to_vec()))],
            }),
        );
        left.unwrap();
        right.unwrap();
        assert_eq!(manager.stats().group_commits, groups_before + 1);
        assert_eq!(
            manager.scan_table("game", "a").unwrap()[0].get("value"),
            Some(b"left".as_slice())
        );
        assert_eq!(
            manager.scan_table("game", "b").unwrap()[0].get("value"),
            Some(b"right".as_slice())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_groups_distinct_table_upsert_commits() {
        let (_temp, manager) = recovery_manager().await;
        manager
            .execute_write(insert_command("a", "1"))
            .await
            .unwrap();
        manager
            .execute_write(insert_command("b", "2"))
            .await
            .unwrap();
        let groups_before = manager.stats().group_commits;
        let upsert = |table: &str, id: &[u8], value: &[u8]| {
            let mut row = Row::new();
            row.push("id", id.to_vec());
            row.push("value", value.to_vec());
            WriteCommand::Upsert {
                database: "game".into(),
                table: table.into(),
                row,
                update_columns: vec!["value".into()],
                ignore: false,
            }
        };
        let (left, right) = tokio::join!(
            manager.execute_write(upsert("a", b"1", b"left")),
            manager.execute_write(upsert("b", b"2", b"right")),
        );
        left.unwrap();
        right.unwrap();
        assert_eq!(manager.stats().group_commits, groups_before + 1);
        assert_eq!(
            manager.scan_table("game", "a").unwrap()[0].get("value"),
            Some(b"left".as_slice())
        );
        assert_eq!(
            manager.scan_table("game", "b").unwrap()[0].get("value"),
            Some(b"right".as_slice())
        );
    }

    #[tokio::test]
    async fn redo_never_resurrects_memory_table_rows() {
        let (_temp, manager) = recovery_manager().await;
        let mut schema = recovery_schema("volatile_state");
        schema.engine = TableEngine::Memory;
        schema.create_sql = Some(
            "CREATE TABLE volatile_state (id BIGINT PRIMARY KEY, value VARCHAR(64)) ENGINE=MEMORY"
                .into(),
        );
        manager
            .execute_write(WriteCommand::CreateTable {
                database: "game".into(),
                schema,
            })
            .await
            .unwrap();
        append_fault_group(&manager, vec![vec![insert_command("volatile_state", "1")]]);

        manager.wal_replay().await.unwrap();
        assert!(manager
            .scan_table("game", "volatile_state")
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn redo_skips_old_dml_for_a_later_durably_dropped_database() {
        let (temp, manager) = recovery_manager().await;
        // Mark setup groups applied so only the deliberately pending INSERT is
        // eligible for the next replay.
        manager.wal_replay().await.unwrap();
        append_fault_group(&manager, vec![vec![insert_command("a", "1")]]);

        // Model a later DROP DATABASE whose filesystem/catalog effect is durable
        // and whose Applied marker was also already checkpointed.
        std::fs::remove_dir_all(temp.path().join("game")).unwrap();
        manager.databases.write().remove("game");

        manager.wal_replay().await.unwrap();
        assert!(manager.get_database("game").is_none());
    }

    #[tokio::test]
    async fn actor_updates_stay_in_wal_backed_memtable_until_checkpoint() {
        let (_temp, manager) = recovery_manager().await;
        manager
            .execute_write(insert_command("a", "1"))
            .await
            .unwrap();
        manager
            .execute_write(WriteCommand::Update {
                database: "game".into(),
                table: "a".into(),
                filter: Some(RowPredicate::Eq("id".into(), b"1".to_vec())),
                assignments: vec![("value".into(), Some(b"latest".to_vec()))],
            })
            .await
            .unwrap();

        let database = manager.get_database("game").unwrap();
        assert!(database.pending_rewrites.read().contains_key("a"));
        assert_eq!(database.get_table("a").unwrap().generation, 0);
        assert_eq!(
            manager.scan_table("game", "a").unwrap()[0].get("value"),
            Some(b"latest".as_slice())
        );

        manager.flush().unwrap();
        assert!(!database.pending_rewrites.read().contains_key("a"));
        assert_eq!(database.get_table("a").unwrap().generation, 1);
        assert_eq!(
            manager.scan_table("game", "a").unwrap()[0].get("value"),
            Some(b"latest".as_slice())
        );
    }

    #[tokio::test]
    async fn consistent_flush_marks_all_completed_actor_groups_applied() {
        let (temp, manager) = recovery_manager().await;
        manager
            .execute_write(insert_command("a", "1"))
            .await
            .unwrap();

        manager.flush_consistent().await.unwrap();

        let reader = WalReader::open(temp.path().join("wal")).unwrap();
        let mut committed = HashSet::new();
        let mut applied = HashSet::new();
        reader
            .replay(|record| match record.record_type {
                WalRecordType::GroupCommit => {
                    committed.insert(record.tx_id);
                }
                WalRecordType::Applied => {
                    applied.insert(record.tx_id);
                }
                _ => {}
            })
            .unwrap();
        assert!(!committed.is_empty());
        assert!(committed.is_subset(&applied));
    }

    #[tokio::test]
    async fn batch_without_commit_is_ignored() {
        let (_temp, manager) = recovery_manager().await;
        append_fault_batch(&manager, vec![insert_command("a", "1")], false);

        manager.wal_replay().await.unwrap();

        assert!(manager.scan_table("game", "a").unwrap().is_empty());
    }

    #[tokio::test]
    async fn partially_applied_multi_table_batch_recovers_without_duplicates() {
        let (_temp, manager) = recovery_manager().await;
        let commands = vec![insert_command("a", "1"), insert_command("b", "2")];
        append_fault_batch(&manager, commands.clone(), true);

        apply_write_batch(
            vec![commands[0].clone()],
            &manager.databases,
            &manager.buffer_pool,
            &manager.wal_writer,
            &manager.data_dir,
            false,
            true,
        )
        .await
        .unwrap();
        manager.wal_replay().await.unwrap();

        assert_eq!(manager.scan_table("game", "a").unwrap().len(), 1);
        assert_eq!(manager.scan_table("game", "b").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn restart_after_recovery_apply_before_applied_marker_is_idempotent() {
        let (temp, manager) = recovery_manager().await;
        let commands = vec![insert_command("a", "1")];
        let tx_id = append_fault_batch(&manager, commands.clone(), true);

        // Model a crash after recovery has made table pages durable but before
        // it can append the WAL Applied marker.
        apply_write_batch(
            commands,
            &manager.databases,
            &manager.buffer_pool,
            &manager.wal_writer,
            &manager.data_dir,
            true,
            true,
        )
        .await
        .unwrap();
        manager
            .get_database("game")
            .unwrap()
            .checkpoint_table("a")
            .unwrap();
        drop(manager);

        let restarted =
            StorageEngineManager::try_new(temp.path().to_path_buf(), 16384, "4M").unwrap();
        restarted.init().await.unwrap();
        let rows = restarted.scan_table("game", "a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("id"), Some(b"1".as_slice()));

        let reader = WalReader::open(temp.path().join("wal")).unwrap();
        let mut applied = 0;
        reader
            .replay(|record| {
                if record.record_type == WalRecordType::Applied && record.tx_id == tx_id {
                    applied += 1;
                }
            })
            .unwrap();
        assert_eq!(applied, 1);
    }

    #[tokio::test]
    async fn replay_skips_older_insert_when_same_primary_key_has_newer_values() {
        let (_temp, manager) = recovery_manager().await;
        let mut command = insert_command("a", "1");
        if let WriteCommand::Insert { row, .. } = &mut command {
            row.push("value", b"old".to_vec());
        }
        append_fault_batch(&manager, vec![command.clone()], true);
        apply_write_batch(
            vec![command],
            &manager.databases,
            &manager.buffer_pool,
            &manager.wal_writer,
            &manager.data_dir,
            false,
            true,
        )
        .await
        .unwrap();
        manager
            .get_database("game")
            .unwrap()
            .update_rows(
                "a",
                Some(&RowPredicate::Eq("id".into(), b"1".to_vec())),
                &[("value".into(), Some(b"new".to_vec()))],
            )
            .unwrap();

        manager.wal_replay().await.unwrap();

        let rows = manager.scan_table("game", "a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("value"), Some(b"new".as_slice()));
    }

    #[tokio::test]
    async fn auto_increment_id_is_materialized_in_batch_wal() {
        let temp = tempfile::tempdir().unwrap();
        let manager = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "4M");
        manager.init().await.unwrap();
        manager.create_database("game").await.unwrap();
        let mut schema = recovery_schema("ids");
        schema.create_sql =
            Some("CREATE TABLE ids (id BIGINT AUTO_INCREMENT PRIMARY KEY) ENGINE=InnoDB".into());
        manager
            .execute_write(WriteCommand::CreateTable {
                database: "game".into(),
                schema,
            })
            .await
            .unwrap();

        let result = manager
            .execute_batch(vec![insert_command("ids", "0"), insert_command("ids", "0")])
            .await
            .unwrap();
        assert_eq!(result.last_insert_id, 1);
        assert_eq!(result.affected_rows, 2);

        let reader = WalReader::open(temp.path().join("wal")).unwrap();
        let mut ids = Vec::new();
        reader
            .replay(|record| {
                let commands = match record.record_type {
                    WalRecordType::Batch => decode_wal_batch(&record.data).unwrap().commands,
                    WalRecordType::GroupCommit => decode_wal_group(&record.data)
                        .unwrap()
                        .transactions
                        .into_iter()
                        .flatten()
                        .collect(),
                    _ => return,
                };
                for command in commands {
                    if let WriteCommand::Insert { table, row, .. } = command {
                        if table == "ids" {
                            ids.push(row.get("id").unwrap().to_vec());
                        }
                    }
                }
            })
            .unwrap();
        assert_eq!(ids, vec![b"1".to_vec(), b"2".to_vec()]);
    }

    #[tokio::test]
    async fn procedure_catalog_is_wal_backed_and_persistent() {
        let temp = tempfile::tempdir().unwrap();
        let manager = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "4M");
        manager.init().await.unwrap();
        manager.create_database("game").await.unwrap();
        manager
            .execute_write(WriteCommand::CreateProcedure {
                database: "game".into(),
                procedure: ProcedureDefinition {
                    name: "grant_coins".into(),
                    parameters: vec![ProcedureParameter {
                        name: "p_amount".into(),
                        mode: ProcedureParameterMode::In,
                        data_type: "BIGINT".into(),
                    }],
                    body: "BEGIN SET @coins=p_amount; END".into(),
                    definer: "root@localhost".into(),
                    create_sql: "CREATE PROCEDURE grant_coins(IN p_amount BIGINT) BEGIN SET @coins=p_amount; END".into(),
                },
            })
            .await
            .unwrap();
        assert_eq!(
            manager
                .get_database("game")
                .unwrap()
                .get_procedure("GRANT_COINS")
                .unwrap()
                .parameters[0]
                .name,
            "p_amount"
        );
        manager
            .execute_write(WriteCommand::AlterProcedure {
                database: "game".into(),
                procedure: "grant_coins".into(),
                create_sql: "CREATE PROCEDURE grant_coins(IN p_amount BIGINT) COMMENT 'altered' BEGIN SET @coins=p_amount; END".into(),
            })
            .await
            .unwrap();
        drop(manager);

        let restarted = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "4M");
        restarted.init().await.unwrap();
        assert!(restarted
            .get_database("game")
            .unwrap()
            .get_procedure("grant_coins")
            .is_some());
        assert!(restarted
            .get_database("game")
            .unwrap()
            .get_procedure("grant_coins")
            .unwrap()
            .create_sql
            .contains("COMMENT 'altered'"));
        restarted
            .execute_write(WriteCommand::DropProcedure {
                database: "game".into(),
                procedure: "grant_coins".into(),
            })
            .await
            .unwrap();
        assert!(restarted
            .get_database("game")
            .unwrap()
            .get_procedure("grant_coins")
            .is_none());
    }

    #[tokio::test]
    async fn procedure_v2_metadata_persists_and_legacy_catalog_upgrades() {
        let temp = tempfile::tempdir().unwrap();
        let legacy_dir = temp.path().join("legacy");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("schema.json"), "{}").unwrap();
        let legacy = HashMap::from([(
            "old_routine".to_string(),
            ProcedureDefinition {
                name: "old_routine".into(),
                parameters: Vec::new(),
                body: "BEGIN END".into(),
                definer: "root@%".into(),
                create_sql: "CREATE PROCEDURE old_routine() BEGIN END".into(),
            },
        )]);
        std::fs::write(
            legacy_dir.join("routines.json"),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let manager = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "4M");
        manager.init().await.unwrap();
        let legacy_database = manager.get_database("legacy").unwrap();
        let legacy_metadata = legacy_database
            .get_procedure_metadata("OLD_ROUTINE")
            .unwrap();
        assert!(!legacy_metadata.created.is_empty());
        assert_eq!(legacy_metadata.created, legacy_metadata.last_altered);
        assert!(legacy_metadata.sql_mode.is_empty());

        manager.create_database("game").await.unwrap();
        let created = ProcedureMetadata::new("STRICT_TRANS_TABLES".into());
        manager
            .execute_write(WriteCommand::CreateProcedureV2 {
                database: "game".into(),
                procedure: ProcedureDefinition {
                    name: "mode_routine".into(),
                    parameters: Vec::new(),
                    body: "BEGIN END".into(),
                    definer: "root@%".into(),
                    create_sql: "CREATE PROCEDURE mode_routine() BEGIN END".into(),
                },
                metadata: created.clone(),
            })
            .await
            .unwrap();
        let altered = created.altered("ANSI_QUOTES".into());
        manager
            .execute_write(WriteCommand::AlterProcedureV2 {
                database: "game".into(),
                procedure: "mode_routine".into(),
                create_sql: "CREATE PROCEDURE mode_routine() COMMENT 'v2' BEGIN END".into(),
                metadata: altered.clone(),
            })
            .await
            .unwrap();
        drop(manager);

        let restarted = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "4M");
        restarted.init().await.unwrap();
        assert_eq!(
            restarted
                .get_database("game")
                .unwrap()
                .get_procedure_metadata("MODE_ROUTINE"),
            Some(altered)
        );
        let catalog: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("game").join("routines.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(catalog["format_version"], 2);
    }

    #[tokio::test]
    async fn alter_index_preserves_auto_increment_across_schema_reload() {
        let temp = tempfile::tempdir().unwrap();
        let manager = StorageEngineManager::new(temp.path().to_path_buf(), 16384, "4M");
        manager.init().await.unwrap();
        manager.create_database("game").await.unwrap();
        let mut schema = recovery_schema("ids");
        schema.create_sql = Some(
            "CREATE TABLE ids (id BIGINT AUTO_INCREMENT PRIMARY KEY, value VARCHAR(64)) ENGINE=InnoDB"
                .into(),
        );
        manager
            .execute_write(WriteCommand::CreateTable {
                database: "game".into(),
                schema,
            })
            .await
            .unwrap();
        manager
            .execute_write(WriteCommand::AlterTable {
                database: "game".into(),
                table: "ids".into(),
                operation: AlterTableOperation::AddIndex(Index {
                    name: "value_idx".into(),
                    columns: vec!["value".into()],
                    unique: false,
                }),
            })
            .await
            .unwrap();

        let persisted = manager
            .get_database("game")
            .unwrap()
            .get_table("ids")
            .unwrap();
        assert_eq!(auto_increment_column(&persisted).as_deref(), Some("id"));
        assert!(persisted
            .create_sql
            .as_deref()
            .unwrap()
            .contains("AUTO_INCREMENT"));
        let reloaded: TableSchema =
            serde_json::from_slice(&serde_json::to_vec(&persisted).unwrap()).unwrap();
        assert_eq!(auto_increment_column(&reloaded).as_deref(), Some("id"));
        assert_eq!(
            manager
                .execute_write(insert_command("ids", "0"))
                .await
                .unwrap()
                .last_insert_id,
            1
        );
    }
}
