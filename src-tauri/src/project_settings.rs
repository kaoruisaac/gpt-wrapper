//! Project Bookmarks, Default Project and Message Prefix, kept separately for
//! each provider. Files written before providers existed hold only ChatGPT
//! data and are migrated without loss.
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, sync::Mutex};
use url::Url;
use uuid::Uuid;

use crate::provider::{is_claude_url, Provider};

pub const CHATGPT_TITLE: &str = "ChatGPT";
const SETTINGS_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabSpec {
    pub url: Url,
    pub title: String,
    pub project_id: Option<Uuid>,
    pub message_prefix: String,
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

/// One provider's bookmarks and Default Project.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectList {
    pub default_project_id: Option<Uuid>,
    pub projects: Vec<ProjectBookmark>,
}

fn home_spec(provider: Provider) -> TabSpec {
    TabSpec {
        url: provider.home_url(),
        title: provider.default_title().to_string(),
        project_id: None,
        message_prefix: String::new(),
    }
}

impl ProjectList {
    pub fn project_tab_spec(&self, provider: Provider, id: Uuid) -> Result<TabSpec, String> {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == id)
            .ok_or("Project 不存在。")?;
        Ok(TabSpec {
            url: stored_url(provider, &project.url)?,
            title: project.name.clone(),
            project_id: Some(project.id),
            message_prefix: project.message_prefix.clone(),
        })
    }

    /// The Default Project, or the provider's built-in home page when unset
    /// or unusable.
    pub fn default_tab_spec(&self, provider: Provider) -> TabSpec {
        self.default_project_id
            .and_then(|id| self.project_tab_spec(provider, id).ok())
            .unwrap_or_else(|| home_spec(provider))
    }

    pub fn resolved_default_id(&self) -> Option<Uuid> {
        self.default_project_id
            .filter(|id| self.projects.iter().any(|project| project.id == *id))
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

/// All providers' bookmark lists. Serialized as
/// `{"version":2,"chatgpt":{...},"claude":{...}}`.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSettings {
    version: u32,
    pub chatgpt: ProjectList,
    pub claude: ProjectList,
}

impl ProjectSettings {
    pub fn list(&self, provider: Provider) -> &ProjectList {
        match provider {
            Provider::Chatgpt => &self.chatgpt,
            Provider::Claude => &self.claude,
        }
    }

    pub fn list_mut(&mut self, provider: Provider) -> &mut ProjectList {
        match provider {
            Provider::Chatgpt => &mut self.chatgpt,
            Provider::Claude => &mut self.claude,
        }
    }

    pub fn default_tab_spec(&self, provider: Provider) -> TabSpec {
        self.list(provider).default_tab_spec(provider)
    }

    fn normalize(&mut self) {
        self.version = SETTINGS_VERSION;
        self.chatgpt.normalize_reference();
        self.claude.normalize_reference();
    }

