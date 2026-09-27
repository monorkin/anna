//! The one record of what Anna did: every thread woken, hand started, grant
//! written, and request refused is a line of JSON in a single append-only
//! file. `anna log` reads it back. Logging never fails loudly — a full disk
//! shouldn't take a thread down with it.

use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
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
    if let Some(directory) = path.parent()
        && paths::make_private_dir(directory).is_err()
    {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).mode(0o600).open(path) {
        // One write for the whole line: threads and processes log at once,
        // and an append is only whole per write call
        let line = json!({ "at": clock::timestamp(), "event": name, "details": details }).to_string() + "\n";
        let _ = file.write_all(line.as_bytes());
    }
}

/// The newest lines of the log, each as `anna log` shows it in colour, the
/// newest last.
pub fn newest_styled(count: usize) -> Vec<String> {
    let look = Look { styled: true, only: None };
    let end = end_of(&paths::log_file(), 256 * 1024);
    let rendered: Vec<String> = end.lines().filter_map(|it| look.render(it)).collect();
    rendered[rendered.len().saturating_sub(count)..].to_vec()
}

/// At most the last `bytes` of a file of lines, from the first whole line
/// in them on.
pub fn end_of(file: &Path, bytes: u64) -> String {
    let Ok(mut opened) = File::open(file) else {
        return String::new();
    };
    let start = opened.metadata().map(|it| it.len()).unwrap_or(0).saturating_sub(bytes);
    let mut end = Vec::new();
    if opened.seek(SeekFrom::Start(start)).is_err() || opened.read_to_end(&mut end).is_err() {
        return String::new();
    }

    let text = String::from_utf8_lossy(&end).into_owned();
    if start == 0 {
        text
    } else {
        text.split_once('\n').map(|(_, whole)| whole.to_string()).unwrap_or_default()
    }
}

/// `only` keeps the lines of one thread, hand or conversation, by any part
/// of its id.
pub fn print(follow: bool, only: Option<&str>) -> anyhow::Result<()> {
    let mut reader = BufReader::new(File::open(paths::log_file())?);
    let look = Look { styled: prompt::styled_for_stdout(), only: only.map(String::from) };
    print_new_lines(&mut reader, &look)?;
    if !follow {
        return Ok(());
    }

    loop {
        thread::sleep(Duration::from_millis(500));
        print_new_lines(&mut reader, &look)?;
    }
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

        if let Some(only) = &self.only
            && !who.contains(only.as_str())
            && !line.contains(only.as_str())
        {
            return None;
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
            Some(format!("{DIM}{at}{PLAIN}  {CYAN}{who:<18}{PLAIN}  {}{event:<24}{PLAIN}  {rest}", colour_of(event, &entry["details"])))
        } else {
            Some(format!("{at}  {who:<18}  {event:<24}  {rest}"))
        }
    }
}

const WHO_KEYS: [&str; 4] = ["hand", "by", "thread", "conversation"];

/// An id short enough for a column: the first part and the last few
/// characters of a long one.
pub fn short_id(id: &str) -> String {
    if id.chars().count() <= 18 {
        id.to_string()
    } else {
        let head: String = id.chars().take(11).collect();
        let tail: String = id.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{head}…{tail}")
    }
}

/// Red for what went wrong, yellow for what fell short or was let go, green
/// for what began or came back good, dim for the routine. A review is
/// coloured by its verdict.
fn colour_of(event: &str, details: &Value) -> &'static str {
    let rejected = event == "review.finished" && details["accepted"] == false;
    let accepted = event == "review.finished" && details["accepted"] == true;
    if rejected || [".failed", ".refused", ".rejected", ".withheld", ".dropped"].iter().any(|it| event.ends_with(it)) {
        RED
    } else if ["waiting", "out_of_time", "without", "unchecked", "ignored", "left_out", "discarded", "fell_back"].iter().any(|it| event.contains(it)) {
        YELLOW
    } else if accepted || [".woken", ".started", ".listening", ".reported"].iter().any(|it| event.ends_with(it)) {
        GREEN
    } else if event.starts_with("broker.") || event.starts_with("proxy.") || event.ends_with(".slept") {
        DIM
    } else if event.ends_with(".finished") || event.ends_with(".claimed") {
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

    #[test]
    fn events_are_coloured_by_how_they_went() {
        let none = Value::Null;
        assert_eq!(colour_of("broker.refused", &none), RED);
        assert_eq!(colour_of("editor.rejected", &none), RED);
        assert_eq!(colour_of("review.finished", &json!({ "accepted": false })), RED);
        assert_eq!(colour_of("hand.discarded", &none), YELLOW);
        assert_eq!(colour_of("judge.fell_back", &none), YELLOW);
        assert_eq!(colour_of("turn.out_of_time", &none), YELLOW);
        assert_eq!(colour_of("review.finished", &json!({ "accepted": true })), GREEN);
        assert_eq!(colour_of("hand.reported", &none), GREEN);
        assert_eq!(colour_of("thread.woken", &none), GREEN);
        assert_eq!(colour_of("thread.slept", &none), DIM);
        assert_eq!(colour_of("work.claimed", &none), MAGENTA);
        assert_eq!(colour_of("schedule.added", &none), BOLD);
    }

    #[test]
    fn the_end_of_a_file_starts_at_a_whole_line() {
        let directory = std::env::temp_dir().join(format!("anna-end-of-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("log.jsonl");
        std::fs::write(&file, "first line\nsecond\nthird\n").unwrap();

        assert_eq!(end_of(&file, 12), "third\n", "the cut-through \"second\" is left out");
        assert_eq!(end_of(&file, 1_000), "first line\nsecond\nthird\n");
        assert_eq!(end_of(&directory.join("missing"), 10), "");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
