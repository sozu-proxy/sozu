use async_dup::Arc;
use futures::channel::{mpsc::*, oneshot};
use futures::{SinkExt, StreamExt};
use futures_lite::io::*;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use async_io::Async;
use blocking::block_on;
use smol::Task;

use sozu_command::command::{
    CommandRequestData, CommandResponse, CommandResponseData, CommandStatus, Event, RunState,
};
use sozu_command::config::Config;
use sozu_command::proxy::{ProxyRequest, ProxyRequestData, ProxyResponseData, ProxyResponseStatus};
use sozu_command::scm_socket::{Listeners, ScmSocket};
use sozu_command::state::ConfigState;

use crate::get_executable_path;
use crate::upgrade::SerializedWorker;
use crate::upgrade::UpgradeData;
use crate::util;
use crate::worker::start_worker;

mod orders;
mod worker;

pub use worker::*;

#[derive(Deserialize, Serialize, Debug)]
pub struct ProxyConfiguration {
    id: String,
    state: ConfigState,
}

/*pub struct CommandServer {
  sock:              UnixListener,
  buffer_size:       usize,
  max_buffer_size:   usize,
  clients:           Slab<CommandClient>,
  workers:           HashMap<Token, Worker>,
  event_subscribers: Vec<FrontToken>,
  next_id:           u32,
  state:             ConfigState,
  pub poll:          Poll,
  config:            Config,
  token_count:       usize,
  must_stop:         bool,
  executable_path:   String,
  //caching the number of backends instead of going through the whole state.backends hashmap
  backends_count:    usize,
  //caching the number of frontends instead of going through the whole state.http/hhtps/tcp_fronts hashmaps
  frontends_count:   usize,
}*/

pub struct CommandServer {
    // file descriptor of the unix listener socket
    fd: i32,
    command_tx: Sender<CommandMessage>,
    command_rx: Receiver<CommandMessage>,
    clients: HashMap<String, Sender<sozu_command::command::CommandResponse>>,
    workers: Vec<Worker>,
    in_flight: HashMap<
        String,
        (
            futures::channel::mpsc::Sender<sozu_command::proxy::ProxyResponse>,
            usize,
        ),
    >,
    event_subscribers: HashSet<String>,
    state: ConfigState,
    config: Config,
    next_id: u32,
    executable_path: String,
    //caching the number of backends instead of going through the whole state.backends hashmap
    backends_count: usize,
    //caching the number of frontends instead of going through the whole state.http/hhtps/tcp_fronts hashmaps
    frontends_count: usize,
    accept_cancel: Option<oneshot::Sender<()>>,
}

impl CommandServer {
    fn new(
        fd: i32,
        config: Config,
        command_tx: Sender<CommandMessage>,
        command_rx: Receiver<CommandMessage>,
        mut workers: Vec<Worker>,
        accept_cancel: oneshot::Sender<()>,
    ) -> Self {
        //FIXME
        if config.metrics.is_some() {
            /*METRICS.with(|metrics| {
              if let Some(sock) = (*metrics.borrow_mut()).socket_mut() {
                poll.registry().register(sock, Token(1), Interest::WRITABLE).expect("should register the metrics socket");
              } else {
                error!("could not register metrics socket");
              }
            });*/
        }

        let state: ConfigState = Default::default();

        for worker in workers.iter_mut() {
            let sock = worker.channel.take().unwrap().sock;
            let (worker_tx, worker_rx) = channel(10000);
            worker.sender = Some(worker_tx);

            let stream = Async::new(unsafe {
                let fd = sock.into_raw_fd();
                UnixStream::from_raw_fd(fd)
            })
            .unwrap();

            let id = worker.id;
            let command_tx = command_tx.clone();
            Task::spawn(async move {
                worker_loop(id, stream, command_tx, worker_rx)
                    .await
                    .unwrap();
            })
            .detach();
        }

        let next_id = workers.len() as u32;
        let executable_path = unsafe { get_executable_path() };
        let backends_count = state.count_backends();
        let frontends_count = state.count_frontends();

        CommandServer {
            fd,
            config,
            state,
            command_tx,
            command_rx,
            clients: HashMap::new(),
            workers,
            event_subscribers: HashSet::new(),
            in_flight: HashMap::new(),
            next_id,
            executable_path,
            backends_count,
            frontends_count,
            accept_cancel: Some(accept_cancel),
        }
    }

