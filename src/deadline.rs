//! Running a small program without waiting on it for ever.
//!
//! Anna asks other programs things on her way up — the keyring for a token,
//! mise for its tools — and nobody is there to notice if one of them never
//! answers. Each gets a few seconds, and an answer that doesn't come is
//! treated as no answer.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// What the program printed and how it ended, or nothing when it couldn't be
/// run or was still going when its time was up — in which case it is stopped.
pub fn output_within(command: &mut Command, limit: Duration) -> Option<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Read on the side, so a program that prints more than a pipe holds
    // doesn't wait on us while we wait on it
    let mut stdout = child.stdout.take()?;
    let reading = thread::spawn(move || {
        let mut printed = Vec::new();
        let _ = stdout.read_to_end(&mut printed);
        printed
    });

    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Some(Output { status, stdout: reading.join().unwrap_or_default(), stderr: Vec::new() });
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_waited_for_only_so_long() {
        let mut quick = Command::new("sh");
        quick.args(["-c", "echo hello"]);
        let output = output_within(&mut quick, Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");

        let mut stuck = Command::new("sh");
        stuck.args(["-c", "exec sleep 30"]);
        let began = Instant::now();
        assert!(output_within(&mut stuck, Duration::from_millis(200)).is_none());
        assert!(began.elapsed() < Duration::from_secs(2));

        assert!(output_within(&mut Command::new("/no/such/program"), Duration::from_secs(1)).is_none());
    }
}
