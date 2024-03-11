use std::{
    collections::{BTreeMap, HashMap},
    env,
    fs::File,
    io::{ErrorKind, Read},
};

use mio::Token;
use nom::{HexDisplay, Offset};

use sozu_command_lib::{
    buffer::fixed::Buffer,
    config::Config,
    logging,
    parser::parse_several_requests,
    proto::command::{
        request::RequestType, response_content::ContentType, AggregatedMetrics, AvailableMetrics,
        CertificatesWithFingerprints, ClusterHashes, ClusterInformations, FrontendFilters,
        HardStop, QueryCertificatesFilters, QueryMetricsOptions, Request, ResponseContent,
        ResponseStatus, RunState, SoftStop, Status, WorkerInfo, WorkerInfos, WorkerRequest,
        WorkerResponses,
    },
};
use sozu_lib::metrics::METRICS;

use crate::command::{
    server::{
        DefaultGatherer, Gatherer, GatheringTask, MessageClient, Server, ServerState, Timeout,
        WorkerId,
    },
    sessions::{ClientSession, OptionalClient},
    upgrade::{upgrade_main, upgrade_worker},
};

impl Server {
    pub fn handle_client_request(&mut self, client: &mut ClientSession, request: Request) {
        let request_type = match request.request_type {
            Some(req) => req,
            None => {
                error!("empty request sent by client {:?}", client);
                return;
            }
        };
        match request_type {
            RequestType::SaveState(path) => save_state(self, client, &path),
            RequestType::LoadState(path) => load_state(self, Some(client), &path),
            RequestType::ListWorkers(_) => list_workers(self, client),
            RequestType::ListFrontends(inner) => list_frontend_command(self, client, inner),
            RequestType::ListListeners(_) => list_listeners(self, client),
            RequestType::UpgradeMain(_) => upgrade_main(self, client),
            RequestType::UpgradeWorker(worker_id) => upgrade_worker(self, client, worker_id),
            RequestType::SubscribeEvents(_) => subscribe_client_to_events(self, client),
            RequestType::ReloadConfiguration(path) => {
                load_static_config(self, Some(client), Some(&path))
            }
            RequestType::Status(_) => status(self, client),
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
            | RequestType::ConfigureMetrics(_)
            | RequestType::DeactivateListener(_)
            | RequestType::RemoveBackend(_)
            | RequestType::RemoveCertificate(_)
            | RequestType::RemoveCluster(_)
            | RequestType::RemoveHttpFrontend(_)
            | RequestType::RemoveHttpsFrontend(_)
            | RequestType::RemoveListener(_)
            | RequestType::RemoveTcpFrontend(_)
            | RequestType::ReplaceCertificate(_) => {
                worker_request(self, client, request_type);
            }
            RequestType::QueryClustersHashes(_)
            | RequestType::QueryClustersByDomain(_)
            | RequestType::QueryCertificatesFromWorkers(_)
            | RequestType::QueryClusterById(_) => {
                query_clusters(self, client, request_type);
            }
            RequestType::QueryMetrics(inner) => query_metrics(self, client, inner),
            RequestType::SoftStop(_) => stop(self, client, false),
            RequestType::HardStop(_) => stop(self, client, true),
            RequestType::Logging(logging_filter) => set_logging_level(self, client, logging_filter),
            RequestType::QueryCertificatesFromTheState(filters) => {
                query_certificates_from_main(self, client, filters)
            }
            RequestType::CountRequests(_) => count_requests(self, client),

            RequestType::LaunchWorker(_) => {} // not yet implemented, nor used, anywhere
            RequestType::ReturnListenSockets(_) => {} // This is only implemented by workers,
        }
    }

    /// get infos from the state of the main process
    fn query_main(&self, request: RequestType) -> Option<ResponseContent> {
        match request {
            RequestType::QueryClusterById(cluster_id) => Some(
                ContentType::Clusters(ClusterInformations {
                    vec: self.state.cluster_state(&cluster_id).into_iter().collect(),
                })
                .into(),
            ),
            RequestType::QueryClustersByDomain(domain) => {
                let cluster_ids = self
                    .state
                    .get_cluster_ids_by_domain(domain.hostname, domain.path);
                let vec = cluster_ids
                    .iter()
                    .filter_map(|cluster_id| self.state.cluster_state(cluster_id))
                    .collect();
                Some(ContentType::Clusters(ClusterInformations { vec }).into())
            }
            RequestType::QueryClustersHashes(_) => Some(
                ContentType::ClusterHashes(ClusterHashes {
                    map: self.state.hash_state(),
                })
                .into(),
            ),
            _ => None,
        }
    }
}

