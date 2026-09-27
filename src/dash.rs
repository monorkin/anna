//! `anna dash`: one screen with her Claude accounts, what is going on now
//! and the end of her log, and a chat line to tell a thread something the
//! way `anna tell` does, with the end of its transcript above it. What it
//! shows is gathered on a thread of its own every few seconds, so a slow
//! answer from ax or a running Anna never holds up the keyboard.

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use crate::dash_ansi::line_of;
use crate::dash_data::{self, Snapshot};
use crate::dash_tables;
use crate::lifecycle;
use crate::transcripts;

const REFRESH: Duration = Duration::from_secs(5);
/// How long a wait for a key lasts before the screen is drawn again.
const TICK: Duration = Duration::from_millis(250);
const REPLIES_KEPT: usize = 300;

pub fn run() -> Result<()> {
    let (sender, snapshots) = mpsc::channel();
    thread::spawn(move || {
        let with_jev = dash_data::jev_is_set_up();
        while sender.send(dash_data::gather(with_jev)).is_ok() {
            thread::sleep(REFRESH);
        }
    });

    let mut terminal = ratatui::init();
    let result = Dash::default().run_on(&mut terminal, &snapshots);
    ratatui::restore();
    result
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pane {
    Accounts,
    Now,
    Log,
    Chat,
}

const PANES: [Pane; 4] = [Pane::Accounts, Pane::Now, Pane::Log, Pane::Chat];

impl Pane {
    fn index(self) -> usize {
        PANES.iter().position(|it| *it == self).unwrap_or_default()
    }

    /// These show their newest line at the bottom, and scroll back from it.
    fn follows_the_end(self) -> bool {
        matches!(self, Pane::Log | Pane::Chat)
    }
}

/// What a key asks of the world outside the dashboard.
#[derive(Debug, PartialEq)]
enum Effect {
    Nothing,
    Tell { thread: String, message: String },
    OtherThread,
}

#[derive(Default)]
struct Dash {
    snapshot: Option<Snapshot>,
    focus: usize,
    /// Lines scrolled, per pane: down from the top, or back from the end
    /// for a pane that follows it.
    scrolls: [u16; PANES.len()],
    thread: Option<String>,
    draft: String,
    said: String,
    /// What was sent from here, to tell apart in the transcript.
    sent: Vec<String>,
    replies: Vec<String>,
    quitting: bool,
}

impl Dash {
    fn run_on(&mut self, terminal: &mut DefaultTerminal, snapshots: &Receiver<Snapshot>) -> Result<()> {
        while !self.quitting {
            while let Ok(snapshot) = snapshots.try_recv() {
                self.take(snapshot);
                self.read_replies();
            }
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(TICK)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                match self.press(key) {
                    Effect::Tell { thread, message } => self.tell(&thread, &message),
                    Effect::OtherThread => self.read_replies(),
                    Effect::Nothing => {}
                }
            }
        }
        Ok(())
    }

    fn take(&mut self, snapshot: Snapshot) {
        if self.thread.is_none() {
            self.thread = snapshot.now.as_ref().and_then(|it| it.threads.first().cloned());
        }
        self.snapshot = Some(snapshot);
    }

    fn read_replies(&mut self) {
        self.replies = match &self.thread {
            Some(thread) => transcripts::tail_of_thread(thread, REPLIES_KEPT),
            None => Vec::new(),
        };
    }

    fn tell(&mut self, thread: &str, message: &str) {
        match lifecycle::told(thread, message) {
            Ok(told) => {
                self.said = told;
                self.sent.push(message.to_string());
                self.draft.clear();
            }
            Err(error) => self.said = format!("Not sent: {error:#}"),
        }
    }

    fn press(&mut self, key: KeyEvent) -> Effect {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quitting = true;
            return Effect::Nothing;
        }

        match key.code {
            KeyCode::Tab => self.focus = (self.focus + 1) % PANES.len(),
            KeyCode::BackTab => self.focus = (self.focus + PANES.len() - 1) % PANES.len(),
            _ if self.pane() == Pane::Chat => return self.press_in_chat(key.code),
            KeyCode::Char('q') => self.quitting = true,
            KeyCode::Char(digit @ '1'..='4') => self.focus = digit as usize - '1' as usize,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            _ => {}
        }
        Effect::Nothing
    }

    /// Everything typed here is the message, `q` included; Esc leaves.
    fn press_in_chat(&mut self, code: KeyCode) -> Effect {
        match code {
            KeyCode::Char(typed) => self.draft.push(typed),
            KeyCode::Backspace => {
                self.draft.pop();
            }
            KeyCode::Enter => {
                if let Some(thread) = &self.thread
                    && !self.draft.trim().is_empty()
                {
                    return Effect::Tell { thread: thread.clone(), message: self.draft.trim().to_string() };
                }
            }
            KeyCode::Up => return self.pick_thread(-1),
            KeyCode::Down => return self.pick_thread(1),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::Esc => self.focus = Pane::Now.index(),
            _ => {}
        }
        Effect::Nothing
    }

    fn pick_thread(&mut self, step: isize) -> Effect {
        let threads = self.threads();
        if threads.is_empty() {
            return Effect::Nothing;
        }

        let at = self.thread.as_ref().and_then(|it| threads.iter().position(|thread| thread == it));
        let next = match at {
            Some(at) => (at as isize + step).rem_euclid(threads.len() as isize) as usize,
            None => 0,
        };
        self.thread = Some(threads[next].clone());
        self.scrolls[self.focus] = 0;
        Effect::OtherThread
    }

    fn threads(&self) -> Vec<String> {
        self.snapshot.as_ref().and_then(|it| it.now.as_ref()).map(|it| it.threads.clone()).unwrap_or_default()
    }

    fn pane(&self) -> Pane {
        PANES[self.focus]
    }

    /// Up the screen is negative, whichever way the pane counts.
    fn scroll_by(&mut self, lines: i32) {
        let lines = if self.pane().follows_the_end() { -lines } else { lines };
        let scroll = &mut self.scrolls[self.focus];
        *scroll = (*scroll as i32 + lines).clamp(0, u16::MAX as i32) as u16;
    }

    fn draw(&self, frame: &mut Frame) {
        let [accounts, now, log, chat] =
            Layout::vertical([Constraint::Length(self.accounts_height()), Constraint::Fill(3), Constraint::Fill(3), Constraint::Fill(4)]).areas(frame.area());
        self.draw_accounts(frame, accounts);
        self.draw_now(frame, now);
        self.draw_log(frame, log);
        self.draw_chat(frame, chat);
    }

    /// As tall as the taller of its two tables, and its border.
    fn accounts_height(&self) -> u16 {
        let tables = match &self.snapshot {
            Some(snapshot) if snapshot.accounts_unknown.is_none() => {
                let jev = snapshot.jev.as_ref().map(|it| it.len() + 1).unwrap_or(0);
                (snapshot.accounts.len() + 1).max(jev)
            }
            _ => 1,
        };
        tables as u16 + 2
    }

    fn block(&self, pane: Pane, title: &str) -> Block<'static> {
        let block = Block::bordered();
        if self.pane() == pane {
            let focused = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
            block.border_style(focused).title(Line::styled(format!(" {title} "), focused))
        } else {
            block
                .border_style(Style::new().fg(Color::DarkGray))
                .title(Line::styled(format!(" {title} "), Style::new().fg(Color::Cyan)))
        }
    }

    fn draw_now(&self, frame: &mut Frame, area: Rect) {
        let block = self.block(Pane::Now, "2 Now").title_bottom(" Tab or 1-4: pane · q quit ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let now = match self.snapshot.as_ref().map(|it| it.now.as_ref()) {
            Some(Some(now)) => now,
            Some(None) => return frame.render_widget(Paragraph::new("Anna isn't running."), inner),
            None => return frame.render_widget(Paragraph::new("Looking…"), inner),
        };
        let [going_on, claimed] = Layout::vertical([Constraint::Length(now.going_on.len().max(1) as u16 + 3), Constraint::Min(0)]).areas(inner);
        frame.render_widget(dash_tables::going_on(now, going_on.width), going_on);
        frame.render_widget(dash_tables::claimed(now, claimed.width), claimed);
    }

    fn draw_accounts(&self, frame: &mut Frame, area: Rect) {
        let block = self.block(Pane::Accounts, "1 Claude");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(snapshot) = &self.snapshot else {
            return frame.render_widget(Paragraph::new("Looking…"), inner);
        };
        if let Some(unknown) = &snapshot.accounts_unknown {
            return frame.render_widget(Paragraph::new(format!("Usage unknown: {unknown}")), inner);
        }
        let jev_width = if snapshot.jev.is_some() { dash_tables::JEV_WIDTH } else { 0 };
        let [accounts, _, jev] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(2), Constraint::Length(jev_width)]).areas(inner);
        frame.render_widget(dash_tables::accounts(&snapshot.accounts, accounts.width), accounts);
        if let Some(rows) = &snapshot.jev {
            frame.render_widget(dash_tables::jev(rows), jev);
        }
    }

    fn draw_log(&self, frame: &mut Frame, area: Rect) {
        let block = self.block(Pane::Log, "3 Log").title_bottom(" ↑↓ PgUp/PgDn read back ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = self.snapshot.as_ref().map(|it| it.log.as_slice()).unwrap_or_default();
        let shown = window_of(lines, inner.height as usize, self.scrolls[Pane::Log.index()] as usize);
        frame.render_widget(Paragraph::new(shown.iter().map(|it| line_of(it)).collect::<Vec<_>>()), inner);
    }

    fn draw_chat(&self, frame: &mut Frame, area: Rect) {
        let block = self.block(Pane::Chat, "4 Chat").title_bottom(" ↑↓ thread · Enter send · PgUp/PgDn read back · Esc leave ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [to, replies, said, draft] = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(inner);

        let thread = self.thread.as_deref().unwrap_or("nobody: no thread is going on or claimed");
        frame.render_widget(Paragraph::new(format!("To {thread}")).style(Style::new().add_modifier(Modifier::BOLD)), to);

        let wrapped = wrapped(&self.replies, replies.width as usize);
        let pieces: Vec<String> = wrapped.iter().map(|(_, piece)| piece.clone()).collect();
        let back = self.scrolls[Pane::Chat.index()] as usize;
        let shown = window_of(&pieces, replies.height as usize, back);
        let first = pieces.len().saturating_sub(back).saturating_sub(shown.len());
        let text: Vec<Line> = shown
            .iter()
            .zip(&wrapped[first..])
            .map(|(piece, (from, _))| Line::styled(piece.clone(), style_of(said_by(&self.replies[*from], &self.sent))))
            .collect();
        frame.render_widget(Paragraph::new(text), replies);
        frame.render_widget(Paragraph::new(self.said.as_str()).style(Style::new().add_modifier(Modifier::DIM)), said);

        let room = (draft.width as usize).saturating_sub(3);
        let typed: String = self.draft.chars().skip(self.draft.chars().count().saturating_sub(room)).collect();
        frame.render_widget(Paragraph::new(format!("> {typed}")).style(Style::new().fg(Color::Green)), draft);
        if self.pane() == Pane::Chat {
            frame.set_cursor_position((draft.x + 2 + typed.chars().count() as u16, draft.y));
        }
    }
}

/// Who a line of a thread's transcript, as `anna transcript` renders it,
/// is from.
#[derive(Debug, PartialEq)]
enum Said {
    /// Sent from this dashboard.
    ByYou,
    /// Anything else said to the thread: a message, a mail, a prompt.
    ToIt,
    Call,
    Answer,
    Failure,
    /// The thread's own words.
    ByIt,
}

fn said_by(line: &str, sent: &[String]) -> Said {
    // After the time, which is eight characters
    let rest = line.get(8..).unwrap_or_default();
    if rest.starts_with("  > ") && sent.iter().any(|it| line.contains(it.as_str())) {
        Said::ByYou
    } else if rest.starts_with("  > ") {
        Said::ToIt
    } else if rest.starts_with("    → ") {
        Said::Call
    } else if rest.starts_with("    ← ") {
        Said::Answer
    } else if rest.starts_with("    ✗ ") {
        Said::Failure
    } else {
        Said::ByIt
    }
}

fn style_of(said: Said) -> Style {
    match said {
        Said::ByYou => Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        Said::ToIt => Style::new().fg(Color::Cyan),
        Said::Call | Said::Answer => Style::new().add_modifier(Modifier::DIM),
        Said::Failure => Style::new().fg(Color::Red),
        Said::ByIt => Style::new(),
    }
}

/// Each line broken into pieces no wider than `width` characters, with the
/// line each piece came from.
fn wrapped(lines: &[String], width: usize) -> Vec<(usize, String)> {
    let width = width.max(1);
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let characters: Vec<char> = line.chars().collect();
        if characters.is_empty() {
            out.push((index, String::new()));
        }
        out.extend(characters.chunks(width).map(|it| (index, it.iter().collect::<String>())));
    }
    out
}

