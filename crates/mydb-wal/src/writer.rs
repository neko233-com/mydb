use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use crc32fast::Hasher;
use tracing::{debug, info};

use super::record::WalRecord;

const WAL_MAGIC: &[u8; 4] = b"WAL1";

const LSN_SIZE: usize = 8;
const PAYLOAD_LEN_SIZE: usize = 4;
const CRC_SIZE: usize = 4;
const HEADER_SIZE: usize = LSN_SIZE + PAYLOAD_LEN_SIZE;
const RECORD_META_SIZE: usize = HEADER_SIZE + CRC_SIZE;

const DEFAULT_MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;
const FILE_PREALLOC_GRANULARITY: u64 = 8 * 1024 * 1024;
const SCRATCHBUF_CAPACITY: usize = 65536;
const ZERO_TAIL_SCAN_BUFFER_SIZE: usize = 8192;

pub(crate) fn is_zero_filled_tail(file: &mut File, start: u64, file_len: u64) -> Result<bool> {
    if start >= file_len {
        return Ok(true);
    }

    file.seek(SeekFrom::Start(start))?;
    let mut buffer = [0u8; ZERO_TAIL_SCAN_BUFFER_SIZE];
    let mut remaining = file_len - start;
    while remaining > 0 {
        let length = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..length])?;
        if buffer[..length].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
        remaining -= length as u64;
    }
    Ok(true)
}

pub struct WalWriter {
    dir: PathBuf,
    current_file: File,
    current_file_index: u32,
    current_file_size: u64,
    current_file_alloc_size: u64,
    max_file_size: u64,
    next_lsn: AtomicU64,
    write_buf: Vec<u8>,
}

impl WalWriter {
    pub fn next_lsn(&self) -> u64 {
        self.next_lsn.load(Ordering::Relaxed)
    }

    pub fn open(dir: PathBuf, max_file_size: Option<u64>) -> Result<Self> {
        fs::create_dir_all(&dir)?;

        let max_file_size = max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE);

        let current_file_index = Self::find_latest_file_index(&dir)?;

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

        info!(
            "WAL opened: {:?} (index={}, size={}, next_lsn={})",
            file_path, current_file_index, current_file_size, next_lsn
        );

