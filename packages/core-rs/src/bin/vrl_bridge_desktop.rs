#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use serde_json::Value;
use single_instance::SingleInstance;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use vrl_osc_core::app_store::{
    AppConfigStore, AppProfile, KeyringSecretStore, ProfileStore, ProfileSummary,
};
use vrl_osc_core::config::{
    DebugConfig, DiscoveryConfig, ForwardTarget, InAppConfig, OscListen, RelayPaths,
};
use vrl_osc_core::diagnostics::{collect_vrchat_diagnostics, VrchatDiagnostics};
use vrl_osc_core::intiface::{
    IntifaceBridgeConfig, IntifaceBridgeHandle, IntifaceBridgeSnapshot, IntifaceClient,
    IntifaceConfig, IntifaceRouteRule, IntifaceSnapshot, IntifaceSourceKind,
};
use vrl_osc_core::json_store::JsonConfigStore;
use vrl_osc_core::mapping::{Curve, Mapping};
use vrl_osc_core::oscquery::{OscQueryClient, OscQueryStatus};
use vrl_osc_core::runtime::{RuntimeLogLine, RuntimeParamValue, RuntimeSnapshot};
use vrl_osc_core::service::{AppBridgeService, DefaultRelayPublisherFactory};
use vrl_osc_core::smoothing::OutputTuning;

type BridgeService =
    AppBridgeService<JsonConfigStore<KeyringSecretStore>, DefaultRelayPublisherFactory>;

fn default_config_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("Vibealong")
            .join("vibealong.json");
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(data_home)
                .join("vibealong")
                .join("vibealong.json");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("vibealong")
                .join("vibealong.json");
        }
    }

    PathBuf::from("data/vibealong.json")
}

fn default_legacy_db_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("Vibealong")
            .join("vibealong.sqlite");
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(data_home)
                .join("vibealong")
                .join("vibealong.sqlite");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("vibealong")
                .join("vibealong.sqlite");
        }
    }

    PathBuf::from("data/vibealong.sqlite")
}

fn maybe_migrate_legacy_sqlite_to_json(
    config_path: &PathBuf,
    legacy_db_path: &PathBuf,
    keyring: &KeyringSecretStore,
) -> Result<(), Box<dyn Error>> {
    if config_path.exists() || !legacy_db_path.exists() {
        return Ok(());
    }
    if let Some(parent) = config_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let legacy_store = AppConfigStore::open(legacy_db_path, keyring.clone())?;
    let mut json_store = JsonConfigStore::open(config_path, keyring.clone())?;

    for summary in legacy_store.list_profiles()? {
        if let Some(loaded) = legacy_store.load_profile(&summary.id)? {
            if let Some(stream_key) = loaded.stream_key {
                let _ = json_store.upsert_profile(&loaded.profile, &stream_key)?;
            }
        }
    }

    if let Some(last_selected) = legacy_store.last_selected_profile_id()? {
        let _ = json_store.set_last_selected_profile_id(Some(&last_selected));
    }
    if let Some(enabled) = legacy_store.close_to_background_preference()? {
        let _ = json_store.set_close_to_background_preference(enabled);
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let app_instance = SingleInstance::new("vibealong-desktop-singleton")
        .map_err(|e| format!("Failed to create single-instance guard: {e}"))?;
    if !app_instance.is_single() {
        return Err("Vibealong is already running".into());
    }

    let db_path = default_config_path();
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let keyring = KeyringSecretStore::new("Vibealong")
        .with_legacy_service_names(vec!["VRLewdsBridge".to_string()]);
    maybe_migrate_legacy_sqlite_to_json(&db_path, &default_legacy_db_path(), &keyring)?;

    let store = JsonConfigStore::open(&db_path, keyring)?;
    let service = AppBridgeService::new(store, DefaultRelayPublisherFactory);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1240.0, 840.0])
        .with_min_inner_size([980.0, 680.0])
        .with_title("Vibealong");
    if let Some(icon_data) = load_egui_icon_data() {
        viewport = viewport.with_icon(icon_data);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let result = eframe::run_native(
        "Vibealong",
        native_options,
        Box::new(move |_cc| {
            let app = DesktopApp::new(service, db_path, _cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    );
    if let Err(err) = result {
        return Err(format!("Failed to launch desktop app: {err}").into());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopTab {
    Login,
    Setup,
    Home,
    Logs,
    AvatarDebugger,
    Settings,
    Diagnostics,
    Intiface,
}

impl DesktopTab {
    fn label(self) -> &'static str {
        match self {
            Self::Login => "Login",
            Self::Setup => "Setup Wizard",
            Self::Home => "Home",
            Self::Logs => "Logs",
            Self::AvatarDebugger => "Parameters",
            Self::Settings => "Settings",
            Self::Diagnostics => "Diagnostics",
            Self::Intiface => "Intiface",
        }
    }

    fn all() -> [Self; 8] {
        [
            Self::Login,
            Self::Setup,
            Self::Home,
            Self::Logs,
            Self::AvatarDebugger,
            Self::Settings,
            Self::Diagnostics,
            Self::Intiface,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveGateRequest {
    base_url: String,
    creator_username: String,
}

#[derive(Debug, Clone, Default)]
struct LiveGateStatus {
    request: Option<LiveGateRequest>,
    checked_at_ms: i64,
    is_live: Option<bool>,
    stream_id: Option<String>,
    stream_title: Option<String>,
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
enum LiveGateCommand {
    Configure(Option<LiveGateRequest>),
    Trigger,
}

#[derive(Debug, Clone)]
struct CompanionBrowserAuthResult {
    state: String,
    token: Option<String>,
    error: Option<String>,
    mode: Option<String>,
    creator_username: Option<String>,
    stream_id: Option<String>,
    expires_at: Option<String>,
    website_base_url: Option<String>,
    creator_offline: bool,
}

#[derive(Debug)]
struct PendingCompanionBrowserAuth {
    state: String,
    started_at: Instant,
    receiver: Receiver<CompanionBrowserAuthResult>,
}

#[derive(Debug, Clone)]
enum TrayCommand {
    OpenWindow,
    ExitApp,
    DebugEvent(String),
}

struct TrayState {
    _icon: TrayIcon,
    bridge_state_item: MenuItem,
    bridge_toggle_item: MenuItem,
    bridge_toggle_id: MenuId,
    open_id: MenuId,
    exit_id: MenuId,
    #[cfg(target_os = "windows")]
    popup_menu_handle: isize,
    last_state_label: String,
    last_toggle_label: String,
    last_can_toggle: bool,
}

impl TrayState {
    fn new() -> Result<Self, String> {
        #[cfg(target_os = "windows")]
        use tray_icon::menu::ContextMenu;

        let menu = Menu::new();
        let bridge_state_item =
            MenuItem::with_id("tray.bridge.state", "Bridge State: Disabled", false, None);
        let bridge_toggle_item =
            MenuItem::with_id("tray.bridge.toggle", "Toggle Bridge: Enable", true, None);
        let open_item = MenuItem::with_id("tray.window.open", "Open Vibealong", true, None);
        let exit_item = MenuItem::with_id("tray.app.exit", "Exit Vibealong", true, None);
        menu.append(&bridge_state_item).map_err(|e| e.to_string())?;
        menu.append(&bridge_toggle_item)
            .map_err(|e| e.to_string())?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        menu.append(&open_item).map_err(|e| e.to_string())?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        menu.append(&exit_item).map_err(|e| e.to_string())?;

        let icon = load_tray_icon()
            .or_else(|| Icon::from_rgba(tray_icon_rgba(), 16, 16).ok())
            .ok_or_else(|| "Failed to build tray icon".to_string())?;
        #[cfg(target_os = "windows")]
        let popup_menu_handle = menu.hpopupmenu();
        let bridge_toggle_id = bridge_toggle_item.id().clone();
        let tray_icon = TrayIconBuilder::new()
            .with_tooltip("Vibealong")
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .build()
            .map_err(|e| format!("Failed to create tray icon: {e}"))?;

        Ok(Self {
            _icon: tray_icon,
            bridge_state_item,
            bridge_toggle_item,
            bridge_toggle_id,
            open_id: open_item.id().clone(),
            exit_id: exit_item.id().clone(),
            #[cfg(target_os = "windows")]
            popup_menu_handle,
            last_state_label: "Bridge State: Disabled".to_string(),
            last_toggle_label: "Toggle Bridge: Enable".to_string(),
            last_can_toggle: true,
        })
    }
}

fn tray_debug_log_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let local_app_data = std::env::var_os("LOCALAPPDATA")?;
        return Some(
            PathBuf::from(local_app_data)
                .join("Vibealong")
                .join("tray-debug.log"),
        );
    }
    #[allow(unreachable_code)]
    None
}

fn append_tray_debug_log(message: &str) {
    let Some(path) = tray_debug_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let line = format!("[{}] {}\n", unix_ms_now(), message);
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
}

fn open_url_in_system_browser(url: &str) -> Result<(), String> {
    webbrowser::open(url)
        .map(|_| ())
        .map_err(|err| format!("failed to open system browser: {err}"))
}

fn build_http_response(status_line: &str, html_body: &str) -> String {
    format!(
        "{status_line}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html_body.as_bytes().len(),
        html_body
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn build_callback_page_html(title: &str, message: &str, is_error: bool) -> String {
    let title_safe = html_escape(title);
    let message_safe = html_escape(message);
    let accent = if is_error { "#ff6b7a" } else { "#00ff99" };
    let badge = if is_error { "LOGIN ERROR" } else { "VIBEALONG" };

    format!(
        "<!doctype html>\
         <html lang='en'>\
         <head>\
           <meta charset='utf-8'/>\
           <meta name='viewport' content='width=device-width, initial-scale=1'/>\
           <title>{title_safe}</title>\
           <style>\
             :root {{\
               --bg: #0a0a0a; --panel: rgba(18,18,18,.95); --text: #f4f4f4;\
               --muted: #a8b3ae; --accent: {accent};\
             }}\
             * {{ box-sizing: border-box; }}\
             body {{\
               margin: 0; min-height: 100vh; display: grid; place-items: center;\
               color: var(--text); font-family: 'DM Sans','Segoe UI',system-ui,sans-serif;\
               background: radial-gradient(1200px 600px at 50% -10%, rgba(0,255,153,.08), transparent 60%), var(--bg);\
             }}\
             .panel {{\
               width: min(92vw, 680px); padding: 30px 26px; border-radius: 16px;\
               background: var(--panel); border: 1px solid color-mix(in srgb, var(--accent) 48%, transparent);\
               box-shadow: 0 24px 60px rgba(0,0,0,.45);\
             }}\
             .kicker {{\
               margin: 0 0 12px 0; font-size: 12px; letter-spacing: .14em; text-transform: uppercase;\
               color: var(--accent); font-weight: 700;\
             }}\
             h1 {{ margin: 0 0 10px 0; font-size: clamp(28px, 4vw, 44px); line-height: 1.08; }}\
             p {{ margin: 0; color: var(--muted); font-size: 17px; line-height: 1.45; }}\
             .hint {{ margin-top: 18px; font-size: 14px; color: #98a29d; }}\
           </style>\
         </head>\
         <body>\
           <main class='panel'>\
             <p class='kicker'>{badge}</p>\
             <h1>{title_safe}</h1>\
             <p>{message_safe}</p>\
             <p class='hint'>You can return to the Vibealong desktop app now.</p>\
           </main>\
         </body>\
         </html>"
    )
}

fn handle_companion_auth_callback_connection(
    mut stream: TcpStream,
    expected_state: &str,
    result_tx: &Sender<CompanionBrowserAuthResult>,
) -> Result<(), String> {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).map_err(|err| err.to_string())?;
    if bytes_read == 0 {
        return Err("browser callback closed without request".to_string());
    }

    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request
        .lines()
        .next()
        .ok_or_else(|| "invalid callback request".to_string())?;
    let mut request_parts = request_line.split_whitespace();
    let _method = request_parts.next().unwrap_or("");
    let target = request_parts.next().unwrap_or("/");
    let parsed = url::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| "invalid callback URL".to_string())?;

    let state = parsed
        .query_pairs()
        .find_map(|(k, v)| {
            if k == "state" {
                Some(v.into_owned())
            } else {
                None
            }
        })
        .unwrap_or_default();
    let token = parsed
        .query_pairs()
        .find_map(|(k, v)| {
            if k == "token" {
                Some(v.into_owned())
            } else {
                None
            }
        })
        .filter(|value| !value.trim().is_empty());
    let error = parsed
        .query_pairs()
        .find_map(|(k, v)| {
            if k == "error" {
                Some(v.into_owned())
            } else {
                None
            }
        })
        .filter(|value| !value.trim().is_empty());
    let mode = parsed
        .query_pairs()
        .find_map(|(k, v)| {
            if k == "mode" {
                Some(v.into_owned())
            } else {
                None
            }
        })
        .filter(|value| !value.trim().is_empty());
    let creator_username = parsed
        .query_pairs()
        .find_map(|(k, v)| {
            if k == "creatorUsername" {
                Some(v.into_owned())
            } else {
                None
            }
        })
        .filter(|value| !value.trim().is_empty());
    let stream_id = parsed
        .query_pairs()
        .find_map(|(k, v)| {
            if k == "streamId" {
                Some(v.into_owned())
            } else {
                None
            }
        })
        .filter(|value| !value.trim().is_empty());
    let expires_at = parsed
        .query_pairs()
        .find_map(|(k, v)| {
            if k == "expiresAt" {
                Some(v.into_owned())
            } else {
                None
            }
        })
        .filter(|value| !value.trim().is_empty());
    let website_base_url = parsed
        .query_pairs()
        .find_map(|(k, v)| {
            if k == "websiteBaseUrl" {
                Some(v.into_owned())
            } else {
                None
            }
        })
        .filter(|value| !value.trim().is_empty());
    let creator_offline = parsed.query_pairs().any(|(k, v)| {
        if k != "creatorOffline" {
            return false;
        }
        let raw = v.as_ref().trim();
        raw == "1" || raw.eq_ignore_ascii_case("true")
    });

    let (html, result) = if state != expected_state {
        (
            build_callback_page_html(
                "Login Failed",
                "State validation failed. Return to Vibealong and try again.",
                true,
            ),
            CompanionBrowserAuthResult {
                state,
                token: None,
                error: Some("State validation failed".to_string()),
                mode,
                creator_username,
                stream_id,
                expires_at,
                website_base_url,
                creator_offline,
            },
        )
    } else if let Some(error_message) = error {
        (
            build_callback_page_html("Login Failed", &error_message, true),
            CompanionBrowserAuthResult {
                state,
                token: None,
                error: Some(error_message),
                mode,
                creator_username,
                stream_id,
                expires_at,
                website_base_url,
                creator_offline,
            },
        )
    } else if let Some(token_value) = token {
        let success_message = if creator_offline {
            "Vibealong login is complete. Credential was saved and will activate once you are live."
        } else {
            "Vibealong login is complete. Credential was saved successfully."
        };
        (
            build_callback_page_html("You're Signed In", success_message, false),
            CompanionBrowserAuthResult {
                state,
                token: Some(token_value),
                error: None,
                mode,
                creator_username,
                stream_id,
                expires_at,
                website_base_url,
                creator_offline,
            },
        )
    } else {
        (
            build_callback_page_html(
                "Login Failed",
                "No credential was returned. Return to Vibealong and try again.",
                true,
            ),
            CompanionBrowserAuthResult {
                state,
                token: None,
                error: Some("No token returned from browser login".to_string()),
                mode,
                creator_username,
                stream_id,
                expires_at,
                website_base_url,
                creator_offline,
            },
        )
    };

    let response = build_http_response("HTTP/1.1 200 OK", &html);
    let _ = stream.write_all(response.as_bytes());
    let _ = result_tx.send(result);
    Ok(())
}

fn start_companion_auth_callback_listener(
    expected_state: String,
) -> Result<(u16, Receiver<CompanionBrowserAuthResult>), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|err| err.to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|err| err.to_string())?;
    let port = listener.local_addr().map_err(|err| err.to_string())?.port();

    let (tx, rx) = channel::<CompanionBrowserAuthResult>();
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if Instant::now() >= deadline {
                let _ = tx.send(CompanionBrowserAuthResult {
                    state: expected_state.clone(),
                    token: None,
                    error: Some("Timed out waiting for browser callback".to_string()),
                    mode: None,
                    creator_username: None,
                    stream_id: None,
                    expires_at: None,
                    website_base_url: None,
                    creator_offline: false,
                });
                break;
            }

            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = handle_companion_auth_callback_connection(stream, &expected_state, &tx);
                    break;
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(err) => {
                    let _ = tx.send(CompanionBrowserAuthResult {
                        state: expected_state.clone(),
                        token: None,
                        error: Some(format!("Local callback listener failed: {err}")),
                        mode: None,
                        creator_username: None,
                        stream_id: None,
                        expires_at: None,
                        website_base_url: None,
                        creator_offline: false,
                    });
                    break;
                }
            }
        }
    });

    Ok((port, rx))
}

