//! What was said in a thread or a hand, read back: `anna transcript`.
//!
//! Claude Code writes every session down as it goes, one JSON line per
//! turn, under the config folder the session ran with. A thread's lives
//! with the login she works as. A hand's would die with its profile, so it
//! is moved into her data folder before the profile goes, and its reviews
//! with it. Reading either is the same: the person's words, hers, and each
//! tool she reached for, with the long parts cut short.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;

const LONGEST_SHOWN: usize = 400;

/// Moves every transcript under a session's Claude config folder into her
/// own folder for that hand. A review is `review-*.jsonl` there, so it sits
/// next to the work it judged.
pub fn keep(profile: &Path, hand: &str, as_review: bool) {
    let kept = paths::hands_dir().join(hand);
    for transcript in transcripts_under(&profile.join("projects")) {
        let name = transcript.file_name().unwrap_or_default().to_string_lossy();
        let name = if as_review { format!("review-{name}") } else { name.into_owned() };
        if paths::make_private_dir(&kept).is_ok() {
            let _ = fs::rename(&transcript, kept.join(&name)).or_else(|_| fs::copy(&transcript, kept.join(&name)).map(|_| ()));
        }
    }
}

/// A thread's transcripts: Claude Code keeps a folder's under the folder's
/// absolute path with everything but letters and digits turned into dashes.
pub fn of_thread(thread: &str) -> PathBuf {
    let folder: String = paths::thread_dir(thread)
        .to_string_lossy()
        .chars()
        .map(|it| if it.is_ascii_alphanumeric() { it } else { '-' })
        .collect();
    paths::claude_config_home().join("projects").join(folder)
}

pub fn list() -> Result<()> {
    let threads = names_in(&paths::data_dir().join("threads"));
    let hands = names_in(&paths::hands_dir());
    if threads.is_empty() && hands.is_empty() {
        println!("Nothing yet. Threads and hands leave their transcripts here as they work.");
        return Ok(());
    }

    if !threads.is_empty() {
        println!("Threads:");
        for thread in threads {
            let transcripts = transcripts_under(&of_thread(&thread));
            println!("  {thread}  ({} transcript{})", transcripts.len(), if transcripts.len() == 1 { "" } else { "s" });
        }
    }
    if !hands.is_empty() {
        println!("Hands:");
        for hand in hands {
            let files = transcripts_under(&paths::hands_dir().join(&hand));
            let reviews = files.iter().filter(|it| it.file_name().unwrap_or_default().to_string_lossy().starts_with("review-")).count();
            println!("  {hand}  ({} round{}, {reviews} review{})", files.len() - reviews, if files.len() - reviews == 1 { "" } else { "s" }, if reviews == 1 { "" } else { "s" });
        }
    }
    println!("\n`anna transcript <id>` shows one; any part of the id will do.");
    Ok(())
}

pub fn show(id: &str) -> Result<()> {
    let thread = names_in(&paths::data_dir().join("threads")).into_iter().find(|it| it.contains(id));
    let hand = names_in(&paths::hands_dir()).into_iter().find(|it| it.contains(id));
    let busy = busy_hand(id);

    let (what, transcripts) = match (thread, hand, busy) {
        (Some(thread), _, _) => (format!("thread {thread}"), transcripts_under(&of_thread(&thread))),
        (None, Some(hand), _) => (format!("hand {hand}"), transcripts_under(&paths::hands_dir().join(hand))),
        (None, None, Some((hand, profile))) => (format!("hand {hand}, still working"), transcripts_under(&profile.join("projects"))),
        (None, None, None) => bail!("nothing called {id}; `anna transcript` lists what there is"),
    };
    if transcripts.is_empty() {
        bail!("{what} has no transcript yet");
    }

    for transcript in transcripts {
        println!("== {what}: {}", transcript.file_name().unwrap_or_default().to_string_lossy());
        print!("{}", rendered(&fs::read_to_string(&transcript).with_context(|| format!("could not read {}", transcript.display()))?));
        println!();
    }
    Ok(())
}

/// A hand that hasn't been discarded still has its transcript in its
/// profile, under whichever Anna process is running it.
fn busy_hand(id: &str) -> Option<(String, PathBuf)> {
    for process in names_in(&paths::all_sessions_dir()) {
        for hand in names_in(&paths::all_sessions_dir().join(&process)) {
            if hand.contains(id) {
                return Some((hand.clone(), paths::all_sessions_dir().join(process).join(hand).join("profile")));
            }
        }
    }
    None
}

