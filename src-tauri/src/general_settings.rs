use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf, sync::Mutex};
use uuid::Uuid;

use crate::provider::Provider;

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

/// A provider-origin device ID the user explicitly confirmed for the shared
/// microphone choice. WebView2 device IDs are per origin, so a ChatGPT ID is
/// never reused on Claude (or the reverse).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmedMicrophone {
    pub device_id: String,
    pub label: String,
    #[serde(default)]
    pub group_id: String,
}

/// The capture policy sent to one tab. `resolve` means no ID is known for the
/// tab's origin: the page must match the chosen device by its exact label in
/// its own inventory and fail closed when that match is missing or ambiguous.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MicrophonePolicy {
    Default,
    Device {
        device_id: String,
        label: String,
        group_id: String,
        resolve: bool,
    },
}

/// How a provider obtains the device ID for the shared choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MicrophoneBinding {
    Default,
    Selected,
    Confirmed,
    NeedsConfirmation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeneralSettings {
    #[serde(default = "default_notification_sound_enabled")]
    notification_sound_enabled: bool,
    #[serde(default = "default_ptt_auto_submit")]
    ptt_auto_submit: bool,
    #[serde(default)]
    preferred_microphone: PreferredMicrophone,
    // Older installs have no value and fall back to ChatGPT.
    #[serde(default, deserialize_with = "lenient_provider")]
    active_provider: Provider,
    // The provider origin the preferred device ID was enumerated in. Legacy
    // selections came from ChatGPT.
    #[serde(default, deserialize_with = "lenient_provider")]
    microphone_source: Provider,
    #[serde(default, deserialize_with = "lenient_confirmations")]
    microphone_confirmations: BTreeMap<Provider, ConfirmedMicrophone>,
}

fn lenient_confirmations<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<Provider, ConfirmedMicrophone>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

impl GeneralSettings {
    fn microphone_policy(&self, provider: Provider) -> MicrophonePolicy {
        match &self.preferred_microphone {
            PreferredMicrophone::Default => MicrophonePolicy::Default,
            PreferredMicrophone::Device {
                device_id,
                label,
                group_id,
            } => {
                if self.microphone_source == provider {
                    return MicrophonePolicy::Device {
                        device_id: device_id.clone(),
                        label: label.clone(),
                        group_id: group_id.clone(),
                        resolve: false,
                    };
                }
                match self
                    .microphone_confirmations
                    .get(&provider)
                    .filter(|confirmed| confirmed.label == *label)
                {
                    Some(confirmed) => MicrophonePolicy::Device {
                        device_id: confirmed.device_id.clone(),
                        label: label.clone(),
                        group_id: confirmed.group_id.clone(),
                        resolve: false,
                    },
                    None => MicrophonePolicy::Device {
                        device_id: String::new(),
                        label: label.clone(),
                        group_id: String::new(),
                        resolve: true,
                    },
                }
            }
        }
    }

    fn microphone_binding(&self, provider: Provider) -> MicrophoneBinding {
        match self.microphone_policy(provider) {
            MicrophonePolicy::Default => MicrophoneBinding::Default,
            MicrophonePolicy::Device { resolve: true, .. } => MicrophoneBinding::NeedsConfirmation,
            MicrophonePolicy::Device { .. } if self.microphone_source == provider => {
                MicrophoneBinding::Selected
            }
            MicrophonePolicy::Device { .. } => MicrophoneBinding::Confirmed,
        }
    }
}

// An unknown provider ID must not discard the other saved preferences.
fn lenient_provider<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Provider, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

const fn default_notification_sound_enabled() -> bool {
    true
}

const fn default_ptt_auto_submit() -> bool {
    true
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            notification_sound_enabled: default_notification_sound_enabled(),
            ptt_auto_submit: default_ptt_auto_submit(),
            preferred_microphone: PreferredMicrophone::default(),
            active_provider: Provider::default(),
            microphone_source: Provider::default(),
            microphone_confirmations: BTreeMap::new(),
        }
    }
}

pub struct GeneralSettingsState {
    path: PathBuf,
    data: Mutex<GeneralSettings>,
}

impl GeneralSettingsState {
    pub fn notification_sound_enabled(&self) -> Result<bool, String> {
        Ok(self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?
            .notification_sound_enabled)
    }

    pub fn set_notification_sound_enabled(&self, enabled: bool) -> Result<bool, String> {
        let mut current = self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?;
        let mut next = current.clone();
        next.notification_sound_enabled = enabled;
        self.persist(&mut current, next)?;
        Ok(current.notification_sound_enabled)
    }

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

    pub fn active_provider(&self) -> Result<Provider, String> {
        Ok(self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?
            .active_provider)
    }

