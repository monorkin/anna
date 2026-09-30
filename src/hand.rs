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
use std::process::Command;

use crate::broker::{self, Endpoint, Tool};
use crate::claude::{self, Reply, Started};
use crate::clock;
use crate::logs;
use crate::paths;
use crate::proxy::{self, Bridge, Proxy};
use crate::sandbox::{self, Outside, Sandbox, Service};
use crate::transcripts;

pub const CARRY_ON: &str = "Your run was cut off part way — the login it ran on was refreshed underneath it — and you have just been started again in the same session. Nothing you did is lost. Look at where things stand and carry on from there; when you are done, report as you would have.";
const ON_ANOTHER_MODEL: &str = "Your run was cut off part way — the Claude allowance for the model it ran on is used up — and you have just been started again in the same session on another model. Nothing you did is lost. The limit message in your history is about that, not about anything in the project. Look at where things stand and carry on with the brief from there; when you are done, report as you would have.";

pub struct Hand {
    id: String,
    project: PathBuf,
    directory: PathBuf,
    build_dir: PathBuf,
    /// Its Claude session, named before the first run so a run that dies
    /// with the work half done can be picked up rather than started over.
    session: String,
    begun: bool,
    granted: Option<Endpoint>,
    services: Vec<Service>,
    /// Gone with the hand, and the sockets its services are reached through
    /// with them.
    _bridges: Vec<Bridge>,
    /// A proxy of its own when it may fetch from the package registries;
    /// otherwise it shares the one that reaches Claude and nothing else.
    registries: Option<Proxy>,
    asked: Vec<String>,
    rejections: u32,
}

impl Hand {
    /// `grant` is every tool this hand may call through the broker. An empty
    /// grant means no broker at all: the hand can touch its project folder
    /// and talk to Claude, nothing more. `ports` are services on this
    /// machine's loopback the hand may reach — a database for its tests —
    /// and only loopback: anything further is the network, which a hand
    /// doesn't have, except the package registries when `registries` says
    /// so.
    pub fn start(project: &Path, grant: Vec<Box<dyn Tool>>, ports: &[u16], registries: bool) -> Result<Hand> {
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
        sandbox::make_scratch(&directory)?;
        let build_dir = sandbox::build_dir_of(&project)?;

        let granted_names: Vec<String> = grant.iter().map(|it| it.name().to_string()).collect();
        let granted = if grant.is_empty() {
            None
        } else {
            Some(Endpoint::open(&paths::socket(&format!("hand-{id}")), grant)?)
        };
        let (services, bridges) = bridge(&id, ports)?;
        let registries = if registries {
            let allowed = [&[proxy::CLAUDE_API], proxy::REGISTRIES.as_slice()].concat();
            Some(Proxy::start(&paths::socket(&format!("proxy-{id}")), &allowed)?)
        } else {
            None
        };

        logs::event("hand.started", json!({ "hand": id, "project": project, "grant": granted_names, "services": ports, "registries": registries.is_some() }));
        Ok(Hand {
            id,
            project,
            directory,
            build_dir,
            session: claude::random_session_id()?,
            begun: false,
            granted,
            services,
            _bridges: bridges,
            registries,
            asked: Vec::new(),
            rejections: 0,
        })
    }

    pub fn services(&self) -> &[Service] {
        &self.services
    }

    pub fn build_dir(&self) -> &Path {
        &self.build_dir
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
        if self.begun {
            claude::refresh_hand_login(&self.directory.join("profile"))?;
        }

        let reply = claude::on_each_model(&outside.models.hands, |model, after_another| {
            if after_another {
                // The model before it ran out part way through, in this same
                // session
                self.begun = true;
                self.run(model, ON_ANOTHER_MODEL, outside, started)
            } else {
                self.run(model, brief, outside, started)
            }
        });
        // Where there was no repository the sandbox covers .git, which leaves
        // an empty folder behind; only an empty one can be removed this way
        let _ = fs::remove_dir(self.project.join(".git"));
        let reply = reply?;
        logs::event("hand.reported", json!({ "hand": self.id, "session": reply.session_id }));
        self.begun = true;
        Ok(reply.result)
    }

