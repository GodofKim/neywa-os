mod cli;
mod claude;
mod config;
mod discord;
mod tray;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use std::sync::mpsc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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
            run_daemon_with_tray()?;
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
    }

    Ok(())
}

fn run_daemon_with_tray() -> Result<()> {
    // Create channels for communication between tray and daemon
    let (status_tx, status_rx) = mpsc::channel();
    let (quit_tx, quit_rx) = mpsc::channel();

    // Spawn Discord bot in a separate thread with its own tokio runtime
    let bot_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

        rt.block_on(async {
            // Send initial status
            let _ = status_tx.send(tray::TrayCommand::UpdateStatus("🟢 Discord 연결됨".to_string()));

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
                        let _ = status_tx.send(tray::TrayCommand::UpdateStatus("🔴 연결 끊김".to_string()));
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

    // Wait for bot thread to finish
    let _ = bot_handle.join();

    tracing::info!("Neywa daemon stopped");
    Ok(())
}
