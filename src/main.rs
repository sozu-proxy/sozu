#[macro_use] extern crate nom;
extern crate mio;
extern crate time;

mod network;
mod parser;

fn main() {
  let (tx, jg) = network::start_listener("hello");
  println!("rustyXORP");
  jg.join();
}

