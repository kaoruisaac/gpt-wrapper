use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::{Color, PageLoadEvent, Webview, WebviewBuilder, WebviewWindowBuilder},
    window::{Window, WindowBuilder},
    AppHandle, LogicalPosition, LogicalSize, Manager, WebviewUrl, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use webview2_com::{
    CallDevToolsProtocolMethodCompletedHandler, ExecuteScriptCompletedHandler,
    Microsoft::Web::WebView2::Win32::ICoreWebView2,
};
use windows::{
    core::HSTRING,
    Win32::UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow, SW_RESTORE},
};

const ADAPTER_SCRIPT: &str = include_str!("chatgpt_adapter.js");
const TAB_BAR_SCRIPT: &str = include_str!("tab_bar.js");
const MAIN_WINDOW_LABEL: &str = "main";
const SETTINGS_WINDOW_LABEL: &str = "settings";
const HOST_WEBVIEW_LABEL: &str = "tab-host";
const CHATGPT_WEBVIEW_PREFIX: &str = "chatgpt-tab-";
const TAB_BOOTSTRAP_PATH: &str = "loading.html";
const TAB_BAR_HEIGHT: f64 = 38.0;
const LOADING_BACKGROUND: Color = Color(0, 0, 0, 255);
const AUTOSTART_ARG: &str = "--autostart";
const TRAY_OPEN_ID: &str = "tray-open";
const TRAY_SETTINGS_ID: &str = "tray-settings";
const TRAY_QUIT_ID: &str = "tray-quit";

#[derive(Debug, Deserialize)]
struct Rect {
    x: f64,
    y: f64,
}

#[derive(Clone, Copy, Debug)]
enum DictationTarget {
    Start,
    Finish,
}

