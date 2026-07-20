use anyhow::{bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use clap::{Args, Parser, Subcommand};
use mysql::{prelude::Queryable, Conn, Opts, OptsBuilder, Value};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::Instant,
};

const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Parser)]
#[command(
    name = "mydbdump",
    about = "Lock-free consistent MySQL 8/MyDB backup, incremental restore, and verification"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a full or table-checksum incremental backup.
    Backup(BackupArgs),
    /// Restore one backup. Apply the full backup first, then incrementals in order.
    Restore(RestoreArgs),
    /// Verify manifest and compressed file checksums without a database connection.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
struct ConnectionArgs {
    /// mysql:// URL. Falls back to MYDBDUMP_URL.
    #[arg(long)]
    url: Option<String>,
    #[arg(short = 'h', long, default_value = "127.0.0.1")]
    host: String,
    #[arg(short = 'P', long, default_value_t = 3306)]
    port: u16,
    #[arg(short = 'u', long, default_value = "root")]
    user: String,
    /// Password. Falls back to MYDBDUMP_PASSWORD.
    #[arg(short = 'p', long)]
    password: Option<String>,
    #[arg(long)]
    database: String,
}

#[derive(Debug, Args)]
#[command(disable_help_flag = true)]
struct BackupArgs {
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: Option<bool>,
    #[command(flatten)]
    connection: ConnectionArgs,
    #[arg(long)]
    output: PathBuf,
    /// Previous manifest. Only changed tables are emitted.
    #[arg(long)]
    incremental_from: Option<PathBuf>,
    /// Comma-separated table allowlist.
    #[arg(long, value_delimiter = ',')]
    tables: Vec<String>,
    /// Rows per multi-value INSERT.
    #[arg(long, default_value_t = 1000)]
    batch_size: usize,
    /// Zstd compression level.
    #[arg(long, default_value_t = 3)]
    compression_level: i32,
    /// Permit non-InnoDB source tables. They are not snapshot-consistent without locks.
    #[arg(long)]
    allow_non_transactional: bool,
}

