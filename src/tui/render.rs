use std::sync::atomic::Ordering;

use chrono::{DateTime, Local, Utc};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use skim::Rank;

use super::util;
use super::App;

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn highlight() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

pub fn format_short_time(ts: i64) -> String {
    let dt: DateTime<Utc> = DateTime::from_timestamp(ts, 0).expect("invalid timestamp");
    let local = dt.with_timezone(&Local);
    let now = Local::now();
    if local.date_naive() == now.date_naive() {
        local.format("%l:%M %p").to_string()
    } else {
        local.format("%Y/%m/%d").to_string()
    }
}

pub fn format_full_time(ts: i64) -> String {
    let dt: DateTime<Utc> = DateTime::from_timestamp(ts, 0).expect("invalid timestamp");
    dt.with_timezone(&Local)
        .format("%Y/%m/%d %l:%M:%S %p")
        .to_string()
}

fn format_time_column(ts: i64) -> String {
    let s = format_short_time(ts);
    format!("{:>11}", s)
}

pub fn centered_rect(width_pct: u16, height_pct: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(popup_layout[1])[1]
}

pub fn group_into_ranges(indices: &[usize]) -> Vec<(usize, usize)> {
    if indices.is_empty() {
        return vec![];
    }
    let mut ranges = vec![];
    let mut start = indices[0];
    let mut end = indices[0];
    for &i in &indices[1..] {
        if i == end + 1 {
            end = i;
        } else {
            ranges.push((start, end + 1));
            start = i;
            end = i;
        }
    }
    ranges.push((start, end + 1));
    ranges
}

fn inject_newlines(spans: Vec<Span<'_>>) -> Vec<Span<'_>> {
    let nl_style = Style::default().fg(Color::Cyan);
    let mut result = vec![];
    for span in spans {
        let mut rest: &str = &span.content;
        let current_style = span.style;
        while let Some(pos) = rest.find('\n') {
            if pos > 0 {
                result.push(Span::styled(rest[..pos].to_string(), current_style));
            }
            result.push(Span::styled("\\n", nl_style));
            rest = &rest[pos + 1..];
        }
        if !rest.is_empty() {
            result.push(Span::styled(rest.to_string(), current_style));
        }
    }
    result
}

pub fn highlight_matches<'a>(
    text: &'a str,
    indices: &[usize],
    base_style: Style,
) -> Vec<Span<'a>> {
    let spans = if indices.is_empty() {
        vec![Span::styled(text, base_style)]
    } else {
        let ranges = group_into_ranges(indices);
        let hl = base_style.patch(highlight());
        let mut spans = vec![];
        let mut last = 0;
        for (start, end) in ranges {
            let start = util::char_boundary_left(text, start);
            let end = util::char_boundary_right(text, end);
            if start > last {
                spans.push(Span::styled(&text[last..start], base_style));
            }
            if end > start {
                spans.push(Span::styled(&text[start..end], hl));
            }
            last = end;
        }
        if last < text.len() {
            spans.push(Span::styled(&text[last..], base_style));
        }
        spans
    };
    inject_newlines(spans)
}

impl App {
    pub fn render(&mut self, f: &mut Frame) {
        let area = f.area();

        let main_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

        self.render_input(f, main_layout[0]);
        self.render_list_and_details(f, main_layout[1]);
        self.render_status(f, main_layout[2]);

        if self.show_overlay {
            self.render_overlay(f, area);
        }
    }

    pub fn render_input(&self, f: &mut Frame, area: Rect) {
        let text = if self.input.is_empty() {
            Line::from(vec![Span::styled(
                "Type to filter...",
                Style::default().fg(Color::DarkGray),
            )])
        } else {
            self.render_input_with_cursor()
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Search")
            .title_style(bold());
        let input = Paragraph::new(text).block(block.clone());
        f.render_widget(input, area);
        if !self.input.is_empty() {
            let inner = block.inner(area);
            f.set_cursor_position((inner.x + self.cursor as u16, inner.y));
        }
    }

    pub fn render_input_with_cursor(&self) -> Line<'_> {
        let before = &self.input[..self.cursor];
        let at = self.input[self.cursor..]
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_default();
        let after = &self.input[self.cursor + at.len()..];
        Line::from(vec![
            Span::raw(before),
            Span::styled(at, Style::default().add_modifier(Modifier::REVERSED)),
            Span::raw(after),
        ])
    }

    pub fn render_list_and_details(&mut self, f: &mut Frame, area: Rect) {
        self.ensure_filtered_len(self.selected.saturating_add(1));

        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        self.render_list(f, split[0]);
        self.render_details(f, split[1]);
    }

