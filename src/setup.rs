//! `anna setup`: the one time a person decides things for Anna. After this
//! she runs on her own.
//!
//! Everything asked here is optional, and running setup again keeps whatever
//! is skipped. The style and the personality are given as paths to markdown
//! files and copied into the config directory, so the originals can live
//! wherever their author keeps them.

use anyhow::{Context, Result};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::paths;

pub fn run() -> Result<()> {
    let stdin = std::io::stdin();
    let config_dir = paths::config_dir();
    let mut config = Config::load()?;

    walk_through(&mut stdin.lock(), &mut std::io::stdout(), &mut config, &config_dir)?;
    config.save()
}

fn walk_through(input: &mut impl BufRead, output: &mut impl Write, config: &mut Config, config_dir: &Path) -> Result<()> {
    writeln!(output, "Everything here is optional. Press enter to skip a question.\n")?;

    writeln!(output, "A Jev API key makes Anna's yes/no judgments fast and nearly free.")?;
    writeln!(output, "Without one she asks haiku, which is slower and spends your Claude subscription.")?;
    if let Some(key) = ask(input, output, "Jev API key")? {
        config.jev_api_key = Some(key);
    }

    writeln!(output, "\nA prose style is a markdown file describing how Anna should write to people.")?;
    writeln!(output, "Everything she posts is held to it, and rewritten when it falls short.")?;
    if let Some(path) = ask(input, output, "Path to your style file")? {
        adopt(&path, &config_dir.join("style.md"))?;
    }

    writeln!(output, "\nA personality is a markdown file describing who Anna is. Every thread reads it.")?;
    if let Some(path) = ask(input, output, "Path to your personality file")? {
        adopt(&path, &config_dir.join("personality.md"))?;
    }

    writeln!(output, "\nAnna only listens to people you name. Give the sender id a source reports for")?;
    writeln!(output, "them — usually their email address — and what she should call them.")?;
    while let Some(sender) = ask(input, output, "Sender id (enter when done)")? {
        if let Some(name) = ask(input, output, "Their name")? {
            config.people.insert(sender, name);
        }
    }

    writeln!(output, "\nSaved. Next:")?;
    writeln!(output, "  anna mcp add basecamp -- basecamp mcp    give her a way to act somewhere")?;
    writeln!(output, "  anna chat \"hello\"                        talk to her from this terminal")?;
    writeln!(output, "  anna run                                 let her listen on her sources")?;
    Ok(())
}

fn ask(input: &mut impl BufRead, output: &mut impl Write, question: &str) -> Result<Option<String>> {
    write!(output, "{question}: ")?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    let answer = answer.trim();
    if answer.is_empty() {
        Ok(None)
    } else {
        Ok(Some(answer.to_string()))
    }
}

fn adopt(given: &str, destination: &Path) -> Result<()> {
    let source = expanded(given);
    let text = fs::read_to_string(&source).with_context(|| format!("could not read {}", source.display()))?;
    if let Some(directory) = destination.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::write(destination, text)?;
    Ok(())
}

fn expanded(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_kept_and_skipped_questions_change_nothing() {
        let directory = std::env::temp_dir().join(format!("anna-setup-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let style = directory.join("my-style.md");
        fs::write(&style, "Short sentences.").unwrap();

        let mut config = Config::default();
        config.jev_api_key = Some("kept".to_string());
        let answers = format!("\n{}\n\nmarta@example.com\nMarta\n\n", style.display());
        let mut printed = Vec::new();

        walk_through(&mut answers.as_bytes(), &mut printed, &mut config, &directory.join("config")).unwrap();

        assert_eq!(config.jev_api_key.as_deref(), Some("kept"));
        assert_eq!(config.people.get("marta@example.com").map(String::as_str), Some("Marta"));
        assert_eq!(fs::read_to_string(directory.join("config/style.md")).unwrap(), "Short sentences.");
        assert!(!directory.join("config/personality.md").exists());
        assert!(String::from_utf8(printed).unwrap().contains("anna run"));

        fs::remove_dir_all(directory).unwrap();
    }
}
