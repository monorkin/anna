//! The editor: everything Anna writes to people passes through here on its
//! way out.
//!
//! Agents write dense, over-explained prose. With a style configured, the
//! judge scores outgoing text against it; text that falls short is rewritten
//! by haiku under one rule — change no facts — and the judge then checks the
//! rewrite still says the same thing. Rewriting comes before rejecting because
//! a rejection loop spends the thread's context on restyling. Only when a
//! faithful rewrite can't be had does the text go back to the thread, with
//! the style attached — three times in a turn at most. After that what she
//! writes goes out as she wrote it.
//!
//! Code blocks are not prose: a rewrite has to carry every one of them
//! through unchanged or it is thrown away.

use anyhow::{Result, bail};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::claude;
use crate::judge::Judge;
use crate::logs;

const FOLLOWS_STYLE: f64 = 0.5;
const UNFAITHFUL: f64 = 0.75;
const MOST_TIMES_SENT_BACK: u32 = 3;

/// How often the editor has sent a turn's writing back to it. Each turn
/// keeps its own count, whichever tool the writing goes out through.
#[derive(Default)]
pub struct SentBack(AtomicU32);

pub struct Editor {
    style: Option<String>,
    judge: Arc<Judge>,
}

impl Editor {
    pub fn new(style: Option<String>, judge: Arc<Judge>) -> Editor {
        Editor { style, judge }
    }

    pub fn polish(&self, text: &str, sent_back: &SentBack) -> Result<String> {
        self.polish_with(text, sent_back, claude::ask_haiku)
    }

    fn polish_with(&self, text: &str, sent_back: &SentBack, rewrite: impl Fn(&str, &str) -> Result<String>) -> Result<String> {
        match &self.style {
            Some(style) if self.judge.probability(&follows_style_question(style), text)? < FOLLOWS_STYLE => {
                self.restyle(style, text, sent_back, rewrite)
            }
            _ => Ok(text.to_string()),
        }
    }

    fn restyle(&self, style: &str, text: &str, sent_back: &SentBack, rewrite: impl Fn(&str, &str) -> Result<String>) -> Result<String> {
        let rewritten = rewrite(&rewrite_prompt(style), text)?;
        let unfaithfulness = self.unfaithfulness(text, &rewritten)?;

        // Sizes, not the texts: the log is kept and backed up, and what she
        // writes to people doesn't belong in it
        if unfaithfulness < UNFAITHFUL {
            logs::event("editor.rewrote", json!({ "from": text.len(), "to": rewritten.len(), "unfaithfulness": unfaithfulness }));
            Ok(rewritten)
        } else if sent_back.0.load(Ordering::Relaxed) >= MOST_TIMES_SENT_BACK {
            logs::event("editor.let_through", json!({ "unfaithfulness": unfaithfulness, "length": text.len() }));
            Ok(text.to_string())
        } else {
            sent_back.0.fetch_add(1, Ordering::Relaxed);
            logs::event("editor.rejected", json!({ "unfaithfulness": unfaithfulness, "from": text.len(), "to": rewritten.len() }));
            bail!(
                "This wasn't sent. It doesn't follow the style below, and it couldn't be restyled without changing what it says. Rewrite it yourself and send it again.\n\n{style}"
            )
        }
    }

    /// How sure the judge is that the rewrite says something else than the
    /// original did — the worse of inventing and dropping, asked separately
    /// because one question about both can't tell a good rewrite from one that
    /// lost a fact. A rewrite that lost or altered a code block is certain.
    fn unfaithfulness(&self, original: &str, rewrite: &str) -> Result<f64> {
        if code_blocks(original).iter().all(|block| rewrite.contains(block)) {
            let pair = format!("FIRST TEXT:\n{original}\n\nSECOND TEXT:\n{rewrite}");
            let invents = self.judge.probability(INVENTS_QUESTION, &pair)?;
            let drops = self.judge.probability(DROPS_QUESTION, &pair)?;
            Ok(invents.max(drops))
        } else {
            Ok(1.0)
        }
    }
}

const INVENTS_QUESTION: &str = "Two texts follow. The second is a shorter, plainer rewrite of the first. Does the rewrite say something the first text does not: a different or extra fact, name, number, cause, action or outcome, or the opposite of what the first text says?";

const DROPS_QUESTION: &str = "Two texts follow. The second is a shorter, plainer rewrite of the first. List in your head the concrete things the first text reports: what happened, why, what was done about it, and what the state is now, plus any request or question. Is at least one of those missing from the rewrite? Greetings, hedging, filler, apologies and offers of further help are not concrete things.";

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
        assert_eq!(editor.polish("Whatever, however long.", &SentBack::default()).unwrap(), "Whatever, however long.");
    }

    #[test]
    fn writing_that_cant_be_restyled_is_sent_back_three_times_a_turn_then_goes_out_as_written() {
        // Each try: off style, and a rewrite that drops something
        let editor = Editor::new(Some("Be brief.".to_string()), Arc::new(crate::judge::answering(&[0.1, 0.1, 0.9].repeat(5))));
        let rewrite = |_: &str, _: &str| Ok("Short.".to_string());
        let this_turn = SentBack::default();

        for _ in 0..3 {
            let refused = editor.polish_with("A long message.", &this_turn, rewrite).unwrap_err();
            assert!(refused.to_string().starts_with("This wasn't sent."));
        }
        assert_eq!(editor.polish_with("A long message.", &this_turn, rewrite).unwrap(), "A long message.");

        let next_turn = SentBack::default();
        assert!(editor.polish_with("A long message.", &next_turn, rewrite).is_err(), "a new turn is held to the style again");
    }

    #[test]
    fn code_blocks_are_found_so_a_rewrite_can_be_held_to_them() {
        let text = "Run this:\n```bash\nmake install\n```\nthen this:\n```\nanna setup\n```\ndone";
        assert_eq!(code_blocks(text), ["bash\nmake install\n", "\nanna setup\n"]);
        assert!(code_blocks("no code here").is_empty());
    }
}