        Ok(Self {
            dir,
            current_file: file,
            current_file_index,
            current_file_size,
            current_file_alloc_size: current_file_size,
            max_file_size,
            next_lsn: AtomicU64::new(next_lsn),
            write_buf: Vec::with_capacity(SCRATCHBUF_CAPACITY),
        })
    }

    pub fn append(&mut self, record: &mut WalRecord) -> Result<u64> {
        let lsn = self.next_lsn.fetch_add(1, Ordering::Relaxed);
        record.lsn = lsn;

        let payload_len = record.payload_len();
        let total_write = RECORD_META_SIZE + payload_len;

        if self.current_file_size + total_write as u64 > self.max_file_size {
            self.rotate_file()?;
        }

        self.ensure_space(total_write as u64)?;

        self.write_buf.clear();
        self.write_buf.reserve_exact(total_write);

        let lsn_bytes = lsn.to_le_bytes();
        let payload_len_bytes = (payload_len as u32).to_le_bytes();

        self.write_buf.extend_from_slice(&lsn_bytes);
        self.write_buf.extend_from_slice(&payload_len_bytes);
        record.write_payload_to_buf(&mut self.write_buf);

        let crc = {
            let mut hasher = Hasher::new();
            hasher.update(&lsn_bytes);
            hasher.update(&self.write_buf[HEADER_SIZE..HEADER_SIZE + payload_len]);
            hasher.finalize()
        };
        self.write_buf.extend_from_slice(&crc.to_le_bytes());

        self.current_file.write_all(&self.write_buf)?;

        self.current_file_size += total_write as u64;

        debug!(
            "WAL append: lsn={} type={} table={}",
            lsn, record.record_type, record.table_name
        );

        Ok(lsn)
    }

    #[inline]
    pub fn append_raw(
        &mut self,
        record_type: u8,
        tx_id: u64,
        table_name: &str,
        data: &[u8],
    ) -> Result<u64> {
        let lsn = self.next_lsn.fetch_add(1, Ordering::Relaxed);

        let name_bytes = table_name.as_bytes();
        let payload_len = 1 + 8 + 2 + name_bytes.len() + 4 + data.len();
        let total_write = RECORD_META_SIZE + payload_len;

        if self.current_file_size + total_write as u64 > self.max_file_size {
            self.rotate_file()?;
        }

        self.ensure_space(total_write as u64)?;

        self.write_buf.clear();
        self.write_buf.reserve_exact(total_write);

        let lsn_bytes = lsn.to_le_bytes();
        let payload_len_bytes = (payload_len as u32).to_le_bytes();
        let tx_id_bytes = tx_id.to_le_bytes();
        let name_len_bytes = (name_bytes.len() as u16).to_le_bytes();
        let data_len_bytes = (data.len() as u32).to_le_bytes();

        self.write_buf.extend_from_slice(&lsn_bytes);
        self.write_buf.extend_from_slice(&payload_len_bytes);
        self.write_buf.push(record_type);
        self.write_buf.extend_from_slice(&tx_id_bytes);
        self.write_buf.extend_from_slice(&name_len_bytes);
        self.write_buf.extend_from_slice(name_bytes);
        self.write_buf.extend_from_slice(&data_len_bytes);
        self.write_buf.extend_from_slice(data);

        let crc = {
            let mut hasher = Hasher::new();
            hasher.update(&lsn_bytes);
            hasher.update(&self.write_buf[HEADER_SIZE..HEADER_SIZE + payload_len]);
            hasher.finalize()
        };
        self.write_buf.extend_from_slice(&crc.to_le_bytes());

        self.current_file.write_all(&self.write_buf)?;
        self.current_file_size += total_write as u64;

        debug!("WAL append_raw: lsn={} type={}", lsn, record_type);
        Ok(lsn)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.current_file.sync_data()?;
        debug!("WAL synced");
        Ok(())
    }

    pub fn truncate_up_to(&mut self, lsn: u64) -> Result<()> {
        info!("WAL truncate_up_to: lsn={}", lsn);

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

    fn ensure_space(&mut self, needed: u64) -> Result<()> {
        let desired = self.current_file_size + needed;
        if desired <= self.current_file_alloc_size {
            return Ok(());
        }
        let mut new_alloc = self.current_file_alloc_size;
        while new_alloc < desired {
            new_alloc += FILE_PREALLOC_GRANULARITY;
        }
        self.current_file.set_len(new_alloc)?;
        self.current_file_alloc_size = new_alloc;
        Ok(())
    }

    fn rotate_file(&mut self) -> Result<()> {
        if self.current_file_alloc_size > self.current_file_size {
            let _ = self.current_file.set_len(self.current_file_size);
        }
        self.current_file.sync_all()?;

        self.current_file_index += 1;
        let file_path = Self::wal_file_path(&self.dir, self.current_file_index);

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&file_path)?;

        file.write_all(WAL_MAGIC)?;
        file.write_all(&0u32.to_le_bytes())?;
        let initial_size = 8u64;
        file.seek(SeekFrom::Start(initial_size))?;

        self.current_file = file;
        self.current_file_size = initial_size;
        self.current_file_alloc_size = initial_size;

        info!("WAL rotated to: {:?}", file_path);
        Ok(())
    }

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
            let record_offset = file.stream_position()?;
            match file.read_exact(&mut lsn_buf) {
                Ok(()) => {}
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }

            if file.read_exact(&mut len_buf).is_err() {
                break;
            }
            if lsn_buf == [0; LSN_SIZE] && len_buf == [0; PAYLOAD_LEN_SIZE] {
                if is_zero_filled_tail(file, record_offset, file_len)? {
                    break;
                }
                anyhow::bail!(
                    "WAL corruption at offset {}: zero record header before nonzero bytes",
                    record_offset
                );
            }
            let payload_len = u32::from_le_bytes(len_buf) as usize;
            let remaining = file_len.saturating_sub(file.stream_position()?);
            if remaining < payload_len as u64 + CRC_SIZE as u64 {
                break;
            }
            let mut payload = vec![0_u8; payload_len];
            file.read_exact(&mut payload)?;
            let mut crc_buf = [0u8; CRC_SIZE];
            file.read_exact(&mut crc_buf)?;
            let mut hasher = Hasher::new();
            hasher.update(&lsn_buf);
            hasher.update(&payload);
            let record_end = file.stream_position()?;
            if hasher.finalize() != u32::from_le_bytes(crc_buf)
                || WalRecord::decode_payload(u64::from_le_bytes(lsn_buf), &payload).is_none()
            {
                if record_end < file_len && !is_zero_filled_tail(file, record_end, file_len)? {
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

        assert!(wal.current_file_index >= 1);
    }

    #[test]
    fn test_wal_reopen_and_resume_lsn() {
        let tmp = tempfile::tempdir().unwrap();

        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            for _ in 0..5 {
                let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![1, 2, 3]);
                wal.append(&mut record).unwrap();
            }
            wal.sync().unwrap();
        }

        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![4, 5, 6]);
            let lsn = wal.append(&mut record).unwrap();
            assert_eq!(lsn, 6);
        }
    }

    #[test]
    fn sync_reopen_and_replay_accept_preallocated_zero_tail() {
        let tmp = tempfile::tempdir().expect("create temporary WAL directory");
        let wal_path = tmp.path().join("wal_000000.log");
        let logical_end;
        {
            let mut wal =
                WalWriter::open(tmp.path().into(), None).expect("open WAL for first append");
            let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![1]);
            assert_eq!(wal.append(&mut record).expect("append WAL record"), 1);
            wal.sync().expect("sync WAL record");
            logical_end = wal.current_file_size;
            assert!(
                fs::metadata(&wal_path)
                    .expect("read preallocated WAL length")
                    .len()
                    > logical_end
            );

            let reader = WalReader::open(tmp.path().into()).expect("open live WAL reader");
            let mut records = Vec::new();
            reader
                .replay(|record| records.push(record.lsn))
                .expect("replay preallocated WAL tail");
            assert_eq!(records, vec![1]);
        }

        {
            let wal = WalWriter::open(tmp.path().into(), None)
                .expect("reopen WAL and truncate its zero tail");
            assert_eq!(wal.current_file_size, logical_end);
            assert_eq!(
                fs::metadata(&wal_path)
                    .expect("read recovered WAL length")
                    .len(),
                logical_end
            );
            assert_eq!(wal.next_lsn(), 2);
        }

        let reader = WalReader::open(tmp.path().into()).expect("open recovered WAL reader");
        assert_eq!(reader.max_lsn().expect("scan recovered WAL"), 1);
    }

    #[test]
    fn reopen_truncates_torn_tail_before_new_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_path = tmp.path().join("wal_000000.log");
        let logical_end;
        {
            let mut wal = WalWriter::open(tmp.path().into(), None).unwrap();
            let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![1]);
            wal.append(&mut record).unwrap();
            wal.sync().unwrap();
            logical_end = wal.current_file_size;
        }
        {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&wal_path)
                .unwrap();
            file.seek(SeekFrom::Start(logical_end)).unwrap();
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
    fn rejects_nonzero_bytes_after_zero_preallocated_tail() {
        let tmp = tempfile::tempdir().expect("create temporary WAL directory");
        let wal_path = tmp.path().join("wal_000000.log");
        let logical_end;
        {
            let mut wal =
                WalWriter::open(tmp.path().into(), None).expect("open WAL before corruption");
            let mut record = WalRecord::new(0, WalRecordType::Insert, 1, "test", vec![1]);
            wal.append(&mut record).expect("append WAL record");
            wal.sync().expect("sync WAL record");
            logical_end = wal.current_file_size;
        }
        {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&wal_path)
                .expect("open WAL for corruption injection");
            file.seek(SeekFrom::Start(logical_end + 12))
                .expect("seek past zero record header");
            file.write_all(&[0xff])
                .expect("write nonzero byte after zero header");
            file.sync_all().expect("persist corruption injection");
        }

        assert!(WalWriter::open(tmp.path().into(), None).is_err());
        let reader = WalReader::open(tmp.path().into()).expect("open corrupt WAL reader");
        assert!(reader.replay(|_| {}).is_err());
        assert!(reader.max_lsn().is_err());
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
