use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    pub discord_bot_token: Option<String>,
    pub discord_guild_id: Option<u64>,
    #[serde(default)]
    pub allowed_user_ids: Vec<u64>,
}

impl Config {
    /// Get the config file path
    pub fn path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .context("Could not find config directory")?
            .join("neywa");

        Ok(config_dir.join("config.json"))
    }

    /// Load config from file
    pub fn load() -> Result<Self> {
        let path = Self::path()?;

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config from {:?}", path))?;

        serde_json::from_str(&content).context("Failed to parse config")
    }

    /// Save config to file
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory {:?}", parent))?;
        }

        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write config to {:?}", path))?;

        Ok(())
    }
}

/// Run the installation wizard
pub async fn install() -> Result<()> {
    println!("=== Neywa Installation ===\n");

    // 1. Discord Bot Token
    println!("Step 1: Discord Bot Setup");
    println!("  1. Go to https://discord.com/developers/applications");
    println!("  2. Click 'New Application' and give it a name");
    println!("  3. Go to 'Bot' tab and click 'Add Bot'");
    println!("  4. Click 'Reset Token' and copy the token");
    println!("  5. Enable 'MESSAGE CONTENT INTENT' under Privileged Gateway Intents\n");

    print!("Enter your Discord bot token: ");
    std::io::Write::flush(&mut std::io::stdout())?;

    let mut token = String::new();
    std::io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    if token.is_empty() {
        anyhow::bail!("Bot token is required");
    }

    // 2. Discord Guild (Server) ID
    println!("\nStep 2: Discord Server ID");
    println!("  1. Enable Developer Mode in Discord (Settings > Advanced > Developer Mode)");
    println!("  2. Right-click your server icon and select 'Copy Server ID'\n");

    print!("Enter your Discord server (guild) ID: ");
    std::io::Write::flush(&mut std::io::stdout())?;

    let mut guild_id_str = String::new();
    std::io::stdin().read_line(&mut guild_id_str)?;
    let guild_id_str = guild_id_str.trim();

    let guild_id: Option<u64> = if guild_id_str.is_empty() {
        println!("  (skipped - you can set it later with 'neywa config')");
        None
    } else {
        match guild_id_str.parse::<u64>() {
            Ok(id) => Some(id),
            Err(_) => {
                println!("  Invalid ID, skipping...");
                None
            }
        }
    };

    // 3. Allowed user IDs (required for command execution)
    println!("\nStep 3: Allowed Discord User IDs");
    println!("  1. Enable Developer Mode in Discord (Settings > Advanced > Developer Mode)");
    println!("  2. Right-click your username (or target user) and click 'Copy User ID'");
    println!("  3. Enter one or more user IDs separated by commas\n");

    print!("Enter allowed user IDs (comma-separated): ");
    std::io::Write::flush(&mut std::io::stdout())?;

    let mut allowed_ids_str = String::new();
    std::io::stdin().read_line(&mut allowed_ids_str)?;
    let allowed_ids_str = allowed_ids_str.trim();

    let allowed_user_ids: Vec<u64> = allowed_ids_str
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<u64>()
                .map_err(|_| anyhow::anyhow!("Invalid user ID: {}", s))
        })
        .collect::<Result<Vec<u64>>>()?;

    if allowed_user_ids.is_empty() {
        anyhow::bail!("At least one allowed user ID is required");
    }

    // 4. Invite bot to server
    println!("\nStep 4: Invite bot to your server");
    println!("  1. Go to 'OAuth2' > 'URL Generator'");
    println!("  2. Select scopes: 'bot'");
    println!("  3. Select permissions: 'Manage Channels', 'Send Messages', 'Read Message History', 'View Channels'");
    println!("  4. Copy the URL and open it to invite the bot\n");

    // 5. Create recommended channels
    println!("Step 5: Create channels in your Discord server");
    println!("  Recommended channel structure:");
    println!("    #general  - General conversation");
    println!("    #code     - Coding tasks");
    println!("    #research - Web search / research");
    println!("    #tasks    - Scheduling and reminders");
    println!("    #logs     - Activity logs (bot writes here)\n");

    print!("Press Enter when ready...");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut _dummy = String::new();
    std::io::stdin().read_line(&mut _dummy)?;

    // Save config
    let config = Config {
        discord_bot_token: Some(token),
        discord_guild_id: guild_id,
        allowed_user_ids,
    };
    config.save()?;

    println!("\n=== Installation Complete ===");
    println!("Config saved to: {:?}", Config::path()?);
    println!();
    println!("Next steps:");
    println!();
    println!("  1. Start the service (auto-start on login):");
    println!("     neywa service install");
    println!();
    println!("  2. Grant Full Disk Access (the service install will guide you)");
    println!();
    println!("  Other commands:");
    println!("    neywa daemon             # Run in foreground (for testing)");
    println!("    neywa service status     # Check service status");
    println!("    neywa service uninstall  # Disable auto-start");

    Ok(())
}

/// Show current configuration
pub fn show() -> Result<()> {
    let config = Config::load()?;
    let path = Config::path()?;

    println!("Config file: {:?}", path);
    println!();

    if let Some(token) = &config.discord_bot_token {
        let masked = if token.len() > 10 {
            format!("{}...{}", &token[..5], &token[token.len() - 5..])
        } else {
            "***".to_string()
        };
        println!("Discord Bot Token: {}", masked);
    } else {
        println!("Discord Bot Token: (not set)");
    }

    if let Some(guild_id) = config.discord_guild_id {
        println!("Discord Guild ID: {}", guild_id);
    }

    if config.allowed_user_ids.is_empty() {
        println!("Allowed User IDs: (none - all requests will be denied)");
    } else {
        println!("Allowed User IDs: {:?}", config.allowed_user_ids);
    }

    Ok(())
}