#[cfg(target_os = "windows")]
fn window_title_wide() -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new("Vibealong")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(target_os = "windows")]
fn try_restore_main_window_native() -> bool {
    #[allow(non_snake_case)]
    extern "system" {
        fn FindWindowW(lpClassName: *const u16, lpWindowName: *const u16) -> isize;
        fn ShowWindow(hWnd: isize, nCmdShow: i32) -> i32;
        fn SetForegroundWindow(hWnd: isize) -> i32;
    }

    const SW_RESTORE: i32 = 9;
    let title = window_title_wide();
    // Safety: Win32 calls with static null-terminated title buffer.
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd == 0 {
            return false;
        }
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
    }
    true
}

#[cfg(not(target_os = "windows"))]
fn try_restore_main_window_native() -> bool {
    false
}

#[cfg(target_os = "windows")]
fn try_close_main_window_native() -> bool {
    #[allow(non_snake_case)]
    extern "system" {
        fn FindWindowW(lpClassName: *const u16, lpWindowName: *const u16) -> isize;
        fn PostMessageW(hWnd: isize, Msg: u32, wParam: usize, lParam: isize) -> i32;
    }

    const WM_CLOSE: u32 = 0x0010;
    let title = window_title_wide();
    // Safety: Win32 calls with static null-terminated title buffer.
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd == 0 {
            return false;
        }
        let _ = PostMessageW(hwnd, WM_CLOSE, 0, 0);
    }
    true
}

#[cfg(not(target_os = "windows"))]
fn try_close_main_window_native() -> bool {
    false
}

fn toggle_bridge_from_tray(
    service: &BridgeService,
    shared: &Arc<Mutex<TrayBridgeShared>>,
) -> Result<String, String> {
    let (enable, selected_profile_id) = {
        let mut lock = shared
            .lock()
            .map_err(|_| "Tray bridge state lock poisoned".to_string())?;
        if lock.auto_bridge_enabled {
            lock.auto_bridge_enabled = false;
            (false, lock.selected_profile_id.clone())
        } else {
            lock.auto_bridge_enabled = true;
            (true, lock.selected_profile_id.clone())
        }
    };

    if !enable {
        service
            .stop_runtime()
            .map_err(|err| format!("stop runtime failed: {err}"))?;
        return Ok("Bridge auto mode disabled from tray".to_string());
    }

    let profile_id =
        match selected_profile_id.or_else(|| service.last_selected_profile_id().ok().flatten()) {
            Some(id) => id,
            None => {
                if let Ok(mut lock) = shared.lock() {
                    lock.auto_bridge_enabled = false;
                }
                return Err("No selected profile to start from tray".to_string());
            }
        };

    let loaded = service
        .load_profile(&profile_id)
        .map_err(|err| format!("load profile failed: {err}"))?
        .ok_or_else(|| format!("Profile not found: {profile_id}"))?;
    let has_key = loaded
        .stream_key
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    if !has_key {
        if let Ok(mut lock) = shared.lock() {
            lock.auto_bridge_enabled = false;
        }
        return Err(format!("Profile has no bridge credential: {profile_id}"));
    }

    service
        .start_profile(&profile_id)
        .map_err(|err| format!("start profile failed: {err}"))?;
    Ok(format!("Bridge enabled from tray for profile {profile_id}"))
}

