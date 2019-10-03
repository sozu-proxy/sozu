#[macro_export]
macro_rules! take_while_complete (
  ($input:expr, $submac:ident!( $($args:tt)* )) => ({
    use nom::InputTakeAtPosition;
    use nom::Err;

    let input = $input;
    match input.split_at_position(|c| !$submac!(c, $($args)*)) {
      Err(Err::Incomplete(_)) => Ok((&input[input.len()..], input)),
      res => res,
    }
  });
  ($input:expr, $f:expr) => (
    take_while_complete!($input, call!($f));
  );
);

#[macro_export]
macro_rules! take_while1_complete (
  ($input:expr, $submac:ident!( $($args:tt)* )) => ({
    use nom::InputTakeAtPosition;
    use nom::Err;
    use nom::ErrorKind;

    let input = $input;
    match input.split_at_position1(|c| !$submac!(c, $($args)*), ErrorKind::TakeWhile1) {
      Err(Err::Incomplete(_)) => Ok((&input[input.len()..], input)),
      res => res,
    }
  });
  ($input:expr, $f:expr) => (
    take_while1_complete!($input, call!($f));
  );
);

#[macro_export]
macro_rules! empty (
  ($i:expr,) => (
    {
      use std::result::Result::*;
      use nom::{Err,ErrorKind};
      use nom::InputLength;

      if ($i).input_len() == 0 {
        Ok(($i, $i))
      } else {
        Err(Err::Error(error_position!($i, ErrorKind::Eof::<u32>)))
      }
    }
  );
);

pub mod http;
pub mod pipe;
#[cfg(feature = "use-openssl")]
pub mod openssl;
pub mod rustls;
pub mod proxy_protocol;
pub mod h2;

#[cfg(feature = "use-openssl")]
pub use self::openssl::TlsHandshake;
pub use self::pipe::Pipe;
pub use self::http::{Http,StickySession};
pub use self::proxy_protocol::send::SendProxyProtocol;

#[cfg(not(feature = "use-openssl"))]
pub use self::rustls::TlsHandshake;

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum ProtocolResult {
  Upgrade,
  Continue,
}
