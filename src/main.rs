mod cli;
mod claude;
mod config;
mod discord;
mod discord_api;
mod service;
mod tray;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command, DiscordAction, ServiceAction};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use sysinfo::{Pid, System};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Get the PID file path
fn pid_file_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?
        .join("neywa");
    fs::create_dir_all(&config_dir)?;
    Ok(config_dir.join("neywa.pid"))
}

/// Kill existing neywa daemon if running
fn kill_existing_daemon() -> Result<()> {
    let pid_path = pid_file_path()?;

    if pid_path.exists() {
        if let Ok(pid_str) = fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                let mut sys = System::new();
                sys.refresh_processes(sysinfo::ProcessesToUpdate::All);

                if let Some(process) = sys.process(Pid::from_u32(pid)) {
                    // Verify it's actually neywa
                    if process.name().to_string_lossy().contains("neywa") {
                        tracing::info!("Killing existing neywa daemon (PID: {})", pid);
                        process.kill();
                        // Give it a moment to terminate
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
            }
        }
        // Remove old PID file
        let _ = fs::remove_file(&pid_path);
    }

    Ok(())
}

/// Write current PID to file
fn write_pid_file() -> Result<()> {
    let pid_path = pid_file_path()?;
    let pid = std::process::id();
    fs::write(&pid_path, pid.to_string())?;
    tracing::info!("PID file written: {:?} (PID: {})", pid_path, pid);
    Ok(())
}

/// Remove PID file on exit
fn remove_pid_file() {
    if let Ok(pid_path) = pid_file_path() {
        let _ = fs::remove_file(pid_path);
    }
}

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "neywa=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Daemon => {
            tracing::info!("Starting Neywa daemon...");

            // Kill existing daemon if running
            kill_existing_daemon()?;

            // Write PID file
            write_pid_file()?;

            // Spawn caffeinate to prevent system sleep (display may still sleep)
            let _caffeinate = std::process::Command::new("/usr/bin/caffeinate")
                .arg("-s")
                .arg("-w")
                .arg(std::process::id().to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok();
            tracing::info!("Sleep prevention: caffeinate started");

            // Run daemon
            let result = run_daemon_with_tray();

            // Cleanup
            remove_pid_file();

            result?;
        }
        Command::Run { message } => {
            // For non-daemon commands, use tokio runtime
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                tracing::info!("Running single command...");
                let response = claude::run(&message, false).await?;
                println!("{}", response);
                Ok::<_, anyhow::Error>(())
            })?;
        }
        Command::Install => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                tracing::info!("Running installation...");
                config::install().await?;
                Ok::<_, anyhow::Error>(())
            })?;
        }
        Command::Config => {
            config::show()?;
        }
        Command::Service { action } => match action {
            ServiceAction::Install => {
                service::install()?;
            }
            ServiceAction::Uninstall => {
                service::uninstall()?;
            }
            ServiceAction::Status => {
                service::status()?;
            }
        },
        Command::Discord { action } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                match action {
                    DiscordAction::Channels => discord_api::list_channels().await?,
                    DiscordAction::Send { channel, message } => {
                        discord_api::send_message(&channel, &message).await?
                    }
                    DiscordAction::Guild => discord_api::show_guild().await?,
                    DiscordAction::Create { name, channel_type, category, topic } => {
                        discord_api::create_channel(
                            &name,
                            &channel_type,
                            category.as_deref(),
                            topic.as_deref(),
                        ).await?
                    }
                    DiscordAction::Delete { channel } => {
                        discord_api::delete_channel(&channel).await?
                    }
                    DiscordAction::Move { channel, category } => {
                        discord_api::move_channel(&channel, &category).await?
                    }
                }
                Ok::<_, anyhow::Error>(())
            })?;
        }
    }

    Ok(())
}

fn run_daemon_with_tray() -> Result<()> {
    // Create channels for communication between tray and daemon
    let (status_tx, status_rx) = mpsc::channel();
    let (quit_tx, quit_rx) = mpsc::channel();

    // Clone quit_tx for Ctrl+C handler
    let ctrlc_quit_tx = quit_tx.clone();

    // Set up Ctrl+C handler
    ctrlc::set_handler(move || {
        tracing::info!("Received Ctrl+C, shutting down...");
        let _ = ctrlc_quit_tx.send(());
        // Force exit after a short delay
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(1));
            tracing::info!("Exiting...");
            remove_pid_file();
            std::process::exit(0);
        });
    })?;

    // Spawn Discord bot in a separate thread with its own tokio runtime
    let bot_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

        rt.block_on(async {
            // Send initial status
            let _ = status_tx.send(tray::TrayCommand::UpdateStatus("🟢 Connected".to_string()));

            // Create a future that completes when quit signal is received
            let quit_future = async {
                loop {
                    if quit_rx.try_recv().is_ok() {
                        tracing::info!("Received quit signal");
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            };

            // Run bot with quit signal
            tokio::select! {
                result = discord::run_bot() => {
                    if let Err(e) = result {
                        tracing::error!("Discord bot error: {}", e);
                        let _ = status_tx.send(tray::TrayCommand::UpdateStatus("🔴 Disconnected".to_string()));
                    }
                }
                _ = quit_future => {
                    tracing::info!("Shutting down Discord bot...");
                }
            }
        });
    });

    // Run tray on main thread (required for macOS)
    tray::run_tray(status_rx, quit_tx);

    // Tray exited, force cleanup and exit
    tracing::info!("Tray closed, cleaning up...");
    remove_pid_file();
    std::process::exit(0);
}
