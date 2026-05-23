pub mod util;
pub mod filter;
pub mod render;
pub mod overlay;

use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use chrono::Local;
use chrono_english::{Dialect, parse_date_string};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
};
use skim::prelude::*;

use self::filter::{FilteredEntry, FilterHeap, FilterMessage, FilterCriteria};
use self::overlay::{OverlayFocus, OverlayRightFocus, TimeField, OverlayFiltered};
use self::util::osc52_copy;
use crate::db::{self, HistdbInfo, HistoryEntry};

pub struct App {
    pub entries: Arc<Vec<HistoryEntry>>,
    pub filtered: Vec<FilteredEntry>,
    pub selected: usize,
    pub scroll: u16,
    pub list_height: u16,
    pub input: String,
    pub cursor: usize,
    pub running: bool,
    pub output: Option<String>,
    pub entry_rx: Option<mpsc::Receiver<Vec<HistoryEntry>>>,
    pub loading: bool,
    pub filter_token: u64,
    pub filter_rx: mpsc::Receiver<FilterMessage>,
    pub filter_tx: mpsc::Sender<FilterMessage>,
    pub cancel_flag: Arc<AtomicBool>,
    pub heap: FilterHeap,
    pub filter_done: bool,
    pub histdb_info: HistdbInfo,
    pub selected_hosts: HashSet<String>,
    pub selected_sessions: HashSet<i64>,
    pub selected_dirs: HashSet<String>,
    pub restore_entry_idx: Option<usize>,
    pub current_dir: Option<String>,
    pub initial_query: Option<String>,
    pub show_overlay: bool,
    pub overlay_active_item: usize,
    pub overlay_focus: OverlayFocus,
    pub overlay_host_selected: usize,
    pub overlay_dir_selected: usize,
    pub overlay_start_time: String,
    pub overlay_start_cursor: usize,
    pub overlay_end_time: String,
    pub overlay_end_cursor: usize,
    pub overlay_active_time_field: TimeField,
    pub overlay_right_subfocus: OverlayRightFocus,
    pub overlay_host_filter: String,
    pub overlay_host_filter_cursor: usize,
    pub overlay_host_filtered: Vec<OverlayFiltered>,
    pub overlay_dir_filter: String,
    pub overlay_dir_filter_cursor: usize,
    pub overlay_dir_filtered: Vec<OverlayFiltered>,
    pub all_hosts: Vec<String>,
    pub all_dirs: Vec<String>,
}

