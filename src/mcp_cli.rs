//! `anna mcp …`: which MCP servers Anna has, and which of their arguments
//! carry prose for people.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;

use crate::config::{Config, McpServer};
use crate::control;
use crate::mcp::{self, Server};

pub fn add(name: &str, command: &[String]) -> Result<()> {
    register(name, command, BTreeMap::new())?;
    println!("Added {name}.");
    list_one(name, &Config::load()?.mcp_servers[name])?;
    say_if_she_has_to_restart();
    Ok(())
}

/// She reads her config when she starts. Saying "removed" while the running
/// Anna goes on offering the server would be a lie about what is protected.
fn say_if_she_has_to_restart() {
    if control::is_running() {
        println!("Anna is running with the config she started with. `anna stop` and `anna start` for this to take effect.");
    }
}

/// Adding without a word, for setup, which has its own way of saying things.
pub fn register(name: &str, command: &[String], env: BTreeMap<String, String>) -> Result<()> {
    register_with_prose(name, command, env, BTreeMap::new())
}

/// `prose` is what setup already knows carries text for people on this
/// server — a tool name to its arguments, an argument being a name or a
/// JSON pointer into a gateway tool's params.
pub fn register_with_prose(name: &str, command: &[String], env: BTreeMap<String, String>, prose: BTreeMap<String, Vec<String>>) -> Result<()> {
    let (program, args) = command.split_first().context("give the command that starts the server after --")?;
    let mut settings = McpServer {
        command: program.clone(),
        args: args.to_vec(),
        env,
        prose,
        fingerprint: None,
    };

    let mut config = Config::load()?;
    if let Some(existing) = config.mcp_servers.get(name) {
        for (tool, arguments) in &existing.prose {
            let marked = settings.prose.entry(tool.clone()).or_default();
            for argument in arguments {
                if !marked.contains(argument) {
                    marked.push(argument.clone());
                }
            }
        }
        if settings.env.is_empty() {
            settings.env = existing.env.clone();
        }
    }
    settings.fingerprint = Some(mcp::fingerprint_of(&settings).with_context(|| format!("{name} did not start"))?);
    config.mcp_servers.insert(name.to_string(), settings);
    config.save()
}

pub fn list() -> Result<()> {
    let config = Config::load()?;
    if config.mcp_servers.is_empty() {
        println!("No MCP servers yet. Add one with: anna mcp add basecamp -- basecamp mcp");
    }
    for (name, settings) in &config.mcp_servers {
        list_one(name, settings)?;
    }
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    let mut config = Config::load()?;
    if config.mcp_servers.remove(name).is_none() {
        bail!("there is no server called {name}");
    }
    config.save()?;
    println!("Removed {name}.");
    say_if_she_has_to_restart();
    Ok(())
}

pub fn mark_prose(name: &str, tool: &str, argument: &str) -> Result<()> {
    let mut config = Config::load()?;
    let settings = config
        .mcp_servers
        .get_mut(name)
        .with_context(|| format!("there is no server called {name}"))?;

    let arguments = settings.prose.entry(tool.to_string()).or_default();
    if !arguments.iter().any(|it| it == argument) {
        arguments.push(argument.to_string());
    }
    config.save()?;
    println!("{tool}'s {argument} now goes through the editor, and hands can't be granted {tool}.");
    say_if_she_has_to_restart();
    Ok(())
}

fn list_one(name: &str, settings: &McpServer) -> Result<()> {
    println!("{name}: {} {}", settings.command, settings.args.join(" "));
    match Server::start(settings).and_then(|mut server| server.tools()) {
        Ok(tools) => {
            for tool in tools {
                let tool_name = tool["name"].as_str().unwrap_or_default();
                match settings.prose.get(tool_name) {
                    Some(arguments) => println!("  {tool_name}  (prose: {})", arguments.join(", ")),
                    None => println!("  {tool_name}"),
                }
            }
        }
        Err(error) => println!("  did not start: {error:#}"),
    }
    Ok(())
}
