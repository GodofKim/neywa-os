use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// System prompt for plan mode - injected into plan-only Claude Code calls
const NEYWA_PLAN_SYSTEM_PROMPT: &str = r#"
## Neywa Plan Mode Guidelines

You are running through Neywa in PLAN MODE, a Discord-based AI assistant interface.
This is a non-interactive environment. You CANNOT get user input during execution.

### Important Rules:
- Explore the codebase thoroughly using Read, Glob, Grep, and Bash (read-only commands)
- Write your complete plan to the plan file
- Your text response will be sent to the user in Discord
- Keep your text response as a concise summary of the plan
- Write detailed implementation steps in the plan file, put a summary in your text response
"#;

/// Neywa system prompt - injected into all Claude Code calls
const NEYWA_SYSTEM_PROMPT: &str = r#"
## Neywa System Guidelines

You are running through Neywa, a Discord-based AI assistant interface.

### Long-Running Processes

When starting servers, daemons, or any process that should persist after this conversation ends, you MUST properly detach the process:

```bash
# Method 1: nohup + disown (recommended)
nohup command > /path/to/log 2>&1 & disown

# Method 2: screen
screen -dmS session_name command

# Method 3: tmux
tmux new-session -d -s session_name 'command'

# Method 4: pm2 (for Node.js)
pm2 start app.js --name myapp
```

IMPORTANT: Never start a server or daemon with just `command &` - it will be killed when this session ends.
After starting a persistent process, verify it's running with `ps aux | grep process_name` or `curl localhost:port`.

### Response Style

- Keep responses concise - this is a chat interface
- Use code blocks for commands and code
- When showing file changes, prefer diffs or key snippets over full files

### Discord Server Control

You can control the Discord server directly using `neywa discord` commands via Bash:

```bash
# List all channels in the server
neywa discord channels

# Send a message to a channel (by name or ID)
neywa discord send general 'Hello!'
neywa discord send logs 'Task completed'
neywa discord send 1234567890 'Message by channel ID'

# Show server info
neywa discord guild

# Create a new text channel
neywa discord create my-channel
neywa discord create my-channel -t text

# Create a voice channel
neywa discord create voice-room -t voice

# Create a channel under a category
neywa discord create dev-logs -c 'Development'

# Create a channel with topic
neywa discord create announcements -t announcement --topic 'Important updates'

# Create a category
neywa discord create 'My Category' -t category

# Move a channel to a different category
neywa discord move dev-logs 'Development'
neywa discord move 1234567890 'Archive'

# Delete a channel (by name or ID)
neywa discord delete old-channel
neywa discord delete 1234567890
```

Use these commands proactively when needed:
- Send status updates or results to relevant channels
- Check available channels before sending
- Create channels when organizing new projects or workflows
- Use the logs channel for activity logging
"#;

/// Find CLI binary in common locations
fn find_cli(name: &str) -> Option<PathBuf> {
    // First try which
    if let Ok(path) = which::which(name) {
        return Some(path);
    }

    // Common installation paths
    let home = dirs::home_dir()?;
    let mut candidates = vec![
        home.join(".local/bin").join(name),
        home.join(".cargo/bin").join(name),
        home.join("bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
        PathBuf::from("/opt/homebrew/bin").join(name),
        home.join(".npm-global/bin").join(name),
    ];

    // Add nvm paths (LaunchAgent doesn't inherit shell PATH)
    let nvm_dir = home.join(".nvm/versions/node");
    if nvm_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&nvm_dir) {
            for entry in entries.flatten() {
                let bin_path = entry.path().join("bin").join(name);
                candidates.push(bin_path);
            }
        }
    }

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
    cmd.arg("--append-system-prompt").arg(NEYWA_SYSTEM_PROMPT);
    cmd
}

