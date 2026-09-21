//! Reading a source: turning what a server's watch tool answers into
//! messages, remembering which ones were already seen, and answering back
//! into the conversation a message came from.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{Call, Pointers, Source};
use crate::conversation::{Conversation, Origin};
use crate::mcp::Catalog;
use crate::paths;

const LONGEST_KEY_PART: usize = 32;

#[derive(Debug, PartialEq)]
pub struct Message {
    pub id: String,
    pub conversation: String,
    pub sender: String,
    pub text: String,
}

pub fn messages_in(source: &Source, answer: &str) -> Result<Vec<Message>> {
    let answer: Value = serde_json::from_str(answer).context("the watch tool did not answer with JSON")?;
    let items = answer
        .pointer(&source.items)
        .and_then(Value::as_array)
        .with_context(|| format!("there is no list at {} in the watch tool's answer", source.items))?;

    Ok(items
        .iter()
        .filter_map(|item| {
            Some(Message {
                id: all_at(item, &source.id, ":")?,
                conversation: text_at(item, &source.conversation)?,
                sender: text_at(item, &source.sender)?,
                text: all_at(item, &source.text, "\n\n")?,
            })
        })
        .collect())
}

/// The values at every pointer, joined. A pointer that finds nothing is
/// skipped, as long as one of them finds something.
fn all_at(item: &Value, pointers: &Pointers, separator: &str) -> Option<String> {
    let found: Vec<String> = pointers.each().into_iter().filter_map(|it| text_at(item, it)).collect();
    if found.is_empty() {
        None
    } else {
        Some(found.join(separator))
    }
}