    pub fn set_active_provider(&self, provider: Provider) -> Result<Provider, String> {
        let mut current = self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?;
        if current.active_provider == provider {
            return Ok(provider);
        }
        let mut next = current.clone();
        next.active_provider = provider;
        self.persist(&mut current, next)?;
        Ok(current.active_provider)
    }

    pub fn preferred_microphone(&self) -> Result<PreferredMicrophone, String> {
        Ok(self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?
            .preferred_microphone
            .clone())
    }

    pub fn microphone_policy(&self, provider: Provider) -> Result<MicrophonePolicy, String> {
        Ok(self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?
            .microphone_policy(provider))
    }

    pub fn microphone_binding(&self, provider: Provider) -> Result<MicrophoneBinding, String> {
        Ok(self
            .data
            .lock()
            .map_err(|_| "general settings lock was poisoned".to_string())?
            .microphone_binding(provider))
    }

    #[cfg(test)]
    pub fn set_preferred_microphone(
        &self,
        preference: PreferredMicrophone,
    ) -> Result<PreferredMicrophone, String> {
        self.set_microphone_for(Provider::Chatgpt, preference)
    }

    /// Saves a choice made from `provider`'s own inventory. Choosing the same
    /// device name from another provider confirms that origin's ID without
    /// changing the shared choice; any other device replaces the choice and
    /// drops confirmations that belonged to the previous device.
    pub fn set_microphone_for(
        &self,
        provider: Provider,
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
        match (&current.preferred_microphone, &preference) {
            (_, PreferredMicrophone::Default) => {
                next.preferred_microphone = PreferredMicrophone::Default;
                next.microphone_source = provider;
                next.microphone_confirmations.clear();
            }
            (
                PreferredMicrophone::Device { label: old, .. },
                PreferredMicrophone::Device {
                    device_id,
                    label,
                    group_id,
                },
            ) if old == label && current.microphone_source != provider => {
                next.microphone_confirmations.insert(
                    provider,
                    ConfirmedMicrophone {
                        device_id: device_id.clone(),
                        label: label.clone(),
                        group_id: group_id.clone(),
                    },
                );
            }
            (old, PreferredMicrophone::Device { label, .. }) => {
                let same_device =
                    matches!(old, PreferredMicrophone::Device { label: l, .. } if l == label);
                next.preferred_microphone = preference.clone();
                next.microphone_source = provider;
                next.microphone_confirmations.remove(&provider);
                if !same_device {
                    next.microphone_confirmations.clear();
                }
            }
        }
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
    fn notification_sound_defaults_persists_and_preserves_other_preferences() {
        let directory = std::env::temp_dir().join(format!("gpt-wrapper-sound-{}", Uuid::new_v4()));
        let path = directory.join("general-settings.json");
        assert!(GeneralSettingsState::load(path.clone())
            .notification_sound_enabled()
            .unwrap());
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, r#"{"pttAutoSubmit":false,"preferredMicrophone":{"mode":"device","deviceId":"mic","label":"External","groupId":"group"}}"#).unwrap();
        let state = GeneralSettingsState::load(path.clone());
        assert!(state.notification_sound_enabled().unwrap());
        let microphone = state.preferred_microphone().unwrap();
        assert!(!state.set_notification_sound_enabled(false).unwrap());
        let loaded = GeneralSettingsState::load(path.clone());
        assert!(!loaded.notification_sound_enabled().unwrap());
        assert!(!loaded.ptt_auto_submit().unwrap());
        assert_eq!(loaded.preferred_microphone().unwrap(), microphone);
        loaded.set_ptt_auto_submit(true).unwrap();
        loaded
            .set_preferred_microphone(PreferredMicrophone::Default)
            .unwrap();
        assert!(!GeneralSettingsState::load(path.clone())
            .notification_sound_enabled()
            .unwrap());
        assert!(loaded.set_notification_sound_enabled(true).unwrap());
        assert!(GeneralSettingsState::load(path)
            .notification_sound_enabled()
            .unwrap());
        let blocked = directory.join("blocked");
        fs::create_dir_all(&blocked).unwrap();
        let state = GeneralSettingsState::load(blocked);
        assert!(state.set_notification_sound_enabled(false).is_err());
        assert!(state.notification_sound_enabled().unwrap());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn active_provider_defaults_to_chatgpt_and_persists_without_touching_others() {
        let directory =
            std::env::temp_dir().join(format!("gpt-wrapper-provider-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("general-settings.json");
        assert_eq!(
            GeneralSettingsState::load(path.clone()).active_provider().unwrap(),
            Provider::Chatgpt
        );
        // Upgrade from a settings file written before providers existed.
        fs::write(&path, r#"{"pttAutoSubmit":false,"notificationSoundEnabled":false}"#).unwrap();
        let state = GeneralSettingsState::load(path.clone());
        assert_eq!(state.active_provider().unwrap(), Provider::Chatgpt);
        assert_eq!(state.set_active_provider(Provider::Claude).unwrap(), Provider::Claude);
        let loaded = GeneralSettingsState::load(path.clone());
        assert_eq!(loaded.active_provider().unwrap(), Provider::Claude);
        assert!(!loaded.ptt_auto_submit().unwrap());
        assert!(!loaded.notification_sound_enabled().unwrap());
        assert!(fs::read_to_string(&path).unwrap().contains(r#""activeProvider": "claude""#));
        // Unknown provider IDs must not crash or silently pick Claude.
        fs::write(&path, r#"{"activeProvider":"gemini","pttAutoSubmit":false}"#).unwrap();
        let unknown = GeneralSettingsState::load(path);
        assert_eq!(unknown.active_provider().unwrap(), Provider::Chatgpt);
        assert!(!unknown.ptt_auto_submit().unwrap());
        let blocked = directory.join("blocked");
        fs::create_dir_all(&blocked).unwrap();
        let state = GeneralSettingsState::load(blocked);
        assert!(state.set_active_provider(Provider::Claude).is_err());
        assert_eq!(state.active_provider().unwrap(), Provider::Chatgpt);
        fs::remove_dir_all(directory).unwrap();
    }

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
    fn microphone_ids_are_never_reused_across_provider_origins() {
        let directory =
            std::env::temp_dir().join(format!("gpt-wrapper-mic-origin-{}", Uuid::new_v4()));
        let path = directory.join("general-settings.json");
        fs::create_dir_all(&directory).unwrap();
        // A legacy selection was enumerated by ChatGPT.
        fs::write(&path, r#"{"preferredMicrophone":{"mode":"device","deviceId":"gpt-B","label":"USB B","groupId":"gB"}}"#).unwrap();
        let settings = GeneralSettingsState::load(path.clone());
        assert_eq!(
            settings.microphone_policy(Provider::Chatgpt).unwrap(),
            MicrophonePolicy::Device {
                device_id: "gpt-B".into(),
                label: "USB B".into(),
                group_id: "gB".into(),
                resolve: false
            }
        );
        assert_eq!(
            settings.microphone_policy(Provider::Claude).unwrap(),
            MicrophonePolicy::Device {
                device_id: String::new(),
                label: "USB B".into(),
                group_id: String::new(),
                resolve: true
            },
            "the ChatGPT ID must not be sent to Claude"
        );
        assert_eq!(
            settings.microphone_binding(Provider::Claude).unwrap(),
            MicrophoneBinding::NeedsConfirmation
        );
        // Confirming the same device from the Claude inventory keeps the choice.
        settings
            .set_microphone_for(
                Provider::Claude,
                PreferredMicrophone::Device {
                    device_id: "claude-B".into(),
                    label: "USB B".into(),
                    group_id: "cB".into(),
                },
            )
            .unwrap();
        let reloaded = GeneralSettingsState::load(path.clone());
        assert_eq!(
            reloaded.microphone_policy(Provider::Claude).unwrap(),
            MicrophonePolicy::Device {
                device_id: "claude-B".into(),
                label: "USB B".into(),
                group_id: "cB".into(),
                resolve: false
            }
        );
        assert_eq!(
            reloaded.microphone_binding(Provider::Chatgpt).unwrap(),
            MicrophoneBinding::Selected
        );
        assert_eq!(
            reloaded.microphone_binding(Provider::Claude).unwrap(),
            MicrophoneBinding::Confirmed
        );
        // Choosing another device from Claude replaces the shared choice and
        // drops the stale ChatGPT binding.
        reloaded
            .set_microphone_for(
                Provider::Claude,
                PreferredMicrophone::Device {
                    device_id: "claude-A".into(),
                    label: "USB A".into(),
                    group_id: "cA".into(),
                },
            )
            .unwrap();
        assert!(matches!(
            reloaded.microphone_policy(Provider::Chatgpt).unwrap(),
            MicrophonePolicy::Device { resolve: true, ref label, .. } if label == "USB A"
        ));
        reloaded
            .set_microphone_for(Provider::Chatgpt, PreferredMicrophone::Default)
            .unwrap();
        assert_eq!(
            reloaded.microphone_policy(Provider::Claude).unwrap(),
            MicrophonePolicy::Default
        );
        // Corrupt binding data never discards the rest of the settings.
        fs::write(&path, r#"{"pttAutoSubmit":false,"microphoneSource":"x","microphoneConfirmations":[1]}"#).unwrap();
        let tolerant = GeneralSettingsState::load(path);
        assert!(!tolerant.ptt_auto_submit().unwrap());
        assert_eq!(
            tolerant.microphone_policy(Provider::Claude).unwrap(),
            MicrophonePolicy::Default
        );
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
