#![allow(unused_variables, unused_must_use)]
extern crate sozu_lib as sozu;
#[macro_use]
extern crate sozu_command_lib as sozu_command;
extern crate time;

use std::{collections::BTreeMap, env, io::stdout, thread};

use anyhow::Context;
use sozu_command::proxy::LoadBalancingAlgorithms;

use crate::sozu_command::{
    channel::Channel,
    logging::{Logger, LoggerBackend},
    proxy::{
        Backend, Cluster, HttpFrontend, HttpListenerConfig, LoadBalancingParams, PathRule,
        ProxyRequest, ProxyRequestOrder, Route, RulePosition,
    },
};

fn main() -> anyhow::Result<()> {
    if env::var("RUST_LOG").is_ok() {
        Logger::init(
            "EXAMPLE".to_string(),
            &env::var("RUST_LOG").with_context(|| "could not get the RUST_LOG env var")?,
            LoggerBackend::Stdout(stdout()),
            None,
        );
    } else {
        Logger::init(
            "EXAMPLE".to_string(),
            "info",
            LoggerBackend::Stdout(stdout()),
            None,
        );
    }

    info!("starting up");

    // To create a HTTP proxy, you first need to create an HTTP listener structure
    // (there are configuration structures for the HTTPS and TCP proxies too).
    let config = HttpListenerConfig {
        address: "127.0.0.1:8080"
            .parse()
            .with_context(|| "could not parse address")?,
        ..Default::default()
    };

    // Then create channels to communicate with the proxy thread.
    // These channels are custom-made wrappers around mio unix sockets.
    let (
        mut client_channel,      // for the proxy to receive requests (and send responses)
        proxy_channel, // to send requests to the proxy (and receive responses)
    ) = Channel::generate(1000, 10000).with_context(|| "should create a channel")?;

    let join_handle = thread::spawn(move || {
        let max_buffers = 500;
        let buffer_size = 16384;
        sozu::http::start_http_worker(config, proxy_channel, max_buffers, buffer_size);
    });

    // Once the thread is launched, the proxy will start its event loop and handle
    // events on the listening interface and port specified in the configuration
    // object. Since no clusters were specified for the proxy, it will receive
    // the connections, parse the request, then send a default (but configurable)
    // answer.

    // Create a cluster.
    // A Cluster is a collection of frontends and backends and routing rules between them.
    // It usually represents an application.

    let mut cluster = Cluster::default_with_id("test");
    cluster.load_balancing = LoadBalancingAlgorithms::RoundRobin;

    client_channel.write_message(&ProxyRequest {
        id: String::from("ID_ABCD"),
        order: ProxyRequestOrder::AddCluster(cluster),
    });

    // Create a frontend.
    // It is very important that:
    // - the hostname redirects to the IP address you specify
    // - the port matches the one of the listener

    let http_front = HttpFrontend {
        route: Route::ClusterId("test".to_owned()),
        address: "127.0.0.1:8080"
            .parse()
            .with_context(|| "could not parse address")?,
        hostname: String::from("127.0.0.1"),
        path: PathRule::Prefix(String::from("/")),
        method: None,
        position: RulePosition::Pre,
        tags: Some(BTreeMap::from([
            ("owner".to_owned(), "John".to_owned()),
            ("id".to_owned(), "my-own-http-front".to_owned()),
        ])),
    };

    client_channel.write_message(&ProxyRequest {
        id: String::from("ID_EFGH"),
        order: ProxyRequestOrder::AddHttpFrontend(http_front),
    });

    // Create a backend.
    // The cluster_id should match with the one previously used for cluster and frontends.
    // The address and port are the one within the infrastructure.

    let http_backend = Backend {
        cluster_id: String::from("test"),
        backend_id: String::from("test-0"),
        address: "127.0.0.1:8000"
            .parse()
            .with_context(|| "could not parse address")?,
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        sticky_id: None,
        backup: None,
    };

    client_channel.write_message(&ProxyRequest {
        id: String::from("ID_IJKL"),
        order: ProxyRequestOrder::AddBackend(http_backend),
    });

    println!("HTTP -> {:?}", client_channel.read_message());
    println!("HTTP -> {:?}", client_channel.read_message());

    let _ = join_handle.join();
    info!("good bye");
    Ok(())
}
