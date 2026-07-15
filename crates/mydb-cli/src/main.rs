use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
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
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let args = Args::parse();

    // Get password if not provided
    let password = match args.password {
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

    let mut client = mydb_wire::Client::connect(&args.host, args.port, &args.user, &password).await?;

    // Select database if specified
    if let Some(db) = &args.database {
        client.execute(&format!("USE {}", db)).await?;
    }

    // Execute single command if provided
    if let Some(cmd) = &args.execute {
        let result = client.execute(cmd).await?;
        print_result(&result);
        return Ok(());
    }

    // Source file if provided
    if let Some(file) = &args.source {
        let content = std::fs::read_to_string(file)?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("--") {
                continue;
            }
            let result = client.execute(line).await?;
            print_result(&result);
        }
        return Ok(());
    }

    // Interactive mode
    let mut rl = DefaultEditor::new()?;

    // Load history
    let history_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".mydb_history");
    let _ = rl.load_history(&history_path);

    // Print welcome message
    println!("MyDB CLI 0.1.0");
    println!("Type 'help' for help, 'quit' to exit.");
    println!();

    let prompt = format!("mydb [{}]> ", args.user);

    loop {
        match rl.readline(&prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                // Add to history
                let _ = rl.add_history_entry(line);

                // Handle special commands
                match line.to_lowercase().as_str() {
                    "quit" | "exit" | "q" => {
                        println!("Bye!");
                        break;
                    }
                    "help" => {
                        print_help();
                    }
                    "status" => {
                        let result = client.execute("SELECT 1").await?;
                        if result.is_empty() {
                            println!("Connected to MyDB server");
                        }
                    }
                    _ => {
                        match client.execute(line).await {
                            Ok(result) => print_result(&result),
                            Err(e) => eprintln!("Error: {}", e),
                        }
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => {
                println!("Bye!");
                break;
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                break;
            }
        }
    }

    // Save history
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

fn print_result(result: &[Vec<String>]) {
    if result.is_empty() {
        println!("Query OK, 0 rows affected");
        return;
    }

    // Print rows
    for row in result {
        println!(
            "{}",
            row.iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join("\t")
        );
    }
    println!("{} rows in set", result.len());
}
