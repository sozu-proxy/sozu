#![allow(unused_variables, unused_must_use)]
#[macro_use]
extern crate sozu_lib;
#[macro_use]
extern crate sozu_command_lib;
extern crate time;

use std::{env, io::stdout, thread};

use anyhow::Context;

use sozu_command_lib::{
    channel::Channel,
    config::ListenerBuilder,
    logging::{Logger, LoggerBackend},
    proto::command::{
        request::RequestType, AddBackend, AddCertificate, CertificateAndKey, LoadBalancingParams,
        PathRule, RequestHttpFrontend, WorkerRequest,
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

    info!("MAIN\tstarting up");

    sozu_lib::metrics::setup(
        &"127.0.0.1:8125"
            .parse()
            .with_context(|| "Could not parse address for metrics setup")?,
        "main",
        false,
        None,
    );
    gauge!("sozu.TEST", 42);

    let http_listener = ListenerBuilder::new_http("127.0.0.1:8080").to_http(None)?;

    let (mut command, channel) =
        Channel::generate(1000, 10000).with_context(|| "should create a channel")?;
    let jg = thread::spawn(move || {
        let max_buffers = 500;
        let buffer_size = 16384;
        sozu_lib::http::start_http_worker(http_listener, channel, max_buffers, buffer_size);
    });

    let http_front = RequestHttpFrontend {
        cluster_id: Some(String::from("cluster_1")),
        address: "127.0.0.1:8080".to_string(),
        hostname: String::from("lolcatho.st"),
        path: PathRule::prefix(String::from("/")),
        ..Default::default()
    };

    let http_backend = AddBackend {
        cluster_id: String::from("cluster_1"),
        backend_id: String::from("cluster_1-0"),
        sticky_id: None,
        address: "127.0.0.1:1026".to_string(),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        backup: None,
    };

    command.write_message(&WorkerRequest {
        id: String::from("ID_ABCD"),
        content: RequestType::AddHttpFrontend(http_front).into(),
    });

    command.write_message(&WorkerRequest {
        id: String::from("ID_EFGH"),
        content: RequestType::AddBackend(http_backend).into(),
    });

    info!("MAIN\tHTTP -> {:?}", command.read_message());
    info!("MAIN\tHTTP -> {:?}", command.read_message());

    let https_listener = ListenerBuilder::new_https("127.0.0.1:8443").to_tls(None)?;

    let (mut command2, channel2) =
        Channel::generate(1000, 10000).with_context(|| "should create a channel")?;
    let jg2 = thread::spawn(move || {
        let max_buffers = 500;
        let buffer_size = 16384;
        sozu_lib::https::start_https_worker(https_listener, channel2, max_buffers, buffer_size)
    });

    let cert1 = include_str!("../assets/certificate.pem");
    let key1 = include_str!("../assets/key.pem");

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(cert1),
        key: String::from(key1),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    command2.write_message(&WorkerRequest {
        id: String::from("ID_IJKL1"),
        content: RequestType::AddCertificate(AddCertificate {
            address: "127.0.0.1:8443"
                .parse()
                .with_context(|| "Could not parse certificate address")?,
            certificate: certificate_and_key,
            expired_at: None,
        })
        .into(),
    });

    let tls_front = RequestHttpFrontend {
        cluster_id: Some(String::from("cluster_1")),
        address: "127.0.0.1:8443".to_string(),
        hostname: String::from("lolcatho.st"),
        path: PathRule::prefix(String::from("/")),
        ..Default::default()
    };

    command2.write_message(&WorkerRequest {
        id: String::from("ID_IJKL2"),
        content: RequestType::AddHttpsFrontend(tls_front).into(),
    });
    let tls_backend = AddBackend {
        cluster_id: String::from("cluster_1"),
        backend_id: String::from("cluster_1-0"),
        sticky_id: None,
        address: "127.0.0.1:1026".to_string(),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        backup: None,
    };

    command2.write_message(&WorkerRequest {
        id: String::from("ID_MNOP"),
        content: RequestType::AddBackend(tls_backend).into(),
    });

    let cert2 = include_str!("../assets/cert_test.pem");
    let key2 = include_str!("../assets/key_test.pem");

    let certificate_and_key2 = CertificateAndKey {
        certificate: String::from(cert2),
        key: String::from(key2),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };

    command2.write_message(&WorkerRequest {
        id: String::from("ID_QRST1"),
        content: RequestType::AddCertificate(AddCertificate {
            address: "127.0.0.1:8443"
                .parse()
                .with_context(|| "Could not parse certificate address")?,
            certificate: certificate_and_key2,
            expired_at: None,
        })
        .into(),
    });

    let tls_front2 = RequestHttpFrontend {
        cluster_id: Some(String::from("cluster_2")),
        address: "127.0.0.1:8443".to_string(),
        hostname: String::from("test.local"),
        path: PathRule::prefix(String::from("/")),
        ..Default::default()
    };

    command2.write_message(&WorkerRequest {
        id: String::from("ID_QRST2"),
        content: RequestType::AddHttpsFrontend(tls_front2).into(),
    });

    let tls_backend2 = AddBackend {
        cluster_id: String::from("cluster_2"),
        backend_id: String::from("cluster_2-0"),
        sticky_id: None,
        address: "127.0.0.1:1026".to_string(),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        backup: None,
    };

    command2.write_message(&WorkerRequest {
        id: String::from("ID_UVWX"),
        content: RequestType::AddBackend(tls_backend2).into(),
    });

    info!("MAIN\tTLS -> {:?}", command2.read_message());
    info!("MAIN\tTLS -> {:?}", command2.read_message());
    info!("MAIN\tTLS -> {:?}", command2.read_message());
    info!("MAIN\tTLS -> {:?}", command2.read_message());

    let _ = jg.join();
    info!("MAIN\tgood bye");
    Ok(())
}