#[cfg(target_os = "windows")]
fn sync_tray_menu_state_native(
    popup_menu_handle: isize,
    service: &BridgeService,
    shared: &Arc<Mutex<TrayBridgeShared>>,
) {
    #[allow(non_snake_case)]
    extern "system" {
        fn GetMenuItemID(hMenu: isize, nPos: i32) -> u32;
        fn ModifyMenuW(
            hMnu: isize,
            uPosition: u32,
            uFlags: u32,
            uIDNewItem: usize,
            lpNewItem: *const u16,
        ) -> i32;
        fn EnableMenuItem(hMenu: isize, uIDEnableItem: u32, uEnable: u32) -> u32;
    }

    const MF_STRING: u32 = 0x0000;
    const MF_BYPOSITION: u32 = 0x0400;
    const MF_ENABLED: u32 = 0x0000;
    const MF_GRAYED: u32 = 0x0001;
    const INVALID_MENU_ID: u32 = u32::MAX;

    fn to_wide(text: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn set_text_by_position(hmenu: isize, position: u32, text: &str) {
        let text_wide = to_wide(text);
        // Safety: valid popup menu handle from muda, changing an existing item by position.
        unsafe {
            let item_id = GetMenuItemID(hmenu, position as i32);
            if item_id == INVALID_MENU_ID {
                return;
            }
            let _ = ModifyMenuW(
                hmenu,
                position,
                MF_BYPOSITION | MF_STRING,
                item_id as usize,
                text_wide.as_ptr(),
            );
        }
    }

    fn set_enabled_by_position(hmenu: isize, position: u32, enabled: bool) {
        let flags = MF_BYPOSITION | if enabled { MF_ENABLED } else { MF_GRAYED };
        // Safety: valid popup menu handle from muda, toggling enabled state by item position.
        unsafe {
            let _ = EnableMenuItem(hmenu, position, flags);
        }
    }

    let (auto_bridge_enabled, selected_profile_id) = match shared.lock() {
        Ok(lock) => (lock.auto_bridge_enabled, lock.selected_profile_id.clone()),
        Err(_) => return,
    };

    let runtime_connected = service.runtime_snapshot().ok().flatten().is_some();
    let state_label = if runtime_connected {
        "Bridge State: Connected"
    } else if auto_bridge_enabled {
        "Bridge State: Enabled"
    } else {
        "Bridge State: Disabled"
    };
    let toggle_label = if auto_bridge_enabled {
        "Toggle Bridge: Disable"
    } else {
        "Toggle Bridge: Enable"
    };

    let selected_has_stream_key = selected_profile_id
        .as_deref()
        .and_then(|profile_id| service.load_profile(profile_id).ok().flatten())
        .and_then(|loaded| loaded.stream_key)
        .is_some_and(|stream_key| !stream_key.trim().is_empty());
    let can_toggle = auto_bridge_enabled || selected_has_stream_key;

    // menu positions: 0 state, 1 toggle, 2 separator, 3 open, 4 separator, 5 exit
    set_text_by_position(popup_menu_handle, 0, state_label);
    set_enabled_by_position(popup_menu_handle, 0, false);
    set_text_by_position(popup_menu_handle, 1, toggle_label);
    set_enabled_by_position(popup_menu_handle, 1, can_toggle);
}

#[cfg(not(target_os = "windows"))]
fn sync_tray_menu_state_native(
    _popup_menu_handle: isize,
    _service: &BridgeService,
    _shared: &Arc<Mutex<TrayBridgeShared>>,
) {
}

fn tray_icon_rgba() -> Vec<u8> {
    let mut out = Vec::with_capacity(16 * 16 * 4);
    for y in 0..16 {
        for x in 0..16 {
            let edge = x == 0 || y == 0 || x == 15 || y == 15;
            let (r, g, b, a) = if edge {
                (18, 110, 76, 255)
            } else {
                (12, 26, 20, 255)
            };
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    out
}

fn app_icon_png_bytes() -> &'static [u8] {
    include_bytes!("../../assets/img/icon.png")
}

fn decode_app_icon_image() -> Option<image::DynamicImage> {
    image::load_from_memory_with_format(app_icon_png_bytes(), image::ImageFormat::Png).ok()
}

fn load_scaled_icon_rgba(size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use image::imageops::FilterType;

    let image = decode_app_icon_image()?;
    let resized = image
        .resize_exact(size, size, FilterType::Lanczos3)
        .into_rgba8();
    let (width, height) = resized.dimensions();
    Some((resized.into_raw(), width, height))
}

fn load_egui_icon_data() -> Option<egui::IconData> {
    let (rgba, width, height) = load_scaled_icon_rgba(128)?;
    Some(egui::IconData {
        rgba,
        width,
        height,
    })
}

fn load_tray_icon() -> Option<Icon> {
    let (rgba, width, height) = load_scaled_icon_rgba(32)?;
    Icon::from_rgba(rgba, width, height).ok()
}

struct DesktopApp {
    service: Arc<BridgeService>,
    db_path: PathBuf,
    profiles: Vec<ProfileSummary>,
    selected_profile_id: Option<String>,
    selected_profile_has_stream_key: bool,
    auto_bridge_enabled: bool,
    status_message: String,
    status_is_error: bool,
    stream_key_hidden: bool,
    form: ProfileForm,
    tab: DesktopTab,

    runtime_snapshot: Option<RuntimeSnapshot>,
    runtime_logs: Vec<RuntimeLogLine>,
    avatar_params: Vec<(String, RuntimeParamValue)>,
    show_all_avatar_params: bool,
    last_runtime_refresh: Instant,

    diagnostics: VrchatDiagnostics,
    diagnostics_shared: Arc<Mutex<VrchatDiagnostics>>,
    diagnostics_refresh_tx: Sender<()>,
    live_gate_status: LiveGateStatus,
    live_gate_shared: Arc<Mutex<LiveGateStatus>>,
    live_gate_tx: Sender<LiveGateCommand>,
    live_gate_configured_request: Option<LiveGateRequest>,
    next_auto_bridge_attempt_at: Instant,
    pending_companion_auth: Option<PendingCompanionBrowserAuth>,
    oscquery_client: Option<OscQueryClient>,
    oscquery_status: OscQueryStatus,
    oscquery_values: Vec<(String, String)>,

    intiface_form: IntifaceForm,
    intiface_snapshot: IntifaceSnapshot,
    intiface_bridge: Option<IntifaceBridgeHandle>,
    intiface_bridge_snapshot: IntifaceBridgeSnapshot,
    intiface_routes: Vec<IntifaceRouteForm>,
    style_applied: bool,
    close_to_background: bool,
    hidden_to_tray: bool,
    tray: Option<TrayState>,
    tray_command_rx: Receiver<TrayCommand>,
    tray_bridge_shared: Arc<Mutex<TrayBridgeShared>>,
}

#[derive(Debug, Clone, Default)]
struct TrayBridgeShared {
    selected_profile_id: Option<String>,
    auto_bridge_enabled: bool,
}

#[derive(Debug, Clone)]
struct IntifaceForm {
    host: String,
    port: String,
    secure: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntifaceSourceMode {
    Intensity,
    AvatarParam,
}

#[derive(Debug, Clone)]
struct IntifaceRouteForm {
    enabled: bool,
    label: String,
    target_device_contains: String,
    target_actuator_type: String,
    source_mode: IntifaceSourceMode,
    source_param: String,
    scale: f64,
    idle: f64,
    min_output: f64,
    max_output: f64,
    invert: bool,
}

impl Default for IntifaceRouteForm {
    fn default() -> Self {
        Self {
            enabled: true,
            label: "All Scalar <- Intensity".to_string(),
            target_device_contains: String::new(),
            target_actuator_type: String::new(),
            source_mode: IntifaceSourceMode::Intensity,
            source_param: "SPS_Contact".to_string(),
            scale: 1.0,
            idle: 0.0,
            min_output: 0.0,
            max_output: 1.0,
            invert: false,
        }
    }
}

impl Default for IntifaceForm {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: "12345".to_string(),
            secure: false,
        }
    }
}

#[derive(Debug, Clone)]
struct ProfileForm {
    name: String,
    creator_username: String,
    stream_key: String,
    website_base_url: String,
    allow_insecure_http: bool,
    osc_host: String,
    osc_port: String,
    allow_network_osc: bool,
    osc_allowed_senders: String,
    mappings: Vec<MappingRowForm>,
    forward_targets: String,
}

#[derive(Debug, Clone)]
struct MappingRowForm {
    address: String,
    weight: String,
    deadzone: String,
    invert: bool,
    curve: Curve,
    min: String,
    max: String,
}

impl Default for ProfileForm {
    fn default() -> Self {
        Self {
            name: "Default".to_string(),
            creator_username: String::new(),
            stream_key: String::new(),
            website_base_url: "https://vrlewds.com".to_string(),
            allow_insecure_http: false,
            osc_host: "127.0.0.1".to_string(),
            osc_port: "9001".to_string(),
            allow_network_osc: false,
            osc_allowed_senders: String::new(),
            mappings: Vec::new(),
            forward_targets: "127.0.0.1:9000".to_string(),
        }
    }
}

impl Default for MappingRowForm {
    fn default() -> Self {
        Self {
            address: String::new(),
            weight: "1.0".to_string(),
            deadzone: "0.02".to_string(),
            invert: false,
            curve: Curve::Linear,
            min: "0.0".to_string(),
            max: "1.0".to_string(),
        }
    }
}

impl DesktopApp {
    fn new(service: BridgeService, db_path: PathBuf, egui_ctx: egui::Context) -> Self {
        let service = Arc::new(service);
        let tray_bridge_shared = Arc::new(Mutex::new(TrayBridgeShared::default()));
        let diagnostics_shared = Arc::new(Mutex::new(VrchatDiagnostics::default()));
        let (diagnostics_refresh_tx, diagnostics_refresh_rx) = channel::<()>();
        let diagnostics_thread_state = Arc::clone(&diagnostics_shared);
        thread::spawn(move || loop {
            let latest = collect_vrchat_diagnostics(None);
            if let Ok(mut lock) = diagnostics_thread_state.lock() {
                *lock = latest;
            }
            match diagnostics_refresh_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        });

        let live_gate_shared = Arc::new(Mutex::new(LiveGateStatus::default()));
        let (live_gate_tx, live_gate_rx) = channel::<LiveGateCommand>();
        let live_gate_thread_state = Arc::clone(&live_gate_shared);
        thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(4))
                .build()
                .ok();
            let mut configured_request: Option<LiveGateRequest> = None;
            loop {
                let mut should_poll = false;
                match live_gate_rx.recv_timeout(Duration::from_secs(4)) {
                    Ok(LiveGateCommand::Configure(request)) => {
                        configured_request = request;
                        should_poll = true;
                    }
                    Ok(LiveGateCommand::Trigger) => should_poll = true,
                    Err(RecvTimeoutError::Timeout) => {
                        if configured_request.is_some() {
                            should_poll = true;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                while let Ok(command) = live_gate_rx.try_recv() {
                    match command {
                        LiveGateCommand::Configure(request) => {
                            configured_request = request;
                            should_poll = true;
                        }
                        LiveGateCommand::Trigger => should_poll = true,
                    }
                }

                let latest = if !should_poll {
                    None
                } else if let Some(request) = configured_request.as_ref() {
                    Some(check_stream_live(request, client.as_ref()))
                } else {
                    Some(LiveGateStatus {
                        request: None,
                        checked_at_ms: unix_ms_now(),
                        is_live: None,
                        stream_id: None,
                        stream_title: None,
                        last_error: None,
                    })
                };

                if let Some(latest) = latest {
                    if let Ok(mut lock) = live_gate_thread_state.lock() {
                        *lock = latest;
                    }
                }
            }
        });

        let (tray_command_tx, tray_command_rx) = channel::<TrayCommand>();

        let mut app = Self {
            service: Arc::clone(&service),
            db_path,
            profiles: Vec::new(),
            selected_profile_id: None,
            selected_profile_has_stream_key: false,
            auto_bridge_enabled: false,
            status_message: String::new(),
            status_is_error: false,
            stream_key_hidden: true,
            form: ProfileForm::default(),
            tab: DesktopTab::Login,
            runtime_snapshot: None,
            runtime_logs: Vec::new(),
            avatar_params: Vec::new(),
            show_all_avatar_params: false,
            last_runtime_refresh: Instant::now() - Duration::from_secs(1),
            diagnostics: VrchatDiagnostics::default(),
            diagnostics_shared,
            diagnostics_refresh_tx,
            live_gate_status: LiveGateStatus::default(),
            live_gate_shared,
            live_gate_tx,
            live_gate_configured_request: None,
            next_auto_bridge_attempt_at: Instant::now(),
            pending_companion_auth: None,
            oscquery_client: OscQueryClient::new().ok(),
            oscquery_status: OscQueryStatus::default(),
            oscquery_values: Vec::new(),
            intiface_form: IntifaceForm::default(),
            intiface_snapshot: IntifaceSnapshot::default(),
            intiface_bridge: None,
            intiface_bridge_snapshot: IntifaceBridgeSnapshot::default(),
            intiface_routes: vec![IntifaceRouteForm::default()],
            style_applied: false,
            close_to_background: true,
            hidden_to_tray: false,
            tray: None,
            tray_command_rx,
            tray_bridge_shared: Arc::clone(&tray_bridge_shared),
        };
        app.tray = TrayState::new().ok();
        if let Some(tray) = app.tray.as_ref() {
            append_tray_debug_log("tray-init-success");
            let tx = tray_command_tx.clone();
            let toggle_id = tray.bridge_toggle_id.clone();
            let open_id = tray.open_id.clone();
            let exit_id = tray.exit_id.clone();
            #[cfg(target_os = "windows")]
            let popup_menu_handle = tray.popup_menu_handle;
            let menu_ctx = egui_ctx.clone();
            let tray_service = Arc::clone(&service);
            let tray_shared = Arc::clone(&tray_bridge_shared);
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                append_tray_debug_log(&format!("menu-event: {}", event.id.as_ref()));
                if let Err(err) = tx.send(TrayCommand::DebugEvent(event.id.as_ref().to_string())) {
                    append_tray_debug_log(&format!("menu-event-debug-send-failed: {err}"));
                }
                if event.id == toggle_id {
                    let toggle_message = match toggle_bridge_from_tray(&tray_service, &tray_shared)
                    {
                        Ok(message) => {
                            append_tray_debug_log("menu-event-action: toggle-ok");
                            message
                        }
                        Err(err) => {
                            append_tray_debug_log(&format!(
                                "menu-event-action: toggle-failed: {err}"
                            ));
                            err
                        }
                    };
                    #[cfg(target_os = "windows")]
                    sync_tray_menu_state_native(popup_menu_handle, &tray_service, &tray_shared);
                    if let Err(err) = tx.send(TrayCommand::DebugEvent(toggle_message)) {
                        append_tray_debug_log(&format!("menu-event-toggle-send-failed: {err}"));
                    }
                } else if event.id == open_id {
                    append_tray_debug_log("menu-event-action: open");
                    menu_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    menu_ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    menu_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    let native_restore = try_restore_main_window_native();
                    append_tray_debug_log(&format!(
                        "menu-event-action: open-native-restore={native_restore}"
                    ));
                    #[cfg(target_os = "windows")]
                    sync_tray_menu_state_native(popup_menu_handle, &tray_service, &tray_shared);
                    if let Err(err) = tx.send(TrayCommand::OpenWindow) {
                        append_tray_debug_log(&format!("menu-event-open-send-failed: {err}"));
                    }
                } else if event.id == exit_id {
                    append_tray_debug_log("menu-event-action: exit");
                    // Ensure update loop resumes even when currently hidden to tray.
                    menu_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    menu_ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    let _ = tray_service.stop_runtime();
                    menu_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    let native_close = try_close_main_window_native();
                    append_tray_debug_log(&format!(
                        "menu-event-action: exit-native-close={native_close}"
                    ));
                    #[cfg(target_os = "windows")]
                    sync_tray_menu_state_native(popup_menu_handle, &tray_service, &tray_shared);
                    if let Err(err) = tx.send(TrayCommand::ExitApp) {
                        append_tray_debug_log(&format!("menu-event-exit-send-failed: {err}"));
                    }
                }
                menu_ctx.request_repaint();
            }));
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                append_tray_debug_log(&format!("tray-icon-event: {:?}", event));
            }));
        } else {
            append_tray_debug_log("tray-init-failed");
        }
        app.refresh_profiles();
        app.restore_close_to_background_preference();
        app.restore_last_selected_profile();
        app.tab = if app.selected_profile_has_stream_key {
            DesktopTab::Home
        } else {
            DesktopTab::Login
        };
        app.refresh_runtime_views(false);
        app.refresh_diagnostics_from_worker();
        app.sync_tray_bridge_shared();
        app.refresh_tray_menu_state();
        app
    }

    fn set_status(&mut self, message: impl Into<String>, is_error: bool) {
        self.status_message = message.into();
        self.status_is_error = is_error;
    }

    fn persist_selected_profile_preference(&mut self) {
        if let Err(err) = self
            .service
            .set_last_selected_profile_id(self.selected_profile_id.as_deref())
        {
            self.set_status(format!("Failed to persist selected profile: {err}"), true);
        }
    }

    fn persist_close_to_background_preference(&mut self) {
        if let Err(err) = self
            .service
            .set_close_to_background_preference(self.close_to_background)
        {
            self.set_status(
                format!("Failed to persist close-to-background setting: {err}"),
                true,
            );
        }
    }

    fn restore_close_to_background_preference(&mut self) {
        let Ok(preference) = self.service.close_to_background_preference() else {
            return;
        };
        if let Some(enabled) = preference {
            self.close_to_background = enabled;
        }
    }

    fn restore_last_selected_profile(&mut self) {
        let Ok(last_selected) = self.service.last_selected_profile_id() else {
            return;
        };
        if let Some(profile_id) = last_selected {
            if self.profiles.iter().any(|profile| profile.id == profile_id) {
                self.load_profile_into_form(&profile_id);
                return;
            }
            let _ = self.service.set_last_selected_profile_id(None);
        }

        if let Some(first_profile_id) = self.profiles.first().map(|profile| profile.id.clone()) {
            self.load_profile_into_form(&first_profile_id);
            let _ = self
                .service
                .set_last_selected_profile_id(Some(&first_profile_id));
        } else {
            self.reset_form_for_new_profile();
        }
    }

    fn refresh_profiles(&mut self) {
        match self.service.list_profiles() {
            Ok(profiles) => {
                self.profiles = profiles;
                if self
                    .selected_profile_id
                    .as_ref()
                    .is_some_and(|id| !self.profiles.iter().any(|profile| &profile.id == id))
                {
                    self.selected_profile_id = None;
                    self.selected_profile_has_stream_key = false;
                    self.persist_selected_profile_preference();
                }
                self.sync_tray_bridge_shared();
            }
            Err(err) => self.set_status(format!("Failed to load profiles: {err}"), true),
        }
    }

    fn sync_tray_bridge_shared(&self) {
        let Ok(mut shared) = self.tray_bridge_shared.lock() else {
            return;
        };
        shared.selected_profile_id = self.selected_profile_id.clone();
        shared.auto_bridge_enabled = self.auto_bridge_enabled;
    }

    fn apply_tray_bridge_shared(&mut self) {
        let Ok(shared) = self.tray_bridge_shared.lock() else {
            return;
        };
        self.auto_bridge_enabled = shared.auto_bridge_enabled;
    }

    fn reset_form_for_new_profile(&mut self) {
        self.form = ProfileForm::default();
        self.selected_profile_id = Some("default".to_string());
        self.selected_profile_has_stream_key = false;
        self.auto_bridge_enabled = false;
        self.persist_selected_profile_preference();
        self.sync_live_gate_worker_request(true);
        self.sync_tray_bridge_shared();
    }

    fn load_profile_into_form(&mut self, profile_id: &str) {
        match self.service.load_profile(profile_id) {
            Ok(Some(loaded)) => {
                self.form.name = loaded.profile.name.clone();
                self.form.creator_username = loaded.profile.config.creator_username.clone();
                self.form.stream_key.clear();
                self.form.website_base_url = loaded.profile.config.website_base_url.clone();
                self.form.allow_insecure_http = loaded.profile.config.allow_insecure_http;
                self.form.osc_host = loaded.profile.config.osc_listen.host.clone();
                self.form.osc_port = loaded.profile.config.osc_listen.port.to_string();
                self.form.allow_network_osc = loaded.profile.config.allow_network_osc;
                self.form.osc_allowed_senders =
                    loaded.profile.config.osc_allowed_senders.join("\n");
                self.form.forward_targets = loaded
                    .profile
                    .config
                    .forward_targets
                    .iter()
                    .map(|target| format!("{}:{}", target.host, target.port))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.form.mappings = loaded
                    .profile
                    .config
                    .mappings
                    .iter()
                    .map(mapping_to_row_form)
                    .collect::<Vec<_>>();
                self.selected_profile_id = Some(profile_id.to_string());
                self.selected_profile_has_stream_key = loaded.stream_key.is_some();
                self.persist_selected_profile_preference();
                self.set_status(
                    format!(
                        "Loaded profile {} ({})",
                        loaded.profile.name,
                        if self.selected_profile_has_stream_key {
                            "credential present"
                        } else {
                            "credential missing"
                        }
                    ),
                    false,
                );
                self.sync_live_gate_worker_request(true);
                self.sync_tray_bridge_shared();
            }
            Ok(None) => self.set_status(format!("Profile not found: {profile_id}"), true),
            Err(err) => self.set_status(format!("Failed to load profile: {err}"), true),
        }
    }

    fn refresh_runtime_views(&mut self, include_logs: bool) {
        self.runtime_snapshot = self.service.runtime_snapshot().ok().flatten();
        if include_logs {
            self.runtime_logs = self.service.runtime_logs(1000).unwrap_or_default();
        }
        let runtime_params = self
            .service
            .runtime_avatar_params()
            .unwrap_or_default()
            .into_iter()
            .filter(|(key, _)| self.should_list_osc_param_key(key))
            .collect::<Vec<_>>();
        if !runtime_params.is_empty() {
            let mut merged = self
                .avatar_params
                .iter()
                .cloned()
                .collect::<std::collections::HashMap<_, _>>();
            for (key, value) in runtime_params {
                merged.insert(key, value);
            }
            let mut merged_list = merged.into_iter().collect::<Vec<_>>();
            merged_list.sort_by(|a, b| a.0.cmp(&b.0));
            self.avatar_params = merged_list;
        }
        self.apply_avatar_param_visibility_filter();
    }

    fn refresh_diagnostics_from_worker(&mut self) {
        if let Ok(diag) = self.diagnostics_shared.lock() {
            self.diagnostics = diag.clone();
        }
    }

    fn request_diagnostics_refresh(&self) {
        let _ = self.diagnostics_refresh_tx.send(());
    }

    fn current_live_gate_request(&self) -> Option<LiveGateRequest> {
        if !self.auto_bridge_enabled || self.selected_profile_id.is_none() {
            return None;
        }
        let base_url = self.form.website_base_url.trim().to_string();
        let creator_username = self.form.creator_username.trim().to_string();
        if base_url.is_empty() || creator_username.is_empty() {
            return None;
        }
        Some(LiveGateRequest {
            base_url,
            creator_username,
        })
    }

    fn sync_live_gate_worker_request(&mut self, force_trigger: bool) {
        let desired = self.current_live_gate_request();
        if self.live_gate_configured_request != desired {
            self.live_gate_configured_request = desired.clone();
            let _ = self.live_gate_tx.send(LiveGateCommand::Configure(desired));
            return;
        }
        if force_trigger {
            let _ = self.live_gate_tx.send(LiveGateCommand::Trigger);
        }
    }

    fn refresh_live_gate_from_worker(&mut self) {
        if let Ok(status) = self.live_gate_shared.lock() {
            self.live_gate_status = status.clone();
        }
    }

    fn apply_auto_bridge_policy(&mut self) {
        if !self.auto_bridge_enabled {
            return;
        }
        let Some(profile_id) = self.selected_profile_id.clone() else {
            return;
        };
        let Some(expected_request) = self.current_live_gate_request() else {
            return;
        };
        if self.live_gate_status.request.as_ref() != Some(&expected_request) {
            return;
        }

        match self.live_gate_status.is_live {
            Some(true) => {
                if self.runtime_running() || Instant::now() < self.next_auto_bridge_attempt_at {
                    return;
                }
                match self.service.start_profile(&profile_id) {
                    Ok(_) => self.set_status(
                        format!(
                            "Bridge auto-engaged for {} (stream live)",
                            expected_request.creator_username
                        ),
                        false,
                    ),
                    Err(err) => {
                        self.next_auto_bridge_attempt_at = Instant::now() + Duration::from_secs(5);
                        self.set_status(format!("Auto-engage failed: {err}"), true);
                    }
                }
            }
            Some(false) => {
                if self.runtime_running() {
                    match self.service.stop_runtime() {
                        Ok(_) => self.set_status(
                            format!(
                                "Bridge auto-disengaged for {} (stream offline)",
                                expected_request.creator_username
                            ),
                            false,
                        ),
                        Err(err) => self.set_status(format!("Auto-disengage failed: {err}"), true),
                    }
                }
            }
            None => {}
        }
    }

    fn enable_bridge_auto_mode(&mut self, source: &str) {
        let can_start = self.selected_profile_id.is_some() && self.selected_profile_has_stream_key;
        if !can_start {
            self.set_status(
                "Bridge enable blocked: configure credential in Settings first",
                true,
            );
            return;
        }
        if self.auto_bridge_enabled {
            self.set_status(
                format!("Bridge auto mode already enabled ({source})"),
                false,
            );
            return;
        }
        self.auto_bridge_enabled = true;
        self.next_auto_bridge_attempt_at = Instant::now();
        self.sync_live_gate_worker_request(true);
        self.sync_tray_bridge_shared();
        self.set_status(format!("Bridge auto mode enabled ({source})"), false);
    }

    fn disable_bridge_auto_mode(&mut self, source: &str) {
        self.auto_bridge_enabled = false;
        self.sync_live_gate_worker_request(true);
        self.sync_tray_bridge_shared();
        if self.runtime_running() {
            match self.service.stop_runtime() {
                Ok(_) => self.set_status(
                    format!("Bridge disengaged and auto mode disabled ({source})"),
                    false,
                ),
                Err(err) => self.set_status(format!("Failed to stop bridge: {err}"), true),
            }
        } else {
            self.set_status(format!("Bridge auto mode disabled ({source})"), false);
        }
    }

    fn parse_forward_target(line: &str) -> Result<ForwardTarget, String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err("empty line".to_string());
        }
        let mut parts = trimmed.split(':');
        let host = parts.next().unwrap_or("").trim().to_string();
        let port_raw = parts.next().unwrap_or("").trim();
        if host.is_empty() || port_raw.is_empty() || parts.next().is_some() {
            return Err(format!(
                "Invalid forward target '{trimmed}', expected host:port"
            ));
        }
        let port: u16 = port_raw
            .parse()
            .map_err(|_| format!("Invalid port in forward target '{trimmed}'"))?;
        if port == 0 {
            return Err(format!("Invalid port in forward target '{trimmed}'"));
        }
        Ok(ForwardTarget { host, port })
    }

    fn parse_osc_allowed_senders(raw: &str) -> Result<Vec<String>, String> {
        let mut senders = Vec::new();
        for (idx, line) in raw.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed: IpAddr = trimmed.parse().map_err(|_| {
                format!(
                    "Allowed sender line {} is invalid IP address: '{}'",
                    idx + 1,
                    trimmed
                )
            })?;
            let normalized = parsed.to_string();
            if !senders.iter().any(|existing| existing == &normalized) {
                senders.push(normalized);
            }
        }
        Ok(senders)
    }

    fn start_companion_browser_login(&mut self) {
        if self.pending_companion_auth.is_some() {
            self.set_status(
                "Companion login is already in progress. Finish the browser flow first.",
                true,
            );
            return;
        }

        let base = self.form.website_base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            self.set_status(
                "Website Base URL is required before opening Vibealong login",
                true,
            );
            return;
        }
        let base_url = match url::Url::parse(base) {
            Ok(url) => url,
            Err(err) => {
                self.set_status(format!("Website Base URL is invalid: {err}"), true);
                return;
            }
        };
        if !self.form.allow_insecure_http && base_url.scheme() != "https" {
            self.set_status(
                "Website Base URL must use HTTPS unless 'Allow Insecure HTTP' is enabled.",
                true,
            );
            return;
        }
        if self.form.allow_insecure_http
            && base_url.scheme() != "https"
            && base_url.scheme() != "http"
        {
            self.set_status("Website Base URL must use HTTP or HTTPS", true);
            return;
        }

        let state = uuid::Uuid::new_v4().to_string();
        let (callback_port, callback_rx) =
            match start_companion_auth_callback_listener(state.clone()) {
                Ok(value) => value,
                Err(err) => {
                    self.set_status(
                        format!("Failed to start local auth callback listener: {err}"),
                        true,
                    );
                    return;
                }
            };
        let callback_url = format!("http://127.0.0.1:{callback_port}/callback");

        let login_url = match base_url.join("/api/vibealong/companion/desktop-login") {
            Ok(mut url) => {
                url.query_pairs_mut()
                    .append_pair("mode", "creator")
                    .append_pair("state", &state)
                    .append_pair("redirect_uri", &callback_url);
                url.to_string()
            }
            Err(err) => {
                self.set_status(format!("Failed to build login URL: {err}"), true);
                return;
            }
        };

        match open_url_in_system_browser(&login_url) {
            Ok(_) => {
                self.pending_companion_auth = Some(PendingCompanionBrowserAuth {
                    state,
                    started_at: Instant::now(),
                    receiver: callback_rx,
                });
                self.set_status(
                    format!(
                        "Opened browser login. Complete sign-in there; credential will be applied automatically. Callback: {callback_url}"
                    ),
                    false,
                );
            }
            Err(err) => {
                self.set_status(
                    format!("Failed to open browser: {err}. Open manually: {login_url}"),
                    true,
                );
            }
        }
    }

    fn apply_companion_browser_auth_result(&mut self, result: CompanionBrowserAuthResult) {
        if let Some(error) = result.error {
            self.set_status(format!("Companion login failed: {error}"), true);
            return;
        }
        let token = match result.token {
            Some(value) if !value.trim().is_empty() => value,
            _ => {
                self.set_status("Companion login failed: empty token", true);
                return;
            }
        };

        if self.form.creator_username.trim().is_empty() {
            if let Some(username) = result.creator_username.clone() {
                self.form.creator_username = username;
            }
        }
        if self.form.website_base_url.trim().is_empty() {
            if let Some(base_url) = result.website_base_url.clone() {
                self.form.website_base_url = base_url;
            }
        }

        self.form.stream_key = token;
        self.save_current_configuration();

        if self.selected_profile_has_stream_key {
            self.tab = DesktopTab::Setup;
            let stream_detail = result
                .stream_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!(" stream={value}"))
                .unwrap_or_default();
            let expires_detail = result
                .expires_at
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!(" expires={value}"))
                .unwrap_or_default();
            let mode_detail = result
                .mode
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!(" mode={value}"))
                .unwrap_or_default();
            let offline_detail = if result.creator_offline {
                " creator is offline; bridge will engage once stream is live".to_string()
            } else {
                String::new()
            };
            self.set_status(
                format!(
                    "Companion login complete. Credential saved.{}{}{}{}",
                    mode_detail, stream_detail, expires_detail, offline_detail
                ),
                false,
            );
        }
    }

    fn poll_companion_browser_login(&mut self) {
        let mut timed_out = false;
        let mut received: Option<CompanionBrowserAuthResult> = None;
        let mut disconnected = false;

        if let Some(pending) = self.pending_companion_auth.as_ref() {
            if pending.started_at.elapsed() >= Duration::from_secs(300) {
                timed_out = true;
            } else {
                match pending.receiver.try_recv() {
                    Ok(result) => received = Some(result),
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => disconnected = true,
                }
            }
        }

        if timed_out {
            self.pending_companion_auth = None;
            self.set_status("Companion login timed out; start login again.", true);
            return;
        }
        if disconnected {
            self.pending_companion_auth = None;
            self.set_status("Companion login listener disconnected unexpectedly.", true);
            return;
        }
        let Some(result) = received else {
            return;
        };
        let expected_state = self
            .pending_companion_auth
            .as_ref()
            .map(|pending| pending.state.clone())
            .unwrap_or_default();
        self.pending_companion_auth = None;

        if expected_state.is_empty() || result.state != expected_state {
            self.set_status("Companion login failed: state mismatch", true);
            return;
        }

        self.apply_companion_browser_auth_result(result);
    }

    fn normalized_stream_key_input(&self) -> String {
        self.form
            .stream_key
            .replace('\r', "")
            .replace('\n', "")
            .trim()
            .to_string()
    }

    fn build_profile_from_form(&self) -> Result<(AppProfile, String), String> {
        let parse_or_default =
            |raw: &str, default: f64, field: &str, row_idx: usize| -> Result<f64, String> {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Ok(default);
                }
                trimmed.parse::<f64>().map_err(|_| {
                    format!("Mapping {}: {} must be a valid number", row_idx + 1, field)
                })
            };

        let name = self.form.name.trim().to_string();
        if name.is_empty() {
            return Err("Profile name is required".to_string());
        }
        let creator_username = self.form.creator_username.trim().to_string();
        if creator_username.is_empty() {
            return Err("Creator username is required".to_string());
        }
        let osc_port: u16 = self
            .form
            .osc_port
            .trim()
            .parse()
            .map_err(|_| "OSC port must be a valid integer".to_string())?;
        if osc_port == 0 {
            return Err("OSC port must be > 0".to_string());
        }

        let mut mappings = Vec::new();
        for (idx, row) in self.form.mappings.iter().enumerate() {
            let address = row.address.trim();
            if address.is_empty() {
                continue;
            }
            if !address.starts_with('/') {
                return Err(format!("Mapping {}: address must start with '/'", idx + 1));
            }
            let weight = parse_or_default(&row.weight, 1.0, "weight", idx)?;
            let deadzone = parse_or_default(&row.deadzone, 0.02, "deadzone", idx)?;
            let min = parse_or_default(&row.min, 0.0, "min", idx)?;
            let max = parse_or_default(&row.max, 1.0, "max", idx)?;
            let mapping = Mapping::new(address, weight, deadzone, row.invert, row.curve, min, max)
                .ok_or_else(|| format!("Mapping {}: invalid values", idx + 1))?;
            mappings.push(mapping);
        }

        let mut forward_targets = Vec::new();
        for line in self.form.forward_targets.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            forward_targets.push(Self::parse_forward_target(trimmed)?);
        }
        let osc_allowed_senders = Self::parse_osc_allowed_senders(&self.form.osc_allowed_senders)?;

        let stream_key = {
            let input_key = self.normalized_stream_key_input();
            if !input_key.is_empty() {
                input_key
            } else if let Some(profile_id) = self.selected_profile_id.as_ref() {
                match self.service.load_profile(profile_id) {
                    Ok(Some(loaded)) => loaded.stream_key.ok_or_else(|| {
                        "Stream key is required for this profile. Enter a key and verify the 'chars entered' indicator is greater than 0.".to_string()
                    })?,
                    Ok(None) => {
                        return Err(
                            "Selected profile could not be loaded. Refresh and try again."
                                .to_string(),
                        )
                    }
                    Err(err) => {
                        return Err(format!("Failed to load existing credential: {err}"));
                    }
                }
            } else {
                return Err("Stream key is required".to_string());
            }
        };

        let config = InAppConfig {
            website_base_url: self.form.website_base_url.trim().to_string(),
            creator_username,
            allow_insecure_http: self.form.allow_insecure_http,
            osc_listen: OscListen {
                host: self.form.osc_host.trim().to_string(),
                port: osc_port,
            },
            allow_network_osc: self.form.allow_network_osc,
            osc_allowed_senders,
            relay: RelayPaths {
                session_path: "/api/sps/session".to_string(),
                ingest_path: "/api/sps/ingest".to_string(),
            },
            debug: DebugConfig {
                log_osc: false,
                log_unmapped_only: false,
                log_configured_only: false,
                log_relay: false,
            },
            discovery: DiscoveryConfig {
                enabled: true,
                file_path: "data/osc-discovered.txt".into(),
                include_arg_types: false,
            },
            forward_targets,
            mappings,
            output: OutputTuning::default(),
        };

        let profile = AppProfile {
            id: self
                .selected_profile_id
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            name,
            config,
        };
        Ok((profile, stream_key))
    }

    fn save_current_configuration(&mut self) {
        match self.build_profile_from_form() {
            Ok((profile, stream_key)) => match self.service.upsert_profile(&profile, &stream_key) {
                Ok(profile_id) => {
                    self.selected_profile_id = Some(profile_id.clone());
                    self.selected_profile_has_stream_key = true;
                    self.form.stream_key.clear();
                    self.refresh_profiles();
                    self.persist_selected_profile_preference();
                    self.sync_live_gate_worker_request(true);
                    self.sync_tray_bridge_shared();
                    self.set_status(format!("Saved settings ({profile_id})"), false);
                }
                Err(err) => self.set_status(format!("Failed to save settings: {err}"), true),
            },
            Err(err) => self.set_status(err, true),
        }
    }

    fn logout_current_profile(&mut self) {
        let Some(profile_id) = self.selected_profile_id.clone() else {
            self.selected_profile_has_stream_key = false;
            self.form.stream_key.clear();
            self.form.website_base_url = "https://vrlewds.com".to_string();
            self.tab = DesktopTab::Login;
            self.set_status("Logged out.", false);
            return;
        };

        self.auto_bridge_enabled = false;
        if self.runtime_running() {
            let _ = self.service.stop_runtime();
        }

        match self.service.clear_stream_key(&profile_id) {
            Ok(()) => {
                self.selected_profile_has_stream_key = false;
                self.form.stream_key.clear();
                self.form.website_base_url = "https://vrlewds.com".to_string();
                self.pending_companion_auth = None;
                self.tab = DesktopTab::Login;
                self.refresh_profiles();
                self.sync_live_gate_worker_request(true);
                self.sync_tray_bridge_shared();
                self.set_status("Logged out. Sign in again to continue.", false);
            }
            Err(err) => {
                self.set_status(format!("Failed to log out: {err}"), true);
            }
        }
    }

    fn runtime_running(&self) -> bool {
        self.runtime_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.running)
    }

    fn build_intiface_source_values(&self) -> std::collections::HashMap<String, f64> {
        let mut values = std::collections::HashMap::new();
        let intensity = self
            .runtime_snapshot
            .as_ref()
            .map(|snapshot| snapshot.current_intensity)
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        values.insert("intensity".to_string(), intensity);

        for (key, value) in &self.avatar_params {
            let numeric = match value {
                RuntimeParamValue::Number(v) => Some((*v).clamp(0.0, 1.0)),
                RuntimeParamValue::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
                RuntimeParamValue::Text(v) => v.parse::<f64>().ok().map(|n| n.clamp(0.0, 1.0)),
            };
            if let Some(numeric) = numeric {
                values.insert(format!("avatar:{key}"), numeric);
            }
        }
        values
    }

    fn build_intiface_route_rules(&self) -> Vec<IntifaceRouteRule> {
        self.intiface_routes
            .iter()
            .map(|route| IntifaceRouteRule {
                enabled: route.enabled,
                label: route.label.clone(),
                target_device_contains: route.target_device_contains.clone(),
                target_actuator_type: route.target_actuator_type.clone(),
                source: match route.source_mode {
                    IntifaceSourceMode::Intensity => IntifaceSourceKind::Intensity,
                    IntifaceSourceMode::AvatarParam => {
                        IntifaceSourceKind::AvatarParam(route.source_param.clone())
                    }
                },
                scale: route.scale,
                idle: route.idle,
                min_output: route.min_output,
                max_output: route.max_output,
                invert: route.invert,
            })
            .collect()
    }

    fn parse_intiface_config(&self) -> Result<IntifaceConfig, String> {
        let host = self.intiface_form.host.trim().to_string();
        if host.is_empty() {
            return Err("Intiface host is required".to_string());
        }
        let port = self
            .intiface_form
            .port
            .trim()
            .parse::<u16>()
            .map_err(|_| "Intiface port must be a valid integer".to_string())?;
        if port == 0 {
            return Err("Intiface port must be > 0".to_string());
        }
        Ok(IntifaceConfig {
            host,
            port,
            secure: self.intiface_form.secure,
        })
    }

    fn probe_intiface(&mut self) {
        match self.parse_intiface_config() {
            Ok(config) => {
                let client = IntifaceClient::new(config);
                self.intiface_snapshot = client.probe_snapshot();
                if self.intiface_snapshot.connected {
                    self.set_status("Intiface probe succeeded", false);
                } else {
                    self.set_status(
                        format!(
                            "Intiface probe failed: {}",
                            self.intiface_snapshot
                                .last_error
                                .clone()
                                .unwrap_or_else(|| "unknown error".to_string())
                        ),
                        true,
                    );
                }
            }
            Err(err) => self.set_status(err, true),
        }
    }

    fn engage_intiface_bridge(&mut self) {
        if self.intiface_bridge.is_some() {
            return;
        }
        match self.parse_intiface_config() {
            Ok(config) => {
                let bridge_config = IntifaceBridgeConfig {
                    intiface: config,
                    routes: self.build_intiface_route_rules(),
                    ..IntifaceBridgeConfig::default()
                };
                match IntifaceBridgeHandle::start(bridge_config) {
                    Ok(handle) => {
                        self.intiface_bridge = Some(handle);
                        self.set_status("Intiface direct bridge engaged", false);
                    }
                    Err(err) => self.set_status(
                        format!("Failed to engage Intiface direct bridge: {err}"),
                        true,
                    ),
                }
            }
            Err(err) => self.set_status(err, true),
        }
    }

    fn disengage_intiface_bridge(&mut self) {
        if let Some(handle) = self.intiface_bridge.take() {
            handle.stop();
            self.intiface_bridge_snapshot = IntifaceBridgeSnapshot::default();
            self.set_status("Intiface direct bridge disengaged", false);
        }
    }

    fn discover_oscquery(&mut self) {
        let Some(client) = self.oscquery_client.as_mut() else {
            self.set_status("OSCQuery client unavailable", true);
            return;
        };
        let endpoint = client.discover(self.diagnostics.oscquery_port_from_logs);
        self.oscquery_status = client.status();
        if let Some(endpoint) = endpoint {
            self.set_status(
                format!(
                    "OSCQuery discovered via {} at {}:{} -> OSC {}:{}",
                    endpoint.source,
                    endpoint.oscquery_host,
                    endpoint.oscquery_port,
                    endpoint.osc_host,
                    endpoint.osc_port
                ),
                false,
            );
        } else {
            let err = self
                .oscquery_status
                .last_error
                .clone()
                .unwrap_or_else(|| "not discovered".to_string());
            self.set_status(format!("OSCQuery discovery failed: {err}"), true);
        }
    }

    fn json_value_to_runtime_param(value: &Value) -> Option<RuntimeParamValue> {
        if let Some(number) = value.as_f64() {
            if number.is_finite() {
                return Some(RuntimeParamValue::Number(number));
            }
        }
        if let Some(boolean) = value.as_bool() {
            return Some(RuntimeParamValue::Bool(boolean));
        }
        if let Some(text) = value.as_str() {
            return Some(RuntimeParamValue::Text(text.to_string()));
        }
        None
    }

    fn is_listed_osc_param_key(key: &str) -> bool {
        key == "OGB" || key.starts_with("OGB/")
    }

    fn should_list_osc_param_key(&self, key: &str) -> bool {
        self.show_all_avatar_params || Self::is_listed_osc_param_key(key)
    }

    fn apply_avatar_param_visibility_filter(&mut self) {
        if self.show_all_avatar_params {
            return;
        }
        self.avatar_params
            .retain(|(key, _)| Self::is_listed_osc_param_key(key));
    }

    fn mapping_address_for_param_key(param_key: &str) -> String {
        const PREFIX: &str = "/avatar/parameters/";
        if param_key.starts_with(PREFIX) {
            param_key.to_string()
        } else if param_key.starts_with('/') {
            param_key.to_string()
        } else {
            format!("{PREFIX}{param_key}")
        }
    }

    fn has_mapping_for_param_key(&self, param_key: &str) -> bool {
        let target_address = Self::mapping_address_for_param_key(param_key);
        self.form
            .mappings
            .iter()
            .any(|row| row.address.trim() == target_address)
    }

    fn add_default_mapping_for_param_key(&mut self, param_key: &str) -> bool {
        if self.has_mapping_for_param_key(param_key) {
            return false;
        }
        let mut row = MappingRowForm::default();
        row.address = Self::mapping_address_for_param_key(param_key);
        self.form.mappings.push(row);
        true
    }

    fn remove_mappings_for_param_key(&mut self, param_key: &str) -> usize {
        let target_address = Self::mapping_address_for_param_key(param_key);
        let before = self.form.mappings.len();
        self.form
            .mappings
            .retain(|row| row.address.trim() != target_address);
        before.saturating_sub(self.form.mappings.len())
    }

    fn ensure_oscquery_discovered(&mut self) -> bool {
        let Some(client) = self.oscquery_client.as_mut() else {
            return false;
        };
        if client.status().endpoint.is_some() {
            self.oscquery_status = client.status();
            return true;
        }
        let endpoint = client.discover(self.diagnostics.oscquery_port_from_logs);
        self.oscquery_status = client.status();
        endpoint.is_some()
    }

    fn refresh_avatar_params_from_oscquery(&mut self, force_status: bool) {
        if !self.ensure_oscquery_discovered() {
            if force_status {
                let err = self
                    .oscquery_status
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "not discovered".to_string());
                self.set_status(format!("OSCQuery discovery failed: {err}"), true);
            }
            return;
        }

        let (fetch_result, status) = {
            let Some(client) = self.oscquery_client.as_mut() else {
                return;
            };
            (client.fetch_avatar_parameters(), client.status())
        };

        match fetch_result {
            Ok(values) => {
                let mut list = values
                    .into_iter()
                    .filter_map(|(key, value)| {
                        if !self.should_list_osc_param_key(&key) {
                            return None;
                        }
                        Self::json_value_to_runtime_param(&value).map(|converted| (key, converted))
                    })
                    .collect::<Vec<_>>();
                list.sort_by(|a, b| a.0.cmp(&b.0));

                // OSCQuery can transiently return an empty set (or no compatible values).
                // Preserve the current discovered list in that case instead of wiping UI state.
                if list.is_empty() && !self.avatar_params.is_empty() {
                    self.oscquery_status = status;
                    if force_status {
                        self.set_status(
                            "OSCQuery returned no compatible OGB params; keeping current list.",
                            true,
                        );
                    }
                    return;
                }

                if self.runtime_running() && !self.avatar_params.is_empty() {
                    let mut merged = self
                        .avatar_params
                        .iter()
                        .cloned()
                        .collect::<std::collections::HashMap<_, _>>();
                    for (key, value) in list {
                        merged.entry(key).or_insert(value);
                    }
                    let mut merged_list = merged.into_iter().collect::<Vec<_>>();
                    merged_list.sort_by(|a, b| a.0.cmp(&b.0));
                    self.avatar_params = merged_list;
                } else {
                    self.avatar_params = list;
                }

                self.oscquery_status = status;
                self.apply_avatar_param_visibility_filter();
                if force_status {
                    self.set_status(
                        format!(
                            "Fetched {} avatar params via OSCQuery",
                            self.avatar_params.len()
                        ),
                        false,
                    );
                }
            }
            Err(err) => {
                self.oscquery_status = status;
                if force_status {
                    self.set_status(format!("OSCQuery avatar fetch failed: {err}"), true);
                }
            }
        }
    }

    fn tray_bridge_state_label(&self) -> &'static str {
        if self.runtime_running() {
            "Connected"
        } else if self.auto_bridge_enabled {
            "Enabled"
        } else {
            "Disabled"
        }
    }

    fn refresh_tray_menu_state(&mut self) {
        let state_label = format!("Bridge State: {}", self.tray_bridge_state_label());
        let toggle_label = if self.auto_bridge_enabled {
            "Toggle Bridge: Disable".to_string()
        } else {
            "Toggle Bridge: Enable".to_string()
        };

        let can_toggle = self.auto_bridge_enabled
            || (self.selected_profile_id.is_some() && self.selected_profile_has_stream_key);

        #[cfg(target_os = "windows")]
        let popup_menu_handle = self.tray.as_ref().map(|tray| tray.popup_menu_handle);

        let Some(tray) = self.tray.as_mut() else {
            return;
        };

        let mut changed = false;
        if tray.last_state_label != state_label {
            tray.bridge_state_item.set_text(&state_label);
            tray.last_state_label = state_label;
            changed = true;
        }
        if tray.last_toggle_label != toggle_label {
            tray.bridge_toggle_item.set_text(&toggle_label);
            tray.last_toggle_label = toggle_label;
            changed = true;
        }
        if tray.last_can_toggle != can_toggle {
            tray.bridge_toggle_item.set_enabled(can_toggle);
            tray.last_can_toggle = can_toggle;
            changed = true;
        }

        if changed {
            #[cfg(target_os = "windows")]
            if let Some(handle) = popup_menu_handle {
                sync_tray_menu_state_native(
                    handle,
                    self.service.as_ref(),
                    &self.tray_bridge_shared,
                );
            }
        }
    }

    fn fetch_oscquery_bulk(&mut self) {
        if !self.ensure_oscquery_discovered() {
            let err = self
                .oscquery_status
                .last_error
                .clone()
                .unwrap_or_else(|| "not discovered".to_string());
            self.set_status(format!("OSCQuery discovery failed: {err}"), true);
            return;
        }

        let Some(client) = self.oscquery_client.as_ref() else {
            self.set_status("OSCQuery client unavailable", true);
            return;
        };
        match client.fetch_bulk_values() {
            Ok(values) => {
                let mut list = values
                    .into_iter()
                    .filter(|(key, _)| Self::is_listed_osc_param_key(key))
                    .map(|(k, v)| (k, v.to_string()))
                    .collect::<Vec<_>>();
                list.sort_by(|a, b| a.0.cmp(&b.0));
                self.oscquery_values = list;
                self.set_status(
                    format!("Fetched {} OSCQuery values", self.oscquery_values.len()),
                    false,
                );
            }
            Err(err) => {
                self.oscquery_status = client.status();
                self.set_status(format!("OSCQuery bulk fetch failed: {err}"), true);
            }
        }
    }

    fn render_login_tab(&mut self, ui: &mut egui::Ui) {
        let login_pending = self.pending_companion_auth.is_some();
        if self.form.name.trim().is_empty() {
            self.form.name = "Default".to_string();
        }
        let accent = egui::Color32::from_rgb(0, 255, 153);
        let accent_soft = egui::Color32::from_rgba_unmultiplied(0, 255, 153, 26);
        let frame_fill = egui::Color32::from_rgba_unmultiplied(18, 18, 18, 232);
        let card_border = egui::Color32::from_rgba_unmultiplied(0, 255, 153, 95);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(18.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("VIBEALONG")
                            .monospace()
                            .size(14.0)
                            .color(accent),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Sign In To Continue")
                            .size(38.0)
                            .strong(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "Secure browser auth, automatic credential handoff, no token copy/paste.",
                        )
                        .size(16.0)
                        .color(egui::Color32::from_rgb(182, 190, 186)),
                    );
                });

                ui.add_space(16.0);
                let card_width = ui.available_width().min(920.0);
                ui.vertical_centered(|ui| {
                    ui.set_max_width(card_width);
                    ui.set_width(card_width);
                    egui::Frame::new()
                        .fill(frame_fill)
                        .stroke(egui::Stroke::new(1.0, card_border))
                        .corner_radius(egui::CornerRadius::same(14))
                        .inner_margin(egui::Margin::symmetric(24, 22))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("Account Setup")
                                        .size(20.0)
                                        .strong(),
                                );
                                if self.selected_profile_has_stream_key {
                                    ui.add_space(8.0);
                                    ui.colored_label(
                                        egui::Color32::from_rgb(90, 220, 130),
                                        "Authenticated",
                                    );
                                }
                            });
                            ui.add_space(10.0);

                            let base_url_trimmed = self.form.website_base_url.trim();
                            let mut login_environment = if base_url_trimmed
                                .eq_ignore_ascii_case("https://vrlewds.com")
                            {
                                0
                            } else if base_url_trimmed
                                .eq_ignore_ascii_case("https://dev.vrlewds.com")
                            {
                                1
                            } else {
                                2
                            };

                            ui.horizontal(|ui| {
                                ui.label("Environment");
                                egui::ComboBox::from_id_salt("login_environment_selector")
                                    .selected_text(match login_environment {
                                        0 => "Stable (vrlewds.com)",
                                        1 => "Nightly (dev.vrlewds.com)",
                                        _ => "Custom URL",
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut login_environment,
                                            0,
                                            "Stable (vrlewds.com)",
                                        );
                                        ui.selectable_value(
                                            &mut login_environment,
                                            1,
                                            "Nightly (dev.vrlewds.com)",
                                        );
                                        ui.selectable_value(
                                            &mut login_environment,
                                            2,
                                            "Custom URL",
                                        );
                                    });
                            });

                            match login_environment {
                                0 => self.form.website_base_url = "https://vrlewds.com".to_string(),
                                1 => {
                                    self.form.website_base_url =
                                        "https://dev.vrlewds.com".to_string()
                                }
                                _ => {}
                            }

                            if login_environment == 2 {
                                ui.add_space(6.0);
                                ui.label("Custom Website Base URL");
                                ui.add_sized(
                                    [ui.available_width(), 30.0],
                                    egui::TextEdit::singleline(&mut self.form.website_base_url),
                                );
                            }

                            ui.add_space(6.0);
                            ui.checkbox(
                                &mut self.form.allow_insecure_http,
                                "Allow Insecure HTTP (local dev only)",
                            );

                            ui.add_space(14.0);
                            let cta = egui::Button::new(
                                egui::RichText::new("Sign in with VRLewds Identity")
                                    .size(18.0)
                                    .strong()
                                    .color(egui::Color32::from_rgb(10, 26, 18)),
                            )
                            .fill(egui::Color32::from_rgb(0, 255, 153))
                            .stroke(egui::Stroke::new(1.0, accent))
                            .corner_radius(egui::CornerRadius::same(10));
                            if ui
                                .add_enabled(!login_pending, cta.min_size(egui::vec2(0.0, 48.0)))
                                .clicked()
                            {
                                self.start_companion_browser_login();
                            }

                            if login_pending {
                                ui.add_space(10.0);
                                egui::Frame::new()
                                    .fill(accent_soft)
                                    .stroke(egui::Stroke::new(
                                        1.0,
                                        egui::Color32::from_rgba_unmultiplied(0, 255, 153, 75),
                                    ))
                                    .corner_radius(egui::CornerRadius::same(8))
                                    .inner_margin(egui::Margin::symmetric(12, 10))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.add(egui::Spinner::new().size(15.0));
                                            ui.colored_label(
                                                egui::Color32::from_rgb(140, 236, 192),
                                                "Waiting for secure browser callback...",
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if ui.button("Cancel").clicked() {
                                                        self.pending_companion_auth = None;
                                                        self.set_status(
                                                            "Companion login canceled.",
                                                            true,
                                                        );
                                                    }
                                                },
                                            );
                                        });
                                    });
                            }

                            ui.add_space(12.0);
                            if self.selected_profile_has_stream_key {
                                if ui
                                    .add(
                                        egui::Button::new("Continue To Setup Wizard")
                                            .fill(egui::Color32::from_rgba_unmultiplied(
                                                255, 255, 255, 12,
                                            ))
                                            .stroke(egui::Stroke::new(1.0, accent)),
                                    )
                                    .clicked()
                                {
                                    self.tab = DesktopTab::Setup;
                                }
                            }
                        });
                });

                if !self.status_message.is_empty() {
                    ui.add_space(10.0);
                    let (bg, border, fg) = if self.status_is_error {
                        (
                            egui::Color32::from_rgba_unmultiplied(255, 80, 80, 20),
                            egui::Color32::from_rgba_unmultiplied(255, 110, 110, 75),
                            egui::Color32::from_rgb(255, 150, 150),
                        )
                    } else {
                        (
                            egui::Color32::from_rgba_unmultiplied(0, 255, 153, 18),
                            egui::Color32::from_rgba_unmultiplied(0, 255, 153, 70),
                            egui::Color32::from_rgb(130, 236, 186),
                        )
                    };
                    ui.vertical_centered(|ui| {
                        ui.set_max_width(card_width);
                        ui.set_width(card_width);
                        egui::Frame::new()
                            .fill(bg)
                            .stroke(egui::Stroke::new(1.0, border))
                            .corner_radius(egui::CornerRadius::same(8))
                            .inner_margin(egui::Margin::symmetric(12, 10))
                            .show(ui, |ui| {
                                ui.colored_label(fg, self.status_message.clone());
                            });
                        });
                    }

                ui.add_space(20.0);
            });
    }

    fn render_setup_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Setup Wizard");
        ui.label("Follow this checklist top-to-bottom. Green means ready.");
        ui.separator();

        let config_ready = self.selected_profile_id.is_some();
        let key_ready = self.selected_profile_has_stream_key;
        let runtime_ready = self.runtime_running();
        let vrchat_osc_ready = self.diagnostics.osc_enabled == Some(true);
        let vrchat_self_ready = self.diagnostics.self_interact_enabled == Some(true);
        let vrchat_everyone_ready = self.diagnostics.everyone_interact_enabled == Some(true);
        let oscquery_ready = self.oscquery_status.endpoint.is_some();
        let intiface_ready = self.intiface_snapshot.connected;
        let login_pending = self.pending_companion_auth.is_some();

        if !key_ready {
            ui.group(|ui| {
                ui.heading("Step 1: Sign in with VRLewds Identity");
                ui.label(
                    "Go to the Login tab and complete browser sign-in first.",
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Open Login Screen").clicked() {
                        self.tab = DesktopTab::Login;
                    }
                    if login_pending {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 180, 70),
                            "Waiting for browser callback...",
                        );
                    }
                });
            });
            ui.separator();
        }

        let mut checklist = Vec::new();
        checklist.push(("Configuration loaded", config_ready));
        checklist.push(("Credential configured", key_ready));
        checklist.push(("Bridge auto mode enabled", self.auto_bridge_enabled));
        checklist.push(("Bridge engaged", runtime_ready));
        checklist.push(("VRChat OSC enabled", vrchat_osc_ready));
        checklist.push(("VRChat self interact enabled", vrchat_self_ready));
        checklist.push(("VRChat everyone interact enabled", vrchat_everyone_ready));
        checklist.push(("OSCQuery discovered", oscquery_ready));
        checklist.push(("Intiface reachable", intiface_ready));

        for (label, ok) in checklist {
            let color = if ok {
                egui::Color32::from_rgb(90, 220, 130)
            } else {
                egui::Color32::from_rgb(230, 170, 70)
            };
            ui.colored_label(color, format!("[{}] {}", if ok { "x" } else { " " }, label));
        }

        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui.button("Reload Configuration").clicked() {
                self.refresh_profiles();
                self.restore_last_selected_profile();
            }
            if ui.button("Refresh Runtime").clicked() {
                self.refresh_runtime_views(false);
            }
            if ui.button("Refresh Diagnostics").clicked() {
                self.request_diagnostics_refresh();
                self.refresh_diagnostics_from_worker();
            }
            if ui.button("Discover OSCQuery").clicked() {
                self.discover_oscquery();
            }
            if ui.button("Probe Intiface").clicked() {
                self.probe_intiface();
            }
        });

        if self.diagnostics.osc_start_failure.is_some() {
            ui.separator();
            ui.colored_label(
                egui::Color32::from_rgb(220, 120, 80),
                "VRChat reported OSC startup failure in log. Resolve this before expecting bridge data.",
            );
        }
    }

    fn render_home_tab(&mut self, ui: &mut egui::Ui) {
        let running = self.runtime_running();
        let active_profile = self.service.active_profile_id().ok().flatten();
        let current_request = self.current_live_gate_request();

        ui.heading("Bridge Runtime");
        ui.horizontal(|ui| {
            if self.auto_bridge_enabled {
                if ui
                    .add(
                        egui::Button::new("Disengage Bridge (Auto OFF)")
                            .fill(egui::Color32::from_rgb(170, 50, 50)),
                    )
                    .clicked()
                {
                    self.disable_bridge_auto_mode("home");
                }
            } else {
                let can_start =
                    self.selected_profile_id.is_some() && self.selected_profile_has_stream_key;
                let button = ui.add_enabled(
                    can_start,
                    egui::Button::new("Engage Bridge (Auto ON)")
                        .fill(egui::Color32::from_rgb(40, 120, 70)),
                );
                if button.clicked() {
                    self.enable_bridge_auto_mode("home");
                }
            }
            if ui.button("Check Live Now").clicked() {
                self.sync_live_gate_worker_request(true);
                self.refresh_live_gate_from_worker();
            }
            if ui.button("Refresh Runtime").clicked() {
                self.refresh_runtime_views(false);
            }
        });

        ui.separator();
        ui.label(format!(
            "Engage toggle: {}",
            if self.auto_bridge_enabled {
                "ON (auto)"
            } else {
                "OFF"
            }
        ));
        ui.label(format!(
            "Stream live gate: {}",
            match self.live_gate_status.is_live {
                Some(true) => "live",
                Some(false) => "offline",
                None => "unknown",
            }
        ));
        if let Some(request) = current_request {
            ui.label(format!(
                "Gate target: {} @ {}",
                request.creator_username, request.base_url
            ));
        }
        if self.live_gate_status.checked_at_ms > 0 {
            ui.label(format!(
                "Last live check: {}",
                self.live_gate_status.checked_at_ms
            ));
        }
        if let Some(stream_id) = &self.live_gate_status.stream_id {
            ui.label(format!("Matched streamId: {stream_id}"));
        }
        if let Some(stream_title) = &self.live_gate_status.stream_title {
            ui.label(format!("Live title: {stream_title}"));
        }
        if let Some(err) = &self.live_gate_status.last_error {
            ui.colored_label(
                egui::Color32::from_rgb(220, 120, 80),
                format!("Live check error: {err}"),
            );
        }

        ui.separator();
        ui.label(format!(
            "Status: {}",
            if running { "Engaged" } else { "Disengaged" }
        ));
        ui.label(format!(
            "Active config: {}",
            active_profile.unwrap_or_else(|| "-".to_string())
        ));

        if let Some(snapshot) = &self.runtime_snapshot {
            ui.label(format!("Seq: {}", snapshot.seq));
            ui.label(format!(
                "Current intensity: {:.3}",
                snapshot.current_intensity
            ));
            ui.label(format!(
                "Target intensity: {:.3}",
                snapshot.target_intensity
            ));
            ui.label(format!("Peak intensity: {:.3}", snapshot.peak_intensity));
            ui.label(format!("Relay connected: {}", snapshot.relay_connected));
            ui.label(format!(
                "Auto-disengaged: {}",
                if snapshot.auto_disengaged {
                    "yes"
                } else {
                    "no"
                }
            ));
            ui.label(format!("OSC packets: {}", snapshot.osc_packets_received));
            ui.label(format!("OSC messages: {}", snapshot.osc_messages_received));
            ui.label(format!(
                "Mapped messages: {}",
                snapshot.mapped_messages_received
            ));
            ui.label(format!(
                "Unmapped messages: {}",
                snapshot.unmapped_messages_received
            ));
            ui.label(format!(
                "Discovery entries: {}",
                snapshot.discovery_entries_written
            ));
            ui.label(format!(
                "Last source: {} [{}]",
                if snapshot.last_source_address.is_empty() {
                    "-"
                } else {
                    snapshot.last_source_address.as_str()
                },
                snapshot.last_source_arg_type
            ));
            if !snapshot.last_error.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 120, 80),
                    format!("Last error: {}", snapshot.last_error),
                );
            }
        } else {
            ui.label("Runtime not started.");
        }
    }

    #[allow(dead_code)]
    fn render_profiles_tab(&mut self, ui: &mut egui::Ui) {
        ui.columns(2, |columns| {
            let left = &mut columns[0];
            left.heading("Profiles");
            left.horizontal(|ui| {
                if ui.button("Refresh Profiles").clicked() {
                    self.refresh_profiles();
                }
                if ui.button("New Profile").clicked() {
                    self.reset_form_for_new_profile();
                }
                if ui.button("Delete Selected").clicked() {
                    if let Some(profile_id) = self.selected_profile_id.clone() {
                        match self.service.delete_profile(&profile_id) {
                            Ok(_) => {
                                self.set_status(format!("Deleted profile {profile_id}"), false);
                                self.reset_form_for_new_profile();
                                self.refresh_profiles();
                            }
                            Err(err) => self.set_status(format!("Delete failed: {err}"), true),
                        }
                    }
                }
            });

            let mut clicked_profile_id: Option<String> = None;
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .show(left, |ui| {
                    for profile in &self.profiles {
                        let selected = self
                            .selected_profile_id
                            .as_ref()
                            .is_some_and(|id| id == &profile.id);
                        if ui
                            .selectable_label(
                                selected,
                                format!(
                                    "{} | {} | key={} | updated={}",
                                    profile.name,
                                    profile.creator_username,
                                    if profile.has_stream_key {
                                        "set"
                                    } else {
                                        "missing"
                                    },
                                    profile.updated_at_ms
                                ),
                            )
                            .clicked()
                        {
                            clicked_profile_id = Some(profile.id.clone());
                        }
                    }
                });
            if let Some(profile_id) = clicked_profile_id {
                self.load_profile_into_form(&profile_id);
            }

            let right = &mut columns[1];
            right.heading("Create/Update Profile");
            right.label(format!(
                "Mode: {}",
                if self.selected_profile_id.is_some() {
                    "Update selected profile"
                } else {
                    "Create new profile"
                }
            ));
            right.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut self.form.name);
            });
            right.horizontal(|ui| {
                ui.label("Creator Username");
                ui.text_edit_singleline(&mut self.form.creator_username);
            });
            right.horizontal(|ui| {
                ui.label("Credential (stream key or companion token)");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form.stream_key)
                        .password(self.stream_key_hidden)
                        .desired_width(260.0),
                );
                ui.checkbox(&mut self.stream_key_hidden, "Hide");
            });
            let entered_stream_key = self.normalized_stream_key_input();
            right.horizontal(|ui| {
                if entered_stream_key.is_empty() {
                    if self.selected_profile_has_stream_key {
                        ui.label("(leave blank to keep existing)");
                    } else {
                        ui.label("No credential entered");
                    }
                } else {
                    ui.label(format!(
                        "{} chars entered",
                        entered_stream_key.chars().count()
                    ));
                }
                if ui.button("Clear").clicked() {
                    self.form.stream_key.clear();
                }
                if ui.button("Re-auth via Browser").clicked() {
                    self.start_companion_browser_login();
                }
                if let Some(profile_id) = self.selected_profile_id.clone() {
                    if ui.button("Set Credential Only").clicked() {
                        if entered_stream_key.is_empty() {
                            self.set_status("Enter credential first", true);
                        } else {
                            match self
                                .service
                                .set_stream_key(&profile_id, &entered_stream_key)
                            {
                                Ok(_) => {
                                    self.selected_profile_has_stream_key = true;
                                    self.form.stream_key.clear();
                                    self.refresh_profiles();
                                    self.set_status(
                                        format!("Updated credential for profile {profile_id}"),
                                        false,
                                    );
                                }
                                Err(err) => self
                                    .set_status(format!("Failed to set credential: {err}"), true),
                            }
                        }
                    }
                }
            });
            right.horizontal(|ui| {
                ui.label("Website Base URL");
                ui.text_edit_singleline(&mut self.form.website_base_url);
            });
            right.checkbox(
                &mut self.form.allow_insecure_http,
                "Allow Insecure HTTP (local only)",
            );
            right.horizontal(|ui| {
                ui.label("OSC Host");
                ui.text_edit_singleline(&mut self.form.osc_host);
            });
            right.horizontal(|ui| {
                ui.label("OSC Port");
                ui.text_edit_singleline(&mut self.form.osc_port);
            });
            right.checkbox(
                &mut self.form.allow_network_osc,
                "Allow network OSC (non-loopback)",
            );
            right.label("Allowed sender IPs (optional, one per line)");
            right.add(
                egui::TextEdit::multiline(&mut self.form.osc_allowed_senders)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            right.separator();
            right.horizontal(|ui| {
                ui.heading("Mappings");
                if ui.button("Add Mapping").clicked() {
                    self.form.mappings.push(MappingRowForm::default());
                }
            });
            let mut remove_mapping_idx: Option<usize> = None;
            for (idx, mapping) in self.form.mappings.iter_mut().enumerate() {
                right.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("Mapping {}", idx + 1));
                        if ui.button("Remove").clicked() {
                            remove_mapping_idx = Some(idx);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Address");
                        ui.text_edit_singleline(&mut mapping.address);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Weight");
                        ui.text_edit_singleline(&mut mapping.weight);
                        ui.label("Deadzone");
                        ui.text_edit_singleline(&mut mapping.deadzone);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Min");
                        ui.text_edit_singleline(&mut mapping.min);
                        ui.label("Max");
                        ui.text_edit_singleline(&mut mapping.max);
                    });
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut mapping.invert, "Invert");
                        egui::ComboBox::from_id_salt(format!("curve_{idx}"))
                            .selected_text(curve_label(mapping.curve))
                            .show_ui(ui, |ui| {
                                for curve in available_curves() {
                                    ui.selectable_value(
                                        &mut mapping.curve,
                                        curve,
                                        curve_label(curve),
                                    );
                                }
                            });
                    });
                });
            }
            if let Some(idx) = remove_mapping_idx {
                self.form.mappings.remove(idx);
                if self.form.mappings.is_empty() {
                    self.form.mappings.push(MappingRowForm::default());
                }
            }

            right.separator();
            right.label("Forward Targets host:port (one per line)");
            right.add(
                egui::TextEdit::multiline(&mut self.form.forward_targets)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );

            if right.button("Save Profile").clicked() {
                match self.build_profile_from_form() {
                    Ok((profile, stream_key)) => {
                        match self.service.upsert_profile(&profile, &stream_key) {
                            Ok(profile_id) => {
                                self.set_status(format!("Saved profile {profile_id}"), false);
                                self.refresh_profiles();
                                self.selected_profile_id = Some(profile_id);
                                self.selected_profile_has_stream_key = true;
                                self.form.stream_key.clear();
                                self.persist_selected_profile_preference();
                                self.sync_live_gate_worker_request(true);
                            }
                            Err(err) => {
                                self.set_status(format!("Failed to save profile: {err}"), true)
                            }
                        }
                    }
                    Err(err) => self.set_status(err, true),
                }
            }
        });
    }

    fn render_logs_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Refresh Logs").clicked() {
                self.refresh_runtime_views(true);
            }
            ui.label(format!("Lines: {}", self.runtime_logs.len()));
        });
        ui.separator();

        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.runtime_logs {
                    let color = match line.level.as_str() {
                        "ERROR" => egui::Color32::from_rgb(230, 100, 100),
                        "WARN" => egui::Color32::from_rgb(230, 180, 70),
                        "RELAY" => egui::Color32::from_rgb(130, 220, 160),
                        "DISCOVERY" => egui::Color32::from_rgb(120, 180, 240),
                        _ => egui::Color32::from_rgb(200, 200, 200),
                    };
                    ui.colored_label(
                        color,
                        format!("[{}][{}] {}", line.ts_ms, line.level, line.message),
                    );
                }
            });
    }

    fn render_avatar_tab(&mut self, ui: &mut egui::Ui) {
        let mut visibility_changed = false;
        ui.horizontal(|ui| {
            if ui.button("Refresh Avatar Params").clicked() {
                self.refresh_runtime_views(false);
            }
            if ui.button("Fetch via OSCQuery").clicked() {
                self.refresh_avatar_params_from_oscquery(true);
            }
            visibility_changed = ui
                .checkbox(&mut self.show_all_avatar_params, "Show all parameters")
                .changed();
            ui.label(format!("Tracked params: {}", self.avatar_params.len()));
        });
        if visibility_changed {
            self.apply_avatar_param_visibility_filter();
            self.refresh_runtime_views(false);
            if self.oscquery_status.endpoint.is_some() {
                self.refresh_avatar_params_from_oscquery(false);
            }
        }
        ui.separator();
        ui.columns(2, |columns| {
            let left = &mut columns[0];
            left.heading("Discovered Params");
            left.label(if self.show_all_avatar_params {
                "Use Add/Remove Mapping to toggle a mapping row for each discovered parameter."
            } else {
                "Use Add/Remove Mapping to toggle a mapping row for each discovered OGB parameter."
            });
            left.separator();

            let mut mapping_toggle_request: Option<(String, bool)> = None;
            if self.avatar_params.is_empty() {
                left.label(
                    "No avatar parameter values yet. Engage bridge and generate OSC traffic, or fetch via OSCQuery.",
                );
            } else {
                egui::ScrollArea::vertical().show(left, |ui| {
                    for (key, value) in &self.avatar_params {
                        let value_color = match value {
                            RuntimeParamValue::Bool(true) => egui::Color32::from_rgb(80, 220, 120),
                            RuntimeParamValue::Bool(false) => egui::Color32::from_rgb(220, 90, 90),
                            RuntimeParamValue::Number(_) => egui::Color32::from_rgb(180, 220, 255),
                            RuntimeParamValue::Text(_) => egui::Color32::from_rgb(210, 210, 210),
                        };
                        let mapping_present = self.has_mapping_for_param_key(key);
                        ui.horizontal(|ui| {
                            ui.label(key);
                            ui.colored_label(value_color, value.display_string());
                            let button_label = if mapping_present {
                                "Remove Mapping"
                            } else {
                                "Add Mapping"
                            };
                            if ui.button(button_label).clicked() {
                                mapping_toggle_request = Some((key.clone(), mapping_present));
                            }
                        });
                    }
                });
            }

            if let Some((param_key, mapping_present)) = mapping_toggle_request {
                if mapping_present {
                    let removed = self.remove_mappings_for_param_key(&param_key);
                    self.set_status(
                        format!(
                            "Removed {removed} mapping(s) for {}. Save settings to persist.",
                            Self::mapping_address_for_param_key(&param_key)
                        ),
                        false,
                    );
                } else if self.add_default_mapping_for_param_key(&param_key) {
                    self.set_status(
                        format!(
                            "Added default mapping for {}. Save settings to persist.",
                            Self::mapping_address_for_param_key(&param_key)
                        ),
                        false,
                    );
                } else {
                    self.set_status(
                        format!(
                            "Mapping already exists for {}.",
                            Self::mapping_address_for_param_key(&param_key)
                        ),
                        false,
                    );
                }
            }

            let right = &mut columns[1];
            right.heading("Mapping Settings");
            self.render_mapping_editor(right);
            if right.button("Save Settings").clicked() {
                self.save_current_configuration();
            }
        });
    }

    fn render_mapping_editor(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Add Mapping").clicked() {
                self.form.mappings.push(MappingRowForm::default());
            }
            ui.label(format!("Rows: {}", self.form.mappings.len()));
        });

        if self.form.mappings.is_empty() {
            ui.label("No mappings configured.");
            return;
        }

        let mut remove_mapping_idx: Option<usize> = None;
        for (idx, mapping) in self.form.mappings.iter_mut().enumerate() {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("Mapping {}", idx + 1));
                    if ui.button("Remove").clicked() {
                        remove_mapping_idx = Some(idx);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Address");
                    ui.text_edit_singleline(&mut mapping.address);
                });
                ui.horizontal(|ui| {
                    ui.label("Weight");
                    ui.text_edit_singleline(&mut mapping.weight);
                    ui.label("Deadzone");
                    ui.text_edit_singleline(&mut mapping.deadzone);
                });
                ui.horizontal(|ui| {
                    ui.label("Min");
                    ui.text_edit_singleline(&mut mapping.min);
                    ui.label("Max");
                    ui.text_edit_singleline(&mut mapping.max);
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut mapping.invert, "Invert");
                    egui::ComboBox::from_id_salt(format!("avatar_mapping_curve_{idx}"))
                        .selected_text(curve_label(mapping.curve))
                        .show_ui(ui, |ui| {
                            for curve in available_curves() {
                                ui.selectable_value(&mut mapping.curve, curve, curve_label(curve));
                            }
                        });
                });
            });
        }

        if let Some(idx) = remove_mapping_idx {
            self.form.mappings.remove(idx);
        }
    }

    fn render_settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("App Settings");
        let changed = ui
            .checkbox(
                &mut self.close_to_background,
                "Close to Background (X hides app)",
            )
            .changed();
        if changed {
            self.persist_close_to_background_preference();
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("Config file:");
            ui.monospace(self.db_path.display().to_string());
        });
        ui.label(format!(
            "System tray: {}",
            if self.tray.is_some() {
                "ready"
            } else {
                "unavailable (fallback: minimize to taskbar)"
            }
        ));

        ui.separator();
        ui.heading("Bridge Configuration");
        ui.label(format!(
            "Config slot: {}",
            self.selected_profile_id.as_deref().unwrap_or("default")
        ));
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut self.form.name);
        });
        ui.horizontal(|ui| {
            ui.label("Creator Username");
            ui.text_edit_singleline(&mut self.form.creator_username);
        });
        ui.horizontal(|ui| {
            ui.label("Credential (stream key or companion token)");
            ui.add(
                egui::TextEdit::singleline(&mut self.form.stream_key)
                    .password(self.stream_key_hidden)
                    .desired_width(260.0),
            );
            ui.checkbox(&mut self.stream_key_hidden, "Hide");
        });
        let entered_stream_key = self.normalized_stream_key_input();
        ui.horizontal(|ui| {
            if entered_stream_key.is_empty() {
                if self.selected_profile_has_stream_key {
                    ui.label("(leave blank to keep existing)");
                } else {
                    ui.label("No credential entered");
                }
            } else {
                ui.label(format!(
                    "{} chars entered",
                    entered_stream_key.chars().count()
                ));
            }
            if ui.button("Clear").clicked() {
                self.form.stream_key.clear();
            }
            if ui.button("Re-auth via Browser").clicked() {
                self.start_companion_browser_login();
            }
        });

        ui.horizontal(|ui| {
            ui.label("Website Base URL");
            ui.text_edit_singleline(&mut self.form.website_base_url);
        });
        ui.checkbox(
            &mut self.form.allow_insecure_http,
            "Allow Insecure HTTP (local only)",
        );
        ui.horizontal(|ui| {
            ui.label("OSC Host");
            ui.text_edit_singleline(&mut self.form.osc_host);
        });
        ui.horizontal(|ui| {
            ui.label("OSC Port");
            ui.text_edit_singleline(&mut self.form.osc_port);
        });
        ui.checkbox(
            &mut self.form.allow_network_osc,
            "Allow network OSC (non-loopback)",
        );
        ui.label("Allowed sender IPs (optional, one per line)");
        ui.add(
            egui::TextEdit::multiline(&mut self.form.osc_allowed_senders)
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );

        ui.separator();
        ui.label("Forward Targets host:port (one per line)");
        ui.add(
            egui::TextEdit::multiline(&mut self.form.forward_targets)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );

        if ui.button("Save Settings").clicked() {
            self.save_current_configuration();
        }
    }

    fn render_diagnostics_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Diagnostics");

        ui.horizontal(|ui| {
            if ui.button("Refresh Diagnostics").clicked() {
                self.request_diagnostics_refresh();
                self.refresh_diagnostics_from_worker();
                self.set_status("Diagnostics refreshed", false);
            }
            if ui.button("Discover OSCQuery").clicked() {
                self.discover_oscquery();
            }
            if ui.button("Fetch OSCQuery Bulk").clicked() {
                self.fetch_oscquery_bulk();
            }
        });
        ui.separator();

        ui.label(format!("Checked at: {}", self.diagnostics.checked_at_ms));
        ui.label(format!(
            "VRChat OSC enabled: {}",
            option_bool_text(self.diagnostics.osc_enabled)
        ));
        ui.label(format!(
            "VRChat self interact enabled: {}",
            option_bool_text(self.diagnostics.self_interact_enabled)
        ));
        ui.label(format!(
            "VRChat everyone interact enabled: {}",
            option_bool_text(self.diagnostics.everyone_interact_enabled)
        ));
        ui.label(format!(
            "Latest VRChat log: {}",
            self.diagnostics
                .latest_log_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".to_string())
        ));
        ui.label(format!(
            "OSCQuery port from VRChat log: {}",
            self.diagnostics
                .oscquery_port_from_logs
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string())
        ));
        if let Some(failure) = &self.diagnostics.osc_start_failure {
            ui.colored_label(
                egui::Color32::from_rgb(220, 120, 80),
                format!("OSC startup failure: {failure}"),
            );
        }

        let warnings = self.diagnostics.warning_lines();
        if !warnings.is_empty() {
            ui.separator();
            ui.heading("Warnings");
            for warning in warnings {
                ui.colored_label(egui::Color32::from_rgb(230, 180, 70), warning);
            }
        }

        if !self.diagnostics.errors.is_empty() {
            ui.separator();
            ui.heading("Diagnostics Errors");
            for err in &self.diagnostics.errors {
                ui.colored_label(egui::Color32::from_rgb(220, 100, 100), err);
            }
        }

        ui.separator();
        ui.heading("OSCQuery");
        if let Some(endpoint) = &self.oscquery_status.endpoint {
            ui.label(format!(
                "Endpoint: {}:{} (source {})",
                endpoint.oscquery_host, endpoint.oscquery_port, endpoint.source
            ));
            ui.label(format!(
                "VRChat OSC target: {}:{}",
                endpoint.osc_host, endpoint.osc_port
            ));
        } else {
            ui.label("Endpoint: not discovered");
        }
        if let Some(err) = &self.oscquery_status.last_error {
            ui.colored_label(
                egui::Color32::from_rgb(220, 120, 80),
                format!("Last OSCQuery error: {err}"),
            );
        }

        ui.separator();
        ui.heading(format!(
            "OSCQuery Bulk Values ({})",
            self.oscquery_values.len()
        ));
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .show(ui, |ui| {
                for (key, value) in &self.oscquery_values {
                    ui.horizontal(|ui| {
                        ui.label(key);
                        ui.label(value);
                    });
                }
            });
    }

    fn render_intiface_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Host");
            ui.text_edit_singleline(&mut self.intiface_form.host);
            ui.label("Port");
            ui.text_edit_singleline(&mut self.intiface_form.port);
            ui.checkbox(&mut self.intiface_form.secure, "WSS");
            if ui.button("Probe Intiface").clicked() {
                self.probe_intiface();
            }
            if self.intiface_bridge.is_some() {
                if ui
                    .add(
                        egui::Button::new("Disengage Direct Bridge")
                            .fill(egui::Color32::from_rgb(170, 50, 50)),
                    )
                    .clicked()
                {
                    self.disengage_intiface_bridge();
                }
            } else if ui
                .add(
                    egui::Button::new("Engage Direct Bridge")
                        .fill(egui::Color32::from_rgb(40, 120, 70)),
                )
                .clicked()
            {
                self.engage_intiface_bridge();
            }
        });
        ui.separator();

        let source_values = self.build_intiface_source_values();
        ui.label(format!(
            "Current intensity source: {:.3}",
            source_values.get("intensity").copied().unwrap_or(0.0)
        ));
        ui.heading("Route Rules");
        ui.horizontal(|ui| {
            if ui.button("Add Route").clicked() {
                self.intiface_routes.push(IntifaceRouteForm::default());
            }
        });
        let mut remove_index: Option<usize> = None;
        for (idx, route) in self.intiface_routes.iter_mut().enumerate() {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut route.enabled, "enabled");
                    ui.label(format!("Route {}", idx + 1));
                    if ui.button("Remove").clicked() {
                        remove_index = Some(idx);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Label");
                    ui.text_edit_singleline(&mut route.label);
                });
                ui.horizontal(|ui| {
                    ui.label("Device filter contains");
                    ui.text_edit_singleline(&mut route.target_device_contains);
                    ui.label("Actuator type");
                    ui.text_edit_singleline(&mut route.target_actuator_type);
                });
                ui.horizontal(|ui| {
                    ui.label("Source");
                    ui.selectable_value(
                        &mut route.source_mode,
                        IntifaceSourceMode::Intensity,
                        "Intensity",
                    );
                    ui.selectable_value(
                        &mut route.source_mode,
                        IntifaceSourceMode::AvatarParam,
                        "Avatar Param",
                    );
                    if route.source_mode == IntifaceSourceMode::AvatarParam {
                        ui.label("Param");
                        ui.text_edit_singleline(&mut route.source_param);
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut route.scale, 0.0..=3.0).text("scale"));
                    ui.add(egui::Slider::new(&mut route.idle, 0.0..=1.0).text("idle"));
                    ui.checkbox(&mut route.invert, "invert");
                });
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut route.min_output, 0.0..=1.0).text("min"));
                    ui.add(egui::Slider::new(&mut route.max_output, 0.0..=1.0).text("max"));
                    if route.max_output < route.min_output {
                        route.max_output = route.min_output;
                    }
                });
            });
        }
        if let Some(index) = remove_index {
            self.intiface_routes.remove(index);
        }
        ui.separator();

        ui.label(format!("Connected: {}", self.intiface_snapshot.connected));
        ui.label(format!("Server: {}", self.intiface_snapshot.server_name));
        ui.label(format!(
            "Checked at: {}",
            self.intiface_snapshot.checked_at_ms
        ));
        if let Some(err) = &self.intiface_snapshot.last_error {
            ui.colored_label(
                egui::Color32::from_rgb(220, 120, 80),
                format!("Last error: {err}"),
            );
        }

        ui.separator();
        ui.heading("Direct Bridge Runtime");
        ui.label(format!(
            "Status: {}",
            if self.intiface_bridge.is_some() {
                "engaged"
            } else {
                "disengaged"
            }
        ));
        ui.label(format!(
            "Connected: {}",
            self.intiface_bridge_snapshot.connected
        ));
        ui.label(format!(
            "Devices: {}",
            self.intiface_bridge_snapshot.device_count
        ));
        ui.label(format!(
            "Commands sent: {}",
            self.intiface_bridge_snapshot.commands_sent
        ));
        ui.label(format!(
            "Last level: {:.3}",
            self.intiface_bridge_snapshot.last_level
        ));
        ui.label(format!(
            "Last sent at: {}",
            self.intiface_bridge_snapshot.last_sent_at_ms
        ));
        if !self.intiface_bridge_snapshot.last_error.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(220, 120, 80),
                format!("Bridge error: {}", self.intiface_bridge_snapshot.last_error),
            );
        }

        let devices = self.intiface_snapshot.devices.clone();
        ui.separator();
        ui.heading(format!("Devices ({})", devices.len()));
        egui::ScrollArea::vertical().show(ui, |ui| {
            for device in devices {
                ui.group(|ui| {
                    ui.label(format!("{} (index {})", device.name, device.device_index));
                    for feature in &device.features {
                        ui.horizontal(|ui| {
                            ui.label(format!(
                                "{} #{} {}",
                                feature.command_type,
                                feature.index,
                                feature.actuator_type.as_deref().unwrap_or("-")
                            ));
                            if feature.command_type == "ScalarCmd" {
                                let button = ui.button("Test 30%");
                                if button.clicked() {
                                    match self.parse_intiface_config() {
                                        Ok(config) => {
                                            let client = IntifaceClient::new(config);
                                            let actuator = feature
                                                .actuator_type
                                                .as_deref()
                                                .unwrap_or("Vibrate");
                                            match client.set_scalar_level(
                                                device.device_index,
                                                feature.index,
                                                actuator,
                                                0.3,
                                            ) {
                                                Ok(_) => self.set_status(
                                                    format!(
                                                        "Sent test scalar cmd to {}:{}",
                                                        device.device_index, feature.index
                                                    ),
                                                    false,
                                                ),
                                                Err(err) => self.set_status(
                                                    format!("Failed scalar cmd: {err}"),
                                                    true,
                                                ),
                                            }
                                        }
                                        Err(err) => self.set_status(err, true),
                                    }
                                }
                            }
                        });
                    }
                });
            }
        });
    }
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn apply_vibealong_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "vibe_dm_sans".to_string(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/DMSans-Variable.ttf"
        ))),
    );
    fonts.font_data.insert(
        "vibe_geist".to_string(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/Geist-Variable.ttf"
        ))),
    );
    fonts.font_data.insert(
        "vibe_geist_mono".to_string(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/GeistMono-Variable.ttf"
        ))),
    );

    if let Some(proportional) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        proportional.insert(0, "vibe_dm_sans".to_string());
        proportional.insert(1, "vibe_geist".to_string());
    }
    if let Some(monospace) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        monospace.insert(0, "vibe_geist_mono".to_string());
    }

    ctx.set_fonts(fonts);
}

