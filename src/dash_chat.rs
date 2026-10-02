//! The dashboard's chat: a line to tell a thread something the way `anna
//! tell` does, with what is going on in that thread at the head and the end
//! of its transcript below.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::dash::Effect;
use crate::dash_data::ThreadNow;
use crate::lifecycle;
use crate::transcripts;

const REPLIES_KEPT: usize = 300;

#[derive(Default)]
pub struct Chat {
    pub thread: Option<String>,
    draft: String,
    said: String,
    /// What was sent from here, to tell apart in the transcript.
    sent: Vec<String>,
    replies: Vec<String>,
    /// Lines scrolled back from the newest.
    back: u16,
}

impl Chat {
    pub fn read_replies(&mut self) {
        self.replies = match &self.thread {
            Some(thread) => transcripts::tail_of_thread(thread, REPLIES_KEPT),
            None => Vec::new(),
        };
    }

    pub fn tell(&mut self, thread: &str, message: &str) {
        match lifecycle::told(thread, message) {
            Ok(told) => {
                self.said = told;
                self.sent.push(message.to_string());
                self.draft.clear();
            }
            Err(error) => self.said = format!("Not sent: {error:#}"),
        }
    }

    /// Everything typed here is the message, digits and `q` included; Esc
    /// leaves.
    pub fn press(&mut self, code: KeyCode, threads: &[String]) -> Effect {
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
            KeyCode::Up => return self.pick_thread(threads, -1),
            KeyCode::Down => return self.pick_thread(threads, 1),
            KeyCode::PageUp => self.back = self.back.saturating_add(10),
            KeyCode::PageDown => self.back = self.back.saturating_sub(10),
            KeyCode::Esc => return Effect::Leave,
            _ => {}
        }
        Effect::Nothing
    }

    fn pick_thread(&mut self, threads: &[String], step: isize) -> Effect {
        if threads.is_empty() {
            return Effect::Nothing;
        }

        let at = self.thread.as_ref().and_then(|it| threads.iter().position(|thread| thread == it));
        let next = match at {
            Some(at) => (at as isize + step).rem_euclid(threads.len() as isize) as usize,
            None => 0,
        };
        self.thread = Some(threads[next].clone());
        self.back = 0;
        Effect::OtherThread
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, about: Option<&ThreadNow>) {
        let head = head_of(self.thread.as_deref(), about);
        let [head_area, replies, said, draft] =
            Layout::vertical([Constraint::Length(head.len() as u16 + 2), Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(area);
        frame.render_widget(Paragraph::new(head).block(Block::bordered().border_style(Style::new().fg(Color::DarkGray))), head_area);

        let wrapped = wrapped(&self.replies, replies.width as usize);
        let pieces: Vec<String> = wrapped.iter().map(|(_, piece)| piece.clone()).collect();
        let back = self.back as usize;
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
        frame.set_cursor_position((draft.x + 2 + typed.chars().count() as u16, draft.y));
    }
}

/// Who it is, what it's doing, whether a turn is running, and its hands.
fn head_of(thread: Option<&str>, about: Option<&ThreadNow>) -> Vec<Line<'static>> {
    let Some(thread) = thread else {
        return vec![Line::from("Nobody to talk to: no thread is going on or claimed.")];
    };
    let mut lines = vec![Line::styled(format!("To {thread}"), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))];
    let Some(about) = about else {
        lines.push(Line::styled("Not going on or claimed right now.", Style::new().add_modifier(Modifier::DIM)));
        return lines;
    };

    lines.push(Line::from(about.title.clone()));
    let mut turn = match &about.turn_for {
        Some(taken) => vec![Span::raw("Turn running for "), Span::styled(taken.clone(), Style::new().fg(Color::Yellow))],
        None => vec![Span::styled("Between turns", Style::new().add_modifier(Modifier::DIM))],
    };
    match about.waiting.as_str() {
        "" => {}
        "1" => turn.push(Span::styled(" · 1 message waiting", Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
        many => turn.push(Span::styled(format!(" · {many} messages waiting"), Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
    }
    lines.push(Line::from(turn));
    for [hand, worktree, taken] in &about.hands {
        lines.push(Line::from(vec![
            Span::styled(format!("Hand {hand}"), Style::new().add_modifier(Modifier::DIM)),
            Span::raw(" in "),
            Span::styled(worktree.clone(), Style::new().fg(Color::Magenta)),
            Span::raw(format!(", {taken}")),
        ]));
    }
    lines
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
pub fn window_of(lines: &[String], height: usize, back: usize) -> &[String] {
    let end = lines.len().saturating_sub(back);
    &lines[end.saturating_sub(height)..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn threads(names: &[&str]) -> Vec<String> {
        names.iter().map(|it| it.to_string()).collect()
    }

    #[test]
    fn a_message_goes_to_the_picked_thread_once_there_is_something_to_say() {
        let threads = threads(&["basecamp-card-1", "basecamp-card-2"]);
        let mut chat = Chat { thread: Some("basecamp-card-1".to_string()), ..Chat::default() };

        assert_eq!(chat.press(KeyCode::Enter, &threads), Effect::Nothing, "nothing typed, nothing sent");
        assert_eq!(chat.press(KeyCode::Up, &threads), Effect::OtherThread);
        assert_eq!(chat.thread.as_deref(), Some("basecamp-card-2"), "up from the first comes round to the last");

        for typed in "Push it 2 q ".chars() {
            chat.press(KeyCode::Char(typed), &threads);
        }
        chat.press(KeyCode::Backspace, &threads);
        assert_eq!(chat.press(KeyCode::Enter, &threads), Effect::Tell { thread: "basecamp-card-2".to_string(), message: "Push it 2 q".to_string() }, "digits and q are part of the message");

        chat.press(KeyCode::PageUp, &threads);
        assert_eq!(chat.back, 10);
        assert_eq!(chat.press(KeyCode::Down, &threads), Effect::OtherThread);
        assert_eq!(chat.back, 0, "another thread is read from its newest line");
        assert_eq!(chat.press(KeyCode::Esc, &threads), Effect::Leave);
    }

    #[test]
    fn the_head_says_what_the_thread_is_doing_and_what_its_hands_are() {
        let text = |lines: Vec<Line>| lines.iter().map(|it| it.to_string()).collect::<Vec<_>>();
        let about = ThreadNow {
            title: "Fixing the login".to_string(),
            turn_for: Some("14m".to_string()),
            waiting: "3".to_string(),
            hands: vec![["h18d9", "login-fix", "13m"].map(String::from)],
        };
        assert_eq!(
            text(head_of(Some("basecamp-card-1"), Some(&about))),
            ["To basecamp-card-1", "Fixing the login", "Turn running for 14m · 3 messages waiting", "Hand h18d9 in login-fix, 13m"]
        );

        let resting = ThreadNow { title: "Waiting on review".to_string(), ..ThreadNow::default() };
        assert_eq!(text(head_of(Some("basecamp-card-2"), Some(&resting)))[2], "Between turns");
        assert_eq!(text(head_of(Some("basecamp-card-9"), None))[1], "Not going on or claimed right now.");
        assert_eq!(text(head_of(None, None)), ["Nobody to talk to: no thread is going on or claimed."]);
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
