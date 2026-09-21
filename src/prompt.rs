//! How setup looks and how it asks: a little colour and a rail down the left
//! of each question, not a full-screen interface. Everything still works
//! piped, in a dumb terminal, or with NO_COLOR set — the styling just falls
//! away.
//!
//! Long answers — the agent's guidance, its prose style — open in $EDITOR,
//! because nobody writes a page of markdown at a prompt. Without an editor
//! they are typed in place and ended with a line holding a single dot.

use anyhow::{Context, Result};
use std::io::{BufRead, IsTerminal, Write};
use std::process::Command;

pub struct Question<'a> {
    pub title: &'a str,
    pub question: &'a str,
    pub hint: &'a str,
}

pub trait Asking {
    fn banner(&mut self, title: &str, subtitle: &str) -> Result<()>;
    fn string(&mut self, question: &Question, current: Option<&str>) -> Result<Option<String>>;
    fn text(&mut self, question: &Question, current: Option<&str>) -> Result<Option<String>>;
    fn secret(&mut self, question: &Question) -> Result<Option<String>>;
    fn yes(&mut self, question: &Question, default: bool) -> Result<bool>;
    fn choice(&mut self, question: &Question, options: &[String]) -> Result<usize>;
    fn done(&mut self, text: &str) -> Result<()>;
    fn trouble(&mut self, text: &str) -> Result<()>;
    fn note(&mut self, text: &str) -> Result<()>;
    /// Hands the terminal to another program until it exits.
    fn hand_over_to(&mut self, command: &mut Command) -> Result<bool>;
}

pub struct Terminal<I, O> {
    input: I,
    output: O,
    styled: bool,
}

impl<I: BufRead, O: Write> Terminal<I, O> {
    pub fn new(input: I, output: O, styled: bool) -> Terminal<I, O> {
        Terminal { input, output, styled }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.styled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn rail(&self) -> String {
        self.paint("38;5;99", "│")
    }

    fn ask(&mut self, question: &Question, suffix: &str) -> Result<()> {
        let title = self.paint("1;38;5;213", question.title);
        let corner = self.paint("38;5;99", "┌");
        let rail = self.rail();
        let hint = self.paint("2", question.hint);

        writeln!(self.output)?;
        writeln!(self.output, "{corner} {title}")?;
        writeln!(self.output, "{rail} {}", question.question)?;
        writeln!(self.output, "{rail} {hint}")?;
        if !suffix.is_empty() {
            let suffix = self.paint("2", suffix);
            writeln!(self.output, "{rail} {suffix}")?;
        }
        Ok(())
    }

    fn line(&mut self) -> Result<String> {
        let arrow = self.paint("38;5;213", "›");
        let foot = self.paint("38;5;99", "└");
        write!(self.output, "{foot} {arrow} ")?;
        self.output.flush()?;

        let mut answer = String::new();
        self.input.read_line(&mut answer)?;
        Ok(answer.trim().to_string())
    }

    fn typed_in_place(&mut self) -> Result<Option<String>> {
        let rail = self.rail();
        let mut lines = Vec::new();
        loop {
            write!(self.output, "{rail} ")?;
            self.output.flush()?;

            let mut line = String::new();
            if self.input.read_line(&mut line)? == 0 || line.trim_end() == "." {
                break;
            }
            lines.push(line.trim_end().to_string());
        }

        let text = lines.join("\n").trim().to_string();
        if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(text))
        }
    }
}

impl<I: BufRead, O: Write> Asking for Terminal<I, O> {
    fn banner(&mut self, title: &str, subtitle: &str) -> Result<()> {
        let title = self.paint("1;38;5;213", title);
        let subtitle = self.paint("2", subtitle);
        writeln!(self.output, "\n  {title}\n  {subtitle}")?;
        Ok(())
    }

    fn string(&mut self, question: &Question, current: Option<&str>) -> Result<Option<String>> {
        let keeps = current.map(|it| format!("Enter keeps: {it}")).unwrap_or_default();
        self.ask(question, &keeps)?;

        let answer = self.line()?;
        if answer.is_empty() {
            Ok(None)
        } else {
            Ok(Some(answer))
        }
    }

    fn text(&mut self, question: &Question, current: Option<&str>) -> Result<Option<String>> {
        match editor() {
            Some(editor) => {
                let keeps = if current.is_some() { "Enter opens what's there in your editor; s skips." } else { "Enter opens your editor; s skips." };
                self.ask(question, keeps)?;
                if self.line()?.eq_ignore_ascii_case("s") {
                    Ok(None)
                } else {
                    written_in(&editor, current.unwrap_or_default())
                }
            }
            None => {
                self.ask(question, "Type it here and end with a line holding only a dot. A dot straight away skips.")?;
                self.typed_in_place()
            }
        }
    }

    fn secret(&mut self, question: &Question) -> Result<Option<String>> {
        self.ask(question, "What you type stays hidden.")?;
        let quiet = Echo::off();
        let answer = self.line();
        drop(quiet);
        writeln!(self.output)?;

        let answer = answer?;
        if answer.is_empty() {
            Ok(None)
        } else {
            Ok(Some(answer))
        }
    }

