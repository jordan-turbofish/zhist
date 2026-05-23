use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use rayon::prelude::*;
use skim::prelude::*;

use crate::db::HistoryEntry;
use super::App;

struct EntryItem {
    entry: HistoryEntry,
}

impl SkimItem for EntryItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.entry.argv)
    }
}

pub const RANK_CRITERIA: &[RankCriteria] =
    &[RankCriteria::Score, RankCriteria::Begin, RankCriteria::End];

#[derive(Clone)]
pub struct FilteredEntry {
    pub idx: usize,
    pub match_indices: Vec<usize>,
    pub rank: Rank,
    pub start_time: i64,
}

impl PartialOrd for FilteredEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FilteredEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
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

pub struct FilterHeap {
    heap: std::collections::BinaryHeap<FilteredEntry>,
}

impl FilterHeap {
    pub fn new() -> Self {
        Self {
            heap: std::collections::BinaryHeap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.heap.clear();
    }

    pub fn extend(&mut self, batch: Vec<FilteredEntry>) {
        self.heap.extend(batch);
    }

    #[allow(dead_code)]
    pub fn push(&mut self, fe: FilteredEntry) {
        self.heap.push(fe);
    }

    pub fn pop(&mut self) -> Option<FilteredEntry> {
        self.heap.pop()
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn top_n(&mut self, n: usize) -> Vec<FilteredEntry> {
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
    pub fn drain(&mut self) -> Vec<FilteredEntry> {
        std::iter::from_fn(|| self.heap.pop()).collect()
    }
}

pub enum FilterMessage {
    Batch(u64, Vec<FilteredEntry>),
    Done(u64),
}

#[derive(Clone)]
pub struct FilterCriteria {
    pub hosts: Vec<String>,
    pub sessions: Vec<i64>,
    pub dirs: Vec<String>,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
}

impl FilterCriteria {
    #[allow(dead_code)]
    pub fn none() -> Self {
        FilterCriteria {
            hosts: Vec::new(),
            sessions: Vec::new(),
            dirs: Vec::new(),
            start_time: None,
            end_time: None,
        }
    }

    pub fn matches(&self, entry: &HistoryEntry) -> bool {
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

pub fn run_filter_streaming(
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

impl App {
    pub fn filter_entries(&mut self) {
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
        let entries = (*self.entries).clone();
        let tx = self.filter_tx.clone();
        let cancel = self.cancel_flag.clone();

        std::thread::spawn(move || {
            run_filter_streaming(&query, &entries, &cancel, token, &tx, &criteria);
        });
    }

    pub fn check_filter_results(&mut self) {
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
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        if received && !self.filter_done {
            let n = (self.list_height as usize).max(20);
            self.filtered = self.heap.top_n(n);
            self.clamp_filter_state();
        }
    }
}
