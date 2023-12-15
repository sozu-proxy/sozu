use std::{
    collections::{HashMap, HashSet},
    fs,
    ops::{Deref, DerefMut},
    path::PathBuf,
};

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

mod requests;

/// This trait is used for command that need to wait for worker responses
pub trait GatheringCallback: std::fmt::Debug {
    /// This is called once every worker has answered
    /// It allows to operate both on the server (lauch workers...) and the client (send an answer...)
    fn gather(
        &mut self,
        server: &mut Server,
        client: &mut ClientSession,
        gathered_responses: GatheredResponses,
    );
}

#[derive(Debug)]
pub struct GatheredResponses {
    pub worker_responses: Vec<WorkerResponse>,
    pub main_response: Option<ResponseContent>,
    pub expected_worker_responses: usize,
}

#[derive(Debug)]
pub enum SessionState {
    WaitingForRequest,
    HandlingRequest,
    Gathering {
        responses: GatheredResponses,
        callback: Box<dyn GatheringCallback>,
    },
    Finishing,
}

#[derive(Debug)]
enum ClientResult {
    NothingToDo,
    NewRequest(Request),
    CloseSession,
}

#[derive(Debug)]
pub struct ClientSession {
    pub channel: Channel<Response, Request>,
    pub id: u32,
    pub token: Token,
    pub state: SessionState,
}

impl ClientSession {
    fn new(mut channel: Channel<Response, Request>, id: u32, token: Token) -> Self {
        channel.interest = Ready::READABLE | Ready::ERROR | Ready::HUP;
        Self {
            channel,
            id,
            token,
            state: SessionState::WaitingForRequest,
        }
    }
    fn finish(&mut self, response: Response) {
        println!("Writing message on client channel");
        self.channel.write_message(&response).unwrap();
        self.state = SessionState::Finishing;
    }

    fn finish_ok(&mut self, content: Option<ResponseContent>) {
        self.finish(Response {
            status: ResponseStatus::Ok.into(),
            message: "Successfully handled request".to_owned(),
            content,
        })
    }

    fn return_processing<S: Into<String>>(&mut self, message: S) {
        let message = message.into();
        println!("Return: {}", message);
        let message = Response {
            status: ResponseStatus::Processing.into(),
            message,
            content: None,
        };
        self.channel.write_message(&message).unwrap()
    }

    fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    fn ready(&mut self) -> ClientResult {
        if self.channel.readiness.is_error() || self.channel.readiness.is_hup() {
            return ClientResult::CloseSession;
        }

        println!("Running: {:?}", self.token);
        let status = self.channel.run();
        println!("{status:?}");
        if let Err(error) = status {
            error!("Error handling client: {:?}", error);
            return ClientResult::NothingToDo;
        }

        match self.state {
            SessionState::WaitingForRequest => {
                let message = self.channel.read_message();
                println!("Client got request: {message:?}");
                match message {
                    Ok(request) => {
                        self.channel.interest = Ready::WRITABLE | Ready::ERROR | Ready::HUP;
                        self.state = SessionState::HandlingRequest;
                        ClientResult::NewRequest(request)
                    }
                    Err(error) => {
                        error!("Client channel read: {:?}", error);
                        ClientResult::NothingToDo
                    }
                }
            }
            SessionState::Finishing => {
                if self.channel.back_buf.available_data() == 0 {
                    self.channel.interest = Ready::READABLE | Ready::ERROR | Ready::HUP;
                    self.state = SessionState::WaitingForRequest;
                }
                ClientResult::NothingToDo
            }
            _ => ClientResult::NothingToDo,
        }
    }
}

#[derive(Debug)]
enum WorkerResult {
    NothingToDo,
    NewResponse(WorkerResponse),
    CloseSession,
}

#[derive(Debug)]
struct WorkerSession {
    pub channel: Channel<WorkerRequest, WorkerResponse>,
    pub id: u32,
    pub pid: pid_t,
    pub run_state: RunState,
    pub token: Token,
}

impl WorkerSession {
    fn new(
        mut channel: Channel<WorkerRequest, WorkerResponse>,
        id: u32,
        pid: pid_t,
        token: Token,
    ) -> Self {
        channel.interest = Ready::READABLE | Ready::WRITABLE | Ready::ERROR | Ready::HUP;
        Self {
            channel,
            id,
            pid,
            run_state: RunState::Running,
            token,
        }
    }

    fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    fn ready(&mut self) -> WorkerResult {
        if self.channel.readiness.is_error() || self.channel.readiness.is_hup() {
            return WorkerResult::CloseSession;
        }

        let status = self.channel.run();
        println!("{status:?}");
        let message = self.channel.read_message();
        println!("{message:?}");
        match message {
            Ok(message) => WorkerResult::NewResponse(message),
            Err(_) => WorkerResult::NothingToDo,
        }
    }
}

#[derive(Debug)]
pub struct Server {
    in_flight: HashMap<String, Token>,
    next_client_id: u32,
    next_session_token: usize,
    poll: Poll,
    sessions_to_tick: HashSet<Token>,
    unix_listener: UnixListener,
    worker_sessions: HashMap<Token, WorkerSession>,
}

#[derive(Debug)]
struct CommandHub {
    server: Server,
    clients: HashMap<Token, ClientSession>,
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

    let mut command_server = CommandHub::new(unix_listener)?;
    for mut worker in workers {
        command_server.register_worker(worker.id, worker.pid, worker.worker_channel.take().unwrap())
    }
    command_server.run();
}

