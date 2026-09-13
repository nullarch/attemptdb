//! AttemptDB desktop: the installer with a window, and the menu-bar line.
//!
//! The app is deliberately thin. It carries the `attempt` binary, puts it on
//! the machine, and calls `attempt setup` — the one place that knows how a
//! machine is wired — then opens the timeline that `attempt ui` serves. It
//! holds no AttemptDB logic of its own, so the terminal installer, this app
//! and the editor plugins cannot disagree about what "set up" means.
//!
//! Three surfaces:
//! - the main window: the state of this machine, one button to set it up,
//!   and the things a person does afterwards (open the timeline, run the
//!   full doctor, remove the hooks);
//! - the timeline window: `attempt ui`, in a window instead of a browser tab;
//! - the menu-bar item: one line — open sessions and what needs the user —
//!   refreshed from the same local server.
//!
//! Closing the main window hides it; the app stays in the menu bar until
//! Quit. Nothing leaves the machine: every request goes to a loopback port.

mod binary;
mod ui_server;

use binary::{Binaries, run_json, run_text};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use ui_server::UiServer;

const INSTALL_COMMAND: &str =
    "curl -fsSL https://raw.githubusercontent.com/nullarch/attemptdb/main/install.sh | sh";

struct AppState {
    ui: Mutex<Option<UiServer>>,
    tray_line: Mutex<Option<MenuItem<tauri::Wry>>>,
}

type CmdResult<T> = Result<T, String>;

fn home(app: &AppHandle) -> CmdResult<PathBuf> {
    app.path().home_dir().map_err(|e| e.to_string())
}

fn binaries(app: &AppHandle) -> CmdResult<Binaries> {
    Ok(Binaries::locate(&home(app)?))
}

/// The binary that drives this machine, or a message that says why none does.
fn active_binary(app: &AppHandle) -> CmdResult<PathBuf> {
    binaries(app)?
        .active()
        .map(|l| l.path.clone())
        .ok_or_else(|| "attempt is not installed on this machine yet".to_string())
}

/// Start `attempt ui` when it is not running, and hand back its URL.
fn ensure_ui(app: &AppHandle) -> CmdResult<String> {
    let state = app.state::<AppState>();
    let mut guard = state.ui.lock().map_err(|e| e.to_string())?;
    if let Some(server) = guard.as_mut()
        && server.alive()
    {
        return Ok(server.url.clone());
    }
    let bin = active_binary(app)?;
    let server = UiServer::start(&bin)?;
    let url = server.url.clone();
    *guard = Some(server);
    Ok(url)
}

