//! A conversation is wherever someone is talking to Anna: a todo's comments,
//! an email thread, a terminal. A thread only knows how to say something back
//! into it; where that lands is the conversation's business.

use anyhow::Result;

pub const TERMINAL: &str = "terminal";

/// Where a conversation lives: enough to find it again later, when a
/// schedule comes due or another thread has something to tell it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    pub source: String,
    pub conversation: String,
}

/// Whose word a turn runs on. Trusted people can change how the agent
/// behaves and have it do things on the machine it runs on. Everyone else —
/// where a source lets them in at all — can hand it work and nothing more.
/// It is decided from the config when a message arrives, never by the thread,
/// and it changes what the thread is able to do, not only what it is told.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Standing {
    Trusted,
    CanAssignWork,
}

pub trait Conversation: Send + Sync {
    fn key(&self) -> &str;
    fn origin(&self) -> Origin;
    fn say(&self, text: &str) -> Result<()>;

    /// Set when there is no one way to say something here, and the thread
    /// has to answer with other tools: what it is told to do instead.
    fn answered_otherwise(&self) -> Option<String> {
        None
    }
}

pub struct Terminal {
    key: String,
    name: String,
}

impl Terminal {
    pub fn new(name: &str) -> Terminal {
        let name: String = name
            .chars()
            .map(|it| if it.is_ascii_alphanumeric() { it.to_ascii_lowercase() } else { '-' })
            .collect();
        Terminal {
            key: format!("{TERMINAL}-{name}"),
            name,
        }
    }
}

impl Conversation for Terminal {
    fn key(&self) -> &str {
        &self.key
    }

    fn origin(&self) -> Origin {
        Origin {
            source: TERMINAL.to_string(),
            conversation: self.name.clone(),
        }
    }

    fn say(&self, text: &str) -> Result<()> {
        println!("{text}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_safe_to_use_as_folder_names() {
        assert_eq!(Terminal::new("main").key(), "terminal-main");
        assert_eq!(Terminal::new("../Etc passwd").key(), "terminal----etc-passwd");
    }
}
