use anyhow::Context;
use futures::channel::mpsc::*;
use futures::{SinkExt, StreamExt};
use nom::{Err, HexDisplay, IResult, Offset};
use serde_json;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use async_io::Async;

use sozu_command::buffer::fixed::Buffer;
use sozu_command::command::{
    CommandRequest, CommandRequestData, CommandResponse, CommandResponseData, CommandStatus,
    RunState, WorkerInfo,
};
use sozu_command::logging;
use sozu_command::proxy::{
    MetricsConfiguration, ProxyRequest, ProxyRequestData, ProxyResponseData, ProxyResponseStatus,
    Query, QueryAnswer, QueryApplicationType, Route, TcpFrontend,
};
use sozu_command::scm_socket::Listeners;
use sozu_command::state::get_application_ids_by_domain;
use sozu_command_lib::config::Config;

use super::{CommandMessage, CommandServer};
use crate::upgrade::start_new_main_process;
use crate::worker::start_worker;

impl CommandServer {
    pub async fn handle_client_message(
        &mut self,
        client_id: String,
        request: sozu_command::command::CommandRequest,
    ) -> anyhow::Result<()> {
        match request.data {
            CommandRequestData::SaveState { path } => {
                self.save_state(client_id, &request.id, &path).await
            }
            CommandRequestData::DumpState => self.dump_state(client_id, &request.id).await,
            CommandRequestData::ListWorkers => self.list_workers(client_id, request.id).await,
            CommandRequestData::LoadState { path } => {
                self.load_state(Some(client_id), request.id, &path).await
            }
            CommandRequestData::LaunchWorker(tag) => {
                self.launch_worker(client_id, request.id, &tag).await
            }
            CommandRequestData::UpgradeMain => self.upgrade_main(client_id, request.id).await,
            CommandRequestData::UpgradeWorker(worker_id) => {
                self.upgrade_worker(client_id, request.id, worker_id).await
            }

            CommandRequestData::Proxy(proxy_request) => match proxy_request {
                ProxyRequestData::Metrics(config) => {
                    self.metrics(client_id, request.id, config).await
                }
                ProxyRequestData::Query(query) => self.query(client_id, request.id, query).await,
                order => {
                    self.worker_order(client_id, request.id, order, request.worker_id)
                        .await
                }
            },
            CommandRequestData::SubscribeEvents => {
                self.event_subscribers.insert(client_id);
                Ok(())
            }
            CommandRequestData::ReloadConfiguration { path } => {
                self.reload_configuration(Some(client_id), request.id, path)
                    .await
            }
        }
    }

    pub async fn save_state(
        &mut self,
        client_id: String,
        message_id: &str,
        path: &str,
    ) -> anyhow::Result<()> {
        if let Ok(mut f) = fs::File::create(&path) {
            match self.save_state_to_file(&mut f) {
                Ok(counter) => {
                    info!("wrote {} commands to {}", counter, path);
                    self.answer_success(
                        client_id,
                        message_id,
                        format!("saved {} config messages to {}", counter, path),
                        None,
                    )
                    .await;
                    Ok(())
                }
                Err(e) => {
                    error!("failed writing state to file: {:?}", e);
                    self.answer_error(client_id, message_id, "could not save state to file", None)
                        .await;
                    Err(anyhow::Error::from(e))
                }
            }
        } else {
            error!("could not open file: {}", &path);
            self.answer_error(client_id, message_id, "could not open file", None)
                .await;
            Err(anyhow::Error::msg(format!(
                "could not open file: {}",
                &path
            )))
        }
    }

    pub fn save_state_to_file(&mut self, f: &mut fs::File) -> anyhow::Result<usize> {
        let mut counter = 0usize;
        let orders = self.state.generate_orders();

        let res: io::Result<usize> = (move || {
            for command in orders {
                let message = CommandRequest::new(
                    format!("SAVE-{}", counter),
                    CommandRequestData::Proxy(command),
                    None,
                );

                f.write_all(
                    &serde_json::to_string(&message)
                        .map(|s| s.into_bytes())
                        .unwrap_or(vec![]),
                )?;
                f.write_all(&b"\n\0"[..])?;

                if counter % 1000 == 0 {
                    info!("writing command {}", counter);
                    f.sync_all()?;
                }
                counter += 1;
            }
            f.sync_all()?;

            Ok(counter)
        })();

        Ok(res?)
    }

