mod audio;
mod ble;
mod control;
mod host;
mod lifecycle;
pub mod runtime;
mod store;
mod wifi;
mod wire;
mod workstation;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use store::{PairState, Record, Store};
use tauri::{AppHandle, Manager, Webview};
use wire::*;
use zeroize::Zeroize;

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    revision: u64,
    receiver: String,
    audio: String,
    selected_id: Option<String>,
    devices: Vec<Value>,
    wizard: Option<Wizard>,
    error: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WizardStage {
    Search,
    Securing,
    Identify,
    Wifi,
    Connecting,
    Discovering,
    Authenticating,
    Preparing,
    Committing,
    WaitingWifiOnly,
    Done,
    Canceling,
    Canceled,
    Error,
    RequiresReset,
}
impl WizardStage {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Securing => "securing",
            Self::Identify => "identify",
            Self::Wifi => "wifi",
            Self::Connecting => "connecting",
            Self::Discovering => "discovering",
            Self::Authenticating => "authenticating",
            Self::Preparing => "preparing",
            Self::Committing => "committing",
            Self::WaitingWifiOnly => "waiting_wifi_only",
            Self::Done => "done",
            Self::Canceling => "canceling",
            Self::Canceled => "canceled",
            Self::Error => "error",
            Self::RequiresReset => "requires_reset",
        }
    }
}
impl From<&str> for WizardStage {
    fn from(s: &str) -> Self {
        match s {
            "search" => Self::Search,
            "securing" => Self::Securing,
            "identify" => Self::Identify,
            "wifi" => Self::Wifi,
            "connecting" => Self::Connecting,
            "discovering" => Self::Discovering,
            "authenticating" => Self::Authenticating,
            "preparing" => Self::Preparing,
            "committing" => Self::Committing,
            "waiting_wifi_only" => Self::WaitingWifiOnly,
            "done" => Self::Done,
            "canceling" => Self::Canceling,
            "canceled" => Self::Canceled,
            "error" => Self::Error,
            "requires_reset" => Self::RequiresReset,
            _ => unreachable!("unknown internal wizard stage"),
        }
    }
}
impl PartialEq<&str> for WizardStage {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceError {
    code: String,
    stage: String,
    reason: String,
    retry_action: String,
    needs_physical_reset: bool,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Wizard {
    id: String,
    revision: u64,
    target_id: Option<String>,
    stage: WizardStage,
    targets: Vec<ble::Target>,
    networks: Vec<Value>,
    error: Option<DeviceError>,
    needs_physical_reset: bool,
}
pub struct Devices {
    engine: Arc<Mutex<Engine>>,
    stop: Arc<AtomicBool>,
    pub(crate) host: Arc<Mutex<host::HostState>>,
}
struct Engine {
    store: Store,
    store_error: Option<String>,
    host: Arc<Mutex<host::HostState>>,
    wizard: Option<Wizard>,
    link: Option<ble::Link>,
    candidate: Option<Record>,
    revision: u64,
    stage_at: Instant,
    finalized: bool,
    finalize_nonces: Option<(String, String)>,
    pending_error: Option<DeviceError>,
}
impl Devices {
    pub fn start(path: PathBuf, app: AppHandle) -> Result<Self, String> {
        let (store, store_error) = match Store::load(path.clone()) {
            Ok(s) => (s, None),
            Err(e) => (
                Store {
                    path,
                    data: Default::default(),
                },
                Some(e),
            ),
        };
        let host = Arc::new(Mutex::new(host::HostState::default()));
        host.lock().map_err(|_| "host_lock")?.records = store
            .data
            .pairs
            .iter()
            .map(|(id, r)| (id.clone(), r.clone()))
            .collect();
        let stop = Arc::new(AtomicBool::new(false));
        host.lock().map_err(|_| "host_lock")?.runtime.selected = store.data.selected.clone();
        let bridge = host.lock().map_err(|_| "host_lock")?.bridge.clone();
        workstation::start(stop.clone());
        audio::start(bridge, stop.clone());
        host::start(host.clone(), stop.clone(), app.clone());
        let notice_host = host.clone();
        let notice_stop = stop.clone();
        thread::spawn(move || {
            let mut notices = lifecycle::Notices::default();
            let mut last_status = None;
            while !notice_stop.load(Ordering::SeqCst) {
                let event = notice_host.lock().ok().and_then(|mut h| {
                    let selected = h.runtime.selected.clone();
                    let online = h.ready
                        && selected.as_ref().is_some_and(|id| {
                            h.records.get(id).is_some_and(|r| {
                                h.evidence.get(id).is_some_and(|p| {
                                    p.pair_id == r.pair_id
                                        && p.wifi_only
                                        && matches!(p.phase.as_str(), "PAIRED" | "FINALIZED")
                                        && p.seen.elapsed() < Duration::from_secs(2)
                                })
                            })
                        });
                    let interrupted = h.interruption.take().is_some_and(|id| Some(id) == selected);
                    let audio = h.bridge.lock().is_ok_and(|b| b.ready());
                    let status = (selected.is_some(), online, audio);
                    if last_status != Some(status) {
                        // Fixed booleans only: no keys, SSIDs, audio or transcript.
                        eprintln!(
                            "[esp32-lifecycle] selected={} host_online={} audio_ready={}",
                            status.0, status.1, status.2
                        );
                        last_status = Some(status);
                    }
                    notices.update(selected, online, audio, interrupted, Instant::now())
                });
                if let Some(interrupted) = event {
                    if interrupted {
                        crate::tab_completion_notifications::device_notice(&app, interrupted);
                    }
                    // Temporarily disable the ESP32 offline notification (non-interrupted).
                    // if !interrupted {
                    //     crate::tab_completion_notifications::device_notice(&app, interrupted);
                    // }
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        let engine = Arc::new(Mutex::new(Engine {
            store,
            store_error,
            host: host.clone(),
            wizard: None,
            link: None,
            candidate: None,
            revision: 0,
            stage_at: Instant::now(),
            finalized: false,
            finalize_nonces: None,
            pending_error: None,
        }));
        let worker = engine.clone();
        let shutdown = stop.clone();
        thread::spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                if let Ok(_apartment) = ble::Apartment::enter() {
                    if let Ok(mut e) = worker.lock() {
                        if let Err(reason) = e.tick() {
                            e.fail(&reason);
                        }
                    }
                }
                thread::sleep(Duration::from_millis(250));
            }
        });
        Ok(Self { engine, stop, host })
    }
    pub fn stop(&self) {
        if let Ok(mut h) = self.host.lock() {
            h.runtime.invalidate += 1;
            h.runtime.cancelled = true;
            h.sessions.clear();
            runtime::close_bridge(&h.bridge);
        }
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut e) = self.engine.try_lock() {
            e.link = None;
            e.host.lock().ok().map(|mut h| h.records.clear());
        }
    }
}
impl Engine {
    fn stage(&mut self, stage: &str) {
        if let Some(w) = &mut self.wizard {
            if w.stage != stage {
                w.stage = stage.into();
                w.error = None;
                self.revision += 1;
                w.revision = self.revision;
                self.stage_at = Instant::now();
            }
        }
    }
    fn fail(&mut self, reason: &str) {
        let failure_stage = self
            .wizard
            .as_ref()
            .map(|w| w.stage.as_str())
            .unwrap_or("receiver")
            .to_owned();
        let uncertain = reason.ends_with("_reset_required")
            || self.wizard.as_ref().is_some_and(|w| w.needs_physical_reset)
            || self.candidate.as_ref().is_some_and(|r| {
                matches!(r.state, PairState::CommittedWaitFinal | PairState::Paired)
            })
            || self.finalized;
        if let Some(c) = self.candidate.as_mut() {
            c.state = PairState::Quarantined;
            let mut next = self.store.data.clone();
            if next.candidates.contains_key(&c.device) {
                next.candidates.insert(c.device.clone(), c.clone());
                let _ = self.store.save(next);
            }
        }
        if let Ok(mut h) = self.host.lock() {
            if let Some(c) = &self.candidate {
                h.records.remove(&c.device);
            }
            h.evidence.clear();
        }
        self.stage(if uncertain { "requires_reset" } else { "error" });
        if let Some(w) = &mut self.wizard {
            w.needs_physical_reset = uncertain
                || (self.candidate.is_some() && !self.link.as_ref().is_some_and(|l| l.usable()));
            let mut error = self.pending_error.take().unwrap_or_else(|| DeviceError {
                code: if reason.starts_with("E08_") {
                    "E08"
                } else if reason.starts_with("E09_") {
                    "E09"
                } else if reason.starts_with("E12_") {
                    "E12"
                } else if reason.starts_with("E21_") {
                    "E21"
                } else if reason.starts_with("ble_") || reason.starts_with("security_") {
                    "E06"
                } else {
                    "HostError"
                }
                .into(),
                stage: failure_stage,
                reason: reason.into(),
                retry_action: "retry".into(),
                needs_physical_reset: false,
            });
            error.needs_physical_reset = w.needs_physical_reset;
            error.retry_action = if w.needs_physical_reset {
                "physical_reset"
            } else {
                match error.code.as_str() {
                    "E08" => "wifi_entry",
                    "E09" => "rediscover",
                    "E12" => "new_session",
                    "E07" => "identify",
                    _ => "retry",
                }
            }
            .into();
            w.error = Some(error);
        }
    }
    fn snapshot(&self) -> Snapshot {
        let h = self.host.lock().ok();
        let mut devices = Vec::new();
        for r in self.store.data.pairs.values() {
            let online = h
                .as_ref()
                .and_then(|h| h.evidence.get(&r.device))
                .is_some_and(|p| {
                    p.pair_id == r.pair_id
                        && matches!(p.phase.as_str(), "PAIRED" | "FINALIZED")
                        && p.wifi_only
                        && p.seen.elapsed() < Duration::from_secs(2)
                });
            devices.push(json!({"id":r.device,"name":r.name,"ssid":r.ssid,"pair":"Paired","selected":self.store.data.selected.as_ref()==Some(&r.device),"hostAuth":online,"error":h.as_ref().and_then(|h|if self.store.data.selected.as_ref()==Some(&r.device){h.runtime.route_error.clone()}else{None}),"link":if online{"Online"}else{"Offline"}}));
        }
        for r in self
            .store
            .data
            .candidates
            .values()
            .filter(|r| !self.store.data.pairs.contains_key(&r.device))
        {
            devices.push(json!({"id":r.device,"name":r.name,"ssid":r.ssid,"pair":"Quarantined","selected":false,"hostAuth":false,"link":"Offline"}));
        }
        Snapshot {
            revision: self.revision,
            receiver: if h.as_ref().is_some_and(|h| h.ready) {
                "Ready".into()
            } else {
                h.as_ref()
                    .and_then(|h| h.error.clone())
                    .unwrap_or("Starting".into())
            },
            audio: h
                .as_ref()
                .and_then(|h| h.bridge.lock().ok().map(|b| b.status.clone()))
                .unwrap_or("Unavailable: worker_failed".into()),
            selected_id: self.store.data.selected.clone(),
            devices,
            wizard: self.wizard.clone(),
            error: self.store_error.clone().or_else(|| {
                if !self.store.data.candidates.is_empty() && self.wizard.is_none() {
                    Some(
                        "上次配對未完成：先在裝置同按雙鍵五秒重置，再忘記該筆本機記錄後重新新增。"
                            .into(),
                    )
                } else {
                    None
                }
            }),
        }
    }
    fn setup(&mut self, op: &str, args: Value) -> Result<Value, String> {
        self.link
            .as_mut()
            .ok_or("ble_disconnected")?
            .setup(op, args)
    }
    fn checked(&self, id: &Option<String>, revision: u64) -> Result<(), String> {
        if revision != self.revision {
            return Err("stale_ui_refresh_and_retry".into());
        }
        if let Some(w) = &self.wizard {
            if id.as_deref() != Some(&w.id) {
                return Err("stale_wizard".into());
            }
        }
        Ok(())
    }
    fn action(&mut self, a: Action) -> Result<Snapshot, String> {
        if let Some(e) = &self.store_error {
            return Err(e.clone());
        }
        self.checked(&a.wizard_id, a.expected_revision)?;
        let current = self
            .wizard
            .as_ref()
            .map(|w| w.stage.as_str())
            .unwrap_or("")
            .to_owned();
        match a.op.as_str() {
            "add" => {
                if !self.store.data.candidates.is_empty() {
                    return Err("previous_pairing_requires_device_reset_and_forget".into());
                }
                if self
                    .wizard
                    .as_ref()
                    .is_some_and(|w| !matches!(w.stage.as_str(), "done" | "canceled"))
                {
                    return Err("wizard_active".into());
                }
                self.link = None;
                self.candidate = None;
                self.finalized = false;
                self.finalize_nonces = None;
                self.wizard = Some(Wizard {
                    id: uuid::Uuid::new_v4().to_string(),
                    revision: self.revision,
                    target_id: None,
                    stage: "search".into(),
                    targets: Vec::new(),
                    networks: Vec::new(),
                    error: None,
                    needs_physical_reset: false,
                });
                self.wizard.as_mut().unwrap().targets = ble::scan()?;
            }
            "scan" if current == "search" => self.wizard.as_mut().unwrap().targets = ble::scan()?,
            "recheck_audio" => {
                let mut h = self.host.lock().map_err(|_| "host_lock")?;
                h.runtime.invalidate += 1;
                let mut b = h.bridge.lock().map_err(|_| "audio_lock")?;
                b.close();
                b.recheck = true;
            }
            "retry_receiver" => {
                self.host.lock().map_err(|_| "host_lock")?.retry = true;
            }
            "choose" if current == "search" => {
                let target = self
                    .wizard
                    .as_ref()
                    .unwrap()
                    .targets
                    .iter()
                    .find(|t| Some(t.id.as_str()) == a.args["id"].as_str())
                    .cloned()
                    .ok_or("unknown_target")?;
                self.stage("securing");
                self.wizard.as_mut().unwrap().target_id = Some(target.id.clone());
                self.link = Some(ble::Link::connect(&target)?);
                let hello = self.setup("HELLO", json!({}))?;
                let d = &hello["data"];
                if d["appProtocol"] != 1
                    || d["audio"] != "P8AU/1"
                    || !d["device"].as_str().is_some_and(|id| {
                        id.len() == 12
                            && id
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
                    })
                    || !["identify1", "commit1", "combo1", "gate1"].iter().all(|c| {
                        d["capabilities"]
                            .as_array()
                            .is_some_and(|caps| caps.iter().any(|v| v == c))
                    })
                {
                    return Err("E21_incompatible_firmware".into());
                }
                // The protocol Wi-Fi MAC differs from the pinned BLE transport address.
                self.wizard.as_mut().unwrap().target_id =
                    Some(d["device"].as_str().unwrap().into());
                if !hello["tx"].is_null() {
                    return Err("existing_device_transaction_reset_required".into());
                }
                self.setup("IDENTIFY", json!({}))?;
                self.stage("identify");
            }
            "identify"
                if matches!(current.as_str(), "identify" | "error") && self.candidate.is_none() =>
            {
                self.setup("IDENTIFY", json!({}))?;
                self.stage("identify");
            }
            "scan_wifi" if current == "wifi" => {
                let networks = self.scan_wifi()?;
                self.wizard.as_mut().unwrap().networks = networks;
            }
            "configure_wifi" if current == "wifi" => {
                if !self.host.lock().map_err(|_| "host_lock")?.ready {
                    return Err("receiver_not_ready_retry".into());
                }
                if a.args["consent"] != true {
                    return Err("wifi_sharing_consent_required".into());
                }
                let ssid = if let Some(raw) = a.args["ssidHex"].as_str() {
                    if raw.len() > 64 {
                        return Err("ssid_length".into());
                    }
                    unhex(raw, raw.len() / 2)?
                } else {
                    a.args["ssid"]
                        .as_str()
                        .ok_or("ssid_required")?
                        .as_bytes()
                        .to_vec()
                };
                let mut password = a.args["password"]
                    .as_str()
                    .unwrap_or("")
                    .as_bytes()
                    .to_vec();
                if ssid.is_empty() || ssid.len() > 32 || password.len() > 64 {
                    password.zeroize();
                    return Err("wifi_length".into());
                }
                if self.candidate.is_none() {
                    let begin = self.setup("BEGIN", json!({}))?;
                    let tx = begin["tx"]
                        .as_str()
                        .ok_or("missing_transaction")?
                        .to_owned();
                    unhex(&tx, 16)?;
                    let key = unhex(begin["data"]["key"].as_str().ok_or("missing_pair_key")?, 32)?;
                    self.link.as_mut().unwrap().tx = Some(tx.clone());
                    let device = self
                        .wizard
                        .as_ref()
                        .unwrap()
                        .target_id
                        .clone()
                        .ok_or("missing_device")?;
                    let r = Record {
                        device: device.clone(),
                        pair_id: tx,
                        key,
                        ssid: String::from_utf8_lossy(&ssid).into(),
                        name: format!("ESP32 {}", &device[6..]),
                        state: PairState::Prepared,
                    };
                    self.host
                        .lock()
                        .map_err(|_| "host_lock")?
                        .records
                        .insert(device, r.clone());
                    self.candidate = Some(r);
                } else {
                    let c = self.candidate.as_mut().ok_or("missing_candidate")?;
                    if c.state != PairState::Prepared
                        || self.store.data.candidates.contains_key(&c.device)
                    {
                        return Err("commit_uncertain_reset_required".into());
                    }
                    c.ssid = String::from_utf8_lossy(&ssid).into();
                    self.host
                        .lock()
                        .map_err(|_| "host_lock")?
                        .records
                        .insert(c.device.clone(), c.clone());
                }
                self.setup("HOST_CONFIG", json!({"controlPort":3335,"audioPort":3333}))?;
                let config = [blob(1, &ssid), blob(2, &password)].concat();
                password.zeroize();
                let link = self.link.as_mut().ok_or("ble_disconnected")?;
                let response = link.encrypted("prov-config", message(2, 12, &config))?;
                if number(bytes(&response, 13)?, 1)? != 0 {
                    return Err("E08_wifi_config_rejected".into());
                }
                let response = link.encrypted("prov-config", message(4, 14, &[]))?;
                if number(bytes(&response, 15)?, 1)? != 0 {
                    return Err("E08_wifi_apply_rejected".into());
                }
                self.stage("connecting");
            }
            "cancel" => {
                self.stage("canceling");
                if self.link.is_some() {
                    self.setup("CANCEL", json!({}))?;
                } else if self.candidate.is_some() {
                    return Err("commit_uncertain_reset_required".into());
                }
                if let Some(c) = self.candidate.take() {
                    let mut next = self.store.data.clone();
                    next.candidates.remove(&c.device);
                    self.store.save(next)?;
                    self.restore_host();
                }
                self.link = None;
                self.stage("canceled");
            }
            "retry"
                if current == "error"
                    && self
                        .wizard
                        .as_ref()
                        .and_then(|w| w.error.as_ref())
                        .is_some_and(|e| e.code == "E08")
                    && self.candidate.is_some()
                    && self.link.as_ref().is_some_and(|l| l.usable()) =>
            {
                let c = self.candidate.as_ref().unwrap();
                if self.store.data.candidates.contains_key(&c.device) {
                    return Err("commit_uncertain_reset_required".into());
                }
                self.setup("WIFI_RETRY", json!({}))?;
                self.candidate.as_mut().unwrap().state = PairState::Prepared;
                self.stage("wifi");
            }
            "retry"
                if current == "error"
                    && self
                        .wizard
                        .as_ref()
                        .and_then(|w| w.error.as_ref())
                        .is_some_and(|e| e.code == "E09")
                    && self.candidate.is_some()
                    && self.link.as_ref().is_some_and(|l| l.usable()) =>
            {
                let c = self.candidate.as_mut().unwrap();
                if self.store.data.candidates.contains_key(&c.device) {
                    return Err("commit_uncertain_reset_required".into());
                }
                c.state = PairState::Prepared;
                self.host
                    .lock()
                    .map_err(|_| "host_lock")?
                    .records
                    .insert(c.device.clone(), c.clone());
                self.setup("HOST_CONFIG", json!({"controlPort":3335,"audioPort":3333}))?;
                self.stage("discovering");
            }
            "retry" if current == "error" => {
                // A fresh session cannot reuse any uncertain CTR state or candidate.
                if self.link.is_some() {
                    self.setup("CANCEL", json!({}))?;
                }
                if let Some(c) = &self.candidate {
                    let mut next = self.store.data.clone();
                    next.candidates.remove(&c.device);
                    self.store.save(next)?;
                }
                self.link = None;
                self.candidate = None;
                self.restore_host();
                self.wizard = None;
                return self.action(Action {
                    op: "add".into(),
                    wizard_id: None,
                    expected_revision: self.revision,
                    args: json!({}),
                });
            }
            "reset_done"
                if matches!(current.as_str(), "error" | "requires_reset")
                    && a.args["confirmed"] == true =>
            {
                let mut next = self.store.data.clone();
                next.candidates.clear();
                self.store.save(next)?;
                self.link = None;
                self.candidate = None;
                self.restore_host();
                self.wizard = None;
                return self.action(Action {
                    op: "add".into(),
                    wizard_id: None,
                    expected_revision: self.revision,
                    args: json!({}),
                });
            }
            "back" if current == "identify" => {
                self.link = None;
                self.wizard.as_mut().unwrap().target_id = None;
                self.stage("search");
            }
            "back" if current == "wifi" => {
                self.setup("IDENTIFY", json!({}))?;
                self.stage("identify");
            }
            "select" => {
                let id = a.args["id"].as_str().ok_or("device_required")?;
                if !self.store.data.pairs.contains_key(id) {
                    return Err("unknown_device".into());
                }
                if self.store.data.selected.as_deref() != Some(id) {
                    self.close_runtime()?;
                }
                let mut next = self.store.data.clone();
                next.selected = Some(id.into());
                self.store.save(next)?;
            }
            "disable" => {
                self.close_runtime()?;
                let mut next = self.store.data.clone();
                next.selected = None;
                self.store.save(next)?;
            }
            "later" if current == "done" => {
                self.wizard = None;
            }
            "rename" => {
                let id = a.args["id"].as_str().ok_or("device_required")?;
                let name = a.args["name"].as_str().ok_or("name_required")?.trim();
                if name.is_empty() || name.chars().count() > 64 {
                    return Err("name_length".into());
                }
                let mut next = self.store.data.clone();
                next.pairs.get_mut(id).ok_or("unknown_device")?.name = name.into();
                self.store.save(next)?;
            }
            "forget" => {
                if a.args["confirmed"] != true {
                    return Err("forget_confirmation_required".into());
                }
                let id = a.args["id"].as_str().ok_or("device_required")?;
                if self.store.data.selected.as_deref() == Some(id) {
                    self.close_runtime()?;
                }
                let mut next = self.store.data.clone();
                next.pairs.remove(id);
                next.candidates.remove(id);
                if next.selected.as_deref() == Some(id) {
                    next.selected = None;
                }
                self.store.save(next)?;
                self.restore_host();
            }
            _ => return Err("illegal_stage_command".into()),
        }
        {
            let mut h = self.host.lock().map_err(|_| "host_lock")?;
            if h.runtime.selected != self.store.data.selected {
                h.runtime.selected = self.store.data.selected.clone();
                h.runtime.invalidate += 1;
                runtime::close_bridge(&h.bridge);
            }
        }
        self.revision += 1;
        if let Some(w) = &mut self.wizard {
            w.revision = self.revision;
        }
        Ok(self.snapshot())
    }
    fn restore_host(&mut self) {
        if let Ok(mut h) = self.host.lock() {
            h.records = self
                .store
                .data
                .pairs
                .iter()
                .map(|(id, r)| (id.clone(), r.clone()))
                .collect();
            h.evidence.clear();
        }
    }
    fn close_runtime(&self) -> Result<(), String> {
        let mut h = self.host.lock().map_err(|_| "host_lock")?;
        h.runtime.invalidate += 1;
        h.runtime.cancelled = true;
        h.sessions.clear();
        runtime::close_bridge(&h.bridge);
        Ok(())
    }
    fn scan_wifi(&mut self) -> Result<Vec<Value>, String> {
        let l = self.link.as_mut().ok_or("ble_disconnected")?;
        let start = l.encrypted(
            "prov-scan",
            message(0, 10, &[num(1, 1), num(3, 4), num(4, 120)].concat()),
        )?;
        if number(&start, 2)? != 0 {
            return Err("E08_wifi_scan".into());
        }
        let status = l.encrypted("prov-scan", message(2, 12, &[]))?;
        if number(&status, 2)? != 0 {
            return Err("E08_wifi_scan".into());
        }
        let status = bytes(&status, 13)?;
        if number(status, 1)? != 1 {
            return Err("wifi_scan_pending".into());
        }
        let count = number(status, 2)?.min(64);
        let mut networks = Vec::new();
        for index in 0..count {
            let r = l.encrypted(
                "prov-scan",
                message(4, 14, &[num(1, index), num(2, 1)].concat()),
            )?;
            if number(&r, 2)? != 0 {
                return Err("E08_wifi_scan".into());
            }
            for entry in fields(bytes(&r, 15)?)?.into_iter().filter(|f| f.id == 1) {
                let raw = bytes(entry.bytes, 1)?;
                let channel = number(entry.bytes, 2)?;
                let auth = number(entry.bytes, 5)?;
                if raw.len() > 32 || !(1..=14).contains(&channel) {
                    continue;
                }
                networks.push(json!({"ssidHex":hex(raw),"name":String::from_utf8_lossy(raw),"channel":channel,"auth":auth,"supported":matches!(auth,0|3|4|6|7)}));
            }
        }
        Ok(networks)
    }
    fn tick(&mut self) -> Result<(), String> {
        let stage = self
            .wizard
            .as_ref()
            .map(|w| w.stage.as_str())
            .unwrap_or("")
            .to_owned();
        if !matches!(
            stage.as_str(),
            "identify" | "connecting" | "discovering" | "authenticating" | "waiting_wifi_only"
        ) {
            return Ok(());
        }
        if stage == "waiting_wifi_only" {
            let c = self.candidate.as_ref().ok_or("missing_candidate")?;
            let proven = self
                .host
                .lock()
                .map_err(|_| "host_lock")?
                .evidence
                .get(&c.device)
                .is_some_and(|p| {
                    p.pair_id == c.pair_id
                        && p.phase == "FINALIZED"
                        && self
                            .finalize_nonces
                            .as_ref()
                            .is_some_and(|(dn, hn)| *dn == p.dn && *hn == p.hn)
                        && p.wifi_only
                        && p.seen.elapsed() < Duration::from_secs(2)
                });
            if proven {
                let mut r = c.clone();
                r.state = PairState::Paired;
                let mut next = self.store.data.clone();
                next.candidates.remove(&r.device);
                if next.selected.as_deref() == Some(&r.device) {
                    next.selected = None;
                }
                next.pairs.insert(r.device.clone(), r.clone());
                self.store.save(next)?;
                {
                    let mut h = self.host.lock().map_err(|_| "host_lock")?;
                    h.records.insert(r.device.clone(), r);
                    if h.runtime.selected != self.store.data.selected {
                        h.runtime.selected = self.store.data.selected.clone();
                        h.runtime.invalidate += 1;
                        h.runtime.cancelled = true;
                        h.sessions.clear();
                        runtime::close_bridge(&h.bridge);
                    }
                }
                self.candidate = None;
                self.link = None;
                self.stage("done");
                return Ok(());
            }
            if self.stage_at.elapsed() > Duration::from_secs(15) {
                return Err("commit_uncertain_reset_required".into());
            }
            return Ok(());
        }
        let response = self.setup("STATUS", json!({}))?;
        if response["data"]["eventsOverflow"] == true {
            return Err("ble_events_overflow_retry".into());
        }
        if let Some(events) = response["events"].as_array() {
            for e in events {
                if let Some(code) = e["code"].as_str() {
                    if matches!(code, "E07" | "E08" | "E09" | "E12" | "HostError") {
                        self.pending_error = Some(DeviceError {
                            code: code.into(),
                            stage: e["stage"].as_str().unwrap_or("unknown").into(),
                            reason: e["reason"].as_str().unwrap_or("failed").into(),
                            retry_action: "retry".into(),
                            needs_physical_reset: false,
                        });
                        return Err(format!(
                            "{} / {} / {}",
                            code,
                            e["stage"].as_str().unwrap_or("unknown"),
                            e["reason"].as_str().unwrap_or("failed")
                        ));
                    }
                }
            }
        }
        if stage == "identify" {
            if response["stage"] == "wifi" {
                self.stage("wifi");
            }
            return Ok(());
        }
        if let Some(s) = response["stage"].as_str() {
            if matches!(s, "connecting" | "discovering" | "authenticating") {
                self.stage(s);
            }
        }
        let c = self.candidate.as_ref().ok_or("missing_candidate")?;
        let evidence = self
            .host
            .lock()
            .map_err(|_| "host_lock")?
            .evidence
            .get(&c.device)
            .cloned();
        if let Some(p) = evidence.filter(|p| p.pair_id == c.pair_id && p.phase == "CANDIDATE") {
            self.finalize_nonces = Some((p.dn.clone(), p.hn.clone()));
            let args = json!({"deviceNonce":p.dn,"hostNonce":p.hn});
            self.stage("preparing");
            let r = self.setup("PREPARE", args.clone())?;
            if r["result"] != "DEVICE_PREPARED" {
                return Err("prepare_not_confirmed".into());
            }
            let c = self.candidate.as_ref().unwrap();
            let mut next = self.store.data.clone();
            next.candidates.insert(c.device.clone(), c.clone());
            self.store.save(next)?;
            self.stage("committing");
            // Mark uncertainty before sending COMMIT, including failures while reading its reply.
            self.candidate.as_mut().unwrap().state = PairState::CommittedWaitFinal;
            let r = self.setup("COMMIT", args.clone())?;
            if r["result"] != "DEVICE_SAVED" {
                return Err("commit_not_confirmed".into());
            }
            let c = self.candidate.as_ref().unwrap();
            let mut next = self.store.data.clone();
            next.candidates.insert(c.device.clone(), c.clone());
            self.store.save(next)?;
            self.finalized = true;
            // An unavailable BLE reply is recoverable only through signed FINALIZED plus W.
            let _ = self.setup("FINALIZE", args);
            self.stage("waiting_wifi_only");
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Action {
    op: String,
    wizard_id: Option<String>,
    expected_revision: u64,
    args: Value,
}
#[tauri::command]
pub async fn devices_wifi(
    app: AppHandle,
    webview: Webview,
    wizard_id: String,
    expected_revision: u64,
    consent: bool,
    ssid_hex: Option<String>,
) -> Result<Value, String> {
    crate::ensure_settings_webview(&webview)?;
    let engine = app.state::<Devices>().engine.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let e = engine.lock().map_err(|_| "devices_lock")?;
        e.checked(&Some(wizard_id), expected_revision)?;
        if e.wizard.as_ref().map(|w| w.stage.as_str()) != Some("wifi") {
            return Err("illegal_stage_command".into());
        }
        match ssid_hex {
            Some(ssid) => {
                if ssid.len() > 64 || ssid.is_empty() {
                    return Err("ssid_length".into());
                }
                unhex(&ssid, ssid.len() / 2)?;
                Ok(json!({"password":wifi::password(&ssid,consent)?}))
            }
            None => wifi::recommendation(),
        }
    })
    .await
    .map_err(|_| "devices_worker".to_string())?
}
#[tauri::command]
pub async fn devices_snapshot(app: AppHandle, webview: Webview) -> Result<Snapshot, String> {
    crate::ensure_settings_webview(&webview)?;
    let engine = app.state::<Devices>().engine.clone();
    tauri::async_runtime::spawn_blocking(move || {
        engine
            .lock()
            .map_err(|_| "devices_lock".to_string())
            .map(|e| e.snapshot())
    })
    .await
    .map_err(|_| "devices_worker".to_string())?
}
#[tauri::command]
pub async fn devices_action(
    app: AppHandle,
    webview: Webview,
    action: Action,
) -> Result<Snapshot, String> {
    crate::ensure_settings_webview(&webview)?;
    let engine = app.state::<Devices>().engine.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _apartment = ble::Apartment::enter()?;
        let mut e = engine.lock().map_err(|_| "devices_lock")?;
        match e.action(action) {
            Ok(s) => Ok(s),
            Err(reason) => {
                if !matches!(
                    reason.as_str(),
                    "stale_ui_refresh_and_retry"
                        | "stale_wizard"
                        | "illegal_stage_command"
                        | "wifi_sharing_consent_required"
                        | "ssid_length"
                        | "wifi_length"
                        | "unknown_device"
                        | "name_length"
                        | "forget_confirmation_required"
                        | "receiver_not_ready_retry"
                ) {
                    e.fail(&reason);
                }
                Err(reason)
            }
        }
    })
    .await
    .map_err(|_| "devices_worker".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        engine: Engine,
        root: PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn fixture() -> Fixture {
        let root =
            std::env::temp_dir().join(format!("gptw-transaction-test-{}", uuid::Uuid::new_v4()));
        let mut store = Store::load(root.join("pairs.bin")).unwrap();
        let old = Record {
            device: "AABBCCDDEEFF".into(),
            pair_id: "01".repeat(16),
            key: vec![1; 32],
            ssid: "old".into(),
            name: "old".into(),
            state: PairState::Paired,
        };
        let candidate = Record {
            device: old.device.clone(),
            pair_id: "02".repeat(16),
            key: vec![2; 32],
            ssid: "new".into(),
            name: "new".into(),
            state: PairState::CommittedWaitFinal,
        };
        let mut data = store.data.clone();
        data.pairs.insert(old.device.clone(), old);
        data.candidates
            .insert(candidate.device.clone(), candidate.clone());
        store.save(data).unwrap();
        Fixture {
            root,
            engine: Engine {
                store,
                store_error: None,
                host: Arc::new(Mutex::new(host::HostState::default())),
                wizard: Some(Wizard {
                    id: "test".into(),
                    revision: 0,
                    target_id: Some(candidate.device.clone()),
                    stage: "waiting_wifi_only".into(),
                    targets: Vec::new(),
                    networks: Vec::new(),
                    error: None,
                    needs_physical_reset: false,
                }),
                link: None,
                candidate: Some(candidate),
                revision: 0,
                stage_at: Instant::now(),
                finalized: true,
                finalize_nonces: Some(("11".repeat(8), "22".repeat(8))),
                pending_error: None,
            },
        }
    }
    fn evidence() -> host::Evidence {
        host::Evidence {
            pair_id: "02".repeat(16),
            dn: "11".repeat(8),
            hn: "22".repeat(8),
            phase: "FINALIZED".into(),
            wifi_only: true,
            seen: Instant::now(),
        }
    }
    #[test]
    fn completion_requires_both_proofs_and_exact_session() {
        let mut f = fixture();
        let e = &mut f.engine;
        let mut cases = Vec::new();
        let mut p = evidence();
        p.wifi_only = false;
        cases.push(p);
        let mut p = evidence();
        p.phase = "PAIRED".into();
        cases.push(p);
        let mut p = evidence();
        p.pair_id = "03".repeat(16);
        cases.push(p);
        let mut p = evidence();
        p.hn = "33".repeat(8);
        cases.push(p);
        let mut p = evidence();
        p.seen = Instant::now() - Duration::from_secs(3);
        cases.push(p);
        for proof in cases {
            e.host
                .lock()
                .unwrap()
                .evidence
                .insert("AABBCCDDEEFF".into(), proof);
            e.tick().unwrap();
            assert_eq!(e.store.data.pairs["AABBCCDDEEFF"].key, vec![1; 32]);
            assert_eq!(e.wizard.as_ref().unwrap().stage, "waiting_wifi_only");
        }
        e.host
            .lock()
            .unwrap()
            .evidence
            .insert("AABBCCDDEEFF".into(), evidence());
        e.tick().unwrap();
        assert_eq!(e.store.data.pairs["AABBCCDDEEFF"].key, vec![2; 32]);
        assert!(e.store.data.candidates.is_empty());
        assert_eq!(e.wizard.as_ref().unwrap().stage, "done");
        assert!(e.store.data.selected.is_none());
    }
    #[test]
    fn every_pending_disk_state_is_quarantined_after_restart() {
        for state in [
            PairState::Prepared,
            PairState::CommittedWaitFinal,
            PairState::Quarantined,
        ] {
            let mut f = fixture();
            let mut data = f.engine.store.data.clone();
            data.candidates.get_mut("AABBCCDDEEFF").unwrap().state = state;
            f.engine.store.save(data).unwrap();
            let loaded = Store::load(f.engine.store.path.clone()).unwrap();
            assert_eq!(
                loaded.data.candidates["AABBCCDDEEFF"].state,
                PairState::Quarantined
            );
            assert_eq!(loaded.data.pairs["AABBCCDDEEFF"].key, vec![1; 32]);
        }
    }
    #[test]
    fn final_disk_failure_cannot_replace_old_credentials_or_report_done() {
        let mut f = fixture();
        let original = f.engine.store.path.clone();
        let before = std::fs::read(&original).unwrap();
        let blocker = f.root.join("directory");
        std::fs::create_dir(&blocker).unwrap();
        std::fs::write(blocker.join("keep"), b"test").unwrap();
        f.engine.store.path = blocker;
        f.engine
            .host
            .lock()
            .unwrap()
            .evidence
            .insert("AABBCCDDEEFF".into(), evidence());
        let reason = f.engine.tick().unwrap_err();
        f.engine.fail(&reason);
        assert_eq!(std::fs::read(original).unwrap(), before);
        assert_eq!(f.engine.store.data.pairs["AABBCCDDEEFF"].key, vec![1; 32]);
        assert_eq!(f.engine.wizard.as_ref().unwrap().stage, "requires_reset");
        assert!(f.engine.host.lock().unwrap().records.is_empty());
    }
    #[test]
    fn stale_ui_does_not_advance_a_transaction() {
        let mut f = fixture();
        assert!(f
            .engine
            .action(Action {
                op: "cancel".into(),
                wizard_id: Some("test".into()),
                expected_revision: 9,
                args: json!({})
            })
            .is_err());
        assert_eq!(f.engine.wizard.as_ref().unwrap().stage, "waiting_wifi_only");
    }
    #[test]
    fn reset_key_cannot_authenticate_an_old_host() {
        let packet = sign(
            &[2; 32],
            "P6|P|AABBCCDDEEFF|1111111111111111|2222222222222222",
        );
        assert!(verified(&[1; 32], &packet).is_none());
    }
    #[test]
    fn replacing_a_previously_selected_pair_requires_explicit_selection_again() {
        let mut f = fixture();
        let mut data = f.engine.store.data.clone();
        data.selected = Some("AABBCCDDEEFF".into());
        f.engine.store.save(data).unwrap();
        f.engine
            .host
            .lock()
            .unwrap()
            .evidence
            .insert("AABBCCDDEEFF".into(), evidence());
        f.engine.tick().unwrap();
        assert!(f.engine.store.data.selected.is_none());
    }

    #[test]
    fn switching_and_forgetting_cancel_before_persisting_and_never_fail_over() {
        let mut f = fixture();
        f.engine.wizard = None;
        f.engine.candidate = None;
        let mut data = f.engine.store.data.clone();
        data.candidates.clear();
        let mut second = data.pairs["AABBCCDDEEFF"].clone();
        second.device = "112233445566".into();
        data.pairs.insert(second.device.clone(), second);
        data.selected = Some("AABBCCDDEEFF".into());
        f.engine.store.save(data).unwrap();
        {
            let mut h = f.engine.host.lock().unwrap();
            h.runtime.selected = Some("AABBCCDDEEFF".into());
            h.runtime.cancelled = false;
            h.sessions
                .insert("AABBCCDDEEFF".into(), ("old_dn".into(), "old_hn".into()));
            h.bridge.lock().unwrap().gate = true;
        }
        f.engine
            .action(Action {
                op: "select".into(),
                wizard_id: None,
                expected_revision: 0,
                args: json!({"id":"112233445566"}),
            })
            .unwrap();
        {
            let h = f.engine.host.lock().unwrap();
            assert!(h.runtime.cancelled);
            assert!(h.sessions.is_empty());
            assert!(!h.bridge.lock().unwrap().gate);
        }
        let loaded = Store::load(f.engine.store.path.clone()).unwrap();
        assert_eq!(loaded.data.selected.as_deref(), Some("112233445566"));
        assert!(f
            .engine
            .snapshot()
            .devices
            .iter()
            .all(|d| d["hostAuth"] == false));
        assert_eq!(
            f.engine.snapshot().selected_id.as_deref(),
            Some("112233445566")
        );
        f.engine
            .action(Action {
                op: "forget".into(),
                wizard_id: None,
                expected_revision: 1,
                args: json!({"id":"112233445566", "confirmed":true}),
            })
            .unwrap();
        let loaded = Store::load(f.engine.store.path.clone()).unwrap();
        assert!(loaded.data.selected.is_none());
        assert!(!loaded.data.pairs.contains_key("112233445566"));
        assert_eq!(loaded.data.pairs["AABBCCDDEEFF"].key, vec![1; 32]);
    }
}
