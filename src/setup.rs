//! `anna setup`: the one time a person decides things for the agent. After
//! this it runs on its own.
//!
//! The walk: a name, who the agent is (its CLAUDE.md), how it writes, whether
//! it starts with the computer, whether Jev makes its judgments, and then one
//! block per tool whose command line is installed — Basecamp, HEY, Fizzy.
//! Every question keeps what is already there when skipped, so running setup
//! again is how anything gets changed later.
//!
//! A tool can be given to the agent in two ways, and they are not the same
//! thing. As an agent of its own — Basecamp can do this — it has its own
//! profile, its own notifications, and it listens: mention it and it answers
//! as itself. As you, it gets the tools and nothing else. It does not listen
//! there, because everything addressed to you would wake it and it would
//! answer your colleagues in your name.
//!
//! Asking and doing are both handed in, which is what lets the whole walk be
//! tested without a terminal, a keyring, systemd, or anyone's Basecamp.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;

use crate::config::{self, Call, Config, Pointers, Source, Trigger};
use crate::fsutil;
use crate::judge::Judge;
use crate::mcp_cli;
use crate::paths;
use crate::prompt::{self, Asking, Question, Terminal};
use crate::secrets::{self, Kept};
use crate::service;

pub fn run() -> Result<()> {
    let stdin = std::io::stdin();
    let mut asking = Terminal::new(stdin.lock(), std::io::stdout(), prompt::styled_for_stdout());

    Wizard {
        asking: &mut asking,
        doing: &mut Machine,
        config_dir: paths::config_dir(),
    }
    .walk_through()
}

/// A tool the agent can be given, when its command line is installed.
struct Tool {
    name: &'static str,
    title: &'static str,
    program: &'static str,
    can_be_its_own_agent: bool,
}

const TOOLS: [Tool; 3] = [
    Tool { name: "basecamp", title: "Basecamp", program: "basecamp", can_be_its_own_agent: true },
    Tool { name: "hey", title: "HEY", program: "hey", can_be_its_own_agent: false },
    Tool { name: "fizzy", title: "Fizzy", program: "fizzy", can_be_its_own_agent: false },
];

#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub id: String,
    pub name: String,
}

/// What setup does to the machine, beyond asking.
trait Doing {
    fn load_config(&mut self) -> Result<Config>;
    fn save_config(&mut self, config: &Config) -> Result<()>;
    fn missing_programs(&mut self) -> Vec<&'static str>;
    fn has_program(&mut self, program: &str) -> bool;
    fn has_secret(&mut self, name: &str) -> bool;
    fn jev_key_works(&mut self, key: &str) -> bool;
    fn keep_secret(&mut self, name: &str, value: &str) -> Result<Kept>;
    fn install_service(&mut self, agent: &str) -> Result<()>;
    /// Who the tool's command line is logged in as, for the confirm question.
    fn logged_in_as(&mut self, tool: &str) -> Vec<String>;
    fn accounts(&mut self, tool: &str, profile: Option<&str>) -> Result<Vec<Account>>;
    fn connect_agent(&mut self, asking: &mut dyn Asking, profile: &str, instruction: &str) -> Result<()>;
    fn can_watch(&mut self, tool: &str) -> bool;
    fn add_server(&mut self, name: &str, command: &[String]) -> Result<()>;
    fn add_source(&mut self, name: &str, source: Source) -> Result<()>;
}

struct Wizard<'a> {
    asking: &'a mut dyn Asking,
    doing: &'a mut dyn Doing,
    config_dir: PathBuf,
}

