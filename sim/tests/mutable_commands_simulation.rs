// Gated on `--cfg tokio_unstable`, like every simulator in this crate: without
// the flag this file compiles to an empty test binary and pulls no moonpool deps.
#![cfg(tokio_unstable)]
//! Deterministic simulation of the persisted `ConfigState` mutation surface.
//!
//! Every run starts with a fixed scenario that accepts all 30 persisted
//! `RequestType` mutation variants, including the two mTLS compatibility
//! verbs. It then runs a seeded sequence of accepted and refused operations.
//! After every operation the harness checks the public business maps directly,
//! proves a refusal changed none of them, and replays `ConfigState::diff` into
//! an empty state as a second consistency check.
//!
//! This is deliberately a control-plane model test. It does not instantiate a
//! worker, sockets, TLS providers, the operating system, or main-to-worker
//! fan-out. Real worker effects, mixed acknowledgements, and unknown outcomes
//! remain the responsibility of the process-level E2E suites.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::File,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use moonpool_sim::{
    RandomProvider, SimContext, SimulationBuilder, SimulationReport, SimulationResult, Workload,
    current_sim_seed,
};
use sozu_command_lib::{
    parser::parse_several_requests,
    proto::command::{
        ActivateListener, AddBackend, AddCertificate, CertificateAndKey, ClientAuthMode,
        ClientAuthPolicy, Cluster, DeactivateListener, HealthCheckConfig, HealthCheckHeader,
        HttpListenerConfig, HttpsListenerConfig, ListenerType, LoadBalancingAlgorithms, PathRule,
        RedirectPolicy, RemoveBackend, RemoveCertificate, RemoveListener, ReplaceCertificate,
        Request, RequestHttpFrontend, RequestTcpFrontend, RequestUdpFrontend, RulePosition,
        SetHealthCheck, SocketAddress, TcpListenerConfig, UdpListenerConfig,
        UpdateHttpListenerConfig, UpdateHttpsListenerConfig, UpdateTcpListenerConfig,
        UpdateUdpListenerConfig, WorkerRequest, request::RequestType,
    },
    state::ConfigState,
};

const DEFAULT_SEEDS: usize = 16;
const DEFAULT_STEPS: usize = 100;
const MAX_SEEDS: usize = 16;
const MAX_STEPS: usize = 100;
const RUN_BUDGET: Duration = Duration::from_secs(3_600);

const HTTP_PORT: u16 = 18_080;
const HTTPS_PORT: u16 = 18_443;
const MTLS_PORT: u16 = 18_444;
const TCP_PORT: u16 = 19_000;
const UDP_PORT: u16 = 15_353;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum MutationKind {
    AddCluster,
    RemoveCluster,
    AddHttpFrontend,
    RemoveHttpFrontend,
    AddHttpsFrontend,
    RemoveHttpsFrontend,
    AddCertificate,
    ReplaceCertificate,
    RemoveCertificate,
    AddTcpFrontend,
    RemoveTcpFrontend,
    AddBackend,
    RemoveBackend,
    AddHttpListener,
    AddHttpsListener,
    AddTcpListener,
    RemoveListener,
    ActivateListener,
    DeactivateListener,
    UpdateHttpListener,
    UpdateHttpsListener,
    UpdateTcpListener,
    SetHealthCheck,
    RemoveHealthCheck,
    AddUdpListener,
    UpdateUdpListener,
    AddUdpFrontend,
    RemoveUdpFrontend,
    AddHttpsListenerWithClientAuth,
    UpdateHttpsListenerWithClientAuth,
}

