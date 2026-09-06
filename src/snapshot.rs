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
/// this names how a scope is doing, and a surface renders it however it likes
/// (the GUI paints it). Four moods, in worsening order, which is this enum's
/// ordering.
///
/// **`Done` does not propagate**: it is the mood of a leaf, and any scope above a
/// `Done` leaf reports `Holding` — attention, drill in — so one fault deep in the
/// tree does not carry the whole cluster to `Done` (ADR-0041). `Fine` up the tree
/// still means every leaf beneath is `Fine`.
///
/// `Holding` is a rollup mood: leaves are `Fine`, `Average` or `Done`; an
/// aggregating scope is `Fine`, `Average` or `Holding`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Health {
    Fine,
    Average,
    Holding,
    Done,
}

/// What a count counts. Never a bare number — ADR-0027 clause 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Counted {
    Streams,
    Messages,
    Journeys,
    Bytes,
}

/// The severity a paused scope publishes. A category, not a measurement: an
/// operator's deliberate stop is the correctable yellow of section 6, and it
/// is the same yellow however long it lasts.
pub const PAUSED_SEVERITY: u8 = 30;

/// One scope's health, how far from healthy it is, the one line that explains
/// it, and when it was seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthRecord {
    pub scope: String,
    pub health: Health,
    /// 0 to 100, shading the colour. Green is 0; red at 100 is as bad as it
    /// gets. Xmip will not always run smoothly, and the word alone cannot say
    /// whether a yellow is worth a look now or tonight.
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

    /// Pause everything at and beneath a scope. Each affected record is set to
    /// yellow at [`PAUSED_SEVERITY`], its prior state kept for resume, and its
    /// counts stop. `who` names the operator, for the evidence line. Returns
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
                    health: Health::Average,
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

    /// Health at and beneath a scope, worst first and, within a state, most
    /// severe first — a red at 90 above a red at 60, so the worst thing an
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
    /// A red **does not propagate**: at the leaf that owns it the state is red,
    /// but any scope *above* that leaf reports `Orange` — attention, drill in
    /// (observability-model §6, ADR-0041). So the worst a red among thousands can
    /// do to the cluster is turn it orange, and an operator drills down through
    /// the oranges and yellows to the red itself. Green up the tree still means
    /// every leaf beneath is green.
    #[must_use]
    pub fn worst(&self, scope: &str) -> Option<Health> {
        let record = self.health(scope).into_iter().next()?;
        if record.health == Health::Done && record.scope != scope {
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
    fn a_red_leaf_shows_red_but_rolls_up_as_orange() {
        // ADR-0041: a red does not propagate. The leaf that owns it is red; every
        // scope above it is orange — attention, drill in — so one fault cannot
        // paint the cluster red.
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///edge-01/receive/a", Health::Fine, 0));
        snapshot.record_health(health("xmip:///edge-01/receive/b", Health::Done, 90));

        assert_eq!(
            snapshot.worst("xmip:///edge-01"),
            Some(Health::Holding),
            "a red below rolls up as orange, not red"
        );
        assert_eq!(
            snapshot.worst("xmip:///edge-01/receive/b"),
            Some(Health::Done),
            "the leaf that owns the fault is still red"
        );
    }

    #[test]
    fn a_yellow_below_still_rolls_up_as_yellow() {
        // Only red is capped; yellow propagates unchanged.
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///edge-01/receive/a", Health::Fine, 0));
        snapshot.record_health(health("xmip:///edge-01/receive/b", Health::Average, 40));

        assert_eq!(snapshot.worst("xmip:///edge-01"), Some(Health::Average));
    }

    #[test]
    fn within_a_state_the_more_severe_comes_first() {
        // The word says which colour; the number orders within it, so the
        // worst thing an operator can act on is the top row.
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///n/receive/mild", Health::Average, 40));
        snapshot.record_health(health("xmip:///n/receive/severe", Health::Average, 85));

        let ordered = snapshot.health("xmip:///n");
        assert_eq!(ordered[0].scope, "xmip:///n/receive/severe");
        assert_eq!(ordered[0].severity, 85);
    }

    #[test]
    fn a_scope_with_nothing_beneath_it_has_no_health() {
        assert_eq!(Snapshot::new().worst("xmip:///edge-01"), None);
    }

    #[test]
    fn pausing_turns_a_scope_yellow_and_stops_its_counts() {
        let mut snapshot = Snapshot::new();
        snapshot.record_health(health("xmip:///edge-01/receive/orders", Health::Fine, 0));
        snapshot.record_count(count("xmip:///edge-01/receive/orders", 40));

        let paused = snapshot.pause("xmip:///edge-01/receive/orders", "ilian", 2_000);

        assert_eq!(paused, 1);
        let record = &snapshot.health("xmip:///edge-01/receive/orders")[0];
        assert_eq!(record.health, Health::Average);
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
        assert_eq!(snapshot.worst("xmip:///n/receive/a"), Some(Health::Average));

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

        assert_eq!(snapshot.worst("xmip:///n/receive/a"), Some(Health::Average));
    }
}
