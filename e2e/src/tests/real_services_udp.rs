//! CoreDNS over Sōzu's UDP datapath with exact DNS wire responses.

use std::{
    fs, io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    path::Path,
    time::{Duration, Instant},
};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{DNSClass, Name, RData, RecordType},
};
use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, DeactivateListener, ListenerType, QueryMetricsOptions,
        RequestUdpFrontend, ResponseStatus, filtered_metrics, request::RequestType,
        response_content::ContentType,
    },
};
use tempfile::TempDir;

use super::{
    real_services_tcp::fixture::{ContainerSpec, OwnedContainer},
    tests::create_unbound_local_address,
};
use crate::sozu::worker::Worker;

const IMAGE: &str = "coredns/coredns:1.14.7@sha256:7efd3c635b03efd68c4e8398fc45f0d993d0e9ab016f72c1cefb0fd6d01aa286";
const CLUSTER: &str = "real_dns_service";
const DNS_PORT: u16 = 53;
const EXCHANGE_TIMEOUT: Duration = Duration::from_millis(500);

struct DnsClient {
    socket: UdpSocket,
    next_id: u16,
}

impl DnsClient {
    fn new(address: SocketAddr) -> Self {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind DNS UDP client");
        socket
            .connect(address)
            .expect("connect DNS UDP client to Sōzu");
        socket
            .set_read_timeout(Some(EXCHANGE_TIMEOUT))
            .expect("set DNS UDP client timeout");
        Self {
            socket,
            next_id: 0x5100,
        }
    }

    fn query(&mut self, name: &str, record_type: RecordType) -> Result<Message, String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let bytes = dns_query(id, name, record_type)?;
        self.socket
            .send(&bytes)
            .map_err(|error| format!("send DNS datagram: {error}"))?;
        let mut response = [0_u8; 4096];
        let read = self
            .socket
            .recv(&mut response)
            .map_err(|error| format!("receive DNS datagram: {error}"))?;
        let message = Message::from_vec(&response[..read])
            .map_err(|error| format!("decode DNS response: {error}"))?;
        if message.metadata.id != id {
            return Err(format!(
                "DNS response ID {} did not match query ID {id}",
                message.metadata.id
            ));
        }
        let expected_name = Name::from_ascii(name)
            .map_err(|error| format!("invalid expected DNS response name: {error}"))?
            .to_lowercase();
        validate_response_question(&message, &expected_name, record_type)?;
        Ok(message)
    }

    fn assert_no_response(&mut self, name: &str, record_type: RecordType) {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let bytes = dns_query(id, name, record_type).expect("encode disabled-listener query");
        self.socket
            .send(&bytes)
            .expect("send query to disabled Sōzu UDP listener");
        let mut response = [0_u8; 4096];
        match self.socket.recv(&mut response) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::ConnectionRefused
                ) => {}
            Err(error) => panic!("unexpected disabled-listener UDP error: {error}"),
            Ok(read) => panic!(
                "disabled Sōzu UDP listener returned {read} bytes instead of blocking the application path"
            ),
        }
    }
}

fn validate_response_question(
    message: &Message,
    expected_name: &Name,
    expected_type: RecordType,
) -> Result<(), String> {
    let [question] = message.queries.as_slice() else {
        return Err(format!(
            "DNS response must contain exactly one question, got {}",
            message.queries.len()
        ));
    };
    let actual_name = question.name().to_lowercase();
    if actual_name != *expected_name
        || question.query_type() != expected_type
        || question.query_class() != DNSClass::IN
    {
        return Err(format!(
            "DNS response question mismatch: expected {expected_name} {expected_type} IN, got {} {} {}",
            question.name(),
            question.query_type(),
            question.query_class()
        ));
    }
    Ok(())
}

struct UdpServiceHarness {
    container: Option<OwnedContainer>,
    worker: Option<Worker>,
    frontend: SocketAddr,
}