    fn run(&mut self, model: &str, told: &str, outside: &Outside, started: &Started) -> Result<Reply> {
        let (id, profile) = (self.id.clone(), self.directory.join("profile"));
        // Her login was refreshed while the hand ran and the token it held
        // was revoked with that, or the token simply expired. The session is
        // whole: it goes on with her current token and what it had done.
        let refresh = || {
            logs::event("hand.login_refreshed", json!({ "hand": id }));
            claude::refresh_hand_login(&profile)
        };
        claude::again_if_the_login_went_stale(refresh, |again| {
            if again {
                self.begun = true;
                claude::reply_of(&mut self.command(model, outside)?, CARRY_ON, outside.time_limit, Some(started))
            } else {
                claude::reply_of(&mut self.command(model, outside)?, told, outside.time_limit, Some(started))
            }
        })
    }

    fn command(&self, model: &str, outside: &Outside) -> Result<Command> {
        let sandbox = Sandbox {
            project: self.project.clone(),
            profile: self.directory.join("profile"),
            outside,
            broker_socket: self.granted.as_ref().map(|it| it.socket().to_path_buf()),
            writable: true,
            build_dir: self.build_dir.clone(),
            services: self.services.clone(),
            registries: self.registries.as_ref().map(|it| it.socket().to_path_buf()),
            scratch: self.directory.clone(),
        };

        let mut command = sandbox.claude(&claude::binary()?);
        command.args(["--dangerously-skip-permissions", "--strict-mcp-config", "--tools", sandbox.tools()]);
        command.args(["--model", model]);
        command.args(["--append-system-prompt", &what_it_can_reach(&self.services, self.registries.is_some())]);
        if self.granted.is_some() {
            command.args(["--mcp-config", &broker::mcp_config(Path::new(sandbox::BROKER_INSIDE))]);
        }
        if self.begun {
            command.args(["--resume", &self.session]);
        } else {
            command.args(["--session-id", &self.session]);
        }
        Ok(command)
    }

    pub fn discard(self) {
        transcripts::keep(&self.directory.join("profile"), &self.id, false);
        let _ = fs::remove_dir_all(&self.directory);
        logs::event("hand.discarded", json!({ "hand": self.id }));
    }
}

/// One bridge per port, listening on a socket of the hand's and connecting
/// to the port on loopback. They go with the hand.
fn bridge(hand: &str, ports: &[u16]) -> Result<(Vec<Service>, Vec<Bridge>)> {
    let mut services = Vec::new();
    let mut bridges = Vec::new();
    for (n, port) in ports.iter().enumerate() {
        let socket = paths::socket(&format!("service-{hand}-{n}"));
        bridges.push(Bridge::start(&socket, *port).with_context(|| format!("could not bridge port {port} into the sandbox"))?);
        services.push(Service { port: *port, socket });
    }
    Ok((services, bridges))
}

/// What a hand can reach, said before it starts. Without this, a database
/// its thread didn't bridge in looks to it like a database that is down, and
/// it reports the machine as broken instead of saying what it wasn't given.
pub fn what_it_can_reach(services: &[Service], registries: bool) -> String {
    let network = if registries {
        "no network beyond the package registries — rubygems, npm, crates.io, PyPI, mise's version list and nodejs.org — so fetching the project's dependencies works and nothing else out there does."
    } else {
        "no network at all."
    };
    let reachable = match services {
        [] => "Nothing of this machine's own is reachable from in here.".to_string(),
        services => format!(
            "On your own loopback you can reach {}, bridged in from this machine.",
            services.iter().map(|it| format!("port {}", it.port)).collect::<Vec<_>>().join(" and ")
        ),
    };

    format!(
        "You work in a sandbox: this project folder, the languages installed here, and {network} {reachable} \
         Anything else you can't reach was never given to you rather than being broken or down — don't try to start it, install it or work around it, and say in your report what you couldn't reach and what it cost."
    )
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
    fn a_hand_is_told_what_it_can_reach_so_it_doesnt_call_it_broken() {
        let granted = [
            Service { port: 33380, socket: PathBuf::from("/run/anna/service-h1-0.sock") },
            Service { port: 6379, socket: PathBuf::from("/run/anna/service-h1-1.sock") },
        ];

        let told = what_it_can_reach(&granted, false);
        assert!(told.contains("you can reach port 33380 and port 6379"), "{told}");
        assert!(told.contains("no network at all") && told.contains("was never given to you rather than being broken"));
        let alone = what_it_can_reach(&[], true);
        assert!(alone.contains("Nothing of this machine's own is reachable") && alone.contains("no network beyond the package registries"));
    }

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
