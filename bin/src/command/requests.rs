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
    time::{Duration, Instant},
};

use mio::Token;
use nom::{HexDisplay, Offset};
use prost::Message as _;
use rusty_ulid::Ulid;
use sha2::{Digest, Sha256};
use sozu_command_lib::{
    buffer::fixed::Buffer,
    certificate::Fingerprint,
    config::Config,
    logging,
    parser::parse_several_requests,
    proto::command::{
        AggregatedMetrics, AvailableMetrics, CertificateAndKey, CertificatesWithFingerprints,
        ClusterHashes, ClusterInformations, Event, EventKind, FrontendFilters, HardStop,
        ListenerType, MetricDetail, MetricDetailStatus, MetricsConfiguration,
        QueryCertificatesFilters, QueryHealthChecks, QueryMetricsOptions, RemoveListener, Request,
        ResponseContent, ResponseStatus, RunState, SetMetricDetail, SoftStop, Status,
        UpdateHttpListenerConfig, UpdateHttpsListenerConfig, UpdateTcpListenerConfig,
        UpdateUdpListenerConfig, WorkerInfo, WorkerInfos, WorkerRequest, WorkerResponse,
        WorkerResponses, request::RequestType, response_content::ContentType,
    },
    sd_notify,
    state::ConfigState,
};
use sozu_lib::{
    metrics::METRICS,
    router::{MAX_HOSTNAME_LENGTH, pattern_trie::TrieNode},
};

