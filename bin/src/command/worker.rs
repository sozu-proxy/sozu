use futures::SinkExt;
use libc::pid_t;
use std::collections::VecDeque;
use std::fmt;
use std::os::unix::io::AsRawFd;

use sozu_command::channel::Channel;
use sozu_command::command::RunState;
use sozu_command::config::Config;
use sozu_command::proxy::{ProxyRequest, ProxyRequestData, ProxyResponse};
use sozu_command::scm_socket::ScmSocket;

pub struct Worker {
    pub id: u32,
    pub fd: i32,
    pub channel: Option<Channel<ProxyRequest, ProxyResponse>>,
    //pub token:         Option<Token>,
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
        if let Some(tx) = self.sender.as_mut() {
            if let Err(e) = tx.send(ProxyRequest {
                id: request_id,
                order: data,
            })
            .await {
                error!("error sending message to worker {:?}: {:?}", self.id, e);
            }
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
