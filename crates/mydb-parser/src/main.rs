use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "mydb-parser", version, about = "MyDB SQL Parser Test")]
struct Args {
    /// SQL query to parse
    #[arg(short, long)]
    query: Option<String>,

    /// Interactive mode
    #[arg(short, long)]
    interactive: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    if let Some(query) = &args.query {
        info!("Parsing query: {}", query);
        // TODO: Implement SQL parsing
        println!("Query: {}", query);
        println!("Status: Parser not yet implemented");
    } else if args.interactive {
        println!("MyDB Parser Interactive Mode");
        println!("Type SQL queries or 'quit' to exit.");
        println!();

        loop {
            eprint!("mydb-parser> ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let input = input.trim();

            if input == "quit" || input == "exit" {
                break;
            }

            if input.is_empty() {
                continue;
            }

            info!("Parsing: {}", input);
            println!("Parsed: {}", input);
        }
    } else {
        println!("MyDB SQL Parser");
        println!("Use --query <SQL> or --interactive");
    }

    Ok(())
}
