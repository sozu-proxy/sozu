//! The main-process side of the protected health-check headers: a change is
//! walked through every worker in four acknowledged steps before the client is
//! told it succeeded.
//!
//! 1. *capability*: a no-secret query every targeted worker must answer
//!    positively. A worker that predates the protocol decodes no verb and never
//!    does, so the absence of a positive answer is a failure. Nothing was sent
//!    but the query, and nothing changed;
//! 2. *prepare*: the single order that carries the clear values, which live in
//!    the channel buffers only. Every worker validates and stages; none
//!    applies;
//! 3. *commit*: every worker applies the staged version and acknowledges it;
//! 4. *release*: once the journal confirmed the version and the main state
//!    names its headers, every worker forgets the state the commit replaced.
//!
//! Any failure, refusal, silence or closed channel before the last commit
//! acknowledgement aborts every targeted worker; the journal rejects the
//! version, the main state is untouched and the client gets a failure. A worker
//! that cannot acknowledge its abort is stopped through the worker lifecycle.
//!
//! The values live in [`ProtectedRuntime::journal`] only. A task carries a
//! [`Transaction`] (cluster, version, per-worker flags), the public
//! configuration (names and fingerprints) and counters, and refuses to be
//! snapshotted: a change in flight blocks a main upgrade.
//!
//! The public verb enters [`begin_protected_health_check`]. A main upgrade hands
//! the idle runtime over through [`ProtectedRuntime::export_for_upgrade`] and
//! [`ProtectedRuntime::import_from_upgrade`] (protocol 3), and
//! [`begin_worker_bootstrap`] replays confirmed values to a new worker before
//! its Status can make it Running.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fmt,
    fs::File,
    path::Path,
};

use mio::Token;
use sozu_command_lib::{
    config::{
        ClusterConfig, Config, FileHealthCheckHeader, HEALTH_CHECK_MAX_HEADER_BYTES,
        validate_health_check_config, validate_health_check_header_values,
    },
    protected_transaction::{Order, Outcome, Step, Transaction},
    proto::command::{
        HealthCheckConfig, HealthCheckHeader, HealthCheckHeaderValue, ProtectedHealthCheckOrder,
        ProtectedHealthCheckPhase, ProtectedHealthCheckReply, QueryProtectedHealthCheckSupport,
        Request, ResponseContent, ResponseStatus, SetHealthCheck, SetProtectedHealthCheck,
        WorkerResponse, request::RequestType, response_content::ContentType,
    },
    secret::{
        ConfirmedBootstrap, FingerprintKey, ProtectedHeaderChange, ProtectedHeaderJournal,
        ProtectedHeaderVersion, ProtectedHealthCheck, ProtectedTransfer, TransferError,
        read_header_value,
    },
    state::ConfigState,
};

use crate::command::{
    server::{
        Gatherer, GatheringTask, MessageClient, Server, TaskSnapshot, TaskSnapshotError,
        TaskSnapshotTiming, Timeout, WorkerId,
    },
    sessions::{ClientSession, OptionalClient},
};

/// Whether the public `SetProtectedHealthCheck` and
/// `QueryProtectedHealthCheckSupport` verbs enter this path: the one gate both
/// share.
pub(crate) const PROTECTED_HEALTH_CHECK_PUBLIC: bool = true;

/// Why a main upgrade is refused while the runtime holds a fence, an
/// unresolved version, or confirmed header state that cannot be exported
/// exactly. Permanent on purpose: no retry empties it.
pub(crate) const PROTECTED_STATE_NOT_SNAPSHOTTABLE: &str = "protected health check state (a change fence, an unresolved version or confirmed header values that cannot be exported exactly) cannot be handed over to a new main process: reconciliation or controlled restart required";

const BUSY: &str = "a protected health check change is in flight; retry once it finished";
const BOOTSTRAPPING: &str = "a worker is being given the protected health check headers; retry once it is ready or proven stopped";
const INTERNAL: &str = "this is an internal worker verb and is refused on the command socket";
const CAPABILITY_UNCONFIRMED: &str =
    "not every worker confirmed support for protected health check headers: nothing changed";
const UNSUPPORTED: &str = "not every worker supports protected health check headers: nothing was sent to a worker and nothing changed";
const FAILED: &str = "the protected health check change failed on a worker: every worker was rolled back and nothing changed";
const INCOHERENT: &str = "the protected health check change failed and a worker could not confirm its rollback: it was stopped; nothing changed on the main process";
const UNFENCED: &str = "the protected health check change failed and a worker could not confirm its rollback, and stopping it could not be confirmed either: nothing changed on the main process, but that worker may still hold the proposed values";

/// The main process's protected state: never serialized, never cloned.
#[derive(Default)]
pub(crate) struct ProtectedRuntime {
    journal: ProtectedHeaderJournal,
    /// Generated on the first protected order, never persisted or logged.
    key: Option<FingerprintKey>,
    /// The version of the change in flight, per cluster: the fence every
    /// conflicting mutation of that cluster waits behind.
    active: HashMap<String, ProtectedHeaderVersion>,
    /// Where the file-backed headers of the startup configuration stand.
    cold: ColdStart,
}

/// The file-backed headers of the configuration a cold main process loads.
/// Their values are held here, and nowhere else, until the protected
/// transaction applies them or startup is decided.
#[derive(Default)]
enum ColdStart {
    /// Nothing to apply, or everything applied: the start is settled.
    #[default]
    Settled,
    /// Orders waiting for the generic startup tasks, by cluster id, which is
    /// the lexical order they are applied in.
    Pending(BTreeMap<String, SetProtectedHealthCheck>),
    /// The orders are being applied, one cluster at a time; the clusters of
    /// the whole sequence stay reserved until it is decided.
    Running(BTreeSet<String>),
    /// A value could not be read or applied: permanent until the main process
    /// restarts, so the cold `READY=1` is never announced.
    Failed,
}

impl fmt::Debug for ColdStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Settled => f.write_str("Settled"),
            Self::Pending(orders) => write!(f, "Pending({})", orders.len()),
            Self::Running(clusters) => write!(f, "Running({})", clusters.len()),
            Self::Failed => f.write_str("Failed"),
        }
    }
}

impl fmt::Debug for ProtectedRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProtectedRuntime")
            .field("journal", &self.journal)
            .field("key", &self.key.is_some())
            .field("active", &self.active.len())
            .field("cold", &self.cold)
            .finish()
    }
}

impl ProtectedRuntime {
    pub(crate) fn is_active(&self, cluster: &str) -> bool {
        self.active.contains_key(cluster)
    }

    /// Keep the file-backed orders of the startup configuration until the
    /// generic startup tasks are over.
    pub(crate) fn hold_cold_orders(&mut self, orders: VecDeque<SetProtectedHealthCheck>) {
        self.cold = if orders.is_empty() {
            ColdStart::Settled
        } else {
            ColdStart::Pending(
                orders
                    .into_iter()
                    .map(|order| (order.cluster_id.to_owned(), order))
                    .collect(),
            )
        };
    }

    /// The startup configuration named values that cannot be applied: drop
    /// them, and keep the cold `READY=1` closed.
    pub(crate) fn fail_cold_start(&mut self) {
        self.cold = ColdStart::Failed;
    }

    /// A saved-state entry was accepted and is being dispatched: the state
    /// file is the authority of a cluster it adds, removes or rewrites the
    /// health check of, so the configuration file's order for it is dropped
    /// unread by any worker. Only the verb and the cluster id are compared:
    /// the fingerprints of another epoch are never matched against a value.
    pub(crate) fn saved_entry_dispatched(&mut self, request: &RequestType) {
        let ColdStart::Pending(orders) = &mut self.cold else {
            return;
        };
        if let Some((cluster, _)) = ordinary_health_check_change(request) {
            orders.remove(cluster);
        }
    }

    /// Whether the startup is decided without any pending or failed
    /// file-backed order.
    pub(crate) fn cold_start_settled(&self) -> bool {
        matches!(self.cold, ColdStart::Settled)
    }

    /// Whether a change in flight or a held cold order owns `cluster`.
    fn is_busy(&self, cluster: &str) -> bool {
        self.is_active(cluster) || self.is_cold_reserved(cluster)
    }

    /// Whether the unsettled cold start holds an order for `cluster`: its file
    /// is the authority of that cluster until the sequence is decided, even
    /// before the order enters `active`.
    fn is_cold_reserved(&self, cluster: &str) -> bool {
        match &self.cold {
            ColdStart::Pending(orders) => orders.contains_key(cluster),
            ColdStart::Running(clusters) => clusters.contains(cluster),
            ColdStart::Settled | ColdStart::Failed => false,
        }
    }

    /// Whether a change in flight forbids `request` on the command socket
    /// before anything of it is applied: any order that rewrites the health
    /// check or the existence of the cluster or the validity of its health
    /// check against its backends, and the bulk paths that replay
    /// orders without naming one.
    pub(crate) fn blocks(&self, request: &RequestType) -> bool {
        // The orders of a startup that is not over are not a state a reload, a
        // state load or a worker upgrade could build on.
        if self.active.is_empty() && self.cold_start_settled() {
            return false;
        }
        match request {
            RequestType::AddCluster(cluster) => self.is_busy(&cluster.cluster_id),
            // A unix backend may not share a cluster with a health check port:
            // the workers validated that at prepare time, so it cannot change
            // before the commit.
            RequestType::AddUnixBackend(backend) => self.is_busy(&backend.cluster_id),
            RequestType::SetHealthCheck(set) => self.is_busy(&set.cluster_id),
            RequestType::SetProtectedHealthCheck(set) => self.is_busy(&set.cluster_id),
            RequestType::RemoveHealthCheck(cluster) | RequestType::RemoveCluster(cluster) => {
                self.is_busy(cluster)
            }
            RequestType::LoadState(_)
            | RequestType::ReloadConfiguration(_)
            | RequestType::UpgradeWorker(_) => true,
            _ => false,
        }
    }

    /// An ordinary (non-protected) order rewrote the health check of `cluster`
    /// with `headers`: the held values survive only when `headers` names
    /// exactly the headers they were issued for.
    pub(crate) fn ordinary_change(&mut self, cluster: &str, headers: &[HealthCheckHeader]) {
        let Some(held) = self.journal.effective(cluster) else {
            return;
        };
        if !headers.is_empty() && held.satisfies(headers) {
            return;
        }
        match self.journal.propose(cluster, ProtectedHeaderChange::Remove) {
            Ok(version) => {
                self.journal.accept(cluster, version);
            }
            Err(error) => error!(
                "could not drop the protected headers of a cluster: {}",
                error
            ),
        }
    }

    /// The confirmed state a new main process takes over: the key, the number
    /// of versions issued and every confirmed header set. Refused while a
    /// fence is up, whether or not a task still owns it, while any version is
    /// unresolved, and when a confirmed set is not the one `state` names.
    pub(crate) fn export_for_upgrade(
        &self,
        state: &ConfigState,
    ) -> Result<ProtectedTransfer, TransferError> {
        if !self.active.is_empty() || !self.cold_start_settled() {
            return Err(TransferError::Pending);
        }
        ProtectedTransfer::export(&self.journal, self.key.as_ref(), cluster_headers(state))
    }

    /// The runtime a new main process builds from a transfer, once `state`
    /// names exactly what it carries. No fence survives a hand-over.
    pub(crate) fn import_from_upgrade(
        transfer: ProtectedTransfer,
        state: &ConfigState,
    ) -> Result<Self, TransferError> {
        let (key, journal) = transfer.restore(cluster_headers(state))?;
        Ok(Self {
            journal,
            key,
            active: HashMap::new(),
            cold: ColdStart::Settled,
        })
    }

    /// What a fresh worker must be given before it may answer `Status`: the
    /// confirmed clusters and the version they are staged under, `None` when
    /// no value is held. Read-only. An error while a change is in flight or
    /// fenced, and when `state` does not name the confirmed sets exactly: a
    /// worker started then would diverge from main.
    pub(crate) fn bootstrap_plan(
        &self,
        state: &ConfigState,
    ) -> Result<Option<ConfirmedBootstrap>, TransferError> {
        if !self.active.is_empty() {
            return Err(TransferError::Pending);
        }
        self.journal.confirmed_bootstrap(cluster_headers(state))
    }

    /// Whether a main upgrade can hand this runtime over exactly.
    pub(crate) fn is_snapshot_safe(&self, state: &ConfigState) -> bool {
        self.export_for_upgrade(state).is_ok()
    }

    #[cfg(test)]
    pub(crate) fn held_set(
        &self,
        cluster: &str,
    ) -> Option<sozu_command_lib::secret::ProtectedHeaderSet> {
        self.journal.effective(cluster).cloned()
    }

    #[cfg(test)]
    pub(crate) fn held_values(&self, cluster: &str) -> Option<Vec<Vec<u8>>> {
        self.journal.effective(cluster).map(|set| {
            set.headers()
                .iter()
                .map(|header| header.value().expose_secret_bytes().to_vec())
                .collect()
        })
    }
}

/// The header metadata `state` holds for each cluster: the authority a
/// transferred set is checked against.
fn cluster_headers<'a>(
    state: &'a ConfigState,
) -> impl Fn(&str) -> Option<&'a [HealthCheckHeader]> + 'a {
    |cluster| {
        state
            .clusters
            .get(cluster)
            .and_then(|cluster| cluster.health_check.as_ref())
            .map(|config| config.headers.as_slice())
    }
}

/// The cluster and header metadata an ordinary order rewrites, for
/// [`ProtectedRuntime::ordinary_change`].
pub(crate) fn ordinary_health_check_change(
    request: &RequestType,
) -> Option<(&str, &[HealthCheckHeader])> {
    match request {
        RequestType::AddCluster(cluster) => Some((
            &cluster.cluster_id,
            cluster
                .health_check
                .as_ref()
                .map_or(&[][..], |config| &config.headers),
        )),
        RequestType::SetHealthCheck(set) => Some((&set.cluster_id, &set.config.headers)),
        RequestType::RemoveHealthCheck(cluster) | RequestType::RemoveCluster(cluster) => {
            Some((cluster, &[]))
        }
        _ => None,
    }
}

/// Refuse the internal worker verbs when a client sends them.
pub(crate) fn refuse_internal_verb(client: &mut ClientSession) {
    client.finish_failure(INTERNAL);
}

/// Refuse `request` when a change in flight forbids it.
pub(crate) fn refuse_while_busy(
    server: &Server,
    client: &mut ClientSession,
    request: &RequestType,
) -> bool {
    if server.protected.blocks(request) {
        client.finish_failure(BUSY);
        return true;
    }
    // A worker being given the confirmed values takes no ordinary order until
    // it is ready: a command that would reach the other workers and not it
    // waits, before anything of it is applied. Queries, the state dump and the
    // stop orders (which reach every child) stay available.
    if server.worker_bootstrap_in_flight()
        && (crate::command::requests::is_mutating_verb(request)
            || matches!(request, RequestType::SetProtectedHealthCheck(_)))
        && !matches!(
            request,
            RequestType::SaveState(_) | RequestType::SoftStop(_) | RequestType::HardStop(_)
        )
    {
        client.finish_failure(BOOTSTRAPPING);
        return true;
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Capability,
    Prepare,
    Commit,
    Abort,
}

impl Stage {
    fn phase(self) -> ProtectedHealthCheckPhase {
        match self {
            Stage::Capability => ProtectedHealthCheckPhase::ProtectedHealthCheckCapability,
            Stage::Prepare => ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare,
            Stage::Commit => ProtectedHealthCheckPhase::ProtectedHealthCheckCommit,
            Stage::Abort => ProtectedHealthCheckPhase::ProtectedHealthCheckAbort,
        }
    }
}

/// Whether `message` is a success whose reply names exactly this `phase`,
/// `cluster` and `version`: no other shape is an acknowledgement.
fn is_exact_reply(
    message: &WorkerResponse,
    phase: ProtectedHealthCheckPhase,
    cluster: &str,
    version: u64,
) -> bool {
    if !matches!(
        ResponseStatus::try_from(message.status),
        Ok(ResponseStatus::Ok)
    ) {
        return false;
    }
    let Some(ContentType::ProtectedHealthCheckReply(reply)) = message
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        return false;
    };
    reply.phase == phase as i32 && reply.cluster_id == cluster && reply.version == version
}

/// One step of a protected change, and the answers gathered for it.
struct ProtectedHealthCheckTask {
    /// `None` for the internal sequence of a cold start, which has no client.
    client_token: Option<Token>,
    stage: Stage,
    cluster_id: String,
    /// Names and fingerprints only: what the main state keeps on success.
    config: HealthCheckConfig,
    txn: Transaction,
    expected: usize,
    answered: BTreeSet<WorkerId>,
    capable: BTreeSet<WorkerId>,
    capability_refused: bool,
    /// The latest decision the transaction reached from the answers.
    step: Option<Step>,
    /// The configuration reload this change belongs to: its remaining clusters
    /// follow once this one is decided, and its client hears one answer.
    reload: Option<Box<ReloadProtected>>,
}

impl fmt::Debug for ProtectedHealthCheckTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProtectedHealthCheckTask")
            .field("stage", &self.stage)
            .field("version", &self.txn.version())
            .field("expected", &self.expected)
            .field("answered", &self.answered.len())
            .finish()
    }
}

impl ProtectedHealthCheckTask {
    fn new(
        client_token: Option<Token>,
        stage: Stage,
        config: HealthCheckConfig,
        txn: Transaction,
    ) -> Self {
        Self {
            client_token,
            stage,
            cluster_id: txn.cluster_id().to_owned(),
            config,
            txn,
            expected: 0,
            answered: BTreeSet::new(),
            capable: BTreeSet::new(),
            capability_refused: false,
            step: None,
            reload: None,
        }
    }

    /// Whether `message` is the positive acknowledgement of this very step: a
    /// success carrying a reply for this phase, cluster and version.
    fn acknowledges(&self, message: &WorkerResponse) -> bool {
        let (cluster, version) = match self.stage {
            Stage::Capability => ("", 0),
            _ => (self.cluster_id.as_str(), self.txn.version().get()),
        };
        is_exact_reply(message, self.stage.phase(), cluster, version)
    }

    fn decide(&mut self, step: Step) {
        if !matches!(step, Step::Wait) {
            self.step = Some(step);
        }
    }
}

impl Gatherer for ProtectedHealthCheckTask {
    fn inc_expected_responses(&mut self, count: usize) {
        self.expected += count;
    }

    fn has_finished(&self) -> bool {
        self.step.is_some() || self.answered.len() >= self.expected
    }

    fn on_message(
        &mut self,
        server: &mut Server,
        _client: &mut OptionalClient,
        worker_id: WorkerId,
        message: WorkerResponse,
    ) {
        let status = ResponseStatus::try_from(message.status);
        if matches!(status, Ok(ResponseStatus::Processing)) || self.answered.contains(&worker_id) {
            return;
        }
        let version = self.txn.version();
        let positive = self.acknowledges(&message);
        let failure = !matches!(status, Ok(ResponseStatus::Ok));
        self.answered.insert(worker_id);
        // An answer that is neither the acknowledgement of this step nor a
        // failure (an old version, a foreign cluster or phase) is not trusted
        // and not counted: the step then ends without a decision, and aborts.
        if !positive && !failure {
            return;
        }
        let journal = &mut server.protected.journal;
        let step = match self.stage {
            Stage::Capability => {
                if positive {
                    self.capable.insert(worker_id);
                } else {
                    self.capability_refused = true;
                }
                return;
            }
            Stage::Prepare if positive => self.txn.prepared(worker_id, version),
            Stage::Commit if positive => {
                // The main state must still be able to name the headers when
                // the last worker applied them.
                if server.state.clusters.contains_key(self.cluster_id.as_str()) {
                    self.txn.committed(journal, worker_id, version)
                } else {
                    self.txn.failed(journal, worker_id, version)
                }
            }
            Stage::Abort if positive => self.txn.aborted(journal, worker_id, version),
            _ => self.txn.failed(journal, worker_id, version),
        };
        self.decide(step);
    }
}

impl GatheringTask for ProtectedHealthCheckTask {
    fn kind(&self) -> &'static str {
        "protected_health_check"
    }

    fn client_token(&self) -> Option<Token> {
        self.client_token
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        self
    }

    fn snapshot(&self, _timing: TaskSnapshotTiming) -> Result<TaskSnapshot, TaskSnapshotError> {
        Err(TaskSnapshotError::ProtectedTransactionInFlight)
    }

    fn on_finish(
        mut self: Box<Self>,
        server: &mut Server,
        client: &mut OptionalClient,
        timed_out: bool,
    ) {
        if self.stage == Stage::Capability {
            let supported = !timed_out
                && !self.capability_refused
                && self.capable.len() == self.txn.workers().len();
            if !supported {
                // Nothing but the query was sent: the proposal is dropped.
                server
                    .protected
                    .journal
                    .reject(&self.cluster_id, self.txn.version());
                server.protected.active.remove(&self.cluster_id);
                conclude_err(server, client, self.reload.take(), UNSUPPORTED);
                return;
            }
            advance(server, client, *self, Step::Wait, true);
            return;
        }
        let step = match self.step.take() {
            Some(step) => step,
            None => self.txn.timed_out(&mut server.protected.journal),
        };
        advance(server, client, *self, step, false);
    }
}

/// The order a worker receives at `stage`. `Prepare` alone carries values,
/// copied out of the journal into carriers that wipe themselves on drop.
fn order_request(
    server: &Server,
    task: &ProtectedHealthCheckTask,
    stage: Stage,
) -> Option<Request> {
    let version = task.txn.version();
    let prepare = match stage {
        Stage::Prepare => {
            let headers = server
                .protected
                .journal
                .proposed(&task.cluster_id, version)??
                .to_owned();
            let protected = ProtectedHealthCheck::from_parts(
                task.cluster_id.to_owned(),
                task.config.to_owned(),
                headers,
            )
            .ok()?;
            Some(protected.to_order())
        }
        _ => None,
    };
    Some(
        RequestType::ProtectedHealthCheckOrder(ProtectedHealthCheckOrder {
            phase: stage.phase() as i32,
            cluster_id: task.cluster_id.to_owned(),
            version: version.get(),
            prepare,
        })
        .into(),
    )
}

