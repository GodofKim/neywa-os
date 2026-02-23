use crate::claude::{self, AiBackend, StreamEvent};
use crate::codex;
use crate::config::Config;
use crate::discord_api;
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
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Current version from Cargo.toml
const VERSION: &str = env!("CARGO_PKG_VERSION");

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
    is_plan_mode: bool,
}

type SessionKey = (u64, u64);

struct SessionStorage;
impl TypeMapKey for SessionStorage {
    type Value = Arc<RwLock<HashMap<SessionKey, String>>>;
}

/// Trim old messages from a Claude Code session JSONL file
/// Removes the oldest ~20% of conversation messages
/// Returns true if trimming was successful
fn trim_session_file(session_id: &str) -> bool {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return false,
    };
    let session_path = home
        .join(".claude/projects/-")
        .join(format!("{}.jsonl", session_id));

    if !session_path.exists() {
        tracing::warn!("Session file not found: {:?}", session_path);
        return false;
    }

    let content = match std::fs::read_to_string(&session_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to read session file: {}", e);
            return false;
        }
    };

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    if total < 50 {
        tracing::info!("Session too small to trim ({} lines)", total);
        return false;
    }

    // Separate system/meta lines from conversation lines
    let mut system_lines = Vec::new();
    let mut conv_lines = Vec::new();

    for line in &lines {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(line) {
            let msg_type = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if msg_type == "system" || msg_type == "queue-operation" {
                system_lines.push(*line);
            } else {
                conv_lines.push(*line);
            }
        } else {
            conv_lines.push(*line);
        }
    }

    // Keep the last 80% of conversation messages (remove oldest 20%)
    let keep_count = (conv_lines.len() as f64 * 0.8).ceil() as usize;
    let keep_count = keep_count.max(20); // At least 20 messages
    let trimmed_conv: Vec<&str> = if conv_lines.len() > keep_count {
        conv_lines[conv_lines.len() - keep_count..].to_vec()
    } else {
        conv_lines
    };

    // Rebuild file: system lines + trimmed conversation
    let mut new_lines = system_lines;
    new_lines.extend(trimmed_conv);
    let new_content = new_lines.join("\n") + "\n";

    match std::fs::write(&session_path, new_content) {
        Ok(_) => {
            tracing::info!(
                "Trimmed session {}: {} -> {} lines",
                session_id,
                total,
                new_lines.len()
            );
            true
        }
        Err(e) => {
            tracing::error!("Failed to write trimmed session: {}", e);
            false
        }
    }
}

/// Path for storing sessions
fn sessions_file_path() -> std::path::PathBuf {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("neywa");
    config_dir.join("sessions.json")
}

/// Load sessions from file
fn load_sessions() -> HashMap<SessionKey, String> {
    let path = sessions_file_path();
    if !path.exists() {
        return HashMap::new();
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            // Parse as array of [key1, key2, value] arrays
            let parsed: Result<Vec<(u64, u64, String)>, _> = serde_json::from_str(&content);
            match parsed {
                Ok(entries) => {
                    let mut map = HashMap::new();
                    for (k1, k2, v) in entries {
                        map.insert((k1, k2), v);
                    }
                    tracing::info!("Loaded {} sessions from file", map.len());
                    map
                }
                Err(e) => {
                    tracing::warn!("Failed to parse sessions file: {}", e);
                    HashMap::new()
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to read sessions file: {}", e);
            HashMap::new()
        }
    }
}

/// Save sessions to file
fn save_sessions(sessions: &HashMap<SessionKey, String>) {
    let path = sessions_file_path();

    // Ensure directory exists
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Convert to serializable format: array of [key1, key2, value]
    let entries: Vec<(u64, u64, &String)> = sessions
        .iter()
        .map(|((k1, k2), v)| (*k1, *k2, v))
        .collect();

    match serde_json::to_string_pretty(&entries) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!("Failed to save sessions: {}", e);
            }
        }
        Err(e) => {
            tracing::warn!("Failed to serialize sessions: {}", e);
        }
    }
}

struct LogsChannel;
impl TypeMapKey for LogsChannel {
    type Value = Arc<RwLock<Option<serenity::model::id::ChannelId>>>;
}

