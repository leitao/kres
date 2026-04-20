//! Token/cost accounting, per-role and per-model.
//!
//! The `/cost` printed per-role accumulated token
//! usage from every API round. Same shape here, made concurrency-safe
//! by keeping the accumulator under a Mutex.

use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UsageEntry {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub calls: u64,
}

impl UsageEntry {
    pub fn add(&mut self, other: &UsageEntry) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.calls = self.calls.saturating_add(other.calls);
    }
}

/// Key under which we accumulate: (role, model).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UsageKey {
    pub role: String,
    pub model: String,
}

#[derive(Debug, Default)]
pub struct UsageTracker {
    inner: Mutex<BTreeMap<UsageKey, UsageEntry>>,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(
        &self,
        role: impl Into<String>,
        model: impl Into<String>,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
    ) {
        let key = UsageKey {
            role: role.into(),
            model: model.into(),
        };
        let mut guard = self.inner.lock().unwrap();
        let entry = guard.entry(key).or_default();
        entry.add(&UsageEntry {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            calls: 1,
        });
    }

    pub fn snapshot(&self) -> Vec<(UsageKey, UsageEntry)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }

    pub fn totals(&self) -> UsageEntry {
        let g = self.inner.lock().unwrap();
        let mut total = UsageEntry::default();
        for v in g.values() {
            total.add(v);
        }
        total
    }

    pub fn reset(&self) {
        self.inner.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_sums() {
        let t = UsageTracker::new();
        t.record("fast", "opus-4-7", 100, 20, 0, 0);
        t.record("fast", "opus-4-7", 50, 10, 0, 0);
        t.record("slow", "opus-4-7", 1000, 500, 0, 0);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 2);
        let fast = snap.iter().find(|(k, _)| k.role == "fast").unwrap().1;
        assert_eq!(fast.input_tokens, 150);
        assert_eq!(fast.output_tokens, 30);
        assert_eq!(fast.calls, 2);
        let total = t.totals();
        assert_eq!(total.input_tokens, 1150);
        assert_eq!(total.output_tokens, 530);
        assert_eq!(total.calls, 3);
    }

    #[test]
    fn reset_clears() {
        let t = UsageTracker::new();
        t.record("x", "m", 10, 10, 0, 0);
        t.reset();
        assert_eq!(t.totals().calls, 0);
    }

    #[test]
    fn concurrent_records_do_not_lose_counts() {
        use std::sync::Arc;
        use std::thread;
        let t = Arc::new(UsageTracker::new());
        let mut hs = vec![];
        for _ in 0..8 {
            let t2 = t.clone();
            hs.push(thread::spawn(move || {
                for _ in 0..100 {
                    t2.record("fast", "m", 1, 1, 0, 0);
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(t.totals().calls, 800);
    }
}
