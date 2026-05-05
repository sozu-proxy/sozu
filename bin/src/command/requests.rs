//! Master command-verb dispatcher and audit envelope.
//!
//! Receives decoded `Request` protos from connected CLI clients, applies
//! `command_allowed_uids` admission, validates and fans out cluster /
//! listener / certificate / runtime mutations to workers, and emits the
//! audit-log envelope (`sanitize_for_audit`-protected JSON sink) for every
//! mutating verb. Long-form lifecycle: `bin/src/command/LIFECYCLE.md`.

use std::{
    collections::{BTreeMap, HashMap},
    env,
    fs::File,
    io::{ErrorKind, Read},
    path::PathBuf,
    time::Instant,
};

use mio::Token;
use nom::{HexDisplay, Offset};
use prost::Message as _;
use rusty_ulid::Ulid;
use sha2::{Digest, Sha256};
use sozu_command_lib::{
    buffer::fixed::Buffer,
    config::Config,
    logging,
    parser::parse_several_requests,
    proto::command::{
        AggregatedMetrics, AvailableMetrics, CertificatesWithFingerprints, ClusterHashes,
        ClusterInformations, Event, EventKind, FrontendFilters, HardStop, MetricDetail,
        MetricDetailStatus, MetricsConfiguration, QueryCertificatesFilters, QueryHealthChecks,
        QueryMetricsOptions, Request, ResponseContent, ResponseStatus, RunState, SetMetricDetail,
        SoftStop, Status, UpdateHttpListenerConfig, UpdateHttpsListenerConfig,
        UpdateTcpListenerConfig, WorkerInfo, WorkerInfos, WorkerRequest, WorkerResponses,
        request::RequestType, response_content::ContentType,
    },
    sd_notify,
};
use sozu_lib::metrics::METRICS;

use crate::command::{
    server::{
        DefaultGatherer, Gatherer, GatheringTask, MessageClient, Server, ServerState, Timeout,
        WorkerId,
    },
    sessions::{ClientSession, OptionalClient, sanitize_for_audit, sanitize_for_audit_kv},
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

/// Render the structured audit log line in the MUX-family layout.
///
/// Expands to a `format!` producing
/// `[session_ulid request_ulid cluster_id|- backend_id|-]\tAUDIT\tCommand(verb=..., actor_uid=..., actor_gid=..., actor_pid=..., actor_comm=..., client_id=..., target=..., result=..., [error_code=..., reason=..., elapsed_ms=..., fanout=..., workers=<ok>/<err>/<expected>,] sozu_version=...)`
/// with ANSI colours when the logger is colour-enabled (empty strings
/// otherwise — see [`sozu_command_lib::logging::ansi_palette`]). Bracketed
/// fields are emitted only when set on [`AuditEntry`] / the caller.
///
/// Bracket layout mirrors `log_context!` in `lib/src/protocol/mux/mod.rs:50`
/// so operators can grep `AUDIT` alongside `MUX` / `RUSTLS` / `PIPE` / `TCP`.
/// Uses the `Command(...)` keyword (vs. `Session(...)` in MUX lines) because
/// the payload describes a control-plane command, not a proxy session. The
/// line is self-contained — no `\t >>>` continuation marker since nothing
/// follows the closing paren.
///
/// Every string field — `verb` is a `&'static str` and therefore trusted,
/// but `target`, `cluster_id`, `backend_id`, `actor_comm`, `reason` can
/// originate from attacker-influenced input (cluster IDs from sozu CLI
/// arguments, hostnames from frontend configs, error messages from
/// `state.dispatch`). All of them go through
/// [`sozu_command_lib::sessions::sanitize_for_audit`] at render time to
/// neutralise `\n`/`\t`/`\x1b` injection that would otherwise forge a
/// second audit line.
macro_rules! audit_log_context {
    ($server:expr, $client:expr, $request_id:expr, $entry:expr, $result:expr) => {{
        use $crate::command::sessions::{sanitize_for_audit, sanitize_for_audit_kv};
        let (open, reset, grey, gray, white) = ::sozu_command_lib::logging::ansi_palette();
        let log_ctx = ::sozu_command_lib::logging::LogContext {
            session_id: $client.session_ulid,
            request_id: Some(*$request_id),
            cluster_id: $entry.cluster_id.as_deref(),
            backend_id: $entry.backend_id.as_deref(),
        };
        let mut extras = String::new();
        if let Some(code) = $entry.extras.error_code {
            extras.push_str(&format!(
                ", {gray}error_code{reset}={white}{code}{reset}",
                gray = gray,
                reset = reset,
                white = white,
                code = code,
            ));
        }
        if let Some(reason) = $entry.extras.reason.as_deref() {
            let sanitized = sanitize_for_audit(reason);
            let truncated = if sanitized.chars().count() > AUDIT_REASON_MAX_CHARS {
                let cut: String = sanitized.chars().take(AUDIT_REASON_MAX_CHARS).collect();
                format!("{cut}…")
            } else {
                sanitized
            };
            extras.push_str(&format!(
                ", {gray}reason{reset}={white}{reason}{reset}",
                gray = gray,
                reset = reset,
                white = white,
                reason = truncated,
            ));
        }
        if let Some(elapsed) = $entry.extras.elapsed_ms {
            extras.push_str(&format!(
                ", {gray}elapsed_ms{reset}={white}{elapsed}{reset}",
                gray = gray,
                reset = reset,
                white = white,
                elapsed = elapsed,
            ));
        }
        if let Some(fanout) = $entry.extras.fanout {
            extras.push_str(&format!(
                ", {gray}fanout{reset}={white}{status}{reset}, {gray}workers{reset}={white}{ok}/{err}/{expected}{reset}",
                gray = gray,
                reset = reset,
                white = white,
                status = fanout.status,
                ok = fanout.workers_ok,
                err = fanout.workers_err,
                expected = fanout.workers_expected,
            ));
        }
        if let Some(hash) = $entry.extras.request_sha256.as_deref() {
            extras.push_str(&format!(
                ", {gray}request_sha256{reset}={white}{hash}{reset}",
                gray = gray,
                reset = reset,
                white = white,
                hash = hash,
            ));
        }
        if let Some(lease_id) = $entry.extras.metric_detail_lease_id.as_deref() {
            let sanitized = sanitize_for_audit_kv(lease_id);
            let truncated = if sanitized.chars().count() > AUDIT_LEASE_ID_MAX_CHARS {
                let cut: String = sanitized.chars().take(AUDIT_LEASE_ID_MAX_CHARS).collect();
                format!("{cut}…")
            } else {
                sanitized
            };
            extras.push_str(&format!(
                ", {gray}lease_id{reset}={white}{value}{reset}",
                gray = gray,
                reset = reset,
                white = white,
                value = truncated,
            ));
        }
        if let Some(detail_reason) = $entry.extras.metric_detail_reason.as_deref() {
            let sanitized = sanitize_for_audit_kv(detail_reason);
            let truncated = if sanitized.chars().count() > AUDIT_REASON_MAX_CHARS {
                let cut: String = sanitized.chars().take(AUDIT_REASON_MAX_CHARS).collect();
                format!("{cut}…")
            } else {
                sanitized
            };
            extras.push_str(&format!(
                ", {gray}metric_detail_reason{reset}={white}{value}{reset}",
                gray = gray,
                reset = reset,
                white = white,
                value = truncated,
            ));
        }
        let now_ts = rfc3339_utc(std::time::SystemTime::now());
        let connect_ts = $client.connect_ts_display();
        format!(
            "{gray}{ctx}{reset}\t{open}AUDIT{reset}\t{grey}Command{reset}({gray}ts{reset}={white}{ts}{reset}, {gray}verb{reset}={white}{verb}{reset}, {gray}actor_uid{reset}={white}{actor_uid}{reset}, {gray}actor_gid{reset}={white}{actor_gid}{reset}, {gray}actor_pid{reset}={white}{actor_pid}{reset}, {gray}actor_role{reset}={white}{actor_role}{reset}, {gray}actor_user{reset}={white}{actor_user}{reset}, {gray}actor_comm{reset}={white}{actor_comm}{reset}, {gray}client_id{reset}={white}{client_id}{reset}, {gray}connect_ts{reset}={white}{connect_ts}{reset}, {gray}socket{reset}={white}{socket_path}{reset}, {gray}target{reset}={white}{target}{reset}, {gray}result{reset}={white}{result}{reset}{extras}, {gray}sozu_version{reset}={white}{sozu_version}{reset}, {gray}build_git_sha{reset}={white}{build_git_sha}{reset}, {gray}boot_generation{reset}={white}{boot_generation}{reset})",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ctx = log_ctx,
            ts = now_ts,
            verb = $entry.verb,
            actor_uid = $client.actor_uid_display(),
            actor_gid = $client.actor_gid_display(),
            actor_pid = $client.actor_pid_display(),
            actor_role = actor_role($client.actor_uid),
            actor_user = $client.actor_user_display(),
            actor_comm = $client.actor_comm_display(),
            client_id = $client.id,
            connect_ts = connect_ts,
            socket_path = sanitize_for_audit(&$client.socket_path),
            target = sanitize_for_audit(&$entry.target),
            result = $result,
            extras = extras,
            sozu_version = SOZU_VERSION,
            build_git_sha = SOZU_BUILD_GIT_SHA,
            boot_generation = $server.boot_generation,
        )
    }};
}

/// Operator-issued verbs that change cluster / listener / certificate
/// state, the saved state file, or the master/worker fleet topology.
/// `true` here brackets the dispatch with `RELOADING=1` / `READY=1`
/// systemd notifications so unit-state-watching tooling can serialise
/// against the change. Read-only verbs (Status / List* / Query* /
/// CountRequests / SubscribeEvents / QueryMaxConnectionsPerIp) return
/// `false` because they're dashboard polls, not transitions.
///
/// `SetMetricDetail` is deliberately excluded: it is a runtime
/// observability knob, not a state transition, and the `sozu top` TUI
/// renews its lease every `ttl/2` seconds (≈ 30 s by default). Including
/// it in this set would flap the systemd unit through `reloading`
/// every renewal for the whole TUI session lifetime. The audit trail
/// for the verb still flows through the special-case inline emission
/// (`EventKind::MetricDetailChanged`, proto tag 30) so SOC visibility
/// is preserved without flapping the unit state.
fn is_mutating_verb(req: &RequestType) -> bool {
    matches!(
        req,
        RequestType::SaveState(_)
            | RequestType::LoadState(_)
            | RequestType::ReloadConfiguration(_)
            | RequestType::UpgradeMain(_)
            | RequestType::UpgradeWorker(_)
            | RequestType::AddCluster(_)
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
            | RequestType::UpdateTcpListener(_)
            | RequestType::SetHealthCheck(_)
            | RequestType::RemoveHealthCheck(_)
            | RequestType::SoftStop(_)
            | RequestType::HardStop(_)
            | RequestType::Logging(_)
            | RequestType::SetMaxConnectionsPerIp(_)
    )
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
        // Optional UID allowlist. `None` preserves the historical
        // behaviour (same-UID local process can do anything).
        // When set, requests from UIDs outside the list are rejected
        // before dispatch — both read and write — and the rejection
        // appears in the audit trail via `client.finish_failure`.
        if let Some(allowed) = self.config.command_allowed_uids.as_ref() {
            let actor_uid = client.actor_uid;
            let permitted = actor_uid.is_some_and(|u| allowed.contains(&u));
            if !permitted {
                warn!(
                    "rejecting command-socket request from non-allowlisted UID: actor_uid={} allowed={:?} verb={:?}",
                    actor_uid
                        .map(|u| u.to_string())
                        .unwrap_or_else(|| "unknown".to_owned()),
                    allowed,
                    std::mem::discriminant(&request_type)
                );
                client.finish_failure(format!(
                    "unauthorized: actor UID {} not in command_allowed_uids",
                    actor_uid
                        .map(|u| u.to_string())
                        .unwrap_or_else(|| "unknown".to_owned())
                ));
                return;
            }
        }

        // #228: bracket every operator-issued command with
        // `RELOADING=1` / `READY=1`. Mutating verbs (LoadState,
        // ReloadConfiguration, AddCluster / Backend / Certificate /
        // Frontend / Listener, Remove*, Replace*, Update*,
        // SetHealthCheck, SetMaxConnectionsPerIp, UpgradeMain,
        // UpgradeWorker) move the master through a brief reload
        // window where downstream tooling watching the unit state
        // can serialise against in-flight changes. Read-only verbs
        // (Status, ListWorkers, ListListeners, ListFrontends,
        // QueryClusters*, QueryMetrics, QueryCertificates*,
        // CountRequests, QueryHealthChecks, SubscribeEvents,
        // QueryMaxConnectionsPerIp) skip the bracketing — those are
        // dashboard polls, not state transitions.
        //
        // Helper is a no-op when `$NOTIFY_SOCKET` is unset, so the
        // cost is one env-var lookup per command in non-systemd
        // deployments.
        let mutating = is_mutating_verb(&request_type);
        if mutating {
            if let Err(e) = sd_notify::notify(sd_notify::STATE_RELOADING) {
                warn!("could not notify systemd RELOADING=1: {}", e);
            }
        }

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
            | RequestType::UpdateTcpListener(_)
            | RequestType::SetHealthCheck(_)
            | RequestType::RemoveHealthCheck(_) => {
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
            RequestType::QueryHealthChecks(query) => list_health_checks(self, client, query),

            RequestType::LaunchWorker(_) => {} // not yet implemented, nor used, anywhere
            RequestType::ReturnListenSockets(_) => {} // This is only implemented by workers,
            // Per-(cluster, source-IP) connection-limit runtime hooks. Both
            // the setter and the query are pure worker-side operations
            // (the live counter lives in `SessionManager`, not in the
            // master's `ConfigState`), so we hand them off to the
            // generic worker fan-out path.
            RequestType::SetMaxConnectionsPerIp(_) | RequestType::QueryMaxConnectionsPerIp(_) => {
                worker_request(self, client, request_type);
            }
            // `sozu top`'s runtime cardinality lease verb. Each worker maintains
            // its own lease table and recomputes the effective `MetricDetail` as
            // `max(configured, max(active leases))`. The master fans the verb out
            // through a dedicated dispatcher that synthesises the aggregate
            // `MetricDetailStatus` reply, captures the master's own
            // configured/effective view, and emits the attempt-time + completion
            // audit rows alongside the per-worker fan-out.
            RequestType::SetMetricDetail(req) => {
                set_metric_detail_request(self, client, req);
            }
        }

        if mutating {
            if let Err(e) = sd_notify::notify(sd_notify::STATE_READY) {
                warn!("could not notify systemd READY=1: {}", e);
            }
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

fn list_health_checks(server: &mut Server, client: &mut ClientSession, query: QueryHealthChecks) {
    let health_checks = server.state.list_health_checks(query.cluster_id.as_deref());
    client.finish_ok_with_content(
        ContentType::HealthChecksList(health_checks).into(),
        "Successfully listed health check configurations",
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
                let (verb, counter) = audit_verb!("state_saved");
                audit_emit_inline(
                    server,
                    client,
                    EventKind::StateSaved,
                    verb,
                    counter,
                    format!("file:{}", path.display()),
                    AuditResult::Err,
                    AuditExtras {
                        error_code: Some(AuditErrorCode::IoError),
                        ..Default::default()
                    },
                );
                client.finish_failure(format!("Cannot get Sōzu working directory: {error}",));
                return;
            }
        }
    }

    debug!("saving state to file {}", &path.display());
    let mut file = match File::create(&path) {
        Ok(file) => file,
        Err(error) => {
            let (verb, counter) = audit_verb!("state_saved");
            audit_emit_inline(
                server,
                client,
                EventKind::StateSaved,
                verb,
                counter,
                format!("file:{}", path.display()),
                AuditResult::Err,
                AuditExtras {
                    error_code: Some(AuditErrorCode::IoError),
                    ..Default::default()
                },
            );
            client.finish_failure(format!(
                "Cannot create file at path {}: {error}",
                path.display()
            ));
            return;
        }
    };

    match server.state.write_requests_to_file(&mut file) {
        Ok(count) => {
            let (verb, counter) = audit_verb!("state_saved");
            audit_emit_inline(
                server,
                client,
                EventKind::StateSaved,
                verb,
                counter,
                format!("file:{} messages:{count}", path.display()),
                AuditResult::Ok,
                AuditExtras::default(),
            );
            client.finish_ok(format!(
                "Saved {count} config messages to {}",
                &path.display()
            ));
        }
        Err(error) => {
            let (verb, counter) = audit_verb!("state_saved");
            audit_emit_inline(
                server,
                client,
                EventKind::StateSaved,
                verb,
                counter,
                format!("file:{}", path.display()),
                AuditResult::Err,
                AuditExtras {
                    error_code: Some(AuditErrorCode::IoError),
                    ..Default::default()
                },
            );
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
        let reason = errors
            .iter()
            .map(logging::LogSpecParseError::to_string)
            .collect::<Vec<String>>()
            .join("; ");
        audit_emit_inline(
            server,
            client,
            EventKind::LoggingLevelChanged,
            verb,
            counter,
            format!("logging:{logging_filter}"),
            AuditResult::Err,
            AuditExtras {
                error_code: Some(AuditErrorCode::InvalidInput),
                reason: Some(reason.clone()),
                ..Default::default()
            },
        );
        client.finish_failure(format!("Error parsing logging filter:\n- {reason}"));
        return;
    }
    logging::LOGGER.with(|logger| {
        logger.borrow_mut().set_directives(directives);
    });

    // also change / set the content of RUST_LOG so future workers / main thread
    // will have the new logging filter value
    // TODO: Audit that the environment access only happens in single-threaded code.
    // SAFETY: `env::set_var` in Rust 2024 is unsafe because it is not
    // thread-safe. The supervisor that handles `LoggingFilter` requests is
    // single-threaded (mio event loop on the command socket), and workers
    // are separate processes that re-read RUST_LOG after fork-and-exec —
    // so the racy read it would otherwise fight with does not exist here.
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
        AuditExtras::default(),
    );

    worker_request(server, client, RequestType::Logging(logging_filter));
}

fn subscribe_client_to_events(server: &mut Server, client: &mut ClientSession) {
    info!("Subscribing client {:?} to listen to events", client.token);
    server.event_subscribers.insert(client.token);
    let (verb, counter) = audit_verb!("events_subscribed");
    audit_emit_inline(
        server,
        client,
        EventKind::EventsSubscribed,
        verb,
        counter,
        format!("subscribe:client_id:{}", client.id),
        AuditResult::Ok,
        AuditExtras::default(),
    );
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
                    AuditExtras::default(),
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
            AuditExtras::default(),
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
/// audit log line.
#[derive(Clone, Copy)]
pub(crate) enum AuditResult {
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

/// Structured failure reason. Exists so SIEM alerts can group-by without
/// grepping free-form error strings. Paired with `result=err` in the
/// audit line; omitted for `result=ok`.
///
/// `PeerCredUnavailable` and `Other` are reserved — they fire once the
/// callers that need them land (SO_PEERCRED failure audit, catch-all
/// emit path). Suppressing dead-code warnings keeps the taxonomy stable
/// as we wire them up.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum AuditErrorCode {
    /// `state.dispatch` rejected the request on the main process.
    DispatchError,
    /// One or more workers returned `Failure` during fan-out.
    WorkerFailure,
    /// Fan-out timed out before every worker responded.
    WorkerTimeout,
    /// `SO_PEERCRED` returned no credentials; actor attribution missing.
    PeerCredUnavailable,
    /// Operator supplied invalid input (e.g. malformed logging filter).
    InvalidInput,
    /// I/O error on state save/load (disk full, permission denied, parse).
    IoError,
    /// Generic bucket for anything that doesn't fit the above. Prefer
    /// adding a new variant over reusing this.
    Other,
}

impl AuditErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            AuditErrorCode::DispatchError => "dispatch_error",
            AuditErrorCode::WorkerFailure => "worker_failure",
            AuditErrorCode::WorkerTimeout => "worker_timeout",
            AuditErrorCode::PeerCredUnavailable => "peer_cred_unavailable",
            AuditErrorCode::InvalidInput => "invalid_input",
            AuditErrorCode::IoError => "io_error",
            AuditErrorCode::Other => "other",
        }
    }
}

