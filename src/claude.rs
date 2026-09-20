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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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

/// A run that was stopped because it used up the time it is allowed. This is
/// the one hard stop Anna has, so callers can tell it from things going wrong.
#[derive(Debug)]
pub struct OutOfTime {
    pub minutes: u64,
}

impl std::fmt::Display for OutOfTime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "stopped after the {} minutes a single run is allowed", self.minutes)
    }
}

impl std::error::Error for OutOfTime {}

pub fn reply_of(command: &mut Command, limit: Duration) -> Result<Reply> {
    let child = command
        .args(["--output-format", "json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run claude")?;

    let ran_out = Arc::new(AtomicBool::new(false));
    let (finished, watchdog) = mpsc::channel::<()>();
    let process = child.id() as libc::pid_t;
    let flag = ran_out.clone();
    thread::spawn(move || {
        if watchdog.recv_timeout(limit).is_err() {
            flag.store(true, Ordering::Relaxed);
            stop_with_everything_it_started(process);
        }
    });

    let output = child.wait_with_output().context("could not wait for claude")?;
    let _ = finished.send(());
    if ran_out.load(Ordering::Relaxed) {
        return Err(OutOfTime { minutes: limit.as_secs() / 60 }.into());
    }

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

/// Claude Code runs shell commands in sessions of their own, so neither the
/// process nor its group reaches them. The whole tree is read off /proc
/// first — once the root dies its children are re-parented and can't be
/// found any more — and then all of it is killed.
fn stop_with_everything_it_started(root: libc::pid_t) {
    let mut doomed = everything_started_by(root);
    doomed.push(root);
    kill(&doomed);
}

/// Everything below a process, leaving the process itself alone — for Anna
/// stopping her own threads and hands on the way out.
pub fn stop_everything_started_by(root: libc::pid_t) {
    kill(&everything_started_by(root));
}

fn everything_started_by(root: libc::pid_t) -> Vec<libc::pid_t> {
    let parents = parents_by_process();
    let mut family = vec![root];

    let mut index = 0;
    while index < family.len() {
        let parent = family[index];
        family.extend(parents.iter().filter(|(_, its_parent)| *its_parent == parent).map(|(process, _)| *process));
        index += 1;
    }
    family.split_off(1)
}

fn kill(processes: &[libc::pid_t]) {
    for process in processes {
        unsafe {
            libc::kill(*process, libc::SIGKILL);
        }
    }
}

fn parents_by_process() -> Vec<(libc::pid_t, libc::pid_t)> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let process: libc::pid_t = entry.file_name().to_str()?.parse().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            Some((process, parent_in(&stat)?))
        })
        .collect()
}

/// The parent is the second field after the command name, and the command
/// name is in parentheses and may itself contain spaces and parentheses.
fn parent_in(stat: &str) -> Option<libc::pid_t> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(1)?.parse().ok()
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

    fn still_running(marker: &str) -> bool {
        fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|entry| fs::read(entry.path().join("cmdline")).ok())
            .any(|command_line| String::from_utf8_lossy(&command_line).contains(marker))
    }

    #[test]
    fn a_run_is_stopped_when_its_time_is_up() {
        let mut quick = Command::new("sh");
        quick.args(["-c", r#"echo '{"result":"done","session_id":"s1"}'"#]);
        assert_eq!(reply_of(&mut quick, Duration::from_secs(10)).unwrap().result, "done");

        let mut endless = Command::new("sh");
        endless.args(["-c", "setsid sleep 31.4159 & wait"]);
        let error = reply_of(&mut endless, Duration::from_millis(300)).unwrap_err();
        assert!(error.downcast_ref::<OutOfTime>().is_some());
        thread::sleep(Duration::from_millis(100));
        assert!(!still_running("31.4159"), "a command in its own session outlived the stop");

        assert_eq!(parent_in("4242 (tmux: server (1)) S 17 4242 4242 0 -1"), Some(17));

        let mut failing = Command::new("sh");
        failing.args(["-c", r#"echo '{"result":"no quota","session_id":"s2","is_error":true}'"#]);
        assert!(reply_of(&mut failing, Duration::from_secs(10)).unwrap_err().to_string().contains("no quota"));
    }

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
