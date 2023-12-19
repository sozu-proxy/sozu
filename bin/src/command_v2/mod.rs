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
        request::RequestType, response_content::ContentType, ClusterHashes, ClusterInformations,
        Request, Response, ResponseContent, ResponseStatus, RunState,
    },
    ready::Ready,
    request::WorkerRequest,
    response::WorkerResponse,
    state::ConfigState,
};

use crate::worker::Worker;

use self::requests::load_static_config;

mod requests;

///
#[allow(unused)]
pub trait Gatherer {
    fn inc_expected_responses(&mut self, count: usize);

    /// Aggregate a response, return true if enough has been gathered
    fn on_message(
        &mut self,
        server: &mut Server,
        client: &mut ClientSession,
        worker_id: u32,
        message: WorkerResponse,
    ) -> bool {
        unimplemented!("The parent task expected no client");
    }

    /// same as on_message but if no client is involved (a CLI, a connector…)
    fn on_message_no_client(
        &mut self,
        server: &mut Server,
        worker_id: u32,
        message: WorkerResponse,
    ) -> bool {
        unimplemented!("The parent task expected a client");
    }
}

/// This trait is used for command that need to wait for worker responses
#[allow(unused)]
pub trait GatheringTask: std::fmt::Debug {
    fn client_token(&self) -> Option<Token>;
    fn get_gatherer(&mut self) -> &mut dyn Gatherer;
    /// This is called once every worker has answered
    /// It allows to operate both on the server (lauch workers...) and the client (send an answer...)
    fn on_finish(self: Box<Self>, server: &mut Server, client: &mut ClientSession) {}
    fn on_finish_no_client(self: Box<Self>, server: &mut Server) {}
}

#[derive(Debug, Default)]
pub struct DefaultGatherer {
    pub responses: Vec<(u32, WorkerResponse)>,
    pub expected_responses: usize,
}

#[allow(unused)]
impl Gatherer for DefaultGatherer {
    fn inc_expected_responses(&mut self, count: usize) {
        self.expected_responses += count;
    }

    fn on_message(
        &mut self,
        server: &mut Server,
        client: &mut ClientSession,
        worker_id: u32,
        message: WorkerResponse,
    ) -> bool {
        self.responses.push((worker_id, message));
        self.responses.len() >= self.expected_responses
    }

    fn on_message_no_client(
        &mut self,
        server: &mut Server,
        worker_id: u32,
        message: WorkerResponse,
    ) -> bool {
        self.responses.push((worker_id, message));
        self.responses.len() >= self.expected_responses
    }
}

#[derive(Debug)]
enum ClientResult {
    NothingToDo,
    NewRequest(Request),
    CloseSession,
}

/// Follows a client request from start to finish
#[derive(Debug)]
pub struct ClientSession {
    pub channel: Channel<Response, Request>,
    pub id: u32,
    pub token: Token,
}

impl ClientSession {
    fn new(mut channel: Channel<Response, Request>, id: u32, token: Token) -> Self {
        channel.interest = Ready::READABLE | Ready::ERROR | Ready::HUP;
        Self { channel, id, token }
    }

    fn finish(&mut self, response: Response) {
        println!("Writing message on client channel");
        self.channel.write_message(&response).unwrap();
        self.channel.interest.insert(Ready::WRITABLE);
    }

    fn finish_ok(&mut self, content: Option<ResponseContent>) {
        self.finish(Response {
            status: ResponseStatus::Ok.into(),
            message: "Successfully handled request".to_owned(),
            content,
        })
    }

