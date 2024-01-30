use std::os::fd::AsRawFd;

use libc::pid_t;
use mio::Token;
use serde::{Deserialize, Serialize};

use sozu_command_lib::{
    config::Config,
    proto::command::{
        request::RequestType, ResponseStatus, ReturnListenSockets, RunState, SoftStop,
    },
    response::WorkerResponse,
    state::ConfigState,
};

use crate::{
    command::{
        server::{
            ClientId, Gatherer, GatheringTask, MessageClient, Server, ServerState, SessionId,
            TaskId, Timeout, WorkerId,
        },
        sessions::{ClientSession, OptionalClient},
    },
    upgrade::{fork_main_into_new_main, UpgradeError},
    util::disable_close_on_exec,
};

use super::sessions::WorkerSession;

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

    let old_worker_token = match server.get_active_worker_by_id(old_worker_id) {
        Some(session) => session.token,
        None => {
            client.finish_failure(format!(
                "Worker {} does not exist, or is stopping / stopped",
                old_worker_id
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
                return client.finish_failure(format!("could not launch new worker: {worker_err}"))
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
        client.return_processing(format!("Soft stopping worker with id {}", old_worker_id));
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
                client.finish_ok(
                    format!(
                        "Upgrade successful:\n- finished soft stop of worker {:?}\n- finished activation of new worker {:?}",
                        old_worker_id, new_worker_id
                    )
                );
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
        match message.status {
            ResponseStatus::Ok => {
                self.ok += 1;
                match self.progress {
                    UpgradeWorkerProgress::RequestingListenSockets { .. } => {}
                    UpgradeWorkerProgress::StopOldActivateNew { .. } => {
                        client.return_processing(format!(
                            "Worker {} answered OK to {}. {}",
                            worker_id, message.id, message.message
                        ))
                    }
                }
            }
            ResponseStatus::Failure => self.errors += 1,
            ResponseStatus::Processing => client.return_processing(format!(
                "Worker {} is processing {}. {}",
                worker_id, message.id, message.message
            )),
        }
        self.responses.push((worker_id, message));
    }
}

//===============================================
// Upgrade the main process

/// Summary of a worker session, meant to be passed to a new main process
/// during an upgrade, in order to recreate the worker
#[derive(Deserialize, Serialize, Debug)]
pub struct SerializedWorkerSession {
    /// file descriptor of the UNIX channel
    pub channel_fd: i32,
    pub pid: pid_t,
    pub id: WorkerId,
    pub run_state: RunState,
    /// file descriptor of the SCM socket
    pub scm_fd: i32,
}

impl TryFrom<&WorkerSession> for SerializedWorkerSession {
    type Error = UpgradeError;

    fn try_from(worker: &WorkerSession) -> Result<Self, Self::Error> {
        disable_close_on_exec(worker.channel.fd()).map_err(|util_err| {
            UpgradeError::DisableCloexec {
                fd_name: format!("main-to-worker-{}-channel", worker.id),
                util_err,
            }
        })?;

        Ok(Self {
            channel_fd: worker.channel.sock.as_raw_fd(),
            pid: worker.pid,
            id: worker.id,
            run_state: worker.run_state,
            scm_fd: worker.scm_socket.raw_fd(),
        })
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct UpgradeData {
    /// file descriptor of the unix command socket
    pub command_socket_fd: i32,
    pub config: Config,
    pub next_client_id: ClientId,
    pub next_session_id: SessionId,
    pub next_task_id: TaskId,
    pub next_worker_id: WorkerId,
    /// JSON serialized workers
    pub workers: Vec<SerializedWorkerSession>,
    pub state: ConfigState,
}

pub fn upgrade_main(server: &mut Server, client: &mut ClientSession) {
    if let Err(err) = server.disable_cloexec_before_upgrade() {
        client.finish_failure(err.to_string());
    }

    client.return_processing("Upgrading the main process...");

    let upgrade_data = server.generate_upgrade_data();

    let (new_main_pid, mut fork_confirmation_channel) =
        match fork_main_into_new_main(server.executable_path.clone(), upgrade_data) {
            Ok(tuple) => tuple,
            Err(fork_error) => {
                client.finish_failure(format!(
                    "Could not start a new main process by forking: {}",
                    fork_error
                ));
                return;
            }
        };

    let received_ok_from_new_process = fork_confirmation_channel.read_message().unwrap_or(false);

    debug!(
        "new main process sent a fork confirmation: {:?}",
        received_ok_from_new_process
    );

    if !received_ok_from_new_process {
        client.finish_failure("Upgrade of main process failed: no feedback from the new main");
    } else {
        client.finish_ok(format!(
            "Upgrade successful, closing main process. New main process has pid {}",
            new_main_pid
        ));
        server.run_state = ServerState::Stopping;
    }
}