impl UdpServiceHarness {
    fn start(spec: impl FnOnce(SocketAddr) -> ContainerSpec) -> Self {
        let frontend = create_unbound_local_address();
        let container = OwnedContainer::start(spec(frontend));
        let (config, listeners, state) = Worker::empty_config();
        let mut worker = Worker::start_new_worker_owned(
            format!("real-service-coredns-{}", frontend.port()),
            config,
            listeners,
            state,
        );
        worker.send_proxy_request_type(RequestType::AddUdpListener(
            ListenerBuilder::new_udp(frontend.into())
                .with_front_timeout(Some(1))
                .with_back_timeout(Some(1))
                .to_udp(None)
                .expect("CoreDNS UDP listener config"),
        ));
        worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
            interface: None,
            address: frontend.into(),
            proxy: ListenerType::Udp.into(),
            from_scm: false,
        }));
        worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(CLUSTER)));
        worker.send_proxy_request_type(RequestType::AddUdpFrontend(RequestUdpFrontend {
            cluster_id: CLUSTER.to_owned(),
            address: frontend.into(),
            tags: Default::default(),
        }));
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            CLUSTER,
            "real_dns_backend",
            container.backend(),
            None,
        )));
        worker.read_to_last();

        Self {
            container: Some(container),
            worker: Some(worker),
            frontend,
        }
    }

    fn client(&self) -> DnsClient {
        DnsClient::new(self.frontend)
    }

    fn probe_backend(&self, name: &str, record_type: RecordType) -> Message {
        let mut client = DnsClient::new(
            self.container
                .as_ref()
                .expect("CoreDNS container must be running")
                .backend(),
        );
        client
            .query(name, record_type)
            .expect("CoreDNS backend did not answer while Sōzu was disabled")
    }

    fn flow_metrics(&mut self) -> (i64, i64, u64) {
        const CREATED: &str = "udp.flows.created";
        const EVICTED: &str = "udp.flows.evicted";
        const ACTIVE: &str = "udp.active_flows";
        let worker = self.worker.as_mut().expect("worker must be running");
        worker.send_proxy_request_type(RequestType::QueryMetrics(QueryMetricsOptions {
            list: false,
            cluster_ids: vec![],
            backend_ids: vec![],
            metric_names: vec![CREATED.to_owned(), EVICTED.to_owned(), ACTIVE.to_owned()],
            no_clusters: true,
            workers: false,
        }));
        let response = worker
            .read_proxy_response()
            .expect("worker should respond to DNS flow metrics query");
        assert_eq!(response.status, ResponseStatus::Ok as i32, "{response:?}");
        let Some(ContentType::WorkerMetrics(metrics)) =
            response.content.and_then(|content| content.content_type)
        else {
            panic!("DNS flow metrics query returned no worker metrics");
        };
        let inner = |name: &str| {
            metrics
                .proxy
                .get(name)
                .and_then(|metric| metric.inner.clone())
        };
        let count = |name: &str| match inner(name) {
            Some(filtered_metrics::Inner::Count(value)) => value,
            None => 0,
            other => panic!("{name} should be a count, got {other:?}"),
        };
        let active = match inner(ACTIVE) {
            Some(filtered_metrics::Inner::Gauge(value)) => value,
            None => 0,
            other => panic!("{ACTIVE} should be a gauge, got {other:?}"),
        };
        (count(CREATED), count(EVICTED), active)
    }

    fn wait_for_flow_metrics(&mut self, expected: (i64, i64, u64), timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut observed = self.flow_metrics();
        while observed != expected && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            observed = self.flow_metrics();
        }
        assert_eq!(
            observed, expected,
            "DNS flow metrics did not reach the expected state"
        );
    }

    fn deactivate(&mut self) {
        let worker = self.worker.as_mut().expect("worker must be running");
        worker.send_proxy_request_type(RequestType::DeactivateListener(DeactivateListener {
            interface: None,
            address: self.frontend.into(),
            proxy: ListenerType::Udp.into(),
            to_scm: false,
        }));
        worker.read_to_last();
    }

    fn reactivate(&mut self) {
        let worker = self.worker.as_mut().expect("worker must be running");
        worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
            interface: None,
            address: self.frontend.into(),
            proxy: ListenerType::Udp.into(),
            from_scm: false,
        }));
        worker.read_to_last();
    }

    fn finish(mut self) {
        let mut worker = self.worker.take().expect("worker must be running");
        worker.soft_stop();
        assert!(
            worker.wait_for_server_stop(),
            "Sōzu UDP worker did not stop"
        );
        self.container
            .take()
            .expect("CoreDNS container must be running")
            .finish();
    }
}

impl Drop for UdpServiceHarness {
    fn drop(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            worker.hard_stop();
            let _ = worker.wait_for_server_stop();
        }
    }
}

fn dns_query(id: u16, name: &str, record_type: RecordType) -> Result<Vec<u8>, String> {
    let name = Name::from_ascii(name).map_err(|error| format!("invalid DNS name: {error}"))?;
    let mut message = Message::new(id, MessageType::Query, OpCode::Query);
    message.add_query(Query::query(name, record_type));
    message
        .to_vec()
        .map_err(|error| format!("encode DNS query: {error}"))
}