    /// Reads current and legacy files. A legacy file's top-level
    /// `projects`/`defaultProjectId` belong to ChatGPT; Claude starts empty.
    /// A broken list falls back to empty without discarding the other one.
    fn from_value(value: serde_json::Value) -> Self {
        let parse = |value: Option<&serde_json::Value>, provider: &str| {
            value
                .map(|value| {
                    serde_json::from_value::<ProjectList>(value.clone()).unwrap_or_else(|error| {
                        eprintln!("[gpt-wrapper] failed to parse {provider} project list: {error}");
                        ProjectList::default()
                    })
                })
                .unwrap_or_default()
        };
        let chatgpt = if value.get("chatgpt").is_some() {
            parse(value.get("chatgpt"), "chatgpt")
        } else {
            parse(Some(&value), "legacy chatgpt")
        };
        let mut settings = Self {
            version: SETTINGS_VERSION,
            chatgpt,
            claude: parse(value.get("claude"), "claude"),
        };
        settings.normalize();
        settings
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

/// URL check for new or edited bookmarks.
fn validate_bookmark_url(provider: Provider, value: &str) -> Result<Url, String> {
    match provider {
        Provider::Claude => {
            let url = Url::parse(value.trim())
                .map_err(|_| "Claude 書籤只接受 https://claude.ai/ 的網址。".to_string())?;
            if !Provider::Claude.trusts_url(&url) {
                return Err("Claude 書籤只接受 https://claude.ai/ 的網址。".to_string());
            }
            Ok(url)
        }
        Provider::Chatgpt => {
            let url = valid_url(value)?;
            if is_claude_url(&url) {
                return Err("這是 Claude 網址，請改加入 Claude（C）的專案書籤。".to_string());
            }
            Ok(url)
        }
    }
}

/// URL check when opening a saved bookmark. Existing ChatGPT bookmarks keep
/// their original lenient rule; Claude bookmarks must stay on claude.ai.
fn stored_url(provider: Provider, value: &str) -> Result<Url, String> {
    match provider {
        Provider::Chatgpt => valid_url(value),
        Provider::Claude => validate_bookmark_url(provider, value),
    }
}

fn validate_fields(
    provider: Provider,
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
    validate_bookmark_url(provider, &url)?;
    Ok((name, url, message_prefix))
}

pub struct ProjectSettingsState {
    path: PathBuf,
    data: Mutex<ProjectSettings>,
}

impl ProjectSettingsState {
    pub fn load(path: PathBuf) -> Self {
        let data = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(value) => ProjectSettings::from_value(value),
                Err(error) => {
                    eprintln!("[gpt-wrapper] failed to parse project settings: {error}");
                    ProjectSettings::from_value(serde_json::Value::Null)
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ProjectSettings::from_value(serde_json::Value::Null)
            }
            Err(error) => {
                eprintln!("[gpt-wrapper] failed to read project settings: {error}");
                ProjectSettings::from_value(serde_json::Value::Null)
            }
        };
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

    /// Applies `change` to one provider's list. The whole file (both lists)
    /// is written atomically. Returns whether that provider's default
    /// tab context changed.
    pub fn update(
        &self,
        provider: Provider,
        change: impl FnOnce(&mut ProjectList) -> Result<(), String>,
    ) -> Result<(ProjectSettings, bool), String> {
        let mut current = self
            .data
            .lock()
            .map_err(|_| "project settings lock was poisoned".to_string())?;
        let old_spec = current.default_tab_spec(provider);
        let mut next = current.clone();
        change(next.list_mut(provider))?;
        next.normalize();
        let default_changed = old_spec != next.default_tab_spec(provider);
        self.persist(&next)?;
        *current = next.clone();
        Ok((next, default_changed))
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
    data: &mut ProjectList,
    provider: Provider,
    name: String,
    url: String,
    message_prefix: String,
) -> Result<(), String> {
    let (name, url, message_prefix) = validate_fields(provider, name, url, message_prefix)?;
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
    data: &mut ProjectList,
    provider: Provider,
    id: Uuid,
    name: String,
    url: String,
    message_prefix: String,
) -> Result<(), String> {
    let (name, url, message_prefix) = validate_fields(provider, name, url, message_prefix)?;
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

pub fn delete(data: &mut ProjectList, id: Uuid) -> Result<(), String> {
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

pub fn set_default(data: &mut ProjectList, id: Option<Uuid>) -> Result<(), String> {
    if id.is_some_and(|id| !data.projects.iter().any(|project| project.id == id)) {
        return Err("Project 不存在。".to_string());
    }
    data.default_project_id = id;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: Provider = Provider::Chatgpt;
    const C: Provider = Provider::Claude;

    fn temp_dir(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("gpt-wrapper-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn crud_keeps_identity_order_and_default_fallback() {
        let mut data = ProjectList::default();
        create(
            &mut data,
            G,
            " First ".into(),
            " https://chatgpt.com/g/g-first ".into(),
            "".into(),
        )
        .unwrap();
        let first = data.projects[0].id;
        create(
            &mut data,
            G,
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
            G,
            first,
            "Renamed".into(),
            "https://example.com/project".into(),
            "text".into(),
        )
        .unwrap();
        assert_eq!(data.projects[1].id, first);
        let default = data.default_tab_spec(G);
        assert_eq!(default.url.as_str(), "https://example.com/project");
        assert_eq!(default.title, "Renamed");
        assert_eq!(default.project_id, Some(first));
        assert_eq!(default.message_prefix, "text");
        delete(&mut data, first).unwrap();
        assert_eq!(data.default_project_id, None);
        assert_eq!(data.projects[0].id, second);
        assert_eq!(data.default_tab_spec(G).url.as_str(), "https://chatgpt.com/");
        assert_eq!(data.default_tab_spec(G).title, CHATGPT_TITLE);
    }

    #[test]
    fn rejects_invalid_fields_and_references() {
        let mut data = ProjectList::default();
        assert!(create(&mut data, G, "  ".into(), "https://example.com".into(), "".into()).is_err());
        assert!(create(&mut data, G, "name".into(), "javascript:alert(1)".into(), "".into()).is_err());
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
        assert_eq!(data.default_tab_spec(G).url.as_str(), "https://chatgpt.com/");
        assert_eq!(data.default_tab_spec(G).title, CHATGPT_TITLE);
        assert!(data.project_tab_spec(G, id).is_err());
        assert!(data.project_tab_spec(G, Uuid::new_v4()).is_err());
    }

    #[test]
    fn selected_project_uses_latest_url_and_name_without_changing_default() {
        let mut data = ProjectList::default();
        create(&mut data, G, "One".into(), "https://example.com/old".into(), "".into()).unwrap();
        let id = data.projects[0].id;
        edit(&mut data, G, id, "Renamed".into(), "https://example.com/new".into(), "".into())
            .unwrap();
        let selected = data.project_tab_spec(G, id).unwrap();
        assert_eq!(selected.url.as_str(), "https://example.com/new");
        assert_eq!(selected.title, "Renamed");
        assert_eq!(selected.project_id, Some(id));
        assert_eq!(selected.message_prefix, "");
        assert_eq!(data.default_project_id, None);
        delete(&mut data, id).unwrap();
        assert!(data.project_tab_spec(G, id).is_err());
    }

    #[test]
    fn default_spec_falls_back_together_and_rename_keeps_url() {
        let mut data = ProjectList::default();
        let fallback = data.default_tab_spec(G);
        assert_eq!(fallback.url.as_str(), "https://chatgpt.com/");
        assert_eq!(fallback.title, CHATGPT_TITLE);
        assert_eq!(fallback.project_id, None);
        assert_eq!(fallback.message_prefix, "");

        data.default_project_id = Some(Uuid::new_v4());
        let missing = data.default_tab_spec(G);
        assert_eq!(missing.url.as_str(), "https://chatgpt.com/");
        assert_eq!(missing.project_id, None);

        create(&mut data, G, "Old Name".into(), "https://example.com/project".into(), "".into())
            .unwrap();
        let id = data.projects[0].id;
        data.default_project_id = Some(id);
        let original = data.default_tab_spec(G);
        edit(&mut data, G, id, "New Name".into(), "https://example.com/project".into(), "".into())
            .unwrap();
        let renamed = data.default_tab_spec(G);
        assert_eq!(original.title, "Old Name");
        assert_eq!(renamed.title, "New Name");
        assert_eq!(renamed.url, original.url);
    }

    #[test]
    fn default_context_changes_even_when_url_does_not() {
        let mut data = ProjectList::default();
        create(&mut data, G, "A".into(), "https://chatgpt.com".into(), "A ".into()).unwrap();
        let a = data.projects[0].id;
        create(&mut data, G, "B".into(), "https://chatgpt.com".into(), "B\n".into()).unwrap();
        let b = data.projects[0].id;
        set_default(&mut data, Some(a)).unwrap();
        let a_spec = data.default_tab_spec(G);
        set_default(&mut data, Some(b)).unwrap();
        let b_spec = data.default_tab_spec(G);
        assert_eq!(a_spec.url, b_spec.url);
        assert_ne!(a_spec, b_spec);
        assert_eq!(b_spec.project_id, Some(b));
        assert_eq!(b_spec.message_prefix, "B\n");
        edit(&mut data, G, b, "B renamed".into(), "https://chatgpt.com".into(), "NEW ".into())
            .unwrap();
        let edited = data.default_tab_spec(G);
        assert_ne!(edited, b_spec);
        assert_eq!(edited.title, "B renamed");
        assert_eq!(edited.message_prefix, "NEW ");
    }

    #[test]
    fn claude_bookmarks_accept_only_https_claude_ai() {
        let mut data = ProjectList::default();
        create(&mut data, C, "Proj".into(), "https://claude.ai/project/abc".into(), "Hi ".into())
            .unwrap();
        let id = data.projects[0].id;
        for rejected in [
            "https://chatgpt.com/g/g-1",
            "http://claude.ai/project/abc",
            "https://claude.ai.example.com/",
            "https://example.com/",
        ] {
            assert!(
                create(&mut data, C, "x".into(), rejected.into(), "".into()).is_err(),
                "{rejected}"
            );
            assert!(edit(&mut data, C, id, "x".into(), rejected.into(), "".into()).is_err());
        }
        set_default(&mut data, Some(id)).unwrap();
        let spec = data.default_tab_spec(C);
        assert_eq!(spec.url.as_str(), "https://claude.ai/project/abc");
        assert_eq!(spec.message_prefix, "Hi ");
        delete(&mut data, id).unwrap();
        let home = data.default_tab_spec(C);
        assert_eq!(home.url.as_str(), "https://claude.ai/");
        assert_eq!(home.title, "Claude");
        // A tampered Claude entry pointing elsewhere is never opened.
        data.projects.push(ProjectBookmark {
            id,
            name: "Moved".into(),
            url: "https://chatgpt.com/".into(),
            message_prefix: String::new(),
        });
        data.default_project_id = Some(id);
        assert!(data.project_tab_spec(C, id).is_err());
        assert_eq!(data.default_tab_spec(C).url.as_str(), "https://claude.ai/");
    }

    #[test]
    fn chatgpt_bookmarks_reject_new_claude_urls_but_keep_existing_entries() {
        let mut data = ProjectList::default();
        assert!(create(&mut data, G, "C".into(), "https://claude.ai/new".into(), "".into()).is_err());
        create(&mut data, G, "Other".into(), "https://example.com/x".into(), "".into()).unwrap();
        let id = data.projects[0].id;
        assert!(edit(&mut data, G, id, "C".into(), "https://claude.ai/".into(), "".into()).is_err());
        // An old G entry that happens to point at claude.ai is not deleted.
        let legacy = Uuid::new_v4();
        data.projects.push(ProjectBookmark {
            id: legacy,
            name: "Legacy".into(),
            url: "https://claude.ai/".into(),
            message_prefix: String::new(),
        });
        assert!(data.project_tab_spec(G, legacy).is_ok());
        assert_eq!(data.projects.len(), 2);
    }

    #[test]
    fn legacy_file_migrates_to_chatgpt_without_loss() {
        let directory = temp_dir("project-migrate");
        let path = directory.join("project-settings.json");
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        fs::write(
            &path,
            format!(
                r#"{{"defaultProjectId":"{b}","projects":[
                {{"id":"{a}","name":"A","url":"https://chatgpt.com/g/a","messagePrefix":"pa"}},
                {{"id":"{b}","name":"B","url":"https://example.com/odd"}}]}}"#
            ),
        )
        .unwrap();
        let state = ProjectSettingsState::load(path.clone());
        let loaded = state.snapshot().unwrap();
        assert_eq!(loaded.chatgpt.default_project_id, Some(b));
        assert_eq!(
            loaded.chatgpt.projects.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![a, b]
        );
        assert_eq!(loaded.chatgpt.projects[0].message_prefix, "pa");
        assert!(loaded.claude.projects.is_empty());
        assert_eq!(loaded.default_tab_spec(C).url.as_str(), "https://claude.ai/");
        // Writing Claude data keeps the migrated ChatGPT list intact.
        state
            .update(C, |list| {
                create(list, C, "C1".into(), "https://claude.ai/project/1".into(), "".into())
            })
            .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains(r#""version": 2"#));
        let reread = ProjectSettingsState::load(path).snapshot().unwrap();
        assert_eq!(reread.chatgpt.default_project_id, Some(b));
        assert_eq!(reread.chatgpt.projects.len(), 2);
        assert_eq!(reread.claude.projects.len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn provider_defaults_are_independent() {
        let directory = temp_dir("project-defaults");
        let state = ProjectSettingsState::load(directory.join("p.json"));
        let (settings, changed) = state
            .update(G, |list| {
                create(list, G, "G1".into(), "https://chatgpt.com/g/1".into(), "".into())?;
                let id = list.projects[0].id;
                set_default(list, Some(id))
            })
            .unwrap();
        assert!(changed);
        let g1 = settings.chatgpt.projects[0].id;
        let (settings, changed) = state
            .update(C, |list| {
                create(list, C, "C1".into(), "https://claude.ai/project/1".into(), "".into())?;
                let id = list.projects[0].id;
                set_default(list, Some(id))
            })
            .unwrap();
        assert!(changed);
        let c1 = settings.claude.projects[0].id;
        assert_eq!(settings.chatgpt.default_project_id, Some(g1));
        assert_eq!(settings.claude.default_project_id, Some(c1));
        // An ID from the other provider is rejected.
        assert!(state.update(C, |list| set_default(list, Some(g1))).is_err());
        assert!(state.update(G, |list| delete(list, c1)).is_err());
        // Deleting C's default falls back to claude.ai and leaves G alone.
        let (settings, changed) = state.update(C, |list| delete(list, c1)).unwrap();
        assert!(changed);
        assert_eq!(settings.claude.default_project_id, None);
        assert_eq!(settings.chatgpt.default_project_id, Some(g1));
        let (_, changed) = state.update(C, |_| Ok(())).unwrap();
        assert!(!changed);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn malformed_file_or_list_falls_back_and_failed_write_does_not_change_state() {
        let directory = temp_dir("project-test");
        let path = directory.join("settings.json");
        fs::write(&path, "not JSON").unwrap();
        let state = ProjectSettingsState::load(path.clone());
        assert!(state.snapshot().unwrap().chatgpt.projects.is_empty());
        state
            .update(G, |data| {
                create(data, G, "Test".into(), "https://example.com".into(), "".into())
            })
            .unwrap();
        assert_eq!(
            ProjectSettingsState::load(path.clone()).snapshot().unwrap().chatgpt.projects.len(),
            1
        );
        // A broken Claude list does not discard the ChatGPT list.
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["claude"] = serde_json::json!({ "projects": 5 });
        fs::write(&path, value.to_string()).unwrap();
        let tolerant = ProjectSettingsState::load(path.clone()).snapshot().unwrap();
        assert_eq!(tolerant.chatgpt.projects.len(), 1);
        assert!(tolerant.claude.projects.is_empty());
        let blocked =
            ProjectSettingsState::load(directory.join("settings.json").join("nested.json"));
        assert!(blocked
            .update(G, |data| create(data, G, "Test".into(), "https://example.com".into(), "".into()))
            .is_err());
        assert!(blocked.snapshot().unwrap().chatgpt.projects.is_empty());
        fs::remove_dir_all(directory).unwrap();
    }
}
