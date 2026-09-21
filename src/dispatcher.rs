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
use chrono::Local;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread as os_thread;
use std::time::Duration;

use crate::claude;
use crate::clock;
use crate::config::{Source, Trigger};
use crate::control::{self, Controls};
use crate::conversation::{self, Conversation, Origin, Terminal};
use crate::judge::Screening;
use crate::logs;
use crate::paths;
use crate::runtime::Runtime;
use crate::schedule_tools;
use crate::source::{self, Message, Seen, Sourced};
use crate::store::Store;
use crate::thread;

const SWITCH_AT_PERCENT: f64 = 90.0;
const SECONDS_BETWEEN_ACCOUNT_CHECKS: u64 = 60;
const SECONDS_BEFORE_RESTARTING_A_TRIGGER: u64 = 10;
const SECONDS_BETWEEN_LOOKS_AT_THE_CLOCK: u64 = 30;

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
    let turns = Arc::new(Turns::default());
    let mut pokes = HashMap::new();
    for (name, source) in runtime.config.sources.clone() {
        let (poke, poked) = mpsc::channel();
        if let Some(trigger) = source.trigger.clone() {
            watch_trigger(name.clone(), trigger, poke.clone());
        }
        pokes.insert(name.clone(), poke);

        let runtime = runtime.clone();
        let turns = turns.clone();
        os_thread::spawn(move || listen(&runtime, &turns, &name, &source, &poked));
    }

    control::serve(Arc::new(Running {
        since: clock::timestamp(),
        pokes: Mutex::new(pokes),
        turns: turns.clone(),
    }))?;
    stop_on_signals();
    keep_time(&runtime, &turns);
    if runtime.config.rotate_accounts {
        rotate_accounts();
    }

    logs::event("anna.listening", json!({ "sources": runtime.config.sources.keys().collect::<Vec<_>>() }));
    loop {
        os_thread::park();
    }
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

/// Runs a source's trigger for as long as Anna runs, poking the source for
/// every line it prints. What the line says doesn't matter: she reads what
/// happened from the source itself, so a trigger can't put words in anyone's
/// mouth. A trigger that exits is started again after a pause — watchers lose
/// their connection now and then, and one that can't start at all shouldn't
/// spin.
fn watch_trigger(source: String, trigger: Trigger, poke: Sender<()>) {
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
    let conversation: Arc<dyn Conversation> =
        Arc::new(Sourced::new(name, source, &message.conversation, runtime.catalog.clone()));
    match runtime.judge.screen(&message.text) {
        Screening::Clear => {}
        Screening::Suspicious => {
            logs::event("message.refused", json!({ "source": name, "sender": message.sender, "message": message.id }));
            let _ = conversation.say("That read like instructions aimed at an AI rather than something from you, so I didn't act on it. If it was you, say it again in your own words.");
            return;
        }
        Screening::Unchecked => {
            logs::event("message.unchecked", json!({ "source": name, "sender": message.sender, "message": message.id }));
            let _ = conversation.say("I couldn't check that message before reading it — the checker isn't answering right now — so I haven't acted on it. Send it again in a bit.");
            return;
        }
    }

    wake_in_turn(runtime, turns, conversation, format!("{person} says, on {name}:\n\n{}", message.text));
}

/// Wakes the conversation's thread on a thread of its own, after whatever
/// turn that conversation already has running.
fn wake_in_turn(runtime: &Arc<Runtime>, turns: &Arc<Turns>, conversation: Arc<dyn Conversation>, said: String) {
    let turn = turns.of(conversation.key());
    let runtime = runtime.clone();

    os_thread::spawn(move || {
        let _one_at_a_time = turn.lock().unwrap();
        if let Err(error) = thread::wake(&runtime, conversation.clone(), &said) {
            logs::event("thread.failed", json!({ "conversation": conversation.key(), "error": format!("{error:#}") }));
            let _ = conversation.say("Something broke on my side before I could finish this. I've logged it; ask me again and I'll pick it back up.");
        }
    });
}