impl std::fmt::Display for AuditErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Worker fan-out outcome, rendered in the completion audit line.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FanoutStatus {
    /// Every expected worker acknowledged with Ok.
    Ok,
    /// Some workers reported Failure; others were Ok.
    Partial,
    /// Fan-out didn't reach all workers within the deadline.
    Timeout,
    /// No workers expected (local-main-only request).
    LocalOnly,
}

impl FanoutStatus {
    fn as_str(self) -> &'static str {
        match self {
            FanoutStatus::Ok => "ok",
            FanoutStatus::Partial => "partial",
            FanoutStatus::Timeout => "timeout",
            FanoutStatus::LocalOnly => "local_only",
        }
    }
}

impl std::fmt::Display for FanoutStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Worker fan-out summary attached to completion-time audit emissions.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FanoutSummary {
    status: FanoutStatus,
    workers_ok: u32,
    workers_err: u32,
    workers_expected: u32,
}

/// Optional audit-line fields populated at completion time or when a
/// failure reason is known. Defaulted to all-`None` at `AuditEntry`
/// construction so the existing build sites don't all need to set them;
/// emitters that know these values fill them in via helper constructors
/// before calling [`audit_emit`] / [`audit_emit_inline`].
#[derive(Debug, Default, Clone)]
pub(crate) struct AuditExtras {
    /// Wall-clock milliseconds between request acceptance and audit emission.
    pub(crate) elapsed_ms: Option<u64>,
    /// Structured failure reason. Set only on `AuditResult::Err` paths.
    pub(crate) error_code: Option<AuditErrorCode>,
    /// Worker fan-out outcome. Set on completion-time emissions.
    pub(crate) fanout: Option<FanoutSummary>,
    /// Short truncated failure detail — mirrors `finish_failure` message.
    pub(crate) reason: Option<String>,
    /// Truncated hex-encoded SHA-256 fingerprint of the proto `Request`
    /// bytes, for dedupe / replay detection. First 16 hex chars (64 bits).
    /// Set for verbs that flow through `worker_request`; `None` for inline
    /// verbs that don't carry a payload worth hashing.
    pub(crate) request_sha256: Option<String>,
    /// Operator-supplied `SetMetricDetail.client_id` (the lease key). Set
    /// only for the `MetricDetailChanged` audit verb. Distinct from the
    /// connection-scoped `ClientSession.id` rendered in the outer audit
    /// envelope: this one identifies the lease, that one identifies the
    /// command-socket caller. Rendered as a dedicated `lease_id=…` field
    /// so attacker-supplied `:` / `=` cannot smuggle a fake column.
    pub(crate) metric_detail_lease_id: Option<String>,
    /// Operator-supplied `SetMetricDetail.reason`. Free-form human note.
    /// Sanitised via [`sanitize_for_audit_kv`] (control bytes + `,` + `=`
    /// stripped) and truncated to [`AUDIT_REASON_MAX_CHARS`].
    pub(crate) metric_detail_reason: Option<String>,
}

