use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, sync::Mutex};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeneralSettings {
    #[serde(default = "default_ptt_auto_submit")]
    ptt_auto_submit: bool,
}

const fn default_ptt_auto_submit() -> bool {
    true
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            ptt_auto_submit: default_ptt_auto_submit(),
        }
    }
}

pub struct GeneralSettingsState {
    path: PathBuf,
    data: Mutex<GeneralSettings>,
}

impl GeneralSettingsState {
    pub fn load(path: PathBuf) -> Self {
        let data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                eprintln!("[gpt-wrapper] failed to parse general settings: {error}");
                GeneralSettings::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                GeneralSettings::default()
            }
            Err(error) => {
                eprintln!("[gpt-wrapper] failed to read general settings: {error}");
                GeneralSettings::default()
            }
        };
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    pub fn ptt_auto_submit(&self) -> Result<bool, String> {
        Ok(self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?
            .ptt_auto_submit)
    }

    pub fn set_ptt_auto_submit(&self, enabled: bool) -> Result<bool, String> {
        let mut current = self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?;
        let next = GeneralSettings {
            ptt_auto_submit: enabled,
        };
        let parent = self
            .path
            .parent()
            .ok_or("general settings path has no parent")?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create settings directory: {error}"))?;
        let temp = parent.join(format!(".general-settings-{}.tmp", Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(&next)
            .map_err(|error| format!("failed to serialize general settings: {error}"))?;
        let result = (|| {
            fs::write(&temp, bytes)
                .map_err(|error| format!("failed to write general settings: {error}"))?;
            fs::rename(&temp, &self.path)
                .map_err(|error| format!("failed to replace general settings: {error}"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        *current = next;
        Ok(current.ptt_auto_submit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_missing_field_are_enabled() {
        let directory =
            std::env::temp_dir().join(format!("gpt-wrapper-general-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("general-settings.json");
        assert!(GeneralSettingsState::load(path.clone())
            .ptt_auto_submit()
            .unwrap());
        fs::write(&path, "{}").unwrap();
        assert!(GeneralSettingsState::load(path.clone())
            .ptt_auto_submit()
            .unwrap());
        fs::write(&path, "not JSON").unwrap();
        assert!(GeneralSettingsState::load(path).ptt_auto_submit().unwrap());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_both_values_and_keeps_runtime_on_write_failure() {
        let directory =
            std::env::temp_dir().join(format!("gpt-wrapper-general-{}", Uuid::new_v4()));
        let path = directory.join("general-settings.json");
        let state = GeneralSettingsState::load(path.clone());
        assert!(!state.set_ptt_auto_submit(false).unwrap());
        assert!(!GeneralSettingsState::load(path.clone())
            .ptt_auto_submit()
            .unwrap());
        assert!(state.set_ptt_auto_submit(true).unwrap());
        assert!(GeneralSettingsState::load(path).ptt_auto_submit().unwrap());
        let blocked = GeneralSettingsState::load(directory.join("blocked"));
        fs::create_dir_all(directory.join("blocked")).unwrap();
        assert!(blocked.set_ptt_auto_submit(false).is_err());
        assert!(blocked.ptt_auto_submit().unwrap());
        fs::remove_dir_all(directory).unwrap();
    }
}
