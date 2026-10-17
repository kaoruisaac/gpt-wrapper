use super::wire::*;
use aes::cipher::{KeyIvInit, StreamCipher};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use windows::{
    core::GUID,
    Devices::Bluetooth::{
        Advertisement::{
            BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher,
        },
        BluetoothCacheMode, BluetoothLEDevice,
        GenericAttributeProfile::{
            GattCharacteristic, GattCommunicationStatus, GattDeviceService, GattSession,
            GattSessionStatus,
        },
    },
    Devices::Enumeration::{
        DeviceInformation, DeviceInformationCustomPairing, DevicePairingKinds,
        DevicePairingProtectionLevel, DevicePairingRequestedEventArgs, DevicePairingResultStatus,
    },
    Foundation::TypedEventHandler,
    Storage::Streams::{DataReader, DataWriter},
};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

type Cipher = ctr::Ctr128BE<aes::Aes256>;
const SERVICE: GUID = GUID::from_u128(0x021a9004_0382_4aea_bff4_6b3f1c5adfb4);
const DESCRIPTION: GUID = GUID::from_u128(0x00002901_0000_1000_8000_00805f9b34fb);
// Shared prototype PoP is an explicitly accepted personal-use tradeoff.
const POP: &[u8] = b"24681357";
pub struct Apartment;
impl Apartment {
    pub fn enter() -> Result<Self, String> {
        unsafe {
            windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            )
        }
        .map_err(|_| "winrt_initialization_failed".to_string())?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::System::WinRT::RoUninitialize();
        }
    }
}
fn win<T>(r: windows::core::Result<T>) -> Result<T, String> {
    win_at("operation", r)
}
fn win_at<T>(operation: &str, r: windows::core::Result<T>) -> Result<T, String> {
    // Only stable operation labels and HRESULTs reach Settings. Never include
    // WinRT messages, packet payloads, credentials or key material.
    r.map_err(|e| format!("ble_{operation}_failed_hresult_{:08X}", e.code().0 as u32))
}
fn pair_win<T>(r: windows::core::Result<T>) -> Result<T, String> {
    r.map_err(|e| format!("ble_pair_failed_hresult_{:08X}", e.code().0 as u32))
}
fn pair(device: &BluetoothLEDevice) -> Result<(), String> {
    // Refresh pairing state, while retaining the selected transport identity.
    let id = pair_win(device.DeviceId())?;
    let info = pair_win(pair_win(DeviceInformation::CreateFromIdAsync(&id))?.join())?;
    let pairing = pair_win(info.Pairing())?;
    if pair_win(pairing.IsPaired())? {
        return Ok(());
    }
    if !pair_win(pairing.CanPair())? {
        return Err("ble_pair_failed_cannot_pair".into());
    }
    let custom = pair_win(pairing.Custom())?;
    let token = pair_win(custom.PairingRequested(&TypedEventHandler::<
        DeviceInformationCustomPairing,
        DevicePairingRequestedEventArgs,
    >::new(|_, args| {
        if let Some(args) = args.as_ref() {
            if args.PairingKind()? == DevicePairingKinds::ConfirmOnly {
                args.Accept()?;
            }
        }
        Ok(())
    })))?;
    // Match the validated Bleak WinRT flow. Only an unsupported protection
    // level allows another OS pairing attempt; never bypass failed pairing.
    let outcome = (|| {
        for level in [
            DevicePairingProtectionLevel::EncryptionAndAuthentication,
            DevicePairingProtectionLevel::Encryption,
            DevicePairingProtectionLevel::Default,
        ] {
            let operation = if level == DevicePairingProtectionLevel::Default {
                custom.PairAsync(DevicePairingKinds::ConfirmOnly)
            } else {
                custom.PairWithProtectionLevelAsync(DevicePairingKinds::ConfirmOnly, level)
            };
            let result = pair_win(pair_win(operation)?.join())?;
            let status = pair_win(result.Status())?;
            if matches!(
                status,
                DevicePairingResultStatus::Paired | DevicePairingResultStatus::AlreadyPaired
            ) {
                return Ok(());
            }
            if status != DevicePairingResultStatus::ProtectionLevelCouldNotBeMet
                || level == DevicePairingProtectionLevel::Default
            {
                return Err(format!(
                    "ble_pair_failed_status_{}_level_{}",
                    status.0, level.0
                ));
            }
        }
        unreachable!("default pairing always returns an outcome");
    })();
    let cleanup = pair_win(custom.RemovePairingRequested(token));
    outcome?;
    cleanup
}
fn read(b: &windows::Storage::Streams::IBuffer) -> Result<Vec<u8>, String> {
    let mut out = vec![0; win(b.Length())? as usize];
    win(win(DataReader::FromBuffer(b))?.ReadBytes(&mut out))?;
    Ok(out)
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub id: String,
    pub name: String,
    pub address: u64,
}
pub fn scan() -> Result<Vec<Target>, String> {
    let found = Arc::new(Mutex::new(HashMap::<u64, Target>::new()));
    let shared = found.clone();
    let w = win(BluetoothLEAdvertisementWatcher::new())?;
    win(w.SetScanningMode(
        windows::Devices::Bluetooth::Advertisement::BluetoothLEScanningMode::Active,
    ))?;
    let token = win(w.Received(&TypedEventHandler::<
        BluetoothLEAdvertisementWatcher,
        BluetoothLEAdvertisementReceivedEventArgs,
    >::new(move |_, args| {
        if let Some(a) = args.as_ref() {
            let name = a.Advertisement()?.LocalName()?.to_string();
            if name.starts_with("PROV_GPT_") {
                let address = a.BluetoothAddress()?;
                if let Ok(mut f) = shared.lock() {
                    if f.len() < 64 {
                        f.insert(
                            address,
                            Target {
                                id: format!("{address:012X}"),
                                name,
                                address,
                            },
                        );
                    }
                }
            }
        }
        Ok(())
    })))?;
    win(w.Start())?;
    thread::sleep(Duration::from_secs(5));
    let result = win(w.Stop());
    let _ = w.RemoveReceived(token);
    result?;
    let mut out: Vec<_> = found
        .lock()
        .map_err(|_| "ble_scan_lock")?
        .values()
        .cloned()
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}
fn gatt_failure(operation: &str, status: GattCommunicationStatus, protocol: Option<u8>) -> String {
    let att = protocol
        .map(|value| format!("{value:02X}"))
        .unwrap_or_else(|| "none".into());
    format!("ble_{operation}_failed_status_{}_att_{att}", status.0)
}
fn retry_discovery(reason: &str) -> bool {
    reason == "ble_session_inactive"
        || reason
            == format!(
                "ble_service_failed_status_{}",
                GattCommunicationStatus::Unreachable.0
            )
        || (reason.starts_with("ble_get_services_failed_hresult_") && reason.ends_with("80070016"))
}
fn discover_with_retry<T>(
    mut discover: impl FnMut() -> Result<T, String>,
    mut backoff: impl FnMut(),
) -> Result<T, String> {
    match discover() {
        Ok(link) => Ok(link),
        Err(reason) if retry_discovery(&reason) => {
            backoff();
            discover().map_err(|reason| format!("{reason}_attempt_2"))
        }
        Err(reason) => Err(reason),
    }
}
pub struct Link {
    device: BluetoothLEDevice,
    session: Option<GattSession>,
    services: Vec<GattDeviceService>,
    endpoints: HashMap<String, GattCharacteristic>,
    cipher: Option<Cipher>,
    id: u32,
    pub tx: Option<String>,
}
impl Drop for Link {
    fn drop(&mut self) {
        self.endpoints.clear();
        for s in &self.services {
            let _ = s.Close();
        }
        if let Some(session) = &self.session {
            let _ = session.SetMaintainConnection(false);
            let _ = session.Close();
        }
        let _ = self.device.Close();
    }
}
impl Link {
    pub fn usable(&self) -> bool {
        self.cipher.is_some()
    }
    pub fn connect(target: &Target) -> Result<Self, String> {
        let mut link = discover_with_retry(
            || Self::discover(target),
            || thread::sleep(Duration::from_secs(1)),
        )?;
        // No retry may encompass this handshake: it starts the CTR session.
        link.secure()?;
        Ok(link)
    }
    fn discover(target: &Target) -> Result<Self, String> {
        let device = win_at(
            "open_device",
            win_at(
                "open_device",
                BluetoothLEDevice::FromBluetoothAddressAsync(target.address),
            )?
            .join(),
        )?;
        // Own every handle immediately, including on discovery/pairing failure.
        let mut link = Self {
            device,
            session: None,
            services: Vec::new(),
            endpoints: HashMap::new(),
            cipher: None,
            id: 0,
            tx: None,
        };
        let device_id = win_at("session_device_id", link.device.BluetoothDeviceId())?;
        link.session = Some(win_at(
            "create_session",
            win_at("create_session", GattSession::FromDeviceIdAsync(&device_id))?.join(),
        )?);
        let session = link.session.as_ref().unwrap();
        if !win_at("session_can_maintain", session.CanMaintainConnection())? {
            return Err("ble_session_cannot_maintain".into());
        }
        win_at("session_maintain", session.SetMaintainConnection(true))?;
        let result = win_at(
            "get_services",
            win_at(
                "get_services",
                link.device
                    .GetGattServicesWithCacheModeAsync(BluetoothCacheMode::Uncached),
            )?
            .join(),
        )?;
        let status = win_at("services_status", result.Status())?;
        if status != GattCommunicationStatus::Success {
            return Err(format!("ble_service_failed_status_{}", status.0));
        }
        // Uncached discovery initiates the connection; waiting before it can
        // deadlock on devices which only connect after a GATT request.
        for service in win_at("services_collection", result.Services())? {
            link.services.push(service);
        }
        let active_until = std::time::Instant::now() + Duration::from_secs(5);
        while win_at("session_status", session.SessionStatus())? != GattSessionStatus::Active {
            if std::time::Instant::now() >= active_until {
                return Err("ble_session_inactive".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
        // The prototype connects/discovers first, then performs OS pairing.
        pair(&link.device)?;
        for service in &link.services {
            if win_at("service_uuid", service.Uuid())? != SERVICE {
                continue;
            }
            let chars = win_at(
                "get_characteristics",
                win_at(
                    "get_characteristics",
                    service.GetCharacteristicsWithCacheModeAsync(BluetoothCacheMode::Uncached),
                )?
                .join(),
            )?;
            if win_at("characteristics_status", chars.Status())? != GattCommunicationStatus::Success
            {
                return Err("ble_characteristics_failed".into());
            }
            for ch in win_at("characteristics_collection", chars.Characteristics())? {
                let ds = win_at(
                    "get_descriptors",
                    win_at(
                        "get_descriptors",
                        ch.GetDescriptorsForUuidWithCacheModeAsync(
                            DESCRIPTION,
                            BluetoothCacheMode::Uncached,
                        ),
                    )?
                    .join(),
                )?;
                if win_at("descriptors_status", ds.Status())? != GattCommunicationStatus::Success {
                    continue;
                }
                for d in win_at("descriptors_collection", ds.Descriptors())? {
                    let r = win_at(
                        "read_descriptor",
                        win_at(
                            "read_descriptor",
                            d.ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached),
                        )?
                        .join(),
                    )?;
                    if win_at("descriptor_status", r.Status())? != GattCommunicationStatus::Success
                    {
                        continue;
                    }
                    if let Ok(name) =
                        String::from_utf8(read(&win_at("descriptor_value", r.Value())?)?)
                    {
                        link.endpoints.insert(name, ch.clone());
                    }
                }
            }
        }
        for endpoint in [
            "prov-session",
            "prov-scan",
            "prov-config",
            "prov-ctrl",
            "gptw-setup",
        ] {
            if !link.endpoints.contains_key(endpoint) {
                return Err("E21_missing_endpoint".into());
            }
        }
        Ok(link)
    }
    fn exchange(&self, ep: &str, data: &[u8]) -> Result<Vec<u8>, String> {
        self.exchange_inner(ep, data)
            .map_err(|reason| format!("ble_endpoint_{ep}_{reason}"))
    }
    fn exchange_inner(&self, ep: &str, data: &[u8]) -> Result<Vec<u8>, String> {
        let ch = self.endpoints.get(ep).ok_or("ble_missing_endpoint")?;
        let writer = win(DataWriter::new())?;
        win(writer.WriteBytes(data))?;
        let result = win_at(
            "write_characteristic",
            win_at(
                "write_characteristic",
                ch.WriteValueWithResultAsync(&win(writer.DetachBuffer())?),
            )?
            .join(),
        )?;
        let status = win_at("write_status", result.Status())?;
        if status != GattCommunicationStatus::Success {
            let protocol = result
                .ProtocolError()
                .ok()
                .and_then(|value| value.Value().ok());
            return Err(gatt_failure("write", status, protocol));
        }
        let r = win_at(
            "read_characteristic",
            win_at(
                "read_characteristic",
                ch.ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached),
            )?
            .join(),
        )?;
        let status = win_at("read_status", r.Status())?;
        if status != GattCommunicationStatus::Success {
            let protocol = r.ProtocolError().ok().and_then(|value| value.Value().ok());
            return Err(gatt_failure("read", status, protocol));
        }
        read(&win_at("characteristic_value", r.Value())?)
    }
    fn secure(&mut self) -> Result<(), String> {
        self.cipher = Some(negotiate(
            |data| self.exchange("prov-session", data),
            random::<32>(),
        )?);
        Ok(())
    }
    pub fn encrypted(&mut self, ep: &str, mut data: Vec<u8>) -> Result<Vec<u8>, String> {
        self.cipher
            .as_mut()
            .ok_or("security_closed")?
            .apply_keystream(&mut data);
        // Any error consumes the session. Never replay CTR ciphertext.
        let response = self.exchange(ep, &data);
        data.zeroize();
        match response {
            Ok(mut r) => {
                self.cipher
                    .as_mut()
                    .ok_or("security_closed")?
                    .apply_keystream(&mut r);
                Ok(r)
            }
            Err(e) => {
                self.cipher = None;
                Err(e)
            }
        }
    }
    pub fn setup(
        &mut self,
        op: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.id = self
            .id
            .checked_add(1)
            .filter(|x| *x < 0xffff_fffe)
            .ok_or("session_request_limit")?;
        let mut encoded = serde_json::to_vec(
            &serde_json::json!({"v":1,"id":self.id,"op":op,"tx":self.tx,"args":args}),
        )
        .map_err(|_| "setup_encode")?;
        if encoded.len() > 2048 {
            encoded.zeroize();
            return Err("setup_length".into());
        }
        let mut raw = self
            .encrypted("gptw-setup", encoded)
            .map_err(|reason| format!("{reason}_op_{op}"))?;
        let parsed = serde_json::from_slice::<serde_json::Value>(&raw).map_err(|_| "setup_decode");
        raw.zeroize();
        let r = parsed?;
        if r["v"] != 1 || r["id"] != self.id {
            return Err("setup_response_identity".into());
        }
        if !matches!(op, "BEGIN" | "HELLO") && r["tx"].as_str() != self.tx.as_deref() {
            return Err("setup_transaction_mismatch".into());
        }
        if r["result"] == "REJECTED" {
            return Err("setup_rejected".into());
        }
        Ok(r)
    }
}

fn negotiate(
    mut exchange: impl FnMut(&[u8]) -> Result<Vec<u8>, String>,
    mut secret_bytes: [u8; 32],
) -> Result<Cipher, String> {
    let secret = StaticSecret::from(secret_bytes);
    secret_bytes.zeroize();
    let public = PublicKey::from(&secret);
    let request = [
        num(2, 1),
        blob(11, &message(0, 20, &blob(1, public.as_bytes()))),
    ]
    .concat();
    let response = exchange(&request)?;
    if number(&response, 2)? != 1 {
        return Err("security_scheme".into());
    }

    let payload = bytes(&response, 11)?;
    if number(payload, 1)? != 1 {
        return Err("security_response_type".into());
    }
    let reply = bytes(payload, 21)?;
    if number(reply, 1)? != 0 {
        return Err("security_rejected".into());
    }
    let peer: [u8; 32] = bytes(reply, 2)?
        .try_into()
        .map_err(|_| "security_key_size")?;
    let iv: [u8; 16] = bytes(reply, 3)?
        .try_into()
        .map_err(|_| "security_iv_size")?;
    let shared = secret.diffie_hellman(&PublicKey::from(peer));
    if !shared.was_contributory() {
        return Err("security_low_order_key".into());
    }
    let mut key = shared.to_bytes();
    let digest = Sha256::digest(POP);
    for (a, b) in key.iter_mut().zip(digest) {
        *a ^= b;
    }
    let mut cipher = Cipher::new((&key).into(), (&iv).into());
    key.zeroize();
    let mut proof = peer;
    cipher.apply_keystream(&mut proof);
    let request = [num(2, 1), blob(11, &message(2, 22, &blob(2, &proof)))].concat();
    let response = exchange(&request)?;
    if number(&response, 2)? != 1 {
        return Err("security_scheme".into());
    }
    let payload = bytes(&response, 11)?;
    if number(payload, 1)? != 3 {
        return Err("security_response_type".into());
    }
    let reply = bytes(payload, 23)?;
    if number(reply, 1)? != 0 {
        return Err("security_rejected".into());
    }
    let mut proof = bytes(reply, 3)?.to_vec();
    cipher.apply_keystream(&mut proof);
    if proof != public.as_bytes() {
        return Err("security_device_proof".into());
    }
    Ok(cipher)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_retry_is_bounded_and_never_replays_security_failures() {
        let mut attempts = 0;
        let mut waits = 0;
        let failure = discover_with_retry::<()>(
            || {
                attempts += 1;
                Err("ble_get_services_failed_hresult_80070016".into())
            },
            || {
                waits += 1;
            },
        )
        .unwrap_err();
        assert_eq!((attempts, waits), (2, 1));
        assert!(failure.ends_with("_attempt_2"));
        for reason in [
            "ble_pair_failed_status_9_level_3",
            "security_device_proof",
            "E21_missing_endpoint",
            "ble_endpoint_prov-session_ble_read_failed",
        ] {
            let mut attempts = 0;
            let outcome = discover_with_retry::<()>(
                || {
                    attempts += 1;
                    Err(reason.into())
                },
                || panic!("non-discovery failures must not be retried"),
            );
            assert_eq!(attempts, 1);
            assert_eq!(outcome.unwrap_err(), reason);
        }
    }
    #[test]
    fn diagnostics_keep_hresult_and_operation_without_native_message() {
        let native = windows::core::Error::new(
            windows::core::HRESULT(0x80070005u32 as i32),
            "synthetic sensitive native message",
        );
        let failure = win_at::<()>("get_services", Err(native)).unwrap_err();
        assert_eq!(failure, "ble_get_services_failed_hresult_80070005");
        assert!(!failure.contains("sensitive"));
    }
    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../../tests/fixtures/security1.json")).unwrap()
    }
    fn value(f: &serde_json::Value, k: &str, n: usize) -> Vec<u8> {
        unhex(f[k].as_str().unwrap(), n).unwrap()
    }
    #[test]
    fn official_security1_handshake_and_continuous_ctr_match_openssl() {
        let f = fixture();
        let mut request = 0;
        let mut cipher = negotiate(
            |data| {
                assert_eq!(number(data, 2).unwrap(), 1);
                let payload = bytes(data, 11).unwrap();
                let response = if request == 0 {
                    assert_eq!(
                        bytes(bytes(payload, 20).unwrap(), 1).unwrap(),
                        value(&f, "clientPublic", 32)
                    );
                    message(
                        1,
                        21,
                        &[
                            blob(2, &value(&f, "devicePublic", 32)),
                            blob(3, &value(&f, "iv", 16)),
                        ]
                        .concat(),
                    )
                } else {
                    assert_eq!(number(payload, 1).unwrap(), 2);
                    assert_eq!(
                        bytes(bytes(payload, 22).unwrap(), 2).unwrap(),
                        value(&f, "proof0", 32)
                    );
                    message(3, 23, &blob(3, &value(&f, "proof1", 32)))
                };
                request += 1;
                Ok([num(2, 1), blob(11, &response)].concat())
            },
            value(&f, "clientSecret", 32).try_into().unwrap(),
        )
        .unwrap();
        let mut continuation = b"synthetic STATUS request then response".to_vec();
        cipher.apply_keystream(&mut continuation);
        assert_eq!(continuation, value(&f, "continuation", continuation.len()));
        assert_eq!(request, 2);
    }
    #[test]
    fn rejects_low_order_peer_before_issuing_client_proof() {
        let result = negotiate(
            |_| {
                Ok([
                    num(2, 1),
                    blob(
                        11,
                        &message(1, 21, &[blob(2, &[0; 32]), blob(3, &[0; 16])].concat()),
                    ),
                ]
                .concat())
            },
            [1; 32],
        );
        assert!(matches!(result,Err(e) if e=="security_low_order_key"));
    }
}
