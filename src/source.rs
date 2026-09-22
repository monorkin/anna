//! Reading a source: turning what a server's watch tool answers into
//! messages, remembering which ones were already seen, and answering back
//! into the conversation a message came from.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{Call, Cursor, Pointers, Source};
use crate::conversation::{Conversation, Origin};
use crate::fsutil;
use crate::logs;
use crate::mcp::Catalog;
use crate::paths;

const LONGEST_KEY_PART: usize = 32;

#[derive(Debug, PartialEq)]
pub struct Message {
    pub id: String,
    pub conversation: String,
    pub sender: String,
    pub sender_name: Option<String>,
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
                sender_name: source.sender_name.as_deref().and_then(|it| text_at(item, it)),
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

/// Text and numbers as they are; anything with structure — an event's
/// details, say — as the JSON it is, since a thread reads that fine.
fn text_at(item: &Value, pointer: &str) -> Option<String> {
    match item.pointer(pointer)? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Null | Value::Bool(_) => None,
        structured => Some(structured.to_string()),
    }
}

/// Where a source that is read from a position has got to.
pub struct Position {
    path: PathBuf,
    cursor: Cursor,
}

impl Position {
    pub fn of(source_name: &str, source: &Source) -> Option<Position> {
        let cursor = source.cursor.clone()?;
        Some(Position { path: paths::source_dir(source_name).join("position"), cursor })
    }

    /// The watch arguments, with the position put where the source wants it
    /// when there is one. The folders on the way are made: `/params/position`
    /// works whether or not the arguments already have `params`.
    pub fn in_arguments(&self, arguments: &Value) -> Value {
        match fs::read_to_string(&self.path) {
            Ok(position) if !position.trim().is_empty() => put_at(arguments, &self.cursor.into, position.trim()),
            _ => arguments.clone(),
        }
    }

    /// Only once everything in the answer has been handed on: a position
    /// moved first would lose the messages of a look that then failed.
    pub fn move_to_where(&self, answer: &str) -> Result<()> {
        let answer: Value = serde_json::from_str(answer).context("the watch tool did not answer with JSON")?;
        match text_at(&answer, &self.cursor.from) {
            Some(position) => fsutil::write_private(&self.path, &position),
            None => Ok(()),
        }
    }
}

fn put_at(arguments: &Value, pointer: &str, value: &str) -> Value {
    let mut arguments = arguments.clone();
    if !arguments.is_object() {
        arguments = json!({});
    }

    let names: Vec<&str> = pointer.split('/').skip(1).collect();
    let mut place = &mut arguments;
    for name in &names[..names.len().saturating_sub(1)] {
        if !place[*name].is_object() {
            place[*name] = json!({});
        }
        place = &mut place[*name];
    }
    if let Some(last) = names.last() {
        place[*last] = Value::from(value);
    }
    arguments
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

    /// For a message that couldn't be acted on yet: it is new again at the
    /// next look.
    pub fn forget(&mut self, id: &str) {
        self.ids.remove(id);
    }

    /// Written whole and then moved into place: a crash half-way through
    /// would otherwise leave a file that reads as a first look, and the
    /// next look would pass over everything new as backlog.
    pub fn save(&mut self) -> Result<()> {
        self.first_look = false;
        if let Some(directory) = self.path.parent() {
            fs::create_dir_all(directory)?;
        }
        fsutil::write_private(&self.path, &serde_json::to_string(&self.ids)?)
    }

    fn load_from(path: PathBuf) -> Seen {
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(ids) => Seen { path, ids, first_look: false },
                Err(error) => {
                    logs::event("source.seen_unreadable", json!({ "file": path, "error": error.to_string() }));
                    Seen { path, ids: BTreeSet::new(), first_look: true }
                }
            },
            Err(_) => Seen { path, ids: BTreeSet::new(), first_look: true },
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

    // Only an id that went through unchanged can stand for itself: `room/a`
    // and `room-a` are different conversations that would otherwise share a
    // thread, its history and its standing
    if safe.len() <= LONGEST_KEY_PART && safe == conversation {
        safe
    } else {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in conversation.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{}-{hash:016x}", &safe[..safe.len().min(12)])
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
                sender_name: None,
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
        assert_eq!(key_part("thread-7"), "thread-7");
        assert!(key_part("thread/7").starts_with("thread-7-"));
        assert_ne!(key_part("thread/7"), key_part("thread-7"), "ids that only look alike once made safe are still two conversations");
        assert_ne!(key_part("thread/7"), key_part("thread.7"));
        assert_eq!(key_part(&sgid).len(), 12 + 1 + 16);
        assert_ne!(key_part(&sgid), key_part(&format!("{sgid}x")));
    }

    #[test]
    fn a_source_read_from_a_position_carries_it_into_the_next_call() {
        let inbox: Source = serde_json::from_value(json!({
            "server": "basecamp",
            "watch": { "tool": "basecamp_eventfeed", "arguments": { "action": "poll_inbox", "params": {} } },
            "items": "/items",
            "id": "/event/id",
            "conversation": "/event/recording_id",
            "sender": "/event/creator_id",
            "text": ["/reason", "/event/kind", "/event/bucket_id", "/event/recording_id", "/event/details"],
            "cursor": { "from": "/position", "into": "/params/position" },
        }))
        .unwrap();
        let answer = json!({
            "items": [{
                "addressing_id": 71, "reason": "mentioned", "addressed_at": "2026-09-21T18:00:00Z",
                "event": { "id": 9001, "kind": "comment_created", "bucket_id": 2, "creator_id": 1001, "recording_id": 3, "details": { "excerpt": "Can you look?" } },
            }],
            "position": "p-71",
        })
        .to_string();

        let message = messages_in(&inbox, &answer).unwrap().remove(0);
        assert_eq!(message.id, "9001", "one event, however many reasons it reached her for");
        assert_eq!(message.sender, "1001", "people are reported by id");
        assert_eq!(message.conversation, "3");
        assert_eq!(message.text, "mentioned\n\ncomment_created\n\n2\n\n3\n\n{\"excerpt\":\"Can you look?\"}");

        let path = std::env::temp_dir().join(format!("anna-position-{}/position", std::process::id()));
        let position = Position { path: path.clone(), cursor: inbox.cursor.clone().unwrap() };
        assert_eq!(position.in_arguments(&inbox.watch.arguments), inbox.watch.arguments, "the first call goes out as written: from now");

        position.move_to_where(&answer).unwrap();
        assert_eq!(position.in_arguments(&inbox.watch.arguments), json!({ "action": "poll_inbox", "params": { "position": "p-71" } }));
        assert_eq!(position.in_arguments(&json!({ "action": "poll_inbox" }))["params"]["position"], "p-71", "the way there is made when it's missing");

        position.move_to_where(&json!({ "items": [] }).to_string()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "p-71", "an answer without a position leaves her where she was");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
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

        reloaded.forget("new-2");
        assert!(reloaded.is_new("new-2"), "a message that couldn't be acted on yet is new again at the next look");
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