    pub fn generate_upgrade_data(&self) -> UpgradeData {
        let workers: Vec<SerializedWorker> = self
            .workers
            .iter()
            .map(|ref worker| SerializedWorker::from_worker(worker))
            .collect();
        //FIXME: ensure there's at least one worker
        let state = self.state.clone();

        UpgradeData {
            command: self.fd,
            config: self.config.clone(),
            workers,
            state,
            next_id: self.next_id,
            //token_count: self.token_count,
        }
    }

    pub fn from_upgrade_data(upgrade_data: UpgradeData) -> CommandServer {
        let UpgradeData {
            command,
            config,
            workers,
            state,
            next_id,
        } = upgrade_data;

        debug!("listener is: {}", command);
        let listener = Async::new(unsafe { UnixListener::from_raw_fd(command) }).unwrap();

        let (accept_cancel_tx, accept_cancel_rx) = oneshot::channel();
        let (command_tx, command_rx) = channel(10000);
        let mut tx = command_tx.clone();

        Task::spawn(async move {
            let mut counter = 0usize;
            let mut accept_cancel_rx = Some(accept_cancel_rx);
            loop {
                /*let (stream, _) = match futures::future::select(
                accept_cancel_rx.take().unwrap(),
                listener.read_with(|l| l.accept())
                ).await {
                */
                let f = listener.accept();
                futures::pin_mut!(f);
                let (stream, _) =
                    match futures::future::select(accept_cancel_rx.take().unwrap(), f).await {
                        futures::future::Either::Left((_canceled, _)) => {
                            info!("stopping listener");
                            break;
                        }
                        futures::future::Either::Right((res, cancel_rx)) => {
                            accept_cancel_rx = Some(cancel_rx);
                            res.unwrap()
                        }
                    };
                println!("Accepted a client from upgraded");

                let (client_tx, client_rx) = channel(10000);
                let id = format!("CL-up-{}", counter);
                Task::spawn(client(id.clone(), stream, tx.clone(), client_rx)).detach();
                tx.send(CommandMessage::ClientNew {
                    id,
                    sender: client_tx,
                })
                .await
                .unwrap();
                counter += 1;
            }
        })
        .detach();

        let tx = command_tx.clone();

        let workers: Vec<Worker> = workers
            .iter()
            .filter_map(move |serialized| {
                if serialized.run_state == RunState::Stopped
                    || serialized.run_state == RunState::Stopping
                {
                    return None;
                }

                let (worker_tx, worker_rx) = channel(10000);
                let sender = Some(worker_tx);

                println!("deserializing worker: {:?}", serialized);
                let stream = Async::new(unsafe { UnixStream::from_raw_fd(serialized.fd) }).unwrap();

                let id = serialized.id;
                let command_tx = tx.clone();
                //async fn worker(id: u32, sock: Async<UnixStream>, tx: Sender<CommandMessage>, rx: Receiver<()>) -> std::io::Result<()> {
                Task::spawn(async move {
                    worker_loop(id, stream, command_tx, worker_rx)
                        .await
                        .unwrap();
                })
                .detach();

                Some(Worker {
                    fd: serialized.fd,
                    id: serialized.id,
                    channel: None,
                    sender,
                    pid: serialized.pid,
                    run_state: serialized.run_state.clone(),
                    queue: serialized.queue.clone().into(),
                    scm: ScmSocket::new(serialized.scm),
                })
            })
            .collect();

        let config_state = state.clone();

        let backends_count = config_state.count_backends();
        let frontends_count = config_state.count_frontends();

        let executable_path = unsafe { get_executable_path() };

        CommandServer {
            fd: command,
            config,
            state,
            command_tx,
            command_rx,
            clients: HashMap::new(),
            workers,
            event_subscribers: HashSet::new(),
            in_flight: HashMap::new(),
            next_id,
            executable_path,
            backends_count,
            frontends_count,
            accept_cancel: Some(accept_cancel_tx),
        }
    }

