// Oracle of Delphi — the native window.
//
// A deliberately tiny shell built directly on tao (windowing) + wry (WebView2),
// with NO Tauri framework: no bundler, no sidecar manifests, no capabilities
// JSON — just a window that shows the HUD `oracle-core` already serves, a global
// hotkey to summon/dismiss it, and a tray icon. All the real work (LLM, tools,
// voice, memory) stays in oracle-core, which this shell launches and reaps.
//
// Lifecycle:
//   * On start: spawn oracle-core (hidden) so the whole assistant comes up.
//   * Window shows the HUD at http://127.0.0.1:8770.
//   * Global hotkey (Ctrl+Alt+O) toggles the window (summon / dismiss).
//   * Closing the window hides it to the tray; "Quit" from the tray shuts the
//     whole Oracle down (core + its children) and exits.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::Child;
use std::time::{Duration, Instant};

use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::{Icon, WindowBuilder},
};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    TrayIconBuilder, TrayIconEvent,
};
use wry::WebViewBuilder;

const HUD_URL: &str = "http://127.0.0.1:8770";

fn main() -> wry::Result<()> {
    // 1) Launch the backend (oracle-core) so the HUD has something to serve.
    let mut core = spawn_core();

    // 2) The window that IS the Oracle.
    let event_loop = EventLoopBuilder::new().build();
    let window = WindowBuilder::new()
        .with_title("Oracle of Delphi")
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .with_min_inner_size(LogicalSize::new(900.0, 600.0))
        .with_window_icon(load_window_icon())
        .build(&event_loop)
        .expect("failed to create the Oracle window");

    // 3) The WebView pointed at the HUD core serves. Core may still be starting,
    //    and the HUD retries its own WebSocket, so loading immediately is fine.
    //
    //    Microphone: an embedded WebView2 denies mic access by default and — with
    //    no browser chrome — can't show a permission prompt, so the HUD's voice
    //    recognition silently gets nothing. `--use-fake-ui-for-media-stream`
    //    auto-accepts the media-permission request against the REAL microphone
    //    (no prompt, real device), which is what a local voice assistant wants.
    //    We keep wry's own default args (mini-menu / SmartScreen suppression) and
    //    add ours; `--autoplay-policy` lets TTS start without a user gesture.
    let webview_builder = WebViewBuilder::new(&window).with_url(HUD_URL);
    #[cfg(windows)]
    let webview_builder = {
        use wry::WebViewBuilderExtWindows;
        webview_builder.with_additional_browser_args(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
             --autoplay-policy=no-user-gesture-required \
             --use-fake-ui-for-media-stream",
        )
    };
    let _webview = webview_builder.build()?;

    // 4) Global summon hotkey: Ctrl+Alt+O.
    let hotkey_manager = GlobalHotKeyManager::new().expect("hotkey manager");
    let summon = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyO);
    let summon_id = summon.id();
    if let Err(e) = hotkey_manager.register(summon) {
        eprintln!("[oracle-shell] could not register Ctrl+Alt+O: {e}");
    }

    // 5) Tray icon: the sun that stays lit while the window is dismissed.
    let tray_menu = Menu::new();
    let show_item = MenuItem::new("Summon the Oracle", true, None);
    let quit_item = MenuItem::new("Quit Oracle", true, None);
    tray_menu
        .append_items(&[&show_item, &quit_item])
        .expect("tray menu");
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("Oracle of Delphi — the eye is open")
        .with_icon(load_tray_icon())
        .build()
        .expect("tray icon");

    let hotkey_rx = GlobalHotKeyEvent::receiver();
    let menu_rx = MenuEvent::receiver();
    let tray_rx = TrayIconEvent::receiver();

    event_loop.run(move |event, _, control_flow| {
        // Wake ~10x/sec to service the hotkey/tray channels.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100));

        // Wake word: core raises a summon flag when it hears "Pythia"; bring the
        // window forward (hands-free) and consume the flag.
        if take_summon_flag() {
            summon_window(&window);
        }

        // Global hotkey → toggle the window.
        if let Ok(ev) = hotkey_rx.try_recv() {
            if ev.state == HotKeyState::Pressed && ev.id == summon_id {
                toggle(&window);
            }
        }

        // Tray menu clicks.
        if let Ok(ev) = menu_rx.try_recv() {
            if ev.id == show_id {
                summon_window(&window);
            } else if ev.id == quit_id {
                shutdown(&mut core);
                *control_flow = ControlFlow::Exit;
            }
        }

        // Left-click the tray icon → toggle.
        if let Ok(TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        }) = tray_rx.try_recv()
        {
            toggle(&window);
        }

        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            // Closing hides to the tray; the Oracle keeps running.
            window.set_visible(false);
        }
    });
}

