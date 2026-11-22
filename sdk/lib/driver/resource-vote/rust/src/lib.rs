// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Per-connection votes on a shared hardware resource (a clock, a power domain, a regulator, ...)
//! that is turned on on behalf of several consumers.
//!
//! [`VoteSet`] is a synchronous state machine. It does no I/O; the caller applies hardware
//! effects. Every state-changing operation is two-phase: a `prepare_*` method computes a [`Plan`]
//! without mutating any state, the caller applies [`Plan::new_aggregate`] to hardware if
//! [`Plan::changed`], and then calls [`VoteSet::commit`] on success or [`VoteSet::abandon`] on
//! failure (state unchanged):
//!
//! ```ignore
//! let plan = votes.prepare_enable(&vote);
//! if plan.changed() {
//!     if let Err(e) = turn_on_hardware().await {
//!         votes.abandon(plan);
//!         return Err(e);
//!     }
//! }
//! votes.commit(plan);
//! ```
//!
//! A plan is invalidated by any intervening `commit` or `abandon` on the same set.
//!
//! Each consumer connection owns a [`Vote`], attributed to the consumer's node (its NodeKey, e.g.
//! the child node / service instance name the connection arrived through). When the connection
//! closes, the caller uses [`VoteSet::prepare_close`]:
//!
//! * [`CloseBehavior::OrphanOnClose`] (driver consumers): if the vote has a request that
//!   contributes to the aggregate, it is kept as an *orphan* for its NodeKey and the aggregate is
//!   unchanged. One warning is logged. The orphan is removed when a new vote for the same NodeKey
//!   sets a request (reclaim) or when [`VoteSet::prepare_node_started`] is committed for that
//!   NodeKey. A request contributes iff `merge(&[request])` is not `None`; a request that
//!   contributes nothing (e.g. `false` in a bool set) is simply dropped on close.
//! * [`CloseBehavior::ReleaseOnClose`] (debug/tool connections): equivalent to clearing the vote.
//!   If applying the resulting plan fails, call [`VoteSet::abandon`]. Because the connection is
//!   gone, the vote cannot stay live: it is instead retained as an orphan for its NodeKey (with
//!   one warning), and its release is retried by the next reclaim or `prepare_node_started` for
//!   that NodeKey. Callers should log the hardware failure itself at error level, and should give
//!   `ReleaseOnClose` votes a NodeKey on which a later reclaim or node start can happen.
//!
//! Named holds (e.g. `"boot"`, `"kernel"`) contribute to the aggregate independently of any vote.
//!
//! `VoteSet` is a plain struct; callers that share it across tasks wrap it as needed.

use fuchsia_inspect as inspect;
use std::collections::BTreeMap;
use std::fmt::{self, Debug};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn next_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Determines what happens to a vote's request when its connection closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseBehavior {
    /// The request is kept as an orphan attributed to the vote's NodeKey (driver consumers).
    OrphanOnClose,
    /// The request is released (debug/tool connections that have no node to rebind).
    ReleaseOnClose,
}

/// Identifies one consumer connection's vote.
///
/// Votes are not tied to a particular [`VoteSet`]; they are lightweight identities. A vote must
/// not be used after a [`VoteSet::prepare_close`] plan for it has been committed or abandoned.
#[derive(Debug)]
pub struct Vote {
    id: u64,
    node_key: String,
    close_behavior: CloseBehavior,
}

impl Vote {
    /// Creates a new vote attributed to the node `node_key`.
    pub fn new(node_key: impl Into<String>, close_behavior: CloseBehavior) -> Self {
        Self { id: next_id(), node_key: node_key.into(), close_behavior }
    }

    /// A process-unique identifier for this vote.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The NodeKey this vote is attributed to.
    pub fn node_key(&self) -> &str {
        &self.node_key
    }

    /// What happens to this vote's request when its connection closes.
    pub fn close_behavior(&self) -> CloseBehavior {
        self.close_behavior
    }
}

/// The kind of an entry that contributes to the aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A vote whose connection is open.
    Live,
    /// A vote whose connection closed while it held a request.
    Orphan,
    /// A named system hold.
    Hold,
}

impl EntryKind {
    /// Returns `"live"`, `"orphan"` or `"hold"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryKind::Live => "live",
            EntryKind::Orphan => "orphan",
            EntryKind::Hold => "hold",
        }
    }
}

/// One entry contributing to the aggregate.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry<R> {
    /// The kind of entry.
    pub kind: EntryKind,
    /// The NodeKey for live and orphaned votes, the hold name for holds.
    pub key: String,
    /// The entry's request.
    pub request: R,
    /// Phase-1 counted votes: the number of outstanding increments. 1 for
    /// [`VoteSet::prepare_set`] votes and holds.
    pub count: u32,
    /// When the entry was created or last changed kind or request.
    pub since: zx::MonotonicInstant,
}

type Entries<R> = BTreeMap<u64, Entry<R>>;

/// The result of a `prepare_*` call: the aggregate before and after the operation.
///
/// Must be passed to [`VoteSet::commit`] (the caller applied [`Plan::new_aggregate`] to hardware,
/// or nothing changed) or [`VoteSet::abandon`] (applying failed). Dropping a plan is equivalent
/// to abandoning it, except for close plans, which must be committed or abandoned.
#[must_use]
#[derive(Debug)]
pub struct Plan<R> {
    set_id: u64,
    generation: u64,
    old_aggregate: Option<R>,
    new_aggregate: Option<R>,
    next: Entries<R>,
    orphaned_id: Option<u64>,
    closing_id: Option<u64>,
}

impl<R: PartialEq> Plan<R> {
    /// The aggregate before the operation.
    pub fn old_aggregate(&self) -> Option<&R> {
        self.old_aggregate.as_ref()
    }

    /// The aggregate after the operation.
    pub fn new_aggregate(&self) -> Option<&R> {
        self.new_aggregate.as_ref()
    }

    /// True if the caller must apply [`Plan::new_aggregate`] to hardware before committing.
    pub fn changed(&self) -> bool {
        self.old_aggregate != self.new_aggregate
    }
}