/// A control-plane mutation, broken down into the pieces the audit trail
/// needs (event kind, verb name for the log, the matching counter key, and
/// the optional target identifiers populated on the emitted [Event]).
#[derive(Debug)]
struct AuditEntry {
    kind: EventKind,
    /// Stable verb tag rendered inside the audit `Command(verb=...)` block.
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
    /// Optional timing / error_code / fanout / reason fields. Defaulted at
    /// construction; populated via [`AuditEntry::with_extras`] on the hot
    /// paths that care.
    extras: AuditExtras,
}

/// Truncated SHA-256 request fingerprint for dedupe / correlation.
/// Render-only helper; see [`audit_log_context!`] for inclusion.
const AUDIT_REASON_MAX_CHARS: usize = 256;

/// Hard cap on the rendered length of `lease_id` (operator-supplied
/// `SetMetricDetail.client_id`) in the audit log. The legitimate TUI
/// format is `top:<pid>:<8-hex>` ≤ 24 bytes; 64 leaves headroom for
/// other operator-side scrapers while keeping the audit line bounded.
const AUDIT_LEASE_ID_MAX_CHARS: usize = 64;

/// Compile-time sozu version tag — rendered in every audit line so
/// operators correlate which binary emitted which log during upgrades.
const SOZU_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build-time short git SHA, embedded by `bin/build.rs`. Falls back to
/// `"unknown"` on builds outside a git tree (vendored tarballs, sysroots).
/// Together with `sozu_version` it pins which exact commit emitted a
/// given audit line — useful when a regression lands between the same
/// semver tag.
const SOZU_BUILD_GIT_SHA: &str = env!("SOZU_BUILD_GIT_SHA");

/// Render an actor role hint for SOC scanning. Rule:
/// - `uid == 0`         → `root`     (super-user, P1 alert)
/// - `1 <= uid < 1000`  → `system`   (service account, expected daemons)
/// - `uid >= 1000`      → `user`     (normal interactive operator)
/// - missing            → `unknown`  (SO_PEERCRED unavailable)
///
/// `1000` is the conventional Linux NSS uid floor for human accounts.
/// Operators on systems with a different convention (BSD, macOS) get the
/// same buckets — the labels are advisory, the raw `actor_uid` is still
/// authoritative.
pub(crate) fn actor_role(uid: Option<u32>) -> &'static str {
    match uid {
        None => "unknown",
        Some(0) => "root",
        Some(u) if u < 1000 => "system",
        Some(_) => "user",
    }
}

/// Render a `SystemTime` as an RFC 3339 / ISO 8601 timestamp at UTC
/// (`YYYY-MM-DDTHH:MM:SS.ffffffZ`). std-only — uses Howard Hinnant's
/// `civil_from_days` algorithm so we don't pull in `chrono` / `time`.
///
/// Six-digit fractional seconds (microseconds) — matches what most SIEM
/// stacks expect and avoids the precision overhead of nanoseconds.
pub(crate) fn rfc3339_utc(t: std::time::SystemTime) -> String {
    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let micros = dur.subsec_micros();

    let days = secs.div_euclid(86_400);
    let sec_of_day = secs.rem_euclid(86_400) as u32;
    let hh = sec_of_day / 3600;
    let mm = (sec_of_day / 60) % 60;
    let ss = sec_of_day % 60;

    // Hinnant's civil_from_days — `days` is days since 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if m <= 2 { 1 } else { 0 };

    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{micros:06}Z")
}

/// Build the [AuditEntry] for a control-plane request, or `None` for
/// non-mutating verbs (the caller skips them — they have no audit footprint).
///
/// `state` is used to snapshot the pre-change listener config for
/// `UpdateHttp/Https/TcpListener` so the audit line can show
/// `field=old→new` pairs. Passing the `ConfigState` by reference stays
/// cheap because only the UpdateListener arms look anything up.
fn audit_entry_for(
    request: &RequestType,
    state: &sozu_command_lib::state::ConfigState,
) -> Option<AuditEntry> {
    use std::net::SocketAddr;
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
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
            })
        }
        RequestType::ReplaceCertificate(replace) => {
            let (verb, counter) = audit_verb!("certificate_replaced");
            // Compute the new cert's fingerprint from its PEM so the audit
            // trail records both the cert being removed AND the cert
            // replacing it. Forensic value: rotation pattern + detection of
            // substituted-cert attacks. Best-effort: on parse failure the
            // new fingerprint falls back to `"unknown"` rather than
            // aborting the audit emission.
            let new_fp =
                compute_certificate_fingerprint(replace.new_certificate.certificate.as_bytes())
                    .unwrap_or_else(|| "unknown".to_owned());
            Some(AuditEntry {
                kind: EventKind::CertificateReplaced,
                verb,
                counter,
                target: format!(
                    "certificate:{}:old={}:new={}",
                    replace.address, replace.old_fingerprint, new_fp
                ),
                cluster_id: None,
                backend_id: None,
                address: Some(replace.address),
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
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
                extras: AuditExtras::default(),
            })
        }
        RequestType::UpdateHttpListener(patch) => {
            let (verb, counter) = audit_verb!("http_listener_updated");
            let current = state.http_listeners.get(&SocketAddr::from(patch.address));
            Some(AuditEntry {
                kind: EventKind::ListenerUpdated,
                verb,
                counter,
                target: format!(
                    "listener:http:{}:{}",
                    patch.address,
                    format_patch_diff_http(patch, current),
                ),
                cluster_id: None,
                backend_id: None,
                address: Some(patch.address),
                extras: AuditExtras::default(),
            })
        }
        RequestType::UpdateHttpsListener(patch) => {
            let (verb, counter) = audit_verb!("https_listener_updated");
            let current = state.https_listeners.get(&SocketAddr::from(patch.address));
            Some(AuditEntry {
                kind: EventKind::ListenerUpdated,
                verb,
                counter,
                target: format!(
                    "listener:https:{}:{}",
                    patch.address,
                    format_patch_diff_https(patch, current),
                ),
                cluster_id: None,
                backend_id: None,
                address: Some(patch.address),
                extras: AuditExtras::default(),
            })
        }
        RequestType::UpdateTcpListener(patch) => {
            let (verb, counter) = audit_verb!("tcp_listener_updated");
            let current = state.tcp_listeners.get(&SocketAddr::from(patch.address));
            Some(AuditEntry {
                kind: EventKind::ListenerUpdated,
                verb,
                counter,
                target: format!(
                    "listener:tcp:{}:{}",
                    patch.address,
                    format_patch_diff_tcp(patch, current),
                ),
                cluster_id: None,
                backend_id: None,
                address: Some(patch.address),
                extras: AuditExtras::default(),
            })
        }
        RequestType::AddHttpListener(listener) => {
            let (verb, counter) = audit_verb!("http_listener_added");
            Some(AuditEntry {
                kind: EventKind::ListenerAdded,
                verb,
                counter,
                target: format!("listener:http:{}", listener.address),
                cluster_id: None,
                backend_id: None,
                address: Some(listener.address),
                extras: AuditExtras::default(),
            })
        }
        RequestType::AddHttpsListener(listener) => {
            let (verb, counter) = audit_verb!("https_listener_added");
            Some(AuditEntry {
                kind: EventKind::ListenerAdded,
                verb,
                counter,
                target: format!("listener:https:{}", listener.address),
                cluster_id: None,
                backend_id: None,
                address: Some(listener.address),
                extras: AuditExtras::default(),
            })
        }
        RequestType::AddTcpListener(listener) => {
            let (verb, counter) = audit_verb!("tcp_listener_added");
            Some(AuditEntry {
                kind: EventKind::ListenerAdded,
                verb,
                counter,
                target: format!("listener:tcp:{}", listener.address),
                cluster_id: None,
                backend_id: None,
                address: Some(listener.address),
                extras: AuditExtras::default(),
            })
        }
        RequestType::RemoveListener(remove) => {
            let (verb, counter) = audit_verb!("listener_removed");
            Some(AuditEntry {
                kind: EventKind::ListenerRemoved,
                verb,
                counter,
                target: format!("listener:{:?}:{}", remove.proxy(), remove.address),
                cluster_id: None,
                backend_id: None,
                address: Some(remove.address),
                extras: AuditExtras::default(),
            })
        }
        // AddBackend / RemoveBackend are intentionally not audited via this
        // taxonomy — they are already covered by the BACKEND_DOWN / BACKEND_UP
        // events emitted by the workers when traffic reaches them.
        _ => None,
    }
}

/// Bump the per-verb counter, queue the [Event] for fan-out to subscribed
/// clients, and write the structured audit log line at `info!` level. Used
/// by every control-plane mutation handler.
fn audit_emit(server: &mut Server, client: &ClientSession, entry: AuditEntry, result: AuditResult) {
    let request_id = Ulid::generate();
    // `entry.counter` is the pre-built `config.<verb>` static-str key
    // chosen at the construction site (paired with `verb` via
    // `audit_verb!`), so dashboards see one counter per verb without a
    // verb→key dispatch table.
    count!(entry.counter, 1);

    let rendered = audit_log_context!(server, client, &request_id, &entry, result);
    info!("{}", rendered);
    // Mirror to the dedicated tamper-resistant sink when configured
    // (`config.audit_logs_target`). Best-effort; failures fall back to the
    // standard `info!` route above. Strip ANSI escapes before writing so
    // the dedicated file stays ASCII and SIEM-parseable even when colour
    // is enabled on stdout.
    server.append_audit_line(&strip_ansi(&rendered));

    // JSON-structured sink (`config.audit_logs_json_target`) — one
    // self-contained record per line, ready for Wazuh / Elastic / Loki
    // ingest without bespoke parser code.
    if server.audit_log_json_writer.is_some() {
        let json = audit_record_to_json(server, client, &request_id, &entry, result);
        server.append_audit_json(&json);
    }

    // Subscribers only see the proto Event; the verb / actor / request_id are
    // captured in the audit log line above (which is already structured).
    server.push_audit_event(Event {
        kind: entry.kind as i32,
        cluster_id: entry.cluster_id,
        backend_id: entry.backend_id,
        address: entry.address,
        // Master-emitted audit events do not carry the
        // `metric_detail` transition; that field is populated by
        // workers via `WorkerResponse::Event` on lease-tick / worker-
        // arm transitions only. See `command.proto`'s `Event` and
        // `MetricDetailTransition` comments.
        metric_detail: None,
    });
}

