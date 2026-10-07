//! A quiet thread's session, compacted while Claude still has it cached.
//!
//! Every turn resends the whole session. While Claude's prompt cache holds
//! it that is cheap; once the cache has gone cold, the next wake writes all
//! of it again — nine hundred thousand tokens for a thread that had talked
//! for days. So a thread that has gone quiet is compacted shortly before
//! its cache would expire, and what the next wake writes is the summary.
//!
//! How long the cache lives isn't fixed. It is read from the session's own
//! transcript: each call says which lifetime its cache write was for.
//!
//! A thread stopped in the middle of a turn is compacted on the way out:
//! its cache is as warm as it gets, and after a stop the cache is cold by
//! the time she is back.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::claude;
use crate::logs;
use crate::paths;
use crate::runtime::Runtime;
use crate::transcripts;

const QUIET_BEFORE_LOOKING: Duration = Duration::from_secs(10 * 60);
const BEFORE_IT_GOES_COLD: Duration = Duration::from_secs(10 * 60);
/// A session this small costs little to write again, and compacting it
/// would only lose what it remembers.
const WORTH_COMPACTING: u64 = 50_000;
const AT_ONCE_WHEN_SWITCHING: usize = 4;

/// How many turns each conversation has had, so a compaction set up when a
/// thread went quiet can tell that it has spoken since, and the session of
/// each turn running now.
#[derive(Default)]
pub struct Cooling {
    turns: Mutex<HashMap<String, u64>>,
    running: Mutex<HashMap<String, String>>,
}

impl Cooling {
    pub fn woke(&self, conversation: &str, session: &str) {
        self.next_turn(conversation);
        self.running.lock().unwrap().insert(conversation.to_string(), session.to_string());
    }

    pub fn turn_over(&self, conversation: &str) {
        self.running.lock().unwrap().remove(conversation);
    }

    /// Each conversation with a turn running, and that turn's session. Taken
    /// before the turns are stopped, since a stopped turn is over.
    pub fn running(&self) -> Vec<(String, String)> {
        self.running.lock().unwrap().clone().into_iter().collect()
    }

    pub fn went_quiet(&self, runtime: &Arc<Runtime>, conversation: &str, session: &str) {
        let quiet_since = self.next_turn(conversation);
        let (runtime, conversation, session) = (runtime.clone(), conversation.to_string(), session.to_string());
        thread::spawn(move || {
            thread::sleep(QUIET_BEFORE_LOOKING);
            if runtime.cooling.still_quiet(&conversation, quiet_since)
                && let Err(error) = compact_before_it_goes_cold(&runtime, &conversation, &session, quiet_since)
            {
                logs::event("thread.not_compacted", json!({ "conversation": conversation, "because": format!("{error:#}") }));
            }
        });
    }

    fn next_turn(&self, conversation: &str) -> u64 {
        let mut turns = self.turns.lock().unwrap();
        let turn = turns.entry(conversation.to_string()).or_default();
        *turn += 1;
        *turn
    }

    fn still_quiet(&self, conversation: &str, since: u64) -> bool {
        self.turns.lock().unwrap().get(conversation) == Some(&since)
    }
}

fn compact_before_it_goes_cold(runtime: &Arc<Runtime>, conversation: &str, session: &str, quiet_since: u64) -> Result<()> {
    let last = last_call_of(conversation, session)?;
    if last.context < WORTH_COMPACTING {
        return Ok(());
    }

    let compact_at = last.at + last.cache_lives - BEFORE_IT_GOES_COLD;
    let Ok(wait) = (compact_at - Utc::now()).to_std() else {
        bail!("its cache lives {} min, too short to compact before it goes cold", last.cache_lives.as_secs() / 60)
    };
    logs::event(
        "thread.compaction_set",
        json!({ "conversation": conversation, "at": compact_at.to_rfc3339(), "tokens": last.context, "cache_minutes": last.cache_lives.as_secs() / 60 }),
    );
    thread::sleep(wait);

    // Through its queue of turns, so it never runs alongside one
    let (queued, conversation, session) = (runtime.clone(), conversation.to_string(), session.to_string());
    runtime.turns.add(
        &conversation.clone(),
        Box::new(move || {
            if queued.cooling.still_quiet(&conversation, quiet_since) {
                match compact(&conversation, &session, &last.model) {
                    Ok(()) => logs::event("thread.compacted", json!({ "conversation": conversation, "tokens": last.context })),
                    Err(error) => logs::event("thread.not_compacted", json!({ "conversation": conversation, "because": format!("{error:#}") })),
                }
            }
        }),
    );
    Ok(())
}

