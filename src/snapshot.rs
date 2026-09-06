//! What the operator boundary reads: health and counts, published here and
//! read from `xmip_operate.h`.
//!
//! ADR-0027 clause 6: the runtime publishes, the boundary reads, and nothing
//! across it asks the hot path. This is the published thing. Receive, Process
//! and Send write into it asynchronously; a surface reads whatever is here.
//!
//! Scope is an Xmip URI path over the execution tree, clause 4. A record at
//! `xmip:///edge-01/transport/ftp` sits beneath `xmip:///edge-01/transport`
//! and beneath `xmip:///edge-01`, so asking for a node gets everything the
//! node holds. That prefix rule is the whole aggregation model: health up the
//! tree is the worst beneath, and a count up the tree is the sum beneath.

use std::collections::BTreeMap;

/// The mood of a scope — observability-model.md section 6. A mood, not a colour:
/// this names what a human gets out of a thread, process, node or cluster, and
/// a surface renders it however it likes (the GUI paints it). It is about the
/// resource under load, not the machine: it tells an operator whether results
/// are flowing and, when they are not, what to do — change the load, replace the
/// hardware, fix the one thing that is stuck (ADR-0041).
///
/// Five **leaf** moods, in worsening order: `Fine` (results flowing), `Working`
/// (handling the load), `Stressed` (strained — change the load), `Exhausted`
/// (spent — replace the hardware), `Done` (blocked or failed — the pain, a cert
/// to renew, a password, a missing folder).
///
/// `Holding` is the **rollup** mood, not a leaf's: in a perfect world everything
/// is `Fine`; the moment anything below is not, the parent is displeased and
/// reports `Holding` — drill in. So a parent is `Fine` or `Holding`, and a leaf
/// carries the real mood. `Fine` up the tree means every leaf beneath is `Fine`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Health {
    Fine,
    Working,
    Stressed,
    Exhausted,
    Done,
    /// Rollup only — a parent with something not-`Fine` beneath it.
    Holding,
}

/// What a count counts. Never a bare number — ADR-0027 clause 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Counted {
    Streams,
    Messages,
    Journeys,
    Bytes,
}

/// The severity a paused scope publishes. A category, not a measurement: a
/// deliberate stop is a correctable state, and it stays that however long it
/// lasts.
pub const PAUSED_SEVERITY: u8 = 30;

/// One scope's health, how far from healthy it is, the one line that explains
/// it, and when it was seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthRecord {
    pub scope: String,
    pub health: Health,
    /// 0 to 100, shading the mood within itself. 0 is the mildest; 100 is as bad
    /// as that mood gets. The mood alone cannot say whether one worth a look now
    /// or tonight — the number does.
    pub severity: u8,
    pub evidence: String,
    pub observed_unix_nanos: i64,
}

/// One count over a window, and when it was taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Count {
    pub scope: String,
    pub counted: Counted,
    pub value: u64,
    pub window_start_unix_nanos: i64,
    pub window_end_unix_nanos: i64,
    pub observed_unix_nanos: i64,
}

/// The published state of a node. One per node, written by the node.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    health: BTreeMap<String, HealthRecord>,
    counts: BTreeMap<(String, Counted), Count>,
    /// What a paused scope looked like before it was paused, so resume puts
    /// it back rather than guessing.
    paused: BTreeMap<String, HealthRecord>,
}

impl Snapshot {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a scope's health. Replaces what was there for that scope. A
    /// paused scope stays paused: the record is kept for resume and the
    /// published state does not change until then.
    pub fn record_health(&mut self, record: HealthRecord) {
        if self.paused.contains_key(&record.scope) {
            self.paused.insert(record.scope.clone(), record);
            return;
        }

        self.health.insert(record.scope.clone(), record);
    }

    /// Record a count. Replaces what was there for that scope and kind. A
    /// paused scope's counts are dropped — it is not doing anything.
    pub fn record_count(&mut self, count: Count) {
        if self.is_paused(&count.scope) {
            return;
        }

        self.counts
            .insert((count.scope.clone(), count.counted), count);
    }

    /// Every health record in the snapshot, for a caller that retains history
    /// rather than reading one scope. Order is by scope; the reader does not
    /// depend on it.
    pub fn health_records(&self) -> impl Iterator<Item = &HealthRecord> {
        self.health.values()
    }

    /// Every count in the snapshot, for the same reason.
    pub fn all_counts(&self) -> impl Iterator<Item = &Count> {
        self.counts.values()
    }

