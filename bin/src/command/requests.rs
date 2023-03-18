use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{Read, Write},
    os::unix::io::{FromRawFd, IntoRawFd},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use async_io::Async;
use futures::{channel::mpsc::*, SinkExt, StreamExt};
use nom::{Err, HexDisplay, Offset};

use sozu_command_lib::{
    buffer::fixed::Buffer,
    config::Config,
    logging,
    parser::parse_several_commands,
    request::{FrontendFilters, MetricsConfiguration, QueryClusterType, Request, WorkerRequest},
    response::{
        AggregatedMetricsData, ListedFrontends, ListenersList, Response, ResponseContent,
        ResponseStatus, RunState, WorkerInfo,
    },
    scm_socket::Listeners,
    state::get_cluster_ids_by_domain,
};

use sozu::metrics::METRICS;

use crate::{
    command::{Advancement, CommandMessage, CommandServer, Success},
    upgrade::fork_main_into_new_main,
    worker::{start_worker, Worker},
};

impl CommandServer {
    pub async fn handle_client_request(
        &mut self,
        client_id: String,
        request: Request,
    ) -> anyhow::Result<Success> {
        trace!("Received request {:?}", request);

        let cloned_client_id = client_id.clone();

        let result: anyhow::Result<Option<Success>> = match request {
            Request::SaveState { path } => self.save_state(&path).await,
            Request::DumpState => self.dump_state().await,
            Request::ListWorkers => self.list_workers().await,
            Request::ListFrontends(filters) => self.list_frontends(filters).await,
            Request::ListListeners => self.list_listeners(),
            Request::LoadState { path } => self.load_state(Some(client_id), &path).await,
            Request::LaunchWorker(tag) => self.launch_worker(client_id, &tag).await,
            Request::UpgradeMain => self.upgrade_main(client_id).await,
            Request::UpgradeWorker(worker_id) => self.upgrade_worker(client_id, worker_id).await,
            Request::ConfigureMetrics(config) => self.configure_metrics(client_id, config).await,
            // request::Query(query) => self.query(client_id, query).await,
            Request::Logging(logging_filter) => self.set_logging_level(logging_filter),
            Request::SubscribeEvents => {
                self.event_subscribers.insert(client_id.clone());
                Ok(Some(Success::SubscribeEvent(client_id.clone())))
            }
            Request::ReloadConfiguration { path } => {
                self.reload_configuration(client_id, path).await
            }
            Request::Status => self.status(client_id).await,

            Request::QueryCertificates(_)
            | Request::QueryClusters(_)
            | Request::QueryClustersHashes
            | Request::QueryMetrics(_) => self.query(client_id, request).await,

            // any other case is an request for the workers, except for SoftStop and HardStop.
            // TODO: we should have something like:
            // RequestContent::SoftStop => self.do_something(),
            // RequestContent::HardStop => self.do_nothing_and_return_early(),
            // but it goes in there instead:
            request_for_workers => self.worker_requests(client_id, request_for_workers).await,
        };

        // Notify the command server by sending using his command_tx
        match result {
            Ok(Some(success)) => {
                info!("{}", success);
                return_success(self.command_tx.clone(), cloned_client_id, success).await;
            }
            Err(anyhow_error) => {
                let formatted = format!("{anyhow_error:#}");
                error!("{:#}", formatted);
                return_error(self.command_tx.clone(), cloned_client_id, formatted).await;
            }
            Ok(None) => {
                // do nothing here. Ok(None) means the function has already returned its result
                // on its own to the command server
            }
        }

        Ok(Success::HandledClientRequest)
    }

    pub async fn save_state(&mut self, path: &str) -> anyhow::Result<Option<Success>> {
        let mut file = File::create(path)
            .with_context(|| format!("could not open file at path: {}", &path))?;

        let counter = self
            .save_state_to_file(&mut file)
            .with_context(|| "failed writing state to file")?;

        info!("wrote {} commands to {}", counter, path);

        Ok(Some(Success::SaveState(counter, path.into())))
    }

    pub fn save_state_to_file(&mut self, file: &mut File) -> anyhow::Result<usize> {
        let mut counter = 0usize;
        let requests = self.state.generate_requests();

        let result: anyhow::Result<usize> = (move || {
            for request in requests {
                let message = WorkerRequest::new(format!("SAVE-{counter}"), request);

                file.write_all(
                    &serde_json::to_string(&message)
                        .map(|s| s.into_bytes())
                        .unwrap_or_default(),
                )
                .with_context(|| {
                    format!(
                        "Could not add this instruction line to the saved state file: {message:?}"
                    )
                })?;

                file.write_all(&b"\n\0"[..])
                    .with_context(|| "Could not add new line to the saved state file")?;

                if counter % 1000 == 0 {
                    info!("writing command {}", counter);
                    file.sync_all()
                        .with_context(|| "Failed to sync the saved state file")?;
                }
                counter += 1;
            }
            file.sync_all()
                .with_context(|| "Failed to sync the saved state file")?;

            Ok(counter)
        })();

        result.with_context(|| "Could not write the state onto the state file")
    }