impl DictationTarget {
    fn rect_script(self) -> String {
        let labels = match self {
            Self::Start => ["聽寫", "開始聽寫", "开始听写", "Start dictation", "Dictate"],
            Self::Finish => [
                "轉錄並傳送",
                "送出聽寫內容",
                "提交听写内容",
                "Submit dictation",
                "Transcribe and send",
            ],
        };

        format!(
            r#"(() => {{
  const labels = {};
  for (const b of document.querySelectorAll("button")) {{
    const label = (b.getAttribute("aria-label") || "").trim();
    const r = b.getBoundingClientRect();
    if (r.width > 0 && r.height > 0 && labels.includes(label)) {{
      return {{ x: r.left + r.width / 2, y: r.top + r.height / 2 }};
    }}
  }}
  return null;
}})()"#,
            serde_json::to_string(&labels).unwrap()
        )
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TabInfo {
    id: usize,
    title: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TabSnapshot {
    tabs: Vec<TabInfo>,
    active_tab_id: Option<usize>,
}

#[derive(Debug)]
struct TabManager {
    tabs: Vec<usize>,
    loaded_tabs: HashSet<usize>,
    active_tab_id: Option<usize>,
    next_tab_id: usize,
    standby_ptt_tab_id: Option<usize>,
    standby_ptt_ready: bool,
}

impl TabManager {
    fn new() -> Self {
        Self {
            tabs: Vec::new(),
            loaded_tabs: HashSet::new(),
            active_tab_id: None,
            next_tab_id: 1,
            standby_ptt_tab_id: None,
            standby_ptt_ready: false,
        }
    }

    fn snapshot(&self) -> TabSnapshot {
        TabSnapshot {
            tabs: self
                .tabs
                .iter()
                .copied()
                .map(|id| TabInfo {
                    id,
                    title: "ChatGPT".to_string(),
                })
                .collect(),
            active_tab_id: self.active_tab_id,
        }
    }
}

struct TabState(Mutex<TabManager>);

impl TabState {
    fn new() -> Self {
        Self(Mutex::new(TabManager::new()))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, TabManager>, String> {
        self.0
            .lock()
            .map_err(|_| "tab state lock was poisoned".to_string())
    }
}

struct PttTracker {
    held: AtomicBool,
    target_tab_id: Mutex<Option<usize>>,
    generation: AtomicUsize,
    scheduled_generation: AtomicUsize,
    started_generation: AtomicUsize,
}

struct PttFinish {
    tab_id: usize,
    started: bool,
}

impl PttTracker {
    fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            target_tab_id: Mutex::new(None),
            generation: AtomicUsize::new(0),
            scheduled_generation: AtomicUsize::new(0),
            started_generation: AtomicUsize::new(0),
        }
    }

    fn is_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    fn begin(&self, tab_id: usize) -> Option<usize> {
        if self.held.swap(true, Ordering::SeqCst) {
            return None;
        }

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.scheduled_generation.store(0, Ordering::SeqCst);
        self.started_generation.store(0, Ordering::SeqCst);
        if let Ok(mut target) = self.target_tab_id.lock() {
            *target = Some(tab_id);
        }
        Some(generation)
    }

    fn finish(&self) -> Option<PttFinish> {
        if !self.held.swap(false, Ordering::SeqCst) {
            return None;
        }

        let generation = self.generation.load(Ordering::SeqCst);
        let tab_id = self.target_tab_id.lock().ok()?.take()?;
        Some(PttFinish {
            tab_id,
            started: self.started_generation.load(Ordering::SeqCst) == generation,
        })
    }

    fn current_generation(&self, tab_id: usize) -> Option<usize> {
        let generation = self.generation.load(Ordering::SeqCst);
        self.is_current(tab_id, generation).then_some(generation)
    }

    fn claim_start(&self, tab_id: usize, generation: usize) -> bool {
        if !self.is_current(tab_id, generation) {
            return false;
        }

        self.scheduled_generation
            .compare_exchange(0, generation, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn mark_started(&self, tab_id: usize, generation: usize) -> bool {
        if !self.is_current(tab_id, generation) {
            return false;
        }

        self.started_generation.store(generation, Ordering::SeqCst);
        true
    }

    fn is_current(&self, tab_id: usize, generation: usize) -> bool {
        if !self.held.load(Ordering::SeqCst) || self.generation.load(Ordering::SeqCst) != generation
        {
            return false;
        }

        self.target_tab_id
            .lock()
            .map(|target| *target == Some(tab_id))
            .unwrap_or(false)
    }
}

struct PttState(Arc<PttTracker>);

struct TrayVisibilityState {
    tray_hidden: AtomicBool,
}

impl TrayVisibilityState {
    fn new() -> Self {
        Self {
            tray_hidden: AtomicBool::new(false),
        }
    }
}

struct SettingsWindowState {
    opening: AtomicBool,
}

impl SettingsWindowState {
    fn new() -> Self {
        Self {
            opening: AtomicBool::new(false),
        }
    }
}

fn tab_label(tab_id: usize) -> String {
    format!("{CHATGPT_WEBVIEW_PREFIX}{tab_id}")
}

fn tab_id_from_label(label: &str) -> Option<usize> {
    label
        .strip_prefix(CHATGPT_WEBVIEW_PREFIX)
        .and_then(|value| value.parse().ok())
}

fn snapshot(app_handle: &AppHandle) -> Result<TabSnapshot, String> {
    Ok(app_handle.state::<TabState>().lock()?.snapshot())
}

fn content_size(window: &Window) -> Result<(f64, f64), String> {
    let scale_factor = window
        .scale_factor()
        .map_err(|error| format!("failed to read window scale factor: {error}"))?;
    let inner_size = window
        .inner_size()
        .map_err(|error| format!("failed to read window size: {error}"))?;
    let logical_size = inner_size.to_logical::<f64>(scale_factor);

    Ok((
        logical_size.width.max(1.0),
        (logical_size.height - TAB_BAR_HEIGHT).max(1.0),
    ))
}

fn layout_webviews(window: &Window) -> Result<(), String> {
    let (width, content_height) = content_size(window)?;

    for webview in window.webviews() {
        if webview.label() == HOST_WEBVIEW_LABEL {
            webview
                .set_position(LogicalPosition::new(0.0, 0.0))
                .map_err(|error| format!("failed to position tab host: {error}"))?;
            webview
                .set_size(LogicalSize::new(width, TAB_BAR_HEIGHT))
                .map_err(|error| format!("failed to resize tab host: {error}"))?;
        } else if tab_id_from_label(webview.label()).is_some() {
            webview
                .set_position(LogicalPosition::new(0.0, TAB_BAR_HEIGHT))
                .map_err(|error| format!("failed to position ChatGPT tab: {error}"))?;
            webview
                .set_size(LogicalSize::new(width, content_height))
                .map_err(|error| format!("failed to resize ChatGPT tab: {error}"))?;
        }
    }

    Ok(())
}

fn get_tab_webview(app_handle: &AppHandle, tab_id: usize) -> Option<Webview> {
    app_handle.get_webview(&tab_label(tab_id))
}

fn is_tab_loaded(app_handle: &AppHandle, tab_id: usize) -> bool {
    app_handle
        .state::<TabState>()
        .lock()
        .map(|tabs| tabs.loaded_tabs.contains(&tab_id))
        .unwrap_or(false)
}

fn create_chatgpt_webview(app_handle: &AppHandle, tab_id: usize) -> Result<Webview, String> {
    let window = app_handle
        .get_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| "main window is missing".to_string())?;
    let (width, content_height) = content_size(&window)?;
    let chatgpt_url = "https://chatgpt.com"
        .parse()
        .map_err(|error| format!("invalid ChatGPT URL: {error}"))?;

    let builder = WebviewBuilder::new(
        tab_label(tab_id),
        WebviewUrl::App(TAB_BOOTSTRAP_PATH.into()),
    )
    .initialization_script(ADAPTER_SCRIPT)
    .background_color(LOADING_BACKGROUND)
    .focused(false)
    .on_page_load(move |webview, payload| {
        if payload.event() != PageLoadEvent::Finished
            || payload.url().path().ends_with(TAB_BOOTSTRAP_PATH)
        {
            return;
        }

        let app_handle = webview.app_handle();
        let state = app_handle.state::<TabState>();
        let (should_show, became_normal_loaded) = {
            let Ok(mut tabs) = state.lock() else {
                return;
            };

            if tabs.standby_ptt_tab_id == Some(tab_id) {
                if tabs.standby_ptt_ready {
                    return;
                }
                tabs.standby_ptt_ready = true;
                (false, false)
            } else if tabs.tabs.contains(&tab_id) {
                if !tabs.loaded_tabs.insert(tab_id) {
                    return;
                }
                (tabs.active_tab_id == Some(tab_id), true)
            } else {
                return;
            }
        };

        let window = webview.window();
        let Ok((width, content_height)) = content_size(&window) else {
            return;
        };

        if !should_show {
            let _ = webview.hide();
        }

        let _ = webview.set_position(LogicalPosition::new(0.0, TAB_BAR_HEIGHT));
        let _ = webview.set_size(LogicalSize::new(width, content_height));

        if should_show {
            let _ = webview.show();
            let _ = webview.set_focus();
        }

        if became_normal_loaded {
            let tracker = Arc::clone(&app_handle.state::<PttState>().0);
            if let Some(generation) = tracker.current_generation(tab_id) {
                schedule_ptt_start(webview.clone(), tracker, tab_id, generation, false);
            }
        }
    });

    let webview = window
        .add_child(
            builder,
            LogicalPosition::new(0.0, TAB_BAR_HEIGHT),
            LogicalSize::new(width, content_height),
        )
        .map_err(|error| format!("failed to create ChatGPT tab {tab_id}: {error}"))?;

    if let Err(error) = webview.hide() {
        let _ = webview.close();
        return Err(format!(
            "failed to hide ChatGPT tab {tab_id} before navigation: {error}"
        ));
    }

    if let Err(error) = webview.navigate(chatgpt_url) {
        let _ = webview.close();
        return Err(format!("failed to navigate ChatGPT tab {tab_id}: {error}"));
    }

    Ok(webview)
}

fn create_tab_internal(app_handle: &AppHandle) -> Result<TabSnapshot, String> {
    let state = app_handle.state::<TabState>();
    let (tab_id, previous_active) = {
        let mut tabs = state.lock()?;
        let tab_id = tabs.next_tab_id;
        tabs.next_tab_id += 1;
        let previous_active = tabs.active_tab_id;
        tabs.tabs.push(tab_id);
        tabs.active_tab_id = Some(tab_id);
        (tab_id, previous_active)
    };

    if let Err(error) = create_chatgpt_webview(app_handle, tab_id) {
        let mut tabs = state.lock()?;
        tabs.tabs.retain(|id| *id != tab_id);
        tabs.loaded_tabs.remove(&tab_id);
        tabs.active_tab_id = previous_active;
        return Err(error);
    }

    if let Some(previous_id) = previous_active {
        if let Some(previous) = get_tab_webview(app_handle, previous_id) {
            if let Err(error) = previous.hide() {
                eprintln!("[gpt-wrapper] failed to hide previous tab {previous_id}: {error:?}");
            }
        }
    }

    snapshot(app_handle)
}

fn ensure_standby_ptt_tab(app_handle: &AppHandle) -> Result<(), String> {
    let state = app_handle.state::<TabState>();
    let tab_id = {
        let mut tabs = state.lock()?;
        if tabs.standby_ptt_tab_id.is_some() {
            return Ok(());
        }

        let tab_id = tabs.next_tab_id;
        tabs.next_tab_id += 1;
        tabs.standby_ptt_tab_id = Some(tab_id);
        tabs.standby_ptt_ready = false;
        tab_id
    };

    if let Err(error) = create_chatgpt_webview(app_handle, tab_id) {
        let mut tabs = state.lock()?;
        if tabs.standby_ptt_tab_id == Some(tab_id) {
            tabs.standby_ptt_tab_id = None;
            tabs.standby_ptt_ready = false;
        }
        tabs.loaded_tabs.remove(&tab_id);
        return Err(error);
    }

    println!("[gpt-wrapper] standby PTT tab {tab_id} warming in background");
    Ok(())
}

fn discard_standby_ptt_tab(app_handle: &AppHandle) {
    let standby_id = {
        let state = app_handle.state::<TabState>();
        let Ok(mut tabs) = state.lock() else {
            return;
        };

        let standby_id = tabs.standby_ptt_tab_id.take();
        tabs.standby_ptt_ready = false;
        if let Some(tab_id) = standby_id {
            tabs.loaded_tabs.remove(&tab_id);
        }
        standby_id
    };

    if let Some(tab_id) = standby_id {
        if let Some(webview) = get_tab_webview(app_handle, tab_id) {
            if let Err(error) = webview.close() {
                eprintln!("[gpt-wrapper] failed to discard standby tab {tab_id}: {error:?}");
            }
        }
    }
}

fn promote_standby_ptt_tab(app_handle: &AppHandle) -> Result<(usize, bool), String> {
    let state = app_handle.state::<TabState>();
    let standby_id = state
        .lock()?
        .standby_ptt_tab_id
        .ok_or_else(|| "standby PTT tab is missing".to_string())?;
    let webview = get_tab_webview(app_handle, standby_id)
        .ok_or_else(|| format!("standby PTT webview {standby_id} is missing"))?;

    let (previous_active, ready) = {
        let mut tabs = state.lock()?;
        if tabs.standby_ptt_tab_id != Some(standby_id) {
            return Err("standby PTT tab changed during promotion".to_string());
        }

        let previous_active = tabs.active_tab_id;
        let ready = tabs.standby_ptt_ready;
        tabs.standby_ptt_tab_id = None;
        tabs.standby_ptt_ready = false;
        tabs.tabs.push(standby_id);
        tabs.active_tab_id = Some(standby_id);
        if ready {
            tabs.loaded_tabs.insert(standby_id);
        }
        (previous_active, ready)
    };

    if let Some(previous_id) = previous_active {
        if let Some(previous) = get_tab_webview(app_handle, previous_id) {
            if let Err(error) = previous.hide() {
                eprintln!("[gpt-wrapper] failed to hide previous tab {previous_id}: {error:?}");
            }
        }
    }

    if ready {
        if let Err(error) = webview.show() {
            eprintln!("[gpt-wrapper] failed to show promoted tab {standby_id}: {error:?}");
        }
        if let Err(error) = webview.set_focus() {
            eprintln!("[gpt-wrapper] failed to focus promoted tab {standby_id}: {error:?}");
        }
    }

    println!("[gpt-wrapper] promoted standby PTT tab {standby_id}");
    Ok((standby_id, ready))
}

fn switch_tab_internal(app_handle: &AppHandle, tab_id: usize) -> Result<TabSnapshot, String> {
    let state = app_handle.state::<TabState>();
    let previous_active = {
        let tabs = state.lock()?;
        if !tabs.tabs.contains(&tab_id) {
            return Err(format!("tab {tab_id} does not exist"));
        }
        tabs.active_tab_id
    };

    let target = get_tab_webview(app_handle, tab_id)
        .ok_or_else(|| format!("webview for tab {tab_id} is missing"))?;
    let is_loaded = state.lock()?.loaded_tabs.contains(&tab_id);

    if previous_active != Some(tab_id) {
        if is_loaded {
            target
                .show()
                .map_err(|error| format!("failed to show tab {tab_id}: {error}"))?;
        }

        if let Some(previous_id) = previous_active {
            if let Some(previous) = get_tab_webview(app_handle, previous_id) {
                previous
                    .hide()
                    .map_err(|error| format!("failed to hide tab {previous_id}: {error}"))?;
            }
        }

        let mut tabs = state.lock()?;
        tabs.active_tab_id = Some(tab_id);
    }

    if is_loaded {
        target
            .set_focus()
            .map_err(|error| format!("failed to focus tab {tab_id}: {error}"))?;
    }

    snapshot(app_handle)
}

fn close_tab_internal(app_handle: &AppHandle, tab_id: usize) -> Result<TabSnapshot, String> {
    let state = app_handle.state::<TabState>();
    let (was_active, replacement) = {
        let tabs = state.lock()?;
        let index = tabs
            .tabs
            .iter()
            .position(|id| *id == tab_id)
            .ok_or_else(|| format!("tab {tab_id} does not exist"))?;

        let was_active = tabs.active_tab_id == Some(tab_id);
        let replacement = if was_active {
            if index + 1 < tabs.tabs.len() {
                Some(tabs.tabs[index + 1])
            } else if index > 0 {
                Some(tabs.tabs[index - 1])
            } else {
                None
            }
        } else {
            tabs.active_tab_id
        };

        (was_active, replacement)
    };

    let webview = get_tab_webview(app_handle, tab_id)
        .ok_or_else(|| format!("webview for tab {tab_id} is missing"))?;
    webview
        .close()
        .map_err(|error| format!("failed to close tab {tab_id}: {error}"))?;

    {
        let mut tabs = state.lock()?;
        tabs.tabs.retain(|id| *id != tab_id);
        tabs.loaded_tabs.remove(&tab_id);
        if was_active {
            tabs.active_tab_id = replacement;
        }
    }

    if was_active {
        if let Some(replacement_id) = replacement {
            if is_tab_loaded(app_handle, replacement_id) {
                if let Some(replacement_webview) = get_tab_webview(app_handle, replacement_id) {
                    replacement_webview
                        .show()
                        .map_err(|error| format!("failed to show tab {replacement_id}: {error}"))?;
                    replacement_webview.set_focus().map_err(|error| {
                        format!("failed to focus tab {replacement_id}: {error}")
                    })?;
                }
            }
        }
    }

    let needs_replacement = state.lock()?.tabs.is_empty();
    if needs_replacement {
        return create_tab_internal(app_handle);
    }

    snapshot(app_handle)
}

fn ensure_host_webview(webview: &Webview) -> Result<(), String> {
    if webview.label() == HOST_WEBVIEW_LABEL {
        Ok(())
    } else {
        Err("tab commands are only available to the host UI".to_string())
    }
}

#[tauri::command]
fn get_tabs(app: AppHandle, webview: Webview) -> Result<TabSnapshot, String> {
    ensure_host_webview(&webview)?;
    snapshot(&app)
}

#[tauri::command]
async fn create_tab(app: AppHandle, webview: Webview) -> Result<TabSnapshot, String> {
    ensure_host_webview(&webview)?;
    create_tab_internal(&app)
}

#[tauri::command]
fn switch_tab(app: AppHandle, webview: Webview, tab_id: usize) -> Result<TabSnapshot, String> {
    ensure_host_webview(&webview)?;
    switch_tab_internal(&app, tab_id)
}

#[tauri::command]
async fn close_tab(app: AppHandle, webview: Webview, tab_id: usize) -> Result<TabSnapshot, String> {
    ensure_host_webview(&webview)?;
    close_tab_internal(&app, tab_id)
}

fn ensure_settings_webview(webview: &Webview) -> Result<(), String> {
    if webview.label() == SETTINGS_WINDOW_LABEL {
        Ok(())
    } else {
        Err("settings commands are only available to the Settings window".to_string())
    }
}

#[tauri::command]
fn get_autostart_enabled(app: AppHandle, webview: Webview) -> Result<bool, String> {
    ensure_settings_webview(&webview)?;

    app.autolaunch().is_enabled().map_err(|error| {
        let message = format!("failed to read autostart state: {error}");
        eprintln!("[gpt-wrapper] {message}");
        message
    })
}

#[tauri::command]
fn set_autostart_enabled(app: AppHandle, webview: Webview, enabled: bool) -> Result<bool, String> {
    ensure_settings_webview(&webview)?;

    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };

    if let Err(error) = result {
        let message = format!(
            "failed to {} autostart: {error}",
            if enabled { "enable" } else { "disable" }
        );
        eprintln!("[gpt-wrapper] {message}");
        return Err(message);
    }

    manager.is_enabled().map_err(|error| {
        let message = format!("failed to confirm autostart state: {error}");
        eprintln!("[gpt-wrapper] {message}");
        message
    })
}

