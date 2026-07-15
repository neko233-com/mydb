use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Result};
use crc32fast::Hasher;
use parking_lot::Mutex;
use tracing::{debug, info};

use super::record::WalRecord;

// WAL file header magic
const WAL_MAGIC: &[u8; 4] = b"WAL1";

// WAL record layout:
// [LSN 8B][payload_len 4B][payload NB][CRC32 4B]
const LSN_SIZE: usize = 8;
const PAYLOAD_LEN_SIZE: usize = 4;
const CRC_SIZE: usize = 4;
const HEADER_SIZE: usize = LSN_SIZE + PAYLOAD_LEN_SIZE;

// Default max WAL file size: 64MB
const DEFAULT_MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

pub struct WalWriter {
    dir: PathBuf,
    current_file: BufWriter<File>,
    current_file_index: u32,
    current_file_size: u64,
    max_file_size: u64,
    next_lsn: AtomicU64,
    flush_lock: Mutex<()>,
}

impl WalWriter {
    /// Open or create WAL directory
    pub fn open(dir: PathBuf, max_file_size: Option<u64>) -> Result<Self> {
        fs::create_dir_all(&dir)?;

        let max_file_size = max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE);

        // Find the latest WAL file index
        let current_file_index = Self::find_latest_file_index(&dir)?;

