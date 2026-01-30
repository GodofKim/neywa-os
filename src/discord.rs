use crate::claude::{self, StreamEvent};
use crate::config::Config;
use anyhow::{Context, Result};
use regex::Regex;
use serenity::async_trait;
use serenity::builder::{CreateAttachment, CreateMessage, EditMessage};
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Channel types based on name
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelType {
    General,
    Code,
    Research,
    Tasks,
    Logs,
    Unknown,
}

impl ChannelType {
    fn from_name(name: &str) -> Self {
        match name.to_lowercase().as_str() {
            "general" | "일반" => ChannelType::General,
            "code" | "코드" | "coding" => ChannelType::Code,
            "research" | "리서치" | "검색" => ChannelType::Research,
            "tasks" | "태스크" | "할일" | "스케줄" => ChannelType::Tasks,
            "logs" | "로그" => ChannelType::Logs,
            _ => ChannelType::Unknown,
        }
    }

    fn get_system_prompt(&self) -> &'static str {
        match self {
            ChannelType::General => {
                "You are Neywa, a helpful AI assistant. Respond naturally to any request."
            }
            ChannelType::Code => {
                "You are Neywa in CODE mode. Focus on coding tasks. \
                 You have access to the user's filesystem via Claude Code. \
                 Be concise and code-focused. Show code snippets when relevant."
            }
            ChannelType::Research => {
                "You are Neywa in RESEARCH mode. Focus on finding information. \
                 Search the web, summarize findings, and provide sources. \
                 Be thorough but concise."
            }
            ChannelType::Tasks => {
                "You are Neywa in TASKS mode. Help manage schedules and tasks. \
                 When the user wants to schedule something recurring, create a cron job using: \
                 crontab -l | { cat; echo \"SCHEDULE neywa run 'COMMAND'\"; } | crontab - \
                 Replace SCHEDULE with cron syntax and COMMAND with what to do. \
                 Confirm what you've scheduled."
            }
            ChannelType::Logs => {
                "This is a logs channel. Do not respond to messages here."
            }
            ChannelType::Unknown => {
                "You are Neywa, a helpful AI assistant."
            }
        }
    }
}

type SessionKey = (u64, u64);

struct SessionStorage;
impl TypeMapKey for SessionStorage {
    type Value = Arc<RwLock<HashMap<SessionKey, String>>>;
}

struct LogsChannel;
impl TypeMapKey for LogsChannel {
    type Value = Arc<RwLock<Option<serenity::model::id::ChannelId>>>;
}

