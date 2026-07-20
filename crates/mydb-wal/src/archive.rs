use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use crc32fast::Hasher;

use crate::{WalReader, WalRecord, WalRecordType};

const WAL_MAGIC: &[u8; 4] = b"WAL1";
const WAL_HEADER_SIZE: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalArchiveInfo {
    pub from_lsn: u64,
    pub to_lsn: u64,
    pub record_count: u64,
    pub bytes: u64,
}

/// Export the complete retained WAL interval `(from_lsn, to_lsn]` as one
/// normal, CRC-protected WAL segment. Missing records make the operation fail
/// instead of silently producing an unusable incremental backup.
pub fn export_wal_range(
    wal_dir: &Path,
    output: &Path,
    from_lsn: u64,
    to_lsn: u64,
) -> Result<WalArchiveInfo> {
    export_wal_range_inner(wal_dir, output, from_lsn, to_lsn, false)
}

/// Export redo for an incremental backup. `Applied` belongs to the source
/// checkpoint state, which is newer than the parent full snapshot. Preserve
/// its LSN as a no-op Checkpoint so the restored chain replays every committed
/// delta without introducing LSN gaps or reusing an old LSN after restart.
pub fn export_wal_redo_range(
    wal_dir: &Path,
    output: &Path,
    from_lsn: u64,
    to_lsn: u64,
) -> Result<WalArchiveInfo> {
    export_wal_range_inner(wal_dir, output, from_lsn, to_lsn, true)
}