/// Per-channel AI backend selection
struct ChannelBackends;
impl TypeMapKey for ChannelBackends {
    type Value = Arc<RwLock<HashMap<u64, AiBackend>>>;
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

/// Channels in human-only mode (Neywa ignores messages)
struct HumanModeChannels;
impl TypeMapKey for HumanModeChannels {
    type Value = Arc<RwLock<std::collections::HashSet<u64>>>;
}

/// Path for storing human mode channel list
fn human_mode_file_path() -> std::path::PathBuf {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("neywa");
    config_dir.join("human_mode.json")
}

/// Load human mode channels from file
fn load_human_mode() -> std::collections::HashSet<u64> {
    let path = human_mode_file_path();
    if !path.exists() {
        return std::collections::HashSet::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => std::collections::HashSet::new(),
    }
}

/// Save human mode channels to file
fn save_human_mode(channels: &std::collections::HashSet<u64>) {
    let path = human_mode_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(channels) {
        let _ = std::fs::write(&path, json);
    }
}

/// Path for storing channel backend selections
fn channel_backends_file_path() -> std::path::PathBuf {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("neywa");
    config_dir.join("channel_backends.json")
}

/// Load channel backends from file
fn load_channel_backends() -> HashMap<u64, AiBackend> {
    let path = channel_backends_file_path();
    if !path.exists() {
        return HashMap::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// Save channel backends to file
fn save_channel_backends(backends: &HashMap<u64, AiBackend>) {
    let path = channel_backends_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(backends) {
        let _ = std::fs::write(&path, json);
    }
}

/// Helper to get the current backend for a channel
async fn get_channel_backend(ctx: &serenity::client::Context, channel_id: u64) -> AiBackend {
    let data = ctx.data.read().await;
    if let Some(backends) = data.get::<ChannelBackends>() {
        backends
            .read()
            .await
            .get(&channel_id)
            .copied()
            .unwrap_or(AiBackend::Claude)
    } else {
        AiBackend::Claude
    }
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
        let status_msg = match msg.channel_id.say(&ctx.http, "⏳ Processing...").await {
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
            "Analyze this file".to_string()
        } else {
            content.to_string()
        };

        let full_prompt = if existing_session.is_some() {
            format!("[{}]: {}{}", username, user_content, attachment_info)
        } else {
            format!(
                "[System: {} Multiple users may participate. Each message is prefixed with [username]. Distinguish users by name in your responses.]\n\n[{}]: {}{}",
                system_prompt, username, user_content, attachment_info
            )
        };

        // Get the AI backend for this channel
        let backend = get_channel_backend(ctx, channel_id).await;

        // Run AI backend with streaming (plan mode or normal)
        let mut rx = if queued.is_plan_mode {
            let use_z = backend == AiBackend::ClaudeZ;
            match claude::run_streaming_plan(&full_prompt, use_z).await {
                Ok(rx) => rx,
                Err(e) => {
                    let _ = msg.channel_id.say(&ctx.http, format!("❌ Error: {}", e)).await;
                    let _ = status_msg.delete(&ctx.http).await;
                    return;
                }
            }
        } else {
            match backend {
                AiBackend::Codex => {
                    match codex::run_streaming(&full_prompt, existing_session.as_deref()).await {
                        Ok(rx) => rx,
                        Err(e) => {
                            let _ = msg.channel_id.say(&ctx.http, format!("❌ Error: {}", e)).await;
                            let _ = status_msg.delete(&ctx.http).await;
                            return;
                        }
                    }
                }
                _ => {
                    let use_z = backend == AiBackend::ClaudeZ;
                    match claude::run_streaming(&full_prompt, existing_session.as_deref(), use_z).await {
                        Ok(rx) => rx,
                        Err(e) => {
                            let _ = msg.channel_id.say(&ctx.http, format!("❌ Error: {}", e)).await;
                            let _ = status_msg.delete(&ctx.http).await;
                            return;
                        }
                    }
                }
            }
        };

        // Process stream events with cancellation support
        let mut final_text = String::new();
        let mut new_session_id: Option<String> = None;
        let mut plan_content: Option<String> = None;
        let mut status_lines: Vec<String> = vec!["⏳ Processing...".to_string()];
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
                        Some(StreamEvent::PlanContent(_path, content)) => {
                            // Keep the longest plan content (may get multiple events)
                            if plan_content.as_ref().map_or(true, |existing| content.len() > existing.len()) {
                                plan_content = Some(content);
                            }
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
            let _ = msg.channel_id.say(&ctx.http, "🛑 Cancelled.").await;
            return;
        }

        // Handle plan mode response separately
        if queued.is_plan_mode {
            // Use plan_content if text response is empty (common due to ExitPlanMode denial)
            let response_text = if !final_text.is_empty() && !final_text.trim().is_empty() {
                // If we have both, prefer the longer/more complete one
                if let Some(ref plan) = plan_content {
                    if plan.len() > final_text.len() { plan.clone() } else { final_text.clone() }
                } else {
                    final_text.clone()
                }
            } else {
                plan_content.unwrap_or_else(|| "(No plan generated)".to_string())
            };

            let full_response = format!("📐 **Plan**\n\n{}", response_text);
            let chunks = split_for_discord(&full_response);
            for chunk in chunks {
                let _ = msg.channel_id.say(&ctx.http, &chunk).await;
            }

            let _ = msg.channel_id.say(&ctx.http, format!("{} ✅ Plan ready!", user_mention)).await;
            log_activity(ctx, &msg.author.name, channel_type, content, &response_text).await;
            return;
        }

        // Save session ID (memory + file)
        if let Some(ref sid) = new_session_id {
            let data = ctx.data.read().await;
            if let Some(sessions) = data.get::<SessionStorage>() {
                let mut sessions_map = sessions.write().await;
                sessions_map.insert(session_key, sid.clone());
                // Persist to file
                save_sessions(&sessions_map);
            }
        }

        // Send final response
        if final_text.is_empty() {
            final_text = "(No response)".to_string();
        }

        // Auto-compact session if prompt is too long (context window exceeded)
        let lower_text = final_text.to_lowercase();
        if lower_text.contains("prompt is too long") || lower_text.contains("context window") || lower_text.contains("too many tokens") {
            let session_to_compact = new_session_id.as_deref().or(existing_session.as_deref());
            if let Some(sid) = session_to_compact {
                // Codex mode: can't compact, reset session
                if backend == AiBackend::Codex {
                    let data = ctx.data.read().await;
                    if let Some(sessions) = data.get::<SessionStorage>() {
                        let mut sessions_map = sessions.write().await;
                        sessions_map.remove(&session_key);
                        save_sessions(&sessions_map);
                    }
                    let _ = msg.channel_id.say(&ctx.http, "⚠️ Context window exceeded. 새 세션으로 시작합니다. 메시지를 다시 보내주세요.").await;
                    return;
                }

                let use_z = backend == AiBackend::ClaudeZ;
                let _ = msg.channel_id.say(&ctx.http, "⚠️ Context window full. Compacting session...").await;

                // Run /compact on the session
                match claude::compact_session(sid, use_z).await {
                    Ok(_) => {
                        let _ = msg.channel_id.say(&ctx.http, "✅ Session compacted. Retrying your message...").await;

                        // Retry the original message with the compacted session
                        match claude::run_streaming(&full_prompt, Some(sid), use_z).await {
                            Ok(mut retry_rx) => {
                                let mut retry_text = String::new();
                                while let Some(event) = retry_rx.recv().await {
                                    match event {
                                        StreamEvent::Text(t) => retry_text.push_str(&t),
                                        StreamEvent::Done => break,
                                        _ => {}
                                    }
                                }
                                if !retry_text.is_empty() {
                                    final_text = retry_text;
                                    // Fall through to normal response handling below
                                } else {
                                    let _ = msg.channel_id.say(&ctx.http, "⚠️ Compact succeeded but retry got empty response. Please send your message again.").await;
                                    return;
                                }
                            }
                            Err(e) => {
                                let _ = msg.channel_id.say(&ctx.http, format!("⚠️ Compact succeeded but retry failed: {}. Please send your message again.", e)).await;
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        // Compact failed, try trimming as fallback
                        tracing::warn!("Compact failed: {}, trying trim fallback", e);
                        let trimmed = trim_session_file(sid);
                        if trimmed {
                            let _ = msg.channel_id.say(&ctx.http, "⚠️ Compact failed. Trimmed old messages instead. Please send your message again.").await;
                        } else {
                            let data = ctx.data.read().await;
                            if let Some(sessions) = data.get::<SessionStorage>() {
                                let mut sessions_map = sessions.write().await;
                                sessions_map.remove(&session_key);
                                save_sessions(&sessions_map);
                            }
                            let _ = msg.channel_id.say(&ctx.http, "⚠️ Context window exceeded. Session has been reset. Please send your message again.").await;
                        }
                        return;
                    }
                }
            } else {
                let _ = msg.channel_id.say(&ctx.http, "⚠️ Context window exceeded. Please start a new session with !new.").await;
                return;
            }
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
            format!("{} ✅ Done!", user_mention)
        } else {
            format!("{} ✅ Done! ({} file(s) attached)", user_mention, sent_files.len())
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

        // Allow !human command even in human mode (to toggle it off)
        // But block all other messages if human mode is active
        if content != "!human" && content != "!인간" {
            let is_human_mode = {
                let data = ctx.data.read().await;
                if let Some(human_channels) = data.get::<HumanModeChannels>() {
                    human_channels.read().await.contains(&channel_id)
                } else {
                    false
                }
            };
            if is_human_mode {
                return;
            }
        }
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

        // Handle help command
        if content == "!help" || content == "!도움" {
            let help_text = format!(
                "**Neywa v{}** - AI Assistant\n\n\
                **Commands** (slash `/` or text `!`):\n\
                `help` - Show this help\n\
                `status` - Check session status\n\
                `new` - Start a new conversation\n\
                `stop` - Stop processing & clear queue\n\
                `queue` - Show queued messages\n\
                `compact` - Compact session context window\n\
                `update` - Update to latest version\n\
                `longtext` - How to send long text\n\
                `slash <cmd>` - Run Claude Code slash command\n\n\
                **Text-only Commands:**\n\
                `!plan <msg>` - Generate a plan without executing (read-only)\n\
                `!z` - Toggle Z mode (claude-z)\n\
                `!codex` - Toggle Codex mode (OpenAI Codex CLI)\n\
                `!human` - Toggle human-only mode (Neywa stops responding)\n\
                `!run <cmd>` - Execute terminal command directly\n\
                `!restart` - Reset all Claude sessions (fixes MCP/connection issues)\n\n\
                Just type a message to chat with AI.",
                VERSION
            );
            let _ = msg.channel_id.say(&ctx.http, help_text).await;
            return;
        }

        // Handle stop command
        if content == "!stop" || content == "!중단" {
            let data = ctx.data.read().await;

            // Cancel current processing
            if let Some(processing) = data.get::<ProcessingChannels>() {
                if let Some(token) = processing.read().await.get(&channel_id) {
                    token.cancel();
                    let _ = msg.channel_id.say(&ctx.http, "🛑 Stop requested...").await;
                } else {
                    let _ = msg.channel_id.say(&ctx.http, "Nothing is being processed.").await;
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
                    let _ = msg.channel_id.say(&ctx.http, format!("📭 Cleared {} queued message(s)", cleared)).await;
                }
            }
            return;
        }

        // Handle reset command
        if content == "!reset" || content == "!새대화" {
            let data = ctx.data.read().await;
            if let Some(sessions) = data.get::<SessionStorage>() {
                let mut sessions_map = sessions.write().await;
                sessions_map.remove(&session_key);
                save_sessions(&sessions_map);
            }
            let _ = msg.channel_id.say(&ctx.http, "Session reset.").await;
            return;
        }

        // Handle !run command - execute terminal command directly
        if let Some(cmd) = content.strip_prefix("!run ") {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                let _ = msg.channel_id.say(&ctx.http, "Usage: `!run <command>`").await;
                return;
            }

            tracing::info!("Executing terminal command: {}", cmd);
            let _ = msg.channel_id.say(&ctx.http, format!("⏳ Running: `{}`", cmd)).await;

            // Run command in spawn_blocking to avoid blocking the async runtime
            let cmd_owned = cmd.to_string();
            let output = tokio::task::spawn_blocking(move || {
                Command::new("bash")
                    .arg("-c")
                    .arg(&cmd_owned)
                    .output()
            }).await;

            let response = match output {
                Ok(Ok(output)) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let exit_code = output.status.code().unwrap_or(-1);

                    let mut result = String::new();
                    if !stdout.is_empty() {
                        result.push_str(&format!("**stdout:**\n```\n{}\n```", stdout));
                    }
                    if !stderr.is_empty() {
                        if !result.is_empty() { result.push_str("\n"); }
                        result.push_str(&format!("**stderr:**\n```\n{}\n```", stderr));
                    }
                    result.push_str(&format!("\n*Exit code: {}*", exit_code));

                    if result.is_empty() {
                        format!("✅ Done (exit code: {})", exit_code)
                    } else {
                        result
                    }
                }
                Ok(Err(e)) => format!("❌ Failed to execute: {}", e),
                Err(e) => format!("❌ Task error: {}", e),
            };

            // Discord has 2000 char limit, truncate if needed
            let response = if response.len() > 1950 {
                format!("{}...\n*(truncated)*", &response[..1900])
            } else {
                response
            };

            let _ = msg.channel_id.say(&ctx.http, response).await;
            return;
        }

        // Handle Z mode toggle command
        if content == "!z" {
            let data = ctx.data.read().await;
            if let Some(backends) = data.get::<ChannelBackends>() {
                let mut map = backends.write().await;
                let current = map.get(&channel_id).copied().unwrap_or(AiBackend::Claude);

                let is_z_mode = if current == AiBackend::ClaudeZ {
                    map.remove(&channel_id);
                    false
                } else {
                    map.insert(channel_id, AiBackend::ClaudeZ);
                    true
                };
                save_channel_backends(&map);

                if let Some(sessions) = data.get::<SessionStorage>() {
                    let mut sessions_map = sessions.write().await;
                    sessions_map.remove(&session_key);
                    save_sessions(&sessions_map);
                }

                let mode_msg = if is_z_mode {
                    "⚡ **Z mode ON** - Using `claude-z` (z.ai API) in this channel"
                } else {
                    "🔄 **Normal mode** - Using `claude` (Anthropic API) in this channel"
                };
                let _ = msg.channel_id.say(&ctx.http, mode_msg).await;
            }
            return;
        }

        // Handle Codex mode toggle command
        if content == "!codex" {
            // Check if codex CLI is available
            if claude::find_cli("codex").is_none() {
                let _ = msg.channel_id.say(&ctx.http, "❌ codex CLI not found. Install: `npm install -g @openai/codex`").await;
                return;
            }

            let channel_name = if let Ok(channel) = msg.channel_id.to_channel(&ctx.http).await {
                channel.guild().map(|gc| gc.name.clone())
            } else {
                None
            };

            let data = ctx.data.read().await;
            if let Some(backends) = data.get::<ChannelBackends>() {
                let mut map = backends.write().await;
                let current = map.get(&channel_id).copied().unwrap_or(AiBackend::Claude);

                let is_codex = if current == AiBackend::Codex {
                    // Turn OFF codex mode
                    map.remove(&channel_id);

                    // Remove 🅾️ emoji from channel name
                    if let Some(name) = &channel_name {
                        let new_name = name.trim_start_matches("🅾️").trim_start_matches('-').to_string();
                        let new_name = if new_name.is_empty() { name.clone() } else { new_name };
                        tokio::spawn({
                            let channel_id_str = channel_id.to_string();
                            async move {
                                if let Err(e) = discord_api::rename_channel(&channel_id_str, &new_name).await {
                                    tracing::warn!("Failed to rename channel: {}", e);
                                }
                            }
                        });
                    }
                    false
                } else {
                    // Turn ON codex mode
                    map.insert(channel_id, AiBackend::Codex);

                    // Add 🅾️ emoji to channel name
                    if let Some(name) = &channel_name {
                        // Remove any existing mode emoji first
                        let clean_name = name.trim_start_matches("🅾️").trim_start_matches('-').to_string();
                        let new_name = format!("🅾️{}", clean_name);
                        tokio::spawn({
                            let channel_id_str = channel_id.to_string();
                            async move {
                                if let Err(e) = discord_api::rename_channel(&channel_id_str, &new_name).await {
                                    tracing::warn!("Failed to rename channel: {}", e);
                                }
                            }
                        });
                    }
                    true
                };
                save_channel_backends(&map);

                // Reset session on mode change
                if let Some(sessions) = data.get::<SessionStorage>() {
                    let mut sessions_map = sessions.write().await;
                    sessions_map.remove(&session_key);
                    save_sessions(&sessions_map);
                }

                let mode_msg = if is_codex {
                    "🅾️ **Codex mode ON** - Using OpenAI Codex CLI in this channel"
                } else {
                    "🔄 **Normal mode** - Using `claude` (Anthropic API) in this channel"
                };
                let _ = msg.channel_id.say(&ctx.http, mode_msg).await;
            }
            return;
        }

        // Handle human mode toggle
        if content == "!human" || content == "!인간" {
            let channel_name = if let Ok(channel) = msg.channel_id.to_channel(&ctx.http).await {
                channel.guild().map(|gc| gc.name.clone())
            } else {
                None
            };

            let data = ctx.data.read().await;
            if let Some(human_channels) = data.get::<HumanModeChannels>() {
                let mut channels = human_channels.write().await;
                let is_human_mode = if channels.contains(&channel_id) {
                    // Turn OFF human mode
                    channels.remove(&channel_id);
                    save_human_mode(&channels);

                    // Remove emoji from channel name
                    if let Some(name) = &channel_name {
                        let new_name = name.trim_start_matches("🙋‍♂️").trim_start_matches('-').to_string();
                        let new_name = if new_name.is_empty() { name.clone() } else { new_name };
                        tokio::spawn({
                            let channel_id_str = channel_id.to_string();
                            async move {
                                if let Err(e) = discord_api::rename_channel(&channel_id_str, &new_name).await {
                                    tracing::warn!("Failed to rename channel: {}", e);
                                }
                            }
                        });
                    }

                    false
                } else {
                    // Turn ON human mode
                    channels.insert(channel_id);
                    save_human_mode(&channels);

                    // Add emoji to channel name
                    if let Some(name) = &channel_name {
                        let new_name = format!("🙋‍♂️{}", name);
                        tokio::spawn({
                            let channel_id_str = channel_id.to_string();
                            async move {
                                if let Err(e) = discord_api::rename_channel(&channel_id_str, &new_name).await {
                                    tracing::warn!("Failed to rename channel: {}", e);
                                }
                            }
                        });
                    }

                    true
                };

                let mode_msg = if is_human_mode {
                    "🙋‍♂️ **Human mode ON** - Neywa will not respond in this channel.\nType `!human` again to turn off."
                } else {
                    "🤖 **Human mode OFF** - Neywa is back online in this channel."
                };
                let _ = msg.channel_id.say(&ctx.http, mode_msg).await;
            }
            return;
        }

        // Handle status command
        if content == "!status" || content == "!상태" {
            let data = ctx.data.read().await;
            let backend = if let Some(backends) = data.get::<ChannelBackends>() {
                backends.read().await.get(&channel_id).copied().unwrap_or(AiBackend::Claude)
            } else {
                AiBackend::Claude
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

            let mode = backend.status_line();
            let processing_status = if is_processing { "🔄 Processing" } else { "✅ Idle" };
            let queue_status = if queue_size > 0 { format!("📬 Queue: {}", queue_size) } else { "📭 Queue: empty".to_string() };

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
                format!("🔄 Processing | 📬 Queue: {}", queue_size)
            } else if queue_size > 0 {
                format!("📬 Queue: {}", queue_size)
            } else {
                "📭 Queue is empty.".to_string()
            };
            let _ = msg.channel_id.say(&ctx.http, status).await;
            return;
        }

        // Handle compact command
        if content == "!compact" {
            let current_backend = get_channel_backend(&ctx, channel_id).await;
            if current_backend == AiBackend::Codex {
                let _ = msg.channel_id.say(&ctx.http, "⚠️ Codex 모드에서는 compact를 지원하지 않습니다. `!new`로 새 세션을 시작하세요.").await;
                return;
            }

            let existing_session = {
                let data = ctx.data.read().await;
                if let Some(sessions) = data.get::<SessionStorage>() {
                    sessions.read().await.get(&session_key).cloned()
                } else {
                    None
                }
            };

            if let Some(sid) = existing_session {
                let _ = msg.channel_id.say(&ctx.http, "🗜️ Compacting session...").await;

                let use_z = current_backend == AiBackend::ClaudeZ;

                match claude::compact_session(&sid, use_z).await {
                    Ok(_) => {
                        let _ = msg.channel_id.say(&ctx.http, "✅ Session compacted.").await;
                    }
                    Err(e) => {
                        // Try trim as fallback
                        if trim_session_file(&sid) {
                            let _ = msg.channel_id.say(&ctx.http, "⚠️ Compact failed, trimmed old messages instead.").await;
                        } else {
                            let _ = msg.channel_id.say(&ctx.http, format!("❌ Compact failed: {}", e)).await;
                        }
                    }
                }
            } else {
                let _ = msg.channel_id.say(&ctx.http, "No active session. Nothing to compact.").await;
            }
            return;
        }

        // Handle slash command passthrough
        if content.starts_with("!slash ") {
            let current_backend = get_channel_backend(&ctx, channel_id).await;
            if current_backend == AiBackend::Codex {
                let _ = msg.channel_id.say(&ctx.http, "ℹ️ Codex 모드에서는 slash 명령을 지원하지 않습니다.").await;
                return;
            }

            let slash_cmd = content.trim_start_matches("!slash ").trim().to_string();
            if slash_cmd.is_empty() {
                let _ = msg.channel_id.say(&ctx.http, "Usage: `!slash <command>` (e.g., `!slash compact`, `!slash cost`)").await;
                return;
            }

            let existing_session = {
                let data = ctx.data.read().await;
                if let Some(sessions) = data.get::<SessionStorage>() {
                    sessions.read().await.get(&session_key).cloned()
                } else {
                    None
                }
            };

            let use_z = current_backend == AiBackend::ClaudeZ;

            let display_cmd = slash_cmd.trim_start_matches('/');
            let _ = msg.channel_id.say(&ctx.http, format!("⚡ Running `/{}`...", display_cmd)).await;

            match claude::run_slash_command(&slash_cmd, existing_session.as_deref(), use_z).await {
                Ok(result) => {
                    let chunks = split_for_discord(&result);
                    for chunk in chunks {
                        let _ = msg.channel_id.say(&ctx.http, &chunk).await;
                    }
                }
                Err(e) => {
                    let _ = msg.channel_id.say(&ctx.http, format!("❌ Error: {}", e)).await;
                }
            }
            return;
        }

        // Handle plan command - run Claude in plan-only mode
        if content.starts_with("!plan ") || content.starts_with("!계획 ") {
            let current_backend = get_channel_backend(&ctx, channel_id).await;
            if current_backend == AiBackend::Codex {
                let _ = msg.channel_id.say(&ctx.http, "⚠️ Codex 모드에서는 plan mode를 지원하지 않습니다.").await;
                return;
            }
            let plan_msg = content
                .strip_prefix("!plan ")
                .or_else(|| content.strip_prefix("!계획 "))
                .unwrap_or("")
                .trim()
                .to_string();

            if plan_msg.is_empty() {
                let _ = msg.channel_id.say(&ctx.http, "Usage: `!plan <request>`").await;
                return;
            }

            let queued = QueuedMessage {
                msg: msg.clone(),
                content: plan_msg,
                attachment_paths,
                channel_type,
                is_plan_mode: true,
            };

            // Use same queue/processing logic as normal messages
            let is_processing = {
                let data = ctx.data.read().await;
                if let Some(processing) = data.get::<ProcessingChannels>() {
                    processing.read().await.contains_key(&channel_id)
                } else {
                    false
                }
            };

            if is_processing {
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
                let _ = msg.channel_id.say(&ctx.http, format!("📬 Queued (#{} in line)", queue_pos)).await;
            } else {
                let cancel_token = CancellationToken::new();
                {
                    let data = ctx.data.read().await;
                    if let Some(processing) = data.get::<ProcessingChannels>() {
                        processing.write().await.insert(channel_id, cancel_token.clone());
                    }
                }

                let ctx_clone = ctx.clone();
                tokio::spawn(async move {
                    Self::process_message(&ctx_clone, queued, cancel_token).await;
                    {
                        let data = ctx_clone.data.read().await;
                        if let Some(processing) = data.get::<ProcessingChannels>() {
                            processing.write().await.remove(&channel_id);
                        }
                    }
                    Self::process_queue(ctx_clone, channel_id).await;
                });
            }
            return;
        }

        // Handle restart command - kills all Claude Code sessions and resets state
        if content == "!restart" || content == "!재시작" {
            let _ = msg.channel_id.say(&ctx.http, "🔄 Restarting all sessions...").await;

            let data = ctx.data.read().await;
            let mut cancelled_count = 0u32;
            let mut cleared_count = 0u32;

            // 1. Cancel all active processing (triggers CancellationToken)
            if let Some(processing) = data.get::<ProcessingChannels>() {
                let tokens = processing.read().await;
                for (_ch, token) in tokens.iter() {
                    token.cancel();
                    cancelled_count += 1;
                }
            }

            // 2. Clear all message queues
            if let Some(queue) = data.get::<MessageQueue>() {
                let mut q = queue.write().await;
                for (_ch, channel_queue) in q.iter_mut() {
                    cleared_count += channel_queue.len() as u32;
                    channel_queue.clear();
                }
            }

            // 3. Clear all session IDs (forces fresh Claude Code sessions)
            if let Some(sessions) = data.get::<SessionStorage>() {
                let mut sessions_map = sessions.write().await;
                sessions_map.clear();
                save_sessions(&sessions_map);
            }

            drop(data);

            // 4. Kill any lingering claude/claude-z/codex child processes
            let _ = Command::new("pkill")
                .arg("-f")
                .arg("claude.*--dangerously-skip-permissions")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();

            let _ = Command::new("pkill")
                .arg("-f")
                .arg("codex exec")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();

            // Brief wait for processes to clean up
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            let _ = msg.channel_id.say(&ctx.http, format!(
                "✅ Sessions restarted.\n\
                 • Cancelled {} active task(s)\n\
                 • Cleared {} queued message(s)\n\
                 • All session history reset\n\
                 • Claude Code processes terminated\n\n\
                 Ready for new messages!",
                cancelled_count, cleared_count
            )).await;
            return;
        }

        // Handle update command
        if content == "!update" {
            let _ = msg.channel_id.say(&ctx.http, "🔄 Checking for updates...").await;

            // Fetch remote version
            let remote_version = match fetch_remote_version().await {
                Ok(v) => v,
                Err(e) => {
                    let _ = msg.channel_id.say(&ctx.http, format!("❌ Failed to check version: {}", e)).await;
                    return;
                }
            };

            // Compare versions
            if remote_version == VERSION {
                let _ = msg.channel_id.say(&ctx.http, format!("✅ Already on the latest version (v{})", VERSION)).await;
                return;
            }

            let _ = msg.channel_id.say(&ctx.http, format!("📥 New version available: v{} → v{}", VERSION, remote_version)).await;

            match self_update().await {
                Ok(()) => {
                    // Save pending update info for notification after restart
                    if let Err(e) = save_update_pending(msg.channel_id.get(), VERSION, &remote_version) {
                        tracing::warn!("Failed to save update pending info: {}", e);
                    }

                    let _ = msg.channel_id.say(&ctx.http, "✅ Update downloaded. Restarting...").await;
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                    restart_after_update();
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
            is_plan_mode: false,
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
            let _ = msg.channel_id.say(&ctx.http, format!("📬 Queued (#{} in line)", queue_pos)).await;
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

        // Check for pending update notification
        if let Some((channel_id, old_version, new_version)) = load_update_pending() {
            tracing::info!(
                "Pending update: {} -> {} (running: v{})",
                old_version, new_version, VERSION
            );

            let http = ctx.http.clone();
            tokio::spawn(async move {
                // Wait for Discord connection to stabilize
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

                let channel = serenity::model::id::ChannelId::new(channel_id);
                let msg = if VERSION == new_version {
                    format!("🎉 **Update complete!** v{} → v{}", old_version, new_version)
                } else {
                    format!(
                        "⚠️ Update done. Expected v{}, running v{}",
                        new_version, VERSION
                    )
                };

                tracing::info!("Sending update notification to channel {}...", channel_id);
                match channel.say(&http, &msg).await {
                    Ok(_) => tracing::info!("Update notification sent"),
                    Err(e) => tracing::error!("Failed to send update notification: {}", e),
                }
            });
        }

        // Register slash commands globally
        let command_defs: Vec<(&str, &str)> = vec![
            ("help", "Show available commands"),
            ("status", "Check session status, processing state, queue"),
            ("new", "Start a new conversation session"),
            ("stop", "Stop current processing and clear queue"),
            ("queue", "Show queued messages"),
            ("compact", "Compact session context window"),
            ("update", "Self-update to latest version"),
            ("longtext", "Get a link to paste long text (over 2000 chars)"),
        ];

        for (name, desc) in &command_defs {
            let cmd = CreateCommand::new(*name).description(*desc);
            if let Err(e) = serenity::model::application::Command::create_global_command(&ctx.http, cmd).await {
                tracing::error!("Failed to register /{}: {}", name, e);
            }
        }

        // Register /slash with a required string option
        {
            use serenity::model::application::CommandOptionType;
            let slash_cmd = CreateCommand::new("slash")
                .description("Run a Claude Code slash command")
                .add_option(
                    serenity::builder::CreateCommandOption::new(
                        CommandOptionType::String,
                        "command",
                        "The slash command to run (e.g., compact, cost, doctor)",
                    )
                    .required(true),
                );
            if let Err(e) = serenity::model::application::Command::create_global_command(&ctx.http, slash_cmd).await {
                tracing::error!("Failed to register /slash: {}", e);
            }
        }

        tracing::info!("Registered {} slash commands", command_defs.len() + 1);

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
            let channel_id = command.channel_id.get();
            let user_id = command.user.id.get();
            let session_key = (user_id, channel_id);

            let response_msg = match command.data.name.as_str() {
                "help" => {
                    format!(
                        "**Neywa v{}** - AI Assistant\n\n\
                        **Commands** (slash `/` or text `!`):\n\
                        `help` - Show this help\n\
                        `status` - Check session status\n\
                        `new` - Start a new conversation\n\
                        `stop` - Stop processing & clear queue\n\
                        `queue` - Show queued messages\n\
                        `compact` - Compact session context window\n\
                        `update` - Update to latest version\n\
                        `longtext` - How to send long text\n\
                        `slash <cmd>` - Run Claude Code slash command\n\n\
                        **Text-only Commands:**\n\
                        `!plan <msg>` - Generate a plan without executing (read-only)\n\
                        `!z` - Toggle Z mode (claude-z)\n\
                        `!codex` - Toggle Codex mode (OpenAI Codex CLI)\n\
                        `!human` - Toggle human-only mode (Neywa stops responding)\n\
                        `!restart` - Reset all Claude sessions (fixes MCP/connection issues)\n\n\
                        Just type a message to chat with AI.",
                        VERSION
                    )
                }
                "status" => {
                    let data = ctx.data.read().await;
                    let backend = if let Some(backends) = data.get::<ChannelBackends>() {
                        backends.read().await.get(&channel_id).copied().unwrap_or(AiBackend::Claude)
                    } else { AiBackend::Claude };
                    let is_processing = if let Some(processing) = data.get::<ProcessingChannels>() {
                        processing.read().await.contains_key(&channel_id)
                    } else { false };
                    let queue_size = if let Some(queue) = data.get::<MessageQueue>() {
                        queue.read().await.get(&channel_id).map(|q| q.len()).unwrap_or(0)
                    } else { 0 };

                    let mode = backend.status_line();
                    let proc = if is_processing { "🔄 Processing" } else { "✅ Idle" };
                    let queue = if queue_size > 0 { format!("📬 Queue: {}", queue_size) } else { "📭 Queue: empty".to_string() };
                    format!("**v{}**\n{}\n{}\n{}", VERSION, mode, proc, queue)
                }
                "new" => {
                    let data = ctx.data.read().await;
                    if let Some(sessions) = data.get::<SessionStorage>() {
                        let mut sessions_map = sessions.write().await;
                        sessions_map.remove(&session_key);
                        save_sessions(&sessions_map);
                    }
                    "🔄 New session started.".to_string()
                }
                "stop" => {
                    let data = ctx.data.read().await;
                    let mut cancelled = false;
                    let mut cleared = 0usize;

                    if let Some(processing) = data.get::<ProcessingChannels>() {
                        if let Some(token) = processing.read().await.get(&channel_id) {
                            token.cancel();
                            cancelled = true;
                        }
                    }
                    if let Some(queue) = data.get::<MessageQueue>() {
                        let mut q = queue.write().await;
                        if let Some(channel_queue) = q.get_mut(&channel_id) {
                            cleared = channel_queue.len();
                            channel_queue.clear();
                        }
                    }

                    let mut parts = Vec::new();
                    if cancelled { parts.push("🛑 Processing stopped".to_string()); }
                    if cleared > 0 { parts.push(format!("📭 {} queued message(s) cleared", cleared)); }
                    if parts.is_empty() { parts.push("Nothing to stop.".to_string()); }
                    parts.join("\n")
                }
                "queue" => {
                    let data = ctx.data.read().await;
                    let queue_size = if let Some(queue) = data.get::<MessageQueue>() {
                        queue.read().await.get(&channel_id).map(|q| q.len()).unwrap_or(0)
                    } else { 0 };
                    let is_processing = if let Some(processing) = data.get::<ProcessingChannels>() {
                        processing.read().await.contains_key(&channel_id)
                    } else { false };

                    if is_processing {
                        format!("🔄 Processing | 📬 Queue: {}", queue_size)
                    } else if queue_size > 0 {
                        format!("📬 Queue: {}", queue_size)
                    } else {
                        "📭 Queue is empty.".to_string()
                    }
                }
                "update" => {
                    // Respond immediately, then handle update asynchronously
                    let response = CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("🔄 Checking for updates...")
                    );
                    let _ = command.create_response(&ctx.http, response).await;

                    // Do the update in the channel as regular messages
                    let channel = command.channel_id;
                    let http = ctx.http.clone();
                    let data = ctx.data.clone();

                    tokio::spawn(async move {
                        let remote_version = match fetch_remote_version().await {
                            Ok(v) => v,
                            Err(e) => {
                                let _ = channel.say(&http, format!("❌ Failed to check version: {}", e)).await;
                                return;
                            }
                        };

                        if remote_version == VERSION {
                            let _ = channel.say(&http, format!("✅ Already on the latest version (v{})", VERSION)).await;
                            return;
                        }

                        let _ = channel.say(&http, format!("📥 v{} → v{}", VERSION, remote_version)).await;

                        match self_update().await {
                            Ok(()) => {
                                if let Err(e) = save_update_pending(channel.get(), VERSION, &remote_version) {
                                    tracing::warn!("Failed to save update pending: {}", e);
                                }

                                let _ = channel.say(&http, "✅ Update downloaded. Restarting...").await;
                                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                                restart_after_update();
                            }
                            Err(e) => {
                                let _ = channel.say(&http, format!("❌ Update failed: {}", e)).await;
                            }
                        }
                    });
                    return; // Already responded
                }
                "compact" => {
                    // Respond immediately, then handle async
                    let response = CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("🗜️ Compacting session...")
                    );
                    let _ = command.create_response(&ctx.http, response).await;

                    let channel = command.channel_id;
                    let http = ctx.http.clone();
                    let data_arc = ctx.data.clone();

                    let existing_session = {
                        let data = data_arc.read().await;
                        if let Some(sessions) = data.get::<SessionStorage>() {
                            sessions.read().await.get(&session_key).cloned()
                        } else {
                            None
                        }
                    };

                    let use_z = {
                        let data = data_arc.read().await;
                        if let Some(backends) = data.get::<ChannelBackends>() {
                            backends.read().await.get(&channel_id).copied() == Some(AiBackend::ClaudeZ)
                        } else {
                            false
                        }
                    };

                    tokio::spawn(async move {
                        if let Some(sid) = existing_session {
                            match claude::compact_session(&sid, use_z).await {
                                Ok(_) => {
                                    let _ = channel.say(&http, "✅ Session compacted.").await;
                                }
                                Err(e) => {
                                    if trim_session_file(&sid) {
                                        let _ = channel.say(&http, "⚠️ Compact failed, trimmed old messages instead.").await;
                                    } else {
                                        let _ = channel.say(&http, format!("❌ Compact failed: {}", e)).await;
                                    }
                                }
                            }
                        } else {
                            let _ = channel.say(&http, "No active session. Nothing to compact.").await;
                        }
                    });
                    return; // Already responded
                }
                "slash" => {
                    let slash_cmd = command.data.options.first()
                        .and_then(|opt| opt.value.as_str())
                        .unwrap_or("")
                        .to_string();

                    if slash_cmd.is_empty() {
                        "Usage: `/slash <command>` (e.g., `/slash compact`, `/slash cost`)".to_string()
                    } else {
                        // Respond immediately, then handle async
                        let display_cmd = slash_cmd.trim_start_matches('/').to_string();
                        let response = CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(format!("⚡ Running `/{}`...", display_cmd))
                        );
                        let _ = command.create_response(&ctx.http, response).await;

                        let channel = command.channel_id;
                        let http = ctx.http.clone();
                        let data_arc = ctx.data.clone();

                        let existing_session = {
                            let data = data_arc.read().await;
                            if let Some(sessions) = data.get::<SessionStorage>() {
                                sessions.read().await.get(&session_key).cloned()
                            } else {
                                None
                            }
                        };

                        let use_z = {
                            let data = data_arc.read().await;
                            if let Some(backends) = data.get::<ChannelBackends>() {
                                backends.read().await.get(&channel_id).copied() == Some(AiBackend::ClaudeZ)
                            } else {
                                false
                            }
                        };

                        tokio::spawn(async move {
                            match claude::run_slash_command(&slash_cmd, existing_session.as_deref(), use_z).await {
                                Ok(result) => {
                                    let chunks = split_for_discord(&result);
                                    for chunk in chunks {
                                        let _ = channel.say(&http, &chunk).await;
                                    }
                                }
                                Err(e) => {
                                    let _ = channel.say(&http, format!("❌ Error: {}", e)).await;
                                }
                            }
                        });
                        return; // Already responded
                    }
                }
                "longtext" => {
                    "📝 **Long Text Input**\n\n\
                    Discord has a 2000 character limit.\n\
                    Use this tool to send longer text:\n\n\
                    👉 **https://copy-once.cc**\n\n\
                    1. Paste your long text there\n\
                    2. Copy the generated link\n\
                    3. Paste the link here with your message".to_string()
                }
                _ => return,
            };

            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(response_msg)
                    .ephemeral(matches!(command.data.name.as_str(), "help" | "longtext"))
            );

            if let Err(e) = command.create_response(&ctx.http, response).await {
                tracing::error!("Failed to respond to /{}: {}", command.data.name, e);
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
        chunks.push("(No response)".to_string());
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
        // Load persisted sessions from file
        let sessions = load_sessions();
        data.insert::<SessionStorage>(Arc::new(RwLock::new(sessions)));
        data.insert::<LogsChannel>(Arc::new(RwLock::new(None)));
        data.insert::<ChannelBackends>(Arc::new(RwLock::new(load_channel_backends())));
        data.insert::<MessageQueue>(Arc::new(RwLock::new(HashMap::new())));
        data.insert::<ProcessingChannels>(Arc::new(RwLock::new(HashMap::new())));
        data.insert::<HumanModeChannels>(Arc::new(RwLock::new(load_human_mode())));
    }

    client.start().await.context("Discord client error")?;

    Ok(())
}

/// Fetch remote version from neywa.ai/version.txt
async fn fetch_remote_version() -> Result<String> {
    let url = "https://neywa.ai/version.txt";
    let response = reqwest::get(url).await?;

    if !response.status().is_success() {
        anyhow::bail!("Failed to fetch version: HTTP {}", response.status());
    }

    let version = response.text().await?.trim().to_string();
    Ok(version)
}

/// Path for storing pending update info
fn update_pending_path() -> std::path::PathBuf {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("neywa");
    config_dir.join("update_pending.json")
}

/// Save pending update info before restart
fn save_update_pending(channel_id: u64, old_version: &str, new_version: &str) -> Result<()> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("neywa");
    std::fs::create_dir_all(&config_dir)?;

    let info = serde_json::json!({
        "channel_id": channel_id,
        "old_version": old_version,
        "new_version": new_version
    });

    std::fs::write(update_pending_path(), serde_json::to_string(&info)?)?;
    Ok(())
}

/// Load and delete pending update info
fn load_update_pending() -> Option<(u64, String, String)> {
    let path = update_pending_path();
    if !path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path); // Delete after reading

    let info: serde_json::Value = serde_json::from_str(&content).ok()?;
    let channel_id = info["channel_id"].as_u64()?;
    let old_version = info["old_version"].as_str()?.to_string();
    let new_version = info["new_version"].as_str()?.to_string();

    Some((channel_id, old_version, new_version))
}

/// Self-update neywa binary from neywa.ai
async fn self_update() -> Result<()> {
    // Detect architecture
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        anyhow::bail!("Unsupported architecture");
    };