/// The `height` lines that end `back` lines before the newest.
fn window_of(lines: &[String], height: usize, back: usize) -> &[String] {
    let end = lines.len().saturating_sub(back);
    &lines[end.saturating_sub(height)..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dash_data::Now;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn dash_with_threads(threads: &[&str]) -> Dash {
        let mut dash = Dash::default();
        let now = Now { threads: threads.iter().map(|it| it.to_string()).collect(), ..Now::default() };
        dash.take(Snapshot { now: Some(now), ..Snapshot::default() });
        dash
    }

    #[test]
    fn tab_moves_between_panes_and_q_quits_everywhere_but_the_chat() {
        let mut dash = dash_with_threads(&[]);
        dash.press(key(KeyCode::BackTab));
        assert_eq!(dash.pane(), Pane::Chat);

        dash.press(key(KeyCode::Char('q')));
        assert!(!dash.quitting, "in the chat a q is part of the message");
        assert_eq!(dash.draft, "q");

        dash.press(key(KeyCode::Tab));
        assert_eq!(dash.pane(), Pane::Accounts);
        dash.press(key(KeyCode::Char('3')));
        assert_eq!(dash.pane(), Pane::Log);
        dash.press(key(KeyCode::Up));
        dash.press(key(KeyCode::PageUp));
        assert_eq!(dash.scrolls[Pane::Log.index()], 11, "up the log is further back from its end");
        dash.press(key(KeyCode::Down));
        assert_eq!(dash.scrolls[Pane::Log.index()], 10);
        dash.press(key(KeyCode::PageDown));
        dash.press(key(KeyCode::Down));
        assert_eq!(dash.scrolls[Pane::Log.index()], 0, "scrolling stops at the newest line");

        dash.press(key(KeyCode::Char('q')));
        assert!(dash.quitting);
    }

    #[test]
    fn a_message_goes_to_the_picked_thread_once_there_is_something_to_say() {
        let mut dash = dash_with_threads(&["basecamp-card-1", "basecamp-card-2"]);
        assert_eq!(dash.thread.as_deref(), Some("basecamp-card-1"), "the first busy thread is picked to begin with");
        dash.press(key(KeyCode::Char('4')));

        assert_eq!(dash.press(key(KeyCode::Enter)), Effect::Nothing, "nothing typed, nothing sent");
        assert_eq!(dash.press(key(KeyCode::Up)), Effect::OtherThread);
        assert_eq!(dash.thread.as_deref(), Some("basecamp-card-2"), "up from the first comes round to the last");

        for typed in "Push it ".chars() {
            dash.press(key(KeyCode::Char(typed)));
        }
        dash.press(key(KeyCode::Backspace));
        assert_eq!(dash.press(key(KeyCode::Enter)), Effect::Tell { thread: "basecamp-card-2".to_string(), message: "Push it".to_string() });

        dash.press(key(KeyCode::PageUp));
        assert_eq!(dash.scrolls[Pane::Chat.index()], 10);
        dash.press(key(KeyCode::Esc));
        assert_eq!(dash.pane(), Pane::Now);
    }

    #[test]
    fn transcript_lines_are_told_apart_by_who_said_them() {
        let sent = ["Push it".to_string()];
        assert_eq!(said_by("04:54:34  > Sam tells you: Push it", &sent), Said::ByYou);
        assert_eq!(said_by("04:54:34  > Marta says: fix the login", &sent), Said::ToIt);
        assert_eq!(said_by("04:54:40  I'll read it first.", &sent), Said::ByIt);
        assert_eq!(said_by("04:54:40    → Bash ls", &sent), Said::Call);
        assert_eq!(said_by("04:54:41    ← done", &sent), Said::Answer);
        assert_eq!(said_by("04:54:46    ✗ no such file", &sent), Said::Failure);
        assert_eq!(said_by("", &sent), Said::ByIt);
    }

    #[test]
    fn replies_are_wrapped_to_the_pane_and_shown_from_the_newest_back() {
        let lines = ["abcdef".to_string(), String::new(), "gh".to_string()];
        let wrapped = wrapped(&lines, 4);
        assert_eq!(wrapped, [(0, "abcd".to_string()), (0, "ef".to_string()), (1, String::new()), (2, "gh".to_string())]);

        let pieces: Vec<String> = wrapped.into_iter().map(|(_, it)| it).collect();
        assert_eq!(window_of(&pieces, 2, 0), ["", "gh"]);
        assert_eq!(window_of(&pieces, 2, 1), ["ef", ""]);
        assert_eq!(window_of(&pieces, 10, 0).len(), 4);
        assert!(window_of(&pieces, 2, 99).is_empty());
    }
}
