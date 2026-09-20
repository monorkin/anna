//! Running Claude Code headlessly, and the credentials it runs with.
//!
//! A hand gets a profile holding the access token and nothing else. Without
//! the refresh token a sandbox can never rotate the real login out from under
//! the brain; refreshing stays on the brain's side, which rewrites the hand's
//! credentials file from outside — Claude Code re-reads it mid-session.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::paths;

#[derive(Debug, Deserialize)]
pub struct Reply {
    pub result: String,
    pub session_id: String,
    #[serde(default)]
    pub is_error: bool,
}

pub fn binary() -> Result<PathBuf> {
    paths::program("claude")
        .context("claude is not on the PATH")?
        .canonicalize()
        .context("could not resolve the claude binary")
}

pub fn reply_of(command: &mut Command) -> Result<Reply> {
    let output = command
        .args(["--output-format", "json"])
        .output()
        .context("could not run claude")?;
    let reply: Reply = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "claude exited with {} and no readable reply: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;

    if reply.is_error {
        bail!("claude reported an error: {}", reply.result);
    }
    Ok(reply)
}

/// One question to haiku about a text, with the text on stdin where it reads
/// as data. No tools beyond Read, no MCP, run from a temp folder.
pub fn ask_haiku(prompt: &str, text: &str) -> Result<String> {
    let mut child = Command::new(binary()?)
        .args(["-p", prompt, "--model", "haiku"])
        .args(["--restricted", "--tools", "Read", "--strict-mcp-config", "--disable-slash-commands"])
        .args(["--output-format", "json"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not run claude")?;
    child.stdin.take().context("claude has no stdin")?.write_all(text.as_bytes())?;

    let output = child.wait_with_output()?;
    let reply: Reply = serde_json::from_slice(&output.stdout).context("haiku gave no readable reply")?;
    if reply.is_error {
        bail!("haiku reported an error: {}", reply.result);
    }
    Ok(reply.result)
}

pub fn write_hand_profile(profile: &Path) -> Result<()> {
    let home = paths::claude_config_home();
    let credentials: Value = read_json(&home.join(".credentials.json"))?;
    let identity: Value = read_json(&home.join(".claude.json"))?;

    fs::create_dir_all(profile)?;
    write_private(&profile.join(".credentials.json"), &access_only(&credentials)?)?;
    write_private(
        &profile.join(".claude.json"),
        &json!({
            "oauthAccount": identity["oauthAccount"],
            "userID": identity["userID"],
            "hasCompletedOnboarding": true,
        }),
    )
}

fn access_only(credentials: &Value) -> Result<Value> {
    let mut login = credentials["claudeAiOauth"]
        .as_object()
        .context("the Claude login has no OAuth credentials")?
        .clone();
    login.retain(|key, _| !key.starts_with("refreshToken"));
    Ok(json!({ "claudeAiOauth": login }))
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))
}

fn write_private(path: &Path, value: &Value) -> Result<()> {
    fs::write(path, value.to_string())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_never_gets_the_refresh_token() {
        let credentials = json!({ "claudeAiOauth": {
            "accessToken": "access",
            "refreshToken": "refresh",
            "refreshTokenExpiresAt": 2,
            "expiresAt": 1,
            "scopes": ["user:inference"],
        }});

        assert_eq!(
            access_only(&credentials).unwrap(),
            json!({ "claudeAiOauth": { "accessToken": "access", "expiresAt": 1, "scopes": ["user:inference"] } })
        );
        assert!(access_only(&json!({})).is_err());
    }
}