impl MutationKind {
    const ALL: [Self; 30] = [
        Self::AddCluster,
        Self::RemoveCluster,
        Self::AddHttpFrontend,
        Self::RemoveHttpFrontend,
        Self::AddHttpsFrontend,
        Self::RemoveHttpsFrontend,
        Self::AddCertificate,
        Self::ReplaceCertificate,
        Self::RemoveCertificate,
        Self::AddTcpFrontend,
        Self::RemoveTcpFrontend,
        Self::AddBackend,
        Self::RemoveBackend,
        Self::AddHttpListener,
        Self::AddHttpsListener,
        Self::AddTcpListener,
        Self::RemoveListener,
        Self::ActivateListener,
        Self::DeactivateListener,
        Self::UpdateHttpListener,
        Self::UpdateHttpsListener,
        Self::UpdateTcpListener,
        Self::SetHealthCheck,
        Self::RemoveHealthCheck,
        Self::AddUdpListener,
        Self::UpdateUdpListener,
        Self::AddUdpFrontend,
        Self::RemoveUdpFrontend,
        Self::AddHttpsListenerWithClientAuth,
        Self::UpdateHttpsListenerWithClientAuth,
    ];
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RunRecord {
    trace: Vec<String>,
    accepted: BTreeMap<MutationKind, usize>,
    refused: BTreeMap<MutationKind, usize>,
}

type RecordSink = Arc<Mutex<Vec<RunRecord>>>;

struct MutableCommandsWorkload {
    steps: usize,
    verbose: bool,
    sink: Option<RecordSink>,
}

struct Harness {
    state: ConfigState,
    record: RunRecord,
    operation_index: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            state: ConfigState::new(),
            record: RunRecord::default(),
            operation_index: 0,
        }
    }

    fn apply(&mut self, request_type: RequestType) -> bool {
        let kind = mutation_kind(&request_type);
        let before = business_state(&self.state);
        let result = self.state.dispatch(&Request::from(request_type));
        let accepted = result.is_ok();
        let counter = if accepted {
            &mut self.record.accepted
        } else {
            &mut self.record.refused
        };
        *counter.entry(kind).or_default() += 1;
        self.record.trace.push(format!(
            "{}:{kind:?}:{}",
            self.operation_index,
            if accepted { "ok" } else { "err" }
        ));

        if !accepted {
            assert_eq!(
                business_state(&self.state),
                before,
                "seed={:#x} operation={}: refused {kind:?} mutated ConfigState: {result:?}",
                current_sim_seed(),
                self.operation_index
            );
        }
        check_business_invariants(&self.state, self.operation_index);
        assert_diff_replay(&self.state, self.operation_index);
        self.operation_index += 1;
        accepted
    }

    fn apply_ok(&mut self, request_type: RequestType) {
        let kind = mutation_kind(&request_type);
        assert!(
            self.apply(request_type),
            "seed={:#x} operation={}: expected {kind:?} to be accepted",
            current_sim_seed(),
            self.operation_index.saturating_sub(1)
        );
    }

    fn apply_err(&mut self, request_type: RequestType) {
        let kind = mutation_kind(&request_type);
        assert!(
            !self.apply(request_type),
            "seed={:#x} operation={}: expected {kind:?} to be refused",
            current_sim_seed(),
            self.operation_index.saturating_sub(1)
        );
    }

    fn fixed_prefix(&mut self) {
        let alpha = cluster("alpha", "initial 503", 3);
        self.apply_ok(RequestType::AddCluster(alpha));
        self.apply_ok(RequestType::AddCluster(cluster("alpha", "updated 503", 5)));
        let stored = self.state.clusters.get("alpha").expect("alpha cluster");
        assert_eq!(stored.answer_503.as_deref(), Some("updated 503"));
        assert_eq!(stored.max_connection_attempts, Some(5));
        assert_eq!(
            stored.answers.get("429").map(String::as_str),
            Some("slow down")
        );

        let health = health_check("/ready");
        self.apply_ok(RequestType::SetHealthCheck(SetHealthCheck {
            cluster_id: "alpha".to_owned(),
            config: health.to_owned(),
        }));
        assert_eq!(
            self.state.clusters["alpha"].health_check.as_ref(),
            Some(&health)
        );
        self.apply_ok(RequestType::RemoveHealthCheck("alpha".to_owned()));
        assert!(self.state.clusters["alpha"].health_check.is_none());

        let backend_address = address(20_080);
        self.apply_ok(RequestType::AddBackend(AddBackend {
            cluster_id: "alpha".to_owned(),
            backend_id: "backend-a".to_owned(),
            address: backend_address,
            sticky_id: Some("sticky-one".to_owned()),
            backup: Some(false),
            ..Default::default()
        }));
        self.apply_ok(RequestType::AddBackend(AddBackend {
            cluster_id: "alpha".to_owned(),
            backend_id: "backend-a".to_owned(),
            address: backend_address,
            sticky_id: Some("sticky-two".to_owned()),
            backup: Some(true),
            ..Default::default()
        }));
        let backend = &self.state.backends["alpha"][0];
        assert_eq!(backend.sticky_id.as_deref(), Some("sticky-two"));
        assert_eq!(backend.backup, Some(true));
        self.apply_ok(RequestType::RemoveBackend(RemoveBackend {
            cluster_id: "alpha".to_owned(),
            backend_id: "backend-a".to_owned(),
            address: backend_address,
        }));

        let http = address(HTTP_PORT);
        let https = address(HTTPS_PORT);
        let mtls = address(MTLS_PORT);
        let tcp = address(TCP_PORT);
        let udp = address(UDP_PORT);
        self.apply_ok(RequestType::AddHttpListener(http_listener(http)));
        self.apply_ok(RequestType::UpdateHttpListener(UpdateHttpListenerConfig {
            address: http,
            front_timeout: Some(17),
            answers: [("503".to_owned(), "patched HTTP".to_owned())].into(),
            ..Default::default()
        }));
        let stored_http = listener_at(&self.state.http_listeners, http);
        assert_eq!(stored_http.front_timeout, 17);
        assert_eq!(stored_http.answers["503"], "patched HTTP");
        self.apply_ok(RequestType::ActivateListener(activate(
            http,
            ListenerType::Http,
        )));
        self.apply_ok(RequestType::DeactivateListener(deactivate(
            http,
            ListenerType::Http,
        )));
        self.apply_ok(RequestType::ActivateListener(activate(
            http,
            ListenerType::Http,
        )));

        self.apply_ok(RequestType::AddHttpsListener(https_listener(https)));
        self.apply_ok(RequestType::UpdateHttpsListener(
            UpdateHttpsListenerConfig {
                address: https,
                front_timeout: Some(18),
                strict_sni_binding: Some(true),
                ..Default::default()
            },
        ));
        let stored_https = listener_at(&self.state.https_listeners, https);
        assert_eq!(stored_https.front_timeout, 18);
        assert_eq!(stored_https.strict_sni_binding, Some(true));

        self.apply_ok(RequestType::AddHttpsListenerWithClientAuth(
            HttpsListenerConfig {
                client_auth: Some(ClientAuthMode::ClientAuthRequired as i32),
                client_ca_certificates: vec!["CA one".to_owned()],
                ..https_listener(mtls)
            },
        ));
        self.apply_ok(RequestType::UpdateHttpsListenerWithClientAuth(
            UpdateHttpsListenerConfig {
                address: mtls,
                client_auth_policy: Some(ClientAuthPolicy {
                    client_auth: Some(ClientAuthMode::ClientAuthRequired as i32),
                    client_ca_certificates: vec!["CA two".to_owned()],
                    client_ca_crls: vec!["CRL one".to_owned()],
                }),
                ..Default::default()
            },
        ));
        let stored_mtls = listener_at(&self.state.https_listeners, mtls);
        assert_eq!(stored_mtls.client_ca_certificates, vec!["CA two"]);
        assert_eq!(stored_mtls.client_ca_crls, vec!["CRL one"]);

        self.apply_ok(RequestType::AddTcpListener(tcp_listener(tcp)));
        self.apply_ok(RequestType::UpdateTcpListener(UpdateTcpListenerConfig {
            address: tcp,
            front_timeout: Some(19),
            ..Default::default()
        }));
        assert_eq!(
            listener_at(&self.state.tcp_listeners, tcp).front_timeout,
            19
        );

        self.apply_ok(RequestType::AddUdpListener(udp_listener(udp)));
        self.apply_ok(RequestType::UpdateUdpListener(UpdateUdpListenerConfig {
            address: udp,
            max_rx_datagram_size: Some(1_200),
            max_flows: Some(77),
            ..Default::default()
        }));
        let stored_udp = listener_at(&self.state.udp_listeners, udp);
        assert_eq!(stored_udp.max_rx_datagram_size, 1_200);
        assert_eq!(stored_udp.max_flows, 77);

        let http_route = http_front("alpha", http, "http.example", "/api");
        self.apply_ok(RequestType::AddHttpFrontend(http_route.to_owned()));
        let stored = self
            .state
            .http_fronts
            .values()
            .find(|front| front.hostname == "http.example")
            .expect("HTTP frontend");
        assert_eq!(stored.method.as_deref(), Some("PATCH"));
        assert_eq!(stored.path, PathRule::prefix("/api".to_owned()));
        assert_eq!(stored.position, RulePosition::Pre);
        assert_eq!(
            stored.tags.as_ref().and_then(|tags| tags.get("owner")),
            Some(&"simulation".to_owned())
        );
        assert_eq!(stored.redirect, Some(RedirectPolicy::Forward as i32));
        self.apply_ok(RequestType::RemoveHttpFrontend(http_route));

        let https_front = http_front("alpha", https, "https.example", "/secure");
        self.apply_ok(RequestType::AddHttpsFrontend(https_front.to_owned()));
        assert!(
            self.state
                .https_fronts
                .values()
                .any(|front| front.hostname == "https.example"
                    && front.method.as_deref() == Some("PATCH"))
        );
        self.apply_ok(RequestType::RemoveHttpsFrontend(https_front));

        let tcp_front = RequestTcpFrontend {
            cluster_id: "alpha".to_owned(),
            address: tcp,
            tags: [("owner".to_owned(), "tcp".to_owned())].into(),
            sni: Some("API.EXAMPLE".to_owned()),
            alpn: vec!["h2".to_owned(), "http/1.1".to_owned(), "h2".to_owned()],
        };
        self.apply_ok(RequestType::AddTcpFrontend(tcp_front.to_owned()));
        let stored = &self.state.tcp_fronts["alpha"][0];
        assert_eq!(stored.sni.as_deref(), Some("api.example"));
        assert_eq!(stored.alpn, vec!["h2", "http/1.1"]);
        assert_eq!(stored.tags["owner"], "tcp");
        self.apply_ok(RequestType::RemoveTcpFrontend(tcp_front));

        let udp_front = RequestUdpFrontend {
            cluster_id: "alpha".to_owned(),
            address: udp,
            tags: [("owner".to_owned(), "udp".to_owned())].into(),
        };
        self.apply_ok(RequestType::AddUdpFrontend(udp_front.to_owned()));
        assert_eq!(self.state.udp_fronts["alpha"][0].tags["owner"], "udp");
        self.apply_ok(RequestType::RemoveUdpFrontend(udp_front));

        let certificate = certificate();
        let fingerprint = certificate
            .fingerprint()
            .expect("fixture certificate fingerprint")
            .to_string();
        self.apply_ok(RequestType::AddCertificate(AddCertificate {
            address: https,
            certificate: certificate.to_owned(),
            expired_at: Some(2_000_000_000),
        }));
        assert!(
            self.state.certificates[&SocketAddr::from(https)]
                .keys()
                .any(|stored| stored.to_string() == fingerprint)
        );
        self.apply_ok(RequestType::ReplaceCertificate(ReplaceCertificate {
            address: https,
            new_certificate: certificate,
            old_fingerprint: fingerprint.to_owned(),
            new_expired_at: Some(2_000_000_001),
        }));
        assert_eq!(self.state.certificates[&SocketAddr::from(https)].len(), 1);
        self.apply_ok(RequestType::RemoveCertificate(RemoveCertificate {
            address: https,
            fingerprint,
        }));
        assert!(
            !self
                .state
                .certificates
                .contains_key(&SocketAddr::from(https))
        );

        let temporary_udp = address(UDP_PORT + 1);
        self.apply_ok(RequestType::AddUdpListener(udp_listener(temporary_udp)));
        self.apply_ok(RequestType::RemoveListener(RemoveListener {
            address: temporary_udp,
            proxy: ListenerType::Udp.into(),
            interface: None,
        }));
        assert!(
            !self
                .state
                .udp_listeners
                .values()
                .any(|listener| listener.address == temporary_udp)
        );

        self.exercise_refusals(http, udp, https);
        self.exercise_cluster_cascade(tcp, udp);

        let accepted: BTreeSet<_> = self.record.accepted.keys().copied().collect();
        let expected: BTreeSet<_> = MutationKind::ALL.into_iter().collect();
        assert_eq!(
            accepted,
            expected,
            "seed={:#x}: fixed prefix must accept every persisted mutator exactly by verb",
            current_sim_seed()
        );
    }

    fn exercise_refusals(&mut self, http: SocketAddress, udp: SocketAddress, https: SocketAddress) {
        self.apply_err(RequestType::AddHttpListener(http_listener(http)));
        self.apply_err(RequestType::RemoveBackend(RemoveBackend {
            cluster_id: "alpha".to_owned(),
            backend_id: "absent".to_owned(),
            address: address(29_999),
        }));
        self.apply_err(RequestType::UpdateHttpListener(UpdateHttpListenerConfig {
            address: http,
            front_timeout: Some(999),
            h2_max_ping_per_window: Some(0),
            ..Default::default()
        }));
        assert_eq!(
            listener_at(&self.state.http_listeners, http).front_timeout,
            17
        );

        let first = RequestUdpFrontend {
            cluster_id: "alpha".to_owned(),
            address: udp,
            tags: [("owner".to_owned(), "one".to_owned())].into(),
        };
        self.apply_ok(RequestType::AddUdpFrontend(first.to_owned()));
        self.apply_err(RequestType::AddUdpFrontend(RequestUdpFrontend {
            cluster_id: "other".to_owned(),
            address: udp,
            tags: [("owner".to_owned(), "two".to_owned())].into(),
        }));
        assert_eq!(self.state.udp_fronts["alpha"][0].tags["owner"], "one");
        self.apply_ok(RequestType::RemoveUdpFrontend(first));

        let certificate = certificate();
        let fingerprint = certificate
            .fingerprint()
            .expect("fixture certificate fingerprint")
            .to_string();
        self.apply_ok(RequestType::AddCertificate(AddCertificate {
            address: https,
            certificate,
            expired_at: None,
        }));
        self.apply_err(RequestType::ReplaceCertificate(ReplaceCertificate {
            address: https,
            new_certificate: CertificateAndKey {
                certificate: "not PEM".to_owned(),
                key: include_str!("../../command/assets/key.pem").to_owned(),
                ..Default::default()
            },
            old_fingerprint: fingerprint.to_owned(),
            new_expired_at: None,
        }));
        assert!(
            self.state.certificates[&SocketAddr::from(https)]
                .keys()
                .any(|stored| stored.to_string() == fingerprint)
        );
        self.apply_ok(RequestType::RemoveCertificate(RemoveCertificate {
            address: https,
            fingerprint,
        }));
        self.apply_err(RequestType::RemoveCluster("absent".to_owned()));
    }

    fn exercise_cluster_cascade(&mut self, tcp: SocketAddress, udp: SocketAddress) {
        self.apply_ok(RequestType::AddCluster(cluster("doomed", "gone", 2)));
        self.apply_ok(RequestType::AddBackend(AddBackend {
            cluster_id: "doomed".to_owned(),
            backend_id: "doomed-backend".to_owned(),
            address: address(20_081),
            ..Default::default()
        }));
        self.apply_ok(RequestType::AddTcpFrontend(RequestTcpFrontend {
            cluster_id: "doomed".to_owned(),
            address: tcp,
            tags: [("cascade".to_owned(), "tcp".to_owned())].into(),
            sni: Some("doomed.example".to_owned()),
            alpn: Vec::new(),
        }));
        self.apply_ok(RequestType::AddUdpFrontend(RequestUdpFrontend {
            cluster_id: "doomed".to_owned(),
            address: udp,
            tags: [("cascade".to_owned(), "udp".to_owned())].into(),
        }));
        self.apply_ok(RequestType::RemoveCluster("doomed".to_owned()));
        assert!(!self.state.clusters.contains_key("doomed"));
        assert!(!self.state.backends.contains_key("doomed"));
        assert!(!self.state.tcp_fronts.contains_key("doomed"));
        assert!(!self.state.udp_fronts.contains_key("doomed"));
    }

    fn random_step(&mut self, ctx: &SimContext, step: usize) {
        let slot = ctx.random().random_range(0..4u16);
        let cluster_id = format!("random-{slot}");
        let listener = address(24_000 + slot);
        let backend = address(25_000 + slot);
        let request = match ctx.random().random_range(0..15u8) {
            0 => RequestType::AddCluster(cluster(
                &cluster_id,
                &format!("seed {} step {step}", current_sim_seed()),
                1 + u32::from(slot),
            )),
            1 => RequestType::RemoveCluster(cluster_id.to_owned()),
            2 => RequestType::AddBackend(AddBackend {
                cluster_id: cluster_id.to_owned(),
                backend_id: format!("backend-{slot}"),
                address: backend,
                sticky_id: Some(format!("sticky-{step}")),
                backup: Some(step.is_multiple_of(2)),
                ..Default::default()
            }),
            3 => RequestType::RemoveBackend(RemoveBackend {
                cluster_id: cluster_id.to_owned(),
                backend_id: format!("backend-{slot}"),
                address: backend,
            }),
            4 => RequestType::SetHealthCheck(SetHealthCheck {
                cluster_id: cluster_id.to_owned(),
                config: health_check(&format!("/health/{step}")),
            }),
            5 => RequestType::RemoveHealthCheck(cluster_id.to_owned()),
            6 => RequestType::AddUdpListener(udp_listener(listener)),
            7 => RequestType::UpdateUdpListener(UpdateUdpListenerConfig {
                address: listener,
                max_flows: Some(step as u32 + 1),
                ..Default::default()
            }),
            8 => RequestType::ActivateListener(activate(listener, ListenerType::Udp)),
            9 => RequestType::DeactivateListener(deactivate(listener, ListenerType::Udp)),
            10 => RequestType::RemoveListener(RemoveListener {
                address: listener,
                proxy: ListenerType::Udp.into(),
                interface: None,
            }),
            11 => RequestType::AddUdpFrontend(RequestUdpFrontend {
                cluster_id: cluster_id.to_owned(),
                address: listener,
                tags: [("step".to_owned(), step.to_string())].into(),
            }),
            12 => RequestType::RemoveUdpFrontend(RequestUdpFrontend {
                cluster_id: cluster_id.to_owned(),
                address: listener,
                tags: [("step".to_owned(), step.saturating_sub(1).to_string())].into(),
            }),
            13 => RequestType::AddHttpFrontend(http_front(
                &cluster_id,
                address(26_000 + slot),
                &format!("random-{slot}.example"),
                &format!("/{step}"),
            )),
            _ => RequestType::RemoveHttpFrontend(http_front(
                &cluster_id,
                address(26_000 + slot),
                &format!("random-{slot}.example"),
                &format!("/{}", step.saturating_sub(1)),
            )),
        };
        self.apply(request);
    }
}

