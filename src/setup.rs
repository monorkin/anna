//! `anna setup`: the one time a person decides things for Anna. After this
//! she runs on her own.
//!
//! A wizard in steps. Every step says what is already in place and keeps it
//! when the answer is empty, so running setup again is how anything gets
//! changed later. The Jev key is typed without echo, checked against the API
//! before it is kept, and goes to the keyring, not the config. The style and
//! `CLAUDE.md` are given as paths and copied next to the config, so the
//! originals can live wherever their author keeps them.
//!
//! Asking and doing are both handed in, which is what lets the whole walk be
//! tested without a terminal, a keyring, or systemd.

use anyhow::{Context, Result};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::fsutil;
use crate::judge::Judge;
use crate::mcp_cli;
use crate::paths;
use crate::secrets::{self, Kept};
use crate::service;

pub fn run() -> Result<()> {
    let stdin = std::io::stdin();
    let mut config = Config::load()?;
    let mut asking = Terminal {
        input: stdin.lock(),
        output: std::io::stdout(),
    };

    Wizard {
        asking: &mut asking,
        doing: &mut Machine,
        config_dir: paths::config_dir(),
    }
    .walk_through(&mut config)
}

/// How the wizard asks.
trait Asking {
    fn say(&mut self, text: &str) -> Result<()>;
    fn text(&mut self, question: &str) -> Result<Option<String>>;
    fn secret(&mut self, question: &str) -> Result<Option<String>>;

    fn confirm(&mut self, question: &str) -> Result<bool> {
        Ok(self
            .text(&format!("{question} [y/N]"))?
            .is_some_and(|it| it.eq_ignore_ascii_case("y") || it.eq_ignore_ascii_case("yes")))
    }
}

/// What the wizard does to the machine beyond writing the config.
trait Doing {
    fn missing_programs(&mut self) -> Vec<&'static str>;
    fn stored_accounts(&mut self) -> usize;
    fn add_current_account(&mut self) -> Result<()>;
    fn has_secret(&mut self, name: &str) -> bool;
    fn jev_key_works(&mut self, key: &str) -> bool;
    fn keep_secret(&mut self, name: &str, value: &str) -> Result<Kept>;
    fn add_mcp_server(&mut self, name: &str, command: &[String]) -> Result<()>;
    fn service_installed(&mut self) -> bool;
    fn install_service(&mut self, start_at_boot: bool) -> Result<()>;
}

struct Wizard<'a> {
    asking: &'a mut dyn Asking,
    doing: &'a mut dyn Doing,
    config_dir: PathBuf,
}

