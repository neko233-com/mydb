use anyhow::{bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use clap::Parser;
use mysql::{prelude::Queryable, Conn, Opts, Value};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf, time::Instant};

#[derive(Debug, Parser)]
#[command(
    name = "mydb-migrate",
    about = "Online MySQL 8 to MyDB migration with per-table verification"
)]
struct Args {
    /// MySQL source URL. Falls back to MYSQL_SOURCE_URL.
    #[arg(long)]
    source: Option<String>,

    /// MyDB target URL. Falls back to MYDB_TARGET_URL.
    #[arg(long)]
    target: Option<String>,

    /// Database to migrate.
    #[arg(long)]
    database: String,

    /// Comma-separated table allowlist. All base tables by default.
    #[arg(long, value_delimiter = ',')]
    tables: Vec<String>,

    /// Rows written in one target transaction.
    #[arg(long, default_value_t = 500)]
    batch_size: usize,

    /// Drop matching target tables before migration.
    #[arg(long)]
    drop_existing: bool,

    /// Create schemas but do not copy rows.
    #[arg(long, conflicts_with = "data_only")]
    schema_only: bool,

    /// Copy rows into already-created target tables.
    #[arg(long, conflicts_with = "schema_only")]
    data_only: bool,

    /// JSON verification report path.
    #[arg(long, default_value = "mydb-migration-report.json")]
    report: PathBuf,
}

#[derive(Debug, Serialize)]
struct MigrationReport {
    database: String,
    started_at: String,
    finished_at: String,
    duration_ms: u128,
    schema_only: bool,
    data_only: bool,
    verified: bool,
    tables: Vec<TableReport>,
}

#[derive(Debug, Serialize)]
struct TableReport {
    table: String,
    source_rows: u64,
    copied_rows: u64,
    target_rows: u64,
    source_checksum: String,
    target_checksum: String,
    verified: bool,
    duration_ms: u128,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.batch_size == 0 {
        bail!("--batch-size must be greater than zero");
    }

    let source_url = required_url(args.source.as_deref(), "MYSQL_SOURCE_URL", "--source")?;
    let target_url = required_url(args.target.as_deref(), "MYDB_TARGET_URL", "--target")?;
    let mut source = connect(&source_url, "MySQL source")?;
    let mut target = connect(&target_url, "MyDB target")?;
    let started_at = Utc::now();
    let started = Instant::now();

    ensure_database(&mut target, &args.database)?;
    source
        .query_drop(format!("USE {}", quote_ident(&args.database)))
        .context("select source database")?;
    target
        .query_drop(format!("USE {}", quote_ident(&args.database)))
        .context("select target database")?;

    let source_tables = list_source_tables(&mut source, &args.database)?;
    let tables = choose_tables(source_tables, &args.tables)?;
    let target_tables = list_target_tables(&mut target)?;

    if !args.data_only {
        for table in &tables {
            if target_tables.iter().any(|existing| existing == table) {
                if !args.drop_existing {
                    bail!(
                        "target table `{table}` exists; rerun with --drop-existing or --data-only"
                    );
                }
                target
                    .query_drop(format!("DROP TABLE {}", quote_ident(table)))
                    .with_context(|| format!("drop target table `{table}`"))?;
            }
            let ddl = show_create_table(&mut source, table)?;
            target
                .query_drop(ddl)
                .with_context(|| format!("create target table `{table}`"))?;
        }
    }