    fn yes(&mut self, question: &Question, default: bool) -> Result<bool> {
        let choices = if default { "Y/n" } else { "y/N" };
        self.ask(question, choices)?;

        let answer = self.line()?;
        if answer.is_empty() {
            Ok(default)
        } else {
            Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
        }
    }

    fn choice(&mut self, question: &Question, options: &[String]) -> Result<usize> {
        let listed = options
            .iter()
            .enumerate()
            .map(|(index, option)| format!("{}. {option}", index + 1))
            .collect::<Vec<_>>()
            .join("   ");
        self.ask(question, &listed)?;

        loop {
            match self.line()?.parse::<usize>() {
                Ok(number) if (1..=options.len()).contains(&number) => return Ok(number - 1),
                _ => self.trouble(&format!("Pick a number from 1 to {}.", options.len()))?,
            }
        }
    }

    fn done(&mut self, text: &str) -> Result<()> {
        let tick = self.paint("38;5;78", "✓");
        writeln!(self.output, "  {tick} {text}")?;
        Ok(())
    }

    fn trouble(&mut self, text: &str) -> Result<()> {
        let cross = self.paint("38;5;203", "✗");
        writeln!(self.output, "  {cross} {text}")?;
        Ok(())
    }

    fn note(&mut self, text: &str) -> Result<()> {
        for line in text.lines() {
            writeln!(self.output, "  {line}")?;
        }
        Ok(())
    }

    fn hand_over_to(&mut self, command: &mut Command) -> Result<bool> {
        self.output.flush()?;
        Ok(command.status().context("could not start it")?.success())
    }
}

pub fn styled_for_stdout() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() && std::env::var("TERM").is_ok_and(|it| it != "dumb")
}

fn editor() -> Option<String> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|it| !it.trim().is_empty())
}

fn written_in(editor: &str, starting_with: &str) -> Result<Option<String>> {
    let path = std::env::temp_dir().join(format!("anna-setup-{}.md", std::process::id()));
    std::fs::write(&path, starting_with)?;

    let mut words = editor.split_whitespace();
    let program = words.next().context("EDITOR is empty")?;
    let status = Command::new(program).args(words).arg(&path).status().with_context(|| format!("could not start {editor}"))?;
    let written = std::fs::read_to_string(&path).unwrap_or_default().trim().to_string();
    let _ = std::fs::remove_file(&path);

    if status.success() && !written.is_empty() {
        Ok(Some(written))
    } else {
        Ok(None)
    }
}

/// Turns the terminal's echo off for as long as it lives, and back on when
/// dropped — also when setup errors out mid-question.
struct Echo {
    before: Option<libc::termios>,
}

impl Echo {
    fn off() -> Echo {
        if !std::io::stdin().is_terminal() {
            return Echo { before: None };
        }

        unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut settings) != 0 {
                return Echo { before: None };
            }
            let before = settings;
            settings.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &settings);
            Echo { before: Some(before) }
        }
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        if let Some(before) = &self.before {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, before);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUESTION: Question = Question { title: "Name", question: "What do you want to name your agent?", hint: "This is how it refers to itself" };

    fn terminal(answers: &str, styled: bool) -> Terminal<&[u8], Vec<u8>> {
        Terminal::new(answers.as_bytes(), Vec::new(), styled)
    }

    #[test]
    fn questions_show_their_title_question_and_hint() {
        let mut plain = terminal("Botten\n", false);
        assert_eq!(plain.string(&QUESTION, Some("Anna")).unwrap(), Some("Botten".to_string()));
        let shown = String::from_utf8(plain.output).unwrap();
        assert!(shown.contains("┌ Name\n│ What do you want to name your agent?\n│ This is how it refers to itself\n│ Enter keeps: Anna\n└ › "));
        assert!(!shown.contains('\x1b'));

        let mut coloured = terminal("\n", true);
        assert_eq!(coloured.string(&QUESTION, None).unwrap(), None);
        assert!(String::from_utf8(coloured.output).unwrap().contains("\x1b[1;38;5;213mName\x1b[0m"));
    }

    #[test]
    fn yes_or_no_falls_back_to_its_default_and_choices_insist_on_a_number() {
        assert!(terminal("\n", false).yes(&QUESTION, true).unwrap());
        assert!(!terminal("\n", false).yes(&QUESTION, false).unwrap());
        assert!(terminal("YES\n", false).yes(&QUESTION, false).unwrap());
        assert!(!terminal("nope\n", false).yes(&QUESTION, true).unwrap());

        let options = ["Work".to_string(), "Personal".to_string()];
        assert_eq!(terminal("7\ntwo\n2\n", false).choice(&QUESTION, &options).unwrap(), 1);
    }

    #[test]
    fn long_answers_without_an_editor_end_at_a_lone_dot() {
        let mut typed = terminal("Lead with the answer.\n\nShort sentences.\n.\nnot read\n", false);
        assert_eq!(typed.text(&QUESTION, None).unwrap(), Some("Lead with the answer.\n\nShort sentences.".to_string()));
        assert_eq!(terminal(".\n", false).text(&QUESTION, None).unwrap(), None);
    }
}
