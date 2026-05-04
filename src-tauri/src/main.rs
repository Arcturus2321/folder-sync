// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State,
};
use tauri_plugin_autostart::MacosLauncher;
use tokio::sync::mpsc;
use uuid::Uuid;

// ─── Data Types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncPair {
    pub id: String,
    pub source: String,
    pub destination: String,
    pub enabled: bool,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncEvent {
    pub pair_id: String,
    pub kind: String, // "added" | "modified" | "removed" | "error"
    pub path: String,
    pub message: String,
    pub timestamp: u64,
}

// ─── App State ────────────────────────────────────────────────────────────────

struct AppState {
    pub pairs: Mutex<HashMap<String, SyncPair>>,
    pub watchers: Mutex<HashMap<String, RecommendedWatcher>>,
    pub log: Mutex<Vec<SyncEvent>>,
    pub pairs_path: PathBuf,
}

impl AppState {
    fn new(pairs_path: PathBuf) -> Self {
        Self {
            pairs: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            log: Mutex::new(Vec::new()),
            pairs_path,
        }
    }
}

// ─── Persistence ──────────────────────────────────────────────────────────────

fn load_pairs(path: &Path) -> Vec<SyncPair> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn save_pairs(path: &Path, pairs: &HashMap<String, SyncPair>) {
    let list: Vec<&SyncPair> = pairs.values().collect();
    if let Ok(s) = serde_json::to_string_pretty(&list) {
        // Write to a temp file then rename for atomic replacement
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn mirror_path(source: &Path, dest: &Path, changed: &Path) -> Option<PathBuf> {
    changed.strip_prefix(source).ok().map(|rel| dest.join(rel))
}

fn sync_add_or_modify(source_root: &Path, dest_root: &Path, changed: &Path) -> std::io::Result<()> {
    let target = match mirror_path(source_root, dest_root, changed) {
        Some(p) => p,
        None => return Ok(()),
    };
    if changed.is_dir() {
        std::fs::create_dir_all(&target)?;
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(changed, &target)?;
    }
    Ok(())
}

fn sync_remove(source_root: &Path, dest_root: &Path, changed: &Path) -> std::io::Result<()> {
    let target = match mirror_path(source_root, dest_root, changed) {
        Some(p) => p,
        None => return Ok(()),
    };
    if target.exists() {
        if target.is_dir() {
            std::fs::remove_dir_all(&target)?;
        } else {
            std::fs::remove_file(&target)?;
        }
    }
    Ok(())
}

fn initial_sync(source: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in walkdir::WalkDir::new(source)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let rel = match path.strip_prefix(source) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let target = dest.join(rel);
        if path.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            if let Some(p) = target.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(path, &target)?;
        }
    }
    Ok(())
}

// ─── Start a watcher for one SyncPair ────────────────────────────────────────

