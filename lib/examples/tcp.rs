#![allow(unused_variables, unused_must_use)]
extern crate sozu_lib as sozu;
// #[macro_use]
extern crate sozu_command_lib as sozu_command;
extern crate time;

use std::{io::stdout, thread};
use tracing::info;

use anyhow::Context;

use crate::sozu_command::{
    channel::Channel,
    // logging::{Logger, LoggerBackend},
    proxy::{self, LoadBalancingParams, TcpListener},
};

fn main() -> anyhow::Result<()> {
    crate::sozu_command::logging::setup_tracing_subscriber_with_env();

    info!("starting up");

    let (mut command, channel) =
        Channel::generate(1000, 10000).with_context(|| "should create a channel")?;

    let jg = thread::spawn(move || {
        let max_listeners = 500;
        let max_buffers = 500;
        let buffer_size = 16384;
        let listener = TcpListener {
            address: "127.0.0.1:8080".parse().expect("could not parse address"),
            public_address: None,
            expect_proxy: false,
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
        };

        // Logger::init(
        //     "TCP".to_string(),
        //     "debug",
        //     LoggerBackend::Stdout(stdout()),
        //     None,
        // );
        sozu::tcp::start(listener, max_buffers, buffer_size, channel);
    });

    let tcp_front = proxy::TcpFrontend {
        cluster_id: String::from("test"),
        address: "127.0.0.1:8080"
            .parse()
            .with_context(|| "could not parse address")?,
        tags: None,
    };
    let tcp_backend = proxy::Backend {
        cluster_id: String::from("test"),
        backend_id: String::from("test-0"),
        address: "127.0.0.1:1026"
            .parse()
            .with_context(|| "could not parse address")?,
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        sticky_id: None,
        backup: None,
    };

    command.write_message(&proxy::ProxyRequest {
        id: String::from("ID_ABCD"),
        order: proxy::ProxyRequestOrder::AddTcpFrontend(tcp_front),
    });

    command.write_message(&proxy::ProxyRequest {
        id: String::from("ID_EFGH"),
        order: proxy::ProxyRequestOrder::AddBackend(tcp_backend),
    });

    info!("TCP -> {:?}", command.read_message());
    info!("TCP -> {:?}", command.read_message());

    let _ = jg.join();
    info!("good bye");
    Ok(())
}
