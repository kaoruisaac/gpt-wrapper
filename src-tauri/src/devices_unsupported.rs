//! ESP32 Devices on platforms without the Windows backend (macOS v1).
//!
//! The Settings UI keeps the same commands and data types, but nothing is
//! started: no BLE scan/pairing, UDP host receiver, Wi-Fi provisioning,
//! workstation monitor or audio bridge, and no `esp32-pairs.dpapi` file is
//! created or read. Every command reports `unsupported_platform`; nothing
//! pretends a device is connected.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Webview};

pub const UNSUPPORTED_PLATFORM: &str = "unsupported_platform";

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    revision: u64,
    receiver: String,
    audio: String,
    selected_id: Option<String>,
    devices: Vec<Value>,
    wizard: Option<Value>,
    error: Option<String>,
}

impl Snapshot {
    fn unsupported() -> Self {
        Self {
            revision: 0,
            receiver: UNSUPPORTED_PLATFORM.into(),
            audio: UNSUPPORTED_PLATFORM.into(),
            selected_id: None,
            devices: Vec::new(),
            wizard: None,
            error: Some(UNSUPPORTED_PLATFORM.into()),
        }
    }
}

/// Managed state placeholder. Holds no device data and runs no workers.
pub struct Devices;

impl Devices {
    pub fn unsupported() -> Self {
        Self
    }

    pub fn stop(&self) {}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Action {
    op: String,
    wizard_id: Option<String>,
    expected_revision: u64,
    args: Value,
}

#[tauri::command]
pub async fn devices_wifi(
    _app: AppHandle,
    webview: Webview,
    _wizard_id: String,
    _expected_revision: u64,
    _consent: bool,
    _ssid_hex: Option<String>,
) -> Result<Value, String> {
    crate::ensure_settings_webview(&webview)?;
    Err(UNSUPPORTED_PLATFORM.into())
}

#[tauri::command]
pub async fn devices_snapshot(_app: AppHandle, webview: Webview) -> Result<Snapshot, String> {
    crate::ensure_settings_webview(&webview)?;
    Ok(Snapshot::unsupported())
}

#[tauri::command]
pub async fn devices_action(
    _app: AppHandle,
    webview: Webview,
    _action: Action,
) -> Result<Snapshot, String> {
    crate::ensure_settings_webview(&webview)?;
    Err(UNSUPPORTED_PLATFORM.into())
}

/// Device PTT routing hooks used by the shared PTT Core. With no device
/// backend there is never a device owner, lease or route, so these are the
/// neutral "no device involved" answers (keyboard PTT is unaffected).
pub mod runtime {
    use tauri::AppHandle;

    /// No device lease can be prepared; a device-owned PTT never starts.
    pub fn prepare_route(_app: &AppHandle, _tab: usize, _generation: usize) -> bool {
        false
    }

    /// None = no device lease for this tab; use the normal microphone policy.
    pub fn route_ready(_app: &AppHandle, _tab: usize) -> Option<bool> {
        None
    }

    pub fn started(_app: &AppHandle, _tab: usize, _generation: usize) {}

    /// Device route messages are never accepted.
    pub fn route_message(_app: &AppHandle, _tab: usize, _value: &serde_json::Value) -> bool {
        false
    }

    pub fn revoke_route(_app: &AppHandle, _finished: Option<(usize, usize)>) {}

    /// Only consulted for a device-owned Finish, which cannot exist here.
    pub fn send_allowed(_app: &AppHandle, _tab: usize, _generation: usize) -> bool {
        false
    }

    pub fn tab_invalidated(_app: &AppHandle, _tab: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reports_unsupported_platform_without_devices() {
        let value = serde_json::to_value(Snapshot::unsupported()).unwrap();
        assert_eq!(value["receiver"], UNSUPPORTED_PLATFORM);
        assert_eq!(value["audio"], UNSUPPORTED_PLATFORM);
        assert_eq!(value["error"], UNSUPPORTED_PLATFORM);
        assert_eq!(value["devices"], serde_json::json!([]));
        assert!(value["selectedId"].is_null());
        assert!(value["wizard"].is_null());
    }
}
