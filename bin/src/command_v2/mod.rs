use std::{collections::HashMap, fs, path::PathBuf};

use anyhow::{bail, Context};
use libc::pid_t;
use mio::{
    net::{UnixListener, UnixStream},
    Events, Interest, Poll, Token,
};
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use sozu_command_lib::{
    channel::Channel,
    config::Config,
    proto::command::{Request, Response},
    ready::Ready,
    request::WorkerRequest,
    response::WorkerResponse,
};

use crate::worker::Worker;

trait Session: std::fmt::Debug {
    fn update_readiness(&mut self, events: Ready);
    fn ready(&mut self);
}

#[derive(Debug)]
struct WorkerSession {
    pub id: u32,
    pub pid: pid_t,
    pub channel: Channel<WorkerRequest, WorkerResponse>,
}

#[derive(Debug)]
struct ClientSession {
    pub id: u32,
    pub channel: Channel<Response, Request>,
}

impl Session for WorkerSession {
    fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    fn ready(&mut self) {
        let status = self.channel.run();
        println!("{status:?}");
    }
}

impl Session for ClientSession {
    fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    fn ready(&mut self) {
        let status = self.channel.run();
        println!("{status:?}");
        if status.is_err() {
            return;
        }
        let message = self.channel.read_message();
        println!("{message:?}");
    }
}

#[derive(Debug)]
struct CommandServer {
    next_client_id: u32,
    next_session_token: usize,
    poll: Poll,
    sessions: HashMap<Token, Box<dyn Session>>,
    unix_listener: UnixListener,
}

pub fn start_server(
    config: Config,
    command_socket_path: String,
    workers: Vec<Worker>,
) -> anyhow::Result<()> {
    let path = PathBuf::from(&command_socket_path);

    if fs::metadata(&path).is_ok() {
        info!("A socket is already present. Deleting...");
        fs::remove_file(&path)
            .with_context(|| format!("could not delete previous socket at {path:?}"))?;
    }

    let unix_listener = match UnixListener::bind(&path) {
        Ok(unix_listener) => unix_listener,
        Err(e) => {
            error!("could not create unix socket: {:?}", e);
            // the workers did not even get the configuration, we can kill them right away
            for worker in workers {
                error!("killing worker n°{} (PID {})", worker.id, worker.pid);
                let _ = kill(Pid::from_raw(worker.pid), Signal::SIGKILL).map_err(|e| {
                    error!("could not kill worker: {:?}", e);
                });
            }
            bail!("couldn't start server");
        }
    };

    // if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
    //     error!("could not set the unix socket permissions: {:?}", e);
    //     let _ = fs::remove_file(&path).map_err(|e2| {
    //         error!("could not remove the unix socket: {:?}", e2);
    //     });
    //     // the workers did not even get the configuration, we can kill them right away
    //     for worker in workers {
    //         error!("killing worker n°{} (PID {})", worker.id, worker.pid);
    //         let _ = kill(Pid::from_raw(worker.pid), Signal::SIGKILL).map_err(|e| {
    //             error!("could not kill worker: {:?}", e);
    //         });
    //     }
    //     bail!("couldn't start server");
    // }

    let mut command_server = CommandServer::new(unix_listener)?;
    for mut worker in workers {
        command_server.register_worker(worker.id, worker.pid, worker.worker_channel.take().unwrap())
    }
    command_server.run();
}

impl CommandServer {
    fn new(mut unix_listener: UnixListener) -> anyhow::Result<Self> {
        let poll = mio::Poll::new().with_context(|| "Poll::new() failed")?;
        poll.registry()
            .register(
                &mut unix_listener,
                Token(0),
                Interest::READABLE | Interest::WRITABLE,
            )
            .with_context(|| "should register the channel")?;

        Ok(Self {
            next_client_id: 0,
            next_session_token: 1,
            poll,
            sessions: HashMap::new(),
            unix_listener,
        })
    }

    fn next_session_token(&mut self) -> Token {
        let token = Token(self.next_session_token);
        self.next_session_token += 1;
        token
    }
    fn next_client_id(&mut self) -> u32 {
        let id = self.next_client_id;
        self.next_client_id += 1;
        id
    }

    fn register(&mut self, token: Token, stream: &mut UnixStream) {
        self.poll
            .registry()
            .register(stream, token, Interest::READABLE | Interest::WRITABLE)
            .expect("could not register channel");
    }

    fn register_worker(
        &mut self,
        id: u32,
        pid: pid_t,
        mut channel: Channel<WorkerRequest, WorkerResponse>,
    ) {
        let token = self.next_session_token();
        self.register(token, &mut channel.sock);
        self.sessions
            .insert(token, Box::new(WorkerSession { id, pid, channel }));
    }

    fn run(&mut self) -> ! {
        let mut events = Events::with_capacity(100);
        println!("{self:#?}");

        loop {
            events.clear();
            match self.poll.poll(&mut events, None) {
                Ok(()) => {}
                Err(error) => error!("Error while polling: {:?}", error),
            }

            for event in &events {
                match event.token() {
                    Token(0) => {
                        if event.is_readable() {
                            while let Ok((mut stream, addr)) = self.unix_listener.accept() {
                                info!("New client connected: {:?}", addr);
                                let token = self.next_session_token();
                                self.register(token, &mut stream);
                                let channel = Channel::new(stream, 4096, usize::MAX);
                                let id = self.next_client_id();
                                let session = Box::new(ClientSession { id, channel });
                                info!("{:#?}", session);
                                self.sessions.insert(token, session);
                            }
                        }
                    }
                    token => {
                        info!("{:?} got event: {:?}", token, event);
                        if let Some(session) = self.sessions.get_mut(&token) {
                            session.update_readiness(Ready::from(event));
                            session.ready();
                        }
                    }
                }
            }
        }
    }
}
