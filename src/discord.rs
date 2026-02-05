use crate::claude::{self, StreamEvent};
use crate::config::Config;
use anyhow::{Context, Result};
use regex::Regex;
use serenity::async_trait;
use serenity::builder::{CreateAttachment, CreateCommand, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, EditMessage};
use serenity::model::application::Interaction;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

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

/// Queued message for processing
#[derive(Clone)]
struct QueuedMessage {
    msg: Message,
    content: String,
    attachment_paths: Vec<String>,
    channel_type: ChannelType,
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

/// Message queue per channel
struct MessageQueue;
impl TypeMapKey for MessageQueue {
    type Value = Arc<RwLock<HashMap<u64, VecDeque<QueuedMessage>>>>;
}

/// Currently processing channels with cancellation tokens
struct ProcessingChannels;
impl TypeMapKey for ProcessingChannels {
    type Value = Arc<RwLock<HashMap<u64, CancellationToken>>>;
}

struct Handler;

impl Handler {
    async fn process_message(
        ctx: &serenity::client::Context,
        queued: QueuedMessage,
        cancel_token: CancellationToken,
    ) {
        let msg = &queued.msg;
        let content = &queued.content;
        let attachment_paths = &queued.attachment_paths;
        let channel_type = &queued.channel_type;

        let user_id = msg.author.id.get();
        let channel_id = msg.channel_id.get();
        let session_key = (user_id, channel_id);
        let user_mention = msg.author.mention().to_string();

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
                let _ = status_msg.delete(&ctx.http).await;
                return;
            }
        };

