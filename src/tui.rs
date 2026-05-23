use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use chrono_english::{Dialect, parse_date_string};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use rayon::prelude::*;
use skim::prelude::*;

use crate::db::{self, HistdbInfo, HistoryEntry};

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[((triple >> 18) & 0x3f) as usize] as char);
        out.push(B64[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn osc52_copy(text: &str) {
    use io::Write;
    let b64 = base64_encode(text.as_bytes());
    let mut stderr = io::stderr();
    let _ = write!(stderr, "\x1b]52;c;{}\x07", b64);
    let _ = stderr.flush();
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn highlight() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

fn format_short_time(ts: i64) -> String {
    let dt: DateTime<Utc> = DateTime::from_timestamp(ts, 0).expect("invalid timestamp");
    let local = dt.with_timezone(&Local);
    let now = Local::now();
    if local.date_naive() == now.date_naive() {
        local.format("%-I:%M %p").to_string()
    } else {
        local.format("%Y/%m/%d").to_string()
    }
}

fn format_full_time(ts: i64) -> String {
    let dt: DateTime<Utc> = DateTime::from_timestamp(ts, 0).expect("invalid timestamp");
    dt.with_timezone(&Local)
        .format("%Y/%m/%d %-I:%M:%S %p")
        .to_string()
}

fn format_time_column(ts: i64) -> String {
    let s = format_short_time(ts);
    format!("{:>11}", s)
}

struct EntryItem {
    entry: HistoryEntry,
}

impl SkimItem for EntryItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.entry.argv)
    }
}

struct FilterItem {
    text: String,
}

impl SkimItem for FilterItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }
}

struct OverlayFiltered {
    idx: usize,
    match_indices: Vec<usize>,
}

enum FilterMessage {
    Batch(u64, Vec<FilteredEntry>),
    Done(u64),
}

const RANK_CRITERIA: &[RankCriteria] =
    &[RankCriteria::Score, RankCriteria::Begin, RankCriteria::End];

#[derive(Clone)]
struct FilteredEntry {
    idx: usize,
    match_indices: Vec<usize>,
    rank: Rank,
    start_time: i64,
}

impl PartialOrd for FilteredEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FilteredEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Max-heap: larger order = pops first. sort_key gives smaller-is-better,
        // so reverse a/b. Tiebreak on start_time (newer first).
        other
            .rank
            .sort_key(RANK_CRITERIA)
            .cmp(&self.rank.sort_key(RANK_CRITERIA))
            .then_with(|| self.start_time.cmp(&other.start_time))
    }
}

impl PartialEq for FilteredEntry {
    fn eq(&self, other: &Self) -> bool {
        self.rank.sort_key(RANK_CRITERIA) == other.rank.sort_key(RANK_CRITERIA)
            && self.start_time == other.start_time
    }
}

impl Eq for FilteredEntry {}

struct FilterHeap {
    heap: std::collections::BinaryHeap<FilteredEntry>,
}

impl FilterHeap {
    fn new() -> Self {
        Self {
            heap: std::collections::BinaryHeap::new(),
        }
    }

    fn clear(&mut self) {
        self.heap.clear();
    }

    fn extend(&mut self, batch: Vec<FilteredEntry>) {
        self.heap.extend(batch);
    }

    #[allow(dead_code)]
    fn push(&mut self, fe: FilteredEntry) {
        self.heap.push(fe);
    }

    fn pop(&mut self) -> Option<FilteredEntry> {
        self.heap.pop()
    }

    fn len(&self) -> usize {
        self.heap.len()
    }

    fn top_n(&mut self, n: usize) -> Vec<FilteredEntry> {
        let mut top = Vec::with_capacity(n);
        for _ in 0..n {
            if let Some(fe) = self.heap.pop() {
                top.push(fe);
            } else {
                break;
            }
        }
        for fe in &top {
            self.heap.push(fe.clone());
        }
        top
    }

    #[allow(dead_code)]
    fn drain(&mut self) -> Vec<FilteredEntry> {
        std::iter::from_fn(|| self.heap.pop()).collect()
    }
}

