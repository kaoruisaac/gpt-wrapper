//! Native WLAN queries. Never export profiles or passwords to disk or a subprocess.
use super::wire::hex;
use serde_json::{json, Value};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{Foundation::HANDLE, NetworkManagement::WiFi::*},
};
use zeroize::Zeroize;
struct Client(HANDLE);
impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            WlanCloseHandle(self.0, None);
        }
    }
}
struct Memory(*mut std::ffi::c_void);
impl Drop for Memory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                WlanFreeMemory(self.0);
            }
        }
    }
}
fn client() -> Result<Client, String> {
    let mut h = HANDLE::default();
    let mut version = 0;
    if unsafe { WlanOpenHandle(2, None, &mut version, &mut h) } != 0 {
        return Err("wlan_unavailable_use_manual_entry".into());
    }
    Ok(Client(h))
}
fn interfaces(c: &Client) -> Result<Vec<windows::core::GUID>, String> {
    let mut p = std::ptr::null_mut();
    if unsafe { WlanEnumInterfaces(c.0, None, &mut p) } != 0 {
        return Err("wlan_interfaces_unavailable".into());
    }
    let _m = Memory(p.cast());
    if p.is_null() {
        return Err("wlan_interfaces_unavailable".into());
    }
    let count = unsafe { (*p).dwNumberOfItems }.min(64) as usize;
    Ok(
        unsafe { std::slice::from_raw_parts((*p).InterfaceInfo.as_ptr(), count) }
            .iter()
            .map(|i| i.InterfaceGuid)
            .collect(),
    )
}
pub fn recommendation() -> Result<Value, String> {
    let c = client()?;
    for guid in interfaces(&c)? {
        let mut p = std::ptr::null_mut();
        let mut size = 0;
        if unsafe {
            WlanQueryInterface(
                c.0,
                &guid,
                wlan_intf_opcode_current_connection,
                None,
                &mut size,
                &mut p,
                None,
            )
        } != 0
        {
            continue;
        }
        let _m = Memory(p);
        if size < std::mem::size_of::<WLAN_CONNECTION_ATTRIBUTES>() as u32 || p.is_null() {
            continue;
        }
        let connection = unsafe { &*p.cast::<WLAN_CONNECTION_ATTRIBUTES>() };
        let ssid = &connection.wlanAssociationAttributes.dot11Ssid;
        if ssid.uSSIDLength > 32 {
            continue;
        }
        let bytes = &ssid.ucSSID[..ssid.uSSIDLength as usize];
        return Ok(json!({"ssidHex":hex(bytes),"name":String::from_utf8_lossy(bytes)}));
    }
    Ok(Value::Null) // Ethernet, disconnected WLAN and access-denied all retain manual selection.
}
pub fn password(ssid_hex: &str, consent: bool) -> Result<String, String> {
    if !consent {
        return Err("saved_password_consent_required".into());
    }
    let c = client()?;
    for guid in interfaces(&c)? {
        let mut profiles = std::ptr::null_mut();
        if unsafe { WlanGetProfileList(c.0, &guid, None, &mut profiles) } != 0 || profiles.is_null()
        {
            continue;
        }
        let _m = Memory(profiles.cast());
        for profile in unsafe {
            std::slice::from_raw_parts(
                (*profiles).ProfileInfo.as_ptr(),
                (*profiles).dwNumberOfItems.min(256) as usize,
            )
        } {
            let mut xml = PWSTR::null();
            let mut flags = 4u32;
            if unsafe {
                WlanGetProfile(
                    c.0,
                    &guid,
                    PCWSTR(profile.strProfileName.as_ptr()),
                    None,
                    &mut xml,
                    Some(&mut flags),
                    None,
                )
            } != 0
                || xml.is_null()
            {
                continue;
            }
            let allocation = Memory(xml.0.cast());
            let mut contents = unsafe { xml.to_string() }.map_err(|_| "profile_decode")?;
            let result = (|| {
                let doc = roxmltree::Document::parse(&contents).ok()?;
                let ssid = doc.descendants().find(|n| n.has_tag_name("SSID"))?;
                let h = ssid.children().find(|n| n.has_tag_name("hex"))?.text()?;
                if !h.eq_ignore_ascii_case(ssid_hex) {
                    return None;
                }
                let shared = doc.descendants().find(|n| n.has_tag_name("sharedKey"))?;
                if shared
                    .children()
                    .find(|n| n.has_tag_name("protected"))?
                    .text()
                    != Some("false")
                {
                    return None;
                }
                Some(
                    shared
                        .children()
                        .find(|n| n.has_tag_name("keyMaterial"))?
                        .text()?
                        .to_owned(),
                )
            })();
            contents.zeroize();
            unsafe {
                let n = xml.len();
                std::slice::from_raw_parts_mut(xml.0, n).zeroize();
            }
            drop(allocation);
            if let Some(password) = result {
                return Ok(password);
            }
        }
    }
    Err("saved_password_unavailable_use_manual_entry".into())
}
