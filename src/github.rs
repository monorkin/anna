//! Her own GitHub login, kept apart from the person's. `gh` and git both
//! take their config from the environment, so a turn of hers runs them with
//! her folder and her gitconfig, and never reads the person's — what she
//! pushes and opens is hers by default, and using the person's login is the
//! exception they make by saying so.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths;

pub fn config_dir() -> PathBuf {
    paths::tools_config_home().join("gh")
}

pub fn gitconfig() -> PathBuf {
    paths::tools_config_home().join("gitconfig")
}

/// She has a login of her own once her gitconfig is written: that is what
/// makes git and gh in her turns hers.
pub fn is_set_up() -> bool {
    gitconfig().is_file()
}

/// `gh auth login` in her own folder, at this terminal: the one time a
/// person is needed. Answers with the login gh wrote down.
pub fn log_in() -> Result<String> {
    let home = config_dir();
    paths::make_private_dir(&home)?;
    let status = Command::new("gh")
        .args(["auth", "login", "--hostname", "github.com", "--git-protocol", "https", "--web"])
        .env("GH_CONFIG_DIR", &home)
        .status()
        .context("could not run gh")?;
    if !status.success() {
        bail!("gh didn't finish logging in");
    }
    login().context("gh finished, but wrote no login down")
}

/// Who she is on GitHub, from what gh keeps in her folder.
pub fn login() -> Option<String> {
    login_in(&fs::read_to_string(config_dir().join("hosts.yml")).ok()?)
}

fn login_in(hosts: &str) -> Option<String> {
    hosts
        .lines()
        .find_map(|line| line.trim().strip_prefix("user:"))
        .map(|it| it.trim().to_string())
        .filter(|it| !it.is_empty())
}

/// What her commits carry, and how git finds her login when it pushes.
pub fn set_identity(name: &str, email: &str) -> Result<()> {
    paths::make_private_dir(&paths::tools_config_home())?;
    write_identity(&gitconfig(), name, email)
}

fn write_identity(path: &Path, name: &str, email: &str) -> Result<()> {
    let written = format!(
        "[user]\n\tname = {name}\n\temail = {email}\n[credential \"https://github.com\"]\n\thelper = \n\thelper = !gh auth git-credential\n"
    );
    fs::write(path, written).with_context(|| format!("could not write {}", path.display()))
}

pub fn email() -> Option<String> {
    email_in(&fs::read_to_string(gitconfig()).ok()?)
}

fn email_in(gitconfig: &str) -> Option<String> {
    gitconfig
        .lines()
        .find_map(|line| line.trim().strip_prefix("email ="))
        .map(|it| it.trim().to_string())
        .filter(|it| !it.is_empty())
}

/// What a turn's git and gh are told, once she has a login of her own.
/// Nothing until then: a turn without these runs them as the person, which
/// is what the exception in her instructions is about.
pub fn environment() -> Vec<(&'static str, PathBuf)> {
    environment_in(&paths::tools_config_home())
}

fn environment_in(tools: &Path) -> Vec<(&'static str, PathBuf)> {
    if tools.join("gitconfig").is_file() {
        vec![("GH_CONFIG_DIR", tools.join("gh")), ("GIT_CONFIG_GLOBAL", tools.join("gitconfig"))]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_and_gh_are_hers_once_her_gitconfig_is_written() {
        let tools = std::env::temp_dir().join(format!("anna-github-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tools);
        fs::create_dir_all(&tools).unwrap();
        assert!(environment_in(&tools).is_empty(), "without a login of her own, a turn runs them as the person");

        write_identity(&tools.join("gitconfig"), "Botten", "botten@example.com").unwrap();
        let written = fs::read_to_string(tools.join("gitconfig")).unwrap();
        assert!(written.contains("name = Botten") && written.contains("email = botten@example.com"));
        assert!(written.contains("helper = !gh auth git-credential"), "git pushes through her gh login");
        assert_eq!(email_in(&written), Some("botten@example.com".to_string()));

        let environment = environment_in(&tools);
        assert_eq!(environment[0], ("GH_CONFIG_DIR", tools.join("gh")));
        assert_eq!(environment[1], ("GIT_CONFIG_GLOBAL", tools.join("gitconfig")));
        fs::remove_dir_all(tools).unwrap();
    }

    #[test]
    fn her_login_is_read_from_what_gh_wrote() {
        let hosts = "github.com:\n    users:\n        botten-agent:\n            oauth_token: gho_x\n    git_protocol: https\n    user: botten-agent\n";
        assert_eq!(login_in(hosts), Some("botten-agent".to_string()));
        assert_eq!(login_in("github.com:\n    git_protocol: https\n"), None);
    }
}
