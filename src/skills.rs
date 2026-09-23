//! Skills: what a session knows about a tool before it calls it.
//!
//! Claude Code reads skills from the folder it is configured with, which is
//! Anna's own, so a skill she has is a skill every thread has — and it costs
//! nothing until the session reads it.
//!
//! Some servers offer a gateway tool: one tool for a whole area, with an
//! `action` to pick what it does and a `describe` action to ask what an
//! action takes. Its schema names no parameters, so every session guesses
//! once, is refused, describes, and calls again — in every conversation, for
//! every action. A server that can describe itself is asked once instead,
//! when it is added, and the answers are written as a skill per tool.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::McpServer;
use crate::mcp::{self, Server};
use crate::paths;

const DESCRIBE: &str = "describe";

pub fn dir() -> PathBuf {
    paths::claude_config_home().join("skills")
}

/// A skill for every tool of this server that can describe its own actions.
/// Returns what was written. A tool that can't be described is left alone:
/// its schema already says what it takes.
pub fn write_for_server(server_name: &str, settings: &McpServer) -> Result<Vec<String>> {
    let mut server = Server::start(settings)?;
    let tools = server.tools()?;
    let mut written = Vec::new();

    for tool in &tools {
        let remote_name = tool["name"].as_str().unwrap_or_default().to_string();
        let actions = actions_of(tool);
        if actions.is_empty() {
            continue;
        }

        let described: Vec<(String, Value)> = actions
            .iter()
            .filter_map(|action| {
                let asked = json!({ "action": DESCRIBE, "params": { "action": action } });
                let answer = server.call(&remote_name, &asked).ok()?;
                Some((action.clone(), serde_json::from_str(&answer).unwrap_or(Value::String(answer))))
            })
            .collect();

        if !described.is_empty() {
            let offered = mcp::offered_name(server_name, &remote_name);
            let skill = skill_named(&offered);
            fs::create_dir_all(&skill)?;
            fs::write(skill.join("SKILL.md"), written_skill(&offered, tool, &described))?;
            written.push(offered);
        }
    }
    Ok(written)
}

/// The actions a gateway tool offers, which is nothing unless it offers to
/// describe them.
fn actions_of(tool: &Value) -> Vec<String> {
    let listed = tool["inputSchema"]["properties"]["action"]["enum"].as_array();
    let actions: Vec<String> = listed
        .map(|it| it.iter().filter_map(|action| action.as_str().map(String::from)).collect())
        .unwrap_or_default();

    if actions.iter().any(|it| it == DESCRIBE) {
        actions.into_iter().filter(|it| it != DESCRIBE).collect()
    } else {
        Vec::new()
    }
}

fn skill_named(offered: &str) -> PathBuf {
    dir().join(offered.replace('_', "-"))
}

fn written_skill(offered: &str, tool: &Value, described: &[(String, Value)]) -> String {
    let what_it_is = tool["description"].as_str().unwrap_or_default().lines().next().unwrap_or_default();
    let actions: Vec<String> = described.iter().map(|(action, description)| one_action(action, description)).collect();

    format!(
        "---\nname: {}\ndescription: What every action of the {offered} tool takes. {what_it_is} Read it before the first {offered} call in a conversation, rather than guessing the parameters.\n---\n\n\
         # {offered}\n\n\
         Call it as `{{\"action\": \"<action>\", \"params\": {{…}}}}`. Everything an action takes goes in `params`, path parts and body fields alike. A `*` marks what is required. `{{\"action\": \"describe\", \"params\": {{\"action\": \"<action>\"}}}}` gives one action's full schema when this isn't enough.\n\n{}\n",
        skill_named(offered).file_name().unwrap_or_default().to_string_lossy(),
        actions.join("\n")
    )
}

/// One action as a line someone can call it from, with what it takes under
/// it. A description in a shape this doesn't know is kept as it came.
fn one_action(action: &str, description: &Value) -> String {
    let Some(summary) = description["summary"].as_str() else {
        return format!("- {action}\n  {}", description.to_string().replace('\n', " "));
    };

    let mut line = format!("- {action}: {summary}");
    if description["readonly"].as_bool().unwrap_or(false) {
        line.push_str(" (reads only)");
    }
    if description["paginated"].as_bool().unwrap_or(false) {
        line.push_str(" (paginated)");
    }

    let takes = [named_params(description), body_params(description)].concat();
    if takes.is_empty() {
        format!("{line}\n  params: none")
    } else {
        format!("{line}\n  params: {}", takes.join(", "))
    }
}

fn named_params(description: &Value) -> Vec<String> {
    let Some(params) = description["params"].as_array() else {
        return Vec::new();
    };
    params
        .iter()
        .filter_map(|param| {
            let name = param["name"].as_str()?;
            Some(named(name, param["required"].as_bool().unwrap_or(false), &param["schema"]))
        })
        .collect()
}

fn body_params(description: &Value) -> Vec<String> {
    let Some(properties) = description["body"]["properties"].as_object() else {
        return Vec::new();
    };
    let required = description["body"]["required"].as_array().cloned().unwrap_or_default();
    properties
        .iter()
        .map(|(name, schema)| named(name, required.iter().any(|it| it.as_str() == Some(name)), schema))
        .collect()
}

