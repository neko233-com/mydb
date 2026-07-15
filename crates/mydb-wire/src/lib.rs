use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use bytes::{Buf, BufMut, BytesMut};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

// MySQL Protocol Constants
pub const MAX_PACKET_SIZE: usize = 1 << 24 - 1;
pub const PROTOCOL_VERSION: u8 = 10;
pub const SERVER_VERSION: &str = "8.0.36-mydb";

// Capability Flags (subset for compatibility)
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

// MySQL Packet Header
#[derive(Debug, Clone)]
pub struct PacketHeader {
    pub length: u32,
    pub sequence_id: u8,
}

// MySQL Packet
#[derive(Debug, Clone)]
pub struct Packet {
    pub header: PacketHeader,
    pub payload: Vec<u8>,
}

// Client Connection State
pub struct Connection {
    stream: TcpStream,
    buffer: BytesMut,
    sequence_id: u8,
    capabilities: CapabilityFlags,
    connection_id: u32,
    user: String,
    database: Option<String>,
    state: ConnectionState,
}

#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    handshake,
    authenticated,
    ready,
}

impl Connection {
    pub fn new(stream: TcpStream, connection_id: u32) -> Self {
        Self {
            stream,
            buffer: BytesMut::with_capacity(4096),
            sequence_id: 0,
            capabilities: CapabilityFlags::empty(),
            connection_id,
            user: String::new(),
            database: None,
            state: ConnectionState::handshake,
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

            // Read more data
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
        // Send server greeting
        let greeting = self.build_greeting()?;
        self.write_packet(&greeting).await?;

        // Read client response
        let packet = self.read_packet().await?;
        self.parse_handshake_response(&packet.payload)?;

        // Send OK
        self.write_packet(&self.build_ok_packet()?).await?;

        self.state = ConnectionState::authenticated;
        Ok(())
    }

    fn build_greeting(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        // Protocol version
        buf.push(PROTOCOL_VERSION);

        // Server version
        buf.extend_from_slice(SERVER_VERSION.as_bytes());
        buf.push(0); // null terminator

        // Connection ID
        buf.put_u32_le(self.connection_id);

        // Auth plugin data (part 1) - 8 bytes
        let auth_data: Vec<u8> = (0..8).map(|_| rand::random::<u8>()).collect();
        buf.extend_from_slice(&auth_data);

        // Filler
        buf.push(0);

        // Capability flags (lower 2 bytes)
        let caps = CapabilityFlags::PROTOCOL_41
            | CapabilityFlags::LONG_PASSWORD
            | CapabilityFlags::LONG_FLAG
            | CapabilityFlags::CONNECT_WITH_DB
            | CapabilityFlags::SECURE_CONNECTION;
        buf.put_u16_le(caps.bits() as u16);

        // Character set
        buf.push(33); // utf8mb4

        // Status flags
        buf.put_u16_le(0x0002); // SERVER_STATUS_AUTOCOMMIT

        // Capability flags (upper 2 bytes)
        buf.put_u16_le((caps.bits() >> 16) as u16);

        // Length of auth plugin data
        buf.push(21);

        // Reserved (10 zero bytes)
        buf.extend_from_slice(&[0u8; 10]);

        // Auth plugin data (part 2)
        buf.extend_from_slice(&auth_data);
        buf.push(0);

        // Auth plugin name
        buf.extend_from_slice(b"mysql_native_password");
        buf.push(0);

        Ok(buf)
    }

    fn parse_handshake_response(&mut self, payload: &[u8]) -> Result<()> {
        let mut pos = 0;

        // Client capabilities (4 bytes)
        if payload.len() < 32 {
            anyhow::bail!("Invalid handshake response");
        }
        let caps_lower = u32::from_le_bytes([
            payload[0],
            payload[1],
            payload[2],
            payload[3],
        ]);
        pos += 4;

        // Max packet size (4 bytes)
        let _max_packet_size = u32::from_le_bytes([
            payload[4],
            payload[5],
            payload[6],
            payload[7],
        ]);
        pos += 4;

        // Character set (1 byte)
        let _charset = payload[8];
        pos += 1;

        // Reserved (23 bytes)
        pos += 23;

        // Username (null-terminated string)
        let username_start = pos;
        while pos < payload.len() && payload[pos] != 0 {
            pos += 1;
        }
        self.user = String::from_utf8_lossy(&payload[username_start..pos]).to_string();
        pos += 1; // Skip null terminator

        // Auth response (length-encoded)
        if pos >= payload.len() {
            anyhow::bail!("Missing auth response");
        }
        let auth_len = payload[pos] as usize;
        pos += 1;
        pos += auth_len;

        // Database (if CONNECT_WITH_DB capability)
        if self.capabilities.contains(CapabilityFlags::CONNECT_WITH_DB) {
            if pos < payload.len() {
                let db_start = pos;
                while pos < payload.len() && payload[pos] != 0 {
                    pos += 1;
                }
                self.database = Some(String::from_utf8_lossy(&payload[db_start..pos]).to_string());
            }
        }

        info!("Client connected: user={}", self.user);
        Ok(())
    }

    fn build_ok_packet(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        buf.push(0x00); // OK
        buf.put_u32_le(0); // affected rows
        buf.put_u32_le(0); // last insert id
        buf.put_u16_le(0x0002); // status flags
        buf.put_u16_le(0); // warnings
        Ok(buf)
    }

    pub async fn handle_command(&mut self, packet: &Packet) -> Result<Vec<u8>> {
        if packet.payload.is_empty() {
            anyhow::bail!("Empty command packet");
        }

        let command_type = packet.payload[0];
        let payload = &packet.payload[1..];

        match command_type {
            0x01 => {
                // COM_QUIT
                info!("Client quit");
                std::process::exit(0);
            }
            0x03 => {
                // COM_QUERY
                let query = String::from_utf8_lossy(payload).to_string();
                debug!("Query: {}", query);
                self.execute_query(&query).await
            }
            0x16 => {
                // COM_SET_OPTION
                Ok(self.build_ok_packet()?)
            }
            0x17 => {
                // COM_STMT_PREPARE
                Ok(self.build_ok_packet()?)
            }
            0x18 => {
                // COM_STMT_EXECUTE
                Ok(self.build_ok_packet()?)
            }
            0x19 => {
                // COM_STMT_CLOSE
                Ok(self.build_ok_packet()?)
            }
            _ => {
                warn!("Unsupported command: 0x{:02X}", command_type);
                Ok(self.build_ok_packet()?)
            }
        }
    }

    async fn execute_query(&self, query: &str) -> Result<Vec<u8>> {
        let upper = query.to_uppercase();

        if upper.starts_with("SELECT") || upper.starts_with("SHOW") {
            // For now, return a mock result set
            self.build_result_set(query).await
        } else if upper.starts_with("USE ") {
            Ok(self.build_ok_packet()?)
        } else if upper.starts_with("SET") {
            Ok(self.build_ok_packet()?)
        } else if upper.starts_with("CREATE") || upper.starts_with("DROP") || upper.starts_with("ALTER") {
            Ok(self.build_ok_packet()?)
        } else if upper.starts_with("INSERT") || upper.starts_with("UPDATE") || upper.starts_with("DELETE") {
            Ok(self.build_ok_packet()?)
        } else {
            // Unknown query, return OK
            Ok(self.build_ok_packet()?)
        }
    }

    async fn build_result_set(&self, query: &str) -> Result<Vec<u8>> {
        let upper = query.to_uppercase();

        let (columns, rows): (Vec<(String, u8)>, Vec<Vec<String>>) = if upper.contains("SHOW DATABASES") {
            (
                vec![("Database".to_string(), 0xFD)],
                vec![
                    vec!["information_schema".to_string()],
                    vec!["mydb".to_string()],
                ],
            )
        } else if upper.contains("SELECT VERSION") || upper.contains("SELECT @@VERSION") {
            (
                vec![("VERSION()".to_string(), 0xFD)],
                vec![vec![SERVER_VERSION.to_string()]],
            )
        } else if upper.contains("SELECT @@PORT") {
            (
                vec![("@@port".to_string(), 0xFD)],
                vec![vec!["3306".to_string()]],
            )
        } else {
            (
                vec![("result".to_string(), 0xFD)],
                vec![vec!["OK".to_string()]],
            )
        };

        let mut buf = Vec::new();

        // Column count
        buf.push(0xFE); // EOF marker for column count
        self.write_lenenc_int(&mut buf, columns.len() as u64);

        // Column definitions
        for (name, col_type) in &columns {
            // Catalog
            buf.push(3); // length
            buf.extend_from_slice(b"def");
            // Schema
            buf.push(0);
            // Table
            buf.push(0);
            // Org table
            buf.push(0);
            // Name
            buf.push(name.len() as u8);
            buf.extend_from_slice(name.as_bytes());
            // Org name
            buf.push(0);
            // Filler
            buf.put_u16_le(0x0C); // length of fixed-length fields
            // Charset
            buf.put_u16_le(33); // utf8mb4
            // Column length
            buf.put_u32_le(255);
            // Column type
            buf.push(*col_type);
            // Flags
            buf.put_u16_le(0);
            // Decimals
            buf.push(0);
        }

        // EOF packet
        buf.push(0xFE);
        buf.put_u16_le(0); // status flags
        buf.put_u16_le(0); // warnings

        // Rows
        for row in &rows {
            for field in row {
                buf.push(field.len() as u8);
                buf.extend_from_slice(field.as_bytes());
            }
        }

        // Final EOF
        buf.push(0xFE);
        buf.put_u16_le(0x0002); // SERVER_STATUS_AUTOCOMMIT
        buf.put_u16_le(0); // warnings

        Ok(buf)
    }

    fn write_lenenc_int(&self, buf: &mut Vec<u8>, value: u64) {
        if value < 251 {
            buf.push(value as u8);
        } else if value < 65536 {
            buf.push(0xFC);
            buf.put_u16_le(value as u16);
        } else if value < 16777216 {
            buf.push(0xFD);
            // Write 3 bytes manually
            buf.push((value & 0xFF) as u8);
            buf.push(((value >> 8) & 0xFF) as u8);
            buf.push(((value >> 16) & 0xFF) as u8);
        } else {
            buf.push(0xFE);
            buf.put_u64_le(value);
        }
    }
}

// Client for connecting to MyDB server
pub struct Client {
    connection: Connection,
}

impl Client {
    pub async fn connect(host: &str, port: u16, user: &str, password: &str) -> Result<Self> {
        let stream = TcpStream::connect(format!("{}:{}", host, port)).await?;

        let mut connection = Connection::new(stream, rand::random::<u32>());

        // Handle server greeting
        let packet = connection.read_packet().await?;
        debug!("Server greeting received");

        // Build and send client response
        let response = Self::build_client_response(user, password, &packet.payload)?;
        connection.write_packet(&response).await?;

        // Read OK packet
        let packet = connection.read_packet().await?;
        if packet.payload.is_empty() || packet.payload[0] != 0x00 {
            anyhow::bail!("Authentication failed");
        }

        Ok(Self { connection })
    }