#[async_trait]
impl Workload for MutableCommandsWorkload {
    fn name(&self) -> &'static str {
        "mutable_commands_simulation"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let mut harness = Harness::new();
        harness.fixed_prefix();
        for step in 0..self.steps {
            harness.random_step(ctx, step);
        }

        if self.verbose {
            eprintln!("{}", harness.record.trace.join("\n"));
        }
        if let Some(sink) = &self.sink {
            sink.lock()
                .expect("record sink is not poisoned")
                .push(harness.record);
        }
        Ok(())
    }
}

fn mutation_kind(request: &RequestType) -> MutationKind {
    match request {
        RequestType::AddCluster(_) => MutationKind::AddCluster,
        RequestType::RemoveCluster(_) => MutationKind::RemoveCluster,
        RequestType::AddHttpFrontend(_) => MutationKind::AddHttpFrontend,
        RequestType::RemoveHttpFrontend(_) => MutationKind::RemoveHttpFrontend,
        RequestType::AddHttpsFrontend(_) => MutationKind::AddHttpsFrontend,
        RequestType::RemoveHttpsFrontend(_) => MutationKind::RemoveHttpsFrontend,
        RequestType::AddCertificate(_) => MutationKind::AddCertificate,
        RequestType::ReplaceCertificate(_) => MutationKind::ReplaceCertificate,
        RequestType::RemoveCertificate(_) => MutationKind::RemoveCertificate,
        RequestType::AddTcpFrontend(_) => MutationKind::AddTcpFrontend,
        RequestType::RemoveTcpFrontend(_) => MutationKind::RemoveTcpFrontend,
        RequestType::AddBackend(_) => MutationKind::AddBackend,
        RequestType::RemoveBackend(_) => MutationKind::RemoveBackend,
        RequestType::AddHttpListener(_) => MutationKind::AddHttpListener,
        RequestType::AddHttpsListener(_) => MutationKind::AddHttpsListener,
        RequestType::AddTcpListener(_) => MutationKind::AddTcpListener,
        RequestType::RemoveListener(_) => MutationKind::RemoveListener,
        RequestType::ActivateListener(_) => MutationKind::ActivateListener,
        RequestType::DeactivateListener(_) => MutationKind::DeactivateListener,
        RequestType::UpdateHttpListener(_) => MutationKind::UpdateHttpListener,
        RequestType::UpdateHttpsListener(_) => MutationKind::UpdateHttpsListener,
        RequestType::UpdateTcpListener(_) => MutationKind::UpdateTcpListener,
        RequestType::SetHealthCheck(_) => MutationKind::SetHealthCheck,
        RequestType::RemoveHealthCheck(_) => MutationKind::RemoveHealthCheck,
        RequestType::AddUdpListener(_) => MutationKind::AddUdpListener,
        RequestType::UpdateUdpListener(_) => MutationKind::UpdateUdpListener,
        RequestType::AddUdpFrontend(_) => MutationKind::AddUdpFrontend,
        RequestType::RemoveUdpFrontend(_) => MutationKind::RemoveUdpFrontend,
        RequestType::AddHttpsListenerWithClientAuth(_) => {
            MutationKind::AddHttpsListenerWithClientAuth
        }
        RequestType::UpdateHttpsListenerWithClientAuth(_) => {
            MutationKind::UpdateHttpsListenerWithClientAuth
        }
        other => panic!("non-persisted request in mutable-command simulator: {other:?}"),
    }
}

