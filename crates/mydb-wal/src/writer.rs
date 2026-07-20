use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
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
    /// The LSN that will be assigned to the next appended record.
    pub fn next_lsn(&self) -> u64 {
        self.next_lsn.load(Ordering::SeqCst)
    }

    /// Open or create WAL directory
    pub fn open(dir: PathBuf, max_file_size: Option<u64>) -> Result<Self> {
        fs::create_dir_all(&dir)?;

        let max_file_size = max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE);

        // Find the latest WAL file index
        let current_file_index = Self::find_latest_file_index(&dir)?;

        // Open or create the current WAL file. Validate and truncate a torn or
        // CRC-invalid tail before appending; otherwise one partial record would
        // hide every valid record appended after the next restart.
        let file_path = Self::wal_file_path(&dir, current_file_index);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&file_path)?;
        let (current_file_size, next_lsn) = if file.metadata()?.len() < 8 {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(WAL_MAGIC)?;
            file.write_all(&0u32.to_le_bytes())?;
            file.sync_all()?;
            (8, 1)
        } else {
            Self::recover_valid_tail(&mut file)?
        };
        file.seek(SeekFrom::Start(current_file_size))?;
        let writer = BufWriter::new(file);

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

        debug!(
            "WAL append: lsn={} type={} table={}",
            lsn, record.record_type, record.table_name
        );

        Ok(lsn)
    }

    /// Flush (fsync) all buffered data to disk
    pub fn sync(&mut self) -> Result<()> {
        let _lock = self.flush_lock.lock();
        self.current_file.flush()?;
        // The WAL file/header already exists; fdatasync persists appended bytes
        // and the file length without forcing unrelated inode metadata each group.
        self.current_file.get_ref().sync_data()?;
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

    /// Return the valid byte length and next LSN, truncating only a torn tail.
    ///
    /// A complete record with an invalid checksum before later bytes is durable
    /// corruption, not a recoverable tail. Refuse to discard the later records.
    fn recover_valid_tail(file: &mut File) -> Result<(u64, u64)> {
        let file_len = file.metadata()?.len();
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0_u8; 4];
        file.read_exact(&mut magic)?;
        if magic != *WAL_MAGIC {
            anyhow::bail!("invalid WAL magic");
        }
        let mut reserved = [0_u8; 4];
        file.read_exact(&mut reserved)?;

        let mut valid_len = 8_u64;
        let mut last_lsn = 0u64;
        let mut lsn_buf = [0u8; 8];
        let mut len_buf = [0u8; 4];
        loop {
            match file.read_exact(&mut lsn_buf) {
                Ok(()) => {}
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }

            if file.read_exact(&mut len_buf).is_err() {
                break;
            }
            let payload_len = u32::from_le_bytes(len_buf) as usize;
            let remaining = file_len.saturating_sub(file.stream_position()?);
            if remaining < payload_len as u64 + CRC_SIZE as u64 {
                break;
            }
            let mut payload = vec![0_u8; payload_len];
            file.read_exact(&mut payload)?;
            let mut crc_buf = [0_u8; CRC_SIZE];
            file.read_exact(&mut crc_buf)?;
            let mut hasher = Hasher::new();
            hasher.update(&lsn_buf);
            hasher.update(&payload);
            let record_end = file.stream_position()?;
            if hasher.finalize() != u32::from_le_bytes(crc_buf)
                || WalRecord::decode_payload(u64::from_le_bytes(lsn_buf), &payload).is_none()
            {
                if record_end < file_len {
                    anyhow::bail!(
                        "WAL corruption at offset {} before later durable bytes",
                        valid_len
                    );
                }
                break;
            }
            last_lsn = u64::from_le_bytes(lsn_buf);
            valid_len = file.stream_position()?;
        }
        if valid_len != file_len {
            file.set_len(valid_len)?;
            file.sync_all()?;
        }
        Ok((valid_len, last_lsn.saturating_add(1).max(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::WalReader;
    use crate::record::{encode_row, WalRecordType};

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
                let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![1, 2, 3]);
                wal.append(&mut record).unwrap();
            }
            wal.sync().unwrap();
        }

        // Second session: reopen and check LSN continues
        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![4, 5, 6]);
            let lsn = wal.append(&mut record).unwrap();
            assert_eq!(lsn, 6); // should continue from 5
        }
    }

    #[test]
    fn reopen_truncates_torn_tail_before_new_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_path = tmp.path().join("wal_000000.log");
        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![1]);
            wal.append(&mut record).unwrap();
            wal.sync().unwrap();
        }
        {
            let mut file = OpenOptions::new().append(true).open(&wal_path).unwrap();
            file.write_all(&2_u64.to_le_bytes()).unwrap();
            file.write_all(&100_u32.to_le_bytes()).unwrap();
            file.write_all(&[0xaa, 0xbb]).unwrap();
            file.sync_all().unwrap();
        }
        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            let mut record = WalRecord::new(0, WalRecordType::Insert, 2, "test", vec![2]);
            assert_eq!(wal.append(&mut record).unwrap(), 2);
            wal.sync().unwrap();
        }

        let reader = WalReader::open(tmp.path().into()).unwrap();
        let mut records = Vec::new();
        reader.replay(|record| records.push(record)).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].lsn, 1);
        assert_eq!(records[1].lsn, 2);
    }

    #[test]
    fn rejects_middle_record_corruption_without_discarding_later_records() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_path = tmp.path().join("wal_000000.log");
        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            for value in 1..=3 {
                let mut record =
                    WalRecord::new(0, WalRecordType::Insert, value, "test", vec![value as u8]);
                wal.append(&mut record).unwrap();
            }
            wal.sync().unwrap();
        }
        {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&wal_path)
                .unwrap();
            // Header, LSN, and payload length precede the first payload byte.
            file.seek(SeekFrom::Start(8 + 8 + 4)).unwrap();
            file.write_all(&[0xff]).unwrap();
            file.sync_all().unwrap();
        }

        assert!(WalWriter::open(tmp.path().into(), None).is_err());
        let reader = WalReader::open(tmp.path().into()).unwrap();
        assert!(reader.replay(|_| {}).is_err());
        assert!(reader.max_lsn().is_err());
    }
}
