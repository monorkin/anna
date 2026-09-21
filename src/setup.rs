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
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::config::{self, Call, Config, Cursor, Pointers, Source, Trigger};
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
    /// Places where colleagues hand out work, so where it matters whose word
    /// the agent acts on. Mail is not one: anyone can write to an inbox.
    asks_who_to_trust: bool,
}

const TOOLS: [Tool; 3] = [
    Tool { name: "basecamp", title: "Basecamp", program: "basecamp", can_be_its_own_agent: true, asks_who_to_trust: true },
    Tool { name: "hey", title: "HEY", program: "hey", can_be_its_own_agent: false, asks_who_to_trust: false },
    Tool { name: "fizzy", title: "Fizzy", program: "fizzy", can_be_its_own_agent: false, asks_who_to_trust: true },
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
    /// The ids the tool knows someone by, in every account the person
    /// running setup can see. Empty when it knows nobody by that address.
    fn person_ids(&mut self, tool: &str, address: &str) -> Vec<String>;
    fn accounts(&mut self, tool: &str, profile: Option<&str>) -> Result<Vec<Account>>;
    fn can_connect_agents(&mut self, tool: &str) -> bool;
    /// Hands the terminal to the tool's own command line until the agent is
    /// connected under `profile`.
    fn connect_agent(&mut self, tool: &str, profile: &str, agent: &str) -> Result<()>;
    fn can_watch(&mut self, tool: &str) -> bool;
    fn add_server(&mut self, name: &str, command: &[String], env: BTreeMap<String, String>) -> Result<()>;
    fn add_source(&mut self, name: &str, source: Source) -> Result<()>;
    fn trust(&mut self, address: &str, name: &str) -> Result<()>;
}

/// What a source reports its senders as, which is what `people` has to be
/// keyed by.
#[derive(Clone, Copy)]
enum RecognizedBy {
    Address,
    PersonId,
}