/// Carry the transaction to its next step. `to_prepare` marks the move from
/// the capability query to the prepare order, which is not a transaction step.
fn advance(
    server: &mut Server,
    client: &mut OptionalClient,
    mut task: ProtectedHealthCheckTask,
    mut step: Step,
    to_prepare: bool,
) {
    let version = task.txn.version();
    let mut next_stage = to_prepare.then_some(Stage::Prepare);
    loop {
        match step {
            Step::Send(order, workers) => {
                // A target that is no longer reachable cannot take the order:
                // before the commit is confirmed that is a failure, and
                // during the abort it is a worker to stop.
                let dead: Vec<WorkerId> = workers
                    .iter()
                    .copied()
                    .filter(|worker| !server.is_worker_reachable(*worker))
                    .collect();
                let mut decided = None;
                for worker in dead {
                    let decision = task
                        .txn
                        .failed(&mut server.protected.journal, worker, version);
                    if !matches!(decision, Step::Wait) {
                        decided = Some(decision);
                    }
                }
                if let Some(decision) = decided {
                    step = decision;
                    continue;
                }
                next_stage = Some(match order {
                    Order::Commit => Stage::Commit,
                    Order::Abort => Stage::Abort,
                });
                break;
            }
            Step::Done(outcome) => {
                finalize(server, client, task, outcome);
                return;
            }
            Step::Wait => break,
        }
    }
    let Some(stage) = next_stage else {
        error!("a protected health check change ended without a decision");
        abort_without_decision(server, client, task);
        return;
    };
    let Some(request) = order_request(server, &task, stage) else {
        error!("could not build the protected health check order");
        abort_without_decision(server, client, task);
        return;
    };
    let workers = task.txn.workers();
    task.stage = stage;
    task.expected = 0;
    task.answered.clear();
    task.step = None;
    server.scatter_to(request, Box::new(task), Timeout::Default, &workers);
}

/// The transaction cannot continue and has no decision: abort it.
fn abort_without_decision(
    server: &mut Server,
    client: &mut OptionalClient,
    mut task: ProtectedHealthCheckTask,
) {
    let outcome = task.txn.timed_out(&mut server.protected.journal);
    match outcome {
        Step::Done(outcome) => finalize(server, client, task, outcome),
        Step::Send(Order::Abort, _) => {
            let workers = task.txn.workers();
            let Some(request) = order_request(server, &task, Stage::Abort) else {
                conclude_err(server, client, task.reload.take(), FAILED);
                return;
            };
            task.stage = Stage::Abort;
            task.expected = 0;
            task.answered.clear();
            task.step = None;
            server.scatter_to(request, Box::new(task), Timeout::Default, &workers);
        }
        _ => {
            server.protected.active.remove(&task.cluster_id);
            conclude_err(server, client, task.reload.take(), FAILED);
        }
    }
}

fn finalize(
    server: &mut Server,
    client: &mut OptionalClient,
    mut task: ProtectedHealthCheckTask,
    outcome: Outcome,
) {
    match outcome {
        Outcome::Committed => {
            // Every worker applied the version and the journal confirmed it:
            // the main state names its headers, then the workers release.
            let applied = server.state.dispatch(
                &RequestType::SetHealthCheck(SetHealthCheck {
                    cluster_id: task.cluster_id.to_owned(),
                    config: task.config.to_owned(),
                })
                .into(),
            );
            if let Err(error) = applied {
                error!(
                    "could not record a committed protected health check, stopping its workers: {}",
                    error
                );
                let reason = fence_all(server, &task.cluster_id, task.txn.workers());
                conclude_err(server, client, task.reload.take(), reason);
                return;
            }
            server.protected.active.remove(&task.cluster_id);
            let workers = task.txn.workers();
            let release = RequestType::ProtectedHealthCheckOrder(ProtectedHealthCheckOrder {
                phase: ProtectedHealthCheckPhase::ProtectedHealthCheckRelease as i32,
                cluster_id: task.cluster_id.to_owned(),
                version: task.txn.version().get(),
                prepare: None,
            })
            .into();
            server.scatter_to(
                release,
                Box::new(ReleaseTask::default()),
                Timeout::Default,
                &workers,
            );
            let message = format!(
                "protected health check applied on {} worker(s)",
                workers.len()
            );
            conclude_ok(server, client, task.reload.take(), message);
        }
        Outcome::Aborted => {
            server.protected.active.remove(&task.cluster_id);
            conclude_err(server, client, task.reload.take(), FAILED);
        }
        Outcome::Incoherent(workers) => {
            let reason = fence_all(server, &task.cluster_id, workers);
            conclude_err(server, client, task.reload.take(), reason);
        }
    }
}

/// Stop every worker of `workers` and name what the client may be told: the
/// worker was stopped only when its `SIGKILL` is proven, never otherwise.
///
/// The fence of `cluster` is lifted only when every worker is proven stopped.
/// Otherwise a worker may still hold the proposed values, so the cluster stays
/// behind its fence — no later change may start on it — until the main process
/// restarts; no other cluster is affected.
fn fence_all(server: &mut Server, cluster: &str, workers: Vec<WorkerId>) -> &'static str {
    let mut reason = INCOHERENT;
    for worker in workers {
        if !server.fence_worker(worker) {
            error!("could not prove that worker {} was stopped", worker);
            reason = UNFENCED;
        }
    }
    if reason == INCOHERENT {
        server.protected.active.remove(cluster);
    } else {
        error!(
            "cluster {} stays blocked: a worker may still hold a protected health check change",
            cluster
        );
    }
    reason
}

/// Waits for the workers to forget the state a commit replaced. A release that
/// is lost only keeps that state until the cluster is next changed, so it is
/// reported and nothing else.
#[derive(Debug, Default)]
struct ReleaseTask {
    gatherer: crate::command::server::DefaultGatherer,
}

impl GatheringTask for ReleaseTask {
    fn kind(&self) -> &'static str {
        "protected_health_check_release"
    }

    fn client_token(&self) -> Option<Token> {
        None
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn snapshot(&self, _timing: TaskSnapshotTiming) -> Result<TaskSnapshot, TaskSnapshotError> {
        Err(TaskSnapshotError::ProtectedTransactionInFlight)
    }

    fn on_finish(self: Box<Self>, _: &mut Server, _: &mut OptionalClient, timed_out: bool) {
        if timed_out || self.gatherer.errors > 0 {
            warn!(
                "a worker did not confirm the release of a protected health check (ok={}, errors={}, timed_out={})",
                self.gatherer.ok, self.gatherer.errors, timed_out
            );
        }
    }
}

/// The read-only capability query: which workers were asked, and how they
/// answered. It carries no cluster, no version and no value, so it owns no
/// fence and no journal entry, and nothing of it is ever replayed.
#[derive(Debug)]
struct CapabilityQueryTask {
    client_token: Token,
    targets: BTreeSet<WorkerId>,
    expected: usize,
    answered: BTreeSet<WorkerId>,
    capable: BTreeSet<WorkerId>,
    /// A worker answered something other than the exact acknowledgement.
    refused: bool,
}

impl Gatherer for CapabilityQueryTask {
    fn inc_expected_responses(&mut self, count: usize) {
        self.expected += count;
    }

    fn has_finished(&self) -> bool {
        self.refused || self.answered.len() >= self.expected
    }

    fn on_message(
        &mut self,
        _server: &mut Server,
        _client: &mut OptionalClient,
        worker_id: WorkerId,
        message: WorkerResponse,
    ) {
        // A progress report is no answer: it neither counts nor extends the
        // deadline, whatever it carries.
        if matches!(
            ResponseStatus::try_from(message.status),
            Ok(ResponseStatus::Processing)
        ) || !self.targets.contains(&worker_id)
            || !self.answered.insert(worker_id)
        {
            return;
        }
        let phase = ProtectedHealthCheckPhase::ProtectedHealthCheckCapability;
        if is_exact_reply(&message, phase, "", 0) {
            self.capable.insert(worker_id);
        } else {
            self.refused = true;
        }
    }
}

impl GatheringTask for CapabilityQueryTask {
    fn kind(&self) -> &'static str {
        "protected_health_check_capability"
    }

    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        self
    }

    /// There is no wire form for this task: a main upgrade waits for it.
    fn snapshot(&self, _timing: TaskSnapshotTiming) -> Result<TaskSnapshot, TaskSnapshotError> {
        Err(TaskSnapshotError::ProtectedTransactionInFlight)
    }

    fn on_finish(self: Box<Self>, _: &mut Server, client: &mut OptionalClient, timed_out: bool) {
        let supported =
            !timed_out && !self.refused && !self.targets.is_empty() && self.capable == self.targets;
        match client.as_deref_mut() {
            // The exact typed payload is the only positive answer a client
            // accepts: a generic `Ok` is a refusal.
            Some(client) if supported => client.finish_ok_with_content(
                ResponseContent {
                    content_type: Some(ContentType::ProtectedHealthCheckReply(
                        ProtectedHealthCheckReply {
                            phase: ProtectedHealthCheckPhase::ProtectedHealthCheckCapability as i32,
                            cluster_id: String::new(),
                            version: 0,
                        },
                    )),
                },
                format!(
                    "protected health checks are supported by {} worker(s)",
                    self.targets.len()
                ),
            ),
            Some(client) => client.finish_failure(CAPABILITY_UNCONFIRMED),
            None => {}
        }
    }
}

/// Answer a client's capability query: positive only when every reachable
/// worker acknowledged it exactly. Nothing but the query reaches a worker; the
/// main state, the journal, the fingerprint key and every fence are left alone.
pub(crate) fn begin_capability_query(server: &mut Server, client: &mut ClientSession) {
    let targets: Vec<WorkerId> = server
        .workers
        .values()
        .filter(|worker| server.is_worker_reachable(worker.id))
        .map(|worker| worker.id)
        .collect();
    if targets.is_empty() {
        client.finish_failure(CAPABILITY_UNCONFIRMED);
        return;
    }
    client.return_processing("Checking that every worker supports protected health checks...");
    let task = CapabilityQueryTask {
        client_token: client.token,
        targets: targets.iter().copied().collect(),
        expected: 0,
        answered: BTreeSet::new(),
        capable: BTreeSet::new(),
        refused: false,
    };
    server.scatter_to(
        RequestType::QueryProtectedHealthCheckSupport(QueryProtectedHealthCheckSupport {}).into(),
        Box::new(task),
        Timeout::Default,
        &targets,
    );
}

/// Start a protected change: validate, fingerprint, propose in the journal and
/// ask every worker whether it supports the protocol. Nothing reaches a worker
/// but that query until all of them answered it.
pub(crate) fn begin_protected_health_check(
    server: &mut Server,
    client: &mut ClientSession,
    order: SetProtectedHealthCheck,
) {
    let token = client.token;
    begin_change(server, &mut Some(client), Some(token), order, None);
}

/// [`begin_protected_health_check`] for a client that may be gone, optionally
/// as one step of a configuration reload: every way this change ends, including
/// a refusal before anything was sent, goes through [`conclude_ok`] or
/// [`conclude_err`] so the reload moves on or ends with a single answer.
fn begin_change(
    server: &mut Server,
    client: &mut OptionalClient,
    client_token: Option<Token>,
    mut order: SetProtectedHealthCheck,
    reload: Option<Box<ReloadProtected>>,
) {
    let cluster = order.cluster_id.to_owned();
    if !server.state.clusters.contains_key(cluster.as_str()) {
        conclude_err(server, client, reload, "the cluster does not exist");
        return;
    }
    if server.protected.is_active(&cluster) {
        conclude_err(server, client, reload, BUSY);
        return;
    }
    if server.worker_bootstrap_in_flight() {
        conclude_err(server, client, reload, BOOTSTRAPPING);
        return;
    }
    let metadata = RequestType::SetHealthCheck(SetHealthCheck {
        cluster_id: cluster.to_owned(),
        config: order.config.to_owned(),
    });
    if let Some(reason) = server.state.health_check_port_unix_conflict(&metadata) {
        conclude_err(server, client, reload, reason);
        return;
    }
    if server.protected.key.is_none() {
        match FingerprintKey::generate() {
            Ok(key) => server.protected.key = Some(key),
            Err(_) => {
                conclude_err(
                    server,
                    client,
                    reload,
                    "could not generate the fingerprint key",
                );
                return;
            }
        }
    }
    let Some(key) = server.protected.key.as_ref() else {
        conclude_err(
            server,
            client,
            reload,
            "could not generate the fingerprint key",
        );
        return;
    };
    let protected = match ProtectedHealthCheck::from_ingress(key, &mut order) {
        Ok(protected) => protected,
        Err(error) => {
            conclude_err(server, client, reload, error.to_string());
            return;
        }
    };
    drop(order);
    let workers: Vec<WorkerId> = server
        .workers
        .values()
        .filter(|worker| server.is_worker_reachable(worker.id))
        .map(|worker| worker.id)
        .collect();
    let config = protected.config().to_owned();
    let txn = match Transaction::begin(
        &mut server.protected.journal,
        &cluster,
        ProtectedHeaderChange::Replace(protected.headers().to_owned()),
        workers,
    ) {
        Ok(txn) => txn,
        Err(error) => {
            conclude_err(server, client, reload, error.to_string());
            return;
        }
    };
    drop(protected);
    server.protected.active.insert(cluster, txn.version());
    if let Some(client) = client.as_deref_mut() {
        client.return_processing("Checking that every worker supports protected health checks...");
    }
    let targets = txn.workers();
    let mut task = ProtectedHealthCheckTask::new(client_token, Stage::Capability, config, txn);
    task.reload = reload;
    server.scatter_to(
        RequestType::QueryProtectedHealthCheckSupport(QueryProtectedHealthCheckSupport {}).into(),
        Box::new(task),
        Timeout::Default,
        &targets,
    );
}

/// The headers a reloaded configuration takes from files: one order per
/// cluster that names `value_file` references, in lexical order of cluster id.
///
/// Everything that can be judged without opening a file is judged first; then
/// each file is read once, relative paths against the directory of the
/// configuration file, into carriers that wipe themselves. A diagnostic names
/// the cluster and the header, never a path or a byte of a value.
pub(crate) fn read_reload_orders(
    config: &Config,
) -> Result<VecDeque<SetProtectedHealthCheck>, String> {
    let mut clusters: Vec<_> = config
        .clusters
        .iter()
        .filter_map(|(id, cluster)| {
            let (health_check, files) = match cluster {
                ClusterConfig::Http(http) => (&http.health_check, &http.health_check_header_files),
                ClusterConfig::Tcp(tcp) => (&tcp.health_check, &tcp.health_check_header_files),
            };
            (!files.is_empty()).then_some((id, health_check, files))
        })
        .collect();
    clusters.sort_by(|left, right| left.0.cmp(right.0));
    let directory = Path::new(&config.config_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    for (id, health_check, files) in &clusters {
        let named = health_check.as_ref().is_some_and(|health_check| {
            health_check.headers.len() == files.len()
                && health_check
                    .headers
                    .iter()
                    .zip(files.iter())
                    .all(|(header, file)| header.name == file.name)
        });
        if !named {
            return Err(format!(
                "cluster {id}: the value files do not match the health check headers"
            ));
        }
        if let Some(health_check) = health_check {
            validate_health_check_config(health_check)
                .map_err(|reason| format!("cluster {id}: {reason}"))?;
        }
    }
    let mut orders = VecDeque::with_capacity(clusters.len());
    for (id, health_check, files) in clusters {
        let Some(health_check) = health_check else {
            continue;
        };
        let values = files
            .iter()
            .map(|file| read_value_file(directory, file))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|reason| format!("cluster {id}: {reason}"))?;
        validate_health_check_header_values(health_check, &values)
            .map_err(|reason| format!("cluster {id}: {reason}"))?;
        orders.push_back(SetProtectedHealthCheck {
            cluster_id: id.to_owned(),
            config: health_check.to_owned(),
            values,
        });
    }
    Ok(orders)
}

/// One value file: a regular file only, opened without blocking on a pipe.
fn read_value_file(
    directory: &Path,
    file: &FileHealthCheckHeader,
) -> Result<HealthCheckHeaderValue, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let opened = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(directory.join(&file.value_file))
        .map_err(|error| {
            format!(
                "could not open the value file of header {}: {}",
                file.name,
                error.kind()
            )
        })?;
    let regular = opened.metadata().is_ok_and(|metadata| metadata.is_file());
    if !regular {
        return Err(format!(
            "the value file of header {} is not a regular file",
            file.name
        ));
    }
    read_header_value(file.name.to_owned(), opened, HEALTH_CHECK_MAX_HEADER_BYTES)
        .map_err(|error| format!("header {}: {error}", file.name))
}

/// Where the clear values of a reload come from, and what the reload does once
/// they are applied. Never serialized, never cloned; nothing but counts and
/// cluster names is ever shown.
pub(crate) struct ReloadProtected {
    /// `None` for the startup of a cold main process, which has no client.
    client_token: Option<Token>,
    /// The clusters still to change, in lexical order, values included.
    orders: VecDeque<SetProtectedHealthCheck>,
    /// The cluster being changed now.
    current_cluster: Option<String>,
    /// The clusters whose new headers are confirmed, in the order they were.
    applied: Vec<String>,
    /// The tally of the generic part of the reload.
    summary: String,
    /// The configuration the reload loaded: the source of the next reload once
    /// every change is confirmed, and not before. `None` at startup, where the
    /// loaded configuration already is the source.
    new_config: Option<Config>,
}

impl fmt::Debug for ReloadProtected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReloadProtected")
            .field("pending", &self.orders.len())
            .field("applied", &self.applied)
            .finish_non_exhaustive()
    }
}

impl ReloadProtected {
    pub(crate) fn new(
        client_token: Token,
        orders: VecDeque<SetProtectedHealthCheck>,
        new_config: Config,
    ) -> Self {
        Self {
            client_token: Some(client_token),
            orders,
            current_cluster: None,
            applied: Vec::new(),
            summary: String::new(),
            new_config: Some(new_config),
        }
    }

    /// The changes of a cold start: no client, no systemd window of their own.
    fn cold(orders: VecDeque<SetProtectedHealthCheck>) -> Self {
        Self {
            client_token: None,
            orders,
            current_cluster: None,
            applied: Vec::new(),
            summary: String::new(),
            new_config: None,
        }
    }

    pub(crate) fn set_summary(&mut self, summary: String) {
        self.summary = summary;
    }

    /// The single answer of the reload, and the end of its systemd window.
    fn finish(self, server: &mut Server, client: &mut OptionalClient, result: Result<(), String>) {
        let Self {
            summary,
            applied,
            new_config,
            ..
        } = self;
        let Some(new_config) = new_config else {
            // A cold start answers no client and owes systemd nothing here:
            // the cold `READY=1` gate reads the outcome.
            match result {
                Ok(()) => {
                    info!(
                        "protected health check headers applied on {} cluster(s) at startup",
                        applied.len()
                    );
                    server.protected.cold = ColdStart::Settled;
                }
                Err(reason) => {
                    error!(
                        "protected health check headers could not be applied at startup, \
                         READY=1 stays closed ({}); clusters applied: [{}]",
                        reason,
                        applied.join(", ")
                    );
                    server.protected.cold = ColdStart::Failed;
                }
            }
            server.update_counts();
            return;
        };
        match result {
            Ok(()) => {
                // Every change is confirmed: the reloaded file is now the
                // source of the next reload.
                server.config = new_config;
                if let Some(client) = client.as_deref_mut() {
                    client.finish_ok(format!(
                        "Successfully loaded the config: {summary}; protected health check headers applied on {} cluster(s)",
                        applied.len()
                    ));
                }
            }
            Err(reason) => {
                if let Some(client) = client.as_deref_mut() {
                    client.finish_failure(format!(
                        "\nloading static configuration failed: the generic entries were applied \
                         ({summary}) but the protected health check headers were not ({reason}); \
                         clusters whose headers were applied: [{}]; the source configuration is \
                         unchanged",
                        applied.join(", ")
                    ));
                }
            }
        }
        server.update_counts();
        // A reload exists only for a client-driven `RELOADING=1`.
        crate::command::requests::notify_ready();
    }
}

/// Start the protected changes of a reload whose generic part succeeded: one
/// cluster at a time, in lexical order, each fully decided before the next.
pub(crate) fn begin_reload_protected(
    server: &mut Server,
    client: &mut OptionalClient,
    reload: Box<ReloadProtected>,
) {
    next_reload_step(server, client, reload);
}

/// Begin the file-backed headers of a cold start, once the generic startup
/// tasks are over (the caller proves no task is queued or active and every
/// worker is running): the orders the state file did not decide are applied by
/// the ordinary transaction, one cluster after the other in lexical order.
pub(crate) fn maybe_begin_cold_start(server: &mut Server) {
    if !matches!(server.protected.cold, ColdStart::Pending(_)) {
        return;
    }
    let ColdStart::Pending(orders) = std::mem::take(&mut server.protected.cold) else {
        return;
    };
    if orders.is_empty() {
        server.protected.cold = ColdStart::Settled;
        return;
    }
    server.protected.cold = ColdStart::Running(orders.keys().cloned().collect());
    let orders: VecDeque<_> = orders.into_values().collect();
    next_reload_step(server, &mut None, Box::new(ReloadProtected::cold(orders)));
}

fn next_reload_step(
    server: &mut Server,
    client: &mut OptionalClient,
    mut reload: Box<ReloadProtected>,
) {
    match reload.orders.pop_front() {
        Some(order) => {
            let token = reload.client_token;
            reload.current_cluster = Some(order.cluster_id.to_owned());
            begin_change(server, client, token, order, Some(reload));
        }
        None => reload.finish(server, client, Ok(())),
    }
}

/// A protected change ended well: the reload moves to its next cluster, or a
/// direct change answers its client.
fn conclude_ok(
    server: &mut Server,
    client: &mut OptionalClient,
    reload: Option<Box<ReloadProtected>>,
    message: String,
) {
    match reload {
        None => {
            if let Some(client) = client.as_deref_mut() {
                client.finish_ok(message);
            }
        }
        Some(mut reload) => {
            if let Some(cluster) = reload.current_cluster.take() {
                reload.applied.push(cluster);
            }
            next_reload_step(server, client, reload);
        }
    }
}

/// A protected change ended badly: a reload stops there and answers once; a
/// direct change answers its client.
fn conclude_err(
    server: &mut Server,
    client: &mut OptionalClient,
    reload: Option<Box<ReloadProtected>>,
    reason: impl Into<String>,
) {
    let reason = reason.into();
    match reload {
        None => {
            if let Some(client) = client.as_deref_mut() {
                client.finish_failure(reason);
            }
        }
        Some(reload) => reload.finish(server, client, Err(reason)),
    }
}

