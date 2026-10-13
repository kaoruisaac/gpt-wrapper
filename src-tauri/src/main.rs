#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod background_project_switcher;
mod ble_central;
mod general_settings;
mod microphone_inventory;
mod project_settings;
mod shortcut_settings;
mod standby_navigation;
mod tab_completion_notifications;

use ble_central::{BleCentral, ControlEvent};
use general_settings::GeneralSettingsState;
use project_settings::{ProjectSettings, ProjectSettingsState, TabSpec};
use serde::{Deserialize, Serialize};
use shortcut_settings::{parse_and_validate, ShortcutSettings, ShortcutSettingsState};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::{Color, PageLoadEvent, Webview, WebviewBuilder, WebviewWindowBuilder},
    window::{Window, WindowBuilder},
    AppHandle, LogicalPosition, LogicalSize, Manager, WebviewUrl, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use uuid::Uuid;
use webview2_com::{
    CallDevToolsProtocolMethodCompletedHandler, CoTaskMemPWSTR, ContextMenuRequestedEventHandler,
    CustomItemSelectedEventHandler, ExecuteScriptCompletedHandler,
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2Environment9, ICoreWebView2_11, ICoreWebView2_2,
        COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_COMMAND,
    },
    NavigationCompletedEventHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler, WebMessageReceivedEventHandler,
};
use windows::{
    core::{Interface, HSTRING, PCWSTR, PWSTR},
    Win32::UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, VK_LBUTTON},
        Shell::ShellExecuteW,
        WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, GetWindowPlacement, IsIconic,
            SetForegroundWindow, ShowWindow, TrackPopupMenuEx, MF_CHECKED, MF_STRING, SW_RESTORE,
            SW_SHOWMAXIMIZED, SW_SHOWNORMAL, TPM_LEFTBUTTON, TPM_NONOTIFY, TPM_RETURNCMD,
            TPM_RIGHTALIGN, WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED,
        },
    },
};

const ADAPTER_SCRIPT: &str = include_str!("chatgpt_adapter.js");
const MICROPHONE_POC_SCRIPT: &str = include_str!("microphone_poc.js");
const MICROPHONE_INVENTORY_SCRIPT: &str = include_str!("microphone_inventory.js");
const MICROPHONE_ROUTE_SCRIPT: &str = include_str!("microphone_route.js");

fn microphone_poc_enabled() -> bool {
    cfg!(debug_assertions) && std::env::var_os("GPT_WRAPPER_MICROPHONE_POC").is_some()
}

fn trusted_chatgpt_source(source: &str) -> bool {
    url::Url::parse(source)
        .map(|url| {
            url.scheme() == "https"
                && matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"))
        })
        .unwrap_or(false)
}

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
    #[serde(default, rename = "nativeSend")]
    native_send: bool,
}

#[derive(Clone, Copy, Debug)]
enum DictationTarget {
    Start,
    // An unsafe Finish control may submit the prompt as part of stopping dictation.
    // Never use it when the user's auto-submit preference is disabled.
    Finish { allow_native_send: bool },
}

impl DictationTarget {
    fn rect_script(self) -> String {
        let start_labels = ["聽寫", "開始聽寫", "开始听写", "Start dictation", "Dictate"];
        // "Submit dictation" inserts the transcription into the composer; a
        // "Transcribe and send" control can submit the entire message instead.
        let insert_labels = [
            "停止聽寫",
            "Stop dictation",
            "Submit dictation",
            "提交听写内容",
            "送出聽寫內容",
            "Insert transcription",
            "完成轉錄",
            "完成转录",
        ];
        let native_send_labels = ["Transcribe and send", "轉錄並傳送"];
        let (labels, native_labels): (Vec<&str>, Vec<&str>) = match self {
            Self::Start => (start_labels.to_vec(), vec![]),
            Self::Finish { allow_native_send } => (
                insert_labels.to_vec(),
                if allow_native_send {
                    native_send_labels.to_vec()
                } else {
                    vec![]
                },
            ),
        };

        format!(
            r#"(() => {{
  const insertLabels = {};
  const nativeLabels = {};
  const buttons = [...document.querySelectorAll("button")];
  for (const [labels, nativeSend] of [[insertLabels, false], [nativeLabels, true]]) {{
    for (const b of buttons) {{
      const label = (b.getAttribute("aria-label") || "").trim();
      const r = b.getBoundingClientRect();
      if (r.width > 0 && r.height > 0 && labels.includes(label)) {{
        return {{ x: r.left + r.width / 2, y: r.top + r.height / 2, nativeSend }};
      }}
    }}
  }}
  return null;
}})()"#,
            serde_json::to_string(&labels).unwrap(),
            serde_json::to_string(&native_labels).unwrap(),
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
    if !trusted_chatgpt_source(source) {
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
    standby_debounce: standby_navigation::Debounce,
    standby_target: Option<TabSpec>,
}

#[derive(Debug)]
struct TabContext {
    spec: TabSpec,
    // WebView document.title, including for standby tabs not yet shown in the Tab Bar.
    page_title: String,
    status: TabStatus,
    prefix_applied: bool,
    prefix_in_progress: bool,
    background_ptt_pending: bool,
    navigation: Option<standby_navigation::Navigation>,
}

impl TabContext {
    fn new(spec: TabSpec) -> Self {
        Self {
            spec,
            page_title: project_settings::CHATGPT_TITLE.to_string(),
            status: TabStatus::Fallback,
            prefix_applied: false,
            prefix_in_progress: false,
            background_ptt_pending: false,
            navigation: None,
        }
    }

    fn adopt_standby_spec(&mut self, spec: TabSpec) -> Result<(bool, bool), String> {
        let navigation = self
            .navigation
            .as_mut()
            .ok_or("standby navigation is missing")?;
        let navigate = navigation.prepare(spec.url.clone());
        let ready = navigation.ready();
        if navigate {
            // A new destination must not inherit the previous document's title.
            self.page_title = project_settings::CHATGPT_TITLE.to_string();
        }
        if self.spec != spec || navigate {
            self.prefix_applied = false;
            self.prefix_in_progress = false;
            self.status = TabStatus::Fallback;
        }
        self.spec = spec;
        Ok((navigate, ready))
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
            standby_debounce: standby_navigation::Debounce::default(),
            standby_target: None,
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

    fn update_title(&mut self, tab_id: usize, page_title: &str) -> bool {
        let Some(context) = self.contexts.get_mut(&tab_id) else {
            return false;
        };
        let title = if page_title.trim().is_empty() {
            project_settings::CHATGPT_TITLE
        } else {
            page_title.trim()
        };
        if context.page_title == title {
            return false;
        }
        context.page_title = title.to_string();
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.title = context.page_title.clone();
            true
        } else {
            // Preserve the title until this standby WebView is promoted.
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
    // An uncertain dictation state must never lock unrelated tabs.
    recovery_needed_tabs: Mutex<HashSet<usize>>,
    recovery_in_flight: AtomicUsize,
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
    // tracker, app, tab, generation, auto-submit setting, queued explicit Submit
    Finish(Arc<PttTracker>, AppHandle, usize, usize, bool, bool),
    Cancel(Arc<PttTracker>, usize),
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
    SubmitKeyboard(Option<usize>),
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
            recovery_needed_tabs: Mutex::new(HashSet::new()),
            recovery_in_flight: AtomicUsize::new(0),
            deferred_events: Mutex::new(VecDeque::new()),
        }
    }

    fn is_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    fn safe_for_background_switch(&self) -> bool {
        !self.is_held()
            && self.start_in_flight.load(Ordering::SeqCst) == 0
            && self.finish_in_flight.load(Ordering::SeqCst) == 0
            && self.recovery_in_flight.load(Ordering::SeqCst) == 0
            && !self.ble_submit_pending.load(Ordering::SeqCst)
            && self
                .deferred_events
                .lock()
                .map(|events| events.is_empty())
                .unwrap_or(false)
    }

    fn recovery_needed(&self, tab_id: usize) -> bool {
        self.recovery_needed_tabs
            .lock()
            .map(|tabs| tabs.contains(&tab_id))
            .unwrap_or(true)
    }

    fn mark_recovery_needed(&self, tab_id: usize) {
        if let Ok(mut tabs) = self.recovery_needed_tabs.lock() {
            tabs.insert(tab_id);
        }
    }

    fn clear_recovery_needed(&self, tab_id: usize) {
        if let Ok(mut tabs) = self.recovery_needed_tabs.lock() {
            tabs.remove(&tab_id);
        }
    }

    fn begin_recovery(&self, source: PttSource, tab_id: usize) -> Option<usize> {
        if !self.recovery_needed(tab_id) {
            return None;
        }
        let generation = self.begin(source, tab_id)?;
        // Defer any Up/Cancel until the DOM probe has returned. In particular,
        // a release before callback delivery must never trigger a late Start.
        self.start_in_flight.store(generation, Ordering::SeqCst);
        self.recovery_in_flight.store(generation, Ordering::SeqCst);
        Some(generation)
    }

    fn is_recovering(&self, tab_id: usize, generation: usize) -> bool {
        self.recovery_in_flight.load(Ordering::SeqCst) == generation
            && self.is_current(tab_id, generation)
    }

    // A successful probe consumes the recovery operation. Only a still-held
    // original Down may proceed to Start. A recovery Stop requires a new press.
    fn complete_recovery(&self, tab_id: usize, generation: usize, can_start: bool) -> bool {
        if !self.is_recovering(tab_id, generation) {
            return false;
        }
        let Some(events) = self.start_completed(generation) else {
            return false;
        };
        if self
            .recovery_in_flight
            .compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        self.clear_recovery_needed(tab_id);
        self.ble_submit_pending.store(false, Ordering::SeqCst);
        let was_released = events
            .iter()
            .any(|event| matches!(event, PttEvent::Up(_) | PttEvent::CancelBle));
        if !can_start || was_released {
            self.clear_tab(tab_id);
            return false;
        }
        true
    }

    fn abort_recovery(&self, tab_id: usize, generation: usize) -> bool {
        if !self.is_recovering(tab_id, generation) {
            return false;
        }
        self.clear_tab(tab_id);
        self.mark_recovery_needed(tab_id);
        true
    }

    fn clear_deferred_submits(&self) {
        if let Ok(mut events) = self.deferred_events.lock() {
            events.retain(|event| {
                !matches!(event, PttEvent::SubmitBle | PttEvent::SubmitKeyboard(_))
            });
        }
    }

