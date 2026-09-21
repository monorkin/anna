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
use crate::claude::{self, Started};
use crate::clock;
use crate::logs;
use crate::paths;
use crate::sandbox::{self, Outside, Sandbox};

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
        refuse_what_is_not_a_project(&project, &paths::home(), &[paths::config_dir(), paths::data_dir(), paths::runtime_dir()])?;

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

    pub fn work(&mut self, brief: &str, outside: &Outside, started: &Started) -> Result<String> {
        self.asked.push(brief.to_string());
        let sandbox = Sandbox {
            project: self.project.clone(),
            profile: self.directory.join("profile"),
            outside,
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

        let reply = claude::reply_of(&mut command, outside.time_limit, Some(started));
        // Where there was no repository the sandbox covers .git, which leaves
        // an empty folder behind; only an empty one can be removed this way
        let _ = fs::remove_dir(self.project.join(".git"));
        let reply = reply?;
        logs::event("hand.reported", json!({ "hand": self.id, "session": reply.session_id }));
        self.session = Some(reply.session_id);
        Ok(reply.result)
    }

    pub fn discard(self) {
        let _ = fs::remove_dir_all(&self.directory);
        logs::event("hand.discarded", json!({ "hand": self.id }));
    }
}

/// A hand writes its project, and the brain acts on what it finds there, so
/// a project is never a folder that holds what Anna or the person runs on:
/// her own state, the home folder itself or anything above it, or a
/// dot-folder in it — keys, configs, other tools' credentials. Whoever asks
/// for the hand doesn't matter; this is what keeps someone who can only hand
/// out work from writing her schedules or her config through one.
fn refuse_what_is_not_a_project(project: &Path, home: &Path, her_own: &[PathBuf]) -> Result<()> {
    let settled: Vec<PathBuf> = her_own.iter().map(|it| it.canonicalize().unwrap_or_else(|_| it.clone())).collect();
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());

    let touches_her_own = settled.iter().any(|it| project.starts_with(it) || it.starts_with(project));
    let holds_home = home.starts_with(project);
    let hidden_in_home = project
        .strip_prefix(&home)
        .ok()
        .and_then(|inside| inside.components().next())
        .is_some_and(|first| first.as_os_str().to_string_lossy().starts_with('.'));

    if touches_her_own || holds_home || hidden_in_home {
        bail!("{} is not a project folder: hands don't work in the home folder, in a dot-folder in it, or anywhere my own files are", project.display())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_is_kept_out_of_everything_that_is_not_a_project() {
        let home = Path::new("/home/someone");
        let her_own = [PathBuf::from("/srv/anna/config"), PathBuf::from("/srv/anna/data")];
        let refused = |project: &str| refuse_what_is_not_a_project(Path::new(project), home, &her_own).is_err();

        assert!(!refused("/home/someone/Work/frontdesk"));
        assert!(!refused("/home/someone/Work/frontdesk/.claude/worktrees/fix"), "only a dot-folder right in home is off limits");
        assert!(!refused("/srv/projects/frontdesk"));

        assert!(refused("/srv/anna/data"), "her database lives here");
        assert!(refused("/srv/anna/data/threads/t1"));
        assert!(refused("/srv/anna"), "a folder that holds her state is as good as her state");
        assert!(refused("/home/someone"));
        assert!(refused("/home"));
        assert!(refused("/"));
        assert!(refused("/home/someone/.ssh"));
        assert!(refused("/home/someone/.config/basecamp"));
    }
}
