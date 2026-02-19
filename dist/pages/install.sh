#!/bin/bash
set -e

echo "🤖 Installing Neywa..."

# Detect architecture
ARCH=$(uname -m)
OS=$(uname -s)

if [ "$OS" != "Darwin" ]; then
    echo "❌ Currently only macOS is supported"
    exit 1
fi

# Select binary based on architecture
if [ "$ARCH" = "arm64" ]; then
    BINARY="neywa-arm64"
    echo "📱 Detected Apple Silicon (arm64)"
else
    BINARY="neywa-x86_64"
    echo "💻 Detected Intel Mac (x86_64)"
fi

DOWNLOAD_URL="https://neywa.pages.dev/$BINARY"
INSTALL_DIR="$HOME/.local/bin"
TMP_FILE="/tmp/neywa-$$"

# Create install directory if it doesn't exist
mkdir -p "$INSTALL_DIR"

echo "📥 Downloading Neywa..."
if ! curl -fSL "$DOWNLOAD_URL" -o "$TMP_FILE"; then
    echo "❌ Download failed. Please check your network connection."
    exit 1
fi

# Verify download
if [ ! -f "$TMP_FILE" ]; then
    echo "❌ Download failed: file not found"
    exit 1
fi

FILE_SIZE=$(stat -f%z "$TMP_FILE" 2>/dev/null || stat -c%s "$TMP_FILE" 2>/dev/null)
if [ "$FILE_SIZE" -lt 1000000 ]; then
    echo "❌ Download failed: file too small ($FILE_SIZE bytes)"
    rm -f "$TMP_FILE"
    exit 1
fi

echo "📦 Installing to $INSTALL_DIR..."
chmod +x "$TMP_FILE"
mv "$TMP_FILE" "$INSTALL_DIR/neywa"

# Verify installation
if [ -x "$INSTALL_DIR/neywa" ]; then
    echo "✅ Neywa installed successfully!"
    echo ""

    # Check if ~/.local/bin is in PATH
    if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
        echo "⚠️  Add this to your shell config (~/.zshrc or ~/.bashrc):"
        echo ""
        echo "    export PATH=\"\$HOME/.local/bin:\$PATH\""
        echo ""
        echo "Then restart your terminal or run: source ~/.zshrc"
        echo ""
    fi

    echo "Next steps:"
    echo ""
    echo "  1. Run 'neywa install' to configure bot token and server ID"
    echo ""
    echo "  2. Start the service (will guide you through Full Disk Access):"
    echo "     neywa service install"
    echo ""
    echo "  Other commands:"
    echo "    neywa daemon               # Run in foreground (for testing)"
    echo "    neywa discord channels     # List server channels"
    echo "    neywa discord send <ch> <msg>  # Send message to channel"
    echo "    neywa discord create <name>    # Create a channel"
    echo "    neywa discord delete <name>    # Delete a channel"
    echo "    neywa discord move <ch> <cat>  # Move channel to category"
    echo "    neywa service status       # Check status"
    echo "    neywa service uninstall    # Disable auto-start"
    echo ""
    echo "  Discord commands:"
    echo "    !human    # Toggle human-only mode (Neywa stops responding)"
    echo "    !restart  # Restart Neywa (fixes MCP/connection issues)"
    echo ""
else
    echo "❌ Installation failed"
    exit 1
fi
