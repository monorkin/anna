//! `anna dash`: her tabs in one terminal. The overview has what is going on
//! now and the end of her log; the chat tells a thread something the way
//! `anna tell` does, with what that thread is doing above; the memory tab
//! reads, archives and brings back what she has learned; usage has her
//! Claude accounts and Jev's bill. What it shows is gathered on a thread of
//! its own every few seconds, so a slow answer from ax or a running Anna
//! never holds up the keyboard.

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Tabs};
use ratatui::{DefaultTerminal, Frame};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use crate::dash_ansi::line_of;
use crate::dash_chat::{Chat, window_of};
use crate::dash_data::{self, Snapshot};
use crate::dash_memory::{self, Memories};
use crate::dash_tables;

const REFRESH: Duration = Duration::from_secs(5);
/// How long a wait for a key lasts before the screen is drawn again.
const TICK: Duration = Duration::from_millis(250);

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

#[derive(Clone, Copy, PartialEq, Debug, Default)]
enum Tab {
    #[default]
    Overview,
    Chat,
    Memory,
    Usage,
}

const TABS: [Tab; 4] = [Tab::Overview, Tab::Chat, Tab::Memory, Tab::Usage];

impl Tab {
    fn index(self) -> usize {
        TABS.iter().position(|it| *it == self).unwrap_or_default()
    }

    fn name(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Chat => "Chat",
            Tab::Memory => "Memory",
            Tab::Usage => "Usage",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Tab::Overview => "Tab or 1-4 switch tabs · ↑↓ PgUp/PgDn read back through the log · q quit",
            Tab::Chat => "Tab switches tabs · ↑↓ pick a thread · Enter send · PgUp/PgDn read back · Esc leave",
            Tab::Memory => "Tab or 1-4 switch tabs · ↑↓ pick · PgUp/PgDn read on · a archive · u bring back · q quit",
            Tab::Usage => "Tab or 1-4 switch tabs · q quit",
        }
    }
}

/// What a key asks of the world outside the dashboard.
#[derive(Debug, PartialEq)]
pub enum Effect {
    Nothing,
    Tell { thread: String, message: String },
    OtherThread,
    Leave,
    Archive { id: String },
    Unarchive { id: String },
}

#[derive(Default)]
struct Dash {
    snapshot: Option<Snapshot>,
    tab: Tab,
    /// Lines the log is scrolled back from its newest.
    log_back: u16,
    chat: Chat,
    memories: Memories,
    quitting: bool,
}

