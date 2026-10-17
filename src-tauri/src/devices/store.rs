use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use zeroize::Zeroize;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PairState {
    Prepared,
    CommittedWaitFinal,
    Paired,
    Quarantined,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    pub device: String,
    pub pair_id: String,
    pub key: Vec<u8>,
    pub ssid: String,
    pub name: String,
    pub state: PairState,
}
impl Drop for Record {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Data {
    pub schema: u32,
    pub selected: Option<String>,
    pub pairs: BTreeMap<String, Record>,
    pub candidates: BTreeMap<String, Record>,
}
impl Default for Data {
    fn default() -> Self {
        Self {
            schema: 1,
            selected: None,
            pairs: BTreeMap::new(),
            candidates: BTreeMap::new(),
        }
    }
}
pub struct Store {
    pub path: PathBuf,
    pub data: Data,
}
#[repr(C)]
struct Blob {
    len: u32,
    ptr: *mut u8,
}
#[link(name = "crypt32")]
unsafe extern "system" {
    fn CryptProtectData(
        input: *const Blob,
        description: *const u16,
        entropy: *const Blob,
        reserved: *const (),
        prompt: *const (),
        flags: u32,
        output: *mut Blob,
    ) -> i32;
    fn CryptUnprotectData(
        input: *const Blob,
        description: *mut *mut u16,
        entropy: *const Blob,
        reserved: *const (),
        prompt: *const (),
        flags: u32,
        output: *mut Blob,
    ) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(ptr: *mut u8) -> *mut u8;
    fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
}
fn protect(input: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    if input.len() > 1024 * 1024 {
        return Err("store_too_large".into());
    }
    let source = Blob {
        len: input.len() as u32,
        ptr: input.as_ptr() as *mut u8,
    };
    let mut dest = Blob {
        len: 0,
        ptr: std::ptr::null_mut(),
    };
    // UI forbidden; no LOCAL_MACHINE flag: only the current Windows user can decrypt.
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &source,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                &mut dest,
            )
        } else {
            CryptUnprotectData(
                &source,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                &mut dest,
            )
        }
    };
    if ok == 0 {
        return Err("dpapi_failed".into());
    }
    let output = unsafe { std::slice::from_raw_parts(dest.ptr, dest.len as usize) }.to_vec();
    unsafe { std::slice::from_raw_parts_mut(dest.ptr, dest.len as usize) }.zeroize();
    unsafe {
        LocalFree(dest.ptr);
    }
    Ok(output)
}
impl Store {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let data = match fs::read(&path) {
            Ok(encrypted) => {
                let mut plaintext = protect(&encrypted, false)?;
                let parsed =
                    serde_json::from_slice::<Data>(&plaintext).map_err(|_| "invalid_pair_store");
                plaintext.zeroize();
                parsed?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(_) => return Err("pair_store_unreadable".into()),
        };
        if data.schema != 1
            || data.pairs.len() > 64
            || data.candidates.len() > 1
            || data
                .pairs
                .values()
                .chain(data.candidates.values())
                .any(|r| r.key.len() != 32 || r.pair_id.len() != 32 || r.device.len() != 12)
        {
            return Err("invalid_pair_store".into());
        }
        let mut store = Self { path, data };
        if !store.data.candidates.is_empty() {
            let mut next = store.data.clone();
            for c in next.candidates.values_mut() {
                c.state = PairState::Quarantined;
            }
            store.save(next)?;
        }
        Ok(store)
    }
    pub fn save(&mut self, next: Data) -> Result<(), String> {
        let mut plain = serde_json::to_vec(&next).map_err(|_| "store_encode")?;
        let protected = protect(&plain, true);
        plain.zeroize();
        let protected = protected?;
        let parent = self.path.parent().ok_or("store_path")?;
        fs::create_dir_all(parent).map_err(|_| "store_directory")?;
        let temp = parent.join(format!(".esp32-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|_| "persist_failed")?;
            file.write_all(&protected)
                .and_then(|_| file.sync_all())
                .map_err(|_| "persist_failed")?;
            drop(file);
            atomic_replace(&temp, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        self.data = next;
        Ok(())
    }
}
fn atomic_replace(temp: &Path, path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let a: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 1 | 8) } == 0 {
        return Err("persist_failed".into());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_user_protection_and_pending_quarantine() {
        let root = std::env::temp_dir().join(format!("gptw-pair-test-{}", uuid::Uuid::new_v4()));
        let path = root.join("pairs.bin");
        let mut s = Store::load(path.clone()).unwrap();
        let mut next = s.data.clone();
        let r = Record {
            device: "AABBCCDDEEFF".into(),
            pair_id: "01".repeat(16),
            key: vec![37; 32],
            ssid: "synthetic".into(),
            name: "test".into(),
            state: PairState::Prepared,
        };
        next.candidates.insert(r.device.clone(), r);
        s.save(next).unwrap();
        let raw = fs::read(&path).unwrap();
        assert!(!raw.windows(32).any(|b| b == [37; 32]));
        let s = Store::load(path.clone()).unwrap();
        assert_eq!(
            s.data.candidates.values().next().unwrap().state,
            PairState::Quarantined
        );
        assert!(s.data.pairs.is_empty());
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
