//! What a thread can do through the broker: speak in its conversation, and
//! run hands. A hand stays alive between calls so the thread can read what it
//! did and send it back to clean up; whatever is still alive when the thread
//! goes back to sleep is discarded.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::broker::Tool;
use crate::conversation::Conversation;
use crate::editor::Editor;
use crate::hand::Hand;
use crate::mcp::Catalog;

#[derive(Default)]
pub struct Hands {
    alive: Mutex<HashMap<String, Hand>>,
}

impl Hands {
    pub fn discard_all(&self) {
        for (_, hand) in self.alive.lock().unwrap().drain() {
            hand.discard();
        }
    }

    fn keep(&self, hand: Hand) {
        self.alive.lock().unwrap().insert(hand.id().to_string(), hand);
    }

    fn take(&self, id: &str) -> Result<Hand> {
        self.alive
            .lock()
            .unwrap()
            .remove(id)
            .with_context(|| format!("there is no hand {id}; it may already be dismissed or busy"))
    }
}

pub struct Reply {
    pub conversation: Arc<dyn Conversation>,
    pub editor: Arc<Editor>,
    pub spoke: Arc<AtomicBool>,
}

impl Tool for Reply {
    fn name(&self) -> &str {
        "reply"
    }

    fn description(&self) -> &str {
        "Say something to the person in this conversation. This is the only way they hear from you."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        let text = self.editor.polish(text_of(arguments, "text")?)?;
        self.conversation.say(&text)?;
        self.spoke.store(true, Ordering::Relaxed);
        Ok("Sent.".to_string())
    }
}

pub struct StartHand {
    pub hands: Arc<Hands>,
    pub catalog: Arc<Catalog>,
    pub proxy_socket: PathBuf,
}

impl Tool for StartHand {
    fn name(&self) -> &str {
        "start_hand"
    }

    fn description(&self) -> &str {
        "Start a sandboxed worker in one project folder and give it a brief. It can read and write that folder and nothing else, has no network and none of your memory, so the brief must carry everything it needs to know. Returns the hand's id and its report. Check the work, then send_back or dismiss."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": { "type": "string", "description": "Absolute path of the project folder" },
                "brief": { "type": "string" },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Names of your own tools this hand may also call. Leave it out unless the work needs them.",
                },
            },
            "required": ["project", "brief"],
        })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        let names: Vec<String> = arguments["tools"]
            .as_array()
            .map(|names| names.iter().filter_map(|it| it.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let grant = self.catalog.grant(&names)?;

        let mut hand = Hand::start(Path::new(text_of(arguments, "project")?), grant)?;
        let outcome = hand.work(text_of(arguments, "brief")?, &self.proxy_socket);
        report(&self.hands, hand, outcome)
    }
}

pub struct SendBack {
    pub hands: Arc<Hands>,
    pub proxy_socket: PathBuf,
}

impl Tool for SendBack {
    fn name(&self) -> &str {
        "send_back"
    }

    fn description(&self) -> &str {
        "Send a hand back to fix or finish its work. It remembers what it did. Returns its new report."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "hand": { "type": "string" }, "notes": { "type": "string" } },
            "required": ["hand", "notes"],
        })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        let mut hand = self.hands.take(text_of(arguments, "hand")?)?;
        let outcome = hand.work(text_of(arguments, "notes")?, &self.proxy_socket);
        report(&self.hands, hand, outcome)
    }
}

pub struct Dismiss {
    pub hands: Arc<Hands>,
}

impl Tool for Dismiss {
    fn name(&self) -> &str {
        "dismiss"
    }

    fn description(&self) -> &str {
        "Let a hand go once you accept its work or give up on it. Its changes to the project folder stay."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "hand": { "type": "string" } }, "required": ["hand"] })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        self.hands.take(text_of(arguments, "hand")?)?.discard();
        Ok("Dismissed.".to_string())
    }
}

fn report(hands: &Hands, hand: Hand, outcome: Result<String>) -> Result<String> {
    let id = hand.id().to_string();
    match outcome {
        Ok(report) => {
            hands.keep(hand);
            Ok(format!("Hand {id} reports:\n\n{report}"))
        }
        Err(error) => {
            hand.discard();
            Err(error.context(format!("hand {id} could not work and was discarded")))
        }
    }
}

fn text_of<'a>(arguments: &'a Value, name: &str) -> Result<&'a str> {
    arguments[name]
        .as_str()
        .filter(|it| !it.trim().is_empty())
        .with_context(|| format!("{name} is required"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorded {
        said: Mutex<Vec<String>>,
    }

    impl Conversation for Recorded {
        fn key(&self) -> &str {
            "recorded"
        }

        fn say(&self, text: &str) -> Result<()> {
            self.said.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    #[test]
    fn replies_land_in_the_conversation() {
        let conversation = Arc::new(Recorded { said: Mutex::new(Vec::new()) });
        let spoke = Arc::new(AtomicBool::new(false));
        let editor = Arc::new(Editor::new(None, Arc::new(crate::judge::Judge::Haiku)));
        let reply = Reply { conversation: conversation.clone(), editor, spoke: spoke.clone() };

        assert!(reply.call(&json!({ "text": "  " })).is_err());
        assert!(!spoke.load(Ordering::Relaxed));

        reply.call(&json!({ "text": "On it." })).unwrap();
        assert_eq!(*conversation.said.lock().unwrap(), ["On it."]);
        assert!(spoke.load(Ordering::Relaxed));
    }

    #[test]
    fn unknown_hands_are_refused() {
        let hands = Arc::new(Hands::default());
        let error = Dismiss { hands }.call(&json!({ "hand": "h404" })).unwrap_err();
        assert!(error.to_string().contains("there is no hand h404"));
    }
}