enum GoOn {
    Connect,
    AsYou,
    Without,
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
        match self.how_to_go_on_as_an_agent(tool, agent)? {
            GoOn::Connect => self.connect_as_an_agent(tool, agent),
            GoOn::AsYou => self.as_you(tool, agent),
            GoOn::Without => self.asking.done(&format!("{agent} stays out of {} for now. Run setup again whenever you like.", tool.title)),
        }
    }

    /// Connecting an agent is the command line's job, and one that is too old
    /// to do it is a thing the person can fix in another terminal while setup
    /// waits here.
    fn how_to_go_on_as_an_agent(&mut self, tool: &Tool, agent: &str) -> Result<GoOn> {
        while !self.doing.can_connect_agents(tool.name) {
            self.asking.trouble(&format!(
                "The installed {} can't connect agents: it has no `auth agent connect`, which is newer than it is.",
                tool.program
            ))?;

            let hint = format!("Update {} in another terminal and check again; setup waits here.", tool.program);
            let question = Question { title: "Connecting an agent", question: "How do you want to go on?", hint: &hint };
            let options = ["Check again".to_string(), format!("Let {agent} use {} as you instead", tool.title), format!("Leave {} for now", tool.title)];
            match self.asking.choice(&question, &options)? {
                0 => {}
                1 => return Ok(GoOn::AsYou),
                _ => return Ok(GoOn::Without),
            }
        }
        Ok(GoOn::Connect)
    }

    /// The command line does the whole handshake: it shows a link and a code,
    /// the person approves which agent this computer acts as, and it keeps
    /// what comes back under the agent's own profile, in the agent's own
    /// config folder. Setup never sees a secret, and the person's own login —
    /// and which profile is their default — is never touched.
    fn connect_as_an_agent(&mut self, tool: &Tool, agent: &str) -> Result<()> {
        let profile = profile_name(agent);
        self.asking.note(&format!("\n  {} takes it from here: approve {agent} in the browser and come back.\n", tool.title))?;
        if let Err(error) = self.doing.connect_agent(tool.name, &profile, agent) {
            return self.asking.trouble(&format!("Couldn't connect {agent}: {error:#}. Nothing was changed; run setup again to retry."));
        }

        let watches = self.doing.can_watch(tool.name);
        let command = words(&[tool.program, "--profile", &profile, "mcp"]);
        if let Err(error) = self.doing.add_server(tool.name, &command, own_tool_config()) {
            return self.asking.trouble(&format!("Couldn't add the {} server: {error:#}", tool.title));
        }

        let trusted = self.trusted_people(tool, agent, RecognizedBy::PersonId)?;
        let anyone = self.anyone_may_assign_work(tool, agent)?;
        self.doing.add_source(tool.name, basecamp_source(&profile, watches, anyone))?;

        self.asking.done(&format!("{agent} is in {} under its own profile.", tool.title))?;
        match (trusted, anyone) {
            (0, false) => self.asking.trouble(&format!("Nobody is trusted and nobody else may assign work, so {agent} won't act on anything in {}. Run setup again to change that.", tool.title))?,
            (_, false) => self.asking.done(&format!("{agent} acts only on the people you named, and ignores everyone else."))?,
            (_, true) => self.asking.done(&format!("Anyone who can reach {agent} there can hand it work; only the people you named can direct it."))?,
        }
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
        if let Err(error) = self.doing.add_server(tool.name, &command, BTreeMap::new()) {
            return self.asking.trouble(&format!("Couldn't add the {} server: {error:#}", tool.title));
        }
        self.asking.done(&format!("{agent} can use {} as {who}.", tool.title))?;

        if tool.asks_who_to_trust {
            self.trusted_people(tool, agent, RecognizedBy::Address)?;
        }
        Ok(())
    }

    /// Who the agent takes direction from: the people who can change how it
    /// behaves and have it do things on the machine it runs on. It is the one
    /// decision about authority that a person makes, here, and that is looked
    /// up in code ever after. Whoever is running setup is offered first.
    fn trusted_people(&mut self, tool: &Tool, agent: &str, recognized_by: RecognizedBy) -> Result<usize> {
        let mut trusted = 0;
        let hint = format!("Trusted people can tell {agent} to change how it behaves, and to do things on this computer, like restarting it. They are found in {} by their email address.", tool.title);

        for address in self.doing.logged_in_as(tool.name) {
            let question = format!("Should {agent} trust {address}?");
            if self.asking.yes(&Question { title: "Trusted person", question: &question, hint: &hint }, true)? {
                trusted += self.trust(tool, agent, &address, recognized_by)?;
            }
        }

        let question = format!("Who else should {agent} trust in {}? Their email address.", tool.title);
        let last_hint = format!("{hint} Enter when there's nobody else.");
        while let Some(address) = self.asking.string(&Question { title: "Trusted person", question: &question, hint: &last_hint }, None)? {
            trusted += self.trust(tool, agent, &address, recognized_by)?;
        }
        Ok(trusted)
    }

    /// Whether everyone else who can reach the agent may hand it work. For
    /// them a turn runs with no shell and no way to write on this machine, so
    /// the difference from a trusted person isn't only what the agent is told.
    fn anyone_may_assign_work(&mut self, tool: &Tool, agent: &str) -> Result<bool> {
        let question = format!("Should anyone in {} be able to assign {agent} work?", tool.title);
        let hint = format!("People you didn't name can hand {agent} work and nothing more. For them it works only through sandboxed hands, with no shell on this computer, and it won't change how it behaves.");
        self.asking.yes(&Question { title: "Work from anyone", question: &question, hint: &hint }, true)
    }

    /// Someone is trusted as whatever the source will report them as. Where
    /// that is an id and not their address, the id is looked up now: a person
    /// the tool doesn't know would be trusted on paper and never recognized.
    fn trust(&mut self, tool: &Tool, agent: &str, address: &str, recognized_by: RecognizedBy) -> Result<usize> {
        let reported_as = match recognized_by {
            RecognizedBy::Address => vec![address.to_string()],
            RecognizedBy::PersonId => self.doing.person_ids(tool.name, address),
        };
        if reported_as.is_empty() {
            self.asking.trouble(&format!("Couldn't find {address} in {}, so {agent} wouldn't recognize them. Nobody was added.", tool.title))?;
            return Ok(0);
        }

        let suggested = name_from(address);
        let question = format!("What should {agent} call them?");
        let name_question = Question { title: "Their name", question: &question, hint: "How the agent refers to this person." };
        let name = self.asking.string(&name_question, Some(&suggested))?.unwrap_or(suggested);

        for reported in &reported_as {
            self.doing.trust(reported, &name)?;
        }
        self.asking.done(&format!("{agent} trusts {name} ({address})."))?;
        Ok(1)
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

const BASECAMP_INBOX_NOTE: &str = "That is an item from your Basecamp inbox, not what they wrote. Its lines are, in order: why it reached you, the kind of event, the project (bucket) id, the recording id, and the event's details when it has any. Read the recording with your Basecamp tools before you do anything, and answer where it was said, with those tools.";

/// How an agent hears things in Basecamp. It has no notifications — Basecamp
/// refuses an agent everything it hasn't opened to agents, the "Hey!" menu
/// included — and gets an inbox of its own instead: every event that reached
/// it, with why (mentioned, assigned, pinged, subscribed…). The inbox is
/// read from a position, and an item is an event, not what someone wrote: it
/// says who, in which project and on which recording, so the thread is told
/// to read the recording before it acts. People are reported by id, which
/// is why the trusted ones were looked up when they were named. There is no
/// single call that answers, so the thread answers with Basecamp's own
/// tools. `anyone` lets people who aren't trusted hand it work; Basecamp
/// already decides who can reach an agent at all — the people in the
/// projects it was added to.
fn basecamp_source(profile: &str, watches: bool, anyone: bool) -> Source {
    Source {
        server: "basecamp".to_string(),
        watch: Call { tool: "basecamp_eventfeed".to_string(), arguments: json!({ "action": "poll_inbox", "params": {} }) },
        every_seconds: if watches { 900 } else { 60 },
        items: "/items".to_string(),
        id: Pointers::One("/addressing_id".to_string()),
        conversation: "/event/recording_id".to_string(),
        sender: "/event/creator_id".to_string(),
        sender_name: None,
        anyone,
        text: Pointers::Several(words(&["/reason", "/event/kind", "/event/bucket_id", "/event/recording_id", "/event/details"])),
        reply: None,
        cursor: Some(Cursor { from: "/position".to_string(), into: "/params/position".to_string() }),
        note: Some(BASECAMP_INBOX_NOTE.to_string()),
        trigger: if watches {
            Some(Trigger {
                command: "basecamp".to_string(),
                args: words(&["--profile", profile, "watch", "--json"]),
                env: own_tool_config(),
            })
        } else {
            None
        },
    }
}

/// The environment a tool runs in when the agent has a profile of its own
/// there: its config lives in the agent's folder, not the person's.
fn own_tool_config() -> BTreeMap<String, String> {
    BTreeMap::from([("XDG_CONFIG_HOME".to_string(), config::HER_TOOLS.to_string())])
}

/// A name to suggest for an address: marta.k@example.com is probably Marta.
fn name_from(address: &str) -> String {
    let first = address.split(['@', '.', '+', '_', '-']).next().unwrap_or_default();
    let mut letters = first.chars();
    match letters.next() {
        Some(initial) => initial.to_uppercase().chain(letters).collect(),
        None => address.to_string(),
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
        Judge::jev_key_works(key, url.as_deref())
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

    /// Asked as the person running setup, who can see people; an agent
    /// can't. Every account of every profile they have is looked through:
    /// which account the agent belongs to is the command line's to know, the
    /// default profile may not be the one that can see it, and an id is only
    /// ever one person's.
    fn person_ids(&mut self, tool: &str, address: &str) -> Vec<String> {
        let mut profiles: Vec<Option<String>> = json_from(tool, &["profile", "list", "--json"])
            .and_then(|listed| listed["data"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|it| it["name"].as_str().map(|name| Some(name.to_string())))
            .collect();
        if profiles.is_empty() {
            profiles.push(None);
        }

        let mut ids = Vec::new();
        for profile in &profiles {
            let through: Vec<&str> = profile.as_deref().map(|it| vec!["--profile", it]).unwrap_or_default();
            for account in self.accounts(tool, profile.as_deref()).unwrap_or_default() {
                let listing = [through.as_slice(), &["people", "list", "--all", "--json", "--account", &account.id]].concat();
                let people = json_from(tool, &listing).and_then(|listed| listed["data"].as_array().cloned()).unwrap_or_default();
                for person in people.iter().filter(|it| it["email_address"].as_str().is_some_and(|it| it.eq_ignore_ascii_case(address))) {
                    let id = person["id"].to_string().trim_matches('"').to_string();
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
        ids
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

    fn can_connect_agents(&mut self, tool: &str) -> bool {
        Command::new(tool)
            .args(["auth", "agent", "connect", "--help"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|it| it.success())
    }

    /// It runs with the agent's own config folder, so the profile it creates
    /// — and the default it may become — are the agent's and not the person's.
    fn connect_agent(&mut self, tool: &str, profile: &str, agent: &str) -> Result<()> {
        std::fs::create_dir_all(paths::tools_config_home())?;
        let status = Command::new(tool)
            .args(["auth", "agent", "connect", "--profile", profile, "--software-name", agent])
            .envs(config::environment(&own_tool_config()))
            .status()
            .with_context(|| format!("could not run {tool}"))?;

        if status.success() {
            Ok(())
        } else {
            bail!("{tool} didn't finish connecting")
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

    fn add_server(&mut self, name: &str, command: &[String], env: BTreeMap<String, String>) -> Result<()> {
        mcp_cli::register(name, command, env)
    }

    fn add_source(&mut self, name: &str, source: Source) -> Result<()> {
        let mut config = Config::load()?;
        config.sources.insert(name.to_string(), source);
        config.save()
    }

    fn trust(&mut self, address: &str, name: &str) -> Result<()> {
        let mut config = Config::load()?;
        config.people.insert(address.to_string(), name.to_string());
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
        trusted: Vec<(String, String)>,
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

        fn add_server(&mut self, name: &str, command: &[String], env: BTreeMap<String, String>) -> Result<()> {
            self.servers.push((name.to_string(), command.to_vec()));
            self.server_env.push(env);
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
        assert_eq!(doing.trusted, [("me@basecamp.example.com".to_string(), "Me".to_string())], "enter trusts whoever runs setup");
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
