use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
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
    Devices::Radios::RadioState,
    Foundation::TypedEventHandler,
    Storage::Streams::DataReader,
    Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
};

const SERVICE: GUID = GUID::from_u128(0xe8a5e640_96d8_4c29_9a71_5e3f2dc10001);
const CONTROL: GUID = GUID::from_u128(0xe8a5e640_96d8_4c29_9a71_5e3f2dc10002);
const HOLD_LIMIT: Duration = Duration::from_secs(180);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

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
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub id: String,
    pub name: String,
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
    Seen(u64, u64, BluetoothAddressType, String),
    Frame(u64, Vec<u8>),
    Lost(u64),
    Stopped(u64, String),
    Quit,
}

pub struct BleCentral {
    tx: Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
}

impl BleCentral {
    pub fn start(path: PathBuf, dispatch: impl Fn(ControlEvent) + Send + 'static) -> Self {
        let approval = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Approval>(&bytes).ok())
            .unwrap_or_default();
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
}
impl Link {
    fn close(self) {
        let _ = self.characteristic.RemoveValueChanged(self.value_token);
        let _ = self
            .characteristic
            .WriteClientCharacteristicConfigurationDescriptorAsync(
                GattClientCharacteristicConfigurationDescriptorValue::None,
            )
            .and_then(|op| op.join());
        let _ = self
            .device
            .RemoveConnectionStatusChanged(self.connection_token);
        let _ = self.device.RemoveGattServicesChanged(self.service_token);
        let _ = self.service.Close();
    }
}

// Own every registration during connection setup. Any failed step drops this
// guard, including failures after the CCCD write has succeeded.
struct PendingLink {
    device: BluetoothLEDevice,
    service: Option<GattDeviceService>,
    characteristic: Option<GattCharacteristic>,
    value_token: Option<i64>,
    connection_token: Option<i64>,
    service_token: Option<i64>,
    subscribed: bool,
}

impl PendingLink {
    fn new(device: BluetoothLEDevice) -> Self {
        Self {
            device,
            service: None,
            characteristic: None,
            value_token: None,
            connection_token: None,
            service_token: None,
            subscribed: false,
        }
    }

    fn finish(mut self, session: u64) -> Link {
        self.subscribed = false;
        Link {
            session,
            device: self.device.clone(),
            service: self.service.take().unwrap(),
            characteristic: self.characteristic.take().unwrap(),
            value_token: self.value_token.take().unwrap(),
            connection_token: self.connection_token.take().unwrap(),
            service_token: self.service_token.take().unwrap(),
        }
    }
}

impl Drop for PendingLink {
    fn drop(&mut self) {
        if let (Some(characteristic), Some(token)) = (&self.characteristic, self.value_token.take())
        {
            let _ = characteristic.RemoveValueChanged(token);
        }
        if self.subscribed {
            if let Some(characteristic) = &self.characteristic {
                let _ = characteristic
                    .WriteClientCharacteristicConfigurationDescriptorAsync(
                        GattClientCharacteristicConfigurationDescriptorValue::None,
                    )
                    .and_then(|op| op.join());
            }
        }
        if let Some(token) = self.connection_token.take() {
            let _ = self.device.RemoveConnectionStatusChanged(token);
        }
        if let Some(token) = self.service_token.take() {
            let _ = self.device.RemoveGattServicesChanged(token);
        }
        if let Some(service) = &self.service {
            let _ = service.Close();
        }
    }
}

