/// Used to hold and accumulate any data on an asynchronous task
pub trait Aggregator {}

/// Gathers only data on sent and received requests
#[derive(Debug, Clone)]
pub struct SimpleAggregator {
    pub requests_received: usize,
    pub responses_sent: usize,
}
impl Aggregator for SimpleAggregator {}