fn export_wal_range_inner(
    wal_dir: &Path,
    output: &Path,
    from_lsn: u64,
    to_lsn: u64,
    force_redo: bool,
) -> Result<WalArchiveInfo> {
    if to_lsn < from_lsn {
        bail!("invalid WAL range {from_lsn}..{to_lsn}");
    }
    let reader = WalReader::open(wal_dir.to_path_buf())?;
    let mut records = Vec::new();
    reader.replay(|record| {
        if record.lsn > from_lsn && record.lsn <= to_lsn {
            records.push(record);
        }
    })?;
    records.sort_by_key(|record| record.lsn);
    validate_record_sequence(&records, from_lsn, to_lsn)?;
    if force_redo {
        for record in &mut records {
            if record.record_type == WalRecordType::Applied {
                *record = WalRecord::new(record.lsn, WalRecordType::Checkpoint, 0, "", Vec::new());
            }
        }
    }

    let parent = output
        .parent()
        .ok_or_else(|| anyhow::anyhow!("WAL archive output has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = output.with_extension("log.tmp");
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(WAL_MAGIC)?;
    file.write_all(&0_u32.to_le_bytes())?;
    for record in &records {
        write_record(&mut file, record)?;
    }
    file.flush()?;
    file.sync_all()?;
    if output.exists() {
        fs::remove_file(output)?;
    }
    fs::rename(&temporary, output)?;
    sync_parent(parent)?;
    Ok(WalArchiveInfo {
        from_lsn,
        to_lsn,
        record_count: records.len() as u64,
        bytes: fs::metadata(output)?.len(),
    })
}

/// Strictly validate an archive against its manifest range. Unlike normal WAL
/// recovery, an archive never accepts a torn tail.
pub fn validate_wal_archive(
    archive: &Path,
    expected_from_lsn: u64,
    expected_to_lsn: u64,
) -> Result<WalArchiveInfo> {
    if expected_to_lsn < expected_from_lsn {
        bail!("invalid expected WAL archive range");
    }
    let (records, bytes) = read_archive_records(archive)?;
    validate_record_sequence(&records, expected_from_lsn, expected_to_lsn)?;
    Ok(WalArchiveInfo {
        from_lsn: expected_from_lsn,
        to_lsn: expected_to_lsn,
        record_count: records.len() as u64,
        bytes,
    })
}

pub fn read_wal_archive(archive: &Path) -> Result<Vec<WalRecord>> {
    read_archive_records(archive).map(|(records, _)| records)
}

/// Install a validated archive as the next WAL segment. The caller must ensure
/// the database is offline and that the current WAL ends at `from_lsn`.
pub fn install_wal_archive(
    archive: &Path,
    wal_dir: &Path,
    from_lsn: u64,
    to_lsn: u64,
) -> Result<PathBuf> {
    validate_wal_archive(archive, from_lsn, to_lsn)?;
    if from_lsn == to_lsn {
        bail!("empty WAL archive must not become the latest segment");
    }
    fs::create_dir_all(wal_dir)?;
    let next_index = next_wal_index(wal_dir)?;
    let destination = wal_dir.join(format!("wal_{next_index:06}.log"));
    let temporary = destination.with_extension("log.tmp");
    fs::copy(archive, &temporary)
        .with_context(|| format!("copy WAL archive {}", archive.display()))?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary)?
        .sync_all()?;
    fs::rename(&temporary, &destination)?;
    sync_parent(wal_dir)?;
    Ok(destination)
}

/// Install only `(from_lsn, target_lsn]` from a fully validated archive. This
/// is used for point-in-time recovery inside the final incremental segment.
pub fn install_wal_archive_prefix(
    archive: &Path,
    wal_dir: &Path,
    from_lsn: u64,
    to_lsn: u64,
    target_lsn: u64,
) -> Result<PathBuf> {
    validate_wal_archive(archive, from_lsn, to_lsn)?;
    if target_lsn <= from_lsn || target_lsn > to_lsn {
        bail!("point-in-time LSN is outside the WAL archive");
    }
    let mut records = read_wal_archive(archive)?;
    records.retain(|record| record.lsn <= target_lsn);
    validate_record_sequence(&records, from_lsn, target_lsn)?;
    fs::create_dir_all(wal_dir)?;
    let next_index = next_wal_index(wal_dir)?;
    let destination = wal_dir.join(format!("wal_{next_index:06}.log"));
    let temporary = destination.with_extension("log.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(WAL_MAGIC)?;
    file.write_all(&0_u32.to_le_bytes())?;
    for record in &records {
        write_record(&mut file, record)?;
    }
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, &destination)?;
    sync_parent(wal_dir)?;
    Ok(destination)
}

fn validate_record_sequence(records: &[WalRecord], from_lsn: u64, to_lsn: u64) -> Result<()> {
    let expected_count = to_lsn - from_lsn;
    if records.len() as u64 != expected_count {
        bail!(
            "WAL range is not fully retained: expected {expected_count} records for ({from_lsn}, {to_lsn}], found {}",
            records.len()
        );
    }
    for (offset, record) in records.iter().enumerate() {
        let expected_lsn = from_lsn + offset as u64 + 1;
        if record.lsn != expected_lsn {
            bail!(
                "WAL archive LSN gap: expected {expected_lsn}, found {}",
                record.lsn
            );
        }
    }
    Ok(())
}

fn write_record(file: &mut File, record: &WalRecord) -> Result<()> {
    let payload = record.encode_payload();
    let payload_len = u32::try_from(payload.len()).context("WAL record payload exceeds u32")?;
    let lsn = record.lsn.to_le_bytes();
    let mut hasher = Hasher::new();
    hasher.update(&lsn);
    hasher.update(&payload);
    file.write_all(&lsn)?;
    file.write_all(&payload_len.to_le_bytes())?;
    file.write_all(&payload)?;
    file.write_all(&hasher.finalize().to_le_bytes())?;
    Ok(())
}

fn read_archive_records(path: &Path) -> Result<(Vec<WalRecord>, u64)> {
    let mut file = File::open(path)?;
    let bytes = file.metadata()?.len();
    if bytes < WAL_HEADER_SIZE {
        bail!("WAL archive is shorter than its header");
    }
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)?;
    if magic != *WAL_MAGIC {
        bail!("invalid WAL archive magic");
    }
    let mut reserved = [0_u8; 4];
    file.read_exact(&mut reserved)?;
    let mut records = Vec::new();
    loop {
        let mut lsn_bytes = [0_u8; 8];
        match file.read_exact(&mut lsn_bytes) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                if file.stream_position()? == bytes {
                    break;
                }
                return Err(error).context("torn WAL archive LSN");
            }
            Err(error) => return Err(error.into()),
        }
        let mut length_bytes = [0_u8; 4];
        file.read_exact(&mut length_bytes)
            .context("torn WAL archive length")?;
        let length = u32::from_le_bytes(length_bytes) as usize;
        let mut payload = vec![0_u8; length];
        file.read_exact(&mut payload)
            .context("torn WAL archive payload")?;
        let mut crc_bytes = [0_u8; 4];
        file.read_exact(&mut crc_bytes)
            .context("torn WAL archive CRC")?;
        let mut hasher = Hasher::new();
        hasher.update(&lsn_bytes);
        hasher.update(&payload);
        if hasher.finalize() != u32::from_le_bytes(crc_bytes) {
            bail!("WAL archive CRC mismatch");
        }
        let lsn = u64::from_le_bytes(lsn_bytes);
        records.push(
            WalRecord::decode_payload(lsn, &payload)
                .ok_or_else(|| anyhow::anyhow!("invalid WAL archive record at LSN {lsn}"))?,
        );
    }
    Ok((records, bytes))
}

