use std::net::SocketAddr;

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        request::RequestType, ActivateListener, AddCertificate, CertificateAndKey, ListenerType,
        RequestHttpFrontend,
    },
};
use sozu_e2e::{
    mock::https_client::build_https_client,
    setup::{create_local_address, provide_port},
    sozu::worker::Worker,
};

fn bench_http(nb_clients: usize, payload_size: usize) {
    let hostname = "localhost".to_string();
    let front_port = provide_port();
    let front_address: SocketAddr = format!("127.0.0.1:{}", front_port)
        .parse()
        .expect("could not parse front address");
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("BACKPRESSURE", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpListener(
        ListenerBuilder::new_http(front_address)
            .to_http(None)
            .unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.to_string(),
        proxy: ListenerType::Http.into(),
        from_scm: false,
    }));
    worker.read_to_last();

    let payload = vec![0u8; payload_size];
    let tasks = (0..nb_clients)
        .into_iter()
        .map(|_| {
            let client = build_https_client();
            client.request(
                hyper::Request::builder()
                    .method("POST")
                    .uri(format!("http://{hostname}:{front_port}/api"))
                    .body(payload.clone().into())
                    .unwrap(),
            )
        })
        .collect::<Vec<_>>();

    let start = std::time::Instant::now();
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    let handles = tasks
        .into_iter()
        .enumerate()
        .map(|(i, task)| {
            rt.spawn(async move {
                match task.await {
                    Ok(response) => {
                        println!("{i}: {}", response.status());
                        return true;
                    }
                    Err(error) => {
                        println!("Could not get response: {error}");
                        return false;
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    rt.block_on(async {
        for handle in handles {
            results.push(handle.await);
        }
    });
    for (i, result) in results.into_iter().enumerate() {
        match result {
            Ok(r) => println!("{i}: {r}"),
            Err(e) => println!("{e:?}"),
        }
    }
    println!("Elapsed: {:?}", start.elapsed());
    worker.hard_stop();
    worker.wait_for_server_stop();
}

fn bench_https(nb_clients: usize, payload_size: usize) {
    let hostname = "localhost".to_string();
    let front_port = provide_port();
    let front_address: SocketAddr = format!("127.0.0.1:{}", front_port)
        .parse()
        .expect("could not parse front address");
    let back_address = create_local_address();

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("BACKPRESSURE", config, &listeners, state);

    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        ListenerBuilder::new_https(front_address)
            .to_tls(None)
            .unwrap(),
    ));

    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        address: front_address.to_string(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
        false,
    )));

    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: hostname.to_owned(),
        ..Worker::default_http_frontend("cluster_0", front_address)
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../lib/assets/local-certificate.pem")),
        key: String::from(include_str!("../../lib/assets/local-key.pem")),
        certificate_chain: vec![],
        versions: vec![],
        names: vec![],
    };
    let add_certificate = AddCertificate {
        address: front_address.to_string(),
        certificate: certificate_and_key,
        expired_at: None,
    };
    worker.send_proxy_request_type(RequestType::AddCertificate(add_certificate));

    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address.to_string(),
        None,
    )));
    worker.read_to_last();

    let backend = std::thread::spawn(move || {
        let listener = std::net::TcpListener::bind(back_address).expect("could not bind");
        listener
            .set_nonblocking(false)
            .expect("could not set blocking on listener");
        let mut clients = Vec::new();
        for i in 0..nb_clients {
            let stream = listener.accept();
            match stream {
                Ok(stream) => {
                    println!("new connection {i}");
                    clients.push(stream.0);
                }
                Err(error) => {
                    println!("IO Error: {error:?}");
                }
            }
        }
    });

    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
        false,
    )));
    let payload = vec![0u8; payload_size];
    let tasks = (0..nb_clients)
        .into_iter()
        .map(|_| {
            let client = build_https_client();
            client.request(
                hyper::Request::builder()
                    .method("POST")
                    .uri(format!("https://{hostname}:{front_port}/api"))
                    .body(payload.clone().into())
                    .unwrap(),
            )
        })
        .collect::<Vec<_>>();

    // let mut set = tokio::task::JoinSet::new();
    let start = std::time::Instant::now();
    let rt = tokio::runtime::Runtime::new().expect("Could not create Runtime");
    let handles = tasks
        .into_iter()
        .enumerate()
        .map(|(i, task)| {
            rt.spawn(async move {
                match task.await {
                    Ok(response) => {
                        println!("{i}: {}", response.status());
                        return true;
                    }
                    Err(error) => {
                        println!("Could not get response: {error}");
                        return false;
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    rt.block_on(async {
        for handle in handles {
            results.push(handle.await);
        }
    });
    for (i, result) in results.into_iter().enumerate() {
        match result {
            Ok(r) => println!("{i}: {r}"),
            Err(e) => println!("{e:?}"),
        }
    }
    println!("Elapsed: {:?}", start.elapsed());
    worker.hard_stop();
    worker.wait_for_server_stop();
    backend.join().expect("bakcend failed to join");
}

#[test]
fn bench_http_1() {
    for _ in 0..5 {
        bench_https(400, 0);
    }
}