fn business_state(state: &ConfigState) -> ConfigState {
    let mut normalized = ConfigState {
        request_counts: BTreeMap::new(),
        ..state.to_owned()
    };
    // Empty Vec buckets are an internal removal artifact, not an object the
    // persisted request stream can express. Compare the replayable business
    // model rather than HashMap/BTreeMap allocation history.
    normalized
        .backends
        .retain(|_, backends| !backends.is_empty());
    normalized.tcp_fronts.retain(|_, fronts| !fronts.is_empty());
    normalized.udp_fronts.retain(|_, fronts| !fronts.is_empty());
    normalized
}

fn check_business_invariants(state: &ConfigState, operation_index: usize) {
    let context = || format!("seed={:#x} operation={operation_index}", current_sim_seed());
    for (key, cluster) in &state.clusters {
        assert_eq!(
            key.as_ref(),
            cluster.cluster_id,
            "{} cluster key",
            context()
        );
    }
    for (key, backends) in &state.backends {
        let unique: HashSet<_> = backends
            .iter()
            .map(|backend| (&backend.backend_id, backend.address))
            .collect();
        assert_eq!(
            unique.len(),
            backends.len(),
            "{} duplicate backend",
            context()
        );
        assert!(
            backends.iter().all(|backend| &backend.cluster_id == key),
            "{} backend bucket mismatch",
            context()
        );
    }
    for (key, listener) in &state.http_listeners {
        assert_eq!(
            *key,
            listener.listener_key(),
            "{} HTTP listener key",
            context()
        );
    }
    for (key, listener) in &state.https_listeners {
        assert_eq!(
            *key,
            listener.listener_key(),
            "{} HTTPS listener key",
            context()
        );
    }
    for (key, listener) in &state.tcp_listeners {
        assert_eq!(
            *key,
            listener.listener_key(),
            "{} TCP listener key",
            context()
        );
    }
    for (key, listener) in &state.udp_listeners {
        assert_eq!(
            *key,
            listener.listener_key(),
            "{} UDP listener key",
            context()
        );
    }
    for (key, front) in &state.http_fronts {
        assert_eq!(
            key,
            &RequestHttpFrontend::from(front.to_owned()).to_string(),
            "{} HTTP frontend key",
            context()
        );
    }
    for (key, front) in &state.https_fronts {
        assert_eq!(
            key,
            &RequestHttpFrontend::from(front.to_owned()).to_string(),
            "{} HTTPS frontend key",
            context()
        );
    }
    for (key, fronts) in &state.tcp_fronts {
        assert!(
            fronts.iter().all(|front| &front.cluster_id == key),
            "{} TCP frontend bucket mismatch",
            context()
        );
        assert_eq!(
            fronts.iter().collect::<HashSet<_>>().len(),
            fronts.len(),
            "{} duplicate TCP frontend",
            context()
        );
    }
    let udp_addresses: HashSet<_> = state
        .udp_fronts
        .values()
        .flatten()
        .map(|front| front.address)
        .collect();
    let udp_count: usize = state.udp_fronts.values().map(Vec::len).sum();
    assert_eq!(
        udp_addresses.len(),
        udp_count,
        "{} UDP addresses must be globally unique",
        context()
    );
    for (key, fronts) in &state.udp_fronts {
        assert!(
            fronts.iter().all(|front| &front.cluster_id == key),
            "{} UDP frontend bucket mismatch",
            context()
        );
    }
    assert!(
        state.certificates.values().all(|certs| !certs.is_empty()),
        "{} certificate buckets must be non-empty",
        context()
    );
}