impl App {
    pub fn new(initial_query: Option<String>) -> Self {
        let (filter_tx, filter_rx) = mpsc::channel();
        let (input, cursor) = match &initial_query {
            Some(q) => {
                let len = q.len();
                (q.clone(), len)
            }
            None => (String::new(), 0),
        };
        App {
            entries: Arc::new(Vec::new()),
            filtered: Vec::new(),
            selected: 0,
            scroll: 0,
            list_height: 0,
            input,
            cursor,
            running: true,
            output: None,
            entry_rx: None,
            loading: true,
            filter_token: 0,
            filter_rx,
            filter_tx,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            heap: FilterHeap::new(),
            filter_done: false,
            histdb_info: HistdbInfo::from_env(),
            selected_hosts: HashSet::new(),
            selected_sessions: HashSet::new(),
            selected_dirs: HashSet::new(),
            restore_entry_idx: None,
            current_dir: std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from)),
            initial_query,
            show_overlay: false,
            overlay_active_item: 0,
            overlay_focus: OverlayFocus::Left,
            overlay_host_selected: 0,
            overlay_dir_selected: 0,
            overlay_start_time: String::new(),
            overlay_start_cursor: 0,
            overlay_end_time: String::new(),
            overlay_end_cursor: 0,
            overlay_active_time_field: TimeField::Start,
            overlay_right_subfocus: OverlayRightFocus::List,
            overlay_host_filter: String::new(),
            overlay_host_filter_cursor: 0,
            overlay_host_filtered: Vec::new(),
            overlay_dir_filter: String::new(),
            overlay_dir_filter_cursor: 0,
            overlay_dir_filtered: Vec::new(),
            all_hosts: Vec::new(),
            all_dirs: Vec::new(),
        }
    }

    pub fn run(&mut self) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.entry_rx = Some(rx);

        let conn = db::open_db(&self.histdb_info).expect("failed to open database");
        std::thread::spawn(move || {
            let _ = db::HistoryEntry::stream_all(&conn, 1000, |chunk| {
                let _ = tx.send(chunk);
            });
        });

        let widget = self.histdb_info.widget;
        if widget {
            self.run_terminal(io::stderr());
        } else {
            let mut terminal = ratatui::init();
            self.event_loop(&mut terminal);
            ratatui::restore();
        }
        self.output.take()
    }

    fn run_terminal(&mut self, mut out: impl io::Write) {
        crossterm::terminal::enable_raw_mode().expect("raw mode");
        crossterm::execute!(out, crossterm::terminal::EnterAlternateScreen).ok();
        let mut terminal = Terminal::new(CrosstermBackend::new(out)).expect("terminal init");
        self.event_loop(&mut terminal);
        crossterm::execute!(
            terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen
        )
        .ok();
        crossterm::terminal::disable_raw_mode().expect("raw mode disable");
    }

    fn event_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<impl io::Write>>) {
        while self.running {
            terminal
                .draw(|f| self.render(f))
                .expect("terminal draw failed");

            if event::poll(Duration::from_millis(50)).expect("poll failed") {
                self.handle_event();
            }

            self.try_load_chunk();
            self.check_filter_results();
        }
    }

    fn try_load_chunk(&mut self) {
        let rx = match self.entry_rx.take() {
            Some(rx) => rx,
            None => return,
        };
        let old_len = self.entries.len();
        let mut done = false;
        loop {
            match rx.try_recv() {
                Ok(chunk) => {
                    if chunk.is_empty() {
                        done = true;
                    } else {
                        Arc::make_mut(&mut self.entries).extend(chunk);
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if !done {
            self.entry_rx = Some(rx);
        }
        self.loading = !done;
        if done && self.input.trim().is_empty() {
            self.filter_done = true;
        }
        if self.entries.len() > old_len {
            let mut hosts_changed = false;
            let mut dirs_changed = false;
            for entry in &self.entries[old_len..] {
                if !self.all_hosts.contains(&entry.host) {
                    self.all_hosts.push(entry.host.clone());
                    hosts_changed = true;
                }
                if !self.all_dirs.contains(&entry.dir) {
                    self.all_dirs.push(entry.dir.clone());
                    dirs_changed = true;
                }
            }
            if hosts_changed && self.show_overlay {
                self.update_host_filter();
            }
            if dirs_changed && self.show_overlay {
                self.update_dir_filter();
            }
            if self.input.trim().is_empty() {
                let criteria = self.active_filter_criteria();
                self.filtered.extend(
                    (old_len..self.entries.len())
                        .filter(|idx| criteria.matches(&self.entries[*idx]))
                        .map(|idx| FilteredEntry {
                            idx,
                            match_indices: Vec::new(),
                            rank: Rank::default(),
                            start_time: self.entries[idx].start_time,
                        }),
                );
            } else {
                self.filter_entries();
            }
        }
    }

    fn parse_time_input(input: &str, end_of_day: bool) -> Option<i64> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return None;
        }
        let dt = parse_date_string(trimmed, Local::now(), Dialect::Us).ok()?;
        if end_of_day && dt.time().format("%H:%M:%S").to_string() == "00:00:00" {
            return Some(dt.timestamp() + 86_399);
        }
        Some(dt.timestamp())
    }

    fn active_filter_criteria(&self) -> FilterCriteria {
        FilterCriteria {
            hosts: self.selected_hosts.iter().cloned().collect(),
            sessions: self.selected_sessions.iter().cloned().collect(),
            dirs: self.selected_dirs.iter().cloned().collect(),
            start_time: Self::parse_time_input(&self.overlay_start_time, false),
            end_time: Self::parse_time_input(&self.overlay_end_time, true),
        }
    }

    fn total_filtered(&self) -> usize {
        if self.filter_done {
            self.filtered.len() + self.heap.len()
        } else {
            self.heap.len()
        }
    }

    fn ensure_filtered_len(&mut self, needed: usize) {
        while self.filtered.len() < needed {
            if let Some(fe) = self.heap.pop() {
                self.filtered.push(fe);
            } else {
                break;
            }
        }
    }

    fn restore_selection(&mut self) {
        let target = match self.restore_entry_idx.take() {
            Some(idx) => idx,
            None => return,
        };
        if let Some(pos) = self.filtered.iter().position(|fe| fe.idx == target) {
            self.selected = pos;
            return;
        }
        let mut drained = Vec::new();
        let mut found = None;
        while let Some(fe) = self.heap.pop() {
            if fe.idx == target {
                found = Some(fe);
                break;
            }
            drained.push(fe);
        }
        for fe in drained {
            self.heap.push(fe);
        }
        if let Some(fe) = found {
            self.filtered.push(fe);
            self.selected = self.filtered.len() - 1;
        }
    }

    fn clamp_filter_state(&mut self) {
        let total = self.total_filtered();
        if total == 0 {
            self.selected = 0;
            self.scroll = 0;
        } else {
            if self.selected >= total {
                self.selected = total - 1;
            }
            let visible = self.list_height as usize;
            if self.selected < self.scroll as usize {
                self.scroll = self.selected as u16;
            } else if self.selected >= self.scroll as usize + visible {
                self.scroll = (self.selected - visible + 1) as u16;
            }
        }
    }

    fn selected_filtered(&self) -> Option<&FilteredEntry> {
        self.filtered.get(self.selected)
    }

    fn selected_entry(&self) -> Option<&HistoryEntry> {
        self.selected_filtered()
            .and_then(|fe| self.entries.get(fe.idx))
    }

    fn move_selection(&mut self, delta: i32) {
        let total = self.total_filtered();
        if total == 0 {
            return;
        }
        let new = self.selected as i32 + delta;
        self.selected = new.clamp(0, total as i32 - 1) as usize;
        let visible = self.list_height as usize;
        if self.selected < self.scroll as usize {
            self.scroll = self.selected as u16;
        } else if self.selected >= self.scroll as usize + visible {
            self.scroll = (self.selected - visible + 1) as u16;
        }
    }

    fn handle_event(&mut self) {
        if let Ok(event) = event::read() {
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if self.show_overlay {
                        self.handle_overlay_event(key);
                        return;
                    }
                    match key.code {
                        KeyCode::Esc => {
                            self.output = self
                                .histdb_info
                                .widget
                                .then(|| self.initial_query.clone())
                                .flatten();
                            self.running = false;
                        }
                        KeyCode::Enter => {
                            self.ensure_filtered_len(self.selected + 1);
                            if let Some(entry) = self.selected_entry() {
                                self.output = self
                                    .histdb_info
                                    .widget
                                    .then(|| Some(entry.argv.clone()))
                                    .flatten();
                                self.running = false;
                            }
                        }
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            self.ensure_filtered_len(self.selected + 1);
                            if let Some(entry) = self.selected_entry() {
                                osc52_copy(&entry.argv);
                            }
                        }
                        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            self.show_overlay = true;
                            self.overlay_focus = OverlayFocus::Left;
                            self.overlay_right_subfocus = OverlayRightFocus::List;
                            self.overlay_host_filter.clear();
                            self.overlay_host_filter_cursor = 0;
                            self.overlay_dir_filter.clear();
                            self.overlay_dir_filter_cursor = 0;
                            self.update_host_filter();
                            self.update_dir_filter();
                        }
                        KeyCode::Up => self.move_selection(-1),
                        KeyCode::Down => self.move_selection(1),
                        KeyCode::PageUp => {
                            let page = (self.list_height as i32).max(1);
                            self.move_selection(-page);
                        }
                        KeyCode::PageDown => {
                            let page = (self.list_height as i32).max(1);
                            self.move_selection(page);
                        }
                        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL)
                            && self.histdb_info.host.is_some() => {
                                let entry_idx = self.selected_filtered().map(|fe| fe.idx);
                                let host = self.histdb_info.host.clone().unwrap();
                                if self.selected_hosts.contains(&host) {
                                    self.selected_hosts.remove(&host);
                                } else {
                                    self.selected_hosts.insert(host);
                                }
                                if let Some(idx) = entry_idx {
                                    self.restore_entry_idx = Some(idx);
                                }
                                self.filter_entries();
                            }
                        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL)
                            && self.histdb_info.session.is_some() => {
                                let entry_idx = self.selected_filtered().map(|fe| fe.idx);
                                let session = self.histdb_info.session.unwrap();
                                if self.selected_sessions.contains(&session) {
                                    self.selected_sessions.remove(&session);
                                } else {
                                    self.selected_sessions.insert(session);
                                }
                                if let Some(idx) = entry_idx {
                                    self.restore_entry_idx = Some(idx);
                                }
                                self.filter_entries();
                            }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            let entry_idx = self.selected_filtered().map(|fe| fe.idx);
                            if let Some(ref dir) = self.current_dir {
                                let dir = dir.clone();
                                if self.selected_dirs.contains(&dir) {
                                    self.selected_dirs.remove(&dir);
                                } else {
                                    self.selected_dirs.insert(dir);
                                }
                            }
                            if let Some(idx) = entry_idx {
                                self.restore_entry_idx = Some(idx);
                            }
                            self.filter_entries();
                        }
                        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::ALT) => {
                            let host = self.selected_entry().map(|e| e.host.clone());
                            let entry_idx = self.selected_filtered().map(|fe| fe.idx);
                            if let (Some(host), Some(entry_idx)) = (host, entry_idx) {
                                if self.selected_hosts.contains(&host) {
                                    self.selected_hosts.remove(&host);
                                } else {
                                    self.selected_hosts.insert(host);
                                }
                                self.restore_entry_idx = Some(entry_idx);
                                self.filter_entries();
                            }
                        }
                        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::ALT) => {
                            let session = self.selected_entry().map(|e| e.session);
                            let entry_idx = self.selected_filtered().map(|fe| fe.idx);
                            if let (Some(session), Some(entry_idx)) = (session, entry_idx) {
                                if self.selected_sessions.contains(&session) {
                                    self.selected_sessions.remove(&session);
                                } else {
                                    self.selected_sessions.insert(session);
                                }
                                self.restore_entry_idx = Some(entry_idx);
                                self.filter_entries();
                            }
                        }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::ALT) => {
                            let dir = self.selected_entry().map(|e| e.dir.clone());
                            let entry_idx = self.selected_filtered().map(|fe| fe.idx);
                            if let (Some(dir), Some(entry_idx)) = (dir, entry_idx) {
                                if self.selected_dirs.contains(&dir) {
                                    self.selected_dirs.remove(&dir);
                                } else {
                                    self.selected_dirs.insert(dir);
                                }
                                self.restore_entry_idx = Some(entry_idx);
                                self.filter_entries();
                            }
                        }
                        KeyCode::Char(c)
                            if !key.modifiers.contains(KeyModifiers::CONTROL)
                                && !key.modifiers.contains(KeyModifiers::ALT) =>
                        {
                            self.input.insert(self.cursor, c);
                            self.cursor += c.len_utf8();
                            self.filter_entries();
                        }
                        KeyCode::Backspace
                            if self.cursor > 0 => {
                                let pos = util::char_boundary_left(&self.input, self.cursor - 1);
                                self.input.remove(pos);
                                self.cursor = pos;
                                self.filter_entries();
                            }
                        KeyCode::Delete
                            if self.cursor < self.input.len() => {
                                self.input.remove(self.cursor);
                                self.filter_entries();
                            }
                        KeyCode::Left
                            if self.cursor > 0 => {
                                self.cursor = util::char_boundary_left(&self.input, self.cursor - 1);
                            }
                        KeyCode::Right
                            if self.cursor < self.input.len() => {
                                self.cursor = util::char_boundary_right(&self.input, self.cursor + 1);
                            }
                        KeyCode::Home => self.cursor = 0,
                        KeyCode::End => self.cursor = self.input.len(),
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            self.input.clear();
                            self.cursor = 0;
                            self.filter_entries();
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use self::filter::{FilterHeap, FilterMessage, FilterCriteria, run_filter_streaming};
    use self::render::{highlight_matches, group_into_ranges};
    use ratatui::style::Style;

    fn cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn entry(id: i64, start_time: i64, argv: &str) -> HistoryEntry {
        HistoryEntry {
            id,
            session: 1,
            exit_status: Some(0),
            start_time,
            duration: Some(100),
            argv: argv.into(),
            host: "host1".into(),
            dir: "/tmp".into(),
        }
    }

    fn run_filter_test(query: &str, entries: &[HistoryEntry]) -> Vec<FilteredEntry> {
        let (tx, rx) = mpsc::channel();
        run_filter_streaming(
            query,
            entries,
            &AtomicBool::new(false),
            1,
            &tx,
            &FilterCriteria::none(),
        );
        drop(tx);
        let mut heap = FilterHeap::new();
        while let Ok(msg) = rx.recv() {
            if let FilterMessage::Batch(_, batch) = msg {
                heap.extend(batch);
            }
        }
        heap.drain()
    }

    fn run_filter(
        query: &str,
        entries: &[HistoryEntry],
        _cancel: &AtomicBool,
    ) -> Vec<FilteredEntry> {
        if query.trim().is_empty() {
            return entries
                .iter()
                .enumerate()
                .map(|(idx, _)| FilteredEntry {
                    idx,
                    match_indices: vec![],
                    rank: Rank::default(),
                    start_time: entries[idx].start_time,
                })
                .collect();
        }
        run_filter_test(query, entries)
    }

    #[test]
    fn empty_query_returns_all() {
        let entries = vec![entry(1, 300, "bar"), entry(2, 200, "foo")];
        let result = run_filter("", &entries, &cancel());
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn fuzzy_match_basic() {
        let entries = vec![
            entry(1, 300, "git commit"),
            entry(2, 200, "cargo build"),
            entry(3, 100, "cargo test"),
        ];
        let result = run_filter_test("cargo", &entries);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn sort_by_score_best_first() {
        let entries = vec![
            entry(1, 300, "zzzzzzzz cargo zzzzzzzz"),
            entry(2, 200, "cargo"),
            entry(3, 100, "car"),
        ];
        let result = run_filter_test("cargo", &entries);
        assert_eq!(entries[result[0].idx].argv, "cargo");
    }

    #[test]
    fn no_match_returns_empty() {
        let entries = vec![entry(1, 300, "ls"), entry(2, 200, "cd")];
        let result = run_filter_test("zzzznomatch", &entries);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn highlight_utf8_boundaries() {
        let entries = vec![entry(1, 300, "python3 sso.py …token…")];
        let result = run_filter_test("token", &entries);
        assert_eq!(result.len(), 1);
        let spans = highlight_matches(&entries[0].argv, &result[0].match_indices, Style::default());
        assert!(!spans.is_empty());
    }

    #[test]
    fn group_ranges() {
        let ranges = group_into_ranges(&[0, 1, 2, 5, 6, 9]);
        assert_eq!(ranges, vec![(0, 3), (5, 7), (9, 10)]);
    }

    #[test]
    fn cancellation_discards_stale() {
        let entries = Arc::new(
            (0..200_000)
                .map(|i| {
                    let argv = if i == 500 {
                        "unique_target_command".into()
                    } else {
                        format!("cmd_{i} foo bar")
                    };
                    entry(i as i64, 200_000 - i as i64, &argv)
                })
                .collect::<Vec<_>>(),
        );
        let (tx, rx) = mpsc::channel();

        let token_a = 1u64;
        let tx_a = tx.clone();
        let entries_a = entries.clone();
        let cancel_a = Arc::new(AtomicBool::new(false));
        let cancel_a2 = cancel_a.clone();
        std::thread::spawn(move || {
            run_filter_streaming(
                "foo",
                &entries_a,
                &cancel_a2,
                token_a,
                &tx_a,
                &FilterCriteria::none(),
            );
        });

        cancel_a.store(true, Ordering::SeqCst);
        let token_b = 2u64;
        let entries_b = entries.clone();
        let cancel_b = AtomicBool::new(false);
        std::thread::spawn(move || {
            run_filter_streaming(
                "unique_target",
                &entries_b,
                &cancel_b,
                token_b,
                &tx,
                &FilterCriteria::none(),
            );
        });

        let mut heap = FilterHeap::new();
        while let Ok(msg) = rx.recv() {
            match msg {
                FilterMessage::Batch(t, batch) if t == token_b => heap.extend(batch),
                FilterMessage::Done(t) if t == token_b => {}
                _ => {}
            }
        }
        let v: Vec<FilteredEntry> = heap.drain();
        assert_eq!(v.len(), 1);
        assert_eq!(entries[v[0].idx].argv, "unique_target_command");
    }

    #[test]
    fn filter_latency_smoke() {
        let entries: Vec<HistoryEntry> = (0..200_000)
            .map(|i| {
                entry(
                    i as i64,
                    1000 - i as i64,
                    &format!("cmd_{i} with args --flag"),
                )
            })
            .collect();
        let start = std::time::Instant::now();
        let result = run_filter_test("cmd with", &entries);
        let elapsed = start.elapsed();
        assert!(!result.is_empty());
        assert!(elapsed.as_millis() < 5000, "took {}ms", elapsed.as_millis());
        eprintln!(
            "latency: 200k matched {} in {}µs",
            result.len(),
            elapsed.as_micros()
        );
    }

    #[test]
    fn heap_pops_best_scores_first() {
        let entries = vec![
            entry(1, 100, "bad match with low score stuff"),
            entry(2, 200, "excellent match"),
            entry(3, 300, "medium match"),
        ];
        let (tx, rx) = mpsc::channel();
        run_filter_streaming(
            "match",
            &entries,
            &AtomicBool::new(false),
            1,
            &tx,
            &FilterCriteria::none(),
        );
        drop(tx);
        let mut heap = FilterHeap::new();
        while let Ok(msg) = rx.recv() {
            if let FilterMessage::Batch(_, batch) = msg {
                heap.extend(batch);
            }
        }
        let mut prev_score = i32::MAX;
        for fe in heap.drain() {
            assert!(
                fe.rank.score <= prev_score,
                "heap popped score {} after {}, ordering broken",
                fe.rank.score,
                prev_score
            );
            prev_score = fe.rank.score;
        }
    }

    #[test]
    fn heap_newer_first_on_score_tie() {
        let entries = vec![
            entry(1, 1000, "older match entry"),
            entry(2, 2000, "newer match entry"),
        ];
        let (tx, rx) = mpsc::channel();
        run_filter_streaming(
            "match",
            &entries,
            &AtomicBool::new(false),
            1,
            &tx,
            &FilterCriteria::none(),
        );
        drop(tx);
        let mut heap = FilterHeap::new();
        while let Ok(msg) = rx.recv() {
            if let FilterMessage::Batch(_, batch) = msg {
                heap.extend(batch);
            }
        }
        let results: Vec<FilteredEntry> = heap.drain();
        assert_eq!(results.len(), 2);
        if results[0].rank.score == results[1].rank.score {
            assert!(
                results[0].start_time > results[1].start_time,
                "same score: expected newer first. Got start_times {} then {}",
                results[0].start_time,
                results[1].start_time,
            );
        }
        assert!(
            results[0].rank.score >= results[1].rank.score,
            "first entry score {} < second entry score {}",
            results[0].rank.score,
            results[1].rank.score,
        );
    }
}
