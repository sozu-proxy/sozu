use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::fmt;

#[derive(PartialEq, Debug)]
pub enum ProxyProtocolHeader {
  V1(HeaderV1),
  V2(HeaderV2),
}

impl ProxyProtocolHeader {
  // Use this method to writte the header in the backend socket
  pub fn into_bytes(&self) -> Vec<u8> {
    match *self {
      ProxyProtocolHeader::V1(ref header) => header.into_bytes(),
      ProxyProtocolHeader::V2(ref header) => header.into_bytes(),
    }
  }
}

/// Indicate the proxied INET protocol and family
#[derive(Debug, PartialEq, Eq)]
pub enum ProtocolSupportedV1 {
  TCP4,    // TCP over IPv4
  TCP6,    // TCP over IPv6
  UNKNOWN, // unsupported,or unknown protocols
}

impl fmt::Display for ProtocolSupportedV1 {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match *self {
      ProtocolSupportedV1::TCP4 => write!(f, "TCP4"),
      ProtocolSupportedV1::TCP6 => write!(f, "TCP6"),
      ProtocolSupportedV1::UNKNOWN => write!(f, "UNKNOWN"),
    }
  }
}

/// Proxy Protocol header for version 1 (text version)
/// Example:
/// - TCP/IPv4: `PROXY TCP4 255.255.255.255 255.255.255.255 65535 65535\r\n`
/// - TCP/IPv6: `PROXY TCP6 ffff:f...f:ffff ffff:f...f:ffff 65535 65535\r\n`
/// - Unknown: `PROXY UNKNOWN\r\n`
#[derive(Debug, PartialEq, Eq)]
pub struct HeaderV1 {
  pub protocol: ProtocolSupportedV1,
  pub addr_src: SocketAddr,
  pub addr_dst: SocketAddr,
}

const PROXY_PROTO_IDENTIFIER: &str = "PROXY";

impl HeaderV1 {

  pub fn new (addr_src: SocketAddr, addr_dst: SocketAddr) -> Self {
    let protocol = if addr_dst.is_ipv6() {
      ProtocolSupportedV1::TCP6
    } else if addr_dst.is_ipv4() {
      ProtocolSupportedV1::TCP4
    } else {
      ProtocolSupportedV1::UNKNOWN
    };

    HeaderV1 {
      protocol,
      addr_src,
      addr_dst,
    }
  }

  pub fn into_bytes(&self) -> Vec<u8> {
    if self.protocol.eq(&ProtocolSupportedV1::UNKNOWN) {
      format!("{} {}\r\n",
          PROXY_PROTO_IDENTIFIER,
          self.protocol,
      ).into_bytes()
    } else {
      format!("{} {} {} {} {} {}\r\n",
        PROXY_PROTO_IDENTIFIER,
        self.protocol,
        self.addr_src.ip(),
        self.addr_dst.ip(),
        self.addr_src.port(),
        self.addr_dst.port(),
      ).into_bytes()
    }
  }
}

#[cfg(test)]
mod test {

  use super::*;
  use std::net::{IpAddr, Ipv4Addr, SocketAddr, Ipv6Addr};

  #[test]
  fn it_should_return_a_correct_header_with_ipv4() {
    let header = HeaderV1::new(
      SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 80),
      SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 17, 40, 59)), 80)
    );

    let header_to_cmp = "PROXY TCP4 127.0.0.1 172.17.40.59 80 80\r\n".as_bytes();

    assert_eq!(header_to_cmp, &header.into_bytes()[..]);
  }

  #[test]
  fn it_should_return_a_correct_header_with_ipv6() {
    let header = HeaderV1::new(
      SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0xffff)), 80),
      SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0x9c, 0x76)), 80)
    );

    let header_to_cmp = "PROXY TCP6 ::0.0.255.255 ::0.156.0.118 80 80\r\n".as_bytes();
    assert_eq!(header_to_cmp, &header.into_bytes()[..]);
  }
}

#[derive(Debug, PartialEq)]
pub enum Command {
  Local,
  Proxy,
}

#[derive(Debug, PartialEq)]
pub struct HeaderV2 {
  pub command: Command,
  pub family: u8,          // protocol family and address
  pub addr: ProxyAddr,
}

impl HeaderV2 {
  pub fn new(command: Command, addr_src: SocketAddr, addr_dst: SocketAddr) -> Self {
    let addr = ProxyAddr::from(addr_src, addr_dst);

    HeaderV2 {
      command,
      family: get_family(&addr),
      addr,
    }
  }

