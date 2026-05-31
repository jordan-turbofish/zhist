use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use skim::prelude::*;

use super::App;
use super::filter::RANK_CRITERIA;
use super::render::{self, bold, highlight_matches};
use super::util;

pub struct FilterItem {
    text: String,
}

impl SkimItem for FilterItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }
}

#[derive(Clone)]
pub struct OverlayFiltered {
    pub idx: usize,
    pub match_indices: Vec<usize>,
}

#[derive(PartialEq, Eq)]
pub enum OverlayFocus {
    Left,
    Right,
}

#[derive(PartialEq, Eq)]
pub enum TimeField {
    Start,
    End,
}

#[derive(PartialEq, Eq)]
pub enum OverlayRightFocus {
    Filter,
    List,
}

fn update_generic_filter(
    filter: &str,
    all_items: &[String],
    selected_set: &HashSet<String>,
) -> Vec<OverlayFiltered> {
    let filter = filter.trim();
    if filter.is_empty() {
        let mut selected = Vec::new();
        let mut rest = Vec::new();
        for i in 0..all_items.len() {
            let entry = OverlayFiltered {
                idx: i,
                match_indices: Vec::new(),
            };
            if all_items
                .get(i)
                .is_some_and(|item| selected_set.contains(item))
            {
                selected.push(entry);
            } else {
                rest.push(entry);
            }
        }
        selected.append(&mut rest);
        selected
    } else {
        let factory = AndOrEngineFactory::new(ExactOrFuzzyEngineFactory::builder().build());
        let engine = factory.create_engine(filter);
        let skim_items: Vec<Arc<dyn SkimItem>> = all_items
            .iter()
            .map(|s| Arc::new(FilterItem { text: s.clone() }) as Arc<dyn SkimItem>)
            .collect();
        let mut results: Vec<(usize, Vec<usize>, Rank)> = skim_items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                engine.match_item(item.as_ref()).map(|r| {
                    let indices = r.range_char_indices(&item.text());
                    (i, indices, r.rank)
                })
            })
            .collect();
        results.sort_by(|a, b| {
            a.2.sort_key(RANK_CRITERIA)
                .cmp(&b.2.sort_key(RANK_CRITERIA))
        });
        results
            .into_iter()
            .map(|(i, mi, _)| OverlayFiltered {
                idx: i,
                match_indices: mi,
            })
            .collect()
    }
}

fn toggle_selection(
    filtered: &[OverlayFiltered],
    selected_idx: usize,
    all_items: &[String],
    selected_set: &mut HashSet<String>,
) -> bool {
    if let Some(item) = filtered.get(selected_idx) {
        if let Some(name) = all_items.get(item.idx) {
            if selected_set.contains(name) {
                selected_set.remove(name);
            } else {
                selected_set.insert(name.clone());
            }
            return true;
        }
    }
    false
}

