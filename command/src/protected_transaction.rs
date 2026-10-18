//! Two-phase coordination of the protected health-check headers of a cluster
//! across the main process and every worker.
//!
//! The main-process SetProtectedHealthCheck task uses [`Transaction`](crate::protected_transaction::Transaction), while each
//! worker uses [`WorkerStore`](crate::protected_transaction::WorkerStore) for its staged and committed versions. Their causal
//! rules are described below.
//!
//! Main side, [`Transaction`](crate::protected_transaction::Transaction): it proposes a
//! [`ProtectedHeaderChange`](crate::secret::ProtectedHeaderChange) in the
//! [`ProtectedHeaderJournal`](crate::secret::ProtectedHeaderJournal) and walks every targeted worker through
//! *prepare* (validate and stage), *commit* (apply) and, when anything fails,
//! *abort* (drop the staged proposal, or undo the applied one). The journal
//! confirms the proposal only when every worker acknowledged the commit; it
//! rejects it otherwise, once every worker is known to be coherent again. A
//! worker that cannot be made coherent is reported, to be restarted: a
//! restarted worker holds no value, which is coherent and probes nothing.
//!
//! A transaction carries a cluster id, a
//! [`ProtectedHeaderVersion`](crate::secret::ProtectedHeaderVersion), a phase
//! and per-worker acknowledgement flags. It never carries a value, a
//! fingerprint key or an encoded order, so a task may persist it.
//!
//! Worker side, [`WorkerStore`](crate::protected_transaction::WorkerStore): it resolves only versions it holds, applies
//! them in version order whatever the order of arrival, and keeps the state
//! before the applied version until it is released, so that an abort after a
//! commit restores exactly that state.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::{
    proto::command::HealthCheckConfig,
    secret::{
        ProtectedHeaderChange, ProtectedHeaderJournal, ProtectedHeaderSet, ProtectedHeaderVersion,
        Resolution, VersionExhausted,
    },
};

/// Identity of a worker within one transaction.
pub type WorkerId = u32;

/// Where a [`Transaction`] stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Every worker is asked to validate and stage the proposal.
    Preparing,
    /// Every worker staged it and is asked to apply it.
    Committing,
    /// Something failed: every worker is asked to drop or undo the proposal.
    Aborting,
}

/// What the owner of a [`Transaction`] sends next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Commit,
    Abort,
}

/// How a finished [`Transaction`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every worker acknowledged the commit; the journal confirmed the change.
    Committed,
    /// Every worker acknowledged the abort; the journal dropped the change.
    Aborted,
    /// The journal dropped the change, but these workers did not acknowledge
    /// the abort and may still hold the proposed values: restart them.
    Incoherent(Vec<WorkerId>),
}

/// What the owner of a [`Transaction`] does after an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing changed that needs an action.
    Wait,
    /// Send this order to these workers, which are the whole target set.
    Send(Order, Vec<WorkerId>),
    /// Reply to the client with this outcome.
    Done(Outcome),
}

/// Why a [`Transaction`] could not begin. Nothing was proposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BeginError {
    #[error("a protected health check change needs at least one worker")]
    NoWorker,
    #[error(transparent)]
    Exhausted(#[from] VersionExhausted),
}

/// One in-flight change of the protected headers of a cluster, as the main
/// process sees it. Persistable: see the module documentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    cluster_id: String,
    version: ProtectedHeaderVersion,
    phase: Phase,
    /// Every targeted worker, and whether it acknowledged the current phase.
    workers: BTreeMap<WorkerId, bool>,
    /// Workers whose abort failed. Only meaningful in [`Phase::Aborting`].
    unrecoverable: Vec<WorkerId>,
    finished: bool,
}