        // Open or create the current WAL file
        let file_path = Self::wal_file_path(&dir, current_file_index);
        let needs_header = !file_path.exists();

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)?;

        let current_file_size = file.metadata()?.len();

        let mut writer = BufWriter::new(file);

        // Write header if new file
        if needs_header {
            writer.write_all(WAL_MAGIC)?;
            writer.write_all(&0u32.to_le_bytes())?; // reserved
        }

        // Determine next LSN by scanning existing records
        let next_lsn = if current_file_size > 0 {
            Self::scan_last_lsn(&file_path)?
        } else {
            1
        };

        info!(
            "WAL opened: {:?} (index={}, size={}, next_lsn={})",
            file_path, current_file_index, current_file_size, next_lsn
        );

        Ok(Self {
            dir,
            current_file: writer,
            current_file_index,
            current_file_size,
            max_file_size,
            next_lsn: AtomicU64::new(next_lsn),
            flush_lock: Mutex::new(()),
        })
    }

    /// Append a WAL record and return its LSN
    pub fn append(&mut self, record: &mut WalRecord) -> Result<u64> {
        let lsn = self.next_lsn.fetch_add(1, Ordering::SeqCst);
        record.lsn = lsn;

        // Check if we need a new file
        let estimated_size = HEADER_SIZE + record.payload_size() + CRC_SIZE;
        if self.current_file_size + estimated_size as u64 > self.max_file_size {
            self.rotate_file()?;
        }

        // Encode payload
        let payload = record.encode_payload();
        let payload_len = payload.len() as u32;

        // Compute CRC over LSN + payload
        let mut hasher = Hasher::new();
        hasher.update(&lsn.to_le_bytes());
        hasher.update(&payload);
        let crc = hasher.finalize();

        // Write: [LSN][payload_len][payload][CRC32]
        self.current_file.write_all(&lsn.to_le_bytes())?;
        self.current_file.write_all(&payload_len.to_le_bytes())?;
        self.current_file.write_all(&payload)?;
        self.current_file.write_all(&crc.to_le_bytes())?;

        self.current_file_size += (HEADER_SIZE + payload.len() + CRC_SIZE) as u64;

        debug!("WAL append: lsn={} type={} table={}", lsn, record.record_type, record.table_name);

        Ok(lsn)
    }

    /// Flush (fsync) all buffered data to disk
    pub fn sync(&mut self) -> Result<()> {
        let _lock = self.flush_lock.lock();
        self.current_file.flush()?;
        self.current_file.get_ref().sync_all()?;
        debug!("WAL synced");
        Ok(())
    }

    /// Truncate WAL files up to (and including) the given LSN
    pub fn truncate_up_to(&mut self, lsn: u64) -> Result<()> {
        // For simplicity: if truncating within current file, we just update metadata
        // In production, you'd delete old WAL files entirely
        info!("WAL truncate_up_to: lsn={}", lsn);

        // Scan all WAL files and remove ones that are fully before this LSN
        let entries: Vec<_> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .starts_with("wal_")
            })
            .collect();

        for entry in entries {
            let path = entry.path();
            let index = Self::parse_file_index(&path);
            if let Some(idx) = index {
                if idx < self.current_file_index {
                    debug!("Removing old WAL file: {:?}", path);
                    fs::remove_file(&path).ok();
                }
            }
        }

        Ok(())
    }

    /// Rotate to a new WAL file
    fn rotate_file(&mut self) -> Result<()> {
        self.current_file.flush()?;
        self.current_file.get_ref().sync_all()?;

        self.current_file_index += 1;
        let file_path = Self::wal_file_path(&self.dir, self.current_file_index);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)?;

        let mut writer = BufWriter::new(file);
        writer.write_all(WAL_MAGIC)?;
        writer.write_all(&0u32.to_le_bytes())?;

        self.current_file = writer;
        self.current_file_size = 8; // magic + reserved

        info!("WAL rotated to: {:?}", file_path);
        Ok(())
    }

    /// Find the latest WAL file index
    fn find_latest_file_index(dir: &Path) -> Result<u32> {
        let mut max_index = 0u32;

        if dir.exists() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                if let Some(idx) = Self::parse_file_index(&entry.path()) {
                    max_index = max_index.max(idx);
                }
            }
        }

        Ok(max_index)
    }

    fn parse_file_index(path: &Path) -> Option<u32> {
        let name = path.file_name()?.to_string_lossy();
        if name.starts_with("wal_") && name.ends_with(".log") {
            let index_str = &name[4..name.len() - 4];
            index_str.parse().ok()
        } else {
            None
        }
    }

    fn wal_file_path(dir: &Path, index: u32) -> PathBuf {
        dir.join(format!("wal_{:06}.log", index))
    }

    /// Scan a WAL file to find the last LSN
    fn scan_last_lsn(file_path: &Path) -> Result<u64> {
        use std::io::Read;

        let mut file = File::open(file_path)?;
        let mut last_lsn = 0u64;

        // Skip header (magic 4 bytes + reserved 4 bytes)
        file.seek(SeekFrom::Start(8))?;

        let mut lsn_buf = [0u8; 8];
        let mut len_buf = [0u8; 4];
        let mut crc_buf = [0u8; 4];

        loop {
            match file.read_exact(&mut lsn_buf) {
                Ok(()) => {}
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }

            if file.read_exact(&mut len_buf).is_err() {
                break;
            }

            let payload_len = u32::from_le_bytes(len_buf) as u64;
            let payload_size = payload_len + CRC_SIZE as u64;

            // Skip payload + CRC
            if file.seek(SeekFrom::Current(payload_size as i64)).is_err() {
                break;
            }

            last_lsn = u64::from_le_bytes(lsn_buf);
        }

        Ok(last_lsn + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{WalRecordType, encode_row};

    #[test]
    fn test_wal_append_and_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let mut wal = WalWriter::open(tmp.path().into(), Some(1024)).unwrap();

        let mut record = WalRecord::new(
            0,
            WalRecordType::Insert,
            1,
            "test",
            encode_row(&[("id".to_string(), 1u32.to_le_bytes().to_vec())]),
        );

        let lsn = wal.append(&mut record).unwrap();
        assert_eq!(lsn, 1);

        let lsn2 = wal.append(&mut record).unwrap();
        assert_eq!(lsn2, 2);

        wal.sync().unwrap();
    }

    #[test]
    fn test_wal_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        // Very small max file size to force rotation
        let mut wal = WalWriter::open(tmp.path().into(), Some(50)).unwrap();

        for i in 0..20 {
            let mut record = WalRecord::new(
                0,
                WalRecordType::Insert,
                1,
                "test",
                encode_row(&[("id".to_string(), (i as u32).to_le_bytes().to_vec())]),
            );
            wal.append(&mut record).unwrap();
        }

        // Should have rotated to at least file index 1
        assert!(wal.current_file_index >= 1);
    }

    #[test]
    fn test_wal_reopen_and_resume_lsn() {
        let tmp = tempfile::tempdir().unwrap();

        // First session: write some records
        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            for _ in 0..5 {
                let mut record = WalRecord::new(
                    0,
                    WalRecordType::Insert,
                    1,
                    "test",
                    vec![1, 2, 3],
                );
                wal.append(&mut record).unwrap();
            }
            wal.sync().unwrap();
        }

        // Second session: reopen and check LSN continues
        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            let mut record = WalRecord::new(
                0,
                WalRecordType::Insert,
                1,
                "test",
                vec![4, 5, 6],
            );
            let lsn = wal.append(&mut record).unwrap();
            assert_eq!(lsn, 6); // should continue from 5
        }
    }
}