/// The sessions of turns that were stopped, all at once, and back when every
/// one is done.
pub fn compact_the_stopped(stopped: Vec<(String, String)>) {
    let compacting: Vec<_> = stopped
        .into_iter()
        .map(|(conversation, session)| {
            thread::spawn(move || {
                if let Err(error) = compact_now(&conversation, &session) {
                    logs::event("thread.not_compacted", json!({ "conversation": conversation, "because": format!("{error:#}") }));
                }
            })
        })
        .collect();
    for it in compacting {
        let _ = it.join();
    }
}

/// Every thread's session worth compacting, a few at a time: those still
/// cached first, since they cost least, then the rest by how recently they
/// were used. Once the account says it has nothing left, the rest stay as
/// they are. How many were compacted.
pub fn compact_every_thread() -> usize {
    let mut sessions: Vec<(String, String, LastCall)> = fs::read_dir(paths::data_dir().join("threads"))
        .map(|it| it.flatten().collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let conversation = entry.file_name().to_string_lossy().into_owned();
            let session = fs::read_to_string(entry.path().join("session")).ok()?.trim().to_string();
            let last = last_call_of(&conversation, &session).ok()?;
            (last.context >= WORTH_COMPACTING).then_some((conversation, session, last))
        })
        .collect();
    cheapest_first(&mut sessions, Utc::now());

    let mut compacted = 0;
    for batch in sessions.chunks(AT_ONCE_WHEN_SWITCHING) {
        let outcomes: Vec<Result<()>> = std::thread::scope(|scope| {
            let running: Vec<_> = batch
                .iter()
                .map(|(conversation, session, last)| scope.spawn(move || compact(conversation, session, &last.model).map(|_| (conversation, last))))
                .collect();
            running
                .into_iter()
                .map(|it| {
                    let (conversation, last) = it.join().unwrap()?;
                    logs::event("thread.compacted", json!({ "conversation": conversation, "tokens": last.context }));
                    Ok(())
                })
                .collect()
        });
        compacted += outcomes.iter().filter(|it| it.is_ok()).count();
        let spent = outcomes.iter().any(|it| it.as_ref().is_err_and(|error| claude::says_the_subscription_is_used_up(&format!("{error:#}"))));
        for error in outcomes.into_iter().filter_map(Result::err) {
            logs::event("thread.not_compacted", json!({ "because": format!("{error:#}") }));
        }
        if spent {
            break;
        }
    }
    compacted
}

/// Still cached before gone cold, and the most recently used first in each.
fn cheapest_first<T>(sessions: &mut [(T, T, LastCall)], now: DateTime<Utc>) {
    sessions.sort_by_key(|(_, _, last)| (last.at + last.cache_lives < now, std::cmp::Reverse(last.at)));
}

fn compact_now(conversation: &str, session: &str) -> Result<()> {
    let last = last_call_of(conversation, session)?;
    if last.context >= WORTH_COMPACTING {
        compact(conversation, session, &last.model)?;
        logs::event("thread.compacted", json!({ "conversation": conversation, "tokens": last.context }));
    }
    Ok(())
}

fn last_call_of(conversation: &str, session: &str) -> Result<LastCall> {
    let transcript = transcripts::of_thread(conversation).join(format!("{session}.jsonl"));
    last_call(&logs::end_of(&transcript, 512 * 1024)).context("its transcript has no call that wrote to the cache")
}

fn compact(conversation: &str, session: &str, model: &str) -> Result<()> {
    let output = Command::new(claude::binary()?)
        .current_dir(paths::thread_dir(conversation))
        .env("CLAUDE_CONFIG_DIR", paths::claude_config_home())
        .args(["-p", "--output-format", "json", "--strict-mcp-config", "--model", model, "--resume", session, "/compact"])
        .stdin(Stdio::null())
        .output()
        .context("could not run claude")?;
    let reply: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("claude exited with {} and no readable reply: {}", output.status, String::from_utf8_lossy(&output.stderr).trim()))?;
    if reply["is_error"].as_bool() == Some(false) {
        Ok(())
    } else {
        bail!("claude reported an error: {}", reply["result"])
    }
}

#[derive(Debug, PartialEq)]
struct LastCall {
    at: DateTime<Utc>,
    model: String,
    context: u64,
    cache_lives: Duration,
}

/// The session's last call of the model, and how long the cache it last
/// wrote lives. A call that only read the cache wrote nothing, so the
/// lifetime comes from the latest one that wrote, and from whichever of its
/// lifetimes held most of what it wrote: that is the session itself.
fn last_call(transcript: &str) -> Option<LastCall> {
    let calls: Vec<Value> = transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == "assistant" && entry["message"]["usage"].is_object())
        .collect();
    let last = calls.last()?;
    let usage = &last["message"]["usage"];
    let context = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
        .iter()
        .filter_map(|it| usage[it].as_u64())
        .sum();
    let cache_lives = calls.iter().rev().find_map(|it| lifetime_written(&it["message"]["usage"]["cache_creation"]))?;

    Some(LastCall {
        at: last["timestamp"].as_str()?.parse().ok()?,
        model: last["message"]["model"].as_str()?.to_string(),
        context,
        cache_lives,
    })
}

