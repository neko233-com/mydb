use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use bytes::{Buf, BufMut, BytesMut};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;
use tracing::{debug, error, info, warn};

// Storage Engine Types
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum StorageEngine {
    InnoDB,
    MyISAM,
}

impl StorageEngine {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "innodb" => Self::InnoDB,
            "myisam" => Self::MyISAM,
            _ => Self::InnoDB,
        }
    }
}

// Page Types
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PageType {
    Index,
    Data,
    Undo,
    Redo,
    System,
}

// Page Header
#[derive(Debug, Clone)]
pub struct PageHeader {
    pub page_number: u32,
    pub page_type: PageType,
    pub lsn: u64, // Log Sequence Number
    pub prev_page: Option<u32>,
    pub next_page: Option<u32>,
    pub is_dirty: bool,
}

// Page
#[derive(Debug, Clone)]
pub struct Page {
    pub header: PageHeader,
    pub data: Vec<u8>,
    pub size: usize,
}

// Buffer Pool
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
            // Simple eviction: remove oldest page
            if let Some((&oldest, _)) = pages.iter().min_by_key(|(_, p)| p.header.lsn) {
                pages.remove(&oldest);
            }
        }
        pages.insert(page.header.page_number, page);
    }

    pub fn flush_all(&self) -> Result<()> {
        let pages = self.pages.read();
        for (page_number, page) in pages.iter() {
            if page.header.is_dirty {
                debug!("Flushing page {}", page_number);
            }
        }
        Ok(())
    }
}

// Table Schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<Column>,
    pub primary_key: Option<Vec<String>>,
    pub indexes: Vec<Index>,
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

// Database
pub struct Database {
    pub name: String,
    pub tables: RwLock<HashMap<String, TableSchema>>,
    pub buffer_pool: Arc<BufferPool>,
    data_dir: PathBuf,
}

impl Database {
    pub fn new(name: &str, data_dir: PathBuf, buffer_pool: Arc<BufferPool>) -> Self {
        Self {
            name: name.to_string(),
            tables: RwLock::new(HashMap::new()),
            buffer_pool,
            data_dir,
        }
    }

    pub async fn load(&mut self) -> Result<()> {
        let schema_file = self.data_dir.join("schema.json");
        if schema_file.exists() {
            let content = fs::read_to_string(&schema_file).await?;
            let schemas: HashMap<String, TableSchema> = serde_json::from_str(&content)?;
            *self.tables.write() = schemas;
        }
        Ok(())
    }

    pub async fn save(&self) -> Result<()> {
        let schema_file = self.data_dir.join("schema.json");
        let schemas = self.tables.read().clone();
        let content = serde_json::to_string_pretty(&schemas)?;
        fs::write(schema_file, content).await?;
        Ok(())
    }

    pub fn create_table(&self, schema: TableSchema) -> Result<()> {
        let mut tables = self.tables.write();
        if tables.contains_key(&schema.name) {
            anyhow::bail!("Table '{}' already exists", schema.name);
        }
        tables.insert(schema.name.clone(), schema);
        Ok(())
    }

    pub fn drop_table(&self, name: &str) -> Result<()> {
        let mut tables = self.tables.write();
        tables.remove(name).ok_or_else(|| anyhow::anyhow!("Table '{}' does not exist", name))?;
        Ok(())
    }

    pub fn get_table(&self, name: &str) -> Option<TableSchema> {
        self.tables.read().get(name).cloned()
    }

    pub fn list_tables(&self) -> Vec<String> {
        self.tables.read().keys().cloned().collect()
    }
}

// Storage Engine
pub struct StorageEngineManager {
    databases: RwLock<HashMap<String, Arc<Database>>>,
    buffer_pool: Arc<BufferPool>,
    data_dir: PathBuf,
}

impl StorageEngineManager {
    pub fn new(data_dir: PathBuf, page_size: usize, buffer_pool_size: &str) -> Self {
        let buffer_pool = Arc::new(BufferPool::new(page_size, buffer_pool_size));

        Self {
            databases: RwLock::new(HashMap::new()),
            buffer_pool,
            data_dir,
        }
    }

    pub async fn init(&self) -> Result<()> {
        // Create data directory if not exists
        fs::create_dir_all(&self.data_dir).await?;

        // Load existing databases
        let mut entries = fs::read_dir(&self.data_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                let db_name = entry.file_name().to_string_lossy().to_string();
                let db_path = entry.path();

                let mut db = Database::new(&db_name, db_path, self.buffer_pool.clone());
                db.load().await?;

                self.databases
                    .write()
                    .insert(db_name, Arc::new(db));
            }
        }

        info!("Storage engine initialized with {} databases", self.databases.read().len());
        Ok(())
    }

    pub async fn create_database(&self, name: &str) -> Result<()> {
        let db_path = self.data_dir.join(name);
        fs::create_dir_all(&db_path).await?;

        let db = Database::new(name, db_path, self.buffer_pool.clone());
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
            fs::remove_dir_all(&db_path).await?;
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
}

// Page operations
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
}
