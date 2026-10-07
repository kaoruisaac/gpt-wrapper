use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, sync::Mutex};
use url::Url;
use uuid::Uuid;

pub const CHATGPT_URL: &str = "https://chatgpt.com";
pub const CHATGPT_TITLE: &str = "ChatGPT";

pub struct TabSpec {
    pub url: Url,
    pub title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectBookmark {
    pub id: Uuid,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub message_prefix: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSettings {
    pub default_project_id: Option<Uuid>,
    pub projects: Vec<ProjectBookmark>,
}

impl ProjectSettings {
    pub fn project_tab_spec(&self, id: Uuid) -> Result<TabSpec, String> {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == id)
            .ok_or("Project 不存在。")?;
        Ok(TabSpec {
            url: valid_url(&project.url)?,
            title: project.name.clone(),
        })
    }

    pub fn default_tab_spec(&self) -> TabSpec {
        self.default_project_id
            .and_then(|id| self.projects.iter().find(|project| project.id == id))
            .and_then(|project| {
                valid_url(&project.url).ok().map(|url| TabSpec {
                    url,
                    title: project.name.clone(),
                })
            })
            .unwrap_or_else(|| TabSpec {
                url: Url::parse(CHATGPT_URL).expect("built-in URL is valid"),
                title: CHATGPT_TITLE.to_string(),
            })
    }

    pub fn resolved_url(&self) -> Url {
        self.default_tab_spec().url
    }

    fn normalize_reference(&mut self) {
        if self
            .default_project_id
            .is_some_and(|id| !self.projects.iter().any(|p| p.id == id))
        {
            self.default_project_id = None;
        }
    }
}

pub fn valid_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value.trim())
        .map_err(|_| "請輸入有效的 http:// 或 https:// URL。".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err("請輸入有效的 http:// 或 https:// URL。".to_string());
    }
    Ok(url)
}

fn validate_fields(
    name: String,
    url: String,
    message_prefix: String,
) -> Result<(String, String, String), String> {
    let name = name.trim().to_string();
    let url = url.trim().to_string();
    if name.is_empty() {
        return Err("Project 名稱不可空白。".to_string());
    }
    if url.is_empty() {
        return Err("Project URL 不可空白。".to_string());
    }
    valid_url(&url)?;
    Ok((name, url, message_prefix))
}

pub struct ProjectSettingsState {
    path: PathBuf,
    data: Mutex<ProjectSettings>,
}