#[derive(Debug, Args)]
#[command(disable_help_flag = true)]
struct RestoreArgs {
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: Option<bool>,
    #[command(flatten)]
    connection: ConnectionArgs,
    #[arg(long)]
    input: PathBuf,
    /// Drop and recreate changed tables. Required for incremental table replacement.
    #[arg(long, default_value_t = true)]
    replace_tables: bool,
    #[arg(long, default_value_t = true)]
    verify: bool,
    /// Skip verification that the target matches the incremental parent snapshot.
    #[arg(long)]
    skip_chain_check: bool,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    #[arg(long)]
    input: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BackupKind {
    Full,
    Incremental,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TableAction {
    Replace,
    Unchanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    tool: String,
    kind: BackupKind,
    database: String,
    created_at: String,
    duration_ms: u128,
    consistent_snapshot: bool,
    table_locks: bool,
    incremental_strategy: String,
    parent_manifest: Option<String>,
    #[serde(default)]
    parent_state: Vec<TableState>,
    tables: Vec<TableEntry>,
    dropped_tables: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TableState {
    name: String,
    rows: u64,
    content_checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TableEntry {
    name: String,
    ddl: String,
    ddl_checksum: String,
    rows: u64,
    content_checksum: String,
    action: TableAction,
    data_file: Option<String>,
    data_file_checksum: Option<String>,
    uncompressed_bytes: u64,
    compressed_bytes: u64,
}

#[derive(Debug, Clone)]
struct SourceTable {
    name: String,
    engine: String,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Backup(args) => backup(args),
        Command::Restore(args) => restore(args),
        Command::Verify(args) => {
            let manifest = verify_backup(&args.input)?;
            println!(
                "verified {} {:?} backup: {} tables",
                manifest.database,
                manifest.kind,
                manifest.tables.len()
            );
            Ok(())
        }
    }
}

fn backup(args: BackupArgs) -> Result<()> {
    if args.batch_size == 0 {
        bail!("--batch-size must be greater than zero");
    }
    prepare_output(&args.output)?;
    let base = args
        .incremental_from
        .as_ref()
        .map(|path| read_manifest(path))
        .transpose()?;
    if let Some(base) = &base {
        if base.database != args.connection.database {
            bail!("incremental base belongs to database `{}`", base.database);
        }
    }

    let started = Instant::now();
    let mut source = connect(&args.connection, "source")?;
    source.query_drop(format!("USE {}", quote_ident(&args.connection.database)))?;
    let is_mydb = source
        .query_first::<String, _>("SELECT VERSION()")?
        .is_some_and(|version| version.contains("mydb"));
    let available = list_tables(&mut source, &args.connection.database, is_mydb)?;
    let tables = choose_tables(available, &args.tables)?;
    if !args.allow_non_transactional {
        let unsupported = tables
            .iter()
            .filter(|table| !table.engine.eq_ignore_ascii_case("InnoDB"))
            .map(|table| format!("{} ({})", table.name, table.engine))
            .collect::<Vec<_>>();
        if !unsupported.is_empty() {
            bail!(
                "lock-free consistency requires transactional tables; unsupported: {}. Use --allow-non-transactional only if inconsistency is acceptable",
                unsupported.join(", ")
            );
        }
    }

    source.query_drop("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ")?;
    source.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")?;
    let base_tables = base
        .as_ref()
        .map(|manifest| {
            manifest
                .tables
                .iter()
                .map(|table| (table.name.clone(), table.clone()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut entries = Vec::with_capacity(tables.len());
    let dump_result = (|| -> Result<()> {
        for table in &tables {
            let prior = base_tables.get(&table.name);
            entries.push(dump_table(
                &mut source,
                table,
                prior,
                &args.output,
                args.batch_size,
                args.compression_level,
            )?);
        }
        Ok(())
    })();
    let _ = source.query_drop(if dump_result.is_ok() {
        "COMMIT"
    } else {
        "ROLLBACK"
    });
    dump_result?;

    let current_names = entries
        .iter()
        .map(|table| table.name.as_str())
        .collect::<HashSet<_>>();
    let mut dropped_tables = base_tables
        .keys()
        .filter(|table| !current_names.contains(table.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    dropped_tables.sort();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let kind = if base.is_some() {
        BackupKind::Incremental
    } else {
        BackupKind::Full
    };
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        tool: format!("mydbdump {}", env!("CARGO_PKG_VERSION")),
        kind,
        database: args.connection.database,
        created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        duration_ms: started.elapsed().as_millis(),
        consistent_snapshot: !args.allow_non_transactional,
        table_locks: false,
        incremental_strategy: "table_content_sha256".to_string(),
        parent_manifest: args
            .incremental_from
            .as_ref()
            .map(|path| path.display().to_string()),
        parent_state: base
            .as_ref()
            .map(|manifest| {
                manifest
                    .tables
                    .iter()
                    .map(|table| TableState {
                        name: table.name.clone(),
                        rows: table.rows,
                        content_checksum: table.content_checksum.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        tables: entries,
        dropped_tables,
    };
    write_manifest(&args.output.join("manifest.json"), &manifest)?;
    println!(
        "{} backup complete: {} tables, {} changed, {} dropped, {} ms",
        match manifest.kind {
            BackupKind::Full => "full",
            BackupKind::Incremental => "incremental",
        },
        manifest.tables.len(),
        manifest
            .tables
            .iter()
            .filter(|table| table.action == TableAction::Replace)
            .count(),
        manifest.dropped_tables.len(),
        manifest.duration_ms
    );
    Ok(())
}

fn dump_table(
    source: &mut Conn,
    table: &SourceTable,
    prior: Option<&TableEntry>,
    output: &Path,
    batch_size: usize,
    compression_level: i32,
) -> Result<TableEntry> {
    let ddl = show_create_table(source, &table.name)?;
    let ddl_checksum = sha256_hex(ddl.as_bytes());
    let columns = table_columns(source, &table.name)?;
    let file_name = format!("table-{}.sql.zst", hex_name(&table.name));
    let path = output.join(&file_name);
    let file = fs::File::create(&path)?;
    let writer = BufWriter::new(file);
    let mut encoder = zstd::stream::write::Encoder::new(writer, compression_level)?;
    encoder.include_checksum(true)?;
    let mut output = CountingWriter::new(encoder);
    let column_list = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(",");
    let mut hashes = Vec::new();
    let mut tuples = Vec::with_capacity(batch_size);
    let mut result = source.query_iter(format!("SELECT * FROM {}", quote_ident(&table.name)))?;
    for row in result.by_ref() {
        let values = row?.unwrap();
        hashes.push(hash_row(&values)?);
        tuples.push(format_tuple(&values)?);
        if tuples.len() == batch_size {
            write_insert(&mut output, &table.name, &column_list, &tuples)?;
            tuples.clear();
        }
    }
    drop(result);
    if !tuples.is_empty() {
        write_insert(&mut output, &table.name, &column_list, &tuples)?;
    }
    let rows = hashes.len() as u64;
    let content_checksum = hash_table(hashes);
    let uncompressed_bytes = output.bytes;
    let encoder = output.into_inner();
    encoder.finish()?.flush()?;
    let compressed_bytes = fs::metadata(&path)?.len();
    let unchanged = prior.is_some_and(|prior| {
        prior.ddl_checksum == ddl_checksum
            && prior.content_checksum == content_checksum
            && prior.rows == rows
    });
    let (action, data_file, data_file_checksum, compressed_bytes) = if unchanged {
        fs::remove_file(&path)?;
        (TableAction::Unchanged, None, None, 0)
    } else {
        let checksum = file_sha256(&path)?;
        (
            TableAction::Replace,
            Some(file_name),
            Some(checksum),
            compressed_bytes,
        )
    };
    Ok(TableEntry {
        name: table.name.clone(),
        ddl,
        ddl_checksum,
        rows,
        content_checksum,
        action,
        data_file,
        data_file_checksum,
        uncompressed_bytes: if unchanged { 0 } else { uncompressed_bytes },
        compressed_bytes,
    })
}

fn restore(args: RestoreArgs) -> Result<()> {
    let manifest = verify_backup(&args.input)?;
    if manifest.database != args.connection.database {
        bail!(
            "backup database `{}` does not match target `{}`",
            manifest.database,
            args.connection.database
        );
    }
    let mut target = connect(&args.connection, "target")?;
    ensure_database(&mut target, &manifest.database)?;
    target.query_drop(format!("USE {}", quote_ident(&manifest.database)))?;
    if manifest.kind == BackupKind::Incremental && !args.skip_chain_check {
        verify_parent_state(&mut target, &manifest.parent_state)?;
    }
    for table in &manifest.dropped_tables {
        drop_table_if_exists(&mut target, table)?;
    }
    for table in &manifest.tables {
        if table.action == TableAction::Unchanged {
            continue;
        }
        if !args.replace_tables {
            bail!("table replacement disabled but `{}` changed", table.name);
        }
        drop_table_if_exists(&mut target, &table.name)?;
        target
            .query_drop(&table.ddl)
            .with_context(|| format!("create table `{}`", table.name))?;
        if let Some(file_name) = &table.data_file {
            restore_data_file(&mut target, &args.input.join(file_name), &table.name)?;
        }
        if args.verify {
            let (rows, checksum) = checksum_target_table(&mut target, &table.name)?;
            if rows != table.rows || checksum != table.content_checksum {
                bail!(
                    "restored table `{}` failed content verification",
                    table.name
                );
            }
        }
    }
    println!(
        "restored {:?} backup for `{}`",
        manifest.kind, manifest.database
    );
    Ok(())
}

fn verify_parent_state(target: &mut Conn, expected: &[TableState]) -> Result<()> {
    if expected.is_empty() {
        bail!("incremental backup has no parent state; use --skip-chain-check only after manual verification");
    }
    let tables: HashSet<String> = target.query("SHOW TABLES")?.into_iter().collect();
    for table in expected {
        if !tables.contains(&table.name) {
            bail!(
                "incremental base mismatch: table `{}` is missing",
                table.name
            );
        }
        let (rows, checksum) = checksum_target_table(target, &table.name)?;
        if rows != table.rows || checksum != table.content_checksum {
            bail!(
                "incremental base mismatch for `{}`: expected {} rows and checksum {}",
                table.name,
                table.rows,
                table.content_checksum
            );
        }
    }
    Ok(())
}

fn restore_data_file(target: &mut Conn, path: &Path, table: &str) -> Result<()> {
    let file = fs::File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(BufReader::new(file))?;
    let reader = BufReader::new(decoder);
    target.query_drop("START TRANSACTION")?;
    for line in reader.lines() {
        let sql = line?;
        if sql.trim().is_empty() {
            continue;
        }
        if let Err(error) = target.query_drop(&sql) {
            let _ = target.query_drop("ROLLBACK");
            return Err(error).with_context(|| format!("restore table `{table}`"));
        }
    }
    if let Err(error) = target.query_drop("COMMIT") {
        let _ = target.query_drop("ROLLBACK");
        return Err(error).with_context(|| format!("commit table `{table}`"));
    }
    Ok(())
}

fn verify_backup(input: &Path) -> Result<Manifest> {
    let manifest = read_manifest(&input.join("manifest.json"))?;
    if manifest.format_version != FORMAT_VERSION {
        bail!("unsupported backup format {}", manifest.format_version);
    }
    for table in &manifest.tables {
        match (&table.data_file, &table.data_file_checksum) {
            (Some(file), Some(expected)) => {
                let path = input.join(file);
                let actual = file_sha256(&path)
                    .with_context(|| format!("verify data file for `{}`", table.name))?;
                if &actual != expected {
                    bail!("data file checksum mismatch for `{}`", table.name);
                }
                let compressed = fs::File::open(&path)?;
                let mut decoder = zstd::stream::read::Decoder::new(BufReader::new(compressed))?;
                let decoded_bytes = std::io::copy(&mut decoder, &mut std::io::sink())?;
                if decoded_bytes != table.uncompressed_bytes {
                    bail!("uncompressed size mismatch for `{}`", table.name);
                }
            }
            (None, None) if table.action == TableAction::Unchanged => {}
            _ => bail!("invalid manifest data file state for `{}`", table.name),
        }
    }
    Ok(manifest)
}

fn prepare_output(output: &Path) -> Result<()> {
    if output.exists() && fs::read_dir(output)?.next().is_some() {
        bail!("output directory `{}` is not empty", output.display());
    }
    fs::create_dir_all(output)?;
    Ok(())
}

fn connect(args: &ConnectionArgs, label: &str) -> Result<Conn> {
    let url = args.url.clone().or_else(|| env::var("MYDBDUMP_URL").ok());
    let opts = if let Some(url) = url {
        Opts::from_url(&url).with_context(|| format!("invalid {label} URL"))?
    } else {
        OptsBuilder::new()
            .ip_or_hostname(Some(args.host.clone()))
            .tcp_port(args.port)
            .user(Some(args.user.clone()))
            .pass(
                args.password
                    .clone()
                    .or_else(|| env::var("MYDBDUMP_PASSWORD").ok()),
            )
            .into()
    };
    Conn::new(opts).with_context(|| format!("connect to {label}"))
}

fn list_tables(source: &mut Conn, database: &str, is_mydb: bool) -> Result<Vec<SourceTable>> {
    if is_mydb {
        let names: Vec<String> = source.query("SHOW TABLES")?;
        return Ok(names
            .into_iter()
            .map(|name| SourceTable {
                name,
                engine: "InnoDB".to_string(),
            })
            .collect());
    }
    let sql = format!(
        "SELECT TABLE_NAME, COALESCE(ENGINE,'UNKNOWN') FROM INFORMATION_SCHEMA.TABLES \
         WHERE TABLE_SCHEMA={} AND TABLE_TYPE='BASE TABLE' ORDER BY TABLE_NAME",
        quote_bytes(database.as_bytes())
    );
    let rows: Vec<(String, String)> = source.query(sql).context("list source tables")?;
    Ok(rows
        .into_iter()
        .map(|(name, engine)| SourceTable { name, engine })
        .collect())
}

fn choose_tables(available: Vec<SourceTable>, requested: &[String]) -> Result<Vec<SourceTable>> {
    if requested.is_empty() {
        return Ok(available);
    }
    let by_name = available
        .into_iter()
        .map(|table| (table.name.clone(), table))
        .collect::<HashMap<_, _>>();
    requested
        .iter()
        .map(|name| {
            by_name
                .get(name)
                .cloned()
                .with_context(|| format!("source table `{name}` does not exist"))
        })
        .collect()
}

fn show_create_table(source: &mut Conn, table: &str) -> Result<String> {
    source
        .query_first::<(String, String), _>(format!("SHOW CREATE TABLE {}", quote_ident(table)))?
        .map(|(_, ddl)| ddl)
        .with_context(|| format!("missing schema for `{table}`"))
}

fn table_columns(source: &mut Conn, table: &str) -> Result<Vec<String>> {
    let rows: Vec<mysql::Row> =
        source.query(format!("SHOW COLUMNS FROM {}", quote_ident(table)))?;
    rows.into_iter()
        .map(|row| {
            row.get_opt::<String, _>(0)
                .transpose()?
                .context("SHOW COLUMNS returned no field name")
        })
        .collect()
}

fn ensure_database(target: &mut Conn, database: &str) -> Result<()> {
    let databases: Vec<String> = target.query("SHOW DATABASES")?;
    if !databases.iter().any(|existing| existing == database) {
        target.query_drop(format!("CREATE DATABASE {}", quote_ident(database)))?;
    }
    Ok(())
}

fn drop_table_if_exists(target: &mut Conn, table: &str) -> Result<()> {
    let tables: Vec<String> = target.query("SHOW TABLES")?;
    if tables.iter().any(|existing| existing == table) {
        target.query_drop(format!("DROP TABLE {}", quote_ident(table)))?;
    }
    Ok(())
}

fn checksum_target_table(target: &mut Conn, table: &str) -> Result<(u64, String)> {
    let mut hashes = Vec::new();
    let mut result = target.query_iter(format!("SELECT * FROM {}", quote_ident(table)))?;
    for row in result.by_ref() {
        hashes.push(hash_row(&row?.unwrap())?);
    }
    drop(result);
    Ok((hashes.len() as u64, hash_table(hashes)))
}

fn write_insert(
    output: &mut impl Write,
    table: &str,
    columns: &str,
    tuples: &[String],
) -> Result<()> {
    writeln!(
        output,
        "INSERT INTO {} ({columns}) VALUES {};",
        quote_ident(table),
        tuples.join(",")
    )?;
    Ok(())
}

fn format_tuple(values: &[Value]) -> Result<String> {
    Ok(format!(
        "({})",
        values
            .iter()
            .map(value_sql)
            .collect::<Result<Vec<_>>>()?
            .join(",")
    ))
}

fn value_sql(value: &Value) -> Result<String> {
    Ok(match value {
        Value::NULL => "NULL".to_string(),
        Value::Bytes(bytes) => quote_bytes(bytes),
        Value::Int(value) => value.to_string(),
        Value::UInt(value) => value.to_string(),
        Value::Float(value) => finite_number(*value as f64)?,
        Value::Double(value) => finite_number(*value)?,
        Value::Date(year, month, day, hour, minute, second, micros) => format!(
            "'{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}{}'",
            fractional(*micros)
        ),
        Value::Time(negative, days, hours, minutes, seconds, micros) => format!(
            "'{}{total_hours:02}:{minutes:02}:{seconds:02}{}'",
            if *negative { "-" } else { "" },
            fractional(*micros),
            total_hours = u64::from(*hours) + u64::from(*days) * 24
        ),
    })
}

fn hash_row(values: &[Value]) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    hash.update((values.len() as u64).to_le_bytes());
    for value in values {
        match value {
            Value::NULL => hash.update([0]),
            _ => {
                let bytes = canonical_bytes(value)?;
                hash.update([1]);
                hash.update((bytes.len() as u64).to_le_bytes());
                hash.update(bytes);
            }
        }
    }
    Ok(hash.finalize().into())
}

fn canonical_bytes(value: &Value) -> Result<Vec<u8>> {
    Ok(match value {
        Value::NULL => Vec::new(),
        Value::Bytes(bytes) => bytes.clone(),
        Value::Int(value) => value.to_string().into_bytes(),
        Value::UInt(value) => value.to_string().into_bytes(),
        Value::Float(value) => finite_number(*value as f64)?.into_bytes(),
        Value::Double(value) => finite_number(*value)?.into_bytes(),
        Value::Date(year, month, day, hour, minute, second, micros) => format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}{}",
            fractional(*micros)
        )
        .into_bytes(),
        Value::Time(negative, days, hours, minutes, seconds, micros) => format!(
            "{}{total_hours:02}:{minutes:02}:{seconds:02}{}",
            if *negative { "-" } else { "" },
            fractional(*micros),
            total_hours = u64::from(*hours) + u64::from(*days) * 24
        )
        .into_bytes(),
    })
}

fn hash_table(mut hashes: Vec<[u8; 32]>) -> String {
    hashes.sort_unstable();
    let mut hash = Sha256::new();
    hash.update((hashes.len() as u64).to_le_bytes());
    for row in hashes {
        hash.update(row);
    }
    format!("{:x}", hash.finalize())
}

fn finite_number(value: f64) -> Result<String> {
    if !value.is_finite() {
        bail!("cannot dump non-finite floating-point value");
    }
    Ok(value.to_string())
}

fn fractional(micros: u32) -> String {
    if micros == 0 {
        String::new()
    } else {
        format!(".{micros:06}")
    }
}

fn quote_ident(value: &str) -> String {
    format!("`{}`", value.replace('`', "``"))
}

fn quote_bytes(value: &[u8]) -> String {
    format!("X'{}'", hex(value))
}

fn hex_name(value: &str) -> String {
    hex(value.as_bytes())
}

fn hex(value: &[u8]) -> String {
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn read_manifest(path: &Path) -> Result<Manifest> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parse {}", path.display()))
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let file = fs::File::create(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, manifest)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

struct CountingWriter<W> {
    inner: W,
    bytes: u64,
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, bytes: 0 }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_hash_matches_source_and_text_protocol_values() {
        let source = vec![
            Value::Int(42),
            Value::Date(2026, 7, 15, 1, 2, 3, 400),
            Value::NULL,
            Value::Bytes(vec![0, 255]),
        ];
        let target = vec![
            Value::Bytes(b"42".to_vec()),
            Value::Bytes(b"2026-07-15 01:02:03.000400".to_vec()),
            Value::NULL,
            Value::Bytes(vec![0, 255]),
        ];
        assert_eq!(hash_row(&source).unwrap(), hash_row(&target).unwrap());
    }

    #[test]
    fn bytes_are_dumped_without_escape_ambiguity() {
        assert_eq!(quote_bytes(&[0, 39, 255]), "X'0027ff'");
    }
}