        // Process stream events with cancellation support
        let mut final_text = String::new();
        let mut new_session_id: Option<String> = None;
        let mut status_lines: Vec<String> = vec!["⏳ 처리 중...".to_string()];
        let mut last_update = Instant::now();
        let update_interval = Duration::from_millis(800);
        let mut was_cancelled = false;

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    was_cancelled = true;
                    tracing::info!("Processing cancelled for channel {}", channel_id);
                    break;
                }
                event = rx.recv() => {
                    match event {
                        Some(StreamEvent::ToolUse(tool_name, detail)) => {
                            let status = if detail.is_empty() {
                                format!("🔧 {}", tool_name)
                            } else {
                                detail
                            };
                            status_lines.push(status);
                            if status_lines.len() > 5 {
                                status_lines.remove(0);
                            }
                            if last_update.elapsed() >= update_interval {
                                let status_text = status_lines.join("\n");
                                let _ = edit_message(ctx, &status_msg, &status_text).await;
                                last_update = Instant::now();
                            }
                        }
                        Some(StreamEvent::Text(text)) => {
                            final_text = text;
                        }
                        Some(StreamEvent::SessionId(sid)) => {
                            new_session_id = Some(sid);
                        }
                        Some(StreamEvent::Done) | None => {
                            break;
                        }
                        Some(StreamEvent::Error(e)) => {
                            let _ = msg.channel_id.say(&ctx.http, format!("❌ Error: {}", e)).await;
                            let _ = status_msg.delete(&ctx.http).await;
                            return;
                        }
                    }
                }
            }
        }

        // Delete status message
        let _ = status_msg.delete(&ctx.http).await;

        if was_cancelled {
            let _ = msg.channel_id.say(&ctx.http, "🛑 중단되었습니다.").await;
            return;
        }

        // Save session ID
        if let Some(sid) = new_session_id {
            let data = ctx.data.read().await;
            if let Some(sessions) = data.get::<SessionStorage>() {
                sessions.write().await.insert(session_key, sid);
            }
        }

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

        // Send completion notification
        let completion_msg = if sent_files.is_empty() {
            format!("{} ✅ 완료!", user_mention)
        } else {
            format!("{} ✅ 완료! ({}개 파일 첨부)", user_mention, sent_files.len())
        };
        let _ = msg.channel_id.say(&ctx.http, completion_msg).await;

        // Log activity
        log_activity(ctx, &msg.author.name, channel_type, content, &final_text).await;
    }

    async fn process_queue(ctx: serenity::client::Context, channel_id: u64) {
        loop {
            // Get next message from queue
            let next_msg = {
                let data = ctx.data.read().await;
                if let Some(queue) = data.get::<MessageQueue>() {
                    queue.write().await.get_mut(&channel_id).and_then(|q| q.pop_front())
                } else {
                    None
                }
            };

            match next_msg {
                Some(queued) => {
                    // Create new cancellation token for this message
                    let cancel_token = CancellationToken::new();

                    // Store the token
                    {
                        let data = ctx.data.read().await;
                        if let Some(processing) = data.get::<ProcessingChannels>() {
                            processing.write().await.insert(channel_id, cancel_token.clone());
                        }
                    }

                    // Process the message
                    Self::process_message(&ctx, queued, cancel_token).await;

                    // Remove from processing
                    {
                        let data = ctx.data.read().await;
                        if let Some(processing) = data.get::<ProcessingChannels>() {
                            processing.write().await.remove(&channel_id);
                        }
                    }
                }
                None => {
                    // Queue is empty, exit the loop
                    break;
                }
            }
        }
    }
}

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

        let content = msg.content.trim().to_string();
        let channel_id = msg.channel_id.get();
        let user_id = msg.author.id.get();
        let session_key = (user_id, channel_id);

        // Download attachments if any
        let mut attachment_paths: Vec<String> = Vec::new();
        for attachment in &msg.attachments {
            if let Ok(path) = download_attachment(&attachment.url, &attachment.filename).await {
                attachment_paths.push(path);
            }
        }

        // Handle commands first (these don't go to queue)

        // Handle stop command
        if content == "!stop" || content == "!중단" {
            let data = ctx.data.read().await;

            // Cancel current processing
            if let Some(processing) = data.get::<ProcessingChannels>() {
                if let Some(token) = processing.read().await.get(&channel_id) {
                    token.cancel();
                    let _ = msg.channel_id.say(&ctx.http, "🛑 중단 요청됨...").await;
                } else {
                    let _ = msg.channel_id.say(&ctx.http, "현재 진행 중인 작업이 없습니다.").await;
                }
            }

            // Clear queue for this channel
            if let Some(queue) = data.get::<MessageQueue>() {
                let cleared = {
                    let mut q = queue.write().await;
                    if let Some(channel_queue) = q.get_mut(&channel_id) {
                        let count = channel_queue.len();
                        channel_queue.clear();
                        count
                    } else {
                        0
                    }
                };
                if cleared > 0 {
                    let _ = msg.channel_id.say(&ctx.http, format!("📭 대기 중인 메시지 {}개 취소됨", cleared)).await;
                }
            }
            return;
        }

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
            let is_processing = if let Some(processing) = data.get::<ProcessingChannels>() {
                processing.read().await.contains_key(&channel_id)
            } else {
                false
            };
            let queue_size = if let Some(queue) = data.get::<MessageQueue>() {
                queue.read().await.get(&channel_id).map(|q| q.len()).unwrap_or(0)
            } else {
                0
            };

            let mode = if is_z_mode { "⚡ Z 모드 (claude-z)" } else { "🤖 일반 모드 (claude)" };
            let processing_status = if is_processing { "🔄 처리 중" } else { "✅ 대기" };
            let queue_status = if queue_size > 0 { format!("📬 대기열: {}개", queue_size) } else { "📭 대기열: 비어있음".to_string() };

            let _ = msg.channel_id.say(&ctx.http, format!("{}\n{}\n{}", mode, processing_status, queue_status)).await;
            return;
        }

        // Handle queue status command
        if content == "!queue" || content == "!대기열" {
            let data = ctx.data.read().await;
            let queue_size = if let Some(queue) = data.get::<MessageQueue>() {
                queue.read().await.get(&channel_id).map(|q| q.len()).unwrap_or(0)
            } else {
                0
            };
            let is_processing = if let Some(processing) = data.get::<ProcessingChannels>() {
                processing.read().await.contains_key(&channel_id)
            } else {
                false
            };

            let status = if is_processing {
                format!("🔄 현재 처리 중 | 📬 대기열: {}개", queue_size)
            } else if queue_size > 0 {
                format!("📬 대기열: {}개", queue_size)
            } else {
                "📭 대기열이 비어있습니다.".to_string()
            };
            let _ = msg.channel_id.say(&ctx.http, status).await;
            return;
        }

        // Handle update command
        if content == "!update" {
            let _ = msg.channel_id.say(&ctx.http, "🔄 Updating Neywa...").await;

            match self_update().await {
                Ok(()) => {
                    let _ = msg.channel_id.say(&ctx.http, "✅ Update downloaded. Restarting...").await;

                    // Give Discord a moment to send the message
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                    // Spawn new daemon process (fully detached via nohup)
                    let exe_path = std::env::current_exe().unwrap_or_else(|_| "neywa".into());
                    let cmd = format!(
                        "nohup \"{}\" daemon > /dev/null 2>&1 &",
                        exe_path.display()
                    );

                    match std::process::Command::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .spawn()
                    {
                        Ok(_) => {
                            tracing::info!("Spawned new daemon via nohup, exiting...");
                        }
                        Err(e) => {
                            tracing::error!("Failed to spawn new daemon: {}", e);
                        }
                    }

                    // Give shell time to start the process
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                    // Exit current process
                    std::process::exit(0);
                }
                Err(e) => {
                    let _ = msg.channel_id.say(&ctx.http, format!("❌ Update failed: {}", e)).await;
                }
            }
            return;
        }

        // Skip if empty content and no attachments
        if content.is_empty() && attachment_paths.is_empty() {
            return;
        }

        tracing::info!("Message from {} in {:?}: {}", msg.author.name, channel_type, content);

        // Create queued message
        let queued = QueuedMessage {
            msg: msg.clone(),
            content,
            attachment_paths,
            channel_type,
        };

        // Check if channel is currently processing
        let is_processing = {
            let data = ctx.data.read().await;
            if let Some(processing) = data.get::<ProcessingChannels>() {
                processing.read().await.contains_key(&channel_id)
            } else {
                false
            }
        };

        if is_processing {
            // Add to queue
            let queue_pos = {
                let data = ctx.data.read().await;
                if let Some(queue) = data.get::<MessageQueue>() {
                    let mut q = queue.write().await;
                    let channel_queue = q.entry(channel_id).or_insert_with(VecDeque::new);
                    channel_queue.push_back(queued);
                    channel_queue.len()
                } else {
                    0
                }
            };
            let _ = msg.channel_id.say(&ctx.http, format!("📬 대기열에 추가됨 ({}번째)", queue_pos)).await;
        } else {
            // Start processing immediately
            let cancel_token = CancellationToken::new();

            // Mark as processing
            {
                let data = ctx.data.read().await;
                if let Some(processing) = data.get::<ProcessingChannels>() {
                    processing.write().await.insert(channel_id, cancel_token.clone());
                }
            }

            // Spawn processing task
            let ctx_clone = ctx.clone();
            tokio::spawn(async move {
                // Process current message
                Self::process_message(&ctx_clone, queued, cancel_token).await;

                // Remove from processing
                {
                    let data = ctx_clone.data.read().await;
                    if let Some(processing) = data.get::<ProcessingChannels>() {
                        processing.write().await.remove(&channel_id);
                    }
                }

                // Process remaining queue
                Self::process_queue(ctx_clone, channel_id).await;
            });
        }
    }

    async fn ready(&self, ctx: serenity::client::Context, ready: Ready) {
        tracing::info!("{} is connected!", ready.user.name);

        // Register slash commands globally
        let longtext_command = CreateCommand::new("longtext")
            .description("Get a link to paste long text (over 2000 chars)");

        if let Err(e) = serenity::model::application::Command::create_global_command(&ctx.http, longtext_command).await {
            tracing::error!("Failed to create slash command: {}", e);
        } else {
            tracing::info!("Registered /longtext slash command");
        }

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

    async fn interaction_create(&self, ctx: serenity::client::Context, interaction: Interaction) {
        if let Interaction::Command(command) = interaction {
            if command.data.name == "longtext" {
                let response_msg = "📝 **Long Text Input**\n\n\
                    Discord has a 2000 character limit.\n\
                    Use this tool to send longer text:\n\n\
                    👉 **https://copy-once.cc**\n\n\
                    1. Paste your long text there\n\
                    2. Copy the generated link\n\
                    3. Paste the link here with your message";

                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(response_msg)
                        .ephemeral(true)
                );

                if let Err(e) = command.create_response(&ctx.http, response).await {
                    tracing::error!("Failed to respond to slash command: {}", e);
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

    let extensions = r"\.(png|jpg|jpeg|gif|webp|pdf|txt|md|rs|py|js|ts|json|csv|zip|tar|gz|mp3|mp4|wav|mov)";

    let abs_re = Regex::new(&format!(r"(/[\w\-\./]+{})", extensions)).unwrap();
    for cap in abs_re.captures_iter(text) {
        if let Some(path) = cap.get(1) {
            let p = path.as_str().to_string();
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    }

    let home_re = Regex::new(&format!(r"(~/[\w\-\./]+{})", extensions)).unwrap();
    for cap in home_re.captures_iter(text) {
        if let Some(path) = cap.get(1) {
            let p = path.as_str();
            if let Some(home) = dirs::home_dir() {
                let expanded = p.replacen("~", &home.to_string_lossy(), 1);
                if !paths.contains(&expanded) {
                    paths.push(expanded);
                }
            }
        }
    }

    let rel_re = Regex::new(&format!(r"([\w\-]+/[\w\-\./]+{})", extensions)).unwrap();
    for cap in rel_re.captures_iter(text) {
        if let Some(path) = cap.get(1) {
            let p = path.as_str();
            if p.starts_with('/') || p.starts_with("Users/") || p.starts_with("home/") || p.starts_with("tmp/") {
                continue;
            }
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
            if line.len() > MAX_LEN {
                let chars: Vec<char> = line.chars().collect();
                let mut i = 0;
                while i < chars.len() {
                    let end = std::cmp::min(i + MAX_LEN, chars.len());
                    chunks.push(chars[i..end].iter().collect());
                    i = end;
                }
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
            let truncated_req: String = request.chars().take(100).collect();
            let truncated_req = if request.chars().count() > 100 {
                format!("{}...", truncated_req)
            } else {
                truncated_req
            };
            let truncated_resp: String = response.chars().take(200).collect();
            let truncated_resp = if response.chars().count() > 200 {
                format!("{}...", truncated_resp)
            } else {
                truncated_resp
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
        data.insert::<MessageQueue>(Arc::new(RwLock::new(HashMap::new())));
        data.insert::<ProcessingChannels>(Arc::new(RwLock::new(HashMap::new())));
    }

    client.start().await.context("Discord client error")?;

    Ok(())
}

/// Self-update neywa binary from neywa.pages.dev
async fn self_update() -> Result<()> {
    // Detect architecture
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        anyhow::bail!("Unsupported architecture");
    };

    let download_url = format!("https://neywa.pages.dev/neywa-{}", arch);
    tracing::info!("Downloading from: {}", download_url);

    // Download new binary
    let response = reqwest::get(&download_url).await?;

    if !response.status().is_success() {
        anyhow::bail!("Failed to download: HTTP {}", response.status());
    }

    let bytes = response.bytes().await?;

    // Find current binary path
    let current_exe = std::env::current_exe()
        .context("Failed to get current executable path")?;

    tracing::info!("Updating binary at: {:?}", current_exe);

    // Write to temp file first
    let temp_path = current_exe.with_extension("new");
    std::fs::write(&temp_path, &bytes)
        .context("Failed to write new binary")?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&temp_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&temp_path, perms)?;
    }

    // Replace current binary
    std::fs::rename(&temp_path, &current_exe)
        .context("Failed to replace binary")?;

    tracing::info!("Binary updated successfully");

    Ok(())
}
