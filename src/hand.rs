//! A hand: one throw-away Claude Code session, sandboxed into one project.
//!
//! It can write its project folder and call the tools it was granted, and
//! that is all. Clean-up rounds resume the same session in the same sandbox.
//! Discarding the hand deletes its profile and everything else it had, except
//! what it did to the project.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

use crate::broker::{self, Endpoint, Tool};
use crate::claude;
use crate::clock;
use crate::logs;
use crate::paths;
use crate::sandbox::{self, Sandbox};

pub struct Hand {
    id: String,
    project: PathBuf,
    directory: PathBuf,
    session: Option<String>,
    granted: Option<Endpoint>,
    asked: Vec<String>,
    rejections: u32,
}

impl Hand {
    /// `grant` is every tool this hand may call through the broker. An empty
    /// grant means no broker at all: the hand can touch its project folder
    /// and talk to Claude, nothing more.
    pub fn start(project: &Path, grant: Vec<Box<dyn Tool>>) -> Result<Hand> {
        let project = project
            .canonicalize()
            .with_context(|| format!("{} does not exist", project.display()))?;
        if !project.is_dir() {
            bail!("{} is not a folder", project.display());
        }

        let id = format!("h{:x}", clock::nanos());
        let directory = paths::sessions_dir().join(&id);
        claude::write_hand_profile(&directory.join("profile"))?;

        let granted_names: Vec<String> = grant.iter().map(|it| it.name().to_string()).collect();
        let granted = if grant.is_empty() {
            None
        } else {
            Some(Endpoint::open(&paths::socket(&format!("hand-{id}")), grant)?)
        };

        logs::event("hand.started", json!({ "hand": id, "project": project, "grant": granted_names }));
        Ok(Hand {
            id,
            project,
            directory,
            session: None,
            granted,
            asked: Vec::new(),
            rejections: 0,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn project(&self) -> &Path {
        &self.project
    }

    /// Everything this hand has been asked so far: the brief, then each
    /// round of notes it was sent back with.
    pub fn asked(&self) -> String {
        self.asked.join("\n\nSent back with these notes:\n")
    }

    pub fn count_rejection(&mut self) -> u32 {
        self.rejections += 1;
        self.rejections
    }

    pub fn work(&mut self, brief: &str, proxy_socket: &Path) -> Result<String> {
        self.asked.push(brief.to_string());
        let sandbox = Sandbox {
            project: self.project.clone(),
            profile: self.directory.join("profile"),
            proxy_socket: proxy_socket.to_path_buf(),
            broker_socket: self.granted.as_ref().map(|it| it.socket().to_path_buf()),
            writable: true,
        };

        let mut command = sandbox.claude(&claude::binary()?);
        command.args(["-p", brief, "--dangerously-skip-permissions", "--strict-mcp-config"]);
        if self.granted.is_some() {
            command.args(["--mcp-config", &broker::mcp_config(Path::new(sandbox::BROKER_INSIDE))]);
        }
        if let Some(session) = &self.session {
            command.args(["--resume", session]);
        }

        let reply = claude::reply_of(&mut command)?;
        logs::event("hand.reported", json!({ "hand": self.id, "session": reply.session_id }));
        self.session = Some(reply.session_id);
        Ok(reply.result)
    }

    pub fn discard(self) {
        let _ = fs::remove_dir_all(&self.directory);
        logs::event("hand.discarded", json!({ "hand": self.id }));
    }
}
