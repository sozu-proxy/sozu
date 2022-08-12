use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{Read, Write},
    os::unix::io::{FromRawFd, IntoRawFd},
    os::unix::net::UnixStream,
    time::Duration,
};

use anyhow::{bail, Context};
use async_io::Async;
use futures::{channel::mpsc::*, SinkExt, StreamExt};
use nom::{Err, HexDisplay, Offset};
use serde_json;

use sozu_command_lib::{
    buffer::fixed::Buffer,
    command::{
        CommandRequest, CommandRequestData, CommandResponse, CommandResponseData, CommandStatus,
        FrontendFilters, ListedFrontends, RunState, WorkerInfo, PROTOCOL_VERSION,
    },
    config::Config,
    logging,
    parser::parse_several_commands,
    proxy::{
        MetricsConfiguration, ProxyRequest, ProxyRequestData, ProxyResponseData,
        ProxyResponseStatus, Query, QueryAnswer, QueryClusterType, Route, TcpFrontend,
    },
    scm_socket::Listeners,
    state::get_cluster_ids_by_domain,
};

use crate::{
    command::{CommandMessage, CommandServer, RequestIdentifier, Response, Success},
    upgrade::start_new_main_process,
    worker::start_worker,
};

impl CommandServer {
    pub async fn handle_client_request(
        &mut self,
        client_id: String,
        request: CommandRequest,
    ) -> anyhow::Result<Success> {
        trace!("Received order {:?}", request);
        let request_identifier = RequestIdentifier {
            client: client_id.to_owned(),
            request: request.id.to_owned(),
        };
        let cloned_identifier = request_identifier.clone();

        let result: anyhow::Result<Option<Success>> = match request.data {
            CommandRequestData::SaveState { path } => self.save_state(&path).await,
            CommandRequestData::DumpState => self.dump_state().await,
            CommandRequestData::ListWorkers => self.list_workers().await,
            CommandRequestData::ListFrontends(filters) => self.list_frontends(filters).await,
            CommandRequestData::LoadState { path } => {
                self.load_state(
                    Some(request_identifier.client),
                    request_identifier.request,
                    &path,
                )
                .await
            }
            CommandRequestData::LaunchWorker(tag) => {
                self.launch_worker(request_identifier, &tag).await
            }
            CommandRequestData::UpgradeMain => self.upgrade_main(request_identifier).await,
            CommandRequestData::UpgradeWorker(worker_id) => {
                self.upgrade_worker(request_identifier, worker_id).await
            }
            CommandRequestData::Proxy(proxy_request) => match proxy_request {
                ProxyRequestData::Metrics(config) => self.metrics(request_identifier, config).await,
                ProxyRequestData::Query(query) => self.query(request_identifier, query).await,
                ProxyRequestData::Logging(logging_filter) => self.set_logging_level(logging_filter),
                // we should have something like
                // ProxyRequestData::SoftStop => self.do_something(),
                // ProxyRequestData::HardStop => self.do_nothing_and_return_early(),
                // but it goes in there instead:
                order => {
                    self.worker_order(request_identifier, order, request.worker_id)
                        .await
                }
            },
            CommandRequestData::SubscribeEvents => {
                self.event_subscribers.insert(client_id.clone());
                Ok(Some(Success::SubscribeEvent(client_id.clone())))
            }
            CommandRequestData::ReloadConfiguration { path } => {
                self.reload_configuration(request_identifier, path).await
            }
        };

        // Notify the command server by sending using his command_tx
        match result {
            Ok(Some(success)) => {
                info!("{}", success);
                return_success(self.command_tx.clone(), cloned_identifier, success).await;
            }
            Err(error_message) => {
                error!("{}", error_message);
                return_error(self.command_tx.clone(), cloned_identifier, error_message).await;
            }
            Ok(None) => {
                // do nothing here. Ok(None) means the function has already returned its result
                // on its own to the command server
            }
        }

        Ok(Success::HandledClientRequest)
    }

    pub async fn save_state(&mut self, path: &str) -> anyhow::Result<Option<Success>> {
        let mut file = File::create(&path)
            .with_context(|| format!("could not open file at path: {}", &path))?;

        let counter = self
            .save_state_to_file(&mut file)
            .with_context(|| "failed writing state to file")?;

        info!("wrote {} commands to {}", counter, path);

        Ok(Some(Success::SaveState(counter, path.into())))
    }

