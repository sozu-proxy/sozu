//! Real gRPC lifecycle tests for the HTTPS/H2 and raw TCP proxy paths.

use std::{
    collections::HashMap,
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use futures::{Stream, stream};
use hyper::Uri;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder, MaybeHttpsStream};
use hyper_util::{
    client::legacy::{connect::HttpConnector, connect::dns::GaiResolver},
    rt::TokioIo,
};
use rustls::ClientConfig;
use sha2::{Digest, Sha256};
use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, Cluster, ListenerType,
        RequestHttpFrontend, SocketAddress, request::RequestType,
    },
};
use tokio::{net::TcpStream, sync::Notify, task::JoinHandle};
use tonic::codegen::Service;
use tonic::{Code, Request, Response, Status, transport::Endpoint};

use crate::{
    mock::https_client::Verifier, port_registry::bind_tokio_listener, sozu::worker::Worker,
};

use super::tests::create_local_address;

mod proto {
    tonic::include_proto!("sozu.e2e.lifecycle");
}

use proto::{
    GetRequest, PutRequest, StreamChunk, ValueReply, WaitRequest,
    lifecycle_store_client::LifecycleStoreClient,
    lifecycle_store_server::{LifecycleStore, LifecycleStoreServer},
};

const CLUSTER_ID: &str = "grpc-cluster";
const BACKEND_ID: &str = "grpc-backend";
const TEST_HOSTNAME: &str = "localhost";
const FRONTEND_BUDGET: Duration = Duration::from_secs(10);
const RESOURCE_RELEASE_BUDGET: Duration = Duration::from_secs(2);
const REQUEST_DEADLINE: Duration = Duration::from_millis(100);
const LARGE_CHUNK_BYTES: usize = 64 * 1024;
const TLS_CERT_PEM: &str = include_str!("../../../lib/assets/local-certificate.pem");
const TLS_CERT_KEY: &str = include_str!("../../../lib/assets/local-key.pem");

#[derive(Default)]
struct StoreState {
    values: Mutex<HashMap<String, (String, Vec<u8>)>>,
    put_invocations: AtomicU64,
    accepted_backend_connections: AtomicUsize,
    active_waits: AtomicUsize,
    waits_started: AtomicUsize,
    wait_started: Notify,
    wait_finished: Notify,
    deadline_header_seen: AtomicBool,
}

#[derive(Clone)]
struct StoreService {
    state: Arc<StoreState>,
}

struct ActiveWait {
    state: Arc<StoreState>,
}

impl ActiveWait {
    fn new(state: Arc<StoreState>) -> Self {
        state.active_waits.fetch_add(1, Ordering::SeqCst);
        state.waits_started.fetch_add(1, Ordering::SeqCst);
        state.wait_started.notify_waiters();
        Self { state }
    }
}

impl Drop for ActiveWait {
    fn drop(&mut self) {
        self.state.active_waits.fetch_sub(1, Ordering::SeqCst);
        self.state.wait_finished.notify_waiters();
    }
}

#[tonic::async_trait]
impl LifecycleStore for StoreService {
    type ExchangeStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, Status>> + Send>>;

    async fn put(&self, request: Request<PutRequest>) -> Result<Response<ValueReply>, Status> {
        let invocation_count = self.state.put_invocations.fetch_add(1, Ordering::SeqCst) + 1;
        let request = request.into_inner();
        self.state
            .values
            .lock()
            .expect("gRPC store lock poisoned")
            .insert(
                request.key.to_owned(),
                (request.operation_id.to_owned(), request.value.to_owned()),
            );

        Ok(backend_response(ValueReply {
            operation_id: request.operation_id,
            key: request.key,
            value: request.value,
            invocation_count,
        }))
    }

    async fn get(&self, request: Request<GetRequest>) -> Result<Response<ValueReply>, Status> {
        let key = request.into_inner().key;
        let value = self
            .state
            .values
            .lock()
            .expect("gRPC store lock poisoned")
            .get(&key)
            .cloned();
        let Some((operation_id, value)) = value else {
            let mut status = Status::not_found(format!("no value stored for {key}"));
            status.metadata_mut().insert(
                "x-sozu-fixture-terminal",
                "not-found".parse().expect("static metadata value"),
            );
            return Err(status);
        };

        Ok(backend_response(ValueReply {
            operation_id,
            key,
            value,
            invocation_count: self.state.put_invocations.load(Ordering::SeqCst),
        }))
    }

