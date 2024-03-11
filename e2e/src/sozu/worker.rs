use std::{
    net::SocketAddr,
    os::unix::prelude::{AsRawFd, FromRawFd, IntoRawFd},
    thread::{self, JoinHandle},
};

use mio::net::UnixStream;

use sozu_command_lib as sozu_command;
use sozu_lib as sozu;

use sozu::server::Server;
use sozu_command::{
    channel::Channel,
    config::{ConfigBuilder, FileConfig},
    logging::setup_default_logging,
    proto::command::{
        request::RequestType, AddBackend, Cluster, HardStop, LoadBalancingParams, PathRule,
        Request, RequestHttpFrontend, RequestTcpFrontend, ReturnListenSockets, RulePosition,
        ServerConfig, SoftStop, WorkerRequest, WorkerResponse,
    },
    scm_socket::{Listeners, ScmSocket},
    state::ConfigState,
};

use crate::sozu::command_id::CommandID;

/// Handle to a detached thread where a Sozu worker runs
pub struct Worker {
    pub name: String,
    pub config: ServerConfig,
    pub state: ConfigState,
    pub scm_main_to_worker: ScmSocket,
    pub scm_worker_to_main: ScmSocket,
    pub command_channel: Channel<WorkerRequest, WorkerResponse>,
    pub command_id: CommandID,
    pub server_job: JoinHandle<()>,
}

/// Used to remove the CLOEXEC flag of socket
/// this allows the socket to live even when its parent process is replaced
pub fn set_no_close_exec(fd: i32) {
    unsafe {
        let old_flags = libc::fcntl(fd, libc::F_GETFD);
        let new_flags = old_flags & !1;
        // println!("flags: {old_flags} -> {new_flags}");
        libc::fcntl(fd, libc::F_SETFD, new_flags);
    }
}

impl Worker {
    pub fn into_config(file_config: FileConfig) -> ServerConfig {
        let config = ConfigBuilder::new(file_config, "")
            .into_config()
            .expect("could not create Config");
        ServerConfig::from(&config)
    }

    pub fn empty_config() -> (ServerConfig, Listeners, ConfigState) {
        let listeners = Listeners::default();
        let config = FileConfig::default();
        let config = Worker::into_config(config);
        let state = ConfigState::new();
        (config, listeners, state)
    }

    // TODO: this seems to be used nowhere. We may want to delete it.
    pub fn create_server(
        config: ServerConfig,
        listeners: Listeners,
        state: ConfigState,
    ) -> (ScmSocket, Channel<WorkerRequest, WorkerResponse>, Server) {
        let (scm_main_to_worker, scm_worker_to_main) =
            UnixStream::pair().expect("could not create unix stream pair");
        let (cmd_main_to_worker, cmd_worker_to_main) =
            Channel::generate(config.command_buffer_size, config.max_command_buffer_size)
                .expect("could not create a channel");

        set_no_close_exec(scm_main_to_worker.as_raw_fd());
        set_no_close_exec(scm_worker_to_main.as_raw_fd());

        let scm_main_to_worker = ScmSocket::new(scm_main_to_worker.into_raw_fd())
            .expect("could not create an SCM socket");
        let scm_worker_to_main = ScmSocket::new(scm_worker_to_main.into_raw_fd())
            .expect("could not create an SCM socket");

        scm_main_to_worker
            .send_listeners(&listeners)
            .expect("could not send listeners");

        let initial_state = state.produce_initial_state();
        let server = Server::try_new_from_config(
            cmd_worker_to_main,
            scm_worker_to_main,
            config,
            initial_state,
            false,
        )
        .expect("could not create sozu worker");

        (scm_main_to_worker, cmd_main_to_worker, server)
    }

    pub fn start_new_worker<S: Into<String>>(
        name: S,
        config: ServerConfig,
        listeners: &Listeners,
        state: ConfigState,
    ) -> Self {
        let name = name.into();
        let (scm_main_to_worker, scm_worker_to_main) =
            UnixStream::pair().expect("could not create unix stream pair");
        let (cmd_main_to_worker, cmd_worker_to_main) =
            Channel::generate(config.command_buffer_size, config.max_command_buffer_size)
                .expect("could not create a channel");

        set_no_close_exec(scm_main_to_worker.as_raw_fd());
        set_no_close_exec(scm_worker_to_main.as_raw_fd());

        let scm_main_to_worker = ScmSocket::new(scm_main_to_worker.into_raw_fd())
            .expect("could not create an SCM socket");
        let scm_worker_to_main = ScmSocket::new(scm_worker_to_main.into_raw_fd())
            .expect("could not create an SCM socket");
        scm_main_to_worker
            .send_listeners(listeners)
            .expect("could not send listeners");

        let thread_config = config.to_owned();
        let initial_state = state.produce_initial_state();
        let thread_name = name.to_owned();
        let thread_scm_worker_to_main = scm_worker_to_main.to_owned();

        println!("Setting up logging");

        let server_job = thread::spawn(move || {
            setup_default_logging(false, "error", &thread_name);
            let mut server = Server::try_new_from_config(
                cmd_worker_to_main,
                thread_scm_worker_to_main,
                thread_config,
                initial_state,
                false,
            )
            .expect("could not create sozu worker");
            server.run();
            println!("{thread_name} STOPPED");
        });

        Self {
            name,
            config,
            state,
            scm_main_to_worker,
            scm_worker_to_main,
            command_channel: cmd_main_to_worker,
            command_id: CommandID::new(),
            server_job,
        }
    }