    pub fn render_list(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("History")
            .title_style(bold());
        let inner = block.inner(area);
        f.render_widget(block, area);

        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(inner);

        let header = Line::from(vec![Span::styled("       Time  Command", bold())]);
        f.render_widget(Paragraph::new(header), split[0]);

        self.list_height = split[1].height;

        self.ensure_filtered_len(
            (self.scroll as usize + split[1].height as usize).min(self.total_filtered()),
        );

        let entries = self.entries.read().unwrap();
        let items: Vec<ListItem> = self
            .filtered
            .iter()
            .skip(self.scroll as usize)
            .enumerate()
            .take_while(|(i, _)| *i < split[1].height as usize)
            .filter_map(|(i, fe)| {
                entries.get(fe.idx).map(|entry| {
                    let base_style = if i + self.scroll as usize == self.selected {
                        Style::default()
                            .bg(Color::Indexed(234))
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let time = Span::raw(format_time_column(entry.start_time));
                    let cmd_spans = highlight_matches(&entry.argv, &fe.match_indices, base_style);
                    let mut spans = vec![time, Span::raw("  ")];
                    spans.extend(cmd_spans);
                    ListItem::new(Line::from(spans)).style(base_style)
                })
            })
            .collect();

        f.render_widget(List::new(items), split[1]);
    }

    pub fn render_details(&self, f: &mut Frame, area: Rect) {
        let entry = self.selected_entry();
        let filtered = self.selected_filtered();

        let title = entry
            .as_ref()
            .map(|e| format!("Details for {}", e.id))
            .unwrap_or_else(|| "Details".into());

        let mut detail = entry
            .as_ref()
            .map(|entry| {
                let b = |s| Span::styled(s, bold());
                let mut lines: Vec<Line> = vec![
                    Line::from(vec![b("Host:        "), Span::raw(entry.host.clone())]),
                    Line::from(vec![
                        b("Session:     "),
                        Span::raw(entry.session.to_string()),
                    ]),
                    Line::from(vec![b("Directory:   "), Span::raw(entry.dir.clone())]),
                    Line::from(vec![
                        b("Start Time:  "),
                        Span::raw(format_full_time(entry.start_time)),
                    ]),
                ];
                if let Some(fe) = filtered
                    && fe.rank != Rank::default() {
                        lines.push(Line::from(vec![
                            b("Score:       "),
                            Span::raw(format!("{}", fe.rank.score)),
                        ]));
                    }
                lines.extend(vec![
                    Line::from(vec![
                        b("Runtime:     "),
                        Span::raw(entry.duration.map_or("-".into(), |d| format!("{}ms", d))),
                    ]),
                    Line::from(vec![
                        b("Exit Status: "),
                        Span::raw(entry.exit_status.map_or("-".into(), |s| s.to_string())),
                    ]),
                    Line::from(vec![b("Command:     ")]),
                    Line::from(""),
                ]);
                let argv_lines: Vec<Line> = entry
                    .argv
                    .split('\n')
                    .map(|s| Line::from(Span::raw(s.to_string())))
                    .collect();
                lines.into_iter().chain(argv_lines).collect::<Vec<Line>>()
            })
            .unwrap_or_default();

        let visible = area.height.saturating_sub(2) as usize;
        while detail.len() < visible {
            detail.push(Line::from(""));
        }

        let details = Paragraph::new(detail).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(bold()),
        );
        f.render_widget(details, area);
    }

    pub fn render_status(&self, f: &mut Frame, area: Rect) {
        let hc = if !self.selected_hosts.is_empty() { '●' } else { '○' };
        let sc = if !self.selected_sessions.is_empty() { '●' } else { '○' };
        let dc = if !self.selected_dirs.is_empty() { '●' } else { '○' };
        let left = format!(" {hc}h {sc}s {dc}d ");
        let status = Span::styled(
            format!("{left} ↑↓:nav  enter:select  ^c:copy  ^f:filter  esc:quit "),
            Style::default().bg(Color::Indexed(234)),
        );
        let match_count = self.total_filtered();
        let total_entries = self.entries.read().unwrap().len();
        let suffix = if self.loading.load(Ordering::Relaxed) {
            format!(
                " {}/{} (loading...) ",
                self.filtered.len(),
                total_entries
            )
        } else if !self.filter_done && !self.input.trim().is_empty() {
            format!(
                " {}/{} (filtering...) ",
                match_count,
                total_entries
            )
        } else {
            format!(" {}/{} ", match_count, total_entries)
        };
        let count_w = suffix.len() as u16;
        let hlayout = Layout::horizontal([Constraint::Min(0), Constraint::Length(count_w)])
            .split(area);
        let status_line = Line::from(status);
        f.render_widget(Paragraph::new(status_line), hlayout[0]);
        let count_line = Line::from(Span::raw(suffix));
        f.render_widget(Paragraph::new(count_line), hlayout[1]);
    }

    pub fn render_filter_input(
        &self,
        f: &mut Frame,
        area: Rect,
        text: &str,
        cursor: usize,
        active: bool,
        title: &str,
        style: Style,
        highlight: bool,
    ) {
        let c = cursor.min(text.len());
        let display = if text.is_empty() {
            Line::from(vec![Span::styled(
                title,
                Style::default().fg(Color::DarkGray),
            )])
        } else if active {
            let cursor_char = text[c..]
                .chars()
                .next()
                .map(|ch| ch.to_string())
                .unwrap_or_default();
            let before = &text[..c];
            let after = if c + cursor_char.len() <= text.len() {
                &text[c + cursor_char.len()..]
            } else {
                ""
            };
            Line::from(vec![
                Span::styled(before.to_string(), style),
                Span::styled(cursor_char, style.add_modifier(Modifier::REVERSED)),
                Span::styled(after.to_string(), style),
            ])
        } else {
            Line::from(vec![Span::styled(text.to_string(), style)])
        };

        let title_style = if active {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_style(title_style);
        let block = if highlight {
            block.style(Style::default().bg(Color::Indexed(234)))
        } else {
            block
        };
        let inner = block.inner(area);
        f.render_widget(Paragraph::new(display).block(block), area);
        if active && !text.is_empty() {
            f.set_cursor_position((inner.x + c as u16, inner.y));
        }
    }
}
