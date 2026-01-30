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
}