    pub async fn dump_state(&mut self) -> anyhow::Result<Option<Success>> {
        let state = self.state.clone();

        Ok(Some(Success::DumpState(ResponseContent::State(Box::new(
            state,
        )))))
    }

    pub async fn load_state(
        &mut self,
        client_id: Option<String>,
        path: &str,
    ) -> anyhow::Result<Option<Success>> {
        let mut file =
            File::open(path).with_context(|| format!("Cannot open file at path {path}"))?;

        let mut buffer = Buffer::with_capacity(200000);

        info!("starting to load state from {}", path);

        let mut message_counter = 0usize;
        let mut diff_counter = 0usize;

        let (load_state_tx, mut load_state_rx) = futures::channel::mpsc::channel(10000);
        loop {
            let previous = buffer.available_data();
            //FIXME: we should read in streaming here
            match file.read(buffer.space()) {
                Ok(sz) => buffer.fill(sz),
                Err(e) => {
                    bail!("Error reading the saved state file: {}", e);
                }
            };

            if buffer.available_data() == 0 {
                debug!("Empty buffer");
                break;
            }

            let mut offset = 0usize;
            match parse_several_commands::<WorkerRequest>(buffer.data()) {
                Ok((i, requests)) => {
                    if !i.is_empty() {
                        debug!("could not parse {} bytes", i.len());
                        if previous == buffer.available_data() {
                            bail!("error consuming load state message");
                        }
                    }
                    offset = buffer.data().offset(i);

                    /*
                    TODO: maybe we should put on WorkerRequest the version field we had on CommandRequest: a u32 to track versionning
                    if requests.iter().any(|o| {
                        if o.version > PROTOCOL_VERSION {
                            error!("configuration protocol version mismatch: Sōzu handles up to version {}, the message uses version {}", PROTOCOL_VERSION, o.version);
                            true
                        } else {
                            false
                        }
                    }) {
                        break;
                    }
                    */

                    for request in requests {
                        message_counter += 1;

                        if self.state.dispatch(&request.content).is_ok() {
                            diff_counter += 1;

                            let mut found = false;
                            let id = format!("LOAD-STATE-{}-{diff_counter}", request.id);

                            for worker in
                                self.workers.iter_mut().filter(|worker| worker.is_active())
                            {
                                let worker_message_id = format!("{}-{}", id, worker.id);
                                worker
                                    .send(worker_message_id.clone(), request.content.clone())
                                    .await;
                                self.in_flight
                                    .insert(worker_message_id, (load_state_tx.clone(), 1));

                                found = true;
                            }

                            if !found {
                                bail!("no worker found");
                            }
                        }
                    }
                }
                Err(Err::Incomplete(_)) => {
                    if buffer.available_data() == buffer.capacity() {
                        error!(
                            "message too big, stopping parsing:\n{}",
                            buffer.data().to_hex(16)
                        );
                        break;
                    }
                }
                Err(parse_error) => {
                    bail!("saved state parse error: {:?}", parse_error);
                }
            }
            buffer.consume(offset);
        }

        info!(
            "stopped loading data from file, remaining: {} bytes, saw {} messages, generated {} diff messages",
            buffer.available_data(), message_counter, diff_counter
        );

        if diff_counter > 0 {
            info!(
                "state loaded from {}, will start sending {} messages to workers",
                path, diff_counter
            );

            let command_tx = self.command_tx.to_owned();
            let path = path.to_owned();

            smol::spawn(async move {
                let mut ok = 0usize;
                let mut error = 0usize;
                while let Some((proxy_response, _)) = load_state_rx.next().await {
                    match proxy_response.status {
                        ResponseStatus::Ok => {
                            ok += 1;
                        }
                        ResponseStatus::Processing => {}
                        ResponseStatus::Failure => {
                            error!("{}", proxy_response.message);
                            error += 1;
                        }
                    };
                    debug!("ok:{}, error: {}", ok, error);
                }

                let client_id = match client_id {
                    Some(client_id) => client_id,
                    None => {
                        match error {
                            0 => info!("loading state: {} ok messages, 0 errors", ok),
                            _ => error!("loading state: {} ok messages, {} errors", ok, error),
                        }
                        return;
                    }
                };

                // notify the command server
                match error {
                    0 => {
                        return_success(
                            command_tx,
                            client_id,
                            Success::LoadState(path.to_string(), ok, error),
                        )
                        .await;
                    }
                    _ => {
                        return_error(
                            command_tx,
                            client_id,
                            format!("Loading state failed, ok: {ok}, error: {error}, path: {path}"),
                        )
                        .await;
                    }
                }
            })
            .detach();
        } else {
            info!("no messages sent to workers: local state already had those messages");
            if let Some(client_id) = client_id {
                return_success(
                    self.command_tx.clone(),
                    client_id,
                    Success::LoadState(path.to_string(), 0, 0),
                )
                .await;
            }
        }

        self.backends_count = self.state.count_backends();
        self.frontends_count = self.state.count_frontends();
        gauge!("configuration.clusters", self.state.clusters.len());
        gauge!("configuration.backends", self.backends_count);
        gauge!("configuration.frontends", self.frontends_count);
        Ok(None)
    }

