use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::claude::{self, StreamEvent, NEYWA_SYSTEM_PROMPT};

/// Build the base codex command
fn base_command() -> Result<Command> {
    let cli_path = claude::find_cli("codex")
        .context("codex CLI not found. Install: npm install -g @openai/codex")?;

    let mut cmd = Command::new(cli_path);
    cmd.arg("exec")
        .arg("--model")
        .arg("gpt-5.3-codex");
    Ok(cmd)
}

/// Run Codex CLI with streaming output (JSON Lines)
/// Returns a receiver for stream events
pub async fn run_streaming(
    message: &str,
    session_id: Option<&str>,
) -> Result<mpsc::Receiver<StreamEvent>> {
    let (tx, rx) = mpsc::channel(100);

    let mut cmd = base_command()?;

    if let Some(sid) = session_id {
        cmd.arg("resume").arg(sid);
    }

    cmd.arg("--json")
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg(message)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("Failed to spawn codex")?;

    let stdout = child.stdout.take().context("Failed to get stdout")?;
    let stderr = child.stderr.take().context("Failed to get stderr")?;

    // Spawn task to read stderr in background
    let stderr_tx = tx.clone();
    tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        let mut stderr_buf = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            stderr_buf.push_str(&line);
            stderr_buf.push('\n');
        }
        if !stderr_buf.is_empty() {
            let lower = stderr_buf.to_lowercase();
            if lower.contains("context window")
                || lower.contains("too many tokens")
                || lower.contains("prompt is too long")
                || lower.contains("max_tokens")
            {
                let _ = stderr_tx
                    .send(StreamEvent::Text(
                        "Prompt is too long".to_string(),
                    ))
                    .await;
                let _ = stderr_tx.send(StreamEvent::Done).await;
            }
        }
    });

    // Spawn task to read streaming JSONL output
    tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut full_text = String::new();
        let mut session_id_sent = false;

        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match event_type {
                    // Thread started → extract session ID (thread_id)
                    "thread.started" => {
                        if !session_id_sent {
                            if let Some(tid) = json.get("thread_id").and_then(|v| v.as_str()) {
                                let _ =
                                    tx.send(StreamEvent::SessionId(tid.to_string())).await;
                                session_id_sent = true;
                            }
                        }
                    }

                    // Item started → tool use beginning
                    "item.started" => {
                        if let Some(item) = json.get("item") {
                            let item_type =
                                item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match item_type {
                                "command_execution" => {
                                    let cmd_str = item
                                        .get("command")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("...");
                                    let short: String = cmd_str.chars().take(50).collect();
                                    let _ = tx
                                        .send(StreamEvent::ToolUse(
                                            "Bash".to_string(),
                                            format!("💻 {}", short),
                                        ))
                                        .await;
                                }
                                "file_read" => {
                                    let path = item
                                        .get("file_path")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("...");
                                    let _ = tx
                                        .send(StreamEvent::ToolUse(
                                            "Read".to_string(),
                                            format!("📖 {}", claude::shorten_path(path)),
                                        ))
                                        .await;
                                }
                                _ => {}
                            }
                        }
                    }

                    // Item completed → process result
                    "item.completed" => {
                        if let Some(item) = json.get("item") {
                            let item_type =
                                item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match item_type {
                                "agent_message" => {
                                    // Accumulate text from agent messages
                                    if let Some(content) = item.get("content") {
                                        if let Some(arr) = content.as_array() {
                                            for part in arr {
                                                if let Some(text) =
                                                    part.get("text").and_then(|v| v.as_str())
                                                {
                                                    if !full_text.is_empty() {
                                                        full_text.push('\n');
                                                    }
                                                    full_text.push_str(text);
                                                }
                                            }
                                        } else if let Some(text) = content.as_str() {
                                            if !full_text.is_empty() {
                                                full_text.push('\n');
                                            }
                                            full_text.push_str(text);
                                        }
                                    }
                                    // Also check for top-level text field
                                    if let Some(text) =
                                        item.get("text").and_then(|v| v.as_str())
                                    {
                                        if !full_text.is_empty() {
                                            full_text.push('\n');
                                        }
                                        full_text.push_str(text);
                                    }
                                    if !full_text.is_empty() {
                                        let _ = tx
                                            .send(StreamEvent::Text(full_text.clone()))
                                            .await;
                                    }
                                }
                                "command_execution" => {
                                    let cmd_str = item
                                        .get("command")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("...");
                                    let short: String = cmd_str.chars().take(50).collect();
                                    let _ = tx
                                        .send(StreamEvent::ToolUse(
                                            "Bash".to_string(),
                                            format!("💻 {} ✓", short),
                                        ))
                                        .await;
                                }
                                "file_changes" => {
                                    let file = item
                                        .get("file_path")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("files");
                                    let _ = tx
                                        .send(StreamEvent::ToolUse(
                                            "Edit".to_string(),
                                            format!(
                                                "✏️ {}",
                                                claude::shorten_path(file)
                                            ),
                                        ))
                                        .await;
                                }
                                "web_searches" => {
                                    let query = item
                                        .get("query")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("search");
                                    let _ = tx
                                        .send(StreamEvent::ToolUse(
                                            "WebSearch".to_string(),
                                            format!(
                                                "🌐 {}",
                                                claude::truncate_str(query, 40)
                                            ),
                                        ))
                                        .await;
                                }
                                "mcp_tool_calls" => {
                                    let tool = item
                                        .get("tool_name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("tool");
                                    let _ = tx
                                        .send(StreamEvent::ToolUse(
                                            "MCP".to_string(),
                                            format!("🔌 {}", tool),
                                        ))
                                        .await;
                                }
                                "reasoning" => {
                                    // Internal reasoning - ignore
                                }
                                _ => {}
                            }
                        }
                    }

                    // Turn completed
                    "turn.completed" => {
                        let _ = tx.send(StreamEvent::Done).await;
                    }

                    // Turn failed
                    "turn.failed" => {
                        let error_msg = json
                            .get("error")
                            .and_then(|v| v.as_str())
                            .or_else(|| json.get("message").and_then(|v| v.as_str()))
                            .unwrap_or("Unknown error");
                        let _ = tx
                            .send(StreamEvent::Error(error_msg.to_string()))
                            .await;
                    }

                    _ => {}
                }
            }
        }

        // Wait for process to complete
        let _ = child.wait().await;

        // Send done if not already sent
        let _ = tx.send(StreamEvent::Done).await;
    });

    Ok(rx)
}

