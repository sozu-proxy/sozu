#![allow(unused_variables, unused_must_use)]
#[macro_use]
extern crate sozu_lib as sozu;
#[macro_use]
extern crate sozu_command_lib as sozu_command;
extern crate time;

use std::{env, io::stdout, thread};

use anyhow::Context;
use sozu_command::config::DEFAULT_RUSTLS_CIPHER_LIST;

use crate::sozu_command::{
    channel::Channel,
    logging::{Logger, LoggerBackend},
    worker,
    worker::{LoadBalancingParams, PathRule, RulePosition},
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

    info!("MAIN\tstarting up");

    sozu::metrics::setup(
        &"127.0.0.1:8125"
            .parse()
            .with_context(|| "Could not parse address for metrics setup")?,
        "main",
        false,
        None,
    );
    gauge!("sozu.TEST", 42);

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
        cluster_id: Some(String::from("cluster_1")),
        address: "127.0.0.1:8080"
            .parse()
            .with_context(|| "Could not parse frontend address")?,
        hostname: String::from("lolcatho.st"),
        path: PathRule::Prefix(String::from("/")),
        method: None,
        position: RulePosition::Tree,
        tags: None,
    };

    let http_backend = worker::Backend {
        cluster_id: String::from("cluster_1"),
        backend_id: String::from("cluster_1-0"),
        sticky_id: None,
        address: "127.0.0.1:1026"
            .parse()
            .with_context(|| "Could not parse backend address")?,
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        backup: None,
    };

    command.write_message(&worker::WorkerRequest {
        id: String::from("ID_ABCD"),
        order: worker::WorkerOrder::AddHttpFrontend(http_front),
    });

    command.write_message(&worker::WorkerRequest {
        id: String::from("ID_EFGH"),
        order: worker::WorkerOrder::AddBackend(http_backend),
    });

    info!("MAIN\tHTTP -> {:?}", command.read_message());
    info!("MAIN\tHTTP -> {:?}", command.read_message());

    let config = worker::HttpsListenerConfig {
        address: "127.0.0.1:8443"
            .parse()
            .with_context(|| "could not parse address")?,
        cipher_list: DEFAULT_RUSTLS_CIPHER_LIST
            .into_iter()
            .map(String::from)
            .collect(),
        ..Default::default()
    };

    let (mut command2, channel2) =
        Channel::generate(1000, 10000).with_context(|| "should create a channel")?;
    let jg2 = thread::spawn(move || {
        let max_buffers = 500;
        let buffer_size = 16384;
        sozu::https::start_https_worker(config, channel2, max_buffers, buffer_size)
    });

    let cert1 = include_str!("../assets/certificate.pem");
    let key1 = include_str!("../assets/key.pem");

    let certificate_and_key = worker::CertificateAndKey {
        certificate: String::from(cert1),
        key: String::from(key1),
        certificate_chain: vec![],
        versions: vec![],
    };
    command2.write_message(&worker::WorkerRequest {
        id: String::from("ID_IJKL1"),
        order: worker::WorkerOrder::AddCertificate(worker::AddCertificate {
            address: "127.0.0.1:8443"
                .parse()
                .with_context(|| "Could not parse certificate address")?,
            certificate: certificate_and_key,
            names: vec![],
            expired_at: None,
        }),
    });

    let tls_front = worker::HttpFrontend {
        cluster_id: Some(String::from("cluster_1")),
        address: "127.0.0.1:8443"
            .parse()
            .with_context(|| "Could not parse frontend address")?,
        hostname: String::from("lolcatho.st"),
        path: PathRule::Prefix(String::from("/")),
        method: None,
        position: RulePosition::Tree,
        tags: None,
    };

    command2.write_message(&worker::WorkerRequest {
        id: String::from("ID_IJKL2"),
        order: worker::WorkerOrder::AddHttpsFrontend(tls_front),
    });
    let tls_backend = worker::Backend {
        cluster_id: String::from("cluster_1"),
        backend_id: String::from("cluster_1-0"),
        sticky_id: None,
        address: "127.0.0.1:1026"
            .parse()
            .with_context(|| "Could not parse backend address")?,
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        backup: None,
    };

    command2.write_message(&worker::WorkerRequest {
        id: String::from("ID_MNOP"),
        order: worker::WorkerOrder::AddBackend(tls_backend),
    });

    let cert2 = include_str!("../assets/cert_test.pem");
    let key2 = include_str!("../assets/key_test.pem");

    let certificate_and_key2 = worker::CertificateAndKey {
        certificate: String::from(cert2),
        key: String::from(key2),
        certificate_chain: vec![],
        versions: vec![],
    };

    command2.write_message(&worker::WorkerRequest {
        id: String::from("ID_QRST1"),
        order: worker::WorkerOrder::AddCertificate(worker::AddCertificate {
            address: "127.0.0.1:8443"
                .parse()
                .with_context(|| "Could not parse certificate address")?,
            certificate: certificate_and_key2,
            names: vec![],
            expired_at: None,
        }),
    });

    let tls_front2 = worker::HttpFrontend {
        cluster_id: Some(String::from("cluster_2")),
        address: "127.0.0.1:8443"
            .parse()
            .with_context(|| "Could not parse frontend address")?,
        hostname: String::from("test.local"),
        path: PathRule::Prefix(String::from("/")),
        method: None,
        position: RulePosition::Tree,
        tags: None,
    };

    command2.write_message(&worker::WorkerRequest {
        id: String::from("ID_QRST2"),
        order: worker::WorkerOrder::AddHttpsFrontend(tls_front2),
    });

    let tls_backend2 = worker::Backend {
        cluster_id: String::from("cluster_2"),
        backend_id: String::from("cluster_2-0"),
        sticky_id: None,
        address: "127.0.0.1:1026"
            .parse()
            .with_context(|| "Could not parse backend address")?,
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        backup: None,
    };

    command2.write_message(&worker::WorkerRequest {
        id: String::from("ID_UVWX"),
        order: worker::WorkerOrder::AddBackend(tls_backend2),
    });

    info!("MAIN\tTLS -> {:?}", command2.read_message());
    info!("MAIN\tTLS -> {:?}", command2.read_message());
    info!("MAIN\tTLS -> {:?}", command2.read_message());
    info!("MAIN\tTLS -> {:?}", command2.read_message());

    let _ = jg.join();
    info!("MAIN\tgood bye");
    Ok(())
}