fn devtools_mouse_event(
    webview: &ICoreWebView2,
    event_type: &str,
    x: f64,
    y: f64,
    button: &str,
    buttons: u32,
) {
    let method = HSTRING::from("Input.dispatchMouseEvent");
    let params = HSTRING::from(
        serde_json::json!({
            "type": event_type,
            "x": x,
            "y": y,
            "button": button,
            "buttons": buttons,
            "clickCount": 1
        })
        .to_string(),
    );

    unsafe {
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
            move |result, _response| {
                if let Err(error) = result {
                    eprintln!("[gpt-wrapper] DevTools mouse event failed: {error:?}");
                }
                Ok(())
            },
        ));

        if let Err(error) = webview.CallDevToolsProtocolMethod(&method, &params, &handler) {
            eprintln!("[gpt-wrapper] CallDevToolsProtocolMethod failed: {error:?}");
        }
    }
}

fn dispatch_trusted_click(webview: &ICoreWebView2, rect: Rect) {
    devtools_mouse_event(webview, "mouseMoved", rect.x, rect.y, "none", 0);
    devtools_mouse_event(webview, "mousePressed", rect.x, rect.y, "left", 1);
    devtools_mouse_event(webview, "mouseReleased", rect.x, rect.y, "left", 0);
}

fn restore_to_foreground(window: &Window) {
    let _ = window.show();

    match window.hwnd() {
        Ok(hwnd) => unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        },
        Err(error) => {
            eprintln!("[gpt-wrapper] hwnd unavailable: {error:?}");
            let _ = window.unminimize();
        }
    }

    let _ = window.set_focus();
}