    async fn exchange(
        &self,
        request: Request<tonic::Streaming<StreamChunk>>,
    ) -> Result<Response<Self::ExchangeStream>, Status> {
        let mut incoming = request.into_inner();
        let mut chunks = Vec::new();
        while let Some(chunk) = incoming.message().await? {
            if sha256_hex(&chunk.payload) != chunk.sha256 {
                return Err(Status::invalid_argument(format!(
                    "payload digest mismatch at sequence {}",
                    chunk.sequence
                )));
            }
            chunks.push(chunk);
        }

        Ok(backend_response(Box::pin(stream::iter(
            chunks.into_iter().map(Ok),
        ))))
    }

    async fn wait(&self, request: Request<WaitRequest>) -> Result<Response<ValueReply>, Status> {
        let _active = ActiveWait::new(Arc::clone(&self.state));
        self.state.deadline_header_seen.store(
            request.metadata().contains_key("grpc-timeout"),
            Ordering::SeqCst,
        );
        let _request = request.into_inner();
        std::future::pending::<()>().await;
        unreachable!("the Wait RPC only ends when its request is cancelled")
    }
}

fn backend_response<T>(message: T) -> Response<T> {
    let mut response = Response::new(message);
    response.metadata_mut().insert(
        "x-sozu-backend-protocol",
        "h2c".parse().expect("static metadata value"),
    );
    response
}

struct GrpcBackend {
    address: SocketAddr,
    state: Arc<StoreState>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl GrpcBackend {
    async fn start(address: SocketAddr) -> Self {
        let listener = bind_tokio_listener(address, "gRPC lifecycle backend");
        let state = Arc::new(StoreState::default());
        let service = StoreService {
            state: Arc::clone(&state),
        };
        let accepted_state = Arc::clone(&state);
        let incoming = stream::unfold(listener, move |listener| {
            let accepted_state = Arc::clone(&accepted_state);
            async move {
                let accepted = listener.accept().await.map(|(stream, _peer)| {
                    accepted_state
                        .accepted_backend_connections
                        .fetch_add(1, Ordering::SeqCst);
                    stream
                });
                Some((accepted, listener))
            }
        });
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(LifecycleStoreServer::new(service))
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("gRPC lifecycle backend failed");
        });
        Self {
            address,
            state,
            shutdown: Some(shutdown),
            task,
        }
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        tokio::time::timeout(FRONTEND_BUDGET, self.task)
            .await
            .expect("gRPC lifecycle backend did not stop")
            .expect("gRPC lifecycle backend task panicked");
    }
}

#[derive(Clone)]
struct CountingTcpConnector {
    connects: Arc<AtomicUsize>,
}

impl Service<Uri> for CountingTcpConnector {
    type Response = TokioIo<TcpStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connects = Arc::clone(&self.connects);
        Box::pin(async move {
            let authority = uri.authority().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "gRPC URI has no authority")
            })?;
            let stream = TcpStream::connect(authority.as_str()).await?;
            connects.fetch_add(1, Ordering::SeqCst);
            Ok(TokioIo::new(stream))
        })
    }
}

type TlsStream = MaybeHttpsStream<TokioIo<TcpStream>>;

#[derive(Clone)]
struct CountingTlsConnector {
    inner: HttpsConnector<HttpConnector<GaiResolver>>,
    connects: Arc<AtomicUsize>,
    negotiated_alpn: Arc<Mutex<Vec<Option<Vec<u8>>>>>,
}