    /// Pause everything at and beneath a scope. Each affected record is held at
    /// [`PAUSED_SEVERITY`], its prior state kept for resume, and its counts stop.
    /// `who` names the operator, for the evidence line. Returns
    /// how many scopes it paused — zero when the scope names nothing.
    pub fn pause(&mut self, scope: &str, who: &str, now: i64) -> usize {
        let targets: Vec<String> = self
            .health
            .keys()
            .filter(|recorded| beneath(recorded, scope))
            .cloned()
            .collect();

        for target in &targets {
            if let Some(record) = self.health.remove(target) {
                self.paused.insert(target.clone(), record);
            }

            self.health.insert(
                target.clone(),
                HealthRecord {
                    scope: target.clone(),
                    health: Health::Stressed,
                    severity: PAUSED_SEVERITY,
                    evidence: format!("paused by {who}"),
                    observed_unix_nanos: now,
                },
            );

            self.counts.retain(|(recorded, _), _| recorded != target);
        }

        targets.len()
    }

    /// Resume everything at and beneath a scope, putting back the state each
    /// had before it was paused. Returns how many scopes it resumed.
    pub fn resume(&mut self, scope: &str) -> usize {
        let targets: Vec<String> = self
            .paused
            .keys()
            .filter(|recorded| beneath(recorded, scope))
            .cloned()
            .collect();

        for target in &targets {
            if let Some(record) = self.paused.remove(target) {
                self.health.insert(target.clone(), record);
            }
        }

        targets.len()
    }

    /// Whether a scope is paused — itself or an ancestor of it.
    #[must_use]
    pub fn is_paused(&self, scope: &str) -> bool {
        self.paused.keys().any(|paused| beneath(scope, paused))
    }

    /// Health at and beneath a scope, worst first and, within a mood, most
    /// severe first — a Done at 90 above a Done at 60, so the worst thing an
    /// operator can do something about is the first thing they see.
    #[must_use]
    pub fn health(&self, scope: &str) -> Vec<HealthRecord> {
        let mut found: Vec<HealthRecord> = self
            .health
            .values()
            .filter(|record| beneath(&record.scope, scope))
            .cloned()
            .collect();

        found.sort_by(|a, b| {
            b.health
                .cmp(&a.health)
                .then(b.severity.cmp(&a.severity))
                .then(a.scope.cmp(&b.scope))
        });

        found
    }

    /// The rolled-up health at or beneath a scope, or `None` when nothing is
    /// recorded there.
    ///
    /// A leaf's mood **does not propagate**: in a perfect world everything is
    /// `Fine`, and the moment anything below is not, the parent is displeased and
    /// reports `Holding` — drill in (observability-model §6, ADR-0041). So a
    /// parent is only ever `Fine` or `Holding`; the leaf that owns the trouble
    /// carries the real mood, and an operator drills down through the `Holding`
    /// scopes to it. `Fine` up the tree still means every leaf beneath is `Fine`.
    #[must_use]
    pub fn worst(&self, scope: &str) -> Option<Health> {
        let record = self.health(scope).into_iter().next()?;
        if record.scope != scope && record.health != Health::Fine {
            Some(Health::Holding)
        } else {
            Some(record.health)
        }
    }

    /// One kind of count, summed over everything at and beneath a scope. The
    /// window is the union of the parts and the observation the oldest, so a
    /// reader sees the staleness of the stalest part. `None` when nothing is
    /// recorded.
    #[must_use]
    pub fn measure(&self, scope: &str, counted: Counted) -> Option<Count> {
        let parts: Vec<&Count> = self
            .counts
            .values()
            .filter(|count| count.counted == counted && beneath(&count.scope, scope))
            .collect();

        let first = parts.first()?;

        Some(Count {
            scope: scope.to_string(),
            counted,
            value: parts.iter().map(|count| count.value).sum(),
            window_start_unix_nanos: parts
                .iter()
                .map(|count| count.window_start_unix_nanos)
                .min()
                .unwrap_or(first.window_start_unix_nanos),
            window_end_unix_nanos: parts
                .iter()
                .map(|count| count.window_end_unix_nanos)
                .max()
                .unwrap_or(first.window_end_unix_nanos),
            observed_unix_nanos: parts
                .iter()
                .map(|count| count.observed_unix_nanos)
                .min()
                .unwrap_or(first.observed_unix_nanos),
        })
    }
}

