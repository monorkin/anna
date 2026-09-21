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
use std::path::{Path, PathBuf};

use crate::fsutil;
use crate::paths;

pub const JEV_API_KEY: &str = "jev_api_key";

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// What the agent calls itself. Anna, unless setup said otherwise.
    #[serde(default = "anna")]
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServer>,
    /// Where people talk to Anna, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, Source>,
    /// Who Anna listens to: the sender id a source reports, to the name she
    /// knows them by. Anyone else is ignored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub people: BTreeMap<String, String>,
    /// Where Jev is reached, when not at TypeSafe directly: a gateway in
    /// front of it, or a stand-in for runs that must not touch the real one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jev_url: Option<String>,
    /// The Claude Code config folder she works from, which is which Claude
    /// login she works as. Left out, it is whatever the shell that started
    /// her had — so started from another terminal she would be someone else,
    /// and she would share a login, and its allowance, with whoever works
    /// there. Pointing her at a folder that is already logged in needs no
    /// login of her own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_config_dir: Option<PathBuf>,
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

fn anna() -> String {
    "Anna".to_string()
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
            name: anna(),
            mcp_servers: BTreeMap::new(),
            sources: BTreeMap::new(),
            people: BTreeMap::new(),
            jev_url: None,
            claude_config_dir: None,
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
    /// What makes a message a new one. Several pointers when the id alone
    /// isn't enough: Basecamp bumps the same notification for every new
    /// comment on a thread, so there it is the id and when it went unread.
    pub id: Pointers,
    pub conversation: String,
    pub sender: String,
    /// What to call the sender, for a source that listens to anyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_name: Option<String>,
    /// Listen to everyone who can reach her here, not only to `people`. For
    /// a source that already decides who that is: an agent in Basecamp only
    /// hears from the projects it was added to.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub anyone: bool,
    /// What the thread is told. Several pointers when a source splits it up —
    /// a title, an excerpt, and a link to read the rest.
    pub text: Pointers,
    /// The call that answers in a conversation. `{conversation}` and
    /// `{text}` in its arguments are filled in. Left out when answering isn't
    /// one call — on Basecamp it depends on what is being answered — and the
    /// thread then answers with the server's own tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<Call>,
    /// A command that runs for as long as Anna does and prints a line
    /// whenever something happens on this source — `hey watch --events new`,
    /// say. Every line is a poke. With one of these the timer only has to
    /// catch what the trigger missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
    /// For a source that is read from a position rather than as a list of
    /// what is unread: where the position is in an answer, and where it goes
    /// in the next call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// Said to the thread with every message from here, for a source whose
    /// messages aren't what someone wrote but a pointer to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Two JSON pointers: `from` into the watch tool's answer, `into` into its
/// arguments. The first call goes out as written, which for such a source
/// means "from now"; every later one carries the position the last answer
/// gave. It is kept on disk, so what arrived while Anna was down is there
/// when she comes back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cursor {
    pub from: String,
    pub into: String,
}

/// One JSON pointer, or several whose values belong together.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Pointers {
    One(String),
    Several(Vec<String>),
}

impl Pointers {
    pub fn each(&self) -> Vec<&str> {
        match self {
            Pointers::One(pointer) => vec![pointer],
            Pointers::Several(pointers) => pointers.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Trigger {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
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

/// Stands for the folder her tools keep their config in, in an `env` value.
/// The config is written once and the folder follows her: a backup restored
/// under another home, or on another machine, still points its tools at
/// where their logins now are.
pub const HER_TOOLS: &str = "{tools}";

/// An `env` map as the program it is for gets it.
pub fn environment(written: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let tools = paths::tools_config_home().to_string_lossy().into_owned();
    written.iter().map(|(name, value)| (name.clone(), value.replace(HER_TOOLS, &tools))).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServer {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Set for the server on top of Anna's own environment. How a tool she
    /// has a profile of her own in is kept to that profile.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
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
                env: BTreeMap::from([("XDG_CONFIG_HOME".to_string(), "/home/someone/.config/anna/tools".to_string())]),
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
