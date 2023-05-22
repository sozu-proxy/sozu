#![allow(unused_variables, unused_must_use)]
extern crate sozu_lib;
#[macro_use]
extern crate sozu_command_lib;
extern crate time;

use std::{io::stdout, thread};

use anyhow::Context;
use sozu_command_lib::{
    channel::Channel,
    logging::{Logger, LoggerBackend},
    proto::command::{
        request::RequestType, AddBackend, LoadBalancingParams, RequestTcpFrontend,
        TcpListenerConfig,
    },
    request::WorkerRequest,
};

fn main() -> anyhow::Result<()> {
    /*
    if env::var("RUST_LOG").is_ok() {
     Logger::init("EXAMPLE".to_string(), &env::var("RUST_LOG").with_context(|| "could not get the RUST_LOG env var"), LoggerBackend::Stdout(stdout()));
    } else {
     Logger::init("EXAMPLE".to_string(), "info", LoggerBackend::Stdout(stdout()));
    }
    */
    Logger::init(
        "EXAMPLE".to_string(),
        "debug",
        LoggerBackend::Stdout(stdout()),
        None,
    );

    info!("starting up");

    let (mut command, channel) =
        Channel::generate(1000, 10000).with_context(|| "should create a channel")?;

    let jg = thread::spawn(move || {
        let max_listeners = 500;
        let max_buffers = 500;
        let buffer_size = 16384;
        let listener = TcpListenerConfig {
            address: "127.0.0.1:8080".parse().expect("could not parse address"),
            ..Default::default()
        };
        Logger::init(
            "TCP".to_string(),
            "debug",
            LoggerBackend::Stdout(stdout()),
            None,
        );
        sozu_lib::tcp::start_tcp_worker(listener, max_buffers, buffer_size, channel);
    });

    let tcp_front = RequestTcpFrontend {
        cluster_id: String::from("test"),
        address: "127.0.0.1:8080".to_string(),
        ..Default::default()
    };
    let tcp_backend = AddBackend {
        cluster_id: String::from("test"),
        backend_id: String::from("test-0"),
        address: "127.0.0.1:1026".to_string(),
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