fn transcripts_under(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut to_look = vec![directory.to_path_buf()];
    while let Some(place) = to_look.pop() {
        let Ok(entries) = fs::read_dir(&place) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                to_look.push(path);
            } else if path.extension().is_some_and(|it| it == "jsonl") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn names_in(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(directory) {
        Ok(entries) => entries.flatten().map(|it| it.file_name().to_string_lossy().into_owned()).collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// The conversation as prose: who said what, and every tool call as one
/// line. Claude Code's own bookkeeping lines are skipped.
fn rendered(transcript: &str) -> String {
    let mut out = String::new();
    for line in transcript.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let at = entry["timestamp"].as_str().and_then(|it| it.get(11..19)).unwrap_or("        ");
        match entry["type"].as_str() {
            Some("user") => render_user(&entry["message"]["content"], at, &mut out),
            Some("assistant") => render_assistant(&entry["message"]["content"], at, &mut out),
            _ => {}
        }
    }
    out
}

fn render_user(content: &Value, at: &str, out: &mut String) {
    match content {
        Value::String(text) => out.push_str(&format!("{at}  > {}\n\n", shortened(text))),
        Value::Array(parts) => {
            for part in parts {
                match part["type"].as_str() {
                    Some("text") => out.push_str(&format!("{at}  > {}\n\n", shortened(part["text"].as_str().unwrap_or_default()))),
                    Some("tool_result") => {
                        let text = match &part["content"] {
                            Value::String(text) => text.clone(),
                            Value::Array(items) => items.iter().filter_map(|it| it["text"].as_str()).collect::<Vec<_>>().join("\n"),
                            _ => String::new(),
                        };
                        let mark = if part["is_error"].as_bool().unwrap_or(false) { "✗" } else { "←" };
                        out.push_str(&format!("{at}    {mark} {}\n", shortened(&text)));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn render_assistant(content: &Value, at: &str, out: &mut String) {
    let Some(parts) = content.as_array() else {
        return;
    };
    for part in parts {
        match part["type"].as_str() {
            Some("text") => out.push_str(&format!("{at}  {}\n\n", shortened(part["text"].as_str().unwrap_or_default()))),
            Some("tool_use") => {
                let name = part["name"].as_str().unwrap_or("?").trim_start_matches("mcp__anna__");
                out.push_str(&format!("{at}    → {name} {}\n", shortened(&arguments_of(&part["input"]))));
            }
            _ => {}
        }
    }
}

/// The one argument that says what a call was about, when there is an
/// obvious one; the whole object otherwise.
fn arguments_of(input: &Value) -> String {
    for key in ["command", "brief", "text", "message", "title", "pattern", "file_path", "query"] {
        if let Some(text) = input[key].as_str() {
            return text.to_string();
        }
    }
    input.to_string()
}

fn shortened(text: &str) -> String {
    let text = text.trim();
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > LONGEST_SHOWN {
        format!("{}…", one_line.chars().take(LONGEST_SHOWN).collect::<String>())
    } else {
        one_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcript_reads_as_a_conversation_with_its_tool_calls() {
        let transcript = [
            r#"{"type":"user","timestamp":"2026-09-22T04:54:34.000Z","message":{"role":"user","content":"Marta says: fix the login"}}"#,
            r#"{"type":"assistant","timestamp":"2026-09-22T04:54:40.000Z","message":{"content":[{"type":"text","text":"I'll read it first."},{"type":"tool_use","name":"mcp__anna__basecamp_todos","input":{"action":"get_todo","params":{"todoId":1}}}]}}"#,
            r#"{"type":"user","timestamp":"2026-09-22T04:54:41.000Z","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"{\"title\":\"Login 500s\"}"}],"is_error":false}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-09-22T04:54:45.000Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls\n  -la","description":"list"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-09-22T04:54:46.000Z","message":{"content":[{"type":"tool_result","content":"no such file","is_error":true}]}}"#,
            r#"{"type":"ai-title","title":"noise"}"#,
        ]
        .join("\n");

        let expected = "04:54:34  > Marta says: fix the login\n\n\
                        04:54:40  I'll read it first.\n\n\
                        04:54:40    → basecamp_todos {\"action\":\"get_todo\",\"params\":{\"todoId\":1}}\n\
                        04:54:41    ← {\"title\":\"Login 500s\"}\n\
                        04:54:45    → Bash ls -la\n\
                        04:54:46    ✗ no such file\n";
        assert_eq!(rendered(&transcript), expected);
    }
}
