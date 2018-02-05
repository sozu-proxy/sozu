pub mod http;
pub mod pipe;
#[cfg(feature = "use-openssl")]
pub mod openssl;
pub mod rustls;
pub mod proxy_protocol;

#[cfg(feature = "use-openssl")]
pub use self::openssl::TlsHandshake;
pub use self::pipe::Pipe;
pub use self::http::{Http,StickySession};
pub use self::proxy_protocol::ProxyProtocol;

#[cfg(not(feature = "use-openssl"))]
pub use self::rustls::TlsHandshake;

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum ProtocolResult {
  Upgrade,
  Continue,
}
