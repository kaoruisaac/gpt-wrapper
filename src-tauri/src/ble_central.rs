use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;
use windows::{
    core::{GUID, HSTRING},
    Devices::Bluetooth::GenericAttributeProfile::{
        GattCharacteristic, GattCharacteristicProperties,
        GattClientCharacteristicConfigurationDescriptorValue, GattCommunicationStatus,
        GattDeviceService,
    },
    Devices::Bluetooth::{
        Advertisement::{
            BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher,
        },
        BluetoothAdapter, BluetoothAddressType, BluetoothCacheMode, BluetoothConnectionStatus,
        BluetoothLEDevice,
    },
    Devices::Enumeration::{DeviceInformation, DevicePairingResultStatus},
    Devices::Radios::RadioState,
    Foundation::TypedEventHandler,
    Storage::Streams::DataReader,
    Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
};

const SERVICE: GUID = GUID::from_u128(0xe8a5e640_96d8_4c29_9a71_5e3f2dc10001);
const CONTROL: GUID = GUID::from_u128(0xe8a5e640_96d8_4c29_9a71_5e3f2dc10002);
const HOLD_LIMIT: Duration = Duration::from_secs(180);
const CONNECT_COOLDOWN: Duration = Duration::from_secs(5);
const MAX_CONNECT_COOLDOWN: Duration = Duration::from_secs(60);
const WATCHER_RETRY: Duration = Duration::from_secs(5);
const MAX_WATCHER_RETRY: Duration = Duration::from_secs(60);
const ADVERTISEMENT_DEBOUNCE: Duration = Duration::from_secs(2);
const MANUAL_SCAN_DURATION: Duration = Duration::from_secs(15);
const APPROVED_ADVERTISEMENT_TTL: Duration = Duration::from_secs(20);
const FAST_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_millis(1500),
    Duration::from_millis(3000),
];
const CCCD_DESCRIPTOR: GUID = GUID::from_u128(0x00002902_0000_1000_8000_00805f9b34fb);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlEvent {
    Down,
    Up,
    Submit,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Packet {
    event: ControlEvent,
    seq: u16,
    uptime_ms: u32,
}

fn decode(bytes: &[u8]) -> Result<Packet, &'static str> {
    if bytes.len() != 8 {
        return Err("frame length must be exactly 8");
    }
    if bytes[0] != 1 {
        return Err("unsupported frame version");
    }
    let event = match bytes[1] {
        1 => ControlEvent::Down,
        2 => ControlEvent::Up,
        3 => ControlEvent::Submit,
        _ => return Err("unknown control event"),
    };
    Ok(Packet {
        event,
        seq: u16::from_le_bytes([bytes[2], bytes[3]]),
        uptime_ms: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    })
}

#[derive(Default)]
struct Sequence {
    last: Option<u16>,
    held_since: Option<Instant>,
}

// A matching advertisement is the only invitation to start a new GATT attempt.
// The worker owns this gate, so a blocking WinRT join cannot create parallel attempts.
struct ReconnectGate {
    next_attempt: Instant,
    failures: u32,
}

impl ReconnectGate {
    fn new(now: Instant) -> Self {
        Self {
            next_attempt: now,
            failures: 0,
        }
    }
    fn ready(&self, now: Instant) -> bool {
        now >= self.next_attempt
    }
    fn reset(&mut self, now: Instant) {
        *self = Self::new(now);
    }
    fn delay(&mut self, now: Instant, base: Duration, max: Duration) {
        let shift = self.failures.min(4);
        self.next_attempt = now + (base * (1 << shift)).min(max);
        self.failures = self.failures.saturating_add(1);
    }
}

fn should_connect(
    approval: &Approval,
    resolved_id: &str,
    is_paired: bool,
    link_ready: bool,
    gate: &ReconnectGate,
    now: Instant,
) -> bool {
    approval.enabled
        && !link_ready
        && is_paired
        && approval.device_id.as_deref() == Some(resolved_id)
        && gate.ready(now)
}

fn active_session(current: Option<u64>, event_session: u64) -> bool {
    current == Some(event_session)
}

// One worker-owned burst consists of one normal attempt and at most three
// fresh-object retries. Slow cooldown is independent and starts on exhaustion.
#[derive(Default)]
struct RecoveryBurst {
    attempts: usize,
    retry_at: Option<Instant>,
}

impl RecoveryBurst {
    fn begin_attempt(&mut self) -> usize {
        assert!(self.attempts < 1 + FAST_RETRY_DELAYS.len());
        self.attempts += 1;
        self.retry_at = None;
        self.attempts
    }

    fn failure(&mut self, now: Instant, recoverable: bool) -> Option<Duration> {
        let delay = if recoverable && self.attempts > 0 {
            FAST_RETRY_DELAYS.get(self.attempts - 1).copied()
        } else {
            None
        };
        self.retry_at = delay.map(|delay| now + delay);
        delay
    }

    fn due(&self, now: Instant) -> bool {
        self.retry_at.is_some_and(|at| now >= at)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Copy)]
struct ApprovedAdvertisement {
    candidate: SeenCandidate,
    received_at: Instant,
}

impl ApprovedAdvertisement {
    fn fresh(self, now: Instant) -> bool {
        now.saturating_duration_since(self.received_at) <= APPROVED_ADVERTISEMENT_TTL
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureKind {
    Transient,
    AccessDenied,
    Security,
    RadioOff,
    Invalid,
}

impl FailureKind {
    fn recoverable(self) -> bool {
        matches!(self, Self::Transient)
    }

    fn from_hresult(code: i32) -> Self {
        match code as u32 {
            0x80070005 => Self::AccessDenied,
            0x80650005 | 0x8065000f => Self::Security,
            _ => Self::Transient,
        }
    }

    fn from_gatt(status: GattCommunicationStatus, protocol: Option<u8>) -> Self {
        if status == GattCommunicationStatus::AccessDenied {
            Self::AccessDenied
        } else if status == GattCommunicationStatus::ProtocolError
            && matches!(protocol, Some(0x05 | 0x0f))
        {
            Self::Security
        } else {
            Self::Transient
        }
    }
}

#[derive(Debug)]
struct ConnectFailure {
    stage: &'static str,
    kind: FailureKind,
    status: Option<GattCommunicationStatus>,
    hresult: Option<i32>,
    detail: String,
}

impl ConnectFailure {
    fn winrt(stage: &'static str, action: &'static str, error: windows::core::Error) -> Self {
        let hresult = error.code().0;
        Self {
            stage,
            kind: FailureKind::from_hresult(hresult),
            status: None,
            hresult: Some(hresult),
            detail: format!("{action}: {error}"),
        }
    }

    fn gatt(stage: &'static str, status: GattCommunicationStatus, protocol: Option<u8>) -> Self {
        Self {
            stage,
            kind: FailureKind::from_gatt(status, protocol),
            status: Some(status),
            hresult: None,
            detail: gatt_failure(stage, status, protocol),
        }
    }

    fn invalid(stage: &'static str, detail: impl Into<String>) -> Self {
        Self {
            stage,
            kind: FailureKind::Invalid,
            status: None,
            hresult: None,
            detail: detail.into(),
        }
    }

    fn radio(detail: String) -> Self {
        Self {
            stage: "radio",
            kind: FailureKind::RadioOff,
            status: None,
            hresult: None,
            detail,
        }
    }

    fn display_state(&self) -> &'static str {
        if self.kind == FailureKind::Security {
            "bond-required"
        } else if matches!(self.kind, FailureKind::AccessDenied | FailureKind::Invalid) {
            "error"
        } else if self.stage == "cccd" {
            "cccd-failed"
        } else if self.status == Some(GattCommunicationStatus::Unreachable) {
            "gatt-unreachable"
        } else {
            "waiting-for-device"
        }
    }
}

impl std::fmt::Display for ConnectFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

fn readiness_blocked_by_queued_command(
    current_session: u64,
    pending: &std::collections::VecDeque<Command>,
) -> bool {
    pending.iter().any(|command| {
        matches!(
            command,
            Command::Enable(false)
                | Command::Disconnect
                | Command::Forget
                | Command::Quit
                | Command::Approve(_)
        ) || matches!(command, Command::Lost(session, _) if *session == current_session)
    })
}

impl Sequence {
    fn accept(&mut self, packet: Packet) -> Result<Option<ControlEvent>, &'static str> {
        if self.last.is_none() && packet.seq == 0 {
            return Err("connection must start at nonzero sequence");
        }
        if let Some(last) = self.last {
            let distance = packet.seq.wrapping_sub(last);
            if distance == 0 || distance >= 0x8000 {
                return Err("duplicate or out-of-order sequence");
            }
        }
        self.last = Some(packet.seq);
        match packet.event {
            ControlEvent::Down if self.held_since.is_none() => {
                self.held_since = Some(Instant::now());
                Ok(Some(ControlEvent::Down))
            }
            ControlEvent::Up if self.held_since.take().is_some() => Ok(Some(ControlEvent::Up)),
            ControlEvent::Submit => Ok(Some(ControlEvent::Submit)),
            _ => Ok(None),
        }
    }
    fn orphaned(&self) -> bool {
        self.held_since.is_some_and(|at| at.elapsed() >= HOLD_LIMIT)
    }
    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Approval {
    enabled: bool,
    device_id: Option<String>,
    device_name: Option<String>,
    #[serde(default)]
    fingerprint_salt: Option<Uuid>,
}

fn identity_label(approval: &Approval, id: &str) -> String {
    let Some(salt) = approval.fingerprint_salt else {
        return "unavailable".into();
    };
    let digest = Sha256::digest([salt.as_bytes().as_slice(), id.as_bytes()].concat());
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Copy)]
struct SeenCandidate {
    address: u64,
    address_type: BluetoothAddressType,
}