impl Service<Uri> for CountingTlsConnector {
    type Response = TlsStream;
    type Error = <HttpsConnector<HttpConnector<GaiResolver>> as Service<Uri>>::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let future = self.inner.call(uri);
        let connects = Arc::clone(&self.connects);
        let negotiated_alpn = Arc::clone(&self.negotiated_alpn);
        Box::pin(async move {
            let stream = future.await?;
            let alpn = match &stream {
                MaybeHttpsStream::Https(stream) => stream
                    .inner()
                    .get_ref()
                    .1
                    .alpn_protocol()
                    .map(<[u8]>::to_vec),
                MaybeHttpsStream::Http(_) => None,
            };
            negotiated_alpn
                .lock()
                .expect("ALPN observation lock poisoned")
                .push(alpn);
            connects.fetch_add(1, Ordering::SeqCst);
            Ok(stream)
        })
    }
}

fn tls_connector(
    connects: Arc<AtomicUsize>,
    negotiated_alpn: Arc<Mutex<Vec<Option<Vec<u8>>>>>,
) -> CountingTlsConnector {
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier))
        .with_no_client_auth();
    let inner = HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_only()
        .enable_http2()
        .build();
    CountingTlsConnector {
        inner,
        connects,
        negotiated_alpn,
    }
}

fn setup_https_grpc_worker(front_address: SocketAddr, backend_address: SocketAddr) -> Worker {
    let address = SocketAddress::from(front_address);
    let (config, listeners, state) = Worker::empty_https_config(front_address);
    let mut worker =
        Worker::start_new_worker_owned("GRPC_HTTPS_LIFECYCLE", config, listeners, state);
    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(address.clone())
            .to_tls(None)
            .expect("valid HTTPS listener"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Cluster {
        http2: Some(true),
        ..Worker::default_cluster(CLUSTER_ID)
    }));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: TEST_HOSTNAME.to_owned(),
        ..Worker::default_http_frontend(CLUSTER_ID, front_address)
    }));
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address,
        certificate: CertificateAndKey {
            certificate: TLS_CERT_PEM.to_owned(),
            key: TLS_CERT_KEY.to_owned(),
            certificate_chain: Vec::new(),
            versions: Vec::new(),
            names: Vec::new(),
        },
        expired_at: None,
    }));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        CLUSTER_ID,
        BACKEND_ID,
        backend_address,
        None,
    )));
    worker.read_to_last();
    worker
}

fn setup_tcp_grpc_worker(front_address: SocketAddr, backend_address: SocketAddr) -> Worker {
    let (config, listeners, state) = Worker::empty_tcp_config(front_address);
    let mut worker = Worker::start_new_worker_owned("GRPC_TCP_LIFECYCLE", config, listeners, state);
    worker.send_proxy_request_type(RequestType::AddTcpListener(
        ListenerBuilder::new_tcp(front_address.into())
            .to_tcp(None)
            .expect("valid TCP listener"),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.into(),
        proxy: ListenerType::Tcp.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(CLUSTER_ID)));
    worker.send_proxy_request_type(RequestType::AddTcpFrontend(Worker::default_tcp_frontend(
        CLUSTER_ID,
        front_address,
    )));
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        CLUSTER_ID,
        BACKEND_ID,
        backend_address,
        None,
    )));
    worker.read_to_last();
    worker
}