/// Audit a worker-local `METRIC_DETAIL_CHANGED` transition. Workers emit
/// these via the `Event` channel when the polled lease janitor retires a
/// lease, or when the worker arm of `SetMetricDetail` applies / clears a
/// lease. The master folds them into the same audit log used for
/// operator-initiated transitions so SOC tooling sees a complete picture
/// of cardinality changes regardless of origin.
///
/// Distinct from [`audit_emit`] / [`audit_emit_inline`] because there is
/// no `ClientSession` behind the event: the actor is the worker itself.
/// The line uses a `worker_id=<id>` field in place of the
/// `actor_uid` / `actor_pid` / `client_id` block; everything else
/// (timestamps, sozu_version, fan-out to subscribers) matches the
/// canonical envelope. Both the text sink and the JSON sink receive a
/// record so SIEM ingest stays unified.
pub fn audit_worker_metric_detail_transition(
    server: &mut Server,
    worker_id: crate::command::server::WorkerId,
    transition: &sozu_command_lib::proto::command::MetricDetailTransition,
) {
    use sozu_command_lib::proto::command::MetricDetail;

    let (verb, counter) = audit_verb!("metric_detail_changed_worker_local");
    count!(counter, 1);

    let prev_label = MetricDetail::try_from(transition.previous_effective)
        .map(|m| format!("{m:?}"))
        .unwrap_or_else(|_| "<invalid>".into());
    let eff_label = MetricDetail::try_from(transition.effective)
        .map(|m| format!("{m:?}"))
        .unwrap_or_else(|_| "<invalid>".into());
    let kind_sanitized = sanitize_for_audit_kv(&transition.transition_kind);
    let target = format!("metric_detail:{prev_label}->{eff_label}");
    let now_ts = rfc3339_utc(std::time::SystemTime::now());

    // Truncate the optional lease client_id with the same cap used in the
    // operator-initiated audit line so SIEM consumers see a consistent
    // upper bound.
    let lease_id = transition.client_id.as_deref().map(|c| {
        let sanitized = sanitize_for_audit_kv(c);
        let truncated: String = sanitized.chars().take(AUDIT_LEASE_ID_MAX_CHARS).collect();
        truncated
    });

    // Render the text-sink line. Match the operator-initiated envelope's
    // KV shape so a SOC analyst can correlate worker-local and operator
    // lines with a single regex. `worker_id` stands in for the
    // `client_id=<connection_id>` block since the worker is its own
    // actor.
    let mut text = format!(
        "[worker:{worker_id} request:- cluster:- backend:-]\tAUDIT\tCommand(ts={now_ts}, verb={verb}, \
         actor_uid=-, actor_gid=-, actor_pid=-, actor_role=worker, actor_user=sozu-worker, \
         actor_comm=sozu-worker, worker_id={worker_id}, socket=(worker-ipc), \
         target={target}, result=ok, transition_kind={kind_sanitized}",
    );
    if let Some(id) = lease_id.as_deref() {
        text.push_str(&format!(", lease_id={id}"));
    }
    text.push_str(&format!(
        ", sozu_version={SOZU_VERSION}, build_git_sha={SOZU_BUILD_GIT_SHA}, boot_generation={})",
        server.boot_generation,
    ));
    info!("{}", text);
    server.append_audit_line(&text);

    if server.audit_log_json_writer.is_some() {
        let mut record = serde_json::Map::new();
        record.insert("ts".to_owned(), serde_json::Value::String(now_ts.clone()));
        record.insert(
            "boot_generation".to_owned(),
            serde_json::json!(server.boot_generation),
        );
        record.insert(
            "verb".to_owned(),
            serde_json::Value::String(verb.to_owned()),
        );
        record.insert(
            "worker_id".to_owned(),
            serde_json::json!(worker_id.to_string()),
        );
        record.insert(
            "actor".to_owned(),
            serde_json::json!({
                "role": "worker",
                "comm": "sozu-worker",
            }),
        );
        record.insert(
            "target".to_owned(),
            serde_json::Value::String(target.clone()),
        );
        record.insert(
            "result".to_owned(),
            serde_json::Value::String("ok".to_owned()),
        );
        record.insert(
            "transition_kind".to_owned(),
            serde_json::Value::String(kind_sanitized.clone()),
        );
        record.insert(
            "previous_effective".to_owned(),
            serde_json::Value::String(prev_label.clone()),
        );
        record.insert(
            "effective".to_owned(),
            serde_json::Value::String(eff_label.clone()),
        );
        if let Some(id) = lease_id {
            record.insert("lease_id".to_owned(), serde_json::Value::String(id));
        }
        record.insert(
            "sozu_version".to_owned(),
            serde_json::Value::String(SOZU_VERSION.to_owned()),
        );
        record.insert(
            "build_git_sha".to_owned(),
            serde_json::Value::String(SOZU_BUILD_GIT_SHA.to_owned()),
        );
        server.append_audit_json(&serde_json::Value::Object(record).to_string());
    }
}

/// Build a single-line JSON record mirroring the audit line. Schema is
/// stable: every key always present, missing values rendered as JSON
/// `null`. Used by the dedicated JSON sink (`audit_logs_json_target`).
///
/// Schema sketch:
/// ```json
/// {
///   "ts": "<RFC3339 UTC>",
///   "boot_generation": <u32>,
///   "session_ulid": "...",
///   "request_ulid": "...",
///   "actor": {"uid": ..., "gid": ..., "pid": ..., "user": "...", "comm": "...", "role": "..."},
///   "client_id": ...,
///   "connect_ts": "<RFC3339 UTC>",
///   "socket": "...",
///   "verb": "...",
///   "target": "...",
///   "result": "ok|err",
///   "cluster_id": "..." or null,
///   "backend_id": "..." or null,
///   "extras": {...}
/// }
/// ```
fn audit_record_to_json(
    server: &Server,
    client: &ClientSession,
    request_id: &Ulid,
    entry: &AuditEntry,
    result: AuditResult,
) -> String {
    use serde_json::{Value, json};
    let extras = {
        let mut map = serde_json::Map::new();
        if let Some(code) = entry.extras.error_code {
            map.insert("error_code".to_owned(), Value::String(code.to_string()));
        }
        if let Some(reason) = entry.extras.reason.as_deref() {
            // INFO-1: every untrusted free-form field that ships to the
            // JSON sink runs through `sanitize_for_audit` to match the
            // text-sink contract. `serde_json` would JSON-escape control
            // bytes correctly, but a SIEM that re-emits JSON to TSV/CSV
            // can resurrect literal `\t` / `\n` and a SOC analyst
            // grepping the flat egress would see forged columns.
            map.insert(
                "reason".to_owned(),
                Value::String(sanitize_for_audit(reason)),
            );
        }
        if let Some(elapsed) = entry.extras.elapsed_ms {
            map.insert("elapsed_ms".to_owned(), json!(elapsed));
        }
        if let Some(fanout) = entry.extras.fanout {
            map.insert(
                "fanout".to_owned(),
                json!({
                    "status": fanout.status.to_string(),
                    "workers_ok": fanout.workers_ok,
                    "workers_err": fanout.workers_err,
                    "workers_expected": fanout.workers_expected,
                }),
            );
        }
        if let Some(hash) = entry.extras.request_sha256.as_deref() {
            map.insert("request_sha256".to_owned(), Value::String(hash.to_owned()));
        }
        if let Some(lease_id) = entry.extras.metric_detail_lease_id.as_deref() {
            // Operator-controlled. Sanitise with the strict KV helper and
            // truncate so JSON consumers that re-emit flat (TSV/CSV) cannot
            // forge an adjacent column.
            let sanitized = sanitize_for_audit_kv(lease_id);
            let truncated: String = sanitized.chars().take(AUDIT_LEASE_ID_MAX_CHARS).collect();
            map.insert("lease_id".to_owned(), Value::String(truncated));
        }
        if let Some(detail_reason) = entry.extras.metric_detail_reason.as_deref() {
            let sanitized = sanitize_for_audit_kv(detail_reason);
            let truncated: String = sanitized.chars().take(AUDIT_REASON_MAX_CHARS).collect();
            map.insert("metric_detail_reason".to_owned(), Value::String(truncated));
        }
        Value::Object(map)
    };
    // INFO-1: free-form attacker-influenced fields go through
    // `sanitize_for_audit` here even though `serde_json` would already
    // escape control bytes — defense in depth against SIEM pipelines
    // that decode JSON and re-emit flat (TSV/CSV/syslog), which
    // resurrects the literal control byte and re-opens the column-
    // smuggling primitive the text sink already defends against.
    let actor_user_sanitized = client.actor_user.as_deref().map(sanitize_for_audit);
    let actor_comm_sanitized = client.actor_comm.as_deref().map(sanitize_for_audit);
    let socket_sanitized = sanitize_for_audit(client.socket_path.as_ref());
    let target_sanitized = sanitize_for_audit(&entry.target);
    let verb_sanitized = sanitize_for_audit(entry.verb);
    let record = json!({
        "ts": rfc3339_utc(std::time::SystemTime::now()),
        "boot_generation": server.boot_generation,
        "session_ulid": client.session_ulid.to_string(),
        "request_ulid": request_id.to_string(),
        "actor": {
            "uid": client.actor_uid,
            "gid": client.actor_gid,
            "pid": client.actor_pid,
            "user": actor_user_sanitized,
            "comm": actor_comm_sanitized,
            "role": actor_role(client.actor_uid),
        },
        "client_id": client.id,
        "connect_ts": client.connect_ts_display(),
        "socket": socket_sanitized,
        "verb": verb_sanitized,
        "target": target_sanitized,
        "result": result.to_string(),
        "cluster_id": entry.cluster_id,
        "backend_id": entry.backend_id,
        "sozu_version": SOZU_VERSION,
        "build_git_sha": SOZU_BUILD_GIT_SHA,
        "extras": extras,
    });
    record.to_string()
}

/// Strip ANSI CSI escape sequences from `s`. Cheap single-pass parser —
/// recognises `\x1b[ ... m` (colour) and any other `\x1b[ ... <final>`
/// sequence. Returns `s.to_owned()` when no ESC byte is found so the common
/// no-colour path doesn't reallocate.
fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // Swallow the `[`...final-byte CSI, or drop the lone ESC.
        if let Some('[') = chars.next() {
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        }
    }
    out
}

/// Same as [audit_emit] but synthesises the [AuditEntry] inline for verbs
/// whose request payload does not carry the relevant identifiers (e.g.
/// `Logging`, `ConfigureMetrics`, `ReloadConfiguration`). Caller MUST pair
/// `verb` and `counter` via `audit_verb!` so the two strings cannot drift.
#[allow(clippy::too_many_arguments)]
pub(crate) fn audit_emit_inline(
    server: &mut Server,
    client: &ClientSession,
    kind: EventKind,
    verb: &'static str,
    counter: &'static str,
    target: String,
    result: AuditResult,
    extras: AuditExtras,
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
            extras,
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
    /// Wall-clock reference captured at `worker_request` entry. Used by
    /// [`WorkerTask::on_finish`] to compute `elapsed_ms` for the
    /// completion-time audit emission.
    started_at: Instant,
    /// Snapshot of the audit entry built from the request. Carried through
    /// the task so the completion-time audit line can attribute the verb
    /// and target. `None` for non-audited verbs (same filter as
    /// `audit_entry_for`).
    audit: Option<AuditEntry>,
    /// Inline-audit target for verbs whose audit factory does not produce
    /// a full `AuditEntry` (`ConfigureMetrics`, `SetMetricDetail`). The
    /// completion handler emits a second `audit_emit_inline` line with
    /// fan-out outcome attached. `None` for verbs that already carry an
    /// `AuditEntry` in `audit`.
    inline_audit: Option<InlineAuditTarget>,
    /// Operator-controlled SetMetricDetail audit fields (lease_id,
    /// reason) folded into the completion-time `AuditExtras` so the
    /// post-fanout audit row also carries the operator-supplied lease key
    /// and human note in their dedicated columns rather than smuggling
    /// them through `target`. `None` for any verb that is not
    /// `SetMetricDetail`.
    metric_detail_audit: Option<MetricDetailAuditFields>,
}

/// Carry the per-verb metadata needed to emit a completion-time audit
/// row for verbs that don't go through `audit_entry_for`. Mirrors the
/// shape `audit_emit_inline` expects (both `verb` and `counter` are
/// `&'static str` produced together by the `audit_verb!` macro so they
/// can never drift).
#[derive(Debug)]
struct InlineAuditTarget {
    kind: EventKind,
    verb: &'static str,
    counter: &'static str,
    target: String,
}

/// Captured audit fields for `SetMetricDetail` whose operator-controlled
/// values flow into dedicated audit extras (NOT into `target`) so that
/// `:` / `=` / `,` smuggled by an attacker cannot forge an adjacent
/// audit column. `target` itself is kept master-controlled
/// (`metric_detail:<level>` only).
#[derive(Debug, Clone)]
struct MetricDetailAuditFields {
    /// `metric_detail:<level>` — fully master-controlled (level is an enum).
    target: String,
    /// Operator-supplied `SetMetricDetail.client_id`. Sanitised at render
    /// time via [`sanitize_for_audit_kv`] and truncated to
    /// [`AUDIT_LEASE_ID_MAX_CHARS`].
    lease_id: String,
    /// Operator-supplied `SetMetricDetail.reason` (free-form human note).
    /// Sanitised + truncated at render time.
    reason: Option<String>,
}