fn assert_diff_replay(state: &ConfigState, operation_index: usize) {
    let expected = business_state(state);
    let mut replayed = ConfigState::new();
    for request in ConfigState::new().diff(state) {
        replayed.dispatch(&request).unwrap_or_else(|error| {
            panic!(
                "seed={:#x} operation={operation_index}: diff replay refused {request:?}: {error}",
                current_sim_seed()
            )
        });
    }
    assert_eq!(
        business_state(&replayed),
        expected,
        "seed={:#x} operation={operation_index}: diff replay changed a business map",
        current_sim_seed()
    );
}

fn cluster(cluster_id: &str, answer_503: &str, max_attempts: u32) -> Cluster {
    Cluster {
        cluster_id: cluster_id.to_owned(),
        sticky_session: false,
        https_redirect: false,
        load_balancing: LoadBalancingAlgorithms::RoundRobin.into(),
        answer_503: Some(answer_503.to_owned()),
        answers: [("429".to_owned(), "slow down".to_owned())].into(),
        max_connection_attempts: Some(max_attempts),
        ..Default::default()
    }
}

fn health_check(uri: &str) -> HealthCheckConfig {
    HealthCheckConfig {
        uri: uri.to_owned(),
        interval: 10,
        timeout: 3,
        healthy_threshold: 2,
        unhealthy_threshold: 2,
        expected_status: 200,
        ..Default::default()
    }
}

