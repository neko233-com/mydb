use std::fs::{self, File};
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};

use anyhow::Result;
use crc32fast::Hasher;
use tracing::{debug, info, warn};

use super::record::WalRecord;
use super::writer::is_zero_filled_tail;

// WAL file header
const WAL_MAGIC: &[u8; 4] = b"WAL1";
pub struct WalReader {
    files: Vec<WalFileInfo>,
}

struct WalFileInfo {
    index: u32,
    path: PathBuf,
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
                        files.push(WalFileInfo { index, path });
                    }
                }
            }
        }

        files.sort_by_key(|f| f.index);

        info!("WAL reader: found {} files", files.len());

        Ok(Self { files })
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
            let mut counting_callback = |record| {
                total_records += 1;
                callback(record);
            };
            let last_lsn = Self::replay_file(&file_info.path, &mut counting_callback, false)?;
            max_lsn = max_lsn.max(last_lsn);
        }

        info!(
            "WAL replay complete: {} records, max_lsn={}",
            total_records, max_lsn
        );

        Ok(max_lsn)
    }

    /// Replay a single WAL file. Recovery accepts an incomplete final record;
    /// backup metadata scans use strict mode and reject every malformed byte.
    fn replay_file<F>(file_path: &Path, callback: &mut F, strict: bool) -> Result<u64>
    where
        F: FnMut(WalRecord),
    {
        let mut file = File::open(file_path)?;
        let file_len = file.metadata()?.len();

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
            let record_offset = file.stream_position()?;
            match file.read_exact(&mut lsn_buf) {
                Ok(()) => {}
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    if record_offset == file_len {
                        break;
                    }
                    if strict {
                        anyhow::bail!("torn WAL LSN at offset {record_offset}");
                    }
                    break;
                }
                Err(e) => return Err(e.into()),
            }

            // Read payload length
            let mut len_buf = [0u8; 4];
            if let Err(error) = file.read_exact(&mut len_buf) {
                if error.kind() != io::ErrorKind::UnexpectedEof {
                    return Err(error.into());
                }
                if strict {
                    anyhow::bail!("torn WAL length at offset {record_offset}");
                }
                break;
            }
            if lsn_buf == [0; 8] && len_buf == [0; 4] {
                if is_zero_filled_tail(&mut file, record_offset, file_len)? {
                    break;
                }
                anyhow::bail!(
                    "WAL corruption at offset {}: zero record header before nonzero bytes",
                    record_offset
                );
            }

            let payload_len = u32::from_le_bytes(len_buf) as usize;
            let remaining = file_len.saturating_sub(file.stream_position()?);
            let record_remaining = payload_len.checked_add(4).ok_or_else(|| {
                anyhow::anyhow!("WAL payload length overflow at offset {record_offset}")
            })? as u64;
            if remaining < record_remaining {
                if strict {
                    anyhow::bail!("torn WAL payload at offset {record_offset}");
                }
                break;
            }

            // Read payload
            let mut payload = vec![0u8; payload_len];
            file.read_exact(&mut payload)?;

            // Read CRC
            let mut crc_buf = [0u8; 4];
            file.read_exact(&mut crc_buf)?;

            let expected_crc = u32::from_le_bytes(crc_buf);

            // Verify CRC
            let mut hasher = Hasher::new();
            hasher.update(&lsn_buf);
            hasher.update(&payload);
            let actual_crc = hasher.finalize();

            if actual_crc != expected_crc {
                let record_end = file.stream_position()?;
                if strict
                    || (record_end < file_len
                        && !is_zero_filled_tail(&mut file, record_end, file_len)?)
                {
                    anyhow::bail!("WAL CRC mismatch at lsn={}", u64::from_le_bytes(lsn_buf));
                }
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
            } else if strict {
                anyhow::bail!("invalid WAL payload at lsn={lsn}");
            } else {
                warn!("invalid WAL payload at lsn={lsn} - stopping replay");
                break;
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
        let mut max_lsn = 0;
        for file_info in &self.files {
            let mut ignore = |_| {};
            max_lsn = max_lsn.max(Self::replay_file(&file_info.path, &mut ignore, true)?);
        }
        Ok(max_lsn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{encode_row, WalRecordType};
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
                let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![]);
                wal.append(&mut record).unwrap();
            }
            wal.sync().unwrap();
        }

        let reader = WalReader::open(tmp.path().into()).unwrap();
        let max = reader.max_lsn().unwrap();
        assert_eq!(max, 5);
    }

    #[test]
    fn max_lsn_rejects_terminal_crc_damage_and_torn_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_path = tmp.path().join("wal_000000.log");
        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![1]);
            wal.append(&mut record).unwrap();
            wal.sync().unwrap();
        }
        {
            use std::io::{Seek, SeekFrom, Write};

            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&wal_path)
                .unwrap();
            file.seek(SeekFrom::Start(8 + 8 + 4)).unwrap();
            file.write_all(&[0xff]).unwrap();
            file.sync_all().unwrap();
        }
        let reader = WalReader::open(tmp.path().into()).unwrap();
        assert_eq!(reader.replay(|_| {}).unwrap(), 0);
        assert!(reader.max_lsn().is_err());

        let torn = tempfile::tempdir().unwrap();
        let torn_path = torn.path().join("wal_000000.log");
        let logical_end;
        {
            let mut wal = WalWriter::open(torn.path().into(), None).unwrap();
            let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![1]);
            logical_end = 8 + 16 + record.payload_len() as u64;
            wal.append(&mut record).unwrap();
            wal.sync().unwrap();
        }
        {
            use std::io::{Seek, SeekFrom, Write};

            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(torn_path)
                .unwrap();
            file.seek(SeekFrom::Start(logical_end)).unwrap();
            file.write_all(&[2, 0, 0]).unwrap();
            file.sync_all().unwrap();
        }
        let reader = WalReader::open(torn.path().into()).unwrap();
        assert_eq!(reader.replay(|_| {}).unwrap(), 1);
        assert!(reader.max_lsn().is_err());
    }
}
