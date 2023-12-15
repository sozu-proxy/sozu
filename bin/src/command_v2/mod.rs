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
    proto::command::{
        request::RequestType, Request, Response, ResponseContent, ResponseStatus, RunState,
    },
    ready::Ready,
    request::WorkerRequest,
    response::WorkerResponse,
};

use crate::worker::Worker;

#[derive(Debug)]
struct WorkerSession {
    pub channel: Channel<WorkerRequest, WorkerResponse>,
    pub id: u32,
    pub pid: pid_t,
    pub run_state: RunState,
    pub token: Token,
}

#[derive(Debug)]
enum SessionState {
    WaitingForRequest,
    Gathering {
        gathered_responses: Vec<WorkerResponse>,
        main_response: Option<ResponseContent>,
        expected_worker_responses: usize,
        request_type: RequestType,
    },
    Finishing,
}

#[derive(Debug)]
struct ClientSession {
    pub channel: Channel<Response, Request>,
    pub id: u32,
    pub token: Token,
    pub state: SessionState,
}

impl ClientSession {
    fn finish(&self, response: Response) {
        self.channel.write_message(&response);
    }
    fn finish_ok(&self, content: Option<ResponseContent>) {
        self.channel.write_message(&Response {
            status: ResponseStatus::Ok.into(),
            message: "Successfully handled request".to_owned(),
            content,
        });
    }
}

impl WorkerSession {
    fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    fn ready(&mut self) -> Option<WorkerResponse> {
        let status = self.channel.run();
        println!("{status:?}");
    }
}

impl ClientSession {
    fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    fn ready(&mut self) -> Option<Request> {
        let status = self.channel.run();
        println!("{status:?}");
        if status.is_err() {
            return None;
        }
        let message = self.channel.read_message();
        println!("{message:?}");
        match self.state {
            SessionState::WaitingForRequest => message.ok(),
            _ => {
                error!("Received a request when we didn't expected it");
                None
            }
        }
        // match self.advancement {
        // Advancement::WaitingForRequest => {
        //     let status = self.channel.run();
        //     println!("{status:?}");
        //     if status.is_err() {
        //         return;
        //     }
        //     let message = self.channel.read_message().unwrap();
        //     println!("{message:?}");
        //     use sozu_command_lib::proto::command::request::RequestType;
        //     match message.request_type.unwrap() {
        //         RequestType::QueryClustersHashes(_) => Advancement::Handling(Box::new()),
        //         _ => todo!(),
        //     }
        // }
        // }
    }
}

// enum Advancement {
//     WaitingForRequest,
//     Handling(Box<dyn Command>),
// }

// trait Command {
//     fn on_request()
// }

// impl Command for Query {
//     fn on_request(&mut self, message, ctx) {
//         let response = ctx.query(message, Main)
//         ctx.finish(response)
//     }
// }

// struct StatusCommand {
//     main_response
//     worker_responses
// }

// impl Command for StatusCommand {
//     fn on_request(&mut self, message, ctx) {
//         self.main_response = ctx.query(message, Main);
//         ctx.scatter(message, ctx.workers);
//     }
//     fn gather_storage(&mut self) -> Vec<Response> {
//         &mut self.responses
//     }
//     fn on_gather(&mut self, response) {
//         match response {
//             Ok => self.gather_storage().push(response)
//             ...
//         }
//     }
//     fn on_gather_complete(&mut self, ctx) {
//         let response = merge(main_response, worker_responses)
//         ctx.finish(response)
//     }
// }

