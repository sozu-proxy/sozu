#![feature(ip_addr)]

#![cfg_attr(test, feature(test))]
#[cfg(test)]
extern crate test;

#[macro_use] extern crate nom;
#[macro_use] extern crate log;
#[macro_use] extern crate lazy_static;
extern crate mio;
extern crate bytes;
extern crate time;
extern crate rustc_serialize;
extern crate rand;
extern crate openssl;
extern crate pool;
extern crate uuid;

pub mod network;
pub mod parser;
pub mod messages;