fn apply_vibealong_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 8.0);
    style.spacing.window_margin = egui::Margin::same(10);
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(22.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(16.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(15.0, egui::FontFamily::Monospace),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(15.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(13.0, egui::FontFamily::Proportional),
    );

    let mut visuals = egui::Visuals::dark();
    let bg = egui::Color32::from_rgb(13, 13, 13);
    let bg2 = egui::Color32::from_rgb(23, 23, 23);
    let bg3 = egui::Color32::from_rgb(31, 31, 31);
    let fg = egui::Color32::from_rgb(245, 245, 245);
    let fg_dim = egui::Color32::from_rgb(162, 162, 162);
    let accent = egui::Color32::from_rgb(0, 255, 153);
    let border = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 28);

    visuals.override_text_color = Some(fg);
    visuals.hyperlink_color = accent;
    visuals.panel_fill = bg;
    visuals.window_fill = bg2;
    visuals.extreme_bg_color = bg3;
    visuals.faint_bg_color = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 8);
    visuals.code_bg_color = bg3;
    visuals.window_stroke = egui::Stroke::new(1.0, border);
    visuals.window_corner_radius = egui::CornerRadius::same(8);
    visuals.menu_corner_radius = egui::CornerRadius::same(8);
    visuals.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(0, 255, 153, 60);
    visuals.selection.stroke = egui::Stroke::new(1.0, accent);

    visuals.widgets.noninteractive.weak_bg_fill = bg2;
    visuals.widgets.noninteractive.fg_stroke.color = fg_dim;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, border);
    visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(8);

    visuals.widgets.inactive.weak_bg_fill = bg3;
    visuals.widgets.inactive.fg_stroke.color = fg;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, border);
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(8);

    visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(0, 255, 153, 24);
    visuals.widgets.hovered.fg_stroke.color = fg;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, accent);
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(8);

    visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(0, 255, 153, 40);
    visuals.widgets.active.fg_stroke.color = fg;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, accent);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(8);

    visuals.widgets.open.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(0, 255, 153, 20);
    visuals.widgets.open.fg_stroke.color = fg;
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, border);
    visuals.widgets.open.corner_radius = egui::CornerRadius::same(8);

    style.visuals = visuals;
    ctx.set_style(style);
}

