use std::{collections::VecDeque, fmt, os::unix::io::AsRawFd};

use futures::SinkExt;
use libc::pid_t;
use nix::{sys::signal::kill, unistd::Pid};

use sozu_command_lib::{
    channel::Channel,
    command::{RunState, WorkerInfo},
    config::Config,
    proxy::{ProxyRequest, ProxyRequestData, ProxyResponse},
    scm_socket::ScmSocket,
};

pub struct Worker {
    pub id: u32,
    /// for the worker to receive requests and respond to the main process
    pub worker_channel: Option<Channel<ProxyRequest, ProxyResponse>>,
    /// file descriptor of the command channel
    pub worker_channel_fd: i32,
    pub pid: pid_t,
    pub run_state: RunState,
    pub queue: VecDeque<ProxyRequest>,
    /// used to receive listeners
    pub scm_socket: ScmSocket,
    pub sender: Option<futures::channel::mpsc::Sender<ProxyRequest>>,
}

impl Worker {
    pub fn new(
        id: u32,
        pid: pid_t,
        command_channel: Channel<ProxyRequest, ProxyResponse>,
        scm_socket: ScmSocket,
        _: &Config,
    ) -> Worker {
        Worker {
            id,
            worker_channel_fd: command_channel.sock.as_raw_fd(),
            worker_channel: Some(command_channel),
            sender: None,
            pid,
            run_state: RunState::Running,
            queue: VecDeque::new(),
            scm_socket,
        }
    }

    pub async fn send(&mut self, request_id: String, data: ProxyRequestData) {
        if let Some(worker_tx) = self.sender.as_mut() {
            if let Err(e) = worker_tx
                .send(ProxyRequest {
                    id: request_id.clone(),
                    order: data,
                })
                .await
            {
                error!(
                    "error sending message {} to worker {:?}: {:?}",
                    request_id, self.id, e
                );
            }
        }
    }

    pub fn the_pid_is_alive(&self) -> bool {
        // send a kill -0 to check on the pid, if it's dead it should be an error
        match kill(Pid::from_raw(self.pid), None) {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    pub fn info(&self) -> WorkerInfo {
        WorkerInfo {
            id: self.id.clone(),
            pid: self.pid.clone(),
            run_state: self.run_state.clone(),
        }
    }

    /*
    pub fn push_message(&mut self, message: ProxyRequest) {
      self.queue.push_back(message);
      self.channel.interest.insert(Ready::writable());
    }

    pub fn can_handle_events(&self) -> bool {
      self.channel.readiness().is_readable() || (!self.queue.is_empty() && self.channel.readiness().is_writable())
    }*/
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Worker {{ id: {}, run_state: {:?} }}",
            self.id, self.run_state
        )
    }
}