impl Wizard<'_> {
    fn walk_through(&mut self, config: &mut Config) -> Result<()> {
        self.asking.say("Setting up Anna. Every question is optional: enter keeps what's there.\n")?;

        self.check_prerequisites()?;
        self.accounts()?;
        self.jev_key()?;
        self.prose_file("2/6  Prose style", "how Anna writes to people; everything she posts is held to it", "style.md")?;
        self.prose_file("3/6  CLAUDE.md", "who Anna is; every thread reads it", "CLAUDE.md")?;
        self.people(config)?;
        config.save()?;

        self.mcp_servers()?;
        self.service()?;

        self.asking.say("\nDone. `anna start` brings her up; `anna chat \"hello\"` talks to her from here.")
    }

    fn check_prerequisites(&mut self) -> Result<()> {
        let missing = self.doing.missing_programs();
        if missing.is_empty() {
            self.asking.say("Found claude, bwrap and socat.\n")
        } else {
            self.asking.say(&format!(
                "Missing: {}. Anna can't run hands without them — install them before starting her.\n",
                missing.join(", ")
            ))
        }
    }

    fn accounts(&mut self) -> Result<()> {
        let stored = self.doing.stored_accounts();
        self.asking.say(&format!(
            "Claude accounts in the rotation: {stored}. Anna moves to the one with the most headroom before a limit hits."
        ))?;

        if self.asking.confirm("Add the account Claude Code is logged into right now?")? {
            match self.doing.add_current_account() {
                Ok(()) => self.asking.say("Added. Log into another account and run `anna claude account add` for more.\n"),
                Err(error) => self.asking.say(&format!("Couldn't add it: {error:#}\n")),
            }
        } else {
            self.asking.say("")
        }
    }

    fn jev_key(&mut self) -> Result<()> {
        self.asking.say("1/6  Jev API key")?;
        self.asking.say("     Makes Anna's yes/no judgments fast and nearly free. Without one she asks haiku,")?;
        self.asking.say("     which is slower and spends your Claude subscription.")?;
        if self.doing.has_secret(config::JEV_API_KEY) {
            self.asking.say("     One is already stored.")?;
        }

        while let Some(key) = self.asking.secret("     Key")? {
            if self.doing.jev_key_works(&key) {
                let place = match self.doing.keep_secret(config::JEV_API_KEY, &key)? {
                    Kept::InKeyring => "your keyring",
                    Kept::InFile => "secrets.json next to the config, readable only by you — no keyring answered",
                };
                self.asking.say(&format!("     It works. Stored in {place}.\n"))?;
                return Ok(());
            }
            self.asking.say("     Jev refused that key. Try again, or enter to skip.")?;
        }
        self.asking.say("")
    }

    fn prose_file(&mut self, title: &str, purpose: &str, name: &str) -> Result<()> {
        let destination = self.config_dir.join(name);
        self.asking.say(title)?;
        self.asking.say(&format!("     A markdown file: {purpose}."))?;
        if destination.exists() {
            self.asking.say(&format!("     {} is in place.", destination.display()))?;
        }

        while let Some(given) = self.asking.text("     Path to your file")? {
            match adopt(&given, &destination) {
                Ok(()) => {
                    self.asking.say(&format!("     Copied to {}.\n", destination.display()))?;
                    return Ok(());
                }
                Err(error) => self.asking.say(&format!("     {error:#}. Try again, or enter to skip."))?,
            }
        }
        self.asking.say("")
    }

    fn people(&mut self, config: &mut Config) -> Result<()> {
        self.asking.say("4/6  People")?;
        self.asking.say("     Anna only listens to people you name. The id is what a source reports as the")?;
        self.asking.say("     sender — usually an email address.")?;
        for (sender, name) in &config.people {
            self.asking.say(&format!("     Listening to {name} ({sender})"))?;
        }

        while let Some(sender) = self.asking.text("     Sender id (enter when done)")? {
            if let Some(name) = self.asking.text("     Their name")? {
                config.people.insert(sender, name);
            }
        }
        self.asking.say("")
    }

    fn mcp_servers(&mut self) -> Result<()> {
        self.asking.say("5/6  MCP servers")?;
        self.asking.say("     How Anna acts, and where she listens. For example: basecamp, then `basecamp mcp`.")?;

        while let Some(name) = self.asking.text("     Server name (enter when done)")? {
            if let Some(command) = self.asking.text("     Command that starts it")? {
                let words: Vec<String> = command.split_whitespace().map(String::from).collect();
                match self.doing.add_mcp_server(&name, &words) {
                    Ok(()) => self.asking.say("")?,
                    Err(error) => self.asking.say(&format!("     Couldn't add it: {error:#}"))?,
                }
            }
        }
        self.asking.say("")
    }

    fn service(&mut self) -> Result<()> {
        self.asking.say("6/6  systemd")?;
        if self.doing.service_installed() {
            self.asking.say("     The service is installed; answering yes rewrites it with the current PATH.")?;
        }

        if self.asking.confirm("     Run Anna as a systemd user service, so she starts on her own?")? {
            let at_boot = self.asking.confirm("     Also while you're not logged in? (enables lingering for your user)")?;
            match self.doing.install_service(at_boot) {
                Ok(()) => self.asking.say("     Installed and enabled. `anna start` and `anna stop` go through it."),
                Err(error) => self.asking.say(&format!("     Couldn't install it: {error:#}")),
            }
        } else {
            Ok(())
        }
    }
}

