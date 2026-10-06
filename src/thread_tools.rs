//! What a thread can do through the broker: speak in its conversation, and
//! run hands. A hand works on after the call that started it, and stays
//! once it is done so the thread can send it back to clean up.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::broker::{Tool, text_of};
use crate::conversation::Conversation;
use crate::editor::{Editor, SentBack};
use crate::hand::Hand;
use crate::held::ReadHeldBack;
use crate::workshop::Workshop;

pub struct Reply {
    pub conversation: Arc<dyn Conversation>,
    pub editor: Arc<Editor>,
    pub sent_back: Arc<SentBack>,
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
        let text = self.editor.polish(text_of(arguments, "text")?, &self.sent_back)?;
        self.conversation.say(&text)?;
        self.spoke.store(true, Ordering::Relaxed);
        Ok("Sent.".to_string())
    }
}

pub struct StartHand {
    /// With the standing of the turn, which is what a hand it starts may be
    /// granted.
    pub workshop: Workshop,
    /// Whether anything for people has gone out this turn. A hand takes
    /// minutes and whoever asked hears nothing from it, so the first one
    /// waits for a word to them.
    pub spoke: Arc<AtomicBool>,
}

impl Tool for StartHand {
    fn name(&self) -> &str {
        "start_hand"
    }

    fn description(&self) -> &str {
        "Start a sandboxed worker in one project folder and give it a brief. It can read and write that folder and nothing else, has no network and none of your memory, so the brief must carry everything it needs to know. It has the languages installed here and builds from what is already fetched, so when the work needs a build, either fetch the project's dependencies yourself first (cargo fetch, bundle install, npm ci) or grant it the registries and say so in the brief. Its builds go to a folder of their own, not the project's. It works on its own: this answers at once with its id, and when it's done a reviewer checks the work against your brief and the verdict comes to you as a message of its own, in a turn of its own. Then send_back or dismiss it. Only one hand works in a folder at a time, and you may have only a few at work — `hands` says how many and which; past that this is refused, so wait for a verdict or dismiss one. A hand takes minutes, and whoever asked hears nothing from it, so the first hand of a turn is refused until you have told them what you're about to do, in one line, where they asked — one line, not an account of your reasoning. Only a turn nobody new is waiting on — one you scheduled for yourself, or one a hand's verdict woke, where they already heard you have it — starts a hand without a word, with `quietly`."
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
                    "description": "Names of your own tools this hand may also call. Only tools that look things up can be given to a hand; anything that posts or changes something out there stays with you. Leave it out unless the work needs them.",
                },
                "services": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Ports on this machine's localhost the hand may reach, as the same port on its own localhost: a database its tests need, say. Only localhost; a hand has no network. Make sure what the hand will use there — a test database, say — is set up and won't collide with anyone else's before you start it.",
                },
                "registries": {
                    "type": "boolean",
                    "description": "Let the hand fetch from the package registries — rubygems, npm, crates.io, PyPI, mise, nodejs.org — and nothing else out there. For work that needs dependencies you haven't fetched. Git remotes stay yours.",
                },
                "quietly": {
                    "type": "boolean",
                    "description": "Start it without having said anything to anyone this turn. Only for a turn nobody new is waiting on: one you scheduled for yourself, or one a hand's verdict woke.",
                },
            },
            "required": ["project", "brief"],
        })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        may_start(self.spoke.load(Ordering::Relaxed), arguments["quietly"].as_bool().unwrap_or(false))?;
        let names: Vec<String> = arguments["tools"]
            .as_array()
            .map(|names| names.iter().filter_map(|it| it.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let runtime = &self.workshop.runtime;
        let mut grant = runtime.catalog.grant(&names, self.workshop.standing)?;
        if !grant.is_empty() {
            // Its tools' answers can be held back like anyone's
            grant.push(Box::new(ReadHeldBack { held: runtime.held.clone(), judge: runtime.judge.clone() }));
        }
        let ports = ports_in(&arguments["services"])?;

        let project = Path::new(text_of(arguments, "project")?);
        self.workshop.refuse_if_busy(project)?;
        let registries = arguments["registries"].as_bool().unwrap_or(false);
        let hand = Hand::start(project, grant, &ports, registries)?;
        self.workshop.start(hand, text_of(arguments, "brief")?)
    }
}