/// Whether `candidate` is `scope` or sits beneath it in the tree. A prefix of
/// characters is not a prefix of path segments: `xmip:///ab` is not beneath
/// `xmip:///a`.
fn beneath(candidate: &str, scope: &str) -> bool {
    let scope = scope.trim_end_matches('/');

    candidate == scope
        || candidate
            .strip_prefix(scope)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health(scope: &str, health: Health, severity: u8) -> HealthRecord {
        HealthRecord {
            scope: scope.to_string(),
            health,
            severity,
            evidence: String::new(),
            observed_unix_nanos: 1_000,
        }
    }

    fn count(scope: &str, value: u64) -> Count {
        Count {
            scope: scope.to_string(),
            counted: Counted::Streams,
            value,
            window_start_unix_nanos: 0,
            window_end_unix_nanos: 60,
            observed_unix_nanos: 60,
        }
    }

    #[test]
    fn a_done_leaf_carries_its_mood_but_rolls_up_as_holding() {
        // ADR-0041: a leaf's mood does not propagate. The leaf that owns the
        // trouble keeps its mood; the parent is displeased — Holding — drill in.
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///edge-01/receive/a", Health::Fine, 0));
        snapshot.record_health(health("xmip:///edge-01/receive/b", Health::Done, 90));

        assert_eq!(
            snapshot.worst("xmip:///edge-01"),
            Some(Health::Holding),
            "a done leaf leaves the parent holding, not done"
        );
        assert_eq!(
            snapshot.worst("xmip:///edge-01/receive/b"),
            Some(Health::Done),
            "the leaf that owns the trouble keeps its mood"
        );
    }

    #[test]
    fn any_non_fine_leaf_rolls_up_as_holding() {
        // Not only Done: the moment anything below is not Fine, the parent is
        // Holding — a Working leaf below still leaves the parent displeased.
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///edge-01/receive/a", Health::Fine, 0));
        snapshot.record_health(health("xmip:///edge-01/receive/b", Health::Working, 20));

        assert_eq!(snapshot.worst("xmip:///edge-01"), Some(Health::Holding));
        // The leaf itself still shows what it is doing.
        assert_eq!(
            snapshot.worst("xmip:///edge-01/receive/b"),
            Some(Health::Working)
        );
    }

    #[test]
    fn within_a_state_the_more_severe_comes_first() {
        // The mood says which; the number orders within it, so the
        // worst thing an operator can act on is the top row.
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///n/receive/mild", Health::Stressed, 40));
        snapshot.record_health(health("xmip:///n/receive/severe", Health::Stressed, 85));

        let ordered = snapshot.health("xmip:///n");
        assert_eq!(ordered[0].scope, "xmip:///n/receive/severe");
        assert_eq!(ordered[0].severity, 85);
    }

    #[test]
    fn a_scope_with_nothing_beneath_it_has_no_health() {
        assert_eq!(Snapshot::new().worst("xmip:///edge-01"), None);
    }

    #[test]
    fn pausing_holds_a_scope_stressed_and_stops_its_counts() {
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///edge-01/receive/orders", Health::Fine, 0));
        snapshot.record_count(count("xmip:///edge-01/receive/orders", 40));

        let paused = snapshot.pause("xmip:///edge-01/receive/orders", "ilian", 2_000);

        assert_eq!(paused, 1);
        let record = &snapshot.health("xmip:///edge-01/receive/orders")[0];
        assert_eq!(record.health, Health::Stressed);
        assert_eq!(record.severity, PAUSED_SEVERITY);
        assert!(record.evidence.contains("ilian"));
        // A paused Location is doing nothing, so its count is gone and a fresh
        // one is dropped rather than recorded.
        assert!(
            snapshot
                .measure("xmip:///edge-01/receive/orders", Counted::Streams)
                .is_none()
        );
        snapshot.record_count(count("xmip:///edge-01/receive/orders", 99));
        assert!(
            snapshot
                .measure("xmip:///edge-01/receive/orders", Counted::Streams)
                .is_none()
        );
    }

    #[test]
    fn resume_puts_back_exactly_what_was_there() {
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///n/receive/a", Health::Done, 70));

        snapshot.pause("xmip:///n/receive/a", "ilian", 2_000);
        assert_eq!(
            snapshot.worst("xmip:///n/receive/a"),
            Some(Health::Stressed)
        );

        let resumed = snapshot.resume("xmip:///n/receive/a");
        assert_eq!(resumed, 1);
        let record = &snapshot.health("xmip:///n/receive/a")[0];
        assert_eq!(record.health, Health::Done);
        assert_eq!(record.severity, 70);
    }

    #[test]
    fn pausing_a_stage_pauses_every_location_in_it() {
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///n/receive/a", Health::Fine, 0));
        snapshot.record_health(health("xmip:///n/receive/b", Health::Fine, 0));
        snapshot.record_health(health("xmip:///n/send/c", Health::Fine, 0));

        let paused = snapshot.pause("xmip:///n/receive", "ilian", 2_000);

        assert_eq!(paused, 2, "both receive locations, not the send one");
        assert!(snapshot.is_paused("xmip:///n/receive/a"));
        assert!(!snapshot.is_paused("xmip:///n/send/c"));
    }

    #[test]
    fn a_paused_scope_stays_paused_when_the_node_republishes_it() {
        // The node goes on observing while an operator holds a Location down;
        // its fresh reading must not un-pause it.
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///n/receive/a", Health::Fine, 0));
        snapshot.pause("xmip:///n/receive/a", "ilian", 2_000);

        snapshot.record_health(health("xmip:///n/receive/a", Health::Fine, 0));

        assert_eq!(
            snapshot.worst("xmip:///n/receive/a"),
            Some(Health::Stressed)
        );
    }
}