async fn wait_for_started(state: &StoreState, target: usize) {
    tokio::time::timeout(RESOURCE_RELEASE_BUDGET, async {
        loop {
            let notified = state.wait_started.notified();
            if state.waits_started.load(Ordering::SeqCst) >= target {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("gRPC Wait handler did not start");
}

async fn wait_for_no_active_waits(state: &StoreState) {
    tokio::time::timeout(RESOURCE_RELEASE_BUDGET, async {
        loop {
            let notified = state.wait_finished.notified();
            if state.active_waits.load(Ordering::SeqCst) == 0 {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("cancelled gRPC Wait handler kept its resource alive");
}

async fn exercise_grpc_lifecycle(
    client: &mut LifecycleStoreClient<tonic::transport::Channel>,
    state: &Arc<StoreState>,
) {
    let stored_value: Vec<u8> = (0..=255).cycle().take(32 * 1024).collect();
    let put = client
        .put(PutRequest {
            operation_id: "put-0001".to_owned(),
            key: "lifecycle-key".to_owned(),
            value: stored_value.to_owned(),
        })
        .await
        .expect("Put through Sōzu")
        .into_inner();
    assert_eq!(put.invocation_count, 1, "Put must execute exactly once");
    assert_eq!(put.value, stored_value);

    assert_value(
        client
            .get(GetRequest {
                key: "lifecycle-key".to_owned(),
            })
            .await
            .expect("Get through Sōzu"),
        &stored_value,
    );

    let missing = client
        .get(GetRequest {
            key: "missing-key".to_owned(),
        })
        .await
        .expect_err("missing Get must carry a terminal gRPC status");
    assert_eq!(missing.code(), Code::NotFound);
    assert_eq!(
        missing
            .metadata()
            .get("x-sozu-fixture-terminal")
            .expect("missing response terminal metadata"),
        "not-found"
    );

    let chunks = vec![
        stream_chunk(0, vec![0x11; 17]),
        stream_chunk(1, vec![0x5a; LARGE_CHUNK_BYTES]),
        stream_chunk(2, vec![0xe3; 23]),
    ];
    let mut echoed = client
        .exchange(stream::iter(chunks.to_owned()))
        .await
        .expect("bidirectional Exchange through Sōzu")
        .into_inner();
    let mut received = Vec::new();
    while let Some(chunk) = echoed.message().await.expect("Exchange response stream") {
        received.push(chunk);
    }
    assert_eq!(
        received, chunks,
        "Exchange must preserve order and payloads"
    );

    let deadline_target = state.waits_started.load(Ordering::SeqCst) + 1;
    let mut deadline_client = client.clone();
    let deadline = tokio::spawn(async move {
        let mut request = Request::new(WaitRequest {
            key: "deadline".to_owned(),
        });
        request.set_timeout(REQUEST_DEADLINE);
        deadline_client.wait(request).await
    });
    wait_for_started(state, deadline_target).await;
    assert!(
        state.deadline_header_seen.load(Ordering::SeqCst),
        "the backend must receive the gRPC timeout contract through Sōzu"
    );
    let deadline = tokio::time::timeout(RESOURCE_RELEASE_BUDGET, deadline)
        .await
        .expect("gRPC request deadline did not expire")
        .expect("deadline client task panicked")
        .expect_err("Wait must finish with its gRPC deadline");
    assert_eq!(deadline.code(), Code::Cancelled);
    wait_for_no_active_waits(state).await;
    assert_value(
        client
            .get(GetRequest {
                key: "lifecycle-key".to_owned(),
            })
            .await
            .expect("same channel Get after deadline"),
        &stored_value,
    );

    let cancel_target = state.waits_started.load(Ordering::SeqCst) + 1;
    let mut cancel_client = client.clone();
    let cancelled = tokio::spawn(async move {
        cancel_client
            .wait(WaitRequest {
                key: "cancel".to_owned(),
            })
            .await
    });
    wait_for_started(state, cancel_target).await;
    cancelled.abort();
    assert!(
        cancelled
            .await
            .expect_err("cancel task unexpectedly completed")
            .is_cancelled(),
        "client-side cancellation must abort the request future"
    );
    wait_for_no_active_waits(state).await;
    assert_value(
        client
            .get(GetRequest {
                key: "lifecycle-key".to_owned(),
            })
            .await
            .expect("same channel Get after cancellation"),
        &stored_value,
    );
}

fn assert_value(response: Response<ValueReply>, expected: &[u8]) {
    assert_eq!(
        response
            .metadata()
            .get("x-sozu-backend-protocol")
            .expect("backend protocol metadata"),
        "h2c"
    );
    let response = response.into_inner();
    assert_eq!(response.operation_id, "put-0001");
    assert_eq!(response.key, "lifecycle-key");
    assert_eq!(response.value, expected);
    assert_eq!(response.invocation_count, 1, "Put must not be replayed");
}

fn stream_chunk(sequence: u32, payload: Vec<u8>) -> StreamChunk {
    StreamChunk {
        sequence,
        sha256: sha256_hex(&payload),
        payload,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn stop(mut worker: Worker, backend: GrpcBackend) {
    worker.soft_stop();
    assert!(worker.wait_for_server_stop(), "Sōzu worker did not stop");
    backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_over_https_h2_preserves_lifecycle_and_opens_a_fresh_frontend_connection() {
    let front_address = create_local_address();
    let backend_address = create_local_address();
    let backend = GrpcBackend::start(backend_address).await;
    let worker = setup_https_grpc_worker(front_address, backend.address);
    let connects = Arc::new(AtomicUsize::new(0));
    let negotiated_alpn = Arc::new(Mutex::new(Vec::new()));
    let endpoint =
        Endpoint::from_shared(format!("https://{TEST_HOSTNAME}:{}", front_address.port()))
            .expect("valid HTTPS gRPC endpoint")
            .connect_timeout(FRONTEND_BUDGET);
    let channel = endpoint
        .connect_with_connector(tls_connector(
            Arc::clone(&connects),
            Arc::clone(&negotiated_alpn),
        ))
        .await
        .expect("TLS/H2 gRPC channel through Sōzu");
    let mut client = LifecycleStoreClient::new(channel);
    exercise_grpc_lifecycle(&mut client, &backend.state).await;
    drop(client);

    let channel = endpoint
        .connect_with_connector(tls_connector(
            Arc::clone(&connects),
            Arc::clone(&negotiated_alpn),
        ))
        .await
        .expect("fresh TLS/H2 gRPC channel through Sōzu");
    let mut client = LifecycleStoreClient::new(channel);
    assert_value(
        client
            .get(GetRequest {
                key: "lifecycle-key".to_owned(),
            })
            .await
            .expect("Get over fresh TLS/H2 frontend connection"),
        &(0..=255).cycle().take(32 * 1024).collect::<Vec<u8>>(),
    );
    assert_eq!(connects.load(Ordering::SeqCst), 2);
    assert_eq!(
        *negotiated_alpn.lock().expect("ALPN observation lock"),
        vec![Some(b"h2".to_vec()), Some(b"h2".to_vec())],
        "both frontend TLS connections must negotiate ALPN h2"
    );
    assert!(
        backend
            .state
            .accepted_backend_connections
            .load(Ordering::SeqCst)
            >= 1,
        "the h2c backend must accept a transport connection"
    );
    drop(client);
    stop(worker, backend).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_over_raw_tcp_preserves_lifecycle_and_opens_a_fresh_frontend_connection() {
    let front_address = create_local_address();
    let backend_address = create_local_address();
    let backend = GrpcBackend::start(backend_address).await;
    let worker = setup_tcp_grpc_worker(front_address, backend.address);
    let connects = Arc::new(AtomicUsize::new(0));
    let endpoint = Endpoint::from_shared(format!("http://{front_address}"))
        .expect("valid TCP gRPC endpoint")
        .connect_timeout(FRONTEND_BUDGET);
    let channel = endpoint
        .connect_with_connector(CountingTcpConnector {
            connects: Arc::clone(&connects),
        })
        .await
        .expect("raw TCP gRPC channel through Sōzu");
    let mut client = LifecycleStoreClient::new(channel);
    exercise_grpc_lifecycle(&mut client, &backend.state).await;
    drop(client);

    let channel = endpoint
        .connect_with_connector(CountingTcpConnector {
            connects: Arc::clone(&connects),
        })
        .await
        .expect("fresh raw TCP gRPC channel through Sōzu");
    let mut client = LifecycleStoreClient::new(channel);
    assert_value(
        client
            .get(GetRequest {
                key: "lifecycle-key".to_owned(),
            })
            .await
            .expect("Get over fresh raw TCP frontend connection"),
        &(0..=255).cycle().take(32 * 1024).collect::<Vec<u8>>(),
    );
    assert_eq!(connects.load(Ordering::SeqCst), 2);
    assert!(
        backend
            .state
            .accepted_backend_connections
            .load(Ordering::SeqCst)
            >= 1,
        "the raw TCP path must carry an h2c transport to the backend"
    );
    drop(client);
    stop(worker, backend).await;
}