fn address(port: u16) -> SocketAddress {
    SocketAddress::from(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
}

fn http_listener(address: SocketAddress) -> HttpListenerConfig {
    HttpListenerConfig {
        address,
        sticky_name: "SOZUBALANCEID".to_owned(),
        front_timeout: 60,
        back_timeout: 30,
        connect_timeout: 3,
        request_timeout: 10,
        ..Default::default()
    }
}

fn https_listener(address: SocketAddress) -> HttpsListenerConfig {
    HttpsListenerConfig {
        address,
        sticky_name: "SOZUBALANCEID".to_owned(),
        front_timeout: 60,
        back_timeout: 30,
        connect_timeout: 3,
        request_timeout: 10,
        send_tls13_tickets: 4,
        ..Default::default()
    }
}

fn tcp_listener(address: SocketAddress) -> TcpListenerConfig {
    TcpListenerConfig {
        address,
        front_timeout: 60,
        back_timeout: 30,
        connect_timeout: 3,
        ..Default::default()
    }
}

fn udp_listener(address: SocketAddress) -> UdpListenerConfig {
    UdpListenerConfig {
        address,
        front_timeout: 30,
        back_timeout: 30,
        max_rx_datagram_size: 1_500,
        max_flows: 0,
        ..Default::default()
    }
}

fn activate(address: SocketAddress, listener_type: ListenerType) -> ActivateListener {
    ActivateListener {
        address,
        proxy: listener_type.into(),
        from_scm: false,
        interface: None,
    }
}

fn deactivate(address: SocketAddress, listener_type: ListenerType) -> DeactivateListener {
    DeactivateListener {
        address,
        proxy: listener_type.into(),
        to_scm: false,
        interface: None,
    }
}

fn http_front(
    cluster_id: &str,
    address: SocketAddress,
    hostname: &str,
    path: &str,
) -> RequestHttpFrontend {
    RequestHttpFrontend {
        cluster_id: Some(cluster_id.to_owned()),
        address,
        hostname: hostname.to_owned(),
        path: PathRule::prefix(path.to_owned()),
        method: Some("PATCH".to_owned()),
        position: RulePosition::Pre.into(),
        tags: [("owner".to_owned(), "simulation".to_owned())].into(),
        redirect: Some(RedirectPolicy::Forward.into()),
        rewrite_host: Some("backend.internal".to_owned()),
        rewrite_path: Some("/rewritten".to_owned()),
        rewrite_port: Some(8_080),
        ..Default::default()
    }
}

fn certificate() -> CertificateAndKey {
    CertificateAndKey {
        certificate: include_str!("../../command/assets/certificate.pem").to_owned(),
        key: include_str!("../../command/assets/key.pem").to_owned(),
        names: vec!["simulation.example".to_owned()],
        ..Default::default()
    }
}

fn tied_certificates() -> (CertificateAndKey, CertificateAndKey) {
    (
        CertificateAndKey {
            certificate: include_str!("../../lib/assets/tests/certificate-1y.pem").to_owned(),
            key: include_str!("../../lib/assets/tests/key-1y.pem").to_owned(),
            names: vec!["tie.example.test".to_owned()],
            ..Default::default()
        },
        CertificateAndKey {
            certificate: include_str!("../../lib/assets/tests/certificate-2y.pem").to_owned(),
            key: include_str!("../../lib/assets/tests/key-2y.pem").to_owned(),
            names: vec!["tie.example.test".to_owned()],
            ..Default::default()
        },
    )
}

fn listener_at<T>(
    listeners: &BTreeMap<sozu_command_lib::listener_key::ListenerKey, T>,
    address: SocketAddress,
) -> &T
where
    T: ListenerAddress,
{
    listeners
        .values()
        .find(|listener| listener.address() == address)
        .expect("listener at address")
}

trait ListenerAddress {
    fn address(&self) -> SocketAddress;
}

impl ListenerAddress for HttpListenerConfig {
    fn address(&self) -> SocketAddress {
        self.address
    }
}

impl ListenerAddress for HttpsListenerConfig {
    fn address(&self) -> SocketAddress {
        self.address
    }
}

impl ListenerAddress for TcpListenerConfig {
    fn address(&self) -> SocketAddress {
        self.address
    }
}

impl ListenerAddress for UdpListenerConfig {
    fn address(&self) -> SocketAddress {
        self.address
    }
}

fn parse_u64(value: &str) -> Option<u64> {
    let value = value.trim();
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or_else(
            || value.parse().ok(),
            |hex| u64::from_str_radix(hex, 16).ok(),
        )
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|value| parse_u64(&value))
}

fn env_usize(key: &str) -> Option<usize> {
    env_u64(key).and_then(|value| usize::try_from(value).ok())
}

fn assert_no_failures(report: &SimulationReport) {
    if report.failed_runs != 0 {
        let errors: Vec<_> = report
            .individual_metrics
            .iter()
            .filter_map(|result| result.as_ref().err().map(|error| format!("{error:?}")))
            .collect();
        panic!(
            "failed_runs={} seeds_failing={:?}\nerrors:\n{}",
            report.failed_runs,
            report.seeds_failing,
            errors.join("\n")
        );
    }
}