impl Wizard<'_> {
    fn walk_through(&mut self) -> Result<()> {
        let mut config = self.doing.load_config()?;
        self.asking.banner("ANNA", "Jag känner en bot, hon heter Anna, Anna heter hon")?;
        self.asking.note("\nEvery question is optional. Enter keeps what's already there.")?;
        self.check_prerequisites()?;

        self.name(&mut config)?;
        let agent = config.name.clone();
        self.about(&agent)?;
        self.style(&agent)?;
        self.doing.save_config(&config)?;

        self.run_at_startup(&agent)?;
        self.jev(&agent)?;
        for tool in &TOOLS {
            if self.doing.has_program(tool.program) {
                self.tool(tool, &agent)?;
            }
        }
        self.closing_notes(&agent)
    }

    fn check_prerequisites(&mut self) -> Result<()> {
        let missing = self.doing.missing_programs();
        if missing.is_empty() {
            self.asking.done("Found claude, bwrap and socat.")
        } else {
            self.asking.trouble(&format!(
                "Missing: {}. Hands can't run without them — install them before starting.",
                missing.join(", ")
            ))
        }
    }

    fn name(&mut self, config: &mut Config) -> Result<()> {
        let question = Question {
            title: "Name",
            question: "What do you want to name your agent?",
            hint: "This is how your agent will refer to itself.",
        };
        if let Some(name) = self.asking.string(&question, Some(&config.name))? {
            config.name = name;
        }
        self.asking.done(&format!("Your agent is called {}.", config.name))
    }

    fn about(&mut self, agent: &str) -> Result<()> {
        let title = format!("About {agent}");
        let question = format!("Give general guidance on how {agent} should behave.");
        let hint = format!("This will become {agent}'s CLAUDE.md file. You can change this later.");
        self.prose(&Question { title: &title, question: &question, hint: &hint }, "CLAUDE.md")
    }

    fn style(&mut self, agent: &str) -> Result<()> {
        let question = format!("How do you want {agent} to write prose?");
        let hint = format!("This will be used as guidance every time {agent} wants to write any kind of prose.");
        self.prose(&Question { title: "Style", question: &question, hint: &hint }, "style.md")
    }

    fn prose(&mut self, question: &Question, file: &str) -> Result<()> {
        let path = self.config_dir.join(file);
        let current = std::fs::read_to_string(&path).ok();

        match self.asking.text(question, current.as_deref())? {
            Some(text) => {
                fsutil::write_private(&path, &(text + "\n"))?;
                self.asking.done(&format!("Saved to {}.", path.display()))
            }
            None if current.is_some() => self.asking.done(&format!("Kept {}.", path.display())),
            None => self.asking.done("Skipped."),
        }
    }

    fn run_at_startup(&mut self, agent: &str) -> Result<()> {
        let question = format!("Do you want to start {agent} as soon as this computer starts?");
        let hint = format!("This will add a systemd entry for {agent}.");
        if !self.asking.yes(&Question { title: "Run at startup", question: &question, hint: &hint }, false)? {
            return self.asking.done(&format!("`anna start` brings {agent} up whenever you want."));
        }

        match self.doing.install_service(agent) {
            Ok(()) => self.asking.done("Installed and enabled. `anna start` and `anna stop` go through it."),
            Err(error) => self.asking.trouble(&format!("Couldn't install it: {error:#}")),
        }
    }

    fn jev(&mut self, agent: &str) -> Result<()> {
        let question = format!("Do you want to improve {agent}'s speed using Jev?");
        let hint = format!("{agent} can use Jev to make quick judgements instead of using Claude.");
        let already = self.doing.has_secret(config::JEV_API_KEY);
        if !self.asking.yes(&Question { title: "Improved efficiency", question: &question, hint: &hint }, already)? {
            return self.asking.done(&format!("{agent} will ask Claude's haiku instead."));
        }
        if already {
            self.asking.note("  A key is already stored; enter keeps it.")?;
        }

        let key_question = Question { title: "Jev API key", question: "Jev API key", hint: "Your Jev API key, from typesafe.ai." };
        while let Some(key) = self.asking.secret(&key_question)? {
            if self.doing.jev_key_works(&key) {
                let place = match self.doing.keep_secret(config::JEV_API_KEY, &key)? {
                    Kept::InKeyring => "your keyring",
                    Kept::InFile => "secrets.json next to the config, readable only by you (no keyring answered)",
                };
                return self.asking.done(&format!("It works. Stored in {place}."));
            }
            self.asking.trouble("Jev refused that key. Try again, or enter to skip.")?;
        }
        self.asking.done("No key changed.")
    }

    fn tool(&mut self, tool: &Tool, agent: &str) -> Result<()> {
        let question = format!("Do you want to allow {agent} access to {}?", tool.title);
        let hint = format!("This will allow {agent} to access {}; you'll be able to work and talk with them like a colleague.", tool.title);
        if !self.asking.yes(&Question { title: tool.title, question: &question, hint: &hint }, false)? {
            return self.asking.done(&format!("{agent} stays out of {}.", tool.title));
        }

        if tool.can_be_its_own_agent && self.wants_an_agent_profile(tool, agent)? {
            self.as_an_agent(tool, agent)
        } else {
            self.as_you(tool, agent)
        }
    }

    fn wants_an_agent_profile(&mut self, tool: &Tool, agent: &str) -> Result<bool> {
        let question = format!("Do you want to set {agent} up as an agent in {}?", tool.title);
        let hint = format!("This will give {agent} a separate profile from your own.");
        self.asking.yes(&Question { title: "Set up as an agent", question: &question, hint: &hint }, true)
    }

    fn as_an_agent(&mut self, tool: &Tool, agent: &str) -> Result<()> {
        let question = Question {
            title: "Paste instruction",
            question: "Paste the set up instructions here.",
            hint: "Create an agent profile in Adminland > Manage agents; it gives you one line to paste.",
        };
        let Some(instruction) = self.asking.string(&question, None)? else {
            return self.asking.done(&format!("Nothing pasted; {agent} stays out of {}.", tool.title));
        };

        let profile = profile_name(agent);
        self.asking.note(&format!("\n  Handing over to Claude to connect {agent}. It will show you a link and a code; approve it, then exit Claude to come back here."))?;
        if let Err(error) = self.doing.connect_agent(&mut *self.asking, &profile, &instruction) {
            return self.asking.trouble(&format!("Couldn't connect: {error:#}"));
        }

        let Some(account) = self.pick_account(tool, Some(&profile))? else {
            return Ok(());
        };
        let watches = self.doing.can_watch(tool.name);
        let command = words(&[tool.program, "--profile", &profile, "mcp", "--account", &account.id]);
        if let Err(error) = self.doing.add_server(tool.name, &command) {
            return self.asking.trouble(&format!("Couldn't add the {} server: {error:#}", tool.title));
        }
        self.doing.add_source(tool.name, basecamp_source(&profile, &account.id, watches))?;

        self.asking.done(&format!("{agent} is in {} under its own profile, in {}.", tool.title, account.name))?;
        if watches {
            self.asking.done(&format!("{agent} hears about mentions and assignments the moment they happen."))
        } else {
            self.asking.note(&format!("  This basecamp has no `watch` command yet, so {agent} checks for mentions once a minute."))
        }
    }

    fn as_you(&mut self, tool: &Tool, agent: &str) -> Result<()> {
        let identities = self.doing.logged_in_as(tool.name);
        let who = if identities.is_empty() { format!("your {} login", tool.title) } else { identities.join(", ") };
        let question = format!("Do you want to allow {agent} to use {} as {who}?", tool.title);
        let hint = format!("{agent} will be able to perform all actions as if it was {who}. It gets the tools only and won't listen there, or it would answer your colleagues in your name.");
        if !self.asking.yes(&Question { title: "Confirm", question: &question, hint: &hint }, false)? {
            return self.asking.done(&format!("{agent} stays out of {}.", tool.title));
        }

        let command = match tool.name {
            "basecamp" => match self.pick_account(tool, None)? {
                Some(account) => words(&[tool.program, "mcp", "--account", &account.id]),
                None => return Ok(()),
            },
            _ => words(&[tool.program, "mcp"]),
        };
        match self.doing.add_server(tool.name, &command) {
            Ok(()) => self.asking.done(&format!("{agent} can use {} as {who}.", tool.title)),
            Err(error) => self.asking.trouble(&format!("Couldn't add the {} server: {error:#}", tool.title)),
        }
    }

    fn pick_account(&mut self, tool: &Tool, profile: Option<&str>) -> Result<Option<Account>> {
        let accounts = match self.doing.accounts(tool.name, profile) {
            Ok(accounts) if !accounts.is_empty() => accounts,
            Ok(_) => {
                self.asking.trouble(&format!("{} shows no accounts for that login.", tool.title))?;
                return Ok(None);
            }
            Err(error) => {
                self.asking.trouble(&format!("Couldn't list {} accounts: {error:#}", tool.title))?;
                return Ok(None);
            }
        };

        if accounts.len() == 1 {
            Ok(accounts.into_iter().next())
        } else {
            let names: Vec<String> = accounts.iter().map(|it| it.name.clone()).collect();
            let question = Question { title: "Account", question: "Which account?", hint: "One agent works in one account." };
            let picked = self.asking.choice(&question, &names)?;
            Ok(accounts.into_iter().nth(picked))
        }
    }

    fn closing_notes(&mut self, agent: &str) -> Result<()> {
        self.asking.banner("Done", &format!("{agent} is set up."))?;
        self.asking.note(&format!(
            "
Bring {agent} up, or talk from here:
    anna start
    anna chat \"hello\"

Any MCP server can be one of {agent}'s tools. Add it the way you would for Claude Code:
    anna mcp add github -- github-mcp-server stdio
    anna mcp list
A tool argument that carries text for people goes through the style first:
    anna mcp prose github create_issue_comment body

More Claude subscriptions for {agent} to rotate between:
    anna claude account add
"
        ))
    }
}

