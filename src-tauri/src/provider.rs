//! Stable chat provider identity shared by tabs, WebViews and persistence.
//!
//! The provider of a tab is assigned when the tab is created and never inferred
//! from its title, URL or tab ID.
use serde::{Deserialize, Serialize};
use url::Url;

pub const CHATGPT_HOME_URL: &str = "https://chatgpt.com";
pub const CLAUDE_HOME_URL: &str = "https://claude.ai";

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Chatgpt,
    Claude,
}

impl Provider {
    pub fn home_url(self) -> Url {
        Url::parse(match self {
            Self::Chatgpt => CHATGPT_HOME_URL,
            Self::Claude => CLAUDE_HOME_URL,
        })
        .expect("built-in provider URL is valid")
    }

    pub fn default_title(self) -> &'static str {
        match self {
            Self::Chatgpt => crate::project_settings::CHATGPT_TITLE,
            Self::Claude => "Claude",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Chatgpt => "chatgpt",
            Self::Claude => "claude",
        }
    }

    /// The UI letter shown on the Tab Bar switcher and in toasts.
    pub fn letter(self) -> &'static str {
        match self {
            Self::Chatgpt => "G",
            Self::Claude => "C",
        }
    }

    /// Exact HTTPS hosts of this provider's top-level pages.
    pub fn hosts(self) -> &'static [&'static str] {
        match self {
            Self::Chatgpt => &["chatgpt.com", "chat.openai.com"],
            Self::Claude => &["claude.ai"],
        }
    }

    /// HTTPS top-level origins whose WebMessages may report control state.
    pub fn trusts_source(self, source: &str) -> bool {
        let Ok(url) = Url::parse(source) else {
            return false;
        };
        self.trusts_url(&url)
    }

    pub fn trusts_url(self, url: &Url) -> bool {
        url.scheme() == "https"
            && url.port().is_none()
            && url
                .host_str()
                .is_some_and(|host| self.hosts().contains(&host))
    }
}

/// A URL that clearly belongs to Claude (claude.ai or one of its subdomains).
/// Used to keep Claude pages out of the ChatGPT bookmark list.
pub fn is_claude_url(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        host == "claude.ai" || host.ends_with(".claude.ai")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_stable_ids() {
        assert_eq!(serde_json::to_string(&Provider::Chatgpt).unwrap(), "\"chatgpt\"");
        assert_eq!(serde_json::to_string(&Provider::Claude).unwrap(), "\"claude\"");
        assert_eq!(
            serde_json::from_str::<Provider>("\"claude\"").unwrap(),
            Provider::Claude
        );
        assert!(serde_json::from_str::<Provider>("\"G\"").is_err());
        assert_eq!(Provider::default(), Provider::Chatgpt);
    }

    #[test]
    fn home_pages_match_contract() {
        assert_eq!(Provider::Chatgpt.home_url().as_str(), "https://chatgpt.com/");
        assert_eq!(Provider::Claude.home_url().as_str(), "https://claude.ai/");
    }

    #[test]
    fn trusted_sources_are_provider_scoped() {
        assert!(Provider::Chatgpt.trusts_source("https://chatgpt.com/c/1"));
        assert!(Provider::Chatgpt.trusts_source("https://chat.openai.com/"));
        assert!(!Provider::Chatgpt.trusts_source("https://claude.ai/"));
        assert!(Provider::Claude.trusts_source("https://claude.ai/new"));
        for rejected in [
            "https://chatgpt.com/",
            "http://claude.ai/",
            "https://evil.claude.ai.example.com/",
            "https://claude.ai.evil.com/",
            "https://accounts.google.com/",
            "not a url",
        ] {
            assert!(!Provider::Claude.trusts_source(rejected), "{rejected}");
        }
        assert!(!Provider::Claude.trusts_source("https://claude.ai:8443/"));
    }

    #[test]
    fn claude_urls_are_recognized_for_bookmark_separation() {
        for claude in ["https://claude.ai/project/1", "http://www.claude.ai/", "https://CLAUDE.AI/"] {
            assert!(is_claude_url(&Url::parse(claude).unwrap()), "{claude}");
        }
        for other in ["https://chatgpt.com/", "https://claude.ai.example.com/", "https://notclaude.ai/"] {
            assert!(!is_claude_url(&Url::parse(other).unwrap()), "{other}");
        }
        assert_eq!(Provider::Claude.letter(), "C");
        assert_eq!(Provider::Chatgpt.id(), "chatgpt");
    }
}
