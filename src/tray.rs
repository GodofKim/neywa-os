use std::sync::mpsc;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    TrayIconBuilder, TrayIconEvent,
};

const ICON_BYTES: &[u8] = include_bytes!("../assets/tray-icon.png");

#[derive(Debug, Clone)]
pub enum TrayCommand {
    UpdateStatus(String),
    Quit,
}

/// Set macOS app to run as menu bar only (no dock icon, no app menu)
#[cfg(target_os = "macos")]
fn set_macos_accessory_mode() {
    use objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let app: *mut objc::runtime::Object = msg_send![class!(NSApplication), sharedApplication];
        // NSApplicationActivationPolicyAccessory = 1
        let _: () = msg_send![app, setActivationPolicy: 1i64];
    }
}

pub fn run_tray(status_rx: mpsc::Receiver<TrayCommand>, quit_tx: mpsc::Sender<()>) {
    // Set as accessory app on macOS (menu bar only)
    #[cfg(target_os = "macos")]
    set_macos_accessory_mode();

    let event_loop = EventLoopBuilder::new().build();

    // Load icon
    let icon = load_icon();

    // Create menu with better structure
    let menu = Menu::new();

    // App header (disabled, just for display)
    let app_name = MenuItem::new("🤖 Neywa", false, None);
    let version = MenuItem::new("   v0.2.0", false, None);
    let separator1 = PredefinedMenuItem::separator();

    // Status section
    let status_label = MenuItem::new("상태", false, None);
    let status_item = MenuItem::new("   🟢 Discord 연결됨", false, None);
    let separator2 = PredefinedMenuItem::separator();

    // Actions
    let open_discord = MenuItem::new("Discord 열기", true, None);
    let separator3 = PredefinedMenuItem::separator();

    // Quit
    let quit_item = MenuItem::new("Neywa 종료", true, None);

    // Build menu
    menu.append(&app_name).unwrap();
    menu.append(&version).unwrap();
    menu.append(&separator1).unwrap();
    menu.append(&status_label).unwrap();
    menu.append(&status_item).unwrap();
    menu.append(&separator2).unwrap();
    menu.append(&open_discord).unwrap();
    menu.append(&separator3).unwrap();
    menu.append(&quit_item).unwrap();

    let quit_item_id = quit_item.id().clone();
    let open_discord_id = open_discord.id().clone();

    // Build tray icon
    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Neywa - AI Personal OS")
        .with_icon(icon)
        .build()
        .expect("Failed to create tray icon");

    // Handle menu events
    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();

    event_loop.run(move |_event, _, control_flow| {
        *control_flow = ControlFlow::Poll;

        // Check for status updates from the daemon
        if let Ok(cmd) = status_rx.try_recv() {
            match cmd {
                TrayCommand::UpdateStatus(status) => {
                    let status_text = format!("   {}", status);
                    status_item.set_text(&status_text);
                }
                TrayCommand::Quit => {
                    *control_flow = ControlFlow::Exit;
                }
            }
        }

        // Handle menu clicks
        if let Ok(event) = menu_channel.try_recv() {
            if event.id == quit_item_id {
                let _ = quit_tx.send(());
                *control_flow = ControlFlow::Exit;
            } else if event.id == open_discord_id {
                // Open Discord app
                let _ = std::process::Command::new("open")
                    .arg("-a")
                    .arg("Discord")
                    .spawn();
            }
        }

        // Handle tray icon clicks (optional)
        if let Ok(_event) = tray_channel.try_recv() {
            // Could show a popup or toggle visibility
        }

        // Small sleep to prevent busy loop
        std::thread::sleep(std::time::Duration::from_millis(16));
    });
}

fn load_icon() -> tray_icon::Icon {
    let image = image::load_from_memory(ICON_BYTES)
        .expect("Failed to load icon")
        .into_rgba8();
    let (width, height) = image.dimensions();
    let rgba = image.into_raw();

    tray_icon::Icon::from_rgba(rgba, width, height).expect("Failed to create icon")
}
