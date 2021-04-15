#![allow(unused_variables,unused_must_use)]
extern crate sozu_lib as sozu;
#[macro_use] extern crate sozu_command_lib as sozu_command;
extern crate time;
extern crate tiny_http;
extern crate ureq;

use std::thread;
use sozu_command::proxy;
use sozu_command::channel::Channel;
use sozu_command::proxy::LoadBalancingParams;
use sozu_command::logging::{Logger,LoggerBackend};
use tiny_http::{Server, Response};
use std::{
    io::{stdout, Read, Write},
    net::{TcpStream,ToSocketAddrs},
    time::Duration,
    str
};

#[test]
fn test() {
    Logger::init("EXAMPLE".to_string(), "debug", LoggerBackend::Stdout(stdout()), None);

    info!("starting up");

    let config = proxy::HttpListener {
        front: "127.0.0.1:8080".parse().expect("could not parse address"),
        ..Default::default()
    };

    let (mut command, channel) = Channel::generate(1000, 10000).expect("should create a channel");

    let jg = thread::spawn(move || {
        Logger::init("SOZU".to_string(), "info", LoggerBackend::Stdout(stdout()), None);
        let max_buffers = 20;
        let buffer_size = 16384;
        sozu::http::start(config, channel, max_buffers, buffer_size);
    });

    command.write_message(&proxy::ProxyRequest {
        id:    String::from("ID_Status"),
        order: proxy::ProxyRequestData::Status
    });

    // wait for sozu to start and answer
    info!("Status -> {:?}", command.read_message());

    let agent = ureq::AgentBuilder::new()
        .resolver(|addr: &str| match addr {
            "example.com:8080" => Ok(vec![([127,0,0,1], 8080).into()]),
            addr => addr.to_socket_addrs().map(Iterator::collect),
        })
    .build();


    info!("expecting 404");
    match agent
        .get("http://example.com:8080/")
        .call().unwrap_err() {
            ureq::Error::Status(404, res) => {
                assert_eq!(res.header("connection"), Some("close"));
            },
            e => panic!("invalid response: {:?}", e),
        };


    let http_front = proxy::HttpFront {
        app_id:     String::from("test"),
        address:    "127.0.0.1:8080".parse().unwrap(),
        hostname:   String::from("example.com"),
        path_begin: String::from("/"),
    };

    command.write_message(&proxy::ProxyRequest {
        id:    String::from("ID_ABCD"),
        order: proxy::ProxyRequestData::AddHttpFront(http_front)
    });
    println!("HTTP -> {:?}", command.read_message());

    info!("expecting 503");
    match agent
        .get("http://example.com:8080/")
        .call().unwrap_err() {
            ureq::Error::Status(503, res) => {
                assert_eq!(res.header("connection"), Some("close"));
            },
            e => panic!("invalid response: {:?}", e),
        };

    let http_backend = proxy::Backend {
        app_id:                    String::from("test"),
        backend_id:                String::from("test-0"),
        address:                   "127.0.0.1:1024".parse().unwrap(),
        load_balancing_parameters: Some(LoadBalancingParams::default()),
        sticky_id:                 None,
        backup:                    None,
    };

    command.write_message(&proxy::ProxyRequest {
        id:    String::from("ID_EFGH"),
        order: proxy::ProxyRequestData::AddBackend(http_backend)
    });

    println!("HTTP -> {:?}", command.read_message());

    start_server(1024);

    info!("expecting 200");
    let res = agent
        .get("http://example.com:8080/")
        .call().unwrap();
    assert_eq!(res.status(), 200);


    info!("sending invalid request");
    let mut client = TcpStream::connect(("127.0.0.1", 8080)).expect("could not parse address");
    // 1 seconds of timeout
    client.set_read_timeout(Some(Duration::new(1,0)));

    let w = client.write(&b"HELLO\r\n\r\n"[..]);
    println!("http client write: {:?}", w);

    let expected_answer = "HTTP/1.1 400 Bad Request\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    let mut buffer = [0;4096];
    let mut index = 0;

    let r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
        Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
        Ok(sz) => {
            index += sz;
            let answer = str::from_utf8(&buffer[..index]).expect("could not make string from buffer");
            println!("Response: {}", answer);
        }
    }

    let answer = str::from_utf8(&buffer[..index]).expect("could not make string from buffer");
    println!("Response: {}", answer);
    assert_eq!(answer, expected_answer);

    //let _ = jg.join();
    info!("good bye");
}

fn start_server(port: u16) {
    thread::spawn(move|| {
        let server = Server::http(&format!("127.0.0.1:{}", port)).expect("could not create server");
        info!("starting web server in port {}", port);

        for request in server.incoming_requests() {
            println!("backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
                     request.method(),
                     request.url(),
                     request.headers()
                    );

            let response = Response::from_string("hello world");
            request.respond(response).unwrap();
            println!("backend web server sent response");
            println!("server session stopped");
        }

        println!("server on port {} closed", port);
    });
}
