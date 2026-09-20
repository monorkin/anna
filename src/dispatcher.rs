//! `anna run`: listen on every source and wake the right thread for every
//! message.
//!
//! No model decides who gets heard. Who sent a message comes from the
//! source's own data, never from the text, and whether Anna listens to them
//! is a lookup in the config. Text from someone she listens to is still
//! untrusted — the judge screens it before a thread, which is not sandboxed,
//! reads it.
//!
//! One thread per conversation, never two turns of the same thread at once;
//! different conversations run side by side.

use anyhow::{Result, bail};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread as os_thread;
use std::time::Duration;

use crate::config::Source;
use crate::conversation::Conversation;
use crate::judge::{MANIPULATION_QUESTION, SUSPICIOUS};
use crate::logs;
use crate::runtime::Runtime;
use crate::source::{self, Message, Seen, Sourced};
use crate::thread;

const SWITCH_AT_PERCENT: f64 = 90.0;
const SECONDS_BETWEEN_ACCOUNT_CHECKS: u64 = 60;

#[derive(Default)]
struct Turns {
    by_conversation: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Turns {
    fn of(&self, conversation: &str) -> Arc<Mutex<()>> {
        self.by_conversation
            .lock()
            .unwrap()
            .entry(conversation.to_string())
            .or_default()
            .clone()
    }
}

pub fn run() -> Result<()> {
    let runtime = Arc::new(Runtime::start()?);
    if runtime.config.sources.is_empty() {
        bail!("there are no sources to listen on yet; `anna chat` works without them");
    }

    if runtime.config.rotate_accounts {
        rotate_accounts();
    }
    let turns = Arc::new(Turns::default());
    let listeners: Vec<_> = runtime
        .config
        .sources
        .clone()
        .into_iter()
        .map(|(name, source)| {
            let runtime = runtime.clone();
            let turns = turns.clone();
            os_thread::spawn(move || listen(&runtime, &turns, &name, &source))
        })
        .collect();

    logs::event("anna.listening", json!({ "sources": runtime.config.sources.keys().collect::<Vec<_>>() }));
    for listener in listeners {
        let _ = listener.join();
    }
    Ok(())
}

/// ax's auto-switch for as long as Anna runs. Threads run as the default
/// Claude login and every new hand copies its token from there, so moving the
/// default login to the account with the most headroom covers both. Only what
/// changes is logged: a switch, or a check that failed.
fn rotate_accounts() {
    os_thread::spawn(|| {
        let mut last = String::new();
        loop {
            let outcome = match ax::auto_switch::tick(SWITCH_AT_PERCENT) {
                Ok(outcome) => outcome,
                Err(error) => format!("check failed: {error:#}"),
            };
            if outcome != last && !outcome.contains("staying put") {
                logs::event("accounts.checked", json!({ "outcome": outcome }));
            }
            last = outcome;
            os_thread::sleep(Duration::from_secs(SECONDS_BETWEEN_ACCOUNT_CHECKS));
        }
    });
}

fn listen(runtime: &Arc<Runtime>, turns: &Arc<Turns>, name: &str, source: &Source) {
    let mut seen = Seen::load(name);
    loop {
        if let Err(error) = check(runtime, turns, name, source, &mut seen) {
            logs::event("source.failed", json!({ "source": name, "error": format!("{error:#}") }));
        }
        os_thread::sleep(Duration::from_secs(source.every_seconds));
    }
}

fn check(runtime: &Arc<Runtime>, turns: &Arc<Turns>, name: &str, source: &Source, seen: &mut Seen) -> Result<()> {
    let answer = runtime.catalog.call(&source.server, &source.watch.tool, &source.watch.arguments)?;

    for message in source::messages_in(source, &answer)? {
        if seen.is_new(&message.id) {
            dispatch(runtime, turns, name, source, message);
        }
    }
    seen.save()
}

fn dispatch(runtime: &Arc<Runtime>, turns: &Arc<Turns>, name: &str, source: &Source, message: Message) {
    let Some(person) = runtime.config.people.get(&message.sender).cloned() else {
        logs::event("message.ignored", json!({ "source": name, "sender": message.sender }));
        return;
    };
    let conversation = Arc::new(Sourced::new(name, source, &message.conversation, runtime.catalog.clone()));
    if runtime.judge.suspects(MANIPULATION_QUESTION, &message.text, SUSPICIOUS) {
        logs::event("message.refused", json!({ "source": name, "sender": message.sender, "message": message.id }));
        let _ = conversation.say("That read like instructions aimed at an AI rather than something from you, so I didn't act on it. If it was you, say it again in your own words.");
        return;
    }

    let turn = turns.of(conversation.key());
    let runtime = runtime.clone();
    let said = format!("{person} says, on {name}:\n\n{}", message.text);

    os_thread::spawn(move || {
        let _one_at_a_time = turn.lock().unwrap();
        if let Err(error) = thread::wake(&runtime, conversation.clone(), &said) {
            logs::event("thread.failed", json!({ "conversation": conversation.key(), "error": format!("{error:#}") }));
            let _ = conversation.say("Something broke on my side before I could finish this. I've logged it; ask me again and I'll pick it back up.");
        }
    });
}
