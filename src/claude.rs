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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::deadline;
use crate::fsutil;
use crate::logs;
use crate::paths;

#[derive(Debug, Deserialize)]
pub struct Reply {
    pub result: String,
    pub session_id: String,
    #[serde(default)]
    pub is_error: bool,
    /// None from a stand-in; from claude, 0 means the prompt never reached
    /// the model — a hook blocked it — though claude calls that a success.
    #[serde(default)]
    pub num_turns: Option<u32>,
}

/// Claude Code itself, never what stands in front of it on the PATH. A mise
/// shim, or a wrapper script that runs mise, works here and not in a
/// sandbox, which has no home, no mise config and no network — every hand
/// would die on startup. For those, mise is asked from the home folder where
/// the installed binary is, as it is for the hands' toolchains.
pub fn binary() -> Result<PathBuf> {
    let found = paths::program("claude")
        .context("claude is not on the PATH")?
        .canonicalize()
        .context("could not resolve the claude binary")?;
    if is_claude_itself(&found) {
        Ok(found)
    } else {
        installed_by_mise().with_context(|| format!("{} stands in front of claude, and mise doesn't say where claude is", found.display()))
    }
}

/// A program rather than a script, and not mise, which a shim resolves to.
fn is_claude_itself(path: &Path) -> bool {
    let mut magic = [0; 4];
    let is_a_program = fs::File::open(path).and_then(|mut it| it.read_exact(&mut magic)).is_ok() && &magic == b"\x7fELF";
    is_a_program && path.file_name().is_some_and(|it| it != "mise")
}