    pub fn save_state_to_file(&mut self, file: &mut File) -> anyhow::Result<usize> {
        let mut counter = 0usize;
        let orders = self.state.generate_orders();

        let result: anyhow::Result<usize> = (move || {
            for command in orders {
                let message = CommandRequest::new(
                    format!("SAVE-{}", counter),
                    CommandRequestData::Proxy(command),
                    None,
                );

                file.write_all(
                    &serde_json::to_string(&message)
                        .map(|s| s.into_bytes())
                        .unwrap_or(vec![]),
                )
                .with_context(|| {
                    format!(
                        "Could not add this instruction line to the saved state file: {:?}",
                        message
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

        Ok(result.with_context(|| "Could not write the state onto the state file")?)
    }

    pub async fn dump_state(&mut self) -> anyhow::Result<Option<Success>> {
        let state = self.state.clone();

        Ok(Some(Success::DumpState(CommandResponseData::State(state))))
    }

    pub async fn load_state(
        &mut self,
        client_id: Option<String>,
        request_id: String,
        path: &str,
    ) -> anyhow::Result<Option<Success>> {
        let mut file =
            File::open(&path).with_context(|| format!("Cannot open file at path {}", path))?;

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
            match parse_several_commands::<CommandRequest>(buffer.data()) {
                Ok((i, requests)) => {
                    if i.len() > 0 {
                        debug!("could not parse {} bytes", i.len());
                        if previous == buffer.available_data() {
                            bail!("error consuming load state message");
                        }
                    }
                    offset = buffer.data().offset(i);

                    if requests.iter().find(|o| {
                        if o.version > PROTOCOL_VERSION {
                            error!("configuration protocol version mismatch: Sōzu handles up to version {}, the message uses version {}", PROTOCOL_VERSION, o.version);
                            true
                        } else {
                            false
                        }
                    }).is_some() {
                        break;
                    }

                    for request in requests {
                        if let CommandRequestData::Proxy(order) = request.data {
                            message_counter += 1;

                            if self.state.handle_order(&order) {
                                diff_counter += 1;

                                let mut found = false;
                                let id = format!("LOAD-STATE-{}-{}", request_id, diff_counter);

                                for ref mut worker in self.workers.iter_mut().filter(|worker| {
                                    worker.run_state != RunState::Stopping
                                        && worker.run_state != RunState::Stopped
                                }) {
                                    let worker_message_id = format!("{}-{}", id, worker.id);
                                    worker.send(worker_message_id.clone(), order.clone()).await;
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
                while let Some(proxy_response) = load_state_rx.next().await {
                    match proxy_response.status {
                        ProxyResponseStatus::Ok => {
                            ok += 1;
                        }
                        ProxyResponseStatus::Processing => {}
                        ProxyResponseStatus::Error(message) => {
                            error!("{}", message);
                            error += 1;
                        }
                    };
                    debug!("ok:{}, error: {}", ok, error);
                }

                let request_identifier = if let Some(client_id) = client_id {
                    RequestIdentifier::new(client_id, request_id)
                } else {
                    if error == 0 {
                        info!("loading state: {} ok messages, 0 errors", ok);
                    } else {
                        error!("loading state: {} ok messages, {} errors", ok, error);
                    }
                    return;
                };

                // notify the command server
                if error == 0 {
                    return_success(
                        command_tx,
                        request_identifier,
                        Success::LoadState(path.to_string(), ok, error),
                    )
                    .await;
                } else {
                    return_error(
                        command_tx,
                        request_identifier,
                        format!(
                            "Loading state failed, ok: {}, error: {}, path: {}",
                            ok, error, path
                        ),
                    )
                    .await;
                }
            })
            .detach();
        } else {
            info!("no messages sent to workers: local state already had those messages");
            if client_id.is_some() {
                return_success(
                    self.command_tx.clone(),
                    RequestIdentifier::new(client_id.unwrap(), request_id),
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

        if (filters.tcp || list_all) && !filters.domain.is_some() {
            for tcp_frontend in self.state.tcp_fronts.values().map(|v| v.iter()).flatten() {
                listed_frontends.tcp_frontends.push(tcp_frontend.to_owned())
            }
        }

        Ok(Some(Success::ListFrontends(
            CommandResponseData::FrontendList(listed_frontends),
        )))
    }

    pub async fn list_workers(&mut self) -> anyhow::Result<Option<Success>> {
        let workers: Vec<WorkerInfo> = self
            .workers
            .iter()
            .map(|ref worker| WorkerInfo {
                id: worker.id,
                pid: worker.pid,
                run_state: worker.run_state.clone(),
            })
            .collect();

        Ok(Some(Success::ListWorkers(CommandResponseData::Workers(
            workers,
        ))))
    }

    pub async fn launch_worker(
        &mut self,
        request_identifier: RequestIdentifier,
        _tag: &str,
    ) -> anyhow::Result<Option<Success>> {
        let mut worker = start_worker(
            self.next_id,
            &self.config,
            self.executable_path.clone(),
            &self.state,
            None,
        )
        .with_context(|| format!("Failed at creating worker {}", self.next_id))?;

        // Todo: refactor the CLI, see issue #740
        // return_processing(
        //     self.command_tx.clone(),
        //     request_identifier.clone(),
        //     "Sending configuration orders",
        // )
        // .await;

        info!("created new worker: {}", worker.id);

        self.next_id += 1;

        let sock = worker.channel.take().unwrap().sock;
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
            worker.scm.send_listeners(&Listeners {
                http: Vec::new(),
                tls: Vec::new(),
                tcp: Vec::new(),
            })
        );

        let activate_orders = self.state.generate_activate_orders();
        let mut count = 0usize;
        for order in activate_orders.into_iter() {
            worker
                .send(format!("{}-ACTIVATE-{}", id, count), order)
                .await;
            count += 1;
        }

        self.workers.push(worker);

        return_success(
            self.command_tx.clone(),
            request_identifier,
            Success::WorkerLaunched(id),
        )
        .await;
        Ok(None)
    }

    pub async fn upgrade_main(
        &mut self,
        _request_identifier: RequestIdentifier,
    ) -> anyhow::Result<Option<Success>> {
        self.disable_cloexec_before_upgrade()?;

        // Todo: refactor the CLI, see issue #740
        // return_processing(
        //     self.command_tx.clone(),
        //     request_identifier,
        //     "The proxy is processing the upgrade command.",
        // )
        // .await;

        let (pid, mut channel) =
            start_new_main_process(self.executable_path.clone(), self.generate_upgrade_data())
                .with_context(|| "Could not start a new main process")?;

        channel.set_blocking(true);
        let res = channel.read_message();
        debug!("upgrade channel sent {:?}", res);

        // signaling the accept loop that it should stop
        if let Err(e) = self.accept_cancel.take().unwrap().send(()) {
            error!("could not close the accept loop: {:?}", e);
        }

        match res {
            Some(true) => {
                info!("wrote final message, closing");
                Ok(Some(Success::UpgradeMain(pid)))
            }
            _ => bail!("could not upgrade main process"),
        }
    }

    pub async fn upgrade_worker(
        &mut self,
        request_identifier: RequestIdentifier,
        id: u32,
    ) -> anyhow::Result<Option<Success>> {
        info!(
            "client[{}] msg {} wants to upgrade worker {}",
            request_identifier.client, request_identifier.request, id
        );

        if self
            .workers
            .iter()
            .find(|worker| {
                worker.id == id
                    && worker.run_state != RunState::Stopping
                    && worker.run_state != RunState::Stopped
            })
            .is_none()
        {
            bail!(format!(
                "The worker {} does not exist, or is stopped / stopping.",
                &id
            ));
        }

        // same as launch_worker
        let next_id = self.next_id;
        let mut worker = start_worker(
            next_id,
            &self.config,
            self.executable_path.clone(),
            &self.state,
            None,
        )
        .with_context(|| "failed at creating worker")?;

        // Todo: refactor the CLI, see issue #740
        // return_processing(
        //     self.command_tx.clone(),
        //     request_identifier.clone(),
        //     "sending configuration orders",
        // )
        // .await;

        info!("created new worker: {}", next_id);

        self.next_id += 1;

        let sock = worker.channel.take().unwrap().sock;
        let (worker_tx, worker_rx) = channel(10000);
        worker.sender = Some(worker_tx);

        worker
            .sender
            .as_mut()
            .unwrap()
            .send(ProxyRequest {
                id: format!("UPGRADE-{}-STATUS", id),
                order: ProxyRequestData::Status,
            })
            .await
            .with_context(|| format!("could not send status message to worker {:?}", worker.id,))?;

        let mut listeners = None;
        {
            let old_worker = self
                .workers
                .iter_mut()
                .filter(|worker| worker.id == id)
                .next()
                .unwrap();

            /*
            old_worker.channel.set_blocking(true);
            old_worker.channel.write_message(&ProxyRequest { id: String::from(message_id), order: ProxyRequestData::ReturnListenSockets });
            info!("sent returnlistensockets message to worker");
            old_worker.channel.set_blocking(false);
            */
            let (sockets_return_tx, mut sockets_return_rx) = futures::channel::mpsc::channel(3);
            let id = format!("{}-return-sockets", request_identifier.client);
            self.in_flight.insert(id.clone(), (sockets_return_tx, 1));
            old_worker
                .send(id.clone(), ProxyRequestData::ReturnListenSockets)
                .await;

            info!("sent ReturnListenSockets to old worker");

            // should we notify the command server here?
            smol::spawn(async move {
                while let Some(proxy_response) = sockets_return_rx.next().await {
                    match proxy_response.status {
                        ProxyResponseStatus::Ok => {
                            info!("returnsockets OK");
                            break;
                        }
                        ProxyResponseStatus::Processing => {
                            info!("returnsockets processing");
                        }
                        ProxyResponseStatus::Error(message) => {
                            info!("return sockets error: {:?}", message);
                            break;
                        }
                    };
                }
            })
            .detach();

            let mut counter = 0usize;

            //FIXME: use blocking
            loop {
                info!("waiting for scm sockets");
                old_worker.scm.set_blocking(true);
                if let Some(l) = old_worker.scm.receive_listeners() {
                    listeners = Some(l);
                    break;
                } else {
                    counter += 1;
                    if counter == 50 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            info!("got scm sockets");
            old_worker.run_state = RunState::Stopping;

            let (softstop_tx, mut softstop_rx) = futures::channel::mpsc::channel(10);
            let id = format!("{}-softstop", request_identifier.client);
            self.in_flight.insert(id.clone(), (softstop_tx, 1));
            old_worker
                .send(id.clone(), ProxyRequestData::SoftStop)
                .await;

            let mut command_tx = self.command_tx.clone();
            let worker_id = old_worker.id;
            smol::spawn(async move {
                while let Some(proxy_response) = softstop_rx.next().await {
                    match proxy_response.status {
                        // should we send all this to the command server?
                        ProxyResponseStatus::Ok => {
                            info!("softstop OK"); // this doesn't display :-(
                            if let Err(e) = command_tx
                                .send(CommandMessage::WorkerClose {
                                    id: worker_id.clone(),
                                })
                                .await
                            {
                                error!(
                                    "could not send worker close message to {}: {:?}",
                                    worker_id, e
                                );
                            }
                            break;
                        }
                        ProxyResponseStatus::Processing => {
                            info!("softstop processing");
                        }
                        ProxyResponseStatus::Error(message) => {
                            info!("softstop error: {:?}", message);
                            break;
                        }
                    };
                }
                // The best would be to return Processing here. See issue #740
            })
            .detach();
        }

        match listeners {
            Some(l) => {
                info!(
                    "sending listeners: to the new worker: {:?}",
                    worker.scm.send_listeners(&l)
                );
                l.close();
            }
            None => error!("could not get the list of listeners from the previous worker"),
        };

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

        let activate_orders = self.state.generate_activate_orders();
        let mut count = 0usize;
        for order in activate_orders.into_iter() {
            worker
                .send(
                    format!("{}-ACTIVATE-{}", request_identifier.client, count),
                    order,
                )
                .await;
            count += 1;
        }
        info!("sent config messages to the new worker");
        self.workers.push(worker);

        info!("finished upgrade");
        Ok(Some(Success::UpgradeWorker(id)))
    }

    pub async fn reload_configuration(
        &mut self,
        request_identifier: RequestIdentifier,
        config_path: Option<String>,
    ) -> anyhow::Result<Option<Success>> {
        // check that this works
        let path = config_path.as_deref().unwrap_or(&self.config.config_path);
        let new_config = Config::load_from_path(path)
            .with_context(|| format!("cannot load configuration from '{}'", path))?;

        let mut diff_counter = 0usize;

        let (load_state_tx, mut load_state_rx) = futures::channel::mpsc::channel(10000);

        for message in new_config.generate_config_messages() {
            if let CommandRequestData::Proxy(order) = message.data {
                if self.state.handle_order(&order) {
                    diff_counter += 1;

                    let mut found = false;
                    let id = format!("LOAD-STATE-{}-{}", request_identifier.request, diff_counter);

                    for ref mut worker in self.workers.iter_mut().filter(|worker| {
                        worker.run_state != RunState::Stopping
                            && worker.run_state != RunState::Stopped
                    }) {
                        let worker_message_id = format!("{}-{}", id, worker.id);
                        worker.send(worker_message_id.clone(), order.clone()).await;
                        self.in_flight
                            .insert(worker_message_id, (load_state_tx.clone(), 1));

                        found = true;
                    }

                    if !found {
                        // FIXME: should send back error here
                        error!("no worker found");
                    }
                }
            }
        }

        // clone everything we will need in the detached thread
        let command_tx = self.command_tx.clone();
        let cloned_identifier = request_identifier.clone();

        if diff_counter > 0 {
            info!(
                "state loaded from {}, will start sending {} messages to workers",
                new_config.config_path, diff_counter
            );
            smol::spawn(async move {
                let mut ok = 0usize;
                let mut error = 0usize;
                while let Some(proxy_response) = load_state_rx.next().await {
                    match proxy_response.status {
                        ProxyResponseStatus::Ok => {
                            ok += 1;
                        }
                        ProxyResponseStatus::Processing => {}
                        ProxyResponseStatus::Error(message) => {
                            error!("{}", message);
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
                            "Reloading configuration failed. ok: {} messages, error: {}",
                            ok, error
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

        // Todo: refactor the CLI to accept processing, see issue #740
        // return_processing(
        //     self.command_tx.clone(),
        //     request_identifier,
        //     "Reloading configuration...",
        // )
        // .await;
        Ok(None)
    }

    // This handles the CLI's "metrics enable", "metrics disable", "metrics clear"
    // To get the proxy's metrics, the cli command is "query metrics", handled by the query() function
    pub async fn metrics(
        &mut self,
        request_identifier: RequestIdentifier,
        config: MetricsConfiguration,
    ) -> anyhow::Result<Option<Success>> {
        let (metrics_tx, mut metrics_rx) = futures::channel::mpsc::channel(self.workers.len() * 2);
        let mut count = 0usize;
        for ref mut worker in self
            .workers
            .iter_mut()
            .filter(|worker| worker.run_state != RunState::Stopped)
        {
            let req_id = format!("{}-metrics-{}", request_identifier.client, worker.id);
            worker
                .send(req_id.clone(), ProxyRequestData::Metrics(config.clone()))
                .await;
            count += 1;
            self.in_flight.insert(req_id, (metrics_tx.clone(), 1));
        }

        let prefix = format!("{}-metrics-", request_identifier.client);

        let command_tx = self.command_tx.clone();
        let thread_request_identifier = request_identifier.clone();
        smol::spawn(async move {
            let mut responses = Vec::new();
            let mut i = 0;
            while let Some(proxy_response) = metrics_rx.next().await {
                match proxy_response.status {
                    ProxyResponseStatus::Ok => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        responses.push((tag, proxy_response));
                    }
                    ProxyResponseStatus::Processing => {
                        //info!("metrics processing");
                        continue;
                    }
                    ProxyResponseStatus::Error(_) => {
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
                    ProxyResponseStatus::Error(ref e) => {
                        messages.push(format!("{}: {}", response.0, e));
                        has_error = true;
                    }
                    _ => messages.push(format!("{}: OK", response.0)),
                }
            }

            if has_error {
                return_error(command_tx, thread_request_identifier, messages.join(", ")).await;
            } else {
                return_success(
                    command_tx,
                    thread_request_identifier,
                    Success::Metrics(config),
                )
                .await;
            }
        })
        .detach();
        Ok(None)
    }

    pub async fn query(
        &mut self,
        request_identifier: RequestIdentifier,
        query: Query,
    ) -> anyhow::Result<Option<Success>> {

        debug!("Received this query: {:?}", query);
        let (query_tx, mut query_rx) = futures::channel::mpsc::channel(self.workers.len() * 2);
        let mut count = 0usize;
        for ref mut worker in self
            .workers
            .iter_mut()
            .filter(|worker| worker.run_state != RunState::Stopped)
        {
            let req_id = format!("{}-query-{}", request_identifier.client, worker.id);
            worker
                .send(req_id.clone(), ProxyRequestData::Query(query.clone()))
                .await;
            count += 1;
            self.in_flight.insert(req_id, (query_tx.clone(), 1));
        }

        let mut main_query_answer = None;
        match &query {
            &Query::ClustersHashes => {
                main_query_answer = Some(QueryAnswer::ClustersHashes(self.state.hash_state()));
            }
            &Query::Clusters(ref query_type) => {
                main_query_answer = Some(QueryAnswer::Clusters(match query_type {
                    QueryClusterType::ClusterId(ref cluster_id) => {
                        vec![self.state.cluster_state(cluster_id)]
                    }
                    QueryClusterType::Domain(ref domain) => {
                        let cluster_ids = get_cluster_ids_by_domain(
                            &self.state,
                            domain.hostname.clone(),
                            domain.path.clone(),
                        );
                        cluster_ids
                            .iter()
                            .map(|ref cluster_id| self.state.cluster_state(cluster_id))
                            .collect()
                    }
                }));
            }
            &Query::Certificates(_) => {}
            &Query::Metrics(_) => {}
        };

        // all theses are passed to the thread
        let prefix = format!("{}-query-", request_identifier.client);
        let command_tx = self.command_tx.clone();
        let cloned_identifier = request_identifier.clone();
        smol::spawn(async move {
            let mut responses = Vec::new();
            let mut i = 0;
            while let Some(proxy_response) = query_rx.next().await {
                match proxy_response.status {
                    ProxyResponseStatus::Ok => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        responses.push((tag, proxy_response));
                    }
                    ProxyResponseStatus::Processing => {
                        info!("metrics processing");
                        continue;
                    }
                    ProxyResponseStatus::Error(_) => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        responses.push((tag, proxy_response));
                    }
                };

                i += 1;
                if i == count {
                    break;
                }
            }

            let mut query_answers_map: BTreeMap<String, QueryAnswer> = responses
                .into_iter()
                .filter_map(|(tag, query)| {
                    if let Some(ProxyResponseData::Query(d)) = query.data {
                        Some((tag, d))
                    } else {
                        None
                    }
                })
                .collect();

            let success = match &query {
                &Query::ClustersHashes | &Query::Clusters(_) => {
                    let main = main_query_answer.unwrap();
                    query_answers_map.insert(String::from("main"), main);
                    Success::Query(CommandResponseData::Query(query_answers_map))
                }
                &Query::Certificates(_) => {
                    info!("certificates query answer received: {:?}", query_answers_map);
                    Success::Query(CommandResponseData::Query(query_answers_map))
                }
                &Query::Metrics(_) => {
                    debug!("metrics query answer received: {:?}", query_answers_map);
                    Success::Query(CommandResponseData::Query(query_answers_map))
                }
            };

            return_success(command_tx, cloned_identifier, success).await;
        })
        .detach();

        // Todo: refactor the CLI to accept processing, see issue #740
        // return_processing(
        //     self.command_tx.clone(),
        //     request_identifier,
        //     "Processing the query…",
        // )
        // .await;
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
        ::std::env::set_var("RUST_LOG", logging_filter.to_owned());
        debug!("Logging level now: {}", ::std::env::var("RUST_LOG")?);
        Ok(Some(Success::Logging(logging_filter)))
    }

    pub async fn worker_order(
        &mut self,
        request_identifier: RequestIdentifier,
        order: ProxyRequestData,
        worker_id: Option<u32>,
    ) -> anyhow::Result<Option<Success>> {
        if let &ProxyRequestData::AddCertificate(_) = &order {
            debug!("workerconfig client order AddCertificate()");
        } else {
            debug!("workerconfig client order {:?}", order);
        }

        if !self.state.handle_order(&order) {
            // Check if the backend or frontend exist before deleting it
            if worker_id.is_none() {
                match order {
                    ProxyRequestData::RemoveBackend(ref backend) => {
                        bail!(format!(
                            "cannot remove backend: cluster {} has no backends {} at {}",
                            backend.cluster_id, backend.backend_id, backend.address,
                        ));
                    }
                    ProxyRequestData::RemoveHttpFrontend(h)
                    | ProxyRequestData::RemoveHttpsFrontend(h) => {
                        let msg = match h.route {
                            Route::ClusterId(cluster_id) => format!(
                                "No such frontend at {} for the cluster {}",
                                h.address, cluster_id
                            ),
                            Route::Deny => format!("No such frontend at {}", h.address),
                        };
                        bail!(msg);
                    }
                    ProxyRequestData::RemoveTcpFrontend(TcpFrontend {
                        ref cluster_id,
                        ref address,
                        ref tags,
                    }) => {
                        bail!(format!(
                            "cannot remove TCP frontend: cluster {} has no frontends at {} (custom tags: {:?})",
                            cluster_id, address, tags
                        ));
                    }
                    _ => {}
                };
            }
        }

        if self.config.automatic_state_save
            & (order != ProxyRequestData::SoftStop || order != ProxyRequestData::HardStop)
        {
            if let Some(path) = self.config.saved_state.clone() {
                let mut file = File::create(&path)
                    .with_context(|| "Could not create file to automatically save the state")?;

                self.save_state_to_file(&mut file)
                    .with_context(|| format!("could not save state automatically to {}", path))?;
            }
        }

        // sent out the order to all workers
        let (worker_order_tx, mut worker_order_rx) =
            futures::channel::mpsc::channel(self.workers.len() * 2);
        let mut found = false;
        let mut stopping_workers = HashSet::new();
        let mut worker_count = 0usize;
        for ref mut worker in self.workers.iter_mut().filter(|worker| {
            worker.run_state != RunState::Stopping && worker.run_state != RunState::Stopped
        }) {
            // sort out the specificly targeted worker, if provided
            if let Some(id) = worker_id {
                if id != worker.id {
                    continue;
                }
            }

            let should_stop_worker =
                order == ProxyRequestData::SoftStop || order == ProxyRequestData::HardStop;
            if should_stop_worker {
                worker.run_state = RunState::Stopping;
                stopping_workers.insert(worker.id);
            }

            let req_id = format!("{}-worker-{}", request_identifier.client, worker.id);
            worker.send(req_id.clone(), order.clone()).await;
            self.in_flight.insert(req_id, (worker_order_tx.clone(), 1));

            found = true;
            worker_count += 1;
        }

        let should_stop_main = (order == ProxyRequestData::SoftStop
            || order == ProxyRequestData::HardStop)
            && worker_id.is_none();

        let mut command_tx = self.command_tx.clone();
        let thread_request_identifier = request_identifier.clone();
        let prefix = format!("{}-worker-", request_identifier.client);
        smol::spawn(async move {
            let mut responses = Vec::new();
            let mut response_count = 0usize;
            while let Some(proxy_response) = worker_order_rx.next().await {
                match proxy_response.status {
                    ProxyResponseStatus::Ok => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        responses.push((tag.clone(), proxy_response));

                        let id: u32 = tag.parse().unwrap();
                        if stopping_workers.contains(&id) {
                            if let Err(e) = command_tx
                                .send(CommandMessage::WorkerClose { id: id.clone() })
                                .await
                            {
                                error!("could not send worker close message to {}: {:?}", id, e);
                            }
                        }
                    }
                    ProxyResponseStatus::Processing => {
                        info!("Order is processing");
                        continue;
                    }
                    ProxyResponseStatus::Error(_) => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        responses.push((tag, proxy_response));
                    }
                };

                response_count += 1;
                if response_count == worker_count {
                    break;
                }
            }

            // send the order to kill the main process only after all workers responded
            if should_stop_main {
                if let Err(e) = command_tx.send(CommandMessage::MasterStop).await {
                    error!("could not send main stop message: {:?}", e);
                }
            }

            let mut messages = vec![];
            let mut has_error = false;
            for response in responses.iter() {
                match response.1.status {
                    ProxyResponseStatus::Error(ref e) => {
                        messages.push(format!("{}: {}", response.0, e));
                        has_error = true;
                    }
                    _ => messages.push(format!("{}: OK", response.0)),
                }
            }

            if has_error {
                return_error(command_tx, thread_request_identifier, messages.join(", ")).await;
            } else {
                return_success(
                    command_tx,
                    thread_request_identifier,
                    Success::WorkerOrder(worker_id),
                )
                .await;
            }
        })
        .detach();

        if !found {
            // FIXME: should send back error here
            // is this fix OK?
            bail!("no worker found");
        }

        match order {
            ProxyRequestData::AddBackend(_) | ProxyRequestData::RemoveBackend(_) => {
                self.backends_count = self.state.count_backends()
            }
            ProxyRequestData::AddHttpFrontend(_)
            | ProxyRequestData::AddHttpsFrontend(_)
            | ProxyRequestData::AddTcpFrontend(_)
            | ProxyRequestData::RemoveHttpFrontend(_)
            | ProxyRequestData::RemoveHttpsFrontend(_)
            | ProxyRequestData::RemoveTcpFrontend(_) => {
                self.frontends_count = self.state.count_frontends()
            }
            _ => {}
        };

        gauge!("configuration.clusters", self.state.clusters.len());
        gauge!("configuration.backends", self.backends_count);
        gauge!("configuration.frontends", self.frontends_count);

        // Todo: refactor the CLI, see issue #740
        // return_processing(
        //     self.command_tx.clone(),
        //     request_identifier,
        //     "Processing worker order",
        // )
        // .await;

        Ok(None)
    }

    pub async fn notify_advancement_to_client(
        &mut self,
        request_identifier: RequestIdentifier,
        response: Response,
    ) -> anyhow::Result<Success> {
        let RequestIdentifier {
            client: client_id,
            request: request_id,
        } = request_identifier.to_owned();

        let command_response = match response {
            Response::Ok(success) => {
                let success_message = success.to_string();

                let command_response_data = match success {
                    // should list Success::Metrics(crd) as well
                    Success::DumpState(crd)
                    | Success::ListFrontends(crd)
                    | Success::ListWorkers(crd)
                    | Success::Query(crd) => Some(crd),
                    _ => None,
                };

                CommandResponse::new(
                    request_id.clone(),
                    CommandStatus::Ok,
                    success_message,
                    command_response_data,
                )
            }
            // Todo: refactor the CLI to accept processing, see issue #740
            // Response::Processing(_) => {
            //     command_response = CommandResponse::new(
            //         request_id.clone(),
            //         CommandStatus::Processing,
            //         processing_message,
            //         None,
            //     );
            // }
            Response::Error(error_message) => CommandResponse::new(
                request_id.clone(),
                CommandStatus::Error,
                error_message,
                None,
            ),
        };

        trace!(
            "Sending response to request {} of client {}: {:?}",
            request_id,
            client_id,
            command_response
        );

        match self.clients.get_mut(&client_id) {
            Some(sender) => {
                sender.send(command_response).await.with_context(|| {
                    format!(
                        "Could not notify client {} about request {}",
                        client_id, request_identifier.request,
                    )
                })?;
            }
            None => bail!(format!("Could not find client {}", client_id)),
        }

        Ok(Success::NotifiedClient(client_id))
    }
}

// Those return functions are meant to be called in detached threads
// to notify the command server of an order's advancement.
async fn return_error<T>(
    mut command_tx: Sender<CommandMessage>,
    request_identifier: RequestIdentifier,
    error_message: T,
) where
    T: ToString,
{
    let error_command_message = CommandMessage::Advancement {
        request_identifier: request_identifier.to_owned(),
        response: Response::Error(error_message.to_string()),
    };

    if let Err(e) = command_tx.send(error_command_message).await {
        error!("Error while return error to the command server: {}", e)
    }
}

// Todo: refactor the CLI, see issue #740
// async fn return_processing<T>(
//     mut command_tx: Sender<CommandMessage>,
//     request_identifier: RequestIdentifier,
//     processing_message: T,
// ) where
//     T: ToString,
// {
//     let processing_command_message = CommandMessage::Advancement {
//         request_identifier: request_identifier.to_owned(),
//         response: Response::Processing(processing_message.to_string()),
//     };
//
//     if let Err(e) = command_tx.send(processing_command_message).await {
//         error!(
//             "Error while returning processing to the command server: {}",
//             e
//         )
//     }
// }

async fn return_success(
    mut command_tx: Sender<CommandMessage>,
    request_identifier: RequestIdentifier,
    success: Success,
) {
    let success_command_message = CommandMessage::Advancement {
        request_identifier: request_identifier.to_owned(),
        response: Response::Ok(success),
    };

    if let Err(e) = command_tx.send(success_command_message).await {
        error!("Error while returning success to the command server: {}", e)
    }
}