/// Merges the requests of all contributing entries into the aggregate. Called with a non-empty
/// slice. Returning `None` means "nothing requested" (off).
pub type MergeFn<R> = Box<dyn Fn(&[R]) -> Option<R> + Send + Sync>;

/// Formats a request for logs and inspect.
pub type FormatFn<R> = Box<dyn Fn(&R) -> String + Send + Sync>;

/// Receives log messages emitted by a [`VoteSet`].
pub type LogSink = Box<dyn Fn(log::Level, &str) + Send + Sync>;

#[derive(Clone, Copy)]
enum OrphanReason {
    /// An `OrphanOnClose` vote's connection closed.
    Closed,
    /// Releasing a `ReleaseOnClose` vote on connection close failed.
    ReleaseFailed,
}

#[derive(Clone, Debug)]
struct State<R> {
    entries: Entries<R>,
    aggregate: Option<R>,
    transition_count: u64,
    orphaned_total: u64,
}

impl<R> State<R> {
    fn orphan_count(&self) -> usize {
        self.entries.values().filter(|e| e.kind == EntryKind::Orphan).count()
    }
}

struct InspectState<R> {
    snapshot: Arc<fuchsia_sync::Mutex<Arc<State<R>>>>,
    // Dropped before `_root` so any in-flight lazy callback finishes before the parent node is
    // removed.
    _values: inspect::LazyNode,
    _root: inspect::Node,
}

/// Tracks votes for a resource and computes their aggregate. See the crate documentation.
pub struct VoteSet<R> {
    id: u64,
    resource_name: String,
    merge: MergeFn<R>,
    format: Arc<FormatFn<R>>,
    log_sink: LogSink,
    state: Arc<State<R>>,
    generation: u64,
    inspect: Option<InspectState<R>>,
}

impl<R: Debug> Debug for VoteSet<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoteSet")
            .field("id", &self.id)
            .field("resource_name", &self.resource_name)
            .field("entries", &self.state.entries)
            .field("aggregate", &self.state.aggregate)
            .field("generation", &self.generation)
            .field("transition_count", &self.state.transition_count)
            .field("orphaned_total", &self.state.orphaned_total)
            .finish_non_exhaustive()
    }
}

impl<R: Clone + PartialEq + Debug> VoteSet<R> {
    /// Creates a vote set for `resource_name`. Requests are formatted with `Debug`.
    pub fn new(
        resource_name: impl Into<String>,
        merge: impl Fn(&[R]) -> Option<R> + Send + Sync + 'static,
    ) -> Self {
        Self::with_formatter(resource_name, merge, |r: &R| format!("{r:?}"))
    }

