use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const PLIST_NAME: &str = "com.neywa.daemon.plist";
const APP_BUNDLE_PATH: &str = "/Applications/Neywa.app";
const BUNDLE_ID: &str = "com.neywa.daemon";

/// App icon embedded at compile time
const APP_ICON: &[u8] = include_bytes!("../assets/AppIcon.icns");

/// Get the LaunchAgent plist path
fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join("Library/LaunchAgents").join(PLIST_NAME))
}

/// Get the current executable path
fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("Could not determine executable path")
}

/// Path to the binary inside the .app bundle
fn app_exe_path() -> PathBuf {
    PathBuf::from(APP_BUNDLE_PATH).join("Contents/MacOS/neywa")
}

/// Create or update the Neywa.app bundle in /Applications
/// This allows the binary to be registered in Full Disk Access
fn create_app_bundle(source_exe: &PathBuf) -> Result<()> {
    let app_path = PathBuf::from(APP_BUNDLE_PATH);
    let contents_path = app_path.join("Contents");
    let macos_path = contents_path.join("MacOS");

    // Create directory structure
    std::fs::create_dir_all(&macos_path)
        .context("Failed to create Neywa.app bundle (try with sudo?)")?;

    // Write app icon
    let resources_path = contents_path.join("Resources");
    std::fs::create_dir_all(&resources_path)?;
    std::fs::write(resources_path.join("AppIcon.icns"), APP_ICON)?;

    // Write Info.plist
    let version = env!("CARGO_PKG_VERSION");
    let info_plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>neywa</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>{}</string>
    <key>CFBundleName</key>
    <string>Neywa</string>
    <key>CFBundleVersion</key>
    <string>{}</string>
    <key>CFBundleShortVersionString</key>
    <string>{}</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
"#,
        BUNDLE_ID, version, version
    );
    std::fs::write(contents_path.join("Info.plist"), info_plist)?;

    // Copy binary into the bundle
    let dest = macos_path.join("neywa");
    std::fs::copy(source_exe, &dest)
        .context("Failed to copy binary to Neywa.app")?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms)?;
    }

    println!("App bundle created: {}", APP_BUNDLE_PATH);
    Ok(())
}

/// Generate the plist content - uses the .app bundle binary
fn generate_plist(exe: &PathBuf) -> String {
    // Use login shell to inherit user's PATH (nvm, etc.)
    // caffeinate -s prevents system sleep while Neywa is running (allows display sleep)
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
        <string>caffeinate -s {} daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
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

    // Create .app bundle and copy binary
    create_app_bundle(&exe)?;

    // Use the binary inside the .app bundle for LaunchAgent
    let app_exe = app_exe_path();

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

    // Write plist file - pointing to the .app bundle binary
    let content = generate_plist(&app_exe);
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
        println!("Sleep prevention: ENABLED (display may turn off, but system stays awake)");
        println!("Logs: /tmp/neywa.log");
        println!("\nTo grant Full Disk Access (prevents permission popups):");
        println!("  System Settings > Privacy & Security > Full Disk Access");
        println!("  Add: Neywa.app (from /Applications)");
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
