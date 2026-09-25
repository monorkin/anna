//! Her memory, in her own hands. Katami puts what is relevant in front of
//! every turn and picks up new observations from the transcript afterwards,
//! so she already reads and grows it without asking; these are for the rest —
//! looking something up she wasn't shown, writing down a thing she was told
//! to keep, correcting one that is wrong, and retiring one that is over.
//!
//! Looking is for any turn: what it finds is hers and was in her prompt
//! before. Writing is for trusted turns only, because a memory is how she
//! behaves next time, and an untrusted turn is told not to change that on
//! anyone's say-so — the transcript review still learns from it.

use anyhow::Result;
use katami::cards;
use katami::embeddings;
use katami::memory::{Kind, Memory, NewMemory};
use katami::search;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::broker::{Tool, text_of};
use crate::conversation::Standing;

const HITS: usize = 10;

pub fn memory_tools(directory: &Path, standing: Standing) -> Vec<Box<dyn Tool>> {
    let directory = directory.to_path_buf();
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(SearchMemory { directory: directory.clone() }),
        Box::new(ShowMemory { directory: directory.clone() }),
    ];
    if standing == Standing::Trusted {
        tools.push(Box::new(Remember { directory: directory.clone() }));
        tools.push(Box::new(CorrectMemory { directory: directory.clone() }));
        tools.push(Box::new(ForgetMemory { directory }));
    }
    tools
}

pub struct SearchMemory {
    pub directory: PathBuf,
}

impl Tool for SearchMemory {
    fn name(&self) -> &str {
        "search_memory"
    }

    fn description(&self) -> &str {
        "Search what you remember, across every conversation. What matters to this message is already in front of you; use this for what isn't — a name, a decision, how something was done last time. Returns ids and titles; show_memory reads one."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"],
        })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        let query = text_of(arguments, "query")?;
        let memory = Memory::open(&self.directory)?;
        let hits = search::hybrid(&memory, query, HITS)?;
        if hits.is_empty() {
            return Ok("Nothing you remember matches that.".to_string());
        }
        let lines: Vec<String> = hits
            .iter()
            .map(|hit| {
                let stored = memory.get(hit.id)?;
                Ok(format!("{} ({}, {}): {}", stored.id, stored.kind, &stored.updated[..10], stored.title))
            })
            .collect::<Result<_>>()?;
        Ok(lines.join("\n"))
    }
}

pub struct ShowMemory {
    pub directory: PathBuf,
}

impl Tool for ShowMemory {
    fn name(&self) -> &str {
        "show_memory"
    }

    fn description(&self) -> &str {
        "Read one memory in full, by the id search_memory gave, with what it links to."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
        })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        let memory = Memory::open(&self.directory)?;
        let id = memory.resolve(text_of(arguments, "id")?)?;
        let stored = memory.get(id)?;

        let mut shown = format!("# {} ({})\n", stored.title, stored.kind);
        if let Some(entity) = &stored.entity {
            shown.push_str(&format!("about: {entity}\n"));
        }
        if stored.archived {
            shown.push_str("forgotten: yes\n");
        }
        shown.push_str(&format!("updated: {}\n\n{}", stored.updated, stored.body));
        for neighbor in memory.neighbors(id)? {
            shown.push_str(&format!("\nlinked: [[{}]] ({})", neighbor.title, neighbor.id));
        }
        Ok(shown)
    }
}

pub struct Remember {
    pub directory: PathBuf,
}

impl Tool for Remember {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "Write down something worth having next time: a decision, a preference, how a thing is done here. One fact per memory, in your own words, with a title that says what it is. Most of what happens is picked up from the conversation on its own, so this is for what you were told to keep or would otherwise lose. `about` names what it belongs to, like project:/path or person:name."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "body": { "type": "string" },
                "about": { "type": "string" },
            },
            "required": ["title", "body"],
        })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        let title = text_of(arguments, "title")?;
        let body = text_of(arguments, "body")?;
        let memory = Memory::open(&self.directory)?;
        let id = memory.add(&NewMemory {
            kind: Kind::Observation,
            entity: arguments["about"].as_str().map(String::from),
            title: title.to_string(),
            body: body.to_string(),
            links: cards::extract_links(body),
            source_session: None,
            class: None,
        })?;
        embeddings::embed_into(&memory, id, &format!("{title}\n{body}"))?;
        Ok(format!("Remembered as {id}."))
    }
}