    fn build_client_response(user: &str, password: &str, server_greeting: &[u8]) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        // Client capabilities
        let caps = CapabilityFlags::PROTOCOL_41
            | CapabilityFlags::LONG_PASSWORD
            | CapabilityFlags::LONG_FLAG
            | CapabilityFlags::CONNECT_WITH_DB
            | CapabilityFlags::SECURE_CONNECTION;
        buf.put_u32_le(caps.bits());

        // Max packet size
        buf.put_u32_le(MAX_PACKET_SIZE as u32);

        // Character set
        buf.push(33); // utf8mb4

        // Reserved (23 bytes)
        buf.extend_from_slice(&[0u8; 23]);

        // Username
        buf.extend_from_slice(user.as_bytes());
        buf.push(0);

        // Auth response (simple hash for now)
        let auth = Self::hash_password(password, &server_greeting[5..13]);
        buf.push(auth.len() as u8);
        buf.extend_from_slice(&auth);

        // Database (empty for now)
        buf.push(0);

        // Auth plugin name
        buf.extend_from_slice(b"mysql_native_password");
        buf.push(0);

        Ok(buf)
    }

    fn hash_password(password: &str, salt: &[u8]) -> Vec<u8> {
        use sha2::{Digest, Sha256};

        let mut sha256 = Sha256::new();
        sha256.update(password.as_bytes());
        let hash = sha256.finalize();

        let mut sha256 = Sha256::new();
        sha256.update(&hash);
        let hash = sha256.finalize();

        let mut result = Vec::new();
        for i in 0..20 {
            result.push(hash[i] ^ salt[i % 8]);
        }
        result
    }

    pub async fn execute(&mut self, query: &str) -> Result<Vec<Vec<String>>> {
        // Build COM_QUERY packet
        let mut payload = Vec::new();
        payload.push(0x03); // COM_QUERY
        payload.extend_from_slice(query.as_bytes());

        self.connection.write_packet(&payload).await?;

        // Read response
        let packet = self.connection.read_packet().await?;

        // For now, return mock results
        Ok(vec![vec!["OK".to_string()]])
    }
}

// Handle a new connection
pub async fn handle_connection(stream: TcpStream) -> Result<()> {
    let connection_id = rand::random::<u32>();
    let mut conn = Connection::new(stream, connection_id);

    // Handle handshake
    conn.handle_handshake().await?;

    // Command loop
    loop {
        match conn.read_packet().await {
            Ok(packet) => {
                let response = conn.handle_command(&packet).await?;
                conn.write_packet(&response).await?;
            }
            Err(e) => {
                error!("Error reading packet: {}", e);
                break;
            }
        }
    }

    Ok(())
}

// Add bitflags dependency
pub use bitflags;