    let mut reports = Vec::with_capacity(tables.len());
    if !args.schema_only {
        source
            .query_drop("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .context("set source isolation level")?;
        source
            .query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")
            .context("start source consistent snapshot")?;
    }

    let migration_result = (|| -> Result<()> {
        for table in &tables {
            reports.push(if args.schema_only {
                TableReport {
                    table: table.clone(),
                    source_rows: 0,
                    copied_rows: 0,
                    target_rows: 0,
                    source_checksum: String::new(),
                    target_checksum: String::new(),
                    verified: true,
                    duration_ms: 0,
                }
            } else {
                migrate_table(
                    &mut source,
                    &mut target,
                    &args.database,
                    table,
                    args.batch_size,
                )?
            });
        }
        Ok(())
    })();

    if !args.schema_only {
        let _ = source.query_drop(if migration_result.is_ok() {
            "COMMIT"
        } else {
            "ROLLBACK"
        });
    }
    migration_result?;

    let report = MigrationReport {
        database: args.database,
        started_at: started_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        finished_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        duration_ms: started.elapsed().as_millis(),
        schema_only: args.schema_only,
        data_only: args.data_only,
        verified: reports.iter().all(|table| table.verified),
        tables: reports,
    };
    let json = serde_json::to_string_pretty(&report)?;
    fs::write(&args.report, format!("{json}\n"))
        .with_context(|| format!("write report {}", args.report.display()))?;
    println!("{json}");
    if !report.verified {
        bail!(
            "migration verification failed; see {}",
            args.report.display()
        );
    }
    Ok(())
}

fn required_url(value: Option<&str>, variable: &str, flag: &str) -> Result<String> {
    value
        .map(ToOwned::to_owned)
        .or_else(|| env::var(variable).ok())
        .with_context(|| format!("missing {flag} or {variable}"))
}

fn connect(url: &str, label: &str) -> Result<Conn> {
    let opts = Opts::from_url(url).with_context(|| format!("invalid {label} URL"))?;
    Conn::new(opts).with_context(|| format!("connect to {label}"))
}

fn ensure_database(target: &mut Conn, database: &str) -> Result<()> {
    let databases: Vec<String> = target.query("SHOW DATABASES")?;
    if !databases.iter().any(|existing| existing == database) {
        target
            .query_drop(format!("CREATE DATABASE {}", quote_ident(database)))
            .with_context(|| format!("create target database `{database}`"))?;
    }
    Ok(())
}

fn list_source_tables(source: &mut Conn, database: &str) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES \
         WHERE TABLE_SCHEMA={} AND TABLE_TYPE='BASE TABLE' ORDER BY TABLE_NAME",
        quote_string(database.as_bytes())
    );
    source.query(sql).context("list source tables")
}

fn list_target_tables(target: &mut Conn) -> Result<Vec<String>> {
    target.query("SHOW TABLES").context("list target tables")
}

fn choose_tables(available: Vec<String>, requested: &[String]) -> Result<Vec<String>> {
    if requested.is_empty() {
        return Ok(available);
    }
    for table in requested {
        if !available.iter().any(|available| available == table) {
            bail!("source table `{table}` does not exist");
        }
    }
    Ok(requested.to_vec())
}

fn show_create_table(source: &mut Conn, table: &str) -> Result<String> {
    let row: Option<(String, String)> = source
        .query_first(format!("SHOW CREATE TABLE {}", quote_ident(table)))
        .with_context(|| format!("read schema for `{table}`"))?;
    row.map(|(_, ddl)| ddl)
        .with_context(|| format!("missing schema for `{table}`"))
}

fn table_columns(source: &mut Conn, database: &str, table: &str) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS \
         WHERE TABLE_SCHEMA={} AND TABLE_NAME={} ORDER BY ORDINAL_POSITION",
        quote_string(database.as_bytes()),
        quote_string(table.as_bytes())
    );
    source
        .query(sql)
        .with_context(|| format!("read columns for `{table}`"))
}

fn migrate_table(
    source: &mut Conn,
    target: &mut Conn,
    database: &str,
    table: &str,
    batch_size: usize,
) -> Result<TableReport> {
    let started = Instant::now();
    let columns = table_columns(source, database, table)?;
    if columns.is_empty() {
        bail!("source table `{table}` has no columns");
    }
    let source_rows: u64 = source
        .query_first(format!("SELECT COUNT(*) FROM {}", quote_ident(table)))?
        .unwrap_or(0);
    let mut copied_rows = 0_u64;
    let mut source_hashes = Vec::with_capacity(source_rows as usize);
    let mut batch = Vec::with_capacity(batch_size);
    let mut result = source
        .query_iter(format!("SELECT * FROM {}", quote_ident(table)))
        .with_context(|| format!("scan source table `{table}`"))?;

    for row in result.by_ref() {
        let values = row?.unwrap();
        source_hashes.push(hash_row(&values)?);
        batch.push(values);
        if batch.len() == batch_size {
            copied_rows += write_batch(target, table, &columns, &batch)?;
            batch.clear();
        }
    }
    drop(result);
    if !batch.is_empty() {
        copied_rows += write_batch(target, table, &columns, &batch)?;
    }

    let mut target_hashes = Vec::with_capacity(source_rows as usize);
    let mut target_result = target
        .query_iter(format!("SELECT * FROM {}", quote_ident(table)))
        .with_context(|| format!("verify target table `{table}`"))?;
    for row in target_result.by_ref() {
        target_hashes.push(hash_row(&row?.unwrap())?);
    }
    drop(target_result);
    let target_rows = target_hashes.len() as u64;
    let source_checksum = hash_table(source_hashes);
    let target_checksum = hash_table(target_hashes);
    Ok(TableReport {
        table: table.to_string(),
        source_rows,
        copied_rows,
        target_rows,
        verified: source_rows == copied_rows
            && copied_rows == target_rows
            && source_checksum == target_checksum,
        source_checksum,
        target_checksum,
        duration_ms: started.elapsed().as_millis(),
    })
}

