//! A hand: one throw-away Claude Code session in a sandbox.
//!
//! The sandbox is bubblewrap with every namespace unshared. The hand sees a
//! read-only /usr, its project folder at /work, its own profile, and the
//! proxy's socket — no home directory, no network. socat turns the socket
//! into the localhost proxy Claude Code is pointed at, so the Claude API is
//! the only thing the hand can reach. Permissions are skipped inside, because
//! the sandbox is the permission system.
//!
//! Clean-up rounds resume the same session in the same sandbox. Discarding
//! the hand deletes its profile and everything else it had, except the
//! project folder it worked in.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::broker::{self, Endpoint, Tool};
use crate::claude;
use crate::clock;
use crate::logs;
use crate::paths;

const INSIDE: &str = r#"
socat TCP-LISTEN:3128,fork,bind=127.0.0.1 UNIX-CONNECT:/run/proxy.sock &
sleep 0.3
exec /opt/claude "$@"
"#;

const BROKER_INSIDE: &str = "/run/broker.sock";

pub struct Hand {
    id: String,
    project: PathBuf,
    directory: PathBuf,
    session: Option<String>,
    granted: Option<Endpoint>,
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
        let directory = paths::hand_dir(&id);
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
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn work(&mut self, brief: &str, proxy_socket: &Path) -> Result<String> {
        let mut command = self.sandbox(proxy_socket, &claude::binary()?);
        command.args(["-p", brief, "--dangerously-skip-permissions", "--strict-mcp-config"]);
        if self.granted.is_some() {
            command.args(["--mcp-config", &broker::mcp_config(Path::new(BROKER_INSIDE))]);
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

    fn sandbox(&self, proxy_socket: &Path, claude_binary: &Path) -> Command {
        let mut command = Command::new("bwrap");
        command
            .args(["--unshare-all", "--die-with-parent"])
            .args(["--ro-bind", "/usr", "/usr"])
            .args(["--symlink", "usr/bin", "/bin"])
            .args(["--symlink", "usr/lib", "/lib"])
            .args(["--symlink", "usr/lib", "/lib64"])
            .args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/home/hand"]);

        for certificates in ["/etc/ssl", "/etc/ca-certificates", "/etc/pki"] {
            if Path::new(certificates).exists() {
                command.args(["--ro-bind", certificates, certificates]);
            }
        }

        command
            .arg("--ro-bind")
            .arg(claude_binary)
            .arg("/opt/claude")
            .arg("--bind")
            .arg(&self.project)
            .arg("/work")
            .arg("--bind")
            .arg(self.directory.join("profile"))
            .arg("/profile")
            .arg("--ro-bind")
            .arg(proxy_socket)
            .arg("/run/proxy.sock");
        if let Some(endpoint) = &self.granted {
            command.arg("--ro-bind").arg(endpoint.socket()).arg(BROKER_INSIDE);
        }

        command
            .args(["--chdir", "/work"])
            .args(["--setenv", "HOME", "/home/hand"])
            .args(["--setenv", "PATH", "/usr/bin"])
            .args(["--setenv", "CLAUDE_CONFIG_DIR", "/profile"])
            .args(["--setenv", "HTTPS_PROXY", "http://127.0.0.1:3128"])
            .args(["/usr/bin/bash", "-c", INSIDE, "hand"]);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sandbox_shares_only_the_project_the_profile_and_the_proxy() {
        let hand = Hand {
            id: "h1".to_string(),
            project: PathBuf::from("/home/someone/project"),
            directory: PathBuf::from("/data/hands/h1"),
            session: None,
            granted: None,
        };

        let command = hand.sandbox(Path::new("/run/anna/proxy.sock"), Path::new("/bin/claude"));
        let arguments: Vec<String> = command
            .get_args()
            .map(|it| it.to_string_lossy().into_owned())
            .collect();
        let writable: Vec<&[String]> = arguments.windows(3).filter(|it| it[0] == "--bind").collect();

        assert!(arguments.contains(&"--unshare-all".to_string()));
        assert_eq!(
            writable,
            [
                ["--bind", "/home/someone/project", "/work"].map(String::from).as_slice(),
                ["--bind", "/data/hands/h1/profile", "/profile"].map(String::from).as_slice(),
            ]
        );
        assert!(!arguments.iter().any(|it| it == "/home" || it == "/etc"));
    }
}
