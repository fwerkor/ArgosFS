//! A byte-budgeted, generation-pinned record cache. Borrowing the source
//! prevents mutation of that generation while this cache is alive.
use super::typed::RecordRead;
use super::{ScanPage, ScanWindow, TypedKey};
use crate::error::Result;
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet};

struct Entry {
    generation: u64,
    charge: usize,
    value: Option<Vec<u8>>,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<TypedKey, Entry>,
    order: BTreeSet<(u64, TypedKey)>,
    used: usize,
    next: u64,
    hits: u64,
    misses: u64,
}

impl State {
    fn generation(&mut self) -> u64 {
        if self.next == u64::MAX {
            self.entries.clear();
            self.order.clear();
            self.used = 0;
            self.next = 0;
        }
        self.next += 1;
        self.next
    }

    fn remove(&mut self, key: &TypedKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.order.remove(&(entry.generation, key.clone()));
            self.used -= entry.charge;
        }
    }

    fn insert(&mut self, budget: usize, key: TypedKey, value: Option<Vec<u8>>) {
        // Include duplicate keys, the owned value, and conservative
        // per-record container overhead. Negative entries are charged too.
        let charge = 128usize
            .saturating_add(key.as_bytes().len().saturating_mul(2))
            .saturating_add(value.as_ref().map_or(0, Vec::capacity));
        self.remove(&key);
        if charge > budget {
            return;
        }
        while self.used > budget - charge {
            let Some((_, oldest)) = self.order.first().cloned() else {
                break;
            };
            self.remove(&oldest);
        }
        let generation = self.generation();
        self.order.insert((generation, key.clone()));
        self.entries.insert(
            key,
            Entry {
                generation,
                charge,
                value,
            },
        );
        self.used += charge;
    }

    fn lookup(&mut self, key: &TypedKey) -> Option<Option<Vec<u8>>> {
        let generation = self.generation();
        if let Some(entry) = self.entries.get_mut(key) {
            self.hits = self.hits.saturating_add(1);
            self.order.remove(&(entry.generation, key.clone()));
            entry.generation = generation;
            self.order.insert((generation, key.clone()));
            Some(entry.value.clone())
        } else {
            self.misses = self.misses.saturating_add(1);
            None
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CacheInfo {
    pub entries: usize,
    pub charged_bytes: usize,
    pub budget_bytes: usize,
    pub hits: u64,
    pub misses: u64,
}

pub struct RecordCache<'a, R: RecordRead + ?Sized> {
    source: &'a R,
    budget: usize,
    state: Mutex<State>,
}

impl<'a, R: RecordRead + ?Sized> RecordCache<'a, R> {
    pub fn new(source: &'a R, budget_bytes: usize) -> Self {
        Self {
            source,
            budget: budget_bytes,
            state: Mutex::new(State::default()),
        }
    }

    pub fn info(&self) -> CacheInfo {
        let state = self.state.lock();
        CacheInfo {
            entries: state.entries.len(),
            charged_bytes: state.used,
            budget_bytes: self.budget,
            hits: state.hits,
            misses: state.misses,
        }
    }
}

impl<R: RecordRead + ?Sized> RecordRead for RecordCache<'_, R> {
    fn get_record(&self, key: TypedKey) -> Result<Option<Vec<u8>>> {
        if let Some(value) = self.state.lock().lookup(&key) {
            return Ok(value);
        }
        let value = self.source.get_record(key.clone())?;
        self.state.lock().insert(self.budget, key, value.clone());
        Ok(value)
    }

    fn scan_records(&self, window: &ScanWindow<TypedKey>) -> Result<ScanPage<TypedKey>> {
        // Range scans are intentionally not admitted to the point cache:
        // a large find/scrub must not evict frequently accessed directories.
        self.source.scan_records(window)
    }
}
