use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use mysql::prelude::Queryable;
use mysql::{Conn, Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Actor-ordered game workload benchmark for MyDB/MySQL")]
struct Args {
    #[arg(long)]
    url: String,

    #[arg(long, default_value_t = 8)]
    actors: usize,

    #[arg(long, default_value_t = 500)]
    writes_per_actor: usize,

    /// Number of independent tables used by actors. One preserves the
    /// same-table contention baseline; higher values measure table parallelism.
    #[arg(long, default_value_t = 1)]
    table_count: usize,

    #[arg(long, default_value_t = 10)]
    transaction_size: usize,

    #[arg(long, default_value_t = 50)]
    reads_per_actor: usize,

    #[arg(long, default_value_t = 256)]
    payload_bytes: usize,

    #[arg(long, default_value_t = 20)]
    reconnect_every_transactions: usize,

    #[arg(long, value_enum, default_value_t = WriteMode::Transaction)]
    write_mode: WriteMode,

    #[arg(long)]
    keep_database: bool,

    /// Print MySQL server version and durability settings, then exit.
    #[arg(long)]
    probe_server: bool,
}

#[derive(Debug, Clone, Copy, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum WriteMode {
    Transaction,
    ActorBatch,
    TransactionUpsert,
    ActorBatchUpsert,
}

#[derive(Debug, Serialize)]
struct Report {
    database: String,
    sql_engine: &'static str,
    actors: usize,
    writes: usize,
    table_count: usize,
    reads: usize,
    transaction_size: usize,
    write_mode: WriteMode,
    reconnects: usize,
    elapsed_ms: u128,
    operations_per_second: f64,
    write_transactions_p50_us: u128,
    write_transactions_p95_us: u128,
    write_transactions_p99_us: u128,
    reads_p50_us: u128,
    reads_p95_us: u128,
    reads_p99_us: u128,
    verified_rows: u64,
}

#[derive(Debug, Serialize)]
struct ServerProbe {
    version: String,
    version_comment: String,
    innodb_flush_log_at_trx_commit: u64,
    sync_binlog: u64,
    transaction_isolation: String,
}

#[derive(Default)]
struct ActorResult {
    transaction_latencies: Vec<Duration>,
    read_latencies: Vec<Duration>,
    reconnects: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let opts = Opts::from_url(&args.url).context("invalid database URL")?;
    if args.probe_server {
        return probe_server(opts);
    }
    anyhow::ensure!(args.actors > 0, "actors must be greater than zero");
    anyhow::ensure!(
        args.table_count > 0,
        "table-count must be greater than zero"
    );
    anyhow::ensure!(
        args.transaction_size > 0,
        "transaction-size must be greater than zero"
    );

    let setup_opts = OptsBuilder::from_opts(opts.clone())
        .pool_opts(PoolOpts::default().with_constraints(PoolConstraints::new_const::<0, 1>()));
    let pool = Pool::new(setup_opts)?;
    let database = format!(
        "mydb_game_bench_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    setup(&pool, &database, args.table_count)?;

    let barrier = Arc::new(Barrier::new(args.actors));
    let started = Instant::now();
    let mut handles = Vec::with_capacity(args.actors);
    for actor in 0..args.actors {
        let opts = opts.clone();
        let barrier = barrier.clone();
        let database = database.clone();
        let writes = args.writes_per_actor;
        let table = benchmark_table_name(actor % args.table_count, args.table_count);
        let transaction_size = args.transaction_size;
        let reads = args.reads_per_actor;
        let payload_bytes = args.payload_bytes;
        let reconnect_every = args.reconnect_every_transactions;
        let write_mode = args.write_mode;
        handles.push(std::thread::spawn(move || {
            run_actor(
                opts,
                &database,
                &table,
                actor,
                writes,
                transaction_size,
                reads,
                payload_bytes,
                reconnect_every,
                write_mode,
                barrier,
            )
        }));
    }

    let mut actor_results = Vec::with_capacity(args.actors);
    for handle in handles {
        actor_results.push(
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("benchmark actor panicked"))??,
        );
    }
    let elapsed = started.elapsed();
    let logical_writes = (args.actors * args.writes_per_actor) as u64;
    let expected_rows = if matches!(
        args.write_mode,
        WriteMode::TransactionUpsert | WriteMode::ActorBatchUpsert
    ) {
        args.actors as u64
    } else {
        logical_writes
    };
    let verified_rows = verify(&pool, &database, args.table_count)?;
    anyhow::ensure!(
        verified_rows == expected_rows,
        "row verification failed: expected {expected_rows}, got {verified_rows}"
    );