struct ResolvedIdentity {
    id: String,
    paired: bool,
    can_pair: bool,
}

fn resolve_identity(
    address: u64,
    address_type: BluetoothAddressType,
) -> Result<ResolvedIdentity, String> {
    let device =
        BluetoothLEDevice::FromBluetoothAddressWithBluetoothAddressTypeAsync(address, address_type)
            .map_err(|e| format!("address lookup start: {e}"))?
            .join()
            .map_err(|e| format!("address lookup: {e}"))?;
    let info = device
        .DeviceInformation()
        .map_err(|e| format!("device information: {e}"))?;
    let id = info
        .Id()
        .map_err(|e| format!("identity id: {e}"))?
        .to_string();
    let pairing = info
        .Pairing()
        .map_err(|e| format!("pairing information: {e}"))?;
    let paired = pairing.IsPaired().map_err(|e| format!("IsPaired: {e}"))?;
    let can_pair = pairing.CanPair().map_err(|e| format!("CanPair: {e}"))?;
    let _ = device.Close();
    Ok(ResolvedIdentity {
        id,
        paired,
        can_pair,
    })
}

// Called only for a candidate explicitly selected in Settings.
fn pair_selected(
    candidate: &SeenCandidate,
    before: &ResolvedIdentity,
) -> Result<ResolvedIdentity, String> {
    if !before.paired {
        if !before.can_pair {
            return Err("Windows reports CanPair=false; check Android bonding support".into());
        }
        let info = DeviceInformation::CreateFromIdAsync(&HSTRING::from(&before.id))
            .map_err(|e| format!("pairing device lookup start: {e}"))?
            .join()
            .map_err(|e| format!("pairing device lookup: {e}"))?;
        let result = info
            .Pairing()
            .map_err(|e| format!("Pairing: {e}"))?
            .PairAsync()
            .map_err(|e| format!("PairAsync start: {e}"))?
            .join()
            .map_err(|e| format!("PairAsync: {e}"))?;
        let outcome = result
            .Status()
            .map_err(|e| format!("PairAsync status: {e}"))?;
        eprintln!("[gpt-wrapper] BLE explicit PairAsync outcome={outcome:?}");
        if outcome != DevicePairingResultStatus::Paired
            && outcome != DevicePairingResultStatus::AlreadyPaired
        {
            return Err(format!("Windows pairing failed ({outcome:?})"));
        }
    }
    for attempt in 0..5 {
        if attempt != 0 {
            thread::sleep(Duration::from_millis(250));
        }
        let after = resolve_identity(candidate.address, candidate.address_type)?;
        if after.paired {
            return Ok(after);
        }
    }
    Err("PairAsync returned but Windows IsPaired=false".into())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub id: String,
    pub name: String,
    pub label: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub enabled: bool,
    pub approved_device: Option<Candidate>,
    pub candidates: Vec<Candidate>,
    pub state: String,
    pub detail: String,
    pub last_rx: Option<String>,
}

impl Snapshot {
    fn new(approval: &Approval) -> Self {
        Self {
            enabled: approval.enabled,
            approved_device: approval.device_id.as_ref().map(|id| Candidate {
                id: id.clone(),
                name: approval.device_name.clone().unwrap_or_default(),
                label: identity_label(approval, id),
            }),
            candidates: vec![],
            state: "disconnected".into(),
            detail: String::new(),
            last_rx: None,
        }
    }
}

enum Command {
    Enable(bool),
    Scan,
    Approve(String),
    Disconnect,
    Forget,
    Seen(u64, Instant, u64, BluetoothAddressType, String),
    Frame(u64, Vec<u8>),
    Lost(u64, ShutdownReason),
    Stopped(u64, String),
    Quit,
}

pub struct BleCentral {
    tx: Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
}

impl BleCentral {
    pub fn start(path: PathBuf, dispatch: impl Fn(ControlEvent) + Send + 'static) -> Self {
        let mut approval = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Approval>(&bytes).ok())
            .unwrap_or_default();
        if approval.fingerprint_salt.is_none() {
            approval.fingerprint_salt = Some(Uuid::new_v4());
            if let Err(error) = persist(&path, &approval) {
                eprintln!("[gpt-wrapper] BLE identity diagnostic salt persistence failed: {error}");
                approval.fingerprint_salt = None;
            }
        }
        let snapshot = Arc::new(Mutex::new(Snapshot::new(&approval)));
        let (tx, rx) = mpsc::channel();
        let state = Arc::clone(&snapshot);
        let worker_tx = tx.clone();
        thread::spawn(move || {
            // WinRT BLE objects and blocking async completions live on this MTA worker.
            match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
                Ok(()) => {
                    worker(path, approval, state, rx, worker_tx, dispatch);
                    unsafe {
                        RoUninitialize();
                    }
                }
                Err(error) => status(
                    &state,
                    "error",
                    format!("Windows Runtime initialization failed: {error}"),
                ),
            }
        });
        Self { tx, snapshot }
    }
    pub fn snapshot(&self) -> Result<Snapshot, String> {
        self.snapshot
            .lock()
            .map(|s| s.clone())
            .map_err(|_| "BLE state lock poisoned".into())
    }
    pub fn enable(&self, enabled: bool) -> Result<(), String> {
        self.send(Command::Enable(enabled))
    }
    pub fn scan(&self) -> Result<(), String> {
        self.send(Command::Scan)
    }
    pub fn approve(&self, id: String) -> Result<(), String> {
        self.send(Command::Approve(id))
    }
    pub fn disconnect(&self) -> Result<(), String> {
        self.send(Command::Disconnect)
    }
    pub fn forget(&self) -> Result<(), String> {
        self.send(Command::Forget)
    }
    pub fn quit(&self) {
        let _ = self.tx.send(Command::Quit);
    }
    fn send(&self, command: Command) -> Result<(), String> {
        self.tx
            .send(command)
            .map_err(|_| "BLE worker unavailable".into())
    }
}

struct Link {
    session: u64,
    device: BluetoothLEDevice,
    service: GattDeviceService,
    characteristic: GattCharacteristic,
    value_token: i64,
    connection_token: i64,
    service_token: i64,
    accepting_frames: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShutdownReason {
    User,
    LinkLost,
    ServiceChanged,
    HoldExpired,
    Quit,
}

impl ShutdownReason {
    fn clear_remote_subscription(self) -> bool {
        matches!(self, Self::User | Self::Quit)
    }
}

fn clear_cccd_if_connected(device: &BluetoothLEDevice, characteristic: &GattCharacteristic) {
    // A disconnected or invalidated peer can leave this WinRT operation pending
    // for a long time. Keep the operation on the owning MTA worker only when the
    // connection is still reported as live.
    if device.ConnectionStatus().ok() == Some(BluetoothConnectionStatus::Connected) {
        let result = characteristic
            .WriteClientCharacteristicConfigurationDescriptorAsync(
                GattClientCharacteristicConfigurationDescriptorValue::None,
            )
            .and_then(|op| op.join());
        if !matches!(result, Ok(GattCommunicationStatus::Success)) {
            eprintln!("[gpt-wrapper] BLE CCCD None during shutdown: {result:?}");
        }
    }
}

impl Link {
    fn close(self, reason: ShutdownReason) {
        self.accepting_frames.store(false, Ordering::Release);
        let _ = self.characteristic.RemoveValueChanged(self.value_token);
        let _ = self
            .device
            .RemoveConnectionStatusChanged(self.connection_token);
        let _ = self.device.RemoveGattServicesChanged(self.service_token);
        if reason.clear_remote_subscription() {
            clear_cccd_if_connected(&self.device, &self.characteristic);
        }
        let _ = self.service.Close();
        let _ = self.device.Close();
    }
}

// Own every registration during connection setup. Any failed step drops this
// guard, including failures after the CCCD write has succeeded.
struct PendingLink {
    device: Option<BluetoothLEDevice>,
    service: Option<GattDeviceService>,
    characteristic: Option<GattCharacteristic>,
    value_token: Option<i64>,
    connection_token: Option<i64>,
    service_token: Option<i64>,
    accepting_frames: Arc<AtomicBool>,
}

impl PendingLink {
    fn new(device: BluetoothLEDevice) -> Self {
        Self {
            device: Some(device),
            service: None,
            characteristic: None,
            value_token: None,
            connection_token: None,
            service_token: None,
            accepting_frames: Arc::new(AtomicBool::new(false)),
        }
    }

