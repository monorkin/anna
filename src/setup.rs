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

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::{self, Config, Source};
use crate::fsutil;
use crate::paths;
use crate::prompt::{self, Asking, Question, Terminal};
use crate::secrets::Kept;
use crate::setup_machine::Machine;

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
pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub program: &'static str,
    pub can_be_its_own_agent: bool,
    /// Places where colleagues hand out work, so where it matters whose word
    /// the agent acts on. Mail is not one: anyone can write to an inbox.
    pub asks_who_to_trust: bool,
}

const TOOLS: [Tool; 3] = [
    Tool { name: "basecamp", title: "Basecamp", program: "basecamp", can_be_its_own_agent: true, asks_who_to_trust: true },
    Tool { name: "hey", title: "HEY", program: "hey", can_be_its_own_agent: false, asks_who_to_trust: false },
    Tool { name: "fizzy", title: "Fizzy", program: "fizzy", can_be_its_own_agent: false, asks_who_to_trust: true },
];

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub id: String,
    pub name: String,
}

/// What setup does to the machine, beyond asking.
pub trait Doing {
    fn load_config(&mut self) -> Result<Config>;
    fn save_config(&mut self, config: &Config) -> Result<()>;
    fn missing_programs(&mut self) -> Vec<&'static str>;
    fn has_program(&mut self, program: &str) -> bool;
    fn has_secret(&mut self, name: &str) -> bool;
    fn jev_key_works(&mut self, key: &str) -> bool;
    fn keep_secret(&mut self, name: &str, value: &str) -> Result<Kept>;
    fn install_service(&mut self, agent: &str) -> Result<()>;
    /// The GitHub login of her own she already has, if any.
    fn github_login(&mut self) -> Option<String>;
    fn git_email(&mut self) -> Option<String>;
    /// Hands the terminal to gh until her own folder is logged in; answers
    /// with the login.
    fn log_in_to_github(&mut self) -> Result<String>;
    fn set_git_identity(&mut self, name: &str, email: &str) -> Result<()>;
    /// Who the tool's command line is logged in as, for the confirm question.
    fn logged_in_as(&mut self, tool: &str) -> Vec<String>;
    /// The profiles the tool's command line has, when it has profiles.
    fn profiles(&mut self, tool: &str) -> Vec<Profile>;
    /// Who one profile is logged in as.
    fn identity_of(&mut self, tool: &str, profile: Option<&str>) -> Option<String>;
    /// The ids the tool knows someone by, in every account the person
    /// running setup can see. Empty when it knows nobody by that address.
    fn person_ids(&mut self, tool: &str, address: &str) -> Vec<String>;
    fn accounts(&mut self, tool: &str, profile: Option<&str>) -> Result<Vec<Account>>;
    fn can_connect_agents(&mut self, tool: &str) -> bool;
    /// Hands the terminal to the tool's own command line until the agent is
    /// connected under `profile`.
    fn connect_agent(&mut self, tool: &str, profile: &str, agent: &str) -> Result<()>;
    fn can_watch(&mut self, tool: &str) -> bool;
    fn add_server(&mut self, name: &str, command: &[String], env: BTreeMap<String, String>, acts_as: ActsAs) -> Result<()>;
    fn add_source(&mut self, name: &str, source: Source) -> Result<()>;
    fn trust(&mut self, address: &str, name: &str) -> Result<()>;
}

/// Whose name a server's tools act in. One that acts as the person is only
/// offered to turns on a trusted person's word.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActsAs {
    Agent,
    Person,
}

/// What a source reports its senders as, which is what `people` has to be
/// keyed by.
#[derive(Clone, Copy)]
pub enum RecognizedBy {
    Address,
    PersonId,
}

pub enum GoOn {
    Connect,
    AsYou,
    Without,
}

