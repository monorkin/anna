//! `anna source …`: looking at a source without acting on it.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::config::{self, Config};
use crate::editor::Editor;
use crate::judge::Judge;
use crate::mcp::Catalog;
use crate::source::{self, Position};

const LONGEST_SHOWN: usize = 70;

/// Calls the source's watch tool once and shows what Anna would make of the
/// answer: which messages she finds, who she thinks sent them, and whether
/// she listens to that person. Nothing is remembered and nobody is woken, so
/// it is safe to run while the pointers are still being worked out.
pub fn check(name: &str) -> Result<()> {
    let config = Config::load()?;
    let source = config
        .sources
        .get(name)
        .with_context(|| format!("there is no source called {name}"))?;

    let judge = Arc::new(Judge::Haiku);
    let editor = Arc::new(Editor::new(config::style()?, judge.clone()));
    let catalog = Catalog::open(&config, editor, judge);

    // Read from where she has got to, and the position left where it is
    let arguments = match Position::of(name, source) {
        Some(position) => position.in_arguments(&source.watch.arguments),
        None => source.watch.arguments.clone(),
    };
    let answer = catalog.call(&source.server, &source.watch.tool, &arguments)?;
    let messages = source::messages_in(source, &answer)?;

    println!("{} messages found in {} bytes of answer.", messages.len(), answer.len());
    if source.cursor.is_some() && messages.is_empty() {
        println!("This source is read from a position, so an empty answer is the usual one: it holds what arrived since she last looked.");
    }
    for message in &messages {
        let listened_to = match config.people.get(&message.sender) {
            Some(person) => format!("listens to: {person}"),
            None if source.anyone => "listens to: anyone here".to_string(),
            None => "ignored: not in people".to_string(),
        };
        println!();
        println!("  id            {}", shortened(&message.id));
        println!("  conversation  {}", shortened(&message.conversation));
        println!("  sender        {} ({listened_to})", message.sender);
        println!("  text          {}", shortened(message.text.lines().next().unwrap_or_default()));
    }
    Ok(())
}

fn shortened(text: &str) -> String {
    if text.chars().count() > LONGEST_SHOWN {
        format!("{}…", text.chars().take(LONGEST_SHOWN).collect::<String>())
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_values_are_cut_on_a_character_not_a_byte() {
        assert_eq!(shortened("short"), "short");
        assert_eq!(shortened(&"š".repeat(80)), format!("{}…", "š".repeat(70)));
    }
}