    fn finish(mut self, session: u64) -> Link {
        Link {
            session,
            device: self.device.take().unwrap(),
            service: self.service.take().unwrap(),
            characteristic: self.characteristic.take().unwrap(),
            value_token: self.value_token.take().unwrap(),
            connection_token: self.connection_token.take().unwrap(),
            service_token: self.service_token.take().unwrap(),
            accepting_frames: std::mem::replace(
                &mut self.accepting_frames,
                Arc::new(AtomicBool::new(false)),
            ),
        }
    }
}

impl Drop for PendingLink {
    fn drop(&mut self) {
        self.accepting_frames.store(false, Ordering::Release);
        if let (Some(characteristic), Some(token)) = (&self.characteristic, self.value_token.take())
        {
            let _ = characteristic.RemoveValueChanged(token);
        }
        // Setup may have failed because the peer became unreachable. Closing
        // the local objects releases this incomplete subscription without a
        // second remote operation on an invalid link.
        if let Some(device) = &self.device {
            if let Some(token) = self.connection_token.take() {
                let _ = device.RemoveConnectionStatusChanged(token);
            }
            if let Some(token) = self.service_token.take() {
                let _ = device.RemoveGattServicesChanged(token);
            }
            if let Some(service) = &self.service {
                let _ = service.Close();
            }
            let _ = device.Close();
        }
    }
}

fn status(state: &Arc<Mutex<Snapshot>>, value: &str, detail: impl Into<String>) {
    trace_stage(value, "");
    if let Ok(mut state) = state.lock() {
        state.state = value.into();
        state.detail = detail.into();
    }
}

fn trace_stage(stage: &str, detail: &str) {
    let time_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_millis())
        .unwrap_or_default();
    eprintln!("[gpt-wrapper] BLE ts_ms={time_ms} stage={stage} {detail}");
}

fn gatt_failure(
    stage: &str,
    result: GattCommunicationStatus,
    protocol_error: impl std::fmt::Debug,
) -> String {
    let diagnosis = if result == GattCommunicationStatus::Unreachable {
        "peer unreachable; wait for another approved advertisement"
    } else if result == GattCommunicationStatus::ProtocolError {
        "ATT protocol error; retry with fresh GATT objects"
    } else if result == GattCommunicationStatus::AccessDenied {
        "Windows denied GATT access; check Bluetooth permissions"
    } else {
        "GATT operation failed"
    };
    format!("{stage}: GattCommunicationStatus={result:?}, ATT protocol error={protocol_error:?}; {diagnosis}")
}

#[cfg(test)]
fn connection_failure_state(error: &str) -> &'static str {
    if error.contains("CCCD Indicate write") {
        "cccd-failed"
    } else if error.contains("Unreachable") || error.contains("peer unreachable") {
        "gatt-unreachable"
    } else {
        "waiting-for-device"
    }
}

fn close_session(
    link: &mut Option<Link>,
    sequence: &mut Sequence,
    state: &Arc<Mutex<Snapshot>>,
    dispatch: &impl Fn(ControlEvent),
    reason: ShutdownReason,
) {
    // Cancel before a potentially slow, explicit remote CCCD write. A single
    // take() owns both cancellation and cleanup, so stale Lost events are inert.
    let old = link.take();
    sequence.reset();
    if let Ok(mut snapshot) = state.lock() {
        snapshot.last_rx = None;
    }
    if let Some(old) = old {
        dispatch(ControlEvent::Cancel);
        old.close(reason);
    }
}

