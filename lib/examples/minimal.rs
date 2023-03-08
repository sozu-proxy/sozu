#![allow(unused_variables, unused_must_use)]
extern crate sozu_lib as sozu;
#[macro_use]
extern crate sozu_command_lib as sozu_command;
extern crate time;

use std::{collections::BTreeMap, env, io::stdout, thread};

use anyhow::Context;

use crate::sozu_command::{
    channel::Channel,
    logging::{Logger, LoggerBackend},
    worker,
    worker::{LoadBalancingParams, PathRule, Route, RulePosition},
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

    let config = worker::HttpListenerConfig {
        address: "127.0.0.1:8080"
            .parse()
            .with_context(|| "could not parse address")?,
        ..Default::default()
    };

    let (mut command, channel) =
        Channel::generate(1000, 10000).with_context(|| "should create a channel")?;

    let jg = thread::spawn(move || {
        let max_buffers = 500;
        let buffer_size = 16384;
        sozu::http::start_http_worker(config, channel, max_buffers, buffer_size);
    });

    let http_front = worker::HttpFrontend {
        route: Route::ClusterId(String::from("test")),
        address: "127.0.0.1:8080"
            .parse()
            .with_context(|| "could not parse address")?,
        hostname: String::from("example.com"),
        path: PathRule::Prefix(String::from("/")),
        method: None,
        position: RulePosition::Pre,
        tags: Some(BTreeMap::from([
            ("owner".to_owned(), "John".to_owned()),
            ("id".to_owned(), "my-own-http-front".to_owned()),
        ])),
    };
    let http_backend = worker::Backend {
        cluster_id: String::from("test"),
        backend_id: String::from("test-0"),
        address: "127.0.0.1:8000"
            .parse()
            .with_context(|| "could not parse address")?,
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        sticky_id: None,
        backup: None,
    };

    command.write_message(&worker::ProxyRequest {
        id: String::from("ID_ABCD"),
        order: worker::ProxyRequestOrder::AddHttpFrontend(http_front),
    });

    command.write_message(&worker::ProxyRequest {
        id: String::from("ID_EFGH"),
        order: worker::ProxyRequestOrder::AddBackend(http_backend),
    });

    println!("HTTP -> {:?}", command.read_message());
    println!("HTTP -> {:?}", command.read_message());

    let _ = jg.join();
    info!("good bye");
    Ok(())
}
