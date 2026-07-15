use std::fmt;

/// WAL record types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WalRecordType {
    Insert = 0x01,
    Update = 0x02,
    Delete = 0x03,
    CreateTable = 0x04,
    DropTable = 0x05,
    Checkpoint = 0x06,
    Commit = 0x07,
    Rollback = 0x08,
}

impl WalRecordType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::Insert),
            0x02 => Some(Self::Update),
            0x03 => Some(Self::Delete),
            0x04 => Some(Self::CreateTable),
            0x05 => Some(Self::DropTable),
            0x06 => Some(Self::Checkpoint),
            0x07 => Some(Self::Commit),
            0x08 => Some(Self::Rollback),
            _ => None,
        }
    }
}

impl fmt::Display for WalRecordType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Insert => write!(f, "INSERT"),
            Self::Update => write!(f, "UPDATE"),
            Self::Delete => write!(f, "DELETE"),
            Self::CreateTable => write!(f, "CREATE_TABLE"),
            Self::DropTable => write!(f, "DROP_TABLE"),
            Self::Checkpoint => write!(f, "CHECKPOINT"),
            Self::Commit => write!(f, "COMMIT"),
            Self::Rollback => write!(f, "ROLLBACK"),
        }
    }
}

/// A single WAL record
#[derive(Debug, Clone)]
pub struct WalRecord {
    pub lsn: u64,
    pub record_type: WalRecordType,
    pub tx_id: u64,
    pub table_name: String,
    pub data: Vec<u8>,
}

impl WalRecord {
    /// Create a new WAL record
    pub fn new(
        lsn: u64,
        record_type: WalRecordType,
        tx_id: u64,
        table_name: &str,
        data: Vec<u8>,
    ) -> Self {
        Self {
            lsn,
            record_type,
            tx_id,
            table_name: table_name.to_string(),
            data,
        }
    }

    /// Serialize record to bytes (without LSN and CRC, those are added by writer)
    pub fn encode_payload(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Record type (1 byte)
        buf.push(self.record_type as u8);

        // Transaction ID (8 bytes)
        buf.extend_from_slice(&self.tx_id.to_le_bytes());

        // Table name length (2 bytes) + table name
        let name_bytes = self.table_name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);

        // Data length (4 bytes) + data
        buf.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.data);

        buf
    }

    /// Deserialize record payload from bytes
    pub fn decode_payload(lsn: u64, payload: &[u8]) -> Option<Self> {
        if payload.len() < 15 {
            // minimum: 1 (type) + 8 (tx_id) + 2 (name_len) + 0 (name) + 4 (data_len)
            return None;
        }

        let mut pos = 0;

        // Record type
        let record_type = WalRecordType::from_u8(payload[pos])?;
        pos += 1;

        // Transaction ID
        let tx_id = u64::from_le_bytes(payload[pos..pos + 8].try_into().ok()?);
        pos += 8;

        // Table name
        let name_len = u16::from_le_bytes(payload[pos..pos + 2].try_into().ok()?) as usize;
        pos += 2;
        if pos + name_len > payload.len() {
            return None;
        }
        let table_name = String::from_utf8_lossy(&payload[pos..pos + name_len]).to_string();
        pos += name_len;

        // Data
        if pos + 4 > payload.len() {
            return None;
        }
        let data_len = u32::from_le_bytes(payload[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;
        if pos + data_len > payload.len() {
            return None;
        }
        let data = payload[pos..pos + data_len].to_vec();

        Some(Self {
            lsn,
            record_type,
            tx_id,
            table_name,
            data,
        })
    }

    /// Total serialized size (payload only, without LSN header and CRC trailer)
    pub fn payload_size(&self) -> usize {
        1 + 8 + 2 + self.table_name.len() + 4 + self.data.len()
    }
}

/// Serialize a row as insert/update data
pub fn encode_row(row: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut buf = Vec::new();

    // Number of columns
    buf.push(row.len() as u8);

    for (name, value) in row {
        // Column name length + name
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(name.as_bytes());

        // Value length + value
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        buf.extend_from_slice(value);
    }

    buf
}

/// Deserialize row data
pub fn decode_row(data: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    if data.is_empty() {
        return None;
    }

    let mut pos = 0;
    let col_count = data[pos] as usize;
    pos += 1;

    let mut columns = Vec::with_capacity(col_count);

    for _ in 0..col_count {
        if pos + 2 > data.len() {
            return None;
        }
        let name_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
        pos += 2;

        if pos + name_len > data.len() {
            return None;
        }
        let name = String::from_utf8_lossy(&data[pos..pos + name_len]).to_string();
        pos += name_len;

        if pos + 4 > data.len() {
            return None;
        }
        let value_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;

        if pos + value_len > data.len() {
            return None;
        }
        let value = data[pos..pos + value_len].to_vec();
        pos += value_len;

        columns.push((name, value));
    }

    Some(columns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_encode_decode_roundtrip() {
        let record = WalRecord::new(
            42,
            WalRecordType::Insert,
            1,
            "users",
            encode_row(&[
                ("id".to_string(), 1u32.to_le_bytes().to_vec()),
                ("name".to_string(), b"Alice".to_vec()),
            ]),
        );

        let payload = record.encode_payload();
        let decoded = WalRecord::decode_payload(42, &payload).unwrap();

        assert_eq!(decoded.lsn, 42);
        assert_eq!(decoded.record_type, WalRecordType::Insert);
        assert_eq!(decoded.tx_id, 1);
        assert_eq!(decoded.table_name, "users");
        assert_eq!(decoded.data, record.data);
    }

    #[test]
    fn test_row_encode_decode_roundtrip() {
        let row = vec![
            ("id".to_string(), 42u32.to_le_bytes().to_vec()),
            ("name".to_string(), b"hello world".to_vec()),
            ("score".to_string(), 99.5f64.to_le_bytes().to_vec()),
        ];

        let encoded = encode_row(&row);
        let decoded = decode_row(&encoded).unwrap();

        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].0, "id");
        assert_eq!(decoded[0].1, 42u32.to_le_bytes());
        assert_eq!(decoded[1].0, "name");
        assert_eq!(decoded[1].1, b"hello world");
        assert_eq!(decoded[2].0, "score");
        assert_eq!(decoded[2].1, 99.5f64.to_le_bytes());
    }

    #[test]
    fn test_record_type_roundtrip() {
        for t in [
            WalRecordType::Insert,
            WalRecordType::Update,
            WalRecordType::Delete,
            WalRecordType::CreateTable,
            WalRecordType::DropTable,
            WalRecordType::Checkpoint,
            WalRecordType::Commit,
            WalRecordType::Rollback,
        ] {
            let v = t as u8;
            assert_eq!(WalRecordType::from_u8(v), Some(t));
        }
        assert!(WalRecordType::from_u8(0xFF).is_none());
    }
}