use crate::command::{
    server::{
        DefaultGatherer, Gatherer, GatheringTask, MessageClient, Server, ServerState, Timeout,
        WorkerId, parse_scatter_request_id,
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
/// Bracket layout mirrors `log_context!` (`lib/src/protocol/mux/mod.rs`) so
/// operators can grep `AUDIT` alongside `MUX` / `RUSTLS` / `PIPE` / `TCP`.
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
            | RequestType::AddUdpFrontend(_)
            | RequestType::AddUdpListener(_)
            | RequestType::ConfigureMetrics(_)
            | RequestType::DeactivateListener(_)
            | RequestType::RemoveBackend(_)
            | RequestType::RemoveCertificate(_)
            | RequestType::RemoveCluster(_)
            | RequestType::RemoveHttpFrontend(_)
            | RequestType::RemoveHttpsFrontend(_)
            | RequestType::RemoveListener(_)
            | RequestType::RemoveTcpFrontend(_)
            | RequestType::RemoveUdpFrontend(_)
            | RequestType::ReplaceCertificate(_)
            | RequestType::UpdateHttpListener(_)
            | RequestType::UpdateHttpsListener(_)
            | RequestType::UpdateTcpListener(_)
            | RequestType::UpdateUdpListener(_)
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
        // INVARIANT: the systemd bracket is symmetric. `mutating` is the
        // sole gate for BOTH the RELOADING=1 (entry) and READY=1 (exit)
        // notifications below, so it must be captured exactly once into this
        // local and reused — never recomputed against a moved/mutated verb —
        // otherwise a verb that opened the reload window could skip closing
        // it (leaving the unit stuck in `reloading`) or vice versa.
        debug_assert_eq!(
            mutating,
            is_mutating_verb(&request_type),
            "is_mutating_verb must be a pure function of the verb (RELOADING/READY bracket gate)"
        );
        // INVARIANT: read-only verbs are never bracketed. The pure dashboard
        // polls (Status / Query* / List* / Count) are dispatched WITHOUT
        // touching `ConfigState` or the fleet, so flapping the systemd unit
        // through `reloading` for them would be a correctness bug for any
        // unit-state-watching tooling. SetMetricDetail is also excluded by
        // design (runtime lease, not a transition — see the doc on the fn).
        debug_assert!(
            !mutating
                || !matches!(
                    request_type,
                    RequestType::Status(_)
                        | RequestType::ListWorkers(_)
                        | RequestType::ListFrontends(_)
                        | RequestType::ListListeners(_)
                        | RequestType::QueryClustersHashes(_)
                        | RequestType::QueryClustersByDomain(_)
                        | RequestType::QueryClusterById(_)
                        | RequestType::QueryCertificatesFromWorkers(_)
                        | RequestType::QueryCertificatesFromTheState(_)
                        | RequestType::QueryMetrics(_)
                        | RequestType::QueryHealthChecks(_)
                        | RequestType::CountRequests(_)
                        | RequestType::SubscribeEvents(_)
                        | RequestType::QueryMaxConnectionsPerIp(_)
                        | RequestType::SetMetricDetail(_)
                ),
            "read-only / non-transition verbs must not open the systemd reload window"
        );
        if mutating && let Err(e) = sd_notify::notify(sd_notify::STATE_RELOADING) {
            warn!("could not notify systemd RELOADING=1: {}", e);
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
            | RequestType::AddUdpFrontend(_)
            | RequestType::AddUdpListener(_)
            | RequestType::ConfigureMetrics(_)
            | RequestType::DeactivateListener(_)
            | RequestType::RemoveBackend(_)
            | RequestType::RemoveCertificate(_)
            | RequestType::RemoveCluster(_)
            | RequestType::RemoveHttpFrontend(_)
            | RequestType::RemoveHttpsFrontend(_)
            | RequestType::RemoveListener(_)
            | RequestType::RemoveTcpFrontend(_)
            | RequestType::RemoveUdpFrontend(_)
            | RequestType::ReplaceCertificate(_)
            | RequestType::UpdateHttpListener(_)
            | RequestType::UpdateHttpsListener(_)
            | RequestType::UpdateTcpListener(_)
            | RequestType::UpdateUdpListener(_)
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

        if mutating && let Err(e) = sd_notify::notify(sd_notify::STATE_READY) {
            warn!("could not notify systemd READY=1: {}", e);
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
                let vec: Vec<_> = cluster_ids
                    .iter()
                    .filter_map(|cluster_id| self.state.cluster_state(cluster_id))
                    .collect();
                // INVARIANT: `filter_map` can only drop entries, so the
                // resolved cluster-info vec never exceeds the matched id set.
                // A larger vec would mean we synthesised a cluster the domain
                // index never resolved.
                debug_assert!(
                    vec.len() <= cluster_ids.len(),
                    "QueryClustersByDomain result must not exceed the matched cluster-id set"
                );
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
    mut filters: QueryCertificatesFilters,
) {
    debug!(
        "querying certificates in the state with filters {:?}",
        filters
    );

    // sozu#1383: `--domain` asks "which certificate would Sōzu present for
    // this host?", so it is resolved through the same SNI trie the resolver
    // uses rather than by exact SAN equality. `take` leaves `filters.domain`
    // at `None` so the fall-through below cannot reach the exact-equality arm
    // of `ConfigState::get_certificates` with a domain still set.
    let certs = match filters.domain.take() {
        Some(domain) => certificates_serving_domain(&server.state, &domain),
        None => server.state.get_certificates(filters),
    };

    client.finish_ok_with_content(
        ContentType::CertificatesWithFingerprints(CertificatesWithFingerprints { certs }).into(),
        "Successfully queried certificates from the state of main process",
    );
}

/// Which certificate would Sōzu present for `domain`, on each HTTPS listener
/// that has one — the question `sozu certificate list --domain <host>` reads
/// as, answered against the main process's own `ConfigState`.
///
/// `ConfigState::get_certificates` filters `--domain` with `Vec::contains`,
/// i.e. exact SAN equality, while the certificate that actually serves a
/// handshake is chosen by `CertificateResolver::domain_lookup`
/// (`lib/src/tls.rs`) — a [`TrieNode`] lookup resolving `*.` wildcard and
/// regex labels. The two disagreed for every certificate not bound by an
/// exact name, so `--domain foo.example.com` reported nothing while TLS for
/// that host succeeded on a `*.example.com` certificate (sozu#1383).
///
/// The fix lives here rather than in `ConfigState` because `TrieNode` is in
/// `sozu-lib`, which already depends on `sozu-command-lib`: the reverse edge
/// is a cargo cycle, moving the trie into `command/` would grow a `regex`
/// production dependency it does not have, and reimplementing the match there
/// would duplicate hostname matching and drift from the resolver — which is
/// this bug. `bin/` already depends on both crates, and this is the only
/// `--domain` consumer of `get_certificates`.
///
/// Three properties are deliberate:
///
/// - **One trie per listener address, unioned.** `ConfigState::certificates`
///   is keyed by listener and each worker listener owns its own resolver, so
///   a single global trie would silently drop a second listener's certificate
///   for the same name: `TrieNode::insert` answers
///   `InsertResult::Existing` and keeps the incumbent.
/// - **Both sides are ASCII-lowercased.** Name derivation does not otherwise
///   drift — `ConfigState::add_certificate` resolves the SAN/CN set exactly as
///   `CertifiedKeyWrapper::try_from` does — but the resolver lowercases that
///   set and `ConfigState` does not, so the query and the stored names are
///   folded here instead. `lib/src/https.rs`'s `query_certificate_for_domain`
///   folds the query the same way.
/// - **At most one certificate per listener**, because a trie lookup resolves
///   to one entry. The exact-equality filter could answer with several, so a
///   `--domain` query returns fewer certificates than it used to whenever more
///   than one carried the requested name verbatim; see `doc/configure_cli.md`.
fn certificates_serving_domain(
    state: &ConfigState,
    domain: &str,
) -> BTreeMap<String, CertificateAndKey> {
    let queried = domain.to_ascii_lowercase();
    let mut serving = BTreeMap::new();

    for certificates in state.certificates.values() {
        let mut domains: TrieNode<Fingerprint> = TrieNode::root();

        // Iterate in fingerprint order, not `HashMap` order. `domain_insert`
        // keeps the incumbent on a collision, so two certificates carrying the
        // same name on one listener would otherwise make the answer vary from
        // run to run. `ConfigState` does not retain `AddCertificate::expired_at`
        // and so cannot reproduce the resolver's longest-lived tie-break;
        // ordering by fingerprint at least makes this answer reproducible, and
        // `sozu certificate list --domain <host> --workers` reports the choice
        // the worker's resolver actually made.
        for (fingerprint, certificate) in certificates.iter().collect::<BTreeMap<_, _>>() {
            for name in &certificate.names {
                // `CertificateResolver::add_certificate` bounds every name the
                // same way, because the trie recurses once per label, and
                // refuses a certificate carrying a name the trie cannot host.
                // `ConfigState` enforces neither, so a name a saved state file
                // carried past it is skipped here rather than indexed: no
                // handshake can reach it either. A name the trie refuses
                // outright makes `domain_insert` a no-op, which is the same
                // outcome.
                if name.len() > MAX_HOSTNAME_LENGTH {
                    continue;
                }
                domains.domain_insert(
                    name.to_ascii_lowercase().into_bytes(),
                    fingerprint.to_owned(),
                );
            }
        }

        if let Some((_, fingerprint)) = domains.domain_lookup(queried.as_bytes(), true)
            && let Some(certificate) = certificates.get(fingerprint)
        {
            serving.insert(fingerprint.to_string(), certificate.to_owned());
        }
    }

    serving
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
                path.display()
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
    /// sozu#1313: per-entry accounting, so an entry no worker acknowledged is
    /// reverted instead of staying in the main-process state.
    gatherer: PerEntryGatherer,
    client_token: Option<Token>,
}

pub fn load_static_config(server: &mut Server, mut client: OptionalClient, path: Option<&str>) {
    let new_config;

    let config = match path {
        Some(path) if !path.is_empty() => {
            info!("loading static configuration at path {}", path);
            match Config::load_from_path(path) {
                Ok(loaded) => {
                    new_config = loaded;
                    &new_config
                }
                Err(config_err) => {
                    // The path comes from `sozu reload --file <path>`, i.e. from
                    // the client. Panicking on it took the MAIN process down and
                    // orphaned every worker over an unreadable or malformed file.
                    // Report it the way the `generate_config_messages` failure
                    // below already does: audit the attempt when a client made
                    // it, then fail that client and leave the fleet untouched.
                    let reason = format!("cannot load configuration from '{path}': {config_err}");
                    error!("{}", reason);
                    if let Some(client_ref) = client.as_deref() {
                        let (verb, counter) = audit_verb!("configuration_reloaded");
                        audit_emit_inline(
                            server,
                            client_ref,
                            EventKind::ConfigurationReloaded,
                            verb,
                            counter,
                            format!("config:{path}"),
                            AuditResult::Err,
                            AuditExtras::default(),
                        );
                    }
                    client.finish_failure(reason);
                    return;
                }
            }
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
            // No task is created before this point on purpose: a task created
            // and never scattered to would be released on the next tick with
            // `expected_responses == 0` and answer the client a second time,
            // contradicting the failure below.

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

    // sozu#1313: bounded deadline, scaled to the number of entries this reload
    // is about to scatter. `Timeout::None` left the task — and the client —
    // waiting forever on an answer a killed worker could never send.
    let timeout = bulk_replay_timeout(server.config.worker_timeout, config_messages.len());
    let task_id = server.new_task(
        Box::new(LoadStaticConfigTask {
            gatherer: PerEntryGatherer::default(),
            client_token: client.as_ref().map(|c| c.token),
        }),
        timeout,
    );

    for (request_index, message) in config_messages.into_iter().enumerate() {
        let request = message.content;
        // sozu#1301: skip an unbuildable listener at boot without reserving its
        // address, so a corrected reload can still add it. sozu#1313: skip a
        // frontend the workers' router refuses, so it never enters the state
        // the master persists and replays. Fail-open — one bad entry does not
        // stop the others (matches the dispatch-error skip below), it just
        // never enters ConfigState. The `warn!` is not redundant with the
        // `return_processing`: at startup there is no client (`client == None`,
        // see the audit comment above) and the processing line goes nowhere.
        if let Some(reason) = request
            .request_type
            .as_ref()
            .and_then(|request_type| validate_request(request_type, RequestOrigin::Authored).err())
        {
            warn!("Skipping invalid config entry: {}", reason);
            client.return_processing(format!("Skipping invalid config entry: {reason}"));
            continue;
        }
        if let Err(error) = server.state.dispatch(&request) {
            // `warn!` as well as `return_processing`: at startup there is no
            // client, so the log line is the only trace of the skipped entry.
            warn!("Skipping a config entry the state refused: {:#}", error);
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
        timed_out: bool,
    ) {
        // PRECONDITION: the gatherer ran to completion — either every expected
        // worker answered, or the bounded deadline of `bulk_replay_timeout`
        // fired.
        debug_assert!(
            timed_out || self.gatherer.has_finished(),
            "LoadStaticConfigTask::on_finish: must be finished (ok+errors >= expected) unless timed out"
        );
        // sozu#1313: revert every entry NO worker acknowledged before reporting.
        // The reload path had the same hole as the replay path — an entry the
        // whole fleet rejected stayed in the main-process state and was
        // re-persisted by the next `SaveState`.
        let reverted = self.gatherer.revert_unacknowledged(server, timed_out);
        // Snapshot the failure tally before the loop consumes `responses`; the
        // failure-message list built below must contain exactly one entry per
        // counted error. Read only inside the post-loop assert (ungated, E0425).
        let errors_before = self.gatherer.inner.errors;
        let mut messages = vec![];
        for (worker_id, response) in self.gatherer.inner.responses {
            match ResponseStatus::try_from(response.status) {
                Ok(ResponseStatus::Failure) => {
                    messages.push(format!("worker {worker_id}: {}", response.message))
                }
                Ok(ResponseStatus::Ok) | Ok(ResponseStatus::Processing) => {}
                Err(e) => warn!("error decoding response status: {}", e),
            }
        }
        // INVARIANT: the gatherer's `errors` counter (incremented in
        // `on_message` for every Failure status) must match the number of
        // failure lines we just collected from the same `Failure` responses.
        // A mismatch means the counter and the response log disagree on how
        // many workers rejected the config.
        debug_assert_eq!(
            messages.len(),
            errors_before,
            "LoadStaticConfig failure-message count must equal the gatherer error tally"
        );

        // A timeout is a failure, never a success: some workers were never
        // heard from, so the reload did not provably apply everywhere.
        if self.gatherer.inner.errors > 0 || timed_out {
            client.finish_failure(format!(
                "\nloading static configuration failed: {} OK, {} errors, timed_out: {}, \
                 reverted entries: {}:\n- {}",
                self.gatherer.inner.ok,
                self.gatherer.inner.errors,
                timed_out,
                reverted,
                messages.join("\n- ")
            ));
        } else {
            client.finish_ok(format!(
                "Successfully loaded the config: {} ok, {} errors",
                self.gatherer.inner.ok, self.gatherer.inner.errors,
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
        RequestType::AddUdpFrontend(frontend) => {
            let (verb, counter) = audit_verb!("udp_frontend_added");
            Some(AuditEntry {
                kind: EventKind::FrontendAdded,
                verb,
                counter,
                target: format!("frontend:udp:{}:{}", frontend.cluster_id, frontend.address),
                cluster_id: Some(frontend.cluster_id.to_owned()),
                backend_id: None,
                address: Some(frontend.address),
                extras: AuditExtras::default(),
            })
        }
        RequestType::RemoveUdpFrontend(frontend) => {
            let (verb, counter) = audit_verb!("udp_frontend_removed");
            Some(AuditEntry {
                kind: EventKind::FrontendRemoved,
                verb,
                counter,
                target: format!("frontend:udp:{}:{}", frontend.cluster_id, frontend.address),
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
        RequestType::UpdateUdpListener(patch) => {
            let (verb, counter) = audit_verb!("udp_listener_updated");
            let current = state.udp_listeners.get(&SocketAddr::from(patch.address));
            Some(AuditEntry {
                kind: EventKind::ListenerUpdated,
                verb,
                counter,
                target: format!(
                    "listener:udp:{}:{}",
                    patch.address,
                    format_patch_diff_udp(patch, current),
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
        RequestType::AddUdpListener(listener) => {
            let (verb, counter) = audit_verb!("udp_listener_added");
            Some(AuditEntry {
                kind: EventKind::ListenerAdded,
                verb,
                counter,
                target: format!("listener:udp:{}", listener.address),
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
    // PRECONDITION: the verb tag and its counter key are non-empty. Both come
    // from `audit_verb!` string literals; an empty `counter` would silently
    // collapse every audited verb onto one metric series, and an empty `verb`
    // would render an unattributable audit line. Bumping the counter exactly
    // once per emission is the metric contract dashboards rely on.
    debug_assert!(
        !entry.counter.is_empty() && !entry.verb.is_empty(),
        "audit entry must carry a non-empty verb tag and counter key"
    );
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
    // POSTCONDITION: the audit lease_id never exceeds the SIEM-visible cap.
    // The operator-initiated path applies the same `take(AUDIT_LEASE_ID_MAX_CHARS)`
    // bound; a worker-local line that slipped past it would give SOC tooling
    // an inconsistent upper bound across the two emission sites.
    debug_assert!(
        lease_id
            .as_deref()
            .is_none_or(|id| id.chars().count() <= AUDIT_LEASE_ID_MAX_CHARS),
        "worker-local audit lease_id must respect AUDIT_LEASE_ID_MAX_CHARS"
    );
    // The counter for this verb is bumped exactly once at entry (above).
    debug_assert!(
        !verb.is_empty() && !counter.is_empty(),
        "worker-local metric-detail audit must carry a non-empty verb/counter"
    );

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
    /// `MetricsConfiguration::Clear` deferred-clear flag. When `true`, the
    /// completion handler wipes the master-side `METRICS` aggregator AFTER
    /// the audit row has been emitted. Done post-audit so the
    /// `count!(metrics_configured, 1)` increment driven by the audit row
    /// itself is not what the operator sees in `sozu metrics` immediately
    /// after `sozu metrics clear` — otherwise the master would be wiped,
    /// the audit would repopulate one counter, and the "wipes everything"
    /// contract would silently drift by exactly one row per clear.
    clear_master_metrics_on_finish: bool,
    /// Inverse of the request (sozu#1301/sozu#1314 rollback safety-net),
    /// computed by [`compute_rollback`] before fan-out. When NO worker
    /// acknowledges the request — see [`should_rollback_fanout`] for the two
    /// triggers — [`WorkerTask::on_finish`] applies this to the main process's
    /// `ConfigState` to revert the committed change. `None` for requests
    /// without a defined inverse (they keep today's best-effort behavior).
    rollback: Option<Request>,
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

/// Master-side pre-validation for listener-add requests (sozu#1301).
///
/// A worker rejects an unbuildable listener configuration only when it
/// constructs the listener (building the rustls context / parsing the answer
/// templates) — but by then the main process has already committed the listener
/// to its `ConfigState` and reserved the address, so a corrected reload is
/// refused with `StateError::Exists` and never reaches the workers. Running the
/// worker's OWN construction check here, BEFORE `ConfigState::dispatch`, keeps
/// the two in lockstep (same binary, same crypto provider) and lets an invalid
/// listener be rejected without ever reserving its address. This mirrors the
/// existing `SetMetricDetail` pre-validation in [`worker_request`]: fail fast
/// before fan-out rather than amplifying a bad input across every worker.
///
/// Only listener-add requests are validated; every other request returns `Ok`.
/// TCP/UDP construction has no fallible config today (see their
/// `validate_config`), so those arms currently always succeed. This is one half
/// of the shared pre-dispatch gate — call [`validate_request`], not this
/// function, from an apply path.
fn validate_listener_request(request: &RequestType) -> Result<(), String> {
    match request {
        RequestType::AddHttpListener(config) => {
            sozu_lib::http::HttpListener::validate_config(config)
        }
        RequestType::AddHttpsListener(config) => {
            sozu_lib::https::HttpsListener::validate_config(config)
        }
        RequestType::AddTcpListener(config) => sozu_lib::tcp::TcpListener::validate_config(config),
        RequestType::AddUdpListener(config) => sozu_lib::udp::UdpListener::validate_config(config),
        _ => return Ok(()),
    }
    .map_err(|listener_error| listener_error.to_string())
}

/// Master-side pre-validation for HTTP/HTTPS frontend-add requests (sozu#1313).
///
/// `ConfigState` has no route-grammar check: `add_http_frontend` only converts
/// the request (the sole failure being an out-of-range `position`), so a
/// frontend whose hostname every worker's router refuses is still recorded by
/// the main process. Once recorded it is authoritative — `SaveState` re-
/// serialises it and every state replay re-injects it, and the replay path has
/// no rollback at all. Running the worker's OWN insertion path here, against a
/// disposable empty [`sozu_lib::router::Router`] and BEFORE
/// `ConfigState::dispatch`, keeps the two in lockstep by construction (same
/// binary, same code) and stops the poisoned entry at the door. This mirrors
/// [`validate_listener_request`] (sozu#1301).
///
/// The probe costs a `Router::new()` — two empty `Vec`s and a `TrieNode::root()`
/// — plus exactly the work each worker would have done anyway, bounded by
/// `MAX_HOSTNAME_LENGTH`. One probe per `Add{Http,Https}Frontend` on the
/// control plane buys back N rejected fan-outs and N audit lines.
///
/// It covers, in the worker's own order: the hostname length bound before any
/// parse, the path rule, the `DomainRule` parse, the route-table trie grammar
/// (`RouterError::AddRoute` — trailing `/` with no openable regex segment,
/// empty labels, non-`.`-anchored regex segments), and the rewrite/header
/// policy built by `Frontend::new`. Every other request returns `Ok`.
///
/// The returned string is a `RouterError` / `RequestError` `Display`, both of
/// which report byte LENGTHS rather than operator values — never format the
/// frontend itself into it: `RequestHttpFrontend`'s `Display` (the state map
/// key) carries the hostname in clear.
fn validate_frontend_request(request: &RequestType) -> Result<(), String> {
    let front = match request {
        RequestType::AddHttpFrontend(front) | RequestType::AddHttpsFrontend(front) => front,
        _ => return Ok(()),
    };
    let front = front
        .to_owned()
        .to_frontend()
        .map_err(|request_error| request_error.to_string())?;
    let mut probe = sozu_lib::router::Router::new();
    probe
        .add_http_front(&front)
        .map_err(|router_error| router_error.to_string())
}

/// Which door a request arrived at. The buildability and route-grammar gates
/// are the same at both; the H2 knob floors are not (sozu#1418).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestOrigin {
    /// Somebody is stating this value right now — a `sozu ctl` invocation, a
    /// raw protobuf request on the command socket, the boot configuration.
    /// An out-of-range knob is a typo, and an error names the key to fix.
    Authored,
    /// Replay of state the main process already accepted and is already
    /// serving — `LoadState`, and the hot-upgrade handover it backs. The
    /// operator is upgrading, not authoring.
    Replayed,
}

/// Master-side pre-validation of the H2 listener knobs (sozu#1418).
///
/// Separate from [`validate_listener_request`] because it is the one gate
/// whose verdict depends on [`RequestOrigin`]: a worker *can* build a listener
/// whose threshold is out of range — `H2FloodConfig::from_optional` clamps it
/// to the floor — so this is a policy about what may be stated, not a
/// buildability check. Every other request returns `Ok`.
fn validate_h2_knob_floors(request: &RequestType) -> Result<(), String> {
    match request {
        RequestType::AddHttpListener(config) => {
            sozu_command_lib::state::validate_h2_flood_knobs_http_listener(config)
        }
        RequestType::AddHttpsListener(config) => {
            sozu_command_lib::state::validate_h2_flood_knobs_https_listener(config)
        }
        _ => return Ok(()),
    }
    .map_err(|state_error| state_error.to_string())
}

/// sozu#1301 + sozu#1313 + sozu#1418: the single pre-dispatch validation every
/// master apply path — [`worker_request`], [`load_state`] and
/// [`load_static_config`] — runs before mutating `ConfigState`.
///
/// [`validate_h2_knob_floors`] runs only for [`RequestOrigin::Authored`], and
/// that asymmetry is deliberate. The other two gates reject configurations
/// that CANNOT function: an answer template that will not parse, a hostname
/// the router refuses. An H2 threshold of `0` is a perfectly buildable
/// listener that the workers clamp to the floor and then serve from. Applying
/// the same skip to a replayed entry would turn degraded-but-serving into
/// absent — on an HTTPS listener, a full outage for every frontend behind it,
/// at upgrade time, to prevent a single silently-corrected threshold.
///
/// Be exact about what the clamp does, because it does NOT tighten anything:
/// `check_flood`'s `flag` is `count > threshold`, so a threshold of `0` trips
/// on the FIRST counted event and the clamped `1` trips on the second. Every
/// clamped knob is loosened, by exactly one event. That is still the right
/// trade, because `0` is not "no limit" and not a protection level anyone
/// tuned — it is a typo whose only effect is to trip immediately, and the
/// clamp moves it one event. Refusing on replay spends an outage to buy that
/// one event back. `load_state` therefore admits the entry and warns, loudly,
/// with the key to fix.
fn validate_request(request: &RequestType, origin: RequestOrigin) -> Result<(), String> {
    validate_listener_request(request)?;
    if origin == RequestOrigin::Authored {
        validate_h2_knob_floors(request)?;
    }
    validate_frontend_request(request)
}

/// The inverse of a mutating request, used by the fan-out rollback safety-net
/// (sozu#1301/sozu#1314, defense-in-depth on top of `validate_listener_request`).
///
/// When the main process commits a request to its `ConfigState` and fans it
/// out, but **no** worker acknowledges it, [`WorkerTask::on_finish`] applies
/// this inverse to revert the main process's own state — so its authoritative
/// `ConfigState` (the source of truth replayed into restarted or upgraded
/// workers) never permanently holds an `Add` no worker confirmed.
/// [`should_rollback_fanout`] owns the exact triggers and the one residual
/// divergence they accept.
///
/// Only requests whose inverse is unambiguous WITHOUT capturing the prior value
/// are covered: the non-upsert `Add` verbs — every listener type (inverse
/// `RemoveListener`) and HTTP/HTTPS frontends (inverse is the same payload under
/// the `Remove` variant). Upsert verbs (`AddCluster`, `AddBackend`, which
/// replace a prior value in place) and certificates would need the prior value
/// captured to revert correctly, so they are deliberately NOT covered and keep
/// today's best-effort behavior; every other request returns `None`.
///
/// `ReplaceCertificate` (sozu-proxy/sozu#1404) is one of those certificate
/// verbs, named explicitly here because it is easy to mistake for an `Add`:
/// its only identity for the certificate it displaces is `old_fingerprint`, a
/// one-way SHA-256 hash, never the displaced certificate's PEM/key bytes. An
/// inverse would have to reconstruct "put the old certificate back", which is
/// not computable from the hash — unlike `RemoveListener`/`Remove*Frontend`,
/// whose covered `Add` requests already carry everything the inverse needs.
/// Recovering that content would mean capturing it out-of-band, before
/// `worker_request` calls `state.dispatch`, and changing this function's
/// request-only signature to accept that snapshot — a materially bigger
/// change than this function's other entries, and out of scope here.
///
/// `ConfigState::replace_certificate` (`command/src/state.rs`) was fixed
/// alongside this comment to add the new certificate before removing the
/// old one, so a rejected dispatch (e.g. malformed PEM) is now a true no-op
/// on `ConfigState`. That no-op property is NOT the one [`worker_request`]'s
/// `state_hash_before` check already asserted: `ConfigState::hash_state`
/// folds only `self.clusters`, `self.backends`, `self.tcp_fronts`,
/// `self.http_fronts` and `self.https_fronts` — never `self.certificates` —
/// so that check could not see a certificate-map corruption before this fix
/// and still cannot see one after it. Nothing in either debug or release
/// builds covered `self.certificates` before this changeset; that is
/// precisely why #1404 could ship and sit there undetected.
/// `ConfigState::check_invariants` (the OTHER debug-only postcondition,
/// run inside `dispatch` itself) gained the missing coverage in the same
/// changeset instead: every `self.certificates[address]` bucket must be
/// non-empty, which in turn required fixing two further dangling-empty-
/// bucket paths this coverage surfaced — `remove_certificate` (did not
/// evict the address key when its last certificate was removed) and
/// `add_certificate` (could leave a bucket behind for a previously-absent
/// address on a rejected add). See `command/src/state.rs`'s
/// `check_invariants` and its certificate regression tests.
/// This closes the internal partial-failure hazard #1404 was filed for. It
/// does NOT touch the separate hazard this function guards against — a
/// successful dispatch that zero workers ever acknowledge — which
/// `ReplaceCertificate` still carries, same as `AddCertificate`/
/// `RemoveCertificate` above.
fn compute_rollback(request: &RequestType) -> Option<Request> {
    let inverse = match request {
        RequestType::AddHttpListener(config) => RequestType::RemoveListener(RemoveListener {
            address: config.address,
            proxy: ListenerType::Http.into(),
        }),
        RequestType::AddHttpsListener(config) => RequestType::RemoveListener(RemoveListener {
            address: config.address,
            proxy: ListenerType::Https.into(),
        }),
        RequestType::AddTcpListener(config) => RequestType::RemoveListener(RemoveListener {
            address: config.address,
            proxy: ListenerType::Tcp.into(),
        }),
        RequestType::AddUdpListener(config) => RequestType::RemoveListener(RemoveListener {
            address: config.address,
            proxy: ListenerType::Udp.into(),
        }),
        RequestType::AddHttpFrontend(front) => RequestType::RemoveHttpFrontend(front.clone()),
        RequestType::AddHttpsFrontend(front) => RequestType::RemoveHttpsFrontend(front.clone()),
        // `remove_tcp_frontend` matches on the very (address, sni, alpn) key
        // `add_tcp_frontend` admitted — its own `INV:` comment and its
        // "drops exactly one entry" assertion pin that mirror — so this inverse
        // evicts exactly the frontend the add inserted. Leaving it out kept
        // sozu#1313's poisoned-state loop open for every TCP frontend.
        //
        // `AddUdpFrontend` inverts the same way now that `remove_udp_frontend`
        // keys on the (cluster_id, address, tags) identity `add_udp_frontend`
        // admits -- same `INV:` comment and same "drops exactly one entry"
        // assertion -- so the inverse evicts exactly the unacknowledged entry
        // and leaves every other acknowledged frontend in place. While the
        // removal key was the address alone it was coarser than its add and
        // stayed out for that reason. Since `add_udp_frontend` admits one
        // frontend per address across every cluster, the entry this inverse
        // removes is the only one on its address, which is what makes the
        // eviction unambiguous. Pinned by
        // `a_udp_frontend_add_inverts_to_its_exact_removal` below and by
        // `remove_udp_frontend_drops_exactly_the_frontend_its_tags_name` in
        // `command/src/state.rs`.
        RequestType::AddTcpFrontend(front) => RequestType::RemoveTcpFrontend(front.clone()),
        RequestType::AddUdpFrontend(front) => RequestType::RemoveUdpFrontend(front.clone()),
        _ => return None,
    };
    Some(inverse.into())
}

/// sozu#1301 / sozu#1314: does the fan-out outcome justify reverting the
/// main-process `ConfigState` with the precomputed inverse of [`compute_rollback`]?
///
/// Two triggers, both requiring ZERO `Ok` answers — an entry a single worker
/// acknowledged might be good, and is NEVER rolled back:
///
/// - unanimous rejection (`!timed_out`): every worker the request was scattered
///   to answered `Failure`. The original sozu#1301 trigger, unchanged.
/// - timeout with zero acknowledgements (`timed_out`): not one of the answers
///   received before the deadline was an `Ok`, and there may have been no
///   answer at all. A worker that PANICS produces no synthetic `Failure` and
///   no `expected_responses` decrement, so
///   `has_finished` (`ok + errors >= expected`) never fires and the task only
///   ends through the event loop's timeout path; `errors > 0` alone therefore
///   cannot express "the fleet refused this". Skipping the revert here left the
///   main process permanently holding — and `SaveState` re-persisting — an entry
///   no worker ever acknowledged, which is exactly how one unpatched worker
///   panicking during a rolling upgrade commits a malformed frontend for good
///   (sozu#1314). The client is already told `Failure` on this same path, so
///   reverting keeps the authoritative state consistent with the reported
///   outcome.
///
/// `expected == 0` (a local-only fan-out) is never a verdict about anything and
/// never reverts; it is unreachable through the timeout path anyway, since
/// `has_finished` is already true at `0 >= 0`.
///
/// ACCEPTED RESIDUAL: a slow-but-healthy worker that applies the request and
/// answers `Ok` AFTER the deadline is invisible — the task is off the in-flight
/// queue and its late answer is dropped. The main process reverts while that
/// worker keeps the frontend until its next restart or state replay: a bounded
/// divergence that self-heals on replay and matches the `Failure` the client
/// already received. The alternative — today's behavior — permanently commits an
/// entry no worker confirmed, which is strictly worse for this incident class.
fn should_rollback_fanout(timed_out: bool, expected: usize, ok: usize, errors: usize) -> bool {
    expected > 0 && ok == 0 && (errors > 0 || timed_out)
}

/// Extra deadline granted per scattered entry by [`bulk_replay_timeout`].
const BULK_TIMEOUT_PER_ENTRY: Duration = Duration::from_millis(10);

/// Ceiling of [`bulk_replay_timeout`], expressed in `worker_timeout` units.
const BULK_TIMEOUT_MAX_MULTIPLIER: u32 = 10;

/// sozu#1313: the bounded deadline of a BULK apply path — [`load_state`] and
/// [`load_static_config`].
///
/// Both used to scatter with [`Timeout::None`], so their task ended only once
/// every expected worker had answered. One answer that never comes — a worker
/// killed mid-replay, a request that could not be queued on a saturated
/// channel — left the task in flight forever, and the client waiting on it with
/// no answer at all. `Timeout::None` also made [`should_rollback_fanout`]'s
/// timeout trigger unreachable on these paths.
///
/// `Timeout::Default` (one `worker_timeout`) is the budget for ONE request, not
/// for a replay that queues N entries onto every worker, each to be serialised,
/// written, parsed and applied. The budget is therefore one `worker_timeout` of
/// fan-out slack plus [`BULK_TIMEOUT_PER_ENTRY`] per scattered entry, capped at
/// [`BULK_TIMEOUT_MAX_MULTIPLIER`] × `worker_timeout` so an unresponsive fleet
/// can never hold the client longer than a bounded, documented window. With the
/// default 10 s `worker_timeout`: 10 s for a small state file, at most 100 s for
/// any file at all.
fn bulk_replay_timeout(worker_timeout_secs: u32, entries: usize) -> Timeout {
    // `Config::default()` leaves `worker_timeout` at zero (tests, upgrade
    // fixtures); a zero-second deadline would expire the task before the first
    // worker could answer.
    let base = Duration::from_secs(worker_timeout_secs.max(1) as u64);
    let cap = base.saturating_mul(BULK_TIMEOUT_MAX_MULTIPLIER);
    let scaled = base.saturating_add(
        BULK_TIMEOUT_PER_ENTRY.saturating_mul(u32::try_from(entries).unwrap_or(u32::MAX)),
    );
    // POSTCONDITION: bounded above by the cap and never below one
    // `worker_timeout` — the two properties `load_state` relies on.
    let timeout = scaled.min(cap);
    debug_assert!(
        timeout <= cap && timeout >= base,
        "a bulk replay deadline must stay within [worker_timeout, cap]"
    );
    Timeout::Custom(timeout)
}

/// sozu#1313: response accounting for the two BULK apply paths.
///
/// [`load_state`] and [`load_static_config`] scatter hundreds of INDEPENDENT
/// entries onto a SINGLE task, so the fleet-wide `ok`/`errors` tally of a
/// [`DefaultGatherer`] says nothing about any particular entry: one entry every
/// worker rejected is invisible behind a hundred entries they accepted. The
/// main process therefore kept — and `SaveState` re-persisted — entries no
/// worker ever acknowledged, and the poisoned file re-injected them on the next
/// replay.
///
/// This gatherer keeps the `DefaultGatherer` behaviour verbatim (it owns one)
/// and adds a per-entry breakdown keyed by the scatter `request_id`, plus the
/// inverse request from [`compute_rollback`] captured while the request is
/// still in hand. [`Self::revert_unacknowledged`] then applies the very
/// predicate the live fan-out uses, [`should_rollback_fanout`], to each entry
/// on its own.
#[derive(Debug, Default)]
struct PerEntryGatherer {
    /// fleet-wide tally, `has_finished` and the response log
    inner: DefaultGatherer,
    /// per scatter `request_id` breakdown
    entries: BTreeMap<usize, ScatteredEntry>,
}

/// What one entry of a bulk apply path scattered, and what came back for it.
#[derive(Debug, Default)]
struct ScatteredEntry {
    expected: usize,
    ok: usize,
    errors: usize,
    /// inverse of the scattered request; `None` when it has no unambiguous one
    /// (see [`compute_rollback`])
    rollback: Option<Request>,
}

impl PerEntryGatherer {
    /// Revert every entry NO worker acknowledged, and return how many were
    /// reverted from the main-process `ConfigState`.
    ///
    /// Same safety bound as the live fan-out: an entry at least one worker
    /// applied is never reverted, and an entry whose verb has no unambiguous
    /// inverse keeps today's best-effort behaviour.
    fn revert_unacknowledged(&self, server: &mut Server, timed_out: bool) -> usize {
        let mut reverted = 0usize;
        for entry in self.entries.values() {
            if !should_rollback_fanout(timed_out, entry.expected, entry.ok, entry.errors) {
                continue;
            }
            let Some(rollback) = entry.rollback.as_ref() else {
                continue;
            };
            // Counts only, never request content: a `RequestHttpFrontend`
            // rendering carries the operator-supplied hostname.
            match server.state.dispatch(rollback) {
                Ok(()) => reverted += 1,
                Err(revert_error) => error!(
                    "sozu#1313 rollback: could not revert an unacknowledged replay entry \
                     (ok=0, errors={}, expected={}, timed_out={}): {}",
                    entry.errors, entry.expected, timed_out, revert_error
                ),
            }
        }
        if reverted > 0 {
            warn!(
                "sozu#1313 rollback: reverted {} replay entries no worker acknowledged \
                 (of {} scattered, timed_out={})",
                reverted,
                self.entries.len(),
                timed_out
            );
        }
        reverted
    }
}

impl Gatherer for PerEntryGatherer {
    fn inc_expected_responses(&mut self, count: usize) {
        self.inner.inc_expected_responses(count);
    }

    fn has_finished(&self) -> bool {
        self.inner.has_finished()
    }

    fn on_scatter(&mut self, request_id: usize, worker_count: usize, request: &Request) {
        let entry = self.entries.entry(request_id).or_default();
        let expected_before = entry.expected;
        entry.expected += worker_count;
        // The bulk paths allocate a fresh `request_id` per entry, so the first
        // scatter is the one that carries the request. Keeping the first
        // inverse also keeps this idempotent if an entry is ever re-scattered.
        if entry.rollback.is_none() {
            entry.rollback = request.request_type.as_ref().and_then(compute_rollback);
        }
        // INVARIANT: the per-entry budget advances by exactly `worker_count`,
        // in lockstep with the fleet-wide one. A drift either way would make
        // `should_rollback_fanout` read a fan-out that never happened.
        debug_assert_eq!(
            self.entries[&request_id].expected,
            expected_before + worker_count,
            "on_scatter must grow the per-entry budget by exactly worker_count"
        );
        self.inner.inc_expected_responses(worker_count);
    }

    fn on_message(
        &mut self,
        server: &mut Server,
        client: &mut OptionalClient,
        worker_id: WorkerId,
        message: WorkerResponse,
    ) {
        // Attribute BEFORE handing the message to the inner gatherer, which
        // consumes it. The id is the one `scatter_on` built, so its last
        // segment is the entry index.
        match parse_scatter_request_id(&message.id) {
            Some((_, _, request_index)) => {
                let entry = self.entries.entry(request_index).or_default();
                match ResponseStatus::try_from(message.status) {
                    Ok(ResponseStatus::Ok) => entry.ok += 1,
                    Ok(ResponseStatus::Failure) => {
                        entry.errors += 1;
                        // One line per rejection, before the tally decides
                        // the rollback: the worker message is already
                        // redacted on its side, so it is safe to relay.
                        warn!(
                            "worker {} rejected replay entry {}: {}",
                            worker_id, request_index, message.message
                        );
                    }
                    // Processing is not terminal, an undecodable status is
                    // reported by the inner gatherer.
                    Ok(ResponseStatus::Processing) | Err(_) => {}
                }
            }
            // Never reached for a response to a scattered request; an
            // unattributable answer still counts fleet-wide below, it just
            // cannot protect its entry from the rollback.
            None => warn!("could not attribute a worker response to a replay entry"),
        }
        self.inner.on_message(server, client, worker_id, message);
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
        // `try_from` rejects any i32 outside the proto-known set. An
        // unknown variant lands here only if the master has been deployed
        // ahead of a worker schema bump (or a malformed IPC payload slips
        // past the dispatch whitelist). Log loudly and fall back to
        // `Disabled` — silently treating "unknown" as "disabled" would
        // mask future enum drift, e.g. a `Clear` value the master no
        // longer recognises would silently skip the master-side wipe.
        match MetricsConfiguration::try_from(*value) {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                error!(
                    "ConfigureMetrics IPC carries unknown enum value {} ({:?}), \
                     falling back to MetricsConfiguration::Disabled",
                    value, err
                );
                Some(MetricsConfiguration::Disabled)
            }
        }
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

    // INVARIANT: a single verb resolves to at most ONE audit channel. The
    // three are derived from disjoint `RequestType` matches: a full
    // `AuditEntry` (audit_entry_for), the inline ConfigureMetrics line
    // (metrics_target), or the inline SetMetricDetail line
    // (metric_detail_audit). If two were ever populated together, the
    // dispatch/error/on_finish arms below — which are `if/else if` chains —
    // would silently drop the second, producing an un-audited mutation. The
    // `as u8` sum counts how many channels fired; it must never exceed one.
    debug_assert!(
        (audit.is_some() as u8)
            + (metrics_target.is_some() as u8)
            + (metric_detail_audit.is_some() as u8)
            <= 1,
        "a verb must map to at most one audit channel (entry / metrics / metric_detail)"
    );

    let request: sozu_command_lib::proto::command::Request = request_content.into();
    let request_sha256 = compute_request_sha256(&request);

    // Snapshot the state hash so the error path can assert the rejected
    // dispatch was a true no-op on `ConfigState` — a partially-applied
    // mutation that then errors would leave the master's persisted state
    // diverged from every worker (which never saw the fan-out). The state
    // handlers guarantee this (see `ConfigState::dispatch` postcondition),
    // and we re-check it here at the request boundary. NOTE: we deliberately
    // do NOT assert the success path *changed* the hash — many "mutating"
    // verbs (ConfigureMetrics, SetMetricDetail, SetMaxConnectionsPerIp,
    // Logging) are runtime/worker-only and `ConfigState::dispatch` is
    // `Ok(())` no-op on ConfigState for them (see its runtime-only `Ok(())`
    // arm in `command/src/state.rs`). `hash_state()` is a cheap per-cluster
    // map; the snapshot is read ONLY inside the debug_assert below, so it is
    // dead code in release but must stay ungated (E0425).
    let state_hash_before = server.state.hash_state();

    // sozu#1301 + sozu#1313: validate the request the way the worker will apply
    // it BEFORE committing to ConfigState — a listener the way the worker builds
    // it (rustls context + answer templates), a frontend the way the worker's
    // router inserts it. Committing an unbuildable listener first reserves its
    // address, and the corrected reload is then refused with StateError::Exists;
    // committing a frontend whose hostname the router refuses poisons the state
    // the master re-persists and replays. Validation is read-only on state and
    // runs first; dispatch happens only if it passes.
    // Tag the two failure classes distinctly for the audit taxonomy: a rejected
    // `validate_request` is operator-invalid *input* (bad TLS versions/ciphers,
    // an unparseable answer template, a malformed frontend hostname) →
    // `InvalidInput` (same class as the `set_logging_level` filter check); a
    // rejected `state.dispatch` is a genuine state conflict → `DispatchError`.
    let apply_result = request
        .request_type
        .as_ref()
        .map_or(Ok(()), |request_type| {
            validate_request(request_type, RequestOrigin::Authored)
        })
        .map_err(|reason| (AuditErrorCode::InvalidInput, reason))
        .and_then(|()| {
            server
                .state
                .dispatch(&request)
                .map_err(|error| (AuditErrorCode::DispatchError, error.to_string()))
        });

    if let Err((error_code, reason)) = apply_result {
        // INVARIANT: neither a rejected validation nor a rejected dispatch may
        // mutate persisted state — validation never touches it, and a rejected
        // dispatch is a guaranteed no-op (see ConfigState::dispatch).
        debug_assert_eq!(
            server.state.hash_state(),
            state_hash_before,
            "a rejected validation or dispatch must leave ConfigState byte-identical (no partial apply)"
        );
        if let Some(mut entry) = audit {
            entry.extras.error_code = Some(error_code);
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
                    error_code: Some(error_code),
                    reason: Some(reason.clone()),
                    ..Default::default()
                },
            );
        } else if let Some(fields) = metric_detail_audit.clone() {
            let (verb, counter) = audit_verb!("metric_detail_changed");
            let target = fields.target.clone();
            let mut extras = fields.into_extras();
            extras.elapsed_ms = Some(elapsed_ms(started_at));
            extras.error_code = Some(error_code);
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
            "could not apply request on the main process state: {reason}",
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
    // INVARIANT: the completion-time channel threaded into `WorkerTask`
    // mirrors the attempt-time channel exactly. `audit_for_task` carries the
    // full-entry verbs; `inline_audit` carries the ConfigureMetrics /
    // SetMetricDetail inline verbs. They are disjoint by construction (an
    // entry-bearing verb has no metrics_target / metric_detail_audit) — if
    // both were ever set, `WorkerTask::on_finish` would emit the entry arm
    // and silently drop the inline completion line.
    debug_assert!(
        !(audit_for_task.is_some() && inline_audit.is_some()),
        "WorkerTask carries at most one completion-time audit channel (entry XOR inline)"
    );
    // INVARIANT: the completion channel is present iff its attempt-time
    // source was. `audit_for_task` is `audit.as_ref().map(...)` so it is Some
    // exactly when `audit` is; losing it here would drop the completion line.
    debug_assert_eq!(
        audit_for_task.is_some(),
        audit.is_some(),
        "audit_for_task must be Some iff the attempt-time AuditEntry was Some"
    );

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

    // Master-side clear: deferred to `WorkerTask::on_finish` AFTER the
    // audit emission so the `count!(metrics_configured, 1)` driven by the
    // completion-time audit row does not immediately repopulate the
    // freshly-cleared `main_metrics`. Without this deferral, the documented
    // "wipes everything" contract drifts by exactly one row per clear and
    // `sozu metrics` snapshot taken right after `sozu metrics clear` would
    // report `config.metrics_configured = 1`.
    let clear_master_metrics_on_finish = metrics_configuration == Some(MetricsConfiguration::Clear);

    client.return_processing("Processing worker request...");

    // sozu#1301/sozu#1314 rollback safety-net: compute the inverse of this
    // request now, while we still hold it, so `on_finish` can revert the
    // main-process state if no worker acknowledges the change. `None` for verbs
    // without a defined inverse (they keep today's best-effort behavior).
    let rollback = request.request_type.as_ref().and_then(compute_rollback);

    server.scatter(
        request,
        Box::new(WorkerTask {
            client_token: client.token,
            gatherer: DefaultGatherer::default(),
            started_at,
            audit: audit_for_task,
            inline_audit,
            metric_detail_audit: metric_detail_audit_completion,
            clear_master_metrics_on_finish,
            rollback,
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
        // PRECONDITION: the gatherer ran to completion. Either every expected
        // worker answered (`ok + errors >= expected`, the `has_finished`
        // predicate that drives the task off the in-flight queue) or the task
        // tripped its timeout. A handler that fires with neither would be a
        // dispatcher accounting bug (a response counted twice, or the task
        // released before its workers replied). Read-only snapshots feed the
        // debug_asserts only → dead code in release, but must stay ungated.
        debug_assert!(
            timed_out
                || self.gatherer.ok + self.gatherer.errors >= self.gatherer.expected_responses,
            "WorkerTask::on_finish: must be finished (ok+errors >= expected) unless timed out"
        );
        // INVARIANT: every accounted ok/err corresponds to a stored response.
        // `on_message` pushes to `responses` for every ok/err/processing, so
        // the response log can never be shorter than the ok+err tally.
        debug_assert!(
            self.gatherer.responses.len() >= self.gatherer.ok + self.gatherer.errors,
            "responses log must hold at least one entry per accounted ok/err"
        );

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

        // INVARIANT: the audit result mirrors the fanout status. A row tagged
        // `result=ok` must never carry a Timeout/Partial fanout, and an
        // `result=err` row must never claim a clean Ok/LocalOnly fanout —
        // a SIEM correlating the two columns would otherwise see a row that
        // contradicts itself.
        debug_assert_eq!(
            matches!(result, AuditResult::Err),
            matches!(fanout_status, FanoutStatus::Timeout | FanoutStatus::Partial),
            "AuditResult and FanoutStatus must agree on success vs failure"
        );
        // INVARIANT: `LocalOnly` means no worker was scattered to, so there
        // can be no per-worker tallies. If a worker answered while the status
        // says local-only, the expected-count accounting drifted.
        debug_assert!(
            !matches!(fanout_status, FanoutStatus::LocalOnly) || (ok == 0 && errors == 0),
            "LocalOnly fanout must have zero worker ok/err tallies"
        );

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

        // Deferred master-side `MetricsConfiguration::Clear`. Runs AFTER
        // the audit emission above so the `count!(metrics_configured, 1)`
        // increment driven by that audit row is wiped here. Operators
        // running `sozu metrics clear; sozu metrics` see an empty master
        // dump, matching the PR contract.
        if self.clear_master_metrics_on_finish {
            METRICS.with(|metrics| {
                (*metrics.borrow_mut()).clear_local();
            });
        }

        // sozu#1301 / sozu#1314 rollback safety-net: when NO worker acknowledged
        // the change committed to ConfigState — every scattered worker rejected
        // it, or the scatter timed out with zero `Ok` (a panicking worker
        // answers nothing at all) — revert the main process's own state with the
        // precomputed inverse. A mixed result (`ok > 0`) is deliberately left
        // as-is to avoid diverging the main process from the workers that did
        // accept. See [`should_rollback_fanout`] for both triggers and for the
        // accepted late-`Ok` residual on the timeout path.
        if should_rollback_fanout(timed_out, expected, ok, errors)
            && let Some(rollback) = self.rollback.as_ref()
        {
            match server.state.dispatch(rollback) {
                // Counts only, never request content: a `RequestHttpFrontend`
                // rendering carries the operator-supplied hostname.
                Ok(()) => warn!(
                    "sozu#1301/sozu#1314 rollback: reverted a change from main-process state \
                     (ok=0, errors={}, expected={}, timed_out={})",
                    errors, expected, timed_out
                ),
                Err(revert_error) => error!(
                    "sozu#1301/sozu#1314 rollback: could not revert main-process state after an \
                     unacknowledged fan-out (ok=0, errors={}, expected={}, timed_out={}): {}",
                    errors, expected, timed_out, revert_error
                ),
            }
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
    // INVARIANT: SHA-256 always yields 32 bytes; we render the first 8 as
    // 2-hex-digit pairs, so the audit prefix is always exactly 16 chars.
    debug_assert_eq!(digest.len(), 32, "SHA-256 digest must be 32 bytes");
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    // POSTCONDITION: greppable fixed-width 16-char (64-bit) audit prefix.
    debug_assert_eq!(
        hex.len(),
        16,
        "request sha256 audit prefix must be 16 hex chars"
    );
    hex
}

/// SHA-256 fingerprint of a PEM certificate, hex-encoded and truncated to
/// 16 hex chars (64 bits) for audit brevity. `None` when the input is not
/// parseable as PEM. Mirrors the full fingerprint workflow in
/// `sozu_command_lib::certificate::calculate_fingerprint` but truncates
/// for log terseness — operators correlate via the 64-bit prefix.
fn compute_certificate_fingerprint(certificate_pem: &[u8]) -> Option<String> {
    let fp = sozu_command_lib::certificate::calculate_fingerprint(certificate_pem).ok()?;
    // INVARIANT: a SHA-256 fingerprint is 32 bytes; we render the first 8.
    debug_assert!(
        fp.len() >= 8,
        "certificate fingerprint must hold at least the 8 bytes we truncate to"
    );
    let mut hex = String::with_capacity(16);
    for byte in fp.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    // POSTCONDITION: fixed-width 16-char (64-bit) fingerprint prefix.
    debug_assert_eq!(
        hex.len(),
        16,
        "certificate fingerprint prefix must be 16 hex chars"
    );
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

    // POSTCONDITION of the master-side pre-validation above: by the time we
    // reach fan-out, the request honours both lease bounds (the two guards
    // returned early otherwise). The worker enforces these again as
    // defence-in-depth, but a request that slipped past here would amplify a
    // bad input across every worker (N rejections + N audit lines).
    debug_assert!(
        req.client_id.len() <= sozu_lib::metrics::LEASE_CLIENT_ID_MAX_BYTES,
        "SetMetricDetail must be length-validated before fan-out"
    );
    debug_assert!(
        req.ttl_seconds
            .is_none_or(|t| u64::from(t) <= sozu_lib::metrics::LEASE_TTL_MAX.as_secs()),
        "SetMetricDetail must be TTL-validated before fan-out"
    );

    let started_at = Instant::now();
    // Snapshot the cluster-hash so we can confirm SetMetricDetail is a
    // ConfigState no-op (it is runtime-only — `ConfigState::dispatch`
    // returns `Ok(())` without touching persisted state; see its
    // runtime-only `Ok(())` arm in `command/src/state.rs`). Read only inside
    // the post-dispatch assert → ungated for the release build (E0425).
    let state_hash_before = server.state.hash_state();
    let request: Request = RequestType::SetMetricDetail(req).into();

    // Attempt-time dispatch gate (mirrors `state.dispatch` in worker_request).
    if let Err(error) = server.state.dispatch(&request) {
        // INVARIANT: a rejected runtime-only dispatch leaves ConfigState
        // untouched (it never mutates it in the first place).
        debug_assert_eq!(
            server.state.hash_state(),
            state_hash_before,
            "SetMetricDetail dispatch must not mutate ConfigState"
        );
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

    // INVARIANT: even on the success path, SetMetricDetail is a ConfigState
    // no-op — the runtime lease lives in each worker's `Aggregator`, never in
    // the master's persisted `ConfigState`. If the hash changed, a future
    // edit accidentally wired a runtime knob into persisted state.
    debug_assert_eq!(
        server.state.hash_state(),
        state_hash_before,
        "SetMetricDetail must not mutate ConfigState even on success"
    );

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
        // PRECONDITION: the scatter ran to completion (or timed out). Mirrors
        // the generic `WorkerTask::on_finish` finished-or-timeout guard so a
        // SetMetricDetail task released before its workers replied trips here.
        debug_assert!(
            timed_out
                || self.gatherer.ok + self.gatherer.errors >= self.gatherer.expected_responses,
            "SetMetricDetailTask::on_finish: must be finished (ok+errors >= expected) unless timed out"
        );

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
        // INVARIANT: the per-worker status map holds only successfully-ACK'd
        // workers (the loop above skips non-Ok responses), so it can never be
        // larger than the OK tally. A larger map would mean a non-Ok worker
        // leaked into the operator-visible `MetricDetailStatus.workers`.
        debug_assert!(
            status.workers.len() <= ok,
            "MetricDetailStatus.workers must not exceed the OK worker tally"
        );
        // INVARIANT: audit result agrees with fanout status (same as the
        // generic WorkerTask path).
        debug_assert_eq!(
            matches!(result, AuditResult::Err),
            matches!(fanout_status, FanoutStatus::Timeout | FanoutStatus::Partial),
            "AuditResult and FanoutStatus must agree on success vs failure"
        );
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
    /// sozu#1313: per-entry accounting, so an entry no worker acknowledged is
    /// reverted from the main-process state instead of being re-persisted by
    /// the next `SaveState` and re-injected by every later replay.
    pub gatherer: PerEntryGatherer,
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

    // sozu#1313: a bounded deadline from the start — the entry count is only
    // known once the file is parsed, so this initial budget covers the
    // degenerate cases (empty or unparseable file) and is re-armed below with
    // the real count, which also restarts the countdown at the END of the
    // fan-out rather than at the beginning of a long parse.
    let worker_timeout = server.config.worker_timeout;
    let task_id = server.new_task(
        Box::new(LoadStateTask {
            client_token: client.as_ref().map(|c| c.token),
            gatherer: PerEntryGatherer::default(),
            path: path.to_owned(),
        }),
        bulk_replay_timeout(worker_timeout, 0),
    );

    let mut buffer = Buffer::with_capacity(200000);
    let mut scatter_request_counter = 0usize;
    // sozu#1313: entries the pre-dispatch validation refused. Counted here
    // rather than on `LoadStateTask` because the task is already owned by the
    // server's queue and `Server` exposes no handle to mutate it afterwards —
    // the tally only needs to reach the operator, which the `return_processing`
    // below does.
    let mut skipped_invalid = 0usize;

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
                    // sozu#1301: skip an unbuildable listener from the saved
                    // state without reserving its address, so a corrected
                    // reload can add it (mirrors the dispatch-failure skip).
                    // sozu#1313: skip a frontend the workers' router refuses —
                    // this replay path has NO rollback, so an entry dispatched
                    // here stays in the main-process state for good and is re-
                    // persisted by the next `SaveState`. `warn!`, not `debug!`:
                    // an entry silently dropped from the state the operator
                    // saved must be visible at the default log level.
                    if let Some(request_type) = &request.content.request_type {
                        if let Err(reason) = validate_request(request_type, RequestOrigin::Replayed)
                        {
                            warn!(
                                "load_state: skipping an entry rejected by pre-dispatch validation: {}",
                                reason
                            );
                            count!("config.load_skipped_invalid", 1);
                            skipped_invalid += 1;
                            continue;
                        }
                        // sozu#1418: an out-of-range H2 knob is NOT a reason to
                        // drop a replayed listener. The entry was accepted
                        // before the gate existed and is serving today; the
                        // workers clamp the threshold to the floor. That clamp
                        // LOOSENS the knob by exactly one event — `count >
                        // threshold` trips on the first counted event at `0`
                        // and on the second at `1` — but `0` is not "no limit"
                        // and not a protection level anyone tuned. Dropping
                        // the listener instead would unbind it and take every
                        // frontend behind it offline — a far larger blast
                        // radius than that one event. Keep it, and say so at
                        // `warn!` so the operator can fix the source rather
                        // than discovering it at the next upgrade.
                        if let Err(reason) = validate_h2_knob_floors(request_type) {
                            warn!(
                                "load_state: keeping a listener whose H2 knob is out of range — \
                                 the workers clamp it to the floor: {}. Correct it with \
                                 `sozu ctl listener http|https update` (or in the configuration \
                                 file) and save the state again.",
                                reason
                            );
                            count!("config.load_h2_knob_clamped", 1);
                        }
                    }
                    if let Err(error) = server.state.dispatch(&request.content) {
                        // The entry never enters ConfigState and is never
                        // scattered; say so, because at startup there is no
                        // client to carry the reason.
                        warn!("load_state: skipping an entry the state refused: {}", error);
                        count!("config.load_skipped_invalid", 1);
                        // The tally the operator is shown below covers BOTH skip
                        // branches: an entry the state refused is just as absent
                        // from the loaded state as one validation refused.
                        skipped_invalid += 1;
                    } else {
                        // INVARIANT: the scatter request_id advances by
                        // exactly one per dispatched request. `scatter_on`
                        // embeds it in the per-worker request id, so a stale
                        // or repeated counter would collide two in-flight ids
                        // and silently drop a worker's response, hanging the
                        // task. Snapshot is read only inside the assert →
                        // ungated for the release (E0425) build.
                        let counter_before = scatter_request_counter;
                        scatter_request_counter += 1;
                        debug_assert_eq!(
                            scatter_request_counter,
                            counter_before + 1,
                            "load_state must advance the scatter request_id by exactly one per dispatch"
                        );
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
            server.rearm_task_timeout(
                task_id,
                bulk_replay_timeout(worker_timeout, scatter_request_counter),
            );
            if skipped_invalid > 0 {
                // sozu#1313: the load is deliberately NOT aborted — every valid
                // entry still applies — but the operator must learn that the
                // state they reloaded is not the state they saved.
                client.return_processing(format!(
                    "load_state: skipped {skipped_invalid} invalid entries — see the main process warnings"
                ));
            }
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
        timed_out: bool,
    ) {
        let Self { gatherer, path, .. } = *self;
        let ok = gatherer.inner.ok;
        let errors = gatherer.inner.errors;
        // PRECONDITION: the gatherer ran to completion — either every expected
        // worker answered, or the bounded deadline of `bulk_replay_timeout`
        // fired. Before sozu#1313 this path scattered with `Timeout::None` and
        // could only end the first way, which is exactly why a worker killed
        // mid-replay hung `sozu state load` with no answer at all.
        debug_assert!(
            timed_out || gatherer.has_finished(),
            "LoadStateTask::on_finish: must be finished (ok+errors >= expected) unless timed out"
        );
        // sozu#1313: revert every replayed entry NO worker acknowledged, so the
        // main process does not keep — and `SaveState` does not re-persist — an
        // entry the whole fleet refused. Runs before `update_counts` so the
        // frontend/backend gauges reflect the reverted state.
        let reverted = gatherer.revert_unacknowledged(server, timed_out);
        server.update_counts();
        // A timeout is a failure, never a success: entries were left
        // unacknowledged, and the reverts above already assume as much.
        let failed = errors > 0 || timed_out;
        let result = if failed {
            AuditResult::Err
        } else {
            AuditResult::Ok
        };
        // INVARIANT: the audit result matches the outcome — an `ok:N errors:0`
        // line that did not time out must be tagged Ok, anything else Err.
        debug_assert_eq!(
            matches!(result, AuditResult::Ok),
            errors == 0 && !timed_out,
            "LoadStateTask audit result must agree with the worker error tally and the deadline"
        );
        if let Some(client_ref) = client.as_deref() {
            let (verb, counter) = audit_verb!("state_loaded");
            audit_emit_inline(
                server,
                client_ref,
                EventKind::StateLoaded,
                verb,
                counter,
                format!("file:{path} ok:{ok} errors:{errors} reverted:{reverted}"),
                result,
                AuditExtras {
                    error_code: failed.then_some(if timed_out {
                        AuditErrorCode::WorkerTimeout
                    } else {
                        AuditErrorCode::WorkerFailure
                    }),
                    ..Default::default()
                },
            );
        }
        if !failed {
            client.finish_ok(format!(
                "Successfully loaded state from path {path}, {ok} ok messages, {errors} errors"
            ));
            return;
        }
        client.finish_failure(format!(
            "loading state: {ok} ok messages, {errors} errors, timed out: {timed_out}, \
             reverted entries: {reverted}"
        ));
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
    // POSTCONDITION: the stop request has opened the shutdown sequence. The
    // matching `StopTask::on_finish` will later advance to `Stopping`. We do
    // NOT assert the prior state was `Running` — an operator may legitimately
    // re-issue stop while a soft-stop is already draining.
    debug_assert_eq!(
        server.run_state,
        ServerState::WorkersStopping,
        "stop() must move the master into WorkersStopping before fan-out"
    );
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
        // PRECONDITION: a StopTask is only created by `stop()`, which moves
        // the master to `WorkersStopping` BEFORE scattering. The server must
        // therefore never still be `Running` when a stop task finishes —
        // that would mean the run-state transition that brackets shutdown
        // was skipped. (`Stopping` is also acceptable: a prior stop task may
        // already have advanced it.)
        debug_assert_ne!(
            server.run_state,
            ServerState::Running,
            "StopTask::on_finish must observe a shutdown run-state, never Running"
        );
        if timed_out && self.hardness {
            client.finish_failure(format!(
                "Workers take too long to stop ({} ok, {} errors), stopping the main process to sever the link",
                self.gatherer.ok, self.gatherer.errors
            ));
        }
        server.run_state = ServerState::Stopping;
        // POSTCONDITION: shutdown is now committed.
        debug_assert_eq!(
            server.run_state,
            ServerState::Stopping,
            "StopTask::on_finish must leave the master in the Stopping state"
        );
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
    diff_opt_copy!(h2_max_header_fields);
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
    diff_opt_copy!(h2_max_header_fields);
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

/// Render a UDP listener patch as a compact `field=old→new` diff for the audit
/// trail, mirroring [`format_patch_diff_tcp`]. UDP has no `expect_proxy` /
/// `connect_timeout`; its distinguishing knobs are `max_rx_datagram_size` and
/// `max_flows`.
fn format_patch_diff_udp(
    p: &UpdateUdpListenerConfig,
    current: Option<&sozu_command_lib::proto::command::UdpListenerConfig>,
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
    diff_req_copy!(front_timeout);
    diff_req_copy!(back_timeout);
    diff_req_copy!(max_rx_datagram_size);
    diff_req_copy!(max_flows);
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
    //! (see `LOGGER_COLORED` in `command/src/logging/logs.rs`), so
    //! `ansi_palette()` returns empty strings and the rendered line is
    //! ANSI-free — stable to match with a plain regex.
    use super::{
        AUDIT_LEASE_ID_MAX_CHARS, AUDIT_REASON_MAX_CHARS, AuditEntry, AuditErrorCode, AuditExtras,
        AuditResult, FanoutStatus, FanoutSummary, SOZU_BUILD_GIT_SHA, SOZU_VERSION, actor_role,
        rfc3339_utc,
    };
    use regex::Regex;
    use rusty_ulid::Ulid;
    use sozu_command_lib::{ObjectKind, proto::command::EventKind, state::StateError};
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
    fn state_error_reason_is_bounded_before_entering_the_audit_envelope() {
        const ERROR_SECRET: &str = "AUDIT_STATE_ERROR_SECRET_SENTINEL";

        let server = sample_server(0);
        let client = sample_client(Some(42));
        let request_id = Ulid::generate();
        let id = format!("{ERROR_SECRET}{}", "x".repeat(4096));
        let id_len = id.len();
        let error = StateError::Exists {
            kind: ObjectKind::Cluster,
            id,
        };
        let mut entry = sample_entry(None);
        entry.extras.error_code = Some(AuditErrorCode::DispatchError);
        entry.extras.reason = Some(error.to_string());

        let rendered = audit_log_context!(server, client, &request_id, &entry, AuditResult::Err);

        assert!(
            !rendered.contains(ERROR_SECRET),
            "audit reason leaked the StateError payload: {rendered}"
        );
        assert!(
            rendered.contains(&format!("id_bytes={id_len}")),
            "audit reason omitted bounded StateError metadata: {rendered}"
        );
        assert!(
            rendered.len() <= 1024,
            "audit line is not bounded: {} bytes",
            rendered.len()
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

#[cfg(test)]
mod rollback_predicate_tests {
    //! sozu#1314: the sozu#1301 rollback safety-net must also fire when the
    //! scatter TIMES OUT with zero worker acknowledgements — a worker that
    //! panics answers nothing at all, so `errors` alone never proves the
    //! fleet refused the change.
    //!
    //! `WorkerTask::on_finish` needs a real `&mut Server` and the timeout
    //! itself lives in the event loop, so the guarantee is composed of two
    //! locked properties: `handle_finishing_task_forwards_timed_out_flag`
    //! (bin/src/command/server.rs) pins the propagation of `timed_out` down
    //! to `on_finish`, and the exhaustive cases below pin the decision the
    //! flag feeds. Same pattern as sozu#1301, where only `compute_rollback`
    //! is unit-tested.
    //!
    //! To SEE THESE RED (regression proof): restore the pre-fix body of
    //! [`super::should_rollback_fanout`],
    //! `!timed_out && expected > 0 && ok == 0 && errors > 0` — the two
    //! rolls-back-on-timeout expectations below then fail.
    use super::should_rollback_fanout;

    #[test]
    fn unanimous_rejection_still_rolls_back() {
        // sozu#1301, unchanged: every scattered worker answered Failure.
        assert!(
            should_rollback_fanout(false, 3, 0, 3),
            "a unanimous rejection must still revert the main-process commit"
        );
    }

    #[test]
    fn timeout_with_no_answer_at_all_rolls_back() {
        // The incident shape: a single worker panics on the malformed
        // hostname, produces no synthetic Failure and no expected_responses
        // decrement, and the task ends through the timeout path.
        assert!(
            should_rollback_fanout(true, 1, 0, 0),
            "a scatter that timed out with zero answers must revert the commit"
        );
    }

    #[test]
    fn timeout_with_only_failures_rolls_back() {
        // The rolling-upgrade shape of sozu#1314: the patched workers answer
        // Failure, the unpatched one panics and stays silent until timeout.
        assert!(
            should_rollback_fanout(true, 3, 0, 2),
            "a timeout whose received answers are all failures must revert the commit"
        );
    }

    #[test]
    fn any_acknowledgement_prevents_a_rollback() {
        // `ok == 0` is the safety bound: an entry at least one worker applied
        // is never reverted, timeout or not — reverting it would diverge the
        // main process from the workers that accepted it.
        for (timed_out, expected, ok, errors) in
            [(false, 3, 1, 2), (true, 3, 1, 0), (true, 3, 1, 2)]
        {
            assert!(
                !should_rollback_fanout(timed_out, expected, ok, errors),
                "an entry acknowledged by a worker must never be rolled back \
                 (timed_out={timed_out}, expected={expected}, ok={ok}, errors={errors})"
            );
        }
    }

    #[test]
    fn a_clean_fanout_never_rolls_back() {
        assert!(
            !should_rollback_fanout(false, 3, 0, 0),
            "a fan-out that neither failed nor timed out must not be reverted"
        );
    }

    #[test]
    fn a_local_only_fanout_never_rolls_back() {
        // expected == 0 means nothing was scattered; there is no fleet verdict
        // to act on. Unreachable through the timeout path anyway (has_finished
        // is true at 0 >= 0), but the predicate stays safe on its own.
        for timed_out in [false, true] {
            assert!(
                !should_rollback_fanout(timed_out, 0, 0, 0),
                "a local-only fan-out must not be reverted (timed_out={timed_out})"
            );
        }
    }
}

#[cfg(test)]
mod listener_validation_tests {
    //! sozu#1301: an invalid listener config must be rejected by the main
    //! process BEFORE it is committed to `ConfigState`, so it never reserves
    //! its address and blocks a corrected reload with `StateError::Exists`.
    //! These exercise the production [`validate_listener_request`] guard that
    //! all three master apply paths (`worker_request`, `load_static_config`,
    //! `load_state`) share.
    //!
    //! To SEE THESE RED (regression proof): make `validate_listener_request`
    //! return `Ok(())` unconditionally — the pre-fix behavior where the main
    //! process committed listener configs without validating them. The
    //! `is_err()` assertions below then fail.
    use super::{
        RequestOrigin, validate_h2_knob_floors, validate_listener_request, validate_request,
    };
    use sozu_command_lib::{
        config::ListenerBuilder,
        proto::command::{
            CertificateAndKey, HttpsListenerConfig, ReplaceCertificate, RequestTcpFrontend,
            RequestUdpFrontend, SocketAddress, request::RequestType,
        },
        state::{ConfigState, StateError},
    };
    use std::collections::BTreeMap;

    /// A default HTTPS listener config for `address`. When `valid` is false, a
    /// malformed answer template is injected so the worker's construction
    /// (`HttpsListener::validate_config` → `HttpAnswers::new`) rejects it. That
    /// trigger is deterministic AND crypto-provider independent, unlike a
    /// `BuildRustls` "no usable cipher suites" message which varies across the
    /// ring / aws-lc-rs / openssl / fips CI cells.
    fn https_config(address: SocketAddress, valid: bool) -> HttpsListenerConfig {
        let mut cfg = ListenerBuilder::new_https(address)
            .to_tls(None)
            .expect("default HTTPS listener config");
        if !valid {
            cfg.answers
                .insert("404".to_owned(), "not a valid http response".to_owned());
        }
        cfg
    }

    #[test]
    fn invalid_https_listener_rejected_before_commit_unblocks_corrected_reload() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8443);

        // The pathology this guard defends against: `ConfigState` itself has no
        // buildability check — it records an unbuildable listener and reserves
        // its address, after which a corrected reload is refused with
        // `StateError::Exists`. Prove it at the state layer so the master-side
        // guard below is demonstrably load-bearing.
        {
            let mut state = ConfigState::new();
            let bad = RequestType::AddHttpsListener(https_config(address, false));
            state
                .dispatch(&bad.into())
                .expect("ConfigState records the bad listener — it has no buildability check");
            let good = RequestType::AddHttpsListener(https_config(address, true));
            let err = state
                .dispatch(&good.into())
                .expect_err("a reserved address blocks the corrected reload");
            assert!(
                matches!(err, StateError::Exists { .. }),
                "expected StateError::Exists, got {err:?}"
            );
        }

        // The fix: the main process validates a listener-add the way the worker
        // will build it BEFORE dispatch, so the unbuildable config is rejected
        // and never reaches `ConfigState`.
        let bad = RequestType::AddHttpsListener(https_config(address, false));
        assert!(
            validate_listener_request(&bad).is_err(),
            "an unbuildable HTTPS listener must be rejected before commit"
        );

        // With the bad listener correctly never committed, the corrected reload
        // validates and applies cleanly — no `StateError::Exists` blocker.
        let mut state = ConfigState::new();
        let good = RequestType::AddHttpsListener(https_config(address, true));
        assert!(
            validate_listener_request(&good).is_ok(),
            "a valid HTTPS listener must pass validation"
        );
        state
            .dispatch(&good.into())
            .expect("corrected reload applies when the bad listener never reserved the address");
    }

    #[test]
    fn valid_listener_adds_of_every_type_pass_validation() {
        let http = RequestType::AddHttpListener(
            ListenerBuilder::new_http(SocketAddress::new_v4(127, 0, 0, 1, 8080))
                .to_http(None)
                .expect("default HTTP listener config"),
        );
        let https = RequestType::AddHttpsListener(https_config(
            SocketAddress::new_v4(127, 0, 0, 1, 8081),
            true,
        ));
        let tcp = RequestType::AddTcpListener(
            ListenerBuilder::new_tcp(SocketAddress::new_v4(127, 0, 0, 1, 8082))
                .to_tcp(None)
                .expect("default TCP listener config"),
        );
        let udp = RequestType::AddUdpListener(
            ListenerBuilder::new_udp(SocketAddress::new_v4(127, 0, 0, 1, 8083))
                .to_udp(None)
                .expect("default UDP listener config"),
        );
        for request in [http, https, tcp, udp] {
            assert!(
                validate_listener_request(&request).is_ok(),
                "a valid listener add must pass validation: {request:?}"
            );
        }
    }

    #[test]
    fn invalid_http_listener_is_rejected_before_commit() {
        // The HTTP path is fallible through answer-template parsing too.
        let mut cfg = ListenerBuilder::new_http(SocketAddress::new_v4(127, 0, 0, 1, 8084))
            .to_http(None)
            .expect("default HTTP listener config");
        cfg.answers
            .insert("404".to_owned(), "not a valid http response".to_owned());
        let request = RequestType::AddHttpListener(cfg);
        assert!(
            validate_listener_request(&request).is_err(),
            "an unbuildable HTTP listener must be rejected before commit"
        );
    }

    /// Regression for sozu-proxy/sozu#1418: the H2 knob floors were enforced
    /// on `UpdateHttp(s)Listener` (`validate_h2_flood_knobs_http`/`_https`)
    /// and, since that issue, at config load (`ListenerBuilder::to_http`/
    /// `to_tls`) — but a raw protobuf `Add{Http,Https}Listener` straight to
    /// the command socket reached neither. `ConfigState::add_http_listener`
    /// clones the config in with no validation, so the value became
    /// authoritative, `SaveState` re-serialised it, every replay re-injected
    /// it, and each worker then rewrote the operator's `0` to `1` with
    /// `H2FloodConfig::from_optional`'s clamp — the silent rewrite the whole
    /// fix exists to avoid, surviving on the one path nothing gated.
    ///
    /// To SEE THIS RED: drop the `validate_h2_knob_floors(request)?` call from
    /// `validate_request`. Both `is_err()` assertions then fail.
    #[test]
    fn raw_protobuf_listener_add_with_an_out_of_range_h2_knob_is_rejected_before_commit() {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8085);
        let mut http = ListenerBuilder::new_http(address)
            .to_http(None)
            .expect("default HTTP listener config");
        // `ListenerBuilder::to_http` refuses this value; a raw protobuf
        // request never went through it.
        http.h2_max_rst_stream_per_window = Some(0);

        // The pathology: `ConfigState` has no knob validation of its own — it
        // records the zero and makes it authoritative. Prove it at the state
        // layer so the master-side guard below is demonstrably load-bearing.
        {
            let mut state = ConfigState::new();
            state
                .dispatch(&RequestType::AddHttpListener(http.clone()).into())
                .expect("ConfigState records the zero — add_http_listener does not validate");
        }

        assert!(
            validate_request(&RequestType::AddHttpListener(http), RequestOrigin::Authored).is_err(),
            "a raw protobuf AddHttpListener carrying h2_max_rst_stream_per_window = 0 must be \
             rejected before commit instead of being clamped by every worker"
        );

        // The HTTPS twin, on the one knob whose floor is 2 rather than 1.
        let mut https = https_config(SocketAddress::new_v4(127, 0, 0, 1, 8086), true);
        https.h2_stream_shrink_ratio = Some(1);
        assert!(
            validate_request(
                &RequestType::AddHttpsListener(https),
                RequestOrigin::Authored
            )
            .is_err(),
            "a raw protobuf AddHttpsListener carrying h2_stream_shrink_ratio = 1 must be \
             rejected before commit"
        );
    }

    /// sozu#1418: the knob floors must NOT drop a replayed listener.
    ///
    /// `load_state` shares `validate_request` with the authoring paths, and an
    /// entry it rejects is skipped — the listener never enters `ConfigState`,
    /// never binds, and every frontend behind it is offline. For an entry that
    /// cannot function (sozu#1301's unparseable answer template, sozu#1313's
    /// unroutable hostname) that is the right trade. For an H2 threshold of
    /// `0` it is not: the listener builds, the workers clamp the threshold to
    /// the floor, and it serves. The clamp LOOSENS that knob by exactly one
    /// event — `count > threshold` trips on the first counted event at `0` and
    /// on the second at `1` — on a `0` nobody chose as a protection level.
    /// Dropping the listener would instead convert degraded-but-serving into a
    /// full outage for its frontends, at upgrade time, and an HTTPS listener
    /// takes its whole TLS surface with it.
    ///
    /// To SEE THIS RED: make `validate_request` run `validate_h2_knob_floors`
    /// unconditionally (drop the `origin == RequestOrigin::Authored` guard).
    /// The `Replayed` assertion then fails — which is exactly the state file
    /// being dropped.
    #[test]
    fn a_replayed_listener_with_an_out_of_range_h2_knob_is_kept_not_dropped() {
        let mut http = ListenerBuilder::new_http(SocketAddress::new_v4(127, 0, 0, 1, 8088))
            .to_http(None)
            .expect("default HTTP listener config");
        http.h2_max_rst_stream_per_window = Some(0);
        let request = RequestType::AddHttpListener(http);

        assert!(
            validate_request(&request, RequestOrigin::Authored).is_err(),
            "the authoring door must still refuse an out-of-range knob"
        );
        assert!(
            validate_request(&request, RequestOrigin::Replayed).is_ok(),
            "a replayed listener whose only fault is an out-of-range H2 knob must be kept — \
             the workers clamp it; dropping it unbinds the listener and takes its frontends down"
        );
        assert!(
            validate_h2_knob_floors(&request).is_err(),
            "the replay path must still be able to see the fault, to warn with the key to fix"
        );

        // A replayed entry that genuinely cannot function is still dropped:
        // the asymmetry is about buildable-but-degraded, not about replay.
        let mut unbuildable = https_config(SocketAddress::new_v4(127, 0, 0, 1, 8089), false);
        unbuildable.h2_max_rst_stream_per_window = Some(0);
        assert!(
            validate_request(
                &RequestType::AddHttpsListener(unbuildable),
                RequestOrigin::Replayed
            )
            .is_err(),
            "an unbuildable replayed listener must still be skipped (sozu#1301)"
        );
    }

    /// The floors must not reject a listener that merely leaves the knobs
    /// unset or sets them at their minimum — `valid_listener_adds_of_every_type_pass_validation`
    /// covers the unset case; this covers the boundary.
    #[test]
    fn listener_add_with_h2_knobs_at_their_minimum_passes_validation() {
        let mut http = ListenerBuilder::new_http(SocketAddress::new_v4(127, 0, 0, 1, 8087))
            .to_http(None)
            .expect("default HTTP listener config");
        http.h2_max_rst_stream_per_window = Some(1);
        http.h2_max_rst_stream_abusive_lifetime = Some(1);
        http.h2_max_header_fields = Some(1);
        http.h2_stream_shrink_ratio = Some(2);
        assert!(
            validate_request(&RequestType::AddHttpListener(http), RequestOrigin::Authored).is_ok(),
            "H2 knobs at their documented minimum must be accepted"
        );
    }

    #[test]
    fn rollback_inverse_targets_the_same_listener_and_skips_uncovered_verbs() {
        use sozu_command_lib::proto::command::ListenerType;

        // sozu#1301 rollback safety-net: a listener add inverts to a
        // RemoveListener for the SAME address and proxy type, so a unanimous
        // worker rejection can revert the main-process commit.
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8443);
        let add = RequestType::AddHttpsListener(https_config(address, true));
        let inverse = super::compute_rollback(&add).expect("a listener add must have an inverse");
        match inverse.request_type {
            Some(RequestType::RemoveListener(remove)) => {
                assert_eq!(
                    remove.proxy,
                    ListenerType::Https as i32,
                    "inverse must target the HTTPS listener type"
                );
                assert_eq!(
                    remove.address, address,
                    "inverse must target the same address"
                );
            }
            other => panic!("expected a RemoveListener inverse, got {other:?}"),
        }

        // sozu#1313: the TCP frontend verbs invert the same way the HTTP ones
        // do — `remove_tcp_frontend` matches on the very (address, sni, alpn)
        // key `add_tcp_frontend` admitted, so the inverse evicts exactly the
        // frontend the add inserted and nothing else.
        let tcp_front = RequestTcpFrontend {
            cluster_id: "cluster".to_owned(),
            address: SocketAddress::new_v4(127, 0, 0, 1, 8090),
            tags: BTreeMap::new(),
            sni: None,
            alpn: Vec::new(),
        };
        match super::compute_rollback(&RequestType::AddTcpFrontend(tcp_front.clone()))
            .expect("a TCP frontend add must have an inverse")
            .request_type
        {
            Some(RequestType::RemoveTcpFrontend(remove)) => assert_eq!(
                remove, tcp_front,
                "the TCP inverse must target the very frontend that was added"
            ),
            other => panic!("expected a RemoveTcpFrontend inverse, got {other:?}"),
        }

        // Upsert Add verbs (AddCluster/AddBackend) and non-add verbs have no
        // simple prior-value-free inverse and are deliberately uncovered — they
        // keep today's best-effort behavior rather than risk a wrong revert.
        assert!(
            super::compute_rollback(&RequestType::Logging("info".to_owned())).is_none(),
            "a non-add verb must have no rollback inverse"
        );

        // sozu-proxy/sozu#1404: `ReplaceCertificate` is deliberately uncovered
        // too, and for the same content-recovery reason as `AddCertificate` /
        // `RemoveCertificate` — its only identity for the certificate it
        // displaces is `old_fingerprint`, a one-way hash, never the displaced
        // certificate's PEM/key bytes, so no `Request` can express "put the
        // old certificate back" from the request's own fields alone. Now that
        // `ConfigState::replace_certificate` is fixed to add-before-remove, a
        // rejected dispatch is a true no-op (see `state.rs`'s regression
        // tests); this pins that the still-open "zero worker acknowledged"
        // divergence intentionally has no compensating request.
        let replace = RequestType::ReplaceCertificate(ReplaceCertificate {
            address: SocketAddress::new_v4(127, 0, 0, 1, 8443),
            new_certificate: CertificateAndKey {
                certificate: include_str!("../../../command/assets/certificate.pem").to_owned(),
                key: include_str!("../../../command/assets/key.pem").to_owned(),
                certificate_chain: Vec::new(),
                versions: Vec::new(),
                names: Vec::new(),
            },
            old_fingerprint: "aa".repeat(32),
            new_expired_at: None,
        });
        assert!(
            super::compute_rollback(&replace).is_none(),
            "ReplaceCertificate must have no rollback inverse: the request never carries the \
             displaced certificate's content, only its fingerprint hash"
        );
    }

    /// `AddUdpFrontend` inverts to the `RemoveUdpFrontend` carrying the very
    /// request the add carried, exactly as the HTTP, HTTPS and TCP adds do.
    ///
    /// `add_udp_frontend` (`command/src/state.rs`) stores a full
    /// `UdpFrontend { cluster_id, address, tags }` and admits one frontend per
    /// address across every cluster. `remove_udp_frontend` retains on that SAME
    /// (cluster, address, tags) identity — the bucket scopes `cluster_id`, and
    /// its own `INV:` comment plus its "drops exactly one entry" assertion pin
    /// the mirror, just like `remove_tcp_frontend`'s (address, sni, alpn) key.
    /// The inverse therefore evicts exactly the frontend the add inserted and
    /// leaves every other acknowledged frontend in place, which is what kept
    /// this verb out of `compute_rollback` while the removal key was coarser.
    /// Leaving it out now would keep sozu#1313's poisoned-state loop open for
    /// every UDP frontend.
    ///
    /// To SEE THIS RED: remove the
    /// `RequestType::AddUdpFrontend(front) => RequestType::RemoveUdpFrontend(front.clone())`
    /// arm from [`super::compute_rollback`]. That the inverse is collateral-free
    /// is pinned separately by
    /// `remove_udp_frontend_drops_exactly_the_frontend_its_tags_name` in
    /// `command/src/state.rs`.
    #[test]
    fn a_udp_frontend_add_inverts_to_its_exact_removal() {
        let udp_front = RequestUdpFrontend {
            cluster_id: "cluster".to_owned(),
            address: SocketAddress::new_v4(127, 0, 0, 1, 8091),
            tags: BTreeMap::from([("owner".to_owned(), "team-a".to_owned())]),
        };
        match super::compute_rollback(&RequestType::AddUdpFrontend(udp_front.clone()))
            .expect("a UDP frontend add must have an inverse")
            .request_type
        {
            Some(RequestType::RemoveUdpFrontend(remove)) => assert_eq!(
                remove, udp_front,
                "the UDP inverse must target the very frontend that was added, tags included"
            ),
            other => panic!("expected a RemoveUdpFrontend inverse, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod frontend_validation_tests {
    //! sozu#1313: a frontend whose hostname the workers' router refuses must be
    //! rejected by the main process BEFORE it is committed to `ConfigState`.
    //! `ConfigState` has no route-grammar check, the replay path has no
    //! rollback, and `SaveState` re-serialises whatever the state holds — so a
    //! frontend that gets in stays in, and re-poisons every replay. These
    //! exercise the production [`validate_frontend_request`] guard and the
    //! [`validate_request`] entry point all three master apply paths
    //! (`worker_request`, `load_static_config`, `load_state`) share.
    //!
    //! To SEE THESE RED (regression proof): make `validate_frontend_request`
    //! return `Ok(())` unconditionally — the pre-fix behavior where the main
    //! process committed frontends without checking the router grammar. The
    //! `is_err()` assertions below then fail.
    use super::{RequestOrigin, validate_frontend_request, validate_request};
    use sozu_command_lib::{
        config::ListenerBuilder,
        proto::command::{
            PathRule, PathRuleKind, RequestHttpFrontend, RulePosition, SocketAddress,
            request::RequestType,
        },
        state::ConfigState,
    };
    use std::collections::BTreeMap;

    /// The hostname that panicked every worker in production: a trailing `/`
    /// with no openable regex segment. The router's trie grammar refuses it.
    const INCIDENT_HOSTNAME: &str = "raat-app.cleverapps.io/";

    /// A minimal `Tree`-positioned frontend add for `hostname`, valid in every
    /// respect except what the caller intends to make invalid.
    fn frontend(hostname: &str) -> RequestHttpFrontend {
        RequestHttpFrontend {
            cluster_id: Some("cluster".to_owned()),
            address: SocketAddress::new_v4(127, 0, 0, 1, 8080),
            hostname: hostname.to_owned(),
            path: PathRule {
                kind: PathRuleKind::Prefix as i32,
                value: "/".to_owned(),
            },
            method: None,
            position: RulePosition::Tree as i32,
            tags: BTreeMap::new(),
            redirect: None,
            redirect_scheme: None,
            redirect_template: None,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            required_auth: None,
            headers: Vec::new(),
            hsts: None,
        }
    }

    #[test]
    fn config_state_commits_a_frontend_the_router_refuses() {
        // The pathology this guard defends against, proven at the state layer
        // so the master-side guard below is demonstrably load-bearing:
        // `ConfigState::add_http_frontend` only runs `to_frontend()`, whose
        // sole failure is an out-of-range `position` — no hostname grammar is
        // checked anywhere. The malformed entry is therefore recorded, and from
        // there `SaveState` re-persists it and every replay re-injects it.
        let mut state = ConfigState::new();
        state
            .dispatch(&RequestType::AddHttpFrontend(frontend(INCIDENT_HOSTNAME)).into())
            .expect("ConfigState records the malformed frontend — it has no route-grammar check");
    }

    #[test]
    fn malformed_hostnames_are_rejected_before_commit() {
        // Every shape the workers' router refuses — the same classes
        // `Router::add_http_front` rejects, reached through the exact code the
        // worker runs.
        for hostname in [
            // The incident: a trailing `/` opens a regex segment that never
            // closes, so the route-table trie refuses the insert.
            INCIDENT_HOSTNAME,
            // Empty label.
            ".example.com",
            // A segment that is not a valid regex.
            "/[/.example.com",
            // A regex segment that is not `.`-anchored.
            "abc/[0-9]+/.example.com",
            // A bare trailing `.` after a completed segment — the release
            // out-of-bounds `convert_regex_domain_rule` fixed in sozu#1312.
            "/a/.",
        ] {
            let request = RequestType::AddHttpFrontend(frontend(hostname));
            assert!(
                validate_frontend_request(&request).is_err(),
                "a frontend the workers' router refuses must be rejected before commit \
                 (hostname_bytes={})",
                hostname.len(),
            );
        }

        // Over the pre-parse length bound (`MAX_HOSTNAME_LENGTH`): the trie
        // recurses once per label and a `/`-segment compiles a regex, so the
        // router bounds the hostname before parsing anything.
        let oversized = "a".repeat(sozu_lib::router::MAX_HOSTNAME_LENGTH + 1);
        assert!(
            validate_frontend_request(&RequestType::AddHttpFrontend(frontend(&oversized))).is_err(),
            "a hostname over MAX_HOSTNAME_LENGTH must be rejected before commit"
        );
    }

    #[test]
    fn https_frontend_arm_is_validated_too() {
        // `AddHttpsFrontend` carries the same `RequestHttpFrontend` payload and
        // reaches the same router insert on the worker; the guard must not
        // cover only the plaintext arm.
        let request = RequestType::AddHttpsFrontend(frontend(INCIDENT_HOSTNAME));
        assert!(
            validate_frontend_request(&request).is_err(),
            "a malformed hostname must be rejected on the HTTPS arm too"
        );
    }

    #[test]
    fn rejection_message_carries_no_operator_value() {
        // The failure string flows into the audit `reason` column and the
        // client failure message. `RouterError`'s Display reports byte lengths
        // only — a hostname leaking here would undo the redaction work of the
        // Security section this release ships.
        let request = RequestType::AddHttpFrontend(frontend(INCIDENT_HOSTNAME));
        let reason = validate_frontend_request(&request)
            .expect_err("the incident hostname must be rejected");
        assert!(
            !reason.contains(INCIDENT_HOSTNAME),
            "the rejection reason must not carry the operator-supplied hostname: {reason:?}"
        );
    }

    #[test]
    fn valid_frontends_pass_validation() {
        for hostname in [
            "example.com",
            "*.example.com",
            // A well-formed `.`-anchored regex segment.
            "/ab+c/.example.com",
        ] {
            let request = RequestType::AddHttpFrontend(frontend(hostname));
            assert!(
                validate_frontend_request(&request).is_ok(),
                "a frontend the workers' router accepts must pass validation \
                 (hostname_bytes={}): {:?}",
                hostname.len(),
                validate_frontend_request(&request),
            );
        }

        // The rewrite/header policy path (`Frontend::new`) is exercised too:
        // valid capture templates pass...
        let mut rewritten = frontend("example.com");
        rewritten.rewrite_host = Some("$HOST[0]".to_owned());
        rewritten.rewrite_path = Some("/prefix$PATH[0]".to_owned());
        let request = RequestType::AddHttpFrontend(rewritten.clone());
        assert!(
            validate_frontend_request(&request).is_ok(),
            "a frontend with valid rewrites must pass validation: {:?}",
            validate_frontend_request(&request),
        );

        // ...and an out-of-range capture index (an `Exact` domain produces one
        // capture) is refused before commit like any other invalid frontend.
        rewritten.rewrite_host = Some("$HOST[9]".to_owned());
        assert!(
            validate_frontend_request(&RequestType::AddHttpFrontend(rewritten)).is_err(),
            "a rewrite referencing a capture the router cannot fill must be rejected"
        );
    }

    #[test]
    fn non_frontend_verbs_pass_and_invalid_listeners_still_fail() {
        // The frontend guard is a no-op on every other verb...
        assert!(
            validate_frontend_request(&RequestType::Logging("info".to_owned())).is_ok(),
            "a non-frontend verb must not be touched by the frontend guard"
        );

        // ...and the shared entry point still enforces sozu#1301 (an
        // unbuildable listener answer template) as well as sozu#1313.
        let mut cfg = ListenerBuilder::new_http(SocketAddress::new_v4(127, 0, 0, 1, 8085))
            .to_http(None)
            .expect("default HTTP listener config");
        cfg.answers
            .insert("404".to_owned(), "not a valid http response".to_owned());
        assert!(
            validate_request(&RequestType::AddHttpListener(cfg), RequestOrigin::Authored).is_err(),
            "validate_request must keep rejecting an unbuildable listener (sozu#1301)"
        );
        assert!(
            validate_request(
                &RequestType::AddHttpFrontend(frontend(INCIDENT_HOSTNAME)),
                RequestOrigin::Authored
            )
            .is_err(),
            "validate_request must reject a malformed frontend (sozu#1313)"
        );
        assert!(
            validate_request(
                &RequestType::AddHttpFrontend(frontend("example.com")),
                RequestOrigin::Authored
            )
            .is_ok(),
            "validate_request must accept a well-formed frontend"
        );
    }
}

#[cfg(test)]
mod load_state_rollback_tests {
    //! sozu#1313: the two BULK apply paths — [`super::load_state`] (the
    //! saved-state replay) and [`super::load_static_config`] — must revert PER
    //! ENTRY every entry no worker acknowledged, must end on a bounded
    //! deadline instead of waiting forever, and must report that deadline as a
    //! failure.
    //!
    //! Before the fix both tasks only tallied the fleet-wide `ok`/`errors` of a
    //! `DefaultGatherer` and never touched `server.state`. An entry every
    //! worker answered `Failure` to therefore stayed in the main-process
    //! `ConfigState`, was re-persisted by the next `SaveState` and re-injected
    //! on every later replay — the poisoned-state loop of sozu#1313. The live
    //! single-request path (`worker_request` → `WorkerTask::on_finish`) already
    //! reverted; the replay path is where the same guarantee was missing.
    //!
    //! To SEE THESE RED (regression proof), restore the pre-fix behaviour:
    //! - delete the `revert_unacknowledged` call from
    //!   `LoadStateTask::on_finish` / `LoadStaticConfigTask::on_finish` — every
    //!   revert expectation below fails;
    //! - restore `let failed = errors > 0;` (without `|| timed_out`) in
    //!   `LoadStateTask::on_finish` — the timeout test fails on the reported
    //!   status;
    //! - make `bulk_replay_timeout` return `Timeout::None`, what both load
    //!   paths passed before — the deadline test fails.
    //! - restore `Config::load_from_path(path).unwrap_or_else(|_| panic!(...))`
    //!   in `load_static_config` — the unloadable-path test panics instead of
    //!   reporting a failure, exactly as the main process did.
    //! - drop the `skipped_invalid += 1` from `load_state`'s state-refused
    //!   branch — the skipped-tally test finds no tally line at all.
    //! - drop the `AddTcpFrontend` arm from `compute_rollback` — the TCP replay
    //!   test keeps the frontend no worker acknowledged, which is sozu#1313's
    //!   poisoned-state loop for that verb
    //!   (`rollback_inverse_targets_the_same_listener_and_skips_uncovered_verbs`
    //!   in `listener_validation_tests` fails on the same mutation). The
    //!   `AddUdpFrontend` arm inverts the same way now that
    //!   `remove_udp_frontend` mirrors its add key; see
    //!   `a_udp_frontend_add_inverts_to_its_exact_removal`.
    //!
    //! The response-accounting and channel-backpressure halves of the same fix
    //! are locked in `bin/src/command/server.rs`
    //! (`a_request_that_cannot_be_queued_is_accounted_as_a_failure`,
    //! `closing_a_worker_only_fails_its_unanswered_requests`,
    //! `a_bulk_scatter_larger_than_the_back_buffer_is_fully_delivered`):
    //! `Server::queued_tasks` and `Server::in_flight` are private to that
    //! module, so the accounting is not observable from here.
    use std::sync::Arc;

    use prost::Message as _;
    use sozu_command_lib::{
        channel::{Channel, delimiter_size},
        config::ListenerBuilder,
        proto::command::{
            PathRule, PathRuleKind, Request, RequestHttpFrontend, RequestTcpFrontend,
            RequestUdpFrontend, Response, ResponseStatus, RulePosition, SocketAddress,
            request::RequestType,
        },
    };

    use super::{
        LoadStateTask, LoadStaticConfigTask, PerEntryGatherer, Server, Timeout,
        bulk_replay_timeout, load_state, load_static_config,
    };
    use crate::command::{
        server::{CommandHub, GatheringTask, PeerCred, parse_scatter_request_id},
        sessions::ClientSession,
    };
    use mio::{Token, net::UnixListener};
    use sozu_command_lib::config::Config;
    use sozu_command_lib::proto::command::WorkerRequest;
    use std::collections::BTreeMap;
    use std::{fs::File, io::Write as _};

    fn create_test_hub() -> (CommandHub, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("Could not create temp dir");
        let socket_path = dir.path().join("test.sock");
        let unix_listener = UnixListener::bind(&socket_path).expect("Could not bind socket");
        let hub = CommandHub::new(unix_listener, Config::default(), "sozu".to_owned())
            .expect("Could not create command hub");
        // The TempDir is returned so the socket outlives the hub.
        (hub, dir)
    }

    /// A minimal `Tree`-positioned frontend add for `hostname` — same fixture
    /// shape as `frontend_validation_tests`, valid in every respect.
    fn frontend(hostname: &str, port: u16) -> RequestHttpFrontend {
        RequestHttpFrontend {
            cluster_id: Some("cluster".to_owned()),
            address: SocketAddress::new_v4(127, 0, 0, 1, port),
            hostname: hostname.to_owned(),
            path: PathRule {
                kind: PathRuleKind::Prefix as i32,
                value: "/".to_owned(),
            },
            method: None,
            position: RulePosition::Tree as i32,
            tags: BTreeMap::new(),
            redirect: None,
            redirect_scheme: None,
            redirect_template: None,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            required_auth: None,
            headers: Vec::new(),
            hsts: None,
        }
    }

    /// One entry of a fan-out: which scatter `request_id` carried it, the
    /// request itself, how many workers it was scattered to, and how many
    /// answered `Ok` / `Failure`.
    struct Fanout {
        request_id: usize,
        request: RequestType,
        expected: usize,
        ok: usize,
        errors: usize,
    }

    /// Build the gatherer state a bulk fan-out would have left behind, through
    /// the production [`Gatherer`] API: `on_scatter` for what was asked, then
    /// one real `WorkerResponse` per answer carrying the very id
    /// `Server::scatter_on` builds. Hand-filling the counters would let the
    /// fixture drift from the accounting it is meant to lock.
    fn fan_out(server: &mut Server, entries: Vec<Fanout>) -> PerEntryGatherer {
        use crate::command::server::Gatherer;
        use sozu_command_lib::proto::command::WorkerResponse;

        let mut gatherer = PerEntryGatherer::default();
        for fanout in entries {
            assert!(
                fanout.ok + fanout.errors <= fanout.expected,
                "a fan-out cannot gather more answers than it expects"
            );
            let content = Request::from(fanout.request);
            gatherer.on_scatter(fanout.request_id, fanout.expected, &content);
            let statuses = std::iter::repeat_n(ResponseStatus::Ok, fanout.ok)
                .chain(std::iter::repeat_n(ResponseStatus::Failure, fanout.errors));
            for (worker_id, status) in statuses.enumerate() {
                let worker_id = worker_id as u32;
                let response = WorkerResponse {
                    id: format!("{}-0-{}", worker_id, fanout.request_id),
                    status: status.into(),
                    message: String::from("rejected by the worker"),
                    content: None,
                };
                gatherer.on_message(server, &mut None, worker_id, response);
            }
        }
        gatherer
    }

    type ClientPair = (ClientSession, Channel<Request, Response>);

    fn test_client() -> ClientPair {
        let (client_channel, peer) =
            Channel::<Response, Request>::generate_nonblocking(4096, 40960)
                .expect("could not create a channel pair");
        let client = ClientSession::new(
            client_channel,
            0,
            Token(1),
            PeerCred {
                uid: None,
                gid: None,
                pid: None,
            },
            None,
            None,
            Arc::from("test.sock"),
        );
        (client, peer)
    }

    /// Decode every framed `Response` queued on a client's back buffer: a
    /// nonblocking `write_message` only fills that buffer (the event loop is
    /// what flushes it), so this is where `finish_ok` / `finish_failure` land.
    fn queued_responses(client: &ClientSession) -> Vec<Response> {
        let data = client.channel.back_buf.data();
        let delimiter = delimiter_size();
        let mut responses = vec![];
        let mut offset = 0usize;
        while offset + delimiter <= data.len() {
            let mut length = [0u8; std::mem::size_of::<usize>()];
            length.copy_from_slice(&data[offset..offset + delimiter]);
            let frame_len = usize::from_le_bytes(length);
            assert!(
                frame_len >= delimiter && offset + frame_len <= data.len(),
                "the client back buffer must hold whole frames"
            );
            responses.push(
                Response::decode(&data[offset + delimiter..offset + frame_len])
                    .expect("a queued client response must decode"),
            );
            offset += frame_len;
        }
        responses
    }

    #[test]
    fn an_entry_the_state_refused_is_counted_in_the_skipped_tally() {
        // `load_state` has two skip branches: one for an entry pre-dispatch
        // validation refuses, one for an entry `ConfigState` itself refuses.
        // Both `warn!` and `count!`, but only the first used to increment
        // `skipped_invalid` — and that tally is the ONLY thing the operator is
        // told, so a state file full of entries the state refused reported a
        // clean load.
        let (mut hub, _dir) = create_test_hub();
        let (mut client, _peer) = test_client();

        // The SAME frontend twice: the first dispatch commits it, the second is
        // refused by `ConfigState` with `StateError::Exists`. Both pass
        // pre-dispatch validation, so the other branch's increment — the one
        // that already worked — can not account for the tally.
        let duplicate = Request::from(RequestType::AddHttpFrontend(frontend(
            "dup.example.com",
            8080,
        )));
        let state_path = _dir.path().join("duplicate.state");
        {
            let mut state_file = File::create(&state_path).expect("a temporary state file");
            for counter in 0..2 {
                // Same framing `ConfigState::write_requests_to_file` produces.
                let message = WorkerRequest::new(format!("SAVE-{counter}"), duplicate.clone());
                let serialized = serde_json::to_string(&message).expect("a serializable request");
                state_file
                    .write_all(serialized.as_bytes())
                    .expect("writing the state entry");
                state_file
                    .write_all(b"\n\0")
                    .expect("writing the delimiter");
            }
        }

        load_state(
            &mut hub.server,
            Some(&mut client),
            state_path.to_str().expect("a UTF-8 temp path"),
        );

        let responses = queued_responses(&client);
        assert!(
            responses
                .iter()
                .any(|response| response.message.contains("skipped 1 invalid entries")),
            "the entry the state refused must be reported in the skipped tally, got {responses:?}"
        );
    }

    /// A minimal no-SNI, no-ALPN TCP frontend for `cluster_id` on `port`.
    /// Distinct ports keep two of them from colliding on the listener-wide
    /// catch-all rule `add_tcp_frontend` enforces.
    fn tcp_frontend(cluster_id: &str, port: u16) -> RequestTcpFrontend {
        RequestTcpFrontend {
            cluster_id: cluster_id.to_owned(),
            address: SocketAddress::new_v4(127, 0, 0, 1, port),
            tags: BTreeMap::new(),
            sni: None,
            alpn: Vec::new(),
        }
    }

    #[test]
    fn a_tcp_frontend_no_worker_acknowledged_is_reverted_while_an_acknowledged_one_stays() {
        // sozu#1313 for the TCP verbs: `ConfigState` dispatches
        // `RemoveTcpFrontend` exactly like `RemoveHttpFrontend`, so a TCP
        // frontend no worker took had an unambiguous inverse all along — it was
        // simply missing from `compute_rollback`, which left the poisoned-state
        // loop open for `AddTcpFrontend`. `AddUdpFrontend` stayed open too,
        // because `remove_udp_frontend` then keyed on the address alone while
        // `add_udp_frontend` keyed on (cluster, address, tags), so its inverse
        // would have evicted same-address siblings. The removal key now mirrors
        // the add key, and the verb is covered; see
        // `a_udp_frontend_add_inverts_to_its_exact_removal`.
        let (mut hub, _dir) = create_test_hub();
        let rejected = RequestType::AddTcpFrontend(tcp_frontend("rejected-cluster", 8090));
        let accepted = RequestType::AddTcpFrontend(tcp_frontend("accepted-cluster", 8091));
        for request in [&rejected, &accepted] {
            hub.server
                .state
                .dispatch(&request.clone().into())
                .expect("ConfigState records the TCP frontend");
        }

        // Entry 1: every worker refused it. Entry 2: one worker applied it —
        // `ok > 0` is the safety bound, it is never reverted.
        let task = LoadStateTask {
            client_token: None,
            gatherer: fan_out(
                &mut hub.server,
                vec![
                    Fanout {
                        request_id: 1,
                        request: rejected,
                        expected: 3,
                        ok: 0,
                        errors: 3,
                    },
                    Fanout {
                        request_id: 2,
                        request: accepted,
                        expected: 3,
                        ok: 1,
                        errors: 2,
                    },
                ],
            ),
            path: "/tmp/replayed.state".to_owned(),
        };
        Box::new(task).on_finish(&mut hub.server, &mut None, false);

        // `remove_tcp_frontend` leaves the cluster bucket in place when it
        // empties it, so count the frontends, not the buckets.
        let mut surviving: Vec<&str> = hub
            .server
            .state
            .tcp_fronts
            .iter()
            .filter(|(_, fronts)| !fronts.is_empty())
            .map(|(cluster_id, _)| cluster_id.as_str())
            .collect();
        surviving.sort_unstable();
        assert_eq!(
            surviving,
            vec!["accepted-cluster"],
            "only the TCP frontend no worker acknowledged must be reverted"
        );
    }

    /// A UDP frontend in a fixed cluster, distinguished by its `owner` tag and
    /// placed at the caller's port. `add_udp_frontend` admits one frontend per
    /// address across every cluster, so two of these must be given two ports;
    /// the tag is what a removal key coarser than the add key used to collapse.
    fn udp_frontend(owner: &str, port: u16) -> RequestUdpFrontend {
        RequestUdpFrontend {
            cluster_id: "udp-cluster".to_owned(),
            address: SocketAddress::new_v4(127, 0, 0, 1, port),
            tags: BTreeMap::from([("owner".to_owned(), owner.to_owned())]),
        }
    }

    /// The UDP twin of the TCP replay revert above, and the end-to-end half
    /// `a_udp_frontend_add_inverts_to_its_exact_removal` cannot reach: that
    /// test asserts the SHAPE [`super::compute_rollback`] returns, this one
    /// drives `revert_unacknowledged` against a real `ConfigState` and proves
    /// the inverse spares the acknowledged sibling.
    ///
    /// The two entries share a cluster and sit at DIFFERENT addresses, the
    /// only shape `add_udp_frontend` admits: a UDP listener address is the
    /// whole routing key, so one frontend holds it across every cluster.
    /// Entry 1 every worker refused and must be reverted; entry 2 one worker
    /// applied, so `ok > 0` protects it.
    ///
    /// To SEE THIS RED: widen `matches_removal` in
    /// `ConfigState::remove_udp_frontend` (`command/src/state.rs`) to the
    /// whole bucket, `|_front| true`. In a build with `debug_assertions` the
    /// production guard fires first, inside the revert's own `dispatch`:
    /// `remove_udp_frontend drops exactly one entry, left: 0, right: 1`.
    /// Strip that `debug_assert_eq!` and its companion too and the failure
    /// lands on the assertion below instead, `left: [], right: ["team-b"]`,
    /// with the task logging `reverted entries: 1`: one revert, both
    /// frontends gone. That is sozu#1313's main/worker drift reintroduced by
    /// the rollback itself, and precisely why this verb stayed out of
    /// [`super::compute_rollback`] until the removal key mirrored the add key.
    #[test]
    fn a_udp_frontend_no_worker_acknowledged_is_reverted_while_its_sibling_stays() {
        let (mut hub, _dir) = create_test_hub();
        let rejected = RequestType::AddUdpFrontend(udp_frontend("team-a", 9100));
        let accepted = RequestType::AddUdpFrontend(udp_frontend("team-b", 9101));
        for request in [&rejected, &accepted] {
            hub.server
                .state
                .dispatch(&request.clone().into())
                .expect("ConfigState records the UDP frontend");
        }

        // Entry 1: every worker refused it. Entry 2: one worker applied it —
        // `ok > 0` is the safety bound, it is never reverted.
        let task = LoadStateTask {
            client_token: None,
            gatherer: fan_out(
                &mut hub.server,
                vec![
                    Fanout {
                        request_id: 1,
                        request: rejected,
                        expected: 3,
                        ok: 0,
                        errors: 3,
                    },
                    Fanout {
                        request_id: 2,
                        request: accepted,
                        expected: 3,
                        ok: 1,
                        errors: 2,
                    },
                ],
            ),
            path: "/tmp/replayed.state".to_owned(),
        };
        Box::new(task).on_finish(&mut hub.server, &mut None, false);

        let surviving: Vec<&str> = hub
            .server
            .state
            .udp_fronts
            .get("udp-cluster")
            .map(|fronts| {
                fronts
                    .iter()
                    .filter_map(|front| front.tags.get("owner").map(String::as_str))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            surviving,
            vec!["team-b"],
            "only the UDP frontend no worker acknowledged must be reverted; its sibling in the \
             same cluster must survive"
        );
    }

    #[test]
    fn an_unloadable_static_config_path_fails_the_client_instead_of_the_main_process() {
        // `sozu reload --file <bad path>` used to take the whole main process
        // down with it — `unwrap_or_else(|_| panic!(...))` — orphaning every
        // worker over a typo in a path only the client controls.
        let (mut hub, _dir) = create_test_hub();
        let (mut client, _peer) = test_client();
        let missing = _dir.path().join("there-is-no-such-config.toml");
        let missing = missing.to_str().expect("a UTF-8 temp path");

        load_static_config(&mut hub.server, Some(&mut client), Some(missing));

        let responses = queued_responses(&client);
        assert!(
            responses
                .iter()
                .any(|response| response.status == ResponseStatus::Failure as i32
                    && response.message.contains(missing)),
            "an unloadable config path must be reported to the client as a failure, got {responses:?}"
        );
    }

    #[test]
    fn an_entry_no_worker_acknowledged_is_reverted_while_an_acknowledged_one_stays() {
        let (mut hub, _dir) = create_test_hub();
        let rejected = RequestType::AddHttpFrontend(frontend("rejected.example.com", 8080));
        let partially_accepted =
            RequestType::AddHttpFrontend(frontend("accepted.example.com", 8080));
        for request in [&rejected, &partially_accepted] {
            hub.server
                .state
                .dispatch(&request.clone().into())
                .expect("ConfigState records the frontend");
        }
        assert_eq!(
            hub.server.state.count_frontends(),
            2,
            "both replayed frontends must start in the main-process state"
        );

        // Entry 1: every worker refused it. Entry 2: one worker applied it —
        // `ok > 0` is the safety bound, it is never reverted.
        let task = LoadStateTask {
            client_token: None,
            gatherer: fan_out(
                &mut hub.server,
                vec![
                    Fanout {
                        request_id: 1,
                        request: rejected,
                        expected: 3,
                        ok: 0,
                        errors: 3,
                    },
                    Fanout {
                        request_id: 2,
                        request: partially_accepted,
                        expected: 3,
                        ok: 1,
                        errors: 2,
                    },
                ],
            ),
            path: "/tmp/replayed.state".to_owned(),
        };
        Box::new(task).on_finish(&mut hub.server, &mut None, false);

        let hostnames: Vec<&str> = hub
            .server
            .state
            .http_fronts
            .values()
            .map(|front| front.hostname.as_str())
            .collect();
        assert_eq!(
            hostnames,
            vec!["accepted.example.com"],
            "only the entry no worker acknowledged must be reverted"
        );
        assert_eq!(hub.server.state.count_frontends(), 1);
    }

    #[test]
    fn a_timed_out_replay_reverts_the_unacknowledged_entry_and_reports_failure() {
        let (mut hub, _dir) = create_test_hub();
        let unanswered = RequestType::AddHttpFrontend(frontend("silent.example.com", 8080));
        hub.server
            .state
            .dispatch(&unanswered.clone().into())
            .expect("ConfigState records the frontend");

        let (mut client, _peer) = test_client();
        // The incident shape: the deadline fired with not one answer for this
        // entry — a worker killed mid-replay answers nothing at all, so
        // `errors` alone can never express "the fleet never took it".
        let task = LoadStateTask {
            client_token: Some(client.token),
            gatherer: fan_out(
                &mut hub.server,
                vec![Fanout {
                    request_id: 1,
                    request: unanswered,
                    expected: 2,
                    ok: 0,
                    errors: 0,
                }],
            ),
            path: "/tmp/replayed.state".to_owned(),
        };
        Box::new(task).on_finish(&mut hub.server, &mut Some(&mut client), true);

        assert_eq!(
            hub.server.state.count_frontends(),
            0,
            "a replay that timed out with zero acknowledgements must revert the entry"
        );
        let responses = queued_responses(&client);
        let last = responses
            .last()
            .expect("the client must be answered exactly once");
        assert_eq!(
            last.status,
            ResponseStatus::Failure as i32,
            "a timed-out replay must be reported as a failure, never as a success: {last:?}"
        );
        assert!(
            last.message.contains("timed out: true"),
            "the failure must name the deadline: {}",
            last.message
        );
    }

    #[test]
    fn per_entry_attribution_reverts_only_the_rejected_entry() {
        let (mut hub, _dir) = create_test_hub();
        let listener = |port: u16| {
            RequestType::AddHttpListener(
                ListenerBuilder::new_http(SocketAddress::new_v4(127, 0, 0, 1, port))
                    .to_http(None)
                    .expect("default HTTP listener config"),
            )
        };
        let rejected = listener(8081);
        let accepted = listener(8082);
        for request in [&rejected, &accepted] {
            hub.server
                .state
                .dispatch(&request.clone().into())
                .expect("ConfigState records the listener");
        }
        assert_eq!(hub.server.state.list_listeners().http_listeners.len(), 2);

        // Both entries are on the SAME task and the same fleet-wide tally
        // (3 ok, 3 errors): only the per-entry breakdown can tell them apart.
        let task = LoadStateTask {
            client_token: None,
            gatherer: fan_out(
                &mut hub.server,
                vec![
                    Fanout {
                        request_id: 1,
                        request: rejected,
                        expected: 3,
                        ok: 0,
                        errors: 3,
                    },
                    Fanout {
                        request_id: 2,
                        request: accepted,
                        expected: 3,
                        ok: 3,
                        errors: 0,
                    },
                ],
            ),
            path: "/tmp/replayed.state".to_owned(),
        };
        Box::new(task).on_finish(&mut hub.server, &mut None, false);

        let listeners = hub.server.state.list_listeners();
        assert_eq!(
            listeners.http_listeners.len(),
            1,
            "exactly the unanimously rejected listener must be reverted"
        );
        assert!(
            listeners.http_listeners.contains_key("127.0.0.1:8082"),
            "the accepted listener must survive: {:?}",
            listeners.http_listeners.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_static_config_reload_reverts_an_unacknowledged_entry_too() {
        let (mut hub, _dir) = create_test_hub();
        let rejected = RequestType::AddHttpsFrontend(frontend("rejected.example.com", 8443));
        hub.server
            .state
            .dispatch(&rejected.clone().into())
            .expect("ConfigState records the frontend");

        let task = LoadStaticConfigTask {
            client_token: None,
            gatherer: fan_out(
                &mut hub.server,
                vec![Fanout {
                    request_id: 0,
                    request: rejected,
                    expected: 2,
                    ok: 0,
                    errors: 2,
                }],
            ),
        };
        Box::new(task).on_finish(&mut hub.server, &mut None, false);

        assert_eq!(
            hub.server.state.count_frontends(),
            0,
            "the reload path must revert an entry no worker acknowledged, like the replay path"
        );
    }

    #[test]
    fn only_scatter_built_response_ids_are_attributed() {
        // `scatter_on` builds exactly `{worker_id}-{task_id}-{request_id}`.
        assert_eq!(parse_scatter_request_id("2-7-42"), Some((2, 7, 42)));
        // Every other producer on the same channel must parse to None rather
        // than be attributed to some entry: `launch_new_worker`'s initial
        // status probe, and anything with a name-shaped prefix.
        for foreign in [
            "INITIAL-STATUS-0",
            "AddHttpFrontend-2-7-42",
            "Status",
            "2-7",
            "2-7-42-1",
        ] {
            assert_eq!(
                parse_scatter_request_id(foreign),
                None,
                "an id that was not built by scatter_on must not be attributed: {foreign}"
            );
        }
    }

    #[test]
    fn per_entry_tallies_follow_the_scatter_request_ids() {
        use crate::command::server::Gatherer;
        use sozu_command_lib::proto::command::WorkerResponse;

        let (mut hub, _dir) = create_test_hub();
        let mut per_entry = PerEntryGatherer::default();
        let first = Request::from(RequestType::AddHttpFrontend(frontend(
            "a.example.com",
            8080,
        )));
        let second = Request::from(RequestType::AddHttpFrontend(frontend(
            "b.example.com",
            8080,
        )));
        per_entry.on_scatter(1, 2, &first);
        per_entry.on_scatter(2, 2, &second);

        let answer = |id: &str, status: ResponseStatus| WorkerResponse {
            id: id.to_owned(),
            status: status.into(),
            message: String::new(),
            content: None,
        };
        // Entry 1 rejected by both workers, entry 2 accepted by both.
        for (id, status) in [
            ("0-3-1", ResponseStatus::Failure),
            ("1-3-1", ResponseStatus::Failure),
            ("0-3-2", ResponseStatus::Ok),
            ("1-3-2", ResponseStatus::Ok),
        ] {
            per_entry.on_message(&mut hub.server, &mut None, 0, answer(id, status));
        }

        assert!(
            per_entry.has_finished(),
            "four answers for four expected responses must finish the task"
        );
        assert_eq!(
            (per_entry.entries[&1].ok, per_entry.entries[&1].errors),
            (0, 2)
        );
        assert_eq!(
            (per_entry.entries[&2].ok, per_entry.entries[&2].errors),
            (2, 0)
        );
        assert!(
            per_entry.entries[&1].rollback.is_some(),
            "an AddHttpFrontend entry must carry its inverse"
        );
    }

    #[test]
    fn a_bulk_replay_arms_a_bounded_deadline() {
        // `Timeout::None` is what hung `sozu state load`: the task ended only
        // when every expected worker answered, and an answer that never comes
        // (a killed worker, a request that could not be queued) never ended it.
        let small = bulk_replay_timeout(10, 0);
        let large = bulk_replay_timeout(10, 100_000);
        let (small, large) = match (small, large) {
            (Timeout::Custom(small), Timeout::Custom(large)) => (small, large),
            _ => panic!("a bulk replay must arm a bounded, custom deadline"),
        };
        assert_eq!(
            small,
            std::time::Duration::from_secs(10),
            "an empty replay gets exactly one worker_timeout of slack"
        );
        assert_eq!(
            large,
            std::time::Duration::from_secs(100),
            "a huge replay is capped at 10 x worker_timeout"
        );
        assert!(
            small < large,
            "the deadline must scale with the number of scattered entries"
        );
        // `Config::default()` leaves worker_timeout at 0; a zero deadline would
        // expire the task before any worker could answer.
        assert!(
            matches!(bulk_replay_timeout(0, 0), Timeout::Custom(d) if d >= std::time::Duration::from_secs(1)),
            "a zero worker_timeout must not produce a zero deadline"
        );
    }
}

#[cfg(test)]
mod certificate_domain_filter_tests {
    //! sozu#1383: `sozu certificate list --domain <host>` reads as "which
    //! certificate would Sōzu present for this host?", but
    //! `ConfigState::get_certificates` answered it with `Vec::contains` — exact
    //! SAN equality — while the serving path resolves it through
    //! `CertificateResolver::domain_lookup`, a `TrieNode` lookup. The two
    //! disagree for every certificate not bound by an exact name, so a
    //! `*.example.com` certificate was invisible to a query for
    //! `foo.example.com` that TLS handshakes were succeeding on.
    //!
    //! To SEE THESE RED, restore the pre-fix behaviour by replacing the body of
    //! [`super::certificates_serving_domain`] with the delegation it displaced:
    //!
    //! ```ignore
    //! state.get_certificates(QueryCertificatesFilters {
    //!     domain: Some(domain.to_owned()),
    //!     fingerprint: None,
    //! })
    //! ```
    //!
    //! Five of the seven tests below then fail —
    //! `a_wildcard_certificate_answers_a_query_for_a_covered_host`,
    //! `every_listener_answers_with_its_own_certificate`,
    //! `the_query_and_the_stored_names_are_ascii_folded`,
    //! `one_listener_answers_with_exactly_one_certificate` each report an empty
    //! answer, which is sozu#1383 as filed, and
    //! `a_name_the_resolver_would_refuse_is_skipped_rather_than_indexed`
    //! answers for a name no handshake can reach, because exact equality does
    //! not care what the trie can host. The two remaining tests stay green under
    //! that mutation — they guard the fix from over-reaching, not the bug.
    //!
    //! The worker-side half of the same question is already correct and is
    //! pinned in `lib/src/https.rs`
    //! (`query_certificate_for_domain_answers_any_spelling_of_the_name`).

    use std::{collections::HashMap, net::SocketAddr};

    use sozu_command_lib::{
        certificate::Fingerprint, proto::command::CertificateAndKey, state::ConfigState,
    };

    use super::{MAX_HOSTNAME_LENGTH, certificates_serving_domain};

    /// One certificate in a fixture: its one-byte fingerprint and its names.
    type TestCertificate<'a> = (u8, &'a [&'a str]);
    /// One HTTPS listener in a fixture: its address and the certificates on it.
    type TestListener<'a> = (&'a str, &'a [TestCertificate<'a>]);

    fn certificate(names: &[&str]) -> CertificateAndKey {
        CertificateAndKey {
            names: names.iter().map(|name| (*name).to_owned()).collect(),
            ..Default::default()
        }
    }

    /// `ConfigState::add_certificate` stores the `CertificateAndKey` verbatim
    /// once `apply_overriding_names` has resolved its `names`, and — unlike
    /// `CertifiedKeyWrapper::try_from` — does not ASCII-lowercase them.
    /// Populating the public `certificates` map directly keeps these tests on
    /// the filter under test rather than on PEM parsing, and preserves that
    /// unfolded spelling.
    ///
    /// Each entry is `(listener address, [(fingerprint byte, names)])`; a
    /// one-byte fingerprint renders as its own hex, so `0xaa` keys the answer
    /// under `"aa"`.
    fn state_with(listeners: &[TestListener<'_>]) -> ConfigState {
        let mut state = ConfigState::new();
        for (address, entries) in listeners {
            let address: SocketAddr = address.parse().expect("test listener address must parse");
            let certificates: HashMap<Fingerprint, CertificateAndKey> = entries
                .iter()
                .map(|(tag, names)| (Fingerprint(vec![*tag]), certificate(names)))
                .collect();
            state.certificates.insert(address, certificates);
        }
        state
    }

    fn fingerprints_for(state: &ConfigState, domain: &str) -> Vec<String> {
        certificates_serving_domain(state, domain)
            .into_keys()
            .collect()
    }

    #[test]
    fn a_wildcard_certificate_answers_a_query_for_a_covered_host() {
        let state = state_with(&[("127.0.0.1:8443", &[(0xaa, &["*.example.com"])])]);

        assert_eq!(
            fingerprints_for(&state, "foo.example.com"),
            vec!["aa".to_owned()],
            "the *.example.com certificate serves foo.example.com, so the query must report it"
        );
    }

    #[test]
    fn a_wildcard_certificate_stays_fail_closed_outside_the_hosts_it_serves() {
        let state = state_with(&[("127.0.0.1:8443", &[(0xaa, &["*.example.com"])])]);

        // `*` is a single-label wildcard and does not cover the apex — the same
        // boundaries `keep_resolving_with_wildcard` (`lib/src/tls.rs`) asserts
        // against the resolver's own trie.
        for host in [
            "deep.foo.example.com",
            "example.com",
            "example.org",
            "fooexample.com",
        ] {
            assert!(
                fingerprints_for(&state, host).is_empty(),
                "*.example.com must not answer for {host}"
            );
        }
    }

    #[test]
    fn an_exact_name_still_answers_and_an_unrelated_host_still_does_not() {
        let state = state_with(&[(
            "127.0.0.1:8443",
            &[(0xaa, &["lolcatho.st", "www.lolcatho.st"])],
        )]);

        for host in ["lolcatho.st", "www.lolcatho.st"] {
            assert_eq!(
                fingerprints_for(&state, host),
                vec!["aa".to_owned()],
                "an exactly-named certificate must keep answering for {host}"
            );
        }
        assert!(
            fingerprints_for(&state, "other.st").is_empty(),
            "resolving through the trie must not make an unrelated host match"
        );
    }

    #[test]
    fn every_listener_answers_with_its_own_certificate() {
        let state = state_with(&[
            ("127.0.0.1:8443", &[(0xaa, &["*.example.com"])]),
            ("127.0.0.1:9443", &[(0xbb, &["*.example.com"])]),
        ]);

        assert_eq!(
            fingerprints_for(&state, "foo.example.com"),
            vec!["aa".to_owned(), "bb".to_owned()],
            "each listener owns its own resolver: a single shared trie keeps the incumbent on a \
             name collision and would drop one of these two certificates"
        );
    }

    #[test]
    fn the_query_and_the_stored_names_are_ascii_folded() {
        let state = state_with(&[("127.0.0.1:8443", &[(0xaa, &["*.Example.COM"])])]);

        for query in ["FOO.example.com", "foo.example.com", "Foo.Example.Com"] {
            assert_eq!(
                fingerprints_for(&state, query),
                vec!["aa".to_owned()],
                "the resolver lowercases certificate names and ConfigState does not, so both \
                 sides are folded here: {query} must find *.Example.COM"
            );
        }
    }

    #[test]
    fn one_listener_answers_with_exactly_one_certificate() {
        // A trie lookup resolves to one entry, so a listener carrying the same
        // name on two certificates answers with one of them — where the
        // exact-equality filter answered with both. `ConfigState` does not
        // retain `AddCertificate::expired_at`, so it cannot reproduce the
        // resolver's longest-lived tie-break; the contract is only that the
        // answer is reproducible rather than `HashMap`-order dependent.
        for _ in 0..16 {
            let state = state_with(&[(
                "127.0.0.1:8443",
                &[(0x01, &["*.example.com"]), (0x02, &["*.example.com"])],
            )]);

            assert_eq!(
                fingerprints_for(&state, "foo.example.com"),
                vec!["01".to_owned()],
                "a name carried twice on one listener must resolve to one reproducible answer"
            );
        }
    }

    #[test]
    fn a_name_the_resolver_would_refuse_is_skipped_rather_than_indexed() {
        // `CertificateResolver::add_certificate` refuses a name longer than
        // `MAX_HOSTNAME_LENGTH` because the trie recurses once per label.
        // `ConfigState` enforces no such bound, so a saved state file can carry
        // one; it must not be indexed here either, while the certificate's
        // other names keep answering.
        let too_long = format!("{}.example.com", "x".repeat(MAX_HOSTNAME_LENGTH));
        let state = state_with(&[(
            "127.0.0.1:8443",
            &[(0xaa, &[too_long.as_str(), "ok.example.com"])],
        )]);

        assert!(
            fingerprints_for(&state, &too_long).is_empty(),
            "a name past MAX_HOSTNAME_LENGTH reaches no handshake and must reach no query either"
        );
        assert_eq!(
            fingerprints_for(&state, "ok.example.com"),
            vec!["aa".to_owned()],
            "skipping one unhostable name must not hide the certificate's other names"
        );
    }
}