fn check_stream_live(
    request: &LiveGateRequest,
    client: Option<&reqwest::blocking::Client>,
) -> LiveGateStatus {
    let mut status = LiveGateStatus {
        request: Some(request.clone()),
        checked_at_ms: unix_ms_now(),
        is_live: None,
        stream_id: None,
        stream_title: None,
        last_error: None,
    };

    let Some(client) = client else {
        status.last_error = Some("HTTP client initialization failed".to_string());
        return status;
    };

    let url = format!(
        "{}/api/streams/live",
        request.base_url.trim_end_matches('/')
    );
    let response = match client.get(&url).send() {
        Ok(response) => response,
        Err(err) => {
            status.last_error = Some(format!("request failed: {err}"));
            return status;
        }
    };
    if !response.status().is_success() {
        status.last_error = Some(format!("unexpected status {}", response.status()));
        return status;
    }
    let payload = match response.json::<serde_json::Value>() {
        Ok(payload) => payload,
        Err(err) => {
            status.last_error = Some(format!("invalid json: {err}"));
            return status;
        }
    };
    let streams = match payload.get("streams").and_then(|value| value.as_array()) {
        Some(streams) => streams,
        None => {
            status.last_error = Some("missing streams array".to_string());
            return status;
        }
    };

    let creator_username = request.creator_username.trim();
    for stream in streams {
        let stream_creator = stream
            .get("creatorUsername")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let status_live = stream
            .get("status")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("live"));
        let creator_live = stream
            .get("creatorIsLive")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);

        if stream_creator.eq_ignore_ascii_case(creator_username) && (status_live || creator_live) {
            status.is_live = Some(true);
            status.stream_id = stream
                .get("streamId")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string());
            status.stream_title = stream
                .get("title")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string());
            return status;
        }
    }

    status.is_live = Some(false);
    status
}