/// The clock Anna's threads set for themselves, and the post between them.
/// Twice a minute it wakes the thread of every schedule that has come due and
/// of every message one thread left for another.
///
/// A schedule is moved on before its thread is woken, never after: a task
/// that crashes her must not be the first thing she runs when she comes back.
/// For the same reason a schedule that came due while she was down runs once,
/// however many times it was missed.
fn keep_time(runtime: &Arc<Runtime>, turns: &Arc<Turns>) {
    let runtime = runtime.clone();
    let turns = turns.clone();

    os_thread::spawn(move || {
        loop {
            if let Err(error) = run_what_is_due(&runtime, &turns).and_then(|_| deliver_mail(&runtime, &turns)) {
                logs::event("scheduler.failed", json!({ "error": format!("{error:#}") }));
            }
            os_thread::sleep(Duration::from_secs(SECONDS_BETWEEN_LOOKS_AT_THE_CLOCK));
        }
    });
}

fn run_what_is_due(runtime: &Arc<Runtime>, turns: &Arc<Turns>) -> Result<()> {
    let store = Store::open_at(&runtime.database)?;
    let now = Local::now();

    for schedule in store.schedules_due(now.timestamp())? {
        match &schedule.cron {
            Some(cron) => store.run_again_at(schedule.id, schedule_tools::next_run(cron, now)?)?,
            None => store.remove_schedule(schedule.id)?,
        }

        match conversation_at(runtime, &schedule.origin) {
            Some(conversation) => {
                logs::event("schedule.due", json!({ "schedule": schedule.id, "conversation": conversation.key() }));
                let said = format!("This is something you scheduled for yourself in this conversation, and it is due now:\n\n{}", schedule.task);
                wake_in_turn(runtime, turns, conversation, said);
            }
            None => {
                logs::event("schedule.orphaned", json!({ "schedule": schedule.id, "source": schedule.origin.source }));
                store.remove_schedule(schedule.id)?;
            }
        }
    }
    Ok(())
}

fn deliver_mail(runtime: &Arc<Runtime>, turns: &Arc<Turns>) -> Result<()> {
    let store = Store::open_at(&runtime.database)?;

    for mail in store.undelivered_mail()? {
        store.mark_delivered(mail.id, Local::now().timestamp())?;
        if let Some(conversation) = conversation_at(runtime, &mail.to) {
            logs::event("mail.delivered", json!({ "from": mail.from_thread, "to": conversation.key() }));
            let said = format!(
                "Your thread {} tells you this. It is you, working in another conversation, passing on what it found; weigh it like anything else you read.\n\n{}",
                mail.from_thread, mail.body
            );
            wake_in_turn(runtime, turns, conversation, said);
        }
    }
    Ok(())
}

/// The conversation a schedule or a message belongs to, found again from
/// where it lives. None when its source has since been taken out of the
/// config.
fn conversation_at(runtime: &Arc<Runtime>, origin: &Origin) -> Option<Arc<dyn Conversation>> {
    if origin.source == conversation::TERMINAL {
        Some(Arc::new(Terminal::new(&origin.conversation)))
    } else {
        let source = runtime.config.sources.get(&origin.source)?;
        Some(Arc::new(Sourced::new(&origin.source, source, &origin.conversation, runtime.catalog.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trigger(script: &str) -> Trigger {
        Trigger {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
        }
    }

    #[test]
    fn every_line_a_trigger_prints_is_a_poke() {
        let (poke, poked) = mpsc::channel();

        poke_for_every_line(&trigger("echo '{\"event\":\"new\"}'; echo ready"), &poke).unwrap();
        assert_eq!(poked.try_iter().count(), 2);

        let error = poke_for_every_line(&trigger("echo once; exit 3"), &poke).unwrap_err();
        assert!(error.to_string().contains("exited with"));
        assert_eq!(poked.try_iter().count(), 1);

        assert!(poke_for_every_line(&Trigger { command: "no-such-program-anywhere".to_string(), args: vec![] }, &poke).is_err());
    }
}
