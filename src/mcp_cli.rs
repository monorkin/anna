//! `anna mcp …`: which MCP servers Anna has, and which of their arguments
//! carry prose for people.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;

use crate::config::{Config, McpServer};
use crate::control;
use crate::mcp_server::{self, Server};
use crate::skills;

pub fn add(name: &str, command: &[String], trusted_only: bool) -> Result<()> {
    let mut settings = settings_for(command)?;
    settings.trusted_only = trusted_only;
    register(name, settings)?;
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

/// A server started by `command`, and nothing else said about it yet.
pub fn settings_for(command: &[String]) -> Result<McpServer> {
    let (program, args) = command.split_first().context("give the command that starts the server after --")?;
    Ok(McpServer {
        command: program.clone(),
        args: args.to_vec(),
        env: BTreeMap::new(),
        prose: BTreeMap::new(),
        fingerprint: None,
        trusted_only: false,
    })
}

/// Adding without a word, for setup, which has its own way of saying
/// things. What was marked as prose on a server of the same name stays
/// marked, and so does its environment when none is given.
pub fn register(name: &str, mut settings: McpServer) -> Result<()> {
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
    settings.fingerprint = Some(mcp_server::fingerprint_of(&settings).with_context(|| format!("{name} did not start"))?);
    // A tool that hides its parameters behind a describe action is asked
    // here, once, rather than by every conversation that uses it
    match skills::write_for_server(name, &settings) {
        Ok(written) if !written.is_empty() => println!("Wrote down what these tools take: {}.", written.join(", ")),
        Ok(_) => {}
        Err(error) => println!("Couldn't write down what its tools take: {error:#}"),
    }
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