    fn defer_during_finish(&self, event: PttEvent) -> bool {
        // A BLE disconnect must cancel an earlier BLE draft even while a
        // keyboard-owned Start is waiting for its DOM callback.
        if event == PttEvent::CancelBle
            && self
                .owner
                .lock()
                .ok()
                .is_some_and(|owner| *owner == Some(PttSource::Keyboard))
        {
            return false;
        }
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
        self.finish_tab_id.store(0, Ordering::SeqCst);
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
            self.recovery_in_flight.store(0, Ordering::SeqCst);
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
        self.clear_recovery_needed(tab_id);
    }

    fn cancel_ble(&self) -> Option<PttCancel> {
        let mut owner = self.owner.lock().ok()?;
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

fn set_tab_title(app_handle: &AppHandle, tab_id: usize, title: &str) {
    let should_refresh = app_handle
        .state::<TabState>()
        .lock()
        .map(|mut tabs| tabs.update_title(tab_id, title))
        .unwrap_or(false);
    if should_refresh {
        refresh_tab_host(app_handle);
    }
}

fn set_tab_status(app_handle: &AppHandle, tab_id: usize, status: TabStatus) {
    let (should_refresh, previous, title) = {
        let state = app_handle.state::<TabState>();
        let Ok(mut tabs) = state.lock() else {
            return;
        };
        let previous = tabs.contexts.get(&tab_id).map(|context| context.status);
        let title = tabs
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.title.clone());
        let changed = tabs.update_status(tab_id, status);
        (changed, previous, title)
    };
    if let Some(previous) = previous {
        if previous != status {
            tab_completion_notifications::on_status(app_handle, tab_id, previous, status, title);
        }
    }
    if should_refresh {
        refresh_tab_host(app_handle);
    }
}

// Navigation resets are not DOM-observed completions.
fn reset_tab_status(app_handle: &AppHandle, tab_id: usize) {
    tab_completion_notifications::cancel_tab(app_handle, tab_id);
    let should_refresh = app_handle
        .state::<TabState>()
        .lock()
        .map(|mut tabs| tabs.update_status(tab_id, TabStatus::Fallback))
        .unwrap_or(false);
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
                if microphone_inventory::handle_route_message(&app_handle, tab_id, &source, &raw)
                    || microphone_inventory::handle_message(&app_handle, tab_id, &source, &raw) {
                    return Ok(());
                }
                // Developer-only capture-path evidence. Reject other origins and
                // never expose these logs in a production build.
                if microphone_poc_enabled()
                    && serde_json::from_str::<serde_json::Value>(&raw)
                        .ok()
                        .and_then(|value| {
                            value.get("kind").and_then(|kind| kind.as_str()).map(str::to_owned)
                        })
                        .as_deref() == Some("gpt-wrapper-microphone-probe")
                {
                    if trusted_chatgpt_source(&source) {
                        println!("[gpt-wrapper] microphone POC tab {tab_id}: {}", raw.chars().take(2048).collect::<String>());
                    }
                    return Ok(());
                }
                let page_ready = app_handle.state::<TabState>().lock().ok()
                    .and_then(|tabs| tabs.contexts.get(&tab_id).map(|c|
                        c.navigation.as_ref().is_none_or(|n| n.ready())))
                    .unwrap_or(false);
                if !page_ready { return Ok(()); }
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

// WebView2 navigation IDs distinguish even A -> B -> A and duplicate completions.
// Tauri's URL-only PageLoad callback cannot provide that guarantee.
fn install_navigation_listener(
    app: &AppHandle,
    webview: &Webview,
    tab_id: usize,
) -> Result<(), String> {
    let app = app.clone();
    let setup_error = Arc::new(Mutex::new(None));
    let callback_error = Arc::clone(&setup_error);
    webview.with_webview(move |platform| {
        let result = (|| -> windows::core::Result<()> {
            let core = unsafe { platform.controller().CoreWebView2()? };
            let starting_app = app.clone();
            let starting = NavigationStartingEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()); };
                let mut id = 0;
                let mut uri = PWSTR::null();
                unsafe { args.NavigationId(&mut id)?; args.Uri(&mut uri)?; }
                let Ok(url) = url::Url::parse(&CoTaskMemPWSTR::from(uri).to_string()) else { return Ok(()); };
                let accepted = {
                    let state = starting_app.state::<TabState>();
                    let Ok(mut tabs) = state.lock() else { return Ok(()); };
                    let accepted = tabs.contexts.get_mut(&tab_id)
                        .and_then(|c| c.navigation.as_mut())
                        .is_some_and(|n| n.started(id, &url));
                    let ready = tabs.contexts.get(&tab_id).and_then(|c| c.navigation.as_ref())
                        .is_some_and(|n| n.ready());
                    if !ready && tabs.standby_ptt_tab_id == Some(tab_id) {
                        tabs.standby_ptt_ready = false;
                    }
                    accepted
                };
                if accepted {
                    if let Some(webview) = get_tab_webview(&starting_app, tab_id) {
                        handle_tab_page_load(&webview, tab_id, PageLoadEvent::Started, &url);
                    }
                }
                Ok(())
            }));
            let completed = NavigationCompletedEventHandler::create(Box::new(move |sender, args| {
                let (Some(sender), Some(args)) = (sender, args) else { return Ok(()); };
                let mut id = 0;
                let mut success = windows::core::BOOL::default();
                let mut source = PWSTR::null();
                unsafe { args.NavigationId(&mut id)?; args.IsSuccess(&mut success)?; sender.Source(&mut source)?; }
                let Ok(url) = url::Url::parse(&CoTaskMemPWSTR::from(source).to_string()) else { return Ok(()); };
                let accepted = app.state::<TabState>().lock().ok()
                    .and_then(|mut tabs| tabs.contexts.get_mut(&tab_id)
                        .and_then(|c| c.navigation.as_mut()).map(|n| n.finished(id, &url, success.as_bool())))
                    .unwrap_or(false);
                if accepted {
                    eprintln!("[gpt-wrapper] standby navigation finished: tab={tab_id}, navigation={id}");
                    if let Some(webview) = get_tab_webview(&app, tab_id) {
                        handle_tab_page_load(&webview, tab_id, PageLoadEvent::Finished, &url);
                    }
                } else if !success.as_bool() {
                    eprintln!("[gpt-wrapper] navigation failed/obsolete: tab={tab_id}, navigation={id}");
                }
                Ok(())
            }));
            let mut token = 0;
            unsafe {
                core.add_NavigationStarting(&starting, &mut token)?;
                core.add_NavigationCompleted(&completed, &mut token)?;
            }
            Ok(())
        })();
        if let Err(error) = result { *callback_error.lock().unwrap() = Some(error.to_string()); }
    }).map_err(|error| error.to_string())?;
    let error = setup_error.lock().unwrap().take();
    error.map_or(Ok(()), Err)
}

