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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::fsutil;
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

/// What a turn has running on its behalf that isn't below it: hands and
/// reviewers are started by the broker, so stopping the turn's own process
/// doesn't reach them. When the turn is over, whatever is still going is
/// stopped, and nothing more is started for it.
#[derive(Default)]
pub struct Started {
    processes: Mutex<HashSet<u32>>,
    over: AtomicBool,
}

impl Started {
    pub fn stop_all(&self) {
        self.over.store(true, Ordering::Relaxed);
        for process in self.processes.lock().unwrap().drain() {
            stop_with_everything_it_started(process as libc::pid_t);
        }
    }

    pub fn is_over(&self) -> bool {
        self.over.load(Ordering::Relaxed)
    }

    fn add(&self, process: u32) {
        self.processes.lock().unwrap().insert(process);
        if self.is_over() {
            self.stop_all();
        }
    }

    fn remove(&self, process: u32) {
        self.processes.lock().unwrap().remove(&process);
    }
}

/// One headless run. What claude is told goes in on stdin, never as an
/// argument: arguments are there for every user of the machine to read, and
/// what people write to Anna is not theirs to see. They also have a size
/// limit that a long review would run into.
pub fn reply_of(command: &mut Command, told: &str, limit: Duration, started: Option<&Started>) -> Result<Reply> {
    if started.is_some_and(|it| it.is_over()) {
        bail!("the turn this was for is over");
    }
    let mut child = command
        .args(["-p", "--output-format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run claude")?;

    // On the side: more than a pipe holds would otherwise wait for claude to
    // read while claude waits for us to read what it prints
    let mut stdin = child.stdin.take().context("claude has no stdin")?;
    let told = told.to_string();
    thread::spawn(move || {
        let _ = stdin.write_all(told.as_bytes());
    });

    let process = child.id();
    let watch = Watch::over(process, limit);
    if let Some(started) = started {
        started.add(process);
    }
    let output = child.wait_with_output().context("could not wait for claude");
    if let Some(started) = started {
        started.remove(process);
    }
    let output = output?;
    if watch.ran_out() {
        return Err(OutOfTime { minutes: limit.as_secs() / 60 }.into());
    }

    let reply: Reply = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "claude exited with {} and no readable reply: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;

    if reply.is_error && says_the_subscription_is_used_up(&reply.result) {
        return Err(OutOfQuota { said: reply.result }.into());
    }
    if reply.is_error {
        bail!("claude reported an error: {}", reply.result);
    }
    Ok(reply)
}

/// The subscription's allowance is used up for now. Nothing is wrong and
/// nothing was started: the same run will work later, so callers can tell it
/// from a failure and wait.
#[derive(Debug)]
pub struct OutOfQuota {
    pub said: String,
}

impl std::fmt::Display for OutOfQuota {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "the Claude subscription is used up for now: {}", self.said)
    }
}

impl std::error::Error for OutOfQuota {}

/// Claude Code says it in a sentence, not a code: "You've hit your session
/// limit · resets 8:20pm", "Claude usage limit reached".
fn says_the_subscription_is_used_up(said: &str) -> bool {
    let said = said.to_lowercase();
    said.contains("limit") && ["session", "usage", "weekly", "resets", "reached"].iter().any(|it| said.contains(it))
}

/// Stops a process, and everything it started, if it is still going when its
/// time is up.
struct Watch {
    finished: mpsc::Sender<()>,
    ran_out: Arc<AtomicBool>,
}

impl Watch {
    fn over(process: u32, limit: Duration) -> Watch {
        let ran_out = Arc::new(AtomicBool::new(false));
        let (finished, watchdog) = mpsc::channel::<()>();
        let flag = ran_out.clone();
        thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = watchdog.recv_timeout(limit) {
                flag.store(true, Ordering::Relaxed);
                stop_with_everything_it_started(process as libc::pid_t);
            }
        });
        Watch { finished, ran_out }
    }

    /// Asked once the process has been waited for, which also calls the
    /// watch off.
    fn ran_out(self) -> bool {
        let _ = self.finished.send(());
        self.ran_out.load(Ordering::Relaxed)
    }
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

