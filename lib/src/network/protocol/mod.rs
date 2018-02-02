pub mod http;
pub mod pipe;
#[cfg(feature = "use_openssl")]
pub mod openssl;
pub mod rustls;

#[cfg(feature = "use_openssl")]
pub use self::openssl::TlsHandshake;

pub use self::pipe::Pipe;
pub use self::http::{Http,StickySession};

#[cfg(not(feature = "use_openssl"))]
pub use self::rustls::TlsHandshake;

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum ProtocolResult {
  Upgrade,
  Continue,
}