    pub fn disable_cloexec_before_upgrade(&mut self) {
        for ref mut worker in self.workers.iter_mut() {
            if worker.run_state == RunState::Running {
                util::disable_close_on_exec(worker.fd);
            }
        }
        trace!("disabling cloexec on listener: {}", self.fd);
        util::disable_close_on_exec(self.fd);
    }

    pub fn enable_cloexec_after_upgrade(&mut self) {
        for ref mut worker in self.workers.iter_mut() {
            if worker.run_state == RunState::Running {
                util::enable_close_on_exec(worker.fd);
            }
        }
        util::enable_close_on_exec(self.fd);
    }

    pub async fn load_static_application_configuration(&mut self) {
        //FIXME: too many loops, this could be cleaner
        for message in self.config.generate_config_messages() {
            if let CommandRequestData::Proxy(order) = message.data {
                self.state.handle_order(&order);

                if let &ProxyRequestData::AddCertificate(_) = &order {
                    debug!("config generated AddCertificate( ... )");
                } else {
                    debug!("config generated {:?}", order);
                }
                let mut found = false;
                for ref mut worker in self.workers.iter_mut().filter(|worker| {
                    worker.run_state != RunState::Stopping && worker.run_state != RunState::Stopped
                }) {
                    worker.send(message.id.clone(), order.clone()).await;
                    found = true;
                }

                if !found {
                    // FIXME: should send back error here
                    error!("no worker found");
                }
            }
        }

        self.backends_count = self.state.count_backends();
        self.frontends_count = self.state.count_frontends();
        gauge!("configuration.applications", self.state.applications.len());
        gauge!("configuration.backends", self.backends_count);
        gauge!("configuration.frontends", self.frontends_count);
    }

    pub async fn check_worker_status(&mut self, worker_id: u32) {
        let ref mut worker = self
            .workers
            .get_mut(worker_id as usize)
            .expect("there should be a worker at that token");
        let res = kill(Pid::from_raw(worker.pid), None);

        if let Ok(()) = res {
            if worker.run_state == RunState::Running {
                error!(
                    "worker process {} (PID = {}) not answering, killing and replacing",
                    worker.id, worker.pid
                );
                if let Err(e) = kill(Pid::from_raw(worker.pid), Signal::SIGKILL) {
                    error!("failed to kill the worker process: {:?}", e);
                } else {
                    worker.run_state = RunState::Stopped;
                }
            } else {
                return;
            }
        } else {
            if worker.run_state == RunState::Running {
                error!(
                    "worker process {} (PID = {}) stopped running, replacing",
                    worker.id, worker.pid
                );
            } else if worker.run_state == RunState::Stopping {
                info!(
                    "worker process {} (PID = {}) not detected, assuming it stopped",
                    worker.id, worker.pid
                );
                worker.run_state = RunState::Stopped;
                return;
            } else {
                error!("failed to check process status: {:?}", res);
                return;
            }
        }

        worker.run_state = RunState::Stopped;

        if self.config.worker_automatic_restart {
            incr!("worker_restart");

            let id = self.next_id;
            let listeners = Some(Listeners {
                http: Vec::new(),
                tls: Vec::new(),
                tcp: Vec::new(),
            });

            if let Ok(mut worker) = start_worker(
                id,
                &self.config,
                self.executable_path.clone(),
                &self.state,
                listeners,
            ) {
                info!("created new worker: {}", id);
                self.next_id += 1;

                let sock = worker.channel.take().unwrap().sock;
                let (worker_tx, worker_rx) = channel(10000);
                worker.sender = Some(worker_tx);

                let stream = Async::new(unsafe {
                    let fd = sock.into_raw_fd();
                    UnixStream::from_raw_fd(fd)
                })
                .unwrap();

                let id = worker.id;
                let command_tx = self.command_tx.clone();
                Task::spawn(async move {
                    worker_loop(id, stream, command_tx, worker_rx)
                        .await
                        .unwrap();
                })
                .detach();

                let mut count = 0usize;
                let mut orders = self.state.generate_activate_orders();
                for order in orders.drain(..) {
                    if let Err(e) = worker
                        .sender
                        .as_mut()
                        .unwrap()
                        .send(ProxyRequest {
                            id: format!("RESTART-{}-ACTIVATE-{}", id, count),
                            order,
                        })
                        .await {
                            error!("could not send activate order to worker {:?}: {:?}", worker.id, e);
                        }
                    count += 1;
                }

                if let Err(e) = worker
                    .sender
                    .as_mut()
                    .unwrap()
                    .send(ProxyRequest {
                        id: format!("RESTART-{}-STATUS", id),
                        order: ProxyRequestData::Status,
                    })
                .await{
                    error!("could not send status message to worker {:?}: {:?}", worker.id, e);
                }

                self.workers.push(worker);
            }
        }
    }
}

