use std::sync::Arc;

use anyhow::Result;
use bytes::{Buf, BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use mydb_storage::{Database, Row, StorageEngineManager};

// MySQL Protocol Constants
pub const MAX_PACKET_SIZE: usize = (1 << 24) - 1;
pub const PROTOCOL_VERSION: u8 = 10;
pub const SERVER_VERSION: &str = "8.0.36-mydb";

// Capability Flags
bitflags::bitflags! {
    pub struct CapabilityFlags: u32 {
        const LONG_PASSWORD = 1;
        const FOUND_ROWS = 2;
        const LONG_FLAG = 4;
        const CONNECT_WITH_DB = 8;
        const PROTOCOL_41 = 512;
        const INTERACTIVE = 1024;
        const TRANSACTIONS = 8192;
        const SECURE_CONNECTION = 32768;
        const MULTI_STATEMENTS = (1 << 16);
        const MULTI_RESULTS = (1 << 17);
    }
}

#[derive(Debug, Clone)]
pub struct PacketHeader {
    pub length: u32,
    pub sequence_id: u8,
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub header: PacketHeader,
    pub payload: Vec<u8>,
}

pub struct Connection {
    stream: TcpStream,
    buffer: BytesMut,
    sequence_id: u8,
    capabilities: CapabilityFlags,
    connection_id: u32,
    user: String,
    database: Option<String>,
    state: ConnectionState,
    storage: Arc<StorageEngineManager>,
}

#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    Handshake,
    Authenticated,
    Ready,
}

impl Connection {
    pub fn new(stream: TcpStream, connection_id: u32, storage: Arc<StorageEngineManager>) -> Self {
        Self {
            stream,
            buffer: BytesMut::with_capacity(4096),
            sequence_id: 0,
            capabilities: CapabilityFlags::empty(),
            connection_id,
            user: String::new(),
            database: None,
            state: ConnectionState::Handshake,
            storage,
        }
    }

    pub async fn read_packet(&mut self) -> Result<Packet> {
        loop {
            if self.buffer.len() >= 4 {
                let length = u32::from_le_bytes([
                    self.buffer[0],
                    self.buffer[1],
                    self.buffer[2],
                    0,
                ]) as usize;
                let sequence_id = self.buffer[3];

                if self.buffer.len() >= 4 + length {
                    let payload = self.buffer[4..4 + length].to_vec();
                    self.buffer.advance(4 + length);

                    return Ok(Packet {
                        header: PacketHeader {
                            length: length as u32,
                            sequence_id,
                        },
                        payload,
                    });
                }
            }

            let n = self.stream.read_buf(&mut self.buffer).await?;
            if n == 0 {
                anyhow::bail!("Connection closed by peer");
            }
        }
    }

    pub async fn write_packet(&mut self, payload: &[u8]) -> Result<()> {
        self.sequence_id = self.sequence_id.wrapping_add(1);

        let length = payload.len();
        let header = [
            (length & 0xFF) as u8,
            ((length >> 8) & 0xFF) as u8,
            ((length >> 16) & 0xFF) as u8,
            self.sequence_id,
        ];

        self.stream.write_all(&header).await?;
        self.stream.write_all(payload).await?;
        self.stream.flush().await?;

        Ok(())
    }

    pub async fn handle_handshake(&mut self) -> Result<()> {
        let greeting = self.build_greeting()?;
        self.write_packet(&greeting).await?;

        let packet = self.read_packet().await?;
        self.parse_handshake_response(&packet.payload)?;

        self.write_packet(&self.build_ok_packet()?).await?;

        self.state = ConnectionState::Authenticated;
        Ok(())
    }