    /// Creates a vote set for `resource_name` with a custom request formatter.
    pub fn with_formatter(
        resource_name: impl Into<String>,
        merge: impl Fn(&[R]) -> Option<R> + Send + Sync + 'static,
        format: impl Fn(&R) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: next_id(),
            resource_name: resource_name.into(),
            merge: Box::new(merge),
            format: Arc::new(Box::new(format)),
            log_sink: Box::new(|level, message| log::log!(level, "{message}")),
            state: Arc::new(State {
                entries: BTreeMap::new(),
                aggregate: None,
                transition_count: 0,
                orphaned_total: 0,
            }),
            generation: 0,
            inspect: None,
        }
    }

    /// The resource name used in logs and inspect.
    pub fn resource_name(&self) -> &str {
        &self.resource_name
    }

    /// The current merged request of all contributing entries.
    pub fn aggregate(&self) -> Option<&R> {
        self.state.aggregate.as_ref()
    }

    /// Idempotently records `request` as `vote`'s request (count 1). Atomically removes all
    /// orphans with the same NodeKey (reclaim).
    pub fn prepare_set(&self, vote: &Vote, request: R) -> Plan<R> {
        let mut next = self.state.entries.clone();
        reclaim(&mut next, vote.node_key());
        upsert_live(&mut next, vote, request, false);
        self.make_plan(next)
    }

    /// Removes `vote`'s request. No-op if it has none.
    pub fn prepare_clear(&self, vote: &Vote) -> Plan<R> {
        let mut next = self.state.entries.clone();
        if next.get(&vote.id).is_some_and(|e| e.kind == EntryKind::Live) {
            next.remove(&vote.id);
        }
        self.make_plan(next)
    }

    /// Phase-1 counted vote: increments `vote`'s count and records `request` as its request.
    /// Atomically removes all orphans with the same NodeKey (reclaim).
    pub fn prepare_increment(&self, vote: &Vote, request: R) -> Plan<R> {
        let mut next = self.state.entries.clone();
        reclaim(&mut next, vote.node_key());
        upsert_live(&mut next, vote, request, true);
        self.make_plan(next)
    }

    /// Phase-1 counted vote: decrements `vote`'s count, saturating at 0. The vote's request is
    /// removed when the count reaches 0.
    pub fn prepare_decrement(&self, vote: &Vote) -> Plan<R> {
        let mut next = self.state.entries.clone();
        if let Some(entry) = next.get_mut(&vote.id)
            && entry.kind == EntryKind::Live
        {
            if entry.count <= 1 {
                next.remove(&vote.id);
            } else {
                entry.count -= 1;
            }
        }
        self.make_plan(next)
    }

    /// Called when `vote`'s connection closed. See [`CloseBehavior`]. Close cannot fail: the plan
    /// must be committed, or abandoned if applying a changed `ReleaseOnClose` plan failed.
    pub fn prepare_close(&self, vote: &Vote) -> Plan<R> {
        let mut next = self.state.entries.clone();
        let mut orphaned_id = None;
        if let Some(entry) = next.get_mut(&vote.id)
            && entry.kind == EntryKind::Live
        {
            if vote.close_behavior == CloseBehavior::OrphanOnClose
                && self.contributes(&entry.request)
            {
                entry.kind = EntryKind::Orphan;
                entry.since = zx::MonotonicInstant::get();
                orphaned_id = Some(vote.id);
            } else {
                next.remove(&vote.id);
            }
        }
        let mut plan = self.make_plan(next);
        plan.orphaned_id = orphaned_id;
        plan.closing_id = Some(vote.id);
        plan
    }

    /// Called when a driver started on node `node_key`. Removes all remaining orphans for it.
    pub fn prepare_node_started(&self, node_key: &str) -> Plan<R> {
        let mut next = self.state.entries.clone();
        reclaim(&mut next, node_key);
        self.make_plan(next)
    }

    /// Adds the named hold `name` with `request`, replacing the request of an existing hold with
    /// the same name.
    pub fn prepare_add_hold(&self, name: impl Into<String>, request: R) -> Plan<R> {
        let name = name.into();
        let mut next = self.state.entries.clone();
        let now = zx::MonotonicInstant::get();
        if let Some(entry) = next.values_mut().find(|e| e.kind == EntryKind::Hold && e.key == name)
        {
            if entry.request != request {
                entry.request = request;
                entry.since = now;
            }
        } else {
            next.insert(
                next_id(),
                Entry { kind: EntryKind::Hold, key: name, request, count: 1, since: now },
            );
        }
        self.make_plan(next)
    }

    /// Releases the named hold `name`. No-op if there is no such hold.
    pub fn prepare_release_hold(&self, name: &str) -> Plan<R> {
        let mut next = self.state.entries.clone();
        if let Some(id) =
            next.iter().find(|(_, e)| e.kind == EntryKind::Hold && e.key == name).map(|(id, _)| *id)
        {
            next.remove(&id);
        }
        self.make_plan(next)
    }

    /// Applies `plan`. The caller must have applied [`Plan::new_aggregate`] to hardware if
    /// [`Plan::changed`].
    pub fn commit(&mut self, plan: Plan<R>) {
        if !self.check_plan(&plan) {
            return;
        }
        let orphaned_id = plan.orphaned_id;
        let transition_count = self.state.transition_count + u64::from(plan.changed());
        let orphaned_total = self.state.orphaned_total + u64::from(orphaned_id.is_some());
        if let Some(state) = Arc::get_mut(&mut self.state) {
            state.entries = plan.next;
            state.aggregate = plan.new_aggregate;
            state.transition_count = transition_count;
            state.orphaned_total = orphaned_total;
        } else {
            self.state = Arc::new(State {
                entries: plan.next,
                aggregate: plan.new_aggregate,
                transition_count,
                orphaned_total,
            });
        }
        self.generation += 1;
        if let Some(id) = orphaned_id {
            self.log_orphan(id, OrphanReason::Closed);
        }
        self.sync_inspect_snapshot();
    }

    /// Discards `plan` because applying it to hardware failed. Vote state is unchanged, except
    /// for close plans: the closing vote's request (if any) is retained as an orphan for its
    /// NodeKey so that it is retried on the next relevant event.
    pub fn abandon(&mut self, plan: Plan<R>) {
        if !self.check_plan(&plan) {
            return;
        }
        self.generation += 1;
        let Some(id) = plan.closing_id else {
            return;
        };
        let Some(entry) = self.state.entries.get(&id) else {
            return;
        };
        if entry.kind != EntryKind::Live {
            return;
        }
        if !self.contributes(&entry.request) {
            // Nothing to hold; removing it does not change the aggregate.
            Arc::make_mut(&mut self.state).entries.remove(&id);
            self.sync_inspect_snapshot();
            return;
        }
        let state = Arc::make_mut(&mut self.state);
        if let Some(entry) = state.entries.get_mut(&id) {
            entry.kind = EntryKind::Orphan;
            entry.since = zx::MonotonicInstant::get();
        }
        state.orphaned_total += 1;
        self.log_orphan(id, OrphanReason::ReleaseFailed);
        self.sync_inspect_snapshot();
    }

    /// Returns all entries contributing to the aggregate, in creation order.
    pub fn entries(&self) -> impl Iterator<Item = &Entry<R>> {
        self.state.entries.values()
    }

    /// The number of committed plans that changed the aggregate.
    pub fn transition_count(&self) -> u64 {
        self.state.transition_count
    }

    /// The number of orphaned entries currently retained.
    pub fn orphan_count(&self) -> usize {
        self.state.orphan_count()
    }

    /// The total number of orphan events since creation.
    pub fn orphaned_total(&self) -> u64 {
        self.state.orphaned_total
    }

    /// Replaces the log sink. Intended for tests.
    pub fn set_log_sink_for_testing(&mut self, sink: LogSink) {
        self.log_sink = sink;
    }

    fn make_plan(&self, next: Entries<R>) -> Plan<R> {
        let new_aggregate = if next.is_empty() {
            None
        } else {
            let requests: Vec<R> = next.values().map(|e| e.request.clone()).collect();
            (self.merge)(&requests)
        };
        Plan {
            set_id: self.id,
            generation: self.generation,
            old_aggregate: self.state.aggregate.clone(),
            new_aggregate,
            next,
            orphaned_id: None,
            closing_id: None,
        }
    }

    fn check_plan(&self, plan: &Plan<R>) -> bool {
        debug_assert_eq!(plan.set_id, self.id, "plan from a different VoteSet");
        debug_assert_eq!(plan.generation, self.generation, "stale plan");
        if plan.set_id != self.id || plan.generation != self.generation {
            (self.log_sink)(
                log::Level::Error,
                &format!("{}: ignoring stale or foreign vote plan", self.resource_name),
            );
            return false;
        }
        true
    }

    /// Whether `request` contributes anything to the aggregate on its own.
    fn contributes(&self, request: &R) -> bool {
        (self.merge)(std::slice::from_ref(request)).is_some()
    }

    fn log_orphan(&self, id: u64, reason: OrphanReason) {
        let entry = &self.state.entries[&id];
        let request = (self.format)(&entry.request);
        let message = match reason {
            OrphanReason::Closed => format!(
                "Vote on resource '{}' from node '{}' orphaned by connection close; holding {} \
                 until the node rebinds",
                self.resource_name, entry.key, request
            ),
            OrphanReason::ReleaseFailed => format!(
                "Vote on resource '{}' from node '{}' could not be released on connection close; \
                 retaining {} as an orphan until reclaimed or the node starts",
                self.resource_name, entry.key, request
            ),
        };
        (self.log_sink)(log::Level::Warn, &message);
    }

    fn sync_inspect_snapshot(&self) {
        if let Some(inspect) = &self.inspect {
            *inspect.snapshot.lock() = Arc::clone(&self.state);
        }
    }
}