/// Where a bootstrap stands: the step whose acknowledgement is awaited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootstrapStage {
    /// The no-secret capability query.
    Capability,
    /// The cluster at this index of the plan: its prepare, commit, release.
    Prepare(usize),
    Commit(usize),
    Release(usize),
}

/// The confirmed protected values a fresh worker receives, one acknowledged
/// order at a time, before the readiness task `finalizer` asks it for its
/// `Status`: query, then `prepare`, `commit` and `release` of each confirmed
/// cluster in lexical order. The worker is `Running` only once `finalizer`
/// has its exact answer.
///
/// It keeps the worker, cluster names, the version, the stage and the
/// response bookkeeping: no carrier and no value. The one order that carries
/// values is built from the journal when it is sent and dropped with it. Each
/// step is its own gathering task, bounded by the worker deadline.
///
/// Any other end (refusal, wrong answer, silence, closed channel, send
/// failure, a plan main cannot honor) hands the failure to `finalizer`, which
/// fences the child through the worker lifecycle exactly as it does when the
/// `Status` itself fails: an `upgrade --worker` keeps its old worker serving.
struct WorkerBootstrapTask {
    worker_id: WorkerId,
    clusters: Vec<String>,
    version: ProtectedHeaderVersion,
    stage: BootstrapStage,
    expected: usize,
    /// `Some(true)` once the worker acknowledged the current step exactly.
    verdict: Option<bool>,
    finalizer: Box<dyn GatheringTask>,
}

impl fmt::Debug for WorkerBootstrapTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerBootstrapTask")
            .field("worker", &self.worker_id)
            .field("clusters", &self.clusters.len())
            .field("version", &self.version)
            .field("stage", &self.stage)
            .field("finalizer", &self.finalizer.kind())
            .finish()
    }
}

impl WorkerBootstrapTask {
    /// The phase, cluster and version the worker must acknowledge now.
    fn awaited(&self) -> (ProtectedHealthCheckPhase, &str, u64) {
        let cluster = |index: usize| self.clusters[index].as_str();
        match self.stage {
            BootstrapStage::Capability => (Stage::Capability.phase(), "", 0),
            BootstrapStage::Prepare(i) => (Stage::Prepare.phase(), cluster(i), self.version.get()),
            BootstrapStage::Commit(i) => (Stage::Commit.phase(), cluster(i), self.version.get()),
            BootstrapStage::Release(i) => (
                ProtectedHealthCheckPhase::ProtectedHealthCheckRelease,
                cluster(i),
                self.version.get(),
            ),
        }
    }

    /// The order of the current stage. Only a prepare carries values, copied
    /// out of the journal into carriers that wipe themselves on drop; it is
    /// refused unless the journal still holds exactly the confirmed set the
    /// main state names.
    fn request(&self, server: &Server) -> Option<Request> {
        let (phase, cluster, version) = self.awaited();
        let request = match self.stage {
            BootstrapStage::Capability => {
                RequestType::QueryProtectedHealthCheckSupport(QueryProtectedHealthCheckSupport {})
            }
            stage => {
                let prepare = match stage {
                    BootstrapStage::Prepare(_) => {
                        if !server.protected.active.is_empty() {
                            return None;
                        }
                        let headers = server.protected.journal.effective(cluster)?.to_owned();
                        let config = server.state.clusters.get(cluster)?.health_check.as_ref()?;
                        let protected = ProtectedHealthCheck::from_parts(
                            cluster.to_owned(),
                            config.to_owned(),
                            headers,
                        )
                        .ok()?;
                        Some(protected.to_order())
                    }
                    _ => None,
                };
                RequestType::ProtectedHealthCheckOrder(ProtectedHealthCheckOrder {
                    phase: phase as i32,
                    cluster_id: cluster.to_owned(),
                    version,
                    prepare,
                })
            }
        };
        Some(request.into())
    }

    /// The stage after the current one, `None` once every release is
    /// acknowledged.
    fn next_stage(&self) -> Option<BootstrapStage> {
        match self.stage {
            BootstrapStage::Capability => {
                (!self.clusters.is_empty()).then_some(BootstrapStage::Prepare(0))
            }
            BootstrapStage::Prepare(i) => Some(BootstrapStage::Commit(i)),
            BootstrapStage::Commit(i) => Some(BootstrapStage::Release(i)),
            BootstrapStage::Release(i) => {
                (i + 1 < self.clusters.len()).then_some(BootstrapStage::Prepare(i + 1))
            }
        }
    }

    /// Send the order of the current stage as a task of its own.
    fn send(mut self, server: &mut Server, client: &mut OptionalClient) {
        let Some(request) = self.request(server) else {
            error!(
                "worker {}: the protected health check bootstrap order could not be built",
                self.worker_id
            );
            return self.fail(server, client, false);
        };
        let worker = self.worker_id;
        self.expected = 0;
        self.verdict = None;
        server.scatter_to(request, Box::new(self), Timeout::Default, &[worker]);
    }

    /// The worker is not given its values: its readiness task fails and fences
    /// it, and answers whoever waits for it.
    fn fail(self, server: &mut Server, client: &mut OptionalClient, timed_out: bool) {
        error!(
            "worker {} did not take the protected health check headers ({}): fencing it",
            self.worker_id,
            if timed_out { "timeout" } else { "refused" }
        );
        self.finalizer.on_finish(server, client, timed_out);
    }
}

impl Gatherer for WorkerBootstrapTask {
    fn inc_expected_responses(&mut self, count: usize) {
        self.expected += count;
    }

    /// A step nobody was asked for (no reachable worker, or a plan main cannot
    /// honor) has nothing to wait for: it ends at once, as a failure.
    fn has_finished(&self) -> bool {
        self.verdict.is_some() || self.expected == 0
    }

    fn on_message(
        &mut self,
        _server: &mut Server,
        _client: &mut OptionalClient,
        worker_id: WorkerId,
        message: WorkerResponse,
    ) {
        // A progress report is no answer, and neither is another worker's.
        if worker_id != self.worker_id
            || self.verdict.is_some()
            || matches!(
                ResponseStatus::try_from(message.status),
                Ok(ResponseStatus::Processing)
            )
        {
            return;
        }
        let (phase, cluster, version) = self.awaited();
        self.verdict = Some(is_exact_reply(&message, phase, cluster, version));
    }
}

impl GatheringTask for WorkerBootstrapTask {
    fn kind(&self) -> &'static str {
        "protected_worker_bootstrap"
    }

    fn client_token(&self) -> Option<Token> {
        self.finalizer.client_token()
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        self
    }

    /// There is no wire form for this task: a main upgrade waits for it, as it
    /// does for the readiness task it precedes.
    fn snapshot(&self, _timing: TaskSnapshotTiming) -> Result<TaskSnapshot, TaskSnapshotError> {
        Err(TaskSnapshotError::WorkerReadinessInFlight)
    }

    fn retained_worker_token(&self) -> Option<Token> {
        self.finalizer.retained_worker_token()
    }

    fn on_finish(
        mut self: Box<Self>,
        server: &mut Server,
        client: &mut OptionalClient,
        timed_out: bool,
    ) {
        if timed_out || self.verdict != Some(true) {
            return self.fail(server, client, timed_out);
        }
        match self.next_stage() {
            Some(stage) => {
                self.stage = stage;
                self.send(server, client);
            }
            None => {
                let Self {
                    worker_id,
                    finalizer,
                    ..
                } = *self;
                server.queue_initial_status(worker_id, finalizer);
            }
        }
    }
}

/// Give the registered `NotAnswering` worker `worker_id` the confirmed values
/// of `plan` before its readiness task `finalizer` asks it for its `Status`.
pub(crate) fn begin_worker_bootstrap(
    server: &mut Server,
    worker_id: WorkerId,
    plan: ConfirmedBootstrap,
    finalizer: Box<dyn GatheringTask>,
) {
    server.bootstrapping_workers.insert(worker_id);
    WorkerBootstrapTask {
        worker_id,
        clusters: plan.clusters().to_owned(),
        version: plan.version(),
        stage: BootstrapStage::Capability,
        expected: 0,
        verdict: None,
        finalizer,
    }
    .send(server, &mut None);
}

/// Main cannot honor the confirmed values for a new worker (a change in
/// flight, a fence, state that does not name them): nothing is sent to it and
/// its readiness task fails it, answering whoever waits for it.
pub(crate) fn refuse_worker_bootstrap(
    server: &mut Server,
    worker_id: WorkerId,
    finalizer: Box<dyn GatheringTask>,
) {
    // Nothing is asked of the worker, so the task ends at once, as a failure.
    server.new_task(
        Box::new(WorkerBootstrapTask {
            worker_id,
            clusters: Vec::new(),
            version: ProtectedHeaderVersion::from_wire(0),
            stage: BootstrapStage::Capability,
            expected: 0,
            verdict: None,
            finalizer,
        }),
        Timeout::Default,
    );
}

#[cfg(test)]
mod tests {
    use std::{
        os::{fd::AsRawFd, unix::process::ExitStatusExt},
        time::{Duration, Instant},
    };

    use mio::{Token, net::UnixListener, net::UnixStream};
    use prost::Message as _;
    use sozu_command_lib::{
        channel::{Channel, delimiter_size},
        config::Config,
        proto::command::{
            AddBackend, Cluster, HealthCheckHeader, HealthCheckHeaderValue,
            ProtectedHealthCheckReply, Response, ResponseContent, RunState, WorkerRequest,
        },
        ready::Ready,
        scm_socket::ScmSocket,
        state::ConfigState,
    };
    use sozu_lib::{backends::BackendMap, protected_health_check::ProtectedWorker};

    use super::*;
    use crate::command::{
        server::CommandHub,
        sessions::{WorkerResult, extract_messages},
    };

    const SECRET: &str = "SYNTHETIC-TICK-SECRET-5d2e";
    const CLUSTER: &str = "app";

    /// A worker as the main process sees it: a real channel, answering with the
    /// real worker-side code (`ProtectedWorker` over a real `ConfigState` and
    /// `BackendMap`). `behaviour` makes it refuse, go silent or answer wrongly.
    struct FakeWorker {
        id: WorkerId,
        side: Channel<WorkerResponse, WorkerRequest>,
        _scm: std::os::unix::net::UnixStream,
        state: ConfigState,
        backends: BackendMap,
        protected: ProtectedWorker,
        /// Answers `Failure` to this phase.
        refuses: Option<ProtectedHealthCheckPhase>,
        /// Holds the answer to this phase back until `release_held`.
        holds: Option<ProtectedHealthCheckPhase>,
        /// Answers this phase with the version of another change.
        answers_wrong_version: Option<ProtectedHealthCheckPhase>,
        /// Answers this phase with a success that is not the exact reply.
        answers_shape: Option<(ProtectedHealthCheckPhase, WrongShape)>,
        /// Decodes none of the protected verbs and never answers them.
        predates_protocol: bool,
        held: Vec<WorkerResponse>,
        /// Every request that reached this worker, protected or not.
        requests_seen: usize,
        /// Prepare orders that reached this worker, and the phases it saw.
        prepares_seen: usize,
        phases_seen: Vec<ProtectedHealthCheckPhase>,
        /// Every request in arrival order, by kind: the order a worker
        /// initialization is judged by.
        sequence: Vec<&'static str>,
        /// The phase, cluster and version of every protected order.
        orders_seen: Vec<(ProtectedHealthCheckPhase, String, u64)>,
        /// The health check every `AddCluster` that reached this worker carried.
        add_cluster_health: Vec<Option<HealthCheckConfig>>,
        /// Applies every `AddCluster` it receives to its own state, as a
        /// started worker does: a cold start begins with no cluster anywhere.
        applies_add_cluster: bool,
        /// Answers `Failure` to every ordinary (non-protected) order.
        refuses_ordinary: bool,
    }

    /// How a success reply departs from the exact acknowledgement.
    #[derive(Clone, Copy)]
    enum WrongShape {
        NoContent,
        OtherPhase,
        OtherCluster,
    }

    impl FakeWorker {
        fn serve(&mut self) {
            self.side.handle_events(Ready::READABLE);
            for mut request in extract_messages(&mut self.side) {
                self.requests_seen += 1;
                let Some(request_type) = request.content.request_type.as_mut() else {
                    continue;
                };
                if let RequestType::AddCluster(cluster) = &*request_type {
                    self.add_cluster_health
                        .push(cluster.health_check.to_owned());
                    if self.applies_add_cluster {
                        let applied: sozu_command_lib::proto::command::Request =
                            RequestType::AddCluster(cluster.to_owned()).into();
                        let _ = self.state.dispatch(&applied);
                        // As the real worker's `Server::add_cluster`: the
                        // values kept survive only for the exact headers.
                        self.backends.set_health_check_config(
                            &cluster.cluster_id,
                            cluster.health_check.to_owned(),
                        );
                    }
                }
                self.sequence.push(match request_type {
                    RequestType::Status(_) => "Status",
                    RequestType::AddCluster(_) => "AddCluster",
                    RequestType::QueryProtectedHealthCheckSupport(_) => "Query",
                    RequestType::ProtectedHealthCheckOrder(order) => {
                        match ProtectedHealthCheckPhase::try_from(order.phase) {
                            Ok(ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare) => "Prepare",
                            Ok(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit) => "Commit",
                            Ok(ProtectedHealthCheckPhase::ProtectedHealthCheckRelease) => "Release",
                            Ok(ProtectedHealthCheckPhase::ProtectedHealthCheckAbort) => "Abort",
                            _ => "Other",
                        }
                    }
                    _ => "Other",
                });
                let phase = match request_type {
                    RequestType::QueryProtectedHealthCheckSupport(_) => {
                        Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
                    }
                    RequestType::ProtectedHealthCheckOrder(order) => {
                        let phase = ProtectedHealthCheckPhase::try_from(order.phase).ok();
                        if let Some(phase) = phase {
                            self.orders_seen.push((
                                phase,
                                order.cluster_id.to_owned(),
                                order.version,
                            ));
                        }
                        phase
                    }
                    _ => None,
                };
                if let Some(phase) = phase {
                    if self.predates_protocol {
                        continue;
                    }
                    self.phases_seen.push(phase);
                    if phase == ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare {
                        self.prepares_seen += 1;
                    }
                }
                let mut response = if phase.is_some() && self.refuses == phase
                    || phase.is_none() && self.refuses_ordinary
                {
                    WorkerResponse::error(request.id.clone(), "synthetic refusal")
                } else {
                    self.protected
                        .handle(
                            &request.id,
                            request_type,
                            &mut self.state,
                            &mut self.backends,
                        )
                        .unwrap_or_else(|| WorkerResponse::ok(request.id.clone()))
                };
                if phase.is_some()
                    && self.answers_wrong_version == phase
                    && let Some(ContentType::ProtectedHealthCheckReply(reply)) = response
                        .content
                        .as_mut()
                        .and_then(|content| content.content_type.as_mut())
                {
                    reply.version += 1000;
                }
                if let Some((shaped, shape)) = self.answers_shape
                    && phase == Some(shaped)
                {
                    match shape {
                        WrongShape::NoContent => response.content = None,
                        WrongShape::OtherPhase | WrongShape::OtherCluster => {
                            if let Some(ContentType::ProtectedHealthCheckReply(reply)) = response
                                .content
                                .as_mut()
                                .and_then(|content| content.content_type.as_mut())
                            {
                                match shape {
                                    WrongShape::OtherPhase => {
                                        reply.phase =
                                            ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare
                                                as i32
                                    }
                                    _ => reply.cluster_id = CLUSTER.to_owned(),
                                }
                            }
                        }
                    }
                }
                if phase.is_some() && self.holds == phase {
                    self.held.push(response);
                } else {
                    self.side
                        .write_message(&response)
                        .expect("queue the answer");
                }
            }
            self.flush();
        }

        fn release_held(&mut self) {
            for response in std::mem::take(&mut self.held) {
                self.side
                    .write_message(&response)
                    .expect("queue the answer");
            }
            self.holds = None;
            self.flush();
        }

        fn flush(&mut self) {
            self.side.interest.insert(Ready::WRITABLE);
            self.side.handle_events(Ready::WRITABLE);
            let _ = self.side.writable();
        }

        fn held_values(&self) -> Option<Vec<Vec<u8>>> {
            self.backends
                .protected_health_check_headers
                .get(CLUSTER)
                .map(|set| {
                    set.headers()
                        .iter()
                        .map(|header| header.value().expose_secret_bytes().to_vec())
                        .collect()
                })
        }
    }

    struct Rig {
        hub: CommandHub,
        workers: Vec<FakeWorker>,
        /// The process each fake worker is registered with, by worker id: a
        /// real child this fixture owns, so that the `SIGKILL` of
        /// `Server::close_worker` reaches it and nothing else. A pid of 0 would
        /// signal the whole process group of the test run.
        children: Children,
        client: Token,
        _dir: tempfile::TempDir,
        _peer: std::os::unix::net::UnixStream,
    }

    /// The stand-in processes: killed and reaped on drop, also when a test
    /// panicked or the rig failed half-way through its construction.
    /// `Child::kill` and `Child::wait` are no-ops on a child already reaped,
    /// so a recycled pid is never signalled.
    struct Children(Vec<std::process::Child>);

    impl Drop for Children {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// A stand-in worker process: it only has to exist, and be killable.
    fn spawn_stand_in() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("120")
            .spawn()
            .expect("could not spawn the stand-in worker process")
    }

    fn add_cluster() -> sozu_command_lib::proto::command::Request {
        RequestType::AddCluster(Cluster {
            cluster_id: CLUSTER.to_owned(),
            ..Default::default()
        })
        .into()
    }

    /// Register a `NotAnswering` fake worker `id` on `hub`, backed by a real
    /// child the fixture owns from the moment it is spawned.
    fn register_fake(
        hub: &mut CommandHub,
        children: &mut Children,
        id: WorkerId,
        state: ConfigState,
    ) -> FakeWorker {
        let child = spawn_stand_in();
        let pid = child.id() as libc::pid_t;
        children.0.push(child);
        let (main_sock, worker_sock) = UnixStream::pair().expect("a worker pair");
        let main_side: Channel<WorkerRequest, WorkerResponse> =
            Channel::new(main_sock, 4096, 65536);
        let side: Channel<WorkerResponse, WorkerRequest> = Channel::new(worker_sock, 4096, 65536);
        let (scm_main, scm_peer) = std::os::unix::net::UnixStream::pair().expect("scm pair");
        let scm = ScmSocket::new(scm_main.as_raw_fd()).expect("scm socket");
        hub.server
            .register_worker(id, pid, main_side, scm)
            .expect("a worker");
        // The descriptor is borrowed by `ScmSocket`: keep it open.
        std::mem::forget(scm_main);
        FakeWorker {
            id,
            side,
            _scm: scm_peer,
            state,
            backends: BackendMap::new(),
            protected: ProtectedWorker::default(),
            refuses: None,
            holds: None,
            answers_wrong_version: None,
            answers_shape: None,
            predates_protocol: false,
            held: Vec::new(),
            requests_seen: 0,
            prepares_seen: 0,
            phases_seen: Vec::new(),
            sequence: Vec::new(),
            orders_seen: Vec::new(),
            add_cluster_health: Vec::new(),
            applies_add_cluster: false,
            refuses_ordinary: false,
        }
    }

    fn rig(worker_count: u32) -> Rig {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let listener = UnixListener::bind(dir.path().join("rig.sock")).expect("bind");
        // `Config::default()` is all zeros: no channel capacity, and a worker
        // deadline that has already passed.
        let config = Config {
            command_buffer_size: 4096,
            max_command_buffer_size: 65536,
            worker_timeout: 10,
            ..Config::default()
        };
        let mut hub = CommandHub::new(listener, config, "sozu".to_owned()).expect("a hub");
        let (peer, accepted) = std::os::unix::net::UnixStream::pair().expect("a client pair");
        accepted.set_nonblocking(true).expect("nonblocking");
        hub.register_client(UnixStream::from_std(accepted));
        let client = hub.only_client_token();
        hub.server
            .state
            .dispatch(&add_cluster())
            .expect("a cluster");
        let mut workers = Vec::new();
        let mut children = Children(Vec::new());
        for id in 0..worker_count {
            let mut state = ConfigState::new();
            state.dispatch(&add_cluster()).expect("a worker cluster");
            workers.push(register_fake(&mut hub, &mut children, id, state));
        }
        Rig {
            hub,
            workers,
            children,
            client,
            _dir: dir,
            _peer: peer,
        }
    }

    fn order(values: &[(&str, &str)]) -> SetProtectedHealthCheck {
        SetProtectedHealthCheck {
            cluster_id: CLUSTER.to_owned(),
            config: HealthCheckConfig {
                uri: "/".to_owned(),
                interval: 10,
                timeout: 5,
                healthy_threshold: 3,
                unhealthy_threshold: 3,
                headers: values
                    .iter()
                    .map(|(name, _)| HealthCheckHeader {
                        name: (*name).to_owned(),
                        fingerprint: String::new(),
                    })
                    .collect(),
                ..Default::default()
            },
            values: values
                .iter()
                .map(|(name, value)| HealthCheckHeaderValue {
                    name: (*name).to_owned(),
                    value: value.as_bytes().to_vec(),
                })
                .collect(),
        }
    }

    impl Rig {
        fn begin(&mut self, order: SetProtectedHealthCheck) {
            let (server, client) = self.hub.get_client_mut(&self.client).expect("the client");
            begin_protected_health_check(server, client, order);
        }

        /// Run the event loop's exchange until nothing moves any more, at
        /// `now`: flush the orders, let every worker answer, route the answers.
        fn pump_at(&mut self, now: Instant) {
            for _ in 0..24 {
                self.hub.finish_ready_tasks(now);
                let mut moved = false;
                for worker in self.hub.server.workers.values_mut() {
                    worker.update_readiness(Ready::WRITABLE);
                    let _ = worker.ready();
                }
                for fake in &mut self.workers {
                    fake.serve();
                }
                let mut answers = Vec::new();
                for worker in self.hub.server.workers.values_mut() {
                    worker.update_readiness(Ready::READABLE);
                    if let WorkerResult::NewResponses(responses) = worker.ready() {
                        answers.extend(responses.into_iter().map(|r| (worker.id, r)));
                    }
                }
                for (worker_id, response) in answers {
                    moved = true;
                    self.hub.handle_worker_response(worker_id, response);
                }
                if !moved {
                    break;
                }
            }
            self.hub.finish_ready_tasks(now);
        }

        fn pump(&mut self) {
            self.pump_at(Instant::now());
        }

