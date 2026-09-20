//! A thread: the Claude Code session that owns one conversation.
//!
//! Waking a thread runs one headless turn of that session — resumed, so it
//! remembers the conversation — with the broker as its only MCP server. The
//! thread is not sandboxed; its hands are. It speaks through the reply tool,
//! and if a turn ends without it having said anything, its closing words are
//! said for it rather than lost.

use anyhow::Result;
use serde_json::json;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::broker::{self, Endpoint, Tool};
use crate::claude::{self, OutOfTime};
use crate::config;
use crate::conversation::Conversation;
use crate::logs;
use crate::paths;
use crate::runtime::Runtime;
use crate::thread_tools::{Dismiss, Hands, Reply, SendBack, StartHand, Workshop};

const TOOL_TIMEOUT_MILLISECONDS: &str = "7200000";

const ROLE: &str = "You are Anna, working as a colleague rather than a tool. \
Someone is talking to you in a conversation; the reply tool is the only way they hear from you, so use it for every answer, question, and update. \
You plan and check; hands do the work inside projects. Give a hand one project folder and a brief that carries everything it needs, because it knows nothing you know. \
A reviewer checks every hand's work against your brief and tells you what was actually done; send the hand back with the reviewer's notes when the work isn't right, and dismiss it when you're done with it. \
You are not sandboxed and hands are, so never run code, scripts, tests or build tools from a folder a hand has worked in — have a hand do it. Reading files there is fine. \
When something can't be done, say what you tried and what you can do instead.";

pub fn wake(runtime: &Runtime, conversation: Arc<dyn Conversation>, message: &str) -> Result<()> {
    let key = conversation.key().to_string();
    let directory = paths::thread_dir(&key);
    fs::create_dir_all(&directory)?;

    let hands = Arc::new(Hands::default());
    let workshop = Arc::new(Workshop {
        hands: hands.clone(),
        judge: runtime.judge.clone(),
        outside: runtime.outside.clone(),
    });
    let spoke = Arc::new(AtomicBool::new(false));

    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(Reply { conversation: conversation.clone(), editor: runtime.editor.clone(), spoke: spoke.clone() }),
        Box::new(StartHand { workshop: workshop.clone(), catalog: runtime.catalog.clone() }),
        Box::new(SendBack { workshop }),
        Box::new(Dismiss { hands: hands.clone() }),
    ];
    tools.extend(runtime.catalog.all());
    let endpoint = Endpoint::open(&paths::socket(&format!("thread-{key}")), tools)?;

    logs::event("thread.woken", json!({ "conversation": key }));
    let session = Session::of(&directory)?;
    let outcome = claude::reply_of(&mut turn(&directory, &endpoint, &session, message)?, runtime.outside.time_limit);
    hands.discard_all();

    match outcome {
        Ok(reply) => {
            session.keep()?;
            if !spoke.load(Ordering::Relaxed) {
                conversation.say(&runtime.editor.polish(&reply.result)?)?;
            }
            logs::event("thread.slept", json!({ "conversation": key, "session": reply.session_id }));
            Ok(())
        }
        Err(error) => match error.downcast_ref::<OutOfTime>() {
            Some(out_of_time) => {
                session.keep()?;
                logs::event("thread.out_of_time", json!({ "conversation": key, "minutes": out_of_time.minutes }));
                conversation.say(&format!(
                    "I ran out of time on this. One go at something gets {} min, and that's used up. Tell me to keep going and I'll pick up where I stopped.",
                    out_of_time.minutes
                ))
            }
            None => Err(error.context(format!("the thread for {key} could not finish its turn"))),
        },
    }
}

/// The Claude session behind a thread. Its id is chosen before the first
/// turn, not learned from it, so a turn that was cut short can still be
/// picked up again.
struct Session {
    id: String,
    file: PathBuf,
    begun: bool,
}

impl Session {
    fn of(directory: &Path) -> Result<Session> {
        let file = directory.join("session");
        match fs::read_to_string(&file) {
            Ok(id) => Ok(Session { id: id.trim().to_string(), file, begun: true }),
            Err(_) => Ok(Session { id: random_uuid()?, file, begun: false }),
        }
    }

    fn keep(&self) -> Result<()> {
        fs::write(&self.file, &self.id)?;
        Ok(())
    }
}

fn random_uuid() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex: String = bytes.iter().map(|it| format!("{it:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]))
}

fn turn(directory: &Path, endpoint: &Endpoint, session: &Session, message: &str) -> Result<Command> {
    let mut command = Command::new(claude::binary()?);
    command
        .current_dir(directory)
        .env("MCP_TOOL_TIMEOUT", TOOL_TIMEOUT_MILLISECONDS)
        .args(["-p", message, "--dangerously-skip-permissions", "--strict-mcp-config"])
        .args(["--mcp-config", &broker::mcp_config(endpoint.socket())])
        .args(["--append-system-prompt", &role()]);
    if session.begun {
        command.args(["--resume", &session.id]);
    } else {
        command.args(["--session-id", &session.id]);
    }

    // A thread must not outlive Anna: stopping her stops everything
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    Ok(command)
}

fn role() -> String {
    match config::personality() {
        Some(personality) => format!("{ROLE}\n\n{personality}"),
        None => ROLE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_keeps_its_id_once_it_has_begun() {
        let directory = std::env::temp_dir().join(format!("anna-session-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();

        let first = Session::of(&directory).unwrap();
        assert!(!first.begun);
        assert_eq!(first.id.len(), 36);
        assert_eq!(&first.id[14..15], "4");
        assert_ne!(first.id, Session::of(&directory).unwrap().id);

        first.keep().unwrap();
        let again = Session::of(&directory).unwrap();
        assert!(again.begun);
        assert_eq!(again.id, first.id);
        fs::remove_dir_all(directory).unwrap();
    }
}