    fn build_greeting(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        buf.push(PROTOCOL_VERSION);
        buf.extend_from_slice(SERVER_VERSION.as_bytes());
        buf.push(0);
        buf.put_u32_le(self.connection_id);

        let auth_data: Vec<u8> = (0..8).map(|_| rand::random::<u8>()).collect();
        buf.extend_from_slice(&auth_data);
        buf.push(0);

        let caps = CapabilityFlags::PROTOCOL_41
            | CapabilityFlags::LONG_PASSWORD
            | CapabilityFlags::LONG_FLAG
            | CapabilityFlags::CONNECT_WITH_DB
            | CapabilityFlags::SECURE_CONNECTION;
        buf.put_u16_le(caps.bits() as u16);
        buf.push(33); // utf8mb4
        buf.put_u16_le(0x0002); // SERVER_STATUS_AUTOCOMMIT
        buf.put_u16_le((caps.bits() >> 16) as u16);
        buf.push(21);
        buf.extend_from_slice(&[0u8; 10]);
        buf.extend_from_slice(&auth_data);
        buf.push(0);
        buf.extend_from_slice(b"mysql_native_password");
        buf.push(0);

        Ok(buf)
    }

    fn parse_handshake_response(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 32 {
            anyhow::bail!("Invalid handshake response");
        }

        let mut pos = 0;
        pos += 4; // capabilities
        pos += 4; // max packet size
        pos += 1; // charset
        pos += 23; // reserved

        let username_start = pos;
        while pos < payload.len() && payload[pos] != 0 {
            pos += 1;
        }
        self.user = String::from_utf8_lossy(&payload[username_start..pos]).to_string();
        pos += 1;

        if pos >= payload.len() {
            anyhow::bail!("Missing auth response");
        }
        let auth_len = payload[pos] as usize;
        pos += 1;
        pos += auth_len;

        if self.capabilities.contains(CapabilityFlags::CONNECT_WITH_DB) && pos < payload.len() {
            let db_start = pos;
            while pos < payload.len() && payload[pos] != 0 {
                pos += 1;
            }
            self.database = Some(String::from_utf8_lossy(&payload[db_start..pos]).to_string());
        }

        info!("Client connected: user={}", self.user);
        Ok(())
    }

    pub async fn handle_command(&mut self, packet: &Packet) -> Result<Vec<u8>> {
        if packet.payload.is_empty() {
            anyhow::bail!("Empty command packet");
        }

        let command_type = packet.payload[0];
        let payload = &packet.payload[1..];

        match command_type {
            0x01 => {
                info!("Client quit");
                // Don't exit, just return empty to break the loop
                Ok(Vec::new())
            }
            0x03 => {
                let query = String::from_utf8_lossy(payload).to_string();
                debug!("Query: {}", query);
                self.execute_query(&query).await
            }
            0x16 | 0x17 | 0x18 | 0x19 => Ok(self.build_ok_packet()?),
            _ => {
                warn!("Unsupported command: 0x{:02X}", command_type);
                Ok(self.build_ok_packet()?)
            }
        }
    }