impl App {
    pub fn handle_overlay_event(&mut self, key: crossterm::event::KeyEvent) {
        let in_filter_subfocus = self.overlay_focus == OverlayFocus::Right
            && (self.overlay_active_item == 0 || self.overlay_active_item == 1)
            && self.overlay_right_subfocus == OverlayRightFocus::Filter;

        match key.code {
            KeyCode::Esc => {
                self.show_overlay = false;
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.show_overlay = false;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if self.overlay_focus == OverlayFocus::Left {
                    self.overlay_focus = OverlayFocus::Right;
                    if self.overlay_active_item == 0 || self.overlay_active_item == 1 {
                        self.overlay_right_subfocus = OverlayRightFocus::Filter;
                    }
                } else {
                    self.overlay_focus = OverlayFocus::Left;
                }
            }
            KeyCode::Enter => {
                if self.overlay_focus == OverlayFocus::Left {
                    self.overlay_focus = OverlayFocus::Right;
                    if self.overlay_active_item == 0 || self.overlay_active_item == 1 {
                        self.overlay_right_subfocus = OverlayRightFocus::Filter;
                    }
                } else if self.overlay_focus == OverlayFocus::Right && self.overlay_active_item == 2
                {
                    match self.overlay_active_time_field {
                        TimeField::Start => self.overlay_active_time_field = TimeField::End,
                        TimeField::End => self.overlay_active_time_field = TimeField::Start,
                    }
                } else if self.overlay_focus == OverlayFocus::Right
                    && (self.overlay_active_item == 0 || self.overlay_active_item == 1)
                {
                    let should_filter = match self.overlay_active_item {
                        0 => toggle_selection(
                            &self.overlay_host_filtered,
                            self.overlay_host_selected,
                            &self.all_hosts,
                            &mut self.selected_hosts,
                        ),
                        1 => toggle_selection(
                            &self.overlay_dir_filtered,
                            self.overlay_dir_selected,
                            &self.all_dirs,
                            &mut self.selected_dirs,
                        ),
                        _ => false,
                    };
                    if should_filter {
                        self.filter_entries();
                    }
                }
            }
            KeyCode::Up => match self.overlay_focus {
                OverlayFocus::Left => {
                    if self.overlay_active_item > 0 {
                        self.overlay_active_item -= 1;
                        self.overlay_right_subfocus = OverlayRightFocus::Filter;
                    }
                }
                OverlayFocus::Right => match self.overlay_active_item {
                    0 => {
                        if self.overlay_host_selected > 0 {
                            self.overlay_host_selected -= 1;
                        }
                    }
                    1 => {
                        if self.overlay_dir_selected > 0 {
                            self.overlay_dir_selected -= 1;
                        }
                    }
                    2 => {
                        self.overlay_active_time_field = TimeField::End;
                    }
                    _ => {}
                },
            },
            KeyCode::Down => match self.overlay_focus {
                OverlayFocus::Left => {
                    if self.overlay_active_item < 2 {
                        self.overlay_active_item += 1;
                        self.overlay_right_subfocus = OverlayRightFocus::Filter;
                    }
                }
                OverlayFocus::Right => match self.overlay_active_item {
                    0 => {
                        let max = self.overlay_host_filtered.len().saturating_sub(1);
                        if self.overlay_host_selected < max {
                            self.overlay_host_selected += 1;
                        }
                    }
                    1 => {
                        let max = self.overlay_dir_filtered.len().saturating_sub(1);
                        if self.overlay_dir_selected < max {
                            self.overlay_dir_selected += 1;
                        }
                    }
                    2 => {
                        self.overlay_active_time_field = TimeField::Start;
                    }
                    _ => {}
                },
            },
            KeyCode::Char(' ') => {
                if self.overlay_focus == OverlayFocus::Right && self.overlay_active_item == 2 {
                    self.handle_overlay_time_input(key);
                }
            }
            _ => {
                if in_filter_subfocus {
                    self.handle_overlay_filter_input(key);
                } else if self.overlay_focus == OverlayFocus::Right && self.overlay_active_item == 2
                {
                    self.handle_overlay_time_input(key);
                }
            }
        }
    }

