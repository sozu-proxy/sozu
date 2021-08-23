#![allow(unused_variables, unused_must_use)]
extern crate sozu_lib as sozu;
#[macro_use]
extern crate sozu_command_lib as sozu_command;
extern crate time;
extern crate tiny_http;
extern crate ureq;

use crate::sozu_command::channel::Channel;
use crate::sozu_command::logging::{Logger, LoggerBackend};
use crate::sozu_command::proxy;
use crate::sozu_command::proxy::{LoadBalancingParams, PathRule, Route, RulePosition};
use std::thread;
use std::{
    io::{stdout, Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    str,
    sync::{Arc, Barrier},
    time::Duration,
};
use tiny_http::{Response, Server};

#[test]
fn test() {
    Logger::init(
        "EXAMPLE".to_string(),
        "debug",
        LoggerBackend::Stdout(stdout()),
        None,
    );

    info!("starting up");

    let config = proxy::HttpListener {
        address: "127.0.0.1:8080".parse().expect("could not parse address"),
        ..Default::default()
    };

    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");

    let jg = thread::spawn(move || {
        Logger::init(
            "SOZU".to_string(),
            "trace",
            LoggerBackend::Stdout(stdout()),
            None,
        );
        let max_buffers = 20;
        let buffer_size = 16384;
        sozu::http::start(config, channel, max_buffers, buffer_size);
    });

    command.write_message(&proxy::ProxyRequest {
        id: String::from("ID_Status"),
        order: proxy::ProxyRequestData::Status,
    });

    // wait for sozu to start and answer
    info!("Status -> {:?}", command.read_message());

    let agent = ureq::AgentBuilder::new()
        .resolver(|addr: &str| match addr {
            "example.com:8080" => Ok(vec![([127, 0, 0, 1], 8080).into()]),
            addr => addr.to_socket_addrs().map(Iterator::collect),
        })
        .build();

    info!("expecting 404");
    match agent.get("http://example.com:8080/").call().unwrap_err() {
        ureq::Error::Status(404, res) => {
            assert_eq!(res.header("connection"), Some("close"));
        }
        e => panic!("invalid response: {:?}", e),
    };

    let http_frontend = proxy::HttpFrontend {
        route: Route::ClusterId(String::from("test")),
        address: "127.0.0.1:8080".parse().unwrap(),
        hostname: String::from("example.com"),
        path: PathRule::Prefix(String::from("/")),
        method: None,
        position: RulePosition::Tree,
    };

    command.write_message(&proxy::ProxyRequest {
        id: String::from("ID_ABCD"),
        order: proxy::ProxyRequestData::AddHttpFrontend(http_frontend),
    });
    println!("HTTP -> {:?}", command.read_message());

    info!("expecting 503");
    match agent.get("http://example.com:8080/").call().unwrap_err() {
        ureq::Error::Status(503, res) => {
            assert_eq!(res.header("connection"), Some("close"));
        }
        e => panic!("invalid response: {:?}", e),
    };

    let http_backend = proxy::Backend {
        cluster_id: String::from("test"),
        backend_id: String::from("test-0"),
        address: "127.0.0.1:2048".parse().unwrap(),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        sticky_id: None,
        backup: None,
    };

    command.write_message(&proxy::ProxyRequest {
        id: String::from("ID_EFGH"),
        order: proxy::ProxyRequestData::AddBackend(http_backend),
    });

    println!("HTTP -> {:?}", command.read_message());

    info!("sending invalid request, expecting 400");
    let mut client = TcpStream::connect(("127.0.0.1", 8080)).expect("could not parse address");
    // 1 seconds of timeout
    client.set_read_timeout(Some(Duration::new(1, 0)));

    let w = client.write(&b"HELLO\r\n\r\n"[..]);
    println!("http client write: {:?}", w);

    let expected_answer =
        "HTTP/1.1 400 Bad Request\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    let mut buffer = [0; 4096];
    let mut index = 0;

    let r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
        Err(e) => assert!(false, "client request should not fail. Error: {:?}", e),
        Ok(sz) => {
            index += sz;
            let answer =
                str::from_utf8(&buffer[..index]).expect("could not make string from buffer");
            println!("Response: {}", answer);
        }
    }

    let answer = str::from_utf8(&buffer[..index]).expect("could not make string from buffer");
    println!("Response: {}", answer);
    assert_eq!(answer, expected_answer);

    let barrier = Arc::new(Barrier::new(2));
    let barrier2 = barrier.clone();

    let _ = thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:2048").expect("could not parse address");
        barrier2.wait();
        let mut stream = listener.incoming().next().unwrap().unwrap();
        let response = b"TEST\r\n\r\n";
        stream.write(&response[..]).unwrap();
    });

    barrier.wait();
    info!("expecting 502");
    match agent.get("http://example.com:8080/").call().unwrap_err() {
        ureq::Error::Status(502, res) => {
            assert_eq!(res.status_text(), "Bad Gateway");
        }
        e => panic!("invalid response: {:?}", e),
    };

    command.write_message(&proxy::ProxyRequest {
        id: String::from("ID_EFGH-2"),
        order: proxy::ProxyRequestData::RemoveBackend(proxy::RemoveBackend {
            cluster_id: String::from("test"),
            backend_id: String::from("test-0"),
            address: "127.0.0.1:2048".parse().unwrap(),
        }),
    });

    let http_backend = proxy::Backend {
        cluster_id: String::from("test"),
        backend_id: String::from("test-0"),
        address: "127.0.0.1:2048".parse().unwrap(),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        sticky_id: None,
        backup: None,
    };

    command.write_message(&proxy::ProxyRequest {
        id: String::from("ID_EFGH-3"),
        order: proxy::ProxyRequestData::AddBackend(http_backend),
    });

    let barrier2 = barrier.clone();
    let _ = thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:2048").expect("could not parse address");
        barrier2.wait();
        let mut stream = listener.incoming().next().unwrap().unwrap();
        let response = b"HTTP/1.1 200 Ok\r\nConnection: close\r\nContent-Length: 5\r\n\r\nHello";
        stream.write(&response[..]).unwrap();
        stream.shutdown(std::net::Shutdown::Both).unwrap();
    });

    barrier.wait();
    info!("expecting 200 then close");
    let res = agent.get("http://example.com:8080/").call().unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.status_text(), "Ok");
    assert_eq!(res.header("Content-Length"), Some("5"));
    assert_eq!(res.header("Connection"), None);
    assert_eq!(res.into_string().unwrap(), "Hello");

    let barrier2 = barrier.clone();
    let _ = thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:2048").expect("could not parse address");
        barrier2.wait();
        let mut stream = listener.incoming().next().unwrap().unwrap();
        let response = b"HTTP/1.1 200 Ok\r\nConnection: close\r\n\r\nHello world!";
        stream.write(&response[..]).unwrap();
        stream.shutdown(std::net::Shutdown::Both).unwrap();
    });

    barrier.wait();
    info!("expecting 200 then close, without content length");
    let res = agent.get("http://example.com:8080/").call().unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.status_text(), "Ok");
    assert_eq!(res.into_string().unwrap(), "Hello world!");

    let barrier2 = barrier.clone();
    let _ = thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:2048").expect("could not parse address");
        barrier2.wait();
        let stream = listener.incoming().next().unwrap().unwrap();
        stream.shutdown(std::net::Shutdown::Both).unwrap();
    });

    barrier.wait();
    info!("server closes, expecting 503");
    match agent.get("http://example.com:8080/").call().unwrap_err() {
        ureq::Error::Status(503, res) => {
            println!("res: {:?}", res);
            assert_eq!(res.status_text(), "Service Unavailable");
        }
        e => panic!("invalid response: {:?}", e),
    };

    command.write_message(&proxy::ProxyRequest {
        id: String::from("ID_EFGH-2"),
        order: proxy::ProxyRequestData::RemoveBackend(proxy::RemoveBackend {
            cluster_id: String::from("test"),
            backend_id: String::from("test-0"),
            address: "127.0.0.1:2048".parse().unwrap(),
        }),
    });

    let http_backend = proxy::Backend {
        cluster_id: String::from("test"),
        backend_id: String::from("test-0"),
        address: "127.0.0.1:2048".parse().unwrap(),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        sticky_id: None,
        backup: None,
    };

    command.write_message(&proxy::ProxyRequest {
        id: String::from("ID_EFGH-3"),
        order: proxy::ProxyRequestData::AddBackend(http_backend),
    });

    let barrier2 = barrier.clone();
    let _ = thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:2048").expect("could not parse address");
        barrier2.wait();
        let mut stream = listener.incoming().next().unwrap().unwrap();
        let response = b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: WebSocket\r\n\r\n";
        stream.write(&response[..]).unwrap();
        stream.shutdown(std::net::Shutdown::Both).unwrap();
    });

    barrier.wait();
    info!("expecting upgrade (101 switching protocols)");
    let res = agent.get("http://example.com:8080/").call().unwrap();
    println!("res: {:?}", res);
    assert_eq!(res.status(), 101);
    assert_eq!(res.header("Upgrade"), Some("WebSocket"));
    assert_eq!(res.header("Connection"), Some("Upgrade"));
    info!("good bye");

    start_server(2048, barrier.clone());

    barrier.wait();
    info!("expecting 200");
    let res = agent.get("http://example.com:8080/").call().unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.into_string().unwrap(), "hello world");

    //let _ = jg.join();
    //barrier.wait();
    info!("expecting 100");
    let res = agent
        .get("http://example.com:8080/100")
        .set("Expect", "100-continue")
        .call()
        .unwrap();
    assert_eq!(res.status(), 100);
    assert_eq!(res.header("Content-Length"), Some("1024"));

    // check that there's no actual body
    /*let mut reader = res.into_reader();
    let mut buf = [0u8; 1024];
    // we should do that in async, to add a short timeout
    assert!(reader.read(&mut buf).is_err());
    */

    info!("good bye");
}

fn start_server(port: u16, barrier: Arc<Barrier>) {
    thread::spawn(move || {
        let server = Server::http(&format!("127.0.0.1:{}", port)).expect("could not create server");
        info!("starting web server in port {}", port);

        barrier.wait();
        for request in server.incoming_requests() {
            eprintln!(
                "backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
                request.method(),
                request.url(),
                request.headers()
            );
            eprintln!("url: {:?}", request.url());

            if request.url() == "/100" {
                let response = Response::new(
                    tiny_http::StatusCode(100),
                    vec![
                        tiny_http::Header::from_bytes(&b"Content-Length"[..], &b"1024"[..])
                            .unwrap(),
                    ],
                    &b""[..],
                    None,
                    None,
                );
                request.respond(response).unwrap();
            } else {
                let response = Response::from_string("hello world");
                request.respond(response).unwrap();
            }

            eprintln!("backend web server sent response");
            eprintln!("server session stopped");
        }

        eprintln!("server on port {} closed", port);
    });
}