fn start_watcher(
    app: AppHandle,
    state: Arc<AppState>,
    pair: SyncPair,
) -> Result<RecommendedWatcher, String> {
    let source = PathBuf::from(&pair.source);
    let dest = PathBuf::from(&pair.destination);
    let pair_id = pair.id.clone();

    {
        let src = source.clone();
        let dst = dest.clone();
        let pid = pair_id.clone();
        let app2 = app.clone();
        let state2 = state.clone();
        std::thread::spawn(move || {
            if let Err(e) = initial_sync(&src, &dst) {
                let ev = SyncEvent {
                    pair_id: pid.clone(),
                    kind: "error".into(),
                    path: src.display().to_string(),
                    message: format!("Initial sync failed: {e}"),
                    timestamp: now_millis(),
                };
                let _ = app2.emit("sync-event", ev.clone());
                state2.log.lock().unwrap().push(ev);
            } else {
                let ev = SyncEvent {
                    pair_id: pid.clone(),
                    kind: "info".into(),
                    path: src.display().to_string(),
                    message: "Initial sync complete".into(),
                    timestamp: now_millis(),
                };
                let _ = app2.emit("sync-event", ev.clone());
                state2.log.lock().unwrap().push(ev);
            }
        });
    }

    let (tx, mut rx) = mpsc::channel::<Result<Event, notify::Error>>(256);

    let app_handle = app.clone();
    let state_handle = state.clone();
    let src_clone = source.clone();
    let dst_clone = dest.clone();
    let pid = pair_id.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            while let Some(res) = rx.recv().await {
                match res {
                    Ok(event) => {
                        for path in event.paths {
                            let (kind_str, result) = match event.kind {
                                EventKind::Remove(_) => {
                                    // Explicit remove event
                                    let r = sync_remove(&src_clone, &dst_clone, &path);
                                    ("removed", r)
                                }
                                EventKind::Modify(_) if !path.exists() => {
                                    // Windows fires Modify instead of Remove when a file
                                    // is deleted; detect this by checking existence.
                                    let r = sync_remove(&src_clone, &dst_clone, &path);
                                    ("removed", r)
                                }
                                EventKind::Create(_) | EventKind::Modify(_) => {
                                    // On Windows a Create fires before the writing process
                                    // has closed its handle, making the file unreadable.
                                    // Retry up to ~500 ms to let the handle be released.
                                    let mut r = sync_add_or_modify(&src_clone, &dst_clone, &path);
                                    if r.is_err() && path.exists() {
                                        for delay_ms in [50u64, 150, 300] {
                                            tokio::time::sleep(
                                                tokio::time::Duration::from_millis(delay_ms)
                                            ).await;
                                            r = sync_add_or_modify(&src_clone, &dst_clone, &path);
                                            if r.is_ok() { break; }
                                        }
                                    }
                                    let k = match event.kind {
                                        EventKind::Create(_) => "added",
                                        _ => "modified",
                                    };
                                    (k, r)
                                }
                                _ => continue,
                            };

                            let ev = SyncEvent {
                                pair_id: pid.clone(),
                                kind: kind_str.into(),
                                path: path.display().to_string(),
                                message: match &result {
                                    Ok(_) => format!(
                                        "{} → {}",
                                        path.file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy(),
                                        dst_clone.display()
                                    ),
                                    Err(e) => format!("Error: {e}"),
                                },
                                timestamp: now_millis(),
                            };

                            let _ = app_handle.emit("sync-event", ev.clone());
                            let mut log = state_handle.log.lock().unwrap();
                            log.push(ev);
                            if log.len() > 500 {
                                log.remove(0);
                            }
                        }
                    }
                    Err(e) => {
                        let ev = SyncEvent {
                            pair_id: pid.clone(),
                            kind: "error".into(),
                            path: String::new(),
                            message: format!("Watcher error: {e}"),
                            timestamp: now_millis(),
                        };
                        let _ = app_handle.emit("sync-event", ev.clone());
                    }
                }
            }
        });
    });

    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.blocking_send(res);
        },
        Config::default(),
    )
    .map_err(|e| e.to_string())?;

    watcher
        .watch(&source, RecursiveMode::Recursive)
        .map_err(|e| e.to_string())?;

    Ok(watcher)
}

// ─── Tauri Commands ───────────────────────────────────────────────────────────

#[tauri::command]
fn add_pair(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    source: String,
    destination: String,
    label: String,
) -> Result<SyncPair, String> {
    let src_path = PathBuf::from(&source);
    let dst_path = PathBuf::from(&destination);

    if !src_path.exists() {
        return Err(format!("Source folder does not exist: {source}"));
    }
    std::fs::create_dir_all(&dst_path).map_err(|e| e.to_string())?;

    let pair = SyncPair {
        id: Uuid::new_v4().to_string(),
        source,
        destination,
        enabled: true,
        label,
    };

    let watcher = start_watcher(app, state.inner().clone(), pair.clone())?;
    state.pairs.lock().unwrap().insert(pair.id.clone(), pair.clone());
    state.watchers.lock().unwrap().insert(pair.id.clone(), watcher);

    save_pairs(&state.pairs_path, &state.pairs.lock().unwrap());

    Ok(pair)
}

#[tauri::command]
fn remove_pair(state: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    state.watchers.lock().unwrap().remove(&id);
    state.pairs.lock().unwrap().remove(&id);
    save_pairs(&state.pairs_path, &state.pairs.lock().unwrap());
    Ok(())
}

#[tauri::command]
fn toggle_pair(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    let pair_opt = state.pairs.lock().unwrap().get(&id).cloned();
    let pair = pair_opt.ok_or("Pair not found")?;

    if enabled {
        let watcher = start_watcher(app, state.inner().clone(), pair.clone())?;
        state.watchers.lock().unwrap().insert(id.clone(), watcher);
    } else {
        state.watchers.lock().unwrap().remove(&id);
    }

    if let Some(p) = state.pairs.lock().unwrap().get_mut(&id) {
        p.enabled = enabled;
    }
    save_pairs(&state.pairs_path, &state.pairs.lock().unwrap());
    Ok(())
}

#[tauri::command]
fn list_pairs(state: State<'_, Arc<AppState>>) -> Vec<SyncPair> {
    state.pairs.lock().unwrap().values().cloned().collect()
}