/// Channels using Z mode (claude-z instead of claude)
struct ZModeChannels;
impl TypeMapKey for ZModeChannels {
    type Value = Arc<RwLock<std::collections::HashSet<u64>>>;
}

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: serenity::client::Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let channel_type = if let Some(channel) = msg.channel_id.to_channel(&ctx.http).await.ok() {
            if let Some(guild_channel) = channel.guild() {
                ChannelType::from_name(&guild_channel.name)
            } else {
                ChannelType::General
            }
        } else {
            ChannelType::General
        };

        if channel_type == ChannelType::Logs {
            return;
        }

        let content = msg.content.trim();

        // Download attachments if any
        let mut attachment_paths: Vec<String> = Vec::new();
        for attachment in &msg.attachments {
            if let Ok(path) = download_attachment(&attachment.url, &attachment.filename).await {
                attachment_paths.push(path);
            }
        }

        // Allow empty content if there are attachments
        if content.is_empty() && attachment_paths.is_empty() {
            return;
        }

        let user_id = msg.author.id.get();
        let channel_id = msg.channel_id.get();
        let session_key = (user_id, channel_id);
        let user_mention = msg.author.mention().to_string();

        // Handle reset command
        if content == "!reset" || content == "!새대화" {
            let data = ctx.data.read().await;
            if let Some(sessions) = data.get::<SessionStorage>() {
                sessions.write().await.remove(&session_key);
            }
            let _ = msg.channel_id.say(&ctx.http, "대화가 초기화되었습니다.").await;
            return;
        }

        // Handle Z mode toggle command
        if content == "!z" {
            let data = ctx.data.read().await;
            if let Some(z_channels) = data.get::<ZModeChannels>() {
                let mut channels = z_channels.write().await;
                let is_z_mode = if channels.contains(&channel_id) {
                    channels.remove(&channel_id);
                    false
                } else {
                    channels.insert(channel_id);
                    true
                };

                // Also reset session when switching modes
                if let Some(sessions) = data.get::<SessionStorage>() {
                    sessions.write().await.remove(&session_key);
                }

                let mode_msg = if is_z_mode {
                    "⚡ **Z 모드 활성화** - 이 채널에서 `claude-z` (z.ai API) 사용"
                } else {
                    "🔄 **일반 모드** - 이 채널에서 `claude` (Anthropic API) 사용"
                };
                let _ = msg.channel_id.say(&ctx.http, mode_msg).await;
            }
            return;
        }

        // Handle status command
        if content == "!status" || content == "!상태" {
            let data = ctx.data.read().await;
            let is_z_mode = if let Some(z_channels) = data.get::<ZModeChannels>() {
                z_channels.read().await.contains(&channel_id)
            } else {
                false
            };
            let mode = if is_z_mode { "⚡ Z 모드 (claude-z)" } else { "🤖 일반 모드 (claude)" };
            let _ = msg.channel_id.say(&ctx.http, format!("현재 모드: {}", mode)).await;
            return;
        }

        tracing::info!("Message from {} in {:?}: {}", msg.author.name, channel_type, content);

        // Get existing session
        let existing_session = {
            let data = ctx.data.read().await;
            if let Some(sessions) = data.get::<SessionStorage>() {
                sessions.read().await.get(&session_key).cloned()
            } else {
                None
            }
        };

        // Send initial "processing" message
        let status_msg = match msg.channel_id.say(&ctx.http, "⏳ 처리 중...").await {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("Failed to send processing message: {}", e);
                return;
            }
        };

        // Build prompt with system context, username, and attachments
        let system_prompt = channel_type.get_system_prompt();
        let username = &msg.author.name;
        let attachment_info = if attachment_paths.is_empty() {
            String::new()
        } else {
            format!("\n\n[Attached files: {}]", attachment_paths.join(", "))
        };

        let user_content = if content.is_empty() {
            "이 파일을 분석해줘".to_string()
        } else {
            content.to_string()
        };

        let full_prompt = if existing_session.is_some() {
            // Include username in follow-up messages too
            format!("[{}]: {}{}", username, user_content, attachment_info)
        } else {
            format!(
                "[System: {} 여러 사용자가 대화할 수 있습니다. 각 메시지 앞에 [사용자이름] 형태로 누가 말하는지 표시됩니다. 사용자를 이름으로 구분해서 응답하세요.]\n\n[{}]: {}{}",
                system_prompt, username, user_content, attachment_info
            )
        };

        // Check if channel is in Z mode
        let use_z = {
            let data = ctx.data.read().await;
            if let Some(z_channels) = data.get::<ZModeChannels>() {
                z_channels.read().await.contains(&channel_id)
            } else {
                false
            }
        };

        // Run Claude Code with streaming
        let mut rx = match claude::run_streaming(&full_prompt, existing_session.as_deref(), use_z).await {
            Ok(rx) => rx,
            Err(e) => {
                let _ = msg.channel_id.say(&ctx.http, format!("❌ Error: {}", e)).await;
                return;
            }
        };

        // Process stream events
        let mut final_text = String::new();
        let mut new_session_id: Option<String> = None;
        let mut status_lines: Vec<String> = vec!["⏳ 처리 중...".to_string()];
        let mut last_update = Instant::now();
        let update_interval = Duration::from_millis(800);

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::ToolUse(tool_name, detail) => {
                    let status = if detail.is_empty() {
                        format!("🔧 {}", tool_name)
                    } else {
                        detail
                    };
                    status_lines.push(status);
                    // Keep only last 5 status lines
                    if status_lines.len() > 5 {
                        status_lines.remove(0);
                    }
                    // Update message periodically
                    if last_update.elapsed() >= update_interval {
                        let status_text = status_lines.join("\n");
                        let _ = edit_message(&ctx, &status_msg, &status_text).await;
                        last_update = Instant::now();
                    }
                }
                StreamEvent::Text(text) => {
                    final_text = text;
                }
                StreamEvent::SessionId(sid) => {
                    new_session_id = Some(sid);
                }
                StreamEvent::Done => {
                    break;
                }
                StreamEvent::Error(e) => {
                    let _ = msg.channel_id.say(&ctx.http, format!("❌ Error: {}", e)).await;
                    return;
                }
            }
        }

        // Save session ID
        if let Some(sid) = new_session_id {
            let data = ctx.data.read().await;
            if let Some(sessions) = data.get::<SessionStorage>() {
                sessions.write().await.insert(session_key, sid);
            }
        }

        // Delete status message
        let _ = status_msg.delete(&ctx.http).await;

        // Send final response
        if final_text.is_empty() {
            final_text = "(응답 없음)".to_string();
        }

        // Detect file paths in response and send as attachments
        let file_paths = extract_file_paths(&final_text);
        tracing::info!("Detected file paths: {:?}", file_paths);
        let mut sent_files: Vec<String> = Vec::new();

        for path in &file_paths {
            tracing::info!("Checking path: {} exists={}", path, Path::new(path).exists());
            if Path::new(path).exists() {
                if let Ok(attachment) = CreateAttachment::path(path).await {
                    let builder = CreateMessage::new().add_file(attachment);
                    if msg.channel_id.send_message(&ctx.http, builder).await.is_ok() {
                        sent_files.push(path.clone());
                        tracing::info!("Sent file: {}", path);
                    }
                }
            }
        }

        // Send text response
        let chunks = split_for_discord(&final_text);
        for chunk in chunks {
            let _ = msg.channel_id.say(&ctx.http, &chunk).await;
        }

        // Send completion notification (triggers push notification)
        let completion_msg = if sent_files.is_empty() {
            format!("{} ✅ 완료!", user_mention)
        } else {
            format!("{} ✅ 완료! ({}개 파일 첨부)", user_mention, sent_files.len())
        };
        let _ = msg.channel_id.say(&ctx.http, completion_msg).await;

        // Log activity
        log_activity(&ctx, &msg.author.name, &channel_type, content, &final_text).await;

    }

    async fn ready(&self, ctx: serenity::client::Context, ready: Ready) {
        tracing::info!("{} is connected!", ready.user.name);

        for guild in &ready.guilds {
            if let Ok(channels) = guild.id.channels(&ctx.http).await {
                for (id, channel) in channels {
                    if ChannelType::from_name(&channel.name) == ChannelType::Logs {
                        let data = ctx.data.read().await;
                        if let Some(logs_channel) = data.get::<LogsChannel>() {
                            *logs_channel.write().await = Some(id);
                            tracing::info!("Found logs channel: #{}", channel.name);
                        }
                        break;
                    }
                }
            }
        }
    }
}

