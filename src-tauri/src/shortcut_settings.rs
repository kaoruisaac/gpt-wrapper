use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::PathBuf, str::FromStr, sync::Mutex};
use tauri_plugin_global_shortcut::{Modifiers, Shortcut};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutSettings {
    pub ptt: Vec<String>,
    pub submit: Vec<String>,
    #[serde(default = "default_switch_tabs_bindings")]
    pub switch_tabs: Vec<String>,
}

// Windows defaults (unchanged contract).
const WINDOWS_PTT: [&str; 2] = ["Ctrl+Digit0", "Ctrl+Numpad0"];
const WINDOWS_SUBMIT: [&str; 2] = ["Ctrl+Period", "Ctrl+NumpadDecimal"];
const WINDOWS_SWITCH_TABS: [&str; 2] = ["Ctrl+Shift+Equal", "Ctrl+NumpadAdd"];

// macOS defaults: Control+Command mirrors the Windows "Ctrl+key" layout while
// avoiding Command+0 (Actual Size), Command+. (Cancel), Shift+Command+.
// (Finder hidden files) and other app/system shortcuts. Global hotkeys on
// macOS are system-wide, so a plain Command+key default would break every app.
#[cfg(target_os = "macos")]
const PLATFORM_PTT: [&str; 2] = ["Ctrl+Command+Digit0", "Ctrl+Command+Numpad0"];
#[cfg(target_os = "macos")]
const PLATFORM_SUBMIT: [&str; 2] = ["Ctrl+Command+Period", "Ctrl+Command+NumpadDecimal"];
#[cfg(target_os = "macos")]
const PLATFORM_SWITCH_TABS: [&str; 2] = ["Ctrl+Command+Equal", "Ctrl+Command+NumpadAdd"];
#[cfg(not(target_os = "macos"))]
const PLATFORM_PTT: [&str; 2] = WINDOWS_PTT;
#[cfg(not(target_os = "macos"))]
const PLATFORM_SUBMIT: [&str; 2] = WINDOWS_SUBMIT;
#[cfg(not(target_os = "macos"))]
const PLATFORM_SWITCH_TABS: [&str; 2] = WINDOWS_SWITCH_TABS;

