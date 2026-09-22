//! The one record of what Anna did: every thread woken, hand started, grant
//! written, and request refused is a line of JSON in a single append-only
//! file. `anna log` reads it back. Logging never fails loudly — a full disk
//! shouldn't take a thread down with it.

use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::thread;
use std::time::Duration;

use crate::clock;
use crate::paths;
use crate::prompt;

pub fn event(name: &str, details: Value) {
    if cfg!(test) {
        return;
    }
    let path = paths::log_file();
    // Who wrote to her, which tools she called and what went wrong are
    // nobody else's to read
    if let Some(directory) = path.parent() {
        if paths::make_private_dir(directory).is_err() {
            return;
        }
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).mode(0o600).open(path) {
        // One write for the whole line: threads and processes log at once,
        // and an append is only whole per write call
        let line = json!({ "at": clock::timestamp(), "event": name, "details": details }).to_string() + "\n";
        let _ = file.write_all(line.as_bytes());
    }
}

/// `only` keeps the lines of one thread, hand or conversation, by any part
/// of its id.
pub fn print(follow: bool, only: Option<&str>) -> anyhow::Result<()> {
    let mut reader = BufReader::new(File::open(paths::log_file())?);
    let look = Look { styled: prompt::styled_for_stdout(), only: only.map(String::from) };
    print_new_lines(&mut reader, &look)?;

    while follow {
        thread::sleep(Duration::from_millis(500));
        print_new_lines(&mut reader, &look)?;
    }
    Ok(())
}

fn print_new_lines(reader: &mut BufReader<File>, look: &Look) -> anyhow::Result<()> {
    let mut line = String::new();
    loop {
        let position = reader.stream_position()?;
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line.ends_with('\n') {
            if let Some(rendered) = look.render(&line) {
                println!("{rendered}");
            }
        } else {
            reader.seek(SeekFrom::Start(position))?;
            return Ok(());
        }
    }
}

struct Look {
    styled: bool,
    only: Option<String>,
}

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";
const PLAIN: &str = "\x1b[0m";

impl Look {
    /// `time  who  event  the rest`. Who is the thread, hand or conversation
    /// the line is about, so a log with several going at once can be read
    /// by column; the rest is the details with those keys taken out.
    fn render(&self, line: &str) -> Option<String> {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return Some(line.trim_end().to_string());
        };
        let event = entry["event"].as_str().unwrap_or("-");
        let mut details = entry["details"].as_object().cloned().unwrap_or_default();
        let who = WHO_KEYS.iter().find_map(|key| details.remove(*key)).and_then(|it| it.as_str().map(short_id)).unwrap_or_default();

        if let Some(only) = &self.only {
            if !who.contains(only.as_str()) && !line.contains(only.as_str()) {
                return None;
            }
        }

        let rest = details
            .iter()
            .map(|(key, value)| match value {
                Value::String(text) => format!("{key}={}", text),
                other => format!("{key}={other}"),
            })
            .collect::<Vec<_>>()
            .join("  ");
        let at = entry["at"].as_str().unwrap_or("-");
        let at = at.get(11..19).unwrap_or(at);

        if self.styled {
            Some(format!("{DIM}{at}{PLAIN}  {CYAN}{who:<18}{PLAIN}  {}{event:<24}{PLAIN}  {rest}", colour_of(event)))
        } else {
            Some(format!("{at}  {who:<18}  {event:<24}  {rest}"))
        }
    }
}

const WHO_KEYS: [&str; 4] = ["hand", "by", "thread", "conversation"];

/// An id short enough for a column: the first part and the last few
/// characters of a long one.
fn short_id(id: &str) -> String {
    if id.chars().count() <= 18 {
        id.to_string()
    } else {
        let head: String = id.chars().take(11).collect();
        let tail: String = id.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{head}…{tail}")
    }
}

fn colour_of(event: &str) -> &'static str {
    if event.ends_with(".failed") || event.ends_with(".refused") || event.ends_with(".withheld") || event.ends_with(".dropped") {
        RED
    } else if event.contains("waiting") || event.contains("out_of_time") || event.contains("without") || event.contains("unchecked") || event.contains("ignored") || event.contains("left_out") {
        YELLOW
    } else if event.ends_with(".woken") || event.ends_with(".started") || event.ends_with(".listening") {
        GREEN
    } else if event.starts_with("broker.") || event.starts_with("proxy.") {
        DIM
    } else if event.ends_with(".finished") || event.ends_with(".slept") || event.ends_with(".claimed") {
        MAGENTA
    } else {
        BOLD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_render_by_column_and_can_be_kept_to_one_hand() {
        let plain = Look { styled: false, only: None };
        let line = r#"{"at":"2026-09-20T10:00:00Z","event":"hand.started","details":{"hand":"h18d78bc145875c5d","project":"/srv/p"}}"#;
        assert_eq!(plain.render(line).unwrap(), "10:00:00  h18d78bc145875c5d   hand.started              project=/srv/p");
        assert_eq!(plain.render("not json\n").unwrap(), "not json");

        let by_hand = r#"{"at":"2026-09-20T10:00:01Z","event":"broker.called","details":{"by":"hand-h18d78bc145875c5d","tool":"Read","ms":3}}"#;
        assert_eq!(plain.render(by_hand).unwrap(), "10:00:01  hand-h18d78…875c5d  broker.called             ms=3  tool=Read");

        let one = Look { styled: false, only: Some("h18d78".to_string()) };
        assert!(one.render(line).is_some());
        assert!(one.render(by_hand).is_some());
        assert!(one.render(r#"{"at":"2026-09-20T10:00:02Z","event":"thread.woken","details":{"conversation":"basecamp-1"}}"#).is_none());

        let styled = Look { styled: true, only: None };
        assert!(styled.render(line).unwrap().contains(GREEN));
    }
}