impl Transaction {
    /// Propose `change` for `cluster_id` and target `workers`. Refused before
    /// the journal is touched when there is no worker.
    pub fn begin(
        journal: &mut ProtectedHeaderJournal,
        cluster_id: &str,
        change: ProtectedHeaderChange,
        workers: impl IntoIterator<Item = WorkerId>,
    ) -> Result<Self, BeginError> {
        let workers: BTreeMap<WorkerId, bool> = workers.into_iter().map(|w| (w, false)).collect();
        if workers.is_empty() {
            return Err(BeginError::NoWorker);
        }
        let version = journal.propose(cluster_id, change)?;
        Ok(Self {
            cluster_id: cluster_id.to_owned(),
            version,
            phase: Phase::Preparing,
            workers,
            unrecoverable: Vec::new(),
            finished: false,
        })
    }

    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
    }

    pub fn version(&self) -> ProtectedHeaderVersion {
        self.version
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Every targeted worker, in order: the recipients of the prepare order.
    pub fn workers(&self) -> Vec<WorkerId> {
        self.workers.keys().copied().collect()
    }

    /// `worker` staged `version`.
    pub fn prepared(&mut self, worker: WorkerId, version: ProtectedHeaderVersion) -> Step {
        if !self.acknowledge(Phase::Preparing, worker, version) {
            return Step::Wait;
        }
        if self.all_acknowledged() {
            self.enter(Phase::Committing);
            return Step::Send(Order::Commit, self.workers());
        }
        Step::Wait
    }

    /// `worker` applied `version`; the last acknowledgement confirms it.
    pub fn committed(
        &mut self,
        journal: &mut ProtectedHeaderJournal,
        worker: WorkerId,
        version: ProtectedHeaderVersion,
    ) -> Step {
        if !self.acknowledge(Phase::Committing, worker, version) {
            return Step::Wait;
        }
        if self.all_acknowledged() {
            self.finished = true;
            // `Ignored` is legitimate: a newer version accepted first already
            // superseded this one, and the journal keeps the newer state.
            let _ = journal.accept(&self.cluster_id, self.version);
            return Step::Done(Outcome::Committed);
        }
        Step::Wait
    }

    /// `worker` refused `version`, or the order to it could not be queued.
    /// Before the commit is confirmed that starts the abort of every worker; a
    /// failed abort marks the worker as unrecoverable.
    pub fn failed(
        &mut self,
        journal: &mut ProtectedHeaderJournal,
        worker: WorkerId,
        version: ProtectedHeaderVersion,
    ) -> Step {
        if self.finished || version != self.version || !self.workers.contains_key(&worker) {
            return Step::Wait;
        }
        match self.phase {
            Phase::Preparing | Phase::Committing => self.begin_abort(),
            Phase::Aborting => {
                if !self.unrecoverable.contains(&worker) {
                    self.unrecoverable.push(worker);
                }
                self.workers.insert(worker, true);
                self.finish_abort(journal)
            }
        }
    }

    /// `worker` dropped or undid `version`.
    pub fn aborted(
        &mut self,
        journal: &mut ProtectedHeaderJournal,
        worker: WorkerId,
        version: ProtectedHeaderVersion,
    ) -> Step {
        if !self.acknowledge(Phase::Aborting, worker, version) {
            return Step::Wait;
        }
        self.finish_abort(journal)
    }

    /// The deadline passed. A transaction not yet aborting starts to abort;
    /// one already aborting gives up on the workers that did not answer.
    pub fn timed_out(&mut self, journal: &mut ProtectedHeaderJournal) -> Step {
        if self.finished {
            return Step::Wait;
        }
        match self.phase {
            Phase::Preparing | Phase::Committing => self.begin_abort(),
            Phase::Aborting => {
                let silent: Vec<WorkerId> = self
                    .workers
                    .iter()
                    .filter(|(_, acknowledged)| !**acknowledged)
                    .map(|(worker, _)| *worker)
                    .collect();
                for worker in silent {
                    if !self.unrecoverable.contains(&worker) {
                        self.unrecoverable.push(worker);
                    }
                    self.workers.insert(worker, true);
                }
                self.finish_abort(journal)
            }
        }
    }

    /// Record the acknowledgement of `worker` for `phase`; `false` when it is
    /// stale, foreign, out of phase or a repeat, in which case nothing changed.
    fn acknowledge(
        &mut self,
        phase: Phase,
        worker: WorkerId,
        version: ProtectedHeaderVersion,
    ) -> bool {
        if self.finished || self.phase != phase || version != self.version {
            return false;
        }
        match self.workers.get_mut(&worker) {
            Some(acknowledged) if !*acknowledged => {
                *acknowledged = true;
                true
            }
            _ => false,
        }
    }

    fn all_acknowledged(&self) -> bool {
        self.workers.values().all(|acknowledged| *acknowledged)
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        for acknowledged in self.workers.values_mut() {
            *acknowledged = false;
        }
    }

    /// Ask every worker to drop or undo the proposal: one that failed to
    /// prepare holds nothing, and a repeated abort is harmless.
    fn begin_abort(&mut self) -> Step {
        self.enter(Phase::Aborting);
        Step::Send(Order::Abort, self.workers())
    }

    fn finish_abort(&mut self, journal: &mut ProtectedHeaderJournal) -> Step {
        if !self.all_acknowledged() {
            return Step::Wait;
        }
        self.finished = true;
        journal.reject(&self.cluster_id, self.version);
        if self.unrecoverable.is_empty() {
            Step::Done(Outcome::Aborted)
        } else {
            Step::Done(Outcome::Incoherent(self.unrecoverable.to_owned()))
        }
    }
}