    async fn execute_query(&self, query: &str) -> Result<Vec<u8>> {
        let upper = query.trim().to_uppercase();

        // USE database
        if upper.starts_with("USE ") {
            let db_name = query.trim()[4..].trim().trim_matches('`').to_string();
            if self.storage.get_database(&db_name).is_some() {
                return Ok(self.build_ok_packet()?);
            } else {
                return self.build_err_packet(&format!("Unknown database: {}", db_name));
            }
        }

        // SELECT VERSION
        if upper.contains("SELECT VERSION") || upper.contains("SELECT @@VERSION") {
            return self.build_result_set(&[("VERSION()".to_string(), vec![SERVER_VERSION.to_string()])]);
        }

        // SELECT @@PORT
        if upper.contains("SELECT @@PORT") {
            return self.build_result_set(&[("@@port".to_string(), vec!["3306".to_string()])]);
        }

        // SHOW DATABASES
        if upper == "SHOW DATABASES" || upper == "SHOW DATABASES;" {
            let mut rows: Vec<Vec<String>> = self
                .storage
                .list_databases()
                .iter()
                .map(|d| vec![d.clone()])
                .collect();
            // Always include information_schema
            if !rows.iter().any(|r| r[0] == "information_schema") {
                rows.insert(0, vec!["information_schema".to_string()]);
            }
            return self.build_result_set_from_rows(&[("Database".to_string(), 0xFD)], &rows);
        }

        // SHOW TABLES
        if upper == "SHOW TABLES" || upper == "SHOW TABLES;" {
            let db_name = self.database.as_deref().unwrap_or("mydb");
            if let Some(db) = self.storage.get_database(db_name) {
                let tables = db.list_tables();
                let rows: Vec<Vec<String>> = tables.iter().map(|t| vec![t.clone()]).collect();
                return self.build_result_set_from_rows(&[("Tables_in_".to_string(), 0xFD)], &rows);
            } else {
                return self.build_result_set_from_rows(&[("Tables_in_mydb".to_string(), 0xFD)], &[]);
            }
        }

        // CREATE DATABASE
        if upper.starts_with("CREATE DATABASE") || upper.starts_with("CREATE SCHEMA") {
            let name = self.extract_name_after_keyword(&upper, "DATABASE")
                .or_else(|| self.extract_name_after_keyword(&upper, "SCHEMA"));
            if let Some(name) = name {
                let name = name.trim_matches('`').to_string();
                self.storage.create_database(&name).await?;
                return Ok(self.build_ok_packet()?);
            }
        }

        // CREATE TABLE
        if upper.starts_with("CREATE TABLE") {
            if let Some((table_name, columns)) = self.parse_create_table(query) {
                let db_name = self.database.as_deref().unwrap_or("mydb");
                if let Some(db) = self.storage.get_database(db_name) {
                    let schema = mydb_storage::TableSchema {
                        name: table_name,
                        columns,
                        primary_key: None,
                        indexes: vec![],
                        next_page_number: 0,
                    };
                    db.create_table(schema)?;
                    return Ok(self.build_ok_packet()?);
                }
            }
        }

        // INSERT INTO
        if upper.starts_with("INSERT") {
            if let Some((table_name, row)) = self.parse_insert(query) {
                let db_name = self.database.as_deref().unwrap_or("mydb");
                if let Some(db) = self.storage.get_database(db_name) {
                    db.insert_row(&table_name, row)?;
                    return Ok(self.build_ok_packet()?);
                }
            }
        }

        // SELECT * FROM
        if upper.starts_with("SELECT") && upper.contains(" FROM ") {
            if let Some(table_name) = self.extract_table_from_select(&upper) {
                let db_name = self.database.as_deref().unwrap_or("mydb");
                if let Some(db) = self.storage.get_database(db_name) {
                    let rows = db.scan_table(&table_name)?;
                    if rows.is_empty() {
                        return self.build_empty_result_set();
                    }

                    // Build column headers from first row
                    if let Some(first_row) = rows.first() {
                        let columns: Vec<(String, u8)> = first_row
                            .values
                            .iter()
                            .map(|(name, _)| (name.clone(), 0xFD))
                            .collect();

                        let row_strings: Vec<Vec<String>> = rows
                            .iter()
                            .map(|r| {
                                r.values
                                    .iter()
                                    .map(|(_, v)| String::from_utf8_lossy(v).to_string())
                                    .collect()
                            })
                            .collect();

                        return self.build_result_set_from_rows_slice(&columns, &row_strings);
                    }
                }
            }
        }

        // DROP TABLE
        if upper.starts_with("DROP TABLE") {
            if let Some(table_name) = self.extract_name_after_keyword(&upper, "TABLE") {
                let table_name = table_name.trim_matches('`').to_string();
                let db_name = self.database.as_deref().unwrap_or("mydb");
                if let Some(db) = self.storage.get_database(db_name) {
                    db.drop_table(&table_name)?;
                    return Ok(self.build_ok_packet()?);
                }
            }
        }

        // DROP DATABASE
        if upper.starts_with("DROP DATABASE") || upper.starts_with("DROP SCHEMA") {
            if let Some(name) = self.extract_name_after_keyword(&upper, "DATABASE")
                .or_else(|| self.extract_name_after_keyword(&upper, "SCHEMA"))
            {
                let name = name.trim_matches('`').to_string();
                self.storage.drop_database(&name).await?;
                return Ok(self.build_ok_packet()?);
            }
        }

        // DELETE FROM
        if upper.starts_with("DELETE") {
            if let Some(table_name) = self.extract_name_after_keyword(&upper, "FROM") {
                let table_name = table_name.trim_matches('`').to_string();
                let db_name = self.database.as_deref().unwrap_or("mydb");
                if let Some(db) = self.storage.get_database(db_name) {
                    let count = db.delete_all_rows(&table_name)?;
                    return self.build_ok_with_info(count, 0);
                }
            }
        }

        // SET / BEGIN / COMMIT / ROLLBACK - just return OK
        if upper.starts_with("SET")
            || upper.starts_with("BEGIN")
            || upper.starts_with("COMMIT")
            || upper.starts_with("ROLLBACK")
            || upper.starts_with("START")
        {
            return Ok(self.build_ok_packet()?);
        }

        // Default: return OK
        Ok(self.build_ok_packet()?)
    }

