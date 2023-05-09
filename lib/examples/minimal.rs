#[macro_use]
extern crate time;

#[macro_use]
extern crate sozu_command_lib;

use std::{collections::BTreeMap, env, io::stdout, thread};

use anyhow::Context;

use sozu_command_lib::{
    channel::Channel,
    config::ListenerBuilder,
    info,
    logging::{Logger, LoggerBackend},
    proto::command::{
        request::RequestType, AddBackend, LoadBalancingParams, PathRule, Request,
        RequestHttpFrontend, RulePosition,
    },
    request::WorkerRequest,
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

    let http_listener = ListenerBuilder::new_http("127.0.0.1:8080").to_http()?;

    let (mut command_channel, proxy_channel) =
        Channel::generate(1000, 10000).with_context(|| "should create a channel")?;

    let jg = thread::spawn(move || {
        let max_buffers = 500;
        let buffer_size = 16384;
        sozu_lib::http::start_http_worker(http_listener, proxy_channel, max_buffers, buffer_size)
            .expect("The worker could not be started, or shut down");
    });

    let http_front = RequestHttpFrontend {
        cluster_id: Some(String::from("test")),
        address: "127.0.0.1:8080".to_string(),
        hostname: String::from("example.com"),
        path: PathRule::prefix(String::from("/")),
        position: RulePosition::Pre.into(),
        tags: BTreeMap::from([
            ("owner".to_owned(), "John".to_owned()),
            ("id".to_owned(), "my-own-http-front".to_owned()),
        ]),
        ..Default::default()
    };
    let http_backend = AddBackend {
        cluster_id: String::from("test"),
        backend_id: String::from("test-0"),
        address: "127.0.0.1:8000".to_string(),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        ..Default::default()
    };

    command_channel
        .write_message(&WorkerRequest {
            id: String::from("ID_ABCD"),
            content: Request {
                request_type: Some(RequestType::AddHttpFrontend(http_front)),
            },
        })
        .expect("Could not send AddHttpFrontend request");

    command_channel
        .write_message(&WorkerRequest {
            id: String::from("ID_EFGH"),
            content: Request {
                request_type: Some(RequestType::AddBackend(http_backend)),
            },
        })
        .expect("Could not send AddBackend request");

    println!("HTTP -> {:?}", command_channel.read_message());
    println!("HTTP -> {:?}", command_channel.read_message());

    let _ = jg.join();
    info!("good bye");
    Ok(())
}
