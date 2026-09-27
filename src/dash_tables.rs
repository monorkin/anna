//! The dashboard's tables: Claude accounts and Jev in the left column, what
//! is going on and what is claimed in the right. Every column gets a width
//! here, and text too long for its column is cut with an ellipsis rather
//! than stopped mid-word at the edge.

use ratatui::layout::Constraint;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Row, Table};

use crate::dash_data::{Account, Limit, Now};

const SPACING: u16 = 1;
const PERCENT: u16 = 7;
const RESETS: u16 = 7;
const THREAD: u16 = 18;
const RUNNING: u16 = 7;
const WAITING: u16 = 7;
const HAND: u16 = 17;
const WORKTREE: u16 = 14;
const HAND_RUNNING: u16 = 8;

/// Room for the Jev table beside the accounts.
pub const JEV_WIDTH: u16 = 36;
const LONGEST_NAME: u16 = 24;

pub fn accounts(accounts: &[Account], width: u16) -> Table<'static> {
    let name_width = width.saturating_sub(3 * (PERCENT + RESETS) + 6 * SPACING).clamp(4, LONGEST_NAME);
    let scoped = accounts.iter().find_map(|it| it.limits[2].as_ref()).map(|it| it.name.clone()).unwrap_or_else(|| "Fable".to_string());
    let header = ["Account".to_string(), "Session".to_string(), "resets".to_string(), "Week".to_string(), "resets".to_string(), scoped, "resets".to_string()];

    let rows = accounts.iter().map(|account| {
        let mut cells = vec![name_cell(account, name_width)];
        for limit in &account.limits {
            cells.extend(limit_cells(limit.as_ref()));
        }
        Row::new(cells)
    });
    let mut widths = vec![Constraint::Length(name_width)];
    widths.extend([PERCENT, RESETS].repeat(3).into_iter().map(Constraint::Length));
    Table::new(rows, widths).header(bold_row(header)).column_spacing(SPACING)
}

pub fn jev(rows: &[[String; 2]]) -> Table<'static> {
    Table::new(rows.iter().map(|it| Row::new(it.clone())), [Constraint::Length(24), Constraint::Length(JEV_WIDTH - 24 - SPACING)])
        .block(Block::new().title(heading("Jev")))
        .column_spacing(SPACING)
}

pub fn going_on(now: &Now, width: u16) -> Table<'static> {
    let fixed = THREAD + RUNNING + WAITING + HAND + WORKTREE + HAND_RUNNING + 6 * SPACING;
    let title = width.saturating_sub(fixed).max(8);
    let widths = [THREAD, RUNNING, WAITING, title, HAND, WORKTREE, HAND_RUNNING];

    let rows: Vec<Row> = if now.going_on.is_empty() {
        vec![Row::new(["Nothing going on right now."])]
    } else {
        now.going_on
            .iter()
            .map(|row| Row::new(row.iter().zip(widths).zip(GOING_ON_STYLES).map(|((text, width), style)| Cell::from(ellipsized(text, width as usize)).style(style(text)))))
            .collect()
    };
    Table::new(rows, widths.map(Constraint::Length))
        .header(bold_row(["Thread", "For", "Waiting", "Title", "Hand", "Worktree", "Hand for"].map(String::from)))
        .block(Block::new().title(heading("Going on now")))
        .column_spacing(SPACING)
}

pub fn claimed(now: &Now, width: u16) -> Table<'static> {
    let title = width.saturating_sub(THREAD + SPACING).max(8);
    let rows: Vec<Row> = if now.claimed.is_empty() {
        vec![Row::new(["Nothing claimed."])]
    } else {
        now.claimed
            .iter()
            .map(|[thread, doing]| Row::new([Cell::from(ellipsized(thread, THREAD as usize)).style(thread_style(thread)), Cell::from(ellipsized(doing, title as usize))]))
            .collect()
    };
    Table::new(rows, [Constraint::Length(THREAD), Constraint::Length(title)])
        .header(bold_row(["Thread", "Title"].map(String::from)))
        .block(Block::new().title(heading("Claimed, between turns")))
        .column_spacing(SPACING)
}

/// How each column of "Going on now" is coloured, by what is in it.
const GOING_ON_STYLES: [fn(&str) -> Style; 7] = [thread_style, age_style, waiting_style, plain_style, hand_style, worktree_style, age_style];

fn thread_style(_: &str) -> Style {
    Style::new().fg(Color::Cyan)
}

