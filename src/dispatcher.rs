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

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread as os_thread;
use std::time::Duration;

use crate::claude;
use crate::clock;
use crate::config::Source;
use crate::control::{self, Controls};
use crate::conversation::Conversation;
use crate::judge::{MANIPULATION_QUESTION, SUSPICIOUS};
use crate::logs;
use crate::paths;
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

    let turns = Arc::new(Turns::default());
    let mut pokes = HashMap::new();
    let mut listeners = Vec::new();
    for (name, source) in runtime.config.sources.clone() {
        let (poke, poked) = mpsc::channel();
        pokes.insert(name.clone(), poke);

        let runtime = runtime.clone();
        let turns = turns.clone();
        listeners.push(os_thread::spawn(move || listen(&runtime, &turns, &name, &source, &poked)));
    }

    control::serve(Arc::new(Running {
        since: clock::timestamp(),
        pokes: Mutex::new(pokes),
        turns: turns.clone(),
    }))?;
    stop_on_signals();
    if runtime.config.rotate_accounts {
        rotate_accounts();
    }

    logs::event("anna.listening", json!({ "sources": runtime.config.sources.keys().collect::<Vec<_>>() }));
    for listener in listeners {
        let _ = listener.join();
    }
    Ok(())
}

/// What the control socket can ask of a running Anna.
struct Running {
    since: String,
    pokes: Mutex<HashMap<String, Sender<()>>>,
    turns: Arc<Turns>,
}

impl Controls for Running {
    fn status(&self) -> Value {
        json!({
            "since": self.since,
            "process": std::process::id(),
            "sources": self.pokes.lock().unwrap().keys().collect::<Vec<_>>(),
            "conversations": self.turns.by_conversation.lock().unwrap().len(),
        })
    }

    fn poke(&self, source: Option<&str>) -> Result<String> {
        let pokes = self.pokes.lock().unwrap();
        match source {
            Some(name) => {
                let poke = pokes.get(name).with_context(|| format!("there is no source called {name}"))?;
                let _ = poke.send(());
                Ok(format!("Checking {name}."))
            }
            None => {
                for poke in pokes.values() {
                    let _ = poke.send(());
                }
                Ok("Checking every source.".to_string())
            }
        }
    }

    fn stop(&self) {
        stop();
    }
}

/// Stopping takes everything Anna started with her: threads, hands,
/// reviewers, and whatever those started. SIGINT and SIGTERM end the same
/// way, so ctrl+c, `anna stop` and `systemctl stop` all leave nothing behind.
fn stop() -> ! {
    logs::event("anna.stopping", json!({}));
    claude::stop_everything_started_by(std::process::id() as libc::pid_t);
    control::forget_socket();
    paths::sweep_sockets(true);
    std::process::exit(0);
}

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn request_stop(_signal: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
}

/// A signal handler may do almost nothing, so it only raises a flag; a
/// thread notices it and does the stopping.
fn stop_on_signals() {
    unsafe {
        libc::signal(libc::SIGINT, request_stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, request_stop as *const () as libc::sighandler_t);
    }
    os_thread::spawn(|| {
        loop {
            if STOP_REQUESTED.load(Ordering::Relaxed) {
                stop();
            }
            os_thread::sleep(Duration::from_millis(200));
        }
    });
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

/// Checks the source whenever it is poked, and on a timer for everything
/// nobody pokes her about.
fn listen(runtime: &Arc<Runtime>, turns: &Arc<Turns>, name: &str, source: &Source, poked: &Receiver<()>) {
    let mut seen = Seen::load(name);
    loop {
        if let Err(error) = check(runtime, turns, name, source, &mut seen) {
            logs::event("source.failed", json!({ "source": name, "error": format!("{error:#}") }));
        }
        let _ = poked.recv_timeout(Duration::from_secs(source.every_seconds));
        while poked.try_recv().is_ok() {}
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