pub fn start(
    config: Config,
    command_socket_path: String,
    workers: Vec<Worker>,
) -> std::io::Result<()> {
    let addr = PathBuf::from(&command_socket_path);
    if let Err(e) = fs::remove_file(&addr) {
        match e.kind() {
            ErrorKind::NotFound => {}
            _ => {
                error!("could not delete previous socket at {:?}: {:?}", addr, e);
                return Ok(());
            }
        }
    }

    let srv = match UnixListener::bind(&addr) {
        Err(e) => {
            error!("could not create unix socket: {:?}", e);
            // the workers did not even get the configuration, we can kill them right away
            for worker in workers {
                error!("killing worker n°{} (PID {})", worker.id, worker.pid);
                let _ = kill(Pid::from_raw(worker.pid), Signal::SIGKILL).map_err(|e| {
                    error!("could not kill worker: {:?}", e);
                });
            }
            return Ok(());
        }
        Ok(srv) => {
            if let Err(e) = fs::set_permissions(&addr, fs::Permissions::from_mode(0o600)) {
                error!("could not set the unix socket permissions: {:?}", e);
                let _ = fs::remove_file(&addr).map_err(|e2| {
                    error!("could not remove the unix socket: {:?}", e2);
                });
                // the workers did not even get the configuration, we can kill them right away
                for worker in workers {
                    error!("killing worker n°{} (PID {})", worker.id, worker.pid);
                    let _ = kill(Pid::from_raw(worker.pid), Signal::SIGKILL).map_err(|e| {
                        error!("could not kill worker: {:?}", e);
                    });
                }
                return Ok(());
            }
            srv
        }
    };

    block_on(async {
        // Create a listener.
        let fd = srv.as_raw_fd();
        let listener = Async::new(srv)?;
        info!("Listening on {:?}", listener.get_ref().local_addr()?);

        let mut counter = 0usize;
        let (accept_cancel_tx, accept_cancel_rx) = oneshot::channel();
        let (mut command_tx, command_rx) = channel(10000);
        let tx = command_tx.clone();
        Task::spawn(async move {
            let mut accept_cancel_rx = Some(accept_cancel_rx);
            loop {
                let f = listener.accept();
                futures::pin_mut!(f);
                let (stream, _) =
                    match futures::future::select(accept_cancel_rx.take().unwrap(), f).await {
                        futures::future::Either::Left((_canceled, _)) => {
                            info!("stopping listener");
                            break;
                        }
                        futures::future::Either::Right((res, cancel_rx)) => {
                            accept_cancel_rx = Some(cancel_rx);
                            res.unwrap()
                        }
                    };

                println!("Accepted a client");

                let (client_tx, client_rx) = channel(10000);
                let id = format!("CL-{}", counter);
                Task::spawn(client(id.clone(), stream, command_tx.clone(), client_rx)).detach();
                command_tx
                    .send(CommandMessage::ClientNew {
                        id,
                        sender: client_tx,
                    })
                    .await
                    .unwrap();
                counter += 1;
            }
        })
        .detach();

        let saved_state = config.saved_state_path();
        let mut server = CommandServer::new(fd, config, tx, command_rx, workers, accept_cancel_tx);
        server.load_static_application_configuration().await;

        if let Some(path) = saved_state {
            server
                .load_state(None, "INITIALIZATION".to_string(), &path)
                .await;
        }
        gauge!("configuration.applications", server.state.applications.len());
        gauge!("configuration.backends", server.backends_count);
        gauge!("configuration.frontends", server.frontends_count);

        info!("waiting for configuration client connections");
        server.run().await;
        Ok(())
    })
}