fn strings(values: [&str; 2]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn default_switch_tabs_bindings() -> Vec<String> {
    strings(PLATFORM_SWITCH_TABS)
}

impl Default for ShortcutSettings {
    fn default() -> Self {
        Self {
            ptt: strings(PLATFORM_PTT),
            submit: strings(PLATFORM_SUBMIT),
            switch_tabs: default_switch_tabs_bindings(),
        }
    }
}

// Modifier combinations accepted on every OS (the original Windows set).
fn common_modifiers(parts: &[&str]) -> bool {
    matches!(
        parts,
        ["Ctrl", _] | ["Alt", _] | ["Ctrl", "Alt", _] | ["Ctrl", "Shift", _]
    )
}

// macOS additionally accepts Command (Super) together with one other
// modifier. Command alone is rejected: it would hijack app shortcuts globally.
#[cfg(target_os = "macos")]
fn platform_modifiers(parts: &[&str]) -> bool {
    common_modifiers(parts)
        || matches!(
            parts,
            ["Ctrl", "Command", _] | ["Alt", "Command", _] | ["Shift", "Command", _]
        )
}

#[cfg(not(target_os = "macos"))]
fn platform_modifiers(parts: &[&str]) -> bool {
    common_modifiers(parts)
}

// macOS system shortcuts that a global hotkey would either never receive or
// would break (input sources, Spaces, screenshots, lock, force quit, Dock,
// zoom, Finder hidden files). Rejected with an explicit error.
#[cfg(target_os = "macos")]
const MACOS_RESERVED: &[&str] = &[
    "Ctrl+Space",
    "Ctrl+Alt+Space",
    "Ctrl+ArrowLeft",
    "Ctrl+ArrowRight",
    "Ctrl+ArrowUp",
    "Ctrl+ArrowDown",
    "Ctrl+Command+KeyQ",
    "Ctrl+Command+KeyF",
    "Ctrl+Command+KeyD",
    "Ctrl+Command+Space",
    "Shift+Command+Digit3",
    "Shift+Command+Digit4",
    "Shift+Command+Digit5",
    "Shift+Command+Digit6",
    "Shift+Command+KeyQ",
    "Shift+Command+Period",
    "Alt+Command+Escape",
    "Alt+Command+KeyD",
    "Alt+Command+KeyH",
    "Alt+Command+KeyM",
    "Alt+Command+Space",
    "Alt+Command+Equal",
    "Alt+Command+Minus",
    "Alt+Command+Digit8",
    "Alt+Command+Backslash",
];

#[cfg(target_os = "macos")]
fn reserved(value: &str) -> bool {
    MACOS_RESERVED.contains(&value)
}

#[cfg(not(target_os = "macos"))]
fn reserved(_value: &str) -> bool {
    false
}

/// A list that is exactly a Windows default is Windows-specific on macOS and
/// is migrated to the macOS default for that action. Custom lists are kept.
#[cfg(target_os = "macos")]
fn migrate_windows_defaults(settings: &mut ShortcutSettings) -> bool {
    let mut changed = false;
    for (list, windows, mac) in [
        (&mut settings.ptt, WINDOWS_PTT, PLATFORM_PTT),
        (&mut settings.submit, WINDOWS_SUBMIT, PLATFORM_SUBMIT),
        (&mut settings.switch_tabs, WINDOWS_SWITCH_TABS, PLATFORM_SWITCH_TABS),
    ] {
        if *list == strings(windows) {
            *list = strings(mac);
            changed = true;
        }
    }
    changed
}

#[cfg(not(target_os = "macos"))]
fn migrate_windows_defaults(_settings: &mut ShortcutSettings) -> bool {
    false
}

// Never trust shortcuts coming from the WebView or from the persisted settings file.
// Restrict modifiers to the four supported combinations and reject duplicate OS hotkey IDs.
pub fn parse_and_validate(
    settings: &ShortcutSettings,
) -> Result<(Vec<Shortcut>, Vec<Shortcut>, Vec<Shortcut>), String> {
    if settings.ptt.is_empty() || settings.submit.is_empty() || settings.switch_tabs.is_empty() {
        return Err("PTT、Explicit Submit 與 Switch Tabs 必須各保留至少一組快捷鍵。".into());
    }
    if settings.ptt.len() > 16 || settings.submit.len() > 16 || settings.switch_tabs.len() > 16 {
        return Err("每個動作最多可設定 16 組快捷鍵。".into());
    }
    let mut ids = HashSet::new();
    let mut parse = |values: &[String]| -> Result<Vec<Shortcut>, String> {
        values
            .iter()
            .map(|value| {
                let parts: Vec<&str> = value.split('+').collect();
                if !platform_modifiers(parts.as_slice()) {
                    return Err(format!("不支援的快捷鍵組合：{value}"));
                }
                if reserved(value) {
                    return Err(format!("{value} 是 macOS 系統保留的快捷鍵，請改用其他組合。"));
                }
                let shortcut = Shortcut::from_str(value)
                    .map_err(|error| format!("無效的快捷鍵 {value}：{error}"))?;
                // No modifier-only shortcuts. Super (Command) is only valid in
                // the explicit macOS Command combinations above.
                let command_allowed = cfg!(target_os = "macos") && parts.contains(&"Command");
                if shortcut.mods.is_empty()
                    || (shortcut.mods.contains(Modifiers::SUPER) && !command_allowed)
                {
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
    let switch_tabs = parse(&settings.switch_tabs)?;
    Ok((ptt, submit, switch_tabs))
}

pub struct ShortcutSettingsState {
    path: PathBuf,
    settings: Mutex<ShortcutSettings>,
}

impl ShortcutSettingsState {
    pub fn load(path: PathBuf) -> Self {
        let settings = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<ShortcutSettings>(&bytes) {
                Ok(mut value) => {
                    let migrated = migrate_windows_defaults(&mut value);
                    if parse_and_validate(&value).is_ok() {
                        if migrated {
                            eprintln!("[gpt-wrapper] migrated Windows default shortcuts to macOS defaults");
                            match serde_json::to_vec_pretty(&value) {
                                Ok(bytes) => {
                                    if let Err(error) = fs::write(&path, bytes) {
                                        eprintln!("[gpt-wrapper] failed to persist shortcut migration: {error}");
                                    }
                                }
                                Err(error) => eprintln!("[gpt-wrapper] failed to encode shortcut migration: {error}"),
                            }
                        }
                        value
                    } else {
                        eprintln!("[gpt-wrapper] invalid persisted shortcuts; using defaults");
                        ShortcutSettings::default()
                    }
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
            switch_tabs: vec!["Ctrl+Shift+Equal".into()],
        };
        assert_eq!(parse_and_validate(&custom).unwrap().1.len(), 2);
        assert_eq!(
            parse_and_validate(&ShortcutSettings::default())
                .unwrap()
                .2
                .len(),
            2
        );
    }

    #[test]
    fn rejects_duplicates_and_unsupported_modifiers() {
        let mut value = ShortcutSettings::default();
        let ptt = value.ptt[0].clone();
        value.submit.push(ptt.clone());
        assert!(parse_and_validate(&value).is_err());
        value.submit = vec!["Ctrl+Alt+Shift+KeyA".into()];
        assert!(parse_and_validate(&value).is_err());
        value.submit = vec!["Shift+KeyA".into()];
        assert!(parse_and_validate(&value).is_err());
        value.submit.clear();
        assert!(parse_and_validate(&value).is_err());
        let mut value = ShortcutSettings::default();
        value.switch_tabs.push(ptt);
        assert!(parse_and_validate(&value).is_err());
        value.switch_tabs.clear();
        assert!(parse_and_validate(&value).is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn windows_defaults_are_unchanged_and_reject_command() {
        let defaults = ShortcutSettings::default();
        assert_eq!(defaults.ptt, ["Ctrl+Digit0", "Ctrl+Numpad0"]);
        assert_eq!(defaults.submit, ["Ctrl+Period", "Ctrl+NumpadDecimal"]);
        assert_eq!(defaults.switch_tabs, ["Ctrl+Shift+Equal", "Ctrl+NumpadAdd"]);
        let mut value = ShortcutSettings::default();
        value.ptt = vec!["Ctrl+Command+Digit0".into()];
        assert!(parse_and_validate(&value).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_defaults_use_control_command_and_are_valid() {
        let defaults = ShortcutSettings::default();
        assert_eq!(defaults.ptt, ["Ctrl+Command+Digit0", "Ctrl+Command+Numpad0"]);
        assert_eq!(defaults.submit, ["Ctrl+Command+Period", "Ctrl+Command+NumpadDecimal"]);
        assert_eq!(defaults.switch_tabs, ["Ctrl+Command+Equal", "Ctrl+Command+NumpadAdd"]);
        let (ptt, _, _) = parse_and_validate(&defaults).unwrap();
        assert!(ptt[0].mods.contains(Modifiers::SUPER | Modifiers::CONTROL));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_rejects_reserved_and_command_only_combinations() {
        for binding in [
            "Command+KeyC",
            "Ctrl+Command+KeyQ",
            "Shift+Command+Digit4",
            "Ctrl+Space",
            "Alt+Command+Escape",
            "Ctrl+Alt+Shift+Command+KeyA",
        ] {
            let mut value = ShortcutSettings::default();
            value.ptt = vec![binding.into()];
            assert!(parse_and_validate(&value).is_err(), "{binding}");
        }
        let mut value = ShortcutSettings::default();
        value.ptt = vec!["Alt+Command+KeyK".into(), "Shift+Command+KeyJ".into(), "Ctrl+Digit9".into()];
        assert!(parse_and_validate(&value).is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_migrates_windows_default_lists_but_keeps_custom_ones() {
        let dir = std::env::temp_dir().join(format!("gpt-shortcuts-{}", uuid::Uuid::new_v4()));
        let path = dir.join("shortcut-settings.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &path,
            r#"{"ptt":["Ctrl+Digit0","Ctrl+Numpad0"],"submit":["Alt+KeyM"],"switchTabs":["Ctrl+Shift+Equal","Ctrl+NumpadAdd"]}"#,
        )
        .unwrap();
        let restored = ShortcutSettingsState::load(path.clone()).get().unwrap();
        assert_eq!(restored.ptt, strings(PLATFORM_PTT));
        assert_eq!(restored.submit, ["Alt+KeyM"]);
        assert_eq!(restored.switch_tabs, strings(PLATFORM_SWITCH_TABS));
        // The migration is persisted, so it happens once.
        assert_eq!(ShortcutSettingsState::load(path).get().unwrap(), restored);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_file_preserves_custom_bindings() {
        let dir = std::env::temp_dir().join(format!("gpt-shortcuts-{}", uuid::Uuid::new_v4()));
        let path = dir.join("shortcut-settings.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, r#"{"ptt":["Ctrl+KeyT"],"submit":["Alt+KeyM"]}"#).unwrap();
        let restored = ShortcutSettingsState::load(path).get().unwrap();
        assert_eq!(restored.ptt, ["Ctrl+KeyT"]);
        assert_eq!(restored.submit, ["Alt+KeyM"]);
        assert_eq!(restored.switch_tabs, default_switch_tabs_bindings());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reload_saved_settings() {
        let dir = std::env::temp_dir().join(format!("gpt-shortcuts-{}", uuid::Uuid::new_v4()));
        let path = dir.join("shortcut-settings.json");
        let state = ShortcutSettingsState::load(path.clone());
        let custom = ShortcutSettings {
            ptt: vec!["Ctrl+Shift+KeyK".into()],
            submit: vec!["Alt+KeyM".into()],
            switch_tabs: vec!["Ctrl+NumpadAdd".into()],
        };
        state.save(custom.clone()).unwrap();
        assert_eq!(ShortcutSettingsState::load(path).get().unwrap(), custom);
        fs::remove_dir_all(dir).unwrap();
    }
}
