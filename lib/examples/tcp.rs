#![allow(unused_must_use)]
extern crate sozu_lib;
#[macro_use]
extern crate sozu_command_lib;

use std::thread;

use anyhow::Context;
use sozu_command_lib::{
    channel::Channel,
    logging::setup_default_logging,
    proto::command::{
        request::RequestType, AddBackend, LoadBalancingParams, RequestTcpFrontend, SocketAddress,
        TcpListenerConfig, WorkerRequest,
    },
};

fn main() -> anyhow::Result<()> {
    setup_default_logging(true, "info", "EXAMPLE");

    info!("starting up");

    let (mut command, channel) =
        Channel::generate(1000, 10000).with_context(|| "should create a channel")?;

    let jg = thread::spawn(move || {
        let max_buffers = 500;
        let buffer_size = 16384;
        let listener = TcpListenerConfig {
            address: SocketAddress::new_v4(127, 0, 0, 1, 8080),
            ..Default::default()
        };
        setup_default_logging(true, "debug", "TCP");
        sozu_lib::tcp::testing::start_tcp_worker(listener, max_buffers, buffer_size, channel);
    });

    let tcp_front = RequestTcpFrontend {
        cluster_id: String::from("test"),
        address: SocketAddress::new_v4(127, 0, 0, 1, 8080),
        ..Default::default()
    };
    let tcp_backend = AddBackend {
        cluster_id: String::from("test"),
        backend_id: String::from("test-0"),
        address: SocketAddress::new_v4(127, 0, 0, 1, 1026),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        sticky_id: None,
        backup: None,
    };

    command.write_message(&WorkerRequest {
        id: String::from("ID_ABCD"),
        content: RequestType::AddTcpFrontend(tcp_front).into(),
    });

    command.write_message(&WorkerRequest {
        id: String::from("ID_EFGH"),
        content: RequestType::AddBackend(tcp_backend).into(),
    });

    info!("TCP -> {:?}", command.read_message());
    info!("TCP -> {:?}", command.read_message());

    let _ = jg.join();
    info!("good bye");
    Ok(())
}