fn adopt(given: &str, destination: &Path) -> Result<()> {
    let source = expanded(given);
    let text = std::fs::read_to_string(&source).with_context(|| format!("could not read {}", source.display()))?;
    fsutil::write_private(destination, &text)
}

fn expanded(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

struct Terminal<I, O> {
    input: I,
    output: O,
}

impl<I: BufRead, O: Write> Terminal<I, O> {
    fn answer(&mut self, question: &str) -> Result<Option<String>> {
        write!(self.output, "{question}: ")?;
        self.output.flush()?;

        let mut answer = String::new();
        self.input.read_line(&mut answer)?;
        let answer = answer.trim();
        if answer.is_empty() {
            Ok(None)
        } else {
            Ok(Some(answer.to_string()))
        }
    }
}

impl<I: BufRead, O: Write> Asking for Terminal<I, O> {
    fn say(&mut self, text: &str) -> Result<()> {
        writeln!(self.output, "{text}")?;
        Ok(())
    }

    fn text(&mut self, question: &str) -> Result<Option<String>> {
        self.answer(question)
    }

    fn secret(&mut self, question: &str) -> Result<Option<String>> {
        let _quiet = Echo::off();
        let answer = self.answer(question);
        writeln!(self.output)?;
        answer
    }
}

/// Turns the terminal's echo off for as long as it lives, and back on when
/// dropped — also when the wizard errors out mid-question.
struct Echo {
    before: Option<libc::termios>,
}

impl Echo {
    fn off() -> Echo {
        if !std::io::stdin().is_terminal() {
            return Echo { before: None };
        }

        unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut settings) != 0 {
                return Echo { before: None };
            }
            let before = settings;
            settings.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &settings);
            Echo { before: Some(before) }
        }
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        if let Some(before) = &self.before {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, before);
            }
        }
    }
}

struct Machine;