    let mut transaction_latencies = Vec::new();
    let mut read_latencies = Vec::new();
    let mut reconnects = 0;
    for result in actor_results {
        transaction_latencies.extend(result.transaction_latencies);
        read_latencies.extend(result.read_latencies);
        reconnects += result.reconnects;
    }
    let operations = logical_writes as usize + args.actors * args.reads_per_actor;
    let report = Report {
        database: database.clone(),
        sql_engine: "InnoDB",
        actors: args.actors,
        writes: logical_writes as usize,
        table_count: args.table_count,
        reads: args.actors * args.reads_per_actor,
        transaction_size: args.transaction_size,
        write_mode: args.write_mode,
        reconnects,
        elapsed_ms: elapsed.as_millis(),
        operations_per_second: operations as f64 / elapsed.as_secs_f64(),
        write_transactions_p50_us: percentile_us(&mut transaction_latencies, 50),
        write_transactions_p95_us: percentile_us(&mut transaction_latencies, 95),
        write_transactions_p99_us: percentile_us(&mut transaction_latencies, 99),
        reads_p50_us: percentile_us(&mut read_latencies, 50),
        reads_p95_us: percentile_us(&mut read_latencies, 95),
        reads_p99_us: percentile_us(&mut read_latencies, 99),
        verified_rows,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);

    if !args.keep_database {
        cleanup(&pool, &database)?;
    }
    Ok(())
}

