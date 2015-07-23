#[macro_use] extern crate nom;
extern crate mio;
extern crate time;

mod network;
mod parser;

use std::sync::mpsc::{channel};
use std::thread;

fn main() {
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

