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
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::fsutil;
use crate::paths;

const SECONDS_FOR_THE_KEYRING: u64 = 10;

#[derive(Debug, PartialEq)]
pub enum Kept {
    InKeyring,
    InFile,
}

pub fn store(name: &str, value: &str) -> Result<Kept> {
    if keyring_wanted() && store_in_keyring(name, value).is_ok() {
        forget_in_file(name)?;
        Ok(Kept::InKeyring)
    } else {
        store_in_file(name, value)?;
        Ok(Kept::InFile)
    }
}

pub fn load(name: &str) -> Option<String> {
    let from_keyring = if keyring_wanted() { load_from_keyring(name) } else { None };
    from_keyring.or_else(|| load_file().ok()?.remove(name))
}

fn keyring_wanted() -> bool {
    std::env::var_os("ANNA_KEYRING").is_none_or(|it| it != "off") && paths::program("secret-tool").is_some()
}

fn store_in_keyring(name: &str, value: &str) -> Result<()> {
    let mut child = Command::new("secret-tool")
        .args(["store", "--label", &format!("Anna: {name}")])
        .args(attributes(name))
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
fn load_from_keyring(name: &str) -> Option<String> {
    let mut child = Command::new("secret-tool")
        .arg("lookup")
        .args(attributes(name))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + Duration::from_secs(SECONDS_FOR_THE_KEYRING);
    while child.try_wait().ok()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let output = child.wait_with_output().ok()?;
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