fn probe_server(opts: Opts) -> Result<()> {
    let mut connection = Conn::new(opts)?;
    let probe = connection
        .query_first::<(String, String, u64, u64, String), _>(
            "SELECT VERSION(), @@version_comment, @@innodb_flush_log_at_trx_commit, \
             @@sync_binlog, @@transaction_isolation",
        )?
        .ok_or_else(|| anyhow::anyhow!("MySQL server probe returned no row"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&ServerProbe {
            version: probe.0,
            version_comment: probe.1,
            innodb_flush_log_at_trx_commit: probe.2,
            sync_binlog: probe.3,
            transaction_isolation: probe.4,
        })?
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_actor(
    opts: Opts,
    database: &str,
    table: &str,
    actor: usize,
    writes: usize,
    transaction_size: usize,
    reads: usize,
    payload_bytes: usize,
    reconnect_every: usize,
    write_mode: WriteMode,
    barrier: Arc<Barrier>,
) -> Result<ActorResult> {
    let mut connection = connect(&opts, database)?;
    let payload = vec![(actor % 251) as u8; payload_bytes];
    let mut result = ActorResult::default();
    barrier.wait();

    for (transaction, start) in (0..writes).step_by(transaction_size).enumerate() {
        if reconnect_every > 0 && transaction > 0 && transaction % reconnect_every == 0 {
            drop(connection);
            connection = connect(&opts, database)?;
            result.reconnects += 1;
        }
        let end = (start + transaction_size).min(writes);
        let began = Instant::now();
        match write_mode {
            WriteMode::Transaction => {
                connection.query_drop("START TRANSACTION")?;
                for sequence in start..end {
                    connection.exec_drop(
                        format!("INSERT INTO `{table}`(actor_id,seq,payload) VALUES (?,?,?)"),
                        (actor as u64, sequence as u64, payload.as_slice()),
                    )?;
                }
                connection.query_drop("COMMIT")?;
            }
            WriteMode::ActorBatch => {
                let payload = hex_literal(&payload);
                let values = (start..end)
                    .map(|sequence| format!("({actor},{sequence},{payload})"))
                    .collect::<Vec<_>>()
                    .join(",");
                connection.query_drop(format!(
                    "INSERT INTO `{table}`(actor_id,seq,payload) VALUES {values}"
                ))?;
            }
            WriteMode::TransactionUpsert => {
                connection.query_drop("START TRANSACTION")?;
                for _ in start..end {
                    connection.exec_drop(
                        format!(
                            "INSERT INTO `{table}`(actor_id,seq,payload) VALUES (?,?,?) \
                         ON DUPLICATE KEY UPDATE payload=VALUES(payload)",
                        ),
                        (actor as u64, 0_u64, payload.as_slice()),
                    )?;
                }
                connection.query_drop("COMMIT")?;
            }
            WriteMode::ActorBatchUpsert => {
                let payload = hex_literal(&payload);
                let values = (start..end)
                    .map(|_| format!("({actor},0,{payload})"))
                    .collect::<Vec<_>>()
                    .join(",");
                connection.query_drop(format!(
                    "INSERT INTO `{table}`(actor_id,seq,payload) VALUES {values} \
                     ON DUPLICATE KEY UPDATE payload=VALUES(payload)"
                ))?;
            }
        }
        result.transaction_latencies.push(began.elapsed());
    }

    for _ in 0..reads {
        let began = Instant::now();
        let _: Option<Vec<u8>> = connection.exec_first(
            format!("SELECT payload FROM `{table}` WHERE actor_id=? LIMIT 1"),
            (actor as u64,),
        )?;
        result.read_latencies.push(began.elapsed());
    }
    Ok(result)
}

fn connect(opts: &Opts, database: &str) -> Result<Conn> {
    let mut connection = Conn::new(opts.clone())?;
    connection.query_drop(format!("USE `{database}`"))?;
    Ok(connection)
}

fn setup(pool: &Pool, database: &str, table_count: usize) -> Result<()> {
    let mut connection = pool.get_conn()?;
    connection.query_drop(format!("CREATE DATABASE `{database}`"))?;
    connection.query_drop(format!("USE `{database}`"))?;
    for index in 0..table_count {
        let table = benchmark_table_name(index, table_count);
        connection.query_drop(format!(
            "CREATE TABLE `{table}`(\
            actor_id BIGINT NOT NULL,\
            seq BIGINT NOT NULL,\
            payload BLOB NOT NULL,\
            PRIMARY KEY(actor_id,seq)\
        ) ENGINE=InnoDB"
        ))?;
    }
    Ok(())
}

fn verify(pool: &Pool, database: &str, table_count: usize) -> Result<u64> {
    let mut connection = pool.get_conn()?;
    connection.query_drop(format!("USE `{database}`"))?;
    let mut rows = 0_u64;
    for index in 0..table_count {
        let table = benchmark_table_name(index, table_count);
        rows += connection
            .query_first::<u64, _>(format!("SELECT COUNT(*) FROM `{table}`"))?
            .ok_or_else(|| anyhow::anyhow!("COUNT(*) returned no row"))?;
    }
    Ok(rows)
}

fn benchmark_table_name(index: usize, table_count: usize) -> String {
    if table_count == 1 {
        "game_events".to_string()
    } else {
        format!("game_events_{index}")
    }
}

fn cleanup(pool: &Pool, database: &str) -> Result<()> {
    anyhow::ensure!(
        database.starts_with("mydb_game_bench_"),
        "refusing to drop non-benchmark database"
    );
    pool.get_conn()?
        .query_drop(format!("DROP DATABASE `{database}`"))?;
    Ok(())
}

fn percentile_us(values: &mut [Duration], percentile: usize) -> u128 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let index = ((values.len() - 1) * percentile) / 100;
    values[index].as_micros()
}

fn hex_literal(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(value.len() * 2 + 3);
    output.push_str("X'");
    for byte in value {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output.push('\'');
    output
}
