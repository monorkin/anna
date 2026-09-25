//! What `anna setup` does to the real machine: the config, the keyring,
//! systemd, and the tools' own command lines.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use crate::config::{self, Config, Source};
use crate::judge::Judge;
use crate::mcp_cli;
use crate::paths;
use crate::secrets::{self, Kept};
use crate::service;
use crate::setup::{Account, ActsAs, Doing, Profile, own_tool_config, prose_of};

pub struct Machine;

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

    fn profiles(&mut self, tool: &str) -> Vec<Profile> {
        json_from(tool, &["profile", "list", "--json"])
            .and_then(|listed| listed["data"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter(|it| it["authenticated"].as_bool().unwrap_or(false))
            .filter_map(|it| it["name"].as_str().map(|name| Profile { name: name.to_string() }))
            .collect()
    }

    fn identity_of(&mut self, tool: &str, profile: Option<&str>) -> Option<String> {
        let mut arguments = Vec::new();
        if let Some(profile) = profile {
            arguments.extend(["--profile", profile]);
        }
        arguments.extend(["me", "--json"]);
        json_from(tool, &arguments)?["data"]["identity"]["email_address"].as_str().map(String::from)
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
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|it| it.success())
    }

    fn add_server(&mut self, name: &str, command: &[String], env: BTreeMap<String, String>, acts_as: ActsAs) -> Result<()> {
        let mut settings = mcp_cli::settings_for(command)?;
        settings.env = env;
        settings.prose = prose_of(name);
        settings.trusted_only = acts_as == ActsAs::Person;
        mcp_cli::register(name, settings)
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
    let output = Command::new(program).args(arguments).stderr(Stdio::null()).output().ok()?;
    serde_json::from_slice(&output.stdout).ok()
}
