<p align="center">
  <img src="assets/logo.png" alt="Neywa Logo" width="120">
</p>

<h1 align="center">Neywa</h1>

<p align="center">
  <strong>AI-Powered Personal OS via Discord + Claude Code</strong>
</p>

<p align="center">
  <a href="https://neywa.pages.dev">Website</a> •
  <a href="#installation">Installation</a> •
  <a href="#features">Features</a> •
  <a href="#commands">Commands</a> •
  <a href="#license">License</a>
</p>

---

## What is Neywa?

Neywa is an AI assistant that lives in your Discord. It connects [Claude Code](https://docs.anthropic.com/en/docs/claude-code) to Discord, giving you access to a powerful AI that can:

- Read and write files on your computer
- Execute terminal commands
- Search the web
- Help with coding, research, and everyday tasks

All through Discord DMs or channels.

## Features

- **Real-time Streaming** - See Claude's responses as they're generated
- **Multi-user Support** - Claude knows who's talking in group channels
- **Message Queue** - Messages sent while processing are queued automatically
- **Instant Stop** - Cancel processing with `!stop`
- **Session Persistence** - Continue conversations across restarts
- **Menu Bar App** - macOS tray icon shows status

## Installation

### Quick Install (macOS)

```bash
curl -fsSL https://neywa.pages.dev/install.sh | bash
```

### Manual Build

```bash
git clone https://github.com/GodofKim/neywa-os.git
cd neywa
cargo build --release
```

### Setup

1. **Install Claude Code CLI** first: [docs.anthropic.com/en/docs/claude-code](https://docs.anthropic.com/en/docs/claude-code)

2. **Create a Discord Bot**:
   - Go to [Discord Developer Portal](https://discord.com/developers/applications)
   - Create new application → Bot → Copy token
   - Enable: Message Content Intent, Server Members Intent, Presence Intent
   - Invite bot to your server with Send Messages, Read Message History permissions

3. **Configure Neywa**:
   ```bash
   neywa install  # Enter your Discord bot token
   ```

4. **Start the daemon**:
   ```bash
   neywa daemon
   ```

## Commands

| Command | Description |
|---------|-------------|
| `!help` | Show available commands |
| `!status` | Check session status and queue |
| `!new` | Start a new conversation |
| `!stop` | Stop current processing and clear queue |
| `!queue` | Show queued messages |

## How It Works

```
Discord Message → Neywa Daemon → Claude Code CLI → Response → Discord
```

Neywa acts as a bridge between Discord and Claude Code. When you send a message:

1. Neywa receives it via Discord API
2. Forwards to Claude Code CLI with your context
3. Streams the response back to Discord in real-time

## Project Structure

```
neywa/
├── src/
│   ├── main.rs       # CLI entry point
│   ├── discord.rs    # Discord bot handler
│   ├── claude.rs     # Claude Code CLI wrapper
│   └── tray.rs       # macOS menu bar icon
├── dist/pages/       # Website & binaries
└── Cargo.toml
```

## ⚠️ Security Warning

Neywa runs Claude Code with the `--dangerously-skip-permissions` flag, meaning **all actions are executed without approval prompts**:

- File read/write/delete
- Terminal command execution
- Web searches

**Use with caution.** To prevent unwanted actions, create a `~/CLAUDE.md` file with restrictions:

```markdown
# Claude Code Rules

- Never delete files without explicit confirmation
- Never run destructive commands (rm -rf, etc.)
- Never modify files outside the current project
```

## Requirements

- macOS (arm64 or x86_64)
- [Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code)
- Discord Bot Token

## Configuration

Config file: `~/.config/neywa/config.json`

```json
{
  "discord_token": "your-bot-token"
}
```

## Development

```bash
# Build for current architecture
cargo build --release

# Build for specific target
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
```

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

MIT License - see [LICENSE](LICENSE) for details.

---

<p align="center">
  Built by <a href="https://alienz.ooo">ALIENZ</a>
</p>