/// Download attachment to temp directory
async fn download_attachment(url: &str, filename: &str) -> Result<String> {
    let response = reqwest::get(url).await?;
    let bytes = response.bytes().await?;

    let temp_dir = std::env::temp_dir().join("neywa_attachments");
    std::fs::create_dir_all(&temp_dir)?;

    let file_path = temp_dir.join(filename);
    std::fs::write(&file_path, &bytes)?;

    Ok(file_path.to_string_lossy().to_string())
}

/// Extract file paths from response text
fn extract_file_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();

    // Common file extensions to detect
    let extensions = r"\.(png|jpg|jpeg|gif|webp|pdf|txt|md|rs|py|js|ts|json|csv|zip|tar|gz|mp3|mp4|wav|mov)";

    // Match absolute paths like /Users/... or /home/... or /tmp/...
    let abs_re = Regex::new(&format!(r"(/[\w\-\./]+{})", extensions)).unwrap();
    for cap in abs_re.captures_iter(text) {
        if let Some(path) = cap.get(1) {
            let p = path.as_str().to_string();
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    }

    // Match ~/... paths
    let home_re = Regex::new(&format!(r"(~/[\w\-\./]+{})", extensions)).unwrap();
    for cap in home_re.captures_iter(text) {
        if let Some(path) = cap.get(1) {
            let p = path.as_str();
            // Expand ~ to home directory
            if let Some(home) = dirs::home_dir() {
                let expanded = p.replacen("~", &home.to_string_lossy(), 1);
                if !paths.contains(&expanded) {
                    paths.push(expanded);
                }
            }
        }
    }

    // Match relative paths like ./... or folder/file.ext
    let rel_re = Regex::new(&format!(r"([\w\-]+/[\w\-\./]+{})", extensions)).unwrap();
    for cap in rel_re.captures_iter(text) {
        if let Some(path) = cap.get(1) {
            let p = path.as_str();
            // Skip if it looks like part of an absolute path (already handled)
            if p.starts_with('/') || p.starts_with("Users/") || p.starts_with("home/") || p.starts_with("tmp/") {
                continue;
            }
            // Convert to absolute path using current working directory
            if let Ok(cwd) = std::env::current_dir() {
                let abs_path = cwd.join(p);
                let abs_str = abs_path.to_string_lossy().to_string();
                if !paths.contains(&abs_str) {
                    paths.push(abs_str);
                }
            }
        }
    }

    paths
}

