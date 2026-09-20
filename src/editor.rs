//! The editor: everything Anna writes to people passes through here on its
//! way out.
//!
//! Agents write dense, over-explained prose. With a style configured, the
//! judge scores outgoing text against it; text that falls short is rewritten
//! by haiku under one rule — change no facts — and the judge then checks the
//! rewrite still says the same thing. Rewriting comes before rejecting because
//! a rejection loop spends the thread's context on restyling. Only when a
//! faithful rewrite can't be had does the text go back to the thread, with
//! the style attached.
//!
//! Code blocks are not prose: a rewrite has to carry every one of them
//! through unchanged or it is thrown away.

use anyhow::{Result, bail};
use serde_json::json;
use std::sync::Arc;

use crate::claude;
use crate::judge::Judge;
use crate::logs;

const FOLLOWS_STYLE: f64 = 0.5;
const SAME_MEANING: f64 = 0.7;

pub struct Editor {
    style: Option<String>,
    judge: Arc<Judge>,
}

impl Editor {
    pub fn new(style: Option<String>, judge: Arc<Judge>) -> Editor {
        Editor { style, judge }
    }

    pub fn polish(&self, text: &str) -> Result<String> {
        match &self.style {
            Some(style) => self.hold_to(style, text),
            None => Ok(text.to_string()),
        }
    }

    fn hold_to(&self, style: &str, text: &str) -> Result<String> {
        if self.judge.probability(&follows_style_question(style), text)? >= FOLLOWS_STYLE {
            Ok(text.to_string())
        } else {
            self.restyle(style, text)
        }
    }

    fn restyle(&self, style: &str, text: &str) -> Result<String> {
        let rewrite = claude::ask_haiku(&rewrite_prompt(style), text)?;
        if self.faithful(text, &rewrite)? {
            logs::event("editor.rewrote", json!({ "from": text.len(), "to": rewrite.len() }));
            Ok(rewrite)
        } else {
            logs::event("editor.rejected", json!({ "length": text.len() }));
            bail!(
                "This wasn't sent. It doesn't follow the style below, and it couldn't be restyled without changing what it says. Rewrite it yourself and send it again.\n\n{style}"
            )
        }
    }

    fn faithful(&self, original: &str, rewrite: &str) -> Result<bool> {
        if code_blocks(original).iter().all(|block| rewrite.contains(block)) {
            let pair = format!("FIRST TEXT:\n{original}\n\nSECOND TEXT:\n{rewrite}");
            Ok(self.judge.probability(SAME_MEANING_QUESTION, &pair)? >= SAME_MEANING)
        } else {
            Ok(false)
        }
    }
}

const SAME_MEANING_QUESTION: &str = "Two texts follow. Does the second text state the same facts, make the same requests, and reach the same conclusions as the first, leaving out nothing the reader needs and adding nothing that wasn't there? Differences in length, tone and wording don't matter.";

fn follows_style_question(style: &str) -> String {
    format!(
        "The text is a message about to be sent to a person. Does it follow this style guide?\n\n{style}"
    )
}

fn rewrite_prompt(style: &str) -> String {
    format!(
        "Rewrite the message on stdin so it follows the style guide below. Change no facts, drop nothing the reader needs, add nothing. Keep code blocks, commands, names, numbers and links exactly as they are. Reply with ONLY the rewritten message.\n\n{style}"
    )
}

fn code_blocks(text: &str) -> Vec<&str> {
    text.split("```").skip(1).step_by(2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_style_nothing_is_touched() {
        let editor = Editor::new(None, Arc::new(Judge::Haiku));
        assert_eq!(editor.polish("Whatever, however long.").unwrap(), "Whatever, however long.");
    }

    #[test]
    fn code_blocks_are_found_so_a_rewrite_can_be_held_to_them() {
        let text = "Run this:\n```bash\nmake install\n```\nthen this:\n```\nanna setup\n```\ndone";
        assert_eq!(code_blocks(text), ["bash\nmake install\n", "\nanna setup\n"]);
        assert!(code_blocks("no code here").is_empty());
    }
}
