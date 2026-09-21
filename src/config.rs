//! What `anna setup` writes and everything else reads.
//!
//! The settings are one JSON file, `config.json`. The two pieces of prose
//! live next to it as markdown, because nobody should have to edit prose
//! inside a JSON string: `style.md` is how Anna writes to people, and
//! `CLAUDE.md` is who she is — every thread reads it. Tokens are not here at
//! all; see `secrets`.
//!
//! All three files steer Anna, so they are written private and refused if
//! anyone but their owner could have written them.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::fsutil;
use crate::paths;

pub const JEV_API_KEY: &str = "jev_api_key";

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
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
    /// How long one run of a thread, a hand or a reviewer may take before it
    /// is stopped. The only hard stop Anna has.
    #[serde(default = "an_hour")]
    pub minutes_per_run: u64,
}

fn an_hour() -> u64 {
    60
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
            mcp_servers: BTreeMap::new(),
            sources: BTreeMap::new(),
            people: BTreeMap::new(),
            rotate_accounts: true,
            minutes_per_run: an_hour(),
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
    /// A command that runs for as long as Anna does and prints a line
    /// whenever something happens on this source — `hey watch --events new`,
    /// say. Every line is a poke. With one of these the timer only has to
    /// catch what the trigger missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Trigger {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
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
            let text = fsutil::read_trusted(path)?;
            serde_json::from_str(&text).with_context(|| format!("{} is not valid", path.display()))
        } else {
            Ok(Config::default())
        }
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        fsutil::write_private(path, &(serde_json::to_string_pretty(self)? + "\n"))
    }
}

pub fn style() -> Result<Option<String>> {
    prose(&paths::config_dir().join("style.md"))
}

pub fn personality() -> Result<Option<String>> {
    prose(&paths::config_dir().join("CLAUDE.md"))
}

/// A missing or empty file means there is none. A file someone else could
/// have written is an error, never a quiet "none".
fn prose(path: &Path) -> Result<Option<String>> {
    if path.exists() {
        let text = fsutil::read_trusted(path)?.trim().to_string();
        if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(text))
        }
    } else {
        Ok(None)
    }
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
        assert_eq!(prose(&directory.join("style.md")).unwrap(), None);

        fsutil::write_private(&directory.join("style.md"), "  Short sentences.\n\n").unwrap();
        assert_eq!(prose(&directory.join("style.md")).unwrap(), Some("Short sentences.".to_string()));

        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o666)).unwrap();
        assert!(Config::load_from(&path).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