    // -----------------------------------------------------------------------
    // SQL Parsing Helpers (simple, not a full parser)
    // -----------------------------------------------------------------------

    fn extract_name_after_keyword(&self, upper: &str, keyword: &str) -> Option<String> {
        let keyword_upper = keyword.to_uppercase();
        if let Some(pos) = upper.find(&keyword_upper) {
            let after = &upper[pos + keyword_upper.len()..].trim();
            // Take the next word (until space, semicolon, or end)
            let end = after
                .find(|c: char| c.is_whitespace() || c == ';' || c == '(')
                .unwrap_or(after.len());
            let name = after[..end].trim().trim_matches('`').to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
        None
    }

    fn parse_create_table(&self, query: &str) -> Option<(String, Vec<mydb_storage::Column>)> {
        let upper = query.to_uppercase();
        let table_name = self.extract_name_after_keyword(&upper, "TABLE")?;

        // Extract column definitions between parentheses
        let open_paren = query.find('(')?;
        let close_paren = query.rfind(')')?;
        let columns_str = &query[open_paren + 1..close_paren];

        let mut columns = Vec::new();
        for part in columns_str.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            // Skip constraints (PRIMARY KEY, FOREIGN KEY, etc.)
            let part_upper = part.to_uppercase();
            if part_upper.starts_with("PRIMARY")
                || part_upper.starts_with("FOREIGN")
                || part_upper.starts_with("INDEX")
                || part_upper.starts_with("KEY")
                || part_upper.starts_with("UNIQUE")
                || part_upper.starts_with("CONSTRAINT")
            {
                continue;
            }

            let tokens: Vec<&str> = part.split_whitespace().collect();
            if tokens.len() >= 2 {
                let col_name = tokens[0].trim_matches('`').to_string();
                let data_type = match tokens[1].to_uppercase().as_str() {
                    "INT" | "INTEGER" => mydb_storage::DataType::Int,
                    "BIGINT" => mydb_storage::DataType::BigInt,
                    "FLOAT" => mydb_storage::DataType::Float,
                    "DOUBLE" => mydb_storage::DataType::Double,
                    "TEXT" => mydb_storage::DataType::Text,
                    "BLOB" => mydb_storage::DataType::Blob,
                    "DATE" => mydb_storage::DataType::Date,
                    "DATETIME" => mydb_storage::DataType::DateTime,
                    "TIMESTAMP" => mydb_storage::DataType::Timestamp,
                    "BOOLEAN" | "BOOL" => mydb_storage::DataType::Boolean,
                    s if s.starts_with("VARCHAR") => {
                        let len = s
                            .trim_start_matches("VARCHAR")
                            .trim_start_matches('(')
                            .trim_end_matches(')')
                            .parse::<u32>()
                            .unwrap_or(255);
                        mydb_storage::DataType::Varchar(len)
                    }
                    _ => mydb_storage::DataType::Varchar(255),
                };
                let nullable = !part_upper.contains("NOT NULL");
                let is_pk = part_upper.contains("PRIMARY KEY") || part_upper.contains("AUTO_INCREMENT");

                columns.push(mydb_storage::Column {
                    name: col_name,
                    data_type,
                    nullable,
                    default: None,
                    is_primary_key: is_pk,
                });
            }
        }

        Some((table_name, columns))
    }

