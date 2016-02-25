#[macro_use] extern crate log;
extern crate env_logger;
extern crate yxorp;

use std::net::{UdpSocket,ToSocketAddrs};
use std::sync::mpsc::{channel};
use yxorp::{network,bus};
use yxorp::messages::{self,Topic};
use yxorp::bus::Message;
use yxorp::network::metrics::{METRICS,ProxyMetrics};

fn main() {
  env_logger::init().unwrap();
  info!("starting up");
  let metrics_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
  let metrics_host   = ("192.168.59.103", 8125).to_socket_addrs().unwrap().next().unwrap();
  METRICS.lock().unwrap().set_up_remote(metrics_socket, metrics_host);
  let metrics_guard = ProxyMetrics::run();
  METRICS.lock().unwrap().gauge("TEST", 42);

  let bus_tx = bus::start_bus();
  let (sender, _) = channel::<network::ServerMessage>();
  let (tx, jg) = network::http::start_listener("127.0.0.1:8080".parse().unwrap(), 10, 500, sender);

  let (http_proxy_conf_input,http_proxy_conf_listener) = channel();
  let _ = bus_tx.send(Message::Subscribe(Topic::HttpProxyConfig, http_proxy_conf_input));
  /*let config_tx = tx.clone();
  if let Ok(Message::SubscribeOk) = http_proxy_conf_listener.recv() {
  info!("Subscribed to http_proxy_conf commands");

    network::amqp::init_rabbitmq(bus_tx);
    info!("Subscribed to http_proxy_conf commands");

    thread::spawn(move || {
      loop {
        if let Ok(Message::Msg(command)) = http_proxy_conf_listener.recv() {
          if let Err(e) = config_tx.send(network::http::HttpProxyOrder::Command(command)) {
            info!("Error sending HttpProxyOrder: {:?}", e);
          }
        }
      }
    });
  }
  */

  let http_front = messages::HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/"), port: 8080 };
  let http_instance = messages::Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1026 };
  tx.send(network::ProxyOrder::Command(messages::Command::AddHttpFront(http_front)));
  tx.send(network::ProxyOrder::Command(messages::Command::AddInstance(http_instance)));

  let (sender2, _) = channel::<network::ServerMessage>();
  let (tx2, jg2) = network::tls::start_listener("127.0.0.1:8443".parse().unwrap(), 10, 500, sender2);
  let tls_front = messages::TlsFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st"), path_begin: String::from("/"), port: 8443, cert_path: String::from("assets/certificate.pem"), key_path: String::from("assets/key.pem") };
  tx2.send(network::ProxyOrder::Command(messages::Command::AddTlsFront(tls_front)));
  let tls_instance = messages::Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1026 };
  tx2.send(network::ProxyOrder::Command(messages::Command::AddInstance(tls_instance)));

  let tls_front2 = messages::TlsFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/"), port: 8443, cert_path: String::from("assets/cert_test.pem"), key_path: String::from("assets/key_test.pem") };
  tx2.send(network::ProxyOrder::Command(messages::Command::AddTlsFront(tls_front2)));
  let tls_instance2 = messages::Instance { app_id: String::from("app_2"), ip_address: String::from("127.0.0.1"), port: 1026 };
  tx2.send(network::ProxyOrder::Command(messages::Command::AddInstance(tls_instance2)));

  let _ = jg.join();
  info!("good bye");
}

