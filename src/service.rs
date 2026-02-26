use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const PLIST_NAME: &str = "com.neywa.daemon.plist";
const APP_BUNDLE_PATH: &str = "/Applications/Neywa.app";
const BUNDLE_ID: &str = "com.neywa.daemon";

/// FDA settings URL for macOS Ventura+ and fallback
const FDA_URL_NEW: &str = "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles";
const FDA_URL_OLD: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/// App icon embedded at compile time
const APP_ICON: &[u8] = include_bytes!("../assets/AppIcon.icns");

/// Open System Settings > Full Disk Access page
fn open_fda_settings() {
    // Try new URL scheme first (macOS Ventura+), fall back to old
    let _ = Command::new("open").arg(FDA_URL_NEW).output()
        .or_else(|_| Command::new("open").arg(FDA_URL_OLD).output());
}

/// Guide user through granting Full Disk Access
fn guide_fda_setup() {
    println!();
    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║              Full Disk Access Setup                      ║");
    println!("╠═══════════════════════════════════════════════════════════╣");
    println!("║                                                           ║");
    println!("║  Without this, macOS will repeatedly show permission      ║");
    println!("║  popups like \"node wants to access your files\".           ║");
    println!("║                                                           ║");
    println!("║  Opening System Settings > Full Disk Access now...        ║");
    println!("║                                                           ║");
    println!("║  Just add Neywa.app:                                      ║");
    println!("║    Click [+] > /Applications > Neywa.app > Open           ║");
    println!("║                                                           ║");
    println!("║  That's it! All child processes (node, claude, etc.)      ║");
    println!("║  will inherit Neywa.app's Full Disk Access.               ║");
    println!("║                                                           ║");
    println!("╚═══════════════════════════════════════════════════════════╝");
    println!();

    // Open FDA settings page
    open_fda_settings();
}

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

fn launchctl_target() -> Result<String> {
    let uid = std::env::var("UID").ok().filter(|v| !v.trim().is_empty()).or_else(|| {
        let output = Command::new("id").arg("-u").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if uid.is_empty() { None } else { Some(uid) }
    }).context("Could not determine current uid")?;

    Ok(format!("gui/{}/com.neywa.daemon", uid))
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

    // Re-sign the .app bundle so macOS launches it without code-signature errors
    #[cfg(target_os = "macos")]
    {
        let sign_output = Command::new("codesign")
            .args(["--force", "--sign", "-", APP_BUNDLE_PATH])
            .output();
        match sign_output {
            Ok(out) if out.status.success() => {
                println!("Re-signed Neywa.app successfully");
            }
            Ok(out) => {
                eprintln!("codesign warning: {}", String::from_utf8_lossy(&out.stderr));
            }
            Err(e) => {
                eprintln!("Failed to run codesign: {}", e);
            }
        }
    }

    println!("App bundle created: {}", APP_BUNDLE_PATH);
    Ok(())
}

/// Detect PATH directories that should be available to the daemon
fn detect_path() -> String {
    let mut paths: Vec<String> = vec![
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
        "/usr/sbin".to_string(),
        "/sbin".to_string(),
    ];

    // Homebrew (Apple Silicon and Intel)
    for p in &["/opt/homebrew/bin", "/opt/homebrew/sbin"] {
        if PathBuf::from(p).exists() && !paths.contains(&p.to_string()) {
            paths.insert(0, p.to_string());
        }
    }

    if let Some(home) = dirs::home_dir() {
        // User-local paths
        for p in &[home.join(".local/bin"), home.join(".cargo/bin")] {
            if p.exists() {
                let s = p.display().to_string();
                if !paths.contains(&s) {
                    paths.insert(0, s);
                }
            }
        }

        // nvm node path
        let nvm_dir = home.join(".nvm/versions/node");
        if nvm_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&nvm_dir) {
                let mut versions: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path().join("bin"))
                    .filter(|p| p.exists())
                    .collect();
                versions.sort();
                if let Some(latest) = versions.last() {
                    let s = latest.display().to_string();
                    if !paths.contains(&s) {
                        paths.insert(0, s);
                    }
                }
            }
        }
    }

    paths.join(":")
}

/// Generate the plist content - launches neywa directly from .app bundle.
/// This ensures Neywa.app is the "responsible process" for TCC/FDA,
/// so child processes (like node) inherit Neywa.app's Full Disk Access.
fn generate_plist(exe: &PathBuf) -> String {
    let home = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_else(|| "/Users/unknown".to_string());
    let path = detect_path();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.neywa.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>daemon</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{}</string>
        <key>HOME</key>
        <string>{}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>3</integer>
    <key>StandardOutPath</key>
    <string>/tmp/neywa.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/neywa.log</string>
</dict>
</plist>
"#,
        exe.display(), path, home
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

        // Auto-guide FDA setup
        guide_fda_setup();
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
    let app_exe = app_exe_path();
    let target = launchctl_target()?;

    println!("LaunchAgent path: {:?}", plist);
    println!("Installed: {}", plist.exists());
    println!("CLI version: {}", env!("CARGO_PKG_VERSION"));
    println!("App binary path: {}", app_exe.display());
    println!("App binary exists: {}", app_exe.exists());

    if app_exe.exists() {
        let app_ver = Command::new(&app_exe).arg("--version").output();
        if let Ok(out) = app_ver {
            if out.status.success() {
                let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
                println!("App binary version: {}", v);
            }
        }
    }

    // Check if service is loaded/running in the current GUI domain
    let output = Command::new("launchctl")
        .args(["print", &target])
        .output()
        .context("Failed to run launchctl")?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let is_running = stdout.contains("state = running");
        println!("Status: {}", if is_running { "Running" } else { "Loaded (not running)" });

        // Parse PID if available from launchctl print output
        for line in stdout.lines() {
            let line = line.trim();
            if !line.starts_with("pid = ") {
                continue;
            }
            if let Some(pid) = line.strip_prefix("pid = ").and_then(|s| s.parse::<u32>().ok()) {
                println!("PID: {}", pid);
            }
        }
    } else {
        println!("Status: Not running");
    }

    println!("Logs: /tmp/neywa.log");

    Ok(())
}