fn active_tab_id(app_handle: &AppHandle) -> Option<usize> {
    app_handle
        .state::<TabState>()
        .lock()
        .ok()
        .and_then(|tabs| tabs.active_tab_id)
}

fn focus_active_tab(app_handle: &AppHandle) {
    let Some(tab_id) = active_tab_id(app_handle) else {
        return;
    };

    if !is_tab_loaded(app_handle, tab_id) {
        return;
    }

    if let Some(webview) = get_tab_webview(app_handle, tab_id) {
        let _ = webview.show();
        if let Err(error) = webview.set_focus() {
            eprintln!("[gpt-wrapper] failed to focus active tab {tab_id}: {error:?}");
        }
    }
}

fn refresh_tab_host(app_handle: &AppHandle) {
    if let Some(host) = app_handle.get_webview(HOST_WEBVIEW_LABEL) {
        if let Err(error) = host.eval("window.__GPT_WRAPPER_TAB_HOST__?.refresh?.();") {
            eprintln!("[gpt-wrapper] failed to refresh tab host: {error:?}");
        }
    }
}

fn restore_main_window_from_tray(app_handle: &AppHandle) {
    app_handle
        .state::<TrayVisibilityState>()
        .tray_hidden
        .store(false, Ordering::SeqCst);
    discard_standby_ptt_tab(app_handle);

    match app_handle.get_window(MAIN_WINDOW_LABEL) {
        Some(window) => {
            restore_to_foreground(&window);
            focus_active_tab(app_handle);
        }
        None => eprintln!("[gpt-wrapper] main window missing"),
    }
}