impl<R: Clone + PartialEq + Debug + Send + Sync + 'static> VoteSet<R> {
    /// Publishes state lazily on demand under `node`, which is typically a child created for this
    /// resource.
    pub fn attach_inspect(&mut self, node: inspect::Node) {
        self.inspect = None;
        let snapshot = Arc::new(fuchsia_sync::Mutex::new(Arc::clone(&self.state)));
        let callback_snapshot = Arc::clone(&snapshot);
        let resource_name = self.resource_name.clone();
        let format = Arc::clone(&self.format);
        let values = node.create_lazy_values("votes", move || {
            let state = Arc::clone(&callback_snapshot.lock());
            let resource_name = resource_name.clone();
            let format = Arc::clone(&format);
            Box::pin(async move {
                let inspector = inspect::Inspector::default();
                let root = inspector.root();
                let aggregate = match &state.aggregate {
                    Some(aggregate) => format(aggregate),
                    None => "none".to_string(),
                };
                root.record_string("resource", &resource_name);
                root.record_string("aggregate", aggregate);
                root.record_uint("transition_count", state.transition_count);
                root.record_uint("orphan_count", state.orphan_count() as u64);
                root.record_uint("orphaned_total", state.orphaned_total);
                root.record_child("entries", |entries| {
                    for (index, entry) in state.entries.values().enumerate() {
                        entries.record_child(index.to_string(), |child| {
                            child.record_string("kind", entry.kind.as_str());
                            child.record_string("key", &entry.key);
                            child.record_string("request", format(&entry.request));
                            child.record_uint("count", u64::from(entry.count));
                            child.record_int("since", entry.since.into_nanos());
                        });
                    }
                });
                Ok(inspector)
            })
        });
        self.inspect = Some(InspectState { snapshot, _values: values, _root: node });
    }
}

fn reclaim<R>(entries: &mut Entries<R>, node_key: &str) {
    entries.retain(|_, e| !(e.kind == EntryKind::Orphan && e.key == node_key));
}

fn upsert_live<R: PartialEq>(entries: &mut Entries<R>, vote: &Vote, request: R, increment: bool) {
    let now = zx::MonotonicInstant::get();
    let Some(entry) = entries.get_mut(&vote.id) else {
        entries.insert(
            vote.id,
            Entry {
                kind: EntryKind::Live,
                key: vote.node_key.clone(),
                request,
                count: 1,
                since: now,
            },
        );
        return;
    };
    debug_assert_eq!(entry.kind, EntryKind::Live, "vote used after close");
    if entry.kind != EntryKind::Live {
        return;
    }
    if increment {
        entry.count = entry.count.saturating_add(1);
    } else {
        entry.count = 1;
    }
    if entry.request != request {
        entry.request = request;
        entry.since = now;
    }
}

/// Merge function for boolean votes: `Some(true)` if any request is `true`, `None` otherwise.
pub fn merge_any_true(requests: &[bool]) -> Option<bool> {
    requests.iter().any(|r| *r).then_some(true)
}

/// A [`VoteSet`] for on/off resources.
///
/// For idempotent on/off votes use `prepare_set(vote, true)` / `prepare_clear(vote)`.
/// `prepare_set(vote, false)` records a vote that does not contribute; it is dropped (not
/// orphaned) when its connection closes.
pub type BoolVoteSet = VoteSet<bool>;

impl VoteSet<bool> {
    /// Creates a vote set for an on/off resource. The aggregate is `Some(true)` when any entry
    /// votes `true`.
    pub fn new_bool(resource_name: impl Into<String>) -> Self {
        Self::with_formatter(resource_name, merge_any_true, |r: &bool| {
            if *r { "on" } else { "off" }.to_string()
        })
    }

    /// Phase-1 counted `Enable()`: increments `vote`'s count.
    pub fn prepare_enable(&self, vote: &Vote) -> Plan<bool> {
        self.prepare_increment(vote, true)
    }

    /// Phase-1 counted `Disable()`: decrements `vote`'s count, saturating at 0.
    pub fn prepare_disable(&self, vote: &Vote) -> Plan<bool> {
        self.prepare_decrement(vote)
    }

    /// Whether the aggregate is "on".
    pub fn enabled(&self) -> bool {
        self.state.aggregate == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyProperty, assert_data_tree};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    type Logs = Arc<Mutex<Vec<(log::Level, String)>>>;

    fn bool_set() -> (BoolVoteSet, Logs) {
        let mut votes = BoolVoteSet::new_bool("clk-test");
        let logs: Logs = Default::default();
        let sink_logs = logs.clone();
        votes.set_log_sink_for_testing(Box::new(move |level, message| {
            sink_logs.lock().unwrap().push((level, message.to_string()));
        }));
        (votes, logs)
    }

    fn count_logs(logs: &Logs, level: log::Level) -> usize {
        logs.lock().unwrap().iter().filter(|(l, _)| *l == level).count()
    }

    #[track_caller]
    fn commit_unchanged<R: Clone + PartialEq + Debug>(votes: &mut VoteSet<R>, plan: Plan<R>) {
        assert!(!plan.changed(), "unexpected change: {plan:?}");
        votes.commit(plan);
    }

    #[track_caller]
    fn commit_changed_to(votes: &mut BoolVoteSet, plan: Plan<bool>, enabled: bool) {
        assert!(plan.changed(), "expected change: {plan:?}");
        assert_eq!(plan.new_aggregate().copied().unwrap_or(false), enabled);
        votes.commit(plan);
        assert_eq!(votes.enabled(), enabled);
    }

