use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use mio::Token;
use nix::{
    sys::{
        signal::{Signal, kill},
        wait::waitpid,
    },
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use sozu_command_lib::{
    config::Config,
    proto::command::{
        Event, ResponseStatus, ReturnListenSockets, RunState, SoftStop, WorkerResponse,
        request::RequestType,
    },
    sd_notify,
    state::ConfigState,
};

use crate::{
    command::{
        requests::{AuditExtras, AuditResult, audit_emit_inline},
        server::{
            ClientId, CommandHub, Gatherer, GatheringTask, MessageClient, RequestId, Server,
            ServerState, SessionId, TaskContainerSnapshot, TaskId, TaskSnapshot, TaskSnapshotError,
            TaskSnapshotTiming, Timeout, WorkerId,
        },
        sessions::{ClientSession, ClientSessionSnapshot, OptionalClient, WorkerSessionSnapshot},
    },
    upgrade::{fork_main_into_new_main, probe_main_upgrade_candidate},
};
use sozu_command_lib::proto::command::EventKind;

#[derive(Debug)]
enum UpgradeWorkerProgress {
    /// 1. request listeners from the old worker
    /// 2. store listeners to pass them to new worker,
    RequestingListenSockets {
        old_worker_token: Token,
        old_worker_id: WorkerId,
    },
    /// 3. soft stop the old worker
    /// 4. activate the listeners of the new worker
    StopOldActivateNew {
        old_worker_id: WorkerId,
        new_worker_id: WorkerId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum UpgradeWorkerProgressSnapshot {
    RequestingListenSockets {
        old_worker_token: usize,
        old_worker_id: WorkerId,
    },
    StopOldActivateNew {
        old_worker_id: WorkerId,
        new_worker_id: WorkerId,
    },
}

/// Serializable continuation of the two-step worker-upgrade task.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct UpgradeWorkerTaskSnapshot {
    client_token: usize,
    progress: UpgradeWorkerProgressSnapshot,
    ok: usize,
    errors: usize,
    responses: Vec<(WorkerId, WorkerResponse)>,
    expected_responses: usize,
}

impl UpgradeWorkerTaskSnapshot {
    pub(crate) fn restore(self) -> Box<dyn GatheringTask> {
        let progress = match self.progress {
            UpgradeWorkerProgressSnapshot::RequestingListenSockets {
                old_worker_token,
                old_worker_id,
            } => UpgradeWorkerProgress::RequestingListenSockets {
                old_worker_token: Token(old_worker_token),
                old_worker_id,
            },
            UpgradeWorkerProgressSnapshot::StopOldActivateNew {
                old_worker_id,
                new_worker_id,
            } => UpgradeWorkerProgress::StopOldActivateNew {
                old_worker_id,
                new_worker_id,
            },
        };
        Box::new(UpgradeWorkerTask {
            client_token: Token(self.client_token),
            progress,
            ok: self.ok,
            errors: self.errors,
            responses: self.responses,
            expected_responses: self.expected_responses,
        })
    }
}

#[derive(Debug)]
struct UpgradeWorkerTask {
    pub client_token: Token,
    progress: UpgradeWorkerProgress,

    ok: usize,
    errors: usize,
    responses: Vec<(WorkerId, WorkerResponse)>,
    expected_responses: usize,
}

pub fn upgrade_worker(server: &mut Server, client: &mut ClientSession, old_worker_id: WorkerId) {
    info!(
        "client[{:?}] msg wants to upgrade worker {}",
        client.token, old_worker_id
    );

    audit_emit_inline(
        server,
        client,
        EventKind::WorkerUpgraded,
        "worker_upgraded",
        "config.worker_upgraded",
        format!("worker:{old_worker_id}"),
        AuditResult::Ok,
        AuditExtras::default(),
    );

    let old_worker_token = match server.get_active_worker_by_id(old_worker_id) {
        Some(session) => session.token,
        None => {
            client.finish_failure(format!(
                "Worker {old_worker_id} does not exist, or is stopping / stopped"
            ));
            return;
        }
    };

    client.return_processing(format!(
        "Requesting listen sockets from worker {old_worker_id}"
    ));
    server.scatter(
        RequestType::ReturnListenSockets(ReturnListenSockets {}).into(),
        Box::new(UpgradeWorkerTask {
            client_token: client.token,
            progress: UpgradeWorkerProgress::RequestingListenSockets {
                old_worker_token,
                old_worker_id,
            },
            ok: 0,
            errors: 0,
            responses: Vec::new(),
            expected_responses: 0,
        }),
        Timeout::Default,
        Some(old_worker_id),
    );
}

impl UpgradeWorkerTask {
    fn receive_listen_sockets(
        self,
        server: &mut Server,
        client: &mut OptionalClient,
        old_worker_token: Token,
        old_worker_id: WorkerId,
    ) {
        let old_worker = match server.workers.get_mut(&old_worker_token) {
            Some(old_worker) => old_worker,
            None => {
                client.finish_failure(format!("Worker {old_worker_id} died while upgrading, it should be restarted automatically"));
                return;
            }
        };
        let old_worker_id = old_worker.id;

        match old_worker.scm_socket.set_blocking(true) {
            Ok(_) => {}
            Err(error) => {
                client.finish_failure(format!("Could not set SCM sockets to blocking: {error:?}"));
                return;
            }
        }

        let listeners = match old_worker.scm_socket.receive_listeners() {
            Ok(listeners) => listeners,
            Err(_) => {
                client.finish_failure(
                    "Could not upgrade worker: did not get back listeners from the old worker",
                );
                return;
            }
        };

        old_worker.run_state = RunState::Stopping;

        // lauch new worker
        let new_worker = match server.launch_new_worker(Some(listeners)) {
            Ok(worker) => worker,
            Err(worker_err) => {
                return client.finish_failure(format!("could not launch new worker: {worker_err}"));
            }
        };
        client.return_processing(format!("Launched a new worker with id {}", new_worker.id));
        let new_worker_id = new_worker.id;

        let finish_task = server.new_task(
            Box::new(UpgradeWorkerTask {
                client_token: self.client_token,
                progress: UpgradeWorkerProgress::StopOldActivateNew {
                    old_worker_id,
                    new_worker_id,
                },

                ok: 0,
                errors: 0,
                responses: Vec::new(),
                expected_responses: 0,
            }),
            Timeout::None,
        );

        // Stop the old worker
        client.return_processing(format!("Soft stopping worker with id {old_worker_id}"));
        server.scatter_on(
            RequestType::SoftStop(SoftStop {}).into(),
            finish_task,
            0,
            Some(old_worker_id),
        );

        // activate new worker
        for (count, request) in server
            .state
            .generate_activate_requests()
            .into_iter()
            .enumerate()
        {
            server.scatter_on(request, finish_task, count + 1, Some(new_worker_id));
        }
    }
}

impl GatheringTask for UpgradeWorkerTask {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn super::server::Gatherer {
        self
    }

    fn snapshot(&self, _timing: TaskSnapshotTiming) -> Result<TaskSnapshot, TaskSnapshotError> {
        let progress = match self.progress {
            UpgradeWorkerProgress::RequestingListenSockets {
                old_worker_token,
                old_worker_id,
            } => UpgradeWorkerProgressSnapshot::RequestingListenSockets {
                old_worker_token: old_worker_token.0,
                old_worker_id,
            },
            UpgradeWorkerProgress::StopOldActivateNew {
                old_worker_id,
                new_worker_id,
            } => UpgradeWorkerProgressSnapshot::StopOldActivateNew {
                old_worker_id,
                new_worker_id,
            },
        };
        Ok(TaskSnapshot::UpgradeWorker(UpgradeWorkerTaskSnapshot {
            client_token: self.client_token.0,
            progress,
            ok: self.ok,
            errors: self.errors,
            responses: self.responses.clone(),
            expected_responses: self.expected_responses,
        }))
    }

    fn retained_worker_token(&self) -> Option<Token> {
        match self.progress {
            UpgradeWorkerProgress::RequestingListenSockets {
                old_worker_token, ..
            } => Some(old_worker_token),
            UpgradeWorkerProgress::StopOldActivateNew { .. } => None,
        }
    }

    fn on_finish(
        self: Box<Self>,
        server: &mut Server,
        client: &mut OptionalClient,
        _timed_out: bool,
    ) {
        match self.progress {
            UpgradeWorkerProgress::RequestingListenSockets {
                old_worker_token,
                old_worker_id,
            } => {
                if self.ok == 1 {
                    self.receive_listen_sockets(server, client, old_worker_token, old_worker_id);
                } else {
                    client.finish_failure(format!(
                        "Could not get listen sockets from old worker:{:?}",
                        self.responses
                    ));
                }
            }
            UpgradeWorkerProgress::StopOldActivateNew {
                old_worker_id,
                new_worker_id,
            } => {
                // A failure is attributed by the worker that produced it: a
                // rejected or unanswered `SoftStop` of the old worker
                // (`CommandHub::fail_in_flight_requests_of_worker` synthesizes
                // "worker N closed before answering" when it exits first), or
                // a rejected activation request of the new worker. A worker
                // that dies fails every request it owes with the same reason,
                // so identical reasons are reported once.
                let failure_reasons = |target: WorkerId| {
                    let mut reasons: Vec<&str> = Vec::new();
                    for (worker_id, response) in &self.responses {
                        if *worker_id == target
                            && ResponseStatus::try_from(response.status)
                                == Ok(ResponseStatus::Failure)
                            && !reasons.contains(&response.message.as_str())
                        {
                            reasons.push(response.message.as_str());
                        }
                    }
                    reasons
                };
                let old_failures = failure_reasons(old_worker_id);
                let new_failures = failure_reasons(new_worker_id);

                if old_failures.is_empty() && new_failures.is_empty() {
                    client.finish_ok(
                        format!(
                            "Upgrade successful:\n- finished soft stop of worker {old_worker_id:?}\n- finished activation of new worker {new_worker_id:?}"
                        )
                    );
                    return;
                }

                let mut message = format!("Upgrade of worker {old_worker_id} failed:");
                if old_failures.is_empty() {
                    message.push_str(&format!(
                        "\n- finished soft stop of old worker {old_worker_id}"
                    ));
                } else {
                    message.push_str(&format!(
                        "\n- old worker {old_worker_id} did not finish its soft stop: {}",
                        old_failures.join("; ")
                    ));
                }
                if new_failures.is_empty() {
                    message.push_str(&format!(
                        "\n- new worker {new_worker_id} is serving: its activation finished"
                    ));
                } else {
                    message.push_str(&format!(
                        "\n- new worker {new_worker_id} activation failed: {}",
                        new_failures.join("; ")
                    ));
                }
                client.finish_failure(message);
            }
        }
    }
}

impl Gatherer for UpgradeWorkerTask {
    fn inc_expected_responses(&mut self, count: usize) {
        self.expected_responses += count;
    }

    fn has_finished(&self) -> bool {
        self.ok + self.errors >= self.expected_responses
    }

    fn on_message(
        &mut self,
        _server: &mut Server,
        client: &mut OptionalClient,
        worker_id: WorkerId,
        message: WorkerResponse,
    ) {
        match ResponseStatus::try_from(message.status) {
            Ok(ResponseStatus::Ok) => {
                self.ok += 1;
                match self.progress {
                    UpgradeWorkerProgress::RequestingListenSockets { .. } => {}
                    UpgradeWorkerProgress::StopOldActivateNew { .. } => {
                        client.return_processing(format!(
                            "Worker {worker_id} answered OK to {}. {}",
                            message.id, message.message
                        ))
                    }
                }
            }
            Ok(ResponseStatus::Failure) => self.errors += 1,
            Ok(ResponseStatus::Processing) => client.return_processing(format!(
                "Worker {worker_id} is processing {}. {}",
                message.id, message.message
            )),
            Err(e) => warn!("error decoding response status: {}", e),
        }
        self.responses.push((worker_id, message));
    }
}

//===============================================
// Upgrade the main process

pub const UPGRADE_PROTOCOL_V2: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum UpgradeStage {
    Prepared = 0,
    Commit = 1,
    Activated = 2,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UpgradeCounts {
    pub clients: u64,
    pub workers: u64,
    pub tasks: u64,
    pub routes: u64,
    pub buffered_bytes: u64,
    pub pending_requests: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UpgradeHandshake {
    #[prost(enumeration = "UpgradeStage", tag = "1")]
    pub stage: i32,
    #[prost(uint32, tag = "2")]
    pub protocol: u32,
    #[prost(uint64, tag = "3")]
    pub clients: u64,
    #[prost(uint64, tag = "4")]
    pub workers: u64,
    #[prost(uint64, tag = "5")]
    pub tasks: u64,
    #[prost(uint64, tag = "6")]
    pub routes: u64,
    #[prost(uint64, tag = "7")]
    pub buffered_bytes: u64,
    #[prost(uint64, tag = "8")]
    pub pending_requests: u64,
}

impl UpgradeHandshake {
    pub fn new(stage: UpgradeStage) -> Self {
        Self {
            stage: stage as i32,
            protocol: u32::from(UPGRADE_PROTOCOL_V2),
            clients: 0,
            workers: 0,
            tasks: 0,
            routes: 0,
            buffered_bytes: 0,
            pending_requests: 0,
        }
    }

    pub fn prepared(counts: UpgradeCounts) -> Self {
        Self {
            stage: UpgradeStage::Prepared as i32,
            protocol: u32::from(UPGRADE_PROTOCOL_V2),
            clients: counts.clients,
            workers: counts.workers,
            tasks: counts.tasks,
            routes: counts.routes,
            buffered_bytes: counts.buffered_bytes,
            pending_requests: counts.pending_requests,
        }
    }

    pub fn counts(&self) -> UpgradeCounts {
        UpgradeCounts {
            clients: self.clients,
            workers: self.workers,
            tasks: self.tasks,
            routes: self.routes,
            buffered_bytes: self.buffered_bytes,
            pending_requests: self.pending_requests,
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct UpgradeData {
    protocol: u16,
    snapshot: UpgradeSnapshot,
}

impl UpgradeData {
    pub(crate) fn new(snapshot: UpgradeSnapshot) -> Self {
        Self {
            protocol: UPGRADE_PROTOCOL_V2,
            snapshot,
        }
    }

    pub(crate) fn into_snapshot(self) -> Result<UpgradeSnapshot, UpgradeDataError> {
        if self.protocol != UPGRADE_PROTOCOL_V2 {
            return Err(UpgradeDataError::UnsupportedProtocol(self.protocol));
        }
        Ok(self.snapshot)
    }

    pub(crate) fn snapshot(&self) -> &UpgradeSnapshot {
        &self.snapshot
    }

    pub fn counts(&self) -> UpgradeCounts {
        UpgradeCounts {
            clients: self.snapshot.clients.len() as u64,
            workers: self.snapshot.workers.len() as u64,
            tasks: self
                .snapshot
                .tasks
                .len()
                .saturating_add(self.snapshot.queued_tasks.len()) as u64,
            routes: self.snapshot.in_flight.len() as u64,
            buffered_bytes: self
                .snapshot
                .clients
                .iter()
                .map(ClientSessionSnapshot::buffered_bytes)
                .chain(
                    self.snapshot
                        .workers
                        .iter()
                        .map(WorkerSessionSnapshot::buffered_bytes),
                )
                .fold(0usize, usize::saturating_add) as u64,
            pending_requests: self
                .snapshot
                .workers
                .iter()
                .map(WorkerSessionSnapshot::pending_len)
                .fold(0usize, usize::saturating_add) as u64,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum UpgradeDataError {
    #[error("unsupported main-upgrade protocol {0}")]
    UnsupportedProtocol(u16),
    #[error("could not sample CLOCK_MONOTONIC: {0}")]
    MonotonicClock(String),
    #[error("CLOCK_MONOTONIC moved backwards across main upgrade")]
    MonotonicClockWentBackwards,
    #[error(transparent)]
    Task(#[from] TaskSnapshotError),
}

pub(crate) fn monotonic_nanos() -> Result<u64, UpgradeDataError> {
    let mut timespec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `timespec` is a valid writable pointer for the duration of the
    // call; CLOCK_MONOTONIC has no side effects and survives exec.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timespec) } != 0 {
        return Err(UpgradeDataError::MonotonicClock(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let seconds = u64::try_from(timespec.tv_sec)
        .map_err(|_| UpgradeDataError::MonotonicClock("negative seconds".to_owned()))?;
    let nanos = u64::try_from(timespec.tv_nsec)
        .map_err(|_| UpgradeDataError::MonotonicClock("negative nanoseconds".to_owned()))?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanos))
        .ok_or_else(|| UpgradeDataError::MonotonicClock("timestamp overflow".to_owned()))
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum UpgradeServerState {
    Running,
}

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct UpgradeSnapshot {
    /// file descriptor of the unix command socket
    pub command_socket_fd: i32,
    pub config: Config,
    pub captured_monotonic_nanos: u64,
    pub server_state: UpgradeServerState,
    pub next_client_id: ClientId,
    pub next_session_id: SessionId,
    pub next_task_id: TaskId,
    pub next_worker_id: WorkerId,
    /// Client whose `UpgradeMain` request caused this handoff. The replacement
    /// main owns its one terminal response after COMMIT.
    pub upgrade_client_token: usize,
    pub clients: Vec<ClientSessionSnapshot>,
    pub workers: Vec<WorkerSessionSnapshot>,
    pub event_subscribers: Vec<usize>,
    pub in_flight: HashMap<RequestId, TaskId>,
    pub tasks: HashMap<TaskId, TaskContainerSnapshot>,
    pub queued_tasks: HashMap<TaskId, TaskContainerSnapshot>,
    pub pending_audit_events: VecDeque<Event>,
    pub state: ConfigState,
    /// Boot-generation counter, bumped each time a `MAIN_UPGRADED` re-exec
    /// happens. Stamps every audit line so SOC tooling can disambiguate
    /// post-upgrade sessions from pre-upgrade ones (PIDs reset, but the
    /// audit log keeps generation+session_ulid as the durable correlation
    /// pair). `0` on first boot.
    #[serde(default)]
    pub boot_generation: u32,
}

/// The old main keeps running after a failed main upgrade: close again what
/// `generate_upgrade_data` and `disable_cloexec_before_upgrade` opened to
/// `exec`, or every worker it forks from then on inherits them.
fn restore_cloexec_after_failed_upgrade(hub: &mut CommandHub) {
    if let Err(err) = hub.enable_cloexec_after_upgrade() {
        error!(
            "could not restore close-on-exec after a failed upgrade: {}",
            err
        );
    }
}

fn close_reload_window_after_failed_upgrade() {
    if let Err(error) = sd_notify::notify(sd_notify::STATE_READY) {
        warn!(
            "could not notify systemd READY=1 after aborted main upgrade: {}",
            error
        );
    }
}

fn reap_failed_child(pid: i32) {
    let pid = Pid::from_raw(pid);
    let _ = kill(pid, Signal::SIGKILL);
    if let Err(error) = waitpid(pid, None) {
        warn!("could not reap failed replacement main {}: {}", pid, error);
    }
}

fn validate_prepared(
    message: &UpgradeHandshake,
    expected_counts: UpgradeCounts,
) -> Result<(), String> {
    if message.stage() != UpgradeStage::Prepared
        || message.protocol != u32::from(UPGRADE_PROTOCOL_V2)
        || message.counts() != expected_counts
    {
        return Err(format!(
            "unexpected child PREPARED message: {message:?}; expected counts {expected_counts:?}"
        ));
    }
    Ok(())
}

/// Cross the irreversible parent-side fence before attempting the first byte
/// of COMMIT, then wait for the replacement's post-activation acknowledgement.
/// Every error from this function is fail-stop: callers must never resume the
/// old Hub after entry.
fn commit_and_wait_for_activation(
    hub: &mut CommandHub,
    channel: &mut sozu_command_lib::channel::Channel<UpgradeHandshake, UpgradeHandshake>,
    timeout: Duration,
) -> Result<(), String> {
    hub.server.run_state = ServerState::Stopping;
    hub.server.upgrading = true;
    channel
        .write_message(&UpgradeHandshake::new(UpgradeStage::Commit))
        .map_err(|error| format!("failed to send irreversible main-upgrade commit: {error}"))?;

    let message = channel
        .read_message_blocking_timeout(Some(timeout))
        .map_err(|error| format!("replacement main failed after commit: {error}"))?;
    if message.stage() != UpgradeStage::Activated
        || message.protocol != u32::from(UPGRADE_PROTOCOL_V2)
    {
        return Err(format!(
            "replacement main returned unexpected post-commit message: {message:?}"
        ));
    }
    Ok(())
}

pub fn upgrade_main(hub: &mut CommandHub, client_token: Token) {
    if let Err(error) = probe_main_upgrade_candidate(&hub.server.executable_path) {
        hub.fail_upgrade(client_token, error.to_string());
        return;
    }
    hub.notify_upgrade_processing(client_token);

    let upgrade_data = match hub.generate_upgrade_data(client_token) {
        Ok(data) => data,
        Err(error) => {
            hub.fail_upgrade(client_token, error.to_string());
            return;
        }
    };
    let expected_counts = upgrade_data.counts();
    let handoff_timeout = Duration::from_secs(hub.server.config.worker_timeout.max(1) as u64);

    if let Err(error) = hub.disable_cloexec_before_upgrade() {
        hub.fail_upgrade(client_token, error.to_string());
        return;
    }
    if let Err(error) = sd_notify::notify(sd_notify::STATE_RELOADING) {
        warn!(
            "could not notify systemd RELOADING=1 for main upgrade: {}",
            error
        );
    }

    let (new_main_pid, mut fork_confirmation_channel) =
        match fork_main_into_new_main(hub.server.executable_path.clone(), upgrade_data) {
            Ok(tuple) => tuple,
            Err(fork_error) => {
                restore_cloexec_after_failed_upgrade(hub);
                close_reload_window_after_failed_upgrade();
                hub.fail_upgrade(
                    client_token,
                    format!("Could not start a new main process by forking: {fork_error}"),
                );
                return;
            }
        };

    match fork_confirmation_channel.read_message_blocking_timeout(Some(handoff_timeout)) {
        Ok(message) if validate_prepared(&message, expected_counts).is_ok() => {}
        Ok(message) => {
            drop(fork_confirmation_channel);
            reap_failed_child(new_main_pid);
            restore_cloexec_after_failed_upgrade(hub);
            close_reload_window_after_failed_upgrade();
            hub.fail_upgrade(client_token, format!(
                "Upgrade of main process failed before commit: unexpected child message {message:?}"
            ));
            return;
        }
        Err(error) => {
            drop(fork_confirmation_channel);
            reap_failed_child(new_main_pid);
            restore_cloexec_after_failed_upgrade(hub);
            close_reload_window_after_failed_upgrade();
            hub.fail_upgrade(
                client_token,
                format!("Upgrade of main process failed before commit: {error}"),
            );
            return;
        }
    }

    match hub.sigterm_pending() {
        Ok(false) => {}
        Ok(true) => {
            drop(fork_confirmation_channel);
            reap_failed_child(new_main_pid);
            restore_cloexec_after_failed_upgrade(hub);
            close_reload_window_after_failed_upgrade();
            hub.fail_upgrade(
                client_token,
                "Main upgrade aborted before commit because SIGTERM is pending".to_owned(),
            );
            return;
        }
        Err(error) => {
            drop(fork_confirmation_channel);
            reap_failed_child(new_main_pid);
            restore_cloexec_after_failed_upgrade(hub);
            close_reload_window_after_failed_upgrade();
            hub.fail_upgrade(
                client_token,
                format!("Main upgrade aborted before commit: could not inspect SIGTERM: {error}"),
            );
            return;
        }
    }

    match commit_and_wait_for_activation(hub, &mut fork_confirmation_channel, handoff_timeout) {
        Ok(()) => info!(
            "replacement main {} activated; old main will exit without touching transferred descriptors",
            new_main_pid
        ),
        Err(error) => error!("{}; old main remains fenced and will stop", error),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        sync::mpsc,
        thread,
        time::Instant,
    };

    use std::sync::Arc;

    use mio::{Token, net::UnixListener};
    use prost::Message;
    use serde_json::Value;

    use super::{
        UPGRADE_PROTOCOL_V2, UpgradeCounts, UpgradeData, UpgradeHandshake, UpgradeServerState,
        UpgradeSnapshot, UpgradeStage, UpgradeWorkerProgress, UpgradeWorkerTask,
        commit_and_wait_for_activation, validate_prepared,
    };
    use crate::command::{
        server::{
            CommandHub, GatheringTask, PeerCred, ServerState, TaskRestoreTiming,
            TaskSnapshotTiming, WorkerId,
        },
        sessions::ClientSession,
    };
    use sozu_command_lib::channel::Channel;
    use sozu_command_lib::{config::Config, state::ConfigState};
    use sozu_command_lib::{
        proto::command::{Request, Response, ResponseStatus, WorkerResponse},
        ready::Ready,
    };

    fn sample_upgrade_data() -> UpgradeData {
        UpgradeData::new(UpgradeSnapshot {
            command_socket_fd: 11,
            config: Config::default(),
            captured_monotonic_nanos: 1,
            server_state: UpgradeServerState::Running,
            next_client_id: 12,
            next_session_id: 13,
            next_task_id: 14,
            next_worker_id: 15,
            upgrade_client_token: 1,
            clients: Vec::new(),
            workers: Vec::new(),
            event_subscribers: Vec::new(),
            in_flight: HashMap::new(),
            tasks: HashMap::new(),
            queued_tasks: HashMap::new(),
            pending_audit_events: VecDeque::new(),
            state: ConfigState::new(),
            boot_generation: 16,
        })
    }

    #[test]
    fn prepared_requires_exact_stage_protocol_and_snapshot_counts() {
        let counts = UpgradeCounts {
            clients: 1,
            workers: 2,
            tasks: 3,
            routes: 4,
            buffered_bytes: 5,
            pending_requests: 6,
        };
        let prepared = UpgradeHandshake::prepared(counts);
        assert!(validate_prepared(&prepared, counts).is_ok());

        let mut wrong_stage = prepared.clone();
        wrong_stage.stage = UpgradeStage::Activated as i32;
        assert!(validate_prepared(&wrong_stage, counts).is_err());

        let mut wrong_protocol = prepared.clone();
        wrong_protocol.protocol += 1;
        assert!(validate_prepared(&wrong_protocol, counts).is_err());

        for field in 0..6 {
            let mut wrong_counts = prepared.clone();
            match field {
                0 => wrong_counts.clients += 1,
                1 => wrong_counts.workers += 1,
                2 => wrong_counts.tasks += 1,
                3 => wrong_counts.routes += 1,
                4 => wrong_counts.buffered_bytes += 1,
                5 => wrong_counts.pending_requests += 1,
                _ => unreachable!(),
            }
            assert!(
                validate_prepared(&wrong_counts, counts).is_err(),
                "PREPARED count field {field} was not validated"
            );
        }
    }

    fn test_hub() -> CommandHub {
        let directory = tempfile::tempdir().expect("create command-hub tempdir");
        let listener =
            UnixListener::bind(directory.path().join("sozu.sock")).expect("bind command listener");
        CommandHub::new(listener, Config::default(), "sozu".to_owned()).expect("create command Hub")
    }

    #[test]
    fn commit_write_failure_leaves_the_old_hub_irreversibly_fenced() {
        let mut hub = test_hub();
        let (mut parent, peer): (
            Channel<UpgradeHandshake, UpgradeHandshake>,
            Channel<UpgradeHandshake, UpgradeHandshake>,
        ) = Channel::generate(4096, 65536).expect("create handshake channel");
        drop(peer);

        let result = commit_and_wait_for_activation(
            &mut hub,
            &mut parent,
            std::time::Duration::from_millis(10),
        );

        assert!(result.is_err(), "closed peer must reject COMMIT");
        assert_eq!(hub.server.run_state, ServerState::Stopping);
        assert!(hub.server.upgrading, "old Hub must stay fenced");
    }

    #[test]
    fn activated_eof_after_a_complete_commit_never_unfences_the_old_hub() {
        let mut hub = test_hub();
        let (mut parent, mut child): (
            Channel<UpgradeHandshake, UpgradeHandshake>,
            Channel<UpgradeHandshake, UpgradeHandshake>,
        ) = Channel::generate(4096, 65536).expect("create handshake channel");
        let child = thread::spawn(move || {
            child.blocking().expect("make child handshake blocking");
            let message = child.read_message().expect("child must receive COMMIT");
            assert_eq!(message.stage(), UpgradeStage::Commit);
            assert_eq!(message.protocol, u32::from(UPGRADE_PROTOCOL_V2));
            // Drop without ACTIVATED to model a post-COMMIT child failure.
        });

        let result = commit_and_wait_for_activation(
            &mut hub,
            &mut parent,
            std::time::Duration::from_secs(1),
        );
        child.join().expect("child handshake witness");

        assert!(result.is_err(), "missing ACTIVATED must fail-stop");
        assert_eq!(hub.server.run_state, ServerState::Stopping);
        assert!(hub.server.upgrading, "old Hub must stay fenced");
    }

    #[test]
    fn activated_timeout_after_a_complete_commit_never_unfences_the_old_hub() {
        let mut hub = test_hub();
        let (mut parent, mut child): (
            Channel<UpgradeHandshake, UpgradeHandshake>,
            Channel<UpgradeHandshake, UpgradeHandshake>,
        ) = Channel::generate(4096, 65536).expect("create handshake channel");
        let (commit_seen_tx, commit_seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let child = thread::spawn(move || {
            child.blocking().expect("make child handshake blocking");
            let message = child.read_message().expect("child must receive COMMIT");
            assert_eq!(message.stage(), UpgradeStage::Commit);
            commit_seen_tx.send(()).expect("publish COMMIT witness");
            release_rx.recv().expect("hold the child without ACTIVATED");
        });

        let result = commit_and_wait_for_activation(
            &mut hub,
            &mut parent,
            std::time::Duration::from_millis(20),
        );
        commit_seen_rx
            .recv()
            .expect("the child must have received COMMIT before the timeout");
        release_tx.send(()).expect("release child witness");
        child.join().expect("child handshake witness");

        assert!(result.is_err(), "missing ACTIVATED must time out");
        assert_eq!(hub.server.run_state, ServerState::Stopping);
        assert!(hub.server.upgrading, "old Hub must stay fenced");
    }

    #[test]
    fn v2_upgrade_wire_has_an_incompatible_top_level_envelope() {
        let encoded = serde_json::to_value(sample_upgrade_data()).unwrap();

        assert_eq!(encoded.get("protocol"), Some(&Value::from(2)));
        assert!(encoded.get("snapshot").is_some());
        assert!(encoded.get("command_socket_fd").is_none());
    }

    #[test]
    fn handshake_wire_distinguishes_prepared_commit_and_activated() {
        for stage in [
            UpgradeStage::Prepared,
            UpgradeStage::Commit,
            UpgradeStage::Activated,
        ] {
            let encoded = UpgradeHandshake::new(stage).encode_to_vec();
            let decoded = UpgradeHandshake::decode(encoded.as_slice()).unwrap();
            assert_eq!(decoded.stage(), stage);
            assert_eq!(decoded.protocol, u32::from(UPGRADE_PROTOCOL_V2));
        }
    }

    #[test]
    fn both_upgrade_worker_phases_round_trip_without_running_the_completed_callback() {
        let now = Instant::now();
        let timing = TaskSnapshotTiming { now };
        let tasks = [
            UpgradeWorkerTask {
                client_token: Token(7),
                progress: UpgradeWorkerProgress::RequestingListenSockets {
                    old_worker_token: Token(8),
                    old_worker_id: 9,
                },
                ok: 1,
                errors: 0,
                responses: Vec::new(),
                expected_responses: 1,
            },
            UpgradeWorkerTask {
                client_token: Token(10),
                progress: UpgradeWorkerProgress::StopOldActivateNew {
                    old_worker_id: 11,
                    new_worker_id: 12,
                },
                ok: 3,
                errors: 1,
                responses: Vec::new(),
                expected_responses: 4,
            },
        ];

        for task in tasks {
            let snapshot = task.snapshot(timing).expect("worker task should snapshot");
            let expected = serde_json::to_value(&snapshot).expect("snapshot should serialize");
            let restored = snapshot
                .restore(TaskRestoreTiming {
                    now,
                    handoff_elapsed: std::time::Duration::ZERO,
                })
                .expect("snapshot should restore directly");
            let actual = serde_json::to_value(
                restored
                    .snapshot(timing)
                    .expect("restored task should snapshot again"),
            )
            .expect("restored snapshot should serialize");
            assert_eq!(actual, expected);
        }
    }

    fn worker_response(id: &str, status: ResponseStatus, message: &str) -> WorkerResponse {
        WorkerResponse {
            id: id.to_owned(),
            status: status.into(),
            message: message.to_owned(),
            content: None,
        }
    }

    /// Runs the `StopOldActivateNew` completion of `upgrade --worker 11` (new
    /// worker 12) over `responses` and returns the single terminal answer its
    /// client receives. The arm reads no worker session, so no worker process
    /// is registered.
    fn finish_stop_old_activate_new(responses: Vec<(WorkerId, WorkerResponse)>) -> Response {
        let mut hub = test_hub();
        let (channel, mut peer): (Channel<Response, Request>, Channel<Request, Response>) =
            Channel::generate_nonblocking(4096, 65536).expect("create client channels");
        let mut client = ClientSession::new(
            channel,
            1,
            Token(1),
            PeerCred::default(),
            None,
            None,
            Arc::from("/tmp/sozu-upgrade-worker-test.sock"),
        );
        let ok = responses
            .iter()
            .filter(|(_, response)| response.status == ResponseStatus::Ok as i32)
            .count();
        let task = Box::new(UpgradeWorkerTask {
            client_token: client.token,
            progress: UpgradeWorkerProgress::StopOldActivateNew {
                old_worker_id: 11,
                new_worker_id: 12,
            },
            ok,
            errors: responses.len() - ok,
            expected_responses: responses.len(),
            responses,
        });

        task.on_finish(&mut hub.server, &mut Some(&mut client), false);

        client.channel.handle_events(Ready::WRITABLE);
        client.channel.run().expect("client response should flush");
        peer.handle_events(Ready::READABLE);
        peer.run().expect("peer should buffer the client response");
        let response = peer
            .read_message()
            .expect("the task must answer its client");
        assert!(
            peer.read_message().is_err(),
            "the task must answer its client exactly once"
        );
        response
    }

    /// The old worker exits without answering its `SoftStop`:
    /// `CommandHub::fail_in_flight_requests_of_worker` synthesizes a failure
    /// for it. The client used to read "Upgrade successful".
    #[test]
    fn upgrade_worker_reports_an_old_worker_closing_before_its_soft_stop_as_a_failure() {
        let response = finish_stop_old_activate_new(vec![
            (
                11,
                worker_response(
                    "UPGRADE-11-0",
                    ResponseStatus::Failure,
                    "worker 11 closed before answering",
                ),
            ),
            (12, worker_response("UPGRADE-12-1", ResponseStatus::Ok, "")),
            (12, worker_response("UPGRADE-12-2", ResponseStatus::Ok, "")),
        ]);

        assert_eq!(
            response.status,
            ResponseStatus::Failure as i32,
            "{response:?}"
        );
        assert!(
            response.message.contains("old worker 11")
                && response
                    .message
                    .contains("worker 11 closed before answering"),
            "the failure must name the old worker and its reason: {response:?}"
        );
        assert!(
            response.message.contains("new worker 12 is serving"),
            "the new worker activated fully and must be reported serving: {response:?}"
        );
    }

    /// A rejected activation of the new worker used to be ignored and
    /// reported as "finished activation of new worker".
    #[test]
    fn upgrade_worker_reports_a_failed_new_worker_activation_as_a_failure() {
        let response = finish_stop_old_activate_new(vec![
            (11, worker_response("UPGRADE-11-0", ResponseStatus::Ok, "")),
            (12, worker_response("UPGRADE-12-1", ResponseStatus::Ok, "")),
            (
                12,
                worker_response(
                    "UPGRADE-12-2",
                    ResponseStatus::Failure,
                    "could not activate listener 127.0.0.1:8080",
                ),
            ),
        ]);

        assert_eq!(
            response.status,
            ResponseStatus::Failure as i32,
            "{response:?}"
        );
        assert!(
            response.message.contains("new worker 12")
                && response
                    .message
                    .contains("could not activate listener 127.0.0.1:8080"),
            "the failure must name the new worker and its reason: {response:?}"
        );
        assert!(
            !response.message.contains("is serving"),
            "a partially activated worker must not be reported serving: {response:?}"
        );
    }

    #[test]
    fn upgrade_worker_reports_success_when_both_workers_answer_ok() {
        let response = finish_stop_old_activate_new(vec![
            (11, worker_response("UPGRADE-11-0", ResponseStatus::Ok, "")),
            (12, worker_response("UPGRADE-12-1", ResponseStatus::Ok, "")),
            (12, worker_response("UPGRADE-12-2", ResponseStatus::Ok, "")),
        ]);

        assert_eq!(response.status, ResponseStatus::Ok as i32, "{response:?}");
        assert_eq!(
            response.message,
            "Upgrade successful:\n- finished soft stop of worker 11\n- finished activation of new worker 12"
        );
    }
}
