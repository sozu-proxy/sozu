#[macro_use]
extern crate sozu_command_lib;

use std::{collections::BTreeMap, thread};

use anyhow::Context;
use sozu_command_lib::{
    channel::Channel,
    config::ListenerBuilder,
    logging::setup_default_logging,
    proto::command::{
        request::RequestType, AddBackend, Cluster, LoadBalancingAlgorithms, LoadBalancingParams,
        PathRule, RequestHttpFrontend, RulePosition, SocketAddress, WorkerRequest, WorkerResponse,
    },
};

fn main() -> anyhow::Result<()> {
    setup_default_logging(true, "info", "EXAMPLE");

    info!("starting up");

    let http_listener = ListenerBuilder::new_http(SocketAddress::new_v4(127, 0, 0, 1, 8080))
        .to_http(None)
        .expect("Could not create HTTP listener");

    let (mut command_channel, proxy_channel): (
        Channel<WorkerRequest, WorkerResponse>,
        Channel<WorkerResponse, WorkerRequest>,
    ) = Channel::generate(1000, 10000).with_context(|| "should create a channel")?;

    let worker_thread_join_handle = thread::spawn(move || {
        let max_buffers = 500;
        let buffer_size = 16384;
        sozu_lib::http::testing::start_http_worker(
            http_listener,
            proxy_channel,
            max_buffers,
            buffer_size,
        )
        .expect("The worker could not be started, or shut down");
    });

    let cluster = Cluster {
        cluster_id: "my-cluster".to_string(),
        sticky_session: false,
        https_redirect: false,
        load_balancing: LoadBalancingAlgorithms::RoundRobin as i32,
        answer_503: Some("A custom forbidden message".to_string()),
        ..Default::default()
    };

    let http_front = RequestHttpFrontend {
        cluster_id: Some("my-cluster".to_string()),
        address: SocketAddress::new_v4(127, 0, 0, 1, 8080),
        hostname: "example.com".to_string(),
        path: PathRule::prefix(String::from("/")),
        position: RulePosition::Pre.into(),
        tags: BTreeMap::from([
            ("owner".to_owned(), "John".to_owned()),
            ("id".to_owned(), "my-own-http-front".to_owned()),
        ]),
        ..Default::default()
    };
    let http_backend = AddBackend {
        cluster_id: "my-cluster".to_string(),
        backend_id: "test-backend".to_string(),
        address: SocketAddress::new_v4(127, 0, 0, 1, 8080),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        ..Default::default()
    };

    command_channel
        .write_message(&WorkerRequest {
            id: String::from("add-the-cluster"),
            content: RequestType::AddCluster(cluster).into(),
        })
        .expect("Could not send AddHttpFrontend request");

    command_channel
        .write_message(&WorkerRequest {
            id: String::from("add-the-frontend"),
            content: RequestType::AddHttpFrontend(http_front).into(),
        })
        .expect("Could not send AddHttpFrontend request");

    command_channel
        .write_message(&WorkerRequest {
            id: String::from("add-the-backend"),
            content: RequestType::AddBackend(http_backend).into(),
        })
        .expect("Could not send AddBackend request");

    println!("HTTP -> {:?}", command_channel.read_message());
    println!("HTTP -> {:?}", command_channel.read_message());

    let _ = worker_thread_join_handle.join();
    info!("good bye");
    Ok(())
}