impl Deref for CommandHub {
    type Target = Server;

    fn deref(&self) -> &Self::Target {
        &self.server
    }
}
impl DerefMut for CommandHub {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.server
    }
}

impl CommandHub {
    fn new(unix_listener: UnixListener) -> anyhow::Result<Self> {
        Ok(Self {
            server: Server::new(unix_listener)?,
            clients: HashMap::new(),
        })
    }

    fn get_client_mut(&mut self, token: &Token) -> Option<(&mut Server, &mut ClientSession)> {
        let Self {
            server,
            clients: client_sessions,
        } = self;
        if let Some(client) = client_sessions.get_mut(token) {
            Some((server, client))
        } else {
            None
        }
    }

    fn run(&mut self) -> ! {
        let mut events = Events::with_capacity(100);
        println!("{self:#?}");

        loop {
            println!("Sessions to tick: {:?}", self.sessions_to_tick);
            let CommandHub {
                server:
                    Server {
                        worker_sessions,
                        sessions_to_tick,
                        ..
                    },
                clients,
            } = self;
            for token in sessions_to_tick.drain() {
                if let Some(client) = clients.get_mut(&token) {
                    client.ready();
                }
                if let Some(worker) = worker_sessions.get_mut(&token) {
                    worker.ready();
                }
            }

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
                                let session = ClientSession::new(channel, id, token);
                                info!("{:#?}", session);
                                self.clients.insert(token, session);
                            }
                        }
                    }
                    token => {
                        info!("{:?} got event: {:?}", token, event);
                        if let Some((server, client)) = self.get_client_mut(&token) {
                            client.update_readiness(Ready::from(event));
                            match client.ready() {
                                ClientResult::NothingToDo => {}
                                ClientResult::NewRequest(request) => {
                                    server.handle_request(request, client);
                                }
                                ClientResult::CloseSession => {
                                    println!("Closing client {client:#?}");
                                    self.clients.remove(&token);
                                }
                            }
                        }
                        if let Some(worker) = self.worker_sessions.get_mut(&token) {
                            worker.update_readiness(Ready::from(event));
                            match worker.ready() {
                                WorkerResult::NothingToDo => {}
                                WorkerResult::NewResponse(response) => {
                                    self.handle_response(response);
                                }
                                WorkerResult::CloseSession => {
                                    self.worker_sessions.remove(&token);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_response(&mut self, response: WorkerResponse) {
        let Some(token) = self.in_flight.remove(&response.id) else { return; };
        let Some((server, client)) = self.get_client_mut(&token) else { return; };
        let SessionState::Gathering { responses, .. } = &mut client.state else { return; };

        responses.worker_responses.push(response);
        if responses.worker_responses.len() >= responses.expected_worker_responses {
            let SessionState::Gathering {
                responses,
                mut callback
            } = std::mem::replace(&mut client.state, SessionState::Finishing) else { return; };
            server.tick_later(client.token);
            callback.gather(server, client, responses);
        }
    }
}

impl Server {
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
            in_flight: HashMap::new(),
            next_client_id: 0,
            next_session_token: 1,
            poll,
            unix_listener,
            worker_sessions: HashMap::new(),
            sessions_to_tick: HashSet::new(),
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
        self.worker_sessions
            .insert(token, WorkerSession::new(channel, id, pid, token));
    }

    fn handle_request(&mut self, request: Request, client: &mut ClientSession) {
        let request_type = request.request_type.unwrap();
        match request_type {
            RequestType::SaveState(_) => todo!(),
            RequestType::LoadState(_) => todo!(),
            RequestType::ListWorkers(_) => todo!(),
            RequestType::ListFrontends(inner) => {
                requests::list_frontend_command(self, client, inner);
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
            RequestType::QueryClustersHashes(inner) => {
                requests::query_cluster_hashes(self, client, inner);
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

        self.tick_later(client.token);
    }

    fn scatter<S: Into<String>>(
        &mut self,
        message: S,
        client: &mut ClientSession,
        request: Request,
        callback: Box<dyn GatheringCallback>,
    ) {
        client.return_processing(message);
        let mut worker_count = 0;
        let mut worker_request = WorkerRequest {
            id: String::new(),
            content: request,
        };
        let Server {
            worker_sessions,
            in_flight,
            sessions_to_tick,
            ..
        } = self;

        for worker in worker_sessions
            .values_mut()
            .filter(|w| w.run_state == RunState::Running)
        {
            worker_count += 1;
            worker_request.id = format!("{}-query-{}", client.id, worker.id);
            in_flight.insert(worker_request.id.clone(), client.token);
            println!("Sending to worker: {worker_request:?}");
            worker
                .channel
                .write_message(&worker_request)
                .expect("could not send request on worker");
            sessions_to_tick.insert(worker.token);
        }

        let request_type = worker_request.content.request_type.unwrap();
        client.state = SessionState::Gathering {
            responses: GatheredResponses {
                worker_responses: Vec::with_capacity(worker_count),
                main_response: self.query_main(&request_type).unwrap(),
                expected_worker_responses: worker_count,
            },
            callback,
        };
    }

    fn query_main(&self, request: &RequestType) -> Result<Option<ResponseContent>, ()> {
        Ok(None)
    }

    fn tick_later(&mut self, token: Token) {
        self.sessions_to_tick.insert(token);
    }
}