/// The rendezvous flag core writes when the wake word fires. Must match
/// oracle-core's `summon_flag_path()` exactly: `%LOCALAPPDATA%\oracle\summon.flag`
/// on Windows, the temp dir elsewhere.
fn summon_flag_path() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
        std::path::Path::new(&base).join("oracle").join("summon.flag")
    }
    #[cfg(not(windows))]
    {
        std::env::temp_dir().join("oracle-summon.flag")
    }
}

/// If the summon flag exists, delete it and report true (so we summon once per
/// wake, not continuously). A missing flag is the common case and costs one
/// cheap stat per tick.
fn take_summon_flag() -> bool {
    let path = summon_flag_path();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
        true
    } else {
        false
    }
}

/// Show + focus the window.
fn summon_window(window: &tao::window::Window) {
    window.set_visible(true);
    window.set_minimized(false);
    window.set_focus();
}

/// Toggle: dismiss if it's the visible foreground, otherwise summon it.
fn toggle(window: &tao::window::Window) {
    if window.is_visible() && window.is_focused() {
        window.set_visible(false);
    } else {
        summon_window(window);
    }
}

/// Spawn oracle-core, hidden, with `--no-window` (the shell IS the window).
/// Resolution order for the core binary: ORACLE_CORE_EXE, then a sibling
/// `oracle-core.exe`, then `oracle-core` on PATH.
fn spawn_core() -> Option<Child> {
    let exe = core_exe_path();
    let config = std::env::var("ORACLE_CONFIG").unwrap_or_else(|_| {
        // %APPDATA%\oracle\oracle.toml by default.
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        format!(r"{appdata}\oracle\oracle.toml")
    });

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("run")
        .arg("--config")
        .arg(&config)
        .arg("--no-window");

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    match cmd.spawn() {
        Ok(child) => {
            eprintln!("[oracle-shell] launched core: {exe} (config {config})");
            Some(child)
        }
        Err(e) => {
            eprintln!("[oracle-shell] could not launch core '{exe}': {e} — is oracle-core.exe next to this app, or ORACLE_CORE_EXE set?");
            None
        }
    }
}

fn core_exe_path() -> String {
    if let Ok(p) = std::env::var("ORACLE_CORE_EXE") {
        if !p.trim().is_empty() {
            return p;
        }
    }
    let name = if cfg!(windows) {
        "oracle-core.exe"
    } else {
        "oracle-core"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(name);
            if cand.exists() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    name.to_string()
}

/// Shut the whole Oracle down: kill core (which reaps llama-server + actd), and
/// as a backstop kill any stragglers by name.
fn shutdown(core: &mut Option<Child>) {
    if let Some(child) = core.as_mut() {
        let _ = child.kill();
    }
    #[cfg(windows)]
    for name in ["oracle-core.exe", "oracle-actd.exe", "llama-server.exe"] {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", name, "/F"])
            .creation_flags(0x0800_0000)
            .spawn();
    }
}

/// Load the window titlebar/taskbar icon (PNG embedded at build time).
fn load_window_icon() -> Option<Icon> {
    let bytes = include_bytes!("../icons/icon.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Load the tray icon.
fn load_tray_icon() -> tray_icon::Icon {
    let bytes = include_bytes!("../icons/icon.png");
    let img = image::load_from_memory(bytes)
        .expect("tray icon decode")
        .into_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).expect("tray icon rgba")
}
