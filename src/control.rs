//! The control socket: how a running Anna is told things from outside.
//!
//! One request per connection, one line of JSON each way. `stop` and `status`
//! are what they say. `poke` is the answer to polling: MCP servers can't say
//! that something arrived, so anything that can — a webhook receiver, a mail
//! filter, cron, a person — runs `anna poke` and she checks her sources now
//! instead of at the next tick. A poke carries no message and no sender; she
//! still reads what happened from the source itself, so being able to poke
//! her is not being able to speak as someone.
//!
//! The socket lives in the runtime directory, which only its owner can enter.
//! It doubles as the lock that keeps a second Anna from starting.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::paths;

pub trait Controls: Send + Sync {
    fn status(&self) -> Value;
    fn poke(&self, source: Option<&str>) -> Result<String>;
    fn stop(&self);
}

/// Being the one Anna here. Taken before anything is started or swept, and
/// held by the kernel for as long as the process lives, so two that start
/// at the same moment can't both win and a crash leaves nothing to clean up.
pub struct OnlyOne {
    _lock: File,
}

pub fn be_the_only_one() -> Result<OnlyOne> {
    let path = paths::runtime_dir().join("anna.lock");
    paths::make_private_dir(&paths::runtime_dir())?;
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;

    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(OnlyOne { _lock: lock })
    } else {
        bail!("Anna is already running")
    }
}

pub fn serve(controls: Arc<dyn Controls>) -> Result<()> {
    let socket = paths::control_socket();
    if let Some(directory) = socket.parent() {
        paths::make_private_dir(directory)?;
    }
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("could not listen on {}", socket.display()))?;

    thread::spawn(move || {
        for connection in listener.incoming().flatten() {
            let controls = controls.clone();
            thread::spawn(move || {
                let _ = answer(connection, controls.as_ref());
            });
        }
    });
    Ok(())
}

pub fn forget_socket() {
    let _ = fs::remove_file(paths::control_socket());
}

fn answer(connection: UnixStream, controls: &dyn Controls) -> Result<()> {
    let mut line = String::new();
    BufReader::new(connection.try_clone()?).read_line(&mut line)?;
    let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);

    let mut connection = connection;
    match request["command"].as_str() {
        Some("status") => writeln!(connection, "{}", json!({ "ok": true, "status": controls.status() }))?,
        Some("poke") => match controls.poke(request["source"].as_str()) {
            Ok(message) => writeln!(connection, "{}", json!({ "ok": true, "message": message }))?,
            Err(error) => writeln!(connection, "{}", json!({ "ok": false, "message": format!("{error:#}") }))?,
        },
        Some("stop") => {
            writeln!(connection, "{}", json!({ "ok": true, "message": "Stopping." }))?;
            connection.flush()?;
            drop(connection);
            controls.stop();
        }
        _ => writeln!(connection, "{}", json!({ "ok": false, "message": "unknown command" }))?,
    }
    Ok(())
}

/// Asks the running Anna something. An error means nobody is listening.
pub fn ask(request: Value) -> Result<Value> {
    let mut connection = UnixStream::connect(paths::control_socket()).context("Anna isn't running")?;
    connection.set_read_timeout(Some(Duration::from_secs(10)))?;
    writeln!(connection, "{request}")?;

    let mut line = String::new();
    BufReader::new(connection).read_line(&mut line)?;
    serde_json::from_str(&line).context("Anna gave no answer")
}

pub fn is_running() -> bool {
    UnixStream::connect(paths::control_socket()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorded {
        poked: Mutex<Vec<Option<String>>>,
        stopped: Mutex<bool>,
    }

    impl Controls for Recorded {
        fn status(&self) -> Value {
            json!({ "sources": ["fake"] })
        }

        fn poke(&self, source: Option<&str>) -> Result<String> {
            if source == Some("nowhere") {
                bail!("there is no source called nowhere");
            }
            self.poked.lock().unwrap().push(source.map(String::from));
            Ok("Checking.".to_string())
        }

        fn stop(&self) {
            *self.stopped.lock().unwrap() = true;
        }
    }

    fn exchange(controls: &Recorded, request: Value) -> Value {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut ours_writer = ours.try_clone().unwrap();
        writeln!(ours_writer, "{request}").unwrap();
        answer(theirs, controls).unwrap();

        let mut line = String::new();
        BufReader::new(ours).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn requests_are_answered_with_one_line_each() {
        let controls = Recorded::default();

        assert_eq!(exchange(&controls, json!({ "command": "status" }))["status"]["sources"][0], "fake");

        assert_eq!(exchange(&controls, json!({ "command": "poke", "source": "fake" }))["ok"], true);
        assert_eq!(exchange(&controls, json!({ "command": "poke" }))["message"], "Checking.");
        assert_eq!(*controls.poked.lock().unwrap(), [Some("fake".to_string()), None]);

        let refused = exchange(&controls, json!({ "command": "poke", "source": "nowhere" }));
        assert_eq!(refused["ok"], false);
        assert_eq!(refused["message"], "there is no source called nowhere");

        assert_eq!(exchange(&controls, json!({ "command": "dance" }))["ok"], false);
        assert!(!*controls.stopped.lock().unwrap());

        assert_eq!(exchange(&controls, json!({ "command": "stop" }))["message"], "Stopping.");
        assert!(*controls.stopped.lock().unwrap());
    }
}