enum CommandMessage {
    ClientNew {
        id: String,
        sender: Sender<sozu_command::command::CommandResponse>,
    },
    ClientClose {
        id: String,
    },
    ClientRequest {
        id: String,
        message: sozu_command::command::CommandRequest,
    },
    WorkerResponse {
        id: u32,
        message: sozu_command::proxy::ProxyResponse,
    },
    WorkerClose {
        id: u32,
    },
    MasterStop,
}

impl CommandServer {
    pub async fn run(&mut self) {
        while let Some(msg) = self.command_rx.next().await {
            match msg {
                CommandMessage::ClientNew { id, sender } => {
                    info!("adding new client {}", id);
                    self.clients.insert(id, sender);
                }
                CommandMessage::ClientClose { id } => {
                    info!("removing client {}", id);
                    self.clients.remove(&id);
                    self.event_subscribers.remove(&id);
                }
                CommandMessage::ClientRequest { id, message } => {
                    info!("client {} sent {:?}", id, message);
                    self.handle_client_message(id, message).await;
                }
                CommandMessage::WorkerClose { id } => {
                    info!("removing worker {}", id);
                    if let Some(w) = self.workers.iter_mut().filter(|w| w.id == id).next() {
                        if self.config.worker_automatic_restart && w.run_state == RunState::Running
                        {
                            self.check_worker_status(id).await;
                        }
                    }
                }
                CommandMessage::WorkerResponse { id, message } => {
                    info!("worker {} sent back {:?}", id, message);
                    if let Some(ProxyResponseData::Event(data)) = message.data {
                        let event: Event = data.into();
                        for client_id in self.event_subscribers.iter() {
                            if let Some(tx) = self.clients.get_mut(client_id) {
                                let event = CommandResponse::new(
                                    message.id.to_string(),
                                    CommandStatus::Processing,
                                    format!("{}", id),
                                    Some(CommandResponseData::Event(event.clone())),
                                );
                                if let Err(e) = tx.send(event).await {
                                    error!("could not send message to client: {:?}", e);
                                }
                            }
                        }
                    } else {
                        match self.in_flight.remove(&message.id) {
                            None => error!("unknown message id: {}", message.id),
                            Some((mut tx, mut nb)) => {
                                let message_id = message.id.clone();

                                // if a worker returned Ok or Error, we're not expecting any more
                                // messages with this id from it
                                match message.status {
                                    ProxyResponseStatus::Ok | ProxyResponseStatus::Error(_) => {
                                        nb -= 1;
                                    }
                                    _ => {}
                                };

                                tx.send(message).await.unwrap();

                                if nb > 1 {
                                    self.in_flight.insert(message_id, (tx, nb - 1));
                                }
                            }
                        }
                    }
                }
                CommandMessage::MasterStop => {
                    info!("stopping main process");
                    break;
                }
            }
        }
    }

    async fn answer_success<T, U>(
        &mut self,
        client_id: String,
        id: T,
        message: U,
        data: Option<CommandResponseData>,
    ) where
        T: Clone + Into<String>,
        U: Clone + Into<String>,
    {
        trace!(
            "answer_success for client {} id {}, message {:#?} data {:#?}",
            client_id,
            id.clone().into(),
            message.clone().into(),
            data
        );
        if let Some(sender) = self.clients.get_mut(&client_id) {
            if let Err(e) = sender
                .send(CommandResponse::new(
                    id.into(),
                    CommandStatus::Ok,
                    message.into(),
                    data,
                ))
                .await {
                    error!("could not send message to client {:?}: {:?}", client_id, e);
                }
        }
    }