/// Command for plan mode (no --dangerously-skip-permissions, uses --permission-mode plan)
fn plan_command(use_z: bool) -> Command {
    let cli_name = if use_z { "claude-z" } else { "claude" };

    let cmd_path = find_cli(cli_name)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| cli_name.to_string());

    let mut cmd = Command::new(&cmd_path);
    cmd.arg("--permission-mode").arg("plan");
    cmd.arg("--append-system-prompt").arg(NEYWA_PLAN_SYSTEM_PROMPT);
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
        // Advanced tools
        "Task" => {
            let agent_type = input.get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("agent");
            let description = input.get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("🤖 [{}] {}", agent_type, description)
        }
        "Skill" => {
            let skill = input.get("skill")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            format!("⚡ /{}", skill)
        }
        "NotebookEdit" => {
            input.get("notebook_path")
                .and_then(|v| v.as_str())
                .map(|p| format!("📓 {}", shorten_path(p)))
                .unwrap_or_default()
        }
        "AskUserQuestion" => {
            "❓ Asking user...".to_string()
        }
        "TaskCreate" => {
            input.get("subject")
                .and_then(|v| v.as_str())
                .map(|s| format!("📝 New: {}", truncate_str(s, 40)))
                .unwrap_or_else(|| "📝 Creating task".to_string())
        }
        "TaskUpdate" => {
            let task_id = input.get("taskId")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let status = input.get("status")
                .and_then(|v| v.as_str())
                .map(|s| format!(" → {}", s))
                .unwrap_or_default();
            format!("📋 Task #{}{}", task_id, status)
        }
        "TaskList" => {
            "📋 Listing tasks".to_string()
        }
        "TaskGet" => {
            input.get("taskId")
                .and_then(|v| v.as_str())
                .map(|id| format!("📋 Get task #{}", id))
                .unwrap_or_else(|| "📋 Getting task".to_string())
        }
        "EnterPlanMode" => {
            "📐 Entering plan mode".to_string()
        }
        "ExitPlanMode" => {
            "📐 Exiting plan mode".to_string()
        }
        "TaskOutput" => {
            input.get("task_id")
                .and_then(|v| v.as_str())
                .map(|id| format!("📤 Reading output: {}", id))
                .unwrap_or_else(|| "📤 Reading task output".to_string())
        }
        "TaskStop" => {
            "🛑 Stopping task".to_string()
        }
        _ => {
            // Handle MCP tools (mcp__server__tool format)
            if tool_name.starts_with("mcp__") {
                let parts: Vec<&str> = tool_name.split("__").collect();
                if parts.len() >= 3 {
                    let server = parts[1];
                    let tool = parts[2];
                    return format!("🔌 {}:{}", server, tool);
                }
            }
            String::new()
        }
    }
}

/// Truncate string for display
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len - 3).collect();
        format!("{}...", truncated)
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
    /// Plan file written (file_path, content)
    PlanContent(String, String),
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
            if lower.contains("prompt is too long") || lower.contains("context window") || lower.contains("too many tokens") {
                let _ = stderr_tx.send(StreamEvent::Text("Prompt is too long".to_string())).await;
                let _ = stderr_tx.send(StreamEvent::Done).await;
            }
        }
    });

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