impl Dash {
    fn run_on(&mut self, terminal: &mut DefaultTerminal, snapshots: &Receiver<Snapshot>) -> Result<()> {
        while !self.quitting {
            while let Ok(snapshot) = snapshots.try_recv() {
                self.take(snapshot);
                self.chat.read_replies();
            }
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(TICK)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                let effect = self.press(key);
                self.act_on(effect);
            }
        }
        Ok(())
    }

    fn take(&mut self, snapshot: Snapshot) {
        if self.chat.thread.is_none() {
            self.chat.thread = snapshot.now.as_ref().ok().and_then(|it| it.threads.first().cloned());
        }
        self.snapshot = Some(snapshot);
    }

    fn act_on(&mut self, effect: Effect) {
        match effect {
            Effect::Tell { thread, message } => self.chat.tell(&thread, &message),
            Effect::OtherThread => self.chat.read_replies(),
            Effect::Leave => self.tab = Tab::Overview,
            Effect::Archive { id } => self.memories.said = self.marked(&id, dash_memory::archive(&id), true),
            Effect::Unarchive { id } => self.memories.said = self.marked(&id, dash_memory::unarchive(&id), false),
            Effect::Nothing => {}
        }
    }

    /// The memory shows as it now is before the next look at the store.
    fn marked(&mut self, id: &str, done: Result<String>, archived: bool) -> String {
        match done {
            Ok(said) => {
                if let Some(Ok(rows)) = self.snapshot.as_mut().map(|it| &mut it.memories)
                    && let Some(row) = rows.iter_mut().find(|it| it.id == id)
                {
                    row.archived = archived;
                }
                said
            }
            Err(error) => format!("Couldn't: {error:#}"),
        }
    }

    fn press(&mut self, key: KeyEvent) -> Effect {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quitting = true;
            return Effect::Nothing;
        }

        match key.code {
            KeyCode::Tab => self.tab = TABS[(self.tab.index() + 1) % TABS.len()],
            KeyCode::BackTab => self.tab = TABS[(self.tab.index() + TABS.len() - 1) % TABS.len()],
            _ if self.tab == Tab::Chat => return self.chat.press(key.code, &self.threads()),
            KeyCode::Char('q') => self.quitting = true,
            KeyCode::Char(digit @ '1'..='4') => self.tab = TABS[digit as usize - '1' as usize],
            _ if self.tab == Tab::Memory => return self.memories.press(key.code, memory_rows_of(&self.snapshot)),
            KeyCode::Up | KeyCode::Char('k') if self.tab == Tab::Overview => self.log_back = self.log_back.saturating_add(1),
            KeyCode::Down | KeyCode::Char('j') if self.tab == Tab::Overview => self.log_back = self.log_back.saturating_sub(1),
            KeyCode::PageUp if self.tab == Tab::Overview => self.log_back = self.log_back.saturating_add(10),
            KeyCode::PageDown if self.tab == Tab::Overview => self.log_back = self.log_back.saturating_sub(10),
            _ => {}
        }
        Effect::Nothing
    }

    fn threads(&self) -> Vec<String> {
        self.snapshot.as_ref().and_then(|it| it.now.as_ref().ok()).map(|it| it.threads.clone()).unwrap_or_default()
    }

    fn draw(&self, frame: &mut Frame) {
        let [tabs, body, help] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        frame.render_widget(self.tabs(), tabs);
        frame.render_widget(Paragraph::new(self.tab.help()).style(Style::new().fg(Color::DarkGray)), help);

        let Some(snapshot) = &self.snapshot else {
            return frame.render_widget(Paragraph::new("Looking…"), body);
        };
        match self.tab {
            Tab::Overview => self.draw_overview(frame, body, snapshot),
            Tab::Chat => {
                let about = self.chat.thread.as_ref().and_then(|thread| snapshot.now.as_ref().ok()?.about.get(thread));
                self.chat.draw(frame, body, about);
            }
            Tab::Memory => self.memories.draw(frame, body, &snapshot.memories),
            Tab::Usage => draw_usage(frame, body, snapshot),
        }
    }

    fn tabs(&self) -> Tabs<'static> {
        Tabs::new(TABS.iter().enumerate().map(|(index, tab)| format!("{} {}", index + 1, tab.name())))
            .select(self.tab.index())
            .style(Style::new().fg(Color::DarkGray))
            .highlight_style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            .divider("·")
    }

    fn draw_overview(&self, frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
        let [now, log] = Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(area);

        let block = framed("Now");
        let inner = block.inner(now);
        frame.render_widget(block, now);
        match &snapshot.now {
            Ok(now) => {
                let [going_on, claimed] = Layout::vertical([Constraint::Length(now.going_on.len().max(1) as u16 + 3), Constraint::Min(0)]).areas(inner);
                frame.render_widget(dash_tables::going_on(now, going_on.width), going_on);
                frame.render_widget(dash_tables::claimed(now, claimed.width), claimed);
            }
            Err(why) => frame.render_widget(Paragraph::new(why.as_str()), inner),
        }

        let block = framed("Log");
        let inner = block.inner(log);
        frame.render_widget(block, log);
        let shown = window_of(&snapshot.log, inner.height as usize, self.log_back as usize);
        frame.render_widget(Paragraph::new(shown.iter().map(|it| line_of(it)).collect::<Vec<_>>()), inner);
    }
}

fn memory_rows_of(snapshot: &Option<Snapshot>) -> &[dash_data::MemoryRow] {
    match snapshot.as_ref().map(|it| &it.memories) {
        Some(Ok(rows)) => rows,
        _ => &[],
    }
}

fn draw_usage(frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
    let block = framed("Claude");
    if let Some(unknown) = &snapshot.accounts_unknown {
        return frame.render_widget(Paragraph::new(format!("Usage unknown: {unknown}")).block(block), area);
    }
    let jev_width = if snapshot.jev.is_some() { dash_tables::JEV_WIDTH + 2 } else { 0 };
    let [accounts, _, jev] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(1), Constraint::Length(jev_width)]).areas(area);
    let accounts_height = (snapshot.accounts.len() as u16 + 3).min(accounts.height);
    let accounts = Rect { height: accounts_height, ..accounts };
    let inner = block.inner(accounts);
    frame.render_widget(block, accounts);
    frame.render_widget(dash_tables::accounts(&snapshot.accounts, inner.width), inner);

    if let Some(rows) = &snapshot.jev {
        let jev = Rect { height: (rows.len() as u16 + 3).min(jev.height), ..jev };
        let block = Block::bordered().border_style(Style::new().fg(Color::DarkGray));
        let inner = block.inner(jev);
        frame.render_widget(block, jev);
        frame.render_widget(dash_tables::jev(rows), inner);
    }
}