    fn parse_insert(&self, query: &str) -> Option<(String, Row)> {
        let upper = query.to_uppercase();
        let table_name = self.extract_name_after_keyword(&upper, "INTO")?;

        // Extract values after VALUES keyword
        let values_pos = upper.find("VALUES")?;
        let values_str = &query[values_pos + 6..].trim();

        // Parse column list if present
        let (column_names, values_str) = if let Some(open) = query.find('(') {
            if let Some(close) = query[open..].find(')') {
                let cols_str = &query[open + 1..open + close];
                let cols: Vec<String> = cols_str
                    .split(',')
                    .map(|c| c.trim().trim_matches('`').to_string())
                    .collect();
                (cols, values_str)
            } else {
                (Vec::new(), values_str)
            }
        } else {
            (Vec::new(), values_str)
        };

        // Extract values between parentheses
        let values_start = values_str.find('(')?;
        let values_end = values_str.rfind(')')?;
        let values_inner = &values_str[values_start + 1..values_end];

        let values: Vec<String> = self.split_sql_values(values_inner);

        let mut row = Row::new();
        for (i, val) in values.iter().enumerate() {
            let name = if i < column_names.len() {
                column_names[i].clone()
            } else {
                format!("col_{}", i)
            };

            let val = val.trim().trim_matches('\'').trim_matches('"');
            // Try to interpret as number
            if let Ok(n) = val.parse::<i64>() {
                row.push(&name, n.to_le_bytes().to_vec());
            } else if let Ok(f) = val.parse::<f64>() {
                row.push(&name, f.to_le_bytes().to_vec());
            } else {
                row.push(&name, val.as_bytes().to_vec());
            }
        }

        Some((table_name, row))
    }

    fn split_sql_values(&self, s: &str) -> Vec<String> {
        let mut values = Vec::new();
        let mut current = String::new();
        let mut in_string = false;

        for ch in s.chars() {
            match ch {
                '\'' if !in_string => {
                    in_string = true;
                    current.push(ch);
                }
                '\'' if in_string => {
                    in_string = false;
                    current.push(ch);
                }
                ',' if !in_string => {
                    values.push(current.trim().to_string());
                    current = String::new();
                }
                _ => current.push(ch),
            }
        }
        if !current.trim().is_empty() {
            values.push(current.trim().to_string());
        }

        values
    }

    fn extract_table_from_select(&self, upper: &str) -> Option<String> {
        // Simple: find FROM and take the next word
        if let Some(from_pos) = upper.find(" FROM ") {
            let after_from = &upper[from_pos + 6..].trim();
            let end = after_from
                .find(|c: char| c.is_whitespace() || c == ';' || c == ')')
                .unwrap_or(after_from.len());
            let name = after_from[..end].trim().trim_matches('`').to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
        None
    }

    // -----------------------------------------------------------------------
    // Response Building
    // -----------------------------------------------------------------------

    fn build_ok_packet(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        buf.push(0x00); // OK
        buf.put_u32_le(0); // affected rows
        buf.put_u32_le(0); // last insert id
        buf.put_u16_le(0x0002); // status flags
        buf.put_u16_le(0); // warnings
        Ok(buf)
    }

    fn build_ok_with_info(&self, affected_rows: u64, last_insert_id: u64) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        buf.push(0x00); // OK
        self.write_lenenc_int(&mut buf, affected_rows);
        self.write_lenenc_int(&mut buf, last_insert_id);
        buf.put_u16_le(0x0002); // status flags
        buf.put_u16_le(0); // warnings
        Ok(buf)
    }