    pub async fn list_frontends(
        &mut self,
        filters: FrontendFilters,
    ) -> anyhow::Result<Option<Success>> {
        info!(
            "Received a request to list frontends, along these filters: {:?}",
            filters
        );

        // if no http / https / tcp filter is provided, list all of them
        let list_all = !filters.http && !filters.https && !filters.tcp;

        let mut listed_frontends = ListedFrontends::default();

        if filters.http || list_all {
            for http_frontend in self.state.http_fronts.iter().filter(|f| {
                if let Some(domain) = &filters.domain {
                    f.1.hostname.contains(domain)
                } else {
                    true
                }
            }) {
                listed_frontends
                    .http_frontends
                    .push(http_frontend.1.to_owned());
            }
        }

        if filters.https || list_all {
            for https_frontend in self.state.https_fronts.iter().filter(|f| {
                if let Some(domain) = &filters.domain {
                    f.1.hostname.contains(domain)
                } else {
                    true
                }
            }) {
                listed_frontends
                    .https_frontends
                    .push(https_frontend.1.to_owned());
            }
        }

        if (filters.tcp || list_all) && filters.domain.is_none() {
            for tcp_frontend in self.state.tcp_fronts.values().flat_map(|v| v.iter()) {
                listed_frontends.tcp_frontends.push(tcp_frontend.to_owned())
            }
        }

        Ok(Some(Success::ListFrontends(ResponseContent::FrontendList(
            listed_frontends,
        ))))
    }

    fn list_listeners(&self) -> anyhow::Result<Option<Success>> {
        Ok(Some(Success::ListListeners(
            ResponseContent::ListenersList(ListenersList {
                http_listeners: self.state.http_listeners.clone(),
                https_listeners: self.state.https_listeners.clone(),
                tcp_listeners: self.state.tcp_listeners.clone(),
            }),
        )))
    }

    pub async fn list_workers(&mut self) -> anyhow::Result<Option<Success>> {
        let workers: Vec<WorkerInfo> = self
            .workers
            .iter()
            .map(|worker| WorkerInfo {
                id: worker.id,
                pid: worker.pid,
                run_state: worker.run_state,
            })
            .collect();

        debug!("workers: {:#?}", workers);

        Ok(Some(Success::ListWorkers(ResponseContent::Workers(
            workers,
        ))))
    }

    pub async fn launch_worker(
        &mut self,
        client_id: String,
        _tag: &str,
    ) -> anyhow::Result<Option<Success>> {
        let mut worker = start_worker(
            self.next_worker_id,
            &self.config,
            self.executable_path.clone(),
            &self.state,
            None,
        )
        .with_context(|| format!("Failed at creating worker {}", self.next_worker_id))?;

        return_processing(
            self.command_tx.clone(),
            client_id.clone(),
            "Sending configuration requests to the new worker...",
        )
        .await;

        info!("created new worker: {}", worker.id);

        self.next_worker_id += 1;

        let sock = worker
            .worker_channel
            .take()
            .expect("No channel on the worker being launched")
            .sock;
        let (worker_tx, worker_rx) = channel(10000);
        worker.sender = Some(worker_tx);

        let stream = Async::new(unsafe {
            let fd = sock.into_raw_fd();
            UnixStream::from_raw_fd(fd)
        })?;

        let id = worker.id;
        let command_tx = self.command_tx.clone();

        smol::spawn(async move {
            super::worker_loop(id, stream, command_tx, worker_rx).await;
        })
        .detach();

        info!(
            "sending listeners: to the new worker: {:?}",
            worker.scm_socket.send_listeners(&Listeners {
                http: Vec::new(),
                tls: Vec::new(),
                tcp: Vec::new(),
            })
        );

        let activate_requests = self.state.generate_activate_requests();
        for (count, request) in activate_requests.into_iter().enumerate() {
            worker.send(format!("{id}-ACTIVATE-{count}"), request).await;
        }

        self.workers.push(worker);

        return_success(
            self.command_tx.clone(),
            client_id,
            Success::WorkerLaunched(id),
        )
        .await;
        Ok(None)
    }

    pub async fn upgrade_main(&mut self, client_id: String) -> anyhow::Result<Option<Success>> {
        self.disable_cloexec_before_upgrade()?;

        return_processing(
            self.command_tx.clone(),
            client_id,
            "The proxy is processing the upgrade command.",
        )
        .await;

        let upgrade_data = self.generate_upgrade_data();

        let (new_main_pid, mut fork_confirmation_channel) =
            fork_main_into_new_main(self.executable_path.clone(), upgrade_data)
                .with_context(|| "Could not start a new main process")?;

        if let Err(e) = fork_confirmation_channel.blocking() {
            error!(
                "Could not block the fork confirmation channel: {}. This is not normal, you may need to restart sozu",
                e
            );
        }
        let received_ok_from_new_process = fork_confirmation_channel.read_message();
        debug!("upgrade channel sent {:?}", received_ok_from_new_process);

        // signaling the accept loop that it should stop
        if let Err(e) = self
            .accept_cancel
            .take() // we should create a method on Self for this frequent procedure
            .expect("No channel on the main process")
            .send(())
        {
            error!("could not close the accept loop: {:?}", e);
        }

        if !received_ok_from_new_process
            .with_context(|| "Did not receive fork confirmation from new worker")?
        {
            bail!("forking the new worker failed")
        }
        info!("wrote final message, closing");
        Ok(Some(Success::UpgradeMain(new_main_pid)))
    }