/// Build a prompt with system instructions injected (for first message only)
pub fn build_prompt_with_system(
    message: &str,
    channel_system_prompt: &str,
    username: &str,
    is_first_message: bool,
) -> String {
    if is_first_message {
        format!(
            "[SYSTEM INSTRUCTIONS]\n{}\n{}\n[END SYSTEM INSTRUCTIONS]\n\n[{}]: {}",
            NEYWA_SYSTEM_PROMPT, channel_system_prompt, username, message
        )
    } else {
        format!("[{}]: {}", username, message)
    }
}

/// Run Codex CLI and return the response (non-streaming)
pub async fn run(message: &str) -> Result<String> {
    let cli_path = claude::find_cli("codex")
        .context("codex CLI not found. Install: npm install -g @openai/codex")?;

    tracing::debug!("Sending to codex: {}", message);

    let output = Command::new(cli_path)
        .arg("exec")
        .arg("--model")
        .arg("gpt-5.3-codex")
        .arg("--json")
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg(message)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to execute codex")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("codex error: {}", stderr);
    }

    // Parse JSONL output - collect all agent_message text
    let stdout = String::from_utf8(output.stdout)
        .context("Invalid UTF-8 in codex response")?;

    let mut result_text = String::new();
    for line in stdout.lines() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if event_type == "item.completed" {
                if let Some(item) = json.get("item") {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if item_type == "agent_message" {
                        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                            if !result_text.is_empty() {
                                result_text.push('\n');
                            }
                            result_text.push_str(text);
                        }
                        if let Some(content) = item.get("content") {
                            if let Some(arr) = content.as_array() {
                                for part in arr {
                                    if let Some(text) =
                                        part.get("text").and_then(|v| v.as_str())
                                    {
                                        if !result_text.is_empty() {
                                            result_text.push('\n');
                                        }
                                        result_text.push_str(text);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(result_text.trim().to_string())
}
