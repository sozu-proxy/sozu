#[macro_use] extern crate nom;
extern crate mio;
extern crate time;
extern crate libc;
extern crate amqp;
extern crate env_logger;
extern crate rustc_serialize;

mod bus;
mod network;
mod parser;
mod messages;

use std::sync::mpsc::{channel};
use std::thread;

use messages::{Command, Instance, TcpFront, Topic};
use bus::Message;

fn main() {
  let bus_tx = bus::start_bus();

  let (http_proxy_conf_input,http_proxy_conf_listener) = channel();
  let _ = bus_tx.send(Message::Subscribe(Topic::HttpProxyConfig, http_proxy_conf_input));
  if let Ok(Message::SubscribeOk) = http_proxy_conf_listener.recv() {
    println!("Subscribed to http_proxy_conf commands");

    network::amqp::init_rabbitmq(bus_tx);
    println!("Subscribed to http_proxy_conf commands");
    let res = http_proxy_conf_listener.recv();
    let _ = res.map(|x| x.display());

    println!("yolo");
    //if let Ok(Message::Msg(t, c)) = res {
    //    println!("Got ADD_ACL command");
    //    println!("{:?}", c);
    //}
  }

  let (sender, receiver) = channel::<network::ServerMessage>();
  let (tx, jg) = network::start_listener(10, 500, sender);
  println!("rustyXORP");

  let _ = tx.send(network::TcpProxyOrder::Command(Command::AddTcpFront(TcpFront { app_id: String::from("yolo"), port: 8080 })));
  thread::sleep_ms(200);
  println!("server said: {:?}", receiver.recv());

  let _ = tx.send(network::TcpProxyOrder::Command(Command::AddInstance(Instance { app_id: String::from("yolo"), port: 9090, ip_address: String::from("127.0.0.1") })));
  thread::sleep_ms(200);
  println!("server said: {:?}", receiver.recv());


  thread::sleep_ms(20000);
  let _ = tx.send(network::TcpProxyOrder::Stop);
  thread::sleep_ms(200);
  println!("server said: {:?}", receiver.recv());
  let _ = jg.join();
  println!("good bye");
}