impl MetricDetailAuditFields {
    /// Build an `AuditExtras` skeleton carrying the operator fields. The
    /// caller layers `elapsed_ms` / `error_code` / `reason` (failure
    /// reason — distinct from `metric_detail_reason`) on top as needed.
    fn into_extras(self) -> AuditExtras {
        AuditExtras {
            metric_detail_lease_id: Some(self.lease_id),
            metric_detail_reason: self.reason,
            ..Default::default()
        }
    }
}

pub fn worker_request(
    server: &mut Server,
    client: &mut ClientSession,
    mut request_content: RequestType,
) {
    // Master-only enrichment: populate `SetMetricDetail`'s peer binding
    // from the connecting `ClientSession` so the worker can authorise
    // subsequent `clear` requests against the apply-time owner. Clients
    // never set these fields themselves — see the proto comment on
    // `SetMetricDetail.peer_pid` / `peer_session_ulid` for the trust
    // model. A `None` actor_pid (non-Linux build, missing SO_PEERCRED)
    // degrades to "binding unknown" on the worker side, which accepts
    // any clear for backward compat.
    if let RequestType::SetMetricDetail(req) = &mut request_content {
        req.peer_pid = client.actor_pid;
        req.peer_session_ulid = Some(client.session_ulid.to_string());
        // Master-side pre-validation: reject obviously bogus inputs
        // BEFORE fan-out so a malicious or buggy caller cannot fan its
        // mistake across every worker (N rejected fan-outs + N audit
        // lines per request). The worker dispatch path still enforces
        // these limits as defence-in-depth, but failing fast here saves
        // the audit-noise amplifier and gives the operator a single
        // clear error rather than N.
        if req.client_id.len() > sozu_lib::metrics::LEASE_CLIENT_ID_MAX_BYTES {
            client.finish_failure(format!(
                "SetMetricDetail: client_id length {} exceeds {} bytes",
                req.client_id.len(),
                sozu_lib::metrics::LEASE_CLIENT_ID_MAX_BYTES,
            ));
            return;
        }
        if let Some(t) = req.ttl_seconds
            && u64::from(t) > sozu_lib::metrics::LEASE_TTL_MAX.as_secs()
        {
            client.finish_failure(format!(
                "SetMetricDetail: ttl_seconds={t} exceeds LEASE_TTL_MAX={}",
                sozu_lib::metrics::LEASE_TTL_MAX.as_secs(),
            ));
            return;
        }
    }
    // Snapshot the audit entry before consuming `request_content` so we can
    // emit even when `state.dispatch` rejects the request AND so the
    // completion handler can re-emit with fanout + elapsed_ms.
    let audit = audit_entry_for(&request_content, &server.state);
    let started_at = Instant::now();

    // Special-case ConfigureMetrics — the proto payload is an i32 enum (no
    // dedicated message), so we synthesise the audit entry inline. The
    // resolved enum value is reused below to clear the main-process METRICS
    // aggregator on `MetricsConfiguration::Clear` (workers see the clear
    // through the scatter; without this the master's own `main_metrics`
    // returned by `dump_local_proxy_metrics` would survive the operator
    // clear and `sozu metrics` would still report stale values).
    let metrics_configuration = if let RequestType::ConfigureMetrics(value) = &request_content {
        Some(MetricsConfiguration::try_from(*value).unwrap_or(MetricsConfiguration::Disabled))
    } else {
        None
    };
    let metrics_target = metrics_configuration
        .as_ref()
        .map(|cfg| format!("metrics:{cfg:?}"));

    // Special-case SetMetricDetail — the same shape as ConfigureMetrics above
    // (no dedicated audit factory in `audit_entry_for`), so we synthesise the
    // entry inline against the new `EventKind::MetricDetailChanged` variant.
    //
    // The `target` field captures the level only (`metric_detail:Backend` /
    // `metric_detail:clear`); the operator-supplied `client_id` (lease key)
    // and free-form `reason` flow into dedicated audit extras
    // (`metric_detail_lease_id`, `metric_detail_reason`) so attacker-supplied
    // `:` / `=` / `,` cannot smuggle a forged column into the audit log.
    let metric_detail_audit = if let RequestType::SetMetricDetail(req) = &request_content {
        let level = if req.clear.unwrap_or(false) {
            "clear".to_owned()
        } else {
            req.detail
                .and_then(|d| MetricDetail::try_from(d).ok())
                .map(|d| format!("{d:?}"))
                .unwrap_or_else(|| "<invalid>".into())
        };
        Some(MetricDetailAuditFields {
            target: format!("metric_detail:{level}"),
            lease_id: req.client_id.clone(),
            reason: req.reason.clone().filter(|s| !s.is_empty()),
        })
    } else {
        None
    };

    let request: sozu_command_lib::proto::command::Request = request_content.into();
    let request_sha256 = compute_request_sha256(&request);

    if let Err(error) = server.state.dispatch(&request) {
        let reason = error.to_string();
        if let Some(mut entry) = audit {
            entry.extras.error_code = Some(AuditErrorCode::DispatchError);
            entry.extras.reason = Some(reason.clone());
            entry.extras.elapsed_ms = Some(elapsed_ms(started_at));
            entry.extras.request_sha256 = Some(request_sha256.clone());
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
                AuditExtras {
                    elapsed_ms: Some(elapsed_ms(started_at)),
                    error_code: Some(AuditErrorCode::DispatchError),
                    reason: Some(reason.clone()),
                    ..Default::default()
                },
            );
        } else if let Some(fields) = metric_detail_audit.clone() {
            let (verb, counter) = audit_verb!("metric_detail_changed");
            let target = fields.target.clone();
            let mut extras = fields.into_extras();
            extras.elapsed_ms = Some(elapsed_ms(started_at));
            extras.error_code = Some(AuditErrorCode::DispatchError);
            extras.reason = Some(reason.clone());
            audit_emit_inline(
                server,
                client,
                EventKind::MetricDetailChanged,
                verb,
                counter,
                target,
                AuditResult::Err,
                extras,
            );
        }
        client.finish_failure(format!(
            "could not dispatch request on the main process state: {error}",
        ));
        return;
    }

    // Attempt-time audit — `result=ok` here only means "accepted by the
    // main process state". The completion-time line (emitted from
    // `WorkerTask::on_finish`) carries the fanout outcome.
    let audit_for_task = audit.as_ref().map(|entry| {
        let mut cloned = clone_entry(entry);
        cloned.extras.request_sha256 = Some(request_sha256.clone());
        cloned
    });
    // Stash an inline-audit target for the completion handler when the
    // verb doesn't carry a full `AuditEntry`. The attempt-time line below
    // emits with `AuditResult::Ok` (state.dispatch accepted); on_finish
    // re-emits with the worker fan-out outcome.
    let inline_audit = if let Some(target) = metrics_target.as_ref() {
        let (verb, counter) = audit_verb!("metrics_configured");
        Some(InlineAuditTarget {
            kind: EventKind::MetricsConfigured,
            verb,
            counter,
            target: target.clone(),
        })
    } else {
        metric_detail_audit.as_ref().map(|fields| {
            let (verb, counter) = audit_verb!("metric_detail_changed");
            InlineAuditTarget {
                kind: EventKind::MetricDetailChanged,
                verb,
                counter,
                target: fields.target.clone(),
            }
        })
    };
    // Operator-controlled SetMetricDetail fields (lease_id + reason) we
    // need to thread into both the attempt-time Ok line below and the
    // completion-time line in `on_finish`. Cloning once keeps the
    // emission sites symmetric without re-deriving from `request_content`
    // (already moved into `request: sozu_command_lib...::Request` above).
    let metric_detail_audit_completion = metric_detail_audit.clone();

    if let Some(mut entry) = audit {
        entry.extras.request_sha256 = Some(request_sha256);
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
            AuditExtras::default(),
        );
    } else if let Some(fields) = metric_detail_audit {
        let (verb, counter) = audit_verb!("metric_detail_changed");
        let target = fields.target.clone();
        let extras = fields.into_extras();
        audit_emit_inline(
            server,
            client,
            EventKind::MetricDetailChanged,
            verb,
            counter,
            target,
            AuditResult::Ok,
            extras,
        );
    }

    // Master-side clear: the main process keeps its own `main_metrics`
    // aggregator (read by `dump_local_proxy_metrics` at this file's
    // metrics-query handler) that is NOT reached by the worker scatter.
    // Apply the clear locally before fanning out so master and workers
    // are wiped consistently.
    if metrics_configuration == Some(MetricsConfiguration::Clear) {
        METRICS.with(|metrics| {
            (*metrics.borrow_mut()).clear_local();
        });
    }

    client.return_processing("Processing worker request...");

    server.scatter(
        request,
        Box::new(WorkerTask {
            client_token: client.token,
            gatherer: DefaultGatherer::default(),
            started_at,
            audit: audit_for_task,
            inline_audit,
            metric_detail_audit: metric_detail_audit_completion,
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
                    // Worker error strings are partially operator-
                    // influenced (request-derived fields, IPC payloads).
                    // Run them through the column-boundary-aware
                    // sanitiser before joining into `extras.reason` so
                    // a `,` / `=` inside one worker's message cannot
                    // forge an additional audit-row column when a SIEM
                    // splits on `, ` / `=`. The strict variant also
                    // strips the bidi class so a Trojan-Source-flavoured
                    // payload cannot visually reorder the reason field.
                    messages.push(format!(
                        "{worker_id}: {}",
                        sanitize_for_audit_kv(&response.message)
                    ))
                }
            }
        }

        let errors = self.gatherer.errors;
        let ok = self.gatherer.ok;
        let expected = self.gatherer.expected_responses;
        let result = if errors > 0 || timed_out {
            AuditResult::Err
        } else {
            AuditResult::Ok
        };

        let fanout_status = if timed_out {
            FanoutStatus::Timeout
        } else if errors > 0 {
            FanoutStatus::Partial
        } else if expected == 0 {
            FanoutStatus::LocalOnly
        } else {
            FanoutStatus::Ok
        };
        let fanout_summary = FanoutSummary {
            status: fanout_status,
            workers_ok: u32::try_from(ok).unwrap_or(u32::MAX),
            workers_err: u32::try_from(errors).unwrap_or(u32::MAX),
            workers_expected: u32::try_from(expected).unwrap_or(u32::MAX),
        };

        // Completion-time audit: attributes the same verb as the attempt-time
        // line but with fanout / worker counts / elapsed_ms filled in. Skip
        // when the client disconnected or the verb is not audited.
        if let (Some(client_ref), Some(mut entry)) = (client.as_deref(), self.audit) {
            entry.extras.elapsed_ms = Some(elapsed_ms(self.started_at));
            entry.extras.fanout = Some(fanout_summary);
            if matches!(result, AuditResult::Err) {
                entry.extras.error_code = Some(if timed_out {
                    AuditErrorCode::WorkerTimeout
                } else {
                    AuditErrorCode::WorkerFailure
                });
                entry.extras.reason = Some(messages.join(", "));
            }
            audit_emit(server, client_ref, entry, result);
        } else if let (Some(client_ref), Some(inline)) = (client.as_deref(), self.inline_audit) {
            // Completion-time inline audit for ConfigureMetrics +
            // SetMetricDetail. Same shape as the entry-bearing arm above
            // but routed through `audit_emit_inline` since these verbs
            // don't synthesise a full `AuditEntry` at attempt time. The
            // operator-supplied `SetMetricDetail` lease_id / reason live
            // in their own audit columns (see `MetricDetailAuditFields`);
            // pre-fill them when present, then layer the completion
            // metadata on top.
            let mut extras = self
                .metric_detail_audit
                .map(MetricDetailAuditFields::into_extras)
                .unwrap_or_default();
            extras.elapsed_ms = Some(elapsed_ms(self.started_at));
            extras.fanout = Some(fanout_summary);
            if matches!(result, AuditResult::Err) {
                extras.error_code = Some(if timed_out {
                    AuditErrorCode::WorkerTimeout
                } else {
                    AuditErrorCode::WorkerFailure
                });
                extras.reason = Some(messages.join(", "));
            }
            audit_emit_inline(
                server,
                client_ref,
                inline.kind,
                inline.verb,
                inline.counter,
                inline.target,
                result,
                extras,
            );
        }

        if errors > 0 || timed_out {
            client.finish_failure(messages.join(", "));
        } else {
            client.finish_ok("Successfully applied request to all workers");
        }

        server.update_counts();
    }
}

