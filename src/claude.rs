use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Find CLI binary in common locations
fn find_cli(name: &str) -> Option<PathBuf> {
    // First try which
    if let Ok(path) = which::which(name) {
        return Some(path);
    }

    // Common installation paths
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".local/bin").join(name),
        home.join(".cargo/bin").join(name),
        home.join("bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
        PathBuf::from("/opt/homebrew/bin").join(name),
        home.join(".npm-global/bin").join(name),
    ];

    for path in candidates {
        if path.exists() && path.is_file() {
            return Some(path);
        }
    }

    None
}

/// Common args for all Claude Code calls
fn base_command(use_z: bool) -> Command {
    let cli_name = if use_z { "claude-z" } else { "claude" };

    // Try to find the full path
    let cmd_path = find_cli(cli_name)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| cli_name.to_string());

    let mut cmd = Command::new(&cmd_path);
    cmd.arg("--dangerously-skip-permissions");
    cmd
}

/// Check which CLI to use
fn get_cli_name(use_z: bool) -> &'static str {
    if use_z { "claude-z" } else { "claude" }
}

/// Find and verify CLI exists
fn verify_cli(use_z: bool) -> Result<PathBuf> {
    let cli_name = get_cli_name(use_z);
    find_cli(cli_name).context(format!(
        "{} CLI not found. Searched in ~/.local/bin, ~/.cargo/bin, /usr/local/bin, /opt/homebrew/bin",
        cli_name
    ))
}

/// Format tool input for display
fn format_tool_input(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "Read" => {
            input.get("file_path")
                .and_then(|v| v.as_str())
                .map(|p| format!("📖 {}", shorten_path(p)))
                .unwrap_or_default()
        }
        "Glob" => {
            input.get("pattern")
                .and_then(|v| v.as_str())
                .map(|p| format!("🔍 {}", p))
                .unwrap_or_default()
        }
        "Grep" => {
            input.get("pattern")
                .and_then(|v| v.as_str())
                .map(|p| format!("🔎 {}", p))
                .unwrap_or_default()
        }
        "Bash" => {
            input.get("command")
                .and_then(|v| v.as_str())
                .map(|c| {
                    let short: String = c.chars().take(50).collect();
                    format!("💻 {}", short)
                })
                .unwrap_or_default()
        }
        "Edit" | "Write" => {
            input.get("file_path")
                .and_then(|v| v.as_str())
                .map(|p| format!("✏️ {}", shorten_path(p)))
                .unwrap_or_default()
        }
        "WebSearch" => {
            input.get("query")
                .and_then(|v| v.as_str())
                .map(|q| format!("🌐 {}", q))
                .unwrap_or_default()
        }
        "WebFetch" => {
            input.get("url")
                .and_then(|v| v.as_str())
                .map(|u| format!("📥 {}", u))
                .unwrap_or_default()
        }
        _ => String::new()
    }
}

/// Shorten file path for display
fn shorten_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Stream event from Claude Code
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Text content update (final response)
    Text(String),
    /// Session ID received
    SessionId(String),
    /// Tool being used (name, brief description)
    ToolUse(String, String),
    /// Processing complete
    Done,
    /// Error occurred
    Error(String),
}

