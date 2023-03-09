use std::{collections::VecDeque, fmt, os::unix::io::AsRawFd};

use futures::SinkExt;
use libc::pid_t;
use nix::{sys::signal::kill, unistd::Pid};

use sozu_command_lib::{
    channel::Channel,
    command::{Order, RunState, WorkerInfo},
    config::Config,
    scm_socket::ScmSocket,
    worker::{InnerOrder, ProxyResponse},
};

/// An instance of Sōzu, as seen from the main process
pub struct Worker {
    pub id: u32,
    /// for the worker to receive requests and respond to the main process
    pub worker_channel: Option<Channel<InnerOrder, ProxyResponse>>,
    /// file descriptor of the command channel
    pub worker_channel_fd: i32,
    pub pid: pid_t,
    pub run_state: RunState,
    pub queue: VecDeque<InnerOrder>,
    /// Used to send and receive listeners (socket addresses and file descriptors)
    pub scm_socket: ScmSocket,
    /// Used to send proxyrequests to the worker loop
    pub sender: Option<futures::channel::mpsc::Sender<InnerOrder>>,
}

impl Worker {
    pub fn new(
        id: u32,
        pid: pid_t,
        command_channel: Channel<InnerOrder, ProxyResponse>,
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

    /// send proxy request to the worker, via the mpsc sender
    pub async fn send(&mut self, order_id: String, content: Order) {
        if let Some(worker_tx) = self.sender.as_mut() {
            if let Err(e) = worker_tx
                .send(InnerOrder {
                    id: order_id.clone(),
                    content,
                })
                .await
            {
                error!(
                    "error sending message {} to worker {:?}: {:?}",
                    order_id, self.id, e
                );
            }
        }
    }

    /// send a kill -0 to check on the pid, if it's dead it should be an error
    pub fn the_pid_is_alive(&self) -> bool {
        kill(Pid::from_raw(self.pid), None).is_ok()
    }

    pub fn info(&self) -> WorkerInfo {
        WorkerInfo {
            id: self.id,
            pid: self.pid,
            run_state: self.run_state,
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