fn next_wal_index(wal_dir: &Path) -> Result<u32> {
    let mut maximum = None;
    for entry in fs::read_dir(wal_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(index) = name
            .strip_prefix("wal_")
            .and_then(|value| value.strip_suffix(".log"))
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        maximum = Some(maximum.map_or(index, |current: u32| current.max(index)));
    }
    Ok(maximum.map_or(0, |index| index + 1))
}

fn sync_parent(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(_path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WalRecordType, WalWriter};

    #[test]
    fn exports_validates_and_installs_exact_lsn_range() {
        let source = tempfile::tempdir().unwrap();
        let archive_dir = tempfile::tempdir().unwrap();
        let installed = tempfile::tempdir().unwrap();
        {
            let mut writer = WalWriter::open(source.path().to_path_buf(), None).unwrap();
            for value in 1..=6 {
                let mut record = WalRecord::new(
                    0,
                    WalRecordType::Insert,
                    value,
                    "players",
                    vec![value as u8],
                );
                writer.append(&mut record).unwrap();
            }
            writer.sync().unwrap();
        }

        let archive = archive_dir.path().join("wal_delta.log");
        let info = export_wal_range(source.path(), &archive, 2, 5).unwrap();
        assert_eq!(info.record_count, 3);
        assert_eq!(
            validate_wal_archive(&archive, 2, 5).unwrap().record_count,
            3
        );
        install_wal_archive(&archive, installed.path(), 2, 5).unwrap();
        let mut lsns = Vec::new();
        WalReader::open(installed.path().to_path_buf())
            .unwrap()
            .replay(|record| lsns.push(record.lsn))
            .unwrap();
        assert_eq!(lsns, vec![3, 4, 5]);
    }

    #[test]
    fn rejects_incomplete_or_torn_archive() {
        let source = tempfile::tempdir().unwrap();
        let archive_dir = tempfile::tempdir().unwrap();
        {
            let mut writer = WalWriter::open(source.path().to_path_buf(), None).unwrap();
            let mut record = WalRecord::new(0, WalRecordType::Commit, 1, "", Vec::new());
            writer.append(&mut record).unwrap();
            writer.sync().unwrap();
        }
        let archive = archive_dir.path().join("wal_delta.log");
        assert!(export_wal_range(source.path(), &archive, 0, 2).is_err());
        export_wal_range(source.path(), &archive, 0, 1).unwrap();
        let file = OpenOptions::new().write(true).open(&archive).unwrap();
        file.set_len(fs::metadata(&archive).unwrap().len() - 1)
            .unwrap();
        assert!(validate_wal_archive(&archive, 0, 1).is_err());

        let empty = archive_dir.path().join("empty.log");
        export_wal_range(source.path(), &empty, 1, 1).unwrap();
        let installed = tempfile::tempdir().unwrap();
        assert!(install_wal_archive(&empty, installed.path(), 1, 1).is_err());
    }

    #[test]
    fn redo_archive_neutralizes_applied_without_changing_lsns() {
        let source = tempfile::tempdir().unwrap();
        let archive_dir = tempfile::tempdir().unwrap();
        {
            let mut writer = WalWriter::open(source.path().to_path_buf(), None).unwrap();
            let mut group = WalRecord::new(0, WalRecordType::Commit, 1, "", Vec::new());
            writer.append(&mut group).unwrap();
            let mut applied = WalRecord::new(0, WalRecordType::Applied, 1, "", Vec::new());
            writer.append(&mut applied).unwrap();
            writer.sync().unwrap();
        }
        let archive = archive_dir.path().join("wal_000000.log");
        export_wal_redo_range(source.path(), &archive, 0, 2).unwrap();
        let reader = WalReader::open(archive_dir.path().to_path_buf()).unwrap();
        let mut records = Vec::new();
        reader
            .replay(|record| records.push((record.lsn, record.record_type)))
            .unwrap();
        assert_eq!(
            records,
            vec![(1, WalRecordType::Commit), (2, WalRecordType::Checkpoint)]
        );

        let installed = tempfile::tempdir().unwrap();
        install_wal_archive_prefix(&archive, installed.path(), 0, 2, 1).unwrap();
        let mut prefix_lsns = Vec::new();
        WalReader::open(installed.path().to_path_buf())
            .unwrap()
            .replay(|record| prefix_lsns.push(record.lsn))
            .unwrap();
        assert_eq!(prefix_lsns, vec![1]);
    }
}