    let download_url = format!("https://neywa.ai/neywa-{}", arch);
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

    // Also update the .app bundle binary (preserves FDA permissions)
    // Skip if current_exe IS the app binary (self-copy truncates to 0 bytes!)
    let app_binary = std::path::PathBuf::from("/Applications/Neywa.app/Contents/MacOS/neywa");
    if app_binary.exists() {
        let is_same = std::fs::canonicalize(&current_exe).ok()
            == std::fs::canonicalize(&app_binary).ok();
        if !is_same {
            if let Err(e) = std::fs::copy(&current_exe, &app_binary) {
                tracing::warn!("Failed to update Neywa.app binary: {}", e);
            } else {
                tracing::info!("Updated Neywa.app binary");
            }
        } else {
            tracing::info!("Binary already at app bundle path, skipping copy");
        }
    }

    tracing::info!("Binary updated successfully");

    Ok(())
}

/// Restart Neywa after update.
/// Uses _exit(0) to bypass atexit handlers (tray cleanup etc.) that may hang.
/// LaunchAgent's KeepAlive=true will auto-restart the process within ThrottleInterval.
/// Note: exec() doesn't work on macOS because replacing the binary invalidates the
/// code signature, causing SIGKILL from the kernel.
fn restart_after_update() -> ! {
    tracing::info!("Exiting for KeepAlive restart...");

    // Safety net: if _exit somehow doesn't work, force kill after 5 seconds
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(5));
        std::process::exit(1);
    });

    // Use _exit(0) to terminate immediately without running atexit handlers.
    // This avoids potential hangs from tray/NSApplication cleanup.
    // KeepAlive=true in LaunchAgent will restart us automatically.
    extern "C" {
        fn _exit(status: i32) -> !;
    }
    unsafe { _exit(0) }
}