/// Run Claude Code with streaming output
/// Returns a receiver for stream events
pub async fn run_streaming(
    message: &str,
    session_id: Option<&str>,
    use_z: bool,
) -> Result<mpsc::Receiver<StreamEvent>> {
    let cli_path = verify_cli(use_z)?;
    let cli_name = cli_path.to_string_lossy();

    let (tx, rx) = mpsc::channel(100);

    let mut cmd = base_command(use_z);

    if let Some(sid) = session_id {
        cmd.arg("--resume").arg(sid);
    }

    cmd.arg("--verbose")
        .arg("--output-format")
        .arg("stream-json")
        .arg(message)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context(format!("Failed to spawn {}", cli_name))?;

    let stdout = child.stdout.take().context("Failed to get stdout")?;

    // Spawn task to read streaming output
    tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut full_text = String::new();
        let mut session_id_sent = false;

        while let Ok(Some(line)) = lines.next_line().await {
            // Parse JSON line
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                // Extract session_id if present
                if !session_id_sent {
                    if let Some(sid) = json.get("session_id").and_then(|v| v.as_str()) {
                        let _ = tx.send(StreamEvent::SessionId(sid.to_string())).await;
                        session_id_sent = true;
                    }
                }

                // Handle different event types
                if let Some(event_type) = json.get("type").and_then(|v| v.as_str()) {
                    match event_type {
                        "assistant" => {
                            // Assistant message content
                            if let Some(message) = json.get("message") {
                                if let Some(content) = message.get("content") {
                                    if let Some(arr) = content.as_array() {
                                        for item in arr {
                                            // Check for tool_use
                                            if let Some(item_type) = item.get("type").and_then(|v| v.as_str()) {
                                                if item_type == "tool_use" {
                                                    let tool_name = item.get("name")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("unknown");
                                                    let input_str = item.get("input")
                                                        .map(|v| format_tool_input(tool_name, v))
                                                        .unwrap_or_default();
                                                    let _ = tx.send(StreamEvent::ToolUse(
                                                        tool_name.to_string(),
                                                        input_str,
                                                    )).await;
                                                } else if item_type == "text" {
                                                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                                        // 텍스트 누적 (여러 assistant 이벤트에서 오는 텍스트 합치기)
                                                        if !full_text.is_empty() {
                                                            full_text.push_str("\n");
                                                        }
                                                        full_text.push_str(text);
                                                        let _ = tx.send(StreamEvent::Text(full_text.clone())).await;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        "result" => {
                            // Final result - use result if available, otherwise keep accumulated text
                            if let Some(result) = json.get("result").and_then(|v| v.as_str()) {
                                if !result.is_empty() {
                                    full_text = result.to_string();
                                    let _ = tx.send(StreamEvent::Text(full_text.clone())).await;
                                }
                            }
                            let _ = tx.send(StreamEvent::Done).await;
                        }
                        _ => {}
                    }
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

/// Run a message through Claude Code and return the response (non-streaming)
pub async fn run(message: &str, use_z: bool) -> Result<String> {
    let cli_path = verify_cli(use_z)?;
    let cli_name = cli_path.to_string_lossy();

    tracing::debug!("Sending to {}: {}", cli_name, message);

    let output = base_command(use_z)
        .arg("--print")
        .arg(message)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context(format!("Failed to execute {}", cli_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{} error: {}", cli_name, stderr);
    }

    let response = String::from_utf8(output.stdout)
        .context(format!("Invalid UTF-8 in {} response", cli_name))?
        .trim()
        .to_string();

    tracing::debug!("{} response: {}", cli_name, response);

    Ok(response)
}

/// Run Claude Code with a specific session (for continuing conversations)
pub async fn run_with_session(message: &str, session_id: &str, use_z: bool) -> Result<String> {
    let cli_path = verify_cli(use_z)?;
    let cli_name = cli_path.to_string_lossy();

    tracing::debug!(
        "Sending to {} (session {}): {}",
        cli_name,
        session_id,
        message
    );

    let output = base_command(use_z)
        .arg("--resume")
        .arg(session_id)
        .arg("--print")
        .arg(message)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context(format!("Failed to execute {}", cli_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{} error: {}", cli_name, stderr);
    }

    let response = String::from_utf8(output.stdout)
        .context(format!("Invalid UTF-8 in {} response", cli_name))?
        .trim()
        .to_string();

    Ok(response)
}

/// Run Claude Code and get JSON output (includes session_id for later resume)
pub async fn run_json(message: &str, use_z: bool) -> Result<ClaudeResponse> {
    let cli_path = verify_cli(use_z)?;
    let cli_name = cli_path.to_string_lossy();

    let output = base_command(use_z)
        .arg("--print")
        .arg("--output-format")
        .arg("json")
        .arg(message)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context(format!("Failed to execute {}", cli_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{} error: {}", cli_name, stderr);
    }

    let response: ClaudeResponse = serde_json::from_slice(&output.stdout)
        .context(format!("Failed to parse {} JSON response", cli_name))?;

    Ok(response)
}

#[derive(Debug, serde::Deserialize)]
pub struct ClaudeResponse {
    pub session_id: String,
    pub result: String,
    #[serde(default)]
    pub cost_usd: Option<f64>,
}
