//! A source's trigger: a command that runs for as long as Anna does and
//! prints a line whenever something happens there, so the source is checked
//! then and not only on its timer.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread as os_thread;
use std::time::Duration;

use crate::config::{self, Trigger};
use crate::logs;

const SECONDS_BEFORE_RESTARTING_A_TRIGGER: u64 = 10;

/// Runs a source's trigger for as long as Anna runs, poking the source for
/// every line it prints. What the line says doesn't matter: she reads what
/// happened from the source itself, so a trigger can't put words in anyone's
/// mouth. A trigger that exits is started again after a pause — watchers lose
/// their connection now and then, and one that can't start at all shouldn't
/// spin.
pub fn watch(source: String, trigger: Trigger, poke: Sender<()>) {
    os_thread::spawn(move || {
        loop {
            logs::event("trigger.starting", json!({ "source": source, "command": trigger.command }));
            if let Err(error) = poke_for_every_line(&trigger, &poke) {
                logs::event("trigger.failed", json!({ "source": source, "error": format!("{error:#}") }));
            }
            os_thread::sleep(Duration::from_secs(SECONDS_BEFORE_RESTARTING_A_TRIGGER));
        }
    });
}

/// Returns when the trigger exits, having waited for it so it leaves no
/// zombie behind.
fn poke_for_every_line(trigger: &Trigger, poke: &Sender<()>) -> Result<()> {
    let mut command = Command::new(&trigger.command);
    command
        .args(&trigger.args)
        .envs(config::environment(&trigger.env))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }

    let mut child = command.spawn().with_context(|| format!("could not start {}", trigger.command))?;
    let output = child.stdout.take().context("the trigger has no output")?;
    for _line in BufReader::new(output).lines().map_while(|line| line.ok()) {
        let _ = poke.send(());
    }

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        bail!("{} exited with {status}", trigger.command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn trigger(script: &str) -> Trigger {
        Trigger {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: Default::default(),
        }
    }

    fn trigger_called(command: &str) -> Trigger {
        Trigger { command: command.to_string(), args: Vec::new(), env: Default::default() }
    }

    #[test]
    fn every_line_a_trigger_prints_is_a_poke() {
        let (poke, poked) = mpsc::channel();

        poke_for_every_line(&trigger("echo '{\"event\":\"new\"}'; echo ready"), &poke).unwrap();
        assert_eq!(poked.try_iter().count(), 2);

        let error = poke_for_every_line(&trigger("echo once; exit 3"), &poke).unwrap_err();
        assert!(error.to_string().contains("exited with"));
        assert_eq!(poked.try_iter().count(), 1);

        assert!(poke_for_every_line(&trigger_called("no-such-program-anywhere"), &poke).is_err());
    }
}
