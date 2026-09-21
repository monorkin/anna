//! `anna start`, `stop`, `status` and `poke`: the commands that talk to a
//! running Anna, or bring one up.
//!
//! When the systemd service is installed, starting and stopping go through
//! it, so systemd's idea of whether she is running stays true. Otherwise
//! `start` puts `anna run` in the background itself, and `stop` asks her over
//! the control socket.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::control;
use crate::paths;
use crate::service;

const PATIENCE: Duration = Duration::from_secs(20);

pub fn start() -> Result<()> {
    if control::is_running() {
        println!("Anna is already running.");
        return Ok(());
    }

    if service::installed() {
        service::start()?;
    } else {
        start_in_the_background()?;
    }

    if wait_until(control::is_running) {
        println!("Anna is running. `anna log -f` shows what she's doing.");
        Ok(())
    } else {
        bail!("Anna did not come up; see {}", paths::output_file().display())
    }
}

pub fn stop() -> Result<()> {
    if !control::is_running() {
        println!("Anna isn't running.");
        return Ok(());
    }

    if service::active() {
        service::stop()?;
    } else {
        control::ask(json!({ "command": "stop" }))?;
    }

    if wait_until(|| !control::is_running()) {
        println!("Anna stopped.");
        Ok(())
    } else {
        bail!("Anna is still running")
    }
}

pub fn status() -> Result<()> {
    match control::ask(json!({ "command": "status" })) {
        Ok(answer) => {
            let status = &answer["status"];
            println!("Anna has been running since {}, as process {}.", text(&status["since"]), status["process"]);
            println!("Working as: {}", text(&status["claude_login"]));
            println!("Listening on: {}", names(&status["sources"]));
            println!("Conversations going on: {}", status["conversations"]);
        }
        Err(_) => println!("Anna isn't running."),
    }
    Ok(())
}

pub fn poke(source: Option<&str>) -> Result<()> {
    let answer = control::ask(json!({ "command": "poke", "source": source }))?;
    if answer["ok"].as_bool().unwrap_or(false) {
        println!("{}", text(&answer["message"]));
        Ok(())
    } else {
        bail!("{}", text(&answer["message"]))
    }
}

/// `anna run` in a session of its own, so it survives the terminal that
/// started it, with its output kept where `start` can point at it.
fn start_in_the_background() -> Result<()> {
    let output_path = paths::output_file();
    if let Some(directory) = output_path.parent() {
        fs::create_dir_all(directory)?;
    }
    let output = OpenOptions::new().create(true).append(true).open(&output_path)?;

    let mut command = Command::new(std::env::current_exe().context("could not determine Anna's own path")?);
    command
        .arg("run")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output);
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().context("could not start Anna")?;
    Ok(())
}

fn wait_until(condition: impl Fn() -> bool) -> bool {
    let began = Instant::now();
    while began.elapsed() < PATIENCE {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(250));
    }
    condition()
}

fn text(value: &serde_json::Value) -> &str {
    value.as_str().unwrap_or("-")
}

fn names(value: &serde_json::Value) -> String {
    value
        .as_array()
        .map(|items| items.iter().filter_map(|it| it.as_str()).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}
