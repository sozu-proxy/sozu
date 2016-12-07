pub mod http;
pub mod tls;

pub use self::tls::TlsHandshake;
pub use self::http::Http;

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum ProtocolResult {
  Upgrade,
  Continue,
}