    async fn answer_error<T, U>(
        &mut self,
        client_id: String,
        id: T,
        message: U,
        data: Option<CommandResponseData>,
    ) where
        T: Clone + Into<String>,
        U: Clone + Into<String>,
    {
        trace!(
            "answer_error for client {} id {}, message {:#?} data {:#?}",
            client_id,
            id.clone().into(),
            message.clone().into(),
            data
        );
        if let Some(sender) = self.clients.get_mut(&client_id) {
            if let Err(e) = sender
                .send(CommandResponse::new(
                    id.into(),
                    CommandStatus::Error,
                    message.into(),
                    data,
                ))
                .await{
                    error!("could not send message to client {:?}: {:?}", client_id, e);
                }
        }
    }
}

async fn client(
    id: String,
    stream: Async<UnixStream>,
    mut tx: Sender<CommandMessage>,
    mut rx: Receiver<sozu_command::command::CommandResponse>,
) -> std::io::Result<()> {
    let stream = Arc::new(stream);
    let mut s = stream.clone();

    Task::spawn(async move {
        while let Some(msg) = rx.next().await {
            //info!("sending back message to client: {:?}", msg);
            let mut message: Vec<u8> = serde_json::to_string(&msg)
                .map(|s| s.into_bytes())
                .unwrap_or_else(|_| Vec::new());
            message.push(0);
            let _ = s.write_all(&message).await;
        }
    })
    .detach();

    info!("will start receiving messages from client {}", id);
    let mut it = BufReader::new(stream).split(0);
    while let Some(message) = it.next().await {
        let message = match message {
            Err(e) => {
                error!("could not split message: {:?}", e);
                break;
            }
            Ok(msg) => msg,
        };

        match serde_json::from_slice::<sozu_command::command::CommandRequest>(&message) {
            Err(e) => {
                error!("could not decode message: {:?}", e);
                break;
            }
            Ok(message) => {
                info!("got message: {:?}", message);
                let id = id.clone();
                if let Err(e) = tx.send(CommandMessage::ClientRequest { id, message })
                    .await {
                    error!("error writing to client: {:?}", e);
                }
            }
        }
    }

    if let Err(e) = tx.send(CommandMessage::ClientClose { id }).await {
        error!("error writing to client: {:?}", e);
    }
    Ok(())
}

async fn worker_loop(
    id: u32,
    stream: Async<UnixStream>,
    mut tx: Sender<CommandMessage>,
    mut rx: Receiver<sozu_command::proxy::ProxyRequest>,
) -> std::io::Result<()> {
    let stream = Arc::new(stream);
    let mut s = stream.clone();

    Task::spawn(async move {
        debug!("will start sending messages to worker {}", id);
        while let Some(msg) = rx.next().await {
            info!("sending to worker {}: {:?}", id, msg);
            let mut message: Vec<u8> = serde_json::to_string(&msg)
                .map(|s| s.into_bytes())
                .unwrap_or_else(|_| Vec::new());
            message.push(0);
            let _ = s.write_all(&message).await;
        }
    })
    .detach();

    debug!("will start receiving messages from worker {}", id);
    let mut it = BufReader::new(stream).split(0);
    while let Some(message) = it.next().await {
        let message = match message {
            Err(e) => {
                error!("could not split message: {:?}", e);
                break;
            }
            Ok(msg) => msg,
        };

        match serde_json::from_slice::<sozu_command::proxy::ProxyResponse>(&message) {
            Err(e) => {
                error!("could not decode message: {:?}", e);
                break;
            }
            Ok(message) => {
                info!("worker {} replied message: {:?}", id, message);
                let id = id.clone();
                tx.send(CommandMessage::WorkerResponse { id, message })
                    .await
                    .unwrap();
            }
        }
    }

    tx.send(CommandMessage::WorkerClose { id }).await.unwrap();

    Ok(())
}