/// Run Claude Code in plan mode with streaming output
/// Uses --permission-mode plan instead of --dangerously-skip-permissions
pub async fn run_streaming_plan(
    message: &str,
    use_z: bool,
) -> Result<mpsc::Receiver<StreamEvent>> {
    let cli_path = verify_cli(use_z)?;
    let cli_name = cli_path.to_string_lossy();

    let (tx, rx) = mpsc::channel(100);

    let mut cmd = plan_command(use_z);

    cmd.arg("--verbose")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--print")
        .arg(message)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context(format!("Failed to spawn {} (plan mode)", cli_name))?;

    let stdout = child.stdout.take().context("Failed to get stdout")?;
    let stderr = child.stderr.take().context("Failed to get stderr")?;

    // Spawn stderr reader
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
            if lower.contains("prompt is too long") || lower.contains("context window") || lower.contains("too many tokens") {
                let _ = stderr_tx.send(StreamEvent::Text("Prompt is too long".to_string())).await;
                let _ = stderr_tx.send(StreamEvent::Done).await;
            }
        }
    });

    // Spawn stdout reader - enhanced to capture plan file writes
    tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut full_text = String::new();
        let mut session_id_sent = false;

        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                // Extract session_id
                if !session_id_sent {
                    if let Some(sid) = json.get("session_id").and_then(|v| v.as_str()) {
                        let _ = tx.send(StreamEvent::SessionId(sid.to_string())).await;
                        session_id_sent = true;
                    }
                }

                if let Some(event_type) = json.get("type").and_then(|v| v.as_str()) {
                    match event_type {
                        "assistant" => {
                            if let Some(message) = json.get("message") {
                                if let Some(content) = message.get("content") {
                                    if let Some(arr) = content.as_array() {
                                        for item in arr {
                                            if let Some(item_type) = item.get("type").and_then(|v| v.as_str()) {
                                                if item_type == "tool_use" {
                                                    let tool_name = item.get("name")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("unknown");

                                                    // Capture Write to plan file
                                                    if tool_name == "Write" {
                                                        if let Some(input) = item.get("input") {
                                                            let file_path = input.get("file_path")
                                                                .and_then(|v| v.as_str())
                                                                .unwrap_or("");
                                                            if file_path.contains("/.claude/plans/") {
                                                                let plan_content = input.get("content")
                                                                    .and_then(|v| v.as_str())
                                                                    .unwrap_or("");
                                                                if !plan_content.is_empty() {
                                                                    let _ = tx.send(StreamEvent::PlanContent(
                                                                        file_path.to_string(),
                                                                        plan_content.to_string(),
                                                                    )).await;
                                                                }
                                                            }
                                                        }
                                                    }

                                                    // Capture ExitPlanMode plan content as fallback
                                                    if tool_name == "ExitPlanMode" {
                                                        // ExitPlanMode reads from the plan file, content may be in allowedPrompts or other fields
                                                        // The plan file was already captured via Write above
                                                    }

                                                    let input_str = item.get("input")
                                                        .map(|v| format_tool_input(tool_name, v))
                                                        .unwrap_or_default();
                                                    let _ = tx.send(StreamEvent::ToolUse(
                                                        tool_name.to_string(),
                                                        input_str,
                                                    )).await;
                                                } else if item_type == "text" {
                                                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
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
                            // In plan mode, result may be empty due to ExitPlanMode denial
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

        let _ = child.wait().await;
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

/// Run /compact on an existing session to compress context
pub async fn compact_session(session_id: &str, use_z: bool) -> Result<()> {
    let cli_path = verify_cli(use_z)?;
    let cli_name = cli_path.to_string_lossy();

    tracing::info!("Compacting session: {}", session_id);

    let output = base_command(use_z)
        .arg("--resume")
        .arg(session_id)
        .arg("--print")
        .arg("/compact")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context(format!("Failed to execute {} compact", cli_name))?;

    if output.status.success() {
        tracing::info!("Session {} compacted successfully", session_id);
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("Compact failed for session {}: {}", session_id, stderr);
    }

    Ok(())
}

/// Run a Claude Code slash command on a session
/// Returns the command output as text
pub async fn run_slash_command(
    command: &str,
    session_id: Option<&str>,
    use_z: bool,
) -> Result<String> {
    let _cli_path = verify_cli(use_z)?;

    let cmd_str = if command.starts_with('/') {
        command.to_string()
    } else {
        format!("/{}", command)
    };

    let mut cmd = base_command(use_z);

    if let Some(sid) = session_id {
        cmd.arg("--resume").arg(sid);
    }

    cmd.arg("--print")
        .arg(&cmd_str)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .context("Failed to execute slash command")?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if stdout.is_empty() && !stderr.is_empty() {
        Ok(stderr)
    } else if stdout.is_empty() {
        Ok("Command executed (no output).".to_string())
    } else {
        Ok(stdout)
    }
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
