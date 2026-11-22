/// Neutral change-data-capture record shared by the storage decoder and the
/// CDC reader. It deliberately avoids storage-internal types so the CDC crate
/// depends only on `mydb-wal` for the event model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalChange {
    pub operation: WalChangeOp,
    pub database: String,
    pub table: String,
    pub before: Vec<(String, Vec<u8>)>,
    pub after: Vec<(String, Vec<u8>)>,
    /// Set for DDL changes; consumers decide whether to persist it.
    pub ddl_sql: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalChangeOp {
    Insert,
    Upsert,
    Update,
    Delete,
    Ddl,
}

impl WalChangeOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Upsert => "upsert",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Ddl => "ddl",
        }
    }
}
