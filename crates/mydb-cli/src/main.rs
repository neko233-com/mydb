use std::{fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use mysql::{prelude::Queryable, Conn, Opts, OptsBuilder, Value as MysqlValue};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Subcommand, Debug)]
enum CliCommand {
    /// Native Agent diagnostics over the local management API
    #[command(disable_help_flag = true)]
    Agent {
        #[command(subcommand)]
        command: Option<AgentCommand>,
        /// Print Agent command help
        #[arg(long, action = clap::ArgAction::SetTrue)]
        help: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AgentCommand {
    /// Show health, locks, actor queue, group commit, and checkpoint signals
    Health,
    /// List captured slow queries and SQL errors
    Slow,
    /// Diagnose current server signals
    Diagnose {
        #[arg(default_value = "diagnose current MyDB health")]
        question: String,
    },
    /// Ask an English or Chinese operational question using the built-in offline Agent
    Ask { question: String },
    /// Statically inspect SQL for common game-service latency risks
    Optimize { sql: String },
}

#[derive(Parser, Debug)]
#[command(
    name = "mydb-cli",
    version,
    about = "MySQL 8.x compatible CLI client",
    disable_help_flag = true
)]
struct Args {
    /// Host to connect to
    #[arg(short = 'h', long, default_value = "127.0.0.1")]
    host: String,

    /// Port to connect to
    #[arg(short = 'P', long, default_value_t = 3306)]
    port: u16,

    /// User to connect as
    #[arg(short = 'u', long, default_value = "root")]
    user: String,

    /// Password (will prompt if not provided)
    #[arg(short, long)]
    password: Option<String>,

    /// Database to use
    #[arg(short = 'D', long)]
    database: Option<String>,

    /// Execute command and exit
    #[arg(short = 'e', long, conflicts_with = "source")]
    execute: Option<String>,

    /// Read input from file
    #[arg(short, long)]
    source: Option<PathBuf>,

    /// Enable verbose output
    #[arg(short, long)]
    verbose: bool,

    /// HTTP management/Agent port
    #[arg(long, default_value_t = 4306)]
    http_port: u16,

    /// HTTP management/Agent bearer password (defaults to --password or root)
    #[arg(long)]
    admin_password: Option<String>,

    #[command(subcommand)]
    command: Option<CliCommand>,

    /// Print help
    #[arg(long, action = clap::ArgAction::SetTrue)]
    help: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let args = Args::parse();

    if args.help {
        Args::command().print_help()?;
        println!();
        return Ok(());
    }

    if let Some(CliCommand::Agent { command, help }) = &args.command {
        if *help || command.is_none() {
            let mut root = Args::command();
            root.find_subcommand_mut("agent")
                .expect("agent subcommand is defined")
                .print_help()?;
            println!();
            return Ok(());
        }
        let admin_password = args
            .admin_password
            .as_deref()
            .or(args.password.as_deref())
            .unwrap_or("root");
        run_agent_command(
            &args.host,
            args.http_port,
            admin_password,
            command.as_ref().expect("checked above"),
        )
        .await?;
        return Ok(());
    }