fn write_batch(
    target: &mut Conn,
    table: &str,
    columns: &[String],
    rows: &[Vec<Value>],
) -> Result<u64> {
    let column_list = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(",");
    let tuples = rows
        .iter()
        .map(|row| {
            if row.len() != columns.len() {
                bail!("source row column count changed during migration");
            }
            let values = row
                .iter()
                .map(value_sql)
                .collect::<Result<Vec<_>>>()?
                .join(",");
            Ok(format!("({values})"))
        })
        .collect::<Result<Vec<_>>>()?
        .join(",");
    let sql = format!(
        "INSERT INTO {} ({column_list}) VALUES {tuples}",
        quote_ident(table)
    );

    target.query_drop("START TRANSACTION")?;
    if let Err(error) = target.query_drop(sql) {
        let _ = target.query_drop("ROLLBACK");
        return Err(error).with_context(|| format!("write target table `{table}`"));
    }
    if let Err(error) = target.query_drop("COMMIT") {
        let _ = target.query_drop("ROLLBACK");
        return Err(error).with_context(|| format!("commit target table `{table}`"));
    }
    Ok(rows.len() as u64)
}

fn value_sql(value: &Value) -> Result<String> {
    Ok(match value {
        Value::NULL => "NULL".to_string(),
        Value::Bytes(bytes) => quote_string(bytes),
        Value::Int(value) => value.to_string(),
        Value::UInt(value) => value.to_string(),
        Value::Float(value) => finite_number(*value as f64)?,
        Value::Double(value) => finite_number(*value)?,
        Value::Date(year, month, day, hour, minute, second, micros) => {
            let fraction = fractional(*micros);
            format!("'{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}{fraction}'")
        }
        Value::Time(negative, days, hours, minutes, seconds, micros) => {
            let sign = if *negative { "-" } else { "" };
            let total_hours = u64::from(*hours) + u64::from(*days) * 24;
            let fraction = fractional(*micros);
            format!("'{sign}{total_hours:02}:{minutes:02}:{seconds:02}{fraction}'")
        }
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
    let mut table_hash = Sha256::new();
    table_hash.update((hashes.len() as u64).to_le_bytes());
    for row_hash in hashes {
        table_hash.update(row_hash);
    }
    format!("{:x}", table_hash.finalize())
}

fn finite_number(value: f64) -> Result<String> {
    if !value.is_finite() {
        bail!("cannot migrate non-finite floating-point value");
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

fn quote_string(value: &[u8]) -> String {
    let mut output = String::with_capacity(value.len() * 2 + 3);
    output.push_str("X'");
    for byte in value {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output.push('\'');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_losslessly_encoded() {
        assert_eq!(value_sql(&Value::NULL).unwrap(), "NULL");
        assert_eq!(
            value_sql(&Value::Bytes(vec![0, 39, 255])).unwrap(),
            "X'0027FF'"
        );
        assert_eq!(
            value_sql(&Value::Date(2026, 7, 15, 12, 3, 4, 500)).unwrap(),
            "'2026-07-15 12:03:04.000500'"
        );
    }

    #[test]
    fn checksum_matches_source_types_and_target_text_values() {
        let source = vec![
            Value::Int(7),
            Value::Date(2026, 7, 15, 12, 3, 4, 500),
            Value::NULL,
            Value::Bytes(vec![0, 255]),
        ];
        let target = vec![
            Value::Bytes(b"7".to_vec()),
            Value::Bytes(b"2026-07-15 12:03:04.000500".to_vec()),
            Value::NULL,
            Value::Bytes(vec![0, 255]),
        ];
        assert_eq!(hash_row(&source).unwrap(), hash_row(&target).unwrap());
    }
}