fn run(seed: u64, steps: usize, verbose: bool) -> RunRecord {
    let sink: RecordSink = Arc::new(Mutex::new(Vec::new()));
    let report = SimulationBuilder::new()
        .workload(MutableCommandsWorkload {
            steps,
            verbose,
            sink: Some(Arc::clone(&sink)),
        })
        .set_debug_seeds(vec![seed])
        .set_iterations(1)
        .run_time_budget(RUN_BUDGET)
        .run();
    assert_no_failures(&report);
    sink.lock()
        .expect("record sink is not poisoned")
        .first()
        .cloned()
        .expect("workload recorded its run")
}

#[test]
fn mutable_commands_fixed_scenario_covers_every_persisted_mutator() {
    let record = run(0x2301, 0, false);
    assert_eq!(record.accepted.len(), MutationKind::ALL.len());
    assert!(record.refused.len() >= 5, "fixed refusal branches ran");
}

#[test]
fn certificate_expiration_and_tie_order_survive_diff_and_partial_recovery() {
    let listener = address(HTTPS_PORT);
    let socket_address: SocketAddr = listener.into();
    let (certificate_a, certificate_b) = tied_certificates();
    let fingerprint_a = certificate_a.fingerprint().expect("fingerprint A");
    let fingerprint_b = certificate_b.fingerprint().expect("fingerprint B");
    let expiration = Some(2_100_000_000);

    let build = |certificates: [CertificateAndKey; 2]| {
        let mut state = ConfigState::new();
        for certificate in certificates {
            state
                .dispatch(
                    &RequestType::AddCertificate(AddCertificate {
                        address: listener,
                        certificate,
                        expired_at: expiration,
                    })
                    .into(),
                )
                .expect("certificate accepted by the model");
        }
        state
    };
    let source = build([certificate_a.to_owned(), certificate_b.to_owned()]);
    let target = build([certificate_b, certificate_a]);

    let source_entries = source.ordered_certificate_entries(socket_address);
    assert_eq!(
        source_entries
            .iter()
            .map(|(fingerprint, _, metadata)| {
                (
                    fingerprint.to_owned(),
                    metadata.expired_at,
                    metadata.insertion_order,
                )
            })
            .collect::<Vec<_>>(),
        vec![
            (fingerprint_a.to_owned(), expiration, 0),
            (fingerprint_b.to_owned(), expiration, 1)
        ],
        "the source model must carry a non-empty equal-expiration tie and its insertion order"
    );
    assert_eq!(
        source_entries.last().map(|(fingerprint, _, _)| fingerprint),
        Some(&fingerprint_b),
        "workers choose the later insertion when expiration ties"
    );
    assert_eq!(
        source
            .certificate_add_requests_for_address(socket_address)
            .into_iter()
            .map(|add| add.expired_at)
            .collect::<Vec<_>>(),
        vec![expiration, expiration],
        "the persisted request stream must carry both non-empty expiration overrides"
    );

    let sequence = source.diff(&target);
    assert_eq!(
        sequence.len(),
        2,
        "A,B -> B,A must move only A with one Remove/Add pair: {sequence:?}"
    );
    assert!(
        matches!(
            &sequence[0].request_type,
            Some(RequestType::RemoveCertificate(remove))
                if remove.fingerprint == fingerprint_a.to_string()
        ) && matches!(
            &sequence[1].request_type,
            Some(RequestType::AddCertificate(add))
                if add.certificate.fingerprint().ok().as_ref() == Some(&fingerprint_a)
        ),
        "the minimal order-only sequence must move only certificate A: {sequence:?}"
    );
    let mut interrupted = source.to_owned();
    for request in sequence.iter().take(1) {
        interrupted
            .dispatch(request)
            .expect("the first order-convergence request applies");
    }
    assert_ne!(
        business_state(&interrupted),
        business_state(&target),
        "the interrupted sequence must expose a real partial state"
    );

    for request in interrupted.diff(&target) {
        interrupted
            .dispatch(&request)
            .expect("replaying from a partial sequence converges");
    }
    assert_eq!(
        business_state(&interrupted),
        business_state(&target),
        "a fresh diff must recover payload, expiry and order after interruption"
    );
    let target_entries = interrupted.ordered_certificate_entries(socket_address);
    assert_eq!(
        target_entries.last().map(|(fingerprint, _, _)| fingerprint),
        Some(&fingerprint_a),
        "recovery must restore the target worker winner, not only metadata cardinality"
    );
}

