//! Text coloured for a terminal, as `anna log` and ax's usage bars write it,
//! turned into what ratatui draws, so the dashboard shows their colours
//! without drawing them a second time. Only the SGR codes they use are
//! known; any other escape is dropped and its text kept.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub fn line_of(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut style = Style::new();
    let mut rest = text;
    while let Some(at) = rest.find("\x1b[") {
        if at > 0 {
            spans.push(Span::styled(rest[..at].to_string(), style));
        }
        let after = &rest[at + 2..];
        match after.find('m') {
            Some(end) => {
                style = styled_by(style, &after[..end]);
                rest = &after[end + 1..];
            }
            None => rest = "",
        }
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), style));
    }
    Line::from(spans)
}

fn styled_by(style: Style, codes: &str) -> Style {
    codes.split(';').fold(style, |style, code| match code {
        "0" | "" => Style::new(),
        "1" => style.add_modifier(Modifier::BOLD),
        "2" => style.add_modifier(Modifier::DIM),
        "31" => style.fg(Color::Red),
        "32" => style.fg(Color::Green),
        "33" => style.fg(Color::Yellow),
        "35" => style.fg(Color::Magenta),
        "36" => style.fg(Color::Cyan),
        _ => style,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_codes_become_styled_spans_and_plain_text_stays_plain() {
        let line = line_of("\x1b[2m10:00:00\x1b[0m  \x1b[31mbroker.refused\x1b[0m  tool=Read");
        let spans: Vec<(&str, Style)> = line.spans.iter().map(|it| (it.content.as_ref(), it.style)).collect();
        assert_eq!(
            spans,
            [
                ("10:00:00", Style::new().add_modifier(Modifier::DIM)),
                ("  ", Style::new()),
                ("broker.refused", Style::new().fg(Color::Red)),
                ("  tool=Read", Style::new()),
            ]
        );

        assert_eq!(line_of("session 40% used").spans.len(), 1);
        assert_eq!(line_of("\x1b[1;32mok").spans[0].style, Style::new().add_modifier(Modifier::BOLD).fg(Color::Green));
        assert_eq!(line_of("cut \x1b[3").to_string(), "cut ", "an escape without its end is dropped");
        assert_eq!(line_of("\x1b[4mplain").spans[0].style, Style::new(), "an unknown code changes nothing");
    }
}
