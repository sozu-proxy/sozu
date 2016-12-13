//! This library provides tools to build a HTTP proxy
//!
//! It handles network polling, HTTP parsing, TLS in a fast single threaded event
//! loop.
//!
//! It is designed to receive configuration changes at runtime instead of
//! reloading from a file regularly. The event loop runs in its own thread
//! and receives commands through a message queue.
//!
//! To create a HTTP proxy, you first need to create a `HttpProxyConfiguration`
//! structure (there are configuration structures for the HTTPS and TCP proxies
//! too).
//!
//! ```ignore
//! let config = messages::HttpProxyConfiguration {
//!   front: "198.51.100.0:80".parse().unwrap(),
//!   ..Default::default()
//! };
//! ```
//!
//! Then create the required elements to communicate with the proxy thread,
//! and launch the thread:
//!
//! ```ignore
//! let config = messages::HttpProxyConfiguration {
//!   front: "198.51.100.0:80".parse().unwrap(),
//!   ..Default::default()
//! };
//!
//! let (tx, rx)      = mio::channel::channel::<ProxyOrder>();
//! let (sender, rec) = mpsc::channel::<network::ServerMessage>();
//!
//! let jg            = thread::spawn(move || {
//!   network::http::start_listener(String::from("HTTP"), config, sender, rx);
//! });
//! ```
//!
//! The `tx, rx` channel here is a mio channel through which the proxy will
//! receive its orders, and it will use the `(sender,rec)` one to send
//! acknowledgements and various data.
//!
//! Once the thread is launched, the proxy will start its event loop and handle
//! events on the listening interface and port specified in the configuration
//! object. Since no applications to proxy for were specified, it will receive
//! the connections, parse the request, then send a default (but configurable)
//! answer.
//!
//! ```ignore
//! let http_front = messages::HttpFront {
//!   app_id:     String::from("test"),
//!   hostname:   String::from("example.com"),
//!   path_begin: String::from("/")
//! };
//! let http_instance = messages::Instance {
//!   app_id:     String::from("test"),
//!   ip_address: String::from("192.0.2.1"),
//!   port:       8080
//! };
//!
//! tx.send(network::ProxyOrder::Command(
//!   String::from("ID_ABCD"),
//!   messages::Command::AddHttpFront(http_front)
//! ));
//!
//! tx.send(network::ProxyOrder::Command(
//!   String::from("ID_EFGH"),
//!   messages::Command::AddInstance(http_instance)
//! ));
//!
//! println!("HTTP -> {:?}", rec.recv().unwrap());
//! println!("HTTP -> {:?}", rec.recv().unwrap());
//! ```
//!
//! An application is identified by its `app_id`, a string that will be shared
//! between one or multiple "fronts", and one or multiple "instances".
//!
//! A "front" is a way to recognize a request and match it to an `app_id`,
//! depending on the hostname and the beginning of the URL path.
//!
//! An instance corresponds to one backend server, indicated by its IP and port.
//!
//! An application can have multiple backend servers, and they can be added or
//! removed while the proxy is running. If a backend is removed from the configuration
//! while the proxy is handling a request to that server, it will finish that
//! request and stop sending new traffic to that server.
//!
//! The fronts and instances are specified with messages sent through the
//! communication channels with the proxy event loop. Once the configuration
//! options are added to the proxy's state, it will send back an acknowledgement
//! message.
//!
//! Here is the comple example for reference:
//!
//! ```ignore
//! #![allow(unused_variables,unused_must_use)]
//! #[macro_use] extern crate log;
//! extern crate env_logger;
//! extern crate sozu_lib as sozu;
//! extern crate openssl;
//! extern crate mio;
//!
//! use std::thread;
//! use std::sync::mpsc;
//! use sozu::messages;
//! use sozu::network::{self,ProxyOrder};

//! fn main() {
//!   env_logger::init().unwrap();
//!   info!("starting up");
//!
//!   let config = messages::HttpProxyConfiguration {
//!     front: "198.51.100.0:80".parse().unwrap(),
//!     ..Default::default()
//!   };
//!
//!   let (tx, rx)      = mio::channel::channel::<ProxyOrder>();
//!   let (sender, rec) = mpsc::channel::<network::ServerMessage>();
//!
//!   let jg            = thread::spawn(move || {
//!     network::http::start_listener(String::from("HTTP"), config, sender, rx);
//!   });
//!
//!   let http_front = messages::HttpFront {
//!     app_id:     String::from("test"),
//!     hostname:   String::from("example.com"),
//!     path_begin: String::from("/")
//!   };
//!   let http_instance = messages::Instance {
//!     app_id:     String::from("test"),
//!     ip_address: String::from("192.0.2.1"),
//!     port:       8080
//!   };
//!
//!   tx.send(network::ProxyOrder::Command(
//!     String::from("ID_ABCD"),
//!     messages::Command::AddHttpFront(http_front)
//!   ));
//!
//!   tx.send(network::ProxyOrder::Command(
//!     String::from("ID_EFGH"),
//!     messages::Command::AddInstance(http_instance)
//!   ));
//!
//!   println!("HTTP -> {:?}", rec.recv().unwrap());
//!   println!("HTTP -> {:?}", rec.recv().unwrap());
//!
//!   let _ = jg.join();
//!   info!("good bye");
//! }
//! ```
//!
#![feature(proc_macro)]
#![cfg_attr(test, feature(test))]
#[cfg(test)]
extern crate test;

#[macro_use] extern crate nom;
#[macro_use] extern crate log;
#[macro_use] extern crate lazy_static;
#[macro_use] extern crate serde_derive;
extern crate mio;
extern crate bytes;
extern crate time;
extern crate serde;
extern crate serde_json;
extern crate rand;
extern crate openssl;
extern crate pool;
extern crate uuid;
extern crate net2;
extern crate libc;
extern crate slab;

pub mod network;
pub mod parser;
pub mod messages;