    fn build_err_packet(&self, message: &str) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        buf.push(0xFF); // ERR
        buf.put_u16_le(1045); // error code
        buf.push(b'#');
        buf.extend_from_slice(b"28000"); // sql state
        buf.extend_from_slice(message.as_bytes());
        Ok(buf)
    }

    fn build_result_set(
        &self,
        columns: &[(String, Vec<String>)],
    ) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        // Column count
        buf.push(0xFE);
        self.write_lenenc_int(&mut buf, columns.len() as u64);

        // Column definitions
        for (name, _) in columns {
            self.write_column_def(&mut buf, name);
        }

        // EOF
        buf.push(0xFE);
        buf.put_u16_le(0);
        buf.put_u16_le(0);

        // Rows
        for (_, values) in columns {
            for val in values {
                buf.push(val.len() as u8);
                buf.extend_from_slice(val.as_bytes());
            }
        }

        // Final EOF
        buf.push(0xFE);
        buf.put_u16_le(0x0002);
        buf.put_u16_le(0);

        Ok(buf)
    }

    fn build_result_set_from_rows(
        &self,
        columns: &[(String, u8)],
        rows: &[Vec<String>],
    ) -> Result<Vec<u8>> {
        self.build_result_set_from_rows_slice(columns, rows)
    }

    fn build_result_set_from_rows_slice(
        &self,
        columns: &[(String, u8)],
        rows: &[Vec<String>],
    ) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        // Column count
        buf.push(0xFE);
        self.write_lenenc_int(&mut buf, columns.len() as u64);

        // Column definitions
        for (name, col_type) in columns {
            self.write_column_def_typed(&mut buf, name, *col_type);
        }

        // EOF
        buf.push(0xFE);
        buf.put_u16_le(0);
        buf.put_u16_le(0);

        // Rows
        for row in rows {
            for val in row {
                let bytes = val.as_bytes();
                if bytes.is_empty() {
                    buf.push(0xFB); // NULL
                } else {
                    buf.push(bytes.len() as u8);
                    buf.extend_from_slice(bytes);
                }
            }
        }

        // Final EOF
        buf.push(0xFE);
        buf.put_u16_le(0x0002);
        buf.put_u16_le(0);

        Ok(buf)
    }

    fn build_empty_result_set(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        // Column count
        buf.push(0xFE);
        buf.push(0); // 0 columns

        // EOF
        buf.push(0xFE);
        buf.put_u16_le(0x0002);
        buf.put_u16_le(0);

        Ok(buf)
    }

    fn write_column_def(&self, buf: &mut Vec<u8>, name: &str) {
        self.write_column_def_typed(buf, name, 0xFD);
    }

    fn write_column_def_typed(&self, buf: &mut Vec<u8>, name: &str, col_type: u8) {
        buf.push(3); // catalog len
        buf.extend_from_slice(b"def");
        buf.push(0); // schema
        buf.push(0); // table
        buf.push(0); // org_table
        buf.push(name.len() as u8);
        buf.extend_from_slice(name.as_bytes());
        buf.push(0); // org_name
        buf.put_u16_le(0x0C);
        buf.put_u16_le(33); // charset utf8mb4
        buf.put_u32_le(255);
        buf.push(col_type);
        buf.put_u16_le(0);
        buf.push(0);
    }

    fn write_lenenc_int(&self, buf: &mut Vec<u8>, value: u64) {
        if value < 251 {
            buf.push(value as u8);
        } else if value < 65536 {
            buf.push(0xFC);
            buf.put_u16_le(value as u16);
        } else if value < 16777216 {
            buf.push(0xFD);
            buf.push((value & 0xFF) as u8);
            buf.push(((value >> 8) & 0xFF) as u8);
            buf.push(((value >> 16) & 0xFF) as u8);
        } else {
            buf.push(0xFE);
            buf.put_u64_le(value);
        }
    }
}

// ============================================================================
// Handle connection
// ============================================================================

pub async fn handle_connection(
    stream: TcpStream,
    storage: Arc<StorageEngineManager>,
) -> Result<()> {
    let connection_id = rand::random::<u32>();
    let mut conn = Connection::new(stream, connection_id, storage);

    conn.handle_handshake().await?;

    loop {
        match conn.read_packet().await {
            Ok(packet) => {
                if packet.payload.is_empty() {
                    break;
                }
                let response = conn.handle_command(&packet).await?;
                if response.is_empty() {
                    break; // QUIT command
                }
                conn.write_packet(&response).await?;
            }
            Err(e) => {
                debug!("Connection closed: {}", e);
                break;
            }
        }
    }

    Ok(())
}

pub use bitflags;
