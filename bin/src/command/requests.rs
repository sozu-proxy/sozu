use std::{
    collections::{BTreeMap, HashMap},
    env,
    fs::File,
    io::{ErrorKind, Read},
    path::PathBuf,
};

use mio::Token;
use nom::{HexDisplay, Offset};
use rusty_ulid::Ulid;
use sozu_command_lib::{
    buffer::fixed::Buffer,
    config::Config,
    logging,
    parser::parse_several_requests,
    proto::command::{
        AggregatedMetrics, AvailableMetrics, CertificatesWithFingerprints, ClusterHashes,
        ClusterInformations, Event, EventKind, FrontendFilters, HardStop, MetricsConfiguration,
        QueryCertificatesFilters, QueryMetricsOptions, Request, ResponseContent, ResponseStatus,
        RunState, SoftStop, Status, UpdateHttpListenerConfig, UpdateHttpsListenerConfig,
        UpdateTcpListenerConfig, WorkerInfo, WorkerInfos, WorkerRequest, WorkerResponses,
        request::RequestType, response_content::ContentType,
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

/// Pair a verb tag with its `config.<verb>` counter key in a single place so
/// the two strings cannot drift. Both must be string literals because the
/// metric drain stores `&'static str` keys.
///
/// Defined at the top of the module so that in-file macros expand before any
/// call site (Rust `macro_rules!` macros are textually scoped — definition
/// must precede use within a module).
macro_rules! audit_verb {
    ($verb:literal) => {
        ($verb, concat!("config.", $verb))
    };
}

/// Render the structured audit log line in the MUX `Session(...)` layout.
///
/// Expands to a `format!` producing
/// `[session_ulid request_ulid cluster_id|- backend_id|-]\tAUDIT\tSession(verb=..., actor_uid=..., client_id=..., target=..., result=...)\t >>>`
/// with ANSI colours when the logger is colour-enabled (empty strings
/// otherwise — see [`sozu_command_lib::logging::ansi_palette`]).
///
/// Mirrors the `log_context!` macro in `lib/src/protocol/mux/mod.rs:50` and
/// the two-arm `log_module_context!` in `lib/src/protocol/mux/router.rs:43`,
/// so operators can grep `AUDIT` alongside `MUX` / `MUX-ROUTER` / `RUSTLS` /
/// `PIPE` / `TCP` lines with a consistent tab layout.
macro_rules! audit_log_context {
    ($client:expr, $request_id:expr, $entry:expr, $result:expr) => {{
        let (open, reset, grey, gray, white) = ::sozu_command_lib::logging::ansi_palette();
        let log_ctx = ::sozu_command_lib::logging::LogContext {
            session_id: $client.session_ulid,
            request_id: Some(*$request_id),
            cluster_id: $entry.cluster_id.as_deref(),
            backend_id: $entry.backend_id.as_deref(),
        };
        format!(
            "{gray}{ctx}{reset}\t{open}AUDIT{reset}\t{grey}Session{reset}({gray}verb{reset}={white}{verb}{reset}, {gray}actor_uid{reset}={white}{actor_uid}{reset}, {gray}client_id{reset}={white}{client_id}{reset}, {gray}target{reset}={white}{target}{reset}, {gray}result{reset}={white}{result}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ctx = log_ctx,
            verb = $entry.verb,
            actor_uid = $client.actor_uid_display(),
            client_id = $client.id,
            target = $entry.target,
            result = $result,
        )
    }};
}

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
            | RequestType::ReplaceCertificate(_)
            | RequestType::UpdateHttpListener(_)
            | RequestType::UpdateHttpsListener(_)
            | RequestType::UpdateTcpListener(_) => {
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
            RequestType::ListFrontends(filters) => {
                Some(ContentType::FrontendList(self.state.list_frontends(filters)).into())
            }
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
    let mut path = PathBuf::from(path);
    if path.is_relative() {
        match std::env::current_dir() {
            Ok(cwd) => path = cwd.join(path),
            Err(error) => {
                client.finish_failure(format!("Cannot get Sōzu working directory: {error}",));
                return;
            }
        }
    }

    debug!("saving state to file {}", &path.display());
    let mut file = match File::create(&path) {
        Ok(file) => file,
        Err(error) => {
            client.finish_failure(format!(
                "Cannot create file at path {}: {error}",
                path.display()
            ));
            return;
        }
    };

    match server.state.write_requests_to_file(&mut file) {
        Ok(counter) => {
            client.finish_ok(format!(
                "Saved {counter} config messages to {}",
                &path.display()
            ));
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
        let (verb, counter) = audit_verb!("logging_level_changed");
        audit_emit_inline(
            server,
            client,
            EventKind::LoggingLevelChanged,
            verb,
            counter,
            format!("logging:{logging_filter}"),
            AuditResult::Err,
        );
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
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { env::set_var("RUST_LOG", &logging_filter) };
    debug!(
        "Logging level now: {}",
        env::var("RUST_LOG").unwrap_or("could get RUST_LOG from env".to_string())
    );

    let (verb, counter) = audit_verb!("logging_level_changed");
    audit_emit_inline(
        server,
        client,
        EventKind::LoggingLevelChanged,
        verb,
        counter,
        format!("logging:{logging_filter}"),
        AuditResult::Ok,
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

    let audit_target = format!("config:{}", config.config_path);

    let config_messages = match config.generate_config_messages() {
        Ok(messages) => messages,
        Err(config_err) => {
            // Only attribute the audit event when a client triggered the
            // reload — at startup (`client == None`) there is no actor.
            if let Some(client_ref) = client.as_deref() {
                let (verb, counter) = audit_verb!("configuration_reloaded");
                audit_emit_inline(
                    server,
                    client_ref,
                    EventKind::ConfigurationReloaded,
                    verb,
                    counter,
                    audit_target.clone(),
                    AuditResult::Err,
                );
            }
            client.finish_failure(format!("could not generate new config: {config_err}"));
            return;
        }
    };

    for (request_index, message) in config_messages.into_iter().enumerate() {
        let request = message.content;
        if let Err(error) = server.state.dispatch(&request) {
            client.return_processing(format!("Could not execute request on state: {error:#}"));
            continue;
        }

        if let &Some(RequestType::AddCertificate(_)) = &request.request_type {
            debug!("config generated AddCertificate( ... )");
        } else {
            debug!("config generated {:?}", request);
        }

        server.scatter_on(request, task_id, request_index, None);
    }

    if let Some(client_ref) = client.as_deref() {
        let (verb, counter) = audit_verb!("configuration_reloaded");
        audit_emit_inline(
            server,
            client_ref,
            EventKind::ConfigurationReloaded,
            verb,
            counter,
            audit_target,
            AuditResult::Ok,
        );
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
// Audit trail (control-plane mutations)

/// Outcome of a control-plane mutation, formatted into the structured
/// audit log line (MUX-style `Session(...)` layout).
#[derive(Clone, Copy)]
enum AuditResult {
    Ok,
    Err,
}

impl AuditResult {
    fn as_str(self) -> &'static str {
        match self {
            AuditResult::Ok => "ok",
            AuditResult::Err => "err",
        }
    }
}

impl std::fmt::Display for AuditResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A control-plane mutation, broken down into the pieces the audit trail
/// needs (event kind, verb name for the log, the matching counter key, and
/// the optional target identifiers populated on the emitted [Event]).
struct AuditEntry {
    kind: EventKind,
    /// Stable verb tag rendered inside the audit `Session(verb=...)` block.
    /// Always a static string.
    verb: &'static str,
    /// Pre-built `config.<verb>` counter key. Stored as a `&'static str` so
    /// `count!` can route it through statsd without a verb→key dispatch
    /// table — the construction sites pair `verb` and `counter` at a single
    /// site, eliminating drift.
    counter: &'static str,
    cluster_id: Option<String>,
    backend_id: Option<String>,
    address: Option<sozu_command_lib::proto::command::SocketAddress>,
    /// Free-form target descriptor for the audit log, e.g. `"address:127.0.0.1:8080"`
    /// or `"cluster:my-cluster"`. Captures whichever identifier is meaningful
    /// for the verb.
    target: String,
}

/// Build the [AuditEntry] for a control-plane request, or `None` for
/// non-mutating verbs (the caller skips them — they have no audit footprint).
fn audit_entry_for(request: &RequestType) -> Option<AuditEntry> {
    match request {
        RequestType::AddCluster(cluster) => {
            let (verb, counter) = audit_verb!("cluster_added");
            Some(AuditEntry {
                kind: EventKind::ClusterAdded,
                verb,
                counter,
                target: format!("cluster:{}", cluster.cluster_id),
                cluster_id: Some(cluster.cluster_id.to_owned()),
                backend_id: None,
                address: None,
            })
        }
        RequestType::RemoveCluster(cluster_id) => {
            let (verb, counter) = audit_verb!("cluster_removed");
            Some(AuditEntry {
                kind: EventKind::ClusterRemoved,
                verb,
                counter,
                target: format!("cluster:{cluster_id}"),
                cluster_id: Some(cluster_id.to_owned()),
                backend_id: None,
                address: None,
            })
        }
        RequestType::AddHttpFrontend(frontend) => {
            let (verb, counter) = audit_verb!("http_frontend_added");
            Some(AuditEntry {
                kind: EventKind::FrontendAdded,
                verb,
                counter,
                target: format!("frontend:http:{}:{}", frontend.hostname, frontend.address),
                cluster_id: frontend.cluster_id.clone(),
                backend_id: None,
                address: Some(frontend.address),
            })
        }
        RequestType::AddHttpsFrontend(frontend) => {
            let (verb, counter) = audit_verb!("https_frontend_added");
            Some(AuditEntry {
                kind: EventKind::FrontendAdded,
                verb,
                counter,
                target: format!("frontend:https:{}:{}", frontend.hostname, frontend.address),
                cluster_id: frontend.cluster_id.clone(),
                backend_id: None,
                address: Some(frontend.address),
            })
        }
        RequestType::AddTcpFrontend(frontend) => {
            let (verb, counter) = audit_verb!("tcp_frontend_added");
            Some(AuditEntry {
                kind: EventKind::FrontendAdded,
                verb,
                counter,
                target: format!("frontend:tcp:{}:{}", frontend.cluster_id, frontend.address),
                cluster_id: Some(frontend.cluster_id.to_owned()),
                backend_id: None,
                address: Some(frontend.address),
            })
        }
        RequestType::RemoveHttpFrontend(frontend) => {
            let (verb, counter) = audit_verb!("http_frontend_removed");
            Some(AuditEntry {
                kind: EventKind::FrontendRemoved,
                verb,
                counter,
                target: format!("frontend:http:{}:{}", frontend.hostname, frontend.address),
                cluster_id: frontend.cluster_id.clone(),
                backend_id: None,
                address: Some(frontend.address),
            })
        }
        RequestType::RemoveHttpsFrontend(frontend) => {
            let (verb, counter) = audit_verb!("https_frontend_removed");
            Some(AuditEntry {
                kind: EventKind::FrontendRemoved,
                verb,
                counter,
                target: format!("frontend:https:{}:{}", frontend.hostname, frontend.address),
                cluster_id: frontend.cluster_id.clone(),
                backend_id: None,
                address: Some(frontend.address),
            })
        }
        RequestType::RemoveTcpFrontend(frontend) => {
            let (verb, counter) = audit_verb!("tcp_frontend_removed");
            Some(AuditEntry {
                kind: EventKind::FrontendRemoved,
                verb,
                counter,
                target: format!("frontend:tcp:{}:{}", frontend.cluster_id, frontend.address),
                cluster_id: Some(frontend.cluster_id.to_owned()),
                backend_id: None,
                address: Some(frontend.address),
            })
        }
        RequestType::AddCertificate(add) => {
            let (verb, counter) = audit_verb!("certificate_added");
            Some(AuditEntry {
                kind: EventKind::CertificateAdded,
                verb,
                counter,
                target: format!("certificate:{}", add.address),
                cluster_id: None,
                backend_id: None,
                address: Some(add.address),
            })
        }
        RequestType::RemoveCertificate(remove) => {
            let (verb, counter) = audit_verb!("certificate_removed");
            Some(AuditEntry {
                kind: EventKind::CertificateRemoved,
                verb,
                counter,
                target: format!("certificate:{}:{}", remove.address, remove.fingerprint),
                cluster_id: None,
                backend_id: None,
                address: Some(remove.address),
            })
        }
        RequestType::ReplaceCertificate(replace) => {
            let (verb, counter) = audit_verb!("certificate_replaced");
            Some(AuditEntry {
                kind: EventKind::CertificateReplaced,
                verb,
                counter,
                target: format!(
                    "certificate:{}:{}",
                    replace.address, replace.old_fingerprint
                ),
                cluster_id: None,
                backend_id: None,
                address: Some(replace.address),
            })
        }
        RequestType::ActivateListener(listener) => {
            let (verb, counter) = audit_verb!("listener_activated");
            Some(AuditEntry {
                kind: EventKind::ListenerActivated,
                verb,
                counter,
                target: format!("listener:{:?}:{}", listener.proxy(), listener.address),
                cluster_id: None,
                backend_id: None,
                address: Some(listener.address),
            })
        }
        RequestType::DeactivateListener(listener) => {
            let (verb, counter) = audit_verb!("listener_deactivated");
            Some(AuditEntry {
                kind: EventKind::ListenerDeactivated,
                verb,
                counter,
                target: format!("listener:{:?}:{}", listener.proxy(), listener.address),
                cluster_id: None,
                backend_id: None,
                address: Some(listener.address),
            })
        }
        RequestType::UpdateHttpListener(patch) => {
            let (verb, counter) = audit_verb!("http_listener_updated");
            Some(AuditEntry {
                kind: EventKind::ListenerUpdated,
                verb,
                counter,
                target: format!(
                    "listener:http:{}:{}",
                    patch.address,
                    format_patch_diff_http(patch),
                ),
                cluster_id: None,
                backend_id: None,
                address: Some(patch.address),
            })
        }
        RequestType::UpdateHttpsListener(patch) => {
            let (verb, counter) = audit_verb!("https_listener_updated");
            Some(AuditEntry {
                kind: EventKind::ListenerUpdated,
                verb,
                counter,
                target: format!(
                    "listener:https:{}:{}",
                    patch.address,
                    format_patch_diff_https(patch),
                ),
                cluster_id: None,
                backend_id: None,
                address: Some(patch.address),
            })
        }
        RequestType::UpdateTcpListener(patch) => {
            let (verb, counter) = audit_verb!("tcp_listener_updated");
            Some(AuditEntry {
                kind: EventKind::ListenerUpdated,
                verb,
                counter,
                target: format!(
                    "listener:tcp:{}:{}",
                    patch.address,
                    format_patch_diff_tcp(patch),
                ),
                cluster_id: None,
                backend_id: None,
                address: Some(patch.address),
            })
        }
        // AddBackend / RemoveBackend are intentionally not audited via this
        // taxonomy — they are already covered by the BACKEND_DOWN / BACKEND_UP
        // events emitted by the workers when traffic reaches them.
        _ => None,
    }
}

/// Bump the per-verb counter, queue the [Event] for fan-out to subscribed
/// clients, and write the structured audit log line at `debug!` level
/// (visible in release builds via the `logs-debug` default feature; the
/// sample configs route `sozu::command::requests=debug` through the runtime
/// log filter so the line reaches stdout without an operator override).
/// Used by every control-plane mutation handler.
fn audit_emit(server: &mut Server, client: &ClientSession, entry: AuditEntry, result: AuditResult) {
    let request_id = Ulid::generate();
    // `entry.counter` is the pre-built `config.<verb>` static-str key
    // chosen at the construction site (paired with `verb` via
    // `audit_verb!`), so dashboards see one counter per verb without a
    // verb→key dispatch table.
    count!(entry.counter, 1);

    debug!(
        "{}",
        audit_log_context!(client, &request_id, &entry, result)
    );

    // Subscribers only see the proto Event; the verb / actor / request_id are
    // captured in the audit log line above (which is already structured).
    server.push_audit_event(Event {
        kind: entry.kind as i32,
        cluster_id: entry.cluster_id,
        backend_id: entry.backend_id,
        address: entry.address,
    });
}

/// Same as [audit_emit] but synthesises the [AuditEntry] inline for verbs
/// whose request payload does not carry the relevant identifiers (e.g.
/// `Logging`, `ConfigureMetrics`, `ReloadConfiguration`). Caller MUST pair
/// `verb` and `counter` via `audit_verb!` so the two strings cannot drift.
fn audit_emit_inline(
    server: &mut Server,
    client: &ClientSession,
    kind: EventKind,
    verb: &'static str,
    counter: &'static str,
    target: String,
    result: AuditResult,
) {
    audit_emit(
        server,
        client,
        AuditEntry {
            kind,
            verb,
            counter,
            target,
            cluster_id: None,
            backend_id: None,
            address: None,
        },
        result,
    );
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
    // Snapshot the audit entry before consuming `request_content` so we can
    // emit even when `state.dispatch` rejects the request.
    let audit = audit_entry_for(&request_content);

    // Special-case ConfigureMetrics — the proto payload is an i32 enum (no
    // dedicated message), so we synthesise the audit entry inline.
    let metrics_target = if let RequestType::ConfigureMetrics(value) = &request_content {
        Some(format!(
            "metrics:{:?}",
            MetricsConfiguration::try_from(*value).unwrap_or(MetricsConfiguration::Disabled)
        ))
    } else {
        None
    };

    let request = request_content.into();

    if let Err(error) = server.state.dispatch(&request) {
        if let Some(entry) = audit {
            audit_emit(server, client, entry, AuditResult::Err);
        } else if let Some(target) = metrics_target {
            let (verb, counter) = audit_verb!("metrics_configured");
            audit_emit_inline(
                server,
                client,
                EventKind::MetricsConfigured,
                verb,
                counter,
                target,
                AuditResult::Err,
            );
        }
        client.finish_failure(format!(
            "could not dispatch request on the main process state: {error}",
        ));
        return;
    }

    if let Some(entry) = audit {
        audit_emit(server, client, entry, AuditResult::Ok);
    } else if let Some(target) = metrics_target {
        let (verb, counter) = audit_verb!("metrics_configured");
        audit_emit_inline(
            server,
            client,
            EventKind::MetricsConfigured,
            verb,
            counter,
            target,
            AuditResult::Ok,
        );
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
        server: &mut Server,
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

        server.update_counts();
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
        server: &mut Server,
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
            summed_proxy_metrics.sort();
            summed_cluster_metrics.sort();
            summed_proxy_metrics.dedup();
            summed_cluster_metrics.dedup();

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

        let mut aggregated_metrics = AggregatedMetrics {
            main: main_metrics,
            clusters: BTreeMap::new(),
            workers: workers_metrics,
            proxying: BTreeMap::new(),
        };

        if !self.options.workers && server.workers.len() > 1 {
            aggregated_metrics.merge_metrics();
        }

        client.finish_ok_with_content(
            ContentType::Metrics(aggregated_metrics).into(),
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
                break Err(format!("saved state parse error: {parse_error:?}"));
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
        server: &mut Server,
        client: &mut OptionalClient,
        _timed_out: bool,
    ) {
        let DefaultGatherer { ok, errors, .. } = self.gatherer;
        server.update_counts();
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

// =========================================================
// Patch diff formatters — for the audit `target=` field.
// Walk each Option field of the patch and join the non-None ones into a
// compact key=value string so the audit log shows exactly what changed.

fn format_patch_diff_http(p: &UpdateHttpListenerConfig) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = p.public_address.as_ref() {
        parts.push(format!("public_address={v}"));
    }
    if let Some(v) = p.expect_proxy {
        parts.push(format!("expect_proxy={v}"));
    }
    if let Some(v) = p.sticky_name.as_deref() {
        parts.push(format!("sticky_name={v}"));
    }
    if let Some(v) = p.front_timeout {
        parts.push(format!("front_timeout={v}"));
    }
    if let Some(v) = p.back_timeout {
        parts.push(format!("back_timeout={v}"));
    }
    if let Some(v) = p.connect_timeout {
        parts.push(format!("connect_timeout={v}"));
    }
    if let Some(v) = p.request_timeout {
        parts.push(format!("request_timeout={v}"));
    }
    if p.http_answers.is_some() {
        parts.push("http_answers=<patched>".to_owned());
    }
    macro_rules! push_opt {
        ($field:ident) => {
            if let Some(v) = p.$field {
                parts.push(format!("{}={}", stringify!($field), v));
            }
        };
    }
    push_opt!(h2_max_rst_stream_per_window);
    push_opt!(h2_max_ping_per_window);
    push_opt!(h2_max_settings_per_window);
    push_opt!(h2_max_empty_data_per_window);
    push_opt!(h2_max_continuation_frames);
    push_opt!(h2_max_glitch_count);
    push_opt!(h2_initial_connection_window);
    push_opt!(h2_max_concurrent_streams);
    push_opt!(h2_stream_shrink_ratio);
    push_opt!(h2_max_rst_stream_lifetime);
    push_opt!(h2_max_rst_stream_abusive_lifetime);
    push_opt!(h2_max_rst_stream_emitted_lifetime);
    push_opt!(h2_max_header_list_size);
    push_opt!(h2_max_header_table_size);
    push_opt!(h2_stream_idle_timeout_seconds);
    push_opt!(h2_graceful_shutdown_deadline_seconds);
    push_opt!(h2_max_window_update_stream0_per_window);
    if let Some(v) = p.sozu_id_header.as_deref() {
        parts.push(format!("sozu_id_header={v}"));
    }
    if parts.is_empty() {
        "(no-op)".to_owned()
    } else {
        parts.join(" ")
    }
}

fn format_patch_diff_https(p: &UpdateHttpsListenerConfig) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = p.public_address.as_ref() {
        parts.push(format!("public_address={v}"));
    }
    if let Some(v) = p.expect_proxy {
        parts.push(format!("expect_proxy={v}"));
    }
    if let Some(v) = p.sticky_name.as_deref() {
        parts.push(format!("sticky_name={v}"));
    }
    if let Some(v) = p.front_timeout {
        parts.push(format!("front_timeout={v}"));
    }
    if let Some(v) = p.back_timeout {
        parts.push(format!("back_timeout={v}"));
    }
    if let Some(v) = p.connect_timeout {
        parts.push(format!("connect_timeout={v}"));
    }
    if let Some(v) = p.request_timeout {
        parts.push(format!("request_timeout={v}"));
    }
    if p.http_answers.is_some() {
        parts.push("http_answers=<patched>".to_owned());
    }
    if let Some(ref alpn) = p.alpn_protocols {
        if alpn.values.is_empty() {
            parts.push("alpn_protocols=<reset>".to_owned());
        } else {
            parts.push(format!("alpn_protocols={}", alpn.values.join(",")));
        }
    }
    if let Some(v) = p.strict_sni_binding {
        parts.push(format!("strict_sni_binding={v}"));
    }
    if let Some(v) = p.disable_http11 {
        parts.push(format!("disable_http11={v}"));
    }
    macro_rules! push_opt {
        ($field:ident) => {
            if let Some(v) = p.$field {
                parts.push(format!("{}={}", stringify!($field), v));
            }
        };
    }
    push_opt!(h2_max_rst_stream_per_window);
    push_opt!(h2_max_ping_per_window);
    push_opt!(h2_max_settings_per_window);
    push_opt!(h2_max_empty_data_per_window);
    push_opt!(h2_max_continuation_frames);
    push_opt!(h2_max_glitch_count);
    push_opt!(h2_initial_connection_window);
    push_opt!(h2_max_concurrent_streams);
    push_opt!(h2_stream_shrink_ratio);
    push_opt!(h2_max_rst_stream_lifetime);
    push_opt!(h2_max_rst_stream_abusive_lifetime);
    push_opt!(h2_max_rst_stream_emitted_lifetime);
    push_opt!(h2_max_header_list_size);
    push_opt!(h2_max_header_table_size);
    push_opt!(h2_stream_idle_timeout_seconds);
    push_opt!(h2_graceful_shutdown_deadline_seconds);
    push_opt!(h2_max_window_update_stream0_per_window);
    if let Some(v) = p.sozu_id_header.as_deref() {
        parts.push(format!("sozu_id_header={v}"));
    }
    if parts.is_empty() {
        "(no-op)".to_owned()
    } else {
        parts.join(" ")
    }
}

fn format_patch_diff_tcp(p: &UpdateTcpListenerConfig) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = p.public_address.as_ref() {
        parts.push(format!("public_address={v}"));
    }
    if let Some(v) = p.expect_proxy {
        parts.push(format!("expect_proxy={v}"));
    }
    if let Some(v) = p.front_timeout {
        parts.push(format!("front_timeout={v}"));
    }
    if let Some(v) = p.back_timeout {
        parts.push(format!("back_timeout={v}"));
    }
    if let Some(v) = p.connect_timeout {
        parts.push(format!("connect_timeout={v}"));
    }
    if parts.is_empty() {
        "(no-op)".to_owned()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod audit_format_tests {
    //! Drift-guard for the audit log line. Rejects any accidental change to
    //! the MUX `Session(...)` layout that would break downstream grep-based
    //! consumers (SIEM pipelines, operator shell recipes).
    //!
    //! The default thread-local logger reports `is_logger_colored() == false`
    //! (see `command/src/logging/logs.rs:30`), so `ansi_palette()` returns
    //! empty strings and the rendered line is ANSI-free — stable to match
    //! with a plain regex.
    use super::{AuditEntry, AuditResult};
    use regex::Regex;
    use rusty_ulid::Ulid;
    use sozu_command_lib::proto::command::EventKind;

    /// Minimal stand-in exposing only the `ClientSession` fields and method
    /// that `audit_log_context!` reads. Avoids constructing the full
    /// `Channel<Response, Request>` that `ClientSession::new` requires.
    struct TestClient {
        session_ulid: Ulid,
        id: u32,
        actor_uid: Option<u32>,
    }

    impl TestClient {
        fn actor_uid_display(&self) -> String {
            match self.actor_uid {
                Some(uid) => uid.to_string(),
                None => String::from("unknown"),
            }
        }
    }

    fn sample_entry(cluster_id: Option<&str>) -> AuditEntry {
        AuditEntry {
            kind: EventKind::ClusterAdded,
            verb: "cluster_added",
            counter: "config.cluster_added",
            cluster_id: cluster_id.map(str::to_owned),
            backend_id: None,
            address: None,
            target: "cluster:my_app".to_owned(),
        }
    }

    fn pattern() -> Regex {
        // Anchored: covers the 4-slot bracket, the `AUDIT\tSession(...)\t >>>`
        // tail, and the field order. Crockford base32 ULIDs are 26 chars over
        // [0-9A-Z]; missing bracket slots render as a dash.
        Regex::new(concat!(
            r"^\[[0-9A-Z]{26} [0-9A-Z]{26} (?:[A-Za-z0-9_:.-]+|-) (?:[A-Za-z0-9_:.-]+|-)\]",
            r"\tAUDIT\tSession\(",
            r"verb=[a-z_]+, actor_uid=(?:\d+|unknown), client_id=\d+, target=[^,]+, result=(?:ok|err)",
            r"\)\t >>>$",
        ))
        .expect("audit-format regex must compile")
    }

    #[test]
    fn layout_with_cluster_id_matches() {
        let client = TestClient {
            session_ulid: Ulid::generate(),
            id: 42,
            actor_uid: Some(1000),
        };
        let request_id = Ulid::generate();
        let entry = sample_entry(Some("my_app"));
        let rendered = audit_log_context!(client, &request_id, &entry, AuditResult::Ok);
        assert!(
            pattern().is_match(&rendered),
            "rendered line did not match audit-log pattern.\nrendered: {rendered:?}"
        );
    }

    #[test]
    fn layout_with_dashed_cluster_and_backend_matches() {
        let client = TestClient {
            session_ulid: Ulid::generate(),
            id: 1,
            actor_uid: None,
        };
        let request_id = Ulid::generate();
        let entry = sample_entry(None);
        let rendered = audit_log_context!(client, &request_id, &entry, AuditResult::Err);
        assert!(
            pattern().is_match(&rendered),
            "rendered line did not match audit-log pattern.\nrendered: {rendered:?}"
        );
    }
}