impl Doing for Machine {
    fn missing_programs(&mut self) -> Vec<&'static str> {
        ["claude", "bwrap", "socat"]
            .into_iter()
            .filter(|it| paths::program(it).is_none())
            .collect()
    }

    fn stored_accounts(&mut self) -> usize {
        ax::store::Roster::load().map(|it| it.accounts.len()).unwrap_or(0)
    }

    fn add_current_account(&mut self) -> Result<()> {
        ax::account::add(None, None, None)
    }

    fn has_secret(&mut self, name: &str) -> bool {
        secrets::load(name).is_some()
    }

    fn jev_key_works(&mut self, key: &str) -> bool {
        Judge::jev(key.to_string(), None)
            .probability("Is this text a greeting?", "Hello there.")
            .is_ok()
    }

    fn keep_secret(&mut self, name: &str, value: &str) -> Result<Kept> {
        secrets::store(name, value)
    }

    fn add_mcp_server(&mut self, name: &str, command: &[String]) -> Result<()> {
        mcp_cli::add(name, command)
    }

    fn service_installed(&mut self) -> bool {
        service::installed()
    }

    fn install_service(&mut self, start_at_boot: bool) -> Result<()> {
        service::install(start_at_boot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[derive(Default)]
    struct Pretend {
        kept: Vec<(String, String)>,
        servers: Vec<(String, Vec<String>)>,
        service: Option<bool>,
        accounts_added: usize,
    }

    impl Doing for Pretend {
        fn missing_programs(&mut self) -> Vec<&'static str> {
            vec!["bwrap"]
        }

        fn stored_accounts(&mut self) -> usize {
            2
        }

        fn add_current_account(&mut self) -> Result<()> {
            self.accounts_added += 1;
            Ok(())
        }

        fn has_secret(&mut self, _name: &str) -> bool {
            false
        }

        fn jev_key_works(&mut self, key: &str) -> bool {
            key == "good-key"
        }

        fn keep_secret(&mut self, name: &str, value: &str) -> Result<Kept> {
            self.kept.push((name.to_string(), value.to_string()));
            Ok(Kept::InKeyring)
        }

        fn add_mcp_server(&mut self, name: &str, command: &[String]) -> Result<()> {
            self.servers.push((name.to_string(), command.to_vec()));
            Ok(())
        }

        fn service_installed(&mut self) -> bool {
            false
        }

        fn install_service(&mut self, start_at_boot: bool) -> Result<()> {
            self.service = Some(start_at_boot);
            Ok(())
        }
    }

    fn walk(answers: &str, config: &mut Config, doing: &mut Pretend, directory: &Path) -> String {
        let mut printed = Vec::new();
        let mut asking = Terminal { input: answers.as_bytes(), output: &mut printed };
        let home = directory.join("config");
        std::fs::create_dir_all(&home).unwrap();

        // Saving goes to the real config path, so the walk stops short of it
        // here: each step is driven on its own
        let mut wizard = Wizard { asking: &mut asking, doing, config_dir: home };
        wizard.check_prerequisites().unwrap();
        wizard.accounts().unwrap();
        wizard.jev_key().unwrap();
        wizard.prose_file("2/6  Prose style", "how Anna writes", "style.md").unwrap();
        wizard.prose_file("3/6  CLAUDE.md", "who Anna is", "CLAUDE.md").unwrap();
        wizard.people(config).unwrap();
        wizard.mcp_servers().unwrap();
        wizard.service().unwrap();
        String::from_utf8(printed).unwrap()
    }

    #[test]
    fn answers_are_acted_on_and_skipped_questions_change_nothing() {
        let directory = std::env::temp_dir().join(format!("anna-setup-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let style = directory.join("my-style.md");
        std::fs::write(&style, "Short sentences.").unwrap();

        let answers = [
            "y",                                  // add the current account
            "bad-key", "good-key",                // the first key is refused
            "/no/such/file.md", style.to_str().unwrap(),
            "",                                   // no CLAUDE.md
            "marta@example.com", "Marta", "",     // one person
            "basecamp", "basecamp mcp", "",       // one server
            "yes", "n",                           // the service, not at boot
        ]
        .join("\n")
            + "\n";

        let mut config = Config::default();
        let mut doing = Pretend::default();
        let printed = walk(&answers, &mut config, &mut doing, &directory);

        assert!(printed.contains("Missing: bwrap"));
        assert_eq!(doing.accounts_added, 1);
        assert_eq!(doing.kept, [("jev_api_key".to_string(), "good-key".to_string())]);
        assert!(printed.contains("Jev refused that key"));
        assert!(printed.contains("Stored in your keyring"));

        let copied = directory.join("config/style.md");
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), "Short sentences.");
        assert_eq!(std::fs::metadata(&copied).unwrap().mode() & 0o777, 0o600);
        assert!(printed.contains("could not read /no/such/file.md"));
        assert!(!directory.join("config/CLAUDE.md").exists());

        assert_eq!(config.people.get("marta@example.com").map(String::as_str), Some("Marta"));
        assert_eq!(doing.servers, [("basecamp".to_string(), vec!["basecamp".to_string(), "mcp".to_string()])]);
        assert_eq!(doing.service, Some(false));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn saying_nothing_does_nothing() {
        let directory = std::env::temp_dir().join(format!("anna-setup-empty-{}", std::process::id()));
        let mut config = Config::default();
        let mut doing = Pretend::default();

        walk(&"\n".repeat(12), &mut config, &mut doing, &directory);

        assert_eq!(config, Config::default());
        assert_eq!(doing.accounts_added, 0);
        assert!(doing.kept.is_empty());
        assert!(doing.servers.is_empty());
        assert_eq!(doing.service, None);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
