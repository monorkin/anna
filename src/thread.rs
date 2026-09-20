//! A thread: the Claude Code session that owns one conversation.
//!
//! Waking a thread runs one headless turn of that session — resumed, so it
//! remembers the conversation — with the broker as its only MCP server. The
//! thread is not sandboxed; its hands are. It speaks through the reply tool,
//! and if a turn ends without it having said anything, its closing words are
//! said for it rather than lost.

use anyhow::{Context, Result};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::broker::{self, Endpoint, Tool};
use crate::claude;
use crate::config::{self, Config};
use crate::conversation::Conversation;
use crate::editor::Editor;
use crate::judge::Judge;
use crate::logs;
use crate::mcp::Catalog;
use crate::paths;
use crate::proxy::{self, Proxy};
use crate::thread_tools::{Dismiss, Hands, Reply, SendBack, StartHand, Workshop};

const TOOL_TIMEOUT_MILLISECONDS: &str = "7200000";

const ROLE: &str = "You are Anna, working as a colleague rather than a tool. \
Someone is talking to you in a conversation; the reply tool is the only way they hear from you, so use it for every answer, question, and update. \
You plan and check; hands do the work inside projects. Give a hand one project folder and a brief that carries everything it needs, because it knows nothing you know. \
A reviewer checks every hand's work against your brief and tells you what was actually done; send the hand back with the reviewer's notes when the work isn't right, and dismiss it when you're done with it. \
You are not sandboxed and hands are, so never run code, scripts, tests or build tools from a folder a hand has worked in — have a hand do it. Reading files there is fine. \
When something can't be done, say what you tried and what you can do instead.";

pub fn wake(conversation: Arc<dyn Conversation>, message: &str) -> Result<()> {
    let key = conversation.key().to_string();
    let directory = paths::thread_dir(&key);
    fs::create_dir_all(&directory)?;

    let config = Config::load()?;
    let judge = Arc::new(Judge::from(&config));
    let editor = Arc::new(Editor::new(config::style(), judge.clone()));
    let catalog = Arc::new(Catalog::open(&config, editor.clone(), judge.clone()));
    let proxy = Proxy::start(&paths::socket(&format!("proxy-{key}")), &[proxy::CLAUDE_API])?;
    let hands = Arc::new(Hands::default());
    let workshop = Arc::new(Workshop {
        hands: hands.clone(),
        judge,
        proxy_socket: proxy.socket().to_path_buf(),
    });
    let spoke = Arc::new(AtomicBool::new(false));

    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(Reply { conversation: conversation.clone(), editor: editor.clone(), spoke: spoke.clone() }),
        Box::new(StartHand { workshop: workshop.clone(), catalog: catalog.clone() }),
        Box::new(SendBack { workshop }),
        Box::new(Dismiss { hands: hands.clone() }),
    ];
    tools.extend(catalog.all());
    let endpoint = Endpoint::open(&paths::socket(&format!("thread-{key}")), tools)?;

    logs::event("thread.woken", json!({ "conversation": key, "tools": catalog.names() }));
    let outcome = claude::reply_of(&mut turn(&directory, &endpoint, message)?);
    hands.discard_all();

    let reply = outcome.with_context(|| format!("the thread for {key} could not finish its turn"))?;
    fs::write(directory.join("session"), &reply.session_id)?;
    if !spoke.load(Ordering::Relaxed) {
        conversation.say(&editor.polish(&reply.result)?)?;
    }
    logs::event("thread.slept", json!({ "conversation": key, "session": reply.session_id }));
    Ok(())
}

fn turn(directory: &Path, endpoint: &Endpoint, message: &str) -> Result<Command> {
    let mut command = Command::new(claude::binary()?);
    command
        .current_dir(directory)
        .env("MCP_TOOL_TIMEOUT", TOOL_TIMEOUT_MILLISECONDS)
        .args(["-p", message, "--dangerously-skip-permissions", "--strict-mcp-config"])
        .args(["--mcp-config", &broker::mcp_config(endpoint.socket())])
        .args(["--append-system-prompt", &role()]);
    if let Ok(session) = fs::read_to_string(directory.join("session")) {
        command.args(["--resume", session.trim()]);
    }
    Ok(command)
}

fn role() -> String {
    match config::personality() {
        Some(personality) => format!("{ROLE}\n\n{personality}"),
        None => ROLE.to_string(),
    }
}