#[derive(Debug)]
struct CommandServer {
    next_client_id: u32,
    next_session_token: usize,
    poll: Poll,
    client_sessions: HashMap<Token, ClientSession>,
    worker_sessions: HashMap<Token, WorkerSession>,
    unix_listener: UnixListener,
    in_flight: HashMap<String, Token>,
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
            client_sessions: HashMap::new(),
            in_flight: HashMap::new(),
            next_client_id: 0,
            next_session_token: 1,
            poll,
            unix_listener,
            worker_sessions: HashMap::new(),
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
        self.worker_sessions.insert(
            token,
            WorkerSession {
                channel,
                id,
                pid,
                run_state: RunState::Running,
                token,
            },
        );
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
                                let session = ClientSession {
                                    id,
                                    token,
                                    channel,
                                    state: SessionState::WaitingForRequest,
                                };
                                info!("{:#?}", session);
                                self.client_sessions.insert(token, session);
                            }
                        }
                    }
                    token => {
                        info!("{:?} got event: {:?}", token, event);
                        if let Some(session) = self.client_sessions.get_mut(&token) {
                            session.update_readiness(Ready::from(event));
                            if let Some(request) = session.ready() {
                                self.handle_request(request, session);
                            }
                        }
                        if let Some(session) = self.worker_sessions.get_mut(&token) {
                            session.update_readiness(Ready::from(event));
                            if let Some(response) = session.ready() {
                                self.handle_response(response, session);
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_response(&mut self, response: WorkerResponse, origin: &mut WorkerSession) {
        let Some(token) = self.in_flight.remove(&response.id) else { return; };
        let Some(session) = self.client_sessions.get(&token) else { return; };
        let SessionState::Gathering {
            gathered_responses,
            main_response,
            expected_worker_responses,
            request_type
        } = &mut session.state else { return; };

        gathered_responses.push(response);
        if gathered_responses.len() >= *expected_worker_responses {
            self.finish_session(session);
        }
    }

    fn handle_request(&mut self, request: Request, origin: &mut ClientSession) {
        let request_type = request.request_type.unwrap();
        match request_type {
            RequestType::SaveState(_) => todo!(),
            RequestType::LoadState(_) => todo!(),
            RequestType::ListWorkers(_) => todo!(),
            RequestType::ListFrontends(_) => {
                let response = self.query_main(request_type).ok();
                origin.finish_ok(response);
            }
            RequestType::ListListeners(_) => todo!(),
            RequestType::LaunchWorker(_) => todo!(),
            RequestType::UpgradeMain(_) => todo!(),
            RequestType::UpgradeWorker(_) => todo!(),
            RequestType::SubscribeEvents(_) => todo!(),
            RequestType::ReloadConfiguration(_) => todo!(),
            RequestType::Status(_) => todo!(),
            RequestType::AddCluster(_) => todo!(),
            RequestType::RemoveCluster(_) => todo!(),
            RequestType::AddHttpFrontend(_) => todo!(),
            RequestType::RemoveHttpFrontend(_) => todo!(),
            RequestType::AddHttpsFrontend(_) => todo!(),
            RequestType::RemoveHttpsFrontend(_) => todo!(),
            RequestType::AddCertificate(_) => todo!(),
            RequestType::ReplaceCertificate(_) => todo!(),
            RequestType::RemoveCertificate(_) => todo!(),
            RequestType::AddTcpFrontend(_) => todo!(),
            RequestType::RemoveTcpFrontend(_) => todo!(),
            RequestType::AddBackend(_) => todo!(),
            RequestType::RemoveBackend(_) => todo!(),
            RequestType::AddHttpListener(_) => todo!(),
            RequestType::AddHttpsListener(_) => todo!(),
            RequestType::AddTcpListener(_) => todo!(),
            RequestType::RemoveListener(_) => todo!(),
            RequestType::ActivateListener(_) => todo!(),
            RequestType::DeactivateListener(_) => todo!(),
            RequestType::QueryClusterById(_) => todo!(),
            RequestType::QueryClustersByDomain(_) => todo!(),
            RequestType::QueryClustersHashes(_) => {
                origin.main_response = self.query_main(request_type).ok();
                self.scatter(origin, request);
            }
            RequestType::QueryMetrics(_) => todo!(),
            RequestType::SoftStop(_) => todo!(),
            RequestType::HardStop(_) => todo!(),
            RequestType::ConfigureMetrics(_) => todo!(),
            RequestType::Logging(_) => todo!(),
            RequestType::ReturnListenSockets(_) => todo!(),
            RequestType::QueryCertificatesFromTheState(_) => todo!(),
            RequestType::QueryCertificatesFromWorkers(_) => todo!(),
            RequestType::CountRequests(_) => todo!(),
        }
    }

    fn finish_session(&mut self, session: &mut ClientSession) {
        let SessionState::Gathering {
            gathered_responses,
            main_response,
            expected_worker_responses,
            request_type
        } = &mut session.state else { return; };
        match request_type {
            RequestType::SaveState(_) => todo!(),
            RequestType::LoadState(_) => todo!(),
            RequestType::ListWorkers(_) => todo!(),
            RequestType::ListFrontends(_) => todo!(),
            RequestType::ListListeners(_) => todo!(),
            RequestType::LaunchWorker(_) => todo!(),
            RequestType::UpgradeMain(_) => todo!(),
            RequestType::UpgradeWorker(_) => todo!(),
            RequestType::SubscribeEvents(_) => todo!(),
            RequestType::ReloadConfiguration(_) => todo!(),
            RequestType::Status(_) => todo!(),
            RequestType::AddCluster(_) => todo!(),
            RequestType::RemoveCluster(_) => todo!(),
            RequestType::AddHttpFrontend(_) => todo!(),
            RequestType::RemoveHttpFrontend(_) => todo!(),
            RequestType::AddHttpsFrontend(_) => todo!(),
            RequestType::RemoveHttpsFrontend(_) => todo!(),
            RequestType::AddCertificate(_) => todo!(),
            RequestType::ReplaceCertificate(_) => todo!(),
            RequestType::RemoveCertificate(_) => todo!(),
            RequestType::AddTcpFrontend(_) => todo!(),
            RequestType::RemoveTcpFrontend(_) => todo!(),
            RequestType::AddBackend(_) => todo!(),
            RequestType::RemoveBackend(_) => todo!(),
            RequestType::AddHttpListener(_) => todo!(),
            RequestType::AddHttpsListener(_) => todo!(),
            RequestType::AddTcpListener(_) => todo!(),
            RequestType::RemoveListener(_) => todo!(),
            RequestType::ActivateListener(_) => todo!(),
            RequestType::DeactivateListener(_) => todo!(),
            RequestType::QueryClusterById(_) => todo!(),
            RequestType::QueryClustersByDomain(_) => todo!(),
            RequestType::QueryClustersHashes(_) => todo!(),
            RequestType::QueryMetrics(_) => todo!(),
            RequestType::SoftStop(_) => todo!(),
            RequestType::HardStop(_) => todo!(),
            RequestType::ConfigureMetrics(_) => todo!(),
            RequestType::Logging(_) => todo!(),
            RequestType::ReturnListenSockets(_) => todo!(),
            RequestType::QueryCertificatesFromTheState(_) => todo!(),
            RequestType::QueryCertificatesFromWorkers(_) => todo!(),
            RequestType::CountRequests(_) => todo!(),
        }
    }

    fn scatter(&mut self, client: &mut ClientSession, request: Request) {
        let mut worker_count = 0;
        for worker in self
            .worker_sessions
            .values()
            .filter(|w| w.run_state == RunState::Running)
        {
            worker_count += 1;
            let req_id = format!("{}-query-{}", client.id, worker.id);
            self.in_flight.insert(req_id, client.token);
            worker
                .channel
                .write_message(&WorkerRequest {
                    id: req_id,
                    content: request,
                })
                .expect("could not send request on worker");
        }
        client.expected_worker_responses = worker_count;
        client.request_type = request.request_type.unwrap();
    }

    fn query_main(&self, request: RequestType) -> Result<ResponseContent, ()> {
        todo!()
    }
}

// trait Command {
//     fn on_request(ctx: &mut CommandServer, request: Request, origin: &mut ClientSession);
//     fn on_finish() {
//         unreachable!()
//     }
// }

// struct Status {}

// impl Command for Status {
//     fn on_request(ctx: &mut CommandServer, request: Request, origin: &mut ClientSession) {
//         ctx.scatter(client, request);
//     }
// }