/// The configuration of a cluster together with the values its headers name.
#[derive(Clone)]
pub struct Payload {
    pub config: HealthCheckConfig,
    pub headers: ProtectedHeaderSet,
}

/// What a worker holds at one version; `None` is an absent cluster.
#[derive(Clone, Default)]
struct Held {
    version: Option<ProtectedHeaderVersion>,
    payload: Option<Payload>,
}

#[derive(Default)]
struct ClusterVersions {
    applied: Held,
    /// The state before `applied`, restored by an abort of `applied`.
    previous: Held,
    staged: BTreeMap<ProtectedHeaderVersion, Option<Payload>>,
}

/// Why a worker refused to stage a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StageError {
    #[error("protected health check version is older than the one already applied")]
    Stale,
}

/// What a commit did on a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commit {
    /// The version is now what the cluster holds.
    Applied,
    /// A newer version is already applied: nothing is overwritten.
    Superseded,
    /// The worker never staged this version, or already resolved it.
    Unknown,
}

/// The versioned protected headers one worker holds. No serde, no `Clone`,
/// and `Debug` prints counts only.
#[derive(Default)]
pub struct WorkerStore {
    clusters: HashMap<String, ClusterVersions>,
}

impl WorkerStore {
    /// Stage `payload` as `version` of `cluster`; `None` stages a removal.
    /// Nothing is applied. A version at or below the applied one is refused,
    /// and staging the same version again keeps the first payload.
    pub fn prepare(
        &mut self,
        cluster: &str,
        version: ProtectedHeaderVersion,
        payload: Option<Payload>,
    ) -> Result<(), StageError> {
        let versions = self.clusters.entry(cluster.to_owned()).or_default();
        if versions
            .applied
            .version
            .is_some_and(|applied| version <= applied)
        {
            return Err(StageError::Stale);
        }
        versions.staged.entry(version).or_insert(payload);
        Ok(())
    }

    /// Apply a staged `version`. Versions apply in order: one older than the
    /// applied version overwrites nothing, but it becomes the state an abort
    /// of the applied version restores when it is newer than the one kept.
    pub fn commit(&mut self, cluster: &str, version: ProtectedHeaderVersion) -> Commit {
        let Some(versions) = self.clusters.get_mut(cluster) else {
            return Commit::Unknown;
        };
        let Some(payload) = versions.staged.remove(&version) else {
            return Commit::Unknown;
        };
        let held = Held {
            version: Some(version),
            payload,
        };
        if versions
            .applied
            .version
            .is_none_or(|applied| version > applied)
        {
            versions.previous = std::mem::replace(&mut versions.applied, held);
            Commit::Applied
        } else {
            if versions.previous.version.is_none_or(|kept| version > kept) {
                versions.previous = held;
            }
            Commit::Superseded
        }
    }

    /// Drop a staged `version`; when it is the applied one, restore the state
    /// that preceded it. Anything else is `Ignored`.
    pub fn abort(&mut self, cluster: &str, version: ProtectedHeaderVersion) -> Resolution {
        let Some(versions) = self.clusters.get_mut(cluster) else {
            return Resolution::Ignored;
        };
        let mut changed = versions.staged.remove(&version).is_some();
        if versions.applied.version == Some(version) {
            versions.applied = std::mem::take(&mut versions.previous);
            changed = true;
        }
        self.forget_if_empty(cluster);
        if changed {
            Resolution::Applied
        } else {
            Resolution::Ignored
        }
    }