fn option_bool_text(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

fn mapping_to_row_form(mapping: &Mapping) -> MappingRowForm {
    MappingRowForm {
        address: mapping.address.clone(),
        weight: mapping.weight.to_string(),
        deadzone: mapping.deadzone.to_string(),
        invert: mapping.invert,
        curve: mapping.curve,
        min: mapping.min.to_string(),
        max: mapping.max.to_string(),
    }
}

fn available_curves() -> [Curve; 4] {
    [
        Curve::Linear,
        Curve::EaseOutQuad,
        Curve::EaseInQuad,
        Curve::EaseInOutQuad,
    ]
}

fn curve_label(curve: Curve) -> &'static str {
    match curve {
        Curve::Linear => "Linear",
        Curve::EaseOutQuad => "EaseOutQuad",
        Curve::EaseInQuad => "EaseInQuad",
        Curve::EaseInOutQuad => "EaseInOutQuad",
    }
}

impl DesktopApp {
    fn handle_tray_menu_events(&mut self, ctx: &egui::Context) {
        let mut open_requested = false;
        let mut exit_requested = false;
        let mut last_debug_event: Option<String> = None;
        while let Ok(command) = self.tray_command_rx.try_recv() {
            match command {
                TrayCommand::OpenWindow => open_requested = true,
                TrayCommand::ExitApp => exit_requested = true,
                TrayCommand::DebugEvent(id) => last_debug_event = Some(id),
            }
        }

        if let Some(id) = last_debug_event {
            append_tray_debug_log(&format!("menu-event-received-in-update: {}", id));
            self.set_status(format!("Tray menu click received: {id}"), false);
        }

        if exit_requested {
            append_tray_debug_log("menu-event-handled-exit");
            self.hidden_to_tray = false;
            self.close_to_background = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        if open_requested {
            append_tray_debug_log("menu-event-handled-open");
            self.hidden_to_tray = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.set_status("Vibealong restored from tray", false);
        }
    }
}

impl eframe::App for DesktopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_tray_bridge_shared();
        if !self.style_applied {
            apply_vibealong_fonts(ctx);
            apply_vibealong_style(ctx);
            self.style_applied = true;
        }