impl ProjectSettingsState {
    pub fn load(path: PathBuf) -> Self {
        let mut data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                eprintln!("[gpt-wrapper] failed to parse project settings: {error}");
                ProjectSettings::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ProjectSettings::default()
            }
            Err(error) => {
                eprintln!("[gpt-wrapper] failed to read project settings: {error}");
                ProjectSettings::default()
            }
        };
        data.normalize_reference();
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    pub fn snapshot(&self) -> Result<ProjectSettings, String> {
        Ok(self
            .data
            .lock()
            .map_err(|_| "project settings lock was poisoned".to_string())?
            .clone())
    }

    pub fn update(
        &self,
        change: impl FnOnce(&mut ProjectSettings) -> Result<(), String>,
    ) -> Result<(ProjectSettings, bool), String> {
        let mut current = self
            .data
            .lock()
            .map_err(|_| "project settings lock was poisoned".to_string())?;
        let old_url = current.resolved_url();
        let mut next = current.clone();
        change(&mut next)?;
        next.normalize_reference();
        let url_changed = old_url != next.resolved_url();
        self.persist(&next)?;
        *current = next.clone();
        Ok((next, url_changed))
    }

    fn persist(&self, data: &ProjectSettings) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or("project settings path has no parent")?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create settings directory: {error}"))?;
        let temp = parent.join(format!(".project-settings-{}.tmp", Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(data)
            .map_err(|error| format!("failed to serialize project settings: {error}"))?;
        let result = (|| {
            fs::write(&temp, bytes)
                .map_err(|error| format!("failed to write project settings: {error}"))?;
            fs::rename(&temp, &self.path)
                .map_err(|error| format!("failed to replace project settings: {error}"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

pub fn create(
    data: &mut ProjectSettings,
    name: String,
    url: String,
    message_prefix: String,
) -> Result<(), String> {
    let (name, url, message_prefix) = validate_fields(name, url, message_prefix)?;
    data.projects.insert(
        0,
        ProjectBookmark {
            id: Uuid::new_v4(),
            name,
            url,
            message_prefix,
        },
    );
    Ok(())
}

pub fn edit(
    data: &mut ProjectSettings,
    id: Uuid,
    name: String,
    url: String,
    message_prefix: String,
) -> Result<(), String> {
    let (name, url, message_prefix) = validate_fields(name, url, message_prefix)?;
    let project = data
        .projects
        .iter_mut()
        .find(|project| project.id == id)
        .ok_or("Project 不存在。")?;
    project.name = name;
    project.url = url;
    project.message_prefix = message_prefix;
    Ok(())
}

pub fn delete(data: &mut ProjectSettings, id: Uuid) -> Result<(), String> {
    let index = data
        .projects
        .iter()
        .position(|project| project.id == id)
        .ok_or("Project 不存在。")?;
    data.projects.remove(index);
    if data.default_project_id == Some(id) {
        data.default_project_id = None;
    }
    Ok(())
}

pub fn set_default(data: &mut ProjectSettings, id: Option<Uuid>) -> Result<(), String> {
    if id.is_some_and(|id| !data.projects.iter().any(|project| project.id == id)) {
        return Err("Project 不存在。".to_string());
    }
    data.default_project_id = id;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_keeps_identity_order_and_default_fallback() {
        let mut data = ProjectSettings::default();
        create(
            &mut data,
            " First ".into(),
            " https://chatgpt.com/g/g-first ".into(),
            "".into(),
        )
        .unwrap();
        let first = data.projects[0].id;
        create(
            &mut data,
            "Second".into(),
            "https://chatgpt.com/g/g-second".into(),
            "prefix".into(),
        )
        .unwrap();
        let second = data.projects[0].id;
        assert_eq!(
            data.projects.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![second, first]
        );
        set_default(&mut data, Some(first)).unwrap();
        edit(
            &mut data,
            first,
            "Renamed".into(),
            "https://example.com/project".into(),
            "text".into(),
        )
        .unwrap();
        assert_eq!(data.projects[1].id, first);
        assert_eq!(data.resolved_url().as_str(), "https://example.com/project");
        let default = data.default_tab_spec();
        assert_eq!(default.url.as_str(), "https://example.com/project");
        assert_eq!(default.title, "Renamed");
        delete(&mut data, first).unwrap();
        assert_eq!(data.default_project_id, None);
        assert_eq!(data.projects[0].id, second);
        assert_eq!(data.resolved_url().as_str(), "https://chatgpt.com/");
        assert_eq!(data.default_tab_spec().title, CHATGPT_TITLE);
    }

    #[test]
    fn rejects_invalid_fields_and_references() {
        let mut data = ProjectSettings::default();
        assert!(create(
            &mut data,
            "  ".into(),
            "https://example.com".into(),
            "".into()
        )
        .is_err());
        assert!(create(
            &mut data,
            "name".into(),
            "javascript:alert(1)".into(),
            "".into()
        )
        .is_err());
        assert!(set_default(&mut data, Some(Uuid::new_v4())).is_err());
        assert!(data.projects.is_empty());
        let id = Uuid::new_v4();
        data.default_project_id = Some(id);
        data.projects.push(ProjectBookmark {
            id,
            name: "Corrupt URL".into(),
            url: "file:///tmp/project".into(),
            message_prefix: String::new(),
        });
        assert_eq!(data.resolved_url().as_str(), "https://chatgpt.com/");
        assert_eq!(data.default_tab_spec().title, CHATGPT_TITLE);
        assert!(data.project_tab_spec(id).is_err());
        assert!(data.project_tab_spec(Uuid::new_v4()).is_err());
    }

    #[test]
    fn selected_project_uses_latest_url_and_name_without_changing_default() {
        let mut data = ProjectSettings::default();
        create(
            &mut data,
            "One".into(),
            "https://example.com/old".into(),
            "".into(),
        )
        .unwrap();
        let id = data.projects[0].id;
        edit(
            &mut data,
            id,
            "Renamed".into(),
            "https://example.com/new".into(),
            "".into(),
        )
        .unwrap();
        let selected = data.project_tab_spec(id).unwrap();
        assert_eq!(selected.url.as_str(), "https://example.com/new");
        assert_eq!(selected.title, "Renamed");
        assert_eq!(data.default_project_id, None);
        delete(&mut data, id).unwrap();
        assert!(data.project_tab_spec(id).is_err());
    }

    #[test]
    fn default_spec_falls_back_together_and_rename_keeps_url() {
        let mut data = ProjectSettings::default();
        let fallback = data.default_tab_spec();
        assert_eq!(fallback.url.as_str(), "https://chatgpt.com/");
        assert_eq!(fallback.title, CHATGPT_TITLE);

        data.default_project_id = Some(Uuid::new_v4());
        let missing = data.default_tab_spec();
        assert_eq!(missing.url.as_str(), "https://chatgpt.com/");
        assert_eq!(missing.title, CHATGPT_TITLE);

        create(
            &mut data,
            "Old Name".into(),
            "https://example.com/project".into(),
            "".into(),
        )
        .unwrap();
        let id = data.projects[0].id;
        data.default_project_id = Some(id);
        let original = data.default_tab_spec();
        edit(
            &mut data,
            id,
            "New Name".into(),
            "https://example.com/project".into(),
            "".into(),
        )
        .unwrap();
        let renamed = data.default_tab_spec();
        assert_eq!(original.title, "Old Name");
        assert_eq!(renamed.title, "New Name");
        assert_eq!(renamed.url, original.url);
    }

    #[test]
    fn malformed_file_falls_back_and_failed_write_does_not_change_state() {
        let directory =
            std::env::temp_dir().join(format!("gpt-wrapper-project-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.json");
        fs::write(&path, "not JSON").unwrap();
        let state = ProjectSettingsState::load(path.clone());
        assert!(state.snapshot().unwrap().projects.is_empty());
        state
            .update(|data| create(data, "Test".into(), "https://example.com".into(), "".into()))
            .unwrap();
        assert_eq!(
            ProjectSettingsState::load(path)
                .snapshot()
                .unwrap()
                .projects
                .len(),
            1
        );
        let blocked =
            ProjectSettingsState::load(directory.join("settings.json").join("nested.json"));
        assert!(blocked
            .update(|data| create(data, "Test".into(), "https://example.com".into(), "".into()))
            .is_err());
        assert!(blocked.snapshot().unwrap().projects.is_empty());
        fs::remove_dir_all(directory).unwrap();
    }
}