fn text_at(item: &Value, pointer: &str) -> Option<String> {
    match item.pointer(pointer)? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The ids of messages already handled, kept on disk so a restart doesn't
/// answer everything again.
pub struct Seen {
    path: PathBuf,
    ids: BTreeSet<String>,
    first_look: bool,
}

impl Seen {
    pub fn load(source_name: &str) -> Seen {
        Seen::load_from(paths::source_dir(source_name).join("seen.json"))
    }

    /// Whether this message is one to act on. Everything found on the very
    /// first look at a source is only remembered: that is the backlog from
    /// before Anna was listening, not something anyone asked her.
    pub fn is_new(&mut self, id: &str) -> bool {
        self.ids.insert(id.to_string()) && !self.first_look
    }

    pub fn save(&mut self) -> Result<()> {
        self.first_look = false;
        if let Some(directory) = self.path.parent() {
            fs::create_dir_all(directory)?;
        }
        fs::write(&self.path, serde_json::to_string(&self.ids)?)?;
        Ok(())
    }

    fn load_from(path: PathBuf) -> Seen {
        match fs::read_to_string(&path).ok().and_then(|it| serde_json::from_str(&it).ok()) {
            Some(ids) => Seen { path, ids, first_look: false },
            None => Seen { path, ids: BTreeSet::new(), first_look: true },
        }
    }
}

/// A conversation on a source. Saying something is the source's reply call
/// with the conversation and the text filled in — when the source has one.
/// Where answering isn't a single call, there is nothing generic to say it
/// with, and the thread is told to use the server's own tools.
pub struct Sourced {
    key: String,
    source_name: String,
    conversation: String,
    server: String,
    reply: Option<Call>,
    catalog: Arc<Catalog>,
}

impl Sourced {
    pub fn new(source_name: &str, source: &Source, conversation: &str, catalog: Arc<Catalog>) -> Sourced {
        Sourced {
            key: format!("{source_name}-{}", key_part(conversation)),
            source_name: source_name.to_string(),
            conversation: conversation.to_string(),
            server: source.server.clone(),
            reply: source.reply.clone(),
            catalog,
        }
    }
}

impl Conversation for Sourced {
    fn key(&self) -> &str {
        &self.key
    }

    fn origin(&self) -> Origin {
        Origin {
            source: self.source_name.clone(),
            conversation: self.conversation.clone(),
        }
    }

    fn say(&self, text: &str) -> Result<()> {
        match &self.reply {
            Some(reply) => {
                let arguments = filled(&reply.arguments, &self.conversation, text);
                self.catalog.call(&self.server, &reply.tool, &arguments)?;
                Ok(())
            }
            None => bail!("there is no single way to reply on {}", self.server),
        }
    }

    fn answered_otherwise(&self) -> Option<String> {
        match self.reply {
            Some(_) => None,
            None => Some(format!(
                "There is no reply tool in this conversation. Answer where the message came from, using the {} tools and the link in the message. Anything you write to a person there is held to the same style as a reply.",
                self.server
            )),
        }
    }
}

/// The conversation's id, made safe for a folder name and short enough for a
/// socket path, which is capped at 108 bytes. A long id — Basecamp's signed
/// ids run to hundreds of characters — keeps its start and gets a hash of the
/// whole, so the same conversation always lands on the same thread.
fn key_part(conversation: &str) -> String {
    let safe: String = conversation
        .chars()
        .map(|it| if it.is_ascii_alphanumeric() { it } else { '-' })
        .collect();

    if safe.len() <= LONGEST_KEY_PART {
        safe
    } else {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in conversation.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{}-{hash:016x}", &safe[..12])
    }
}

fn filled(template: &Value, conversation: &str, text: &str) -> Value {
    match template {
        Value::String(it) if it == "{conversation}" => number_or_text(conversation),
        Value::String(it) => Value::from(it.replace("{conversation}", conversation).replace("{text}", text)),
        Value::Array(items) => items.iter().map(|it| filled(it, conversation, text)).collect(),
        Value::Object(fields) => fields
            .iter()
            .map(|(name, value)| (name.clone(), filled(value, conversation, text)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
        other => other.clone(),
    }
}

/// An id that came in as a number goes back out as one; servers that take a
/// numeric id usually refuse a string.
fn number_or_text(value: &str) -> Value {
    match value.parse::<i64>() {
        Ok(number) => Value::from(number),
        Err(_) => Value::from(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source() -> Source {
        serde_json::from_value(json!({
            "server": "basecamp",
            "watch": { "tool": "my_notifications" },
            "items": "/unread",
            "id": "/id",
            "conversation": "/recording/id",
            "sender": "/creator/email",
            "text": "/content",
            "reply": { "tool": "create_comment", "arguments": { "recording_id": "{conversation}", "content": "{text}" } },
        }))
        .unwrap()
    }

    #[test]
    fn messages_are_picked_out_of_the_answer_by_pointer() {
        let answer = json!({ "unread": [
            { "id": 11, "recording": { "id": 900 }, "creator": { "email": "marta@example.com" }, "content": "Can you look at this?" },
            { "id": 12, "recording": { "id": 901 }, "creator": {}, "content": "No sender, so skipped" },
        ]});

        assert_eq!(
            messages_in(&source(), &answer.to_string()).unwrap(),
            [Message {
                id: "11".to_string(),
                conversation: "900".to_string(),
                sender: "marta@example.com".to_string(),
                text: "Can you look at this?".to_string(),
            }]
        );
        assert_eq!(source().every_seconds, 60);
        assert!(messages_in(&source(), "not json").is_err());
        assert!(messages_in(&source(), "{}").is_err());
    }

    #[test]
    fn a_basecamp_notification_is_new_again_when_its_thread_gets_another_comment() {
        let basecamp: Source = serde_json::from_value(json!({
            "server": "basecamp",
            "watch": { "tool": "basecamp_account", "arguments": { "action": "get_my_notifications" } },
            "trigger": { "command": "basecamp", "args": ["watch", "--json"] },
            "items": "/unreads",
            "id": ["/id", "/unread_at"],
            "conversation": "/readable_sgid",
            "sender": "/creator/email_address",
            "text": ["/title", "/content_excerpt", "/app_url"],
        }))
        .unwrap();
        assert_eq!(basecamp.reply, None);

        let reading = |unread_at: &str| {
            json!({ "unreads": [{
                "id": 501,
                "unread_at": unread_at,
                "readable_sgid": "BAh7CEkiCG".repeat(20),
                "creator": { "email_address": "marta@example.com" },
                "title": "Re: Deploy checklist",
                "content_excerpt": "Can you take the staging part?",
                "app_url": "https://3.basecamp.com/1/buckets/2/messages/3",
            }]})
            .to_string()
        };

        let first = messages_in(&basecamp, &reading("2026-09-21T08:00:00Z")).unwrap().remove(0);
        let second = messages_in(&basecamp, &reading("2026-09-21T09:30:00Z")).unwrap().remove(0);
        assert_eq!(first.id, "501:2026-09-21T08:00:00Z");
        assert_ne!(first.id, second.id);
        assert_eq!(first.conversation, second.conversation);
        assert_eq!(
            first.text,
            "Re: Deploy checklist\n\nCan you take the staging part?\n\nhttps://3.basecamp.com/1/buckets/2/messages/3"
        );
    }

    #[test]
    fn long_conversation_ids_are_shortened_the_same_way_every_time() {
        let sgid = "BAh7CEkiCG--".repeat(30);

        assert_eq!(key_part("900"), "900");
        assert_eq!(key_part("thread/7"), "thread-7");
        assert_eq!(key_part(&sgid), key_part(&sgid));
        assert_eq!(key_part(&sgid).len(), 12 + 1 + 16);
        assert_ne!(key_part(&sgid), key_part(&format!("{sgid}x")));
    }

    #[test]
    fn the_backlog_is_remembered_but_only_later_messages_are_new() {
        let path = std::env::temp_dir().join(format!("anna-seen-{}/seen.json", std::process::id()));
        let mut seen = Seen::load_from(path.clone());
        assert!(!seen.is_new("old-1"));
        seen.save().unwrap();

        assert!(seen.is_new("new-1"));
        assert!(!seen.is_new("new-1"));
        seen.save().unwrap();

        let mut reloaded = Seen::load_from(path.clone());
        assert!(!reloaded.is_new("old-1"));
        assert!(!reloaded.is_new("new-1"));
        assert!(reloaded.is_new("new-2"));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn replies_fill_in_the_conversation_and_the_text() {
        let arguments = filled(&source().reply.unwrap().arguments, "900", "On it.");
        assert_eq!(arguments, json!({ "recording_id": 900, "content": "On it." }));

        let nested = filled(&json!({ "to": ["thread/{conversation}"], "draft": false }), "abc", "hi");
        assert_eq!(nested, json!({ "to": ["thread/abc"], "draft": false }));
    }
}