/// Judging and editing sit in front of every message in and out, outside the
/// time a run is given, so they get a limit of their own.
const SECONDS_FOR_HAIKU: u64 = 180;

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
    let watch = Watch::over(child.id(), Duration::from_secs(SECONDS_FOR_HAIKU));
    child.stdin.take().context("claude has no stdin")?.write_all(text.as_bytes())?;

    let output = child.wait_with_output()?;
    if watch.ran_out() {
        bail!("haiku did not answer within {SECONDS_FOR_HAIKU} seconds");
    }
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

/// Private from the first byte, in a folder that is: it holds a Claude
/// login, and written and then closed off it could be read in between.
fn write_private(path: &Path, value: &Value) -> Result<()> {
    fsutil::write_private(path, &value.to_string())
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
        quick.args(["-c", r#"read told; echo "{\"result\":\"heard: $told\",\"session_id\":\"s1\"}""#]);
        let reply = reply_of(&mut quick, "fix the login", Duration::from_secs(10), None).unwrap();
        assert_eq!(reply.result, "heard: fix the login", "what claude is told arrives on stdin");
        assert!(!quick.get_args().any(|it| it.to_string_lossy().contains("fix the login")), "and never as an argument");

        let mut endless = Command::new("sh");
        endless.args(["-c", "setsid sleep 31.4159 & wait"]);
        let error = reply_of(&mut endless, "", Duration::from_millis(300), None).unwrap_err();
        assert!(error.downcast_ref::<OutOfTime>().is_some());
        thread::sleep(Duration::from_millis(100));
        assert!(!still_running("31.4159"), "a command in its own session outlived the stop");

        assert_eq!(parent_in("4242 (tmux: server (1)) S 17 4242 4242 0 -1"), Some(17));

        let mut failing = Command::new("sh");
        failing.args(["-c", r#"echo '{"result":"no quota","session_id":"s2","is_error":true}'"#]);
        assert!(reply_of(&mut failing, "", Duration::from_secs(10), None).unwrap_err().to_string().contains("no quota"));
    }

    #[test]
    fn a_used_up_subscription_is_told_apart_from_something_going_wrong() {
        let mut used_up = Command::new("sh");
        used_up.args(["-c", r#"echo '{"result":"You have hit your session limit · resets 8:20pm (Europe/Zagreb)","session_id":"s1","is_error":true}'"#]);
        let error = reply_of(&mut used_up, "", Duration::from_secs(10), None).unwrap_err();
        assert!(error.context("the thread could not finish its turn").downcast_ref::<OutOfQuota>().is_some(), "and still is once it has been given context");

        assert!(says_the_subscription_is_used_up("Claude usage limit reached. Your limit will reset at 9pm."));
        assert!(!says_the_subscription_is_used_up("Invalid API key · Please run /login"));
        assert!(!says_the_subscription_is_used_up("The file is over the size limit"));
    }

    #[test]
    fn what_a_turn_started_is_stopped_when_the_turn_is_over() {
        let started = Arc::new(Started::default());
        let for_the_hand = started.clone();
        let hand = thread::spawn(move || {
            let mut working = Command::new("sh");
            working.args(["-c", "setsid sleep 27.1828 & wait"]);
            reply_of(&mut working, "", Duration::from_secs(60), Some(&for_the_hand))
        });

        thread::sleep(Duration::from_millis(300));
        assert!(still_running("27.1828"));
        started.stop_all();
        assert!(hand.join().unwrap().is_err());
        assert!(!still_running("27.1828"), "a hand outlived the turn it worked for");

        let mut late = Command::new("sh");
        late.args(["-c", "echo '{}'"]);
        let refused = reply_of(&mut late, "", Duration::from_secs(10), Some(&started)).unwrap_err();
        assert_eq!(refused.to_string(), "the turn this was for is over");
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