/// Write `state` with `ConfigState::write_requests_to_file`, read the exact
/// bytes back, parse them and dispatch them into a fresh state.
fn save_and_replay(state: &ConfigState) -> ConfigState {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("state.json");
    let mut file = File::create(&path).expect("create the state file");
    let written = state
        .write_requests_to_file(&mut file)
        .expect("SaveState writes the state");
    drop(file);
    let mut saved = Vec::new();
    File::open(&path)
        .expect("open the state file")
        .read_to_end(&mut saved)
        .expect("read the state file");
    let (remainder, requests) =
        parse_several_requests::<WorkerRequest>(&saved).expect("the state file parses");
    assert!(remainder.is_empty(), "the whole state file must parse");
    assert_eq!(requests.len(), written);
    // Compare what the file carries, not only what replay rebuilds: a dropped
    // or reordered health-check header must fail here, before dispatch.
    let parsed_clusters: Vec<&Cluster> = requests
        .iter()
        .filter_map(|request| match &request.content.request_type {
            Some(RequestType::AddCluster(cluster)) => Some(cluster),
            _ => None,
        })
        .collect();
    assert_eq!(
        parsed_clusters.len(),
        state.clusters.len(),
        "the state file must carry exactly one AddCluster per source cluster"
    );
    assert_eq!(
        parsed_clusters
            .iter()
            .map(|cluster| cluster.cluster_id.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        parsed_clusters.len(),
        "the state file must not repeat an AddCluster"
    );
    for parsed in parsed_clusters {
        match state.clusters.get(parsed.cluster_id.as_str()) {
            Some(source) => assert_eq!(
                parsed, source,
                "the parsed AddCluster for {:?} differs from the source cluster \
                 (health check and ordered headers included)",
                parsed.cluster_id
            ),
            None => panic!(
                "the state file carries an AddCluster for {:?} absent from the source",
                parsed.cluster_id
            ),
        }
    }
    let mut replayed = ConfigState::new();
    for request in &requests {
        replayed
            .dispatch(&request.content)
            .unwrap_or_else(|error| panic!("replay refused {request:?}: {error}"));
    }
    replayed
}

/// Header metadata (name and main-process fingerprint, never a value) must
/// reach the saved-state file in order and survive replay through add,
/// removal and replacement of a cluster health check, without disturbing an
/// unrelated listener, frontend and backend. Metadata and state replay only:
/// no clear value, worker acknowledgement or protected verb is involved.
#[test]
fn health_check_header_metadata_survives_save_state_replay() {
    let header = |name: &str, tag: u128| HealthCheckHeader {
        name: name.to_owned(),
        fingerprint: format!("sozu-hc-hmac-sha256:v1:{:032x}:{:064x}", 9, tag),
    };
    let probe = |headers: Vec<HealthCheckHeader>| HealthCheckConfig {
        headers,
        ..health_check("/probe")
    };
    let initial = vec![
        header("Z-Last", 3),
        header("Authorization", 1),
        header("A-First", 2),
    ];
    let replacement = vec![header("X-Second", 5), header("B-First", 4)];
    assert!(
        initial.windows(2).any(|pair| pair[0].name > pair[1].name),
        "the initial order must not be sorted"
    );

    let http = address(HTTP_PORT);
    let backend_address = address(20_082);
    let mut state = ConfigState::new();
    let apply = |state: &mut ConfigState, request_type: RequestType| {
        state
            .dispatch(&request_type.into())
            .unwrap_or_else(|error| panic!("the model refused a request: {error}"));
    };
    let headers_of = |state: &ConfigState| {
        state.clusters["meta"]
            .health_check
            .as_ref()
            .map(|config| config.headers.to_owned())
    };
    let unrelated = |state: &ConfigState| {
        (
            state.http_listeners.to_owned(),
            state.http_fronts.to_owned(),
            state.backends.to_owned(),
        )
    };
    // Save, parse and replay `state`, then require the exact ordered header
    // metadata and the exact unrelated listener, frontend and backend state.
    let round_trip = |state: &ConfigState,
                      expected: Option<Vec<HealthCheckHeader>>,
                      stage: &str|
     -> ConfigState {
        let replayed = save_and_replay(state);
        assert_eq!(
            business_state(&replayed),
            business_state(state),
            "{stage}: replay must rebuild the saved state"
        );
        assert_eq!(
            headers_of(&replayed),
            expected,
            "{stage}: ordered header metadata must survive replay"
        );
        assert_eq!(
            unrelated(&replayed),
            unrelated(state),
            "{stage}: unrelated state must survive replay"
        );
        replayed
    };

    apply(
        &mut state,
        RequestType::AddCluster(Cluster {
            health_check: Some(probe(initial.to_owned())),
            ..cluster("meta", "unavailable", 3)
        }),
    );

    apply(
        &mut state,
        RequestType::AddHttpListener(http_listener(http)),
    );
    assert_eq!(unrelated(&state).0.len(), 1);
    round_trip(&state, Some(initial.to_owned()), "after AddHttpListener");

    apply(
        &mut state,
        RequestType::AddHttpFrontend(http_front("meta", http, "meta.example", "/")),
    );
    assert_eq!(unrelated(&state).1.len(), 1);
    round_trip(&state, Some(initial.to_owned()), "after AddHttpFrontend");

    apply(
        &mut state,
        RequestType::AddBackend(AddBackend {
            cluster_id: "meta".to_owned(),
            backend_id: "meta-backend".to_owned(),
            address: backend_address,
            ..Default::default()
        }),
    );
    let baseline = unrelated(&state);
    assert_eq!(baseline.0.len(), 1);
    assert_eq!(baseline.1.len(), 1);
    assert_eq!(baseline.2["meta"].len(), 1);
    let replayed = round_trip(&state, Some(initial.to_owned()), "after AddBackend");
    assert_eq!(unrelated(&replayed), baseline);

    apply(
        &mut state,
        RequestType::RemoveHealthCheck("meta".to_owned()),
    );
    let replayed = round_trip(&state, None, "after RemoveHealthCheck");
    assert_eq!(replayed.clusters["meta"].health_check, None);
    assert_eq!(unrelated(&replayed), baseline);

    apply(
        &mut state,
        RequestType::SetHealthCheck(SetHealthCheck {
            cluster_id: "meta".to_owned(),
            config: probe(replacement.to_owned()),
        }),
    );
    let replayed = round_trip(&state, Some(replacement), "after SetHealthCheck");
    assert_eq!(unrelated(&replayed), baseline);
}

#[test]
fn mutable_commands_simulation_is_deterministic() {
    let first = run(0x2301_0001, DEFAULT_STEPS, false);
    let second = run(0x2301_0001, DEFAULT_STEPS, false);
    assert_eq!(
        first, second,
        "one seed must produce one trace and coverage map"
    );
}

#[test]
fn mutable_commands_simulation_seed_sweep() {
    let steps = env_usize("SOZU_MUTABLE_COMMANDS_SIM_STEPS")
        .unwrap_or(DEFAULT_STEPS)
        .clamp(1, MAX_STEPS);
    if let Some(seed) = env_u64("SOZU_MUTABLE_COMMANDS_SIM_SEED") {
        let record = run(seed, steps, true);
        assert_eq!(record.accepted.len(), MutationKind::ALL.len());
        return;
    }

    let seeds = env_usize("SOZU_MUTABLE_COMMANDS_SIM_SEEDS")
        .unwrap_or(DEFAULT_SEEDS)
        .clamp(1, MAX_SEEDS);
    let report = SimulationBuilder::new()
        .workload(MutableCommandsWorkload {
            steps,
            verbose: false,
            sink: None,
        })
        .set_debug_seeds((0..seeds as u64).collect())
        .set_iterations(seeds)
        .run_time_budget(RUN_BUDGET)
        .run();
    assert_no_failures(&report);
    assert_eq!(report.successful_runs, seeds);
}
