//! The one record of what Anna did: every thread woken, hand started, grant
//! written, and request refused is a line of JSON in a single append-only
//! file. `anna log` reads it back. Logging never fails loudly — a full disk
//! shouldn't take a thread down with it.

use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::thread;
use std::time::Duration;

use crate::clock;
use crate::paths;

pub fn event(name: &str, details: Value) {
    if cfg!(test) {
        return;
    }
    let path = paths::log_file();
    if let Some(directory) = path.parent() {
        if fs::create_dir_all(directory).is_err() {
            return;
        }
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let line = json!({ "at": clock::timestamp(), "event": name, "details": details });
        let _ = writeln!(file, "{line}");
    }
}

pub fn print(follow: bool) -> anyhow::Result<()> {
    let mut reader = BufReader::new(File::open(paths::log_file())?);
    print_new_lines(&mut reader)?;

    while follow {
        thread::sleep(Duration::from_millis(500));
        print_new_lines(&mut reader)?;
    }
    Ok(())
}

fn print_new_lines(reader: &mut BufReader<File>) -> anyhow::Result<()> {
    let mut line = String::new();
    loop {
        let position = reader.stream_position()?;
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line.ends_with('\n') {
            println!("{}", render(&line));
        } else {
            reader.seek(SeekFrom::Start(position))?;
            return Ok(());
        }
    }
}

fn render(line: &str) -> String {
    match serde_json::from_str::<Value>(line) {
        Ok(entry) => format!(
            "{} {} {}",
            entry["at"].as_str().unwrap_or("-"),
            entry["event"].as_str().unwrap_or("-"),
            entry["details"]
        ),
        Err(_) => line.trim_end().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_render_on_one_line() {
        let line = r#"{"at":"2026-09-20T10:00:00Z","event":"hand.started","details":{"id":"h1"}}"#;
        assert_eq!(
            render(line),
            r#"2026-09-20T10:00:00Z hand.started {"id":"h1"}"#
        );
        assert_eq!(render("not json\n"), "not json");
    }
}
