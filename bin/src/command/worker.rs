use std::{collections::VecDeque, fmt, os::unix::io::AsRawFd};

use futures::SinkExt;
use libc::pid_t;
use nix::{sys::signal::kill, unistd::Pid};

use sozu_command_lib::{
    channel::Channel,
    command::RunState,
    config::Config,
    proxy::{ProxyRequest, ProxyRequestData, ProxyResponse},
    scm_socket::ScmSocket,
};

pub struct Worker {
    pub id: u32,
    pub fd: i32,
    /// for the worker to receive and respond to the main process
    pub channel: Option<Channel<ProxyRequest, ProxyResponse>>,
    pub pid: pid_t,
    pub run_state: RunState,
    pub queue: VecDeque<ProxyRequest>,
    pub scm: ScmSocket,
    pub sender: Option<futures::channel::mpsc::Sender<ProxyRequest>>,
}

impl Worker {
    pub fn new(
        id: u32,
        pid: pid_t,
        channel: Channel<ProxyRequest, ProxyResponse>,
        scm: ScmSocket,
        _: &Config,
    ) -> Worker {
        Worker {
            id,
            fd: channel.sock.as_raw_fd(),
            channel: Some(channel),
            sender: None,
            pid,
            run_state: RunState::Running,
            queue: VecDeque::new(),
            scm,
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
