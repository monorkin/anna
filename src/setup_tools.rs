//! The part of the setup walk that gives the agent a tool: as an agent of
//! its own, through one of the person's profiles, or as the person — and,
//! with it, who it trusts there.

use anyhow::Result;
use std::collections::BTreeMap;

use crate::prompt::Question;
use crate::setup::{Account, ActsAs, GoOn, RecognizedBy, Tool, Wizard, name_from, own_tool_config, profile_name, words};
use crate::setup_basecamp;

impl Wizard<'_> {
    pub fn tool(&mut self, tool: &Tool, agent: &str) -> Result<()> {
        let question = format!("Do you want to allow {agent} access to {}?", tool.title);
        let hint = format!("This will allow {agent} to access {}; you'll be able to work and talk with them like a colleague.", tool.title);
        if !self.asking.yes(&Question { title: tool.title, question: &question, hint: &hint }, false)? {
            return self.asking.done(&format!("{agent} stays out of {}.", tool.title));
        }

        if tool.can_be_its_own_agent && self.wants_an_agent_profile(tool, agent)? {
            self.as_an_agent(tool, agent)
        } else if tool.name == "basecamp" {
            self.as_a_profile(tool, agent)
        } else {
            self.as_you(tool, agent)
        }
    }

    /// Basecamp through one of the command line's profiles: the person
    /// picks which, and the command line has already done the logging in.
    /// A profile that is the person's own gets the agent the tools and no
    /// ears. One made for the agent — a user of its own in Basecamp — is
    /// the agent's, and then it listens there, the way a person would.
    fn as_a_profile(&mut self, tool: &Tool, agent: &str) -> Result<()> {
        let profiles = self.doing.profiles(tool.name);
        let profile = match profiles.len() {
            0 => None,
            1 => Some(profiles[0].name.clone()),
            _ => {
                let names: Vec<String> = profiles.iter().map(|it| it.name.clone()).collect();
                let hint = format!("The profiles your {} command line is logged in to. To give {agent} a user of its own, log a profile in as that user first: {} profile create <name>, then {} auth login -P <name>.", tool.program, tool.program, tool.program);
                let question = Question { title: "Profile", question: "Which profile should it use?", hint: &hint };
                Some(names[self.asking.choice(&question, &names)?].clone())
            }
        };

        let who = self.doing.identity_of(tool.name, profile.as_deref()).unwrap_or_else(|| format!("your {} login", tool.title));
        let yours = self.doing.logged_in_as(tool.name).contains(&who);
        let question = format!("Do you want to allow {agent} to use {} as {who}?", tool.title);
        let hint = format!("{agent} will be able to perform all actions as if it was {who}.");
        if !self.asking.yes(&Question { title: "Confirm", question: &question, hint: &hint }, false)? {
            return self.asking.done(&format!("{agent} stays out of {}.", tool.title));
        }
        let Some(account) = self.pick_account(tool, profile.as_deref())? else {
            return Ok(());
        };

        let its_own = !yours && {
            let question = format!("Is {who} {agent}'s own user, so it should listen there?");
            let hint = format!("Say yes only if that user is {agent}'s and nobody else's: everything addressed to it will wake {agent}, and {agent} will answer as it.");
            self.asking.yes(&Question { title: "Listen", question: &question, hint: &hint }, false)?
        };

        let mut command = vec![tool.program.to_string()];
        if let Some(profile) = &profile {
            command.extend(words(&["--profile", profile]));
        }
        command.extend(words(&["mcp", "--account", &account.id]));
        // A user that isn't hers acts in someone else's name
        let acts_as = if its_own { ActsAs::Agent } else { ActsAs::Person };
        if let Err(error) = self.doing.add_server(tool.name, &command, BTreeMap::new(), acts_as) {
            return self.asking.trouble(&format!("Couldn't add the {} server: {error:#}", tool.title));
        }
        self.asking.done(&format!("{agent} can use {} as {who}.", tool.title))?;
        // A user that isn't an admin sees everyone else's address redacted,
        // so what her notifications report about a sender is their id
        let recognized_by = if its_own { RecognizedBy::PersonId } else { RecognizedBy::Address };
        let trusted = self.trusted_people(tool, agent, recognized_by)?;
        if its_own {
            let anyone = self.anyone_may_assign_work(tool, agent)?;
            let watches = self.doing.can_watch(tool.name);
            self.doing.add_source(tool.name, setup_basecamp::notifications_source(profile.as_deref(), &account.id, watches, anyone))?;
            self.asking.done(&format!("{agent} listens in {} as {who}.", tool.title))?;
            self.say_who_is_heard(tool, agent, trusted, anyone)
        } else {
            self.asking.note(&format!("  {agent} gets the tools only and won't listen there, or it would answer your colleagues in your name."))
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
        if let Err(error) = self.doing.add_server(tool.name, &command, own_tool_config(), ActsAs::Agent) {
            return self.asking.trouble(&format!("Couldn't add the {} server: {error:#}", tool.title));
        }

        let trusted = self.trusted_people(tool, agent, RecognizedBy::PersonId)?;
        let anyone = self.anyone_may_assign_work(tool, agent)?;
        self.doing.add_source(tool.name, setup_basecamp::agent_source(&profile, watches, anyone))?;

        self.asking.done(&format!("{agent} is in {} under its own profile.", tool.title))?;
        self.say_who_is_heard(tool, agent, trusted, anyone)
    }

    fn say_who_is_heard(&mut self, tool: &Tool, agent: &str, trusted: usize, anyone: bool) -> Result<()> {
        match (trusted, anyone) {
            (0, false) => self.asking.trouble(&format!("Nobody is trusted and nobody else may assign work, so {agent} won't act on anything in {}. Run setup again to change that.", tool.title))?,
            (_, false) => self.asking.done(&format!("{agent} acts only on the people you named, and ignores everyone else."))?,
            (_, true) => self.asking.done(&format!("Anyone who can reach {agent} there can hand it work; only the people you named can direct it."))?,
        }
        if self.doing.can_watch(tool.name) {
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
        if let Err(error) = self.doing.add_server(tool.name, &command, BTreeMap::new(), ActsAs::Person) {
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
}
