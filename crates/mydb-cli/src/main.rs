use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "mydb-cli", version, about = "MySQL 8.x compatible CLI client")]
struct Args {
    /// Host to connect to
    #[arg(short = 'h', long, default_value = "127.0.0.1")]
    host: String,

    /// Port to connect to
    #[arg(short, long, default_value_t = 3306)]
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
    #[arg(short = 'e', long)]
    execute: Option<String>,

    /// Read input from file
    #[arg(short, long)]
    source: Option<PathBuf>,

    /// Enable verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let args = Args::parse();

    // Get password if not provided
    let _password = match args.password {
        Some(p) => p,
        None => {
            eprint!("Enter password: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input.trim().to_string()
        }
    };

    // Connect to server
    info!(
        "Connecting to {}:{} as {}",
        args.host, args.port, args.user
    );

    // Simple TCP connection with raw MySQL protocol
    let stream = tokio::net::TcpStream::connect(format!("{}:{}", args.host, args.port)).await?;
    info!("Connected!");

    // For now, print a message
    println!("MyDB CLI 0.1.0");
    println!("Connected to {}:{} as {}", args.host, args.port, args.user);
    println!("Type 'help' for help, 'quit' to exit.");
    println!();

    // Interactive mode
    let mut rl = rustyline::DefaultEditor::new()?;

    let history_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".mydb_history");
    let _ = rl.load_history(&history_path);

    let prompt = format!("mydb [{}]> ", args.user);

    loop {
        match rl.readline(&prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let _ = rl.add_history_entry(line);

                match line.to_lowercase().as_str() {
                    "quit" | "exit" | "q" => {
                        println!("Bye!");
                        break;
                    }
                    "help" => {
                        print_help();
                    }
                    "status" => {
                        println!("Connected to MyDB server");
                    }
                    _ => {
                        // Send query via raw TCP (simplified)
                        println!("Query: {}", line);
                        println!("(Query execution not yet implemented in CLI)");
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
