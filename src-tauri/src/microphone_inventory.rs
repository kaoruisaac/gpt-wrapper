//! Trusted ChatGPT microphone inventory, per-tab capture configuration and
//! per-tab policy synchronization. Audio is handled exclusively by ChatGPT/WebView2.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Mutex,
    },
    time::Duration,
};
use tauri::{AppHandle, Manager, Webview};
use uuid::Uuid;

use super::{ensure_settings_webview, trusted_chatgpt_source, TabState};
use crate::general_settings::{GeneralSettingsState, PreferredMicrophone};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InputDevice {
    pub device_id: String,
    pub group_id: String,
    pub label: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub devices: Vec<InputDevice>,
    pub status: &'static str,
    pub detail: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InventoryMessage {
    kind: String,
    request_id: String,
    #[serde(default)]
    devices: Vec<InputDevice>,
    #[serde(default)]
    error: Option<String>,
}

type Reply = mpsc::Sender<Result<Inventory, String>>;
pub struct InventoryBridge(Mutex<HashMap<String, (usize, Reply)>>);

// The epoch identifies the currently authorized choice. There is no global
// error lock: only acknowledgements and the latest per-tab attempt are shared.
pub struct MicrophoneRouting {
    epoch: AtomicUsize,
    acknowledged: Mutex<HashMap<usize, usize>>,
    latest_attempt: Mutex<HashMap<usize, String>>,
}
impl MicrophoneRouting {
    pub fn new() -> Self {
        Self {
            epoch: AtomicUsize::new(1),
            acknowledged: Mutex::new(HashMap::new()),
            latest_attempt: Mutex::new(HashMap::new()),
        }
    }
    fn reset_for_explicit_choice(&self) -> usize {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut ack) = self.acknowledged.lock() {
            ack.clear();
        }
        if let Ok(mut attempts) = self.latest_attempt.lock() {
            attempts.clear();
        }
        epoch
    }
    pub fn forget_tab(&self, tab_id: usize) {
        if let Ok(mut ack) = self.acknowledged.lock() {
            ack.remove(&tab_id);
        }
        if let Ok(mut attempts) = self.latest_attempt.lock() {
            attempts.remove(&tab_id);
        }
    }
    fn remember_attempt(&self, tab_id: usize, attempt: String) {
        if !attempt.is_empty() && attempt.len() <= 64 {
            if let Ok(mut attempts) = self.latest_attempt.lock() {
                attempts.insert(tab_id, attempt);
            }
        }
    }
    fn take_failed_attempt(&self, tab_id: usize, attempt: &str) -> bool {
        if attempt.is_empty() {
            return false;
        }
        self.latest_attempt
            .lock()
            .map(|mut attempts| {
                if attempts.get(&tab_id).map(String::as_str) == Some(attempt) {
                    attempts.remove(&tab_id);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RouteMessage {
    kind: String,
    event: String,
    epoch: usize,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    attempt: String,
    #[serde(default)]
    purpose: String,
}

fn error_message(reason: &str) -> &'static str {
    match reason {
        "unavailable" => "選定麥克風目前無法使用，可重試或至 Settings 檢查。",
        "permission" => "麥克風權限被拒絕，請檢查 ChatGPT 權限後重試。",
        "mismatch" => "實際音訊來源與選擇不同，已停止該次收音。",
        _ => "指定麥克風擷取失敗，下一次可直接重試。",
    }
}

pub fn handle_route_message(app: &AppHandle, tab_id: usize, source: &str, raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    if value.get("kind").and_then(|kind| kind.as_str()) != Some("gpt-wrapper-microphone-route") {
        return false;
    }
    if !trusted_chatgpt_source(source) {
        return true;
    }
    let Ok(message) = serde_json::from_value::<RouteMessage>(value) else {
        return true;
    };
    if message.kind != "gpt-wrapper-microphone-route" {
        return true;
    }
    let state = app.state::<MicrophoneRouting>();
    if state.epoch.load(Ordering::SeqCst) != message.epoch {
        return true;
    }
    match message.event.as_str() {
        "ready" => {
            if let Ok(mut ack) = state.acknowledged.lock() {
                ack.insert(tab_id, message.epoch);
            }
        }
        "attempt" => {
            if message.purpose == "dictation" {
                state.remember_attempt(tab_id, message.attempt);
            } else if message.purpose == "voice" {
                // A new Voice gesture supersedes any previous Dictation
                // attempt in this tab; a delayed error cannot cancel PTT.
                if let Ok(mut attempts) = state.latest_attempt.lock() {
                    attempts.remove(&tab_id);
                }
            }
        }
        "error" => {
            let text = error_message(&message.reason);
            if message.purpose == "voice" {
                // Voice must never cancel a PTT hold or its pending Send,
                // including delayed Voice errors from the same tab.
                eprintln!("[gpt-wrapper] Voice microphone capture failed on tab {tab_id}: {text}");
                return true;
            }
            if message.purpose != "dictation" {
                return true;
            }
            // A delayed failure from a prior Dictation click must not cancel
            // a newer PTT session (or a PTT in a different tab).
            if !state.take_failed_attempt(tab_id, &message.attempt) {
                return true;
            }
            let tracker = &app.state::<super::PttState>().0;
            let cancelled_hold = tracker.current_generation(tab_id);
            tracker.clear_tab(tab_id);
            if let Some(tab) = super::get_tab_webview(app, tab_id) {
                let _ = tab.eval("window.__GPT_WRAPPER__?.cancelExplicitSend?.();");
                if let Some(generation) = cancelled_hold {
                    // Avoid stopping a newer session if this Stop is delayed.
                    super::trusted_click_guarded(
                        &tab,
                        super::DictationTarget::Finish {
                            allow_native_send: false,
                        },
                        Some(super::ClickGuard::Cancel(
                            std::sync::Arc::clone(tracker),
                            generation + 1,
                        )),
                    );
                }
            }
            eprintln!("[gpt-wrapper] Dictation microphone capture failed on tab {tab_id}: {text}");
        }
        "openSettings" => super::open_settings_window(app),
        "captureVerified" => {
            #[cfg(debug_assertions)]
            eprintln!("[gpt-wrapper] {} requested exact microphone and audio track ID verified on tab {tab_id}; actual isolated audio unverified", message.purpose);
        }
        _ => {}
    }
    true
}

pub fn sync_tab(app: &AppHandle, _tab_id: usize, webview: &Webview) -> Result<(), String> {
    let url = webview.url().map_err(|e| e.to_string())?;
    if !trusted_chatgpt_source(url.as_str()) {
        return Ok(());
    }
    let settings = app.state::<GeneralSettingsState>();
    let routing = app.state::<MicrophoneRouting>();
    // Read a stable version: a concurrent selection must not pair old A
    // with new B's epoch and override the correct config in a new tab.
    let (selection, epoch) = loop {
        let before = routing.epoch.load(Ordering::SeqCst);
        let selection = settings.preferred_microphone()?;
        let after = routing.epoch.load(Ordering::SeqCst);
        if before == after {
            break (selection, after);
        }
    };
    let script = format!(
        "window.__GPT_WRAPPER_MIC_ROUTE__?.configure({}, {});",
        serde_json::to_string(&selection).map_err(|e| e.to_string())?,
        epoch
    );
    webview.eval(&script).map_err(|e| e.to_string())
}

fn sync_all_tabs(app: &AppHandle) {
    let tab_ids = app
        .state::<TabState>()
        .lock()
        .map(|tabs| tabs.contexts.keys().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    for tab_id in tab_ids {
        if let Some(tab) = super::get_tab_webview(app, tab_id) {
            if let Err(error) = sync_tab(app, tab_id, &tab) {
                eprintln!(
                    "[gpt-wrapper] failed to sync microphone preference on tab {tab_id}: {error}"
                );
            }
        }
    }
}

pub fn ptt_start_guard(app: &AppHandle, tab_id: usize) -> Result<(), String> {
    let state = app.state::<MicrophoneRouting>();
    let epoch = state.epoch.load(Ordering::SeqCst);
    // Even Default must have received its policy before PTT clicks Start;
    // an unconfigured WebView cannot safely distinguish Default from B.
    let ready = state
        .acknowledged
        .lock()
        .map(|ack| ack.get(&tab_id) == Some(&epoch))
        .unwrap_or(false);
    if !ready {
        if let Some(tab) = super::get_tab_webview(app, tab_id) {
            let _ = sync_tab(app, tab_id, &tab);
        }
        return Err("麥克風設定尚未同步至 ChatGPT 分頁，請再試一次。".into());
    }
    Ok(())
}

pub fn show_ptt_error(app: &AppHandle, tab_id: Option<usize>, reason: &str) {
    eprintln!("[gpt-wrapper] PTT blocked: {reason}");
    if let Some(tab_id) = tab_id {
        if let Some(tab) = super::get_tab_webview(app, tab_id) {
            let error_type = if reason.contains("尚未同步") {
                "pending"
            } else {
                "unavailable"
            };
            let _ = tab.eval(&format!(
                "window.__GPT_WRAPPER_MIC_ROUTE__?.showError('{}');",
                error_type
            ));
        }
    }
    // Surface errors to the main window when the user is triggering tray-hidden PTT.
    if app
        .state::<super::TrayVisibilityState>()
        .tray_hidden
        .load(Ordering::SeqCst)
    {
        super::restore_main_window_from_tray(app);
    }
}

impl InventoryBridge {
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
    fn add(&self, id: String, tab_id: usize, sender: Reply) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|_| "inventory mutex poisoned".to_string())?
            .insert(id, (tab_id, sender));
        Ok(())
    }
    fn remove(&self, id: &str) {
        if let Ok(mut pending) = self.0.lock() {
            pending.remove(id);
        }
    }
}

pub fn handle_message(app: &AppHandle, tab_id: usize, source: &str, raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    if value.get("kind").and_then(|kind| kind.as_str()) != Some("gpt-wrapper-microphone-inventory")
    {
        return false;
    }
    if !trusted_chatgpt_source(source) {
        return true;
    }
    let Ok(message) = serde_json::from_value::<InventoryMessage>(value) else {
        return true;
    };
    if message.kind != "gpt-wrapper-microphone-inventory" || message.request_id.len() > 100 {
        return true;
    }
    let state = app.state::<InventoryBridge>();
    let Ok(mut pending) = state.0.lock() else {
        return true;
    };
    let Some((requested_tab, _)) = pending.get(&message.request_id) else {
        return true;
    };
    if *requested_tab != tab_id {
        return true;
    }
    let Some((_, sender)) = pending.remove(&message.request_id) else {
        return true;
    };
    drop(pending);
    let result = if let Some(error) = message.error {
        Err(format!(
            "ChatGPT microphone permission or listing failed: {}",
            error.chars().take(120).collect::<String>()
        ))
    } else if message.devices.len() > 128
        || message.devices.iter().any(|device| {
            device.device_id.len() > 1024
                || device.group_id.len() > 1024
                || device.label.len() > 512
        })
    {
        Err("microphone inventory response rejected".into())
    } else {
        let mut devices = message.devices;
        devices.retain(|device| {
            !device.device_id.is_empty()
                && !matches!(device.device_id.as_str(), "default" | "communications")
        });
        let has_unnamed = devices.iter().any(|device| device.label.is_empty());
        Ok(Inventory {
            devices,
            status: if has_unnamed {
                "needsPermission"
            } else {
                "ready"
            },
            detail: if has_unnamed {
                "請先在 ChatGPT 頁面允許麥克風權限，再重新整理清單。".into()
            } else {
                "來自 ChatGPT WebView 的音訊輸入清單。".into()
            },
        })
    };
    let _ = sender.send(result);
    true
}

fn trusted_inventory_webview(app: &AppHandle) -> Result<(usize, Webview), String> {
    let tabs = app.state::<TabState>();
    let tabs = tabs.lock()?;
    let active = tabs.active_tab_id;
    let mut candidates = tabs.contexts.keys().copied().collect::<Vec<_>>();
    candidates.sort_by_key(|tab_id| (*tab_id != active.unwrap_or(0), *tab_id));
    for tab_id in candidates {
        let Some(webview) = super::get_tab_webview(app, tab_id) else {
            continue;
        };
        if let Ok(url) = webview.url() {
            if trusted_chatgpt_source(url.as_str()) {
                return Ok((tab_id, webview));
            }
        }
    }
    Err("沒有可用的 ChatGPT 頁面。請先開啟並載入 ChatGPT 分頁。".into())
}

async fn query_inventory(app: AppHandle) -> Result<Inventory, String> {
    let (tab_id, webview) = trusted_inventory_webview(&app)?;
    let request_id = Uuid::new_v4().to_string();
    let (sender, receiver) = mpsc::channel();
    let state = app.state::<InventoryBridge>();
    state.add(request_id.clone(), tab_id, sender)?;
    let script = format!(
        "window.__GPT_WRAPPER_MIC_INVENTORY__?.query({});",
        serde_json::to_string(&request_id).map_err(|error| error.to_string())?
    );
    if let Err(error) = webview.eval(&script) {
        state.remove(&request_id);
        return Err(format!("無法查詢 ChatGPT 麥克風清單：{error}"));
    }
    let result =
        tauri::async_runtime::spawn_blocking(move || receiver.recv_timeout(Duration::from_secs(5)))
            .await
            .map_err(|error| error.to_string())?;
    app.state::<InventoryBridge>().remove(&request_id);
    result.map_err(|_| "ChatGPT 音訊裝置查詢逾時，請重整頁面或確認權限。".to_string())?
}

#[tauri::command]
pub async fn list_microphone_inputs(app: AppHandle, webview: Webview) -> Result<Inventory, String> {
    ensure_settings_webview(&webview)?;
    query_inventory(app).await
}

#[tauri::command]
pub fn get_preferred_microphone(
    app: AppHandle,
    webview: Webview,
) -> Result<PreferredMicrophone, String> {
    ensure_settings_webview(&webview)?;
    app.state::<GeneralSettingsState>().preferred_microphone()
}

// Validate the exact browser-origin identity before persisting; a label match
// alone does not authorize switching a device.
#[tauri::command]
pub async fn set_preferred_microphone(
    app: AppHandle,
    webview: Webview,
    preference: PreferredMicrophone,
) -> Result<PreferredMicrophone, String> {
    ensure_settings_webview(&webview)?;
    if let PreferredMicrophone::Device {
        device_id,
        label,
        group_id,
    } = &preference
    {
        let inventory = query_inventory(app.clone()).await?;
        if inventory.status != "ready"
            || !inventory.devices.iter().any(|input| {
                input.device_id == *device_id
                    && input.label == *label
                    && input.group_id == *group_id
                    && !input.label.is_empty()
            })
        {
            return Err("所選麥克風已變更或不可用，請重新整理音源清單後選取。".into());
        }
    }
    let saved = app
        .state::<GeneralSettingsState>()
        .set_preferred_microphone(preference)?;
    app.state::<MicrophoneRouting>().reset_for_explicit_choice();
    sync_all_tabs(&app);
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_json_and_concrete_device_json_have_explicit_modes() {
        let default = serde_json::to_value(PreferredMicrophone::Default).unwrap();
        assert_eq!(default["mode"], "default");
        let selected = PreferredMicrophone::Device {
            device_id: "web-device".into(),
            label: "USB microphone".into(),
            group_id: "g".into(),
        };
        let value = serde_json::to_value(&selected).unwrap();
        assert_eq!(value["mode"], "device");
        assert_eq!(value["deviceId"], "web-device");
        assert_eq!(
            serde_json::from_value::<PreferredMicrophone>(value).unwrap(),
            selected
        );
    }
    #[test]
    fn preference_version_and_tab_attempts_are_isolated() {
        let route = MicrophoneRouting::new();
        let generation = route.epoch.load(Ordering::SeqCst);
        route.acknowledged.lock().unwrap().insert(2, generation);
        route.remember_attempt(2, "first".into());
        route.remember_attempt(3, "other".into());
        route.remember_attempt(2, "latest".into());
        assert!(
            !route.take_failed_attempt(2, "first"),
            "stale error cannot cancel a new click"
        );
        assert!(route.take_failed_attempt(2, "latest"));
        assert!(
            !route.take_failed_attempt(2, "latest"),
            "duplicate error is ignored"
        );
        assert_eq!(
            route
                .latest_attempt
                .lock()
                .unwrap()
                .get(&3)
                .map(String::as_str),
            Some("other")
        );
        route.forget_tab(2);
        assert!(route.acknowledged.lock().unwrap().is_empty());
        assert_eq!(
            route
                .latest_attempt
                .lock()
                .unwrap()
                .get(&3)
                .map(String::as_str),
            Some("other")
        );
        let new_epoch = route.reset_for_explicit_choice();
        assert!(new_epoch > generation);
        assert!(route.acknowledged.lock().unwrap().is_empty());
        assert!(route.latest_attempt.lock().unwrap().is_empty());
    }

    #[test]
    fn inventory_response_schema_rejects_extra_fields() {
        let data = r#"{"kind":"gpt-wrapper-microphone-inventory","requestId":"request","devices":[{"deviceId":"abc","groupId":"g","label":"USB"}]}"#;
        let parsed: InventoryMessage = serde_json::from_str(data).unwrap();
        assert_eq!(parsed.devices[0].label, "USB");
        assert!(serde_json::from_str::<InventoryMessage>(
            r#"{"kind":"gpt-wrapper-microphone-inventory","requestId":"a","unexpected":1}"#
        )
        .is_err());
    }
}