fn framed(title: &str) -> Block<'static> {
    Block::bordered()
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Line::styled(format!(" {title} "), Style::new().fg(Color::Cyan)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dash_data::{MemoryRow, Now};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn dash_with(threads: &[&str], memories: Vec<MemoryRow>) -> Dash {
        let mut dash = Dash::default();
        let now = Now { threads: threads.iter().map(|it| it.to_string()).collect(), ..Now::default() };
        dash.take(Snapshot { accounts: Vec::new(), accounts_unknown: None, jev: None, log: Vec::new(), now: Ok(now), memories: Ok(memories) });
        dash
    }

    #[test]
    fn tabs_switch_by_number_and_tab_and_q_quits_everywhere_but_the_chat() {
        let mut dash = dash_with(&["basecamp-card-1"], Vec::new());
        assert_eq!(dash.tab, Tab::Overview);
        assert_eq!(dash.chat.thread.as_deref(), Some("basecamp-card-1"), "the first busy thread is picked to begin with");

        dash.press(key(KeyCode::Char('3')));
        assert_eq!(dash.tab, Tab::Memory);
        dash.press(key(KeyCode::BackTab));
        assert_eq!(dash.tab, Tab::Chat);

        dash.press(key(KeyCode::Char('q')));
        dash.press(key(KeyCode::Char('4')));
        assert!(!dash.quitting, "in the chat a q is part of the message");
        assert_eq!(dash.tab, Tab::Chat, "and so is a digit");
        dash.press(key(KeyCode::Tab));
        assert_eq!(dash.tab, Tab::Memory, "Tab leaves the chat");

        let effect = dash.press(key(KeyCode::Char('2')));
        dash.act_on(effect);
        let effect = dash.press(key(KeyCode::Esc));
        dash.act_on(effect);
        assert_eq!(dash.tab, Tab::Overview, "Esc leaves the chat for the overview");

        dash.press(key(KeyCode::Char('q')));
        assert!(dash.quitting);
    }

    #[test]
    fn the_overview_reads_back_through_the_log() {
        let mut dash = dash_with(&[], Vec::new());
        dash.press(key(KeyCode::Up));
        dash.press(key(KeyCode::PageUp));
        assert_eq!(dash.log_back, 11, "up the log is further back from its end");
        dash.press(key(KeyCode::Down));
        assert_eq!(dash.log_back, 10);
        dash.press(key(KeyCode::PageDown));
        dash.press(key(KeyCode::Down));
        assert_eq!(dash.log_back, 0, "scrolling stops at the newest line");
    }

    #[test]
    fn a_memory_archived_from_the_tab_shows_archived_at_once() {
        let memory = |id: &str| MemoryRow {
            id: id.to_string(),
            kind: "observation".to_string(),
            entity: None,
            title: id.to_string(),
            body: String::new(),
            archived: false,
            uses: 0,
            last_used: None,
            updated: "2026-10-02T08:00:00Z".to_string(),
        };
        let mut dash = dash_with(&[], vec![memory("aaaa1111"), memory("bbbb2222")]);
        dash.press(key(KeyCode::Char('3')));
        dash.press(key(KeyCode::Down));
        assert_eq!(dash.press(key(KeyCode::Char('a'))), Effect::Archive { id: "bbbb2222".to_string() });

        assert_eq!(dash.marked("bbbb2222", Ok("Archived “bbbb2222”.".to_string()), true), "Archived “bbbb2222”.");
        assert!(memory_rows_of(&dash.snapshot)[1].archived);
        assert!(!memory_rows_of(&dash.snapshot)[0].archived);
        assert!(dash.marked("aaaa1111", Err(anyhow::anyhow!("locked")), true).starts_with("Couldn't: locked"));
        assert!(!memory_rows_of(&dash.snapshot)[0].archived, "a failed archive changes nothing");
    }
}