    pub async fn dump_state(&mut self, client_id: String, message_id: &str) -> anyhow::Result<()> {
        let state = self.state.clone();
        self.answer_success(
            client_id,
            message_id,
            String::new(),
            Some(CommandResponseData::State(state)),
        )
        .await;
        Ok(())
    }

    pub async fn load_state(
        &mut self,
        client_id: Option<String>,
        message_id: String,
        path: &str,
    ) -> anyhow::Result<()> {
        let mut file = match fs::File::open(&path) {
            Err(e) => {
                error!("cannot open file at path '{}': {:?}", path, e);
                if let Some(id) = client_id {
                    self.answer_error(
                        id,
                        message_id,
                        format!("cannot open file at path '{}': {:?}", path, e),
                        None,
                    )
                    .await;
                }
                return Err(anyhow::Error::from(e));
            }
            Ok(file) => file,
        };

        let mut buffer = Buffer::with_capacity(200000);

        info!("starting to load state from {}", path);

        let mut message_counter = 0usize;
        let mut diff_counter = 0usize;

        let (load_state_tx, mut load_state_rx) = futures::channel::mpsc::channel(10000);
        loop {
            let previous = buffer.available_data();
            //FIXME: we should read in streaming here
            let sz = file
                .read(buffer.space())
                .with_context(|| "error reading state file")?;
            buffer.fill(sz);

            if buffer.available_data() == 0 {
                break;
            }

            let mut offset = 0usize;
            match parse(buffer.data()) {
                Ok((i, orders)) => {
                    if i.len() > 0 {
                        //info!("could not parse {} bytes", i.len());
                        if previous == buffer.available_data() {
                            error!("error consuming load state message");
                            break;
                        }
                    }
                    offset = buffer.data().offset(i);

                    if orders.iter().find(|o| {
                        if o.version > sozu_command::command::PROTOCOL_VERSION {
                            error!("configuration protocol version mismatch: Sōzu handles up to version {}, the message uses version {}", sozu_command::command::PROTOCOL_VERSION, o.version);
                            true
                        } else {
                            false
                        }
                    }).is_some() {
                        break;
                    }

                    for message in orders {
                        if let CommandRequestData::Proxy(order) = message.data {
                            message_counter += 1;

                            if self.state.handle_order(&order) {
                                diff_counter += 1;

                                let mut found = false;
                                let id = format!("LOAD-STATE-{}-{}", message_id, diff_counter);

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
                Err(e) => {
                    error!("saved state parse error: {:?}", e);
                    break;
                }
            }
            buffer.consume(offset);
        }

        let client_tx = if let Some(id) = client_id.as_ref() {
            self.clients.get(id).cloned()
        } else {
            None
        };

        error!("stopped loading data from file, remaining: {} bytes, saw {} messages, generated {} diff messages",
        buffer.available_data(), message_counter, diff_counter);
        if diff_counter > 0 {
            info!(
                "state loaded from {}, will start sending {} messages to workers",
                path, diff_counter
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

                if let Some(mut sender) = client_tx {
                    if error == 0 {
                        if let Err(e) = sender
                            .send(CommandResponse::new(
                                message_id.to_string(),
                                CommandStatus::Ok,
                                format!("ok: {} messages, error: 0", ok),
                                None,
                            ))
                            .await
                        {
                            error!("could not send message to client {:?}: {:?}", client_id, e);
                        }
                    } else {
                        if let Err(e) = sender
                            .send(CommandResponse::new(
                                message_id.to_string(),
                                CommandStatus::Error,
                                format!("ok: {} messages, error: {}", ok, error),
                                None,
                            ))
                            .await
                        {
                            error!("could not send message to client {:?}: {:?}", client_id, e);
                        }
                    }
                } else {
                    if error == 0 {
                        info!("loading state: {} ok messages, 0 errors", ok);
                    } else {
                        error!("loading state: {} ok messages, {} errors", ok, error);
                    }
                }
            })
            .detach();
        } else {
            info!("no messages sent to workers: local state already had those messages");
            if let Some(id) = client_id {
                self.answer_success(id, message_id, format!("ok: 0 messages, error: 0"), None)
                    .await;
            }
        }

        self.backends_count = self.state.count_backends();
        self.frontends_count = self.state.count_frontends();
        gauge!("configuration.clusters", self.state.clusters.len());
        gauge!("configuration.backends", self.backends_count);
        gauge!("configuration.frontends", self.frontends_count);
        Ok(())
    }

    pub async fn list_workers(
        &mut self,
        client_id: String,
        request_id: String,
    ) -> anyhow::Result<()> {
        let workers: Vec<WorkerInfo> = self
            .workers
            .iter()
            .map(|ref worker| WorkerInfo {
                id: worker.id,
                pid: worker.pid,
                run_state: worker.run_state.clone(),
            })
            .collect();
        self.answer_success(
            client_id,
            request_id,
            "",
            Some(CommandResponseData::Workers(workers)),
        )
        .await;
        Ok(())
    }

    pub async fn launch_worker(
        &mut self,
        client_id: String,
        request_id: String,
        _tag: &str,
    ) -> anyhow::Result<()> {
        let mut worker = match start_worker(
            self.next_id,
            &self.config,
            self.executable_path.clone(),
            &self.state,
            None,
        ) {
            Ok(worker) => worker,
            Err(e) => {
                error!("Failed at creating worker");
                self.answer_error(client_id, request_id, "failed creating worker", None)
                    .await;
                return Err(e);
            }
        };

        if let Some(sender) = self.clients.get_mut(&client_id) {
            if let Err(e) = sender
                .send(CommandResponse::new(
                    request_id.clone(),
                    CommandStatus::Processing,
                    "sending configuration orders".to_string(),
                    None,
                ))
                .await
            {
                error!("could not send message to client {:?}: {:?}", client_id, e);
            }
        }

        info!("created new worker: {}", worker.id);

        self.next_id += 1;
        /*
        let worker_token = self.token_count + 1;
        self.token_count = worker_token;
        worker.token     = Some(Token(worker_token));*/

        /*debug!("registering new sock {:?} at token {:?} for tag {} and id {} (sock error: {:?})", worker.channel.sock,
        worker_token, tag, worker.id, worker.channel.sock.take_error());
        self.poll.registry().register(&mut worker.channel.sock, Token(worker_token),
          Interest::READABLE | Interest::WRITABLE).unwrap();
        worker.token = Some(Token(worker_token));
        */
        let sock = worker.channel.take().unwrap().sock;
        let (worker_tx, worker_rx) = channel(10000);
        worker.sender = Some(worker_tx);

        let stream = Async::new(unsafe {
            let fd = sock.into_raw_fd();
            UnixStream::from_raw_fd(fd)
        })?;

        let id = worker.id;
        let command_tx = self.command_tx.clone();
        //async fn worker(id: u32, sock: Async<UnixStream>, tx: Sender<CommandMessage>, rx: Receiver<()>) -> std::io::Result<()> {
        smol::spawn(async move {
            super::worker_loop(id, stream, command_tx, worker_rx)
                .await
                .unwrap();
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

        self.answer_success(client_id, request_id, "", None).await;

        Ok(())
    }

    pub async fn upgrade_main(
        &mut self,
        client_id: String,
        request_id: String,
    ) -> anyhow::Result<()> {
        self.disable_cloexec_before_upgrade()?;

        if let Some(sender) = self.clients.get_mut(&client_id) {
            if let Err(e) = sender
                .send(CommandResponse::new(
                    request_id.clone(),
                    CommandStatus::Processing,
                    "".to_string(),
                    None,
                ))
                .await
            {
                error!("could not send message to client {:?}: {:?}", client_id, e);
            }
        }

        let (pid, mut channel) =
            start_new_main_process(self.executable_path.clone(), self.generate_upgrade_data())?;
        channel.set_blocking(true);
        let res = channel.read_message();
        debug!("upgrade channel sent: {:?}", res);

        // signaling the accept loop that it should stop
        if let Err(e) = self.accept_cancel.take().unwrap().send(()) {
            error!("could not close the accept loop: {:?}", e);
        }

        if let Some(true) = res {
            if let Some(sender) = self.clients.get_mut(&client_id) {
                if let Err(e) = sender
                    .send(CommandResponse::new(
                        request_id.clone(),
                        CommandStatus::Ok,
                        format!(
                            "new main process launched with pid {}, closing the old one",
                            pid
                        ),
                        None,
                    ))
                    .await
                {
                    error!("could not send message to client {:?}: {:?}", client_id, e);
                }
            }

            info!("wrote final message, closing");

            //FIXME: should do some cleanup before exiting
            std::thread::sleep(Duration::from_secs(2));
            std::process::exit(0);
        } else {
            self.answer_error(
                client_id,
                request_id,
                "could not upgrade main process",
                None,
            )
            .await;
            Ok(())
        }
    }

    pub async fn upgrade_worker(
        &mut self,
        client_id: String,
        request_id: String,
        id: u32,
    ) -> anyhow::Result<()> {
        info!(
            "client[{}] msg {} wants to upgrade worker {}",
            client_id, request_id, id
        );

        // same as launch_worker
        let next_id = self.next_id;
        let mut worker = if let Ok(worker) = start_worker(
            next_id,
            &self.config,
            self.executable_path.clone(),
            &self.state,
            None,
        ) {
            if let Some(sender) = self.clients.get_mut(&client_id) {
                if let Err(e) = sender
                    .send(CommandResponse::new(
                        request_id.clone(),
                        CommandStatus::Processing,
                        "sending configuration orders".to_string(),
                        None,
                    ))
                    .await
                {
                    error!("could not send message to client {:?}: {:?}", client_id, e);
                }
            }
            info!("created new worker: {}", next_id);

            self.next_id += 1;

            worker
        } else {
            return Ok(self
                .answer_error(client_id, &request_id, "failed creating worker", None)
                .await);
        };

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
            self.answer_error(client_id, &request_id, "worker not found", None)
                .await;
            return Ok(());
        }

        let sock = worker.channel.take().unwrap().sock;
        let (worker_tx, worker_rx) = channel(10000);
        worker.sender = Some(worker_tx);

        if let Err(e) = worker
            .sender
            .as_mut()
            .unwrap()
            .send(ProxyRequest {
                id: format!("UPGRADE-{}-STATUS", id),
                order: ProxyRequestData::Status,
            })
            .await
        {
            error!(
                "could not send status message to worker {:?}: {:?}",
                worker.id, e
            );
        }

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
            let (tx, mut rx) = futures::channel::mpsc::channel(3);
            let id = format!("{}-return-sockets", request_id);
            self.in_flight.insert(id.clone(), (tx, 1));
            old_worker
                .send(id.clone(), ProxyRequestData::ReturnListenSockets)
                .await;

            info!("sent ReturnListenSockets to old worker");
            smol::spawn(async move {
                while let Some(proxy_response) = rx.next().await {
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

            let (tx, mut rx) = futures::channel::mpsc::channel(10);
            let id = format!("{}-softstop", request_id);
            self.in_flight.insert(id.clone(), (tx, 1));
            old_worker
                .send(id.clone(), ProxyRequestData::SoftStop)
                .await;

            let mut command_tx = self.command_tx.clone();
            let worker_id = old_worker.id;
            smol::spawn(async move {
                while let Some(proxy_response) = rx.next().await {
                    match proxy_response.status {
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
        //async fn worker(id: u32, sock: Async<UnixStream>, tx: Sender<CommandMessage>, rx: Receiver<()>) -> std::io::Result<()> {
        smol::spawn(async move {
            super::worker_loop(id, stream, command_tx, worker_rx)
                .await
                .unwrap();
        })
        .detach();

        let activate_orders = self.state.generate_activate_orders();
        let mut count = 0usize;
        for order in activate_orders.into_iter() {
            worker
                .send(format!("{}-ACTIVATE-{}", request_id, count), order)
                .await;
            count += 1;
        }
        info!("sent config messages to the new worker");
        self.workers.push(worker);

        self.answer_success(client_id, request_id, "", None).await;
        info!("finished upgrade");
        Ok(())
    }

    pub async fn reload_configuration(
        &mut self,
        client_id: Option<String>,
        message_id: String,
        config_path: Option<String>,
    ) -> anyhow::Result<()> {
        let new_config = match Config::load_from_path(
            config_path.as_deref().unwrap_or(&self.config.config_path),
        ) {
            Err(e) => {
                if let Some(id) = client_id {
                    self.answer_error(
                        id,
                        message_id,
                        format!(
                            "cannot load configuration from '{}': {:?}",
                            self.config.config_path, e
                        ),
                        None,
                    )
                    .await;
                }
                return Err(anyhow::Error::from(e));
            }
            Ok(c) => c,
        };

        let mut diff_counter = 0usize;

        let (load_state_tx, mut load_state_rx) = futures::channel::mpsc::channel(10000);

        for message in new_config.generate_config_messages() {
            if let CommandRequestData::Proxy(order) = message.data {
                if self.state.handle_order(&order) {
                    diff_counter += 1;

                    let mut found = false;
                    let id = format!("LOAD-STATE-{}-{}", message_id, diff_counter);

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

        let client_tx = if let Some(id) = client_id.as_ref() {
            self.clients.get(id).cloned()
        } else {
            None
        };

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

                if let Some(mut sender) = client_tx {
                    if error == 0 {
                        if let Err(e) = sender
                            .send(CommandResponse::new(
                                message_id.to_string(),
                                CommandStatus::Ok,
                                format!("ok: {} messages, error: 0", ok),
                                None,
                            ))
                            .await
                        {
                            error!("could not send message to client {:?}: {:?}", client_id, e);
                        }
                    } else {
                        if let Err(e) = sender
                            .send(CommandResponse::new(
                                message_id.to_string(),
                                CommandStatus::Error,
                                format!("ok: {} messages, error: {}", ok, error),
                                None,
                            ))
                            .await
                        {
                            error!("could not send message to client {:?}: {:?}", client_id, e);
                        }
                    }
                } else {
                    if error == 0 {
                        info!("loading state: {} ok messages, 0 errors", ok);
                    } else {
                        error!("loading state: {} ok messages, {} errors", ok, error);
                    }
                }
            })
            .detach();
        } else {
            info!("no messages sent to workers: local state already had those messages");
            if let Some(id) = client_id {
                self.answer_success(id, message_id, format!("ok: 0 messages, error: 0"), None)
                    .await;
            }
        }

        self.backends_count = self.state.count_backends();
        self.frontends_count = self.state.count_frontends();
        gauge!("configuration.clusters", self.state.clusters.len());
        gauge!("configuration.backends", self.backends_count);
        gauge!("configuration.frontends", self.frontends_count);

        self.config = new_config;
        Ok(())
    }

    pub async fn metrics(
        &mut self,
        client_id: String,
        request_id: String,
        config: MetricsConfiguration,
    ) -> anyhow::Result<()> {
        let (tx, mut rx) = futures::channel::mpsc::channel(self.workers.len() * 2);
        let mut count = 0usize;
        for ref mut worker in self
            .workers
            .iter_mut()
            .filter(|worker| worker.run_state != RunState::Stopped)
        {
            let req_id = format!("{}-metrics-{}", request_id, worker.id);
            worker
                .send(req_id.clone(), ProxyRequestData::Metrics(config.clone()))
                .await;
            count += 1;
            self.in_flight.insert(req_id, (tx.clone(), 1));
        }

        let mut client_tx = self.clients.get_mut(&client_id).unwrap().clone();
        let prefix = format!("{}-metrics-", request_id);
        smol::spawn(async move {
            let mut v = Vec::new();
            let mut i = 0;
            while let Some(proxy_response) = rx.next().await {
                match proxy_response.status {
                    ProxyResponseStatus::Ok => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        v.push((tag, proxy_response));
                    }
                    ProxyResponseStatus::Processing => {
                        //info!("metrics processing");
                        continue;
                    }
                    ProxyResponseStatus::Error(_) => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        v.push((tag, proxy_response));
                    }
                };

                i += 1;
                if i == count {
                    break;
                }
            }

            if let Err(e) = client_tx
                .send(CommandResponse::new(
                    request_id.clone(),
                    CommandStatus::Ok,
                    "".to_string(),
                    None,
                ))
                .await
            {
                error!("could not send back metrics to client: {:?}", e);
            }
        })
        .detach();
        Ok(())
    }

    pub async fn query(
        &mut self,
        client_id: String,
        request_id: String,
        query: Query,
    ) -> anyhow::Result<()> {
        let (tx, mut rx) = futures::channel::mpsc::channel(self.workers.len() * 2);
        let mut count = 0usize;
        for ref mut worker in self
            .workers
            .iter_mut()
            .filter(|worker| worker.run_state != RunState::Stopped)
        {
            let req_id = format!("{}-query-{}", request_id, worker.id);
            worker
                .send(req_id.clone(), ProxyRequestData::Query(query.clone()))
                .await;
            count += 1;
            self.in_flight.insert(req_id, (tx.clone(), 1));
        }

        let mut main_query_answer = None;
        match &query {
            &Query::ApplicationsHashes => {
                main_query_answer = Some(QueryAnswer::ApplicationsHashes(self.state.hash_state()));
            }
            &Query::Applications(ref query_type) => {
                main_query_answer = Some(QueryAnswer::Applications(match query_type {
                    QueryApplicationType::ClusterId(ref cluster_id) => {
                        vec![self.state.application_state(cluster_id)]
                    }
                    QueryApplicationType::Domain(ref domain) => {
                        let cluster_ids = get_application_ids_by_domain(
                            &self.state,
                            domain.hostname.clone(),
                            domain.path.clone(),
                        );
                        cluster_ids
                            .iter()
                            .map(|ref cluster_id| self.state.application_state(cluster_id))
                            .collect()
                    }
                }));
            }
            &Query::Certificates(_) => {}
            &Query::Metrics(_) => {}
        };

        let mut client_tx = self.clients.get_mut(&client_id).unwrap().clone();
        let prefix = format!("{}-query-", request_id);
        smol::spawn(async move {
            let mut v = Vec::new();
            let mut i = 0;
            while let Some(proxy_response) = rx.next().await {
                match proxy_response.status {
                    ProxyResponseStatus::Ok => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        v.push((tag, proxy_response));
                    }
                    ProxyResponseStatus::Processing => {
                        info!("metrics processing");
                        continue;
                    }
                    ProxyResponseStatus::Error(_) => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        v.push((tag, proxy_response));
                    }
                };

                i += 1;
                if i == count {
                    break;
                }
            }

            let mut data: BTreeMap<String, QueryAnswer> = v
                .into_iter()
                .filter_map(|(tag, query)| {
                    if let Some(ProxyResponseData::Query(d)) = query.data {
                        Some((tag, d))
                    } else {
                        None
                    }
                })
                .collect();

            match &query {
                &Query::ApplicationsHashes => {
                    let main = main_query_answer.unwrap();
                    data.insert(String::from("main"), main);

                    if let Err(e) = client_tx
                        .send(CommandResponse::new(
                            request_id.clone(),
                            CommandStatus::Ok,
                            "".to_string(),
                            Some(CommandResponseData::Query(data)),
                        ))
                        .await
                    {
                        error!("could not send message to client {:?}: {:?}", client_id, e);
                    }
                }
                &Query::Applications(_) => {
                    let main = main_query_answer.unwrap();
                    data.insert(String::from("main"), main);

                    if let Err(e) = client_tx
                        .send(CommandResponse::new(
                            request_id.clone(),
                            CommandStatus::Ok,
                            "".to_string(),
                            Some(CommandResponseData::Query(data)),
                        ))
                        .await
                    {
                        error!("could not send message to client {:?}: {:?}", client_id, e);
                    }
                }
                &Query::Certificates(_) => {
                    info!("certificates query received: {:?}", data);
                    if let Err(e) = client_tx
                        .send(CommandResponse::new(
                            request_id.clone(),
                            CommandStatus::Ok,
                            "".to_string(),
                            Some(CommandResponseData::Query(data)),
                        ))
                        .await
                    {
                        error!("could not send message to client {:?}: {:?}", client_id, e);
                    }
                }
                &Query::Metrics(_) => {
                    debug!("metrics query received: {:?}", data);
                    let _res = client_tx
                        .send(CommandResponse::new(
                            request_id.clone(),
                            CommandStatus::Ok,
                            "".to_string(),
                            Some(CommandResponseData::Query(data)),
                        ))
                        .await;
                }
            };
        })
        .detach();
        Ok(())
    }

    pub async fn worker_order(
        &mut self,
        client_id: String,
        request_id: String,
        order: ProxyRequestData,
        worker_id: Option<u32>,
    ) -> anyhow::Result<()> {
        if let &ProxyRequestData::AddCertificate(_) = &order {
            debug!("workerconfig client order AddCertificate()");
        } else {
            debug!("workerconfig client order {:?}", order);
        }

        if let &ProxyRequestData::Logging(ref logging_filter) = &order {
            debug!("Changing main process log level to {}", logging_filter);
            logging::LOGGER.with(|l| {
                let directives = logging::parse_logging_spec(&logging_filter);
                l.borrow_mut().set_directives(directives);
            });
            // also change / set the content of RUST_LOG so future workers / main thread
            // will have the new logging filter value
            ::std::env::set_var("RUST_LOG", logging_filter);
        }

        if !self.state.handle_order(&order) {
            // Check if the backend or frontend exist before deleting it
            if worker_id.is_none() {
                match order {
                    ProxyRequestData::RemoveBackend(ref backend) => {
                        let msg = format!(
                            "cannot remove backend: cluster {} has no backends {} at {}",
                            backend.cluster_id, backend.backend_id, backend.address,
                        );
                        error!("{}", msg);
                        self.answer_error(client_id, request_id, msg, None).await;
                        return Ok(());
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
                        error!("{}", msg);
                        self.answer_error(client_id, request_id, msg, None).await;
                        return Ok(());
                    }
                    ProxyRequestData::RemoveTcpFrontend(TcpFrontend {
                        ref cluster_id,
                        ref address,
                    }) => {
                        let msg = format!(
                            "cannot remove TCP frontend: cluster {} has no frontends at {}",
                            cluster_id, address,
                        );
                        error!("{}", msg);
                        self.answer_error(client_id, request_id, msg, None).await;
                        return Ok(());
                    }
                    _ => {}
                };
            }
        }

        if self.config.automatic_state_save {
            if order != ProxyRequestData::SoftStop || order != ProxyRequestData::HardStop {
                if let Some(path) = self.config.saved_state.clone() {
                    if let Ok(mut f) = fs::File::create(&path) {
                        let _ = self.save_state_to_file(&mut f).map_err(|e| {
                            error!("could not save state automatically to {}: {:?}", path, e);
                        });
                    }
                }
            }
        }

        let (tx, mut rx) = futures::channel::mpsc::channel(self.workers.len() * 2);
        let mut found = false;
        let mut stopping_workers = HashSet::new();

        let mut count = 0usize;
        for ref mut worker in self.workers.iter_mut().filter(|worker| {
            worker.run_state != RunState::Stopping && worker.run_state != RunState::Stopped
        }) {
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

            let req_id = format!("{}-worker-{}", request_id, worker.id);
            worker.send(req_id.clone(), order.clone()).await;
            self.in_flight.insert(req_id, (tx.clone(), 1));

            found = true;
            count += 1;
        }

        let should_stop_main = (order == ProxyRequestData::SoftStop
            || order == ProxyRequestData::HardStop)
            && worker_id.is_none();

        let mut client_tx = self.clients.get_mut(&client_id).unwrap().clone();
        let mut command_tx = self.command_tx.clone();
        let prefix = format!("{}-worker-", request_id);
        smol::spawn(async move {
            let mut v = Vec::new();
            let mut i = 0usize;
            while let Some(proxy_response) = rx.next().await {
                match proxy_response.status {
                    ProxyResponseStatus::Ok => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        v.push((tag.clone(), proxy_response));

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
                        info!("metrics processing");
                        continue;
                    }
                    ProxyResponseStatus::Error(_) => {
                        let tag = proxy_response.id.trim_start_matches(&prefix).to_string();
                        v.push((tag, proxy_response));
                    }
                };

                i += 1;
                if i == count {
                    break;
                }
            }

            if should_stop_main {
                if let Err(e) = command_tx.send(CommandMessage::MasterStop).await {
                    error!("could not send main stop message: {:?}", e);
                }
            }

            let mut messages = vec![];
            let mut has_error = false;
            for response in v.iter() {
                if let ProxyResponseStatus::Error(ref e) = response.1.status {
                    messages.push(format!("{}: {}", response.0, e));
                    has_error = true;
                } else {
                    messages.push(format!("{}: OK", response.0));
                }
            }

            if has_error {
                if let Err(e) = client_tx
                    .send(CommandResponse::new(
                        request_id,
                        CommandStatus::Error,
                        messages.join(", "),
                        None,
                    ))
                    .await
                {
                    error!("could not send message to client {:?}: {:?}", client_id, e);
                }
            } else {
                if let Err(e) = client_tx
                    .send(CommandResponse::new(
                        request_id,
                        CommandStatus::Ok,
                        "".to_string(),
                        None,
                    ))
                    .await
                {
                    error!("could not send message to client {:?}: {:?}", client_id, e);
                }
            }
        })
        .detach();

        if !found {
            // FIXME: should send back error here
            error!("no worker found");
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
        Ok(())
    }
}

use nom::{
    bytes::streaming::is_not,
    character::streaming::char,
    combinator::{complete, map_res},
    multi::many0,
    sequence::terminated,
};
pub fn parse(input: &[u8]) -> IResult<&[u8], Vec<CommandRequest>> {
    use serde_json::from_slice;
    many0(complete(terminated(
        map_res(is_not("\0"), from_slice),
        char('\0'),
    )))(input)
}
