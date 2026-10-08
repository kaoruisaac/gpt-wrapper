#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ble_central;
mod general_settings;
mod project_settings;

use ble_central::{BleCentral, ControlEvent};
use general_settings::GeneralSettingsState;
use project_settings::{ProjectSettings, ProjectSettingsState, TabSpec};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
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
use uuid::Uuid;
use webview2_com::{
    CallDevToolsProtocolMethodCompletedHandler, CoTaskMemPWSTR, ExecuteScriptCompletedHandler,
    Microsoft::Web::WebView2::Win32::ICoreWebView2, WebMessageReceivedEventHandler,
};
use windows::{
    core::{HSTRING, PCWSTR, PWSTR},
    Win32::UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, VK_LBUTTON},
        WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, SetForegroundWindow, ShowWindow,
            TrackPopupMenuEx, MF_CHECKED, MF_STRING, SW_RESTORE, TPM_LEFTBUTTON, TPM_NONOTIFY,
            TPM_RETURNCMD, TPM_RIGHTALIGN,
        },
    },
};

const ADAPTER_SCRIPT: &str = include_str!("chatgpt_adapter.js");
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TabStatus {
    Working,
    Send,
    Voice,
    Fallback,
}

#[derive(Debug, Deserialize)]
struct TabStatusMessage {
    kind: String,
    status: TabStatus,
}