    fn finish_failure<T: Into<String>>(&mut self, error: T) {
        self.finish(Response {
            status: ResponseStatus::Failure.into(),
            message: error.into(),
            content: None,
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
        self.channel.write_message(&message).unwrap();
        self.channel.interest.insert(Ready::WRITABLE);
    }

    fn update_readiness(&mut self, events: Ready) {
        self.channel.handle_events(events);
    }

    fn ready(&mut self) -> ClientResult {
        if self.channel.readiness.is_error() || self.channel.readiness.is_hup() {
            return ClientResult::CloseSession;
        }

        println!(
            "Running: {:?}, readiness: {:?}, interest: {:?}",
            self.token, self.channel.readiness, self.channel.interest
        );
        let status = self.channel.run();
        println!("{status:?}");
        if let Err(error) = status {
            error!("Error handling client: {:?}", error);
            return ClientResult::NothingToDo;
        }

        let message = self.channel.read_message();
        println!("Client got request: {message:?}");
        match message {
            Ok(request) => ClientResult::NewRequest(request),
            Err(error) => {
                error!("Client channel read: {:?}", error);
                ClientResult::NothingToDo
            }
        }
    }
}

#[derive(Debug)]
enum WorkerResult {
    NothingToDo,
    NewResponses(Vec<WorkerResponse>),
    CloseSession,
}

/// Follow a worker thoughout its lifetime (launching, communitation, softstop/hardstop)
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
        channel.interest = Ready::READABLE | Ready::ERROR | Ready::HUP;
        Self {
            channel,
            id,
            pid,
            run_state: RunState::Running,
            token,
        }
    }

    fn send(&mut self, message: &WorkerRequest) {
        println!("Sending to worker: {message:?}");
        self.channel.write_message(message).unwrap();
        self.channel.interest.insert(Ready::WRITABLE);
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
        let mut responses = Vec::new();
        while let Ok(response) = self.channel.read_message() {
            responses.push(response);
        }
        println!("{responses:?}");
        if responses.is_empty() {
            return WorkerResult::NothingToDo;
        }

        WorkerResult::NewResponses(responses)
    }
}

/// Manages workers
#[derive(Debug)]
pub struct Server {
    in_flight: HashMap<String, usize>,
    next_client_id: u32,
    next_session_token: usize,
    next_task_id: usize,
    poll: Poll,
    queued_tasks: HashMap<usize, Box<dyn GatheringTask>>,
    sessions_to_tick: HashSet<Token>,
    unix_listener: UnixListener,
    workers: HashMap<Token, WorkerSession>,
    state: ConfigState,
    frontends_count: usize,
    backends_count: usize,
    config: Config,
}

/// A platform to receive client connections, pass orders to workers,
/// gather data, etc.
#[derive(Debug)]
struct CommandHub {
    server: Server,
    clients: HashMap<Token, ClientSession>,
    tasks: HashMap<usize, Box<dyn GatheringTask>>,
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

    let mut command_hub = CommandHub::new(unix_listener, config)?;
    for mut worker in workers {
        command_hub.register_worker(worker.id, worker.pid, worker.worker_channel.take().unwrap())
    }

    load_static_config(&mut command_hub.server);