    pub async fn upgrade_worker(
        &mut self,
        client_id: String,
        worker_id: u32,
    ) -> anyhow::Result<Option<Success>> {
        info!(
            "client[{}] msg wants to upgrade worker {}",
            client_id, worker_id
        );

        if !self
            .workers
            .iter()
            .any(|worker| worker.id == worker_id && worker.is_active())
        {
            bail!(format!(
                "The worker {} does not exist, or is stopped / stopping.",
                &worker_id
            ));
        }

        // same as launch_worker
        let next_id = self.next_worker_id;
        let mut new_worker = start_worker(
            next_id,
            &self.config,
            self.executable_path.clone(),
            &self.state,
            None,
        )
        .with_context(|| "failed at creating worker")?;

        return_processing(
            self.command_tx.clone(),
            client_id.clone(),
            "Sending configuration requests to the worker",
        )
        .await;

        info!("created new worker: {}", next_id);

        self.next_worker_id += 1;

        let sock = new_worker
            .worker_channel
            .take()
            .with_context(|| "No channel on new worker".to_string())?
            .sock;
        let (worker_tx, worker_rx) = channel(10000);
        new_worker.sender = Some(worker_tx);

        new_worker
            .sender
            .as_mut()
            .with_context(|| "No sender on new worker".to_string())?
            .send(WorkerRequest {
                id: format!("UPGRADE-{worker_id}-STATUS"),
                content: Request::Status,
            })
            .await
            .with_context(|| {
                format!(
                    "could not send status message to worker {:?}",
                    new_worker.id,
                )
            })?;

        let mut listeners = None;
        {
            let old_worker: &mut Worker = self
                .workers
                .iter_mut()
                .find(|worker| worker.id == worker_id)
                .unwrap();

            /*
            old_worker.channel.set_blocking(true);
            old_worker.channel.write_message(&ProxyRequest { id: String::from(message_id), request: RequestContent::ReturnListenSockets });
            info!("sent returnlistensockets message to worker");
            old_worker.channel.set_blocking(false);
            */
            let (sockets_return_tx, mut sockets_return_rx) = futures::channel::mpsc::channel(3);
            let id = format!("{}-return-sockets", client_id);
            self.in_flight.insert(id.clone(), (sockets_return_tx, 1));
            old_worker
                .send(id.clone(), Request::ReturnListenSockets)
                .await;

            info!("sent ReturnListenSockets to old worker");

            let cloned_command_tx = self.command_tx.clone();
            let cloned_req_id = client_id.clone();
            smol::spawn(async move {
                while let Some((proxy_response, _)) = sockets_return_rx.next().await {
                    match proxy_response.status {
                        ResponseStatus::Ok => {
                            info!("returnsockets OK");
                            break;
                        }
                        ResponseStatus::Processing => {
                            info!("returnsockets processing");
                        }
                        ResponseStatus::Failure => {
                            return_error(cloned_command_tx, cloned_req_id, proxy_response.message)
                                .await;
                            break;
                        }
                    };
                }
            })
            .detach();

            let mut counter = 0usize;

            loop {
                info!("waiting for listen sockets from the old worker");
                if let Err(e) = old_worker.scm_socket.set_blocking(true) {
                    error!("Could not set the old worker socket to blocking: {}", e);
                };
                match old_worker.scm_socket.receive_listeners() {
                    Ok(l) => {
                        listeners = Some(l);
                        break;
                    }
                    Err(error) => {
                        error!(
                            "Could not receive listerners from scm socket with file descriptor {}:\n{:?}",
                            old_worker.scm_socket.fd, error
                        );
                        counter += 1;
                        if counter == 50 {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
            info!("got the listen sockets from the old worker");
            old_worker.run_state = RunState::Stopping;

            let (softstop_tx, mut softstop_rx) = futures::channel::mpsc::channel(10);
            let softstop_id = format!("{}-softstop", client_id);
            self.in_flight.insert(softstop_id.clone(), (softstop_tx, 1));
            old_worker
                .send(softstop_id.clone(), Request::SoftStop)
                .await;

            let mut command_tx = self.command_tx.clone();
            let cloned_client_id = client_id.clone();
            let worker_id = old_worker.id;
            smol::spawn(async move {
                while let Some((proxy_response, _)) = softstop_rx.next().await {
                    match proxy_response.status {
                        // should we send all this to the command server?
                        ResponseStatus::Ok => {
                            info!("softstop OK"); // this doesn't display :-(
                            if let Err(e) = command_tx
                                .send(CommandMessage::WorkerClose { worker_id })
                                .await
                            {
                                error!(
                                    "could not send worker close message to {}: {:?}",
                                    worker_id, e
                                );
                            }
                            break;
                        }
                        ResponseStatus::Processing => {
                            info!("softstop processing");
                        }
                        ResponseStatus::Failure => {
                            info!("softstop error: {:?}", proxy_response.message);
                            break;
                        }
                    };
                }
                return_processing(
                    command_tx.clone(),
                    cloned_client_id,
                    "Processing softstop responses from the workers...",
                )
                .await;
            })
            .detach();
        }

        match listeners {
            Some(l) => {
                info!(
                    "sending listeners: to the new worker: {:?}",
                    new_worker.scm_socket.send_listeners(&l)
                );
                l.close();
            }
            None => error!("could not get the list of listeners from the previous worker"),
        };

        let stream = Async::new(unsafe {
            let fd = sock.into_raw_fd();
            UnixStream::from_raw_fd(fd)
        })?;

        let id = new_worker.id;
        let command_tx = self.command_tx.clone();
        smol::spawn(async move {
            super::worker_loop(id, stream, command_tx, worker_rx).await;
        })
        .detach();

        let activate_requests = self.state.generate_activate_requests();
        for (count, request) in activate_requests.into_iter().enumerate() {
            new_worker
                .send(format!("{}-ACTIVATE-{}", client_id, count), request)
                .await;
        }

        info!("sent config messages to the new worker");
        self.workers.push(new_worker);

        info!("finished upgrade");
        Ok(Some(Success::UpgradeWorker(id)))
    }

    pub async fn reload_configuration(
        &mut self,
        client_id: String,
        config_path: Option<String>,
    ) -> anyhow::Result<Option<Success>> {
        // check that this works
        let path = config_path.as_deref().unwrap_or(&self.config.config_path);
        let new_config = Config::load_from_path(path)
            .with_context(|| format!("cannot load configuration from '{path}'"))?;

        let mut diff_counter = 0usize;

        let (load_state_tx, mut load_state_rx) = futures::channel::mpsc::channel(10000);

        return_processing(
            self.command_tx.clone(),
            client_id.clone(),
            "Reloading configuration, sending config messages to workers...",
        )
        .await;

        for request in new_config.generate_config_messages()? {
            if self.state.dispatch(&request.content).is_ok() {
                diff_counter += 1;

                let mut found = false;
                let id = format!("LOAD-STATE-{}-{}", &request.id, diff_counter);

                for worker in self.workers.iter_mut().filter(|worker| worker.is_active()) {
                    let worker_message_id = format!("{}-{}", id, worker.id);
                    worker
                        .send(worker_message_id.clone(), request.content.clone())
                        .await;
                    self.in_flight
                        .insert(worker_message_id, (load_state_tx.clone(), 1));

                    found = true;
                }

                if !found {
                    bail!("no worker found");
                }
            }
        }

        // clone everything we will need in the detached thread
        let command_tx = self.command_tx.clone();
        let cloned_identifier = client_id.clone();

        if diff_counter > 0 {
            info!(
                "state loaded from {}, will start sending {} messages to workers",
                new_config.config_path, diff_counter
            );
            smol::spawn(async move {
                let mut ok = 0usize;
                let mut error = 0usize;
                while let Some((proxy_response, _)) = load_state_rx.next().await {
                    match proxy_response.status {
                        ResponseStatus::Ok => {
                            ok += 1;
                        }
                        ResponseStatus::Processing => {}
                        ResponseStatus::Failure => {
                            error!("{}", proxy_response.message);
                            error += 1;
                        }
                    };
                    debug!("ok:{}, error: {}", ok, error);
                }

                if error == 0 {
                    return_success(
                        command_tx,
                        cloned_identifier,
                        Success::ReloadConfiguration(ok, error),
                    )
                    .await;
                } else {
                    return_error(
                        command_tx,
                        cloned_identifier,
                        format!(
                            "Reloading configuration failed. ok: {ok} messages, error: {error}"
                        ),
                    )
                    .await;
                }
            })
            .detach();
        } else {
            info!("no messages sent to workers: local state already had those messages");
        }

        self.backends_count = self.state.count_backends();
        self.frontends_count = self.state.count_frontends();
        gauge!("configuration.clusters", self.state.clusters.len());
        gauge!("configuration.backends", self.backends_count);
        gauge!("configuration.frontends", self.frontends_count);

        self.config = new_config;

        Ok(None)
    }

    pub async fn status(&mut self, client_id: String) -> anyhow::Result<Option<Success>> {
        info!("Requesting the status of all workers.");

        let (status_tx, mut status_rx) = futures::channel::mpsc::channel(self.workers.len() * 2);

        // create a status list with the available info of the main process
        let mut worker_info_map: BTreeMap<String, WorkerInfo> = BTreeMap::new();

        let prefix = format!("{}-status-", client_id);

        return_processing(
            self.command_tx.clone(),
            client_id.clone(),
            "Sending status requests to workers...",
        )
        .await;

        let mut count = 0usize;
        for worker in self.workers.iter_mut() {
            info!("Worker {} is {}", worker.id, worker.run_state);

            // create request ids even if we don't send any request, as keys in the tree map
            let worker_request_id = format!("{}{}", prefix, worker.id);
            // send a status request to supposedly running workers to update the list afterwards
            if worker.run_state == RunState::Running {
                info!("Summoning status of worker {}", worker.id);
                worker
                    .send(worker_request_id.clone(), Request::Status)
                    .await;
                count += 1;
                self.in_flight
                    .insert(worker_request_id.clone(), (status_tx.clone(), 1));
            }
            worker_info_map.insert(worker_request_id, worker.info());
        }

        let command_tx = self.command_tx.clone();
        let thread_client_id = client_id.clone();

        let now = Instant::now();

        smol::spawn(async move {
            let mut i = 0;

            while let Some((proxy_response, _)) = status_rx.next().await {
                info!(
                    "received response with id {}: {:?}",
                    proxy_response.id, proxy_response
                );
                let new_run_state = match proxy_response.status {
                    ResponseStatus::Ok => RunState::Running,
                    ResponseStatus::Processing => continue,
                    ResponseStatus::Failure => RunState::NotAnswering,
                };
                worker_info_map
                    .entry(proxy_response.id)
                    .and_modify(|worker_info| worker_info.run_state = new_run_state);

                i += 1;
                if i == count || now.elapsed() > Duration::from_secs(10) {
                    break;
                }
            }

            let worker_info_vec: Vec<WorkerInfo> = worker_info_map
                .values()
                .map(|worker_info| worker_info.to_owned())
                .collect();

            return_success(
                command_tx,
                thread_client_id,
                Success::Status(ResponseContent::Status(worker_info_vec)),
            )
            .await;
        })
        .detach();
        Ok(None)
    }

    // This handles the CLI's "metrics enable", "metrics disable", "metrics clear"
    // To get the proxy's metrics, the cli command is "metrics get", handled by the query() function
    pub async fn configure_metrics(
        &mut self,
        client_id: String,
        config: MetricsConfiguration,
    ) -> anyhow::Result<Option<Success>> {
        let (metrics_tx, mut metrics_rx) = futures::channel::mpsc::channel(self.workers.len() * 2);
        let mut count = 0usize;
        for worker in self
            .workers
            .iter_mut()
            .filter(|worker| worker.run_state != RunState::Stopped)
        {
            let req_id = format!("{}-metrics-{}", client_id, worker.id);
            worker
                .send(req_id.clone(), Request::ConfigureMetrics(config.clone()))
                .await;
            count += 1;
            self.in_flight.insert(req_id, (metrics_tx.clone(), 1));
        }

        let prefix = format!("{}-metrics-", client_id);

        let command_tx = self.command_tx.clone();
        let thread_client_id = client_id.clone();
        smol::spawn(async move {
            let mut responses = Vec::new();
            let mut i = 0;
            while let Some((proxy_response, _)) = metrics_rx.next().await {
                match proxy_response.status {
                    ResponseStatus::Ok => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        responses.push((tag, proxy_response));
                    }
                    ResponseStatus::Processing => {
                        //info!("metrics processing");
                        continue;
                    }
                    ResponseStatus::Failure => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        responses.push((tag, proxy_response));
                    }
                };

                i += 1;
                if i == count {
                    break;
                }
            }

            let mut messages = vec![];
            let mut has_error = false;
            for response in responses.iter() {
                match response.1.status {
                    ResponseStatus::Failure => {
                        messages.push(format!("{}: {}", response.0, response.1.message));
                        has_error = true;
                    }
                    _ => messages.push(format!("{}: OK", response.0)),
                }
            }

            if has_error {
                return_error(command_tx, thread_client_id, messages.join(", ")).await;
            } else {
                return_success(command_tx, thread_client_id, Success::Metrics(config)).await;
            }
        })
        .detach();
        Ok(None)
    }

    pub async fn query(
        &mut self,
        client_id: String,
        request: Request,
    ) -> anyhow::Result<Option<Success>> {
        debug!("Received this query: {:?}", request);
        let (query_tx, mut query_rx) = futures::channel::mpsc::channel(self.workers.len() * 2);
        let mut count = 0usize;
        for worker in self
            .workers
            .iter_mut()
            .filter(|worker| worker.run_state != RunState::Stopped)
        {
            let req_id = format!("{}-query-{}", client_id, worker.id);
            worker.send(req_id.clone(), request.clone()).await;
            count += 1;
            self.in_flight.insert(req_id, (query_tx.clone(), 1));
        }

        return_processing(
            self.command_tx.clone(),
            client_id.clone(),
            "Query was sent to the workers...",
        )
        .await;

        let mut main_response_content = None;
        match &request {
            Request::QueryClustersHashes => {
                main_response_content = Some(ResponseContent::ClustersHashes(self.state.hash_state()));
            }
            Request::QueryClusters(query_type) => {
                main_response_content = Some(ResponseContent::Clusters(match query_type {
                    QueryClusterType::ClusterId(cluster_id) => {
                        vec![self.state.cluster_state(cluster_id)]
                    }
                    QueryClusterType::Domain(domain) => {
                        let cluster_ids = get_cluster_ids_by_domain(
                            &self.state,
                            domain.hostname.clone(),
                            domain.path.clone(),
                        );
                        cluster_ids
                            .iter()
                            .map(|cluster_id| self.state.cluster_state(cluster_id))
                            .collect()
                    }
                }));
            }
            Request::QueryCertificates(_) => {}
            Request::QueryMetrics(_) => {}
            _ => {}
        };

        // all these are passed to the thread
        let command_tx = self.command_tx.clone();
        let cloned_identifier = client_id.clone();

        // this may waste resources and time in case of queries others than Metrics
        let main_metrics =
            METRICS.with(|metrics| (*metrics.borrow_mut()).dump_local_proxy_metrics());

        smol::spawn(async move {
            let mut responses = Vec::new();
            let mut i = 0;
            while let Some((proxy_response, worker_id)) = query_rx.next().await {
                match proxy_response.status {
                    ResponseStatus::Ok => {
                        responses.push((worker_id, proxy_response));
                    }
                    ResponseStatus::Processing => {
                        info!("metrics processing");
                        continue;
                    }
                    ResponseStatus::Failure => {
                        responses.push((worker_id, proxy_response));
                    }
                };

                i += 1;
                if i == count {
                    break;
                }
            }

            let mut worker_responses: BTreeMap<String, ResponseContent> = responses
                .into_iter()
                .filter_map(|(worker_id, proxy_response)| {
                    if let Some(response_content) = proxy_response.content {
                        Some((worker_id.to_string(), response_content))
                    } else {
                        None
                    }
                })
                .collect();

            let success = match &request {
                &Request::QueryClustersHashes | &Request::QueryClusters(_) => {
                    let main = main_response_content.unwrap(); // we should refactor to avoid this unwrap()
                    worker_responses.insert(String::from("main"), main);
                    Success::Query(ResponseContent::WorkerResponses(worker_responses))
                }
                &Request::QueryCertificates(_) => {
                    info!("certificates query answer received: {:?}", worker_responses);
                    Success::Query(ResponseContent::WorkerResponses(worker_responses))
                }
                Request::QueryMetrics(options) => {
                    debug!("metrics query answer received: {:?}", worker_responses);

                    if options.list {
                        Success::Query(ResponseContent::WorkerResponses(worker_responses))
                    } else {
                        Success::Query(ResponseContent::Metrics(AggregatedMetricsData {
                            main: main_metrics,
                            workers: worker_responses,
                        }))
                    }
                }
                _ => return, // very very unlikely
            };

            return_success(command_tx, cloned_identifier, success).await;
        })
        .detach();

        Ok(None)
    }

    pub fn set_logging_level(&mut self, logging_filter: String) -> anyhow::Result<Option<Success>> {
        debug!("Changing main process log level to {}", logging_filter);
        logging::LOGGER.with(|l| {
            let directives = logging::parse_logging_spec(&logging_filter);
            l.borrow_mut().set_directives(directives);
        });
        // also change / set the content of RUST_LOG so future workers / main thread
        // will have the new logging filter value
        ::std::env::set_var("RUST_LOG", &logging_filter);
        debug!("Logging level now: {}", ::std::env::var("RUST_LOG")?);
        Ok(Some(Success::Logging(logging_filter)))
    }

    pub async fn worker_requests(
        &mut self,
        client_id: String,
        request: Request,
    ) -> anyhow::Result<Option<Success>> {
        if let &Request::AddCertificate(_) = &request {
            debug!("workerconfig client request AddCertificate()");
        } else {
            debug!("workerconfig client request {:?}", request);
        }

        self.state
            .dispatch(&request)
            .with_context(|| "Could not execute request on the state")?;

        if self.config.automatic_state_save
            & (request != Request::SoftStop || request != Request::HardStop)
        {
            if let Some(path) = self.config.saved_state.clone() {
                return_processing(
                    self.command_tx.clone(),
                    client_id.clone(),
                    "Saving state to file",
                )
                .await;
                let mut file = File::create(&path)
                    .with_context(|| "Could not create file to automatically save the state")?;

                self.save_state_to_file(&mut file)
                    .with_context(|| format!("could not save state automatically to {path}"))?;
            }
        }

        return_processing(
            self.command_tx.clone(),
            client_id.clone(),
            "Sending the request to all workers".to_owned(),
        )
        .await;

        let (worker_request_tx, mut worker_request_rx) =
            futures::channel::mpsc::channel(self.workers.len() * 2);
        let mut found = false;
        let mut stopping_workers = HashSet::new();
        let mut worker_count = 0usize;
        for worker in self.workers.iter_mut().filter(|worker| worker.is_active()) {
            let should_stop_worker = request == Request::SoftStop || request == Request::HardStop;
            if should_stop_worker {
                worker.run_state = RunState::Stopping;
                stopping_workers.insert(worker.id);
            }

            let req_id = format!("{}-worker-{}", client_id, worker.id);
            worker.send(req_id.clone(), request.clone()).await;
            self.in_flight
                .insert(req_id, (worker_request_tx.clone(), 1));

            found = true;
            worker_count += 1;
        }

        let should_stop_main = request.is_a_stop();

        let mut command_tx = self.command_tx.clone();
        let thread_client_id = client_id.clone();

        smol::spawn(async move {
            let mut responses = Vec::new();
            let mut response_count = 0usize;
            while let Some((proxy_response, worker_id)) = worker_request_rx.next().await {
                match proxy_response.status {
                    ResponseStatus::Ok => {
                        responses.push((worker_id, proxy_response));

                        if stopping_workers.contains(&worker_id) {
                            if let Err(e) = command_tx
                                .send(CommandMessage::WorkerClose { worker_id })
                                .await
                            {
                                error!(
                                    "could not send worker close message to {}: {:?}",
                                    worker_id, e
                                );
                            }
                        }
                    }
                    ResponseStatus::Processing => {
                        info!("request is processing");
                        continue;
                    }
                    ResponseStatus::Failure => {
                        responses.push((worker_id, proxy_response));
                    }
                };

                response_count += 1;
                if response_count == worker_count {
                    break;
                }
            }

            // send the request to kill the main process only after all workers responded
            if should_stop_main {
                if let Err(e) = command_tx.send(CommandMessage::MasterStop).await {
                    error!("could not send main stop message: {:?}", e);
                }
            }

            let mut messages = vec![];
            let mut has_error = false;
            for response in responses.iter() {
                match response.1.status {
                    ResponseStatus::Failure => {
                        messages.push(format!("{}: {}", response.0, response.1.message));
                        has_error = true;
                    }
                    _ => messages.push(format!("{}: OK", response.0)),
                }
            }

            if has_error {
                return_error(command_tx, thread_client_id, messages.join(", ")).await;
            } else {
                return_success(command_tx, thread_client_id, Success::WorkerRequest).await;
            }
        })
        .detach();

        if !found {
            bail!("no worker found");
        }

        match request {
            Request::AddBackend(_) | Request::RemoveBackend(_) => {
                self.backends_count = self.state.count_backends()
            }
            Request::AddHttpFrontend(_)
            | Request::AddHttpsFrontend(_)
            | Request::AddTcpFrontend(_)
            | Request::RemoveHttpFrontend(_)
            | Request::RemoveHttpsFrontend(_)
            | Request::RemoveTcpFrontend(_) => self.frontends_count = self.state.count_frontends(),
            _ => {}
        };

        gauge!("configuration.clusters", self.state.clusters.len());
        gauge!("configuration.backends", self.backends_count);
        gauge!("configuration.frontends", self.frontends_count);

        Ok(None)
    }

    pub async fn notify_advancement_to_client(
        &mut self,
        client_id: String,
        response: Advancement,
    ) -> anyhow::Result<Success> {
        let command_response = match response {
            Advancement::Ok(success) => {
                let success_message = success.to_string();

                let command_response_data = match success {
                    // should list Success::Metrics(crd) as well
                    Success::DumpState(crd)
                    | Success::ListFrontends(crd)
                    | Success::ListWorkers(crd)
                    | Success::Query(crd)
                    | Success::ListListeners(crd)
                    | Success::Status(crd) => Some(crd),
                    _ => None,
                };

                Response::new(ResponseStatus::Ok, success_message, command_response_data)
            }
            Advancement::Processing(processing_message) => {
                Response::new(ResponseStatus::Processing, processing_message, None)
            }
            Advancement::Error(error_message) => {
                Response::new(ResponseStatus::Failure, error_message, None)
            }
        };

        trace!(
            "Sending response to request sent by client {}: {:?}",
            client_id,
            command_response
        );

        match self.clients.get_mut(&client_id) {
            Some(client_tx) => {
                trace!("sending from main process to client loop");
                client_tx.send(command_response).await.with_context(|| {
                    format!("Could not notify client {} about request", client_id)
                })?;
            }
            None => bail!(format!("Could not find client {client_id}")),
        }

        Ok(Success::NotifiedClient(client_id))
    }
}

// Those return functions are meant to be called in detached threads
// to notify the command server of an request's advancement.
async fn return_error<T>(
    mut command_tx: Sender<CommandMessage>,
    client_id: String,
    error_message: T,
) where
    T: ToString,
{
    let advancement = CommandMessage::Advancement {
        client_id,
        advancement: Advancement::Error(error_message.to_string()),
    };

    trace!("return_error: sending event to the command server");
    if let Err(e) = command_tx.send(advancement).await {
        error!("Error while return error to the command server: {}", e)
    }
}

async fn return_processing<T>(
    mut command_tx: Sender<CommandMessage>,
    client_id: String,
    processing_message: T,
) where
    T: ToString,
{
    let advancement = CommandMessage::Advancement {
        client_id,
        advancement: Advancement::Processing(processing_message.to_string()),
    };

    trace!("return_processing: sending event to the command server");
    if let Err(e) = command_tx.send(advancement).await {
        error!(
            "Error while returning processing to the command server: {}",
            e
        )
    }
}

async fn return_success(
    mut command_tx: Sender<CommandMessage>,
    client_id: String,
    success: Success,
) {
    let advancement = CommandMessage::Advancement {
        client_id,
        advancement: Advancement::Ok(success),
    };
    trace!("return_success: sending event to the command server");
    if let Err(e) = command_tx.send(advancement).await {
        error!("Error while returning success to the command server: {}", e)
    }
}
