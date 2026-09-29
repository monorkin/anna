//! Where Anna keeps tokens: the system keyring when there is one, a private
//! file when there isn't.
//!
//! The keyring is the freedesktop Secret Service, reached through
//! `secret-tool` so there is no D-Bus stack to link. A headless box often has
//! no keyring running, and a user service may start before it is unlocked, so
//! the file is a real second home and not an error path: storing falls back
//! to it, and loading looks in both. Entries are keyed by the config
//! directory, so two Annas on one machine don't share tokens.
//!
//! `ANNA_KEYRING=off` keeps everything in the file.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::deadline;
use crate::fsutil;
use crate::github;
use crate::paths;

const SECONDS_FOR_THE_KEYRING: u64 = 10;

#[derive(Debug, PartialEq)]
pub enum Kept {
    InKeyring,
    InFile,
}

pub fn store(name: &str, value: &str) -> Result<Kept> {
    if keyring_wanted() && store_in_keyring(&format!("Anna: {name}"), &attributes(name), value).is_ok() {
        forget_in_file(name)?;
        Ok(Kept::InKeyring)
    } else {
        store_in_file(name, value)?;
        Ok(Kept::InFile)
    }
}

pub fn load(name: &str) -> Option<String> {
    let from_keyring = if keyring_wanted() { load_from_keyring(&attributes(name)) } else { None };
    from_keyring.or_else(|| load_file().ok()?.remove(name))
}

/// The logins that the tools she has a profile of her own in keep in the
/// keyring instead of their config — gh's token, Basecamp's — so her folder
/// for those tools can travel as files and still arrive logged out. Only
/// hers: the person's own logins for the same tools sit next to hers, under
/// the same service, and never leave with her.
pub fn tool_logins() -> Vec<ToolLogin> {
    if keyring_wanted() {
        let basecamp = fs::read_to_string(paths::tools_config_home().join("basecamp/config.json")).ok();
        tool_logins_named(github::login(), basecamp.as_deref())
            .into_iter()
            .filter_map(|(service, username)| {
                let secret = load_from_keyring(&tool_attributes(&service, &username))?;
                Some(ToolLogin { service, username, secret })
            })
            .collect()
    } else {
        Vec::new()
    }
}

/// Put back where the tool looks for it, under the label it gives its own.
pub fn restore_tool_login(login: &ToolLogin) -> Result<()> {
    if !keyring_wanted() {
        bail!("there is no keyring here");
    }
    let label = format!("Password for '{}' on '{}'", login.username, login.service);
    store_in_keyring(&label, &tool_attributes(&login.service, &login.username), &login.secret)
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolLogin {
    pub service: String,
    pub username: String,
    pub secret: String,
}

fn tool_logins_named(github_login: Option<String>, basecamp_config: Option<&str>) -> Vec<(String, String)> {
    let github = github_login.map(|login| ("gh:github.com".to_string(), login));
    let basecamp_profiles = basecamp_config
        .and_then(|it| serde_json::from_str::<Value>(it).ok())
        .and_then(|it| it["profiles"].as_object().map(|profiles| profiles.keys().cloned().collect::<Vec<_>>()))
        .unwrap_or_default();
    github
        .into_iter()
        .chain(basecamp_profiles.into_iter().map(|profile| ("basecamp".to_string(), format!("basecamp::profile:{profile}"))))
        .collect()
}

/// How gh and Basecamp's CLI name what they keep: go-keyring's two.
fn tool_attributes(service: &str, username: &str) -> [String; 4] {
    ["service".to_string(), service.to_string(), "username".to_string(), username.to_string()]
}

fn keyring_wanted() -> bool {
    std::env::var_os("ANNA_KEYRING").is_none_or(|it| it != "off") && paths::program("secret-tool").is_some()
}

fn store_in_keyring(label: &str, attributes: &[String], value: &str) -> Result<()> {
    let mut child = Command::new("secret-tool")
        .args(["store", "--label", label])
        .args(attributes)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("could not run secret-tool")?;
    child.stdin.take().context("secret-tool has no stdin")?.write_all(value.as_bytes())?;

    if child.wait()?.success() {
        Ok(())
    } else {
        bail!("the keyring did not take it")
    }
}

/// A locked keyring asks someone to unlock it and waits for them. Nobody is
/// there when Anna starts on her own, so the lookup is given up on and the
/// file is tried instead.
fn load_from_keyring(attributes: &[String]) -> Option<String> {
    let mut lookup = Command::new("secret-tool");
    lookup.arg("lookup").args(attributes);

    let output = deadline::output_within(&mut lookup, Duration::from_secs(SECONDS_FOR_THE_KEYRING))?;
    let value = String::from_utf8(output.stdout).ok()?;
    if output.status.success() && !value.is_empty() {
        Some(value)
    } else {
        None
    }
}

fn attributes(name: &str) -> [String; 6] {
    [
        "service".to_string(),
        "anna".to_string(),
        "config".to_string(),
        paths::config_dir().to_string_lossy().into_owned(),
        "name".to_string(),
        name.to_string(),
    ]
}

fn store_in_file(name: &str, value: &str) -> Result<()> {
    let mut secrets = load_file().unwrap_or_default();
    secrets.insert(name.to_string(), value.to_string());
    fsutil::write_private(&file(), &(serde_json::to_string_pretty(&secrets)? + "\n"))
}

fn forget_in_file(name: &str) -> Result<()> {
    let mut secrets = load_file().unwrap_or_default();
    if secrets.remove(name).is_some() {
        fsutil::write_private(&file(), &(serde_json::to_string_pretty(&secrets)? + "\n"))?;
    }
    Ok(())
}

fn load_file() -> Result<BTreeMap<String, String>> {
    let path = file();
    if path.exists() {
        Ok(serde_json::from_str(&fsutil::read_private(&path)?)?)
    } else {
        Ok(BTreeMap::new())
    }
}

fn file() -> PathBuf {
    paths::config_dir().join("secrets.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn her_tool_logins_are_her_github_login_and_her_basecamp_profiles() {
        let basecamp = r#"{ "experimental": { "tui": true }, "profiles": { "anna": { "scope": "full" }, "work": { "scope": "read" } } }"#;
        assert_eq!(
            tool_logins_named(Some("example-bot".to_string()), Some(basecamp)),
            [
                ("gh:github.com".to_string(), "example-bot".to_string()),
                ("basecamp".to_string(), "basecamp::profile:anna".to_string()),
                ("basecamp".to_string(), "basecamp::profile:work".to_string()),
            ]
        );
        assert_eq!(tool_logins_named(None, None), []);
        assert_eq!(tool_logins_named(None, Some("not json")), [], "a config the CLI can't read has no logins to take");
    }
}