    pub fn upgrade<S: Into<String>>(&mut self, name: S) -> Self {
        self.send_proxy_request_type(RequestType::ReturnListenSockets(ReturnListenSockets {}));
        self.read_to_last();

        self.scm_main_to_worker
            .set_blocking(true)
            .expect("Could not set scm socket to blocking");
        let listeners = self
            .scm_main_to_worker
            .receive_listeners()
            .expect("receive listeners");
        println!("Listeners from old worker: {listeners:?}");
        println!("State from old worker: {:?}", self.state);
        self.soft_stop();

        let mut worker = Worker::start_new_worker(
            name,
            self.config.to_owned(),
            &listeners,
            self.state.to_owned(),
        );
        worker
            .scm_main_to_worker
            .send_listeners(&listeners)
            .expect("send listeners");
        listeners.close();
        worker.command_id.prefix = "ACTIVATE_".to_string();
        for request in self.state.generate_activate_requests() {
            worker.send_proxy_request(request);
        }
        worker.command_id.prefix = "ID_".to_string();
        worker.read_to_last();

        println!("Upgrade successful, new worker ready");
        worker
    }

    pub fn send_proxy_request(&mut self, request: Request) {
        //self.state.handle_order(&order);
        self.command_channel
            .write_message(&WorkerRequest {
                id: self.command_id.next(),
                content: request,
            })
            .expect("Could not write message on command channel");
    }
    pub fn send_proxy_request_type(&mut self, request: RequestType) {
        self.send_proxy_request(request.into());
    }

    pub fn read_proxy_response(&mut self) -> Option<WorkerResponse> {
        let response = self
            .command_channel
            .read_message()
            .expect("Could not read message on command channel");
        println!("{} received: {:?}", self.name, response);
        Some(response)
    }

    pub fn read_to_last(&mut self) {
        loop {
            let response = self.read_proxy_response();
            if response.unwrap().id == self.command_id.last {
                break;
            }
        }
    }

    pub fn hard_stop(&mut self) {
        self.send_proxy_request_type(RequestType::HardStop(HardStop {}));
    }
    pub fn soft_stop(&mut self) {
        self.send_proxy_request_type(RequestType::SoftStop(SoftStop {}));
    }

    pub fn wait_for_server_stop(self) -> bool {
        let result = if self.server_job.is_finished() {
            println!("already finished...");
            true
        } else {
            println!("waiting...");
            match self.server_job.join() {
                Ok(_) => {
                    println!("finished!");
                    true
                }
                Err(error) => {
                    println!("could not join: {error:#?}");
                    false
                }
            }
        };
        unsafe {
            UnixStream::from_raw_fd(self.scm_main_to_worker.fd);
            UnixStream::from_raw_fd(self.scm_worker_to_main.fd);
        }
        result
    }

    pub fn default_cluster<S: Into<String>>(cluster_id: S, sticky_session: bool) -> Cluster {
        Cluster {
            cluster_id: cluster_id.into(),
            sticky_session,
            https_redirect: false,
            ..Default::default()
        }
    }

    pub fn default_tcp_frontend<S: Into<String>>(
        cluster_id: S,
        address: SocketAddr,
    ) -> RequestTcpFrontend {
        RequestTcpFrontend {
            cluster_id: cluster_id.into(),
            address: address.into(),
            ..Default::default()
        }
    }

    pub fn default_http_frontend<S: Into<String>>(
        cluster_id: S,
        address: SocketAddr,
    ) -> RequestHttpFrontend {
        RequestHttpFrontend {
            cluster_id: Some(cluster_id.into()),
            address: address.into(),
            hostname: String::from("localhost"),
            path: PathRule::prefix(String::from("/")),
            position: RulePosition::Tree.into(),
            ..Default::default()
        }
    }

    pub fn default_backend<S1: Into<String>, S2: Into<String>>(
        cluster_id: S1,
        backend_id: S2,
        address: SocketAddr,
        sticky_id: Option<String>,
    ) -> AddBackend {
        AddBackend {
            cluster_id: cluster_id.into(),
            backend_id: backend_id.into(),
            address: address.into(),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id,
            backup: None,
        }
    }
}
