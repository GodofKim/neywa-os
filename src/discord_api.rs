use crate::config::Config;
use anyhow::{Context, Result};
use serde::Deserialize;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

#[derive(Debug, Deserialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
    pub member_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Channel {
    pub id: String,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub channel_type: u8,
    pub position: Option<i32>,
    pub parent_id: Option<String>,
}

impl Channel {
    pub fn type_name(&self) -> &str {
        match self.channel_type {
            0 => "text",
            2 => "voice",
            4 => "category",
            5 => "announcement",
            13 => "stage",
            15 => "forum",
            _ => "other",
        }
    }
}

fn build_client(token: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bot {}", token)).unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

fn load_token_and_guild() -> Result<(String, u64)> {
    let config = Config::load()?;
    let token = config
        .discord_bot_token
        .context("Discord bot token not configured. Run 'neywa install' first.")?;
    let guild_id = config
        .discord_guild_id
        .context("Discord guild ID not configured. Run 'neywa install' to set it.")?;
    Ok((token, guild_id))
}

/// List all channels in the guild
pub async fn list_channels() -> Result<()> {
    let (token, guild_id) = load_token_and_guild()?;
    let client = build_client(&token);

    let url = format!("{}/guilds/{}/channels", DISCORD_API_BASE, guild_id);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Discord API error ({}): {}", status, body);
    }

    let mut channels: Vec<Channel> = response.json().await?;
    channels.sort_by(|a, b| a.position.cmp(&b.position));

    // Group by category
    let categories: Vec<&Channel> = channels
        .iter()
        .filter(|c| c.channel_type == 4)
        .collect();

    let uncategorized: Vec<&Channel> = channels
        .iter()
        .filter(|c| c.channel_type != 4 && c.parent_id.is_none())
        .collect();

    if !uncategorized.is_empty() {
        for ch in &uncategorized {
            println!(
                "  #{:<20} {:>6}  ({})",
                ch.name.as_deref().unwrap_or("?"),
                ch.id,
                ch.type_name()
            );
        }
    }

    for cat in &categories {
        println!(
            "\n📁 {}",
            cat.name.as_deref().unwrap_or("?")
        );
        let children: Vec<&Channel> = channels
            .iter()
            .filter(|c| c.parent_id.as_deref() == Some(&cat.id) && c.channel_type != 4)
            .collect();
        for ch in children {
            println!(
                "  #{:<20} {:>6}  ({})",
                ch.name.as_deref().unwrap_or("?"),
                ch.id,
                ch.type_name()
            );
        }
    }

    Ok(())
}

/// Send a message to a channel (by name or ID)
pub async fn send_message(channel: &str, message: &str) -> Result<()> {
    let (token, guild_id) = load_token_and_guild()?;
    let client = build_client(&token);

    // Resolve channel: try as ID first, then search by name
    let channel_id = if channel.parse::<u64>().is_ok() {
        channel.to_string()
    } else {
        // Strip leading # if present
        let name = channel.strip_prefix('#').unwrap_or(channel);
        resolve_channel_by_name(&client, guild_id, name).await?
    };

    let url = format!("{}/channels/{}/messages", DISCORD_API_BASE, channel_id);
    let body = serde_json::json!({ "content": message });

    let response = client.post(&url).json(&body).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Failed to send message ({}): {}", status, body);
    }

    println!("Message sent to channel {}", channel_id);
    Ok(())
}

/// Show guild info
pub async fn show_guild() -> Result<()> {
    let (token, guild_id) = load_token_and_guild()?;
    let client = build_client(&token);

    let url = format!(
        "{}/guilds/{}?with_counts=true",
        DISCORD_API_BASE, guild_id
    );
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Discord API error ({}): {}", status, body);
    }

    let guild: serde_json::Value = response.json().await?;

    println!("Server: {}", guild["name"].as_str().unwrap_or("?"));
    println!("ID: {}", guild_id);
    if let Some(count) = guild["approximate_member_count"].as_u64() {
        println!("Members: {}", count);
    }
    if let Some(desc) = guild["description"].as_str() {
        if !desc.is_empty() {
            println!("Description: {}", desc);
        }
    }

    Ok(())
}