fn write_zone_fixture(root: &Path) {
    fs::write(
        root.join("Corefile"),
        "owned.test:53 {\n  errors\n  log\n  file /zones/owned.test.db\n}\n",
    )
    .expect("write CoreDNS Corefile");
    fs::write(
        root.join("owned.test.db"),
        "$ORIGIN owned.test.\n\
         @ 60 IN SOA ns.owned.test. hostmaster.owned.test. 1 60 60 60 60\n\
         @ 60 IN NS ns.owned.test.\n\
         ns 60 IN A 192.0.2.53\n\
         a 60 IN A 192.0.2.10\n\
         aaaa 60 IN AAAA 2001:db8::10\n\
         txt 60 IN TXT \"sozu-coredns-owned\"\n",
    )
    .expect("write CoreDNS authoritative zone");
}

fn expect_a(message: &Message, expected: Ipv4Addr) {
    assert_eq!(message.metadata.response_code, ResponseCode::NoError);
    assert_eq!(
        message.answers.len(),
        1,
        "unexpected A answer set: {message:?}"
    );
    assert!(
        matches!(&message.answers[0].data, RData::A(address) if address.0 == expected),
        "unexpected A answer: {message:?}"
    );
}

fn expect_aaaa(message: &Message, expected: Ipv6Addr) {
    assert_eq!(message.metadata.response_code, ResponseCode::NoError);
    assert_eq!(
        message.answers.len(),
        1,
        "unexpected AAAA answer set: {message:?}"
    );
    assert!(
        matches!(&message.answers[0].data, RData::AAAA(address) if address.0 == expected),
        "unexpected AAAA answer: {message:?}"
    );
}

fn expect_txt(message: &Message, expected: &[u8]) {
    assert_eq!(message.metadata.response_code, ResponseCode::NoError);
    assert_eq!(
        message.answers.len(),
        1,
        "unexpected TXT answer set: {message:?}"
    );
    assert!(
        matches!(
            &message.answers[0].data,
            RData::TXT(txt)
                if txt.txt_data.len() == 1 && txt.txt_data[0].as_ref() == expected
        ),
        "unexpected TXT answer: {message:?}"
    );
}

fn expect_nxdomain(message: &Message) {
    assert_eq!(message.metadata.response_code, ResponseCode::NXDomain);
    assert!(
        message.answers.is_empty(),
        "NXDOMAIN returned answer records: {message:?}"
    );
}

fn wait_for_coredns(client: &mut DnsClient) -> Message {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_error = None;
    while Instant::now() < deadline {
        match client.query("a.owned.test.", RecordType::A) {
            Ok(response) if response.metadata.response_code == ResponseCode::NoError => {
                return response;
            }
            Ok(response) => {
                last_error = Some(format!(
                    "response code {:?}",
                    response.metadata.response_code
                ))
            }
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "CoreDNS did not answer through Sōzu before 30s: {}",
        last_error.as_deref().unwrap_or("no query completed")
    );
}

#[test]
fn authoritative_queries_expiry_and_reactivation_via_sozu_udp() {
    let zone = TempDir::new().expect("create owned CoreDNS zone directory");
    write_zone_fixture(zone.path());
    let mut harness = UdpServiceHarness::start(|_| {
        ContainerSpec::new("coredns", IMAGE, DNS_PORT)
            .udp()
            .mount_read_only(zone.path(), "/zones")
            .command(["-conf", "/zones/Corefile"])
    });

    let mut client = harness.client();
    expect_a(&wait_for_coredns(&mut client), Ipv4Addr::new(192, 0, 2, 10));
    expect_aaaa(
        &client
            .query("aaaa.owned.test.", RecordType::AAAA)
            .expect("AAAA query through Sōzu"),
        "2001:db8::10".parse().expect("parse expected IPv6 address"),
    );
    expect_txt(
        &client
            .query("txt.owned.test.", RecordType::TXT)
            .expect("TXT query through Sōzu"),
        b"sozu-coredns-owned",
    );
    expect_nxdomain(
        &client
            .query("missing.owned.test.", RecordType::A)
            .expect("NXDOMAIN query through Sōzu"),
    );
    assert_eq!(harness.flow_metrics(), (1, 0, 1));

    harness.wait_for_flow_metrics((1, 1, 0), Duration::from_secs(5));
    expect_a(
        &client
            .query("a.owned.test.", RecordType::A)
            .expect("same DNS client must open a new flow after expiry"),
        Ipv4Addr::new(192, 0, 2, 10),
    );
    assert_eq!(harness.flow_metrics(), (2, 1, 1));

    harness.deactivate();
    expect_a(
        &harness.probe_backend("a.owned.test.", RecordType::A),
        Ipv4Addr::new(192, 0, 2, 10),
    );
    harness
        .client()
        .assert_no_response("txt.owned.test.", RecordType::TXT);
    harness.reactivate();
    expect_txt(
        &harness
            .client()
            .query("txt.owned.test.", RecordType::TXT)
            .expect("fresh DNS client after listener reactivation"),
        b"sozu-coredns-owned",
    );

    harness.finish();
}