        ctx.request_repaint_after(Duration::from_millis(200));
        self.handle_tray_menu_events(ctx);
        self.poll_companion_browser_login();

        if ctx.input(|i| i.viewport().close_requested()) && self.close_to_background {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.tray.is_some() {
                if !self.hidden_to_tray {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    self.hidden_to_tray = true;
                    self.set_status(
                        "Window hidden to tray. Use tray icon -> Open Vibealong to restore.",
                        false,
                    );
                }
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                self.set_status(
                    "Tray unavailable, window minimized to taskbar instead.",
                    true,
                );
            }
        }

        if self.last_runtime_refresh.elapsed() >= Duration::from_millis(250) {
            self.sync_live_gate_worker_request(false);
            self.refresh_live_gate_from_worker();
            self.apply_auto_bridge_policy();
            self.refresh_runtime_views(matches!(self.tab, DesktopTab::Logs));
            self.refresh_diagnostics_from_worker();
            self.last_runtime_refresh = Instant::now();
        }
        if self.selected_profile_has_stream_key && self.tab == DesktopTab::Login {
            self.tab = DesktopTab::Home;
        }
        self.sync_tray_bridge_shared();
        self.refresh_tray_menu_state();

        let bridge_sources = self.build_intiface_source_values();
        let bridge_routes = self.build_intiface_route_rules();
        if let Some(handle) = self.intiface_bridge.as_ref() {
            handle.set_source_values(bridge_sources);
            handle.set_routes(bridge_routes);
            self.intiface_bridge_snapshot = handle.snapshot();
        } else {
            self.intiface_bridge_snapshot = IntifaceBridgeSnapshot::default();
        }

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.add_space(7.0);
            ui.horizontal(|ui| {
                for tab in DesktopTab::all() {
                    if tab == DesktopTab::Login && self.selected_profile_has_stream_key {
                        continue;
                    }
                    if ui.selectable_label(self.tab == tab, tab.label()).clicked() {
                        self.tab = tab;
                    }
                }
                if self.selected_profile_has_stream_key {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Log out")
                                        .color(egui::Color32::from_rgb(255, 210, 210)),
                                )
                                .fill(egui::Color32::from_rgba_unmultiplied(170, 50, 50, 34))
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    egui::Color32::from_rgb(190, 80, 80),
                                )),
                            )
                            .clicked()
                        {
                            self.logout_current_profile();
                        }
                    });
                }
            });
            ui.add_space(2.0);
        });

        egui::TopBottomPanel::bottom("status_panel").show(ctx, |ui| {
            let text = if self.status_message.is_empty() {
                "Ready".to_string()
            } else {
                self.status_message.clone()
            };
            if self.status_is_error {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), text);
            } else {
                ui.colored_label(egui::Color32::from_rgb(120, 220, 160), text);
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            DesktopTab::Login => self.render_login_tab(ui),
            DesktopTab::Setup => self.render_setup_tab(ui),
            DesktopTab::Home => self.render_home_tab(ui),
            DesktopTab::Logs => self.render_logs_tab(ui),
            DesktopTab::AvatarDebugger => self.render_avatar_tab(ui),
            DesktopTab::Settings => self.render_settings_tab(ui),
            DesktopTab::Diagnostics => self.render_diagnostics_tab(ui),
            DesktopTab::Intiface => self.render_intiface_tab(ui),
        });
    }
}