pub struct Wizard<'a> {
    pub asking: &'a mut dyn Asking,
    pub doing: &'a mut dyn Doing,
    pub config_dir: PathBuf,
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
        if self.doing.has_program("gh") {
            self.github(&agent)?;
        }
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

    /// A GitHub account of her own, so what she pushes and opens is hers
    /// and not the person's. The login is gh's to do; setup then writes the
    /// identity her commits carry, which needs the account's email.
    fn github(&mut self, agent: &str) -> Result<()> {
        let already = self.doing.github_login();
        let question = format!("Do you want {agent} to have a GitHub login of its own?");
        let hint = format!("{agent} then pushes and opens pull requests as that account instead of as you. Make the account first, and give it access to the repositories {agent} works in.");
        if !self.asking.yes(&Question { title: "GitHub", question: &question, hint: &hint }, already.is_some())? {
            return self.asking.done(&format!("{agent} pushes nothing on its own; when you tell it to use your login, it does."));
        }

        let login = match already {
            Some(login) => {
                self.asking.done(&format!("Already logged in as {login}."))?;
                login
            }
            None => {
                self.asking.note("\n  gh takes it from here: approve the login in the browser and come back.\n")?;
                match self.doing.log_in_to_github() {
                    Ok(login) => login,
                    Err(error) => return self.asking.trouble(&format!("Couldn't log in: {error:#}. Nothing was changed; run setup again to retry.")),
                }
            }
        };

        let current = self.doing.git_email();
        let email_question = Question {
            title: "GitHub email",
            question: "Which email address does that account use?",
            hint: "GitHub credits commits to the account by it.",
        };
        match self.asking.string(&email_question, current.as_deref())?.or(current) {
            Some(email) => {
                self.doing.set_git_identity(agent, &email)?;
                self.asking.done(&format!("{agent} pushes and opens pull requests as {login}, with commits by {email}."))
            }
            None => self.asking.trouble(&format!("Without an email, {agent}'s commits are credited to nobody. Run setup again, or `anna github login --email <address>`.")),
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

/// Basecamp shows adjacent `<p>`s with no space between them; its own editor
/// writes paragraphs as text with a blank line (`<br><br>`) between, which
/// is what both notes say to do.
/// What carries text for people on each tool's server, so the editor sees
/// it and hands never get it. Basecamp's tools are gateways: one tool per
/// area, the action's arguments under `params`.
pub fn prose_of(tool: &str) -> BTreeMap<String, Vec<String>> {
    match tool {
        "basecamp" => BTreeMap::from([
            ("basecamp_messages".to_string(), words(&["/params/content", "/params/subject"])),
            ("basecamp_todos".to_string(), words(&["/params/content", "/params/description"])),
            ("basecamp_campfires".to_string(), words(&["/params/content"])),
            ("basecamp_cards".to_string(), words(&["/params/content", "/params/title"])),
        ]),
        "hey" => BTreeMap::from([("hey_threads".to_string(), words(&["/params/body", "/params/subject"]))]),
        _ => BTreeMap::new(),
    }
}

/// The environment a tool runs in when the agent has a profile of its own
/// there: its config lives in the agent's folder, not the person's.
pub fn own_tool_config() -> BTreeMap<String, String> {
    BTreeMap::from([("XDG_CONFIG_HOME".to_string(), config::HER_TOOLS.to_string())])
}

/// A name to suggest for an address: marta.k@example.com is probably Marta.
pub fn name_from(address: &str) -> String {
    let first = address.split(['@', '.', '+', '_', '-']).next().unwrap_or_default();
    let mut letters = first.chars();
    match letters.next() {
        Some(initial) => initial.to_uppercase().chain(letters).collect(),
        None => address.to_string(),
    }
}

pub fn profile_name(agent: &str) -> String {
    let name: String = agent
        .chars()
        .filter(|it| it.is_ascii_alphanumeric())
        .map(|it| it.to_ascii_lowercase())
        .collect();
    if name.is_empty() { "agent".to_string() } else { name }
}

pub fn words(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|it| it.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Cursor;
    use serde_json::json;
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
    }

    #[derive(Default)]
    struct Pretend {
        installed: Vec<&'static str>,
        config: Option<Config>,
        kept: Vec<(String, String)>,
        service: Option<String>,
        too_old_for_checks: usize,
        connected: Vec<String>,
        servers: Vec<(String, Vec<String>)>,
        server_env: Vec<BTreeMap<String, String>>,
        acts_as: Vec<ActsAs>,
        trusted: Vec<(String, String)>,
        sources: Vec<(String, Source)>,
        watches: bool,
        profiles: Vec<&'static str>,
        github_login: Option<String>,
        git_identity: Option<(String, String)>,
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

        fn github_login(&mut self) -> Option<String> {
            self.github_login.clone()
        }

        fn git_email(&mut self) -> Option<String> {
            self.git_identity.as_ref().map(|(_, email)| email.clone())
        }

        fn log_in_to_github(&mut self) -> Result<String> {
            self.github_login = Some("botten-agent".to_string());
            Ok("botten-agent".to_string())
        }

        fn set_git_identity(&mut self, name: &str, email: &str) -> Result<()> {
            self.git_identity = Some((name.to_string(), email.to_string()));
            Ok(())
        }

        fn logged_in_as(&mut self, tool: &str) -> Vec<String> {
            vec![format!("me@{tool}.example.com")]
        }

        fn profiles(&mut self, _tool: &str) -> Vec<Profile> {
            self.profiles.iter().map(|it| Profile { name: it.to_string() }).collect()
        }

        fn identity_of(&mut self, tool: &str, profile: Option<&str>) -> Option<String> {
            match profile {
                Some("bot") => Some("bot@basecamp.example.com".to_string()),
                _ => Some(format!("me@{tool}.example.com")),
            }
        }

        fn person_ids(&mut self, _tool: &str, address: &str) -> Vec<String> {
            match address {
                "me@basecamp.example.com" => vec!["1001".to_string()],
                "marta.k@example.com" => vec!["1002".to_string(), "2002".to_string()],
                _ => Vec::new(),
            }
        }

        fn accounts(&mut self, _tool: &str, _profile: Option<&str>) -> Result<Vec<Account>> {
            Ok(vec![
                Account { id: "111".to_string(), name: "Work".to_string() },
                Account { id: "222".to_string(), name: "Personal".to_string() },
            ])
        }

        fn can_connect_agents(&mut self, _tool: &str) -> bool {
            if self.too_old_for_checks == 0 {
                true
            } else {
                self.too_old_for_checks -= 1;
                false
            }
        }

        fn connect_agent(&mut self, tool: &str, profile: &str, agent: &str) -> Result<()> {
            self.connected.push(format!("{tool} {profile} {agent}"));
            Ok(())
        }

        fn can_watch(&mut self, _tool: &str) -> bool {
            self.watches
        }

        fn add_server(&mut self, name: &str, command: &[String], env: BTreeMap<String, String>, acts_as: ActsAs) -> Result<()> {
            self.servers.push((name.to_string(), command.to_vec()));
            self.server_env.push(env);
            self.acts_as.push(acts_as);
            Ok(())
        }

        fn add_source(&mut self, name: &str, source: Source) -> Result<()> {
            self.sources.push((name.to_string(), source));
            Ok(())
        }

        fn trust(&mut self, address: &str, name: &str) -> Result<()> {
            self.trusted.push((address.to_string(), name.to_string()));
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
            "y", "y",                                  // Basecamp, as an agent
            "y", "",                                 // trust whoever runs setup, under the suggested name
            "marta.k@example.com", "Marta K", "",      // and one more person
            "n",                                       // nobody else may assign work
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

        assert_eq!(doing.connected, ["basecamp botten Botten"], "the profile is named after the agent");
        assert!(!asking.asked.iter().any(|it| it.contains("secret")), "connecting is the command line's, and setup never asks for a secret");
        assert_eq!(doing.servers[0], ("basecamp".to_string(), words(&["basecamp", "--profile", "botten", "mcp"])));
        assert_eq!(doing.acts_as, [ActsAs::Agent, ActsAs::Person], "the agent acts as itself; HEY used as you acts as you");
        assert_eq!(doing.server_env[0]["XDG_CONFIG_HOME"], "{tools}", "her Basecamp profile lives in her own folder, wherever that is");
        assert_eq!(source_env(&doing), "{tools}");
        assert!(config::environment(&doing.server_env[0])["XDG_CONFIG_HOME"].ends_with("/anna/tools"));
        assert!(doing.server_env[1].is_empty(), "a tool used as you runs with your own config");
        let (name, source) = &doing.sources[0];
        assert_eq!(name, "basecamp");
        assert_eq!(
            doing.trusted,
            [("1001".to_string(), "Me".to_string()), ("1002".to_string(), "Marta K".to_string()), ("2002".to_string(), "Marta K".to_string())],
            "an agent's inbox reports people by id, so that is what they are trusted as — in every account they are in"
        );
        assert_eq!(source.watch.arguments, json!({ "action": "poll_inbox", "params": {} }), "an agent has an inbox, not notifications");
        assert_eq!(source.sender, "/event/creator_id");
        assert_eq!(source.cursor, Some(Cursor { from: "/position".to_string(), into: "/params/position".to_string() }));
        assert!(source.note.as_ref().unwrap().contains("Read the recording"));
        assert!(!source.anyone, "nobody but the trusted people is heard");
        assert!(asking.said.iter().any(|it| it.contains("acts only on the people you named")));
        assert_eq!(source.reply, None);
        assert_eq!(source.trigger.as_ref().unwrap().args, words(&["--profile", "botten", "watch", "--json"]));

        assert_eq!(doing.servers[1], ("hey".to_string(), words(&["hey", "mcp"])));
        assert_eq!(doing.sources.len(), 1, "a tool used as you gets no source");
        assert!(asking.asked.iter().any(|it| it == "Confirm: Do you want to allow Botten to use HEY as me@hey.example.com?"));
        assert!(!asking.asked.iter().any(|it| it.contains("Fizzy")), "fizzy isn't installed, so it isn't offered");

        std::fs::remove_dir_all(config_dir).unwrap();
    }

    #[test]
    fn a_github_login_of_her_own_is_made_at_setup_and_kept_after() {
        let mut asking = Scripted::answering(&[
            "Botten", "", "", "n", "n",   // name, about, style, no service, no Jev
            "y", "botten@example.com",    // GitHub, and the account's email
        ]);
        let mut doing = Pretend { installed: vec!["gh"], ..Pretend::default() };
        walk(&mut asking, &mut doing, "github");
        assert_eq!(doing.github_login.as_deref(), Some("botten-agent"));
        assert_eq!(doing.git_identity, Some(("Botten".to_string(), "botten@example.com".to_string())));
        assert!(asking.said.iter().any(|it| it.contains("as botten-agent, with commits by botten@example.com")), "{:?}", asking.said);

        let mut again = Scripted::answering(&["Botten", "", "", "n", "n", "", ""]);
        walk(&mut again, &mut doing, "github-again");
        assert_eq!(doing.git_identity, Some(("Botten".to_string(), "botten@example.com".to_string())), "enter keeps the login and the email");
        assert!(again.said.iter().any(|it| it.contains("Already logged in as botten-agent")));

        let mut without_gh = Scripted::answering(&["Botten", "", "", "n", "n"]);
        let mut nothing = Pretend::default();
        walk(&mut without_gh, &mut nothing, "github-without");
        assert!(!without_gh.asked.iter().any(|it| it.contains("GitHub")), "no gh, no question");
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
        assert!(!asking.asked.iter().any(|it| it.starts_with("Profile:") || it.starts_with("Listen:")), "one profile, and it is the person's: nothing to pick, nothing to listen as");
        assert_eq!(doing.trusted, [("me@basecamp.example.com".to_string(), "Me".to_string())], "enter trusts whoever runs setup");
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn a_profile_logged_in_as_her_own_user_is_hers_to_listen_with() {
        // Basecamp, not as an agent, the second profile, confirm, the first account, listen: yes, trust me, my name, nobody else, anyone may assign: no
        let mut asking = Scripted::answering(&["", "", "", "", "", "y", "n", "2", "y", "1", "y", "y", "", "", "n"]);
        let mut doing = Pretend { installed: vec!["basecamp"], profiles: vec!["personal", "bot"], watches: true, ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "own-user");

        assert!(asking.asked.iter().any(|it| it == "Confirm: Do you want to allow Anna to use Basecamp as bot@basecamp.example.com?"));
        assert_eq!(doing.servers, [("basecamp".to_string(), words(&["basecamp", "--profile", "bot", "mcp", "--account", "111"]))]);
        assert!(doing.server_env[0].is_empty(), "the profile is in the person's own command line config, where they logged it in");
        assert_eq!(doing.acts_as, [ActsAs::Agent], "her own user acts as her");
        let (_, source) = &doing.sources[0];
        assert_eq!(source.watch.tool, "basecamp_account", "a user hears through notifications, the way a person does");
        assert_eq!(source.sender, "/creator/id", "a user that isn't an admin sees addresses redacted");
        assert_eq!(source.trigger.as_ref().unwrap().args, words(&["--profile", "bot", "watch", "--json", "--account", "111"]));
        assert!(!source.anyone);
        assert_eq!(doing.trusted, [("1001".to_string(), "Me".to_string())], "so the trusted person is known by id");
        let _ = std::fs::remove_dir_all(config_dir);

        // The same, but the person's own profile picked: no listening offered
        let mut asking = Scripted::answering(&["", "", "", "", "", "y", "n", "1", "y", "1", "y", ""]);
        let mut doing = Pretend { installed: vec!["basecamp"], profiles: vec!["personal", "bot"], ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "own-profile");
        assert_eq!(doing.servers[0].1, words(&["basecamp", "--profile", "personal", "mcp", "--account", "111"]));
        assert_eq!(doing.acts_as, [ActsAs::Person], "the person's own profile is only for turns on a trusted word");
        assert!(doing.sources.is_empty());
        assert!(!asking.asked.iter().any(|it| it.starts_with("Listen:")));
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn anyone_can_hand_an_agent_work_while_only_trusted_people_direct_it() {
        let mut asking = Scripted::answering(&["", "", "", "", "", "y", "y", "y", "", "", ""]);
        let mut doing = Pretend { installed: vec!["basecamp"], ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "anyone");

        assert_eq!(doing.trusted.len(), 1);
        assert!(doing.sources[0].1.anyone, "enter says yes: anyone may assign work");
        assert!(asking.asked.iter().any(|it| it == "Work from anyone: Should anyone in Basecamp be able to assign Anna work?"));
        assert!(asking.said.iter().any(|it| it.contains("only the people you named can direct it")));
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn an_agent_that_would_hear_nobody_is_pointed_out() {
        let mut asking = Scripted::answering(&["", "", "", "", "", "y", "y", "n", "", "n"]);
        let mut doing = Pretend { installed: vec!["basecamp"], ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "nobody");

        assert!(doing.trusted.is_empty());
        assert!(!doing.sources[0].1.anyone);
        assert!(asking.said.iter().any(|it| it.starts_with("TROUBLE Nobody is trusted and nobody else may assign work")));
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn someone_basecamp_does_not_know_is_not_trusted_on_paper() {
        let mut asking = Scripted::answering(&["", "", "", "", "", "y", "y", "n", "ghost@example.com", "", "n"]);
        let mut doing = Pretend { installed: vec!["basecamp"], ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "ghost");

        assert!(doing.trusted.is_empty());
        assert!(asking.said.iter().any(|it| it.starts_with("TROUBLE Couldn't find ghost@example.com in Basecamp")));
        assert!(!asking.asked.iter().any(|it| it.starts_with("Their name")), "nobody is named who couldn't be found");
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn a_name_is_suggested_from_the_address() {
        assert_eq!(name_from("marta.k@example.com"), "Marta");
        assert_eq!(name_from("ivo@example.com"), "Ivo");
        assert_eq!(name_from("@example.com"), "@example.com");
    }

    #[test]
    fn setup_waits_while_an_old_basecamp_is_updated() {
        let mut asking = Scripted::answering(&["", "", "", "", "", "y", "y", "1", "1", "y", "", "", ""]);
        let mut doing = Pretend { installed: vec!["basecamp"], too_old_for_checks: 2, ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "updated");

        assert_eq!(asking.said.iter().filter(|it| it.starts_with("TROUBLE The installed basecamp can't connect agents")).count(), 2);
        assert_eq!(doing.connected, ["basecamp anna Anna"], "checking again after the update carries on from where it was");
        assert_eq!(doing.sources.len(), 1);
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn an_old_basecamp_can_still_be_used_as_you_or_left_for_later() {
        let mut asking = Scripted::answering(&["", "", "", "", "", "y", "y", "2", "y", "1"]);
        let mut doing = Pretend { installed: vec!["basecamp"], too_old_for_checks: usize::MAX, ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "old-as-you");

        assert_eq!(doing.servers, [("basecamp".to_string(), words(&["basecamp", "mcp", "--account", "111"]))]);
        assert!(doing.connected.is_empty());
        assert!(doing.sources.is_empty());
        let _ = std::fs::remove_dir_all(config_dir);

        let mut asking = Scripted::answering(&["", "", "", "", "", "y", "y", "3"]);
        let mut doing = Pretend { installed: vec!["basecamp"], too_old_for_checks: usize::MAX, ..Pretend::default() };
        let config_dir = walk(&mut asking, &mut doing, "old-later");

        assert!(doing.servers.is_empty());
        assert!(asking.said.iter().any(|it| it.contains("stays out of Basecamp for now")));
        let _ = std::fs::remove_dir_all(config_dir);
    }

    fn source_env(doing: &Pretend) -> String {
        doing.sources[0].1.trigger.as_ref().unwrap().env["XDG_CONFIG_HOME"].clone()
    }

    #[test]
    fn profile_names_are_safe_for_a_command_line() {
        assert_eq!(profile_name("Botten Anna"), "bottenanna");
        assert_eq!(profile_name("--profile evil"), "profileevil");
        assert_eq!(profile_name("🤖"), "agent");
    }
}