    command_hub.run();
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
    fn new(unix_listener: UnixListener, config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            server: Server::new(unix_listener, config)?,
            clients: HashMap::new(),
            tasks: HashMap::new(),
        })
    }

    fn get_client_mut(&mut self, token: &Token) -> Option<(&mut Server, &mut ClientSession)> {
        let Self {
            server, clients, ..
        } = self;
        if let Some(client) = clients.get_mut(token) {
            Some((server, client))
        } else {
            None
        }
    }

    fn run(&mut self) -> ! {
        let mut events = Events::with_capacity(100);
        println!("{self:#?}");

        loop {
            let CommandHub {
                server:
                    Server {
                        queued_tasks,
                        sessions_to_tick,
                        workers,
                        ..
                    },
                tasks,
                clients,
            } = self;
            tasks.extend(queued_tasks.drain());

            println!("Sessions to tick: {:?}", sessions_to_tick);
            for token in sessions_to_tick.drain() {
                if let Some(client) = clients.get_mut(&token) {
                    client.ready();
                } else if let Some(worker) = workers.get_mut(&token) {
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
                                    server.handle_request(client, request);
                                }
                                ClientResult::CloseSession => {
                                    println!("Closing client {client:#?}");
                                    self.clients.remove(&token);
                                }
                            }
                        } else if let Some(worker) = self.workers.get_mut(&token) {
                            worker.update_readiness(Ready::from(event));
                            let worker_id = worker.id;
                            match worker.ready() {
                                WorkerResult::NothingToDo => {}
                                WorkerResult::NewResponses(responses) => {
                                    for response in responses {
                                        self.handle_response(worker_id, response);
                                    }
                                }
                                WorkerResult::CloseSession => {
                                    self.workers.remove(&token);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_response(&mut self, worker_id: u32, response: WorkerResponse) {
        let Some(task_id) = self.in_flight.get(&response.id).copied() else {
            println!("==========HANDLE_RESPONSE not in flight: {}", response);
            return;
        };
        let CommandHub {
            server,
            clients,
            tasks,
        } = self;
        let task = tasks.get_mut(&task_id).unwrap();

        if let Some(token) = task.client_token() {
            let client = clients.get_mut(&token).unwrap();
            let is_finished = task
                .get_gatherer()
                .on_message(server, client, worker_id, response);
            if is_finished {
                let task = self.tasks.remove(&task_id).unwrap();
                task.on_finish(server, client);
                self.in_flight.retain(|_, val| *val != task_id);
            }
            self.tick_later(token);
        } else {
            let is_finished =
                task.get_gatherer()
                    .on_message_no_client(&mut self.server, worker_id, response);
            if is_finished {
                let task = self.tasks.remove(&task_id).unwrap();
                task.on_finish_no_client(&mut self.server);
                self.in_flight.retain(|_, val| *val != task_id);
            }
        }
    }
}

impl Server {
    fn new(mut unix_listener: UnixListener, config: Config) -> anyhow::Result<Self> {
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
            next_session_token: 1, // 0 is reserved for the UnixListener
            next_task_id: 0,
            poll,
            state: Default::default(),
            queued_tasks: HashMap::new(),
            sessions_to_tick: HashSet::new(),
            unix_listener,
            workers: HashMap::new(),
            frontends_count: 0,
            backends_count: 0,
            config,
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

    fn next_task_id(&mut self) -> usize {
        let id = self.next_task_id;
        self.next_task_id += 1;
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
        self.workers
            .insert(token, WorkerSession::new(channel, id, pid, token));
    }

    fn handle_request(&mut self, client: &mut ClientSession, request: Request) {
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
            RequestType::AddCluster(_)
            | RequestType::ActivateListener(_)
            | RequestType::AddBackend(_)
            | RequestType::AddCertificate(_)
            | RequestType::AddHttpFrontend(_)
            | RequestType::AddHttpListener(_)
            | RequestType::AddHttpsFrontend(_)
            | RequestType::AddHttpsListener(_)
            | RequestType::AddTcpFrontend(_)
            | RequestType::AddTcpListener(_)
            | RequestType::DeactivateListener(_)
            | RequestType::RemoveBackend(_)
            | RequestType::RemoveCertificate(_)
            | RequestType::RemoveCluster(_)
            | RequestType::RemoveHttpFrontend(_)
            | RequestType::RemoveHttpsFrontend(_)
            | RequestType::RemoveListener(_)
            | RequestType::RemoveTcpFrontend(_)
            | RequestType::ReplaceCertificate(_) => {
                requests::worker_request(self, client, request_type);
            }
            RequestType::QueryClustersHashes(_)
            | RequestType::QueryClustersByDomain(_)
            | RequestType::QueryClusterById(_) => {
                requests::query_clusters(self, client, request_type);
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

    fn query_main(&self, request: &RequestType) -> Result<Option<ResponseContent>, ()> {
        match request {
            // RequestType::SaveState(_) => todo!(),
            // RequestType::LoadState(_) => todo!(),
            // RequestType::ListWorkers(_) => todo!(),
            // RequestType::ListFrontends(_) => todo!(),
            // RequestType::ListListeners(_) => todo!(),
            // RequestType::LaunchWorker(_) => todo!(),
            // RequestType::UpgradeMain(_) => todo!(),
            // RequestType::UpgradeWorker(_) => todo!(),
            // RequestType::SubscribeEvents(_) => todo!(),
            // RequestType::ReloadConfiguration(_) => todo!(),
            // RequestType::Status(_) => todo!(),
            // RequestType::AddCluster(_) => todo!(),
            // RequestType::RemoveCluster(_) => todo!(),
            // RequestType::AddHttpFrontend(_) => todo!(),
            // RequestType::RemoveHttpFrontend(_) => todo!(),
            // RequestType::AddHttpsFrontend(_) => todo!(),
            // RequestType::RemoveHttpsFrontend(_) => todo!(),
            // RequestType::AddCertificate(_) => todo!(),
            // RequestType::ReplaceCertificate(_) => todo!(),
            // RequestType::RemoveCertificate(_) => todo!(),
            // RequestType::AddTcpFrontend(_) => todo!(),
            // RequestType::RemoveTcpFrontend(_) => todo!(),
            // RequestType::AddBackend(_) => todo!(),
            // RequestType::RemoveBackend(_) => todo!(),
            // RequestType::AddHttpListener(_) => todo!(),
            // RequestType::AddHttpsListener(_) => todo!(),
            // RequestType::AddTcpListener(_) => todo!(),
            // RequestType::RemoveListener(_) => todo!(),
            // RequestType::ActivateListener(_) => todo!(),
            // RequestType::DeactivateListener(_) => todo!(),
            RequestType::QueryClusterById(cluster_id) => Ok(Some(
                ContentType::Clusters(ClusterInformations {
                    vec: self.state.cluster_state(cluster_id).into_iter().collect(),
                })
                .into(),
            )),
            RequestType::QueryClustersByDomain(domain) => {
                let cluster_ids = self
                    .state
                    .get_cluster_ids_by_domain(domain.hostname.clone(), domain.path.clone());
                let vec = cluster_ids
                    .iter()
                    .filter_map(|cluster_id| self.state.cluster_state(cluster_id))
                    .collect();
                Ok(Some(
                    ContentType::Clusters(ClusterInformations { vec }).into(),
                ))
            }
            RequestType::QueryClustersHashes(_) => Ok(Some(
                ContentType::ClusterHashes(ClusterHashes {
                    map: self.state.hash_state(),
                })
                .into(),
            )),
            // RequestType::QueryMetrics(_) => todo!(),
            // RequestType::SoftStop(_) => todo!(),
            // RequestType::HardStop(_) => todo!(),
            // RequestType::ConfigureMetrics(_) => todo!(),
            // RequestType::Logging(_) => todo!(),
            // RequestType::ReturnListenSockets(_) => todo!(),
            // RequestType::QueryCertificatesFromTheState(_) => todo!(),
            // RequestType::QueryCertificatesFromWorkers(_) => todo!(),
            // RequestType::CountRequests(_) => todo!(),
            _ => Ok(None),
        }
    }

    fn new_task(&mut self, task: Box<dyn GatheringTask>) -> usize {
        let task_id = self.next_task_id();
        self.queued_tasks.insert(task_id, task);
        task_id
    }

    fn scatter(&mut self, request: Request, task: Box<dyn GatheringTask>) {
        let task_id = self.next_task_id();
        self.queued_tasks.insert(task_id, task);
        self.scatter_on(request, task_id, 0);
    }

    fn scatter_on(&mut self, request: Request, task_id: usize, request_id: usize) {
        let Server {
            workers,
            queued_tasks,
            sessions_to_tick,
            ..
        } = self;

        let task = queued_tasks.get_mut(&task_id).unwrap();

        let mut worker_count = 0;
        let mut worker_request = WorkerRequest {
            id: String::new(),
            content: request,
        };
        for worker in workers
            .values_mut()
            .filter(|w| w.run_state == RunState::Running)
        {
            worker_count += 1;
            worker_request.id = format!("{}-{}-query-{}", task_id, request_id, worker.id);
            worker.send(&worker_request);
            self.in_flight.insert(worker_request.id, task_id);
            sessions_to_tick.insert(worker.token);
        }
        task.get_gatherer().inc_expected_responses(worker_count);
    }

    fn tick_later(&mut self, token: Token) {
        self.sessions_to_tick.insert(token);
    }
}