fn persist(path: &PathBuf, approval: &Approval) -> Result<(), String> {
    let parent = path.parent().ok_or("BLE settings path has no parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temp = parent.join(format!(".ble-settings-{}.tmp", Uuid::new_v4()));
    fs::write(
        &temp,
        serde_json::to_vec_pretty(approval).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::rename(&temp, path).map_err(|e| {
        let _ = fs::remove_file(&temp);
        e.to_string()
    })
}

fn connect(
    id: &str,
    session: u64,
    attempt: usize,
    tx: &Sender<Command>,
    state: &Arc<Mutex<Snapshot>>,
) -> Result<Link, ConnectFailure> {
    let started = Instant::now();
    let label = format!("Opening fresh Windows BLE session (attempt {attempt}/4)");
    status(
        state,
        if attempt == 1 {
            "connecting"
        } else {
            "retrying"
        },
        label,
    );
    check_radio().map_err(ConnectFailure::radio)?;
    let device = BluetoothLEDevice::FromIdAsync(&HSTRING::from(id))
        .map_err(|e| ConnectFailure::winrt("open", "device open start", e))?
        .join()
        .map_err(|e| ConnectFailure::winrt("open", "device open", e))?;
    let mut pending = PendingLink::new(device);
    // Last two attempts may warm cached enumeration, but all authoritative
    // objects below are ALWAYS derived from this session's uncached chain.
    if attempt >= 3 {
        let warm = pending
            .device
            .as_ref()
            .unwrap()
            .GetGattServicesForUuidWithCacheModeAsync(SERVICE, BluetoothCacheMode::Cached)
            .and_then(|op| op.join());
        trace_stage(
            "service-cache-warmup",
            &format!(
                "session={session} attempt={attempt} result={:?}",
                warm.as_ref().map(|response| response.Status())
            ),
        );
    }

    let services = pending
        .device
        .as_ref()
        .unwrap()
        .GetGattServicesForUuidWithCacheModeAsync(SERVICE, BluetoothCacheMode::Uncached)
        .map_err(|e| ConnectFailure::winrt("service", "Uncached PTT service query start", e))?
        .join()
        .map_err(|e| {
            ConnectFailure::winrt("service", "Uncached PTT service query completion", e)
        })?;
    let service_status = services
        .Status()
        .map_err(|e| ConnectFailure::winrt("service", "Uncached PTT service status", e))?;
    if service_status != GattCommunicationStatus::Success {
        let protocol = services.ProtocolError().ok().and_then(|v| v.Value().ok());
        return Err(ConnectFailure::gatt("service", service_status, protocol));
    }
    let service_list = services
        .Services()
        .map_err(|e| ConnectFailure::winrt("service", "Uncached PTT service list", e))?;
    if service_list
        .Size()
        .map_err(|e| ConnectFailure::winrt("service", "Service count", e))?
        == 0
    {
        return Err(ConnectFailure::invalid(
            "service",
            "Required PTT service absent (zero matches)",
        ));
    }
    pending.service = Some(
        service_list
            .GetAt(0)
            .map_err(|e| ConnectFailure::winrt("service", "Service list first item", e))?,
    );
    if attempt >= 3 {
        let warm = pending
            .service
            .as_ref()
            .unwrap()
            .GetCharacteristicsForUuidWithCacheModeAsync(CONTROL, BluetoothCacheMode::Cached)
            .and_then(|op| op.join());
        trace_stage(
            "characteristic-cache-warmup",
            &format!(
                "session={session} attempt={attempt} result={:?}",
                warm.as_ref().map(|response| response.Status())
            ),
        );
    }
    let chars = pending
        .service
        .as_ref()
        .unwrap()
        .GetCharacteristicsForUuidWithCacheModeAsync(CONTROL, BluetoothCacheMode::Uncached)
        .map_err(|e| {
            ConnectFailure::winrt(
                "characteristic",
                "Uncached PTT characteristic query start",
                e,
            )
        })?
        .join()
        .map_err(|e| {
            ConnectFailure::winrt(
                "characteristic",
                "Uncached PTT characteristic query completion",
                e,
            )
        })?;
    let characteristic_status = chars.Status().map_err(|e| {
        ConnectFailure::winrt("characteristic", "Uncached PTT characteristic status", e)
    })?;
    if characteristic_status != GattCommunicationStatus::Success {
        let protocol = chars.ProtocolError().ok().and_then(|v| v.Value().ok());
        return Err(ConnectFailure::gatt(
            "characteristic",
            characteristic_status,
            protocol,
        ));
    }
    let characteristic_list = chars.Characteristics().map_err(|e| {
        ConnectFailure::winrt("characteristic", "Uncached PTT characteristic list", e)
    })?;
    if characteristic_list
        .Size()
        .map_err(|e| ConnectFailure::winrt("characteristic", "Characteristic count", e))?
        == 0
    {
        return Err(ConnectFailure::invalid(
            "characteristic",
            "Required PTT control characteristic absent",
        ));
    }
    pending.characteristic = Some(characteristic_list.GetAt(0).map_err(|e| {
        ConnectFailure::winrt("characteristic", "Characteristic list first item", e)
    })?);
    let characteristic = pending.characteristic.as_ref().unwrap();
    if !characteristic
        .CharacteristicProperties()
        .map_err(|e| ConnectFailure::winrt("characteristic", "PTT characteristic properties", e))?
        .contains(GattCharacteristicProperties::Indicate)
    {
        return Err(ConnectFailure::invalid(
            "characteristic",
            "PTT control characteristic lacks INDICATE",
        ));
    }
    // A single uncached 0x2902 query precedes every CCCD write. Discovery is
    // best effort: even zero results or transient errors retain the normal
    // characteristic-owned CCCD write fallback, never a guessed handle.
    let descriptor_query = characteristic
        .GetDescriptorsForUuidWithCacheModeAsync(CCCD_DESCRIPTOR, BluetoothCacheMode::Uncached)
        .and_then(|op| op.join());
    match descriptor_query {
        Ok(result) => {
            let status = result.Status();
            let count = result.Descriptors().and_then(|descriptors| descriptors.Size());
            trace_stage("cccd-discovery", &format!("session={session} attempt={attempt} status={status:?} count={count:?}"));
        }
        Err(error) => trace_stage("cccd-discovery", &format!("session={session} attempt={attempt} error_hresult=0x{:08x} fallback=characteristic-write", error.code().0 as u32)),
    }

    let frame_tx = tx.clone();
    let accepting_frames = Arc::clone(&pending.accepting_frames);
    pending.value_token = Some(
        characteristic
            .ValueChanged(&TypedEventHandler::<
                GattCharacteristic,
                windows::Devices::Bluetooth::GenericAttributeProfile::GattValueChangedEventArgs,
            >::new(move |_, args| {
                if !accepting_frames.load(Ordering::Acquire) {
                    return Ok(());
                }
                if let Some(args) = args.as_ref() {
                    if let Ok(buffer) = args.CharacteristicValue() {
                        if let Ok(reader) = DataReader::FromBuffer(&buffer) {
                            if let Ok(len) = buffer.Length() {
                                if len == 8 {
                                    let mut bytes = vec![0; len as usize];
                                    if reader.ReadBytes(&mut bytes).is_ok() {
                                        let _ = frame_tx.send(Command::Frame(session, bytes));
                                    }
                                } else {
                                    let _ = frame_tx.send(Command::Frame(session, Vec::new()));
                                }
                            }
                        }
                    }
                }
                Ok(())
            }))
            .map_err(|e| ConnectFailure::winrt("handler", "ValueChanged registration", e))?,
    );
    let invalidated = Arc::new(AtomicBool::new(false));
    let lost_tx = tx.clone();
    let connection_invalidated = Arc::clone(&invalidated);
    pending.connection_token = Some(
        pending
            .device
            .as_ref()
            .unwrap()
            .ConnectionStatusChanged(&TypedEventHandler::<
                BluetoothLEDevice,
                windows::core::IInspectable,
            >::new(move |sender, _| {
                if sender.as_ref().is_some_and(|d| {
                    d.ConnectionStatus().ok() == Some(BluetoothConnectionStatus::Disconnected)
                }) {
                    connection_invalidated.store(true, Ordering::Release);
                    let _ = lost_tx.send(Command::Lost(session, ShutdownReason::LinkLost));
                }
                Ok(())
            }))
            .map_err(|e| {
                ConnectFailure::winrt("handler", "ConnectionStatusChanged registration", e)
            })?,
    );
    let service_tx = tx.clone();
    let service_invalidated = Arc::clone(&invalidated);
    pending.service_token = Some(
        pending
            .device
            .as_ref()
            .unwrap()
            .GattServicesChanged(&TypedEventHandler::<
                BluetoothLEDevice,
                windows::core::IInspectable,
            >::new(move |_, _| {
                service_invalidated.store(true, Ordering::Release);
                let _ = service_tx.send(Command::Lost(session, ShutdownReason::ServiceChanged));
                Ok(())
            }))
            .map_err(|e| ConnectFailure::winrt("handler", "GattServicesChanged registration", e))?,
    );
    let result = characteristic
        .WriteClientCharacteristicConfigurationDescriptorAsync(
            GattClientCharacteristicConfigurationDescriptorValue::Indicate,
        )
        .and_then(|op| op.join());
    match result {
        Ok(GattCommunicationStatus::Success) => trace_stage(
            "cccd-success",
            &format!("session={session} attempt={attempt}"),
        ),
        Ok(other) => return Err(ConnectFailure::gatt("cccd", other, None)),
        Err(error) => return Err(ConnectFailure::winrt("cccd", "CCCD Indicate write", error)),
    }
    if invalidated.load(Ordering::Acquire) {
        return Err(ConnectFailure {
            stage: "validate",
            kind: FailureKind::Transient,
            status: None,
            hresult: None,
            detail: "GATT service or BLE connection changed during subscription".into(),
        });
    }
    if pending
        .device
        .as_ref()
        .unwrap()
        .ConnectionStatus()
        .map_err(|e| ConnectFailure::winrt("validate", "BLE connection after CCCD", e))?
        != BluetoothConnectionStatus::Connected
    {
        return Err(ConnectFailure {
            stage: "validate",
            kind: FailureKind::Transient,
            status: None,
            hresult: None,
            detail: "GATT subscription completed but BLE link is disconnected".into(),
        });
    }
    // Keep frames disabled until the worker has processed any queued cancellation
    // or Lost event. CCCD completion alone is not activation of the new session.
    trace_stage(
        "subscribed",
        &format!(
            "session={session} attempt={attempt} elapsed_ms={}",
            started.elapsed().as_millis()
        ),
    );
    Ok(pending.finish(session))
}

// Run only on the existing MTA worker, after processing the next queued command.
// Each attempt owns a new WinRT link and never overlaps another connect().
fn drive_recovery(
    approval: &Approval,
    approved_seen: &mut Option<ApprovedAdvertisement>,
    burst: &mut RecoveryBurst,
    reconnect: &mut ReconnectGate,
    suspended: &mut bool,
    link: &mut Option<Link>,
    sequence: &mut Sequence,
    session: &mut u64,
    rx: &Receiver<Command>,
    pending_commands: &mut VecDeque<Command>,
    tx: &Sender<Command>,
    state: &Arc<Mutex<Snapshot>>,
) {
    if !approval.enabled || approval.device_id.is_none() || link.is_some() || *suspended {
        return;
    }
    let now = Instant::now();
    let due = if burst.attempts == 0 {
        reconnect.ready(now)
    } else {
        burst.due(now)
    };
    if !due {
        return;
    }
    let Some(ad) = approved_seen.filter(|ad| ad.fresh(now)) else {
        if approved_seen.take().is_some() {
            burst.reset();
            status(
                state,
                "waiting-for-device",
                "等待裝置重新廣播／持續自動重連",
            );
        }
        return;
    };
    // A fresh ad is not enough: pairing and exact Windows identity must
    // still be verified before EACH new GATT session.
    let identity = match resolve_identity(ad.candidate.address, ad.candidate.address_type) {
        Ok(identity)
            if approval.device_id.as_deref() == Some(identity.id.as_str()) && identity.paired =>
        {
            identity
        }
        Ok(identity) if approval.device_id.as_deref() == Some(identity.id.as_str()) => {
            *approved_seen = None;
            burst.reset();
            *suspended = true;
            status(
                state,
                "bond-required",
                "Approved device is no longer paired; Scan/Approve to pair",
            );
            return;
        }
        Ok(_) => {
            *approved_seen = None;
            burst.reset();
            status(
                state,
                "approved-identity-mismatch",
                "Approved device identity changed; ignored",
            );
            return;
        }
        Err(error) => {
            *approved_seen = None;
            burst.reset();
            status(
                state,
                "waiting-for-device",
                format!(
                "Could not validate device identity: {error}; waiting for approved advertisement"
            ),
            );
            return;
        }
    };
    if burst.attempts == 0
        && !should_connect(
            approval,
            &identity.id,
            identity.paired,
            link.is_some(),
            reconnect,
            now,
        )
    {
        return;
    }
    pending_commands.extend(rx.try_iter());
    let user_cancel = pending_commands.iter().any(|command| {
        matches!(
            command,
            Command::Enable(false)
                | Command::Disconnect
                | Command::Forget
                | Command::Quit
                | Command::Approve(_)
        )
    });
    if user_cancel {
        return;
    }
    *session = session.wrapping_add(1);
    let number = burst.begin_attempt();
    let started = Instant::now();
    trace_stage(
        "attempt-start",
        &format!("session={session} attempt={number}/4"),
    );
    let result = connect(&identity.id, *session, number, tx, state);
    pending_commands.extend(rx.try_iter());
    let lost_or_canceled = readiness_blocked_by_queued_command(*session, pending_commands);
    let result = match result {
        Ok(new_link) if lost_or_canceled => {
            new_link.close(ShutdownReason::LinkLost);
            Err(ConnectFailure {
                stage: "validate",
                kind: FailureKind::Transient,
                status: None,
                hresult: None,
                detail: "New GATT link was canceled before activation".into(),
            })
        }
        other => other,
    };
    match result {
        Ok(new_link) => {
            new_link.accepting_frames.store(true, Ordering::Release);
            *link = Some(new_link);
            sequence.reset();
            if let Ok(mut snapshot) = state.lock() {
                snapshot.last_rx = None;
            }
            reconnect.reset(Instant::now());
            burst.reset();
            status(state, "ready", "Indicate subscription ready");
            trace_stage(
                "attempt-ready",
                &format!(
                    "session={session} attempt={number} elapsed_ms={}",
                    started.elapsed().as_millis()
                ),
            );
        }
        Err(error) => {
            let user_cancel = pending_commands.iter().any(|command| {
                matches!(
                    command,
                    Command::Enable(false)
                        | Command::Disconnect
                        | Command::Forget
                        | Command::Quit
                        | Command::Approve(_)
                )
            });
            let delay = burst.failure(Instant::now(), error.kind.recoverable() && !user_cancel);
            trace_stage("attempt-failed", &format!(
                "session={session} attempt={number} stage={} kind={:?} gatt={:?} hresult={} elapsed_ms={} next_retry_ms={}",
                error.stage, error.kind, error.status,
                error.hresult.map(|code| format!("0x{:08x}", code as u32))
                    .unwrap_or_else(|| "none".into()),
                started.elapsed().as_millis(),
                delay.map(|d| d.as_millis().to_string()).unwrap_or_else(|| "none".into())
            ));
            if let Some(delay) = delay {
                status(
                    state,
                    "retrying",
                    format!(
                        "正在恢復連線（第 {}/4 次，{}ms 後）；{}: {}",
                        number + 1,
                        delay.as_millis(),
                        error.display_state(),
                        error
                    ),
                );
            } else {
                burst.reset();
                if user_cancel {
                    status(
                        state,
                        "waiting-for-device",
                        "Connection canceled; processing user action",
                    );
                } else if matches!(
                    error.kind,
                    FailureKind::Security | FailureKind::AccessDenied | FailureKind::Invalid
                ) {
                    *suspended = true;
                    status(
                        state,
                        error.display_state(),
                        format!(
                            "{}; check Bluetooth access and pairing before retrying",
                            error
                        ),
                    );
                } else {
                    if error.kind == FailureKind::RadioOff {
                        *approved_seen = None;
                    }
                    reconnect.delay(Instant::now(), CONNECT_COOLDOWN, MAX_CONNECT_COOLDOWN);
                    status(
                        state,
                        "waiting-for-device",
                        format!(
                            "等待裝置重新廣播／持續自動重連；{}: {}",
                            error.display_state(),
                            error
                        ),
                    );
                }
            }
        }
    }
}

fn worker(
    path: PathBuf,
    mut approval: Approval,
    state: Arc<Mutex<Snapshot>>,
    rx: Receiver<Command>,
    tx: Sender<Command>,
    dispatch: impl Fn(ControlEvent),
) {
    let mut watcher: Option<(BluetoothLEAdvertisementWatcher, i64, i64)> = None;
    let mut seen: HashMap<String, SeenCandidate> = HashMap::new();
    let mut recent_addresses: HashMap<u64, Instant> = HashMap::new();
    let mut unknown_addresses: HashSet<u64> = HashSet::new();
    let mut link: Option<Link> = None;
    let mut sequence = Sequence::default();
    let mut session = 0_u64;
    let mut scan_epoch = 0_u64;
    let mut manual_until: Option<Instant> = None;
    let mut reconnect = ReconnectGate::new(Instant::now());
    let mut watcher_retry = ReconnectGate::new(Instant::now());
    let mut approved_seen: Option<ApprovedAdvertisement> = None;
    let mut burst = RecoveryBurst::default();
    let mut suspended = false;
    let mut pending_commands = VecDeque::new();
    loop {
        let command = if let Some(command) = pending_commands.pop_front() {
            Ok(command)
        } else {
            rx.recv_timeout(Duration::from_millis(250))
        };
        let mut shutdown = None;
        match command {
            Ok(Command::Enable(enabled)) => {
                approval.enabled = enabled;
                if let Err(e) = persist(&path, &approval) {
                    status(&state, "error", format!("Saving BLE settings: {e}"));
                }
                if let Ok(mut s) = state.lock() {
                    s.enabled = enabled;
                }
                if !enabled {
                    approved_seen = None;
                    burst.reset();
                    suspended = false;
                    shutdown = Some(ShutdownReason::User);
                    stop_watcher(&mut watcher);
                    scan_epoch = scan_epoch.wrapping_add(1);
                    manual_until = None;
                    recent_addresses.clear();
                    unknown_addresses.clear();
                    status(&state, "disconnected", "Bluetooth controller disabled");
                } else {
                    suspended = false;
                    reconnect.reset(Instant::now());
                    watcher_retry.reset(Instant::now());
                }
            }
            Ok(Command::Scan) => {
                suspended = false;
                manual_until = Some(Instant::now() + MANUAL_SCAN_DURATION);
                seen.clear();
                if let Ok(mut s) = state.lock() {
                    s.candidates.clear();
                }
                watcher_retry.reset(Instant::now());
                if link.is_none() && !(approval.enabled && approval.device_id.is_some()) {
                    status(
                        &state,
                        "scanning",
                        "Searching for PTT service advertisements",
                    );
                }
            }
            Ok(Command::Seen(epoch, received_at, address, address_type, name)) => {
                let manual = manual_until.is_some_and(|until| Instant::now() < until);
                let awaiting = approval.enabled && approval.device_id.is_some() && link.is_none();
                if watcher.is_some()
                    && epoch == scan_epoch
                    && received_at.elapsed() < Duration::from_secs(10)
                    && (manual || awaiting)
                    && !recent_addresses.get(&address).is_some_and(|at| {
                        received_at.saturating_duration_since(*at) < ADVERTISEMENT_DEBOUNCE
                    })
                {
                    if recent_addresses.len() > 128 {
                        recent_addresses.clear();
                    }
                    recent_addresses.insert(address, received_at);
                    trace_stage(
                        "ad-service-filter-pass",
                        &format!("epoch={epoch} address_type={address_type:?}"),
                    );
                    let identity = resolve_identity(address, address_type);
                    match identity {
                        Ok(identity) => {
                            watcher_retry.reset(Instant::now());
                            let id = identity.id;
                            let approved_label = approval
                                .device_id
                                .as_deref()
                                .map(|value| identity_label(&approval, value))
                                .unwrap_or_else(|| "none".into());
                            let resolved_label = identity_label(&approval, &id);
                            trace_stage("identity-resolved", &format!("approved_slot={approved_label} seen_slot={resolved_label} IsPaired={} CanPair={} address_type={address_type:?}", identity.paired, identity.can_pair));
                            if manual && !seen.contains_key(&id) {
                                seen.insert(
                                    id.clone(),
                                    SeenCandidate {
                                        address,
                                        address_type,
                                    },
                                );
                                if let Ok(mut s) = state.lock() {
                                    s.candidates.push(Candidate {
                                        id: id.clone(),
                                        name,
                                        label: resolved_label.clone(),
                                    });
                                }
                            }
                            if awaiting
                                && approval.device_id.as_deref() == Some(id.as_str())
                                && identity.paired
                            {
                                approved_seen = Some(ApprovedAdvertisement {
                                    candidate: SeenCandidate {
                                        address,
                                        address_type,
                                    },
                                    received_at,
                                });
                            } else if awaiting
                                && approval.device_id.as_deref() == Some(id.as_str())
                                && !identity.paired
                            {
                                approved_seen = None;
                                burst.reset();
                                suspended = true;
                                status(&state, "bond-required",
                                    "Approved device is not paired in Windows; Scan and select it once to pair");
                            } else if awaiting && approval.device_id.as_deref() != Some(id.as_str())
                            {
                                if unknown_addresses.len() > 128 {
                                    unknown_addresses.clear();
                                }
                                if unknown_addresses.insert(address) {
                                    trace_stage("identity-mismatch", &format!(
                                        "approved_slot={approved_label} seen_slot={resolved_label}; ignored"
                                    ));
                                    status(&state, "approved-identity-mismatch",
                                        "PTT advertisement identity differs from approved Windows device; ignored");
                                }
                            } else if awaiting && !reconnect.ready(Instant::now()) {
                                trace_stage("attempt-gated", "reconnect cooldown");
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "[gpt-wrapper] BLE advertisement identity resolution failed: {e}"
                            );
                            if awaiting {
                                status(&state, "waiting-for-device", format!("Advertisement identity unavailable: {e}; waiting for approved device"));
                            }
                        }
                    }
                }
            }
            Ok(Command::Approve(id)) => {
                if let Some(selected) = seen.get(&id).copied() {
                    status(
                        &state,
                        "pairing",
                        "Checking Windows Bluetooth pairing for selected device",
                    );
                    let paired = resolve_identity(selected.address, selected.address_type)
                        .and_then(|before| {
                            if before.id != id {
                                return Err("Selected device identity changed; scan again".into());
                            }
                            pair_selected(&selected, &before)
                        });
                    let paired = match paired {
                        Ok(identity) => identity,
                        Err(error) => {
                            status(
                                &state,
                                "pairing-failed",
                                format!(
                                    "{error}; check Android bonding support and retry Scan/Approve"
                                ),
                            );
                            continue;
                        }
                    };
                    let paired_id = paired.id;
                    eprintln!(
                        "[gpt-wrapper] BLE explicit approval paired identity slot={} migrated={}",
                        identity_label(&approval, &paired_id),
                        paired_id != id
                    );
                    approved_seen = None;
                    burst.reset();
                    suspended = false;
                    shutdown = Some(ShutdownReason::User);
                    stop_watcher(&mut watcher);
                    scan_epoch = scan_epoch.wrapping_add(1);
                    manual_until = None;
                    recent_addresses.clear();
                    unknown_addresses.clear();
                    let name = state
                        .lock()
                        .ok()
                        .and_then(|s| {
                            s.candidates
                                .iter()
                                .find(|c| c.id == id)
                                .map(|c| c.name.clone())
                        })
                        .unwrap_or_default();
                    let candidate = Approval {
                        enabled: true,
                        device_id: Some(paired_id.clone()),
                        device_name: Some(name.clone()),
                        fingerprint_salt: approval.fingerprint_salt,
                    };
                    match persist(&path, &candidate) {
                        Ok(()) => {
                            approval = candidate;
                            seen.clear();
                            let paired_label = identity_label(&approval, &paired_id);
                            if let Ok(mut s) = state.lock() {
                                s.enabled = true;
                                s.approved_device = Some(Candidate {
                                    id: paired_id,
                                    name,
                                    label: paired_label,
                                });
                                s.candidates.clear();
                            }
                            reconnect.reset(Instant::now());
                            watcher_retry.reset(Instant::now());
                        }
                        Err(e) => status(
                            &state,
                            "error",
                            format!("Could not save device approval: {e}"),
                        ),
                    }
                } else {
                    status(&state, "error", "Select a device seen in the current scan");
                }
            }
            Ok(Command::Disconnect) => {
                approved_seen = None;
                burst.reset();
                suspended = false;
                shutdown = Some(ShutdownReason::User);
                approval.enabled = false;
                stop_watcher(&mut watcher);
                scan_epoch = scan_epoch.wrapping_add(1);
                manual_until = None;
                recent_addresses.clear();
                unknown_addresses.clear();
                if let Err(e) = persist(&path, &approval) {
                    status(&state, "error", format!("Saving BLE settings: {e}"));
                } else {
                    if let Ok(mut s) = state.lock() {
                        s.enabled = false;
                    }
                    status(
                        &state,
                        "disconnected",
                        "Disconnected by user; enable to reconnect",
                    );
                }
            }
            Ok(Command::Forget) => {
                approved_seen = None;
                burst.reset();
                suspended = false;
                shutdown = Some(ShutdownReason::User);
                stop_watcher(&mut watcher);
                scan_epoch = scan_epoch.wrapping_add(1);
                manual_until = None;
                recent_addresses.clear();
                unknown_addresses.clear();
                seen.clear();
                let candidate = Approval {
                    enabled: false,
                    device_id: None,
                    device_name: None,
                    fingerprint_salt: approval.fingerprint_salt,
                };
                if let Err(e) = persist(&path, &candidate) {
                    status(&state, "error", format!("Could not forget device: {e}"));
                } else {
                    approval = candidate;
                    if let Ok(mut s) = state.lock() {
                        s.enabled = false;
                        s.approved_device = None;
                        s.candidates.clear();
                    }
                    status(&state, "disconnected", "Device forgotten");
                }
            }
            Ok(Command::Frame(frame_session, bytes)) => {
                if active_session(link.as_ref().map(|current| current.session), frame_session)
                    && approval.enabled
                {
                    match decode(&bytes)
                        .and_then(|packet| sequence.accept(packet).map(|event| (packet, event)))
                    {
                        Ok((packet, Some(event))) => {
                            if let Ok(mut s) = state.lock() {
                                s.last_rx = Some(format!(
                                    "{:?} seq={} uptimeMs={}",
                                    event, packet.seq, packet.uptime_ms
                                ));
                            }
                            dispatch(event);
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("[gpt-wrapper] BLE packet rejected: {e}"),
                    }
                }
            }
            Ok(Command::Lost(lost_session, reason)) => {
                if active_session(link.as_ref().map(|current| current.session), lost_session) {
                    shutdown = Some(reason);
                    burst.reset();
                    reconnect.reset(Instant::now());
                    status(
                        &state,
                        "waiting-for-device",
                        format!("BLE {reason:?}; waiting for approved device advertisement"),
                    );
                }
            }
            Ok(Command::Stopped(epoch, reason)) => {
                if watcher.is_some() && epoch == scan_epoch {
                    stop_watcher(&mut watcher);
                    scan_epoch = scan_epoch.wrapping_add(1);
                    recent_addresses.clear();
                    unknown_addresses.clear();
                    approved_seen = None;
                    burst.reset();
                    watcher_retry.delay(Instant::now(), WATCHER_RETRY, MAX_WATCHER_RETRY);
                    eprintln!("[gpt-wrapper] {reason}");
                    if link.is_none() {
                        status(
                            &state,
                            "waiting-for-device",
                            format!("{reason}; retrying Bluetooth scan"),
                        );
                    }
                }
            }
            Ok(Command::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                stop_watcher(&mut watcher);
                close_session(
                    &mut link,
                    &mut sequence,
                    &state,
                    &dispatch,
                    ShutdownReason::Quit,
                );
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(reason) = shutdown {
            close_session(&mut link, &mut sequence, &state, &dispatch, reason);
        }
        if sequence.orphaned() {
            close_session(
                &mut link,
                &mut sequence,
                &state,
                &dispatch,
                ShutdownReason::HoldExpired,
            );
            reconnect.delay(Instant::now(), CONNECT_COOLDOWN, MAX_CONNECT_COOLDOWN);
            status(
                &state,
                "waiting-for-device",
                "PTT hold exceeded 180 seconds; waiting for device advertisement",
            );
        }
        if manual_until.is_some_and(|until| Instant::now() >= until) {
            manual_until = None;
            if !approval.enabled || approval.device_id.is_none() {
                status(
                    &state,
                    "disconnected",
                    "Manual scan finished; select a discovered device",
                );
            }
        }
        drive_recovery(
            &approval,
            &mut approved_seen,
            &mut burst,
            &mut reconnect,
            &mut suspended,
            &mut link,
            &mut sequence,
            &mut session,
            &rx,
            &mut pending_commands,
            &tx,
            &state,
        );
        let awaiting = approval.enabled && approval.device_id.is_some() && link.is_none();
        let manual = manual_until.is_some();
        if (awaiting || manual) && watcher.is_none() && watcher_retry.ready(Instant::now()) {
            scan_epoch = scan_epoch.wrapping_add(1);
            recent_addresses.clear();
            unknown_addresses.clear();
            match start_scan(tx.clone(), scan_epoch) {
                Ok(w) => {
                    watcher = Some(w);
                    if awaiting {
                        if burst.retry_at.is_none() && !suspended {
                            status(
                                &state,
                                "waiting-for-device",
                                "Waiting for approved device advertisement / automatic reconnect",
                            );
                        }
                    } else if link.is_none() {
                        status(
                            &state,
                            "scanning",
                            "Searching for PTT service advertisements",
                        );
                    }
                }
                Err(e) => {
                    eprintln!("[gpt-wrapper] BLE watcher start failed: {e}");
                    watcher_retry.delay(Instant::now(), WATCHER_RETRY, MAX_WATCHER_RETRY);
                    if link.is_none() {
                        status(
                            &state,
                            "waiting-for-device",
                            format!("{e}; retrying Bluetooth scan"),
                        );
                    }
                }
            }
        }
        if !(awaiting || manual) && watcher.is_some() {
            stop_watcher(&mut watcher);
            scan_epoch = scan_epoch.wrapping_add(1);
            recent_addresses.clear();
            unknown_addresses.clear();
        }
    }
}

fn stop_watcher(watcher: &mut Option<(BluetoothLEAdvertisementWatcher, i64, i64)>) {
    if let Some((watcher, received, stopped)) = watcher.take() {
        let _ = watcher.RemoveReceived(received);
        let _ = watcher.RemoveStopped(stopped);
        let _ = watcher.Stop();
    }
}

fn start_scan(
    tx: Sender<Command>,
    scan_epoch: u64,
) -> Result<(BluetoothLEAdvertisementWatcher, i64, i64), String> {
    check_radio()?;
    let watcher = BluetoothLEAdvertisementWatcher::new().map_err(|e| e.to_string())?;
    let filter = watcher.AdvertisementFilter().map_err(|e| e.to_string())?;
    filter
        .Advertisement()
        .map_err(|e| e.to_string())?
        .ServiceUuids()
        .map_err(|e| e.to_string())?
        .Append(SERVICE)
        .map_err(|e| e.to_string())?;
    let seen_tx = tx.clone();
    let recent = Arc::new(Mutex::new(HashMap::<u64, Instant>::new()));
    let received = watcher
        .Received(&TypedEventHandler::<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        >::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                if let (Ok(address), Ok(address_type), Ok(ad)) = (
                    args.BluetoothAddress(),
                    args.BluetoothAddressType(),
                    args.Advertisement(),
                ) {
                    let now = Instant::now();
                    if let Ok(mut recent) = recent.lock() {
                        if recent.get(&address).is_some_and(|at| {
                            now.saturating_duration_since(*at) < ADVERTISEMENT_DEBOUNCE
                        }) {
                            return Ok(());
                        }
                        if recent.len() > 128 {
                            recent.clear();
                        }
                        recent.insert(address, now);
                    }
                    let name = ad.LocalName().map(|s| s.to_string()).unwrap_or_default();
                    let _ =
                        seen_tx.send(Command::Seen(scan_epoch, now, address, address_type, name));
                }
            }
            Ok(())
        }))
        .map_err(|e| e.to_string())?;
    let stopped = watcher.Stopped(&TypedEventHandler::<BluetoothLEAdvertisementWatcher, windows::Devices::Bluetooth::Advertisement::BluetoothLEAdvertisementWatcherStoppedEventArgs>::new(move |_, args| {
        let reason = args.as_ref().and_then(|a| a.Error().ok()).map(|e| format!("BLE scan stopped: {e:?}" )).unwrap_or_else(|| "BLE scan stopped".into());
        let _ = tx.send(Command::Stopped(scan_epoch, reason)); Ok(())
    })).map_err(|e| e.to_string())?;
    watcher
        .Start()
        .map_err(|e| format!("BLE scan start failed (radio off or access denied): {e}"))?;
    Ok((watcher, received, stopped))
}

fn check_radio() -> Result<(), String> {
    let adapter = BluetoothAdapter::GetDefaultAsync()
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|e| format!("Bluetooth adapter unavailable: {e}"))?;
    if !adapter.IsLowEnergySupported().map_err(|e| e.to_string())? {
        return Err("No Bluetooth LE radio available".into());
    }
    if adapter
        .GetRadioAsync()
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|e| e.to_string())?
        .State()
        .map_err(|e| e.to_string())?
        != RadioState::On
    {
        return Err("Windows Bluetooth radio is off".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_burst_has_three_timed_retries_and_returns_to_slow_gate() {
        let start = Instant::now();
        let mut burst = RecoveryBurst::default();
        assert_eq!(burst.begin_attempt(), 1);
        for (index, delay) in FAST_RETRY_DELAYS.iter().enumerate() {
            let at = start + Duration::from_secs(index as u64 * 6);
            assert_eq!(burst.failure(at, true), Some(*delay));
            assert!(!burst.due(at + *delay - Duration::from_millis(1)));
            assert!(burst.due(at + *delay));
            assert_eq!(burst.begin_attempt(), index + 2);
        }
        assert_eq!(burst.failure(start + Duration::from_secs(20), true), None);
        assert!(!burst.due(start + Duration::from_secs(60)));
        let mut gate = ReconnectGate::new(start);
        gate.delay(start, CONNECT_COOLDOWN, MAX_CONNECT_COOLDOWN);
        assert!(!gate.ready(start + Duration::from_secs(4)));
        assert!(gate.ready(start + Duration::from_secs(5)));
        // A subsequent success clears both the retry schedule and slow failures.
        burst.reset();
        gate.reset(start);
        assert_eq!(burst.begin_attempt(), 1);
        assert_eq!(gate.failures, 0);
    }

    #[test]
    fn recoverable_failures_are_not_security_or_permission_failures() {
        assert_eq!(
            FailureKind::from_hresult(0x80650003_u32 as i32),
            FailureKind::Transient
        );
        assert_eq!(
            FailureKind::from_hresult(0x80650001_u32 as i32),
            FailureKind::Transient
        );
        assert_eq!(
            FailureKind::from_hresult(0x80650005_u32 as i32),
            FailureKind::Security
        );
        assert_eq!(
            FailureKind::from_hresult(0x8065000f_u32 as i32),
            FailureKind::Security
        );
        assert_eq!(
            FailureKind::from_hresult(0x80070005_u32 as i32),
            FailureKind::AccessDenied
        );
        assert_eq!(
            FailureKind::from_gatt(GattCommunicationStatus::ProtocolError, Some(0x05)),
            FailureKind::Security
        );
        assert_eq!(
            FailureKind::from_gatt(GattCommunicationStatus::Unreachable, None),
            FailureKind::Transient
        );
        assert!(!FailureKind::RadioOff.recoverable());
        assert!(!FailureKind::AccessDenied.recoverable());
        assert!(!FailureKind::Security.recoverable());
        assert!(!FailureKind::Invalid.recoverable());
        let mut burst = RecoveryBurst::default();
        burst.begin_attempt();
        assert_eq!(burst.failure(Instant::now(), false), None);
    }

    #[test]
    fn advertisement_must_be_recent_and_session_queue_cannot_override_cancel() {
        let now = Instant::now();
        let ad = ApprovedAdvertisement {
            candidate: SeenCandidate {
                address: 0,
                address_type: BluetoothAddressType::Public,
            },
            received_at: now,
        };
        assert!(ad.fresh(now + APPROVED_ADVERTISEMENT_TTL));
        assert!(!ad.fresh(now + APPROVED_ADVERTISEMENT_TTL + Duration::from_millis(1)));
        let mut queue = VecDeque::new();
        assert!(!readiness_blocked_by_queued_command(42, &queue));
        queue.push_back(Command::Frame(41, vec![1]));
        queue.push_back(Command::Lost(41, ShutdownReason::LinkLost));
        assert!(!readiness_blocked_by_queued_command(42, &queue));
        queue.push_back(Command::Lost(42, ShutdownReason::ServiceChanged));
        assert!(readiness_blocked_by_queued_command(42, &queue));
        queue.clear();
        queue.push_back(Command::Disconnect);
        assert!(readiness_blocked_by_queued_command(42, &queue));
        queue.clear();
        queue.push_back(Command::Forget);
        assert!(readiness_blocked_by_queued_command(42, &queue));
        queue.clear();
        queue.push_back(Command::Enable(false));
        assert!(readiness_blocked_by_queued_command(42, &queue));
        queue.clear();
        queue.push_back(Command::Quit);
        assert!(readiness_blocked_by_queued_command(42, &queue));
    }

    #[test]
    fn first_attempt_failure_then_success_has_no_remaining_retry() {
        let now = Instant::now();
        let mut burst = RecoveryBurst::default();
        assert_eq!(burst.begin_attempt(), 1);
        assert_eq!(burst.failure(now, true), Some(FAST_RETRY_DELAYS[0]));
        assert!(burst.due(now + FAST_RETRY_DELAYS[0]));
        assert_eq!(burst.begin_attempt(), 2);
        // Worker calls reset on READY, before processing any further advertisement.
        burst.reset();
        assert_eq!(burst.attempts, 0);
        assert_eq!(burst.retry_at, None);
        assert!(!burst.due(now + Duration::from_secs(30)));
    }

    #[test]
    fn strict_packets() {
        let good = [1, 1, 1, 0, 232, 3, 0, 0];
        assert_eq!(decode(&good).unwrap().uptime_ms, 1000);
        assert!(decode(&good[..7]).is_err());
        let mut bad = good;
        bad[0] = 2;
        assert!(decode(&bad).is_err());
        bad = good;
        bad[1] = 4;
        assert!(decode(&bad).is_err());
    }
    #[test]
    fn ordering_dedupe_and_reset() {
        let mut seq = Sequence::default();
        let packet = |event, n| Packet {
            event,
            seq: n,
            uptime_ms: 0,
        };
        assert_eq!(seq.accept(packet(ControlEvent::Up, 1)).unwrap(), None);
        assert_eq!(
            seq.accept(packet(ControlEvent::Down, 2)).unwrap(),
            Some(ControlEvent::Down)
        );
        assert!(seq.accept(packet(ControlEvent::Up, 2)).is_err());
        assert_eq!(seq.accept(packet(ControlEvent::Down, 3)).unwrap(), None);
        assert_eq!(
            seq.accept(packet(ControlEvent::Up, 4)).unwrap(),
            Some(ControlEvent::Up)
        );
        assert!(seq.accept(packet(ControlEvent::Submit, 3)).is_err());
        seq.reset();
        assert_eq!(
            seq.accept(packet(ControlEvent::Down, 1)).unwrap(),
            Some(ControlEvent::Down)
        );
        seq.reset();
        assert!(seq.accept(packet(ControlEvent::Submit, 0)).is_err());
        assert_eq!(
            seq.accept(packet(ControlEvent::Submit, u16::MAX)).unwrap(),
            Some(ControlEvent::Submit)
        );
        assert_eq!(
            seq.accept(packet(ControlEvent::Submit, 0)).unwrap(),
            Some(ControlEvent::Submit)
        );
    }

    #[test]
    fn orphaned_hold_is_bounded_and_disconnect_reset_clears_it() {
        let mut seq = Sequence::default();
        seq.held_since = Some(Instant::now() - HOLD_LIMIT);
        assert!(seq.orphaned());
        seq.reset();
        assert!(!seq.orphaned());
        assert_eq!(seq.last, None);
    }

    #[test]
    fn only_approved_advertisement_opens_a_connection() {
        let now = Instant::now();
        let gate = ReconnectGate::new(now);
        let mut approval = Approval {
            enabled: true,
            device_id: Some("approved-windows-id".into()),
            device_name: Some("same name".into()),
            fingerprint_salt: Some(Uuid::new_v4()),
        };
        assert!(should_connect(
            &approval,
            "approved-windows-id",
            true,
            false,
            &gate,
            now
        ));
        assert!(!should_connect(
            &approval,
            "approved-windows-id",
            false,
            false,
            &gate,
            now
        ));
        assert!(!should_connect(
            &approval,
            "different-windows-id",
            true,
            false,
            &gate,
            now
        ));
        assert!(!should_connect(
            &approval,
            "approved-windows-id",
            true,
            true,
            &gate,
            now
        ));
        approval.enabled = false;
        assert!(!should_connect(
            &approval,
            "approved-windows-id",
            true,
            false,
            &gate,
            now
        ));
        approval.enabled = true;
        approval.device_id = None;
        assert!(!should_connect(
            &approval,
            "approved-windows-id",
            true,
            false,
            &gate,
            now
        ));
    }

    #[test]
    fn legacy_approval_needs_windows_pairing_and_labels_do_not_expose_ids() {
        let legacy: Approval =
            serde_json::from_str(r#"{"enabled":true,"deviceId":"old-id","deviceName":"phone"}"#)
                .unwrap();
        assert!(legacy.fingerprint_salt.is_none());
        let mut migrated = legacy;
        migrated.fingerprint_salt =
            Some(Uuid::parse_str("f15c7afa-72e5-458e-8ddb-ec42d18e0d4e").unwrap());
        let first = identity_label(&migrated, "old-id");
        assert_eq!(first, identity_label(&migrated, "old-id"));
        assert_ne!(first, identity_label(&migrated, "other-id"));
        assert!(!first.contains("old-id"));
    }

    #[test]
    fn connection_failure_states_keep_gatt_and_cccd_distinct() {
        assert_eq!(
            connection_failure_state("Uncached PTT service: peer unreachable"),
            "gatt-unreachable"
        );
        assert_eq!(
            connection_failure_state("CCCD Indicate write: failed"),
            "cccd-failed"
        );
    }

    #[test]
    fn failed_connections_and_watcher_restarts_are_throttled() {
        let now = Instant::now();
        let mut gate = ReconnectGate::new(now);
        gate.delay(now, CONNECT_COOLDOWN, MAX_CONNECT_COOLDOWN);
        assert!(!gate.ready(now + CONNECT_COOLDOWN - Duration::from_millis(1)));
        assert!(gate.ready(now + CONNECT_COOLDOWN));
        gate.delay(
            now + CONNECT_COOLDOWN,
            CONNECT_COOLDOWN,
            MAX_CONNECT_COOLDOWN,
        );
        assert!(!gate.ready(now + CONNECT_COOLDOWN * 2));
        assert!(gate.ready(now + CONNECT_COOLDOWN * 3));
        gate.reset(now);
        assert!(gate.ready(now));

        let mut watcher = ReconnectGate::new(now);
        watcher.delay(now, WATCHER_RETRY, MAX_WATCHER_RETRY);
        assert!(!watcher.ready(now));
        assert!(watcher.ready(now + WATCHER_RETRY));
    }

    #[test]
    fn shutdown_skips_remote_write_for_faulted_links() {
        assert!(ShutdownReason::User.clear_remote_subscription());
        assert!(ShutdownReason::Quit.clear_remote_subscription());
        assert!(!ShutdownReason::LinkLost.clear_remote_subscription());
        assert!(!ShutdownReason::ServiceChanged.clear_remote_subscription());
        assert!(!ShutdownReason::HoldExpired.clear_remote_subscription());
    }

    #[test]
    fn gatt_failures_keep_their_stage_and_diagnosis() {
        let unreachable = gatt_failure(
            "Uncached PTT service query",
            GattCommunicationStatus::Unreachable,
            None::<u8>,
        );
        assert!(unreachable.contains("Uncached PTT service query"));
        assert!(unreachable.contains("peer unreachable"));
        assert!(!unreachable.contains("OS GATT cache"));
        let protocol = gatt_failure(
            "Uncached PTT characteristic query",
            GattCommunicationStatus::ProtocolError,
            Some(1_u8),
        );
        assert!(protocol.contains("ATT protocol error=Some(1)"));
        let denied = gatt_failure(
            "CCCD Indicate write",
            GattCommunicationStatus::AccessDenied,
            "unavailable",
        );
        assert!(denied.contains("Windows denied GATT access"));
    }

    #[test]
    fn new_session_rejects_old_callbacks_and_resets_sequence() {
        let mut sequence = Sequence::default();
        let packet = |event, seq| Packet {
            event,
            seq,
            uptime_ms: 0,
        };
        assert!(active_session(Some(10), 10));
        assert_eq!(
            sequence.accept(packet(ControlEvent::Down, 1)).unwrap(),
            Some(ControlEvent::Down)
        );
        sequence.reset();
        assert!(!active_session(Some(11), 10));
        assert!(!active_session(None, 11));
        assert!(active_session(Some(11), 11));
        assert_eq!(
            sequence.accept(packet(ControlEvent::Down, 1)).unwrap(),
            Some(ControlEvent::Down)
        );
        assert_eq!(
            sequence.accept(packet(ControlEvent::Submit, 2)).unwrap(),
            Some(ControlEvent::Submit)
        );
        assert!(sequence.accept(packet(ControlEvent::Submit, 2)).is_err());
    }
}
