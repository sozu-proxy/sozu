#[macro_use] extern crate nom;
extern crate mio;
extern crate time;

mod network;
mod parser;

fn main() {
  let (tx, jg) = network::start_listener(10, 500);
  println!("rustyXORP");

  //use std::thread;
  //tx.send(network::Message::AddServer("127.0.0.1:1234".to_string(), "127.0.0.1:5678".to_string()));
  //thread::sleep_ms(200);
  //tx.send(network::Message::RemoveServer(0));
  //jg.join();
}