fn handle_tab_page_load(
    webview: &Webview,
    tab_id: usize,
    event: PageLoadEvent,
    page_url: &url::Url,
) {
    if event == PageLoadEvent::Finished {
        let current_spec = webview
            .app_handle()
            .state::<ProjectSettingsState>()
            .snapshot()
            .ok()
            .map(|settings| settings.default_tab_spec());
        let allowed = webview
            .app_handle()
            .state::<TabState>()
            .lock()
            .ok()
            .is_some_and(|tabs| {
                let Some(context) = tabs.contexts.get(&tab_id) else {
                    return false;
                };
                let Some(navigation) = &context.navigation else {
                    return true;
                };
                navigation.ready()
                    && (tabs.standby_ptt_tab_id != Some(tab_id)
                        || (tabs.standby_debounce.pending.is_none()
                            && tabs
                                .standby_target
                                .as_ref()
                                .is_none_or(|target| target == &context.spec)
                            && current_spec.as_ref() == Some(&context.spec)))
            });
        if !allowed {
            return;
        }
    }
    if event == PageLoadEvent::Started {
        webview
            .app_handle()
            .state::<microphone_inventory::MicrophoneRouting>()
            .forget_tab(tab_id);
        let was_loaded = webview
            .app_handle()
            .state::<TabState>()
            .lock()
            .map(|mut tabs| tabs.loaded_tabs.remove(&tab_id))
            .unwrap_or(false);
        if was_loaded {
            webview.app_handle().state::<PttState>().0.clear_tab(tab_id);
        }
        reset_tab_status(webview.app_handle(), tab_id);
        return;
    }
    if event != PageLoadEvent::Finished || page_url.path().ends_with(TAB_BOOTSTRAP_PATH) {
        return;
    }

    if let Err(error) = microphone_inventory::sync_tab(webview.app_handle(), tab_id, &webview) {
        eprintln!("[gpt-wrapper] failed to sync microphone selection on tab {tab_id}: {error}");
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
        // The promoted page is now confirmed. Subsequent user navigation has
        // the ordinary visible-tab semantics rather than a standby target gate.
        if let Ok(mut tabs) = app_handle.state::<TabState>().lock() {
            if let Some(context) = tabs.contexts.get_mut(&tab_id) {
                context.navigation = None;
            }
        }
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
}

// Links opened into a GPT Wrapper tab do not inherit the default Project prefix.
fn link_tab_spec(link_uri: &str) -> Result<TabSpec, String> {
    Ok(TabSpec {
        url: project_settings::valid_url(link_uri)?,
        title: project_settings::CHATGPT_TITLE.to_string(),
        project_id: None,
        message_prefix: String::new(),
    })
}

// Only web links are sent outside the app. Never pass arbitrary URL schemes to the OS.
fn external_link_url(link_uri: &str) -> Result<url::Url, String> {
    project_settings::valid_url(link_uri)
}

// ShellExecuteW uses the user's Windows default browser. Run it off the WebView
// event thread so link activation cannot stall WebView2 or Tauri's UI loop.
fn open_in_system_browser(url: url::Url, tab_id: usize) {
    tauri::async_runtime::spawn_blocking(move || {
        let wide_url: Vec<u16> = url
            .as_str()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                windows::core::w!("open"),
                PCWSTR(wide_url.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if (result.0 as usize) <= 32 {
            eprintln!("[gpt-wrapper] failed to open external link from tab {tab_id}: {url} (ShellExecuteW code {})", result.0 as usize);
        }
    });
}

#[cfg(test)]
mod external_link_tests {
    use super::*;

    #[test]
    fn new_tabs_use_clicked_urls_without_project_prefix() {
        let spec = link_tab_spec("https://chatgpt.com/c/test?view=full#latest").unwrap();
        assert_eq!(
            spec.url.as_str(),
            "https://chatgpt.com/c/test?view=full#latest"
        );
        assert_eq!(spec.project_id, None);
        assert_eq!(spec.message_prefix, "");
        assert_eq!(spec.title, project_settings::CHATGPT_TITLE);
    }

    #[test]
    fn new_tabs_reject_non_web_urls() {
        for url in [
            "javascript:alert(1)",
            "file:///C:/secret",
            "mailto:user@example.com",
        ] {
            assert!(link_tab_spec(url).is_err(), "unexpected allowed URL: {url}");
        }
    }

    #[test]
    fn external_links_preserve_web_urls() {
        assert_eq!(
            external_link_url("https://chatgpt.com/c/test?view=full#latest")
                .unwrap()
                .as_str(),
            "https://chatgpt.com/c/test?view=full#latest"
        );
        assert_eq!(
            external_link_url("http://example.com/path")
                .unwrap()
                .as_str(),
            "http://example.com/path"
        );
    }

    #[test]
    fn external_links_reject_non_web_urls() {
        for url in [
            "javascript:alert(1)",
            "file:///C:/secret",
            "mailto:user@example.com",
            "https://",
            "/relative",
        ] {
            assert!(
                external_link_url(url).is_err(),
                "unexpected allowed URL: {url}"
            );
        }
    }
}

fn install_link_context_menu(
    app: &AppHandle,
    webview: &Webview,
    tab_id: usize,
) -> Result<(), String> {
    let app = app.clone();
    let setup_error = Arc::new(Mutex::new(None));
    let setup_error_for_callback = Arc::clone(&setup_error);
    webview
        .with_webview(move |platform| {
            let setup = (|| -> windows::core::Result<()> {
                let core = unsafe { platform.controller().CoreWebView2()? };
                let core11: ICoreWebView2_11 = core.cast()?;
                let core2: ICoreWebView2_2 = core.cast()?;
                let environment: ICoreWebView2Environment9 =
                    unsafe { core2.Environment()? }.cast()?;

                let handler = ContextMenuRequestedEventHandler::create(Box::new(move |_, args| {
                    // Leave the native menu unchanged unless a valid web link was clicked.
                    let Some(args) = args else {
                        return Ok(());
                    };
                    let result = (|| -> windows::core::Result<()> {
                        let target = unsafe { args.ContextMenuTarget()? };
                        let mut has_link = windows::core::BOOL::default();
                        unsafe { target.HasLinkUri(&mut has_link)? };
                        if !has_link.as_bool() {
                            return Ok(());
                        }

                        let mut raw_link = PWSTR::null();
                        unsafe { target.LinkUri(&mut raw_link)? };
                        let link_uri = CoTaskMemPWSTR::from(raw_link).to_string();
                        let Ok(spec) = link_tab_spec(&link_uri) else {
                            return Ok(());
                        };
                        let url = spec.url.clone();
                        let items = unsafe { args.MenuItems()? };

                        // Remove the native "open in new window" action. WebView2 uses
                        // this unlocalized Name regardless of the UI language.
                        // A native new-tab action would also hit NewWindowRequested,
                        // so replace it with our in-app tab action if it exists.
                        let mut count = 0;
                        unsafe { items.Count(&mut count)? };
                        for index in (0..count).rev() {
                            let existing = unsafe { items.GetValueAtIndex(index)? };
                            let mut raw_name = PWSTR::null();
                            unsafe { existing.Name(&mut raw_name)? };
                            let name = CoTaskMemPWSTR::from(raw_name).to_string();
                            if matches!(name.as_str(), "openLinkInNewWindow" | "openLinkInNewTab") {
                                unsafe { items.RemoveValueAtIndex(index)? };
                            }
                        }

                        let tab_label: Vec<u16> = "在新分頁中開啟連結\0".encode_utf16().collect();
                        let tab_item = unsafe {
                            environment.CreateContextMenuItem(
                                PCWSTR(tab_label.as_ptr()),
                                None,
                                COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_COMMAND,
                            )?
                        };
                        let tab_app = app.clone();
                        let tab_selected =
                            CustomItemSelectedEventHandler::create(Box::new(move |_, _| {
                                // add_child waits on the UI loop. Never create tabs inside
                                // the WebView2 context-menu callback itself.
                                let worker_app = tab_app.clone();
                                let worker_spec = spec.clone();
                                tauri::async_runtime::spawn_blocking(move || {
                                    match create_tab_with_spec(&worker_app, worker_spec, false) {
                                        Ok(_) => refresh_tab_host(&worker_app),
                                        Err(error) => eprintln!(
                                            "[gpt-wrapper] failed to open link in new tab: {error}"
                                        ),
                                    }
                                });
                                Ok(())
                            }));
                        let mut tab_selected_token = 0;
                        unsafe {
                            tab_item
                                .add_CustomItemSelected(&tab_selected, &mut tab_selected_token)?
                        };
                        unsafe { items.InsertValueAtIndex(0, &tab_item)? };

                        let label: Vec<u16> = "在預設瀏覽器開啟連結\0".encode_utf16().collect();
                        let item = unsafe {
                            environment.CreateContextMenuItem(
                                PCWSTR(label.as_ptr()),
                                None,
                                COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_COMMAND,
                            )?
                        };

                        let selected =
                            CustomItemSelectedEventHandler::create(Box::new(move |_, _| {
                                open_in_system_browser(url.clone(), tab_id);
                                Ok(())
                            }));
                        let mut selected_token = 0;
                        unsafe { item.add_CustomItemSelected(&selected, &mut selected_token)? };

                        unsafe { items.InsertValueAtIndex(1, &item)? };
                        Ok(())
                    })();
                    if let Err(error) = result {
                        eprintln!(
                            "[gpt-wrapper] failed to extend context menu on tab {tab_id}: {error:?}"
                        );
                    }
                    Ok(())
                }));
                let mut token = 0;
                unsafe { core11.add_ContextMenuRequested(&handler, &mut token)? };
                Ok(())
            })();
            if let Err(error) = setup {
                *setup_error_for_callback.lock().unwrap() = Some(format!(
                    "failed to install link context menu on tab {tab_id}: {error:?}"
                ));
            }
        })
        .map_err(|error| format!("failed to access WebView2 on tab {tab_id}: {error}"))?;
    let result = setup_error.lock().unwrap().take();
    result.map_or(Ok(()), Err)
}

fn install_external_new_window_handler(webview: &Webview, tab_id: usize) -> Result<(), String> {
    let setup_error = Arc::new(Mutex::new(None));
    let callback_error = Arc::clone(&setup_error);
    webview
        .with_webview(move |platform| {
            let setup = (|| -> windows::core::Result<()> {
                let core = unsafe { platform.controller().CoreWebView2()? };
                let handler = NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else {
                        return Ok(());
                    };
                    // Suppress WebView2's default popup even for unsupported schemes.
                    unsafe { args.SetHandled(true)? };
                    let mut raw_uri = PWSTR::null();
                    unsafe { args.Uri(&mut raw_uri)? };
                    let uri = CoTaskMemPWSTR::from(raw_uri).to_string();
                    match external_link_url(&uri) {
                        Ok(url) => open_in_system_browser(url, tab_id),
                        Err(_) => eprintln!(
                            "[gpt-wrapper] ignored non-web popup URL on tab {tab_id}: {uri}"
                        ),
                    }
                    Ok(())
                }));
                let mut token = 0;
                unsafe { core.add_NewWindowRequested(&handler, &mut token)? };
                Ok(())
            })();
            if let Err(error) = setup {
                *callback_error.lock().unwrap() = Some(format!(
                    "failed to intercept new windows on tab {tab_id}: {error:?}"
                ));
            }
        })
        .map_err(|error| format!("failed to access WebView2 on tab {tab_id}: {error}"))?;
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
    .initialization_script(MICROPHONE_INVENTORY_SCRIPT)
    .initialization_script(MICROPHONE_ROUTE_SCRIPT)
    .initialization_script(if microphone_poc_enabled() {
        MICROPHONE_POC_SCRIPT
    } else {
        ""
    })
    .background_color(LOADING_BACKGROUND)
    .focused(false)
    .on_document_title_changed(move |webview, title| {
        set_tab_title(webview.app_handle(), tab_id, &title);
    })
    .on_page_load(move |webview, payload| {
        let managed = webview
            .app_handle()
            .state::<TabState>()
            .lock()
            .ok()
            .and_then(|tabs| tabs.contexts.get(&tab_id).map(|c| c.navigation.is_some()))
            .unwrap_or(false);
        if !managed {
            handle_tab_page_load(&webview, tab_id, payload.event(), payload.url());
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

    if let Err(error) = install_navigation_listener(app_handle, &webview, tab_id) {
        let _ = webview.close();
        return Err(error);
    }

    if let Err(error) = install_external_new_window_handler(&webview, tab_id) {
        let _ = webview.close();
        return Err(error);
    }

    // Keep the main browser usable even if an old WebView2 runtime lacks this API.
    if let Err(error) = install_link_context_menu(app_handle, &webview, tab_id) {
        eprintln!("[gpt-wrapper] link context menu unavailable on tab {tab_id}: {error}");
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
            title: project_settings::CHATGPT_TITLE.to_string(),
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

fn schedule_standby_target(app: &AppHandle) -> Result<(), String> {
    let spec = app
        .state::<ProjectSettingsState>()
        .snapshot()?
        .default_tab_spec();
    let start_worker = {
        let state = app.state::<TabState>();
        let mut tabs = state.lock()?;
        let id = tabs.standby_ptt_tab_id;
        if id.is_none()
            && !app
                .state::<TrayVisibilityState>()
                .tray_hidden
                .load(Ordering::SeqCst)
        {
            return Ok(());
        }
        if let Some(id) = id {
            if let Some(navigation) = tabs
                .contexts
                .get_mut(&id)
                .and_then(|c| c.navigation.as_mut())
            {
                navigation.desired = spec.url.clone();
            }
            // Includes same-URL metadata changes: readiness is reconciled with
            // the full spec when the debounce expires or PTT takes over.
            tabs.standby_ptt_ready = false;
        }
        tabs.standby_target = Some(spec);
        // Zero means create only if still tray-hidden with no standby at execution.
        let start = tabs
            .standby_debounce
            .schedule(id.unwrap_or(0), Instant::now());
        eprintln!(
            "[gpt-wrapper] standby navigation scheduled: tab={id:?}, generation={}",
            tabs.standby_debounce.generation
        );
        start
    };
    if start_worker {
        spawn_standby_worker(app.clone());
    }
    Ok(())
}

// At most one sleeping worker or queued UI callback per app, irrespective of
// key count. The deadline and generation are checked again on the UI thread.
fn spawn_standby_worker(app: AppHandle) {
    thread::spawn(move || loop {
        let work = app.state::<TabState>().lock().ok().and_then(|mut tabs| {
            match tabs.standby_debounce.pending {
                Some((id, deadline)) => Some((id, deadline, tabs.standby_debounce.generation)),
                None => {
                    tabs.standby_debounce.worker_running = false;
                    None
                }
            }
        });
        let Some((id, deadline, generation)) = work else {
            return;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            thread::sleep(remaining.min(standby_navigation::DEBOUNCE));
            continue;
        }
        let callback_app = app.clone();
        if let Err(error) = app.run_on_main_thread(move || {
            let (execute, restart) = {
                let state = callback_app.state::<TabState>();
                let Ok(mut tabs) = state.lock() else {
                    return;
                };
                let due = tabs.standby_debounce.due(generation, id, Instant::now());
                let identity_matches = tabs.standby_ptt_tab_id.unwrap_or(0) == id;
                let execute = due && identity_matches;
                if due {
                    tabs.standby_debounce.pending = None;
                }
                let restart = tabs.standby_debounce.pending.is_some();
                tabs.standby_debounce.worker_running = restart;
                (execute, restart)
            };
            if execute {
                let result = if id == 0 {
                    if callback_app
                        .state::<TrayVisibilityState>()
                        .tray_hidden
                        .load(Ordering::SeqCst)
                    {
                        ensure_standby_ptt_tab(&callback_app)
                    } else {
                        Ok(())
                    }
                } else {
                    apply_standby_target(&callback_app).map(|_| ())
                };
                if let Err(error) = result {
                    eprintln!("[gpt-wrapper] standby target update failed: {error}");
                }
            }
            if restart {
                spawn_standby_worker(callback_app);
            }
        }) {
            eprintln!("[gpt-wrapper] standby UI dispatch failed: {error}");
            if let Ok(mut tabs) = app.state::<TabState>().lock() {
                tabs.standby_debounce.worker_running = false;
            }
        }
        return;
    });
}

// All callers run on the UI thread. Never hold TabState across WebView operations.
fn apply_standby_target(app: &AppHandle) -> Result<usize, String> {
    let spec = app
        .state::<ProjectSettingsState>()
        .snapshot()?
        .default_tab_spec();
    let (id, navigate) = {
        let state = app.state::<TabState>();
        let mut tabs = state.lock()?;
        let id = tabs
            .standby_ptt_tab_id
            .ok_or("standby PTT tab is missing")?;
        let context = tabs
            .contexts
            .get_mut(&id)
            .ok_or("standby context is missing")?;
        let (navigate, ready) = context.adopt_standby_spec(spec.clone())?;
        tabs.standby_target = Some(spec.clone());
        tabs.standby_ptt_ready = ready;
        if !ready {
            tabs.loaded_tabs.remove(&id);
        }
        (id, navigate)
    };
    let webview = get_tab_webview(app, id).ok_or("standby WebView is missing")?;
    if navigate {
        eprintln!("[gpt-wrapper] standby navigation started: tab={id}");
        // Ready/load flags were reset before navigate can invoke callbacks.
        app.state::<microphone_inventory::MicrophoneRouting>()
            .forget_tab(id);
        // Cancel the old document/request before issuing another URL, including
        // an old request whose NavigationStarting has not arrived yet.
        let stop_error = Arc::new(Mutex::new(None));
        let callback_error = Arc::clone(&stop_error);
        webview
            .with_webview(move |platform| {
                let result = unsafe {
                    platform
                        .controller()
                        .CoreWebView2()
                        .and_then(|core| core.Stop())
                };
                if let Err(error) = result {
                    *callback_error.lock().unwrap() = Some(error.to_string());
                }
            })
            .map_err(|error| error.to_string())?;
        if let Some(error) = stop_error.lock().unwrap().take() {
            if let Ok(mut tabs) = app.state::<TabState>().lock() {
                if let Some(n) = tabs
                    .contexts
                    .get_mut(&id)
                    .and_then(|c| c.navigation.as_mut())
                {
                    n.fail();
                }
            }
            return Err(format!(
                "failed to stop obsolete standby navigation: {error}"
            ));
        }
        if let Err(error) = webview.navigate(spec.url) {
            if let Ok(mut tabs) = app.state::<TabState>().lock() {
                tabs.standby_ptt_ready = false;
                if let Some(n) = tabs
                    .contexts
                    .get_mut(&id)
                    .and_then(|c| c.navigation.as_mut())
                {
                    n.fail();
                }
            }
            return Err(format!("standby navigation failed on tab {id}: {error}"));
        }
    } else {
        eprintln!("[gpt-wrapper] standby navigation reused: tab={id}");
        let ready = app.state::<TabState>().lock()?.standby_ptt_ready;
        if ready {
            handle_tab_page_load(&webview, id, PageLoadEvent::Finished, &spec.url);
        }
    }
    Ok(id)
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
        let mut context = TabContext::new(spec.clone());
        context.navigation = Some(standby_navigation::Navigation::new(spec.url.clone()));
        tabs.contexts.insert(tab_id, context);
        tab_id
    };

    if let Err(error) = create_chatgpt_webview(app_handle, tab_id, spec.url.clone()) {
        let mut tabs = state.lock()?;
        if tabs.standby_ptt_tab_id == Some(tab_id) {
            tabs.standby_ptt_tab_id = None;
            tabs.standby_ptt_ready = false;
        }
        tabs.loaded_tabs.remove(&tab_id);
        tabs.contexts.remove(&tab_id);
        return Err(error);
    }

    // A settings write may have overlapped the first WebView creation. Convert
    // any newer target into a normal debounce against this newly assigned ID.
    if app_handle
        .state::<ProjectSettingsState>()
        .snapshot()?
        .default_tab_spec()
        != spec
    {
        schedule_standby_target(app_handle)?;
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

        tabs.standby_debounce.cancel();
        tabs.standby_target = None;
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
    {
        app_handle
            .state::<TabState>()
            .lock()?
            .standby_debounce
            .cancel();
    }
    apply_standby_target(app_handle)?;
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
        tabs.standby_debounce.cancel();
        tabs.standby_target = None;
        tabs.standby_ptt_tab_id = None;
        tabs.standby_ptt_ready = false;
        let context = tabs
            .contexts
            .get_mut(&standby_id)
            .ok_or("standby context missing")?;
        context.background_ptt_pending = true;
        if ready {
            context.navigation = None;
        }
        let title = context.page_title.clone();
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
    tab_completion_notifications::cancel_tab(app_handle, tab_id);

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

#[tauri::command]
fn dismiss_tab_completion_notification(
    app: AppHandle,
    webview: Webview,
    tab_id: usize,
    generation: u64,
) -> Result<(), String> {
    if webview.label() != tab_completion_notifications::WINDOW_LABEL {
        return Err("only the completion notification UI may dismiss cards".into());
    }
    let source = webview.url().map_err(|error| error.to_string())?;
    if !matches!(source.scheme(), "tauri" | "http" | "https")
        || !matches!(source.host_str(), Some("localhost" | "tauri.localhost"))
        || source.path() != "/settings/tab-completion-notifications.html"
    {
        return Err("completion notification source rejected".into());
    }
    tab_completion_notifications::dismiss_card(&app, tab_id, generation);
    Ok(())
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
        if let Err(error) = schedule_standby_target(app) {
            eprintln!("[gpt-wrapper] failed to schedule persisted standby target: {error}");
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

// SW_RESTORE must only run for minimized windows: it unmaximizes visible
// maximized windows. Windows also records whether a minimized window should
// return to maximized state in WINDOWPLACEMENT.
fn minimized_restore_command(
    restore_maximized: bool,
) -> windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD {
    if restore_maximized {
        SW_SHOWMAXIMIZED
    } else {
        SW_RESTORE
    }
}

fn restore_to_foreground(window: &Window) {
    match window.hwnd() {
        Ok(hwnd) => unsafe {
            let was_minimized = IsIconic(hwnd).as_bool();
            let restore_maximized = if was_minimized {
                let mut placement = WINDOWPLACEMENT {
                    length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                    ..Default::default()
                };
                GetWindowPlacement(hwnd, &mut placement).is_ok()
                    && placement.flags.contains(WPF_RESTORETOMAXIMIZED)
            } else {
                false
            };

            let _ = window.show();
            if was_minimized {
                let _ = ShowWindow(hwnd, minimized_restore_command(restore_maximized));
            }
            let _ = SetForegroundWindow(hwnd);
        },
        Err(error) => {
            eprintln!("[gpt-wrapper] hwnd unavailable: {error:?}");
            let _ = window.show();
            if window.is_minimized().unwrap_or(false) {
                let _ = window.unminimize();
            }
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
    background_project_switcher::dismiss(app_handle);
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

// A regular second launch should foreground the existing instance instead of
// initializing another tray icon, BLE client, shortcut registry, or set of tabs.
fn second_instance_requests_foreground(args: &[String]) -> bool {
    !args.iter().skip(1).any(|arg| arg == AUTOSTART_ARG)
}

fn activate_existing_main_window(app_handle: &AppHandle) {
    if app_handle
        .state::<TrayVisibilityState>()
        .tray_hidden
        .load(Ordering::SeqCst)
    {
        restore_main_window_from_tray(app_handle);
    } else if let Some(window) = app_handle.get_window(MAIN_WINDOW_LABEL) {
        restore_to_foreground(&window);
        focus_active_tab(app_handle);
    } else {
        eprintln!("[gpt-wrapper] second launch: main window unavailable");
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
                background_project_switcher::dismiss(app_handle);
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
                let mut native_sent = false;
                if let Err(error) = result {
                    eprintln!("[gpt-wrapper] button lookup script failed: {error:?}");
                } else { match serde_json::from_str::<Option<Rect>>(&raw) {
                    Ok(Some(rect)) => {
                        match &guard {
                            Some(ClickGuard::Start(tracker, app, tab_id, generation)) => {
                                if !tracker.is_current(*tab_id, *generation) {
                                    return Ok(());
                                }
                                // Settings can change in the 180 ms between
                                // scheduling Start and dispatching the click.
                                if let Err(reason) = microphone_inventory::ptt_start_guard(app, *tab_id) {
                                    tracker.clear_tab(*tab_id);
                                    microphone_inventory::show_ptt_error(app, Some(*tab_id), &reason);
                                    return Ok(());
                                }
                                if !tracker.mark_start_clicked(*tab_id, *generation) {
                                    return Ok(());
                                }
                            }
                            Some(ClickGuard::Finish(tracker, _, _, generation, _, _))
                                if tracker.finish_in_flight.load(Ordering::SeqCst) != *generation =>
                            {
                                return Ok(())
                            }
                            Some(ClickGuard::Cancel(tracker, cancelled_generation))
                                if tracker.is_held()
                                    || tracker.generation.load(Ordering::SeqCst) != *cancelled_generation
                                    || tracker.finish_in_flight.load(Ordering::SeqCst) != 0 =>
                            {
                                return Ok(())
                            }
                            _ => {}
                        }
                        println!(
                            "[gpt-wrapper] trusted click dispatch requested {:?} at ({:.1}, {:.1}); recording/delivery unverified",
                            target, rect.x, rect.y
                        );
                        native_sent = rect.native_send;
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
                    Some(ClickGuard::Finish(tracker, app, tab_id, generation, auto_submit, submit_pending)) => {
                        if clicked {
                            verify_finish_stopped(app, tracker, tab_id, generation, auto_submit, submit_pending, native_sent, 0);
                        } else {
                            complete_finish(app, tracker, tab_id, generation, auto_submit, submit_pending, false, false);
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

// PTT ends recording first. Only the setting or an independent, explicit
// Submit signal may request a follow-up composer Send. A native send-on-finish
// button has already done that job and must never cause a second click.
fn should_send_after_finish(
    auto_submit: bool,
    submit_pending: bool,
    native_sent: bool,
    disconnected: bool,
) -> bool {
    !disconnected && !native_sent && (auto_submit || submit_pending)
}

fn complete_finish(
    app: AppHandle,
    tracker: Arc<PttTracker>,
    tab_id: usize,
    generation: usize,
    auto_submit: bool,
    submit_pending: bool,
    stopped: bool,
    native_sent: bool,
) {
    let Some(events) = tracker.finish_completed(generation) else {
        return;
    };
    if !stopped {
        if events.contains(&PttEvent::CancelBle) {
            let _ = tracker.cancel_ble();
        }
        tracker.clear_tab(tab_id);
        tracker.mark_recovery_needed(tab_id);
        if let Some(tab) = get_tab_webview(&app, tab_id) {
            let _ = tab.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
        }
        report_ptt_recovery(
            &app,
            tab_id,
            generation,
            "無法確認聽寫已結束；下次按 PTT 將自動檢查狀態。",
        );
        return;
    }
    let disconnected = events.contains(&PttEvent::CancelBle);
    tracker.clear_recovery_needed(tab_id);
    if let Some(tab) = get_tab_webview(&app, tab_id) {
        if disconnected {
            let _ = tab.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
        } else {
            let script = format!("window.__GPT_WRAPPER__?.finishPttSegment?.({native_sent});");
            let _ = tab.eval(&script);
            if submit_pending {
                tracker.note_ble_send(tab_id);
            }
            if should_send_after_finish(auto_submit, submit_pending, native_sent, disconnected) {
                request_explicit_send(&app, tab_id);
            }
        }
    }
    for event in events {
        // A second Submit pressed while native Finish was still resolving
        // belongs to the same already-sent dictation, not a second message.
        if native_sent && matches!(event, PttEvent::SubmitBle | PttEvent::SubmitKeyboard(_)) {
            continue;
        }
        handle_ptt_event(&app, event, &tracker);
    }
}

fn fail_start(app: AppHandle, tracker: Arc<PttTracker>, tab_id: usize, generation: usize) {
    if tracker.start_completed(generation).is_none() {
        return;
    }
    tracker.clear_tab(tab_id);
    tracker.mark_recovery_needed(tab_id);
    if let Some(tab) = get_tab_webview(&app, tab_id) {
        let _ = tab.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
    }
    report_ptt_recovery(
        &app,
        tab_id,
        generation,
        "無法確認聽寫已開始；下次按 PTT 將自動檢查狀態。",
    );
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
    let script = HSTRING::from(
        DictationTarget::Finish {
            allow_native_send: true,
        }
        .rect_script(),
    );
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
    auto_submit: bool,
    submit_pending: bool,
    native_sent: bool,
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
            auto_submit,
            submit_pending,
            false,
            native_sent,
        );
        return;
    }
    let Some(tab) = get_tab_webview(&app, tab_id) else {
        complete_finish(
            app,
            tracker,
            tab_id,
            generation,
            auto_submit,
            submit_pending,
            false,
            native_sent,
        );
        return;
    };
    // Check every known recording completion control, not just the selected
    // action, so a remaining Send/Insert choice isn't mistaken for completion.
    let script = HSTRING::from(
        DictationTarget::Finish {
            allow_native_send: true,
        }
        .rect_script(),
    );
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
                    auto_submit,
                    submit_pending,
                    true,
                    native_sent,
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
                            auto_submit,
                            submit_pending,
                            native_sent,
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
                tracker.clear_tab(tab_id);
                tracker.mark_recovery_needed(tab_id);
                if let Some(tab) = get_tab_webview(&app, tab_id) {
                    let _ = tab.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
                }
                report_ptt_recovery(
                    &app,
                    tab_id,
                    generation,
                    "停止聽寫逾時；下次按 PTT 將自動檢查狀態。",
                );
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
    if let Err(reason) = microphone_inventory::ptt_start_guard(webview.app_handle(), tab_id) {
        tracker.clear_tab(tab_id);
        microphone_inventory::show_ptt_error(webview.app_handle(), Some(tab_id), &reason);
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

// Reuse the exact Start/Finish selectors for the recovery probe. Only the
// safe (nativeSend=false) Finish can be clicked during recovery.
fn recovery_probe_script() -> String {
    format!(
        r#"(() => {{
  const start = {};
  const finish = {};
  if (finish) return {{kind: finish.nativeSend ? "unsafe" : "safe", x: finish.x, y: finish.y}};
  if (start) return {{kind: "idle"}};
  return {{kind: "unknown"}};
}})()"#,
        DictationTarget::Start.rect_script(),
        DictationTarget::Finish {
            allow_native_send: true
        }
        .rect_script()
    )
}

fn report_ptt_recovery(app: &AppHandle, tab_id: usize, generation: usize, message: &str) {
    eprintln!("[gpt-wrapper] PTT recovery tab {tab_id} generation {generation}: {message}");
    if let Some(tab) = get_tab_webview(app, tab_id) {
        let text = serde_json::to_string(message).unwrap_or_default();
        let _ = tab.eval(&format!(
            "window.__GPT_WRAPPER__?.showRecoveryStatus?.({text});"
        ));
    }
}

fn abort_ptt_recovery(
    app: &AppHandle,
    tracker: &Arc<PttTracker>,
    tab_id: usize,
    generation: usize,
    reason: &str,
) {
    if !tracker.abort_recovery(tab_id, generation) {
        return;
    }
    if let Some(tab) = get_tab_webview(app, tab_id) {
        let _ = tab.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
    }
    report_ptt_recovery(app, tab_id, generation, reason);
}

fn probe_ptt_recovery(
    app: AppHandle,
    tracker: Arc<PttTracker>,
    tab_id: usize,
    generation: usize,
    attempt: u8,
    safe_stop_clicked: bool,
) {
    if !tracker.is_recovering(tab_id, generation) {
        return;
    }
    if attempt >= 25 {
        abort_ptt_recovery(
            &app,
            &tracker,
            tab_id,
            generation,
            "聽寫狀態仍不確定，請再次按下 PTT 重試。",
        );
        return;
    }
    let Some(tab) = get_tab_webview(&app, tab_id).filter(|_| is_tab_loaded(&app, tab_id)) else {
        abort_ptt_recovery(
            &app,
            &tracker,
            tab_id,
            generation,
            "ChatGPT 分頁尚未準備好，請再試一次。",
        );
        return;
    };
    let script = HSTRING::from(recovery_probe_script());
    let result = tab.with_webview(move |platform| {
        let Ok(core) = (unsafe { platform.controller().CoreWebView2() }) else {
            abort_ptt_recovery(&app, &tracker, tab_id, generation, "無法讀取 ChatGPT 聽寫狀態，請再試一次。");
            return;
        };
        let click_core = core.clone();
        let app_for_callback = app.clone();
        let tracker_for_callback = Arc::clone(&tracker);
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |result, raw| {
            let app = app_for_callback.clone();
            let tracker = Arc::clone(&tracker_for_callback);
            if !tracker.is_recovering(tab_id, generation) {
                return Ok(());
            }
            let value = if result.is_ok() {
                serde_json::from_str::<serde_json::Value>(&raw).ok()
            } else {
                None
            };
            let kind = value.as_ref().and_then(|v| v.get("kind")).and_then(|v| v.as_str());
            match kind {
                Some("idle") => {
                    // A safe Stop only ends the old segment. Never start a new
                    // recording from an old/released recovery press.
                    let start_now = tracker.complete_recovery(tab_id, generation, !safe_stop_clicked);
                    if start_now {
                        if let Some(webview) = get_tab_webview(&app, tab_id) {
                            schedule_ptt_start(webview, Arc::clone(&tracker), tab_id, generation, true);
                        }
                    } else if safe_stop_clicked {
                        report_ptt_recovery(&app, tab_id, generation, "已安全結束上一段聽寫，請再次按下 PTT。");
                    }
                    println!("[gpt-wrapper] PTT recovery idle tab {tab_id} generation {generation}; start={start_now}");
                }
                Some("safe") if !safe_stop_clicked => {
                    let x = value.as_ref().and_then(|v| v.get("x")).and_then(|v| v.as_f64());
                    let y = value.as_ref().and_then(|v| v.get("y")).and_then(|v| v.as_f64());
                    if let (Some(x), Some(y)) = (x, y) {
                        if tracker.is_recovering(tab_id, generation) {
                            // Trusted click only on a known transcription-only Finish.
                            let rect = Rect { x, y, native_send: false };
                            dispatch_trusted_click(&click_core, rect);
                        }
                    } else {
                        abort_ptt_recovery(&app, &tracker, tab_id, generation, "無法定位停止聽寫按鈕，請再試一次。");
                        return Ok(());
                    }
                    let app = app.clone();
                    let tracker = Arc::clone(&tracker);
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(150));
                        probe_ptt_recovery(app, tracker, tab_id, generation, attempt + 1, true);
                    });
                }
                Some("unsafe") => {
                    abort_ptt_recovery(&app, &tracker, tab_id, generation,
                        "目前只找到會送出訊息的按鈕，請手動停止聽寫後再按 PTT。");
                }
                _ => {
                    let app = app.clone();
                    let tracker = Arc::clone(&tracker);
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(120));
                        probe_ptt_recovery(app, tracker, tab_id, generation, attempt + 1, safe_stop_clicked);
                    });
                }
            }
            Ok(())
        }));
        if let Err(error) = unsafe { core.ExecuteScript(&script, &handler) } {
            eprintln!("[gpt-wrapper] recovery probe ExecuteScript failed: {error:?}");
            abort_ptt_recovery(&app, &tracker, tab_id, generation, "讀取聽寫狀態失敗，請再試一次。");
        }
    });
    if let Err(error) = result {
        eprintln!("[gpt-wrapper] recovery WebView unavailable: {error:?}");
    }
}

// Both the BLE Submit signal and the Ctrl+. shortcut invoke this same WebView action.
// The DOM adapter checks the draft, dictation state, Send button and duplicate requests.
fn request_explicit_send(app_handle: &AppHandle, tab_id: usize) {
    if !is_tab_loaded(app_handle, tab_id) {
        eprintln!("[gpt-wrapper] Submit target tab {tab_id} is not loaded");
        return;
    }
    match get_tab_webview(app_handle, tab_id) {
        Some(webview) => {
            if let Err(error) = webview.eval("window.__GPT_WRAPPER__?.requestExplicitSend?.();") {
                eprintln!("[gpt-wrapper] Submit script failed on tab {tab_id}: {error}");
            }
        }
        None => eprintln!("[gpt-wrapper] Submit target tab {tab_id} is unavailable"),
    }
}

// BLE transport dispatches control events, never synthetic keyboard shortcuts.
fn handle_ptt_event(app_handle: &AppHandle, event: PttEvent, tracker: &Arc<PttTracker>) {
    // Tray-only Submit must never borrow a pending PTT target or a deferred Send.
    // Run before the foreground defer/target routing; this path needs no active tab.
    if app_handle
        .state::<TrayVisibilityState>()
        .tray_hidden
        .load(Ordering::SeqCst)
        && matches!(event, PttEvent::SubmitKeyboard(_) | PttEvent::SubmitBle)
    {
        if tracker.safe_for_background_switch() {
            background_project_switcher::press(app_handle);
        } else {
            eprintln!("[gpt-wrapper] background Project switch rejected: PTT/Finish still active");
        }
        return;
    }
    if tracker.defer_during_finish(event) {
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
                background_project_switcher::dismiss(app_handle);
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
            if tracker.recovery_needed(tab_id) {
                let Some(generation) = tracker.begin_recovery(source, tab_id) else {
                    return;
                };
                println!("[gpt-wrapper] PTT recovery probe tab {tab_id} generation {generation}");
                probe_ptt_recovery(
                    app_handle.clone(),
                    Arc::clone(tracker),
                    tab_id,
                    generation,
                    0,
                    false,
                );
                let app = app_handle.clone();
                let tracker = Arc::clone(tracker);
                thread::spawn(move || {
                    thread::sleep(Duration::from_secs(5));
                    abort_ptt_recovery(
                        &app,
                        &tracker,
                        tab_id,
                        generation,
                        "聽寫狀態檢查逾時，請再次按下 PTT 重試。",
                    );
                });
                return;
            }
            if let Err(reason) = microphone_inventory::ptt_start_guard(app_handle, tab_id) {
                microphone_inventory::show_ptt_error(app_handle, Some(tab_id), &reason);
                return;
            }
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

            // Read the setting for both keyboard and BLE releases, using the
            // state at release time. A queued BLE Submit is an explicit intent.
            let auto_submit = app_handle
                .state::<GeneralSettingsState>()
                .ptt_auto_submit()
                .unwrap_or(false);
            let allow_native_send = auto_submit || finish.submit_pending;
            println!(
                "[gpt-wrapper] hotkey released on tab {}; auto submit: {auto_submit}",
                finish.tab_id
            );
            tracker.finish_tab_id.store(finish.tab_id, Ordering::SeqCst);
            tracker
                .finish_in_flight
                .store(finish.generation, Ordering::SeqCst);
            trusted_click_guarded(
                &webview,
                DictationTarget::Finish { allow_native_send },
                Some(ClickGuard::Finish(
                    Arc::clone(tracker),
                    app_handle.clone(),
                    finish.tab_id,
                    finish.generation,
                    auto_submit,
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
        PttEvent::SubmitBle => {
            match tracker.request_ble_submit() {
                BleSubmitTarget::Queued => println!("[gpt-wrapper] BLE Submit queued for held PTT"),
                BleSubmitTarget::Busy => {
                    eprintln!("[gpt-wrapper] BLE Submit refused while keyboard PTT is held")
                }
                BleSubmitTarget::Tab(tab_id) => {
                    if tracker.recovery_needed(tab_id) {
                        eprintln!("[gpt-wrapper] BLE Submit blocked on uncertain tab {tab_id}; retry PTT recovery");
                    } else {
                        request_explicit_send(app_handle, tab_id);
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
                        if is_tab_loaded(app_handle, tab_id)
                            && get_tab_webview(app_handle, tab_id).is_some()
                        {
                            if tracker.recovery_needed(tab_id) {
                                eprintln!("[gpt-wrapper] BLE Submit blocked on uncertain active tab {tab_id}");
                            } else {
                                tracker.note_ble_send(tab_id);
                                request_explicit_send(app_handle, tab_id);
                            }
                        }
                    }
                }
            }
        }
        PttEvent::SubmitKeyboard(tab_id) => {
            if tracker.is_held() {
                eprintln!(
                    "[gpt-wrapper] Ctrl+. Submit refused while PTT is held; release PTT first"
                );
            } else if let Some(tab_id) = tab_id {
                if tracker.recovery_needed(tab_id) {
                    eprintln!("[gpt-wrapper] Ctrl+. Submit blocked on uncertain tab {tab_id}; retry PTT recovery");
                } else {
                    request_explicit_send(app_handle, tab_id);
                }
            } else {
                eprintln!("[gpt-wrapper] foreground Submit refused: no active tab");
            }
        }
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
                            DictationTarget::Finish {
                                allow_native_send: false,
                            },
                            Some(ClickGuard::Finish(
                                Arc::clone(tracker),
                                app_handle.clone(),
                                cancel.tab_id,
                                cancel.generation,
                                false,
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

struct KeyboardShortcutState {
    update_lock: Mutex<()>,
    ptt_keys_down: Arc<Mutex<HashSet<u32>>>,
    submit_keys_down: Arc<Mutex<HashSet<u32>>>,
}

impl KeyboardShortcutState {
    fn new() -> Self {
        Self {
            update_lock: Mutex::new(()),
            ptt_keys_down: Arc::new(Mutex::new(HashSet::new())),
            submit_keys_down: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn reset(&self) {
        if let Ok(mut held) = self.ptt_keys_down.lock() {
            held.clear();
        }
        if let Ok(mut held) = self.submit_keys_down.lock() {
            held.clear();
        }
    }
}

// Recording a combination inside Settings must not trigger an existing PTT/Submit binding.
fn settings_has_focus(app: &AppHandle) -> bool {
    app.get_webview_window(SETTINGS_WINDOW_LABEL)
        .and_then(|window| window.is_focused().ok())
        .unwrap_or(false)
}

fn register_keyboard_shortcuts(
    app: &AppHandle,
    settings: &ShortcutSettings,
    tracker: &Arc<PttTracker>,
    keyboard: &KeyboardShortcutState,
) -> Result<(), String> {
    let (ptt, submit) = parse_and_validate(settings)?;
    for (index, shortcut) in ptt.into_iter().enumerate() {
        let held = Arc::clone(&keyboard.ptt_keys_down);
        let tracker = Arc::clone(tracker);
        app.global_shortcut()
            .on_shortcut(shortcut, move |app, shortcut, event| {
                if event.state() == ShortcutState::Pressed && settings_has_focus(app) {
                    return;
                }
                let Ok(mut pressed) = held.lock() else {
                    return;
                };
                let should_dispatch = match event.state() {
                    ShortcutState::Pressed => {
                        if !pressed.insert(shortcut.id()) {
                            false
                        } else {
                            pressed.len() == 1
                        }
                    }
                    ShortcutState::Released => pressed.remove(&shortcut.id()) && pressed.is_empty(),
                };
                drop(pressed);
                if should_dispatch {
                    let action = match event.state() {
                        ShortcutState::Pressed => PttEvent::Down(PttSource::Keyboard),
                        ShortcutState::Released => PttEvent::Up(PttSource::Keyboard),
                    };
                    handle_ptt_event(app, action, &tracker);
                }
            })
            .map_err(|error| {
                format!(
                    "無法註冊 PTT 快捷鍵 {}（{}）：{error}",
                    index + 1,
                    settings.ptt[index]
                )
            })?;
    }

    for (index, shortcut) in submit.into_iter().enumerate() {
        let tracker = Arc::clone(tracker);
        let held = Arc::clone(&keyboard.submit_keys_down);
        app.global_shortcut()
            .on_shortcut(shortcut, move |app, shortcut, event| {
                if event.state() == ShortcutState::Pressed && settings_has_focus(app) {
                    return;
                }
                let Ok(mut pressed) = held.lock() else {
                    return;
                };
                if event.state() == ShortcutState::Released {
                    pressed.remove(&shortcut.id());
                    return;
                }
                if !pressed.insert(shortcut.id()) {
                    return; // Ignore auto-repeat for this specific physical shortcut.
                }
                drop(pressed);
                let hidden = app
                    .state::<TrayVisibilityState>()
                    .tray_hidden
                    .load(Ordering::SeqCst);
                let tab_id = if hidden { None } else { active_tab_id(app) };
                handle_ptt_event(app, PttEvent::SubmitKeyboard(tab_id), &tracker);
            })
            .map_err(|error| {
                format!(
                    "無法註冊 Submit 快捷鍵 {}（{}）：{error}",
                    index + 1,
                    settings.submit[index]
                )
            })?;
    }
    Ok(())
}

#[tauri::command]
fn get_shortcut_settings(app: AppHandle, webview: Webview) -> Result<ShortcutSettings, String> {
    ensure_settings_webview(&webview)?;
    app.state::<ShortcutSettingsState>().get()
}

#[tauri::command]
fn set_shortcut_settings(
    app: AppHandle,
    webview: Webview,
    settings: ShortcutSettings,
) -> Result<ShortcutSettings, String> {
    ensure_settings_webview(&webview)?;
    parse_and_validate(&settings)?;
    let keyboard = app.state::<KeyboardShortcutState>();
    let _guard = keyboard
        .update_lock
        .lock()
        .map_err(|_| "shortcut update lock poisoned")?;
    let tracker = &app.state::<PttState>().0;
    if tracker.is_held() {
        return Err("正在使用 PTT 聽寫，請放開快捷鍵後再儲存。".into());
    }

    let state = app.state::<ShortcutSettingsState>();
    let previous = state.get()?;
    if previous == settings {
        return Ok(previous);
    }
    let manager = app.global_shortcut();
    manager
        .unregister_all()
        .map_err(|e| format!("無法更新快捷鍵：{e}"))?;
    keyboard.reset();
    if let Err(error) = register_keyboard_shortcuts(&app, &settings, tracker, &keyboard) {
        let _ = manager.unregister_all();
        keyboard.reset();
        if let Err(rollback) = register_keyboard_shortcuts(&app, &previous, tracker, &keyboard) {
            return Err(format!("{error}；恢復原有快捷鍵也失敗：{rollback}"));
        }
        return Err(format!("{error}；已恢復原有快捷鍵。"));
    }

    if let Err(error) = state.save(settings.clone()) {
        let _ = manager.unregister_all();
        keyboard.reset();
        if let Err(rollback) = register_keyboard_shortcuts(&app, &previous, tracker, &keyboard) {
            return Err(format!("{error}；恢復原有快捷鍵失敗：{rollback}"));
        }
        return Err(error);
    }
    Ok(settings)
}

fn main() {
    // A second launch can arrive before setup has built the main window.
    // Keep a pending activation until the first instance is ready.
    let pending_activation = Arc::new(AtomicBool::new(false));
    let pending_for_plugin = Arc::clone(&pending_activation);
    let pending_for_setup = Arc::clone(&pending_activation);

    tauri::Builder::default()
        // Must be the first plugin so second instances stop before other setup.
        .plugin(tauri_plugin_single_instance::init(
            move |app, args, _cwd| {
                if !second_instance_requests_foreground(&args) {
                    return; // --autostart remains silent even if already running.
                }
                pending_for_plugin.store(true, Ordering::SeqCst);
                let pending = Arc::clone(&pending_for_plugin);
                let app_for_activation = app.clone();
                if let Err(error) = app.run_on_main_thread(move || {
                    if app_for_activation.get_window(MAIN_WINDOW_LABEL).is_some()
                        && pending.swap(false, Ordering::SeqCst)
                    {
                        activate_existing_main_window(&app_for_activation);
                    }
                }) {
                    eprintln!("[gpt-wrapper] could not dispatch second launch: {error:?}");
                }
            },
        ))
        .invoke_handler(tauri::generate_handler![
            get_tabs,
            create_tab,
            create_project_tab,
            show_project_bookmark_menu,
            switch_tab,
            reorder_tabs,
            close_tab,
            dismiss_tab_completion_notification,
            get_autostart_enabled,
            set_autostart_enabled,
            get_ptt_auto_submit_enabled,
            set_ptt_auto_submit_enabled,
            get_shortcut_settings,
            set_shortcut_settings,
            microphone_inventory::get_preferred_microphone,
            microphone_inventory::set_preferred_microphone,
            microphone_inventory::list_microphone_inputs,
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
        .setup(move |app| {
            let launched_by_autostart = std::env::args().any(|arg| arg == AUTOSTART_ARG);

            app.manage(TabState::new());
            app.manage(microphone_inventory::InventoryBridge::new());
            app.manage(microphone_inventory::MicrophoneRouting::new());

            let quitting = Arc::new(AtomicBool::new(false));
            let ptt_tracker = Arc::new(PttTracker::new());
            app.manage(PttState(Arc::clone(&ptt_tracker)));
            app.manage(TrayVisibilityState::new());
            app.manage(background_project_switcher::BackgroundProjectSwitcherState::new());
            app.manage(tab_completion_notifications::NotificationState::new());
            app.manage(SettingsWindowState::new());
            let project_settings_path: PathBuf =
                app.path().app_config_dir()?.join("project-settings.json");
            app.manage(ProjectSettingsState::load(project_settings_path));
            let general_settings_path = app.path().app_config_dir()?.join("general-settings.json");
            app.manage(GeneralSettingsState::load(general_settings_path));
            let shortcut_settings_path =
                app.path().app_config_dir()?.join("shortcut-settings.json");
            app.manage(ShortcutSettingsState::load(shortcut_settings_path));
            app.manage(KeyboardShortcutState::new());
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
            let tracker_for_events = Arc::clone(&ptt_tracker);
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
                        // Foreground Submit events deferred during Finish must not become switch presses.
                        tracker_for_events.clear_deferred_submits();
                        background_project_switcher::dismiss(&app_handle_for_events);

                        if let Err(error) = window_for_events.hide() {
                            eprintln!("[gpt-wrapper] failed to hide main window: {error:?}");
                        }

                        if let Err(error) = ensure_standby_ptt_tab(&app_handle_for_events) {
                            eprintln!("[gpt-wrapper] failed to prepare standby PTT tab: {error}");
                        }
                    }
                }
                WindowEvent::Focused(true) => {
                    tab_completion_notifications::clear_all(&app_handle_for_events);
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

            let settings = app
                .state::<ShortcutSettingsState>()
                .get()
                .map_err(tauri::Error::AssetNotFound)?;
            let keyboard = app.state::<KeyboardShortcutState>();
            register_keyboard_shortcuts(app.handle(), &settings, &ptt_tracker, &keyboard)
                .map_err(tauri::Error::AssetNotFound)?;
            println!("[gpt-wrapper] registered configured keyboard shortcuts.");

            if !launched_by_autostart {
                window.set_focus()?;
                focus_active_tab(app.handle());
            }

            // Handle an ordinary second launch that arrived before setup finished.
            if pending_for_setup.swap(false, Ordering::SeqCst) {
                activate_existing_main_window(app.handle());
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
    fn standby_same_url_adopts_full_spec_and_resets_prefix_without_navigation() {
        let mut spec = ProjectSettings::default().default_tab_spec();
        spec.project_id = Some(Uuid::new_v4());
        spec.message_prefix = "old".into();
        let mut context = TabContext::new(spec.clone());
        let mut navigation = standby_navigation::Navigation::new(spec.url.clone());
        assert!(navigation.started(1, &spec.url));
        assert!(navigation.finished(1, &spec.url, true));
        context.navigation = Some(navigation);
        context.prefix_applied = true;
        context.prefix_in_progress = true;
        spec.project_id = Some(Uuid::new_v4());
        spec.title = "Updated".into();
        spec.message_prefix = "new".into();
        assert_eq!(context.adopt_standby_spec(spec.clone()), Ok((false, true)));
        assert_eq!(context.spec, spec);
        assert!(!context.prefix_applied);
        assert!(!context.prefix_in_progress);
        assert!(!context.background_ptt_pending);
    }

    #[test]
    fn standby_spec_change_during_loading_never_reuses_old_prefix_or_ready() {
        let spec = ProjectSettings::default().default_tab_spec();
        let mut context = TabContext::new(spec.clone());
        let mut navigation = standby_navigation::Navigation::new(spec.url.clone());
        assert!(navigation.started(1, &spec.url));
        context.navigation = Some(navigation);
        context.prefix_applied = true;
        let mut target = spec.clone();
        target.url = url::Url::parse("https://chatgpt.com/g/new/project").unwrap();
        target.message_prefix = "new".into();
        assert_eq!(
            context.adopt_standby_spec(target.clone()),
            Ok((true, false))
        );
        let navigation = context.navigation.as_mut().unwrap();
        assert!(!navigation.finished(1, &spec.url, true));
        assert!(!navigation.ready());
        assert_eq!(context.spec, target);
        assert!(!context.prefix_applied);
    }

    #[test]
    fn repeat_launch_ignores_autostart_but_foregrounds_manual_launch() {
        assert!(second_instance_requests_foreground(&[
            "gpt-wrapper.exe".into()
        ]));
        assert!(second_instance_requests_foreground(&[
            "gpt-wrapper.exe".into(),
            "--other-flag".into(),
        ]));
        assert!(!second_instance_requests_foreground(&[
            "gpt-wrapper.exe".into(),
            "--autostart".into(),
        ]));
        assert!(!second_instance_requests_foreground(&[
            "gpt-wrapper.exe".into(),
            "--other-flag".into(),
            "--autostart".into(),
        ]));
    }

    #[test]
    fn minimized_window_restores_its_previous_maximized_state() {
        assert_eq!(minimized_restore_command(false), SW_RESTORE);
        assert_eq!(minimized_restore_command(true), SW_SHOWMAXIMIZED);
    }

    #[test]
    fn finish_selectors_never_offer_native_send_when_auto_submit_is_off() {
        let insert_only = DictationTarget::Finish {
            allow_native_send: false,
        }
        .rect_script();
        assert!(insert_only.contains("停止聽寫"));
        assert!(insert_only.contains("Stop dictation"));
        assert!(insert_only.contains("Submit dictation"));
        assert!(!insert_only.contains("Transcribe and send"));
        assert!(!insert_only.contains("轉錄並傳送"));
        let with_fallback = DictationTarget::Finish {
            allow_native_send: true,
        }
        .rect_script();
        assert!(with_fallback.contains("Transcribe and send"));
        let native: Rect = serde_json::from_str(r#"{"x":1,"y":2,"nativeSend":true}"#).unwrap();
        assert!(native.native_send);
    }

    #[test]
    fn keyboard_and_ble_share_ptt_send_policy() {
        // Both sources must use this same policy; the explicit Submit button
        // remains independent of Settings, including when pressed during PTT.
        assert!(!should_send_after_finish(false, false, false, false));
        assert!(should_send_after_finish(true, false, false, false));
        assert!(should_send_after_finish(false, true, false, false));
        assert!(!should_send_after_finish(true, true, true, false));
        assert!(!should_send_after_finish(true, true, false, true));
    }

    #[test]
    fn background_switch_only_runs_when_ptt_and_finish_are_idle() {
        let ptt = PttTracker::new();
        assert!(ptt.safe_for_background_switch());
        let generation = ptt.begin(PttSource::Ble, 7).unwrap();
        assert!(!ptt.safe_for_background_switch());
        ptt.ble_submit_pending.store(true, Ordering::SeqCst);
        assert!(!ptt.safe_for_background_switch());
        ptt.ble_submit_pending.store(false, Ordering::SeqCst);
        ptt.finish(PttSource::Ble);
        ptt.finish_in_flight.store(generation, Ordering::SeqCst);
        assert!(!ptt.safe_for_background_switch());
        ptt.finish_in_flight.store(0, Ordering::SeqCst);
        assert!(ptt.safe_for_background_switch());
        ptt.mark_recovery_needed(7);
        assert!(
            ptt.safe_for_background_switch(),
            "historical failure must not block tray project switching"
        );
    }

    #[test]
    fn recovery_is_per_tab_and_never_locks_unrelated_tabs_or_background_switch() {
        let ptt = PttTracker::new();
        ptt.mark_recovery_needed(7);
        ptt.mark_recovery_needed(9);
        assert!(ptt.recovery_needed(7));
        assert!(ptt.recovery_needed(9));
        assert!(!ptt.recovery_needed(11));
        assert!(ptt.safe_for_background_switch());
        assert!(ptt.begin(PttSource::Ble, 11).is_some());
        assert!(ptt.is_held());
        ptt.clear_tab(11);
        assert!(ptt.safe_for_background_switch());
        ptt.clear_tab(9);
        assert!(ptt.recovery_needed(7));
        assert!(!ptt.recovery_needed(9));
        assert!(!ptt.is_held());
    }

    #[test]
    fn recovered_idle_resumes_only_a_still_held_ptt_press() {
        let ptt = PttTracker::new();
        ptt.mark_recovery_needed(2);
        let generation = ptt.begin_recovery(PttSource::Keyboard, 2).unwrap();
        assert!(!ptt.safe_for_background_switch());
        assert!(ptt.complete_recovery(2, generation, true));
        assert!(!ptt.recovery_needed(2));
        assert!(ptt.is_current(2, generation));
        assert_eq!(ptt.start_in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(ptt.recovery_in_flight.load(Ordering::SeqCst), 0);
        ptt.clear_tab(2);

        ptt.mark_recovery_needed(2);
        let second = ptt.begin_recovery(PttSource::Keyboard, 2).unwrap();
        assert!(ptt.defer_during_finish(PttEvent::Up(PttSource::Keyboard)));
        assert!(!ptt.complete_recovery(2, second, true));
        assert!(!ptt.recovery_needed(2), "verified idle clears uncertainty");
        assert!(!ptt.is_held(), "released PTT must not produce a late Start");
        assert_eq!(ptt.start_in_flight.load(Ordering::SeqCst), 0);
        assert!(ptt.safe_for_background_switch());
    }

    #[test]
    fn safe_recovery_stop_never_restarts_or_replays_submit() {
        let ptt = PttTracker::new();
        ptt.mark_recovery_needed(4);
        let generation = ptt.begin_recovery(PttSource::Ble, 4).unwrap();
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::Queued);
        assert!(!ptt.complete_recovery(4, generation, false));
        assert!(!ptt.is_held());
        assert!(!ptt.recovery_needed(4));
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::None);
        assert!(ptt.safe_for_background_switch());
        assert!(!ptt.abort_recovery(4, generation), "late watchdog ignored");
    }

    #[test]
    fn recovery_abort_is_bounded_and_old_generation_cannot_clear_new_attempt() {
        let ptt = PttTracker::new();
        ptt.mark_recovery_needed(3);
        let generation = ptt.begin_recovery(PttSource::Ble, 3).unwrap();
        assert!(ptt.defer_during_finish(PttEvent::CancelBle));
        assert!(ptt.abort_recovery(3, generation));
        assert!(ptt.recovery_needed(3));
        assert!(!ptt.is_held());
        assert_eq!(ptt.start_in_flight.load(Ordering::SeqCst), 0);
        assert!(ptt.safe_for_background_switch());
        let next = ptt.begin_recovery(PttSource::Keyboard, 3).unwrap();
        assert_ne!(generation, next);
        assert!(!ptt.abort_recovery(3, generation));
        assert!(ptt.is_recovering(3, next));
        ptt.clear_tab(3);
        assert!(!ptt.is_recovering(3, next));
        assert!(!ptt.is_held());
    }

    #[test]
    fn recovery_probe_uses_safe_finish_before_native_send() {
        let script = recovery_probe_script();
        assert!(script.contains("停止聽寫"));
        assert!(script.contains("Stop dictation"));
        assert!(script.contains("Transcribe and send"));
        assert!(script.contains("nativeSend"));
        assert!(script.contains("finish.nativeSend ? \"unsafe\" : \"safe\""));
    }

    #[test]
    fn entering_tray_discards_deferred_submit_but_preserves_ptt_edges() {
        let ptt = PttTracker::new();
        let generation = ptt.begin(PttSource::Keyboard, 4).unwrap();
        ptt.finish(PttSource::Keyboard);
        ptt.finish_in_flight.store(generation, Ordering::SeqCst);
        assert!(ptt.defer_during_finish(PttEvent::SubmitKeyboard(Some(4))));
        assert!(ptt.defer_during_finish(PttEvent::SubmitBle));
        assert!(ptt.defer_during_finish(PttEvent::Down(PttSource::Ble)));
        ptt.clear_deferred_submits();
        assert_eq!(
            ptt.finish_completed(generation)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![PttEvent::Down(PttSource::Ble)]
        );
    }

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

        ptt.note_ble_send(11);
        let keyboard_generation = ptt.begin(PttSource::Keyboard, 2).unwrap();
        ptt.start_in_flight
            .store(keyboard_generation, Ordering::SeqCst);
        assert!(!ptt.defer_during_finish(PttEvent::CancelBle));
        assert_eq!(ptt.cancel_ble().unwrap().tab_id, 11);
        assert!(
            ptt.is_held(),
            "BLE disconnect must not release keyboard PTT"
        );
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::Busy);
        assert!(ptt.start_completed(keyboard_generation).unwrap().is_empty());
        ptt.finish(PttSource::Keyboard);
        assert_eq!(ptt.request_ble_submit(), BleSubmitTarget::None);

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
    fn keyboard_submit_keeps_its_captured_tab_when_finish_is_pending() {
        let ptt = PttTracker::new();
        let generation = ptt.begin(PttSource::Keyboard, 4).unwrap();
        assert!(ptt.mark_start_clicked(4, generation));
        ptt.finish(PttSource::Keyboard).unwrap();
        ptt.finish_in_flight.store(generation, Ordering::SeqCst);
        assert!(ptt.defer_during_finish(PttEvent::SubmitKeyboard(Some(4))));
        assert!(ptt.defer_during_finish(PttEvent::CancelBle));
        assert_eq!(
            ptt.finish_completed(generation)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![PttEvent::SubmitKeyboard(Some(4)), PttEvent::CancelBle],
            "Ctrl+. retains the pressed tab and BLE cleanup cannot discard it",
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
    fn document_titles_update_matching_visible_tabs_only() {
        let mut tabs = TabManager::new();
        let spec = ProjectSettings::default().default_tab_spec();
        for id in [1, 2] {
            tabs.contexts.insert(id, TabContext::new(spec.clone()));
            tabs.tabs.push(TabInfo {
                id,
                title: project_settings::CHATGPT_TITLE.to_string(),
                status: TabStatus::Fallback,
            });
        }

        assert!(tabs.update_title(2, "Conversation A — ChatGPT"));
        assert_eq!(tabs.tabs[0].title, "ChatGPT");
        assert_eq!(tabs.tabs[1].title, "Conversation A — ChatGPT");
        assert!(!tabs.update_title(2, "Conversation A — ChatGPT"));
        assert!(tabs.update_title(2, "Conversation B — ChatGPT"));
        assert_eq!(tabs.tabs[1].title, "Conversation B — ChatGPT");

        assert!(tabs.update_title(2, "   "));
        assert_eq!(tabs.tabs[1].title, "ChatGPT");
        assert!(!tabs.update_title(2, ""));
        assert!(!tabs.update_title(99, "Missing tab"));
    }

    #[test]
    fn standby_title_is_retained_on_promotion() {
        let mut tabs = TabManager::new();
        let spec = ProjectSettings::default().default_tab_spec();
        let id = 3;
        let mut standby = TabContext::new(spec.clone());
        standby.navigation = Some(standby_navigation::Navigation::new(spec.url.clone()));
        tabs.contexts.insert(id, standby);

        assert!(!tabs.update_title(id, "Background conversation — ChatGPT"));
        assert!(tabs.tabs.is_empty());
        assert_eq!(
            tabs.contexts[&id].page_title,
            "Background conversation — ChatGPT"
        );

        // Promotion must use the stored document title rather than the Project name.
        let context = &tabs.contexts[&id];
        tabs.tabs.push(TabInfo {
            id,
            title: context.page_title.clone(),
            status: context.status,
        });
        assert_eq!(
            tabs.snapshot().tabs[0].title,
            "Background conversation — ChatGPT"
        );
        assert!(tabs.update_title(id, "Renamed conversation — ChatGPT"));
        assert_eq!(tabs.tabs[0].title, "Renamed conversation — ChatGPT");
    }

    #[test]
    fn standby_navigation_clears_previous_document_title() {
        let spec = ProjectSettings::default().default_tab_spec();
        let mut standby = TabContext::new(spec.clone());
        standby.navigation = Some(standby_navigation::Navigation::new(spec.url.clone()));
        standby.page_title = "Old project conversation — ChatGPT".to_string();

        // Reusing the same target keeps the current document title.
        let (navigate, _) = standby.adopt_standby_spec(spec.clone()).unwrap();
        assert!(!navigate);
        assert_eq!(standby.page_title, "Old project conversation — ChatGPT");

        // Switching the standby WebView to a new URL resets the title until loaded.
        let mut replacement = spec;
        replacement.url = url::Url::parse("https://chatgpt.com/g/another-project").unwrap();
        let (navigate, ready) = standby.adopt_standby_spec(replacement).unwrap();
        assert!(navigate);
        assert!(!ready);
        assert_eq!(standby.page_title, "ChatGPT");
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