pub struct CorrectMemory {
    pub directory: PathBuf,
}

impl Tool for CorrectMemory {
    fn name(&self) -> &str {
        "correct_memory"
    }

    fn description(&self) -> &str {
        "Rewrite a memory that is wrong or out of date, by id. Give the whole new title and body; what was there is replaced."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "title": { "type": "string" },
                "body": { "type": "string" },
            },
            "required": ["id", "title", "body"],
        })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        let title = text_of(arguments, "title")?;
        let body = text_of(arguments, "body")?;
        let memory = Memory::open(&self.directory)?;
        let id = memory.resolve(text_of(arguments, "id")?)?;
        let stored = memory.get(id)?;
        memory.update(id, title, body, stored.entity.as_deref())?;
        memory.replace_links(id, &cards::extract_links(body))?;
        embeddings::embed_into(&memory, id, &format!("{title}\n{body}"))?;
        Ok(format!("Corrected {id}."))
    }
}

pub struct ForgetMemory {
    pub directory: PathBuf,
}

impl Tool for ForgetMemory {
    fn name(&self) -> &str {
        "forget_memory"
    }

    fn description(&self) -> &str {
        "Retire a memory that no longer holds, by id, and say why. It stops being put in front of you; nothing is deleted."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string" }, "why": { "type": "string" } },
            "required": ["id", "why"],
        })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        let memory = Memory::open(&self.directory)?;
        let id = memory.resolve(text_of(arguments, "id")?)?;
        memory.archive(id, text_of(arguments, "why")?)?;
        Ok(format!("Forgotten {id}."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn she_can_look_up_write_down_correct_and_retire_what_she_remembers() {
        let directory = std::env::temp_dir().join(format!("anna-memory-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);

        let untrusted: Vec<String> = memory_tools(&directory, Standing::CanAssignWork).iter().map(|it| it.name().to_string()).collect();
        assert_eq!(untrusted, ["search_memory", "show_memory"], "an untrusted turn reads and nothing more");
        let trusted = memory_tools(&directory, Standing::Trusted);
        let tool = |name: &str| trusted.iter().find(|it| it.name() == name).unwrap();

        assert_eq!(tool("search_memory").call(&json!({ "query": "deploys" })).unwrap(), "Nothing you remember matches that.");

        let remembered = tool("remember")
            .call(&json!({ "title": "Deploys go through Kamal", "body": "Every app here deploys with Kamal; nothing is deployed by hand.", "about": "project:/home/someone/frontdesk" }))
            .unwrap();
        let id = remembered.trim_start_matches("Remembered as ").trim_end_matches('.').to_string();

        let found = tool("search_memory").call(&json!({ "query": "kamal deploy" })).unwrap();
        assert!(found.starts_with(&id) && found.ends_with("Deploys go through Kamal"), "{found}");
        let shown = tool("show_memory").call(&json!({ "id": id })).unwrap();
        assert!(shown.contains("# Deploys go through Kamal (observation)") && shown.contains("about: project:/home/someone/frontdesk"));

        tool("correct_memory").call(&json!({ "id": id, "title": "Deploys go through Kamal 2", "body": "Kamal 2 since March; nothing is deployed by hand." })).unwrap();
        let shown = tool("show_memory").call(&json!({ "id": id })).unwrap();
        assert!(shown.contains("Kamal 2 since March") && shown.contains("about: project:/home/someone/frontdesk"), "what it is about stays");

        assert_eq!(tool("forget_memory").call(&json!({ "id": id, "why": "Kamal was dropped" })).unwrap(), format!("Forgotten {id}."));
        assert!(tool("show_memory").call(&json!({ "id": id })).unwrap().contains("forgotten: yes"));
        assert!(tool("remember").call(&json!({ "title": "x" })).unwrap_err().to_string().contains("body is required"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