fn decode_tab_status(source: &str, payload: &str) -> Result<TabStatus, &'static str> {
    let trusted = url::Url::parse(source)
        .map(|url| {
            url.scheme() == "https"
                && matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"))
        })
        .unwrap_or(false);
    if !trusted {
        return Err("source rejected");
    }
    let message: TabStatusMessage =
        serde_json::from_str(payload).map_err(|_| "payload rejected")?;
    if message.kind != "gpt-wrapper-tab-status" {
        return Err("payload rejected");
    }
    Ok(message.status)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TabInfo {
    id: usize,
    title: String,
    status: TabStatus,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TabSnapshot {
    tabs: Vec<TabInfo>,
    active_tab_id: Option<usize>,
}

#[derive(Debug)]
struct TabManager {
    tabs: Vec<TabInfo>,
    contexts: HashMap<usize, TabContext>,
    loaded_tabs: HashSet<usize>,
    active_tab_id: Option<usize>,
    next_tab_id: usize,
    standby_ptt_tab_id: Option<usize>,
    standby_ptt_ready: bool,
}

#[derive(Debug)]
struct TabContext {
    spec: TabSpec,
    status: TabStatus,
    prefix_applied: bool,
    prefix_in_progress: bool,
    background_ptt_pending: bool,
}

impl TabContext {
    fn new(spec: TabSpec) -> Self {
        Self {
            spec,
            status: TabStatus::Fallback,
            prefix_applied: false,
            prefix_in_progress: false,
            background_ptt_pending: false,
        }
    }
}

impl TabManager {
    fn new() -> Self {
        Self {
            tabs: Vec::new(),
            contexts: HashMap::new(),
            loaded_tabs: HashSet::new(),
            active_tab_id: None,
            next_tab_id: 1,
            standby_ptt_tab_id: None,
            standby_ptt_ready: false,
        }
    }

    fn update_status(&mut self, tab_id: usize, status: TabStatus) -> bool {
        let Some(context) = self.contexts.get_mut(&tab_id) else {
            return false;
        };
        if context.status == status {
            return false;
        }
        context.status = status;
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.status = status;
            true
        } else {
            // Standby PTT tabs are not shown until promoted.
            false
        }
    }

    fn snapshot(&self) -> TabSnapshot {
        TabSnapshot {
            tabs: self.tabs.iter().cloned().collect(),
            active_tab_id: self.active_tab_id,
        }
    }

    // Tab order is independent from tab identity and the active WebView.
    fn reorder(&mut self, tab_ids: &[usize]) -> Result<(), String> {
        if tab_ids.len() != self.tabs.len() {
            return Err("tab order must contain every visible tab exactly once".to_string());
        }

        let positions: HashMap<usize, usize> = tab_ids
            .iter()
            .copied()
            .enumerate()
            .map(|(index, id)| (id, index))
            .collect();
        if positions.len() != self.tabs.len()
            || self.tabs.iter().any(|tab| !positions.contains_key(&tab.id))
        {
            return Err("tab order contains missing, duplicate, or unknown tab IDs".to_string());
        }

        self.tabs.sort_by_key(|tab| positions[&tab.id]);
        Ok(())
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
    owner: Mutex<Option<PttSource>>,
    target_tab_id: Mutex<Option<usize>>,
    ble_target_tab_id: Mutex<Option<usize>>,
    ble_send_tab_id: Mutex<Option<usize>>,
    ble_submit_pending: AtomicBool,
    generation: AtomicUsize,
    scheduled_generation: AtomicUsize,
    clicked_generation: AtomicUsize,
    finish_in_flight: AtomicUsize,
    start_in_flight: AtomicUsize,
    finish_tab_id: AtomicUsize,
    failed_finish_tab_id: AtomicUsize,
    deferred_events: Mutex<VecDeque<PttEvent>>,
}

struct PttFinish {
    tab_id: usize,
    generation: usize,
    start_clicked: bool,
    submit_pending: bool,
}

struct PttCancel {
    tab_id: usize,
    generation: usize,
    start_clicked: bool,
}

enum ClickGuard {
    Start(Arc<PttTracker>, AppHandle, usize, usize),
    Finish(Arc<PttTracker>, AppHandle, usize, usize, PttSource, bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PttSource {
    Keyboard,
    Ble,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // BLE transport connects these control events in Phase 2.
enum PttEvent {
    Down(PttSource),
    Up(PttSource),
    SubmitBle,
    CancelBle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BleSubmitTarget {
    Queued,
    Busy,
    Tab(usize),
    None,
}

impl PttTracker {
    fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            owner: Mutex::new(None),
            target_tab_id: Mutex::new(None),
            ble_target_tab_id: Mutex::new(None),
            ble_send_tab_id: Mutex::new(None),
            ble_submit_pending: AtomicBool::new(false),
            generation: AtomicUsize::new(0),
            scheduled_generation: AtomicUsize::new(0),
            clicked_generation: AtomicUsize::new(0),
            finish_in_flight: AtomicUsize::new(0),
            start_in_flight: AtomicUsize::new(0),
            finish_tab_id: AtomicUsize::new(0),
            failed_finish_tab_id: AtomicUsize::new(0),
            deferred_events: Mutex::new(VecDeque::new()),
        }
    }

    fn is_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    fn defer_during_finish(&self, event: PttEvent) -> bool {
        let Ok(mut events) = self.deferred_events.lock() else {
            return true;
        };
        if self.finish_in_flight.load(Ordering::SeqCst) == 0
            && !(matches!(event, PttEvent::Up(_) | PttEvent::CancelBle)
                && self.start_in_flight.load(Ordering::SeqCst) != 0)
        {
            return false;
        }
        // A disconnect invalidates any queued BLE button edges and Submit.
        if event == PttEvent::CancelBle {
            events.retain(|queued| {
                !matches!(
                    queued,
                    PttEvent::Down(PttSource::Ble)
                        | PttEvent::Up(PttSource::Ble)
                        | PttEvent::SubmitBle
                )
            });
        }
        events.push_back(event);
        true
    }

    fn finish_completed(&self, generation: usize) -> Option<VecDeque<PttEvent>> {
        let mut events = self.deferred_events.lock().ok()?;
        if self
            .finish_in_flight
            .compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        Some(std::mem::take(&mut *events))
    }

    fn start_completed(&self, generation: usize) -> Option<VecDeque<PttEvent>> {
        let mut events = self.deferred_events.lock().ok()?;
        self.start_in_flight
            .compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(std::mem::take(&mut *events))
    }

    fn begin(&self, source: PttSource, tab_id: usize) -> Option<usize> {
        let mut owner = self.owner.lock().ok()?;
        if owner.is_some() {
            return None;
        }
        *owner = Some(source);
        self.held.store(true, Ordering::SeqCst);

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.scheduled_generation.store(0, Ordering::SeqCst);
        self.clicked_generation.store(0, Ordering::SeqCst);
        if let Ok(mut target) = self.target_tab_id.lock() {
            *target = Some(tab_id);
        }
        if source == PttSource::Ble {
            if let Ok(mut target) = self.ble_target_tab_id.lock() {
                *target = Some(tab_id);
            }
            self.ble_submit_pending.store(false, Ordering::SeqCst);
            if let Ok(mut send_target) = self.ble_send_tab_id.lock() {
                *send_target = None;
            }
        }
        Some(generation)
    }

    fn finish(&self, source: PttSource) -> Option<PttFinish> {
        let mut owner = self.owner.lock().ok()?;
        if *owner != Some(source) {
            return None;
        }
        *owner = None;
        self.held.store(false, Ordering::SeqCst);

        let generation = self.generation.load(Ordering::SeqCst);
        let tab_id = self.target_tab_id.lock().ok()?.take()?;
        Some(PttFinish {
            tab_id,
            generation,
            start_clicked: self.clicked_generation.load(Ordering::SeqCst) == generation,
            submit_pending: source == PttSource::Ble
                && self.ble_submit_pending.swap(false, Ordering::SeqCst),
        })
    }

    fn request_ble_submit(&self) -> BleSubmitTarget {
        let Ok(owner) = self.owner.lock() else {
            return BleSubmitTarget::None;
        };
        if *owner == Some(PttSource::Ble) {
            self.ble_submit_pending.store(true, Ordering::SeqCst);
            return BleSubmitTarget::Queued;
        }
        if owner.is_some() {
            return BleSubmitTarget::Busy;
        }
        if let Some(tab_id) = self.ble_send_tab_id.lock().ok().and_then(|target| *target) {
            return BleSubmitTarget::Tab(tab_id);
        }
        let tab_id = self
            .ble_target_tab_id
            .lock()
            .ok()
            .and_then(|mut target| target.take());
        if let Some(tab_id) = tab_id {
            self.note_ble_send(tab_id);
        }
        tab_id
            .map(BleSubmitTarget::Tab)
            .unwrap_or(BleSubmitTarget::None)
    }

    fn note_ble_send(&self, tab_id: usize) {
        if let Ok(mut target) = self.ble_send_tab_id.lock() {
            *target = Some(tab_id);
        }
    }

    fn clear_tab(&self, tab_id: usize) {
        let Ok(mut owner) = self.owner.lock() else {
            return;
        };
        if let Ok(mut target) = self.ble_target_tab_id.lock() {
            if *target == Some(tab_id) {
                *target = None;
            }
        }
        if let Ok(mut target) = self.ble_send_tab_id.lock() {
            if *target == Some(tab_id) {
                *target = None;
            }
        }
        if self
            .target_tab_id
            .lock()
            .ok()
            .is_some_and(|target| *target == Some(tab_id))
        {
            // Invalidate delayed starts before the WebView is destroyed.
            self.held.store(false, Ordering::SeqCst);
            self.generation.fetch_add(1, Ordering::SeqCst);
            *owner = None;
            if let Ok(mut target) = self.target_tab_id.lock() {
                *target = None;
            }
            self.ble_submit_pending.store(false, Ordering::SeqCst);
            self.start_in_flight.store(0, Ordering::SeqCst);
            if let Ok(mut events) = self.deferred_events.lock() {
                events.clear();
            }
        }
        // The callback checks this token before dispatching a click to a closed tab.
        if self.finish_tab_id.load(Ordering::SeqCst) == tab_id {
            self.finish_in_flight.store(0, Ordering::SeqCst);
            self.finish_tab_id.store(0, Ordering::SeqCst);
            if let Ok(mut events) = self.deferred_events.lock() {
                events.clear();
            }
        }
        if self.failed_finish_tab_id.load(Ordering::SeqCst) == tab_id {
            self.failed_finish_tab_id.store(0, Ordering::SeqCst);
        }
    }

    fn cancel_ble(&self) -> Option<PttCancel> {
        let mut owner = self.owner.lock().ok()?;
        if *owner == Some(PttSource::Keyboard) {
            return None;
        }
        let was_held = *owner == Some(PttSource::Ble);
        let start_clicked = was_held
            && self.clicked_generation.load(Ordering::SeqCst)
                == self.generation.load(Ordering::SeqCst);
        if was_held {
            *owner = None;
            self.held.store(false, Ordering::SeqCst);
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.start_in_flight.store(0, Ordering::SeqCst);
            if let Ok(mut events) = self.deferred_events.lock() {
                events.clear();
            }
        }
        self.ble_submit_pending.store(false, Ordering::SeqCst);
        let held_tab = if was_held {
            self.target_tab_id.lock().ok()?.take()
        } else {
            None
        };
        let target_tab = self.ble_target_tab_id.lock().ok()?.take();
        let send_tab = self.ble_send_tab_id.lock().ok()?.take();
        held_tab
            .or(send_tab)
            .or(target_tab)
            .map(|tab_id| PttCancel {
                tab_id,
                generation: self.generation.load(Ordering::SeqCst),
                start_clicked,
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

    fn mark_start_clicked(&self, tab_id: usize, generation: usize) -> bool {
        if !self.is_current(tab_id, generation) {
            return false;
        }

        self.clicked_generation
            .compare_exchange(0, generation, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
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

    fn cancel_pending(&self, tab_id: usize, generation: usize) {
        if self.is_current(tab_id, generation) {
            if let Ok(mut target) = self.target_tab_id.lock() {
                *target = None;
            }
        }
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

#[derive(Clone, Copy)]
enum PrefixFlow {
    Normal,
    BackgroundPtt(usize),
}

fn prefix_flow_current(app: &AppHandle, tab_id: usize, flow: PrefixFlow) -> bool {
    let exists = app
        .state::<TabState>()
        .lock()
        .map(|tabs| tabs.contexts.contains_key(&tab_id) && tabs.tabs.iter().any(|t| t.id == tab_id))
        .unwrap_or(false);
    exists
        && match flow {
            PrefixFlow::Normal => true,
            PrefixFlow::BackgroundPtt(generation) => {
                app.state::<PttState>().0.is_current(tab_id, generation)
            }
        }
}

fn finish_prefix_flow(app: &AppHandle, tab_id: usize, flow: PrefixFlow, success: bool) {
    if !prefix_flow_current(app, tab_id, flow) {
        return;
    }
    if let Ok(mut tabs) = app.state::<TabState>().lock() {
        if let Some(context) = tabs.contexts.get_mut(&tab_id) {
            context.prefix_in_progress = false;
            if success {
                context.prefix_applied = true;
            }
            context.background_ptt_pending = false;
        }
    }

    if let PrefixFlow::BackgroundPtt(generation) = flow {
        let tracker = Arc::clone(&app.state::<PttState>().0);
        if success {
            if let Some(webview) = get_tab_webview(app, tab_id) {
                schedule_ptt_start(webview, tracker, tab_id, generation, false);
            }
        } else {
            tracker.cancel_pending(tab_id, generation);
        }
    }
}

fn inject_prefix_attempt(
    app: AppHandle,
    webview: Webview,
    tab_id: usize,
    flow: PrefixFlow,
    prefix: String,
    attempt: usize,
) {
    if !prefix_flow_current(&app, tab_id, flow) {
        return;
    }
    let script = HSTRING::from(format!(
        "window.__GPT_WRAPPER__?.applyMessagePrefix?.({}) ?? 'not-ready'",
        serde_json::to_string(&prefix).expect("prefix is serializable")
    ));
    let app_for_callback = app.clone();
    let webview_for_callback = webview.clone();
    let result = webview.with_webview(move |platform_webview| {
        let core = match unsafe { platform_webview.controller().CoreWebView2() } {
            Ok(core) => core,
            Err(error) => {
                eprintln!("[gpt-wrapper] prefix injection WebView2 unavailable on tab {tab_id}: {error:?}");
                finish_prefix_flow(&app_for_callback, tab_id, flow, false);
                return;
            }
        };
        let app_for_result = app_for_callback.clone();
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |result, raw| {
            if !prefix_flow_current(&app_for_result, tab_id, flow) {
                return Ok(());
            }
            let status = if let Err(error) = result {
                eprintln!("[gpt-wrapper] prefix injection script failed on tab {tab_id}: {error:?}");
                "failure".to_string()
            } else {
                serde_json::from_str::<String>(&raw).unwrap_or_else(|_| "failure".to_string())
            };
            match status.as_str() {
                "success" => finish_prefix_flow(&app_for_result, tab_id, flow, true),
                "not-ready" if attempt < 10 => {
                    let app = app_for_result.clone();
                    let webview = webview_for_callback.clone();
                    let prefix = prefix.clone();
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(200));
                        inject_prefix_attempt(app, webview, tab_id, flow, prefix, attempt + 1);
                    });
                }
                _ => {
                    eprintln!("[gpt-wrapper] message prefix injection failed on tab {tab_id}: {status} (attempt {})", attempt + 1);
                    finish_prefix_flow(&app_for_result, tab_id, flow, false);
                }
            }
            Ok(())
        }));
        if let Err(error) = unsafe { core.ExecuteScript(&script, &handler) } {
            eprintln!("[gpt-wrapper] prefix injection ExecuteScript failed on tab {tab_id}: {error:?}");
            finish_prefix_flow(&app_for_callback, tab_id, flow, false);
        }
    });
    if let Err(error) = result {
        eprintln!("[gpt-wrapper] prefix injection with_webview failed on tab {tab_id}: {error:?}");
        finish_prefix_flow(&app, tab_id, flow, false);
    }
}

fn begin_prefix_flow(app: &AppHandle, webview: Webview, tab_id: usize, flow: PrefixFlow) {
    if !prefix_flow_current(app, tab_id, flow) {
        return;
    }
    let prefix = {
        let state = app.state::<TabState>();
        let Ok(mut tabs) = state.lock() else {
            return;
        };
        let Some(context) = tabs.contexts.get_mut(&tab_id) else {
            return;
        };
        if context.prefix_in_progress {
            return;
        }
        if context.prefix_applied || context.spec.message_prefix.is_empty() {
            context.prefix_applied = true;
            String::new()
        } else {
            context.prefix_in_progress = true;
            context.spec.message_prefix.clone()
        }
    };
    if prefix.is_empty() {
        finish_prefix_flow(app, tab_id, flow, true);
    } else {
        inject_prefix_attempt(app.clone(), webview, tab_id, flow, prefix, 0);
    }
}

fn set_tab_status(app_handle: &AppHandle, tab_id: usize, status: TabStatus) {
    let should_refresh = {
        let state = app_handle.state::<TabState>();
        let Ok(mut tabs) = state.lock() else {
            return;
        };
        tabs.update_status(tab_id, status)
    };
    if should_refresh {
        refresh_tab_host(app_handle);
    }
}

fn install_tab_status_listener(
    app_handle: &AppHandle,
    webview: &Webview,
    tab_id: usize,
) -> Result<(), String> {
    let app_handle = app_handle.clone();
    let setup_error = Arc::new(Mutex::new(None));
    let setup_error_for_callback = Arc::clone(&setup_error);
    webview
        .with_webview(move |platform_webview| {
            let Ok(core) = (unsafe { platform_webview.controller().CoreWebView2() }) else {
                *setup_error_for_callback.lock().unwrap() =
                    Some(format!("WebView2 missing on tab {tab_id}"));
                return;
            };
            let rejected_source = AtomicBool::new(false);
            let rejected_payload = AtomicBool::new(false);
            let handler = WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
                let Some(args) = args else {
                    if !rejected_payload.swap(true, Ordering::Relaxed) {
                        #[cfg(debug_assertions)]
                        eprintln!("[gpt-wrapper] status tab {tab_id}: payload rejected (missing event args)");
                    }
                    return Ok(());
                };
                let mut source = PWSTR(std::ptr::null_mut());
                if unsafe { args.Source(&mut source) }.is_err() {
                    if !rejected_source.swap(true, Ordering::Relaxed) {
                        #[cfg(debug_assertions)]
                        eprintln!("[gpt-wrapper] status tab {tab_id}: source rejected (read failed)");
                    }
                    return Ok(());
                }
                let source = CoTaskMemPWSTR::from(source).to_string();
                let mut raw = PWSTR(std::ptr::null_mut());
                if unsafe { args.TryGetWebMessageAsString(&mut raw) }.is_err() {
                    if !rejected_payload.swap(true, Ordering::Relaxed) {
                        #[cfg(debug_assertions)]
                        eprintln!("[gpt-wrapper] status tab {tab_id}: payload rejected (not a string)");
                    }
                    return Ok(());
                }
                let raw = CoTaskMemPWSTR::from(raw).to_string();
                match decode_tab_status(&source, &raw) {
                    Ok(status) => {
                        #[cfg(debug_assertions)]
                        eprintln!("[gpt-wrapper] status tab {tab_id}: accepted {status:?}");
                        set_tab_status(&app_handle, tab_id, status);
                    }
                    Err("source rejected") => {
                        if !rejected_source.swap(true, Ordering::Relaxed) {
                            #[cfg(debug_assertions)]
                            eprintln!("[gpt-wrapper] status tab {tab_id}: source rejected");
                        }
                    }
                    Err(_) => {
                        if !rejected_payload.swap(true, Ordering::Relaxed) {
                            #[cfg(debug_assertions)]
                            eprintln!("[gpt-wrapper] status tab {tab_id}: payload rejected");
                        }
                    }
                }
                Ok(())
            }));
            let mut token = 0;
            if let Err(error) = unsafe { core.add_WebMessageReceived(&handler, &mut token) } {
                *setup_error_for_callback.lock().unwrap() = Some(format!(
                    "failed to install status observer on tab {tab_id}: {error:?}"
                ));
            } else {
                #[cfg(debug_assertions)]
                eprintln!("[gpt-wrapper] status listener installed on tab {tab_id}");
            }
        })
        .map_err(|error| format!("failed to access WebView2 for tab {tab_id}: {error}"))?;
    let result = setup_error.lock().unwrap().take();
    result.map_or(Ok(()), Err)
}

fn create_chatgpt_webview(
    app_handle: &AppHandle,
    tab_id: usize,
    chatgpt_url: url::Url,
) -> Result<Webview, String> {
    let window = app_handle
        .get_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| "main window is missing".to_string())?;
    let (width, content_height) = content_size(&window)?;

    let builder = WebviewBuilder::new(
        tab_label(tab_id),
        WebviewUrl::App(TAB_BOOTSTRAP_PATH.into()),
    )
    .initialization_script(ADAPTER_SCRIPT)
    .background_color(LOADING_BACKGROUND)
    .focused(false)
    .on_page_load(move |webview, payload| {
        if payload.event() == PageLoadEvent::Started {
            let was_loaded = webview
                .app_handle()
                .state::<TabState>()
                .lock()
                .map(|mut tabs| tabs.loaded_tabs.remove(&tab_id))
                .unwrap_or(false);
            if was_loaded {
                webview.app_handle().state::<PttState>().0.clear_tab(tab_id);
            }
            set_tab_status(webview.app_handle(), tab_id, TabStatus::Fallback);
            return;
        }
        if payload.event() != PageLoadEvent::Finished
            || payload.url().path().ends_with(TAB_BOOTSTRAP_PATH)
        {
            return;
        }

        // Re-report the current DOM state after navigation even if the first event was missed.
        if let Err(error) = webview.eval("window.__GPT_WRAPPER__?.reportStatus?.();") {
            #[cfg(debug_assertions)]
            eprintln!("[gpt-wrapper] status tab {tab_id}: snapshot request failed: {error:?}");
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
            } else if tabs.tabs.iter().any(|tab| tab.id == tab_id) {
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
            let background_pending = app_handle
                .state::<TabState>()
                .lock()
                .ok()
                .and_then(|tabs| tabs.contexts.get(&tab_id).map(|c| c.background_ptt_pending))
                .unwrap_or(false);
            if background_pending {
                if let Some(generation) = tracker.current_generation(tab_id) {
                    begin_prefix_flow(
                        app_handle,
                        webview.clone(),
                        tab_id,
                        PrefixFlow::BackgroundPtt(generation),
                    );
                }
            } else {
                begin_prefix_flow(app_handle, webview.clone(), tab_id, PrefixFlow::Normal);
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

    if let Err(error) = install_tab_status_listener(app_handle, &webview, tab_id) {
        let _ = webview.close();
        return Err(error);
    }

    if let Err(error) = webview.navigate(chatgpt_url) {
        let _ = webview.close();
        return Err(format!("failed to navigate ChatGPT tab {tab_id}: {error}"));
    }

    Ok(webview)
}

fn create_tab_internal(app_handle: &AppHandle) -> Result<TabSnapshot, String> {
    let spec = app_handle
        .state::<ProjectSettingsState>()
        .snapshot()?
        .default_tab_spec();
    create_tab_with_spec(app_handle, spec, false)
}

fn create_tab_with_spec(
    app_handle: &AppHandle,
    spec: TabSpec,
    background_ptt: bool,
) -> Result<TabSnapshot, String> {
    let state = app_handle.state::<TabState>();
    let (tab_id, previous_active) = {
        let mut tabs = state.lock()?;
        let tab_id = tabs.next_tab_id;
        tabs.next_tab_id += 1;
        let previous_active = tabs.active_tab_id;
        tabs.tabs.push(TabInfo {
            id: tab_id,
            title: spec.title.clone(),
            status: TabStatus::Fallback,
        });
        let mut context = TabContext::new(spec.clone());
        context.background_ptt_pending = background_ptt;
        tabs.contexts.insert(tab_id, context);
        tabs.active_tab_id = Some(tab_id);
        (tab_id, previous_active)
    };

    if let Err(error) = create_chatgpt_webview(app_handle, tab_id, spec.url) {
        let mut tabs = state.lock()?;
        tabs.tabs.retain(|tab| tab.id != tab_id);
        tabs.contexts.remove(&tab_id);
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
    let spec = app_handle
        .state::<ProjectSettingsState>()
        .snapshot()?
        .default_tab_spec();
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
        tabs.contexts.insert(tab_id, TabContext::new(spec.clone()));
        tab_id
    };

    if let Err(error) = create_chatgpt_webview(app_handle, tab_id, spec.url) {
        let mut tabs = state.lock()?;
        if tabs.standby_ptt_tab_id == Some(tab_id) {
            tabs.standby_ptt_tab_id = None;
            tabs.standby_ptt_ready = false;
        }
        tabs.loaded_tabs.remove(&tab_id);
        tabs.contexts.remove(&tab_id);
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
            tabs.contexts.remove(&tab_id);
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
    let current_spec = app_handle
        .state::<ProjectSettingsState>()
        .snapshot()?
        .default_tab_spec();
    let state = app_handle.state::<TabState>();
    let standby_id = {
        let tabs = state.lock()?;
        let standby_id = tabs
            .standby_ptt_tab_id
            .ok_or_else(|| "standby PTT tab is missing".to_string())?;
        if tabs.contexts.get(&standby_id).map(|c| &c.spec) != Some(&current_spec) {
            return Err("standby Project context is outdated".to_string());
        }
        standby_id
    };
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
        let context = tabs
            .contexts
            .get_mut(&standby_id)
            .ok_or("standby context missing")?;
        context.background_ptt_pending = true;
        let title = context.spec.title.clone();
        let status = context.status;
        tabs.tabs.push(TabInfo {
            id: standby_id,
            title,
            status,
        });
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
        if !tabs.tabs.iter().any(|tab| tab.id == tab_id) {
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
    app_handle.state::<PttState>().0.clear_tab(tab_id);
    let state = app_handle.state::<TabState>();
    let (was_active, replacement) = {
        let tabs = state.lock()?;
        let index = tabs
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .ok_or_else(|| format!("tab {tab_id} does not exist"))?;

        let was_active = tabs.active_tab_id == Some(tab_id);
        let replacement = if was_active {
            if index + 1 < tabs.tabs.len() {
                Some(tabs.tabs[index + 1].id)
            } else if index > 0 {
                Some(tabs.tabs[index - 1].id)
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
        tabs.tabs.retain(|tab| tab.id != tab_id);
        tabs.loaded_tabs.remove(&tab_id);
        tabs.contexts.remove(&tab_id);
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
async fn create_project_tab(
    app: AppHandle,
    webview: Webview,
    project_id: Uuid,
) -> Result<TabSnapshot, String> {
    ensure_host_webview(&webview)?;
    let spec = if project_id.is_nil() {
        ProjectSettings::default().default_tab_spec()
    } else {
        app.state::<ProjectSettingsState>()
            .snapshot()?
            .project_tab_spec(project_id)?
    };
    create_tab_with_spec(&app, spec, false)
}

#[derive(Debug, Deserialize)]
struct MenuAnchor {
    right: f64,
    bottom: f64,
}

struct PopupMenu(windows::Win32::UI::WindowsAndMessaging::HMENU);

impl Drop for PopupMenu {
    fn drop(&mut self) {
        if let Err(error) = unsafe { DestroyMenu(self.0) } {
            eprintln!("[gpt-wrapper] failed to destroy bookmark menu: {error}");
        }
    }
}

fn track_project_bookmark_menu(
    app: &AppHandle,
    settings: ProjectSettings,
    anchor: MenuAnchor,
) -> Result<Option<Uuid>, String> {
    // IPC can reach the UI thread after the mouse has already been released.
    if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } >= 0 {
        return Ok(None);
    }
    let window = app
        .get_window(MAIN_WINDOW_LABEL)
        .ok_or("main window is missing")?;
    let scale = window.scale_factor().map_err(|error| error.to_string())?;
    let origin = window.inner_position().map_err(|error| error.to_string())?;
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    let menu = PopupMenu(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);

    let builtin_label: Vec<u16> = "ChatGPT.com\0".encode_utf16().collect();
    let builtin_flags = if settings.default_project_id.is_none() {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    unsafe { AppendMenuW(menu.0, builtin_flags, 1, PCWSTR(builtin_label.as_ptr())) }
        .map_err(|error| error.to_string())?;
    {
        for (index, project) in settings.projects.iter().enumerate() {
            let label: Vec<u16> =
                format!("{}\0", project.name.replace('&', "&&").replace('\0', ""))
                    .encode_utf16()
                    .collect();
            let mut flags = MF_STRING;
            if settings.default_project_id == Some(project.id) {
                flags |= MF_CHECKED;
            }
            unsafe { AppendMenuW(menu.0, flags, index + 2, PCWSTR(label.as_ptr())) }
                .map_err(|error| error.to_string())?;
        }
    }

    // WebView2 may still own capture from pointerdown; the menu loop needs it.
    let _ = unsafe { ReleaseCapture() };
    if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } >= 0 {
        return Ok(None);
    }
    let x = origin.x + (anchor.right * scale).round() as i32;
    let y = origin.y + (anchor.bottom * scale).round() as i32;
    let flags = TPM_LEFTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTALIGN;
    let command = unsafe { TrackPopupMenuEx(menu.0, flags.0, x, y, hwnd, None) }.0 as usize;
    Ok(if command == 1 {
        Some(Uuid::nil())
    } else {
        command
            .checked_sub(2)
            .and_then(|index| settings.projects.get(index).map(|p| p.id))
    })
}

#[tauri::command]
async fn show_project_bookmark_menu(
    app: AppHandle,
    webview: Webview,
    anchor: MenuAnchor,
) -> Result<Option<Uuid>, String> {
    ensure_host_webview(&webview)?;
    if !anchor.right.is_finite() || !anchor.bottom.is_finite() {
        return Err("invalid bookmark menu anchor".into());
    }
    let settings = app.state::<ProjectSettingsState>().snapshot()?;
    tauri::async_runtime::spawn_blocking(move || {
        let (sender, receiver) = std::sync::mpsc::channel();
        let app_for_ui = app.clone();
        app.run_on_main_thread(move || {
            let _ = sender.send(track_project_bookmark_menu(&app_for_ui, settings, anchor));
        })
        .map_err(|error| error.to_string())?;
        receiver.recv().map_err(|error| error.to_string())?
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn switch_tab(app: AppHandle, webview: Webview, tab_id: usize) -> Result<TabSnapshot, String> {
    ensure_host_webview(&webview)?;
    switch_tab_internal(&app, tab_id)
}

#[tauri::command]
fn reorder_tabs(
    app: AppHandle,
    webview: Webview,
    tab_ids: Vec<usize>,
) -> Result<TabSnapshot, String> {
    ensure_host_webview(&webview)?;
    let state = app.state::<TabState>();
    let mut tabs = state.lock()?;
    tabs.reorder(&tab_ids)?;
    Ok(tabs.snapshot())
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

fn update_project_settings(
    app: &AppHandle,
    change: impl FnOnce(&mut ProjectSettings) -> Result<(), String>,
) -> Result<ProjectSettings, String> {
    let (settings, default_changed) = app.state::<ProjectSettingsState>().update(change)?;
    if default_changed {
        let has_standby = app.state::<TabState>().lock()?.standby_ptt_tab_id.is_some();
        if has_standby {
            discard_standby_ptt_tab(app);
            if app
                .state::<TrayVisibilityState>()
                .tray_hidden
                .load(Ordering::SeqCst)
            {
                if let Err(error) = ensure_standby_ptt_tab(app) {
                    eprintln!("[gpt-wrapper] failed to refresh standby PTT tab: {error}");
                }
            }
        }
    }
    Ok(settings)
}

#[tauri::command]
fn get_project_settings(app: AppHandle, webview: Webview) -> Result<ProjectSettings, String> {
    ensure_settings_webview(&webview)?;
    app.state::<ProjectSettingsState>().snapshot()
}

#[tauri::command]
fn create_project(
    app: AppHandle,
    webview: Webview,
    name: String,
    url: String,
    message_prefix: String,
) -> Result<ProjectSettings, String> {
    ensure_settings_webview(&webview)?;
    update_project_settings(&app, |data| {
        project_settings::create(data, name, url, message_prefix)
    })
}

#[tauri::command]
fn edit_project(
    app: AppHandle,
    webview: Webview,
    id: Uuid,
    name: String,
    url: String,
    message_prefix: String,
) -> Result<ProjectSettings, String> {
    ensure_settings_webview(&webview)?;
    update_project_settings(&app, |data| {
        project_settings::edit(data, id, name, url, message_prefix)
    })
}

#[tauri::command]
fn delete_project(app: AppHandle, webview: Webview, id: Uuid) -> Result<ProjectSettings, String> {
    ensure_settings_webview(&webview)?;
    update_project_settings(&app, |data| project_settings::delete(data, id))
}

#[tauri::command]
fn set_default_project(
    app: AppHandle,
    webview: Webview,
    id: Option<Uuid>,
) -> Result<ProjectSettings, String> {
    ensure_settings_webview(&webview)?;
    update_project_settings(&app, |data| project_settings::set_default(data, id))
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
fn get_ptt_auto_submit_enabled(app: AppHandle, webview: Webview) -> Result<bool, String> {
    ensure_settings_webview(&webview)?;
    app.state::<GeneralSettingsState>().ptt_auto_submit()
}

#[tauri::command]
fn set_ptt_auto_submit_enabled(
    app: AppHandle,
    webview: Webview,
    enabled: bool,
) -> Result<bool, String> {
    ensure_settings_webview(&webview)?;
    app.state::<GeneralSettingsState>()
        .set_ptt_auto_submit(enabled)
}

#[tauri::command]
fn ble_status(app: AppHandle, webview: Webview) -> Result<ble_central::Snapshot, String> {
    ensure_settings_webview(&webview)?;
    app.state::<BleCentral>().snapshot()
}

#[tauri::command]
fn ble_enable(app: AppHandle, webview: Webview, enabled: bool) -> Result<(), String> {
    ensure_settings_webview(&webview)?;
    app.state::<BleCentral>().enable(enabled)
}

#[tauri::command]
fn ble_scan(app: AppHandle, webview: Webview) -> Result<(), String> {
    ensure_settings_webview(&webview)?;
    app.state::<BleCentral>().scan()
}

#[tauri::command]
fn ble_approve(app: AppHandle, webview: Webview, id: String) -> Result<(), String> {
    ensure_settings_webview(&webview)?;
    app.state::<BleCentral>().approve(id)
}

#[tauri::command]
fn ble_disconnect(app: AppHandle, webview: Webview) -> Result<(), String> {
    ensure_settings_webview(&webview)?;
    app.state::<BleCentral>().disconnect()
}

#[tauri::command]
fn ble_forget(app: AppHandle, webview: Webview) -> Result<(), String> {
    ensure_settings_webview(&webview)?;
    app.state::<BleCentral>().forget()
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
            #[cfg(debug_assertions)]
            eprintln!("[gpt-wrapper] failed to refresh tab host: {error:?}");
        }
    } else {
        #[cfg(debug_assertions)]
        eprintln!("[gpt-wrapper] failed to refresh tab host: host webview missing");
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
                app_handle.state::<BleCentral>().quit();
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

fn trusted_click_guarded(webview: &Webview, target: DictationTarget, guard: Option<ClickGuard>) {
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
                let mut clicked = false;
                if let Err(error) = result {
                    eprintln!("[gpt-wrapper] button lookup script failed: {error:?}");
                } else { match serde_json::from_str::<Option<Rect>>(&raw) {
                    Ok(Some(rect)) => {
                        match &guard {
                            Some(ClickGuard::Start(tracker, _, tab_id, generation))
                                if !tracker.mark_start_clicked(*tab_id, *generation) =>
                            {
                                return Ok(())
                            }
                            Some(ClickGuard::Finish(tracker, _, _, generation, _, _))
                                if tracker.finish_in_flight.load(Ordering::SeqCst) != *generation =>
                            {
                                return Ok(())
                            }
                            _ => {}
                        }
                        println!(
                            "[gpt-wrapper] trusted click dispatch requested {:?} at ({:.1}, {:.1}); recording/delivery unverified",
                            target, rect.x, rect.y
                        );
                        dispatch_trusted_click(&webview_for_callback, rect);
                        clicked = true;
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
                }}

                match guard {
                    Some(ClickGuard::Finish(tracker, app, tab_id, generation, source, submit_pending)) => {
                        if clicked {
                            verify_finish_stopped(app, tracker, tab_id, generation, source, submit_pending, 0);
                        } else {
                            complete_finish(app, tracker, tab_id, generation, source, submit_pending, false);
                        }
                    }
                    Some(ClickGuard::Start(tracker, app, tab_id, generation)) if clicked => {
                        verify_start_visible(app, tracker, tab_id, generation, 0);
                    }
                    _ => {}
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

fn complete_finish(
    app: AppHandle,
    tracker: Arc<PttTracker>,
    tab_id: usize,
    generation: usize,
    source: PttSource,
    submit_pending: bool,
    stopped: bool,
) {
    let Some(events) = tracker.finish_completed(generation) else {
        return;
    };
    if !stopped {
        if events.contains(&PttEvent::CancelBle) {
            let _ = tracker.cancel_ble();
        }
        tracker.failed_finish_tab_id.store(tab_id, Ordering::SeqCst);
        if let Some(tab) = get_tab_webview(&app, tab_id) {
            let _ = tab.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
        }
        eprintln!("[gpt-wrapper] Finish could not be verified on tab {tab_id}; queued events rejected; recording state uncertain");
        return;
    }
    let disconnected = events.contains(&PttEvent::CancelBle);
    if let Some(tab) = get_tab_webview(&app, tab_id) {
        if source == PttSource::Ble {
            if disconnected {
                let _ = tab.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
            } else {
                let _ = tab.eval("window.__GPT_WRAPPER__?.finishBleSegment?.();");
            }
            if submit_pending && !disconnected {
                tracker.note_ble_send(tab_id);
                let _ = tab.eval("window.__GPT_WRAPPER__?.requestExplicitSend?.();");
            }
        } else {
            let _ = tab.eval("window.__GPT_WRAPPER__?.finishKeyboardSegment?.();");
            if app
                .state::<GeneralSettingsState>()
                .ptt_auto_submit()
                .unwrap_or(false)
            {
                let _ = tab.eval("window.__GPT_WRAPPER__?.waitAndSend?.();");
            }
        }
    }
    for event in events {
        handle_ptt_event(&app, event, &tracker);
    }
}

fn fail_start(app: AppHandle, tracker: Arc<PttTracker>, tab_id: usize, generation: usize) {
    if tracker.start_completed(generation).is_none() {
        return;
    }
    let source = tracker.owner.lock().ok().and_then(|owner| *owner);
    if let Some(source) = source {
        let _ = tracker.finish(source);
    }
    tracker.failed_finish_tab_id.store(tab_id, Ordering::SeqCst);
    if let Some(tab) = get_tab_webview(&app, tab_id) {
        let _ = tab.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
    }
    eprintln!("[gpt-wrapper] Start could not be verified on tab {tab_id}; queued release rejected; recording state uncertain");
}

fn verify_start_visible(
    app: AppHandle,
    tracker: Arc<PttTracker>,
    tab_id: usize,
    generation: usize,
    attempt: u8,
) {
    if tracker.start_in_flight.load(Ordering::SeqCst) != generation {
        return;
    }
    if attempt >= 30 {
        fail_start(app, tracker, tab_id, generation);
        return;
    }
    let Some(tab) = get_tab_webview(&app, tab_id) else {
        fail_start(app, tracker, tab_id, generation);
        return;
    };
    let script = HSTRING::from(DictationTarget::Finish.rect_script());
    let result = tab.with_webview(move |platform| {
        let Ok(core) = (unsafe { platform.controller().CoreWebView2() }) else {
            return;
        };
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |result, raw| {
            if result.is_ok() && matches!(serde_json::from_str::<Option<Rect>>(&raw), Ok(Some(_))) {
                if let Some(events) = tracker.start_completed(generation) {
                    for event in events {
                        handle_ptt_event(&app, event, &tracker);
                    }
                }
            } else {
                thread::spawn({
                    let app = app.clone();
                    let tracker = Arc::clone(&tracker);
                    move || {
                        thread::sleep(Duration::from_millis(100));
                        verify_start_visible(app, tracker, tab_id, generation, attempt + 1);
                    }
                });
            }
            Ok(())
        }));
        if let Err(error) = unsafe { core.ExecuteScript(&script, &handler) } {
            eprintln!("[gpt-wrapper] Start verification query failed: {error:?}");
        }
    });
    if let Err(error) = result {
        eprintln!("[gpt-wrapper] Start verification WebView unavailable: {error:?}");
    }
}

fn watch_start_callback(
    app_handle: AppHandle,
    tracker: Arc<PttTracker>,
    tab_id: usize,
    generation: usize,
) {
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(5));
        let app = app_handle.clone();
        let _ = app_handle.run_on_main_thread(move || {
            fail_start(app, tracker, tab_id, generation);
        });
    });
}

fn verify_finish_stopped(
    app: AppHandle,
    tracker: Arc<PttTracker>,
    tab_id: usize,
    generation: usize,
    source: PttSource,
    submit_pending: bool,
    attempt: u8,
) {
    if tracker.finish_in_flight.load(Ordering::SeqCst) != generation {
        return;
    }
    if attempt >= 30 {
        complete_finish(
            app,
            tracker,
            tab_id,
            generation,
            source,
            submit_pending,
            false,
        );
        return;
    }
    let Some(tab) = get_tab_webview(&app, tab_id) else {
        complete_finish(
            app,
            tracker,
            tab_id,
            generation,
            source,
            submit_pending,
            false,
        );
        return;
    };
    let script = HSTRING::from(DictationTarget::Finish.rect_script());
    let result = tab.with_webview(move |platform| {
        let Ok(core) = (unsafe { platform.controller().CoreWebView2() }) else {
            return;
        };
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |result, raw| {
            if result.is_ok() && matches!(serde_json::from_str::<Option<Rect>>(&raw), Ok(None)) {
                complete_finish(
                    app.clone(),
                    Arc::clone(&tracker),
                    tab_id,
                    generation,
                    source,
                    submit_pending,
                    true,
                );
            } else {
                thread::spawn({
                    let app = app.clone();
                    let tracker = Arc::clone(&tracker);
                    move || {
                        thread::sleep(Duration::from_millis(100));
                        verify_finish_stopped(
                            app,
                            tracker,
                            tab_id,
                            generation,
                            source,
                            submit_pending,
                            attempt + 1,
                        );
                    }
                });
            }
            Ok(())
        }));
        if let Err(error) = unsafe { core.ExecuteScript(&script, &handler) } {
            eprintln!("[gpt-wrapper] Finish verification query failed: {error:?}");
        }
    });
    if let Err(error) = result {
        eprintln!("[gpt-wrapper] Finish verification WebView unavailable: {error:?}");
    }
}

fn watch_finish_callback(
    app_handle: AppHandle,
    tracker: Arc<PttTracker>,
    tab_id: usize,
    generation: usize,
) {
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(5));
        let app = app_handle.clone();
        let _ = app_handle.run_on_main_thread(move || {
            if let Some(events) = tracker.finish_completed(generation) {
                if events.contains(&PttEvent::CancelBle) {
                    let _ = tracker.cancel_ble();
                }
                tracker.failed_finish_tab_id.store(tab_id, Ordering::SeqCst);
                if let Some(tab) = get_tab_webview(&app, tab_id) {
                    let _ = tab.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
                }
                eprintln!("[gpt-wrapper] Finish callback timed out on tab {tab_id}; queued events rejected; recording state uncertain");
            }
        });
    });
}

fn schedule_ptt_start(
    webview: Webview,
    tracker: Arc<PttTracker>,
    tab_id: usize,
    generation: usize,
    _consider_started_immediately: bool,
) {
    if !tracker.claim_start(tab_id, generation) {
        return;
    }
    tracker.start_in_flight.store(generation, Ordering::SeqCst);
    watch_start_callback(
        webview.app_handle().clone(),
        Arc::clone(&tracker),
        tab_id,
        generation,
    );

    println!(
        "[gpt-wrapper] scheduling dictation Start on tab {tab_id}; recording readiness unverified"
    );
    let _ = webview.eval("window.__GPT_WRAPPER__?.prepareStart?.();");

    thread::spawn(move || {
        for delay_ms in [180_u64, 300_u64] {
            thread::sleep(Duration::from_millis(delay_ms));
            if !tracker.is_current(tab_id, generation) {
                return;
            }
            trusted_click_guarded(
                &webview,
                DictationTarget::Start,
                Some(ClickGuard::Start(
                    Arc::clone(&tracker),
                    webview.app_handle().clone(),
                    tab_id,
                    generation,
                )),
            );
        }
    });
}

// Phase 2 BLE transport calls this with Down(Ble), Up(Ble), SubmitBle, or CancelBle.
// It must never synthesize a keyboard shortcut.
fn handle_ptt_event(app_handle: &AppHandle, event: PttEvent, tracker: &Arc<PttTracker>) {
    if tracker.defer_during_finish(event) {
        return;
    }
    if tracker.failed_finish_tab_id.load(Ordering::SeqCst) != 0
        && matches!(event, PttEvent::Down(_) | PttEvent::SubmitBle)
    {
        eprintln!("[gpt-wrapper] PTT/Submit rejected: previous Finish failed; resolve dictation in the affected tab or reload it");
        return;
    }
    match event {
        PttEvent::Down(source) => {
            if tracker.is_held() {
                return;
            }

            let was_tray_hidden = app_handle
                .state::<TrayVisibilityState>()
                .tray_hidden
                .swap(false, Ordering::SeqCst);

            if was_tray_hidden {
                let tab_id = match promote_standby_ptt_tab(app_handle) {
                    Ok((tab_id, _ready)) => tab_id,
                    Err(error) => {
                        eprintln!(
                            "[gpt-wrapper] standby promotion failed, creating fallback tab: {error}"
                        );
                        discard_standby_ptt_tab(app_handle);
                        let fallback = app_handle
                            .state::<ProjectSettingsState>()
                            .snapshot()
                            .map(|settings| settings.default_tab_spec())
                            .and_then(|spec| create_tab_with_spec(app_handle, spec, true));
                        if let Err(create_error) = fallback {
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
                        tab_id
                    }
                };

                if let Some(window) = app_handle.get_window(MAIN_WINDOW_LABEL) {
                    restore_to_foreground(&window);
                } else {
                    eprintln!("[gpt-wrapper] main window missing");
                    return;
                }

                let Some(generation) = tracker.begin(source, tab_id) else {
                    return;
                };

                refresh_tab_host(app_handle);

                if is_tab_loaded(app_handle, tab_id) {
                    if let Some(webview) = get_tab_webview(app_handle, tab_id) {
                        let _ = webview.show();
                        let _ = webview.set_focus();
                        begin_prefix_flow(
                            app_handle,
                            webview,
                            tab_id,
                            PrefixFlow::BackgroundPtt(generation),
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
            let Some(generation) = tracker.begin(source, tab_id) else {
                return;
            };

            if let Some(window) = app_handle.get_window(MAIN_WINDOW_LABEL) {
                restore_to_foreground(&window);
            }
            let _ = webview.set_focus();

            println!("[gpt-wrapper] hotkey pressed on tab {tab_id}");
            schedule_ptt_start(webview, Arc::clone(tracker), tab_id, generation, true);
        }
        PttEvent::Up(source) => {
            let Some(finish) = tracker.finish(source) else {
                return;
            };

            if !finish.start_clicked {
                println!(
                    "[gpt-wrapper] PTT released before dictation Start click on tab {}",
                    finish.tab_id
                );
                if source == PttSource::Ble {
                    tracker.clear_tab(finish.tab_id);
                }
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
            tracker.finish_tab_id.store(finish.tab_id, Ordering::SeqCst);
            tracker
                .finish_in_flight
                .store(finish.generation, Ordering::SeqCst);
            trusted_click_guarded(
                &webview,
                DictationTarget::Finish,
                Some(ClickGuard::Finish(
                    Arc::clone(tracker),
                    app_handle.clone(),
                    finish.tab_id,
                    finish.generation,
                    source,
                    finish.submit_pending,
                )),
            );
            watch_finish_callback(
                app_handle.clone(),
                Arc::clone(tracker),
                finish.tab_id,
                finish.generation,
            );
        }
        PttEvent::SubmitBle => match tracker.request_ble_submit() {
            BleSubmitTarget::Queued => println!("[gpt-wrapper] BLE Submit queued for held PTT"),
            BleSubmitTarget::Busy => {
                eprintln!("[gpt-wrapper] BLE Submit refused while keyboard PTT is held")
            }
            BleSubmitTarget::Tab(tab_id) => {
                if is_tab_loaded(app_handle, tab_id) {
                    if let Some(webview) = get_tab_webview(app_handle, tab_id) {
                        let _ = webview.eval("window.__GPT_WRAPPER__?.requestExplicitSend?.();");
                    }
                } else {
                    eprintln!("[gpt-wrapper] BLE Submit target tab {tab_id} unavailable");
                }
            }
            BleSubmitTarget::None => {
                if app_handle
                    .state::<TrayVisibilityState>()
                    .tray_hidden
                    .load(Ordering::SeqCst)
                {
                    eprintln!("[gpt-wrapper] BLE Submit refused: no target while tray-hidden");
                } else if let Some(tab_id) = active_tab_id(app_handle) {
                    if is_tab_loaded(app_handle, tab_id) {
                        if let Some(webview) = get_tab_webview(app_handle, tab_id) {
                            tracker.note_ble_send(tab_id);
                            let _ =
                                webview.eval("window.__GPT_WRAPPER__?.requestExplicitSend?.();");
                        }
                    }
                }
            }
        },
        PttEvent::CancelBle => {
            if let Some(cancel) = tracker.cancel_ble() {
                if let Some(webview) = get_tab_webview(app_handle, cancel.tab_id) {
                    let _ = webview.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
                    if cancel.start_clicked {
                        tracker.finish_tab_id.store(cancel.tab_id, Ordering::SeqCst);
                        tracker
                            .finish_in_flight
                            .store(cancel.generation, Ordering::SeqCst);
                        trusted_click_guarded(
                            &webview,
                            DictationTarget::Finish,
                            Some(ClickGuard::Finish(
                                Arc::clone(tracker),
                                app_handle.clone(),
                                cancel.tab_id,
                                cancel.generation,
                                PttSource::Ble,
                                false,
                            )),
                        );
                        watch_finish_callback(
                            app_handle.clone(),
                            Arc::clone(tracker),
                            cancel.tab_id,
                            cancel.generation,
                        );
                    }
                }
            }
        }
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_tabs,
            create_tab,
            create_project_tab,
            show_project_bookmark_menu,
            switch_tab,
            reorder_tabs,
            close_tab,
            get_autostart_enabled,
            set_autostart_enabled,
            get_ptt_auto_submit_enabled,
            set_ptt_auto_submit_enabled,
            ble_status,
            ble_enable,
            ble_scan,
            ble_approve,
            ble_disconnect,
            ble_forget,
            get_project_settings,
            create_project,
            edit_project,
            delete_project,
            set_default_project
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
            let project_settings_path: PathBuf =
                app.path().app_config_dir()?.join("project-settings.json");
            app.manage(ProjectSettingsState::load(project_settings_path));
            let general_settings_path = app.path().app_config_dir()?.join("general-settings.json");
            app.manage(GeneralSettingsState::load(general_settings_path));
            let ble_app = app.handle().clone();
            let ble_tracker = Arc::clone(&ptt_tracker);
            let ble_path = app.path().app_config_dir()?.join("ble-controller.json");
            app.manage(BleCentral::start(ble_path, move |control| {
                let app = ble_app.clone();
                let tracker = Arc::clone(&ble_tracker);
                let _ = ble_app.run_on_main_thread(move || {
                    let event = match control {
                        ControlEvent::Down => PttEvent::Down(PttSource::Ble),
                        ControlEvent::Up => PttEvent::Up(PttSource::Ble),
                        ControlEvent::Submit => PttEvent::SubmitBle,
                        ControlEvent::Cancel => PttEvent::CancelBle,
                    };
                    handle_ptt_event(&app, event, &tracker);
                });
            }));

            let window = WindowBuilder::new(app, MAIN_WINDOW_LABEL)
                .title("GPT Wrapper")
                .background_color(LOADING_BACKGROUND)
                .inner_size(1200.0, 860.0)
                .min_inner_size(720.0, 540.0)
                .visible(!launched_by_autostart)
                .focused(!launched_by_autostart)
                .build()?;

            let (width, _) = content_size(&window).map_err(tauri::Error::AssetNotFound)?;
            let host_builder = WebviewBuilder::new(
                HOST_WEBVIEW_LABEL,
                WebviewUrl::App("tab-bar/index.html".into()),
            )
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
                    handle_ptt_event(
                        app_handle,
                        match event.state() {
                            ShortcutState::Pressed => PttEvent::Down(PttSource::Keyboard),
                            ShortcutState::Released => PttEvent::Up(PttSource::Keyboard),
                        },
                        &tracker_for_primary,
                    );
                },
            )?;

            let tracker_for_numpad = Arc::clone(&ptt_tracker);
            app.global_shortcut().on_shortcut(
                numpad_shortcut,
                move |app_handle, _shortcut, event| {
                    handle_ptt_event(
                        app_handle,
                        match event.state() {
                            ShortcutState::Pressed => PttEvent::Down(PttSource::Keyboard),
                            ShortcutState::Released => PttEvent::Up(PttSource::Keyboard),
                        },
                        &tracker_for_numpad,
                    );
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

#[cfg(test)]
mod ptt_tests {
    use super::*;

    #[test]
    fn source_owns_the_held_session_and_repeated_events_are_safe() {
        let ptt = PttTracker::new();
        let generation = ptt.begin(PttSource::Keyboard, 4).unwrap();
        assert!(ptt.begin(PttSource::Keyboard, 4).is_none());
        assert!(ptt.begin(PttSource::Ble, 5).is_none());
        assert!(ptt.finish(PttSource::Ble).is_none());
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::Busy);
        assert!(ptt.mark_start_clicked(4, generation));
        let finish = ptt.finish(PttSource::Keyboard).unwrap();
        assert_eq!(finish.tab_id, 4);
        assert!(finish.start_clicked);
        assert!(!finish.submit_pending);
        assert!(ptt.finish(PttSource::Keyboard).is_none());
    }

    #[test]
    fn ble_segments_keep_target_until_submit_and_queue_only_once() {
        let ptt = PttTracker::new();
        let first = ptt.begin(PttSource::Ble, 7).unwrap();
        assert!(ptt.mark_start_clicked(7, first));
        assert!(!ptt.finish(PttSource::Ble).unwrap().submit_pending);
        let second = ptt.begin(PttSource::Ble, 7).unwrap();
        assert!(ptt.mark_start_clicked(7, second));
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::Queued);
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::Queued);
        assert!(ptt.finish(PttSource::Ble).unwrap().submit_pending);
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::Tab(7));
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::Tab(7));
    }

    #[test]
    fn disconnect_and_tab_close_invalidate_only_owned_work() {
        let ptt = PttTracker::new();
        ptt.begin(PttSource::Keyboard, 2).unwrap();
        assert!(ptt.cancel_ble().is_none());
        assert!(ptt.is_held());
        ptt.finish(PttSource::Keyboard);

        let generation = ptt.begin(PttSource::Ble, 3).unwrap();
        let cancel = ptt.cancel_ble().unwrap();
        assert_eq!(cancel.tab_id, 3);
        assert!(!cancel.start_clicked);
        assert!(!ptt.is_current(3, generation));
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::None);
        ptt.begin(PttSource::Ble, 5).unwrap();
        ptt.clear_tab(5);
        assert!(!ptt.is_held());
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::None);

        let generation = ptt.begin(PttSource::Ble, 6).unwrap();
        assert!(ptt.mark_start_clicked(6, generation));
        let cancel = ptt.cancel_ble().unwrap();
        assert_eq!(cancel.tab_id, 6);
        assert!(cancel.start_clicked);
    }

    #[test]
    fn delayed_finish_orders_next_session_and_disconnect_drops_ble_edges() {
        let ptt = PttTracker::new();
        let first = ptt.begin(PttSource::Ble, 7).unwrap();
        assert!(ptt.mark_start_clicked(7, first));
        ptt.finish(PttSource::Ble).unwrap();
        ptt.finish_tab_id.store(7, Ordering::SeqCst);
        ptt.finish_in_flight.store(first, Ordering::SeqCst);
        assert!(ptt.defer_during_finish(PttEvent::Down(PttSource::Ble)));
        assert!(ptt.defer_during_finish(PttEvent::Up(PttSource::Ble)));
        assert_eq!(
            ptt.finish_completed(first)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![PttEvent::Down(PttSource::Ble), PttEvent::Up(PttSource::Ble)]
        );

        let second = ptt.begin(PttSource::Ble, 7).unwrap();
        assert!(second > first);
        assert!(ptt.mark_start_clicked(7, second));
        ptt.finish(PttSource::Ble).unwrap();
        ptt.finish_in_flight.store(second, Ordering::SeqCst);
        assert!(ptt.defer_during_finish(PttEvent::Down(PttSource::Ble)));
        assert!(ptt.defer_during_finish(PttEvent::SubmitBle));
        assert!(ptt.defer_during_finish(PttEvent::CancelBle));
        assert_eq!(
            ptt.finish_completed(second)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![PttEvent::CancelBle]
        );
    }

    #[test]
    fn closing_finish_tab_invalidates_its_pending_callback() {
        let ptt = PttTracker::new();
        let generation = ptt.begin(PttSource::Ble, 8).unwrap();
        assert!(ptt.mark_start_clicked(8, generation));
        ptt.finish(PttSource::Ble).unwrap();
        ptt.finish_tab_id.store(8, Ordering::SeqCst);
        ptt.finish_in_flight.store(generation, Ordering::SeqCst);
        ptt.clear_tab(8);
        assert!(ptt.finish_completed(generation).is_none());
    }

    #[test]
    fn release_and_disconnect_wait_for_delayed_start_observation() {
        let ptt = PttTracker::new();
        let generation = ptt.begin(PttSource::Ble, 9).unwrap();
        ptt.start_in_flight.store(generation, Ordering::SeqCst);
        assert!(ptt.defer_during_finish(PttEvent::Up(PttSource::Ble)));
        assert!(ptt.defer_during_finish(PttEvent::CancelBle));
        assert_eq!(
            ptt.start_completed(generation)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![PttEvent::CancelBle]
        );
    }
}

#[cfg(test)]
mod tab_status_tests {
    use super::*;

    #[test]
    fn decodes_string_wire_format_and_rejects_untrusted_messages() {
        let source = "https://chatgpt.com/c/123";
        assert_eq!(
            decode_tab_status(
                source,
                r#"{"kind":"gpt-wrapper-tab-status","status":"working"}"#
            ),
            Ok(TabStatus::Working)
        );
        assert_eq!(
            decode_tab_status(
                "https://chat.openai.com/",
                r#"{"kind":"gpt-wrapper-tab-status","status":"voice"}"#
            ),
            Ok(TabStatus::Voice)
        );
        for untrusted in [
            "http://chatgpt.com/",
            "https://evil.example/",
            "https://chatgpt.com.evil.example/",
        ] {
            assert_eq!(
                decode_tab_status(
                    untrusted,
                    r#"{"kind":"gpt-wrapper-tab-status","status":"send"}"#
                ),
                Err("source rejected")
            );
        }
        assert_eq!(
            decode_tab_status(source, r#"{"kind":"other","status":"send"}"#),
            Err("payload rejected")
        );
        assert_eq!(
            decode_tab_status(
                source,
                r#"{"kind":"gpt-wrapper-tab-status","status":"unknown"}"#
            ),
            Err("payload rejected")
        );
        assert_eq!(
            decode_tab_status(
                source,
                r#"{"kind":"gpt-wrapper-tab-status","status":"send"}"#
            ),
            Ok(TabStatus::Send)
        );
        assert_eq!(
            decode_tab_status(
                source,
                r#"{"kind":"gpt-wrapper-tab-status","status":"fallback"}"#
            ),
            Ok(TabStatus::Fallback)
        );
    }

    #[test]
    fn updates_only_the_matching_tab_and_preserves_standby_status() {
        let mut tabs = TabManager::new();
        let spec = ProjectSettings::default().default_tab_spec();
        for id in [1, 2, 3] {
            tabs.contexts.insert(id, TabContext::new(spec.clone()));
        }
        for id in [1, 2] {
            tabs.tabs.push(TabInfo {
                id,
                title: String::new(),
                status: TabStatus::Fallback,
            });
        }
        assert!(tabs.update_status(2, TabStatus::Working));
        assert_eq!(tabs.tabs[0].status, TabStatus::Fallback);
        assert_eq!(tabs.tabs[1].status, TabStatus::Working);
        assert!(!tabs.update_status(2, TabStatus::Working));
        assert!(!tabs.update_status(3, TabStatus::Voice));
        assert_eq!(tabs.contexts[&3].status, TabStatus::Voice);
        assert!(!tabs.update_status(4, TabStatus::Send));
    }

    #[test]
    fn reorder_tabs_changes_only_visible_order() {
        let mut tabs = TabManager::new();
        let spec = ProjectSettings::default().default_tab_spec();
        for id in [1, 2, 3] {
            tabs.contexts.insert(id, TabContext::new(spec.clone()));
            tabs.tabs.push(TabInfo {
                id,
                title: format!("Tab {id}"),
                status: TabStatus::Fallback,
            });
        }
        tabs.active_tab_id = Some(2);
        tabs.standby_ptt_tab_id = Some(4);
        tabs.contexts.insert(4, TabContext::new(spec));

        assert!(tabs.reorder(&[3, 1, 2]).is_ok());
        assert_eq!(
            tabs.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![3, 1, 2]
        );
        assert_eq!(tabs.active_tab_id, Some(2));
        assert_eq!(tabs.standby_ptt_tab_id, Some(4));
        assert_eq!(tabs.contexts.len(), 4);

        for invalid in [vec![1, 2], vec![1, 1, 2], vec![1, 2, 5], vec![1, 2, 3, 4]] {
            assert!(tabs.reorder(&invalid).is_err());
            assert_eq!(
                tabs.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
                vec![3, 1, 2]
            );
        }
    }
}