//===============================================
// non-scattered commands

pub fn query_certificates_from_main(
    server: &mut Server,
    client: &mut ClientSession,
    filters: QueryCertificatesFilters,
) {
    debug!(
        "querying certificates in the state with filters {}",
        filters
    );

    let certs = server.state.get_certificates(filters);

    client.finish_ok_with_content(
        ContentType::CertificatesWithFingerprints(CertificatesWithFingerprints { certs }).into(),
        "Successfully queried certificates from the state of main process",
    );
}

/// return how many requests were received by Sōzu since startup
fn count_requests(server: &mut Server, client: &mut ClientSession) {
    let request_counts = server.state.get_request_counts();

    client.finish_ok_with_content(
        ContentType::RequestCounts(request_counts).into(),
        "Successfully counted requests received by the state",
    );
}

pub fn list_frontend_command(
    server: &mut Server,
    client: &mut ClientSession,
    filters: FrontendFilters,
) {
    match server.query_main(RequestType::ListFrontends(filters)) {
        Some(response) => client.finish_ok_with_content(response, "Successfully listed frontends"),
        None => client.finish_failure("main process could not list frontends"),
    }
}

fn list_workers(server: &mut Server, client: &mut ClientSession) {
    let vec = server
        .workers
        .values()
        .map(|worker| WorkerInfo {
            id: worker.id,
            pid: worker.pid,
            run_state: worker.run_state as i32,
        })
        .collect();

    debug!("workers: {:?}", vec);
    client.finish_ok_with_content(
        ContentType::Workers(WorkerInfos { vec }).into(),
        "Successfully listed workers",
    );
}

fn list_listeners(server: &mut Server, client: &mut ClientSession) {
    let vec = server.state.list_listeners();
    client.finish_ok_with_content(
        ContentType::ListenersList(vec).into(),
        "Successfully listed listeners",
    );
}

fn save_state(server: &mut Server, client: &mut ClientSession, path: &str) {
    debug!("saving state to file {}", path);
    let mut file = match File::create(path) {
        Ok(file) => file,
        Err(error) => {
            client.finish_failure(format!("Cannot create file at path {path}: {error}"));
            return;
        }
    };

    match server.state.write_requests_to_file(&mut file) {
        Ok(counter) => {
            client.finish_ok(format!("Saved {counter} config messages to {path}"));
        }
        Err(error) => {
            client.finish_failure(format!("Failed writing state to file: {error}"));
        }
    }
}

/// change logging level on the main process, and on all workers
fn set_logging_level(server: &mut Server, client: &mut ClientSession, logging_filter: String) {
    debug!("Changing main process log level to {}", logging_filter);
    let (directives, errors) = logging::parse_logging_spec(&logging_filter);
    if !errors.is_empty() {
        client.finish_failure(format!(
            "Error parsing logging filter:\n- {}",
            errors
                .iter()
                .map(logging::LogSpecParseError::to_string)
                .collect::<Vec<String>>()
                .join("\n- ")
        ));
        return;
    }
    logging::LOGGER.with(|logger| {
        logger.borrow_mut().set_directives(directives);
    });

    // also change / set the content of RUST_LOG so future workers / main thread
    // will have the new logging filter value
    env::set_var("RUST_LOG", &logging_filter);
    debug!(
        "Logging level now: {}",
        env::var("RUST_LOG").unwrap_or("could get RUST_LOG from env".to_string())
    );

    worker_request(server, client, RequestType::Logging(logging_filter));
}

fn subscribe_client_to_events(server: &mut Server, client: &mut ClientSession) {
    info!("Subscribing client {:?} to listen to events", client.token);
    server.event_subscribers.insert(client.token);
}

//===============================================
// Query clusters

#[derive(Debug)]
pub struct QueryClustersTask {
    pub client_token: Token,
    pub request_type: RequestType,
    pub gatherer: DefaultGatherer,
    main_process_response: Option<ResponseContent>,
}

pub fn query_clusters(
    server: &mut Server,
    client: &mut ClientSession,
    request_content: RequestType,
) {
    client.return_processing("Querying cluster...");

    server.scatter(
        request_content.clone().into(),
        Box::new(QueryClustersTask {
            client_token: client.token,
            gatherer: DefaultGatherer::default(),
            main_process_response: server.query_main(request_content.clone()),
            request_type: request_content,
        }),
        Timeout::Default,
        None,
    )
}