/// Elapsed milliseconds since `started_at`, saturating on overflow.
fn elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// SHA-256 of the proto `Request` wire-encoding, hex-truncated to 16 chars
/// (64 bits) so the audit line stays greppable without blowing up line
/// width. Cheap: single prost `encode_to_vec` + one `Sha256::digest` on
/// a control-plane path (one call per worker request, not per packet).
fn compute_request_sha256(request: &sozu_command_lib::proto::command::Request) -> String {
    let bytes = request.encode_to_vec();
    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// SHA-256 fingerprint of a PEM certificate, hex-encoded and truncated to
/// 16 hex chars (64 bits) for audit brevity. `None` when the input is not
/// parseable as PEM. Mirrors the full fingerprint workflow in
/// `sozu_command_lib::certificate::calculate_fingerprint` but truncates
/// for log terseness — operators correlate via the 64-bit prefix.
fn compute_certificate_fingerprint(certificate_pem: &[u8]) -> Option<String> {
    let fp = sozu_command_lib::certificate::calculate_fingerprint(certificate_pem).ok()?;
    let mut hex = String::with_capacity(16);
    for byte in fp.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Some(hex)
}

/// Shallow clone of [`AuditEntry`] so the completion handler can re-emit a
/// second line with the same taxonomy as the attempt-time line but enriched
/// with fanout + elapsed_ms. Manual implementation because `AuditEntry`
/// does not derive `Clone` by default (it owns `String`s that the
/// attempt-time line consumes by value).
fn clone_entry(entry: &AuditEntry) -> AuditEntry {
    AuditEntry {
        kind: entry.kind,
        verb: entry.verb,
        counter: entry.counter,
        cluster_id: entry.cluster_id.clone(),
        backend_id: entry.backend_id.clone(),
        address: entry.address,
        target: entry.target.clone(),
        extras: entry.extras.clone(),
    }
}

// =========================================================
// SetMetricDetail — dedicated dispatcher.
//
// Performs master-side length / TTL pre-validation that mirrors
// `worker_request`, populates the peer binding from the connecting
// `ClientSession`, emits the attempt-time audit row, and fans the
// request out to every worker via the standard scatter path. Workers
// that pre-date this verb return `WorkerResponse::error("unknown
// request type")` which folds into the standard fan-out error tally
// (`extras.fanout.workers_err`); operators see "succeeded with errors"
// rather than a dedicated capability-skip list. Production keeps
// master + workers in sync via `UpgradeMain`, so the mixed-version
// state is transient.

/// Gathers per-worker `SetMetricDetail` responses, synthesises a
/// `MetricDetailStatus` reply for the client, and audits the
/// completion alongside operator-initiated transitions. Wraps the
/// generic worker-task fields (`audit`, `inline_audit`,
/// `metric_detail_audit`) so the existing audit pipeline keeps
/// emitting the same shape it does for any other audited verb.
#[derive(Debug)]
struct SetMetricDetailTask {
    pub client_token: Token,
    pub gatherer: DefaultGatherer,
    started_at: Instant,
    /// Master-side `(configured, effective_before)` captured pre-apply
    /// so the response can carry the `previous_effective` field that
    /// `MetricDetailStatus` advertises. The master also runs an
    /// `Aggregator`; its `effective` participates in operator-visible
    /// cardinality alongside per-worker leases.
    master_configured: MetricDetail,
    master_previous_effective: MetricDetail,
    /// Completion-time inline-audit target so the post-fanout audit
    /// row carries the same `target` / verb shape as the attempt-time
    /// line. Cloned from the `inline_audit` slot the generic
    /// `worker_request` path uses.
    inline_audit: InlineAuditTarget,
    /// Operator-controlled audit fields (lease_id + reason) carried
    /// into the completion-time `AuditExtras` so the post-fan-out
    /// audit row also surfaces the lease key + free-form note in
    /// their dedicated columns.
    metric_detail_audit: MetricDetailAuditFields,
}

/// Dispatch a `SetMetricDetail` request. Performs the same length /
/// TTL pre-validation that `worker_request` does, populates the peer
/// binding from the connecting `ClientSession`, emits the attempt-time
/// audit row, and fans out unconditionally to every worker through the
/// standard scatter path.
pub fn set_metric_detail_request(
    server: &mut Server,
    client: &mut ClientSession,
    mut req: SetMetricDetail,
) {
    // Master-side enrichment + pre-validation (mirrors `worker_request`).
    req.peer_pid = client.actor_pid;
    req.peer_session_ulid = Some(client.session_ulid.to_string());
    if req.client_id.len() > sozu_lib::metrics::LEASE_CLIENT_ID_MAX_BYTES {
        client.finish_failure(format!(
            "SetMetricDetail: client_id length {} exceeds {} bytes",
            req.client_id.len(),
            sozu_lib::metrics::LEASE_CLIENT_ID_MAX_BYTES,
        ));
        return;
    }
    if let Some(t) = req.ttl_seconds
        && u64::from(t) > sozu_lib::metrics::LEASE_TTL_MAX.as_secs()
    {
        client.finish_failure(format!(
            "SetMetricDetail: ttl_seconds={t} exceeds LEASE_TTL_MAX={}",
            sozu_lib::metrics::LEASE_TTL_MAX.as_secs(),
        ));
        return;
    }

    // Capture master-side cardinality view BEFORE we touch anything.
    let (master_configured, master_previous_effective) = METRICS.with(|m| {
        let m = m.borrow();
        (
            MetricDetail::from(m.detail_configured()),
            MetricDetail::from(m.detail_effective()),
        )
    });

    // Build the audit-field skeleton used by both the attempt-time and
    // completion-time emissions. Mirrors `worker_request`.
    let level_label = if req.clear.unwrap_or(false) {
        "clear".to_owned()
    } else {
        req.detail
            .and_then(|d| MetricDetail::try_from(d).ok())
            .map(|d| format!("{d:?}"))
            .unwrap_or_else(|| "<invalid>".into())
    };
    let metric_detail_audit = MetricDetailAuditFields {
        target: format!("metric_detail:{level_label}"),
        lease_id: req.client_id.clone(),
        reason: req.reason.clone().filter(|s| !s.is_empty()),
    };

    let started_at = Instant::now();
    let request: Request = RequestType::SetMetricDetail(req).into();

    // Attempt-time dispatch gate (mirrors `state.dispatch` in worker_request).
    if let Err(error) = server.state.dispatch(&request) {
        let reason = error.to_string();
        let (verb, counter) = audit_verb!("metric_detail_changed");
        let target = metric_detail_audit.target.clone();
        let mut extras = metric_detail_audit.into_extras();
        extras.elapsed_ms = Some(elapsed_ms(started_at));
        extras.error_code = Some(AuditErrorCode::DispatchError);
        extras.reason = Some(reason.clone());
        audit_emit_inline(
            server,
            client,
            EventKind::MetricDetailChanged,
            verb,
            counter,
            target,
            AuditResult::Err,
            extras,
        );
        client.finish_failure(format!(
            "could not dispatch request on the main process state: {error}",
        ));
        return;
    }

    // Attempt-time audit Ok.
    let (verb, counter) = audit_verb!("metric_detail_changed");
    {
        let target = metric_detail_audit.target.clone();
        let extras = metric_detail_audit.clone().into_extras();
        audit_emit_inline(
            server,
            client,
            EventKind::MetricDetailChanged,
            verb,
            counter,
            target,
            AuditResult::Ok,
            extras,
        );
    }

    client.return_processing("Processing SetMetricDetail...");

    let inline_audit = InlineAuditTarget {
        kind: EventKind::MetricDetailChanged,
        verb,
        counter,
        target: metric_detail_audit.target.clone(),
    };

    // Fan out unconditionally to every worker through the standard
    // scatter path. Workers that pre-date `SetMetricDetail` reply with
    // `WorkerResponse::error("unknown request type")` which folds into
    // the standard fan-out error tally; `on_finish` surfaces them via
    // the existing fanout summary rather than a dedicated skip list.
    let task = Box::new(SetMetricDetailTask {
        client_token: client.token,
        gatherer: DefaultGatherer::default(),
        started_at,
        master_configured,
        master_previous_effective,
        inline_audit,
        metric_detail_audit,
    });
    server.scatter(request, task, Timeout::Default, None);
}

impl GatheringTask for SetMetricDetailTask {
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
        // Per-worker status: each worker now returns its own
        // `WorkerMetricDetailStatus` payload via
        // `ContentType::WorkerMetricDetailStatus` in the SetMetricDetail
        // ok-with-content response (lib/src/server.rs::notify). Pull
        // each worker's actual quartet — workers hold independent
        // `Aggregator`s, so the master's view is NOT a reliable
        // stand-in for the per-worker state. Workers that returned an
        // error (e.g. peer-binding refusal) get skipped; the operator
        // sees `MetricDetailStatus.workers` populated only for the
        // ACK'd subset.
        let mut workers_map = BTreeMap::new();
        for (worker_id, response) in &self.gatherer.responses {
            if !matches!(
                ResponseStatus::try_from(response.status),
                Ok(ResponseStatus::Ok)
            ) {
                continue;
            }
            if let Some(ResponseContent {
                content_type: Some(ContentType::WorkerMetricDetailStatus(status)),
            }) = response.content.as_ref()
            {
                // `WorkerMetricDetailStatus` is `Copy` (four `i32`s +
                // one `u32`); dereferencing avoids the `clippy::clone_on_copy`
                // lint that CI's `-D warnings` rejects.
                workers_map.insert(worker_id.to_string(), *status);
            }
        }

        let master_effective = METRICS.with(|m| MetricDetail::from(m.borrow().detail_effective()));
        let status = MetricDetailStatus {
            configured: self.master_configured as i32,
            effective: master_effective as i32,
            previous_effective: self.master_previous_effective as i32,
            workers: workers_map,
        };

        // Completion-time audit row. Same shape as the generic WorkerTask
        // completion path; reuses `metric_detail_audit` for the
        // `lease_id` / `metric_detail_reason` columns and folds the
        // fan-out summary on top.
        let errors = self.gatherer.errors;
        let ok = self.gatherer.ok;
        let expected = self.gatherer.expected_responses;
        let result = if errors > 0 || timed_out {
            AuditResult::Err
        } else {
            AuditResult::Ok
        };
        let fanout_status = if timed_out {
            FanoutStatus::Timeout
        } else if errors > 0 {
            FanoutStatus::Partial
        } else if expected == 0 {
            FanoutStatus::LocalOnly
        } else {
            FanoutStatus::Ok
        };
        let fanout_summary = FanoutSummary {
            status: fanout_status,
            workers_ok: u32::try_from(ok).unwrap_or(u32::MAX),
            workers_err: u32::try_from(errors).unwrap_or(u32::MAX),
            workers_expected: u32::try_from(expected).unwrap_or(u32::MAX),
        };
        if let Some(client_ref) = client.as_deref() {
            let mut extras = self.metric_detail_audit.into_extras();
            extras.elapsed_ms = Some(elapsed_ms(self.started_at));
            extras.fanout = Some(fanout_summary);
            if matches!(result, AuditResult::Err) {
                extras.error_code = Some(if timed_out {
                    AuditErrorCode::WorkerTimeout
                } else {
                    AuditErrorCode::WorkerFailure
                });
                let mut msgs = Vec::new();
                for (worker_id, response) in &self.gatherer.responses {
                    // Same column-boundary sanitisation as
                    // `WorkerTask::on_finish` above.
                    // `SetMetricDetail` is itself the
                    // operator-controlled verb most likely to be probed
                    // for SIEM column smuggling, so this site is the
                    // higher-leverage of the two reason-join paths.
                    msgs.push(format!(
                        "{worker_id}: {}",
                        sanitize_for_audit_kv(&response.message)
                    ));
                }
                extras.reason = Some(msgs.join(", "));
            }
            audit_emit_inline(
                server,
                client_ref,
                self.inline_audit.kind,
                self.inline_audit.verb,
                self.inline_audit.counter,
                self.inline_audit.target,
                result,
                extras,
            );
        }

        client.finish_ok_with_content(
            ContentType::MetricDetailStatus(status).into(),
            if errors > 0 || timed_out {
                "SetMetricDetail completed with worker errors"
            } else {
                "Successfully applied SetMetricDetail to all workers"
            },
        );
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

        // Always fold when the caller asked for merged data, regardless of
        // worker count. `merge_metrics` relocates each worker's `clusters`
        // and `proxying` into the top-level maps via `std::mem::take`; the
        // previous `> 1` guard left single-worker fleets with empty
        // top-level maps and stranded the per-worker data in `workers`,
        // which silently zeroed every CLI/TUI consumer that reads
        // `m.clusters` / `m.proxying`.
        if !self.options.workers {
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

    let audit_target = format!("file:{path}");

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if matches!(err.kind(), ErrorKind::NotFound) => {
            if let Some(client_ref) = client.as_deref() {
                let (verb, counter) = audit_verb!("state_loaded");
                audit_emit_inline(
                    server,
                    client_ref,
                    EventKind::StateLoaded,
                    verb,
                    counter,
                    audit_target.clone(),
                    AuditResult::Err,
                    AuditExtras {
                        error_code: Some(AuditErrorCode::IoError),
                        ..Default::default()
                    },
                );
            }
            client.finish_failure(format!("Cannot find file at path {path}"));
            return;
        }
        Err(error) => {
            if let Some(client_ref) = client.as_deref() {
                let (verb, counter) = audit_verb!("state_loaded");
                audit_emit_inline(
                    server,
                    client_ref,
                    EventKind::StateLoaded,
                    verb,
                    counter,
                    audit_target.clone(),
                    AuditResult::Err,
                    AuditExtras {
                        error_code: Some(AuditErrorCode::IoError),
                        ..Default::default()
                    },
                );
            }
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
            // Success audit is emitted from `LoadStateTask::on_finish` once
            // every worker has acknowledged — that's where we know the final
            // ok/err split.
        }
        Err(message) => {
            if let Some(client_ref) = client.as_deref() {
                let (verb, counter) = audit_verb!("state_loaded");
                audit_emit_inline(
                    server,
                    client_ref,
                    EventKind::StateLoaded,
                    verb,
                    counter,
                    audit_target,
                    AuditResult::Err,
                    AuditExtras {
                        error_code: Some(AuditErrorCode::IoError),
                        ..Default::default()
                    },
                );
            }
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
        let result = if errors == 0 {
            AuditResult::Ok
        } else {
            AuditResult::Err
        };
        if let Some(client_ref) = client.as_deref() {
            let (verb, counter) = audit_verb!("state_loaded");
            audit_emit_inline(
                server,
                client_ref,
                EventKind::StateLoaded,
                verb,
                counter,
                format!("file:{} ok:{ok} errors:{errors}", self.path),
                result,
                AuditExtras::default(),
            );
        }
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
    let (verb, counter) = audit_verb!("sozu_stop_requested");
    audit_emit_inline(
        server,
        client,
        EventKind::SozuStopRequested,
        verb,
        counter,
        format!("stop:{}", if hardness { "hard" } else { "soft" }),
        AuditResult::Ok,
        AuditExtras::default(),
    );

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
// Walk each Option field of the patch and, when a pre-patch listener
// snapshot is available, emit `field=old→new` so operators see both the
// prior and the replacement value. Falls back to `field=new` (no arrow)
// when no current listener is known (e.g. patch arriving before the
// listener is registered) so the audit line never swallows a change.
//
// Helpers live inline as macros instead of functions so `stringify!($field)`
// picks up each field name without runtime formatting.

fn format_patch_diff_http(
    p: &UpdateHttpListenerConfig,
    current: Option<&sozu_command_lib::proto::command::HttpListenerConfig>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    // Patch field is `Option<T>`, current field is `T` (required on the
    // stored listener): `to_string()` directly.
    macro_rules! diff_req_copy {
        ($field:ident) => {
            if let Some(v) = p.$field {
                let old = current
                    .map(|c| c.$field.to_string())
                    .unwrap_or_else(|| "?".to_owned());
                parts.push(format!("{}={old}→{v}", stringify!($field)));
            }
        };
    }
    macro_rules! diff_req_str {
        ($field:ident) => {
            if let Some(v) = p.$field.as_deref() {
                let old = current
                    .map(|c| c.$field.clone())
                    .unwrap_or_else(|| "?".to_owned());
                parts.push(format!("{}={old}→{v}", stringify!($field)));
            }
        };
    }
    // Patch field is `Option<T>`, current field is `Option<T>` (optional on
    // the stored listener): flatten current via `and_then`.
    macro_rules! diff_opt_copy {
        ($field:ident) => {
            if let Some(v) = p.$field {
                let old = current
                    .and_then(|c| c.$field)
                    .map(|o| o.to_string())
                    .unwrap_or_else(|| "?".to_owned());
                parts.push(format!("{}={old}→{v}", stringify!($field)));
            }
        };
    }
    macro_rules! diff_opt_str {
        ($field:ident) => {
            if let Some(v) = p.$field.as_deref() {
                let old = current
                    .and_then(|c| c.$field.clone())
                    .unwrap_or_else(|| "?".to_owned());
                parts.push(format!("{}={old}→{v}", stringify!($field)));
            }
        };
    }
    if let Some(v) = p.public_address.as_ref() {
        let old = current
            .and_then(|c| c.public_address)
            .map(|o| o.to_string())
            .unwrap_or_else(|| "?".to_owned());
        parts.push(format!("public_address={old}→{v}"));
    }
    diff_req_copy!(expect_proxy);
    diff_req_str!(sticky_name);
    diff_req_copy!(front_timeout);
    diff_req_copy!(back_timeout);
    diff_req_copy!(connect_timeout);
    diff_req_copy!(request_timeout);
    if p.http_answers.is_some() {
        parts.push("http_answers=<patched>".to_owned());
    }
    diff_opt_copy!(h2_max_rst_stream_per_window);
    diff_opt_copy!(h2_max_ping_per_window);
    diff_opt_copy!(h2_max_settings_per_window);
    diff_opt_copy!(h2_max_empty_data_per_window);
    diff_opt_copy!(h2_max_continuation_frames);
    diff_opt_copy!(h2_max_glitch_count);
    diff_opt_copy!(h2_initial_connection_window);
    diff_opt_copy!(h2_max_concurrent_streams);
    diff_opt_copy!(h2_stream_shrink_ratio);
    diff_opt_copy!(h2_max_rst_stream_lifetime);
    diff_opt_copy!(h2_max_rst_stream_abusive_lifetime);
    diff_opt_copy!(h2_max_rst_stream_emitted_lifetime);
    diff_opt_copy!(h2_max_header_list_size);
    diff_opt_copy!(h2_max_header_table_size);
    diff_opt_copy!(h2_stream_idle_timeout_seconds);
    diff_opt_copy!(h2_graceful_shutdown_deadline_seconds);
    diff_opt_copy!(h2_max_window_update_stream0_per_window);
    diff_opt_str!(sozu_id_header);
    if parts.is_empty() {
        "(no-op)".to_owned()
    } else {
        parts.join(" ")
    }
}

fn format_patch_diff_https(
    p: &UpdateHttpsListenerConfig,
    current: Option<&sozu_command_lib::proto::command::HttpsListenerConfig>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    macro_rules! diff_req_copy {
        ($field:ident) => {
            if let Some(v) = p.$field {
                let old = current
                    .map(|c| c.$field.to_string())
                    .unwrap_or_else(|| "?".to_owned());
                parts.push(format!("{}={old}→{v}", stringify!($field)));
            }
        };
    }
    macro_rules! diff_req_str {
        ($field:ident) => {
            if let Some(v) = p.$field.as_deref() {
                let old = current
                    .map(|c| c.$field.clone())
                    .unwrap_or_else(|| "?".to_owned());
                parts.push(format!("{}={old}→{v}", stringify!($field)));
            }
        };
    }
    macro_rules! diff_opt_copy {
        ($field:ident) => {
            if let Some(v) = p.$field {
                let old = current
                    .and_then(|c| c.$field)
                    .map(|o| o.to_string())
                    .unwrap_or_else(|| "?".to_owned());
                parts.push(format!("{}={old}→{v}", stringify!($field)));
            }
        };
    }
    macro_rules! diff_opt_str {
        ($field:ident) => {
            if let Some(v) = p.$field.as_deref() {
                let old = current
                    .and_then(|c| c.$field.clone())
                    .unwrap_or_else(|| "?".to_owned());
                parts.push(format!("{}={old}→{v}", stringify!($field)));
            }
        };
    }
    if let Some(v) = p.public_address.as_ref() {
        let old = current
            .and_then(|c| c.public_address)
            .map(|o| o.to_string())
            .unwrap_or_else(|| "?".to_owned());
        parts.push(format!("public_address={old}→{v}"));
    }
    diff_req_copy!(expect_proxy);
    diff_req_str!(sticky_name);
    diff_req_copy!(front_timeout);
    diff_req_copy!(back_timeout);
    diff_req_copy!(connect_timeout);
    diff_req_copy!(request_timeout);
    if p.http_answers.is_some() {
        parts.push("http_answers=<patched>".to_owned());
    }
    if let Some(ref alpn) = p.alpn_protocols {
        let old = current
            .map(|c| c.alpn_protocols.join(","))
            .unwrap_or_else(|| "?".to_owned());
        let new = if alpn.values.is_empty() {
            "<reset>".to_owned()
        } else {
            alpn.values.join(",")
        };
        parts.push(format!("alpn_protocols={old}→{new}"));
    }
    diff_opt_copy!(strict_sni_binding);
    diff_opt_copy!(disable_http11);
    diff_opt_copy!(h2_max_rst_stream_per_window);
    diff_opt_copy!(h2_max_ping_per_window);
    diff_opt_copy!(h2_max_settings_per_window);
    diff_opt_copy!(h2_max_empty_data_per_window);
    diff_opt_copy!(h2_max_continuation_frames);
    diff_opt_copy!(h2_max_glitch_count);
    diff_opt_copy!(h2_initial_connection_window);
    diff_opt_copy!(h2_max_concurrent_streams);
    diff_opt_copy!(h2_stream_shrink_ratio);
    diff_opt_copy!(h2_max_rst_stream_lifetime);
    diff_opt_copy!(h2_max_rst_stream_abusive_lifetime);
    diff_opt_copy!(h2_max_rst_stream_emitted_lifetime);
    diff_opt_copy!(h2_max_header_list_size);
    diff_opt_copy!(h2_max_header_table_size);
    diff_opt_copy!(h2_stream_idle_timeout_seconds);
    diff_opt_copy!(h2_graceful_shutdown_deadline_seconds);
    diff_opt_copy!(h2_max_window_update_stream0_per_window);
    diff_opt_str!(sozu_id_header);
    if parts.is_empty() {
        "(no-op)".to_owned()
    } else {
        parts.join(" ")
    }
}

fn format_patch_diff_tcp(
    p: &UpdateTcpListenerConfig,
    current: Option<&sozu_command_lib::proto::command::TcpListenerConfig>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    macro_rules! diff_req_copy {
        ($field:ident) => {
            if let Some(v) = p.$field {
                let old = current
                    .map(|c| c.$field.to_string())
                    .unwrap_or_else(|| "?".to_owned());
                parts.push(format!("{}={old}→{v}", stringify!($field)));
            }
        };
    }
    if let Some(v) = p.public_address.as_ref() {
        let old = current
            .and_then(|c| c.public_address)
            .map(|o| o.to_string())
            .unwrap_or_else(|| "?".to_owned());
        parts.push(format!("public_address={old}→{v}"));
    }
    diff_req_copy!(expect_proxy);
    diff_req_copy!(front_timeout);
    diff_req_copy!(back_timeout);
    diff_req_copy!(connect_timeout);
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
    use super::{
        AUDIT_LEASE_ID_MAX_CHARS, AUDIT_REASON_MAX_CHARS, AuditEntry, AuditErrorCode, AuditExtras,
        AuditResult, FanoutStatus, FanoutSummary, SOZU_BUILD_GIT_SHA, SOZU_VERSION, actor_role,
        rfc3339_utc,
    };
    use regex::Regex;
    use rusty_ulid::Ulid;
    use sozu_command_lib::proto::command::EventKind;
    use std::time::SystemTime;

    /// Minimal stand-in exposing only the `ClientSession` fields and methods
    /// that `audit_log_context!` reads. Avoids constructing the full
    /// `Channel<Response, Request>` that `ClientSession::new` requires.
    struct TestClient {
        session_ulid: Ulid,
        id: u32,
        actor_uid: Option<u32>,
        actor_gid: Option<u32>,
        actor_pid: Option<i32>,
        actor_comm: Option<String>,
        actor_user: Option<String>,
        socket_path: std::sync::Arc<str>,
        connect_ts: SystemTime,
    }

    /// Minimal stand-in for `Server` exposing only `boot_generation`, the
    /// only field the macro reads off of `$server`. Avoids the full Server
    /// (Poll, listener, workers map, …) ceremony.
    struct TestServer {
        boot_generation: u32,
    }

    impl TestClient {
        fn actor_uid_display(&self) -> String {
            self.actor_uid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        }
        fn actor_gid_display(&self) -> String {
            self.actor_gid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        }
        fn actor_pid_display(&self) -> String {
            self.actor_pid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        }
        fn actor_comm_display(&self) -> String {
            self.actor_comm
                .clone()
                .unwrap_or_else(|| "unknown".to_owned())
        }
        fn actor_user_display(&self) -> String {
            self.actor_user
                .clone()
                .unwrap_or_else(|| "unknown".to_owned())
        }
        fn connect_ts_display(&self) -> String {
            rfc3339_utc(self.connect_ts)
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
            extras: AuditExtras::default(),
        }
    }

    fn sample_client(uid: Option<u32>) -> TestClient {
        TestClient {
            session_ulid: Ulid::generate(),
            id: 42,
            actor_uid: uid,
            actor_gid: uid,
            actor_pid: uid.map(|v| v as i32),
            actor_comm: uid.map(|_| "sozu".to_owned()),
            actor_user: uid.map(|_| "florentin".to_owned()),
            socket_path: std::sync::Arc::from("/run/sozu/sock"),
            connect_ts: SystemTime::now(),
        }
    }

    fn sample_server(boot_generation: u32) -> TestServer {
        TestServer { boot_generation }
    }

    fn pattern() -> Regex {
        // Anchored; covers bracket + `AUDIT\tCommand(...)` + every mandatory
        // field in order. Optional fields (error_code, reason, elapsed_ms,
        // fanout, workers, request_sha256) may appear between `result=...`
        // and `sozu_version=...`. Crockford base32 ULIDs are 26 chars
        // over [0-9A-Z]. Timestamps are RFC 3339 UTC with microsecond
        // precision; build_git_sha is 12 hex chars or `unknown`.
        Regex::new(concat!(
            r"^\[[0-9A-Z]{26} [0-9A-Z]{26} (?:[A-Za-z0-9_:.-]+|-) (?:[A-Za-z0-9_:.-]+|-)\]",
            r"\tAUDIT\tCommand\(",
            r"ts=\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}Z, ",
            r"verb=[a-z_]+, ",
            r"actor_uid=(?:\d+|unknown), ",
            r"actor_gid=(?:\d+|unknown), ",
            r"actor_pid=(?:\d+|unknown), ",
            r"actor_role=(?:root|system|user|unknown), ",
            r"actor_user=\S+, ",
            r"actor_comm=\S+, ",
            r"client_id=\d+, ",
            r"connect_ts=\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}Z, ",
            r"socket=\S+, ",
            r"target=[^,]+, ",
            r"result=(?:ok|err)",
            // Optional extras block.
            r"(?:, (?:error_code=\S+|reason=[^,)]+|elapsed_ms=\d+|fanout=\S+|workers=\d+/\d+/\d+|request_sha256=[0-9a-f]+))*",
            r", sozu_version=[^,]+",
            r", build_git_sha=\S+",
            r", boot_generation=\d+",
            r"\)$",
        ))
        .expect("audit-format regex must compile")
    }

    #[test]
    fn layout_with_cluster_id_matches() {
        let server = sample_server(0);
        let client = sample_client(Some(1000));
        let request_id = Ulid::generate();
        let entry = sample_entry(Some("my_app"));
        let rendered = audit_log_context!(server, client, &request_id, &entry, AuditResult::Ok);
        assert!(
            pattern().is_match(&rendered),
            "rendered line did not match audit-log pattern.\nrendered: {rendered:?}"
        );
    }

    #[test]
    fn layout_with_dashed_cluster_and_backend_matches() {
        let server = sample_server(3);
        let client = sample_client(None);
        let request_id = Ulid::generate();
        let entry = sample_entry(None);
        let rendered = audit_log_context!(server, client, &request_id, &entry, AuditResult::Err);
        assert!(
            pattern().is_match(&rendered),
            "rendered line did not match audit-log pattern.\nrendered: {rendered:?}"
        );
    }

    #[test]
    fn layout_with_extras_matches() {
        let server = sample_server(0);
        let client = sample_client(Some(42));
        let request_id = Ulid::generate();
        let mut entry = sample_entry(Some("my_app"));
        entry.extras.elapsed_ms = Some(17);
        entry.extras.error_code = Some(AuditErrorCode::WorkerFailure);
        entry.extras.reason = Some("worker 1: failed".to_owned());
        entry.extras.fanout = Some(FanoutSummary {
            status: FanoutStatus::Partial,
            workers_ok: 1,
            workers_err: 1,
            workers_expected: 2,
        });
        let rendered = audit_log_context!(server, client, &request_id, &entry, AuditResult::Err);
        assert!(
            pattern().is_match(&rendered),
            "rendered line with extras did not match.\nrendered: {rendered:?}"
        );
    }

    #[test]
    fn sanitizer_strips_tab_and_escape_in_target() {
        let server = sample_server(0);
        let client = sample_client(Some(0));
        let request_id = Ulid::generate();
        let mut entry = sample_entry(None);
        entry.target = "cluster:\tforge\x1b[Kghost".to_owned();
        let rendered = audit_log_context!(server, client, &request_id, &entry, AuditResult::Ok);
        assert!(
            !rendered.contains('\t') || rendered.matches('\t').count() == 2,
            "target field must not introduce additional tabs (only the three structural tabs allowed): {rendered:?}"
        );
        assert!(
            !rendered.contains('\x1b'),
            "target field must not carry ANSI escape codes: {rendered:?}"
        );
    }

    #[test]
    fn actor_role_buckets() {
        assert_eq!(actor_role(None), "unknown");
        assert_eq!(actor_role(Some(0)), "root");
        assert_eq!(actor_role(Some(99)), "system");
        assert_eq!(actor_role(Some(999)), "system");
        assert_eq!(actor_role(Some(1000)), "user");
        assert_eq!(actor_role(Some(65534)), "user");
    }

    #[test]
    fn rfc3339_utc_round_numbers() {
        // Unix epoch.
        assert_eq!(
            rfc3339_utc(SystemTime::UNIX_EPOCH),
            "1970-01-01T00:00:00.000000Z"
        );
        // Y2K + 1 day, with microseconds.
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::new(946_771_200, 123_456_000);
        assert_eq!(rfc3339_utc(t), "2000-01-02T00:00:00.123456Z");
    }

    #[test]
    fn build_git_sha_format() {
        // Either 12 hex chars (set by build.rs) or the literal "unknown"
        // fallback for builds outside a git tree.
        let s = SOZU_BUILD_GIT_SHA;
        assert!(
            s == "unknown" || (s.len() == 12 && s.chars().all(|c| c.is_ascii_hexdigit())),
            "unexpected SOZU_BUILD_GIT_SHA: {s:?}"
        );
    }

    #[test]
    fn worker_message_join_sanitises_smuggled_kv_pair() {
        // Both `WorkerTask::on_finish` and `SetMetricDetailTask::on_finish`
        // route each worker's `response.message` through
        // `sanitize_for_audit_kv` before joining into `extras.reason`.
        // Verify the call-site shape catches the canonical SIEM-column-
        // smuggling attempt — `,` and `=` inside the operator-influenced
        // worker payload — without relying on a full Server / Gatherer
        // ceremony to drive the on_finish path end-to-end.
        let worker_id = 7u32;
        let attacker_payload = "x,actor_user=mallory,sozu_version=hijacked";
        let formatted = format!(
            "{worker_id}: {}",
            super::sanitize_for_audit_kv(attacker_payload)
        );
        assert!(
            !formatted.contains("actor_user=mallory"),
            "sanitised worker message must not propagate `=` into the \
             reason column (column-boundary forge defence)"
        );
        assert!(
            !formatted.contains(",actor_user"),
            "sanitised worker message must not propagate `,` into the \
             reason column (column-boundary forge defence)"
        );
        // The replacement character does survive — operators still see
        // SOMETHING in the slot so the failure mode is visible.
        assert!(formatted.contains('?'));
    }
}

#[cfg(test)]
mod mutating_verb_policy_tests {
    //! Regression guard for the systemd `RELOADING=1` bracket policy:
    //! `is_mutating_verb` must NOT include `SetMetricDetail`. The TUI
    //! auto-renews its cardinality lease every `ttl/2` seconds, and a
    //! mutating-verb bracket on each renewal would flap the systemd
    //! unit state every 30 s for the whole TUI session lifetime.
    use super::is_mutating_verb;
    use sozu_command_lib::proto::command::{MetricDetail, SetMetricDetail, request::RequestType};

    #[test]
    fn set_metric_detail_is_not_mutating() {
        let req = RequestType::SetMetricDetail(SetMetricDetail {
            client_id: "top:1:abcdef01".to_owned(),
            detail: Some(MetricDetail::DetailBackend as i32),
            ttl_seconds: Some(60),
            reason: Some("operator dashboard".to_owned()),
            clear: Some(false),
            peer_pid: None,
            peer_session_ulid: None,
        });
        assert!(
            !is_mutating_verb(&req),
            "SetMetricDetail is an observability knob, not a state transition; \
             keeping it out of is_mutating_verb prevents RELOADING flap on lease renewal"
        );
    }
}