        /// Let every deadline expire, as a silent worker would.
        fn expire(&mut self) {
            self.pump_at(Instant::now() + Duration::from_secs(100_000));
            self.pump();
        }

        fn responses(&mut self) -> Vec<Response> {
            let (_, client) = self.hub.get_client_mut(&self.client).expect("the client");
            let data = client.channel.back_buf.data();
            let delimiter = delimiter_size();
            let mut responses = vec![];
            let mut offset = 0usize;
            while offset + delimiter <= data.len() {
                let mut length = [0u8; std::mem::size_of::<usize>()];
                length.copy_from_slice(&data[offset..offset + delimiter]);
                let frame_len = usize::from_le_bytes(length);
                responses.push(
                    Response::decode(&data[offset + delimiter..offset + frame_len])
                        .expect("a decodable response"),
                );
                offset += frame_len;
            }
            responses
        }

        fn final_status(&mut self) -> Option<ResponseStatus> {
            self.responses()
                .iter()
                .rev()
                .map(|response| ResponseStatus::try_from(response.status).expect("a status"))
                .find(|status| *status != ResponseStatus::Processing)
        }

        fn main_headers(&self) -> Vec<HealthCheckHeader> {
            self.hub.server.state.clusters[CLUSTER]
                .health_check
                .as_ref()
                .map(|config| config.headers.to_owned())
                .unwrap_or_default()
        }

        /// Everything an observer of the main process can read, as one string.
        fn observable(&mut self) -> String {
            let responses = format!("{:?}", self.responses());
            let tasks = format!("{:?}", self.hub);
            let runtime = format!("{:?}", self.hub.server.protected);
            let state = state_view(&self.hub.server.state);
            let workers = self
                .workers
                .iter()
                .map(|worker| format!("{:?}{}", worker.protected, state_view(&worker.state)))
                .collect::<String>();
            format!("{responses}{tasks}{runtime}{state}{workers}")
        }
    }

    /// Everything a `ConfigState` exposes publicly about its clusters and the
    /// routes behind them: the cluster configurations (health-check names and
    /// fingerprints included), the backends and the per-cluster state hashes.
    fn state_view(state: &ConfigState) -> String {
        format!(
            "{:?}{:?}{:?}",
            state.clusters,
            state.backends,
            state.hash_state()
        )
    }

    fn assert_nothing_applied(rig: &mut Rig) {
        assert!(rig.main_headers().is_empty(), "main state is untouched");
        assert!(
            rig.hub.server.protected.held_values(CLUSTER).is_none(),
            "the journal holds no value"
        );
        assert!(
            !rig.hub.server.protected.is_active(CLUSTER),
            "the fence is lifted"
        );
        for worker in &rig.workers {
            assert!(
                worker.held_values().is_none(),
                "worker {} holds a value",
                worker.id
            );
            assert!(
                worker.state.clusters[CLUSTER].health_check.is_none(),
                "worker {} state changed",
                worker.id
            );
        }
        let observable = rig.observable();
        assert!(!observable.contains(SECRET), "a secret is observable");
    }

