use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "neywa")]
#[command(about = "AI-powered personal OS via Claude Code + Discord")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the Discord bot daemon (listens for messages)
    Daemon,

    /// Run a single command through Claude Code
    Run {
        /// The message/command to send to Claude Code
        message: String,
    },

    /// Initial setup (Discord token, Claude Code hooks)
    Install,

    /// Show current configuration
    Config,

    /// Manage auto-start service (LaunchAgent)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },

    /// Discord server control commands
    Discord {
        #[command(subcommand)]
        action: DiscordAction,
    },
}

#[derive(Subcommand)]
pub enum ServiceAction {
    /// Install and enable auto-start on login
    Install,

    /// Uninstall and disable auto-start
    Uninstall,

    /// Show service status
    Status,
}

#[derive(Subcommand)]
pub enum DiscordAction {
    /// List channels in the configured server
    Channels,

    /// Send a message to a channel
    Send {
        /// Channel name (e.g., "general") or channel ID
        channel: String,

        /// Message to send
        message: String,
    },

    /// Show server (guild) info
    Guild,
}