impl GatheringTask for QueryClustersTask {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(
        self: Box<Self>,
        _server: &mut Server,
        client: &mut OptionalClient,
        _timed_out: bool,
    ) {
        let mut worker_responses: BTreeMap<String, ResponseContent> = self
            .gatherer
            .responses
            .into_iter()
            .filter_map(|(worker_id, proxy_response)| {
                proxy_response
                    .content
                    .map(|response_content| (worker_id.to_string(), response_content))
            })
            .collect();

        if let Some(main_response) = self.main_process_response {
            worker_responses.insert(String::from("main"), main_response);
        }

        client.finish_ok_with_content(
            ContentType::WorkerResponses(WorkerResponses {
                map: worker_responses,
            })
            .into(),
            "Successfully queried clusters",
        );
    }
}

//===============================================
// Load static configuration

#[derive(Debug)]
struct LoadStaticConfigTask {
    gatherer: DefaultGatherer,
    client_token: Option<Token>,
}

pub fn load_static_config(server: &mut Server, mut client: OptionalClient, path: Option<&str>) {
    let task_id = server.new_task(
        Box::new(LoadStaticConfigTask {
            gatherer: DefaultGatherer::default(),
            client_token: client.as_ref().map(|c| c.token),
        }),
        Timeout::None,
    );

    let new_config;

    let config = match path {
        Some(path) if !path.is_empty() => {
            info!("loading static configuration at path {}", path);
            new_config = Config::load_from_path(path)
                .unwrap_or_else(|_| panic!("cannot load configuration from '{path}'"));
            &new_config
        }
        _ => {
            info!("reloading static configuration");
            &server.config
        }
    };

    client.return_processing(format!(
        "Reloading static configuration at path {}",
        config.config_path
    ));

    let config_messages = match config.generate_config_messages() {
        Ok(messages) => messages,
        Err(config_err) => {
            client.finish_failure(format!("could not generate new config: {}", config_err));
            return;
        }
    };

    for (request_index, message) in config_messages.into_iter().enumerate() {
        let request = message.content;
        if let Err(error) = server.state.dispatch(&request) {
            client.return_processing(format!("Could not execute request on state: {:#}", error));
            continue;
        }

        if let &Some(RequestType::AddCertificate(_)) = &request.request_type {
            debug!("config generated AddCertificate( ... )");
        } else {
            debug!("config generated {:?}", request);
        }

        server.scatter_on(request, task_id, request_index, None);
    }
}

