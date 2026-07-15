use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "mydb-server", version, about = "MySQL 8.x compatible database server")]
struct Args {
    /// Configuration file path
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Run as daemon (Linux/macOS only)
    #[arg(long)]
    daemon: bool,

    /// Install as system service
    #[arg(long)]
    service: Option<String>,

    /// Override listen port
    #[arg(short, long)]
    port: Option<u16>,

    /// Override data directory
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    // Handle service commands
    if let Some(cmd) = &args.service {
        match cmd.as_str() {
            "install" => {
                info!("Installing mydb-server as system service...");
                install_service()?;
                return Ok(());
            }
            "uninstall" => {
                info!("Uninstalling mydb-server service...");
                uninstall_service()?;
                return Ok(());
            }
            "start" => {
                info!("Starting mydb-server service...");
                start_service()?;
                return Ok(());
            }
            "stop" => {
                info!("Stopping mydb-server service...");
                stop_service()?;
                return Ok(());
            }
            _ => {
                eprintln!("Unknown service command: {}", cmd);
                eprintln!("Available commands: install, uninstall, start, stop");
                std::process::exit(1);
            }
        }
    }

    // Load configuration
    let mut config = mydb_config::ServerConfig::load(args.config.as_deref())?;

    // Apply command line overrides
    if let Some(port) = args.port {
        config.server.port = port;
    }
    if let Some(data_dir) = &args.data_dir {
        config.storage.data_dir = data_dir.clone();
    }

    // Ensure data directory exists
    std::fs::create_dir_all(&config.storage.data_dir)?;

    info!(
        "Starting MyDB server on {}:{}",
        config.server.host, config.server.port
    );
    info!("Data directory: {:?}", config.storage.data_dir);
    info!("Storage engine: {}", config.storage.engine);

    // Start the server
    let listener = tokio::net::TcpListener::bind(format!(
        "{}:{}",
        config.server.host, config.server.port
    ))
    .await?;

    info!("MyDB server is ready for connections");

    // Accept connections
    loop {
        let (stream, addr) = listener.accept().await?;
        info!("New connection from: {}", addr);

        tokio::spawn(async move {
            if let Err(e) = mydb_wire::handle_connection(stream).await {
                tracing::error!("Connection error from {}: {}", addr, e);
            }
        });
    }
}

fn install_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let service_content = format!(
            r#"[Unit]
Description=MyDB Server
After=network.target

[Service]
Type=simple
ExecStart={}/mydb-server
Restart=always
RestartSec=5
User=mydb
Group=mydb
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
"#,
            std::env::current_exe()?.parent().unwrap().display()
        );

        let service_path = "/etc/systemd/system/mydb.service";
        std::fs::write(service_path, service_content)?;

        // Create mydb user if not exists
        std::process::Command::new("useradd")
            .args(["-r", "-s", "/bin/false", "mydb"])
            .output()
            .ok(); // Ignore error if user already exists

        // Create data directory
        std::fs::create_dir_all("/var/lib/mydb")?;
        std::process::Command::new("chown")
            .args(["-R", "mydb:mydb", "/var/lib/mydb"])
            .output()?;

        println!("Service installed. Enable with: systemctl enable mydb");
        println!("Start with: systemctl start mydb");
    }

    #[cfg(target_os = "macos")]
    {
        let service_content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.mydb.server</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}/mydb-server</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/usr/local/var/log/mydb.log</string>
    <key>StandardErrorPath</key>
    <string>/usr/local/var/log/mydb.log</string>
</dict>
</plist>
"#,
            std::env::current_exe()?.parent().unwrap().display()
        );

        let service_path = format!(
            "{}/Library/LaunchDaemons/com.mydb.server.plist",
            dirs::home_dir().unwrap().display()
        );
        std::fs::write(&service_path, service_content)?;

        println!("Service installed. Load with: launchctl load {}", service_path);
        println!("Start with: launchctl start com.mydb.server");
    }

    #[cfg(target_os = "windows")]
    {
        println!("For Windows, use sc.exe or install as a Windows Service:");
        println!("  sc.exe create MyDBServer binPath= \"{}\\mydb-server.exe\"", 
                 std::env::current_exe()?.parent().unwrap().display());
        println!("  sc.exe start MyDBServer");
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        println!("Service installation not supported on this platform");
    }

    Ok(())
}

fn uninstall_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("systemctl")
            .args(["stop", "mydb"])
            .output()
            .ok();
        std::process::Command::new("systemctl")
            .args(["disable", "mydb"])
            .output()
            .ok();
        std::fs::remove_file("/etc/systemd/system/mydb.service")?;
        std::process::Command::new("systemctl")
            .args(["daemon-reload"])
            .output()?;
        println!("Service uninstalled");
    }

    #[cfg(target_os = "macos")]
    {
        let service_path = format!(
            "{}/Library/LaunchDaemons/com.mydb.server.plist",
            dirs::home_dir().unwrap().display()
        );
        std::process::Command::new("launchctl")
            .args(["unload", &service_path])
            .output()
            .ok();
        std::fs::remove_file(service_path)?;
        println!("Service uninstalled");
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("sc.exe")
            .args(["stop", "MyDBServer"])
            .output()
            .ok();
        std::process::Command::new("sc.exe")
            .args(["delete", "MyDBServer"])
            .output()?;
        println!("Service uninstalled");
    }

    Ok(())
}

fn start_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("systemctl")
            .args(["start", "mydb"])
            .output()?;
        println!("Service started");
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("launchctl")
            .args(["start", "com.mydb.server"])
            .output()?;
        println!("Service started");
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("sc.exe")
            .args(["start", "MyDBServer"])
            .output()?;
        println!("Service started");
    }

    Ok(())
}

fn stop_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("systemctl")
            .args(["stop", "mydb"])
            .output()?;
        println!("Service stopped");
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("launchctl")
            .args(["stop", "com.mydb.server"])
            .output()?;
        println!("Service stopped");
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("sc.exe")
            .args(["stop", "MyDBServer"])
            .output()?;
        println!("Service stopped");
    }

    Ok(())
}
