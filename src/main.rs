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

use messages::Topic;
use bus::Message;

fn main() {
  let bus_tx = bus::start_bus();

  let (http_proxy_conf_input,http_proxy_conf_listener) = channel();
  bus_tx.send(Message::Subscribe(Topic::HttpProxyConfig, http_proxy_conf_input));
  if let Ok(Message::SubscribeOk) = http_proxy_conf_listener.recv() {
    println!("Subscribed to http_proxy_conf commands");

    network::amqp::init_rabbitmq(bus_tx);
    println!("Subscribed to http_proxy_conf commands");
    let res = http_proxy_conf_listener.recv();
    res.map(|x| x.display());

    println!("yolo");
    //if let Ok(Message::Msg(t, c)) = res {
    //    println!("Got ADD_ACL command");
    //    println!("{:?}", c);
    //}
  }

  let (sender, receiver) = channel::<network::ServerMessage>();
  let (tx, jg) = network::start_listener(10, 500, sender);
  println!("rustyXORP");

  tx.send(network::ServerOrder::AddServer("127.0.0.1:1234".to_string(), "127.0.0.1:5678".to_string()));
  thread::sleep_ms(200);
  println!("server said: {:?}", receiver.recv());
  //tx.send(network::ServerOrder::RemoveServer(0));
  tx.send(network::ServerOrder::Stop);
  println!("server said: {:?}", receiver.recv());
  jg.join();
  println!("good bye");
}

