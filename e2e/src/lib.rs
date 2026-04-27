#![allow(dead_code)]
#![allow(unexpected_cfgs)]

mod http_utils;
mod mock;
mod port_registry;
mod sozu;
#[cfg(test)]
#[cfg(not(tarpaulin))]
mod tests;

const BUFFER_SIZE: usize = 4096;
