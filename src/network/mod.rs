#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

pub mod amqp;
pub mod http;

#[cfg(feature = "splice")]
mod splice;

pub mod tcp;


