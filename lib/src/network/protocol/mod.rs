pub mod http;
pub mod pipe;
pub mod openssl;

pub use self::openssl::TlsHandshake;
pub use self::pipe::Pipe;
pub use self::http::{Http,StickySession};

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum ProtocolResult {
  Upgrade,
  Continue,
}
