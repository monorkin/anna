//! How Basecamp is listened to, worked out against the real thing: as an
//! agent of the person's own, or as one of their users.

use serde_json::json;
use std::collections::BTreeMap;

use crate::config::{Call, Cursor, Pointers, Source, Trigger};
use crate::setup::{own_tool_config, words};

const NOTIFICATIONS_NOTE: &str = "That is the notification, not the comment: its excerpt and link are from whatever first put this on your desk, and stay the same however many comments follow. Being woken by it again means someone said something new there that you haven't seen. List the comments with your Basecamp tools and read the newest before you decide anything, every time, and answer where it was said, with those tools. Write what you post as Basecamp's editor does: plain text with <br><br> between paragraphs, <strong>, <code> and <a> where they help, <pre> for a block of code or a command with its lines as they are, and <ul><li> for a list. Not <p>: Basecamp shows adjacent paragraphs with no space between them.";

const INBOX_NOTE: &str ="That is an item from your Basecamp inbox, not what they wrote. Its lines are, in order: why it reached you, the kind of event, the project (bucket) id, the recording id, and the event's details when it has any. Read the recording with your Basecamp tools before you do anything. As an agent you can read most things but write only message board messages — comments, to-dos, cards and chat refuse you, and the tool then says \"insufficient scope\", which is not the reason — so answer with a new message on that project's message board, and name what you are answering. Write what you post as Basecamp's editor does: plain text with <br><br> between paragraphs, <strong>, <code> and <a> where they help, <pre> for a block of code or a command with its lines as they are, and <ul><li> for a list. Not <p>: Basecamp shows adjacent paragraphs with no space between them.";

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
pub fn agent_source(profile: &str, watches: bool, anyone: bool) -> Source {
    Source {
        server: "basecamp".to_string(),
        watch: Call { tool: "basecamp_eventfeed".to_string(), arguments: json!({ "action": "poll_inbox", "params": {} }) },
        every_seconds: if watches { 900 } else { 60 },
        items: "/items".to_string(),
        // One event reaches the agent once per reason — mentioned, and
        // subscribed — and is one message, not two
        id: Pointers::One("/event/id".to_string()),
        conversation: "/event/recording_id".to_string(),
        sender: "/event/creator_id".to_string(),
        sender_name: None,
        anyone,
        text: Pointers::Several(words(&["/reason", "/event/kind", "/event/bucket_id", "/event/recording_id", "/event/details"])),
        reply: None,
        cursor: Some(Cursor { from: "/position".to_string(), into: "/params/position".to_string() }),
        note: Some(INBOX_NOTE.to_string()),
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

/// How a user of the agent's own hears things in Basecamp: its
/// notifications, the "Hey!" menu, worked out against the real thing. The
/// same notification is bumped for every new comment, so what makes one
/// new is its id and when it went unread; the conversation is what the
/// notification is about — the to-do, the message — named by its
/// subscription, because a comment, a mention and an assignment on one to-do
/// are each a notification of their own; the body is an excerpt, so the
/// thread gets the title, the excerpt and the link to read the rest; and
/// there is no single call that answers, so the thread answers with
/// Basecamp's own tools. The profile lives in the person's own command
/// line config, where they logged it in, so nothing is pointed elsewhere.
pub fn notifications_source(profile: Option<&str>, account: &str, watches: bool, anyone: bool) -> Source {
    let mut through = Vec::new();
    if let Some(profile) = profile {
        through.extend(words(&["--profile", profile]));
    }
    Source {
        server: "basecamp".to_string(),
        watch: Call { tool: "basecamp_account".to_string(), arguments: json!({ "action": "get_my_notifications" }) },
        every_seconds: if watches { 900 } else { 60 },
        items: "/unreads".to_string(),
        id: Pointers::Several(words(&["/id", "/unread_at"])),
        conversation: "/subscription_url".to_string(),
        sender: "/creator/id".to_string(),
        sender_name: Some("/creator/name".to_string()),
        anyone,
        text: Pointers::Several(words(&["/title", "/content_excerpt", "/app_url"])),
        reply: None,
        cursor: None,
        note: Some(NOTIFICATIONS_NOTE.to_string()),
        trigger: if watches {
            Some(Trigger {
                command: "basecamp".to_string(),
                args: [through, words(&["watch", "--json", "--account", account])].concat(),
                env: BTreeMap::new(),
            })
        } else {
            None
        },
    }
}
