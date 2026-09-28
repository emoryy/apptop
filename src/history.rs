use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::model::Node;

const STEP: Duration = Duration::from_secs(10);
const KEEP: Duration = Duration::from_secs(30 * 60);
/// window of the Δ column
pub const TREND: Duration = Duration::from_secs(5 * 60);

/// Memory of every row over time, keyed by the row key, at STEP resolution.
#[derive(Default)]
pub struct History {
    series: HashMap<String, VecDeque<(Instant, u64)>>,
}

impl History {
    pub fn update(&mut self, roots: &mut [Node], now: Instant) {
        let mut seen = HashSet::new();
        for r in roots {
            self.walk(r, now, &mut seen);
        }
        self.series.retain(|k, _| seen.contains(k));
    }

    fn walk(&mut self, n: &mut Node, now: Instant, seen: &mut HashSet<String>) {
        seen.insert(n.key.clone());
        let s = self.series.entry(n.key.clone()).or_default();
        // allow a little jitter so a 2 s refresh does not skip every other 10 s slot
        let due = s
            .back()
            .is_none_or(|(t, _)| now.duration_since(*t) + Duration::from_millis(500) >= STEP);
        if due {
            s.push_back((now, n.mem));
        }
        while s.front().is_some_and(|(t, _)| now.duration_since(*t) > KEEP) {
            s.pop_front();
        }
        let base = s.iter().find(|(t, _)| now.duration_since(*t) <= TREND);
        if let Some(&(t, m)) = base
            && s.len() > 1
        {
            let full = now.duration_since(t) + STEP >= TREND;
            n.delta = Some((n.mem as i64 - m as i64, full));
        }
        for c in &mut n.children {
            self.walk(c, now, seen);
        }
    }

    /// The most recent `n` memory values of a row, oldest first.
    pub fn recent(&self, key: &str, n: usize) -> Vec<u64> {
        self.series
            .get(key)
            .map(|s| s.iter().skip(s.len().saturating_sub(n)).map(|(_, m)| *m).collect())
            .unwrap_or_default()
    }
}
