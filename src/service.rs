use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const PLIST_NAME: &str = "com.neywa.daemon.plist";

/// Get the LaunchAgent plist path
fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join("Library/LaunchAgents").join(PLIST_NAME))
}

/// Get the current executable path
fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("Could not determine executable path")
}

/// Generate the plist content
fn generate_plist(exe: &PathBuf) -> String {
    // Use login shell to inherit user's PATH (nvm, etc.)
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.neywa.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/zsh</string>
        <string>-l</string>
        <string>-c</string>
        <string>{} daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>/tmp/neywa.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/neywa.log</string>
</dict>
</plist>
"#,
        exe.display()
    )
}

/// Install the LaunchAgent
pub fn install() -> Result<()> {
    let plist = plist_path()?;
    let exe = exe_path()?;

    // Create LaunchAgents directory if needed
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Unload existing service if present
    if plist.exists() {
        let _ = Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(&plist)
            .output();
    }

    // Write plist file
    let content = generate_plist(&exe);
    std::fs::write(&plist, content)?;

    println!("LaunchAgent installed: {:?}", plist);

    // Load the service
    let output = Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist)
        .output()
        .context("Failed to run launchctl")?;

    if output.status.success() {
        println!("Service enabled and started");
        println!("\nNeywa will now start automatically on login.");
        println!("Logs: /tmp/neywa.log");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("service already loaded") {
            println!("Service already running");
        } else {
            anyhow::bail!("Failed to load service: {}", stderr);
        }
    }

    Ok(())
}

/// Uninstall the LaunchAgent
pub fn uninstall() -> Result<()> {
    let plist = plist_path()?;

    if !plist.exists() {
        println!("Service not installed");
        return Ok(());
    }

    // Unload the service
    let output = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist)
        .output()
        .context("Failed to run launchctl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "not loaded" errors
        if !stderr.contains("Could not find specified service") {
            tracing::warn!("launchctl unload warning: {}", stderr);
        }
    }

    // Remove plist file
    std::fs::remove_file(&plist)?;

    println!("Service uninstalled");
    println!("Neywa will no longer start automatically on login.");

    Ok(())
}

/// Show service status
pub fn status() -> Result<()> {
    let plist = plist_path()?;

    println!("LaunchAgent path: {:?}", plist);
    println!("Installed: {}", plist.exists());

    // Check if service is loaded
    let output = Command::new("launchctl")
        .args(["list", "com.neywa.daemon"])
        .output()
        .context("Failed to run launchctl")?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("Status: Running");

        // Parse PID if available
        for line in stdout.lines() {
            if line.contains("PID") || line.starts_with('"') {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 1 {
                if let Ok(pid) = parts[0].parse::<u32>() {
                    println!("PID: {}", pid);
                }
            }
        }
    } else {
        println!("Status: Not running");
    }

    println!("Logs: /tmp/neywa.log");

    Ok(())
}
