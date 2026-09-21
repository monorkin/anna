//! Anna as a systemd user service, for the people who want her to come back
//! after a reboot without being asked.
//!
//! It is a user service, not a system one: she runs as the person she works
//! for, with their Claude login and their keyring. The unit carries the PATH
//! setup was run with, because a user service starts with next to none and
//! Anna needs claude, bwrap, socat and whatever the MCP servers are.
//! `KillMode=control-group` is systemd's version of what `anna stop` does
//! herself: nothing she started outlives her.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fsutil;

const NAME: &str = "anna.service";

pub fn installed() -> bool {
    unit_path().is_some_and(|it| it.exists())
}

pub fn install(agent: &str, start_at_boot: bool) -> Result<()> {
    let path = unit_path().context("could not determine the home directory")?;
    let executable = std::env::current_exe().context("could not determine Anna's own path")?;
    let search_path = std::env::var("PATH").unwrap_or_default();

    fsutil::write_private(&path, &unit(agent, &executable, &search_path))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", NAME])?;
    if start_at_boot {
        linger()?;
    }
    Ok(())
}

pub fn start() -> Result<()> {
    systemctl(&["start", NAME])
}

pub fn stop() -> Result<()> {
    systemctl(&["stop", NAME])
}

pub fn active() -> bool {
    Command::new("systemctl")
        .args(["--user", "--quiet", "is-active", NAME])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|it| it.success())
}

fn unit(agent: &str, executable: &Path, search_path: &str) -> String {
    let description: String = agent.chars().filter(|it| !it.is_control()).collect();
    format!(
        "[Unit]\n\
         Description={description}\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={} run\n\
         Environment=PATH={search_path}\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         KillMode=control-group\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        executable.display()
    )
}

fn unit_path() -> Option<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|it| !it.is_empty()) {
        Some(directory) => PathBuf::from(directory),
        None => dirs::home_dir()?.join(".config"),
    };
    Some(config.join("systemd/user").join(NAME))
}

/// Without lingering, a user's services only run while they are logged in.
fn linger() -> Result<()> {
    let user = std::env::var("USER").context("USER is not set")?;
    let status = Command::new("loginctl").args(["enable-linger", &user]).status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("could not enable lingering; run `loginctl enable-linger {user}` yourself")
    }
}

fn systemctl(arguments: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .status()
        .context("could not run systemctl")?;
    if status.success() {
        Ok(())
    } else {
        bail!("systemctl --user {} failed", arguments.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_runs_anna_with_the_path_she_was_set_up_with() {
        let unit = unit("Botten\nExecStartPre=/bin/evil", Path::new("/opt/anna/bin/anna"), "/opt/tools/bin:/usr/bin");

        assert!(unit.contains("Description=BottenExecStartPre=/bin/evil\n"));
        assert!(unit.contains("ExecStart=/opt/anna/bin/anna run\n"));
        assert!(unit.contains("Environment=PATH=/opt/tools/bin:/usr/bin\n"));
        assert!(unit.contains("KillMode=control-group\n"));
        assert!(unit.contains("WantedBy=default.target\n"));
    }
}