/// What Basecamp's notifications look like, worked out against the real
/// thing: the same notification is bumped for every new comment, so what
/// makes one new is its id and when it went unread; the body is an excerpt,
/// so the thread gets the title, the excerpt and the link to read the rest;
/// and there is no single call that answers, so the thread answers with
/// Basecamp's own tools. It listens to anyone, because Basecamp already
/// decides who can reach an agent: the people in the projects it was added to.
fn basecamp_source(profile: &str, account: &str, watches: bool) -> Source {
    Source {
        server: "basecamp".to_string(),
        watch: Call { tool: "basecamp_account".to_string(), arguments: json!({ "action": "get_my_notifications" }) },
        every_seconds: if watches { 900 } else { 60 },
        items: "/unreads".to_string(),
        id: Pointers::Several(vec!["/id".to_string(), "/unread_at".to_string()]),
        conversation: "/readable_sgid".to_string(),
        sender: "/creator/email_address".to_string(),
        sender_name: Some("/creator/name".to_string()),
        anyone: true,
        text: Pointers::Several(vec!["/title".to_string(), "/content_excerpt".to_string(), "/app_url".to_string()]),
        reply: None,
        trigger: if watches {
            Some(Trigger { command: "basecamp".to_string(), args: words(&["--profile", profile, "watch", "--json", "--account", account]) })
        } else {
            None
        },
    }
}