fn restore_settings_window(window: &tauri::WebviewWindow) {
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

fn open_settings_window(app_handle: &AppHandle) {
    if let Some(window) = app_handle.get_webview_window(SETTINGS_WINDOW_LABEL) {
        restore_settings_window(&window);
        return;
    }

    let state = app_handle.state::<SettingsWindowState>();
    if state
        .opening
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let app_handle = app_handle.clone();
    thread::spawn(move || {
        if let Some(window) = app_handle.get_webview_window(SETTINGS_WINDOW_LABEL) {
            restore_settings_window(&window);
            app_handle
                .state::<SettingsWindowState>()
                .opening
                .store(false, Ordering::SeqCst);
            return;
        }

        let result = WebviewWindowBuilder::new(
            &app_handle,
            SETTINGS_WINDOW_LABEL,
            WebviewUrl::App("settings/index.html".into()),
        )
        .title("GPT Wrapper Settings")
        .inner_size(640.0, 460.0)
        .min_inner_size(520.0, 360.0)
        .build();

        match result {
            Ok(window) => restore_settings_window(&window),
            Err(error) => eprintln!("[gpt-wrapper] failed to create Settings window: {error:?}"),
        }

        app_handle
            .state::<SettingsWindowState>()
            .opening
            .store(false, Ordering::SeqCst);
    });
}

fn setup_tray(app: &tauri::App, quitting: Arc<AtomicBool>) -> tauri::Result<()> {
    let open_item = MenuItemBuilder::with_id(TRAY_OPEN_ID, "Open GPT Wrapper").build(app)?;
    let settings_item = MenuItemBuilder::with_id(TRAY_SETTINGS_ID, "Settings").build(app)?;
    let quit_item = MenuItemBuilder::with_id(TRAY_QUIT_ID, "Quit").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&open_item, &settings_item, &quit_item])
        .build()?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/icon.ico"))?;

    let quitting_for_menu = Arc::clone(&quitting);
    TrayIconBuilder::new()
        .icon(icon)
        .tooltip("GPT Wrapper")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app_handle, event| match event.id().as_ref() {
            TRAY_OPEN_ID => restore_main_window_from_tray(app_handle),
            TRAY_SETTINGS_ID => open_settings_window(app_handle),
            TRAY_QUIT_ID => {
                quitting_for_menu.store(true, Ordering::SeqCst);
                discard_standby_ptt_tab(app_handle);
                app_handle.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                restore_main_window_from_tray(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

fn trusted_click(webview: &Webview, target: DictationTarget) {
    let script = HSTRING::from(target.rect_script());

    let result = webview.with_webview(move |platform_webview| {
        let controller = platform_webview.controller();

        let webview = match unsafe { controller.CoreWebView2() } {
            Ok(webview) => webview,
            Err(error) => {
                eprintln!("[gpt-wrapper] CoreWebView2 unavailable: {error:?}");
                return;
            }
        };

        let webview_for_callback = webview.clone();

        unsafe {
            let handler = ExecuteScriptCompletedHandler::create(Box::new(move |result, raw| {
                if let Err(error) = result {
                    eprintln!("[gpt-wrapper] button lookup script failed: {error:?}");
                    return Ok(());
                }

                match serde_json::from_str::<Option<Rect>>(&raw) {
                    Ok(Some(rect)) => {
                        println!(
                            "[gpt-wrapper] trusted click {:?} at ({:.1}, {:.1})",
                            target, rect.x, rect.y
                        );
                        dispatch_trusted_click(&webview_for_callback, rect);
                    }
                    Ok(None) => {
                        eprintln!(
                            "[gpt-wrapper] visible {:?} dictation button not found",
                            target
                        );
                    }
                    Err(error) => {
                        eprintln!("[gpt-wrapper] could not parse button rect {raw:?}: {error}");
                    }
                }

                Ok(())
            }));

            if let Err(error) = webview.ExecuteScript(&script, &handler) {
                eprintln!("[gpt-wrapper] ExecuteScript failed: {error:?}");
            }
        }
    });

    if let Err(error) = result {
        eprintln!("[gpt-wrapper] with_webview failed: {error:?}");
    }
}

fn schedule_ptt_start(
    webview: Webview,
    tracker: Arc<PttTracker>,
    tab_id: usize,
    generation: usize,
    consider_started_immediately: bool,
) {
    if !tracker.claim_start(tab_id, generation) {
        return;
    }
    if consider_started_immediately && !tracker.mark_started(tab_id, generation) {
        return;
    }

    println!("[gpt-wrapper] starting dictation on tab {tab_id}");
    let _ = webview.eval("window.__GPT_WRAPPER__?.prepareStart?.();");

    thread::spawn(move || {
        let mut needs_started_mark = !consider_started_immediately;
        for delay_ms in [180_u64, 300_u64] {
            thread::sleep(Duration::from_millis(delay_ms));
            if !tracker.is_current(tab_id, generation) {
                return;
            }
            if needs_started_mark {
                if !tracker.mark_started(tab_id, generation) {
                    return;
                }
                needs_started_mark = false;
            }
            trusted_click(&webview, DictationTarget::Start);
        }
    });
}

fn handle_ptt_shortcut(
    app_handle: &AppHandle,
    event_state: ShortcutState,
    tracker: &Arc<PttTracker>,
) {
    match event_state {
        ShortcutState::Pressed => {
            if tracker.is_held() {
                return;
            }

            let was_tray_hidden = app_handle
                .state::<TrayVisibilityState>()
                .tray_hidden
                .swap(false, Ordering::SeqCst);

            if was_tray_hidden {
                let (tab_id, ready_before_session) = match promote_standby_ptt_tab(app_handle) {
                    Ok((tab_id, ready)) => (tab_id, ready),
                    Err(error) => {
                        eprintln!(
                            "[gpt-wrapper] standby promotion failed, creating fallback tab: {error}"
                        );
                        discard_standby_ptt_tab(app_handle);
                        if let Err(create_error) = create_tab_internal(app_handle) {
                            eprintln!(
                                "[gpt-wrapper] failed to create fallback PTT tab: {create_error}"
                            );
                            if let Some(window) = app_handle.get_window(MAIN_WINDOW_LABEL) {
                                restore_to_foreground(&window);
                                focus_active_tab(app_handle);
                            }
                            return;
                        }

                        let Some(tab_id) = active_tab_id(app_handle) else {
                            eprintln!("[gpt-wrapper] fallback PTT tab missing");
                            return;
                        };
                        (tab_id, is_tab_loaded(app_handle, tab_id))
                    }
                };

                let Some(generation) = tracker.begin(tab_id) else {
                    return;
                };

                if let Some(window) = app_handle.get_window(MAIN_WINDOW_LABEL) {
                    restore_to_foreground(&window);
                } else {
                    eprintln!("[gpt-wrapper] main window missing");
                    return;
                }

                refresh_tab_host(app_handle);

                if is_tab_loaded(app_handle, tab_id) {
                    if let Some(webview) = get_tab_webview(app_handle, tab_id) {
                        let _ = webview.show();
                        let _ = webview.set_focus();
                        schedule_ptt_start(
                            webview,
                            Arc::clone(tracker),
                            tab_id,
                            generation,
                            ready_before_session,
                        );
                    }
                } else {
                    println!(
                        "[gpt-wrapper] background PTT tab {tab_id} still loading; waiting for page ready"
                    );
                }
                return;
            }

            let Some(tab_id) = active_tab_id(app_handle) else {
                eprintln!("[gpt-wrapper] active ChatGPT tab missing");
                return;
            };
            if !is_tab_loaded(app_handle, tab_id) {
                eprintln!("[gpt-wrapper] active ChatGPT tab {tab_id} is still loading");
                return;
            }
            let Some(webview) = get_tab_webview(app_handle, tab_id) else {
                eprintln!("[gpt-wrapper] active ChatGPT webview missing");
                return;
            };
            let Some(generation) = tracker.begin(tab_id) else {
                return;
            };

            if let Some(window) = app_handle.get_window(MAIN_WINDOW_LABEL) {
                restore_to_foreground(&window);
            }
            let _ = webview.set_focus();

            println!("[gpt-wrapper] hotkey pressed on tab {tab_id}");
            schedule_ptt_start(webview, Arc::clone(tracker), tab_id, generation, true);
        }
        ShortcutState::Released => {
            let Some(finish) = tracker.finish() else {
                return;
            };

            if !finish.started {
                println!(
                    "[gpt-wrapper] hotkey released before dictation start on tab {}",
                    finish.tab_id
                );
                return;
            }

            let Some(webview) = get_tab_webview(app_handle, finish.tab_id) else {
                eprintln!(
                    "[gpt-wrapper] dictation target tab {} no longer exists",
                    finish.tab_id
                );
                return;
            };

            println!("[gpt-wrapper] hotkey released on tab {}", finish.tab_id);
            trusted_click(&webview, DictationTarget::Finish);
            let _ = webview.eval("window.__GPT_WRAPPER__?.waitAndSend?.();");
        }
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_tabs,
            create_tab,
            switch_tab,
            close_tab,
            get_autostart_enabled,
            set_autostart_enabled
        ])
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_ARG]),
        ))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let launched_by_autostart = std::env::args().any(|arg| arg == AUTOSTART_ARG);

            app.manage(TabState::new());

            let quitting = Arc::new(AtomicBool::new(false));
            let ptt_tracker = Arc::new(PttTracker::new());
            app.manage(PttState(Arc::clone(&ptt_tracker)));
            app.manage(TrayVisibilityState::new());
            app.manage(SettingsWindowState::new());

            let window = WindowBuilder::new(app, MAIN_WINDOW_LABEL)
                .title("GPT Wrapper")
                .background_color(LOADING_BACKGROUND)
                .inner_size(1200.0, 860.0)
                .min_inner_size(720.0, 540.0)
                .visible(!launched_by_autostart)
                .focused(!launched_by_autostart)
                .build()?;

            let (width, _) = content_size(&window).map_err(tauri::Error::AssetNotFound)?;
            let host_builder =
                WebviewBuilder::new(HOST_WEBVIEW_LABEL, WebviewUrl::App("index.html".into()))
                    .initialization_script(TAB_BAR_SCRIPT)
                    .background_color(LOADING_BACKGROUND)
                    .focused(false);

            window.add_child(
                host_builder,
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(width, TAB_BAR_HEIGHT),
            )?;

            create_tab_internal(app.handle())
                .map_err(|error| tauri::Error::AssetNotFound(error))?;
            layout_webviews(&window).map_err(|error| tauri::Error::AssetNotFound(error))?;

            let window_for_events = window.clone();
            let app_handle_for_events = app.handle().clone();
            let quitting_for_close = Arc::clone(&quitting);
            window.on_window_event(move |event| match event {
                WindowEvent::CloseRequested { api, .. } => {
                    if !quitting_for_close.load(Ordering::SeqCst) {
                        api.prevent_close();
                        app_handle_for_events
                            .state::<TrayVisibilityState>()
                            .tray_hidden
                            .store(true, Ordering::SeqCst);

                        if let Err(error) = window_for_events.hide() {
                            eprintln!("[gpt-wrapper] failed to hide main window: {error:?}");
                        }

                        if let Err(error) = ensure_standby_ptt_tab(&app_handle_for_events) {
                            eprintln!("[gpt-wrapper] failed to prepare standby PTT tab: {error}");
                        }
                    }
                }
                WindowEvent::Resized(_) => {
                    if let Err(error) = layout_webviews(&window_for_events) {
                        eprintln!("[gpt-wrapper] failed to lay out webviews: {error}");
                    }
                }
                _ => {}
            });

            setup_tray(app, Arc::clone(&quitting))?;

            if launched_by_autostart {
                app.state::<TrayVisibilityState>()
                    .tray_hidden
                    .store(true, Ordering::SeqCst);
                if let Err(error) = ensure_standby_ptt_tab(app.handle()) {
                    eprintln!("[gpt-wrapper] failed to prepare autostart standby PTT tab: {error}");
                }
                println!("[gpt-wrapper] launched by Windows autostart in tray-only mode.");
            }

            println!("[gpt-wrapper] main window and initial ChatGPT tab created.");
            println!("[gpt-wrapper] Hold Ctrl+0 or Ctrl+Numpad0 to dictate; release to finish.");

            let primary_shortcut = Shortcut::new(Some(Modifiers::CONTROL), Code::Digit0);
            let numpad_shortcut = Shortcut::new(Some(Modifiers::CONTROL), Code::Numpad0);

            let tracker_for_primary = Arc::clone(&ptt_tracker);
            app.global_shortcut().on_shortcut(
                primary_shortcut,
                move |app_handle, _shortcut, event| {
                    handle_ptt_shortcut(app_handle, event.state(), &tracker_for_primary);
                },
            )?;

            let tracker_for_numpad = Arc::clone(&ptt_tracker);
            app.global_shortcut().on_shortcut(
                numpad_shortcut,
                move |app_handle, _shortcut, event| {
                    handle_ptt_shortcut(app_handle, event.state(), &tracker_for_numpad);
                },
            )?;

            if !launched_by_autostart {
                window.set_focus()?;
                focus_active_tab(app.handle());
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running GPT Wrapper");
}