struct FilterCriteria {
    hosts: Vec<String>,
    sessions: Vec<i64>,
    dirs: Vec<String>,
    start_time: Option<i64>,
    end_time: Option<i64>,
}

impl FilterCriteria {
    #[allow(dead_code)]
    fn none() -> Self {
        FilterCriteria {
            hosts: Vec::new(),
            sessions: Vec::new(),
            dirs: Vec::new(),
            start_time: None,
            end_time: None,
        }
    }

    fn matches(&self, entry: &HistoryEntry) -> bool {
        if !self.hosts.is_empty() && !self.hosts.contains(&entry.host) {
            return false;
        }
        if !self.sessions.is_empty() && !self.sessions.contains(&entry.session) {
            return false;
        }
        if !self.dirs.is_empty() && !self.dirs.contains(&entry.dir) {
            return false;
        }
        if let Some(start) = self.start_time {
            if entry.start_time < start {
                return false;
            }
        }
        if let Some(end) = self.end_time {
            if entry.start_time > end {
                return false;
            }
        }
        true
    }
}

#[derive(PartialEq, Eq)]
enum OverlayFocus {
    Left,
    Right,
}

#[derive(PartialEq, Eq)]
enum TimeField {
    Start,
    End,
}

#[derive(PartialEq, Eq)]
enum OverlayRightFocus {
    Filter,
    List,
}

pub struct App {
    entries: Arc<Vec<HistoryEntry>>,
    filtered: Vec<FilteredEntry>,
    selected: usize,
    scroll: u16,
    list_height: u16,
    input: String,
    cursor: usize,
    running: bool,
    output: Option<String>,
    entry_rx: Option<mpsc::Receiver<Vec<HistoryEntry>>>,
    loading: bool,
    filter_token: u64,
    filter_rx: mpsc::Receiver<FilterMessage>,
    filter_tx: mpsc::Sender<FilterMessage>,
    cancel_flag: Arc<AtomicBool>,
    heap: FilterHeap,
    filter_done: bool,
    histdb_info: HistdbInfo,
    selected_hosts: HashSet<String>,
    selected_sessions: HashSet<i64>,
    selected_dirs: HashSet<String>,
    restore_entry_idx: Option<usize>,
    current_dir: Option<String>,
    initial_query: Option<String>,
    show_overlay: bool,
    overlay_active_item: usize,
    overlay_focus: OverlayFocus,
    overlay_host_selected: usize,
    overlay_dir_selected: usize,
    overlay_start_time: String,
    overlay_start_cursor: usize,
    overlay_end_time: String,
    overlay_end_cursor: usize,
    overlay_active_time_field: TimeField,
    overlay_right_subfocus: OverlayRightFocus,
    overlay_host_filter: String,
    overlay_host_filter_cursor: usize,
    overlay_host_filtered: Vec<OverlayFiltered>,
    overlay_dir_filter: String,
    overlay_dir_filter_cursor: usize,
    overlay_dir_filtered: Vec<OverlayFiltered>,
    all_hosts: Vec<String>,
    all_dirs: Vec<String>,
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

    fn filter_entries(&mut self) {
        let criteria = self.active_filter_criteria();
        let query = self.input.trim();
        if query.is_empty() {
            self.heap.clear();
            self.filter_done = true;
            self.filtered = (0..self.entries.len())
                .filter(|idx| criteria.matches(&self.entries[*idx]))
                .map(|idx| FilteredEntry {
                    idx,
                    match_indices: Vec::new(),
                    rank: Rank::default(),
                    start_time: self.entries[idx].start_time,
                })
                .collect();
            self.filter_token += 1;
            self.restore_selection();
            self.clamp_filter_state();
            return;
        }

        self.cancel_flag.store(true, Ordering::SeqCst);
        self.cancel_flag = Arc::new(AtomicBool::new(false));
        self.heap.clear();
        self.filter_done = false;

        self.filter_token += 1;
        let token = self.filter_token;
        let query = query.to_string();
        let entries = self.entries.clone();
        let tx = self.filter_tx.clone();
        let cancel = self.cancel_flag.clone();

        std::thread::spawn(move || {
            run_filter_streaming(&query, &entries, &cancel, token, &tx, &criteria);
        });
    }