fn profile_name(agent: &str) -> String {
    let name: String = agent
        .chars()
        .filter(|it| it.is_ascii_alphanumeric())
        .map(|it| it.to_ascii_lowercase())
        .collect();
    if name.is_empty() { "agent".to_string() } else { name }
}

fn words(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|it| it.to_string()).collect()
}

struct Machine;

impl Doing for Machine {
    fn load_config(&mut self) -> Result<Config> {
        Config::load()
    }

    fn save_config(&mut self, config: &Config) -> Result<()> {
        config.save()
    }

    fn missing_programs(&mut self) -> Vec<&'static str> {
        ["claude", "bwrap", "socat"].into_iter().filter(|it| paths::program(it).is_none()).collect()
    }

    fn has_program(&mut self, program: &str) -> bool {
        paths::program(program).is_some()
    }

    fn has_secret(&mut self, name: &str) -> bool {
        secrets::load(name).is_some()
    }

    fn jev_key_works(&mut self, key: &str) -> bool {
        let url = Config::load().ok().and_then(|it| it.jev_url);
        Judge::jev(key.to_string(), url.as_deref())
            .probability("Is this text a greeting?", "Hello there.")
            .is_ok()
    }

    fn keep_secret(&mut self, name: &str, value: &str) -> Result<Kept> {
        secrets::store(name, value)
    }

    fn install_service(&mut self, agent: &str) -> Result<()> {
        service::install(agent, true)
    }

    fn logged_in_as(&mut self, tool: &str) -> Vec<String> {
        let addresses = match tool {
            "basecamp" => json_from("basecamp", &["me", "--json"]).map(|it| vec![it["data"]["identity"]["email_address"].clone()]),
            "hey" => json_from("hey", &["account", "list", "--json"])
                .and_then(|it| it["data"].as_array().cloned())
                .map(|accounts| accounts.iter().map(|it| it["email"].clone()).collect()),
            _ => None,
        };

        addresses
            .unwrap_or_default()
            .iter()
            .filter_map(|it| it.as_str())
            .filter(|it| !it.is_empty())
            .map(String::from)
            .collect()
    }

    fn accounts(&mut self, tool: &str, profile: Option<&str>) -> Result<Vec<Account>> {
        let mut arguments = Vec::new();
        if let Some(profile) = profile {
            arguments.extend(["--profile", profile]);
        }
        arguments.extend(["accounts", "list", "--json"]);

        let listed = json_from(tool, &arguments).context("it gave no list")?;
        Ok(listed["data"]
            .as_array()
            .map(|accounts| {
                accounts
                    .iter()
                    .map(|it| Account { id: it["id"].to_string().trim_matches('"').to_string(), name: it["name"].as_str().unwrap_or_default().to_string() })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Basecamp's instruction is written for an AI tool to follow — fetch a
    /// page, start a device-code connection, wait for the person to approve —
    /// so an AI tool follows it: Claude, with the terminal, and told where
    /// the token has to end up.
    fn connect_agent(&mut self, asking: &mut dyn Asking, profile: &str, instruction: &str) -> Result<()> {
        let prompt = format!(
            "{instruction}\n\nWhen the connection is approved and you hold the access token, store it for the Basecamp command line under the profile \"{profile}\": run `basecamp profile create {profile}` (it may already exist, which is fine), then pipe the token into `basecamp --profile {profile} auth login --with-token`. Never print the token. Show the person the link and the code they need, wait for them to approve, and tell them plainly when it's done so they can exit."
        );
        let mut claude = Command::new(crate::claude::binary()?);
        claude.arg(prompt);
        asking.hand_over_to(&mut claude)?;

        let status = json_from("basecamp", &["--profile", profile, "auth", "status", "--json"]);
        if status.is_some_and(|it| it["data"]["authenticated"] == json!(true)) {
            Ok(())
        } else {
            bail!("the {profile} profile isn't logged in; run setup again once the agent is connected")
        }
    }

    fn can_watch(&mut self, tool: &str) -> bool {
        Command::new(tool)
            .args(["watch", "--help"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|it| it.success())
    }

    fn add_server(&mut self, name: &str, command: &[String]) -> Result<()> {
        mcp_cli::register(name, command)
    }

    fn add_source(&mut self, name: &str, source: Source) -> Result<()> {
        let mut config = Config::load()?;
        config.sources.insert(name.to_string(), source);
        config.save()
    }
}

fn json_from(program: &str, arguments: &[&str]) -> Option<Value> {
    let output = Command::new(program).args(arguments).stderr(std::process::Stdio::null()).output().ok()?;
    serde_json::from_slice(&output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Answers in the order the questions come, and a record of what was
    /// asked and said.
    #[derive(Default)]
    struct Scripted {
        answers: VecDeque<&'static str>,
        asked: Vec<String>,
        said: Vec<String>,
    }

    impl Scripted {
        fn answering(answers: &[&'static str]) -> Scripted {
            Scripted { answers: answers.iter().copied().collect(), ..Scripted::default() }
        }

        fn next(&mut self, question: &Question) -> String {
            self.asked.push(format!("{}: {}", question.title, question.question));
            self.answers.pop_front().unwrap_or_default().to_string()
        }

        fn optional(&mut self, question: &Question) -> Option<String> {
            Some(self.next(question)).filter(|it| !it.is_empty())
        }
    }

    impl Asking for Scripted {
        fn banner(&mut self, _title: &str, _subtitle: &str) -> Result<()> {
            Ok(())
        }

        fn string(&mut self, question: &Question, _current: Option<&str>) -> Result<Option<String>> {
            Ok(self.optional(question))
        }

        fn text(&mut self, question: &Question, _current: Option<&str>) -> Result<Option<String>> {
            Ok(self.optional(question))
        }

        fn secret(&mut self, question: &Question) -> Result<Option<String>> {
            Ok(self.optional(question))
        }

        fn yes(&mut self, question: &Question, default: bool) -> Result<bool> {
            Ok(match self.next(question).as_str() {
                "" => default,
                answer => answer == "y",
            })
        }

        fn choice(&mut self, question: &Question, _options: &[String]) -> Result<usize> {
            Ok(self.next(question).parse::<usize>().unwrap() - 1)
        }

        fn done(&mut self, text: &str) -> Result<()> {
            self.said.push(text.to_string());
            Ok(())
        }

        fn trouble(&mut self, text: &str) -> Result<()> {
            self.said.push(format!("TROUBLE {text}"));
            Ok(())
        }

        fn note(&mut self, text: &str) -> Result<()> {
            self.said.push(text.to_string());
            Ok(())
        }

        fn hand_over_to(&mut self, _command: &mut Command) -> Result<bool> {
            Ok(true)
        }
    }

    #[derive(Default)]
    struct Pretend {
        installed: Vec<&'static str>,
        config: Option<Config>,
        kept: Vec<(String, String)>,
        service: Option<String>,
        connected: Vec<(String, String)>,
        servers: Vec<(String, Vec<String>)>,
        sources: Vec<(String, Source)>,
        watches: bool,
    }

    impl Doing for Pretend {
        fn load_config(&mut self) -> Result<Config> {
            Ok(Config::default())
        }

        fn save_config(&mut self, config: &Config) -> Result<()> {
            self.config = Some(Config { name: config.name.clone(), ..Config::default() });
            Ok(())
        }

        fn missing_programs(&mut self) -> Vec<&'static str> {
            Vec::new()
        }

        fn has_program(&mut self, program: &str) -> bool {
            self.installed.contains(&program)
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

        fn install_service(&mut self, agent: &str) -> Result<()> {
            self.service = Some(agent.to_string());
            Ok(())
        }

        fn logged_in_as(&mut self, tool: &str) -> Vec<String> {
            vec![format!("me@{tool}.example.com")]
        }

        fn accounts(&mut self, _tool: &str, _profile: Option<&str>) -> Result<Vec<Account>> {
            Ok(vec![
                Account { id: "111".to_string(), name: "Work".to_string() },
                Account { id: "222".to_string(), name: "Personal".to_string() },
            ])
        }

        fn connect_agent(&mut self, _asking: &mut dyn Asking, profile: &str, instruction: &str) -> Result<()> {
            self.connected.push((profile.to_string(), instruction.to_string()));
            Ok(())
        }

        fn can_watch(&mut self, _tool: &str) -> bool {
            self.watches
        }

        fn add_server(&mut self, name: &str, command: &[String]) -> Result<()> {
            self.servers.push((name.to_string(), command.to_vec()));
            Ok(())
        }

        fn add_source(&mut self, name: &str, source: Source) -> Result<()> {
            self.sources.push((name.to_string(), source));
            Ok(())
        }
    }

    fn walk(asking: &mut Scripted, doing: &mut Pretend, name: &str) -> PathBuf {
        let config_dir = std::env::temp_dir().join(format!("anna-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config_dir);
        Wizard { asking, doing, config_dir: config_dir.clone() }.walk_through().unwrap();
        config_dir
    }

    #[test]
    fn an_agent_of_her_own_listens_and_a_tool_used_as_you_does_not() {
        let mut asking = Scripted::answering(&[
            "Botten",                                  // Name
            "You are dry and direct.",                 // About
            "Short sentences.",                        // Style
            "y",                                       // Run at startup
            "y", "bad-key", "good-key",                // Jev, a refused key, a good one
            "y", "y", "Connect this AI agent: fetch…", // Basecamp, as an agent, the pasted line
            "2",                                       // the second account
            "y", "y",                                  // HEY, confirm as me
        ]);
        let mut doing = Pretend { installed: vec!["basecamp", "hey"], watches: true, ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "agent");

        assert_eq!(doing.config.as_ref().unwrap().name, "Botten");
        assert_eq!(std::fs::read_to_string(config_dir.join("CLAUDE.md")).unwrap(), "You are dry and direct.\n");
        assert_eq!(std::fs::read_to_string(config_dir.join("style.md")).unwrap(), "Short sentences.\n");
        assert_eq!(doing.service.as_deref(), Some("Botten"));
        assert_eq!(doing.kept, [("jev_api_key".to_string(), "good-key".to_string())]);
        assert!(asking.said.iter().any(|it| it.contains("Jev refused that key")));

        assert_eq!(doing.connected, [("botten".to_string(), "Connect this AI agent: fetch…".to_string())]);
        assert_eq!(doing.servers[0], ("basecamp".to_string(), words(&["basecamp", "--profile", "botten", "mcp", "--account", "222"])));
        let (name, source) = &doing.sources[0];
        assert_eq!(name, "basecamp");
        assert!(source.anyone);
        assert_eq!(source.reply, None);
        assert_eq!(source.trigger.as_ref().unwrap().args, words(&["--profile", "botten", "watch", "--json", "--account", "222"]));

        assert_eq!(doing.servers[1], ("hey".to_string(), words(&["hey", "mcp"])));
        assert_eq!(doing.sources.len(), 1, "a tool used as you gets no source");
        assert!(asking.asked.iter().any(|it| it == "Confirm: Do you want to allow Botten to use HEY as me@hey.example.com?"));
        assert!(!asking.asked.iter().any(|it| it.contains("Fizzy")), "fizzy isn't installed, so it isn't offered");

        std::fs::remove_dir_all(config_dir).unwrap();
    }

    #[test]
    fn saying_nothing_changes_nothing() {
        let mut asking = Scripted::answering(&[]);
        let mut doing = Pretend { installed: vec!["basecamp", "hey", "fizzy"], ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "empty");

        assert_eq!(doing.config.as_ref().unwrap().name, "Anna");
        assert!(!config_dir.join("CLAUDE.md").exists());
        assert!(!config_dir.join("style.md").exists());
        assert_eq!(doing.service, None);
        assert!(doing.kept.is_empty());
        assert!(doing.servers.is_empty());
        assert!(doing.sources.is_empty());
        assert!(asking.asked.iter().any(|it| it.starts_with("Fizzy:")));
    }

    #[test]
    fn basecamp_as_you_asks_which_account_and_adds_tools_only() {
        let mut asking = Scripted::answering(&["", "", "", "", "", "y", "n", "y", "1"]);
        let mut doing = Pretend { installed: vec!["basecamp"], ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "as-you");

        assert_eq!(doing.servers, [("basecamp".to_string(), words(&["basecamp", "mcp", "--account", "111"]))]);
        assert!(doing.sources.is_empty());
        assert!(doing.connected.is_empty());
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn profile_names_are_safe_for_a_command_line() {
        assert_eq!(profile_name("Botten Anna"), "bottenanna");
        assert_eq!(profile_name("--profile evil"), "profileevil");
        assert_eq!(profile_name("🤖"), "agent");
    }
}
