use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::Result;
use crc32fast::Hasher;
use tracing::{debug, info, warn};

use super::record::{WalRecord, WalRecordType};

// WAL file header
const WAL_MAGIC: &[u8; 4] = b"WAL1";
const CRC_SIZE: usize = 4;
const LSN_SIZE: usize = 8;
const PAYLOAD_LEN_SIZE: usize = 4;
const HEADER_SIZE: usize = LSN_SIZE + PAYLOAD_LEN_SIZE;

pub struct WalReader {
    dir: PathBuf,
    files: Vec<WalFileInfo>,
}

struct WalFileInfo {
    index: u32,
    path: PathBuf,
    size: u64,
}

impl WalReader {
    /// Open WAL directory for reading
    pub fn open(dir: PathBuf) -> Result<Self> {
        let mut files = Vec::new();

        if dir.exists() {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy();

                if name.starts_with("wal_") && name.ends_with(".log") {
                    let index_str = &name[4..name.len() - 4];
                    if let Ok(index) = index_str.parse::<u32>() {
                        let size = entry.metadata()?.len();
                        files.push(WalFileInfo {
                            index,
                            path,
                            size,
                        });
                    }
                }
            }
        }

        files.sort_by_key(|f| f.index);

        info!("WAL reader: found {} files", files.len());

        Ok(Self { dir, files })
    }

    /// Replay all WAL records starting from the given LSN
    /// Calls `callback` for each record
    pub fn replay<F>(&self, mut callback: F) -> Result<u64>
    where
        F: FnMut(WalRecord),
    {
        let mut max_lsn = 0u64;
        let mut total_records = 0u64;

        for file_info in &self.files {
            match Self::replay_file(&file_info.path, &mut callback) {
                Ok(last_lsn) => {
                    max_lsn = max_lsn.max(last_lsn);
                }
                Err(e) => {
                    warn!("Error replaying WAL file {:?}: {}", file_info.path, e);
                    // Continue with other files
                }
            }
        }

        info!(
            "WAL replay complete: {} records, max_lsn={}",
            total_records, max_lsn
        );

        Ok(max_lsn)
    }

    /// Replay a single WAL file
    fn replay_file<F>(file_path: &Path, callback: &mut F) -> Result<u64>
    where
        F: FnMut(WalRecord),
    {
        let mut file = File::open(file_path)?;

        // Read and verify header
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if magic != *WAL_MAGIC {
            anyhow::bail!("Invalid WAL magic in {:?}", file_path);
        }

        let mut reserved = [0u8; 4];
        file.read_exact(&mut reserved)?;

        let mut last_lsn = 0u64;
        let mut record_count = 0u64;

        loop {
            // Read LSN
            let mut lsn_buf = [0u8; 8];
            match file.read_exact(&mut lsn_buf) {
                Ok(()) => {}
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }

            // Read payload length
            let mut len_buf = [0u8; 4];
            if file.read_exact(&mut len_buf).is_err() {
                break;
            }

            let payload_len = u32::from_le_bytes(len_buf) as usize;

            // Read payload
            let mut payload = vec![0u8; payload_len];
            if file.read_exact(&mut payload).is_err() {
                break;
            }

            // Read CRC
            let mut crc_buf = [0u8; 4];
            if file.read_exact(&mut crc_buf).is_err() {
                break;
            }

            let expected_crc = u32::from_le_bytes(crc_buf);

            // Verify CRC
            let mut hasher = Hasher::new();
            hasher.update(&lsn_buf);
            hasher.update(&payload);
            let actual_crc = hasher.finalize();

            if actual_crc != expected_crc {
                warn!(
                    "WAL CRC mismatch at lsn={}: expected={:08x} actual={:08x} - stopping replay",
                    u64::from_le_bytes(lsn_buf),
                    expected_crc,
                    actual_crc
                );
                break;
            }

            // Decode record
            let lsn = u64::from_le_bytes(lsn_buf);
            if let Some(record) = WalRecord::decode_payload(lsn, &payload) {
                callback(record);
                last_lsn = lsn;
                record_count += 1;
            }
        }

        debug!(
            "Replayed {} records from {:?}, last_lsn={}",
            record_count, file_path, last_lsn
        );

        Ok(last_lsn)
    }

    /// Get the maximum LSN across all WAL files
    pub fn max_lsn(&self) -> Result<u64> {
        let mut max_lsn = 0u64;

        for file_info in &self.files {
            let mut file = File::open(&file_info.path)?;

            // Skip header
            file.seek(SeekFrom::Start(8))?;

            let mut lsn_buf = [0u8; 8];
            let mut len_buf = [0u8; 4];

            loop {
                match file.read_exact(&mut lsn_buf) {
                    Ok(()) => {}
                    Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(_) => break,
                }

                if file.read_exact(&mut len_buf).is_err() {
                    break;
                }

                let payload_len = u32::from_le_bytes(len_buf) as u64;
                if file
                    .seek(SeekFrom::Current((payload_len + CRC_SIZE as u64) as i64))
                    .is_err()
                {
                    break;
                }

                max_lsn = max_lsn.max(u64::from_le_bytes(lsn_buf));
            }
        }

        Ok(max_lsn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{WalRecordType, encode_row};
    use crate::writer::WalWriter;

    #[test]
    fn test_replay_records() {
        let tmp = tempfile::tempdir().unwrap();

        // Write some records
        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();

            for i in 0..10 {
                let mut record = WalRecord::new(
                    0,
                    WalRecordType::Insert,
                    1,
                    "users",
                    encode_row(&[("id".to_string(), (i as u32).to_le_bytes().to_vec())]),
                );
                wal.append(&mut record).unwrap();
            }
            wal.sync().unwrap();
        }

        // Read them back
        let reader = WalReader::open(tmp.path().into()).unwrap();
        let mut count = 0u64;
        let mut last_lsn = 0u64;

        reader
            .replay(|record| {
                count += 1;
                last_lsn = record.lsn;
                assert_eq!(record.record_type, WalRecordType::Insert);
                assert_eq!(record.table_name, "users");
            })
            .unwrap();

        assert_eq!(count, 10);
        assert_eq!(last_lsn, 10);
    }

    #[test]
    fn test_max_lsn() {
        let tmp = tempfile::tempdir().unwrap();

        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            for _ in 0..5 {
                let mut record = WalRecord::new(
                    0,
                    WalRecordType::Insert,
                    1,
                    "test",
                    vec![],
                );
                wal.append(&mut record).unwrap();
            }
            wal.sync().unwrap();
        }

        let reader = WalReader::open(tmp.path().into()).unwrap();
        let max = reader.max_lsn().unwrap();
        assert_eq!(max, 5);
    }
}