fn status(state: &Arc<Mutex<Snapshot>>, value: &str, detail: impl Into<String>) {
    if let Ok(mut state) = state.lock() {
        state.state = value.into();
        state.detail = detail.into();
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
    tx: &Sender<Command>,
    state: &Arc<Mutex<Snapshot>>,
) -> Result<Link, String> {
    status(state, "connecting", "Opening Windows BLE device");
    check_radio()?;
    let device = BluetoothLEDevice::FromIdAsync(&HSTRING::from(id))
        .map_err(|e| format!("device open: {e}"))?
        .join()
        .map_err(|e| format!("device open: {e}"))?;
    let mut pending = PendingLink::new(device);
    status(
        state,
        "link-connected",
        "BLE device opened; checking GATT service",
    );
    let services = pending
        .device
        .GetGattServicesForUuidWithCacheModeAsync(SERVICE, BluetoothCacheMode::Uncached)
        .map_err(|e| format!("Uncached PTT service query start: {e}"))?
        .join()
        .map_err(|e| format!("Uncached PTT service query completion: {e}"))?;
    let service_status = services
        .Status()
        .map_err(|e| format!("Uncached PTT service status: {e}"))?;
    if service_status != GattCommunicationStatus::Success {
        let protocol_error = services
            .ProtocolError()
            .ok()
            .and_then(|value| value.Value().ok());
        return Err(format!("Uncached PTT service query: GattCommunicationStatus={service_status:?}, ATT protocol error={protocol_error:?}; confirm the Android app is advertising and reconnect. If it persists, check Bluetooth pairing and the Windows OS GATT cache"));
    }
    let service_list = services
        .Services()
        .map_err(|e| format!("Uncached PTT service list: {e}"))?;
    if service_list
        .Size()
        .map_err(|e| format!("Uncached PTT service list size: {e}"))?
        == 0
    {
        return Err(
            "Uncached PTT service query: required PTT service absent (zero matches)".into(),
        );
    }
    let service = service_list
        .GetAt(0)
        .map_err(|e| format!("Uncached PTT service list first item: {e}"))?;
    pending.service = Some(service);
    status(
        state,
        "service-found",
        "PTT GATT service found; subscribing",
    );
    let chars = pending
        .service
        .as_ref()
        .unwrap()
        .GetCharacteristicsForUuidWithCacheModeAsync(CONTROL, BluetoothCacheMode::Uncached)
        .map_err(|e| format!("Uncached PTT characteristic query start: {e}"))?
        .join()
        .map_err(|e| format!("Uncached PTT characteristic query completion: {e}"))?;
    let characteristic_status = chars
        .Status()
        .map_err(|e| format!("Uncached PTT characteristic status: {e}"))?;
    if characteristic_status != GattCommunicationStatus::Success {
        let protocol_error = chars
            .ProtocolError()
            .ok()
            .and_then(|value| value.Value().ok());
        return Err(format!(
            "Uncached PTT characteristic query: GattCommunicationStatus={characteristic_status:?}, ATT protocol error={protocol_error:?}"
        ));
    }
    let characteristic_list = chars
        .Characteristics()
        .map_err(|e| format!("Uncached PTT characteristic list: {e}"))?;
    if characteristic_list
        .Size()
        .map_err(|e| format!("Uncached PTT characteristic list size: {e}"))?
        == 0
    {
        return Err("Uncached PTT characteristic query: required PTT control characteristic absent (zero matches)".into());
    }
    let characteristic = characteristic_list
        .GetAt(0)
        .map_err(|e| format!("Uncached PTT characteristic list first item: {e}"))?;
    pending.characteristic = Some(characteristic);
    let characteristic = pending.characteristic.as_ref().unwrap();
    if !characteristic
        .CharacteristicProperties()
        .map_err(|e| format!("PTT characteristic properties: {e}"))?
        .contains(GattCharacteristicProperties::Indicate)
    {
        return Err("PTT characteristic properties: control characteristic lacks INDICATE".into());
    }
    let frame_tx = tx.clone();
    pending.value_token = Some(
        characteristic
            .ValueChanged(&TypedEventHandler::<
                GattCharacteristic,
                windows::Devices::Bluetooth::GenericAttributeProfile::GattValueChangedEventArgs,
            >::new(move |_, args| {
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
                                    // Avoid allocating from an untrusted length; decoder logs rejection.
                                    let _ = frame_tx.send(Command::Frame(session, Vec::new()));
                                }
                            }
                        }
                    }
                }
                Ok(())
            }))
            .map_err(|e| format!("ValueChanged handler registration: {e}"))?,
    );
    let result = pending
        .characteristic
        .as_ref()
        .unwrap()
        .WriteClientCharacteristicConfigurationDescriptorAsync(
            GattClientCharacteristicConfigurationDescriptorValue::Indicate,
        )
        .and_then(|op| op.join());
    if !matches!(result, Ok(GattCommunicationStatus::Success)) {
        return Err(format!("CCCD Indicate write failed: {result:?}; confirm the Android app is advertising and reconnect. If it persists, check Bluetooth pairing and the Windows OS GATT cache"));
    }
    pending.subscribed = true;
    let lost_tx = tx.clone();
    pending.connection_token = Some(
        pending
            .device
            .ConnectionStatusChanged(&TypedEventHandler::<
                BluetoothLEDevice,
                windows::core::IInspectable,
            >::new(move |sender, _| {
                if sender.as_ref().is_some_and(|d| {
                    d.ConnectionStatus().ok() == Some(BluetoothConnectionStatus::Disconnected)
                }) {
                    let _ = lost_tx.send(Command::Lost(session));
                }
                Ok(())
            }))
            .map_err(|e| format!("ConnectionStatusChanged handler registration: {e}"))?,
    );
    let service_tx = tx.clone();
    pending.service_token = Some(
        pending
            .device
            .GattServicesChanged(&TypedEventHandler::<
                BluetoothLEDevice,
                windows::core::IInspectable,
            >::new(move |_, _| {
                let _ = service_tx.send(Command::Lost(session));
                Ok(())
            }))
            .map_err(|e| format!("GattServicesChanged handler registration: {e}"))?,
    );
    if pending
        .device
        .ConnectionStatus()
        .map_err(|e| format!("BLE connection status after CCCD write: {e}"))?
        != BluetoothConnectionStatus::Connected
    {
        return Err("GATT subscription completed but BLE link is disconnected".into());
    }
    status(state, "ready", "Indicate subscription ready");
    Ok(pending.finish(session))
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
    let mut seen: HashMap<String, u64> = HashMap::new();
    let mut link: Option<Link> = None;
    let mut sequence = Sequence::default();
    let mut session = 0_u64;
    let mut scan_epoch = 0_u64;
    let mut next_reconnect = Instant::now();
    loop {
        let command = rx.recv_timeout(Duration::from_millis(250));
        let mut cancel = false;
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
                    cancel = true;
                    stop_watcher(&mut watcher);
                    status(&state, "disconnected", "Bluetooth controller disabled");
                } else {
                    next_reconnect = Instant::now();
                }
            }
            Ok(Command::Scan) => {
                stop_watcher(&mut watcher);
                scan_epoch = scan_epoch.wrapping_add(1);
                seen.clear();
                if let Ok(mut s) = state.lock() {
                    s.candidates.clear();
                }
                match start_scan(tx.clone(), scan_epoch) {
                    Ok(w) => {
                        watcher = Some(w);
                        if link.is_some() {
                            status(&state, "ready", "Subscribed; scanning for another device");
                        } else {
                            status(
                                &state,
                                "scanning",
                                "Searching for PTT service advertisements",
                            );
                        }
                    }
                    Err(e) => status(&state, "error", e),
                }
            }
            Ok(Command::Seen(epoch, address, address_type, name)) => {
                if watcher.is_some() && epoch == scan_epoch && !seen.values().any(|a| *a == address)
                {
                    match BluetoothLEDevice::FromBluetoothAddressWithBluetoothAddressTypeAsync(
                        address,
                        address_type,
                    )
                    .and_then(|op| op.join())
                    .and_then(|d| d.DeviceInformation())
                    {
                        Ok(info) => {
                            if let Ok(id) = info.Id() {
                                let id = id.to_string();
                                seen.insert(id.clone(), address);
                                if let Ok(mut s) = state.lock() {
                                    s.candidates.push(Candidate { id, name });
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[gpt-wrapper] BLE seen device identity unavailable: {e}")
                        }
                    }
                }
            }
            Ok(Command::Approve(id)) => {
                if seen.contains_key(&id) {
                    cancel = true;
                    stop_watcher(&mut watcher);
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
                        device_id: Some(id.clone()),
                        device_name: Some(name.clone()),
                    };
                    match persist(&path, &candidate) {
                        Ok(()) => {
                            approval = candidate;
                            if let Ok(mut s) = state.lock() {
                                s.enabled = true;
                                s.approved_device = Some(Candidate { id, name });
                            }
                            next_reconnect = Instant::now();
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
                cancel = true;
                approval.enabled = false;
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
                cancel = true;
                stop_watcher(&mut watcher);
                seen.clear();
                let candidate = Approval {
                    enabled: false,
                    device_id: None,
                    device_name: None,
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
                if link
                    .as_ref()
                    .is_some_and(|current| current.session == frame_session)
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
            Ok(Command::Lost(lost_session)) => {
                if link
                    .as_ref()
                    .is_some_and(|current| current.session == lost_session)
                {
                    cancel = true;
                    next_reconnect = Instant::now() + RECONNECT_DELAY;
                    status(&state, "disconnected", "BLE link or GATT service lost");
                }
            }
            Ok(Command::Stopped(epoch, reason)) => {
                if watcher.is_some() && epoch == scan_epoch {
                    stop_watcher(&mut watcher);
                    status(&state, "error", reason);
                }
            }
            Ok(Command::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                stop_watcher(&mut watcher);
                dispatch(ControlEvent::Cancel);
                if let Some(old) = link.take() {
                    old.close();
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if cancel {
            sequence.reset();
            dispatch(ControlEvent::Cancel);
            if let Some(old) = link.take() {
                old.close();
            }
        }
        if sequence.orphaned() {
            sequence.reset();
            dispatch(ControlEvent::Cancel);
            if let Some(old) = link.take() {
                old.close();
            }
            next_reconnect = Instant::now() + RECONNECT_DELAY;
            status(&state, "error", "PTT hold exceeded 180 seconds; link reset");
        }
        if approval.enabled
            && link.is_none()
            && approval.device_id.is_some()
            && Instant::now() >= next_reconnect
        {
            let id = approval.device_id.as_ref().unwrap();
            session = session.wrapping_add(1);
            match connect(id, session, &tx, &state) {
                Ok(new_link) => {
                    link = Some(new_link);
                    sequence.reset();
                }
                Err(e) => {
                    status(&state, "error", e);
                    next_reconnect = Instant::now() + RECONNECT_DELAY;
                }
            }
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
                    let name = ad.LocalName().map(|s| s.to_string()).unwrap_or_default();
                    let _ = seen_tx.send(Command::Seen(scan_epoch, address, address_type, name));
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
}