    /// The parent pid of `pid`, from `/proc`.
    fn parent_of(pid: i32) -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `comm` may contain spaces and parentheses: the fields that follow
        // start after its last closing parenthesis.
        let rest = stat.rsplit_once(')')?.1;
        rest.split_whitespace().nth(1)?.parse().ok()
    }

    /// The fixture must never register a worker whose pid is not a child it
    /// owns: `close_worker` signals that pid, and pid 0 is the caller's whole
    /// process group. This test only builds the rig and reads it: it expires,
    /// fences and signals nothing.
    #[test]
    fn every_registered_fake_worker_is_a_positive_pid_child_owned_by_the_fixture() {
        let rig = rig(2);

        let mut pids = Vec::new();
        for worker in rig.hub.server.workers.values() {
            assert!(
                worker.pid > 0,
                "worker {} is registered with pid {}: a kill would reach a process group",
                worker.id,
                worker.pid
            );
            assert_eq!(
                parent_of(worker.pid),
                Some(std::process::id()),
                "worker {} (pid {}) is not a child of the fixture",
                worker.id,
                worker.pid
            );
            pids.push(worker.pid);
        }
        assert_eq!(pids.len(), 2, "every fake worker is registered");
        pids.sort_unstable();
        pids.dedup();
        assert_eq!(pids.len(), 2, "each fake worker owns its own child process");
    }

    #[test]
    fn two_workers_commit_and_the_client_succeeds_only_after_the_second_commit() {
        let mut rig = rig(2);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);

        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();

        // Worker 0 applied and acknowledged; worker 1 applied but its
        // acknowledgement is held back.
        assert!(rig.workers[0].held_values().is_some());
        assert_eq!(
            rig.final_status(),
            None,
            "no final answer before the last ack"
        );
        assert!(rig.main_headers().is_empty(), "main waits for the last ack");
        assert!(rig.hub.server.protected.is_active(CLUSTER));

        rig.workers[1].release_held();
        rig.pump();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        let expected = vec![SECRET.as_bytes().to_vec()];
        for worker in &rig.workers {
            assert_eq!(worker.held_values(), Some(expected.clone()));
            assert_eq!(
                worker.state.clusters[CLUSTER]
                    .health_check
                    .as_ref()
                    .unwrap()
                    .headers,
                rig.main_headers()
            );
        }
        assert_eq!(
            rig.hub.server.protected.held_values(CLUSTER),
            Some(expected)
        );
        assert_eq!(rig.main_headers().len(), 1);
        assert!(!rig.hub.server.protected.is_active(CLUSTER));
        // The release reached both workers, and no secret is observable.
        for worker in &rig.workers {
            assert_eq!(
                worker.phases_seen,
                vec![
                    ProtectedHealthCheckPhase::ProtectedHealthCheckCapability,
                    ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare,
                    ProtectedHealthCheckPhase::ProtectedHealthCheckCommit,
                    ProtectedHealthCheckPhase::ProtectedHealthCheckRelease,
                ]
            );
        }
        assert!(!rig.observable().contains(SECRET));
    }

    #[test]
    fn a_prepare_refusal_rolls_back_every_worker_and_a_new_order_follows() {
        let mut rig = rig(2);
        rig.workers[1].refuses = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare);

        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        rig.pump();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Failure));
        assert_nothing_applied(&mut rig);
        assert!(
            rig.workers[0]
                .phases_seen
                .contains(&ProtectedHealthCheckPhase::ProtectedHealthCheckAbort),
            "the staged proposal is dropped on the worker that accepted it"
        );

        // A different valid order goes through immediately afterwards.
        rig.workers[1].refuses = None;
        rig.begin(order(&[("X-Token", "SYNTHETIC-OTHER-3f")]));
        rig.pump();
        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        assert_eq!(
            rig.hub.server.protected.held_values(CLUSTER),
            Some(vec![b"SYNTHETIC-OTHER-3f".to_vec()])
        );
    }

    #[test]
    fn a_commit_failure_after_a_partial_apply_restores_the_exact_predecessor() {
        let mut rig = rig(2);
        // An ordinary health check the change replaces and an abort must give back.
        let ordinary = HealthCheckConfig {
            uri: "/before".to_owned(),
            interval: 7,
            timeout: 3,
            healthy_threshold: 2,
            unhealthy_threshold: 2,
            ..Default::default()
        };
        for worker in &mut rig.workers {
            worker.state.clusters.get_mut(CLUSTER).unwrap().health_check =
                Some(ordinary.to_owned());
            worker
                .backends
                .set_health_check_config(CLUSTER, Some(ordinary.to_owned()));
        }
        rig.workers[1].refuses = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);

        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        rig.pump();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Failure));
        assert!(rig.main_headers().is_empty());
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_none());
        assert!(!rig.hub.server.protected.is_active(CLUSTER));
        for worker in &rig.workers {
            assert!(worker.held_values().is_none(), "worker {}", worker.id);
            assert_eq!(
                worker.backends.health_check_configs.get(CLUSTER),
                Some(&ordinary),
                "worker {} gets its predecessor back",
                worker.id
            );
            assert_eq!(
                worker.state.clusters[CLUSTER].health_check.as_ref(),
                Some(&ordinary)
            );
        }
        assert!(!rig.observable().contains(SECRET));

        rig.workers[1].refuses = None;
        rig.begin(order(&[("X-Token", "SYNTHETIC-NEXT-81")]));
        rig.pump();
        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
    }

    #[test]
    fn a_stale_or_foreign_acknowledgement_can_never_commit() {
        for phase in [
            ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare,
            ProtectedHealthCheckPhase::ProtectedHealthCheckCommit,
        ] {
            let mut rig = rig(2);
            rig.workers[1].answers_wrong_version = Some(phase);

            rig.begin(order(&[("Authorization", SECRET)]));
            rig.pump();
            rig.expire();
            rig.pump();

            assert_ne!(
                rig.final_status(),
                Some(ResponseStatus::Ok),
                "{phase:?}: an acknowledgement of another version committed"
            );
            assert_eq!(
                rig.final_status(),
                Some(ResponseStatus::Failure),
                "{phase:?}"
            );
            assert_nothing_applied(&mut rig);
        }
    }

    #[test]
    fn a_worker_that_predates_the_protocol_fails_before_any_value_is_sent() {
        let mut rig = rig(2);
        rig.workers[1].predates_protocol = true;

        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        assert!(
            rig.final_status() != Some(ResponseStatus::Ok),
            "a silent worker is not a positive acknowledgement"
        );
        rig.expire();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Failure));
        assert_nothing_applied(&mut rig);
        for worker in &rig.workers {
            assert_eq!(
                worker.prepares_seen, 0,
                "worker {} was sent a value",
                worker.id
            );
        }
    }

    #[test]
    fn a_closed_worker_fails_the_change_and_is_never_healthy_again() {
        let mut rig = rig(2);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();

        // Worker 1 closes before acknowledging its commit.
        let token = *rig
            .hub
            .server
            .workers
            .iter()
            .find(|(_, worker)| worker.id == 1)
            .map(|(token, _)| token)
            .expect("worker 1");
        rig.hub.on_worker_channel_closed(&token, 1);
        rig.pump();
        rig.expire();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Failure));
        assert!(rig.main_headers().is_empty());
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_none());
        assert!(
            rig.workers[0].held_values().is_none(),
            "worker 0 rolled back"
        );
        assert!(
            !rig.hub.server.is_worker_reachable(1),
            "the closed worker is fenced"
        );
    }

    #[test]
    fn conflicting_direct_orders_are_refused_while_a_change_is_in_flight() {
        let mut rig = rig(1);
        rig.workers[0].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        assert!(rig.hub.server.protected.is_active(CLUSTER));

        let blocked = [
            RequestType::AddCluster(Cluster {
                cluster_id: CLUSTER.to_owned(),
                ..Default::default()
            }),
            RequestType::SetHealthCheck(SetHealthCheck {
                cluster_id: CLUSTER.to_owned(),
                config: HealthCheckConfig::default(),
            }),
            RequestType::RemoveHealthCheck(CLUSTER.to_owned()),
            RequestType::RemoveCluster(CLUSTER.to_owned()),
            RequestType::LoadState("state".to_owned()),
        ];
        let before = rig.hub.server.state.hash_state();
        for request in blocked {
            let refused = {
                let (server, client) = rig.hub.get_client_mut(&rig.client).expect("the client");
                refuse_while_busy(server, client, &request)
            };
            assert!(refused, "{request:?} must be refused");
        }
        assert_eq!(rig.hub.server.state.hash_state(), before);
        // Another cluster is not behind the fence.
        assert!(
            !rig.hub
                .server
                .protected
                .blocks(&RequestType::RemoveCluster("other".to_owned()))
        );

        rig.workers[0].release_held();
        rig.pump();
        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        assert!(!rig.hub.server.protected.is_active(CLUSTER));
    }

    /// The message of the last answer the client received that is not a
    /// progress report.
    fn final_message(rig: &mut Rig) -> Option<String> {
        rig.responses()
            .into_iter()
            .rev()
            .find(|response| response.status != ResponseStatus::Processing as i32)
            .map(|response| response.message)
    }

    /// A rollback one worker never confirms ends in the real fence: the
    /// production `fence_worker` -> `close_worker` path `SIGKILL`s the process
    /// that worker was registered with, and that process only.
    #[test]
    fn an_unconfirmed_rollback_kills_and_reaps_exactly_the_owned_child_of_that_worker() {
        let mut rig = rig(2);
        // An unrelated process of the same group: a kill aimed at the group
        // or at the wrong pid would take it down.
        let mut bystander = spawn_stand_in();
        // Worker 0 refuses the commit; worker 1 applied it and never answers
        // the abort that follows.
        rig.workers[0].refuses = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckAbort);
        let fenced_pid = rig
            .hub
            .server
            .workers
            .values()
            .find(|worker| worker.id == 1)
            .map(|worker| worker.pid)
            .expect("worker 1");
        assert_eq!(fenced_pid as u32, rig.children.0[1].id());

        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        assert_eq!(rig.final_status(), None, "the abort of worker 1 is awaited");
        assert!(
            rig.children.0[1].try_wait().expect("try_wait").is_none(),
            "nothing is killed while the abort is awaited"
        );
        rig.expire();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Failure));
        assert_eq!(final_message(&mut rig).as_deref(), Some(INCOHERENT));
        let status = rig.children.0[1].wait().expect("reap the fenced child");
        assert_eq!(
            status.signal(),
            Some(nix::sys::signal::Signal::SIGKILL as i32),
            "the fenced child died of the SIGKILL of close_worker"
        );
        assert!(
            rig.children.0[0].try_wait().expect("try_wait").is_none(),
            "the other worker's child is untouched"
        );
        assert!(
            bystander.try_wait().expect("try_wait").is_none(),
            "an unrelated process of the same group is untouched"
        );
        for worker in rig.hub.server.workers.values() {
            assert_eq!(
                worker.run_state == RunState::Stopped,
                worker.id == 1,
                "only worker 1 is stopped"
            );
        }
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_none());
        assert!(rig.main_headers().is_empty());
        assert!(!rig.hub.server.protected.is_active(CLUSTER));
        // Fencing it again proves it from the registry without a second signal.
        assert!(rig.hub.server.fence_worker(1));
        let _ = bystander.kill();
        let _ = bystander.wait();
    }

    /// The client is told the worker was stopped only when that is proven: a
    /// worker the main process no longer knows cannot be.
    #[test]
    fn a_worker_that_cannot_be_fenced_is_never_reported_as_stopped() {
        let mut rig = rig(2);
        rig.workers[0].refuses = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckAbort);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        rig.hub.server.workers.retain(|_, worker| worker.id != 1);
        rig.expire();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Failure));
        assert_eq!(final_message(&mut rig).as_deref(), Some(UNFENCED));
        assert!(rig.children.0[1].try_wait().expect("try_wait").is_none());
    }

    /// A rollback no worker confirmed and whose worker could not be stopped
    /// leaves a worker that may still hold the proposed values: its cluster
    /// stays behind the fence, so a second change cannot reach prepare while
    /// the first one's values may still be applied. Another cluster is not
    /// behind it.
    #[test]
    fn an_unfenced_rollback_keeps_its_cluster_blocked_and_only_its_cluster() {
        const OTHER: &str = "other";
        let mut rig = rig(2);
        let other: sozu_command_lib::proto::command::Request = RequestType::AddCluster(Cluster {
            cluster_id: OTHER.to_owned(),
            ..Default::default()
        })
        .into();
        rig.hub
            .server
            .state
            .dispatch(&other)
            .expect("a second cluster");
        for worker in &mut rig.workers {
            worker
                .state
                .dispatch(&other)
                .expect("a worker second cluster");
        }
        // Worker 1 applies the commit and never answers it.
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        // The main process lost track of worker 1 before it could be stopped.
        rig.hub.server.workers.retain(|_, worker| worker.id != 1);
        rig.expire();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Failure));
        assert_eq!(final_message(&mut rig).as_deref(), Some(UNFENCED));
        assert!(
            rig.workers[1].held_values().is_some(),
            "worker 1 still holds the proposed values"
        );

        // B on the same cluster must not reach prepare, nor any worker.
        let requests_before = rig.workers[0].requests_seen;
        let prepares_before = rig.workers[0].prepares_seen;
        let before = rig.hub.server.state.hash_state();
        rig.begin(order(&[("X-Token", "SYNTHETIC-SECOND-91")]));
        rig.pump();
        assert_eq!(
            final_message(&mut rig).as_deref(),
            Some(BUSY),
            "B advanced behind a worker that may still hold A"
        );
        assert_eq!(
            rig.workers[0].requests_seen, requests_before,
            "B reached a worker"
        );
        assert_eq!(
            rig.workers[0].prepares_seen, prepares_before,
            "B reached prepare"
        );
        assert!(
            rig.hub.server.protected.is_active(CLUSTER),
            "the fence is retained"
        );
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_none());
        assert_eq!(rig.hub.server.state.hash_state(), before);
        assert!(!rig.observable().contains("SYNTHETIC-SECOND-91"));

        // Another cluster is not blocked and goes through.
        assert!(
            !rig.hub
                .server
                .protected
                .blocks(&RequestType::RemoveCluster(OTHER.to_owned()))
        );
        let mut other_order = order(&[("X-Other", "SYNTHETIC-OTHER-CLUSTER-3")]);
        other_order.cluster_id = OTHER.to_owned();
        rig.begin(other_order);
        rig.pump();
        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        assert!(!rig.hub.server.protected.is_active(OTHER));
        assert!(rig.hub.server.protected.is_active(CLUSTER), "still fenced");
    }

    /// One change per cluster at a time. While A is in flight a second
    /// protected change B is refused by the real entry point before it
    /// proposes, stages or sends anything: no worker ever holds A and B at
    /// once, so the abort of A cannot restore over a B applied behind it.
    #[test]
    fn a_second_protected_change_cannot_reach_prepare_while_the_first_is_active() {
        let mut rig = rig(2);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        // A is applied on worker 0, applied on worker 1 with its answer held.
        assert!(rig.hub.server.protected.is_active(CLUSTER));
        assert_eq!(
            rig.workers[0].held_values(),
            Some(vec![SECRET.as_bytes().to_vec()])
        );
        let version = rig.hub.server.protected.active[CLUSTER];
        let before = rig.hub.server.state.hash_state();

        rig.begin(order(&[("X-Token", "SYNTHETIC-SECOND-77")]));
        rig.pump();

        assert_eq!(final_message(&mut rig).as_deref(), Some(BUSY));
        for worker in &rig.workers {
            assert_eq!(
                worker.prepares_seen, 1,
                "worker {}: B reached prepare",
                worker.id
            );
            assert_eq!(
                worker.phases_seen,
                vec![
                    ProtectedHealthCheckPhase::ProtectedHealthCheckCapability,
                    ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare,
                    ProtectedHealthCheckPhase::ProtectedHealthCheckCommit,
                ],
                "worker {}: B sent something",
                worker.id
            );
            assert_eq!(worker.held_values().map(|values| values.len()), Some(1));
        }
        assert_eq!(
            rig.hub.server.protected.active[CLUSTER], version,
            "A still owns the fence"
        );
        assert_eq!(rig.hub.server.state.hash_state(), before);
        assert!(!rig.observable().contains("SYNTHETIC-SECOND-77"));

        // A fails (worker 1 never confirms its commit): every worker is back
        // to the exact predecessor, B included in nothing.
        rig.expire();
        assert_eq!(rig.final_status(), Some(ResponseStatus::Failure));
        assert_nothing_applied(&mut rig);

        // B is accepted once A released the cluster.
        rig.workers[1].holds = None;
        rig.begin(order(&[("X-Token", "SYNTHETIC-SECOND-77")]));
        rig.pump();
        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        assert_eq!(
            rig.hub.server.protected.held_values(CLUSTER),
            Some(vec![b"SYNTHETIC-SECOND-77".to_vec()])
        );
    }

    /// Every ordinary direct order that rewrites the cluster, its health check
    /// or the validity of the one against the other is refused by the real
    /// request entry point before any state, journal or worker is touched.
    #[test]
    fn every_conflicting_ordinary_entry_is_refused_by_the_real_entry_point_before_mutation() {
        let mut rig = rig(2);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        assert!(rig.hub.server.protected.is_active(CLUSTER));
        let held_before = rig.hub.server.protected.held_values(CLUSTER);
        // `assert!` and not `assert_eq!`: a failure must not print the values.
        assert!(
            held_before == Some(vec![SECRET.as_bytes().to_vec()]),
            "the active transaction holds exactly its proposed values"
        );

        let blocked = [
            RequestType::AddCluster(Cluster {
                cluster_id: CLUSTER.to_owned(),
                ..Default::default()
            }),
            RequestType::AddUnixBackend(AddBackend {
                cluster_id: CLUSTER.to_owned(),
                unix_path: Some("/tmp/synthetic.sock".to_owned()),
                ..Default::default()
            }),
            RequestType::SetHealthCheck(SetHealthCheck {
                cluster_id: CLUSTER.to_owned(),
                config: HealthCheckConfig::default(),
            }),
            RequestType::RemoveHealthCheck(CLUSTER.to_owned()),
            RequestType::RemoveCluster(CLUSTER.to_owned()),
            RequestType::LoadState("state".to_owned()),
            RequestType::ReloadConfiguration("config".to_owned()),
            RequestType::UpgradeWorker(0),
        ];
        let before = rig.hub.server.state.hash_state();
        let sent = rig
            .workers
            .iter()
            .map(|worker| worker.requests_seen)
            .collect::<Vec<_>>();
        let answers = rig.responses().len();
        for request in blocked {
            let (server, client) = rig.hub.get_client_mut(&rig.client).expect("the client");
            server.handle_client_request(client, request.to_owned().into());
            assert_eq!(
                final_message(&mut rig).as_deref(),
                Some(BUSY),
                "{request:?} must be refused"
            );
        }
        assert_eq!(rig.responses().len(), answers + 8, "one refusal per order");
        assert_eq!(rig.hub.server.state.hash_state(), before);
        rig.pump();
        assert_eq!(
            rig.workers
                .iter()
                .map(|worker| worker.requests_seen)
                .collect::<Vec<_>>(),
            sent,
            "nothing was scattered to a worker"
        );
        // The refused orders left the active transaction's values untouched.
        assert_eq!(
            rig.hub.server.protected.held_values(CLUSTER),
            Some(vec![SECRET.as_bytes().to_vec()])
        );
        assert!(
            rig.hub.server.protected.held_values(CLUSTER) == held_before,
            "the eight refusals changed the held values"
        );
        assert!(rig.hub.server.protected.is_active(CLUSTER));

        // The public verb is validated before any mutation on a cluster that
        // is not in flight (on the active one the busy gate answers first), and
        // that cluster stays free.
        {
            let mut public = order(&[("X", "SYNTHETIC-PUBLIC-1")]);
            public.cluster_id = "other".to_owned();
            let (server, client) = rig.hub.get_client_mut(&rig.client).expect("the client");
            server
                .handle_client_request(client, RequestType::SetProtectedHealthCheck(public).into());
        }
        assert_eq!(
            final_message(&mut rig).as_deref(),
            Some("the cluster does not exist")
        );
        assert!(
            !rig.hub
                .server
                .protected
                .blocks(&RequestType::AddUnixBackend(AddBackend {
                    cluster_id: "other".to_owned(),
                    ..Default::default()
                }))
        );
        assert!(!rig.observable().contains("SYNTHETIC-PUBLIC-1"));
    }

    #[test]
    fn an_ordinary_rewrite_keeps_the_values_only_for_the_exact_headers() {
        let mut rig = rig(1);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        let kept = rig.main_headers();

        rig.hub.server.protected.ordinary_change(CLUSTER, &kept);
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_some());

        let mut renamed = kept.to_owned();
        renamed[0].name = "X-Other".to_owned();
        rig.hub.server.protected.ordinary_change(CLUSTER, &renamed);
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_none());
    }

    #[test]
    fn a_change_in_flight_refuses_to_be_snapshotted() {
        let mut rig = rig(1);
        rig.workers[0].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        let refused =
            rig.hub.task_snapshot_results().into_iter().any(|result| {
                matches!(result, Err(TaskSnapshotError::ProtectedTransactionInFlight))
            });
        assert!(refused, "a change in flight must block a main upgrade");
    }

    /// Everything the capability query must leave exactly as it found it,
    /// except the task counter: the product state, the protected runtime, the
    /// request census and the health configuration of every party.
    fn untouched_view(rig: &mut Rig) -> String {
        let runtime = format!("{:?}", rig.hub.server.protected);
        let hash = format!("{:?}", rig.hub.server.state.hash_state());
        let state = state_view(&rig.hub.server.state);
        let census = format!("{:?}", rig.hub.server.state.request_counts);
        let audit = rig.hub.server.pending_audit_events.len();
        let workers = rig
            .workers
            .iter()
            .map(|worker| {
                format!(
                    "{:?}{}{:?}",
                    worker.protected,
                    state_view(&worker.state),
                    worker.held_values()
                )
            })
            .collect::<String>();
        format!("{runtime}{hash}{state}{census}{audit}{workers}")
    }

    fn query(rig: &mut Rig) {
        let (server, client) = rig.hub.get_client_mut(&rig.client).expect("the client");
        begin_capability_query(server, client);
    }

    /// The query changed nothing but the ordinary task-id allocator, which the
    /// scatter machinery advances exactly once (not at all when no worker is
    /// asked); no task is left behind.
    fn assert_query_changed_nothing(rig: &mut Rig, before: &str, next_task_id: usize) {
        assert_eq!(
            rig.hub.server.peek_next_task_id(),
            next_task_id,
            "the allocator moves only for the query's own task"
        );
        assert_eq!(rig.hub.live_task_count(), 0, "no task is left behind");
        assert!(rig.hub.server.protected.key.is_none(), "no fence key");
        assert!(rig.hub.server.protected.active.is_empty(), "no fence");
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_none());
        assert!(rig.main_headers().is_empty(), "main state is untouched");
        for worker in &rig.workers {
            assert_eq!(worker.prepares_seen, 0, "worker {} got a value", worker.id);
            assert!(
                worker.phases_seen.iter().all(
                    |phase| *phase == ProtectedHealthCheckPhase::ProtectedHealthCheckCapability
                ),
                "worker {} saw {:?}",
                worker.id,
                worker.phases_seen
            );
            assert!(worker.held_values().is_none());
        }
        assert_eq!(untouched_view(rig), before, "the query mutated state");
        assert!(!rig.observable().contains("sozu-hc-hmac"), "no fingerprint");
    }

    fn assert_never_positive(rig: &mut Rig, what: &str) {
        assert_eq!(rig.final_status(), Some(ResponseStatus::Failure), "{what}");
        assert!(
            !rig.responses()
                .iter()
                .any(|response| response.status == ResponseStatus::Ok as i32),
            "{what}: a positive answer was sent"
        );
    }

    #[test]
    fn the_capability_query_is_positive_only_when_every_worker_answers_and_changes_nothing() {
        let mut rig = rig(2);
        let before = untouched_view(&mut rig);
        let first_task_id = rig.hub.server.peek_next_task_id();

        query(&mut rig);
        rig.pump();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        // The CLI accepts nothing but this exact typed payload as a yes.
        let positive: Vec<Response> = rig
            .responses()
            .into_iter()
            .filter(|response| response.status == ResponseStatus::Ok as i32)
            .collect();
        assert_eq!(positive.len(), 1, "one positive answer");
        assert_eq!(
            positive[0].content,
            Some(ResponseContent {
                content_type: Some(ContentType::ProtectedHealthCheckReply(
                    ProtectedHealthCheckReply {
                        phase: ProtectedHealthCheckPhase::ProtectedHealthCheckCapability as i32,
                        cluster_id: String::new(),
                        version: 0,
                    }
                )),
            }),
            "the capability answer carries the typed payload"
        );
        for worker in &rig.workers {
            assert_eq!(
                worker.phases_seen,
                vec![ProtectedHealthCheckPhase::ProtectedHealthCheckCapability],
                "worker {} was asked exactly once, and for nothing else",
                worker.id
            );
            assert_eq!(worker.requests_seen, 1, "worker {}", worker.id);
        }
        assert_query_changed_nothing(&mut rig, &before, first_task_id + 1);
    }

    #[test]
    fn a_silent_refusing_or_wrong_worker_never_makes_the_query_positive() {
        type Break = fn(&mut FakeWorker);
        let breaks: [(&str, Break); 6] = [
            ("silent", |worker| worker.predates_protocol = true),
            ("refusing", |worker| {
                worker.refuses = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
            }),
            ("wrong version", |worker| {
                worker.answers_wrong_version =
                    Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
            }),
            ("no content", |worker| {
                worker.answers_shape = Some((
                    ProtectedHealthCheckPhase::ProtectedHealthCheckCapability,
                    WrongShape::NoContent,
                ))
            }),
            ("wrong phase", |worker| {
                worker.answers_shape = Some((
                    ProtectedHealthCheckPhase::ProtectedHealthCheckCapability,
                    WrongShape::OtherPhase,
                ))
            }),
            ("wrong cluster", |worker| {
                worker.answers_shape = Some((
                    ProtectedHealthCheckPhase::ProtectedHealthCheckCapability,
                    WrongShape::OtherCluster,
                ))
            }),
        ];
        for (what, break_worker) in breaks {
            let mut rig = rig(2);
            break_worker(&mut rig.workers[1]);
            let before = untouched_view(&mut rig);
            let first_task_id = rig.hub.server.peek_next_task_id();

            query(&mut rig);
            rig.pump();
            assert_ne!(rig.final_status(), Some(ResponseStatus::Ok), "{what}");
            rig.expire();

            assert_never_positive(&mut rig, what);
            assert_query_changed_nothing(&mut rig, &before, first_task_id + 1);
        }
    }

    /// A progress report is not an answer: it never counts, whatever it
    /// carries, and the silent worker's deadline still ends the query.
    #[test]
    fn a_processing_reply_neither_renews_nor_converts_the_query() {
        let mut rig = rig(2);
        rig.workers[1].predates_protocol = true;
        let before = untouched_view(&mut rig);
        let first_task_id = rig.hub.server.peek_next_task_id();

        query(&mut rig);
        rig.pump();
        assert_eq!(rig.final_status(), None, "worker 1 is still owed");
        for worker in 0..2 {
            let mut processing = WorkerResponse::processing(format!("{worker}-{first_task_id}-0"));
            processing.content = Some(
                ContentType::ProtectedHealthCheckReply(
                    sozu_command_lib::proto::command::ProtectedHealthCheckReply {
                        phase: ProtectedHealthCheckPhase::ProtectedHealthCheckCapability as i32,
                        cluster_id: String::new(),
                        version: 0,
                    },
                )
                .into(),
            );
            rig.hub.handle_worker_response(worker, processing);
        }
        rig.pump();
        assert_eq!(rig.final_status(), None, "a Processing reply is no answer");
        assert_eq!(rig.hub.live_task_count(), 1, "the query is still waiting");

        rig.expire();
        assert_never_positive(&mut rig, "processing only");
        assert_query_changed_nothing(&mut rig, &before, first_task_id + 1);
    }

    #[test]
    fn a_closed_worker_or_no_worker_never_makes_the_query_positive() {
        let mut rig = rig(2);
        rig.workers[1].predates_protocol = true;
        let before = untouched_view(&mut rig);
        let first_task_id = rig.hub.server.peek_next_task_id();
        query(&mut rig);
        let token = *rig
            .hub
            .server
            .workers
            .iter()
            .find(|(_, worker)| worker.id == 1)
            .map(|(token, _)| token)
            .expect("worker 1");
        rig.hub.on_worker_channel_closed(&token, 1);
        rig.pump();
        rig.expire();
        assert_never_positive(&mut rig, "closed worker");
        assert_query_changed_nothing(&mut rig, &before, first_task_id + 1);

        // With no worker at all there is nobody to hold a value, and nobody to
        // vouch for the protocol: the query is not positive.
        let mut empty = super::tests::rig(0);
        let before = untouched_view(&mut empty);
        let first_task_id = empty.hub.server.peek_next_task_id();
        query(&mut empty);
        empty.pump();
        assert_never_positive(&mut empty, "no worker");
        assert_query_changed_nothing(&mut empty, &before, first_task_id);
    }

    /// Send `request` through the production socket route.
    fn public(rig: &mut Rig, request: RequestType) {
        let (server, client) = rig.hub.get_client_mut(&rig.client).expect("the client");
        server.handle_client_request(client, request.into());
    }

    fn public_query() -> RequestType {
        RequestType::QueryProtectedHealthCheckSupport(QueryProtectedHealthCheckSupport {})
    }

    /// The typed payload the CLI accepts as a yes, and nothing else.
    fn assert_one_positive_capability_answer(rig: &mut Rig) {
        let positive: Vec<Response> = rig
            .responses()
            .into_iter()
            .filter(|response| response.status == ResponseStatus::Ok as i32)
            .collect();
        assert_eq!(positive.len(), 1, "one positive answer");
        assert_eq!(
            positive[0].content,
            Some(ResponseContent {
                content_type: Some(ContentType::ProtectedHealthCheckReply(
                    ProtectedHealthCheckReply {
                        phase: ProtectedHealthCheckPhase::ProtectedHealthCheckCapability as i32,
                        cluster_id: String::new(),
                        version: 0,
                    }
                )),
            })
        );
    }

    /// Tag 73 through `Server::handle_client_request` runs the whole
    /// acknowledged transaction: Success only after the last Commit ack, then
    /// the exact metadata and value on main and on every worker.
    #[test]
    fn the_public_set_route_commits_on_every_worker_and_leaks_nothing() {
        let mut rig = rig(2);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);

        public(
            &mut rig,
            RequestType::SetProtectedHealthCheck(order(&[("Authorization", SECRET)])),
        );
        rig.pump();
        assert_eq!(rig.final_status(), None, "no answer before the last ack");
        assert!(rig.main_headers().is_empty(), "main waits for the last ack");
        assert!(rig.hub.server.protected.is_active(CLUSTER));

        rig.workers[1].release_held();
        rig.pump();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        let expected = vec![SECRET.as_bytes().to_vec()];
        for worker in &rig.workers {
            assert_eq!(worker.held_values(), Some(expected.clone()));
            assert_eq!(
                worker.state.clusters[CLUSTER]
                    .health_check
                    .as_ref()
                    .unwrap()
                    .headers,
                rig.main_headers()
            );
            assert_eq!(
                worker.phases_seen,
                vec![
                    ProtectedHealthCheckPhase::ProtectedHealthCheckCapability,
                    ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare,
                    ProtectedHealthCheckPhase::ProtectedHealthCheckCommit,
                    ProtectedHealthCheckPhase::ProtectedHealthCheckRelease,
                ],
                "worker {}",
                worker.id
            );
        }
        assert_eq!(
            rig.hub.server.protected.held_values(CLUSTER),
            Some(expected)
        );
        let headers = rig.main_headers();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].name, "Authorization");
        assert!(!rig.hub.server.protected.is_active(CLUSTER));
        assert_eq!(rig.hub.live_task_count(), 0);
        assert!(!rig.observable().contains(SECRET));
    }

    /// Tag 74 through the socket route is positive only once every worker has
    /// acknowledged the Query, and it changes nothing but the task allocator.
    #[test]
    fn the_public_query_route_is_positive_only_after_every_worker_acknowledges() {
        let mut rig = rig(2);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability);
        let before = untouched_view(&mut rig);
        let first_task_id = rig.hub.server.peek_next_task_id();

        public(&mut rig, public_query());
        rig.pump();
        assert_eq!(rig.final_status(), None, "worker 1 has not acknowledged");

        rig.workers[1].release_held();
        rig.pump();

        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        assert_one_positive_capability_answer(&mut rig);
        for worker in &rig.workers {
            assert_eq!(
                worker.phases_seen,
                vec![ProtectedHealthCheckPhase::ProtectedHealthCheckCapability],
                "worker {}",
                worker.id
            );
            assert_eq!(worker.requests_seen, 1, "worker {}", worker.id);
        }
        assert_query_changed_nothing(&mut rig, &before, first_task_id + 1);
    }

    /// A worker that cannot vouch for the protocol, reached through the socket
    /// route, ends tag 74 without a positive answer and tag 73 without any
    /// mutation: no value is sent to a single worker.
    #[test]
    fn the_public_routes_never_succeed_through_an_unsupported_worker() {
        type Break = fn(&mut FakeWorker);
        let breaks: [(&str, Break); 3] = [
            ("silent", |worker| worker.predates_protocol = true),
            ("refusing", |worker| {
                worker.refuses = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
            }),
            ("wrong version", |worker| {
                worker.answers_wrong_version =
                    Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
            }),
        ];
        for (what, break_worker) in breaks {
            let mut rig = rig(2);
            break_worker(&mut rig.workers[1]);
            let before = untouched_view(&mut rig);
            let first_task_id = rig.hub.server.peek_next_task_id();
            public(&mut rig, public_query());
            rig.pump();
            rig.expire();
            assert_never_positive(&mut rig, what);
            assert_query_changed_nothing(&mut rig, &before, first_task_id + 1);

            let mut rig = super::tests::rig(2);
            break_worker(&mut rig.workers[1]);
            public(
                &mut rig,
                RequestType::SetProtectedHealthCheck(order(&[("Authorization", SECRET)])),
            );
            rig.pump();
            rig.expire();
            assert_never_positive(&mut rig, what);
            assert_nothing_applied(&mut rig);
            for worker in &rig.workers {
                assert_eq!(
                    worker.prepares_seen, 0,
                    "{what}: worker {} was sent a value",
                    worker.id
                );
            }
        }

        // With no worker at all, tag 74 is not positive either, and tag 73
        // changes nothing.
        let mut empty = super::tests::rig(0);
        public(&mut empty, public_query());
        empty.pump();
        assert_never_positive(&mut empty, "no worker");
        let mut empty = super::tests::rig(0);
        public(
            &mut empty,
            RequestType::SetProtectedHealthCheck(order(&[("Authorization", SECRET)])),
        );
        empty.pump();
        empty.expire();
        assert_ne!(empty.final_status(), Some(ResponseStatus::Ok));
        assert!(empty.main_headers().is_empty());
        assert!(empty.hub.server.protected.held_values(CLUSTER).is_none());
        assert!(!empty.observable().contains(SECRET));
    }

    /// Tag 75 stays an internal verb on the command socket.
    #[test]
    fn the_internal_order_verb_is_refused_on_the_socket() {
        let mut rig = rig(1);
        public(
            &mut rig,
            RequestType::ProtectedHealthCheckOrder(ProtectedHealthCheckOrder {
                phase: ProtectedHealthCheckPhase::ProtectedHealthCheckAbort as i32,
                cluster_id: CLUSTER.to_owned(),
                version: 1,
                prepare: None,
            }),
        );
        assert_eq!(final_message(&mut rig).as_deref(), Some(INTERNAL));
        assert_eq!(rig.workers[0].requests_seen, 0);
    }

    fn snapshot_refusal(rig: &Rig) -> Option<String> {
        match rig.hub.generate_upgrade_data(rig.client) {
            Ok(_) => None,
            Err(error) => Some(error.to_string()),
        }
    }

    #[test]
    fn a_runtime_with_no_fence_and_no_value_stays_snapshotable() {
        let rig = rig(1);
        assert_eq!(snapshot_refusal(&rig), None);
    }

    /// A rollback no worker confirmed, whose worker could not be stopped, ends
    /// with the task gone and the fence up. The task snapshot has nothing to
    /// refuse any more; the runtime must.
    #[test]
    fn a_finished_unfenced_transaction_with_no_task_refuses_the_snapshot_for_good() {
        let mut rig = rig(2);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        rig.hub.server.workers.retain(|_, worker| worker.id != 1);
        rig.expire();
        assert_eq!(final_message(&mut rig).as_deref(), Some(UNFENCED));
        assert!(
            rig.hub.live_task_count() == 0,
            "no task owns the fence any more"
        );
        assert!(rig.hub.server.protected.is_active(CLUSTER));
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_none());

        let refusal = snapshot_refusal(&rig).expect("the snapshot must be refused");
        assert!(
            refusal.contains("reconciliation or controlled restart required"),
            "{refusal}"
        );
        assert!(!refusal.to_lowercase().contains("retry"), "{refusal}");
        assert!(!refusal.contains(SECRET), "{refusal}");
    }

    /// Intended behavior change: confirmed values the configuration names
    /// exactly are handed over by protocol 3, so they no longer refuse the
    /// snapshot. Values the configuration does not name still do.
    #[test]
    fn confirmed_values_the_configuration_names_are_exported_and_others_refuse_the_snapshot() {
        let mut rig = rig(1);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        assert!(rig.hub.server.protected.active.is_empty(), "no fence left");
        assert_eq!(rig.hub.live_task_count(), 0, "no task owns the values");
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_some());
        assert_eq!(snapshot_refusal(&rig), None, "named values are exported");

        // The configuration loses the header: the held value is no longer
        // named, so nothing can vouch for it and the snapshot is refused.
        let kept = rig.hub.server.state.clusters.get_mut(CLUSTER).unwrap();
        let health_check = kept.health_check.take();
        let refusal = snapshot_refusal(&rig).expect("an unnamed value refuses the snapshot");
        assert!(
            refusal.contains("reconciliation or controlled restart required"),
            "{refusal}"
        );
        assert!(!refusal.contains(SECRET), "{refusal}");
        rig.hub
            .server
            .state
            .clusters
            .get_mut(CLUSTER)
            .unwrap()
            .health_check = health_check;
        assert_eq!(snapshot_refusal(&rig), None);

        // An ordinary rewrite drops the values: nothing is held any more.
        rig.hub.server.protected.ordinary_change(CLUSTER, &[]);
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_none());
    }

    /// A non-empty protected header set, built the way an order builds it.
    fn held_set() -> sozu_command_lib::secret::ProtectedHeaderSet {
        let key = FingerprintKey::generate().expect("a key");
        let mut incoming = order(&[("Authorization", SECRET)]);
        ProtectedHealthCheck::from_ingress(&key, &mut incoming)
            .expect("a valid order")
            .headers()
            .to_owned()
    }

    #[test]
    fn retained_values_refuse_the_snapshot_even_when_no_fence_and_no_task_exist() {
        let mut rig = rig(1);
        let journal = &mut rig.hub.server.protected.journal;
        let pending = journal
            .propose(CLUSTER, ProtectedHeaderChange::Replace(held_set()))
            .expect("a version");
        assert!(rig.hub.server.protected.active.is_empty(), "no fence");
        assert_eq!(rig.hub.live_task_count(), 0, "no task");
        let refusal = snapshot_refusal(&rig).expect("a pending value must refuse");
        assert!(
            refusal.contains("reconciliation or controlled restart required"),
            "{refusal}"
        );
        assert!(!refusal.to_lowercase().contains("retry"), "{refusal}");

        // Confirmed, then hidden by a pending Clear: the bytes are still held.
        let journal = &mut rig.hub.server.protected.journal;
        journal.accept(CLUSTER, pending);
        let clear = journal
            .propose(CLUSTER, ProtectedHeaderChange::Clear)
            .expect("a version");
        assert!(
            rig.hub
                .server
                .protected
                .held_values(CLUSTER)
                .unwrap()
                .is_empty()
        );
        assert!(
            snapshot_refusal(&rig).is_some(),
            "hidden values still refuse"
        );

        // Only once the clear is confirmed is nothing retained.
        rig.hub.server.protected.journal.accept(CLUSTER, clear);
        assert_eq!(snapshot_refusal(&rig), None);
    }

    #[test]
    fn a_fence_alone_refuses_the_snapshot_and_lifting_it_restores_the_ordinary_one() {
        let mut rig = rig(1);
        rig.hub
            .server
            .protected
            .active
            .insert(CLUSTER.to_owned(), ProtectedHeaderVersion::from_wire(1));
        let refusal = snapshot_refusal(&rig).expect("a fence must refuse");
        assert!(
            refusal.contains("reconciliation or controlled restart required"),
            "{refusal}"
        );
        rig.hub.server.protected.active.clear();
        assert_eq!(snapshot_refusal(&rig), None);
    }

    // ---- confirmed-value bootstrap of a fresh worker ----

    /// The state of worker `id` as the main process sees it.
    fn run_state_of(rig: &Rig, id: WorkerId) -> Option<RunState> {
        rig.hub
            .server
            .workers
            .values()
            .find(|worker| worker.id == id)
            .map(|worker| worker.run_state)
    }

    /// A rig whose single worker 0 applied and released the confirmed set
    /// `[("Authorization", SECRET)]` on `CLUSTER`.
    fn rig_with_confirmed_values() -> Rig {
        let mut rig = rig(1);
        rig.begin(order(&[("Authorization", SECRET)]));
        rig.pump();
        rig.pump();
        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        assert!(rig.hub.server.protected.held_values(CLUSTER).is_some());
        rig
    }

    /// Register a fresh fake worker `id`, forked from the main state as a real
    /// fork would be, and start its initialization the way
    /// `Server::launch_worker_awaiting_status` does once the child exists.
    fn start_fresh_worker(rig: &mut Rig, id: WorkerId) {
        start_worker_with(
            rig,
            id,
            Box::new(crate::command::server::WorkerReadinessTask::new(id)),
        );
    }

    /// Like [`start_fresh_worker`], judged by the readiness task `job`.
    fn start_worker_with(rig: &mut Rig, id: WorkerId, job: Box<dyn GatheringTask>) {
        let state = rig.hub.server.state.clone();
        let fake = register_fake(&mut rig.hub, &mut rig.children, id, state);
        rig.workers.push(fake);
        rig.hub.server.begin_worker_initialization(id, job);
    }

    #[test]
    fn a_fresh_worker_receives_the_confirmed_values_before_its_status() {
        let mut rig = rig_with_confirmed_values();
        let before = rig
            .hub
            .server
            .protected
            .held_set(CLUSTER)
            .expect("a confirmed set");
        rig.workers[0].sequence.clear();
        let next_worker_requests = rig.workers[0].requests_seen;

        start_fresh_worker(&mut rig, 1);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckRelease);
        rig.pump();

        assert_eq!(
            rig.workers[1].sequence,
            ["Query", "Prepare", "Commit", "Release"],
            "the values precede the status, which waits for the release"
        );
        assert_eq!(
            run_state_of(&rig, 1),
            Some(RunState::NotAnswering),
            "not running before the release and the status are acknowledged"
        );

        rig.workers[1].release_held();
        rig.pump();

        assert_eq!(
            rig.workers[1].sequence,
            ["Query", "Prepare", "Commit", "Release", "Status"]
        );
        assert_eq!(run_state_of(&rig, 1), Some(RunState::Running));
        assert_eq!(
            rig.workers[1].held_values(),
            Some(vec![SECRET.as_bytes().to_vec()])
        );
        assert_eq!(rig.workers[0].requests_seen, next_worker_requests);
        let after = rig
            .hub
            .server
            .protected
            .held_set(CLUSTER)
            .expect("still confirmed");
        assert!(before.ptr_eq(&after), "the journal handle is unchanged");
        assert!(
            rig.hub
                .server
                .protected
                .is_snapshot_safe(&rig.hub.server.state),
            "no pending version is left behind"
        );
        assert!(!rig.observable().contains(SECRET));
    }

    fn token_of(rig: &Rig, id: WorkerId) -> Token {
        *rig.hub
            .server
            .workers
            .iter()
            .find(|(_, worker)| worker.id == id)
            .map(|(token, _)| token)
            .expect("a registered worker")
    }

    /// Everything the main process and its workers expose about a change that
    /// must not have happened, but the client's own answers.
    fn world(rig: &Rig) -> String {
        let fakes = rig
            .workers
            .iter()
            .map(|fake| format!("{}:{}:{:?}", fake.id, fake.requests_seen, fake.sequence))
            .collect::<String>();
        format!(
            "{:?}{:?}{}{}",
            rig.hub.server,
            rig.hub.server.protected,
            state_view(&rig.hub.server.state),
            fakes
        )
    }

    /// How the fresh worker of [`a_bootstrap_that_is_not_acknowledged_never_promotes`]
    /// misbehaves.
    enum Fault {
        Refuses(ProtectedHealthCheckPhase),
        Shape(ProtectedHealthCheckPhase, WrongShape),
        WrongVersion(ProtectedHealthCheckPhase),
        Silent,
        Closes,
    }

    #[test]
    fn a_bootstrap_that_is_not_acknowledged_never_promotes() {
        use ProtectedHealthCheckPhase as Phase;
        let faults = [
            Fault::Refuses(Phase::ProtectedHealthCheckCapability),
            Fault::Shape(Phase::ProtectedHealthCheckCapability, WrongShape::NoContent),
            Fault::Shape(Phase::ProtectedHealthCheckPrepare, WrongShape::NoContent),
            Fault::Shape(Phase::ProtectedHealthCheckCommit, WrongShape::OtherPhase),
            Fault::WrongVersion(Phase::ProtectedHealthCheckPrepare),
            Fault::Silent,
            Fault::Closes,
            Fault::Refuses(Phase::ProtectedHealthCheckCommit),
            Fault::Refuses(Phase::ProtectedHealthCheckRelease),
        ];
        let cases = faults.len();
        for (case, fault) in faults.into_iter().enumerate() {
            let mut rig = rig_with_confirmed_values();
            let held = rig.hub.server.protected.held_set(CLUSTER).expect("held");
            let state_before = state_view(&rig.hub.server.state);

            start_fresh_worker(&mut rig, 1);
            match fault {
                Fault::Refuses(phase) => rig.workers[1].refuses = Some(phase),
                Fault::Shape(phase, shape) => rig.workers[1].answers_shape = Some((phase, shape)),
                Fault::WrongVersion(phase) => rig.workers[1].answers_wrong_version = Some(phase),
                Fault::Silent => rig.workers[1].predates_protocol = true,
                Fault::Closes => {
                    let token = token_of(&rig, 1);
                    rig.hub.on_worker_channel_closed(&token, 1);
                }
            }
            rig.pump();
            rig.expire();

            assert_eq!(
                run_state_of(&rig, 1),
                Some(RunState::Stopped),
                "case {case}: the worker is fenced"
            );
            assert!(
                !rig.workers[1].sequence.contains(&"Status"),
                "case {case}: no status is asked of a worker that took no values"
            );
            assert!(!rig.hub.server.worker_bootstrap_in_flight(), "case {case}");
            let status = rig.children.0[1].wait().expect("reap the fenced child");
            assert_eq!(
                status.signal(),
                Some(nix::sys::signal::Signal::SIGKILL as i32),
                "case {case}: the child of the fenced worker is killed and reaped"
            );
            assert!(
                held.ptr_eq(&rig.hub.server.protected.held_set(CLUSTER).expect("held")),
                "case {case}: the journal handle is unchanged"
            );
            assert_eq!(
                state_view(&rig.hub.server.state),
                state_before,
                "case {case}"
            );
            assert!(
                rig.hub
                    .server
                    .protected
                    .is_snapshot_safe(&rig.hub.server.state),
                "case {case}: no pending version"
            );
            assert!(!rig.observable().contains(SECRET), "case {case}");
            assert_eq!(
                run_state_of(&rig, 0),
                Some(RunState::NotAnswering),
                "case {case}: worker 0 is untouched"
            );

            // The next fresh worker bootstraps.
            start_fresh_worker(&mut rig, 2);
            rig.pump();
            assert_eq!(
                rig.workers[2].sequence,
                ["Query", "Prepare", "Commit", "Release", "Status"],
                "case {case}"
            );
            assert_eq!(
                run_state_of(&rig, 2),
                Some(RunState::Running),
                "case {case}"
            );
        }
        assert_eq!(cases, 9, "every fault ran");
    }

    #[test]
    fn two_confirmed_clusters_are_visited_in_lexical_order_with_one_query_and_one_status() {
        let mut rig = rig(1);
        let beta = "alpha";
        let add = RequestType::AddCluster(Cluster {
            cluster_id: beta.to_owned(),
            ..Default::default()
        })
        .into();
        rig.hub.server.state.dispatch(&add).expect("a cluster");
        rig.workers[0]
            .state
            .dispatch(&add)
            .expect("a worker cluster");
        for (cluster, secret) in [(CLUSTER, SECRET), (beta, "SYNTHETIC-SECOND-SECRET-71c0")] {
            let mut change = order(&[("Authorization", secret)]);
            change.cluster_id = cluster.to_owned();
            rig.begin(change);
            rig.pump();
            rig.pump();
            assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        }
        let plan = rig
            .hub
            .server
            .protected
            .bootstrap_plan(&rig.hub.server.state)
            .expect("a plan")
            .expect("values are held");
        assert_eq!(plan.clusters(), [beta, CLUSTER], "lexical order");
        assert_eq!(plan.version().get(), 2, "the last issued version");
        let rendered = format!("{plan:?}");
        assert!(!rendered.contains(SECRET) && !rendered.contains("SECOND"));

        start_fresh_worker(&mut rig, 1);
        rig.pump();

        let fresh = &rig.workers[1];
        assert_eq!(
            fresh.sequence,
            [
                "Query", "Prepare", "Commit", "Release", "Prepare", "Commit", "Release", "Status"
            ]
        );
        let expected: Vec<(ProtectedHealthCheckPhase, &str, u64)> = [
            (Stage::Prepare.phase(), beta),
            (Stage::Commit.phase(), beta),
            (ProtectedHealthCheckPhase::ProtectedHealthCheckRelease, beta),
            (Stage::Prepare.phase(), CLUSTER),
            (Stage::Commit.phase(), CLUSTER),
            (
                ProtectedHealthCheckPhase::ProtectedHealthCheckRelease,
                CLUSTER,
            ),
        ]
        .into_iter()
        .map(|(phase, cluster)| (phase, cluster, 2))
        .collect();
        let seen: Vec<(ProtectedHealthCheckPhase, &str, u64)> = fresh
            .orders_seen
            .iter()
            .filter(|(phase, ..)| *phase != Stage::Capability.phase())
            .map(|(phase, cluster, version)| (*phase, cluster.as_str(), *version))
            .collect();
        assert_eq!(seen, expected);
        assert_eq!(run_state_of(&rig, 1), Some(RunState::Running));
        assert!(!rig.observable().contains(SECRET));
        assert!(!rig.observable().contains("SECOND"));
    }

    #[test]
    fn an_upgrade_replacement_serves_only_after_its_bootstrap_and_a_failure_keeps_the_old_worker() {
        for failing in [false, true] {
            let mut rig = rig_with_confirmed_values();
            assert!(rig.hub.server.mark_worker_ready(0));
            let old_token = token_of(&rig, 0);
            let finalizer =
                crate::command::upgrade::awaiting_readiness_task(rig.client, old_token, 0, 1);
            start_worker_with(&mut rig, 1, finalizer);
            if failing {
                rig.workers[1].refuses =
                    Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
            } else {
                rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckRelease);
            }
            let old_requests = rig.workers[0].requests_seen;
            rig.pump();

            if failing {
                assert_eq!(
                    run_state_of(&rig, 0),
                    Some(RunState::Running),
                    "the old worker keeps serving"
                );
                assert_eq!(
                    run_state_of(&rig, 1),
                    Some(RunState::Stopped),
                    "the new one is fenced"
                );
                assert_eq!(rig.final_status(), Some(ResponseStatus::Failure));
                let message = final_message(&mut rig).expect("a message");
                assert!(message.contains("keeps serving"), "{message}");
                assert!(!message.contains(SECRET));
                assert_eq!(
                    rig.workers[0].requests_seen, old_requests,
                    "the old worker was asked nothing"
                );
                continue;
            }
            assert_eq!(run_state_of(&rig, 0), Some(RunState::Running));
            assert_eq!(run_state_of(&rig, 1), Some(RunState::NotAnswering));
            assert_eq!(
                rig.workers[0].requests_seen, old_requests,
                "no soft stop before the replacement is ready"
            );
            rig.workers[1].release_held();
            rig.pump();
            assert_eq!(
                rig.workers[1].sequence[..5],
                ["Query", "Prepare", "Commit", "Release", "Status"]
            );
            assert_eq!(run_state_of(&rig, 1), Some(RunState::Running));
            assert_eq!(
                run_state_of(&rig, 0),
                Some(RunState::Stopping),
                "now the old worker stops"
            );
        }
    }

    fn send(rig: &mut Rig, request: RequestType) {
        let (server, client) = rig.hub.get_client_mut(&rig.client).expect("the client");
        server.handle_client_request(client, request.into());
    }

    fn add_beta() -> RequestType {
        RequestType::AddCluster(Cluster {
            cluster_id: "beta".to_owned(),
            ..Default::default()
        })
    }

    #[test]
    fn a_headerless_initializer_receives_the_orders_queued_after_its_status() {
        for upgrade in [false, true] {
            let mut rig = rig(1);
            assert!(rig.hub.server.mark_worker_ready(0));
            let job: Box<dyn GatheringTask> = if upgrade {
                let old = token_of(&rig, 0);
                crate::command::upgrade::awaiting_readiness_task(rig.client, old, 0, 1)
            } else {
                Box::new(crate::command::server::WorkerReadinessTask::new(1))
            };
            start_worker_with(&mut rig, 1, job);

            // Queued while the worker is still `NotAnswering`, behind its status.
            send(&mut rig, add_beta());
            rig.pump();

            assert_eq!(
                rig.workers[1].sequence,
                ["Status", "AddCluster"],
                "upgrade={upgrade}: the status first, then the order"
            );
            assert!(rig.workers[0].sequence.contains(&"AddCluster"));
            assert_eq!(run_state_of(&rig, 1), Some(RunState::Running));
            assert!(rig.hub.server.state.clusters.contains_key("beta"));
            if !upgrade {
                assert_eq!(
                    rig.final_status(),
                    Some(ResponseStatus::Ok),
                    "the order is acknowledged by every worker, the new one included"
                );
            }
        }
    }

    #[test]
    fn nothing_mutates_while_a_worker_is_being_given_the_confirmed_values() {
        for fails in [false, true] {
            let mut rig = rig_with_confirmed_values();
            assert!(rig.hub.server.mark_worker_ready(0));
            start_fresh_worker(&mut rig, 1);
            rig.workers[1].holds = Some(if fails {
                ProtectedHealthCheckPhase::ProtectedHealthCheckCommit
            } else {
                ProtectedHealthCheckPhase::ProtectedHealthCheckRelease
            });
            rig.pump();
            assert!(rig.hub.server.worker_bootstrap_in_flight());
            let held = rig.hub.server.protected.held_set(CLUSTER).expect("held");
            let before = world(&rig);

            let dir = rig._dir.path().join("state.json");
            let path = dir.to_string_lossy().into_owned();
            for request in [
                add_beta(),
                RequestType::LoadState(path.to_owned()),
                RequestType::ReloadConfiguration(path.to_owned()),
                RequestType::UpgradeWorker(0),
                RequestType::SetHealthCheck(SetHealthCheck {
                    cluster_id: CLUSTER.to_owned(),
                    config: HealthCheckConfig::default(),
                }),
            ] {
                send(&mut rig, request);
                rig.pump();
                assert_eq!(final_message(&mut rig).as_deref(), Some(BOOTSTRAPPING));
                assert_eq!(world(&rig), before, "a refused command changed something");
                assert!(held.ptr_eq(&rig.hub.server.protected.held_set(CLUSTER).expect("held")));
            }

            // A read-only query stays available.
            send(
                &mut rig,
                RequestType::ListWorkers(sozu_command_lib::proto::command::ListWorkers {}),
            );
            assert_ne!(final_message(&mut rig).as_deref(), Some(BOOTSTRAPPING));
            assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));

            // Terminal: success or fence, the gate clears and a valid order goes.
            if fails {
                rig.expire();
                assert_eq!(run_state_of(&rig, 1), Some(RunState::Stopped));
            } else {
                rig.workers[1].release_held();
                rig.pump();
                assert_eq!(run_state_of(&rig, 1), Some(RunState::Running));
            }
            assert!(!rig.hub.server.worker_bootstrap_in_flight());
            send(&mut rig, add_beta());
            rig.pump();
            assert_eq!(
                rig.final_status(),
                Some(ResponseStatus::Ok),
                "fails={fails}"
            );
            assert!(rig.hub.server.state.clusters.contains_key("beta"));
            assert_eq!(rig.workers[0].sequence.last(), Some(&"AddCluster"));
            assert_eq!(
                rig.workers[1].sequence.last() == Some(&"AddCluster"),
                !fails,
                "the new worker takes the order only when it is running"
            );
        }
    }

    #[test]
    fn a_main_upgrade_waits_for_the_bootstrap_even_without_any_protected_change() {
        let mut rig = rig_with_confirmed_values();
        assert!(rig.hub.server.mark_worker_ready(0));
        assert_eq!(snapshot_refusal(&rig), None, "nothing is in flight yet");

        start_fresh_worker(&mut rig, 1);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckRelease);
        rig.pump();

        assert!(
            rig.hub.server.protected.active.is_empty(),
            "no change fence"
        );
        let refusal = snapshot_refusal(&rig).expect("the bootstrap refuses the snapshot");
        assert!(refusal.contains("not confirmed it is ready"), "{refusal}");
        assert!(!refusal.contains(SECRET));

        rig.workers[1].release_held();
        rig.pump();
        assert_eq!(snapshot_refusal(&rig), None, "ready again: snapshotable");
    }

    /// A kill that could not be delivered leaves a worker `Stopped` for
    /// routing while its process may still act: the bootstrap barrier it holds
    /// survives that, the end of its bootstrap task, a late `Status` answer and
    /// every further fence, because `Stopped` is routing state, never a proof of
    /// death.
    ///
    /// To SEE THIS RED: let `Server::close_worker_with` drop the worker from
    /// `bootstrapping_workers` whatever the termination was.
    #[test]
    fn an_unconfirmed_fence_keeps_the_bootstrap_barrier_after_its_task_is_gone() {
        let mut rig = rig_with_confirmed_values();
        assert!(rig.hub.server.mark_worker_ready(0));
        rig.hub.server.config.worker_automatic_restart = true;
        rig.hub.server.config.worker_count = 2;
        start_fresh_worker(&mut rig, 1);
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckRelease);
        rig.pump();
        assert!(rig.hub.server.bootstrapping_workers.contains(&1));
        assert!(rig.hub.server.worker_bootstrap_in_flight());
        assert!(
            rig.hub.live_task_count() > 0,
            "the bootstrap task is in flight"
        );
        let alive = |rig: &mut Rig| rig.children.0[1].try_wait().expect("try_wait").is_none();
        assert_eq!(
            rig.hub.server.workers_to_spawn(),
            0,
            "worker 1 holds the slot"
        );

        // The kill is refused: the process may still run.
        let token = token_of(&rig, 1);
        let pid = rig.hub.server.workers[&token].pid;
        let mut signalled = Vec::new();
        let first = rig.hub.server.close_worker_with(&token, |pid| {
            signalled.push(pid);
            Err(nix::errno::Errno::EPERM)
        });
        assert_eq!(
            first,
            crate::command::server::WorkerTermination::Unconfirmed(nix::errno::Errno::EPERM)
        );
        assert_eq!(signalled, vec![pid]);
        assert_eq!(run_state_of(&rig, 1), Some(RunState::Stopped));
        assert!(alive(&mut rig), "nothing was delivered to the child");
        assert!(
            rig.hub.server.worker_bootstrap_in_flight(),
            "Stopped is routing state: the barrier stays held"
        );
        let held = rig.hub.server.protected.held_set(CLUSTER).expect("held");
        let barrier_holds = |rig: &mut Rig| {
            let before = world(rig);
            send(rig, add_beta());
            rig.pump();
            assert_eq!(final_message(rig).as_deref(), Some(BOOTSTRAPPING));
            assert_eq!(world(rig), before, "a refused command changed something");
            assert!(!rig.hub.server.state.clusters.contains_key("beta"));
            assert!(held.ptr_eq(&rig.hub.server.protected.held_set(CLUSTER).expect("held")));
            assert!(rig.hub.server.worker_bootstrap_in_flight());
        };
        barrier_holds(&mut rig);

        // A read-only query stays available.
        send(
            &mut rig,
            RequestType::ListWorkers(sozu_command_lib::proto::command::ListWorkers {}),
        );
        assert_ne!(final_message(&mut rig).as_deref(), Some(BOOTSTRAPPING));
        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));

        // The bootstrap task ends: the unresolved fence survives it.
        rig.expire();
        assert_eq!(
            rig.hub.live_task_count(),
            0,
            "no task owns the barrier any more"
        );
        assert_eq!(run_state_of(&rig, 1), Some(RunState::Stopped));
        assert!(alive(&mut rig), "the expired task signalled nothing");
        barrier_holds(&mut rig);
        let refusal = snapshot_refusal(&rig).expect("an unresolved fence refuses the snapshot");
        assert!(
            refusal.contains("reconciliation or controlled restart required"),
            "{refusal}"
        );
        assert!(!refusal.contains(SECRET));
        assert_eq!(
            rig.hub.server.workers_to_spawn(),
            0,
            "the slot of the unresolved worker is not refilled"
        );

        // A late `Status` answer neither promotes the worker nor clears the barrier.
        let mut late = crate::command::server::WorkerReadinessTask::new(1);
        late.on_message(
            &mut rig.hub.server,
            &mut None,
            1,
            WorkerResponse::ok("1-9999-0".to_owned()),
        );
        assert_eq!(run_state_of(&rig, 1), Some(RunState::Stopped));
        assert!(!rig.hub.server.mark_worker_ready(1));
        barrier_holds(&mut rig);

        // Fencing again proves nothing and never signals the saved pid.
        let second = rig.hub.server.close_worker_with(&token, |pid| {
            signalled.push(pid);
            Ok(())
        });
        assert_eq!(
            second,
            crate::command::server::WorkerTermination::StoppedUnproven
        );
        assert!(!rig.hub.server.fence_worker(1));
        assert_eq!(
            signalled,
            vec![pid],
            "the saved pid is never signalled again"
        );
        assert!(alive(&mut rig), "the child outlives every fence attempt");
        barrier_holds(&mut rig);
        assert_eq!(run_state_of(&rig, 0), Some(RunState::Running));
        assert!(!rig.observable().contains(SECRET));
    }

    // ---- live TOML reload of file-backed protected headers ----

    use crate::command::requests::load_static_config;

    const A_VALUE: &str = "SYNTHETIC-RELOAD-A-31c7";
    const B_ONE: &str = "SYNTHETIC-RELOAD-B1-8e40";
    const B_TWO: &str = "SYNTHETIC RELOAD B2 5ab9";

    /// One cluster of a reload file: its id and, per header, the name, the
    /// file name (relative to the configuration) and the bytes of that file.
    type ReloadCluster<'a> = (&'a str, Vec<(&'a str, &'a str, &'a [u8])>);

    /// Write a configuration whose clusters read their header values from
    /// files next to it, and return its path.
    fn write_reload_config(dir: &std::path::Path, clusters: &[ReloadCluster<'_>]) -> String {
        let mut toml = String::new();
        for (id, headers) in clusters {
            toml.push_str(&format!(
                "[clusters.{id}]\nprotocol = \"http\"\nfrontends = []\nbackends = []\n\n\
                 [clusters.{id}.health_check]\nuri = \"/\"\ninterval = 10\ntimeout = 5\n\
                 healthy_threshold = 3\nunhealthy_threshold = 3\n\n"
            ));
            for (name, file, bytes) in headers {
                std::fs::write(dir.join(file), bytes).expect("a value file");
                toml.push_str(&format!(
                    "[[clusters.{id}.health_check.headers]]\nname = \"{name}\"\nvalue_file = \"{file}\"\n\n"
                ));
            }
        }
        let path = dir.join("reload.toml");
        std::fs::write(&path, toml).expect("a configuration");
        path.to_str().expect("a utf-8 path").to_owned()
    }

    impl Rig {
        fn reload(&mut self, path: &str) {
            let (server, client) = self.hub.get_client_mut(&self.client).expect("the client");
            load_static_config(server, Some(client), Some(path));
        }

        /// Every non-progress answer the client received, oldest first.
        fn terminals(&mut self) -> Vec<ResponseStatus> {
            self.responses()
                .iter()
                .map(|response| ResponseStatus::try_from(response.status).expect("a status"))
                .filter(|status| *status != ResponseStatus::Processing)
                .collect()
        }

        fn main_health(&self) -> Option<HealthCheckConfig> {
            self.hub.server.state.clusters[CLUSTER]
                .health_check
                .to_owned()
        }
    }

    fn names(headers: &[HealthCheckHeader]) -> Vec<String> {
        headers
            .iter()
            .map(|header| header.name.to_owned())
            .collect()
    }

    /// A rig whose workers answer ordinary orders, as started workers do.
    fn running_rig(worker_count: u32) -> Rig {
        let mut rig = rig(worker_count);
        for id in 0..worker_count {
            assert!(rig.hub.server.mark_worker_ready(id));
        }
        rig
    }

    fn install_a(rig: &mut Rig) {
        rig.begin(order(&[("x-a", A_VALUE)]));
        rig.pump();
        assert_eq!(rig.terminals(), vec![ResponseStatus::Ok]);
    }

    #[test]
    fn a_reload_moves_confirmed_a_to_file_backed_b_only_after_the_capability_ack() {
        let mut rig = running_rig(2);
        install_a(&mut rig);
        let a_health = rig.main_health().expect("a confirmed health check");
        let a_values = vec![A_VALUE.as_bytes().to_vec()];
        let prepares = [rig.workers[0].prepares_seen, rig.workers[1].prepares_seen];
        assert_eq!(rig.hub.server.config.config_path, "");

        let dir = tempfile::tempdir().expect("a directory");
        let path = write_reload_config(
            dir.path(),
            &[(
                CLUSTER,
                vec![
                    ("x-b1", "b1.val", format!("{B_ONE}\r\n").as_bytes()),
                    ("x-b2", "b2.val", format!("{B_TWO}\n").as_bytes()),
                    ("x-b3", "b3.val", b""),
                ],
            )],
        );
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability);
        rig.reload(&path);
        rig.pump();

        // The capability of worker 1 is not acknowledged: nothing of B moved.
        assert_eq!(rig.terminals(), vec![ResponseStatus::Ok], "no terminal yet");
        for (index, worker) in rig.workers.iter().enumerate() {
            assert_eq!(worker.prepares_seen, prepares[index], "no Prepare of B");
            assert_eq!(worker.held_values(), Some(a_values.clone()));
            assert_eq!(
                worker.add_cluster_health.last(),
                Some(&Some(a_health.to_owned())),
                "the generic AddCluster carries the whole of A"
            );
        }
        assert_eq!(rig.main_health(), Some(a_health.to_owned()));
        assert_eq!(rig.hub.server.config.config_path, "", "old source config");

        rig.workers[1].release_held();
        rig.pump();

        assert_eq!(
            rig.terminals(),
            vec![ResponseStatus::Ok, ResponseStatus::Ok],
            "exactly one terminal answer for the reload"
        );
        let b_values = vec![
            B_ONE.as_bytes().to_vec(),
            B_TWO.as_bytes().to_vec(),
            Vec::new(),
        ];
        assert_eq!(names(&rig.main_headers()), vec!["x-b1", "x-b2", "x-b3"]);
        assert_eq!(
            rig.hub.server.protected.held_values(CLUSTER),
            Some(b_values.clone())
        );
        for worker in &rig.workers {
            assert_eq!(worker.held_values(), Some(b_values.clone()));
            assert_eq!(
                names(
                    &worker.state.clusters[CLUSTER]
                        .health_check
                        .as_ref()
                        .unwrap()
                        .headers
                ),
                vec!["x-b1", "x-b2", "x-b3"]
            );
            let tail: Vec<_> = worker
                .sequence
                .iter()
                .rev()
                .take(4)
                .rev()
                .copied()
                .collect();
            assert_eq!(tail, vec!["Query", "Prepare", "Commit", "Release"]);
        }
        assert_eq!(rig.hub.server.config.config_path, path, "new source config");

        let observable = format!("{}{:?}", rig.observable(), rig.hub.server.config);
        for secret in [A_VALUE, B_ONE, B_TWO, "b1.val", "b2.val", "b3.val"] {
            assert!(!observable.contains(secret), "leaked {secret}");
        }
    }

    #[test]
    fn a_headerless_reload_of_a_confirmed_cluster_removes_a_from_every_holder() {
        let mut rig = running_rig(2);
        install_a(&mut rig);
        assert_eq!(
            rig.hub.server.protected.held_values(CLUSTER),
            Some(vec![A_VALUE.as_bytes().to_vec()])
        );
        // The workers apply the generic AddCluster to their state and backends.
        for worker in &mut rig.workers {
            assert_eq!(
                worker.held_values(),
                Some(vec![A_VALUE.as_bytes().to_vec()])
            );
            worker.applies_add_cluster = true;
        }

        // The same cluster, with a health check and no header at all: the
        // authoritative file intentionally drops A.
        let dir = tempfile::tempdir().expect("a directory");
        let path = write_reload_config(dir.path(), &[(CLUSTER, Vec::new())]);
        rig.reload(&path);
        rig.pump();

        assert_eq!(
            rig.terminals(),
            vec![ResponseStatus::Ok, ResponseStatus::Ok],
            "exactly one terminal answer for the reload"
        );
        assert!(rig.main_health().is_some(), "the health check itself stays");
        assert!(rig.main_headers().is_empty(), "main metadata follows");
        assert_eq!(
            rig.hub.server.protected.held_values(CLUSTER),
            None,
            "the main journal no longer holds A"
        );
        for worker in &rig.workers {
            assert_eq!(worker.held_values(), None, "no value on the worker");
            assert!(
                worker.state.clusters[CLUSTER]
                    .health_check
                    .as_ref()
                    .expect("a health check")
                    .headers
                    .is_empty(),
                "worker metadata follows"
            );
        }
        assert!(
            rig.hub
                .server
                .protected
                .bootstrap_plan(&rig.hub.server.state)
                .expect("no stale protected state blocks a bootstrap")
                .is_none(),
            "a new worker is given nothing"
        );
        assert!(
            rig.hub
                .server
                .protected
                .is_snapshot_safe(&rig.hub.server.state),
            "a main upgrade can hand the runtime over"
        );
        let observable = rig.observable();
        for secret in [A_VALUE, "x-a"] {
            assert!(!observable.contains(secret), "leaked {secret}");
        }
    }

    /// The reload of the single-cluster fixture: two files next to the config.
    fn reload_b_config(dir: &std::path::Path) -> String {
        write_reload_config(
            dir,
            &[(
                CLUSTER,
                vec![
                    ("x-b1", "b1.val", format!("{B_ONE}\r\n").as_bytes()),
                    ("x-b2", "b2.val", format!("{B_TWO}\n").as_bytes()),
                ],
            )],
        )
    }

    /// A is still what the main process and every worker hold and probe with.
    fn assert_a_remains(rig: &mut Rig, a_health: &HealthCheckConfig) {
        assert_eq!(rig.main_health().as_ref(), Some(a_health));
        for worker in &rig.workers {
            assert_eq!(
                worker.held_values(),
                Some(vec![A_VALUE.as_bytes().to_vec()])
            );
            assert_eq!(
                names(
                    &worker.state.clusters[CLUSTER]
                        .health_check
                        .as_ref()
                        .unwrap()
                        .headers
                ),
                vec!["x-a"]
            );
        }
        assert_eq!(rig.hub.server.config.config_path, "", "old source config");
        let observable = rig.observable();
        for secret in [A_VALUE, B_ONE, B_TWO, "b1.val", "b2.val"] {
            assert!(!observable.contains(secret), "leaked {secret}");
        }
    }

    #[test]
    fn a_reload_whose_capability_is_not_acknowledged_keeps_a_and_the_next_command_progresses() {
        type Break = fn(&mut FakeWorker);
        let cases: [(&str, Break); 4] = [
            ("refusal", |worker| {
                worker.refuses = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
            }),
            ("wrong id", |worker| {
                worker.answers_wrong_version =
                    Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
            }),
            ("timeout", |worker| {
                worker.holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
            }),
            ("predates the protocol", |worker| {
                worker.predates_protocol = true
            }),
        ];
        for (label, break_worker) in cases {
            let mut rig = running_rig(2);
            install_a(&mut rig);
            let a_health = rig.main_health().expect("a confirmed health check");
            let prepares = [rig.workers[0].prepares_seen, rig.workers[1].prepares_seen];
            let dir = tempfile::tempdir().expect("a directory");
            let path = reload_b_config(dir.path());
            break_worker(&mut rig.workers[1]);
            rig.reload(&path);
            rig.pump();
            if label == "timeout" || label == "predates the protocol" {
                assert_eq!(
                    rig.terminals(),
                    vec![ResponseStatus::Ok],
                    "{label}: waiting"
                );
                rig.expire();
            }
            assert_eq!(
                rig.terminals(),
                vec![ResponseStatus::Ok, ResponseStatus::Failure],
                "{label}: exactly one terminal answer"
            );
            assert_a_remains(&mut rig, &a_health);
            assert_eq!(rig.workers[0].prepares_seen, prepares[0], "{label}");
            assert_eq!(rig.workers[1].prepares_seen, prepares[1], "{label}");
            assert!(!rig.hub.server.protected.is_active(CLUSTER), "{label}");
            assert!(
                rig.hub
                    .server
                    .protected
                    .held_values(CLUSTER)
                    .is_some_and(|values| values == vec![A_VALUE.as_bytes().to_vec()]),
                "{label}: the journal drops the proposal of B"
            );

            // The next valid command is not blocked by the failed reload.
            rig.workers[1].refuses = None;
            rig.workers[1].holds = None;
            rig.workers[1].answers_wrong_version = None;
            rig.workers[1].predates_protocol = false;
            rig.begin(order(&[("x-c", "SYNTHETIC-RELOAD-C-77d1")]));
            rig.pump();
            assert_eq!(
                rig.terminals().last(),
                Some(&ResponseStatus::Ok),
                "{label}: the next command progresses"
            );
        }
    }

    #[test]
    fn a_reload_that_cannot_read_its_values_applies_nothing_at_all() {
        let mut rig = running_rig(2);
        install_a(&mut rig);
        let a_health = rig.main_health().expect("a confirmed health check");
        let dir = tempfile::tempdir().expect("a directory");
        let path = reload_b_config(dir.path());
        std::fs::remove_file(dir.path().join("b2.val")).expect("remove a value file");
        let seen = [rig.workers[0].requests_seen, rig.workers[1].requests_seen];
        rig.reload(&path);
        rig.pump();
        assert_eq!(
            rig.terminals(),
            vec![ResponseStatus::Ok, ResponseStatus::Failure]
        );
        assert_a_remains(&mut rig, &a_health);
        assert_eq!(
            [rig.workers[0].requests_seen, rig.workers[1].requests_seen],
            seen,
            "not even a generic order was sent"
        );
    }

    #[test]
    fn a_pre_commit_refusal_of_b_aborts_it_and_keeps_a() {
        for phase in [
            ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare,
            ProtectedHealthCheckPhase::ProtectedHealthCheckCommit,
        ] {
            let mut rig = running_rig(2);
            install_a(&mut rig);
            let a_health = rig.main_health().expect("a confirmed health check");
            let dir = tempfile::tempdir().expect("a directory");
            let path = reload_b_config(dir.path());
            rig.workers[1].refuses = Some(phase);
            rig.reload(&path);
            rig.pump();
            assert_eq!(
                rig.terminals(),
                vec![ResponseStatus::Ok, ResponseStatus::Failure],
                "{phase:?}"
            );
            assert_a_remains(&mut rig, &a_health);
            assert!(
                rig.hub
                    .server
                    .protected
                    .held_values(CLUSTER)
                    .is_some_and(|values| values == vec![A_VALUE.as_bytes().to_vec()])
            );
        }
    }

    #[test]
    fn a_lost_release_after_the_commit_never_claims_a_rollback() {
        let mut rig = running_rig(2);
        install_a(&mut rig);
        let dir = tempfile::tempdir().expect("a directory");
        let path = reload_b_config(dir.path());
        rig.workers[1].refuses = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckRelease);
        rig.reload(&path);
        rig.pump();
        assert_eq!(
            rig.terminals(),
            vec![ResponseStatus::Ok, ResponseStatus::Ok],
            "B is confirmed whatever the release did"
        );
        let b_values = vec![B_ONE.as_bytes().to_vec(), B_TWO.as_bytes().to_vec()];
        assert_eq!(names(&rig.main_headers()), vec!["x-b1", "x-b2"]);
        assert_eq!(
            rig.hub.server.protected.held_values(CLUSTER),
            Some(b_values.clone())
        );
        for worker in &rig.workers {
            assert_eq!(worker.held_values(), Some(b_values.clone()));
            assert_eq!(worker.sequence.last(), Some(&"Release"));
        }
        assert_eq!(rig.hub.server.config.config_path, path);
        let answers = format!("{:?}", rig.responses());
        assert!(!answers.contains("rolled back"), "{answers}");
    }

    #[test]
    fn several_clusters_change_one_after_the_other_in_lexical_order() {
        let mut rig = running_rig(2);
        send(&mut rig, add_beta());
        for worker in &mut rig.workers {
            worker
                .state
                .dispatch(&add_beta().into())
                .expect("a worker cluster");
        }
        rig.pump();
        let dir = tempfile::tempdir().expect("a directory");
        let path = write_reload_config(
            dir.path(),
            &[
                (
                    "beta",
                    vec![("x-beta", "beta.val", b"SYNTHETIC-BETA-0c1e\n")],
                ),
                (
                    CLUSTER,
                    vec![("x-app", "app.val", b"SYNTHETIC-APP-9f02\r\n")],
                ),
            ],
        );
        let before = rig.terminals().len();
        rig.reload(&path);
        rig.pump();
        assert_eq!(rig.terminals().len(), before + 1, "one terminal answer");
        assert_eq!(rig.final_status(), Some(ResponseStatus::Ok));
        for worker in &rig.workers {
            let clusters: Vec<_> = worker
                .orders_seen
                .iter()
                .map(|(phase, cluster, _)| (*phase, cluster.as_str()))
                .collect();
            use ProtectedHealthCheckPhase::*;
            assert_eq!(
                clusters,
                vec![
                    (ProtectedHealthCheckPrepare, "app"),
                    (ProtectedHealthCheckCommit, "app"),
                    (ProtectedHealthCheckRelease, "app"),
                    (ProtectedHealthCheckPrepare, "beta"),
                    (ProtectedHealthCheckCommit, "beta"),
                    (ProtectedHealthCheckRelease, "beta"),
                ]
            );
            assert_eq!(
                worker.backends.protected_health_check_headers["beta"].headers()[0]
                    .value()
                    .expose_secret_bytes(),
                b"SYNTHETIC-BETA-0c1e"
            );
        }
        assert_eq!(rig.hub.server.config.config_path, path);
    }

    // ---- cold start of file-backed protected headers ----

    use crate::command::requests::READY_NOTIFICATIONS;

    const COLD_VALUE: &str = "Bearer SYNTHETIC-COLD-AUTH-7f3a";

    /// A rig as the main process finds it at startup: no cluster in the main
    /// state or in any worker, workers answering ordinary orders.
    fn cold_rig(worker_count: u32) -> Rig {
        let mut rig = running_rig(worker_count);
        rig.hub.server.state = ConfigState::new();
        rig.hub.server.config.worker_count = worker_count.try_into().expect("a worker count");
        for worker in &mut rig.workers {
            worker.state = ConfigState::new();
            worker.applies_add_cluster = true;
        }
        rig
    }

    /// The configuration of a cold start: `path`, with the rig's channel
    /// sizes and deadlines.
    fn cold_config(rig: &Rig, path: &str) -> Config {
        let mut config = Config::load_from_path(path).expect("a configuration");
        config.config_path = path.to_owned();
        config.command_buffer_size = rig.hub.server.config.command_buffer_size;
        config.max_command_buffer_size = rig.hub.server.config.max_command_buffer_size;
        config.worker_timeout = rig.hub.server.config.worker_timeout;
        config.worker_count = rig.hub.server.config.worker_count;
        config
    }

    impl Rig {
        /// `load_static_config` as the cold start runs it.
        fn cold_load(&mut self, path: &str) {
            self.hub.server.config = cold_config(self, path);
            load_static_config(&mut self.hub.server, None, None);
            self.hub.arm_cold_ready();
        }

        /// The event loop's sweeps until nothing moves: exchange, then the
        /// cold READY gate.
        fn cold_pump(&mut self) {
            for _ in 0..12 {
                self.pump();
                self.hub.announce_cold_ready();
            }
        }
    }

    fn cold_ready_count() -> usize {
        READY_NOTIFICATIONS.with(|count| count.get())
    }

    fn cold_auth_config(dir: &std::path::Path) -> String {
        write_reload_config(
            dir,
            &[(
                CLUSTER,
                vec![("Authorization", "auth.val", COLD_VALUE.as_bytes())],
            )],
        )
    }

    #[test]
    fn a_cold_value_file_reaches_the_workers_only_through_the_protected_transaction() {
        let mut rig = cold_rig(2);
        let dir = tempfile::tempdir().expect("a directory");
        let path = cold_auth_config(dir.path());
        let ready = cold_ready_count();

        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
        rig.cold_load(&path);
        rig.cold_pump();

        for worker in &rig.workers {
            assert_eq!(
                worker.add_cluster_health,
                vec![None],
                "the generic AddCluster carries no probe"
            );
        }
        assert_eq!(cold_ready_count(), ready, "READY waits for the commit");

        rig.workers[1].release_held();
        rig.cold_pump();

        assert_eq!(cold_ready_count(), ready + 1, "READY exactly once");
        rig.cold_pump();
        assert_eq!(cold_ready_count(), ready + 1);
        assert_eq!(names(&rig.main_headers()), vec!["Authorization"]);
        let held = Some(vec![COLD_VALUE.as_bytes().to_vec()]);
        assert_eq!(rig.hub.server.protected.held_values(CLUSTER), held);
        for worker in &rig.workers {
            assert_eq!(
                worker.sequence,
                vec!["AddCluster", "Query", "Prepare", "Commit", "Release"]
            );
            assert_eq!(worker.held_values(), held);
            assert_eq!(
                names(
                    &worker.state.clusters[CLUSTER]
                        .health_check
                        .as_ref()
                        .unwrap()
                        .headers
                ),
                vec!["Authorization"]
            );
        }
        let observable = format!("{}{:?}", rig.observable(), rig.hub.server.config);
        for secret in [COLD_VALUE, "auth.val"] {
            assert!(!observable.contains(secret), "leaked {secret}");
        }
    }

    /// A fingerprint some other main process issued: it verifies against no
    /// key of this one.
    const FOREIGN_FINGERPRINT: &str = "sozu-hc-hmac-sha256:v1:00112233445566778899aabbccddeeff:\
         0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn foreign_health() -> HealthCheckConfig {
        HealthCheckConfig {
            uri: "/".to_owned(),
            interval: 10,
            timeout: 5,
            healthy_threshold: 3,
            unhealthy_threshold: 3,
            headers: vec![HealthCheckHeader {
                name: "Authorization".to_owned(),
                fingerprint: FOREIGN_FINGERPRINT.to_owned(),
            }],
            ..Default::default()
        }
    }

    /// A state file, framed as `ConfigState::write_requests_to_file` does.
    fn write_saved_state(dir: &std::path::Path, entries: Vec<RequestType>) -> String {
        use std::io::Write as _;

        let path = dir.join("saved.state");
        let mut file = std::fs::File::create(&path).expect("a state file");
        for (counter, entry) in entries.into_iter().enumerate() {
            let message = sozu_command_lib::proto::command::WorkerRequest::new(
                format!("SAVE-{counter}"),
                entry.into(),
            );
            let serialized = serde_json::to_string(&message).expect("a serializable request");
            file.write_all(serialized.as_bytes()).expect("an entry");
            file.write_all(b"\n\0").expect("a delimiter");
        }
        path.to_str().expect("a utf-8 path").to_owned()
    }

    impl Rig {
        fn cold_load_state(&mut self, path: &str) {
            crate::command::requests::load_state(&mut self.hub.server, None, path);
        }

        /// The protected verbs a worker saw, in order.
        fn protected_sequence(&self, worker: usize) -> Vec<&'static str> {
            self.workers[worker]
                .sequence
                .iter()
                .copied()
                .filter(|kind| !matches!(*kind, "AddCluster" | "Other"))
                .collect()
        }
    }

    /// The saved state decides the cluster, whichever of the four verbs it
    /// used: the configuration file's value is never read by a worker, never
    /// matched by name, and the cold start still becomes ready.
    #[test]
    fn a_saved_entry_for_the_cluster_wins_over_the_configuration_value_file() {
        type Entries = fn() -> Vec<RequestType>;
        let cases: [(&str, Entries, Option<&str>); 4] = [
            (
                "AddCluster",
                || {
                    vec![RequestType::AddCluster(Cluster {
                        cluster_id: CLUSTER.to_owned(),
                        health_check: Some(foreign_health()),
                        ..Default::default()
                    })]
                },
                Some("Authorization"),
            ),
            (
                "SetHealthCheck",
                || {
                    vec![RequestType::SetHealthCheck(SetHealthCheck {
                        cluster_id: CLUSTER.to_owned(),
                        config: foreign_health(),
                    })]
                },
                Some("Authorization"),
            ),
            (
                "RemoveHealthCheck",
                || {
                    vec![
                        RequestType::SetHealthCheck(SetHealthCheck {
                            cluster_id: CLUSTER.to_owned(),
                            config: foreign_health(),
                        }),
                        RequestType::RemoveHealthCheck(CLUSTER.to_owned()),
                    ]
                },
                None,
            ),
            (
                "RemoveCluster",
                || vec![RequestType::RemoveCluster(CLUSTER.to_owned())],
                None,
            ),
        ];
        for (verb, entries, expected) in cases {
            let mut rig = cold_rig(2);
            let dir = tempfile::tempdir().expect("a directory");
            let path = cold_auth_config(dir.path());
            let saved = write_saved_state(dir.path(), entries());
            let ready = cold_ready_count();

            rig.cold_load(&path);
            rig.cold_load_state(&saved);
            rig.cold_pump();

            assert_eq!(cold_ready_count(), ready + 1, "{verb}: READY once");
            let removed = verb == "RemoveCluster";
            assert_eq!(
                rig.hub.server.state.clusters.contains_key(CLUSTER),
                !removed,
                "{verb}"
            );
            if !removed {
                assert_eq!(
                    rig.hub.server.state.clusters[CLUSTER]
                        .health_check
                        .as_ref()
                        .map(|config| names(&config.headers)),
                    expected.map(|name| vec![name.to_owned()]),
                    "{verb}: the saved metadata stays"
                );
                if expected.is_some() {
                    assert_eq!(
                        rig.main_headers()[0].fingerprint,
                        FOREIGN_FINGERPRINT,
                        "{verb}: a foreign fingerprint is kept as it is"
                    );
                }
            }
            assert!(
                rig.hub.server.protected.held_values(CLUSTER).is_none(),
                "{verb}: no value in the journal"
            );
            for index in 0..2 {
                assert_eq!(
                    rig.protected_sequence(index),
                    Vec::<&str>::new(),
                    "{verb}: no Query, Prepare or value for worker {index}"
                );
                assert!(rig.workers[index].held_values().is_none(), "{verb}");
            }
            let observable = format!("{}{:?}", rig.observable(), rig.hub.server.config);
            for secret in [COLD_VALUE, "auth.val"] {
                assert!(!observable.contains(secret), "{verb}: leaked {secret}");
            }
        }
    }

    /// The saved state names only the clusters it names: the other file-backed
    /// clusters follow in lexical order.
    #[test]
    fn a_saved_remove_drops_only_its_own_cluster_and_the_others_follow_in_lexical_order() {
        let mut rig = cold_rig(1);
        let dir = tempfile::tempdir().expect("a directory");
        let path = write_reload_config(
            dir.path(),
            &[
                ("zeta", vec![("x-z", "z.val", b"SYNTHETIC-COLD-Z-91")]),
                (
                    CLUSTER,
                    vec![("Authorization", "auth.val", COLD_VALUE.as_bytes())],
                ),
                ("beta", vec![("x-b", "b.val", b"SYNTHETIC-COLD-B-17")]),
            ],
        );
        let saved = write_saved_state(
            dir.path(),
            vec![RequestType::RemoveCluster(CLUSTER.to_owned())],
        );
        let ready = cold_ready_count();

        rig.cold_load(&path);
        rig.cold_load_state(&saved);
        rig.cold_pump();

        assert_eq!(cold_ready_count(), ready + 1);
        assert!(!rig.hub.server.state.clusters.contains_key(CLUSTER));
        assert_eq!(
            rig.workers[0].orders_seen,
            [
                ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare,
                ProtectedHealthCheckPhase::ProtectedHealthCheckCommit,
                ProtectedHealthCheckPhase::ProtectedHealthCheckRelease,
            ]
            .into_iter()
            .map(|phase| (phase, "beta".to_owned(), 1))
            .chain(
                [
                    ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare,
                    ProtectedHealthCheckPhase::ProtectedHealthCheckCommit,
                    ProtectedHealthCheckPhase::ProtectedHealthCheckRelease,
                ]
                .into_iter()
                .map(|phase| (phase, "zeta".to_owned(), 2))
            )
            .collect::<Vec<_>>(),
            "beta then zeta, never the removed cluster"
        );
        let observable = format!("{}{:?}", rig.observable(), rig.hub.server.config);
        for secret in [COLD_VALUE, "SYNTHETIC-COLD-Z-91", "SYNTHETIC-COLD-B-17"] {
            assert!(!observable.contains(secret), "leaked {secret}");
        }
    }

    /// Everything a refused direct order must leave exactly as it found it.
    /// The held values are compared with `==` and never formatted.
    #[allow(clippy::type_complexity)]
    fn effect_view(
        rig: &Rig,
    ) -> (
        String,
        String,
        Vec<usize>,
        Vec<Option<Vec<Vec<u8>>>>,
        Vec<String>,
    ) {
        (
            format!(
                "{}{:?}{:?}",
                state_view(&rig.hub.server.state),
                rig.hub.server.peek_next_task_id(),
                rig.hub.server.state.request_counts
            ),
            format!("{:?}", rig.hub.server.protected),
            rig.workers.iter().map(|w| w.requests_seen).collect(),
            ["alpha", "zeta"]
                .iter()
                .map(|cluster| rig.hub.server.protected.held_values(cluster))
                .collect(),
            rig.workers.iter().map(|w| state_view(&w.state)).collect(),
        )
    }

    /// A cold intent reserves its cluster from the moment it is held until the
    /// whole cold sequence is decided, whether or not it has entered `active`:
    /// no direct order may rewrite that cluster, and an unrelated cluster is
    /// still served.
    #[test]
    fn a_held_cold_intent_reserves_its_cluster_against_direct_orders() {
        for running in [false, true] {
            let phase = if running { "running" } else { "pending" };
            let mut rig = cold_rig(1);
            let dir = tempfile::tempdir().expect("a directory");
            let path = write_reload_config(
                dir.path(),
                &[
                    ("alpha", vec![("x-a", "a.val", b"SYNTHETIC-COLD-A-31")]),
                    ("zeta", vec![("x-z", "z.val", b"SYNTHETIC-COLD-Z-32")]),
                ],
            );
            if running {
                rig.workers[0].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit);
            }
            rig.cold_load(&path);
            if running {
                rig.cold_pump();
                assert!(rig.hub.server.protected.is_active("alpha"), "{phase}");
                assert!(!rig.hub.server.protected.is_active("zeta"), "{phase}");
            } else {
                assert!(!rig.hub.server.protected.cold_start_settled(), "{phase}");
                assert!(!rig.hub.server.protected.is_active("alpha"), "{phase}");
            }

            for cluster in ["alpha", "zeta"] {
                let requests = [
                    RequestType::AddCluster(Cluster {
                        cluster_id: cluster.to_owned(),
                        ..Default::default()
                    }),
                    RequestType::SetHealthCheck(SetHealthCheck {
                        cluster_id: cluster.to_owned(),
                        config: HealthCheckConfig::default(),
                    }),
                    RequestType::RemoveHealthCheck(cluster.to_owned()),
                    RequestType::RemoveCluster(cluster.to_owned()),
                    RequestType::AddUnixBackend(AddBackend {
                        cluster_id: cluster.to_owned(),
                        backend_id: "unix-1".to_owned(),
                        unix_path: Some("/tmp/synthetic-cold.sock".to_owned()),
                        ..Default::default()
                    }),
                ];
                for request in requests {
                    let before = effect_view(&rig);
                    {
                        let (server, client) =
                            rig.hub.get_client_mut(&rig.client).expect("the client");
                        server.handle_client_request(client, request.to_owned().into());
                    }
                    assert_eq!(
                        final_message(&mut rig).as_deref(),
                        Some(BUSY),
                        "{phase}: {request:?} on {cluster} must be refused"
                    );
                    assert!(
                        effect_view(&rig) == before,
                        "{phase}: {request:?} on {cluster} changed something"
                    );
                }
            }

            // An unrelated cluster and the read-only verbs are not behind it.
            let answers = rig.responses().len();
            {
                let (server, client) = rig.hub.get_client_mut(&rig.client).expect("the client");
                server.handle_client_request(
                    client,
                    RequestType::AddCluster(Cluster {
                        cluster_id: "free".to_owned(),
                        ..Default::default()
                    })
                    .into(),
                );
            }
            assert!(
                rig.responses()
                    .iter()
                    .skip(answers)
                    .all(|response| response.message != BUSY),
                "{phase}: an unrelated cluster was refused"
            );
            for request in [
                RequestType::RemoveCluster("free".to_owned()),
                RequestType::AddCluster(Cluster {
                    cluster_id: "free".to_owned(),
                    ..Default::default()
                }),
                RequestType::QueryHealthChecks(Default::default()),
            ] {
                assert!(
                    !rig.hub.server.protected.blocks(&request),
                    "{phase}: {request:?}"
                );
            }

            // The sequence settling lifts the reservation.
            rig.workers[0].release_held();
            rig.cold_pump();
            assert!(rig.hub.server.protected.cold_start_settled(), "{phase}");
            for cluster in ["alpha", "zeta"] {
                assert!(
                    !rig.hub
                        .server
                        .protected
                        .blocks(&RequestType::RemoveCluster(cluster.to_owned())),
                    "{phase}: {cluster} is free once settled"
                );
            }
            let observable = format!("{}{:?}", rig.observable(), rig.hub.server.config);
            for secret in ["SYNTHETIC-COLD-A-31", "SYNTHETIC-COLD-Z-32"] {
                assert!(!observable.contains(secret), "{phase}: leaked {secret}");
            }
        }
    }

    // ---- live replay of a saved state over confirmed values ----

    /// Replay `entries` as `LoadState` does, over the rig's confirmed A.
    fn replay(rig: &mut Rig, entries: Vec<RequestType>) {
        let dir = tempfile::tempdir().expect("a directory");
        let path = write_saved_state(dir.path(), entries);
        crate::command::requests::load_state(&mut rig.hub.server, None, &path);
        rig.pump();
    }

    /// Whether the journal still holds exactly A. `==` and never a formatted
    /// comparison: a failure must not print the value.
    fn holds_a(rig: &Rig) -> bool {
        rig.hub.server.protected.held_values(CLUSTER) == Some(vec![A_VALUE.as_bytes().to_vec()])
    }

    /// The journal and the main state name the same headers for the cluster.
    fn journal_matches_main(rig: &Rig) -> bool {
        match rig.hub.server.protected.held_set(CLUSTER) {
            Some(held) => held.satisfies(&rig.main_headers()) && !rig.main_headers().is_empty(),
            None => true,
        }
    }

    fn a_rig() -> Rig {
        let mut rig = running_rig(1);
        install_a(&mut rig);
        assert!(holds_a(&rig));
        rig
    }

    #[test]
    fn a_replayed_entry_naming_the_exact_headers_keeps_the_held_values() {
        let mut rig = a_rig();
        let health = rig.main_health().expect("a confirmed health check");
        replay(
            &mut rig,
            vec![
                RequestType::SetHealthCheck(SetHealthCheck {
                    cluster_id: CLUSTER.to_owned(),
                    config: health.to_owned(),
                }),
                RequestType::AddCluster(Cluster {
                    cluster_id: CLUSTER.to_owned(),
                    health_check: Some(health.to_owned()),
                    ..Default::default()
                }),
            ],
        );
        assert!(holds_a(&rig));
        assert_eq!(rig.main_headers(), health.headers);
        assert!(journal_matches_main(&rig));
    }

    #[test]
    fn a_replayed_entry_that_rewrites_the_health_check_drops_the_held_values() {
        let health_with = |headers: Vec<HealthCheckHeader>| HealthCheckConfig {
            uri: "/".to_owned(),
            interval: 10,
            timeout: 5,
            healthy_threshold: 3,
            unhealthy_threshold: 3,
            headers,
            ..Default::default()
        };
        let b = foreign_health().headers;
        let cases: Vec<(&str, RequestType, Option<Vec<String>>)> = vec![
            (
                "SetHealthCheck with other headers",
                RequestType::SetHealthCheck(SetHealthCheck {
                    cluster_id: CLUSTER.to_owned(),
                    config: health_with(b.to_owned()),
                }),
                Some(vec!["Authorization".to_owned()]),
            ),
            (
                "SetHealthCheck without headers",
                RequestType::SetHealthCheck(SetHealthCheck {
                    cluster_id: CLUSTER.to_owned(),
                    config: health_with(Vec::new()),
                }),
                Some(Vec::new()),
            ),
            (
                "RemoveHealthCheck",
                RequestType::RemoveHealthCheck(CLUSTER.to_owned()),
                Some(Vec::new()),
            ),
            (
                "AddCluster with other headers",
                RequestType::AddCluster(Cluster {
                    cluster_id: CLUSTER.to_owned(),
                    health_check: Some(health_with(b.to_owned())),
                    ..Default::default()
                }),
                Some(vec!["Authorization".to_owned()]),
            ),
            (
                "AddCluster without a health check",
                RequestType::AddCluster(Cluster {
                    cluster_id: CLUSTER.to_owned(),
                    ..Default::default()
                }),
                Some(Vec::new()),
            ),
            (
                "RemoveCluster",
                RequestType::RemoveCluster(CLUSTER.to_owned()),
                None,
            ),
        ];
        for (what, entry, expected) in cases {
            let mut rig = a_rig();
            replay(&mut rig, vec![entry]);
            assert!(
                rig.hub.server.protected.held_values(CLUSTER).is_none(),
                "{what}: the journal still holds a value"
            );
            match expected {
                Some(names_after) => assert_eq!(names(&rig.main_headers()), names_after, "{what}"),
                None => assert!(
                    !rig.hub.server.state.clusters.contains_key(CLUSTER),
                    "{what}"
                ),
            }
            assert!(journal_matches_main(&rig), "{what}");
            let observable = format!("{}{:?}", rig.observable(), rig.hub.server.config);
            assert!(!observable.contains(A_VALUE), "{what}: leaked");
        }
    }

    #[test]
    fn a_replayed_backend_or_frontend_leaves_the_held_values_alone() {
        let mut rig = a_rig();
        let address: std::net::SocketAddr = "127.0.0.1:18080".parse().expect("an address");
        replay(
            &mut rig,
            vec![
                RequestType::AddBackend(AddBackend {
                    cluster_id: CLUSTER.to_owned(),
                    backend_id: "b-1".to_owned(),
                    address: address.into(),
                    ..Default::default()
                }),
                RequestType::AddHttpFrontend(
                    sozu_command_lib::proto::command::RequestHttpFrontend {
                        cluster_id: Some(CLUSTER.to_owned()),
                        address: address.into(),
                        hostname: "synthetic.example".to_owned(),
                        path: sozu_command_lib::proto::command::PathRule::prefix(""),
                        ..Default::default()
                    },
                ),
            ],
        );
        assert!(
            !rig.hub.server.state.backends.is_empty(),
            "the backend landed"
        );
        assert!(
            !rig.hub.server.state.http_fronts.is_empty(),
            "the frontend landed"
        );
        assert!(holds_a(&rig));
        assert_eq!(names(&rig.main_headers()), vec!["x-a"]);
        assert!(journal_matches_main(&rig));
    }

    #[test]
    fn a_replayed_entry_the_state_refuses_changes_neither_state_nor_journal() {
        let mut rig = a_rig();
        let state = state_view(&rig.hub.server.state);
        let seen = rig.workers[0].requests_seen;
        replay(
            &mut rig,
            vec![
                // Zero thresholds and no leading `/`: refused by the state.
                RequestType::SetHealthCheck(SetHealthCheck {
                    cluster_id: CLUSTER.to_owned(),
                    config: HealthCheckConfig::default(),
                }),
                RequestType::RemoveHealthCheck("absent".to_owned()),
            ],
        );
        assert_eq!(state_view(&rig.hub.server.state), state);
        assert_eq!(rig.workers[0].requests_seen, seen, "nothing was scattered");
        assert!(holds_a(&rig));
        assert!(journal_matches_main(&rig));
    }

    /// The only inverses the revert path owns are those of listeners and
    /// frontends: none touches a health check, so the journal stays on A while
    /// the main state loses the frontend no worker acknowledged.
    #[test]
    fn a_reverted_replay_entry_leaves_the_journal_on_the_final_main_state() {
        let mut rig = a_rig();
        rig.workers[0].refuses_ordinary = true;
        let address: std::net::SocketAddr = "127.0.0.1:18081".parse().expect("an address");
        let dir = tempfile::tempdir().expect("a directory");
        let path = write_saved_state(
            dir.path(),
            vec![RequestType::AddHttpFrontend(
                sozu_command_lib::proto::command::RequestHttpFrontend {
                    cluster_id: Some(CLUSTER.to_owned()),
                    address: address.into(),
                    hostname: "synthetic.example".to_owned(),
                    path: sozu_command_lib::proto::command::PathRule::prefix(""),
                    ..Default::default()
                },
            )],
        );
        crate::command::requests::load_state(&mut rig.hub.server, None, &path);
        assert!(!rig.hub.server.state.http_fronts.is_empty(), "dispatched");
        rig.pump();
        assert!(
            rig.hub.server.state.http_fronts.is_empty(),
            "the unacknowledged frontend was reverted"
        );
        assert!(holds_a(&rig));
        assert_eq!(names(&rig.main_headers()), vec!["x-a"]);
        assert!(journal_matches_main(&rig));
    }

    /// Whatever goes wrong with a file-backed header at startup, no probe is
    /// confirmed, no clear byte is shown, the rest of the configuration is
    /// what a headerless start gives, and READY is never announced.
    #[test]
    fn a_cold_start_that_cannot_apply_its_values_never_becomes_ready() {
        type Break = fn(&mut Rig);
        let cases: [(&str, Break); 6] = [
            ("missing file", |rig| {
                let _ = rig;
            }),
            ("capability refusal", |rig| {
                rig.workers[1].refuses =
                    Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
            }),
            ("wrong id", |rig| {
                rig.workers[1].answers_wrong_version =
                    Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
            }),
            ("capability timeout", |rig| {
                rig.workers[1].holds =
                    Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability)
            }),
            ("prepare refusal", |rig| {
                rig.workers[1].refuses =
                    Some(ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare)
            }),
            ("commit timeout", |rig| {
                rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCommit)
            }),
        ];
        for (what, breakage) in cases {
            let mut rig = cold_rig(2);
            let dir = tempfile::tempdir().expect("a directory");
            let path = write_reload_config(
                dir.path(),
                &[
                    (
                        CLUSTER,
                        vec![("Authorization", "auth.val", COLD_VALUE.as_bytes())],
                    ),
                    ("plain", Vec::new()),
                ],
            );
            if what == "missing file" {
                std::fs::remove_file(dir.path().join("auth.val")).expect("a value file");
            }
            let ready = cold_ready_count();
            breakage(&mut rig);

            rig.cold_load(&path);
            rig.cold_pump();
            rig.expire();
            rig.cold_pump();

            assert_eq!(cold_ready_count(), ready, "{what}: no READY");
            assert!(!rig.hub.server.protected.cold_start_settled(), "{what}");
            assert!(
                rig.hub.server.protected.held_values(CLUSTER).is_none(),
                "{what}: no confirmed value"
            );
            if what != "commit timeout" {
                assert!(rig.main_headers().is_empty(), "{what}: no confirmed B");
            }
            assert!(
                rig.hub.server.state.clusters["plain"]
                    .health_check
                    .is_some(),
                "{what}: the headerless cluster is what a headerless start gives"
            );
            for worker in &rig.workers {
                assert_eq!(
                    worker.add_cluster_health.iter().flatten().count(),
                    1,
                    "{what}: only the headerless cluster carries a probe"
                );
            }
            if what == "missing file" {
                for worker in &rig.workers {
                    assert_eq!(worker.sequence, vec!["AddCluster", "AddCluster"]);
                }
            } else {
                assert!(
                    rig.workers[0].sequence.contains(&"Query"),
                    "{what}: the protected sequence did start"
                );
            }
            let observable = format!("{}{:?}", rig.observable(), rig.hub.server.config);
            for secret in [COLD_VALUE, "auth.val"] {
                assert!(!observable.contains(secret), "{what}: leaked {secret}");
            }
        }
    }

    #[test]
    fn a_headerless_cold_start_is_settled_and_ready_once() {
        let mut rig = cold_rig(2);
        let dir = tempfile::tempdir().expect("a directory");
        let path = write_reload_config(dir.path(), &[("plain", Vec::new())]);
        let ready = cold_ready_count();

        rig.cold_load(&path);
        assert!(rig.hub.server.protected.cold_start_settled());
        rig.cold_pump();
        rig.cold_pump();

        assert_eq!(cold_ready_count(), ready + 1);
        for worker in &rig.workers {
            assert_eq!(worker.sequence, vec!["AddCluster"]);
        }
        assert!(
            rig.hub
                .server
                .protected
                .is_snapshot_safe(&rig.hub.server.state)
        );
    }

    fn ready_count() -> usize {
        crate::command::requests::READY_NOTIFICATIONS.with(|count| count.get())
    }

    #[test]
    fn systemd_ready_follows_the_decision_of_a_reload_and_nothing_else() {
        // A reload that fails before any task exists closes its window at once.
        let mut rig = running_rig(2);
        install_a(&mut rig);
        let start = ready_count();
        send(
            &mut rig,
            RequestType::ReloadConfiguration("/nonexistent/reload.toml".to_owned()),
        );
        assert_eq!(ready_count(), start + 1, "an immediate failure is terminal");

        // A reload with protected changes closes it once, when they are decided.
        let dir = tempfile::tempdir().expect("a directory");
        let path = reload_b_config(dir.path());
        let start = ready_count();
        rig.workers[1].holds = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckCapability);
        send(&mut rig, RequestType::ReloadConfiguration(path.to_owned()));
        assert_eq!(ready_count(), start, "dispatching is not a terminal state");
        rig.pump();
        assert_eq!(
            ready_count(),
            start,
            "the generic part alone is not terminal"
        );
        rig.workers[1].release_held();
        rig.pump();
        assert_eq!(ready_count(), start + 1, "the success is terminal, once");
        rig.pump();
        assert_eq!(ready_count(), start + 1);

        // The same reload failing in the protected phase is terminal once too.
        let start = ready_count();
        let mut rig = running_rig(2);
        install_a(&mut rig);
        rig.workers[1].refuses = Some(ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare);
        send(&mut rig, RequestType::ReloadConfiguration(path));
        rig.pump();
        assert_eq!(ready_count(), start + 1);

        // Another mutating verb keeps its immediate READY.
        let start = ready_count();
        send(&mut rig, add_beta());
        assert_eq!(ready_count(), start + 1);
    }
}
