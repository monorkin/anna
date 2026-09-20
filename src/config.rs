//! What `anna setup` writes and everything else reads.
//!
//! The settings are one JSON file. The two pieces of prose — the style Anna
//! writes in and her personality — are their own markdown files next to it,
//! because nobody should have to edit prose inside a JSON string. All of it
//! is optional: without a Jev key the judge falls back to haiku, without a
//! style nothing is restyled, without a personality she has none.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::paths;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jev_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServer>,
    /// Where people talk to Anna, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, Source>,
    /// Who Anna listens to: the sender id a source reports, to the name she
    /// knows them by. Anyone else is ignored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub people: BTreeMap<String, String>,
    /// Set to false when something else already runs `ax auto-switch`.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub rotate_accounts: bool,
}

fn yes() -> bool {
    true
}

fn is_yes(value: &bool) -> bool {
    *value
}

impl Default for Config {
    fn default() -> Config {
        Config {
            jev_api_key: None,
            mcp_servers: BTreeMap::new(),
            sources: BTreeMap::new(),
            people: BTreeMap::new(),
            rotate_accounts: true,
        }
    }
}

/// A source is an MCP server Anna also listens on. There is no push in MCP,
/// so listening is calling one of the server's own tools on a timer and
/// picking the new messages out of the JSON it answers with. The four
/// pointers are JSON pointers into each item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Source {
    pub server: String,
    pub watch: Call,
    #[serde(default = "default_interval")]
    pub every_seconds: u64,
    /// JSON pointer to the array of messages in the watch tool's answer.
    pub items: String,
    pub id: String,
    pub conversation: String,
    pub sender: String,
    pub text: String,
    /// The call that answers in a conversation. `{conversation}` and
    /// `{text}` in its arguments are filled in.
    pub reply: Call,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Call {
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

fn default_interval() -> u64 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServer {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Tool name to the arguments that carry prose for people, which the
    /// editor restyles before the call goes out.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prose: BTreeMap<String, Vec<String>>,
    /// What the server's tool listing looked like when it was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl Config {
    pub fn load() -> Result<Config> {
        Config::load_from(&paths::config_dir().join("config.json"))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&paths::config_dir().join("config.json"))
    }

    fn load_from(path: &Path) -> Result<Config> {
        if path.exists() {
            let text = fs::read_to_string(path)?;
            serde_json::from_str(&text).with_context(|| format!("{} is not valid", path.display()))
        } else {
            Ok(Config::default())
        }
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(directory) = path.parent() {
            fs::create_dir_all(directory)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }
}

pub fn style() -> Option<String> {
    prose(&paths::config_dir().join("style.md"))
}

pub fn personality() -> Option<String> {
    prose(&paths::config_dir().join("personality.md"))
}

fn prose(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|it| it.trim().to_string())
        .filter(|it| !it.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_an_empty_config_and_saving_round_trips() {
        let directory = std::env::temp_dir().join(format!("anna-config-{}", std::process::id()));
        let path = directory.join("config.json");
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());

        let mut config = Config::default();
        config.mcp_servers.insert(
            "basecamp".to_string(),
            McpServer {
                command: "basecamp".to_string(),
                args: vec!["mcp".to_string()],
                prose: BTreeMap::from([("create_comment".to_string(), vec!["content".to_string()])]),
                fingerprint: Some("00000000deadbeef".to_string()),
            },
        );
        config.save_to(&path).unwrap();

        assert_eq!(Config::load_from(&path).unwrap(), config);
        assert!(!fs::read_to_string(&path).unwrap().contains("jev_api_key"));
        assert_eq!(prose(&directory.join("style.md")), None);

        fs::write(directory.join("style.md"), "  Short sentences.\n\n").unwrap();
        assert_eq!(prose(&directory.join("style.md")), Some("Short sentences.".to_string()));
        fs::remove_dir_all(directory).unwrap();
    }
}