#[tauri::command]
fn get_log(state: State<'_, Arc<AppState>>) -> Vec<SyncEvent> {
    state.log.lock().unwrap().clone()
}

#[tauri::command]
fn clear_log(state: State<'_, Arc<AppState>>) {
    state.log.lock().unwrap().clear();
}

/// Returns whether autostart is currently enabled.
#[tauri::command]
fn autostart_status(app: AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Toggle autostart on/off.
#[tauri::command]
fn set_autostart(app: AppHandle, enable: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    if enable {
        app.autolaunch().enable().map_err(|e| e.to_string())
    } else {
        app.autolaunch().disable().map_err(|e| e.to_string())
    }
}

// ─── Tray setup ───────────────────────────────────────────────────────────────

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    use tauri_plugin_autostart::ManagerExt;

    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart_label = if autostart_on {
        "✓ Launch at Login"
    } else {
        "  Launch at Login"
    };

    let show    = MenuItem::with_id(app, "show",       "Show Window",        true, None::<&str>)?;
    let hide    = MenuItem::with_id(app, "hide",       "Hide Window",        true, None::<&str>)?;
    let auto    = MenuItem::with_id(app, "autostart",  autostart_label,      true, None::<&str>)?;
    let sep     = PredefinedMenuItem::separator(app)?;
    let quit    = MenuItem::with_id(app, "quit",       "Quit FolderSync",    true, None::<&str>)?;

    let menu = Menu::with_items(app, &[&show, &hide, &auto, &sep, &quit])?;

    let icon = app.default_window_icon().cloned().expect("no app icon");

    TrayIconBuilder::with_id("tray")
        .icon(icon)
        .icon_as_template(true)          // macOS monochrome template
        .tooltip("FolderSync")
        .menu(&menu)
        .on_menu_event(|app, event| {
            match event.id.as_ref() {
                "show" => {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
                "hide" => {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.hide();
                    }
                }
                "autostart" => {
                    use tauri_plugin_autostart::ManagerExt;
                    let al = app.autolaunch();
                    let currently = al.is_enabled().unwrap_or(false);
                    if currently {
                        let _ = al.disable();
                    } else {
                        let _ = al.enable();
                    }
                    // Rebuild tray menu to reflect new state
                    let _ = setup_tray(app);
                }
                "quit" => {
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            // Left-click toggles window visibility
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(w) = app.get_webview_window("main") {
                    if w.is_visible().unwrap_or(false) {
                        let _ = w.hide();
                    } else {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            }
        })
        .build(app)?;

    Ok(())
}

// ─── App Entry ────────────────────────────────────────────────────────────────

fn main() {
    env_logger::init();

    // Resolve a stable data directory for the pairs file.
    // On all platforms this is the OS app-data dir, e.g.:
    //   macOS  ~/Library/Application Support/com.foldersync.app/
    //   Windows %APPDATA%\com.foldersync.app\
    //   Linux  ~/.local/share/com.foldersync.app/
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("com.foldersync.app");
    std::fs::create_dir_all(&data_dir).ok();
    let pairs_path = data_dir.join("pairs.json");

    let state = Arc::new(AppState::new(pairs_path.clone()));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--autostarted"]),
        ))
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            add_pair,
            remove_pair,
            toggle_pair,
            list_pairs,
            get_log,
            clear_log,
            autostart_status,
            set_autostart,
        ])
        .setup(move |app| {
            // Build the tray
            setup_tray(&app.handle())?;

            // Restore persisted sync pairs and start their watchers
            let state = app.state::<Arc<AppState>>();
            let saved = load_pairs(&pairs_path);
            for pair in saved {
                // Skip pairs whose source no longer exists; don't crash startup
                if !PathBuf::from(&pair.source).exists() {
                    log::warn!("Skipping pair '{}': source path gone", pair.label);
                    state.pairs.lock().unwrap().insert(pair.id.clone(), pair);
                    continue;
                }
                if pair.enabled {
                    match start_watcher(app.handle().clone(), state.inner().clone(), pair.clone()) {
                        Ok(watcher) => {
                            state.watchers.lock().unwrap().insert(pair.id.clone(), watcher);
                        }
                        Err(e) => log::error!("Could not start watcher for '{}': {e}", pair.label),
                    }
                }
                state.pairs.lock().unwrap().insert(pair.id.clone(), pair);
            }

            // If launched at login (--autostarted flag), stay hidden.
            // Otherwise show the window on first launch.
            let autostarted = std::env::args().any(|a| a == "--autostarted");
            if !autostarted {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // Close button hides the window instead of quitting
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