/// Green while it's fresh, yellow past half an hour, red past two.
fn age_style(span: &str) -> Style {
    match minutes_in(span) {
        Some(minutes) if minutes < 30 => Style::new().fg(Color::Green),
        Some(minutes) if minutes < 120 => Style::new().fg(Color::Yellow),
        Some(_) => Style::new().fg(Color::Red),
        None => Style::new(),
    }
}

fn waiting_style(count: &str) -> Style {
    if count.parse::<u64>().is_ok_and(|it| it > 0) {
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    }
}

fn plain_style(_: &str) -> Style {
    Style::new()
}

fn hand_style(_: &str) -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn worktree_style(_: &str) -> Style {
    Style::new().fg(Color::Magenta)
}

/// A span as a running Anna gives it — "16s", "58m", "1h 17m", "2d 3h" —
/// in whole minutes; None for anything else.
fn minutes_in(span: &str) -> Option<u64> {
    if span.trim().is_empty() {
        return None;
    }

    let mut minutes = 0;
    for part in span.split_whitespace() {
        let (number, unit) = part.split_at(part.find(|it: char| !it.is_ascii_digit())?);
        let number: u64 = number.parse().ok()?;
        minutes += match unit {
            "d" => number * 24 * 60,
            "h" => number * 60,
            "m" => number,
            "s" => 0,
            _ => return None,
        };
    }
    Some(minutes)
}

fn name_cell(account: &Account, width: u16) -> Cell<'static> {
    let name = ellipsized(&account.name, width.saturating_sub(2) as usize);
    if account.in_use {
        Cell::from(Line::from(vec![Span::raw("● "), Span::raw(name)])).style(Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))
    } else {
        Cell::from(format!("  {name}"))
    }
}

fn limit_cells(limit: Option<&Limit>) -> [Cell<'static>; 2] {
    match limit {
        Some(limit) => [
            Cell::from(format!("{:>4.0}%", limit.percent)).style(Style::new().fg(level_colour(limit.percent))),
            Cell::from(limit.resets_in.clone()).style(Style::new().add_modifier(Modifier::DIM)),
        ],
        None => [Cell::from("    —").style(Style::new().add_modifier(Modifier::DIM)), Cell::from("")],
    }
}

/// The same thresholds ax colours its bars by.
fn level_colour(percent: f64) -> Color {
    if percent >= 90.0 {
        Color::Red
    } else if percent >= 70.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn bold_row<const N: usize>(cells: [String; N]) -> Row<'static> {
    Row::new(cells).style(Style::new().add_modifier(Modifier::BOLD))
}

fn heading(text: &'static str) -> Line<'static> {
    Line::styled(text, Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
}

/// `text` as it fits in `width` characters: whole, or cut with an ellipsis
/// as its last one.
fn ellipsized(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else if width == 0 {
        String::new()
    } else {
        format!("{}…", text.chars().take(width - 1).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_cut_with_an_ellipsis_to_its_width() {
        assert_eq!(ellipsized("Fixing the login", 20), "Fixing the login");
        assert_eq!(ellipsized("Fixing the login", 16), "Fixing the login");
        assert_eq!(ellipsized("Fixing the login", 10), "Fixing th…");
        assert_eq!(ellipsized("Fixing", 1), "…");
        assert_eq!(ellipsized("Fixing", 0), "");
    }

    #[test]
    fn how_long_something_has_run_is_coloured_by_its_age() {
        assert_eq!(minutes_in("16s"), Some(0));
        assert_eq!(minutes_in("1h 17m"), Some(77));
        assert_eq!(minutes_in("2d 3h"), Some(2 * 24 * 60 + 180));
        assert_eq!(minutes_in("-"), None);
        assert_eq!(minutes_in(""), None);
        assert_eq!(minutes_in("5 minutes"), None);

        assert_eq!(age_style("29m").fg, Some(Color::Green));
        assert_eq!(age_style("30m").fg, Some(Color::Yellow));
        assert_eq!(age_style("1h 59m").fg, Some(Color::Yellow));
        assert_eq!(age_style("2h 0m").fg, Some(Color::Red));
        assert_eq!(age_style("-"), Style::new());
    }

    #[test]
    fn only_a_waiting_count_above_nothing_stands_out() {
        assert_eq!(waiting_style("3").fg, Some(Color::Yellow));
        assert_eq!(waiting_style("0"), Style::new());
        assert_eq!(waiting_style(""), Style::new());
    }

    #[test]
    fn a_limit_is_green_then_yellow_then_red_as_it_fills() {
        assert_eq!(level_colour(0.0), Color::Green);
        assert_eq!(level_colour(69.9), Color::Green);
        assert_eq!(level_colour(70.0), Color::Yellow);
        assert_eq!(level_colour(90.0), Color::Red);
    }
}