fn installed_by_mise() -> Option<PathBuf> {
    let mut which = Command::new(paths::program("mise")?);
    which.args(["which", "claude"]).current_dir(dirs::home_dir()?);
    let output = deadline::output_within(&mut which, Duration::from_secs(15))?;
    let installed = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()).canonicalize().ok()?;
    is_claude_itself(&installed).then_some(installed)
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
    if reply.num_turns == Some(0) {
        bail!("what was said never reached the model: {}", reply.result);
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

/// A run on each model in turn, for as long as the one before it is out of
/// quota. The run is told whether an earlier model already had a go, so it
/// can pick up from where that one stopped.
pub fn on_each_model<T>(models: &[String], mut run: impl FnMut(&str, bool) -> Result<T>) -> Result<T> {
    let mut outcome = run(&models[0], false);
    for (before, model) in models.iter().zip(&models[1..]) {
        match &outcome {
            Err(error) if error.downcast_ref::<OutOfQuota>().is_some() => {
                logs::event("claude.fell_back", json!({ "from": before, "to": model, "because": format!("{error:#}") }));
                outcome = run(model, true);
            }
            _ => break,
        }
    }
    outcome
}

/// A resume that found nothing: "No conversation found with session ID …".
pub fn says_the_session_is_gone(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("No conversation found with session ID")
}

/// Claude Code refreshes her login now and then, and the token a session
/// held until then is refused from that moment: "401 OAuth access token has
/// been revoked". A hand's copy has no refresh token either, so one that
/// outlives its access token is told "has expired". Either way the session
/// itself is whole and goes on with her current one.
pub fn says_the_login_went_stale(error: &anyhow::Error) -> bool {
    let error = format!("{error:#}");
    error.contains("access token has been revoked") || error.contains("access token has expired")
}

/// A hand's or a reviewer's run, and once more after `refresh` when the
/// login it ran on went stale part way. The run is told whether this is that
/// second go, so it can resume its session instead of starting over.
pub fn again_if_the_login_went_stale<T>(refresh: impl FnOnce() -> Result<()>, mut run: impl FnMut(bool) -> Result<T>) -> Result<T> {
    match run(false) {
        Err(error) if says_the_login_went_stale(&error) => {
            refresh()?;
            run(true)
        }
        outcome => outcome,
    }
}

/// A session id Claude Code will take, made here so a session can be
/// resumed even when its first run never answered.
pub fn random_session_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex: String = bytes.iter().map(|it| format!("{it:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]))
}

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
const SECONDS_TO_ANSWER: u64 = 180;

/// One question to a model about a text, with the text on stdin where it
/// reads as data. No tools beyond Read, no MCP, run from a temp folder.
pub fn ask(model: &str, prompt: &str, text: &str) -> Result<String> {
    let mut child = Command::new(binary()?)
        .args(["-p", prompt, "--model", model])
        .args(["--restricted", "--tools", "Read", "--strict-mcp-config", "--disable-slash-commands"])
        .args(["--output-format", "json"])
        .env("CLAUDE_CONFIG_DIR", paths::claude_config_home())
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not run claude")?;
    let watch = Watch::over(child.id(), Duration::from_secs(SECONDS_TO_ANSWER));
    child.stdin.take().context("claude has no stdin")?.write_all(text.as_bytes())?;

    let output = child.wait_with_output()?;
    if watch.ran_out() {
        bail!("{model} did not answer within {SECONDS_TO_ANSWER} seconds");
    }
    let reply: Reply = serde_json::from_slice(&output.stdout).with_context(|| format!("{model} gave no readable reply"))?;
    if reply.is_error && says_the_subscription_is_used_up(&reply.result) {
        return Err(OutOfQuota { said: reply.result }.into());
    }
    if reply.is_error {
        bail!("{model} reported an error: {}", reply.result);
    }
    Ok(reply.result)
}

/// `claude auth login` in her own folder, at this terminal: the one time
/// a person is needed. Nothing of the person's own Claude folder is read.
pub fn log_in() -> Result<()> {
    let home = paths::claude_config_home();
    paths::make_private_dir(&home)?;
    let status = Command::new(binary()?)
        .args(["auth", "login"])
        .env("CLAUDE_CONFIG_DIR", &home)
        .status()
        .context("could not run claude")?;
    if !status.success() {
        bail!("claude didn't finish logging in");
    }
    println!("{}", login());
    println!("`anna claude account add --alias <name>` keeps this login in her rotation.");
    Ok(())
}

/// Who she is to Claude right now, for `anna status`: the login in the
/// config folder she works from, which is the allowance she is spending.
pub fn login() -> String {
    let home = paths::claude_config_home();
    let account = read_json(&home.join(".claude.json")).ok().map(|it| it["oauthAccount"].clone()).unwrap_or_default();
    match account["emailAddress"].as_str() {
        Some(address) => format!("{address} ({})", home.display()),
        None => format!("nobody is logged in under {}", home.display()),
    }
}

/// How long one shell command of hers may run: a code review or a test
/// suite takes longer than Claude Code's ten minutes, and a wait that is
/// cut off is a turn that ends with the work still running.
const LONGEST_COMMAND_MS: &str = "3600000";

/// What a commit or a pull request says is hers to say: Claude Code's own
/// co-author trailer, "made with" line and session link are all turned off,
/// in her folder and in every hand's. Whatever else is set there stays.
pub fn write_settings(home: &Path) -> Result<()> {
    let path = home.join("settings.json");
    let mut settings = if path.exists() { read_json(&path)? } else { json!({}) };
    settings["attribution"] = json!({ "commit": "", "pr": "", "sessionUrl": false });
    settings["env"]["BASH_MAX_TIMEOUT_MS"] = json!(LONGEST_COMMAND_MS);
    fs::write(&path, serde_json::to_string_pretty(&settings)?).with_context(|| format!("could not write {}", path.display()))
}

pub fn write_hand_profile(profile: &Path) -> Result<()> {
    let home = paths::claude_config_home();
    let credentials: Value = read_json(&home.join(".credentials.json"))?;
    let identity: Value = read_json(&home.join(".claude.json"))?;

    write_private(&profile.join(".credentials.json"), &access_only(&credentials)?)?;
    write_settings(profile)?;
    // A sandboxed session sees no folder of hers but this one, and what she
    // knows about using a tool is worth as much to a hand as to a thread
    crate::skills::copy_into_profile(profile)?;
    write_private(
        &profile.join(".claude.json"),
        &json!({
            "oauthAccount": identity["oauthAccount"],
            "userID": identity["userID"],
            "hasCompletedOnboarding": true,
        }),
    )
}

/// A hand's copy of the login, taken again before a round: Claude Code
/// refreshes the token in her folder now and then and the old one is
/// revoked with it, so a hand sent back an hour later on the copy it started
/// with would be refused at the door and discarded, work and all.
pub fn refresh_hand_login(profile: &Path) -> Result<()> {
    copy_login(&paths::claude_config_home().join(".credentials.json"), &profile.join(".credentials.json"))
}

fn copy_login(from: &Path, to: &Path) -> Result<()> {
    write_private(to, &access_only(&read_json(from)?)?)
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
    fn nothing_she_commits_or_opens_carries_claude_codes_attribution() {
        let home = std::env::temp_dir().join(format!("anna-settings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();

        write_settings(&home).unwrap();
        let written: Value = read_json(&home.join("settings.json")).unwrap();
        assert_eq!(written["attribution"], json!({ "commit": "", "pr": "", "sessionUrl": false }));

        fs::write(home.join("settings.json"), r#"{"theme":"dark","env":{"FOO":"1"},"attribution":{"commit":"Co-authored-by: Claude"}}"#).unwrap();
        write_settings(&home).unwrap();
        let written: Value = read_json(&home.join("settings.json")).unwrap();
        assert_eq!(written["theme"], "dark", "what else is set there stays");
        assert_eq!(written["env"]["FOO"], "1");
        assert_eq!(written["env"]["BASH_MAX_TIMEOUT_MS"], "3600000", "a review or a test suite can be waited for");
        assert_eq!(written["attribution"]["commit"], "");
        assert_eq!(written["attribution"]["sessionUrl"], false);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn the_next_model_takes_over_only_while_the_one_before_it_is_out_of_quota() {
        let models = ["fable".to_string(), "opus".to_string(), "sonnet".to_string()];
        let tried = std::cell::RefCell::new(Vec::new());
        let used_up = |model: &str| OutOfQuota { said: format!("You've hit your {model} limit · resets 8pm") };

        let answer = on_each_model(&models, |model, after_another| {
            tried.borrow_mut().push((model.to_string(), after_another));
            match model {
                "fable" => Err(used_up(model).into()),
                _ => Ok(model.to_string()),
            }
        });
        assert_eq!(answer.unwrap(), "opus");
        assert_eq!(tried.take(), [("fable".to_string(), false), ("opus".to_string(), true)]);

        let broken = on_each_model(&models, |model, _| -> Result<()> {
            tried.borrow_mut().push((model.to_string(), false));
            bail!("claude exited with signal: 9 (SIGKILL)")
        });
        assert!(broken.is_err());
        assert_eq!(tried.take().len(), 1, "any other failure is not the model's allowance");

        let all_out = on_each_model(&models, |model, _| -> Result<()> { Err(used_up(model).into()) });
        assert!(all_out.unwrap_err().downcast_ref::<OutOfQuota>().is_some(), "with every one used up it is a wait, as before");
    }

    #[test]
    fn a_revoked_or_expired_login_is_told_apart_from_other_failures() {
        assert!(says_the_login_went_stale(&anyhow::anyhow!("claude reported an error: Failed to authenticate. API Error: 401 OAuth access token has been revoked.")));
        assert!(says_the_login_went_stale(&anyhow::anyhow!("claude reported an error: Failed to authenticate. API Error: 401 OAuth access token has expired. Re-authenticate to continue.")));
        assert!(!says_the_login_went_stale(&anyhow::anyhow!("claude exited with signal: 9 (SIGKILL)")));
        let id = random_session_id().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
    }

    #[test]
    fn a_wrapper_or_a_shim_in_front_of_claude_is_not_claude() {
        let folder = std::env::temp_dir().join(format!("anna-claude-binary-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let program = std::env::current_exe().unwrap();

        fs::copy(&program, folder.join("claude")).unwrap();
        fs::copy(&program, folder.join("mise")).unwrap();
        fs::write(folder.join("wrapper"), "#!/bin/bash\nmise use -g --quiet claude || exit 1\nexec mise x claude -- claude \"$@\"\n").unwrap();

        assert!(is_claude_itself(&folder.join("claude")));
        assert!(!is_claude_itself(&folder.join("wrapper")), "omarchy's wrapper runs mise, which fails in a sandbox");
        assert!(!is_claude_itself(&folder.join("mise")), "a mise shim resolves to mise");
        assert!(!is_claude_itself(&folder.join("missing")));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn a_run_whose_login_went_stale_goes_again_once_on_a_fresh_one() {
        let refreshed = std::cell::RefCell::new(0);
        let goes = std::cell::RefCell::new(Vec::new());
        let stale = || anyhow::anyhow!("claude reported an error: Failed to authenticate. API Error: 401 OAuth access token has been revoked.");

        let outcome = again_if_the_login_went_stale(
            || {
                *refreshed.borrow_mut() += 1;
                Ok(())
            },
            |again| {
                goes.borrow_mut().push(again);
                if again {
                    Ok("verdict")
                } else {
                    Err(stale())
                }
            },
        );
        assert_eq!(outcome.unwrap(), "verdict");
        assert_eq!((*refreshed.borrow(), goes.take()), (1, vec![false, true]));

        let still_stale = again_if_the_login_went_stale(|| Ok(()), |_| -> Result<()> { Err(stale()) });
        assert!(still_stale.is_err(), "a second stale login is a failure, not a loop");

        let other = again_if_the_login_went_stale(|| panic!("nothing to refresh"), |_| -> Result<()> { bail!("claude exited with signal: 9") });
        assert!(other.is_err());
    }

    #[test]
    fn a_hands_login_is_the_current_access_token_and_never_the_refresh_token() {
        let home = std::env::temp_dir().join(format!("anna-hand-login-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("profile")).unwrap();
        let hers = home.join(".credentials.json");
        let hands = home.join("profile/.credentials.json");

        fs::write(&hers, r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"secret","expiresAt":1}}"#).unwrap();
        copy_login(&hers, &hands).unwrap();
        let copied: Value = read_json(&hands).unwrap();
        assert_eq!(copied["claudeAiOauth"]["accessToken"], "old");
        assert!(copied["claudeAiOauth"].get("refreshToken").is_none(), "a sandbox can never rotate the real login");

        fs::write(&hers, r#"{"claudeAiOauth":{"accessToken":"new","refreshToken":"secret","expiresAt":2}}"#).unwrap();
        copy_login(&hers, &hands).unwrap();
        assert_eq!(read_json(&hands).unwrap()["claudeAiOauth"]["accessToken"], "new", "a round after a refresh runs on the token that works");
        fs::remove_dir_all(home).unwrap();
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

        // What claude answers when a UserPromptSubmit hook blocks the prompt
        let mut blocked = Command::new("sh");
        blocked.args(["-c", r#"echo '{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"UserPromptSubmit operation blocked by hook","session_id":"s3"}'"#]);
        let error = reply_of(&mut blocked, "", Duration::from_secs(10), None).unwrap_err();
        assert!(error.to_string().contains("never reached the model: UserPromptSubmit operation blocked by hook"), "{error}");
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