impl GatheringTask for LoadStaticConfigTask {
    fn client_token(&self) -> Option<Token> {
        self.client_token
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(
        self: Box<Self>,
        server: &mut Server,
        client: &mut OptionalClient,
        _timed_out: bool,
    ) {
        let mut messages = vec![];
        for (worker_id, response) in self.gatherer.responses {
            match ResponseStatus::try_from(response.status) {
                Ok(ResponseStatus::Failure) => {
                    messages.push(format!("worker {worker_id}: {}", response.message))
                }
                Ok(ResponseStatus::Ok) | Ok(ResponseStatus::Processing) => {}
                Err(e) => warn!("error decoding response status: {}", e),
            }
        }

        if self.gatherer.errors > 0 {
            client.finish_failure(format!(
                "\nloading static configuration failed: {} OK, {} errors:\n- {}",
                self.gatherer.ok,
                self.gatherer.errors,
                messages.join("\n- ")
            ));
        } else {
            client.finish_ok(format!(
                "Successfully loaded the config: {} ok, {} errors",
                self.gatherer.ok, self.gatherer.errors,
            ));
        }

        server.update_counts();
    }
}

// =========================================================
// Worker request

#[derive(Debug)]
struct WorkerTask {
    pub client_token: Token,
    pub gatherer: DefaultGatherer,
}

pub fn worker_request(
    server: &mut Server,
    client: &mut ClientSession,
    request_content: RequestType,
) {
    let request = request_content.into();

    if let Err(error) = server.state.dispatch(&request) {
        client.finish_failure(format!(
            "could not dispatch request on the main process state: {error}",
        ));
        return;
    }
    client.return_processing("Processing worker request...");

    server.scatter(
        request,
        Box::new(WorkerTask {
            client_token: client.token,
            gatherer: DefaultGatherer::default(),
        }),
        Timeout::Default,
        None,
    )
}

impl GatheringTask for WorkerTask {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(
        self: Box<Self>,
        _server: &mut Server,
        client: &mut OptionalClient,
        timed_out: bool,
    ) {
        let mut messages = vec![];

        for (worker_id, response) in self.gatherer.responses {
            match ResponseStatus::try_from(response.status) {
                Ok(ResponseStatus::Ok) => messages.push(format!("{worker_id}: OK")),
                Ok(ResponseStatus::Failure) | Ok(ResponseStatus::Processing) | Err(_) => {
                    messages.push(format!("{worker_id}: {}", response.message))
                }
            }
        }

        if self.gatherer.errors > 0 || timed_out {
            client.finish_failure(messages.join(", "));
        } else {
            client.finish_ok("Successfully applied request to all workers");
        }
    }
}

// =========================================================
// Query Metrics

#[derive(Debug)]
struct QueryMetricsTask {
    pub client_token: Token,
    pub gatherer: DefaultGatherer,
    options: QueryMetricsOptions,
}

fn query_metrics(server: &mut Server, client: &mut ClientSession, options: QueryMetricsOptions) {
    client.return_processing("Querrying metrics...");

    server.scatter(
        RequestType::QueryMetrics(options.clone()).into(),
        Box::new(QueryMetricsTask {
            client_token: client.token,
            gatherer: DefaultGatherer::default(),
            options,
        }),
        Timeout::Default,
        None,
    );
}

impl GatheringTask for QueryMetricsTask {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(
        self: Box<Self>,
        _server: &mut Server,
        client: &mut OptionalClient,
        _timed_out: bool,
    ) {
        let main_metrics =
            METRICS.with(|metrics| (*metrics.borrow_mut()).dump_local_proxy_metrics());

        if self.options.list {
            let mut summed_proxy_metrics = Vec::new();
            let mut summed_cluster_metrics = Vec::new();
            for (_, response) in self.gatherer.responses {
                if let Some(ResponseContent {
                    content_type:
                        Some(ContentType::AvailableMetrics(AvailableMetrics {
                            proxy_metrics: listed_proxy_metrics,
                            cluster_metrics: listed_cluster_metrics,
                        })),
                }) = response.content
                {
                    summed_proxy_metrics.append(&mut listed_proxy_metrics.clone());
                    summed_cluster_metrics.append(&mut listed_cluster_metrics.clone());
                }
            }
            return client.finish_ok_with_content(
                ContentType::AvailableMetrics(AvailableMetrics {
                    proxy_metrics: summed_proxy_metrics,
                    cluster_metrics: summed_cluster_metrics,
                })
                .into(),
                "Successfully listed available metrics",
            );
        }

        let workers_metrics = self
            .gatherer
            .responses
            .into_iter()
            .filter_map(
                |(worker_id, worker_response)| match worker_response.content {
                    Some(ResponseContent {
                        content_type: Some(ContentType::WorkerMetrics(worker_metrics)),
                    }) => Some((worker_id.to_string(), worker_metrics)),
                    _ => None,
                },
            )
            .collect();

        client.finish_ok_with_content(
            ContentType::Metrics(AggregatedMetrics {
                main: main_metrics,
                workers: workers_metrics,
            })
            .into(),
            "Successfully aggregated all metrics",
        );
    }
}

// =========================================================
// Load state

#[derive(Debug)]
struct LoadStateTask {
    /// this task may be called by the main process, without a client
    pub client_token: Option<Token>,
    pub gatherer: DefaultGatherer,
    path: String,
}

pub fn load_state(server: &mut Server, mut client: OptionalClient, path: &str) {
    info!("loading state at path {}", path);

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if matches!(err.kind(), ErrorKind::NotFound) => {
            client.finish_failure(format!("Cannot find file at path {path}"));
            return;
        }
        Err(error) => {
            client.finish_failure(format!("Cannot open file at path {path}: {error}"));
            return;
        }
    };

    client.return_processing(format!("Parsing state file from {path}..."));

    let task_id = server.new_task(
        Box::new(LoadStateTask {
            client_token: client.as_ref().map(|c| c.token),
            gatherer: DefaultGatherer::default(),
            path: path.to_owned(),
        }),
        Timeout::None,
    );

    let mut buffer = Buffer::with_capacity(200000);
    let mut scatter_request_counter = 0usize;

    let status = loop {
        let previous = buffer.available_data();

        match file.read(buffer.space()) {
            Ok(bytes_read) => buffer.fill(bytes_read),
            Err(error) => break Err(format!("Error reading the saved state file: {error}")),
        };

        if buffer.available_data() == 0 {
            trace!("load_state: empty buffer");
            break Ok(());
        }

        let mut offset = 0usize;
        match parse_several_requests::<WorkerRequest>(buffer.data()) {
            Ok((i, requests)) => {
                if !i.is_empty() {
                    debug!("load_state: could not parse {} bytes", i.len());
                    if previous == buffer.available_data() {
                        break Err("Error consuming load state message".into());
                    }
                }
                offset = buffer.data().offset(i);

                for request in requests {
                    if server.state.dispatch(&request.content).is_ok() {
                        scatter_request_counter += 1;
                        server.scatter_on(request.content, task_id, scatter_request_counter, None);
                    }
                }
            }
            Err(nom::Err::Incomplete(_)) => {
                if buffer.available_data() == buffer.capacity() {
                    break Err(format!(
                        "message too big, stopping parsing:\n{}",
                        buffer.data().to_hex(16)
                    ));
                }
            }
            Err(parse_error) => {
                break Err(format!("saved state parse error: {:?}", parse_error));
            }
        }
        buffer.consume(offset);
    };

