use std::{
    net::SocketAddr,
    os::unix::prelude::{AsRawFd, FromRawFd, IntoRawFd},
    thread::{self, JoinHandle},
};

use mio::net::UnixStream;
use sozu::server::Server;
use sozu_command::{
    channel::Channel,
    config::{ConfigBuilder, FileConfig},
    logging::{Logger, setup_default_logging},
    proto::command::{
        AddBackend, Cluster, HardStop, LoadBalancingParams, PathRule, Request, RequestHttpFrontend,
        RequestTcpFrontend, ReturnListenSockets, RulePosition, ServerConfig, SoftStop,
        WorkerRequest, WorkerResponse, request::RequestType,
    },
    scm_socket::{Listeners, ScmSocket},
    state::ConfigState,
};
use sozu_command_lib as sozu_command;
use sozu_lib as sozu;

use crate::port_registry::{
    attach_reserved_http_listener, attach_reserved_https_listener, attach_reserved_tcp_listener,
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
        let config = FileConfig::default();
        let config = Worker::into_config(config);
        let state = ConfigState::new();
        let listeners = Listeners::default();
        (config, listeners, state)
    }

    pub fn empty_config_with_listeners(
        listeners: Listeners,
    ) -> (ServerConfig, Listeners, ConfigState) {
        let config = FileConfig::default();
        let config = Worker::into_config(config);
        let state = ConfigState::new();
        (config, listeners, state)
    }

    pub fn empty_http_config(address: SocketAddr) -> (ServerConfig, Listeners, ConfigState) {
        let mut listeners = Listeners::default();
        attach_reserved_http_listener(&mut listeners, address);
        Self::empty_config_with_listeners(listeners)
    }

    pub fn empty_https_config(address: SocketAddr) -> (ServerConfig, Listeners, ConfigState) {
        let mut listeners = Listeners::default();
        attach_reserved_https_listener(&mut listeners, address);
        Self::empty_config_with_listeners(listeners)
    }

    pub fn empty_tcp_config(address: SocketAddr) -> (ServerConfig, Listeners, ConfigState) {
        let mut listeners = Listeners::default();
        attach_reserved_tcp_listener(&mut listeners, address);
        Self::empty_config_with_listeners(listeners)
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

    /// Start a worker whose thread-local logger is configured exactly as every
    /// worker in this suite has always configured it: `setup_default_logging(
    /// false, "error", &thread_name)`, i.e. target `"stdout"` at level
    /// `"error"`, with `RUST_LOG` still able to override the level.
    ///
    /// To read back what a worker logged, use
    /// [`Worker::start_new_worker_with_logging`] instead.
    pub fn start_new_worker<S: Into<String>>(
        name: S,
        config: ServerConfig,
        listeners: &Listeners,
        state: ConfigState,
    ) -> Self {
        Self::spawn_worker(name.into(), config, listeners, state, None)
    }

    /// [`Worker::start_new_worker`] with the worker thread's log target and
    /// level chosen by the caller, so a test can assert on what the worker
    /// actually logged.
    ///
    /// `log_target` is a `target_to_backend` string
    /// (`command/src/logging/logs.rs`): `"stdout"`, `"file:///absolute/path"`,
    /// `"udp://addr"`, `"tcp://addr"` or `"unix://path"`. Prefer `file://` with
    /// [`WorkerLogCapture`](crate::sozu::log_capture::WorkerLogCapture): the
    /// UDP backend drops datagrams once the kernel socket buffer fills, which
    /// an H2 conversation at `trace` does reach, and a file has no loss mode.
    ///
    /// `log_spec` is a `parse_logging_spec` string: either a bare level
    /// (`"error"`) or a comma-separated list where each `module=level`
    /// directive is matched against the call site's `module_path!()` by
    /// prefix — `"error,sozu_lib::protocol::mux=trace"` traces the mux and
    /// leaves the rest of the worker at `error`.
    ///
    /// The target must be chosen HERE rather than by the test thread: `LOGGER`
    /// is a `thread_local!` with a one-shot `initialized` guard, so the only
    /// logger this worker will ever have is the one installed by the first
    /// call made on its own thread — the one in [`Worker::spawn_worker`]. The
    /// flip side is that each worker owns its own logger, so a per-worker
    /// target needs no serialisation between tests.
    ///
    /// Unlike [`Worker::start_new_worker`], this path installs the spec with
    /// `Logger::init` rather than `setup_logging`, so `RUST_LOG` does NOT
    /// override it. A capture test asks for a level precisely in order to
    /// assert on what that level emits; an ambient `RUST_LOG=error` silently
    /// emptying the capture would be a failure about the environment rather
    /// than about the code. `lib/src/lib.rs`'s `capture_test_logs_at_level`
    /// calls `Logger::init` directly for the same reason.
    pub fn start_new_worker_with_logging<S: Into<String>>(
        name: S,
        config: ServerConfig,
        listeners: &Listeners,
        state: ConfigState,
        log_target: &str,
        log_spec: &str,
    ) -> Self {
        Self::spawn_worker(
            name.into(),
            config,
            listeners,
            state,
            Some((log_target.to_owned(), log_spec.to_owned())),
        )
    }

    /// Shared body of [`Worker::start_new_worker`] and
    /// [`Worker::start_new_worker_with_logging`]. `logging` is `None` for the
    /// historical `setup_default_logging(false, "error", ...)` call and
    /// `Some((target, spec))` for an explicit override.
    fn spawn_worker(
        name: String,
        config: ServerConfig,
        listeners: &Listeners,
        state: ConfigState,
        logging: Option<(String, String)>,
    ) -> Self {
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
            let logging_setup = match logging {
                None => setup_default_logging(false, "error", &thread_name),
                Some((target, spec)) => Logger::init(
                    thread_name.to_owned(),
                    &spec,
                    &target,
                    false,
                    None,
                    None,
                    None,
                ),
            };
            if let Err(e) = logging_setup {
                println!("could not setup logging: {e}");
            }
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

    pub fn start_new_worker_owned<S: Into<String>>(
        name: S,
        config: ServerConfig,
        listeners: Listeners,
        state: ConfigState,
    ) -> Self {
        let worker = Self::start_new_worker(name, config, &listeners, state);
        listeners.close();
        worker
    }

    /// [`Worker::start_new_worker_owned`] with the logging override of
    /// [`Worker::start_new_worker_with_logging`].
    pub fn start_new_worker_owned_with_logging<S: Into<String>>(
        name: S,
        config: ServerConfig,
        listeners: Listeners,
        state: ConfigState,
        log_target: &str,
        log_spec: &str,
    ) -> Self {
        let worker = Self::start_new_worker_with_logging(
            name, config, &listeners, state, log_target, log_spec,
        );
        listeners.close();
        worker
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

        // Deactivate listeners in the state clone so that produce_initial_state()
        // won't generate ActivateListener requests: activation is a single
        // explicit step below, via generate_activate_requests().
        //
        // This is no longer a workaround for the worker. `Server::new` receives
        // the SCM listeners BEFORE it applies the initial state (sozu#1342), so
        // an initial state that still marks the listeners active would adopt the
        // inherited descriptors rather than fall back to server_bind(). Both
        // sequences are correct; this one keeps activation observable as its own
        // set of ACTIVATE_ requests, which every scenario below reads back, and
        // the worker keeps a descriptor whose address has no listener yet
        // (`InheritedSocketFate::Unclaimed`) exactly so the ACTIVATE_ requests
        // below still find it.
        let mut upgrade_state = self.state.to_owned();
        for listener in upgrade_state.http_listeners.values_mut() {
            listener.active = false;
        }
        for listener in upgrade_state.https_listeners.values_mut() {
            listener.active = false;
        }
        for listener in upgrade_state.tcp_listeners.values_mut() {
            listener.active = false;
        }
        for listener in upgrade_state.udp_listeners.values_mut() {
            listener.active = false;
        }

        let mut worker =
            Worker::start_new_worker(name, self.config.to_owned(), &listeners, upgrade_state);
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
        let _ = self.state.dispatch(&request);
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

    pub fn default_cluster<S: Into<String>>(cluster_id: S) -> Cluster {
        Cluster {
            cluster_id: cluster_id.into(),
            sticky_session: false,
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

    /// TCP frontend scoped to an SNI hostname (and optionally a set of
    /// ALPN protocol names), for the passthrough SNI+ALPN preread routing
    /// added by sozu-proxy/sozu#1279 (`RequestTcpFrontend.sni` / `.alpn`).
    /// Added alongside `default_tcp_frontend` rather than folding the two
    /// new fields into it: every e2e test exercising the SNI-preread
    /// listener needs `sni`/`alpn` set (an SNI-scoped route is meaningless
    /// with them absent), while every pre-existing no-SNI TCP e2e test
    /// needs them absent — keeping both builders separate avoids an
    /// `Option`/`Vec` argument creeping into every existing
    /// `default_tcp_frontend` call site in `tcp_tests.rs`.
    pub fn sni_tcp_frontend<S: Into<String>>(
        cluster_id: S,
        address: SocketAddr,
        sni: Option<&str>,
        alpn: &[&str],
    ) -> RequestTcpFrontend {
        RequestTcpFrontend {
            cluster_id: cluster_id.into(),
            address: address.into(),
            sni: sni.map(str::to_owned),
            alpn: alpn.iter().map(|protocol| protocol.to_string()).collect(),
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