/// What this machine looks like right now, from `attempt setup --dry-run`.
#[tauri::command]
async fn probe(app: AppHandle) -> CmdResult<Value> {
    tauri::async_runtime::spawn_blocking(move || {
        let found = binaries(&app)?;
        let (report, error) = match found.active() {
            Some(b) => match run_json(&b.path, &["setup", "--dry-run"]) {
                Ok(r) => (Some(r), None),
                Err(e) => (None, Some(e)),
            },
            None => (None, None),
        };
        Ok(json!({
            "app_version": env!("CARGO_PKG_VERSION"),
            "binaries": found,
            "install_dir": binary::install_dir(&home(&app)?),
            "report": report,
            "error": error,
            "install_command": INSTALL_COMMAND,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Put the binary on the machine and wire it: database, hooks, daemon, check.
#[tauri::command]
async fn setup(app: AppHandle, capture_mode: Option<String>) -> CmdResult<Value> {
    let result = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || {
            let installed = binary::install(&home(&app)?)?;
            let mut args = vec!["setup", "--source", "app"];
            if let Some(mode) = capture_mode.as_deref() {
                args.extend(["--capture-mode", mode]);
            }
            let report = run_json(&installed.path, &args)?;
            Ok::<Value, String>(json!({ "install": installed, "report": report }))
        }
    })
    .await
    .map_err(|e| e.to_string())??;
    // The timeline server can start now that there is a database; the
    // menu-bar line switches to it on its next tick.
    let _ = ensure_ui(&app);
    refresh_tray_line(&app);
    Ok(result)
}

/// Open the timeline window (or bring it to the front).
#[tauri::command]
async fn open_timeline(app: AppHandle) -> CmdResult<()> {
    let url = ensure_ui(&app)?;
    show_timeline(&app, &url)
}

fn show_timeline(app: &AppHandle, url: &str) -> CmdResult<()> {
    if let Some(w) = app.get_webview_window("timeline") {
        w.show().map_err(|e| e.to_string())?;
        w.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let parsed = url.parse().map_err(|e| format!("timeline url: {e}"))?;
    WebviewWindowBuilder::new(app, "timeline", WebviewUrl::External(parsed))
        .title("AttemptDB — Agent Timeline")
        .inner_size(1180.0, 800.0)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// `attempt doctor`, in full — the activity scan included, which on a large
/// database takes a while; the window says so while it waits.
#[tauri::command]
async fn doctor(app: AppHandle) -> CmdResult<Value> {
    tauri::async_runtime::spawn_blocking(move || {
        let bin = active_binary(&app)?;
        let (ok, text) = run_text(&bin, &["doctor"])?;
        Ok(json!({ "ok": ok, "text": text }))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Remove the hooks and the background service; keep the history.
#[tauri::command]
async fn uninstall(app: AppHandle) -> CmdResult<Value> {
    let result = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || {
            let bin = active_binary(&app)?;
            let (ok, text) = run_text(&bin, &["uninstall"])?;
            Ok::<Value, String>(json!({ "ok": ok, "text": text }))
        }
    })
    .await
    .map_err(|e| e.to_string())??;
    refresh_tray_line(&app);
    Ok(result)
}

/// The one line the menu bar shows.
fn tray_line(app: &AppHandle) -> String {
    let state = app.state::<AppState>();
    if let Ok(mut guard) = state.ui.lock()
        && let Some(server) = guard.as_mut()
        && server.alive()
        && let Ok(v) = server.api("/api/attention")
    {
        let open = v["open_sessions"].as_u64().unwrap_or(0);
        let need = v["total"].as_u64().unwrap_or(0);
        return format!(
            "{open} open session{} · {}",
            if open == 1 { "" } else { "s" },
            match need {
                0 => "nothing needs you".to_string(),
                1 => "1 needs you".to_string(),
                n => format!("{n} need you"),
            }
        );
    }
    match binaries(app).ok().and_then(|b| b.active().cloned()) {
        None => "attempt is not installed".into(),
        Some(b) => match run_json(&b.path, &["status"]) {
            Ok(v) => format!(
                "{} events · {} sessions",
                v["events"].as_u64().unwrap_or(0),
                v["sessions"].as_u64().unwrap_or(0)
            ),
            Err(_) => "not set up yet".into(),
        },
    }
}

fn refresh_tray_line(app: &AppHandle) {
    let line = tray_line(app);
    let state = app.state::<AppState>();
    if let Ok(guard) = state.tray_line.lock()
        && let Some(item) = guard.as_ref()
    {
        let _ = item.set_text(&line);
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        #[cfg(target_os = "macos")]
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let line = MenuItem::with_id(app, "line", "AttemptDB — checking…", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open AttemptDB", true, None::<&str>)?;
    let timeline = MenuItem::with_id(app, "timeline", "Open timeline", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit AttemptDB", true, Some("CmdOrCtrl+Q"))?;
    let menu = Menu::with_items(
        app,
        &[
            &line,
            &PredefinedMenuItem::separator(app)?,
            &open,
            &timeline,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    TrayIconBuilder::with_id("main")
        .icon(tauri::include_image!("icons/tray.png"))
        .icon_as_template(true)
        .tooltip("AttemptDB")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main(app),
            "timeline" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = open_timeline(app.clone()).await {
                        eprintln!("timeline: {e}");
                        show_main(&app);
                    }
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    if let Ok(mut guard) = app.state::<AppState>().tray_line.lock() {
        *guard = Some(line);
    }
    Ok(())
}

pub fn run() {
    let app = tauri::Builder::default()
        .manage(AppState {
            ui: Mutex::new(None),
            tray_line: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            probe,
            setup,
            open_timeline,
            doctor,
            uninstall
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            build_tray(&handle)?;
            // The line refreshes on its own clock; the first tick also starts
            // the timeline server when the machine is already set up.
            std::thread::spawn(move || {
                let _ = ensure_ui(&handle);
                loop {
                    refresh_tray_line(&handle);
                    std::thread::sleep(Duration::from_secs(30));
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main"
                && let WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                let _ = window.hide();
                #[cfg(target_os = "macos")]
                let _ = window
                    .app_handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
        })
        .build(tauri::generate_context!())
        .expect("building the AttemptDB app");
    app.run(|app, event| match event {
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_main(app),
        RunEvent::Exit => {
            // Stop `attempt ui` with the app; a Drop does the kill.
            if let Ok(mut guard) = app.state::<AppState>().ui.lock() {
                guard.take();
            }
        }
        _ => {}
    });
}