    fn check_filter_results(&mut self) {
        let mut received = false;
        loop {
            match self.filter_rx.try_recv() {
                Ok(msg) => {
                    let token = match &msg {
                        FilterMessage::Batch(t, _) => *t,
                        FilterMessage::Done(t) => *t,
                    };
                    if token == self.filter_token {
                        match msg {
                            FilterMessage::Done(_) => {
                                self.filter_done = true;
                                // Clear filtered (heap still has everything from
                                // the peek push-back). Lazy-pop from heap as
                                // the user scrolls.
                                self.filtered.clear();
                                self.restore_selection();
                                self.clamp_filter_state();
                            }
                            FilterMessage::Batch(_, batch) => {
                                self.heap.extend(batch);
                                received = true;
                            }
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if received && !self.filter_done {
            let n = (self.list_height as usize).max(20);
            self.filtered = self.heap.top_n(n);
            self.clamp_filter_state();
        }
    }

    fn total_filtered(&self) -> usize {
        if self.filter_done {
            self.filtered.len() + self.heap.len()
        } else {
            // During filtering, filtered holds clones of peeked entries; heap
            // has the canonical set (including the peeked ones).
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
                                let pos = char_boundary_left(&self.input, self.cursor - 1);
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
                                self.cursor = char_boundary_left(&self.input, self.cursor - 1);
                            }
                        KeyCode::Right
                            if self.cursor < self.input.len() => {
                                self.cursor = char_boundary_right(&self.input, self.cursor + 1);
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

    fn handle_overlay_event(&mut self, key: crossterm::event::KeyEvent) {
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
                } else if self.overlay_focus == OverlayFocus::Right && self.overlay_active_item == 2 {
                    match self.overlay_active_time_field {
                        TimeField::Start => self.overlay_active_time_field = TimeField::End,
                        TimeField::End => self.overlay_active_time_field = TimeField::Start,
                    }
                } else if self.overlay_focus == OverlayFocus::Right
                    && (self.overlay_active_item == 0 || self.overlay_active_item == 1)
                {
                    match self.overlay_active_item {
                        0 => {
                            if let Some(item) = self.overlay_host_filtered.get(self.overlay_host_selected) {
                                if let Some(host) = self.all_hosts.get(item.idx) {
                                    if self.selected_hosts.contains(host) {
                                        self.selected_hosts.remove(host);
                                    } else {
                                        self.selected_hosts.insert(host.clone());
                                    }
                                    self.filter_entries();
                                }
                            }
                        }
                        1 => {
                            if let Some(item) = self.overlay_dir_filtered.get(self.overlay_dir_selected) {
                                if let Some(dir) = self.all_dirs.get(item.idx) {
                                    if self.selected_dirs.contains(dir) {
                                        self.selected_dirs.remove(dir);
                                    } else {
                                        self.selected_dirs.insert(dir.clone());
                                    }
                                    self.filter_entries();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            KeyCode::Up => {
                match self.overlay_focus {
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
                }
            }
            KeyCode::Down => {
                match self.overlay_focus {
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
                }
            }
            KeyCode::Char(' ') => {
                if self.overlay_focus == OverlayFocus::Right
                    && self.overlay_active_item == 2
                {
                    self.handle_overlay_time_input(key);
                }
            }
            _ => {
                if in_filter_subfocus {
                    self.handle_overlay_filter_input(key);
                } else if self.overlay_focus == OverlayFocus::Right
                    && self.overlay_active_item == 2
                {
                    self.handle_overlay_time_input(key);
                }
            }
        }
    }

    fn handle_overlay_time_input(&mut self, key: crossterm::event::KeyEvent) {
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
                    let pos = char_boundary_left(input, *cursor - 1);
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
                *cursor = char_boundary_left(input, *cursor - 1);
            }
            KeyCode::Right if *cursor < input.len() => {
                *cursor = char_boundary_right(input, *cursor + 1);
            }
            _ => {}
        }
    }

    fn handle_overlay_filter_input(&mut self, key: crossterm::event::KeyEvent) {
        let (filter, cursor, active_item) = match self.overlay_active_item {
            0 => (&mut self.overlay_host_filter, &mut self.overlay_host_filter_cursor, 0),
            1 => (&mut self.overlay_dir_filter, &mut self.overlay_dir_filter_cursor, 1),
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
                    let pos = char_boundary_left(filter, *cursor - 1);
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
                *cursor = char_boundary_left(filter, *cursor - 1);
            }
            KeyCode::Right if *cursor < filter.len() => {
                *cursor = char_boundary_right(filter, *cursor + 1);
            }
            _ => {}
        }
    }

    fn update_host_filter(&mut self) {
        let filter = self.overlay_host_filter.trim();
        if filter.is_empty() {
            let mut selected = Vec::new();
            let mut rest = Vec::new();
            for i in 0..self.all_hosts.len() {
                let entry = OverlayFiltered {
                    idx: i,
                    match_indices: Vec::new(),
                };
                if self.all_hosts.get(i).is_some_and(|h| self.selected_hosts.contains(h)) {
                    selected.push(entry);
                } else {
                    rest.push(entry);
                }
            }
            selected.append(&mut rest);
            self.overlay_host_filtered = selected;
        } else {
            let factory = AndOrEngineFactory::new(ExactOrFuzzyEngineFactory::builder().build());
            let engine = factory.create_engine(filter);
            let items: Vec<Arc<dyn SkimItem>> = self
                .all_hosts
                .iter()
                .map(|h| Arc::new(FilterItem { text: h.clone() }) as Arc<dyn SkimItem>)
                .collect();
            let mut results: Vec<(usize, Vec<usize>, Rank)> = items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    engine.match_item(item.as_ref()).map(|r| {
                        let indices = r.range_char_indices(&item.text());
                        (i, indices, r.rank)
                    })
                })
                .collect();
            results.sort_by(|a, b| a.2.sort_key(RANK_CRITERIA).cmp(&b.2.sort_key(RANK_CRITERIA)));
            self.overlay_host_filtered = results
                .into_iter()
                .map(|(i, mi, _)| OverlayFiltered {
                    idx: i,
                    match_indices: mi,
                })
                .collect();
        }
        self.overlay_host_selected = 0;
    }

    fn update_dir_filter(&mut self) {
        let filter = self.overlay_dir_filter.trim();
        if filter.is_empty() {
            let mut selected = Vec::new();
            let mut rest = Vec::new();
            for i in 0..self.all_dirs.len() {
                let entry = OverlayFiltered {
                    idx: i,
                    match_indices: Vec::new(),
                };
                if self.all_dirs.get(i).is_some_and(|d| self.selected_dirs.contains(d)) {
                    selected.push(entry);
                } else {
                    rest.push(entry);
                }
            }
            selected.append(&mut rest);
            self.overlay_dir_filtered = selected;
        } else {
            let factory = AndOrEngineFactory::new(ExactOrFuzzyEngineFactory::builder().build());
            let engine = factory.create_engine(filter);
            let items: Vec<Arc<dyn SkimItem>> = self
                .all_dirs
                .iter()
                .map(|d| Arc::new(FilterItem { text: d.clone() }) as Arc<dyn SkimItem>)
                .collect();
            let mut results: Vec<(usize, Vec<usize>, Rank)> = items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    engine.match_item(item.as_ref()).map(|r| {
                        let indices = r.range_char_indices(&item.text());
                        (i, indices, r.rank)
                    })
                })
                .collect();
            results.sort_by(|a, b| a.2.sort_key(RANK_CRITERIA).cmp(&b.2.sort_key(RANK_CRITERIA)));
            self.overlay_dir_filtered = results
                .into_iter()
                .map(|(i, mi, _)| OverlayFiltered {
                    idx: i,
                    match_indices: mi,
                })
                .collect();
        }
        self.overlay_dir_selected = 0;
    }

    fn render(&mut self, f: &mut Frame) {
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

    fn render_input(&self, f: &mut Frame, area: Rect) {
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

    fn render_input_with_cursor(&self) -> Line<'_> {
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

    fn render_list_and_details(&mut self, f: &mut Frame, area: Rect) {
        self.ensure_filtered_len(self.selected.saturating_add(1));

        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        self.render_list(f, split[0]);
        self.render_details(f, split[1]);
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect) {
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

        let items: Vec<ListItem> = self
            .filtered
            .iter()
            .skip(self.scroll as usize)
            .enumerate()
            .take_while(|(i, _)| *i < split[1].height as usize)
            .filter_map(|(i, fe)| {
                self.entries.get(fe.idx).map(|entry| {
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

    fn render_details(&self, f: &mut Frame, area: Rect) {
        let entry = self.selected_entry();
        let filtered = self.selected_filtered();

        let title = entry
            .map(|e| format!("Details for {}", e.id))
            .unwrap_or_else(|| "Details".into());

        let mut detail = entry
            .map(|entry| {
                let b = |s| Span::styled(s, bold());
                let mut lines: Vec<Line> = vec![
                    Line::from(vec![b("Host:        "), Span::raw(entry.host.as_str())]),
                    Line::from(vec![
                        b("Session:     "),
                        Span::raw(entry.session.to_string()),
                    ]),
                    Line::from(vec![b("Directory:   "), Span::raw(entry.dir.as_str())]),
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
                lines
                    .into_iter()
                    .chain(entry.argv.split('\n').map(|s| Line::from(Span::raw(s))))
                    .collect::<Vec<Line>>()
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

    fn render_status(&self, f: &mut Frame, area: Rect) {
        let hc = if !self.selected_hosts.is_empty() { '●' } else { '○' };
        let sc = if !self.selected_sessions.is_empty() { '●' } else { '○' };
        let dc = if !self.selected_dirs.is_empty() { '●' } else { '○' };
        let left = format!(" {hc}h {sc}s {dc}d ");
        let status = Span::styled(
            format!("{left} ↑↓:nav  enter:select  ^c:copy  ^f:filter  esc:quit "),
            Style::default().bg(Color::Indexed(234)),
        );
        let match_count = self.total_filtered();
        let suffix = if self.loading {
            format!(
                " {}/{} (loading...) ",
                self.filtered.len(),
                self.entries.len()
            )
        } else if !self.filter_done && !self.input.trim().is_empty() {
            format!(
                " {}/{} (filtering...) ",
                match_count,
                self.entries.len()
            )
        } else {
            format!(" {}/{} ", match_count, self.entries.len())
        };
        let count_w = suffix.len() as u16;
        let hlayout = Layout::horizontal([Constraint::Min(0), Constraint::Length(count_w)])
            .split(area);
        let status_line = Line::from(status);
        f.render_widget(Paragraph::new(status_line), hlayout[0]);
        let count_line = Line::from(Span::raw(suffix));
        f.render_widget(Paragraph::new(count_line), hlayout[1]);
    }

    fn render_overlay(&self, f: &mut Frame, area: Rect) {
        let popup_area = centered_rect(70, 65, area);
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
            .constraints([
                Constraint::Percentage(30),
                Constraint::Percentage(70),
            ])
            .split(inner);

        self.render_overlay_left(f, split[0]);
        match self.overlay_active_item {
            0 => self.render_overlay_hosts(f, split[1]),
            1 => self.render_overlay_dirs(f, split[1]),
            2 => self.render_overlay_time(f, split[1]),
            _ => {}
        }
    }

    fn render_overlay_left(&self, f: &mut Frame, area: Rect) {
        let active = Style::default()
            .bg(Color::Indexed(234))
            .add_modifier(Modifier::BOLD);
        let items: Vec<ListItem> = ["Host", "Directory", "Time Range"]
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let prefix = if self.overlay_focus == OverlayFocus::Left
                    && i == self.overlay_active_item
                {
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
                ListItem::new(Line::from(Span::styled(
                    format!("{prefix}{label}"),
                    style,
                )))
            })
            .collect();
        let block = Block::default()
            .borders(Borders::RIGHT)
            .style(Style::default());
        f.render_widget(List::new(items).block(block), area);
    }

    fn render_overlay_hosts(&self, f: &mut Frame, area: Rect) {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);

        let is_filter_active = self.overlay_focus == OverlayFocus::Right
            && self.overlay_active_item == 0
            && self.overlay_right_subfocus == OverlayRightFocus::Filter;

        self.render_filter_input(f, split[0], &self.overlay_host_filter, self.overlay_host_filter_cursor, is_filter_active, "Host", Style::default(), false);

        let list_area = split[1];
        let visible = list_area.height.saturating_sub(1) as usize;
        let filtered = &self.overlay_host_filtered;
        let total = filtered.len();
        let start = self
            .overlay_host_selected
            .saturating_sub(visible.saturating_sub(1));
        let items: Vec<ListItem> = filtered
            .iter()
            .skip(start)
            .take(visible)
            .enumerate()
            .filter_map(|(i, item)| {
                self.all_hosts.get(item.idx).map(|host| {
                    let checked = self.selected_hosts.contains(host);
                    let marker = if checked { "[x] " } else { "[ ] " };
                    let is_selected = self.overlay_focus == OverlayFocus::Right
                        && (start + i) == self.overlay_host_selected;
                    let sel_style = if is_selected {
                        Style::default()
                            .bg(Color::Indexed(234))
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let mut spans = vec![Span::styled(marker, sel_style)];
                    spans.extend(highlight_matches(host, &item.match_indices, sel_style));
                    ListItem::new(Line::from(spans))
                })
            })
            .collect();
        let count = format!("{total} hosts");
        let block = Block::default()
            .borders(Borders::NONE)
            .title(count)
            .title_style(bold());
        f.render_widget(List::new(items).block(block), list_area);
    }

    fn render_overlay_dirs(&self, f: &mut Frame, area: Rect) {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);

        let is_filter_active = self.overlay_focus == OverlayFocus::Right
            && self.overlay_active_item == 1
            && self.overlay_right_subfocus == OverlayRightFocus::Filter;

        self.render_filter_input(f, split[0], &self.overlay_dir_filter, self.overlay_dir_filter_cursor, is_filter_active, "Dir", Style::default(), false);

        let list_area = split[1];
        let visible = list_area.height.saturating_sub(1) as usize;
        let filtered = &self.overlay_dir_filtered;
        let total = filtered.len();
        let start = self
            .overlay_dir_selected
            .saturating_sub(visible.saturating_sub(1));
        let items: Vec<ListItem> = filtered
            .iter()
            .skip(start)
            .take(visible)
            .enumerate()
            .filter_map(|(i, item)| {
                self.all_dirs.get(item.idx).map(|dir| {
                    let checked = self.selected_dirs.contains(dir);
                    let marker = if checked { "[x] " } else { "[ ] " };
                    let is_selected = self.overlay_focus == OverlayFocus::Right
                        && (start + i) == self.overlay_dir_selected;
                    let sel_style = if is_selected {
                        Style::default()
                            .bg(Color::Indexed(234))
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let mut spans = vec![Span::styled(marker, sel_style)];
                    spans.extend(highlight_matches(dir, &item.match_indices, sel_style));
                    ListItem::new(Line::from(spans))
                })
            })
            .collect();
        let count = format!("{total} dirs");
        let block = Block::default()
            .borders(Borders::NONE)
            .title(count)
            .title_style(bold());
        f.render_widget(List::new(items).block(block), list_area);
    }

    fn render_overlay_time(&self, f: &mut Frame, area: Rect) {
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
            if start_focused { s.add_modifier(Modifier::BOLD) } else { s }
        } else {
            let s = Style::default().fg(Color::Red);
            if start_focused { s.add_modifier(Modifier::BOLD) } else { s }
        };
        let end_focused = self.overlay_focus == OverlayFocus::Right
            && self.overlay_active_time_field == TimeField::End;
        let end_style = if self.overlay_end_time.is_empty() {
            Style::default()
        } else if Self::parse_time_input(&self.overlay_end_time, true).is_some() {
            let s = Style::default().fg(Color::Green);
            if end_focused { s.add_modifier(Modifier::BOLD) } else { s }
        } else {
            let s = Style::default().fg(Color::Red);
            if end_focused { s.add_modifier(Modifier::BOLD) } else { s }
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

    fn render_filter_input(&self, f: &mut Frame, area: Rect, text: &str, cursor: usize, active: bool, title: &str, style: Style, highlight: bool) {
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

fn centered_rect(width_pct: u16, height_pct: u16, r: Rect) -> Rect {
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

fn group_into_ranges(indices: &[usize]) -> Vec<(usize, usize)> {
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

fn highlight_matches<'a>(text: &'a str, indices: &[usize], base_style: Style) -> Vec<Span<'a>> {
    let spans = if indices.is_empty() {
        vec![Span::styled(text, base_style)]
    } else {
        let ranges = group_into_ranges(indices);
        let hl = base_style.patch(highlight());
        let mut spans = vec![];
        let mut last = 0;
        for (start, end) in ranges {
            let start = char_boundary_left(text, start);
            let end = char_boundary_right(text, end);
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

fn char_boundary_left(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn char_boundary_right(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

fn run_filter_streaming(
    query: &str,
    entries: &[HistoryEntry],
    cancel: &AtomicBool,
    token: u64,
    tx: &mpsc::Sender<FilterMessage>,
    criteria: &FilterCriteria,
) {
    if query.trim().is_empty() {
        return;
    }
    let factory = AndOrEngineFactory::new(ExactOrFuzzyEngineFactory::builder().build());
    let engine = factory.create_engine(query.trim());
    let batch_size = 5_000;

    for batch_start in (0..entries.len()).step_by(batch_size) {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let batch_end = (batch_start + batch_size).min(entries.len());
        let batch = &entries[batch_start..batch_end];
        let items: Vec<Arc<dyn SkimItem>> = batch
            .iter()
            .map(|e| Arc::new(EntryItem { entry: e.clone() }) as Arc<dyn SkimItem>)
            .collect();

        let batch_results: Vec<FilteredEntry> = items
            .par_iter()
            .enumerate()
            .filter_map(|(i, item)| {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                if !criteria.matches(&batch[i]) {
                    return None;
                }
                let result = engine.match_item(item.as_ref())?;
                let text = batch[i].argv.as_str();
                let match_indices = result.range_char_indices(text);
                Some(FilteredEntry {
                    idx: batch_start + i,
                    match_indices,
                    rank: result.rank,
                    start_time: batch[i].start_time,
                })
            })
            .collect();

        let _ = tx.send(FilterMessage::Batch(token, batch_results));
    }
    let _ = tx.send(FilterMessage::Done(token));
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Verify max-heap: highest scores pop first
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
        // Pop and verify scores are descending
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
        // Entries that both contain "match" — scores should be similar.
        // Newer entry (higher start_time) should pop first when scores tie.
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
        // If scores are equal, newer (higher start_time) should be first.
        if results[0].rank.score == results[1].rank.score {
            assert!(
                results[0].start_time > results[1].start_time,
                "same score: expected newer first. Got start_times {} then {}",
                results[0].start_time,
                results[1].start_time,
            );
        }
        // Regardless, first entry should not have a worse score.
        assert!(
            results[0].rank.score >= results[1].rank.score,
            "first entry score {} < second entry score {}",
            results[0].rank.score,
            results[1].rank.score,
        );
    }

}
