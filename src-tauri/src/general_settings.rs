use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, sync::Mutex};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PreferredMicrophone {
    Default,
    Device {
        device_id: String,
        label: String,
        #[serde(default)]
        group_id: String,
    },
}

impl Default for PreferredMicrophone {
    fn default() -> Self {
        Self::Default
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeneralSettings {
    #[serde(default = "default_ptt_auto_submit")]
    ptt_auto_submit: bool,
    #[serde(default)]
    preferred_microphone: PreferredMicrophone,
}

const fn default_ptt_auto_submit() -> bool {
    true
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            ptt_auto_submit: default_ptt_auto_submit(),
            preferred_microphone: PreferredMicrophone::default(),
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
        let mut next = current.clone();
        next.ptt_auto_submit = enabled;
        self.persist(&mut current, next)?;
        Ok(current.ptt_auto_submit)
    }

    pub fn preferred_microphone(&self) -> Result<PreferredMicrophone, String> {
        Ok(self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?
            .preferred_microphone
            .clone())
    }

    pub fn set_preferred_microphone(
        &self,
        preference: PreferredMicrophone,
    ) -> Result<PreferredMicrophone, String> {
        if let PreferredMicrophone::Device {
            device_id,
            label,
            group_id,
        } = &preference
        {
            if device_id.is_empty()
                || device_id == "default"
                || device_id.len() > 1024
                || label.trim().is_empty()
                || label.len() > 512
                || group_id.len() > 1024
            {
                return Err("invalid microphone selection".into());
            }
        }
        let mut current = self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?;
        let mut next = current.clone();
        next.preferred_microphone = preference;
        self.persist(&mut current, next)?;
        Ok(current.preferred_microphone.clone())
    }

    fn persist(&self, current: &mut GeneralSettings, next: GeneralSettings) -> Result<(), String> {
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
        Ok(())
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
    fn microphone_preference_persists_and_does_not_reset_auto_submit() {
        let directory = std::env::temp_dir().join(format!("gpt-wrapper-mic-{}", Uuid::new_v4()));
        let path = directory.join("general-settings.json");
        let settings = GeneralSettingsState::load(path.clone());
        assert_eq!(
            settings.preferred_microphone().unwrap(),
            PreferredMicrophone::Default
        );
        settings.set_ptt_auto_submit(false).unwrap();
        let chosen = PreferredMicrophone::Device {
            device_id: "non-default-device".into(),
            label: "External microphone".into(),
            group_id: "hardware-group".into(),
        };
        assert_eq!(
            settings.set_preferred_microphone(chosen.clone()).unwrap(),
            chosen
        );
        assert!(!GeneralSettingsState::load(path.clone())
            .ptt_auto_submit()
            .unwrap());
        assert_eq!(
            GeneralSettingsState::load(path.clone())
                .preferred_microphone()
                .unwrap(),
            chosen
        );
        settings.set_ptt_auto_submit(true).unwrap();
        assert_eq!(settings.preferred_microphone().unwrap(), chosen);
        settings
            .set_preferred_microphone(PreferredMicrophone::Default)
            .unwrap();
        assert_eq!(
            GeneralSettingsState::load(path)
                .preferred_microphone()
                .unwrap(),
            PreferredMicrophone::Default
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_microphone_error_is_ignored_without_losing_selection() {
        let directory =
            std::env::temp_dir().join(format!("gpt-wrapper-mic-legacy-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("general-settings.json");
        fs::write(&path, r#"{"pttAutoSubmit":false,"preferredMicrophone":{"mode":"device","deviceId":"B","label":"External","groupId":"gB"},"microphoneError":"device unplugged"}"#).unwrap();
        let settings = GeneralSettingsState::load(path.clone());
        assert!(!settings.ptt_auto_submit().unwrap());
        assert_eq!(
            settings.preferred_microphone().unwrap(),
            PreferredMicrophone::Device {
                device_id: "B".into(),
                label: "External".into(),
                group_id: "gB".into()
            }
        );
        settings.set_ptt_auto_submit(true).unwrap();
        let raw = fs::read_to_string(path).unwrap();
        assert!(
            !raw.contains("microphoneError"),
            "legacy lock naturally disappears on write"
        );
        assert!(raw.contains("External"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_microphone_preference_and_write_failures_keep_saved_state() {
        let directory = std::env::temp_dir().join(format!("gpt-wrapper-mic-{}", Uuid::new_v4()));
        let path = directory.join("blocked");
        fs::create_dir_all(&path).unwrap();
        let settings = GeneralSettingsState::load(path);
        let invalid = PreferredMicrophone::Device {
            device_id: "default".into(),
            label: "Invalid".into(),
            group_id: String::new(),
        };
        assert!(settings.set_preferred_microphone(invalid).is_err());
        let valid = PreferredMicrophone::Device {
            device_id: "device-1".into(),
            label: "Valid".into(),
            group_id: String::new(),
        };
        assert!(settings.set_preferred_microphone(valid).is_err());
        assert_eq!(
            settings.preferred_microphone().unwrap(),
            PreferredMicrophone::Default
        );
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