    let password = match args.password.clone() {
        Some(p) => p,
        None => {
            eprint!("Enter password: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input.trim().to_string()
        }
    };

    let mut connection = connect_sql(&args, &password)?;
    if let Some(sql) = &args.execute {
        for statement in split_sql_script(sql)? {
            execute_sql(&mut connection, &statement)?;
        }
        return Ok(());
    }
    if let Some(path) = &args.source {
        let script = fs::read_to_string(path)
            .with_context(|| format!("read SQL script {}", path.display()))?;
        for statement in split_sql_script(&script)? {
            execute_sql(&mut connection, &statement)
                .with_context(|| format!("execute statement from {}", path.display()))?;
        }
        return Ok(());
    }

    println!("MyDB CLI {}", env!("CARGO_PKG_VERSION"));
    println!("Connected to {}:{} as {}", args.host, args.port, args.user);
    println!("End SQL with ';'. Type 'help', 'status', or 'quit'.");
    println!();

    // Interactive mode
    let mut rl = rustyline::DefaultEditor::new()?;

    let history_path = dirs::home_dir().unwrap_or_default().join(".mydb_history");
    let _ = rl.load_history(&history_path);

    let prompt = format!("mydb [{}]> ", args.user);
    let mut pending = String::new();

    loop {
        let current_prompt = if pending.is_empty() {
            &prompt
        } else {
            "    -> "
        };
        match rl.readline(current_prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let _ = rl.add_history_entry(line);

                match (pending.is_empty(), line.to_lowercase().as_str()) {
                    (true, "quit" | "exit" | "q") => {
                        println!("Bye!");
                        break;
                    }
                    (true, "help") => {
                        print_help();
                    }
                    (true, "status") => match connection.ping() {
                        Ok(()) => println!("Connection is alive"),
                        Err(error) => {
                            eprintln!("Connection lost: {error}");
                            match connect_sql(&args, &password) {
                                Ok(new_connection) => {
                                    connection = new_connection;
                                    println!("Reconnected");
                                }
                                Err(error) => eprintln!("Reconnect failed: {error:#}"),
                            }
                        }
                    },
                    (_, _) => {
                        pending.push_str(line);
                        pending.push('\n');
                        let statements = match split_complete_sql(&pending) {
                            Ok(value) => value,
                            Err(error) => {
                                eprintln!("Parse error: {error:#}");
                                pending.clear();
                                continue;
                            }
                        };
                        if statements.is_empty() {
                            continue;
                        }
                        let consumed_all = pending.trim_end().ends_with(';');
                        if !consumed_all {
                            continue;
                        }
                        pending.clear();
                        for statement in statements {
                            if let Err(error) = execute_sql(&mut connection, &statement) {
                                eprintln!("Query failed: {error:#}");
                                eprintln!(
                                    "Reconnecting for the next command; the failed SQL is not replayed"
                                );
                                match connect_sql(&args, &password) {
                                    Ok(new_connection) => connection = new_connection,
                                    Err(error) => eprintln!("Reconnect failed: {error:#}"),
                                }
                                break;
                            }
                        }
                    }
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(rustyline::error::ReadlineError::Eof) => {
                println!("Bye!");
                break;
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                break;
            }
        }
    }

    let _ = rl.save_history(&history_path);

    Ok(())
}

fn connect_sql(args: &Args, password: &str) -> Result<Conn> {
    let options = OptsBuilder::default()
        .ip_or_hostname(Some(args.host.clone()))
        .tcp_port(args.port)
        .user(Some(args.user.clone()))
        .pass(Some(password.to_string()))
        .db_name(args.database.clone())
        .prefer_socket(false);
    Conn::new(Opts::from(options)).with_context(|| {
        format!(
            "cannot connect to MySQL protocol at {}:{} as {}",
            args.host, args.port, args.user
        )
    })
}

fn execute_sql(connection: &mut Conn, sql: &str) -> Result<()> {
    let sql = sql.trim().trim_end_matches(';').trim();
    if sql.is_empty() {
        return Ok(());
    }
    let mut result = connection.query_iter(sql)?;
    while let Some(mut result_set) = result.iter() {
        let columns = result_set
            .columns()
            .as_ref()
            .iter()
            .map(|column| column.name_str().to_string())
            .collect::<Vec<_>>();
        let affected_rows = result_set.affected_rows();
        let last_insert_id = result_set.last_insert_id();
        if !columns.is_empty() {
            println!("{}", columns.join("\t"));
        }
        let mut row_count = 0_u64;
        for row in result_set.by_ref() {
            let values = row?.unwrap();
            println!(
                "{}",
                values
                    .iter()
                    .map(format_mysql_value)
                    .collect::<Vec<_>>()
                    .join("\t")
            );
            row_count += 1;
        }
        if columns.is_empty() {
            if let Some(id) = last_insert_id.filter(|id| *id != 0) {
                println!("Query OK, {affected_rows} rows affected, insert id {id}");
            } else {
                println!("Query OK, {affected_rows} rows affected");
            }
        } else {
            println!("{row_count} rows in set");
        }
    }
    Ok(())
}

fn format_mysql_value(value: &MysqlValue) -> String {
    match value {
        MysqlValue::NULL => "NULL".into(),
        MysqlValue::Bytes(bytes) => match std::str::from_utf8(bytes) {
            Ok(value) if !value.chars().any(char::is_control) => value.to_string(),
            _ => format!(
                "0x{}",
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        },
        MysqlValue::Int(value) => value.to_string(),
        MysqlValue::UInt(value) => value.to_string(),
        MysqlValue::Float(value) => value.to_string(),
        MysqlValue::Double(value) => value.to_string(),
        MysqlValue::Date(year, month, day, hour, minute, second, micros) => {
            let date = format!("{year:04}-{month:02}-{day:02}");
            if *hour == 0 && *minute == 0 && *second == 0 && *micros == 0 {
                date
            } else if *micros == 0 {
                format!("{date} {hour:02}:{minute:02}:{second:02}")
            } else {
                format!("{date} {hour:02}:{minute:02}:{second:02}.{micros:06}")
            }
        }
        MysqlValue::Time(negative, days, hours, minutes, seconds, micros) => {
            let total_hours = days * 24 + u32::from(*hours);
            let sign = if *negative { "-" } else { "" };
            if *micros == 0 {
                format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}")
            } else {
                format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}.{micros:06}")
            }
        }
    }
}

fn split_sql_script(script: &str) -> Result<Vec<String>> {
    let (statements, remainder) = split_sql(script)?;
    if !remainder.trim().is_empty() {
        let mut statements = statements;
        statements.push(remainder.trim().to_string());
        Ok(statements)
    } else {
        Ok(statements)
    }
}

fn split_complete_sql(script: &str) -> Result<Vec<String>> {
    let (statements, _) = split_sql(script)?;
    Ok(statements)
}

fn split_sql(script: &str) -> Result<(Vec<String>, String)> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in script.chars() {
        current.push(character);
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            continue;
        }
        if matches!(character, '\'' | '"' | '`') {
            quote = Some(character);
        } else if character == ';' {
            let statement = current.trim().trim_end_matches(';').trim();
            if !statement.is_empty() {
                statements.push(statement.to_string());
            }
            current.clear();
        }
    }
    if let Some(character) = quote {
        bail!("unterminated {character} quote");
    }
    Ok((statements, current))
}