    fn entries<R: Clone + PartialEq + Debug>(votes: &VoteSet<R>) -> Vec<Entry<R>> {
        votes.entries().cloned().collect()
    }

    #[fuchsia::test]
    fn starts_off() {
        let (votes, _) = bool_set();
        assert_eq!(votes.aggregate(), None);
        assert!(!votes.enabled());
        assert_eq!(votes.entries().count(), 0);
        assert_eq!(votes.transition_count(), 0);
        assert_eq!(votes.resource_name(), "clk-test");
    }

    #[fuchsia::test]
    fn vote_ids_are_unique() {
        let a = Vote::new("a", CloseBehavior::OrphanOnClose);
        let b = Vote::new("a", CloseBehavior::ReleaseOnClose);
        assert_ne!(a.id(), b.id());
        assert_eq!(a.node_key(), "a");
        assert_eq!(a.close_behavior(), CloseBehavior::OrphanOnClose);
        assert_eq!(b.close_behavior(), CloseBehavior::ReleaseOnClose);
    }

    #[fuchsia::test]
    fn counted_enable_disable() {
        let (mut votes, logs) = bool_set();
        let vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);

        let plan = votes.prepare_enable(&vote);
        assert_eq!(plan.old_aggregate(), None);
        assert_eq!(plan.new_aggregate(), Some(&true));
        commit_changed_to(&mut votes, plan, true);

