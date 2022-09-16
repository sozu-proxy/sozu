
#![allow(dead_code)]

mod http_utils;
mod mock;
mod sozu;
#[cfg(test)]
#[cfg(not(tarpaulin))]
mod tests;

const BUFFER_SIZE: usize = 4096;