/// Create a new channel in the guild
pub async fn create_channel(
    name: &str,
    channel_type: &str,
    category: Option<&str>,
    topic: Option<&str>,
) -> Result<()> {
    let (token, guild_id) = load_token_and_guild()?;
    let client = build_client(&token);

    // Map type string to Discord channel type number
    let type_num: u8 = match channel_type.to_lowercase().as_str() {
        "text" => 0,
        "voice" => 2,
        "category" => 4,
        "announcement" | "news" => 5,
        "forum" => 15,
        _ => anyhow::bail!(
            "Unknown channel type '{}'. Use: text, voice, category, announcement, forum",
            channel_type
        ),
    };

    let mut body = serde_json::json!({
        "name": name,
        "type": type_num,
    });

    // Resolve parent category if provided
    if let Some(cat) = category {
        let parent_id = if cat.parse::<u64>().is_ok() {
            cat.to_string()
        } else {
            // Find category by name
            let url = format!("{}/guilds/{}/channels", DISCORD_API_BASE, guild_id);
            let response = client.get(&url).send().await?;
            let channels: Vec<Channel> = response.json().await?;
            let lower_cat = cat.to_lowercase();
            channels
                .iter()
                .find(|c| c.channel_type == 4 && c.name.as_ref().map(|n| n.to_lowercase() == lower_cat).unwrap_or(false))
                .map(|c| c.id.clone())
                .context(format!("Category '{}' not found", cat))?
        };
        body["parent_id"] = serde_json::Value::String(parent_id);
    }

    if let Some(t) = topic {
        body["topic"] = serde_json::Value::String(t.to_string());
    }

    let url = format!("{}/guilds/{}/channels", DISCORD_API_BASE, guild_id);
    let response = client.post(&url).json(&body).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Failed to create channel ({}): {}", status, body);
    }

    let created: Channel = response.json().await?;
    println!(
        "Channel created: #{} (ID: {}, type: {})",
        created.name.as_deref().unwrap_or(name),
        created.id,
        created.type_name()
    );
    Ok(())
}

/// Delete a channel from the guild
pub async fn delete_channel(channel: &str) -> Result<()> {
    let (token, guild_id) = load_token_and_guild()?;
    let client = build_client(&token);

    // Resolve channel name to ID
    let channel_id = if channel.parse::<u64>().is_ok() {
        channel.to_string()
    } else {
        let name = channel.strip_prefix('#').unwrap_or(channel);
        resolve_channel_by_name(&client, guild_id, name).await?
    };

    let url = format!("{}/channels/{}", DISCORD_API_BASE, channel_id);
    let response = client.delete(&url).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Failed to delete channel ({}): {}", status, body);
    }

    println!("Channel deleted: {}", channel);
    Ok(())
}

/// Move a channel to a different category
pub async fn move_channel(channel: &str, category: &str) -> Result<()> {
    let (token, guild_id) = load_token_and_guild()?;
    let client = build_client(&token);

    // Resolve channel
    let channel_id = if channel.parse::<u64>().is_ok() {
        channel.to_string()
    } else {
        let name = channel.strip_prefix('#').unwrap_or(channel);
        resolve_channel_by_name(&client, guild_id, name).await?
    };

    // Resolve category
    let category_id = if category.parse::<u64>().is_ok() {
        category.to_string()
    } else {
        let url = format!("{}/guilds/{}/channels", DISCORD_API_BASE, guild_id);
        let response = client.get(&url).send().await?;
        let channels: Vec<Channel> = response.json().await?;
        let lower_cat = category.to_lowercase();
        channels
            .iter()
            .find(|c| c.channel_type == 4 && c.name.as_ref().map(|n| n.to_lowercase() == lower_cat).unwrap_or(false))
            .map(|c| c.id.clone())
            .context(format!("Category '{}' not found", category))?
    };

    let url = format!("{}/channels/{}", DISCORD_API_BASE, channel_id);
    let body = serde_json::json!({ "parent_id": category_id });
    let response = client.patch(&url).json(&body).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Failed to move channel ({}): {}", status, body);
    }

    println!("Channel '{}' moved to category '{}'", channel, category);
    Ok(())
}

/// Rename a channel
pub async fn rename_channel(channel_id: &str, new_name: &str) -> Result<()> {
    let (token, _guild_id) = load_token_and_guild()?;
    let client = build_client(&token);

    let url = format!("{}/channels/{}", DISCORD_API_BASE, channel_id);
    let body = serde_json::json!({ "name": new_name });
    let response = client.patch(&url).json(&body).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Failed to rename channel ({}): {}", status, body);
    }

    Ok(())
}

/// Resolve channel name to ID
async fn resolve_channel_by_name(
    client: &reqwest::Client,
    guild_id: u64,
    name: &str,
) -> Result<String> {
    let url = format!("{}/guilds/{}/channels", DISCORD_API_BASE, guild_id);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        anyhow::bail!("Failed to fetch channels");
    }

    let channels: Vec<Channel> = response.json().await?;
    let lower_name = name.to_lowercase();

    channels
        .iter()
        .find(|c| {
            c.name
                .as_ref()
                .map(|n| n.to_lowercase() == lower_name)
                .unwrap_or(false)
        })
        .map(|c| c.id.clone())
        .context(format!("Channel '{}' not found", name))
}