  pub fn into_bytes(&self) -> Vec<u8> {
    let mut header = Vec::with_capacity(self.len());

    let signature = [0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A];
    header.extend_from_slice(&signature);

    let command = match self.command {
      Command::Local => 0,
      Command::Proxy => 1,
    };
    let ver_and_cmd = 0x20 | command;
    header.push(ver_and_cmd);

    header.push(self.family);
    header.extend_from_slice(&u16_to_array_of_u8(self.addr.len()));
    self.addr.into_bytes(&mut header);
    header
  }

  pub fn len(&self) -> usize {
    // signature + ver_and_cmd + family + len + addr
    12 + 1 + 1 + 2 + self.addr.len() as usize
  }
}

pub enum ProxyAddr {
  Ipv4Addr {
    src_addr: SocketAddrV4,
    dst_addr: SocketAddrV4,
  },
  Ipv6Addr {
    src_addr: SocketAddrV6,
    dst_addr: SocketAddrV6,
  },
  UnixAddr {
    src_addr: [u8; 108],
    dst_addr: [u8; 108],
  },
  AfUnspec,
}

impl ProxyAddr {

  pub fn from(addr_src: SocketAddr, addr_dst : SocketAddr) -> Self {
    match (addr_src, addr_dst) {
      (SocketAddr::V4(addr_ipv4_src), SocketAddr::V4(addr_ipv4_dst)) => {
        ProxyAddr::Ipv4Addr{
          src_addr: addr_ipv4_src,
          dst_addr: addr_ipv4_dst,
        }
      },
      (SocketAddr::V6(addr_ipv6_src), SocketAddr::V6(addr_ipv6_dst)) => {
        ProxyAddr::Ipv6Addr{
          src_addr: addr_ipv6_src,
          dst_addr: addr_ipv6_dst,
        }
      },
      _ => ProxyAddr::AfUnspec,
    }
  }

  fn len(&self) -> u16 {
    match *self {
      ProxyAddr::Ipv4Addr{ .. } => 12,
      ProxyAddr::Ipv6Addr{ .. } => 36,
      ProxyAddr::UnixAddr{ .. } => 216,
      ProxyAddr::AfUnspec => 0,
    }
  }

  pub fn source(&self) -> Option<SocketAddr> {
    match self {
      &ProxyAddr::Ipv4Addr { src_addr: src, .. } => Some(SocketAddr::V4(src)),
      &ProxyAddr::Ipv6Addr { src_addr: src, .. } => Some(SocketAddr::V6(src)),
      _                              => None,
    }
  }

  pub fn destination(&self) -> Option<SocketAddr> {
    match self {
      &ProxyAddr::Ipv4Addr { dst_addr: dst, .. } => Some(SocketAddr::V4(dst)),
      &ProxyAddr::Ipv6Addr { dst_addr: dst, .. } => Some(SocketAddr::V6(dst)),
      _                              => None,
    }
  }

  fn into_bytes(&self, buf: &mut Vec<u8>) {
    match *self {
      ProxyAddr::Ipv4Addr{ src_addr, dst_addr } => {
        buf.extend_from_slice(&src_addr.ip().octets());
        buf.extend_from_slice(&dst_addr.ip().octets());
        buf.extend_from_slice(&u16_to_array_of_u8(src_addr.port()));
        buf.extend_from_slice(&u16_to_array_of_u8(dst_addr.port()));
      },
      ProxyAddr::Ipv6Addr{ src_addr, dst_addr } => {
        buf.extend_from_slice(&src_addr.ip().octets());
        buf.extend_from_slice(&dst_addr.ip().octets());
        buf.extend_from_slice(&u16_to_array_of_u8(src_addr.port()));
        buf.extend_from_slice(&u16_to_array_of_u8(dst_addr.port()));
      },
      ProxyAddr::UnixAddr{ src_addr, dst_addr } => {
        buf.extend_from_slice(&src_addr);
        buf.extend_from_slice(&dst_addr);
      },
      ProxyAddr::AfUnspec => {},
    };
  }
}

