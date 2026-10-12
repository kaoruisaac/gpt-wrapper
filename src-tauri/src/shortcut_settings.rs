use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::PathBuf, str::FromStr, sync::Mutex};
use tauri_plugin_global_shortcut::{Modifiers, Shortcut};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutSettings {
    pub ptt: Vec<String>,
    pub submit: Vec<String>,
}

impl Default for ShortcutSettings {
    fn default() -> Self {
        Self {
            ptt: vec!["Ctrl+Digit0".into(), "Ctrl+Numpad0".into()],
            submit: vec!["Ctrl+Period".into(), "Ctrl+NumpadDecimal".into()],
        }
    }
}

// Never trust shortcuts coming from the WebView or from the persisted settings file.
// Restrict modifiers to the four supported combinations and reject duplicate OS hotkey IDs.
pub fn parse_and_validate(
    settings: &ShortcutSettings,
) -> Result<(Vec<Shortcut>, Vec<Shortcut>), String> {
    if settings.ptt.is_empty() || settings.submit.is_empty() {
        return Err("PTT 與 Explicit Submit 必須各保留至少一組快捷鍵。".into());
    }
    if settings.ptt.len() > 16 || settings.submit.len() > 16 {
        return Err("每個動作最多可設定 16 組快捷鍵。".into());
    }
    let mut ids = HashSet::new();
    let mut parse = |values: &[String]| -> Result<Vec<Shortcut>, String> {
        values
            .iter()
            .map(|value| {
                let parts: Vec<&str> = value.split('+').collect();
                let valid_modifiers = matches!(
                    parts.as_slice(),
                    ["Ctrl", _] | ["Alt", _] | ["Ctrl", "Alt", _] | ["Ctrl", "Shift", _]
                );
                if !valid_modifiers {
                    return Err(format!("不支援的快捷鍵組合：{value}"));
                }
                let shortcut = Shortcut::from_str(value)
                    .map_err(|error| format!("無效的快捷鍵 {value}：{error}"))?;
                // No modifier-only shortcuts; avoid reserved modifier codes.
                if shortcut.mods.is_empty() || shortcut.mods.contains(Modifiers::SUPER) {
                    return Err(format!("不支援的快捷鍵組合：{value}"));
                }
                if !ids.insert(shortcut.id()) {
                    return Err(format!("快捷鍵重複設定：{value}"));
                }
                Ok(shortcut)
            })
            .collect()
    };
    let ptt = parse(&settings.ptt)?;
    let submit = parse(&settings.submit)?;
    Ok((ptt, submit))
}

pub struct ShortcutSettingsState {
    path: PathBuf,
    settings: Mutex<ShortcutSettings>,
}

impl ShortcutSettingsState {
    pub fn load(path: PathBuf) -> Self {
        let settings = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<ShortcutSettings>(&bytes) {
                Ok(value) if parse_and_validate(&value).is_ok() => value,
                Ok(_) => {
                    eprintln!("[gpt-wrapper] invalid persisted shortcuts; using defaults");
                    ShortcutSettings::default()
                }
                Err(error) => {
                    eprintln!("[gpt-wrapper] failed to parse shortcuts: {error}");
                    ShortcutSettings::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ShortcutSettings::default()
            }
            Err(error) => {
                eprintln!("[gpt-wrapper] failed to read shortcuts: {error}");
                ShortcutSettings::default()
            }
        };
        Self {
            path,
            settings: Mutex::new(settings),
        }
    }

    pub fn get(&self) -> Result<ShortcutSettings, String> {
        Ok(self
            .settings
            .lock()
            .map_err(|_| "shortcut settings lock poisoned")?
            .clone())
    }

    pub fn save(&self, value: ShortcutSettings) -> Result<(), String> {
        parse_and_validate(&value)?;
        let parent = self
            .path
            .parent()
            .ok_or("shortcut settings path has no parent")?;
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
        fs::write(&self.path, bytes).map_err(|error| format!("無法儲存快捷鍵：{error}"))?;
        *self
            .settings
            .lock()
            .map_err(|_| "shortcut settings lock poisoned")? = value;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_all_modifier_combinations() {
        assert_eq!(
            parse_and_validate(&ShortcutSettings::default())
                .unwrap()
                .0
                .len(),
            2
        );
        let custom = ShortcutSettings {
            ptt: vec!["Ctrl+Shift+KeyK".into(), "Alt+F8".into()],
            submit: vec!["Ctrl+Alt+KeyQ".into(), "Ctrl+NumpadDecimal".into()],
        };
        assert_eq!(parse_and_validate(&custom).unwrap().1.len(), 2);
    }

    #[test]
    fn rejects_duplicates_and_unsupported_modifiers() {
        let mut value = ShortcutSettings::default();
        value.submit.push("Ctrl+Digit0".into());
        assert!(parse_and_validate(&value).is_err());
        value.submit = vec!["Ctrl+Alt+Shift+KeyA".into()];
        assert!(parse_and_validate(&value).is_err());
        value.submit = vec!["Shift+KeyA".into()];
        assert!(parse_and_validate(&value).is_err());
        value.submit.clear();
        assert!(parse_and_validate(&value).is_err());
    }

    #[test]
    fn reload_saved_settings() {
        let dir = std::env::temp_dir().join(format!("gpt-shortcuts-{}", uuid::Uuid::new_v4()));
        let path = dir.join("shortcut-settings.json");
        let state = ShortcutSettingsState::load(path.clone());
        let custom = ShortcutSettings {
            ptt: vec!["Ctrl+Shift+KeyK".into()],
            submit: vec!["Alt+KeyM".into()],
        };
        state.save(custom.clone()).unwrap();
        assert_eq!(ShortcutSettingsState::load(path).get().unwrap(), custom);
        fs::remove_dir_all(dir).unwrap();
    }
}