/// Split text into chunks for Discord's 2000 char limit
fn split_for_discord(text: &str) -> Vec<String> {
    const MAX_LEN: usize = 1900;
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        if current.len() + line.len() + 1 > MAX_LEN {
            if !current.is_empty() {
                chunks.push(current);
                current = String::new();
            }
            // If single line is too long, split it
            if line.len() > MAX_LEN {
                let mut remaining = line;
                while remaining.len() > MAX_LEN {
                    chunks.push(remaining[..MAX_LEN].to_string());
                    remaining = &remaining[MAX_LEN..];
                }
                current = remaining.to_string();
            } else {
                current = line.to_string();
            }
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    if chunks.is_empty() {
        chunks.push("(응답 없음)".to_string());
    }

    chunks
}

/// Edit a message
async fn edit_message(ctx: &serenity::client::Context, msg: &Message, content: &str) -> Result<()> {
    msg.channel_id
        .edit_message(&ctx.http, msg.id, EditMessage::new().content(content))
        .await?;
    Ok(())
}

/// Log activity to logs channel
async fn log_activity(
    ctx: &serenity::client::Context,
    user: &str,
    channel_type: &ChannelType,
    request: &str,
    response: &str,
) {
    let data = ctx.data.read().await;
    if let Some(logs_channel) = data.get::<LogsChannel>() {
        if let Some(channel_id) = *logs_channel.read().await {
            let truncated_req = if request.len() > 100 {
                format!("{}...", &request[..100])
            } else {
                request.to_string()
            };
            let truncated_resp = if response.len() > 200 {
                format!("{}...", &response[..200])
            } else {
                response.to_string()
            };

            let log_msg = format!(
                "**{}** in `{:?}`\n> {}\n```\n{}\n```",
                user, channel_type, truncated_req, truncated_resp
            );

            let _ = channel_id.say(&ctx.http, log_msg).await;
        }
    }
}

pub async fn run_bot() -> Result<()> {
    let config = Config::load()?;

    let token = config
        .discord_bot_token
        .context("Discord bot token not configured. Run 'neywa install' first.")?;

    tracing::info!("Starting Discord bot...");

    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(&token, intents)
        .event_handler(Handler)
        .await
        .context("Failed to create Discord client")?;

    {
        let mut data = client.data.write().await;
        data.insert::<SessionStorage>(Arc::new(RwLock::new(HashMap::new())));
        data.insert::<LogsChannel>(Arc::new(RwLock::new(None)));
        data.insert::<ZModeChannels>(Arc::new(RwLock::new(std::collections::HashSet::new())));
    }

    client.start().await.context("Discord client error")?;

    Ok(())
}