fn named(name: &str, required: bool, schema: &Value) -> String {
    let kind = schema["type"].as_str().unwrap_or("value");
    let mark = if required { "*" } else { "" };
    match schema["description"].as_str() {
        Some(what) => format!("{name}{mark} ({kind}: {what})"),
        None => format!("{name}{mark} ({kind})"),
    }
}

/// A skill of the person's own, copied in so Anna has it too.
pub fn add(from: &Path) -> Result<String> {
    let folder = from.canonicalize().with_context(|| format!("{} does not exist", from.display()))?;
    if !folder.join("SKILL.md").is_file() {
        bail!("{} has no SKILL.md in it", folder.display());
    }
    let name = folder.file_name().context("that folder has no name")?.to_string_lossy().to_string();

    let into = dir().join(&name);
    let _ = fs::remove_dir_all(&into);
    copy_folder(&folder, &into)?;
    Ok(name)
}

fn copy_folder(from: &Path, into: &Path) -> Result<()> {
    fs::create_dir_all(into)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let to = into.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_folder(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

pub fn list() -> Result<()> {
    let mut names = named_skills()?;
    names.sort();
    if names.is_empty() {
        println!("No skills yet. They are written for you when you add an MCP server, and `anna skill add <folder>` takes one of your own.");
    }
    for name in names {
        match description_of(&dir().join(&name)) {
            Some(description) => println!("{name}: {description}"),
            None => println!("{name}"),
        }
    }
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    let skill = dir().join(name);
    if !skill.join("SKILL.md").is_file() {
        bail!("there is no skill called {name}");
    }
    fs::remove_dir_all(skill)?;
    println!("Removed {name}.");
    Ok(())
}

fn named_skills() -> Result<Vec<String>> {
    let Ok(entries) = fs::read_dir(dir()) else {
        return Ok(Vec::new());
    };
    Ok(entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().join("SKILL.md").is_file())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect())
}

/// The one line the frontmatter says a skill is for, read the way Claude
/// Code reads it: the `description` of the block the file opens with.
fn description_of(skill: &Path) -> Option<String> {
    let written = fs::read_to_string(skill.join("SKILL.md")).ok()?;
    written
        .strip_prefix("---")?
        .split("\n---")
        .next()?
        .lines()
        .find_map(|line| line.strip_prefix("description:"))
        .map(|it| it.trim().to_string())
}

/// Every skill, copied where a sandboxed session can read it: its profile is
/// the only folder of Anna's it has.
pub fn copy_into_profile(profile: &Path) -> Result<()> {
    if dir().is_dir() {
        copy_folder(&dir(), &profile.join("skills"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn description(action: &str) -> Value {
        json!({
            "action": action,
            "method": "GET",
            "path": "/todos/{todoId}",
            "summary": "Get a single todo by id",
            "readonly": true,
            "paginated": false,
            "params": [{ "name": "todoId", "in": "path", "required": true, "schema": { "type": "integer" } }],
        })
    }

    #[test]
    fn only_a_tool_that_can_describe_its_actions_gets_a_skill() {
        let gateway = json!({ "inputSchema": { "properties": { "action": { "enum": ["get_todo", "create_todo", "describe"] } } } });
        assert_eq!(actions_of(&gateway), ["get_todo", "create_todo"]);

        let plain = json!({ "inputSchema": { "properties": { "query": { "type": "string" } } } });
        assert!(actions_of(&plain).is_empty());
        let no_describing = json!({ "inputSchema": { "properties": { "action": { "enum": ["get_todo"] } } } });
        assert!(actions_of(&no_describing).is_empty(), "without describe there is nothing to ask");
    }

    #[test]
    fn an_action_is_written_as_the_line_it_is_called_from() {
        let with_body = json!({
            "summary": "Create a new todo in a todolist",
            "params": [{ "name": "todolistId", "required": true, "schema": { "type": "integer" } }],
            "body": { "properties": { "content": { "type": "string" }, "due_on": { "type": "string" } }, "required": ["content"] },
        });

        assert_eq!(one_action("get_todo", &description("get_todo")), "- get_todo: Get a single todo by id (reads only)\n  params: todoId* (integer)");
        assert_eq!(
            one_action("create_todo", &with_body),
            "- create_todo: Create a new todo in a todolist\n  params: todolistId* (integer), content* (string), due_on (string)"
        );
        assert!(one_action("odd", &json!({ "whatever": true })).starts_with("- odd\n  {\"whatever\":true}"));
    }

    #[test]
    fn a_skill_says_what_it_is_for_in_its_frontmatter() {
        let tool = json!({ "name": "basecamp_todos", "description": "Todos, todolists and todosets.\n\nGateway tool." });
        let written = written_skill("basecamp_todos", &tool, &[("get_todo".to_string(), description("get_todo"))]);

        assert!(written.starts_with("---\nname: basecamp-todos\ndescription: What every action of the basecamp_todos tool takes. Todos, todolists and todosets."));
        assert!(written.contains("\n---\n\n# basecamp_todos"));
        assert!(written.contains("- get_todo: Get a single todo by id"));
        assert_eq!(description_of_written(&written).unwrap().starts_with("What every action"), true);
    }

    fn description_of_written(written: &str) -> Option<String> {
        let skill = std::env::temp_dir().join(format!("anna-skill-{}", std::process::id()));
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), written).unwrap();
        let found = description_of(&skill);
        fs::remove_dir_all(skill).unwrap();
        found
    }
}