    /// The transaction that applied `version` succeeded everywhere: the state
    /// before it can no longer be wanted, and is dropped.
    pub fn release(&mut self, cluster: &str, version: ProtectedHeaderVersion) {
        if let Some(versions) = self.clusters.get_mut(cluster)
            && versions.applied.version == Some(version)
        {
            versions.previous = Held::default();
        }
        self.forget_if_empty(cluster);
    }

    /// The payload the cluster currently holds, if any.
    pub fn effective(&self, cluster: &str) -> Option<&Payload> {
        self.clusters.get(cluster)?.applied.payload.as_ref()
    }

    /// The cluster is gone: every held, kept and staged value goes with it.
    pub fn remove_cluster(&mut self, cluster: &str) {
        self.clusters.remove(cluster);
    }

    fn forget_if_empty(&mut self, cluster: &str) {
        if self.clusters.get(cluster).is_some_and(|v| {
            v.applied.version.is_none() && v.previous.version.is_none() && v.staged.is_empty()
        }) {
            self.clusters.remove(cluster);
        }
    }
}

impl std::fmt::Debug for WorkerStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerStore")
            .field("clusters", &self.clusters.len())
            .field(
                "staged",
                &self
                    .clusters
                    .values()
                    .map(|v| v.staged.len())
                    .sum::<usize>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroizing;

    use super::*;
    use crate::{
        proto::command::HealthCheckHeaderValue,
        secret::{FingerprintKey, ProtectedHeader, SecretValue},
    };

    const SENTINEL: &str = "SYNTHETIC-TXN-SECRET-77aa";

    fn set(value: &str) -> ProtectedHeaderSet {
        let key = FingerprintKey::from_parts(Zeroizing::new([5; 32]), [6; 16]);
        let mut carrier = HealthCheckHeaderValue {
            name: "authorization".to_owned(),
            value: value.as_bytes().to_vec(),
        };
        let secret = SecretValue::take_from_ingress(&mut carrier).unwrap();
        ProtectedHeaderSet::new(vec![ProtectedHeader::new(
            &key,
            "authorization".to_owned(),
            secret,
        )])
    }

    fn replace(value: &str) -> ProtectedHeaderChange {
        ProtectedHeaderChange::Replace(set(value))
    }

    fn payload(value: &str) -> Payload {
        Payload {
            config: HealthCheckConfig::default(),
            headers: set(value),
        }
    }

    fn held_ptr(journal: &ProtectedHeaderJournal, cluster: &str) -> Option<Vec<u8>> {
        journal
            .effective(cluster)
            .and_then(|s| s.headers().first())
            .map(|h| h.value().expose_secret_bytes().to_vec())
    }

    /// Drive a transaction through prepare and commit with every worker acking.
    fn commit_all(txn: &mut Transaction, journal: &mut ProtectedHeaderJournal) -> Step {
        let version = txn.version();
        let mut last = Step::Wait;
        for worker in txn.workers() {
            txn.prepared(worker, version);
        }
        for worker in txn.workers() {
            last = txn.committed(journal, worker, version);
        }
        last
    }

    #[test]
    fn begin_without_worker_touches_nothing() {
        let mut journal = ProtectedHeaderJournal::default();
        let error = Transaction::begin(&mut journal, "app", replace("a"), []).unwrap_err();
        assert_eq!(error, BeginError::NoWorker);
        assert!(journal.effective("app").is_none());
        // The next valid request progresses and gets the first version.
        let txn = Transaction::begin(&mut journal, "app", replace("a"), [1]).unwrap();
        assert_eq!(txn.phase(), Phase::Preparing);
        assert!(journal.effective("app").is_some());
    }

    #[test]
    fn all_workers_commit_confirms_the_journal() {
        let mut journal = ProtectedHeaderJournal::default();
        let mut txn = Transaction::begin(&mut journal, "app", replace("a"), [1, 2]).unwrap();
        let version = txn.version();

        assert_eq!(txn.prepared(1, version), Step::Wait);
        assert_eq!(
            txn.prepared(2, version),
            Step::Send(Order::Commit, vec![1, 2])
        );
        assert_eq!(txn.phase(), Phase::Committing);
        assert_eq!(txn.committed(&mut journal, 1, version), Step::Wait);
        // Not yet success: one worker has not acknowledged.
        assert!(!txn.is_finished());
        assert_eq!(
            txn.committed(&mut journal, 2, version),
            Step::Done(Outcome::Committed)
        );
        assert!(txn.is_finished());
        // Confirmed: rejecting it now finds nothing to drop.
        assert_eq!(journal.reject("app", version), Resolution::Ignored);
        assert!(journal.effective("app").is_some());
    }

    #[test]
    fn zero_acknowledgement_failure_aborts_everyone_and_drops_the_proposal() {
        let mut journal = ProtectedHeaderJournal::default();
        let mut txn = Transaction::begin(&mut journal, "app", replace("a"), [1, 2]).unwrap();
        let version = txn.version();

        assert_eq!(
            txn.failed(&mut journal, 1, version),
            Step::Send(Order::Abort, vec![1, 2])
        );
        assert_eq!(txn.aborted(&mut journal, 1, version), Step::Wait);
        assert_eq!(
            txn.aborted(&mut journal, 2, version),
            Step::Done(Outcome::Aborted)
        );
        assert!(journal.effective("app").is_none(), "no value is kept");
    }

    #[test]
    fn one_success_one_failure_never_commits() {
        let mut journal = ProtectedHeaderJournal::default();
        let mut txn = Transaction::begin(&mut journal, "app", replace("a"), [1, 2]).unwrap();
        let version = txn.version();
        assert_eq!(txn.prepared(1, version), Step::Wait);
        assert_eq!(
            txn.failed(&mut journal, 2, version),
            Step::Send(Order::Abort, vec![1, 2])
        );
        // A late prepare from the other worker cannot revive the commit.
        assert_eq!(txn.prepared(2, version), Step::Wait);
        assert_eq!(txn.committed(&mut journal, 1, version), Step::Wait);
        txn.aborted(&mut journal, 1, version);
        assert_eq!(
            txn.aborted(&mut journal, 2, version),
            Step::Done(Outcome::Aborted)
        );
        assert!(journal.effective("app").is_none());
    }

    #[test]
    fn failure_after_a_partial_commit_aborts_and_restores() {
        let mut journal = ProtectedHeaderJournal::default();
        let mut txn = Transaction::begin(&mut journal, "app", replace("a"), [1, 2]).unwrap();
        let version = txn.version();
        txn.prepared(1, version);
        txn.prepared(2, version);
        assert_eq!(txn.committed(&mut journal, 1, version), Step::Wait);
        // Timeout (or a failure, or an unqueueable commit) after worker 1 applied.
        assert_eq!(
            txn.timed_out(&mut journal),
            Step::Send(Order::Abort, vec![1, 2])
        );
        assert_eq!(txn.phase(), Phase::Aborting);
        // The commit acknowledgement of worker 2 arriving now is out of phase.
        assert_eq!(txn.committed(&mut journal, 2, version), Step::Wait);
        txn.aborted(&mut journal, 1, version);
        assert_eq!(
            txn.aborted(&mut journal, 2, version),
            Step::Done(Outcome::Aborted)
        );
        assert!(journal.effective("app").is_none());
    }

    #[test]
    fn a_failed_abort_is_reported_not_hidden() {
        let mut journal = ProtectedHeaderJournal::default();
        let mut txn = Transaction::begin(&mut journal, "app", replace("a"), [1, 2, 3]).unwrap();
        let version = txn.version();
        txn.failed(&mut journal, 1, version);
        assert_eq!(txn.aborted(&mut journal, 1, version), Step::Wait);
        // Worker 2's abort fails; worker 3 never answers.
        assert_eq!(txn.failed(&mut journal, 2, version), Step::Wait);
        assert_eq!(
            txn.timed_out(&mut journal),
            Step::Done(Outcome::Incoherent(vec![2, 3]))
        );
        assert!(journal.effective("app").is_none(), "main never commits");
    }

    #[test]
    fn stale_foreign_and_duplicate_acknowledgements_change_nothing() {
        let mut journal = ProtectedHeaderJournal::default();
        let mut first = Transaction::begin(&mut journal, "app", replace("a"), [1]).unwrap();
        let old = first.version();
        assert!(matches!(
            commit_all(&mut first, &mut journal),
            Step::Done(_)
        ));

        let mut second = Transaction::begin(&mut journal, "app", replace("b"), [1, 2]).unwrap();
        let version = second.version();
        let before = second.clone();
        assert_eq!(second.prepared(1, old), Step::Wait, "older version");
        assert_eq!(second.prepared(9, version), Step::Wait, "foreign worker");
        assert_eq!(second.committed(&mut journal, 1, version), Step::Wait);
        assert_eq!(second.aborted(&mut journal, 1, version), Step::Wait);
        assert_eq!(second.failed(&mut journal, 1, old), Step::Wait);
        assert_eq!(second, before);
        assert_eq!(second.prepared(1, version), Step::Wait);
        assert_eq!(second.prepared(1, version), Step::Wait, "duplicate");
        assert_eq!(
            second.prepared(2, version),
            Step::Send(Order::Commit, vec![1, 2])
        );
        // The finished first transaction ignores everything.
        let done = first.clone();
        assert_eq!(first.failed(&mut journal, 1, old), Step::Wait);
        assert_eq!(first.timed_out(&mut journal), Step::Wait);
        assert_eq!(first, done);
        assert!(journal.effective("app").is_some());
    }

    #[test]
    fn a_then_b_completes_in_either_order_and_the_newest_wins() {
        for a_first in [true, false] {
            let mut journal = ProtectedHeaderJournal::default();
            let mut a = Transaction::begin(&mut journal, "app", replace("a"), [1]).unwrap();
            let mut b = Transaction::begin(&mut journal, "app", replace("b"), [1]).unwrap();
            let b_ptr = held_ptr(&journal, "app");
            if a_first {
                commit_all(&mut a, &mut journal);
                commit_all(&mut b, &mut journal);
            } else {
                commit_all(&mut b, &mut journal);
                commit_all(&mut a, &mut journal);
            }
            assert!(a.is_finished() && b.is_finished());
            assert_eq!(held_ptr(&journal, "app"), b_ptr, "B stays effective");
            // Nothing is left pending: both versions are resolved.
            assert_eq!(journal.reject("app", a.version()), Resolution::Ignored);
            assert_eq!(journal.reject("app", b.version()), Resolution::Ignored);
        }
    }

    #[test]
    fn an_aborted_ancestor_leaves_a_live_descendant_alone() {
        let mut journal = ProtectedHeaderJournal::default();
        let mut a = Transaction::begin(&mut journal, "app", replace("a"), [1]).unwrap();
        let mut b = Transaction::begin(&mut journal, "app", replace("b"), [1]).unwrap();
        let b_ptr = held_ptr(&journal, "app");
        let version = a.version();
        a.failed(&mut journal, 1, version);
        assert_eq!(
            a.aborted(&mut journal, 1, version),
            Step::Done(Outcome::Aborted)
        );
        assert_eq!(held_ptr(&journal, "app"), b_ptr);
        assert!(matches!(
            commit_all(&mut b, &mut journal),
            Step::Done(Outcome::Committed)
        ));
    }

    #[test]
    fn a_persisted_transaction_carries_no_value() {
        let mut journal = ProtectedHeaderJournal::default();
        let txn = Transaction::begin(&mut journal, "app", replace(SENTINEL), [1, 2]).unwrap();
        let json = serde_json::to_string(&txn).unwrap();
        assert!(!json.contains(SENTINEL), "{json}");
        assert!(!json.contains("authorization"), "{json}");
        assert!(!format!("{txn:?}").contains(SENTINEL));
        assert!(!format!("{journal:?}").contains(SENTINEL));
        let back: Transaction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, txn);
    }

    fn versions(count: usize) -> Vec<ProtectedHeaderVersion> {
        let mut journal = ProtectedHeaderJournal::default();
        (0..count)
            .map(|_| journal.propose("v", ProtectedHeaderChange::Clear).unwrap())
            .collect()
    }

    fn ptr(store: &WorkerStore, cluster: &str) -> Option<Vec<u8>> {
        store
            .effective(cluster)
            .and_then(|p| p.headers.headers().first())
            .map(|h| h.value().expose_secret_bytes().to_vec())
    }

    #[test]
    fn worker_stages_without_applying_and_commit_applies() {
        let v = versions(1)[0];
        let mut store = WorkerStore::default();
        store.prepare("app", v, Some(payload("a"))).unwrap();
        assert!(store.effective("app").is_none(), "staged is not applied");
        assert_eq!(store.commit("app", v), Commit::Applied);
        assert!(store.effective("app").is_some());
        assert_eq!(store.commit("app", v), Commit::Unknown, "already resolved");
    }

    #[test]
    fn worker_abort_drops_staged_and_restores_the_applied_predecessor() {
        let v = versions(2);
        let mut store = WorkerStore::default();
        store.prepare("app", v[0], Some(payload("a"))).unwrap();
        store.commit("app", v[0]);
        let before = ptr(&store, "app");

        store.prepare("app", v[1], Some(payload("b"))).unwrap();
        assert_eq!(store.abort("app", v[1]), Resolution::Applied);
        assert_eq!(ptr(&store, "app"), before);

        store.prepare("app", v[1], Some(payload("b"))).unwrap();
        store.commit("app", v[1]);
        assert_ne!(ptr(&store, "app"), before);
        // Undo after the commit restores exactly the previous state.
        assert_eq!(store.abort("app", v[1]), Resolution::Applied);
        assert_eq!(ptr(&store, "app"), before);
        assert_eq!(store.abort("app", v[1]), Resolution::Ignored, "repeat");
    }

    #[test]
    fn worker_never_lets_an_older_version_overwrite_a_newer_one() {
        let v = versions(2);
        let mut store = WorkerStore::default();
        store.prepare("app", v[0], Some(payload("a"))).unwrap();
        store.prepare("app", v[1], Some(payload("b"))).unwrap();
        assert_eq!(store.commit("app", v[1]), Commit::Applied);
        let newest = ptr(&store, "app");
        assert_eq!(store.commit("app", v[0]), Commit::Superseded);
        assert_eq!(ptr(&store, "app"), newest);
        assert_eq!(
            store.prepare("app", v[0], Some(payload("a"))),
            Err(StageError::Stale)
        );
        // B fails afterwards: A, which was superseded, is what comes back.
        assert_eq!(store.abort("app", v[1]), Resolution::Applied);
        assert!(store.effective("app").is_some(), "A is restored, not lost");
        assert_ne!(ptr(&store, "app"), newest);
    }

    #[test]
    fn worker_release_forgets_the_predecessor() {
        let v = versions(2);
        let mut store = WorkerStore::default();
        store.prepare("app", v[0], Some(payload("a"))).unwrap();
        store.commit("app", v[0]);
        store.prepare("app", v[1], Some(payload("b"))).unwrap();
        store.commit("app", v[1]);
        let newest = ptr(&store, "app");
        store.release("app", v[1]);
        // A stale abort of another version changes nothing.
        assert_eq!(store.abort("app", v[0]), Resolution::Ignored);
        assert_eq!(ptr(&store, "app"), newest);
        // The predecessor is gone: the owner never aborts after a release,
        // and if it did the worker would hold nothing rather than old values.
        assert_eq!(store.abort("app", v[1]), Resolution::Applied);
        assert!(store.effective("app").is_none());
    }

    #[test]
    fn worker_clear_and_removal_erase_future_values() {
        let v = versions(3);
        let mut store = WorkerStore::default();
        store.prepare("app", v[0], Some(payload("a"))).unwrap();
        store.commit("app", v[0]);
        let held = store.effective("app").unwrap().headers.clone();
        assert_eq!(held.len(), 1);

        let cleared = Payload {
            config: HealthCheckConfig::default(),
            headers: ProtectedHeaderSet::new(Vec::new()),
        };
        store.prepare("app", v[1], Some(cleared)).unwrap();
        store.commit("app", v[1]);
        assert!(store.effective("app").unwrap().headers.is_empty());
        // The snapshot taken before the clear keeps its exact bytes.
        assert_eq!(
            held.headers()[0].value().expose_secret_bytes(),
            b"a".as_slice()
        );

        store.prepare("app", v[2], None).unwrap();
        store.commit("app", v[2]);
        assert!(store.effective("app").is_none());
        store.remove_cluster("app");
        assert_eq!(
            format!("{store:?}"),
            "WorkerStore { clusters: 0, staged: 0 }"
        );
    }

    #[test]
    fn worker_debug_prints_counts_only() {
        let v = versions(1)[0];
        let mut store = WorkerStore::default();
        store.prepare("app", v, Some(payload(SENTINEL))).unwrap();
        store.commit("app", v);
        let shown = format!("{store:?}");
        assert!(!shown.contains(SENTINEL) && !shown.contains("authorization"));
    }
}
