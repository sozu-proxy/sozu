pub mod tls;

pub use self::tls::TlsHandshake;

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum ProtocolResult {
  Upgrade,
  Continue,
}
