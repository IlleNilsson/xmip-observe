//! History: observation over time, bounded. ADR-0029.
//!
//! [`Snapshot`] is the present; `History` is how a scope got there. It keeps a
//! bounded series per scope — a health timeline (when it broke, when it
//! recovered) and a throughput series per counted kind (the curve) — so every
//! operator surface can show the past, not only the present. Bounded because a
//! node runs for weeks: each series holds its last `capacity` points and drops
//! the oldest, so memory is a function of how many scopes there are, not how
//! long the node has run.
//!
//! It is monitoring history, never content: a point is a state, a severity and a
//! number. Content that must be preserved is retention's, not this.

use std::collections::{BTreeMap, VecDeque};

use crate::snapshot::{Count, Counted, HealthRecord, Snapshot};

/// The default points kept per series: enough to watch a shift at one point a
/// second without unbounded growth.
pub const DEFAULT_CAPACITY: usize = 4096;

/// A bounded series of observation over time, per scope.
pub struct History {
    capacity: usize,
    health: BTreeMap<String, VecDeque<HealthRecord>>,
    counts: BTreeMap<(String, Counted), VecDeque<Count>>,
}

impl History {
    /// A history keeping `capacity` points per series. Capacity is clamped to at
    /// least one — a series that keeps nothing is not a history.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            health: BTreeMap::new(),
            counts: BTreeMap::new(),
        }
    }

    /// Append everything a snapshot holds to its series, dropping the oldest
    /// point of any series that is now over capacity. Call it once per tick.
    pub fn record(&mut self, snapshot: &Snapshot) {
        for record in snapshot.health_records() {
            push(
                self.health.entry(record.scope.clone()).or_default(),
                record.clone(),
                self.capacity,
            );
        }

        for count in snapshot.all_counts() {
            push(
                self.counts
                    .entry((count.scope.clone(), count.counted))
                    .or_default(),
                count.clone(),
                self.capacity,
            );
        }
    }

    /// One scope's health over time, oldest first.
    #[must_use]
    pub fn health_series(&self, scope: &str) -> Vec<HealthRecord> {
        self.health
            .get(scope)
            .map(|series| series.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// One scope's throughput of one kind over time, oldest first.
    #[must_use]
    pub fn count_series(&self, scope: &str, counted: Counted) -> Vec<Count> {
        self.counts
            .get(&(scope.to_string(), counted))
            .map(|series| series.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// One scope's health at or after `since_unix_nanos`, oldest first. The
    /// range a surface asks for when it shows a window rather than everything.
    #[must_use]
    pub fn health_since(&self, scope: &str, since_unix_nanos: i64) -> Vec<HealthRecord> {
        self.health_series(scope)
            .into_iter()
            .filter(|record| record.observed_unix_nanos >= since_unix_nanos)
            .collect()
    }

    /// One scope's throughput of one kind at or after `since_unix_nanos`.
    #[must_use]
    pub fn count_since(&self, scope: &str, counted: Counted, since_unix_nanos: i64) -> Vec<Count> {
        self.count_series(scope, counted)
            .into_iter()
            .filter(|count| count.observed_unix_nanos >= since_unix_nanos)
            .collect()
    }
}

impl Default for History {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

fn push<T>(series: &mut VecDeque<T>, point: T, capacity: usize) {
    series.push_back(point);
    while series.len() > capacity {
        series.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Health;

    fn health(scope: &str, health: Health, now: i64) -> HealthRecord {
        HealthRecord {
            scope: scope.to_string(),
            health,
            severity: 0,
            evidence: String::new(),
            observed_unix_nanos: now,
        }
    }

    fn count(scope: &str, value: u64, now: i64) -> Count {
        Count {
            scope: scope.to_string(),
            counted: Counted::Bytes,
            value,
            window_start_unix_nanos: now,
            window_end_unix_nanos: now,
            observed_unix_nanos: now,
        }
    }

    #[test]
    fn a_series_accumulates_over_ticks_oldest_first() {
        let mut history = History::default();

        for tick in 0..3 {
            let mut snapshot = Snapshot::new();
            snapshot.record_health(health("xmip:///n/receive/a", Health::Fine, tick));
            snapshot.record_count(count("xmip:///n", u64::try_from(tick).unwrap_or(0), tick));
            history.record(&snapshot);
        }

        let series = history.health_series("xmip:///n/receive/a");
        assert_eq!(series.len(), 3);
        assert_eq!(series.first().map(|r| r.observed_unix_nanos), Some(0));
        assert_eq!(series.last().map(|r| r.observed_unix_nanos), Some(2));

        let throughput = history.count_series("xmip:///n", Counted::Bytes);
        assert_eq!(throughput.len(), 3);
        assert_eq!(throughput.last().map(|c| c.value), Some(2));
    }

    #[test]
    fn the_series_is_bounded_and_drops_the_oldest() {
        let mut history = History::with_capacity(2);

        for tick in 0..5 {
            let mut snapshot = Snapshot::new();
            snapshot.record_health(health("xmip:///n/receive/a", Health::Fine, tick));
            history.record(&snapshot);
        }

        let series = history.health_series("xmip:///n/receive/a");
        assert_eq!(series.len(), 2, "capacity is two");
        assert_eq!(series.first().map(|r| r.observed_unix_nanos), Some(3));
        assert_eq!(series.last().map(|r| r.observed_unix_nanos), Some(4));
    }

    #[test]
    fn a_range_query_returns_only_points_at_or_after_the_bound() {
        let mut history = History::default();

        for tick in 0..5 {
            let mut snapshot = Snapshot::new();
            snapshot.record_health(health("xmip:///n/send/b", Health::Fine, tick));
            history.record(&snapshot);
        }

        let recent = history.health_since("xmip:///n/send/b", 3);
        assert_eq!(recent.len(), 2);
        assert!(recent.iter().all(|r| r.observed_unix_nanos >= 3));
    }

    #[test]
    fn an_unknown_scope_has_an_empty_series() {
        let history = History::default();
        assert!(history.health_series("xmip:///nothing").is_empty());
        assert!(
            history
                .count_series("xmip:///nothing", Counted::Streams)
                .is_empty()
        );
    }
}
