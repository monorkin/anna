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

/// Stop, then start. Nothing is lost in between: every turn that was going
/// or waiting is kept in the store and asked again when she is back.
pub fn restart() -> Result<()> {
    stop()?;
    start()
}

pub fn status() -> Result<()> {
    match control::ask(json!({ "command": "status" })) {
        Ok(answer) => {
            let status = &answer["status"];
            println!("Anna has been running since {}, as process {}.", text(&status["since"]), status["process"]);
            println!("Working as: {}", text(&status["claude_login"]));
            if let Some(login) = status["github_login"].as_str() {
                println!("On GitHub as: {login}");
            }
            println!("Listening on: {}", names(&status["sources"]));
            match status["going_on"].as_array() {
                Some(turns) => {
                    print_going_on(turns);
                    print_board(&status["board"]);
                }
                // An Anna started before this build knows the count and
                // nothing else; saying nothing is going on would be a guess
                None => println!("Conversations going on: {}", status["conversations"]),
            }
        }
        Err(_) => println!("Anna isn't running."),
    }
    Ok(())
}

pub fn poke(source: Option<&str>) -> Result<()> {
    answered(control::ask(json!({ "command": "poke", "source": source }))?)
}

pub fn tell(thread: &str, message: &str) -> Result<()> {
    answered(control::ask(json!({ "command": "tell", "thread": thread, "message": message }))?)
}

/// What a running Anna said back: printed when she did it, an error when she
/// couldn't.
fn answered(answer: serde_json::Value) -> Result<()> {
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

fn print_going_on(turns: &[serde_json::Value]) {
    if turns.is_empty() {
        println!("\nNothing going on right now.");
        return;
    }

    println!("\nGoing on right now:");
    for turn in turns {
        println!("  {} for {}{}", text(&turn["conversation"]), text(&turn["for"]), behind(&turn["waiting"]));
        if let Some(doing) = turn["doing"].as_str() {
            println!("      {doing}");
        }
        for hand in turn["hands"].as_array().map(Vec::as_slice).unwrap_or_default() {
            println!(
                "      hand {} in {}, {}",
                text(&hand["hand"]),
                text(&hand["project"]),
                text(&hand["for"])
            );
        }
    }
}

fn print_board(board: &serde_json::Value) {
    let claimed = board.as_array().map(Vec::as_slice).unwrap_or_default();
    if claimed.is_empty() {
        return;
    }

    println!("\nClaimed, between turns:");
    for work in claimed {
        println!("  {}", text(&work["conversation"]));
        println!("      {}", text(&work["doing"]));
    }
}

fn behind(waiting: &serde_json::Value) -> String {
    match waiting.as_u64().unwrap_or(0) {
        0 => String::new(),
        1 => ", 1 message waiting".to_string(),
        many => format!(", {many} messages waiting"),
    }
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