    pub fn handle_overlay_time_input(&mut self, key: crossterm::event::KeyEvent) {
        let (input, cursor) = match self.overlay_active_time_field {
            TimeField::Start => (&mut self.overlay_start_time, &mut self.overlay_start_cursor),
            TimeField::End => (&mut self.overlay_end_time, &mut self.overlay_end_cursor),
        };
        match key.code {
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.clear();
                *cursor = 0;
                self.filter_entries();
            }
            KeyCode::Char(c) => {
                input.insert(*cursor, c);
                *cursor += c.len_utf8();
                self.filter_entries();
            }
            KeyCode::Backspace if *cursor > 0 => {
                let pos = util::char_boundary_left(input, *cursor - 1);
                input.remove(pos);
                *cursor = pos;
                self.filter_entries();
            }
            KeyCode::Delete if *cursor < input.len() => {
                input.remove(*cursor);
                self.filter_entries();
            }
            KeyCode::Home => *cursor = 0,
            KeyCode::End => *cursor = input.len(),
            KeyCode::Left if *cursor > 0 => {
                *cursor = util::char_boundary_left(input, *cursor - 1);
            }
            KeyCode::Right if *cursor < input.len() => {
                *cursor = util::char_boundary_right(input, *cursor + 1);
            }
            _ => {}
        }
    }

    pub fn handle_overlay_filter_input(&mut self, key: crossterm::event::KeyEvent) {
        let (filter, cursor, active_item) = match self.overlay_active_item {
            0 => (
                &mut self.overlay_host_filter,
                &mut self.overlay_host_filter_cursor,
                0,
            ),
            1 => (
                &mut self.overlay_dir_filter,
                &mut self.overlay_dir_filter_cursor,
                1,
            ),
            _ => return,
        };
        match key.code {
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                filter.clear();
                *cursor = 0;
                if active_item == 0 {
                    self.update_host_filter();
                } else {
                    self.update_dir_filter();
                }
            }
            KeyCode::Char(c) => {
                filter.insert(*cursor, c);
                *cursor += c.len_utf8();
                if active_item == 0 {
                    self.update_host_filter();
                } else {
                    self.update_dir_filter();
                }
            }
            KeyCode::Backspace if *cursor > 0 => {
                let pos = util::char_boundary_left(filter, *cursor - 1);
                filter.remove(pos);
                *cursor = pos;
                if active_item == 0 {
                    self.update_host_filter();
                } else {
                    self.update_dir_filter();
                }
            }
            KeyCode::Delete if *cursor < filter.len() => {
                filter.remove(*cursor);
                if active_item == 0 {
                    self.update_host_filter();
                } else {
                    self.update_dir_filter();
                }
            }
            KeyCode::Home => *cursor = 0,
            KeyCode::End => *cursor = filter.len(),
            KeyCode::Left if *cursor > 0 => {
                *cursor = util::char_boundary_left(filter, *cursor - 1);
            }
            KeyCode::Right if *cursor < filter.len() => {
                *cursor = util::char_boundary_right(filter, *cursor + 1);
            }
            _ => {}
        }
    }

    pub fn update_host_filter(&mut self) {
        self.overlay_host_filtered = update_generic_filter(
            &self.overlay_host_filter,
            &self.all_hosts,
            &self.selected_hosts,
        );
        self.overlay_host_selected = 0;
    }

    pub fn update_dir_filter(&mut self) {
        self.overlay_dir_filtered = update_generic_filter(
            &self.overlay_dir_filter,
            &self.all_dirs,
            &self.selected_dirs,
        );
        self.overlay_dir_selected = 0;
    }

    pub fn render_overlay(&self, f: &mut Frame, area: Rect) {
        let popup_area = render::centered_rect(70, 65, area);
        f.render_widget(Clear, popup_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Filter")
            .title_style(bold())
            .border_style(Style::default().fg(Color::Cyan));
        f.render_widget(block.clone(), popup_area);

        let inner = block.inner(popup_area);
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(inner);

        self.render_overlay_left(f, split[0]);
        match self.overlay_active_item {
            0 => self.render_overlay_hosts(f, split[1]),
            1 => self.render_overlay_dirs(f, split[1]),
            2 => self.render_overlay_time(f, split[1]),
            _ => {}
        }
    }

    pub fn render_overlay_left(&self, f: &mut Frame, area: Rect) {
        let active = Style::default()
            .bg(Color::Indexed(234))
            .add_modifier(Modifier::BOLD);
        let items: Vec<ListItem> = ["Host", "Directory", "Time Range"]
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let prefix =
                    if self.overlay_focus == OverlayFocus::Left && i == self.overlay_active_item {
                        " ▶ "
                    } else if i == self.overlay_active_item {
                        " ▸ "
                    } else {
                        "   "
                    };
                let style = if i == self.overlay_active_item {
                    active
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(format!("{prefix}{label}"), style)))
            })
            .collect();
        let block = Block::default()
            .borders(Borders::RIGHT)
            .style(Style::default());
        f.render_widget(List::new(items).block(block), area);
    }

    fn render_overlay_filter_list(
        &self,
        f: &mut Frame,
        area: Rect,
        active_item: usize,
        filter_text: &str,
        cursor: usize,
        title: &str,
        filtered: &[OverlayFiltered],
        selected_index: usize,
        all_items: &[String],
        selected_set: &HashSet<String>,
        count_label: &str,
    ) {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);

        let is_filter_active = self.overlay_focus == OverlayFocus::Right
            && self.overlay_active_item == active_item
            && self.overlay_right_subfocus == OverlayRightFocus::Filter;

        self.render_filter_input(
            f,
            split[0],
            filter_text,
            cursor,
            is_filter_active,
            title,
            Style::default(),
            false,
        );

        let list_area = split[1];
        let visible = list_area.height.saturating_sub(1) as usize;
        let total = filtered.len();
        let start = selected_index.saturating_sub(visible.saturating_sub(1));
        let items: Vec<ListItem> = filtered
            .iter()
            .skip(start)
            .take(visible)
            .enumerate()
            .filter_map(|(i, item)| {
                all_items.get(item.idx).map(|name| {
                    let checked = selected_set.contains(name);
                    let marker = if checked { "[x] " } else { "[ ] " };
                    let is_selected =
                        self.overlay_focus == OverlayFocus::Right && (start + i) == selected_index;
                    let sel_style = if is_selected {
                        Style::default()
                            .bg(Color::Indexed(234))
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let mut spans = vec![Span::styled(marker, sel_style)];
                    spans.extend(highlight_matches(name, &item.match_indices, sel_style));
                    ListItem::new(Line::from(spans))
                })
            })
            .collect();
        let count = format!("{total} {count_label}");
        let block = Block::default()
            .borders(Borders::NONE)
            .title(count)
            .title_style(bold());
        f.render_widget(List::new(items).block(block), list_area);
    }

    pub fn render_overlay_hosts(&self, f: &mut Frame, area: Rect) {
        self.render_overlay_filter_list(
            f,
            area,
            0,
            &self.overlay_host_filter,
            self.overlay_host_filter_cursor,
            "Host",
            &self.overlay_host_filtered,
            self.overlay_host_selected,
            &self.all_hosts,
            &self.selected_hosts,
            "hosts",
        );
    }

    pub fn render_overlay_dirs(&self, f: &mut Frame, area: Rect) {
        self.render_overlay_filter_list(
            f,
            area,
            1,
            &self.overlay_dir_filter,
            self.overlay_dir_filter_cursor,
            "Dir",
            &self.overlay_dir_filtered,
            self.overlay_dir_selected,
            &self.all_dirs,
            &self.selected_dirs,
            "dirs",
        );
    }

    pub fn render_overlay_time(&self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::NONE)
            .title("Time Range")
            .title_style(bold());
        let inner = block.inner(area);
        f.render_widget(block, area);

        let start_focused = self.overlay_focus == OverlayFocus::Right
            && self.overlay_active_time_field == TimeField::Start;
        let start_style = if self.overlay_start_time.is_empty() {
            Style::default()
        } else if Self::parse_time_input(&self.overlay_start_time, false).is_some() {
            let s = Style::default().fg(Color::Green);
            if start_focused {
                s.add_modifier(Modifier::BOLD)
            } else {
                s
            }
        } else {
            let s = Style::default().fg(Color::Red);
            if start_focused {
                s.add_modifier(Modifier::BOLD)
            } else {
                s
            }
        };
        let end_focused = self.overlay_focus == OverlayFocus::Right
            && self.overlay_active_time_field == TimeField::End;
        let end_style = if self.overlay_end_time.is_empty() {
            Style::default()
        } else if Self::parse_time_input(&self.overlay_end_time, true).is_some() {
            let s = Style::default().fg(Color::Green);
            if end_focused {
                s.add_modifier(Modifier::BOLD)
            } else {
                s
            }
        } else {
            let s = Style::default().fg(Color::Red);
            if end_focused {
                s.add_modifier(Modifier::BOLD)
            } else {
                s
            }
        };

        let hint = Line::from(Span::styled(
            "Format: YYYY-MM-DD or YYYY-MM-DD HH:MM",
            Style::default().fg(Color::DarkGray),
        ));

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(inner);

        self.render_filter_input(
            f,
            layout[0],
            &self.overlay_start_time,
            self.overlay_start_cursor,
            start_focused,
            "Start",
            start_style,
            start_focused,
        );
        self.render_filter_input(
            f,
            layout[1],
            &self.overlay_end_time,
            self.overlay_end_cursor,
            end_focused,
            "End",
            end_style,
            end_focused,
        );
        f.render_widget(Paragraph::new(hint), layout[2]);
    }
}