/// From `{"ephemeral_1h_input_tokens": 2546, "ephemeral_5m_input_tokens": 0}`.
fn lifetime_written(cache_creation: &Value) -> Option<Duration> {
    let (name, _) = cache_creation
        .as_object()?
        .iter()
        .filter_map(|(name, tokens)| Some((name, tokens.as_u64()?)))
        .filter(|(_, tokens)| *tokens > 0)
        .max_by_key(|(_, tokens)| *tokens)?;
    let lifetime = name.strip_prefix("ephemeral_")?.strip_suffix("_input_tokens")?;
    let (count, unit) = lifetime.split_at(lifetime.len() - 1);
    let count: u64 = count.parse().ok()?;
    match unit {
        "m" => Some(Duration::from_secs(count * 60)),
        "h" => Some(Duration::from_secs(count * 60 * 60)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(at: &str, read: u64, written_1h: u64, written_5m: u64) -> String {
        json!({
            "type": "assistant",
            "timestamp": at,
            "message": {
                "model": "claude-fable-5-1",
                "usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": read,
                    "cache_creation_input_tokens": written_1h + written_5m,
                    "cache_creation": { "ephemeral_1h_input_tokens": written_1h, "ephemeral_5m_input_tokens": written_5m },
                },
            },
        })
        .to_string()
    }

    #[test]
    fn the_cache_lifetime_is_read_from_the_last_call_that_wrote_to_it() {
        let transcript = [
            call("2026-10-06T09:00:00Z", 0, 400_000, 0),
            r#"{"type":"user","message":{"content":"Ping"}}"#.to_string(),
            call("2026-10-06T09:05:00Z", 400_000, 0, 0),
        ]
        .join("\n");
        assert_eq!(
            last_call(&transcript),
            Some(LastCall {
                at: "2026-10-06T09:05:00Z".parse().unwrap(),
                model: "claude-fable-5-1".to_string(),
                context: 400_010,
                cache_lives: Duration::from_secs(60 * 60),
            }),
            "the last call only read, so its lifetime is the one written before it"
        );

        let short = [call("2026-10-06T09:00:00Z", 0, 300, 90_000), call("2026-10-06T09:01:00Z", 90_000, 0, 0)].join("\n");
        assert_eq!(last_call(&short).unwrap().cache_lives, Duration::from_secs(5 * 60), "whichever lifetime held most of the write");

        assert_eq!(last_call(&call("2026-10-06T09:00:00Z", 1_000, 0, 0)), None, "nothing written, nothing known");
        assert_eq!(last_call(""), None);
    }

    #[test]
    fn a_compaction_set_up_when_a_thread_went_quiet_is_dropped_once_it_wakes_again() {
        let cooling = Cooling::default();
        let quiet_since = cooling.next_turn("card-1");
        assert!(cooling.still_quiet("card-1", quiet_since));

        cooling.woke("card-2", "s2");
        assert!(cooling.still_quiet("card-1", quiet_since), "another conversation waking changes nothing");

        cooling.woke("card-1", "s1");
        assert!(!cooling.still_quiet("card-1", quiet_since));
    }

    #[test]
    fn the_turns_running_when_she_stops_are_the_ones_compacted() {
        let cooling = Cooling::default();
        cooling.woke("card-1", "s1");
        cooling.woke("card-2", "s2");
        cooling.turn_over("card-2");

        assert_eq!(cooling.running(), [("card-1".to_string(), "s1".to_string())]);
    }

    #[test]
    fn before_a_switch_the_threads_still_cached_are_compacted_first_and_the_newest_first_among_each() {
        let at = |minutes_ago: i64| LastCall {
            at: "2026-10-06T12:00:00Z".parse::<DateTime<Utc>>().unwrap() - chrono::Duration::minutes(minutes_ago),
            model: "claude-fable-5-1".to_string(),
            context: 300_000,
            cache_lives: Duration::from_secs(60 * 60),
        };
        let mut sessions = vec![
            ("cold-old", "", at(600)),
            ("warm-old", "", at(50)),
            ("cold-new", "", at(90)),
            ("warm-new", "", at(5)),
        ];
        cheapest_first(&mut sessions, "2026-10-06T12:00:00Z".parse().unwrap());

        let order: Vec<&str> = sessions.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(order, ["warm-new", "warm-old", "cold-new", "cold-old"]);
    }
}