// Implemented because we don't have the Debug for [u8; 108] (UnixAddr case)
impl fmt::Debug for ProxyAddr {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match *self {
      ProxyAddr::Ipv4Addr{src_addr, dst_addr} => write!(f, "{:?} {:?}", dst_addr, src_addr),
      ProxyAddr::Ipv6Addr{src_addr, dst_addr} => write!(f, "{:?} {:?}", dst_addr, src_addr),
      ProxyAddr::UnixAddr{src_addr, dst_addr} => write!(f, "{:?} {:?}", &dst_addr[..], &src_addr[..]),
      ProxyAddr::AfUnspec =>  write!(f, "AFUNSPEC"),
    }
  }
}

// Implemented because we don't have the PartialEq for [u8; 108] (UnixAddr case)
impl PartialEq for ProxyAddr {
  fn eq(&self, other: &ProxyAddr) -> bool {
    match *self {
      ProxyAddr::Ipv4Addr{src_addr, dst_addr} => {
        match other {
          ProxyAddr::Ipv4Addr{src_addr: src_other, dst_addr: dst_other} => *src_other == src_addr && *dst_other == dst_addr,
          _ => false,
        }
      },
      ProxyAddr::Ipv6Addr{src_addr, dst_addr} => {
        match other {
          ProxyAddr::Ipv6Addr{src_addr: src_other, dst_addr: dst_other} => *src_other == src_addr && *dst_other == dst_addr,
          _ => false,
        }
      },
      ProxyAddr::UnixAddr{src_addr, dst_addr} => {
        match other {
          ProxyAddr::UnixAddr{src_addr: src_other, dst_addr: dst_other} => src_other[..] == src_addr[..] && dst_other[..] == dst_addr[..],
          _ => false,
        }
      },
      ProxyAddr::AfUnspec => {
        if let ProxyAddr::AfUnspec = other {
          return true
        }
        false
      },
    }
  }
}

fn get_family(addr: &ProxyAddr) -> u8 {
  match *addr {
    ProxyAddr::Ipv4Addr{ .. } => 0x10 | 0x01, // AF_INET  = 1 + STREAM = 1
    ProxyAddr::Ipv6Addr{ .. } => 0x20 | 0x01, // AF_INET6 = 2 + STREAM = 1
    ProxyAddr::UnixAddr{ .. } => 0x30 | 0x01, // AF_UNIX  = 3 + STREAM = 1
    ProxyAddr::AfUnspec => 0x00, // AF_UNSPEC + UNSPEC
  }
}

fn u16_to_array_of_u8(x: u16) -> [u8; 2] {
    let b1 : u8 = ((x >> 8) & 0xff) as u8;
    let b2 : u8 = (x & 0xff) as u8;
    [b1, b2]
}

#[cfg(test)]
mod test_v2 {

  use super::*;
  use std::net::{Ipv4Addr, Ipv6Addr, IpAddr};

  #[test]
  fn test_u16_to_array_of_u8() {
    let val_u16: u16 = 65534;
    let expected = [0xff, 0xfe];
    assert_eq!(expected, u16_to_array_of_u8(val_u16));
  }

  #[test]
  fn test_deserialize_tcp_ipv4_proxy_protocol_header() {
    let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080);
    let dst_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200);

    let header = HeaderV2::new(Command::Local, src_addr, dst_addr);
    let expected = &[
      0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // MAGIC header
      0x20,                                                                   // Version 2 and command LOCAL
      0x11,                                                                   // family AF_UNIX with IPv4
      0x00, 0x0C,                                                             // address sizes = 12
      0x7D, 0x19, 0x0A, 0x01,                                                 // source address
      0x0A, 0x04, 0x05, 0x08,                                                 // destination address
      0x1F, 0x90,                                                             // source port
      0x10, 0x68,                                                             // destination port
    ];

    assert_eq!(expected, &header.into_bytes()[..]);
  }

  #[test]
  fn test_deserialize_tcp_ipv6_proxy_protocol_header() {
    let src_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)), 8080);
    let dst_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)), 4200);

    let header = HeaderV2::new(Command::Proxy, src_addr, dst_addr);
    let expected = vec![
      0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,                         // MAGIC header
      0x21,                                                                                           // Version 2 and command PROXY
      0x21,                                                                                           // family AF_UNIX with IPv6
      0x00, 0x24,                                                                                     // address sizes = 36
      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // source address
      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // destination address
      0x1F, 0x90,                                                                                     // source port
      0x10, 0x68,                                                                                     // destination port
    ];

    assert_eq!(&expected[..], &header.into_bytes()[..]);
  }
}