        // Phase 1: a second enable on the same connection stacks.
        let plan = votes.prepare_enable(&vote);
        commit_unchanged(&mut votes, plan);
        let e = entries(&votes);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].count, 2);
        assert_eq!(e[0].kind, EntryKind::Live);
        assert_eq!(e[0].key, "node-a");

        let plan = votes.prepare_disable(&vote);
        commit_unchanged(&mut votes, plan);
        assert!(votes.enabled());
        assert_eq!(entries(&votes)[0].count, 1);

        let plan = votes.prepare_disable(&vote);
        commit_changed_to(&mut votes, plan, false);
        assert_eq!(votes.entries().count(), 0);

        // Disable without a vote saturates at 0.
        let plan = votes.prepare_disable(&vote);
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.entries().count(), 0);
        assert_eq!(votes.transition_count(), 2);
        assert!(logs.lock().unwrap().is_empty());
    }

    #[fuchsia::test]
    fn aggregate_of_several_votes() {
        let (mut votes, _) = bool_set();
        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let b = Vote::new("node-b", CloseBehavior::OrphanOnClose);

        let plan = votes.prepare_enable(&a);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_enable(&b);
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_disable(&a);
        commit_unchanged(&mut votes, plan);
        assert!(votes.enabled());
        let plan = votes.prepare_disable(&b);
        commit_changed_to(&mut votes, plan, false);
    }

    #[fuchsia::test]
    fn abandon_leaves_state_unchanged() {
        let (mut votes, logs) = bool_set();
        let vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);

        let plan = votes.prepare_enable(&vote);
        assert!(plan.changed());
        votes.abandon(plan);
        assert!(!votes.enabled());
        assert_eq!(votes.entries().count(), 0);
        assert_eq!(votes.transition_count(), 0);

        let plan = votes.prepare_enable(&vote);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_disable(&vote);
        assert!(plan.changed());
        votes.abandon(plan);
        assert!(votes.enabled());
        assert_eq!(entries(&votes)[0].count, 1);
        assert!(logs.lock().unwrap().is_empty());
    }

    #[fuchsia::test]
    fn dropped_plan_leaves_state_unchanged() {
        let (mut votes, _) = bool_set();
        let vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let _ = votes.prepare_enable(&vote);
        assert!(!votes.enabled());
        let plan = votes.prepare_enable(&vote);
        commit_changed_to(&mut votes, plan, true);
    }

    #[fuchsia::test]
    fn close_without_request_is_silent() {
        let (mut votes, logs) = bool_set();
        let vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_close(&vote);
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.entries().count(), 0);
        assert_eq!(votes.orphan_count(), 0);

        let balanced = Vote::new("node-b", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&balanced);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_disable(&balanced);
        commit_changed_to(&mut votes, plan, false);
        let plan = votes.prepare_close(&balanced);
        commit_unchanged(&mut votes, plan);
        assert!(logs.lock().unwrap().is_empty());
    }

    #[fuchsia::test]
    fn orphan_on_close_holds_and_warns_once() {
        let (mut votes, logs) = bool_set();
        let vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&vote);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_enable(&vote);
        commit_unchanged(&mut votes, plan);

        let plan = votes.prepare_close(&vote);
        commit_unchanged(&mut votes, plan);
        assert!(votes.enabled());
        assert_eq!(votes.orphan_count(), 1);
        assert_eq!(votes.orphaned_total(), 1);

        // The whole count is orphaned as one entry.
        let e = entries(&votes);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, EntryKind::Orphan);
        assert_eq!(e[0].key, "node-a");
        assert_eq!(e[0].count, 2);

        let logs = logs.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, log::Level::Warn);
        assert!(logs[0].1.contains("clk-test"), "{}", logs[0].1);
        assert!(logs[0].1.contains("node-a"), "{}", logs[0].1);
        assert!(logs[0].1.contains("on"), "{}", logs[0].1);
    }

    #[fuchsia::test]
    fn reclaim_on_enable_from_same_node() {
        let (mut votes, _) = bool_set();
        let old_vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&old_vote);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_enable(&old_vote);
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_close(&old_vote);
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.orphan_count(), 1);

        // The rebound driver's enable reclaims the orphan in place: no hardware change.
        let new_vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&new_vote);
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.orphan_count(), 0);
        let e = entries(&votes);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, EntryKind::Live);
        assert_eq!(e[0].count, 1);

        // The new instance's count is not inflated by the old instance's enables.
        let plan = votes.prepare_disable(&new_vote);
        commit_changed_to(&mut votes, plan, false);
        assert_eq!(votes.transition_count(), 2);
    }

    #[fuchsia::test]
    fn reclaim_removes_all_orphans_of_node_only() {
        let (mut votes, logs) = bool_set();
        let a1 = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let a2 = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let b = Vote::new("node-b", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&a1);
        commit_changed_to(&mut votes, plan, true);
        for v in [&a2, &b] {
            let plan = votes.prepare_enable(v);
            commit_unchanged(&mut votes, plan);
        }
        for v in [&a1, &a2, &b] {
            let plan = votes.prepare_close(v);
            commit_unchanged(&mut votes, plan);
        }
        assert_eq!(votes.orphan_count(), 3);
        assert_eq!(count_logs(&logs, log::Level::Warn), 3);

        let a3 = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&a3);
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.orphan_count(), 1);
        let e = entries(&votes);
        assert_eq!(e.len(), 2);
        assert_eq!((e[0].kind, e[0].key.as_str()), (EntryKind::Orphan, "node-b"));
        assert_eq!((e[1].kind, e[1].key.as_str()), (EntryKind::Live, "node-a"));
    }

    #[fuchsia::test]
    fn disable_does_not_reclaim() {
        let (mut votes, _) = bool_set();
        let old_vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&old_vote);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_close(&old_vote);
        commit_unchanged(&mut votes, plan);

        let new_vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_disable(&new_vote);
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_clear(&new_vote);
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.orphan_count(), 1);
        assert!(votes.enabled());
    }

    #[fuchsia::test]
    fn node_started_releases_leftovers() {
        let (mut votes, _) = bool_set();
        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let b = Vote::new("node-b", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&a);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_enable(&b);
        commit_unchanged(&mut votes, plan);
        for v in [&a, &b] {
            let plan = votes.prepare_close(v);
            commit_unchanged(&mut votes, plan);
        }

        let plan = votes.prepare_node_started("node-c");
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.orphan_count(), 2);

        let plan = votes.prepare_node_started("node-a");
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.orphan_count(), 1);
        assert!(votes.enabled());

        let plan = votes.prepare_node_started("node-b");
        commit_changed_to(&mut votes, plan, false);
        assert_eq!(votes.orphan_count(), 0);
        assert_eq!(votes.orphaned_total(), 2);
    }

    #[fuchsia::test]
    fn node_started_keeps_live_votes() {
        let (mut votes, _) = bool_set();
        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&a);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_node_started("node-a");
        commit_unchanged(&mut votes, plan);
        assert!(votes.enabled());
    }

    #[fuchsia::test]
    fn node_started_failure_keeps_orphan() {
        let (mut votes, _) = bool_set();
        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&a);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_close(&a);
        commit_unchanged(&mut votes, plan);

        let plan = votes.prepare_node_started("node-a");
        assert!(plan.changed());
        votes.abandon(plan);
        assert_eq!(votes.orphan_count(), 1);
        assert!(votes.enabled());

        // Retried on the next event.
        let plan = votes.prepare_node_started("node-a");
        commit_changed_to(&mut votes, plan, false);
    }

    #[fuchsia::test]
    fn release_on_close() {
        let (mut votes, logs) = bool_set();
        let debug = Vote::new("debug", CloseBehavior::ReleaseOnClose);
        let plan = votes.prepare_enable(&debug);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_enable(&debug);
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_close(&debug);
        commit_changed_to(&mut votes, plan, false);
        assert_eq!(votes.entries().count(), 0);
        assert_eq!(votes.orphaned_total(), 0);
        assert!(logs.lock().unwrap().is_empty());
    }

    #[fuchsia::test]
    fn release_on_close_with_other_voter() {
        let (mut votes, _) = bool_set();
        let driver = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let debug = Vote::new("debug", CloseBehavior::ReleaseOnClose);
        let plan = votes.prepare_enable(&driver);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_enable(&debug);
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_close(&debug);
        commit_unchanged(&mut votes, plan);
        assert!(votes.enabled());
        assert_eq!(votes.entries().count(), 1);
    }

    #[fuchsia::test]
    fn release_on_close_failure_orphans() {
        let (mut votes, logs) = bool_set();
        let debug = Vote::new("debug", CloseBehavior::ReleaseOnClose);
        let plan = votes.prepare_enable(&debug);
        commit_changed_to(&mut votes, plan, true);

        let plan = votes.prepare_close(&debug);
        assert!(plan.changed());
        votes.abandon(plan);
        assert!(votes.enabled());
        assert_eq!(votes.orphan_count(), 1);
        assert_eq!(count_logs(&logs, log::Level::Warn), 1);
        {
            let logs = logs.lock().unwrap();
            assert!(logs[0].1.contains("could not be released"), "{}", logs[0].1);
            assert!(logs[0].1.contains("debug"), "{}", logs[0].1);
        }

        let plan = votes.prepare_node_started("debug");
        commit_changed_to(&mut votes, plan, false);
    }

    #[fuchsia::test]
    fn holds() {
        let (mut votes, _) = bool_set();
        let plan = votes.prepare_add_hold("boot", true);
        commit_changed_to(&mut votes, plan, true);
        // Re-adding a hold with the same name replaces it.
        let plan = votes.prepare_add_hold("boot", true);
        commit_unchanged(&mut votes, plan);
        let e = entries(&votes);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].kind, e[0].key.as_str()), (EntryKind::Hold, "boot"));

        let vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&vote);
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_disable(&vote);
        commit_unchanged(&mut votes, plan);
        assert!(votes.enabled());

        // Holds are not affected by node events.
        let plan = votes.prepare_node_started("boot");
        commit_unchanged(&mut votes, plan);
        assert!(votes.enabled());

        let plan = votes.prepare_release_hold("unknown");
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_release_hold("boot");
        commit_changed_to(&mut votes, plan, false);
        assert_eq!(votes.entries().count(), 0);
    }

    #[fuchsia::test]
    fn release_hold_failure_keeps_hold() {
        let (mut votes, logs) = bool_set();
        let plan = votes.prepare_add_hold("boot", true);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_release_hold("boot");
        assert!(plan.changed());
        votes.abandon(plan);
        assert!(votes.enabled());
        assert_eq!(votes.entries().count(), 1);
        assert!(logs.lock().unwrap().is_empty());
    }

    #[fuchsia::test]
    fn set_and_clear_are_idempotent() {
        let (mut votes, _) = bool_set();
        let vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_set(&vote, true);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_set(&vote, true);
        commit_unchanged(&mut votes, plan);
        assert_eq!(entries(&votes)[0].count, 1);
        let plan = votes.prepare_clear(&vote);
        commit_changed_to(&mut votes, plan, false);
        let plan = votes.prepare_clear(&vote);
        commit_unchanged(&mut votes, plan);
    }

    #[fuchsia::test]
    fn false_requests_merge_to_off() {
        let (mut votes, logs) = bool_set();
        let vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_set(&vote, false);
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.aggregate(), None);
        assert_eq!(votes.entries().count(), 1);

        // A vote that contributes nothing is dropped, not orphaned, on close.
        let plan = votes.prepare_close(&vote);
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.entries().count(), 0);
        assert_eq!(votes.orphan_count(), 0);
        assert_eq!(votes.orphaned_total(), 0);
        assert!(logs.lock().unwrap().is_empty());
    }

    #[fuchsia::test]
    fn abandoned_close_of_false_vote_drops_it() {
        let (mut votes, logs) = bool_set();
        let vote = Vote::new("debug", CloseBehavior::ReleaseOnClose);
        let plan = votes.prepare_set(&vote, false);
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_close(&vote);
        votes.abandon(plan);
        assert_eq!(votes.entries().count(), 0);
        assert_eq!(votes.orphaned_total(), 0);
        assert!(logs.lock().unwrap().is_empty());
    }

    #[fuchsia::test]
    fn release_on_close_without_request() {
        let (mut votes, logs) = bool_set();
        let debug = Vote::new("debug", CloseBehavior::ReleaseOnClose);
        let plan = votes.prepare_close(&debug);
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.entries().count(), 0);

        let balanced = Vote::new("debug", CloseBehavior::ReleaseOnClose);
        let plan = votes.prepare_enable(&balanced);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_disable(&balanced);
        commit_changed_to(&mut votes, plan, false);
        let plan = votes.prepare_close(&balanced);
        commit_unchanged(&mut votes, plan);
        assert_eq!(votes.entries().count(), 0);
        assert_eq!(votes.transition_count(), 2);
        assert!(logs.lock().unwrap().is_empty());
    }

    #[fuchsia::test(logging = false)]
    #[cfg_attr(debug_assertions, should_panic(expected = "stale plan"))]
    fn stale_plan_is_rejected() {
        let (mut votes, logs) = bool_set();
        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let b = Vote::new("node-b", CloseBehavior::OrphanOnClose);
        let plan_a = votes.prepare_enable(&a);
        let plan_b = votes.prepare_enable(&b);
        votes.commit(plan_a);
        votes.commit(plan_b);
        assert_eq!(votes.entries().count(), 1);
        assert_eq!(count_logs(&logs, log::Level::Error), 1);
    }

    #[fuchsia::test(logging = false)]
    #[cfg_attr(debug_assertions, should_panic(expected = "plan from a different VoteSet"))]
    fn foreign_plan_is_rejected() {
        let (mut votes, _) = bool_set();
        let (other, _) = bool_set();
        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = other.prepare_enable(&a);
        votes.commit(plan);
        assert_eq!(votes.entries().count(), 0);
    }

    #[fuchsia::test]
    async fn inspect() {
        let inspector = inspect::Inspector::default();
        let (mut votes, _) = bool_set();
        votes.attach_inspect(inspector.root().create_child("clk-test"));

        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let b = Vote::new("node-b", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&a);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_enable(&b);
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_close(&a);
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_add_hold("boot", true);
        commit_unchanged(&mut votes, plan);

        assert_data_tree!(inspector, root: {
            "clk-test": {
                resource: "clk-test",
                aggregate: "on",
                transition_count: 1u64,
                orphan_count: 1u64,
                orphaned_total: 1u64,
                entries: {
                    "0": {
                        kind: "orphan",
                        key: "node-a",
                        request: "on",
                        count: 1u64,
                        since: AnyProperty,
                    },
                    "1": {
                        kind: "live",
                        key: "node-b",
                        request: "on",
                        count: 1u64,
                        since: AnyProperty,
                    },
                    "2": {
                        kind: "hold",
                        key: "boot",
                        request: "on",
                        count: 1u64,
                        since: AnyProperty,
                    },
                },
            },
        });

        let plan = votes.prepare_node_started("node-a");
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_release_hold("boot");
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_disable(&b);
        commit_changed_to(&mut votes, plan, false);

        assert_data_tree!(inspector, root: {
            "clk-test": {
                resource: "clk-test",
                aggregate: "none",
                transition_count: 2u64,
                orphan_count: 0u64,
                orphaned_total: 1u64,
                entries: {},
            },
        });
    }

    #[fuchsia::test]
    async fn inspect_is_lazy() {
        let format_calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&format_calls);
        let mut votes =
            VoteSet::with_formatter("clk-test", merge_any_true, move |request: &bool| {
                counter.fetch_add(1, Ordering::SeqCst);
                if *request { "on" } else { "off" }.to_string()
            });

        let inspector = inspect::Inspector::default();
        votes.attach_inspect(inspector.root().create_child("clk-test"));

        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_enable(&a);
        commit_changed_to(&mut votes, plan, true);
        let plan = votes.prepare_add_hold("boot", true);
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_release_hold("boot");
        commit_unchanged(&mut votes, plan);
        let plan = votes.prepare_disable(&a);
        commit_changed_to(&mut votes, plan, false);
        let plan = votes.prepare_enable(&a);
        commit_changed_to(&mut votes, plan, true);

        // Neither `attach_inspect` nor `commit` formats requests eagerly.
        assert_eq!(format_calls.load(Ordering::SeqCst), 0);

        assert_data_tree!(inspector, root: {
            "clk-test": {
                resource: "clk-test",
                aggregate: "on",
                transition_count: 3u64,
                orphan_count: 0u64,
                orphaned_total: 0u64,
                entries: {
                    "0": {
                        kind: "live",
                        key: "node-a",
                        request: "on",
                        count: 1u64,
                        since: AnyProperty,
                    },
                },
            },
        });
        let calls_after_first_read = format_calls.load(Ordering::SeqCst);
        assert!(calls_after_first_read > 0);

        // Re-attaching replaces the previous node and lazy callback cleanly.
        votes.attach_inspect(inspector.root().create_child("clk-test-2"));
        assert_eq!(format_calls.load(Ordering::SeqCst), calls_after_first_read);

        assert_data_tree!(inspector, root: {
            "clk-test-2": {
                resource: "clk-test",
                aggregate: "on",
                transition_count: 3u64,
                orphan_count: 0u64,
                orphaned_total: 0u64,
                entries: {
                    "0": {
                        kind: "live",
                        key: "node-a",
                        request: "on",
                        count: 1u64,
                        since: AnyProperty,
                    },
                },
            },
        });

        // Dropping `votes` removes the node and lazy callback without dangling.
        let calls_before_drop = format_calls.load(Ordering::SeqCst);
        drop(votes);
        assert_data_tree!(inspector, root: {});
        assert_eq!(format_calls.load(Ordering::SeqCst), calls_before_drop);
    }

    #[fuchsia::test]
    fn merge_any_true_values() {
        assert_eq!(merge_any_true(&[]), None);
        assert_eq!(merge_any_true(&[false, false]), None);
        assert_eq!(merge_any_true(&[false, true]), Some(true));
    }

    /// A table-style request, as used by regulators.
    #[derive(Clone, Debug, Default, PartialEq)]
    struct RegulatorRequest {
        enable: bool,
        microvolts: u32,
    }

    fn merge_regulator(requests: &[RegulatorRequest]) -> Option<RegulatorRequest> {
        Some(requests.iter().fold(RegulatorRequest::default(), |acc, r| RegulatorRequest {
            enable: acc.enable || r.enable,
            microvolts: acc.microvolts.max(r.microvolts),
        }))
    }

    fn req(enable: bool, microvolts: u32) -> RegulatorRequest {
        RegulatorRequest { enable, microvolts }
    }

    fn table_set() -> (VoteSet<RegulatorRequest>, Logs) {
        let mut votes = VoteSet::new("ldo-1", merge_regulator);
        let logs: Logs = Default::default();
        let sink_logs = logs.clone();
        votes.set_log_sink_for_testing(Box::new(move |level, message| {
            sink_logs.lock().unwrap().push((level, message.to_string()));
        }));
        (votes, logs)
    }

    #[fuchsia::test]
    fn table_set_replaces_request_and_merges() {
        let (mut votes, _) = table_set();
        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let b = Vote::new("node-b", CloseBehavior::OrphanOnClose);

        let plan = votes.prepare_set(&a, req(true, 1_800_000));
        assert!(plan.changed());
        votes.commit(plan);
        assert_eq!(votes.aggregate(), Some(&req(true, 1_800_000)));

        let plan = votes.prepare_set(&b, req(false, 3_300_000));
        votes.commit(plan);
        assert_eq!(votes.aggregate(), Some(&req(true, 3_300_000)));

        // Replacing a's request does not stack.
        let plan = votes.prepare_set(&a, req(false, 1_200_000));
        votes.commit(plan);
        assert_eq!(votes.aggregate(), Some(&req(false, 3_300_000)));
        assert_eq!(entries(&votes)[0].count, 1);

        let plan = votes.prepare_clear(&b);
        assert_eq!(plan.old_aggregate(), Some(&req(false, 3_300_000)));
        assert_eq!(plan.new_aggregate(), Some(&req(false, 1_200_000)));
        votes.commit(plan);

        let plan = votes.prepare_clear(&a);
        assert_eq!(plan.new_aggregate(), None);
        votes.commit(plan);
        assert_eq!(votes.transition_count(), 5);
    }

    #[fuchsia::test]
    fn table_orphan_log_formats_request() {
        let (mut votes, logs) = table_set();
        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_set(&a, req(true, 1_800_000));
        votes.commit(plan);
        let plan = votes.prepare_close(&a);
        commit_unchanged(&mut votes, plan);
        let logs = logs.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].1.contains("ldo-1"), "{}", logs[0].1);
        assert!(logs[0].1.contains("node-a"), "{}", logs[0].1);
        assert!(logs[0].1.contains("microvolts: 1800000"), "{}", logs[0].1);
    }

    #[fuchsia::test]
    fn table_disabled_request_with_voltage_is_orphaned() {
        // `enable: false` still contributes a voltage, so it is held on close.
        let (mut votes, logs) = table_set();
        let a = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_set(&a, req(false, 1_800_000));
        votes.commit(plan);
        let plan = votes.prepare_close(&a);
        votes.commit(plan);
        assert_eq!(votes.orphan_count(), 1);
        assert_eq!(votes.aggregate(), Some(&req(false, 1_800_000)));
        assert_eq!(logs.lock().unwrap().len(), 1);
    }

    #[fuchsia::test]
    fn table_reclaim_replaces_orphan_request() {
        let (mut votes, _) = table_set();
        let old_vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_set(&old_vote, req(true, 3_300_000));
        votes.commit(plan);
        let plan = votes.prepare_close(&old_vote);
        votes.commit(plan);

        let new_vote = Vote::new("node-a", CloseBehavior::OrphanOnClose);
        let plan = votes.prepare_set(&new_vote, req(true, 1_800_000));
        assert_eq!(plan.new_aggregate(), Some(&req(true, 1_800_000)));
        votes.commit(plan);
        assert_eq!(votes.orphan_count(), 0);
    }

    #[fuchsia::test]
    fn table_hold_request_replaced() {
        let (mut votes, _) = table_set();
        let plan = votes.prepare_add_hold("extra-client", req(true, 0));
        votes.commit(plan);
        let plan = votes.prepare_add_hold("extra-client", req(true, 1_000_000));
        assert!(plan.changed());
        votes.commit(plan);
        let e = entries(&votes);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].request.microvolts, 1_000_000);
    }
}