/// Whoever asked hears nothing while a hand runs, so the first hand waits
/// for a word to them — the rule her instructions state, held here because
/// stating it wasn't enough.
fn may_start(spoke: bool, quietly: bool) -> Result<()> {
    if spoke || quietly {
        Ok(())
    } else {
        bail!("Nobody has heard from you this turn. Say what you're about to do, in one line, where the work was asked — then start the hand. If nobody new is waiting on this turn — you scheduled it for yourself, or a hand's verdict woke it — start it with quietly: true.")
    }
}

fn ports_in(services: &Value) -> Result<Vec<u16>> {
    services
        .as_array()
        .map(|it| it.iter())
        .into_iter()
        .flatten()
        .map(|it| u16::try_from(it.as_u64().context("a service is a port number")?).context("that is not a port number"))
        .collect()
}

pub struct SendBack {
    pub workshop: Workshop,
}

impl Tool for SendBack {
    fn name(&self) -> &str {
        "send_back"
    }

    fn description(&self) -> &str {
        "Send a hand that is done back to fix or finish its work. It remembers what it did. Like start_hand, this answers at once, and the reviewer's new verdict comes to you as a message of its own."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "hand": { "type": "string" }, "notes": { "type": "string" } },
            "required": ["hand", "notes"],
        })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        self.workshop.send_back(text_of(arguments, "hand")?, text_of(arguments, "notes")?)
    }
}

/// How her Claude allowance is doing, read from ax: each account she can
/// work as, its session, weekly and Fable limits, and when they reset.
pub struct ClaudeUsage;

impl Tool for ClaudeUsage {
    fn name(&self) -> &str {
        "claude_usage"
    }

    fn description(&self) -> &str {
        "How much of your Claude allowance is used on each account you can work as — the session, weekly and Fable limits and when each resets — and which account you're on now. Use it when someone asks how your tokens or limits are doing, and answer in your own words."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn call(&self, _arguments: &Value) -> Result<String> {
        let reports = ax::usage::of_every_account()?;
        if reports.is_empty() {
            Ok("No accounts are stored for you, so there is nothing to read usage from. `anna claude account add` stores the one you're logged in as.".to_string())
        } else {
            Ok(ax::usage::described(&reports, ax::clock::now_seconds()))
        }
    }
}

pub struct Dismiss {
    pub workshop: Workshop,
}

impl Tool for Dismiss {
    fn name(&self) -> &str {
        "dismiss"
    }

    fn description(&self) -> &str {
        "Let a hand go once you accept its work or give up on it. A hand still working is stopped. Its changes to the project folder stay."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "hand": { "type": "string" } }, "required": ["hand"] })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        self.workshop.dismiss(text_of(arguments, "hand")?)
    }
}

pub struct ListHands {
    pub workshop: Workshop,
}

impl Tool for ListHands {
    fn name(&self) -> &str {
        "hands"
    }

    fn description(&self) -> &str {
        "Your hands: those at work, with what you asked of each and for how long, how many you may have at work, and those that are done and waiting for you to send back or dismiss. Look here before start_hand when you aren't sure you can start one, and to find a hand to dismiss."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn call(&self, _arguments: &Value) -> Result<String> {
        Ok(self.workshop.described())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Recorded {
        said: Mutex<Vec<String>>,
    }

    impl Conversation for Recorded {
        fn key(&self) -> &str {
            "recorded"
        }

        fn origin(&self) -> crate::conversation::Origin {
            crate::conversation::Origin {
                source: "test".to_string(),
                conversation: "recorded".to_string(),
            }
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
        let editor = Arc::new(Editor::new(None, Arc::new(crate::judge::Judge::claude("haiku")), crate::config::Chain::of(&["haiku"])));
        let reply = Reply { conversation: conversation.clone(), editor, sent_back: Arc::default(), spoke: spoke.clone() };

        assert!(reply.call(&json!({ "text": "  " })).is_err());
        assert!(!spoke.load(Ordering::Relaxed));

        reply.call(&json!({ "text": "On it." })).unwrap();
        assert_eq!(*conversation.said.lock().unwrap(), ["On it."]);
        assert!(spoke.load(Ordering::Relaxed));
    }

    #[test]
    fn the_first_hand_waits_for_a_word_to_whoever_asked() {
        let refused = may_start(false, false).unwrap_err().to_string();
        assert!(refused.contains("Nobody has heard from you this turn"), "{refused}");
        assert!(may_start(true, false).is_ok(), "once something went out, hands may start");
        assert!(may_start(false, true).is_ok(), "a turn nobody is waiting on says so and goes ahead");
    }
}