    match status {
        Ok(()) => {
            client.return_processing("Applying state file...");
        }
        Err(message) => {
            client.finish_failure(message);
            server.cancel_task(task_id);
        }
    }
}

impl GatheringTask for LoadStateTask {
    fn client_token(&self) -> Option<Token> {
        self.client_token
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(
        self: Box<Self>,
        _server: &mut Server,
        client: &mut OptionalClient,
        _timed_out: bool,
    ) {
        let DefaultGatherer { ok, errors, .. } = self.gatherer;
        if errors == 0 {
            client.finish_ok(format!(
                "Successfully loaded state from path {}, {} ok messages, {} errors",
                self.path, ok, errors
            ));
            return;
        }
        client.finish_failure(format!("loading state: {ok} ok messages, {errors} errors"));
    }
}

// ==========================================================
// status

#[derive(Debug)]
struct StatusTask {
    pub client_token: Token,
    pub gatherer: DefaultGatherer,
    worker_infos: HashMap<WorkerId, WorkerInfo>,
}

fn status(server: &mut Server, client: &mut ClientSession) {
    client.return_processing("Querying status of workers...");

    let worker_infos = server
        .workers
        .values()
        .map(|worker| (worker.id, worker.querying_info()))
        .collect();

    server.scatter(
        RequestType::Status(Status {}).into(),
        Box::new(StatusTask {
            client_token: client.token,
            gatherer: DefaultGatherer::default(),
            worker_infos,
        }),
        Timeout::Default,
        None,
    );
}

impl GatheringTask for StatusTask {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(
        mut self: Box<Self>,
        _server: &mut Server,
        client: &mut OptionalClient,
        _timed_out: bool,
    ) {
        for (worker_id, response) in self.gatherer.responses {
            let new_run_state = match ResponseStatus::try_from(response.status) {
                Ok(ResponseStatus::Ok) => RunState::Running,
                Ok(ResponseStatus::Processing) => continue,
                Ok(ResponseStatus::Failure) => RunState::NotAnswering,
                Err(e) => {
                    warn!("error decoding response status: {}", e);
                    continue;
                }
            };

            self.worker_infos
                .entry(worker_id)
                .and_modify(|worker_info| worker_info.run_state = new_run_state as i32);
        }

        let worker_info_vec = WorkerInfos {
            vec: self.worker_infos.into_values().collect(),
        };

        client.finish_ok_with_content(
            ContentType::Workers(worker_info_vec).into(),
            "Successfully collected the status of workers",
        );
    }
}

// ==========================================================
// Soft stop and hard stop

#[derive(Debug)]
struct StopTask {
    pub client_token: Token,
    pub gatherer: DefaultGatherer,
    pub hardness: bool,
}

/// stop the main process and workers, true for hard stop
fn stop(server: &mut Server, client: &mut ClientSession, hardness: bool) {
    let task = Box::new(StopTask {
        client_token: client.token,
        gatherer: DefaultGatherer::default(),
        hardness,
    });

    server.run_state = ServerState::WorkersStopping;
    if hardness {
        client.return_processing("Performing hard stop...");
        server.scatter(
            RequestType::HardStop(HardStop {}).into(),
            task,
            Timeout::Default,
            None,
        );
    } else {
        client.return_processing("Performing soft stop...");
        server.scatter(
            RequestType::SoftStop(SoftStop {}).into(),
            task,
            Timeout::None,
            None,
        );
    }
}

impl GatheringTask for StopTask {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(
        self: Box<Self>,
        server: &mut Server,
        client: &mut OptionalClient,
        timed_out: bool,
    ) {
        if timed_out && self.hardness {
            client.finish_failure(format!(
                "Workers take too long to stop ({} ok, {} errors), stopping the main process to sever the link",
                self.gatherer.ok, self.gatherer.errors
            ));
        }
        server.run_state = ServerState::Stopping;
        client.finish_ok(format!(
            "Successfully closed {} workers, {} errors, stopping the main process...",
            self.gatherer.ok, self.gatherer.errors
        ));
    }
}
