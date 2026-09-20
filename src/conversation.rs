//! A conversation is wherever someone is talking to Anna: a todo's comments,
//! an email thread, a terminal. A thread only knows how to say something back
//! into it; where that lands is the conversation's business.

use anyhow::Result;

pub trait Conversation: Send + Sync {
    fn key(&self) -> &str;
    fn say(&self, text: &str) -> Result<()>;
}

pub struct Terminal {
    key: String,
}

impl Terminal {
    pub fn new(name: &str) -> Terminal {
        let name: String = name
            .chars()
            .map(|it| if it.is_ascii_alphanumeric() { it.to_ascii_lowercase() } else { '-' })
            .collect();
        Terminal {
            key: format!("terminal-{name}"),
        }
    }
}

impl Conversation for Terminal {
    fn key(&self) -> &str {
        &self.key
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