async fn run_agent_command(
    host: &str,
    port: u16,
    password: &str,
    command: &AgentCommand,
) -> Result<()> {
    let (method, path, body) = match command {
        AgentCommand::Health => ("GET", "/api/v1/agent/health", None),
        AgentCommand::Slow => ("GET", "/api/v1/agent/slow-queries", None),
        AgentCommand::Diagnose { question } => (
            "POST",
            "/api/v1/agent/diagnose",
            Some(json!({"question": question})),
        ),
        AgentCommand::Ask { question } => (
            "POST",
            "/api/v1/agent/ask",
            Some(json!({"question": question})),
        ),
        AgentCommand::Optimize { sql } => ("POST", "/api/v1/agent/sql", Some(json!({"sql": sql}))),
    };
    let response = agent_http_request(host, port, password, method, path, body.as_ref()).await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn agent_http_request(
    host: &str,
    port: u16,
    password: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    let payload = body
        .map(serde_json::to_vec)
        .transpose()?
        .unwrap_or_default();
    let mut stream = tokio::net::TcpStream::connect((host, port))
        .await
        .with_context(|| format!("cannot connect to Agent API at {host}:{port}"))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Bearer {password}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(&payload).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    parse_agent_http_response(&response)
}

fn parse_agent_http_response(response: &[u8]) -> Result<Value> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("invalid Agent HTTP response"))?;
    let headers = std::str::from_utf8(&response[..header_end])?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("invalid Agent HTTP status"))?;
    let body = &response[header_end + 4..];
    let body = if headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
    {
        decode_chunked(body)?
    } else {
        body.to_vec()
    };
    if !(200..300).contains(&status) {
        anyhow::bail!(
            "Agent API returned HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    serde_json::from_slice(&body).context("Agent API returned invalid JSON")
}

fn decode_chunked(mut input: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let line_end = input
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| anyhow::anyhow!("invalid chunked Agent response"))?;
        let size = usize::from_str_radix(std::str::from_utf8(&input[..line_end])?.trim(), 16)?;
        input = &input[line_end + 2..];
        if size == 0 {
            break;
        }
        if input.len() < size + 2 || &input[size..size + 2] != b"\r\n" {
            anyhow::bail!("truncated chunked Agent response");
        }
        output.extend_from_slice(&input[..size]);
        input = &input[size + 2..];
    }
    Ok(output)
}

fn print_help() {
    println!("MyDB CLI Commands:");
    println!("  help              Show this help message");
    println!("  status            Show server status");
    println!("  quit/exit/q       Exit the client");
    println!();
    println!("SQL Commands:");
    println!("  Any valid MySQL SQL statement");
    println!();
    println!("Examples:");
    println!("  SHOW DATABASES;");
    println!("  CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(100));");
    println!("  INSERT INTO users VALUES (1, 'Alice');");
    println!("  SELECT * FROM users;");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_agent_response() {
        let response = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 16\r\n\r\n{\"healthy\":true}";
        assert_eq!(
            parse_agent_http_response(response).unwrap()["healthy"],
            true
        );
    }

    #[test]
    fn parses_chunked_agent_response() {
        let response = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n10\r\n{\"healthy\":true}\r\n0\r\n\r\n";
        assert_eq!(
            parse_agent_http_response(response).unwrap()["healthy"],
            true
        );
    }

    #[test]
    fn splits_scripts_without_breaking_quoted_semicolons() {
        let statements = split_sql_script(
            "INSERT INTO notes VALUES ('actor;one'); SELECT `semi;column` FROM notes",
        )
        .unwrap();
        assert_eq!(statements.len(), 2);
        assert_eq!(statements[0], "INSERT INTO notes VALUES ('actor;one')");
        assert_eq!(statements[1], "SELECT `semi;column` FROM notes");
    }

    #[test]
    fn renders_null_text_and_binary_values() {
        assert_eq!(format_mysql_value(&MysqlValue::NULL), "NULL");
        assert_eq!(
            format_mysql_value(&MysqlValue::Bytes(b"player-1".to_vec())),
            "player-1"
        );
        assert_eq!(
            format_mysql_value(&MysqlValue::Bytes(vec![0, 0xff])),
            "0x00ff"
        );
    }
}
