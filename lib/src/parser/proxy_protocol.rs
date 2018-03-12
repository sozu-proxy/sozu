use nom::IResult;
use nom::IResult::*;
use nom::{be_u8, be_u16, ErrorKind};

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::convert::AsMut;
use std::convert::From;

use network::protocol::proxy_protocol::header::*;

pub const UNKNOWN_FAMILY: u32 = 1;
pub const INCORRECT_VERSION: u32 = 2;

const PROTOCOL_SIGNATURE_V2: [u8; 12] = [0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A];

//NOTE: Only support the V2 for the moment
pub fn parse(buf: &[u8]) -> IResult<&[u8], ProxyProtocolHeader> {
  match tag!(buf, &PROTOCOL_SIGNATURE_V2) {
    Done(rest, _) => {
      match parse_v2_header(rest) {
        Done(rest, header) => Done(rest, ProxyProtocolHeader::V2(header)),
        Incomplete(i) => Incomplete(i),
        Error(e) => Error(e),
      }
    },
    Incomplete(i) => Incomplete(i),
    Error(e) => Error(e),
  }
}

//TODO: Refactor this method using cond! parser if it's possible
fn read_and_verify_ver_and_command(input: &[u8]) -> IResult<&[u8], u8> {
  match be_u8(input) {
    Done(rest, ver_and_cmd) => {
      // Verify if it's the version 2
      if (ver_and_cmd >> 4) & 0x0f == 0x02 {
        return Done(rest, ver_and_cmd)
      }
      Error(error_code!(ErrorKind::Custom(INCORRECT_VERSION)))
    },
    Error(e)      => Error(e),
    Incomplete(e) => Incomplete(e)
  }
}

fn parse_v2_header(input: &[u8]) -> IResult<&[u8], HeaderV2> {
  do_parse!(
    input,
    ver_and_cmd: read_and_verify_ver_and_command >>
    family: be_u8 >>
    len: be_u16 >>
    addr: apply!(parse_addr_v2, family) >>
    (
      HeaderV2 {
        signature: PROTOCOL_SIGNATURE_V2,
        ver_and_cmd,
        family,
        len,
        addr
      }
    )
  )
}

fn parse_addr_v2(input: &[u8], family: u8) -> IResult<&[u8], ProxyAddr> {
  // family: 4 bits for address family and 4 bits for transport protocol
  match (family >> 4) & 0x0f {
    0x00 => Done(input, ProxyAddr::AfUnspec),
    0x01 => parse_ipv4_on_v2(input),
    0x02 => parse_ipv6_on_v2(input),
    _ => Error(error_code!(ErrorKind::Custom(UNKNOWN_FAMILY))),
  }
}

//TODO: Maybe we can refactor: e.g. count_fixed!(u8, take!(2), 3)
named!(take_4_bytes, take!(4));

fn parse_ipv4_on_v2(input: &[u8]) -> IResult<&[u8], ProxyAddr> {
  do_parse!(
    input,
    src_ip: take_4_bytes >>
    dest_ip: take_4_bytes >>
    src_port: be_u16 >>
    dest_port: be_u16 >>
    (
      ProxyAddr::Ipv4Addr {
        src_addr: SocketAddrV4::new(Ipv4Addr::new(src_ip[0], src_ip[1], src_ip[2], src_ip[3]), src_port),
        dst_addr: SocketAddrV4::new(Ipv4Addr::new(dest_ip[0], dest_ip[1], dest_ip[2], dest_ip[3]), dest_port)
      }
    )
  )
}

named!(take_16_bytes, take!(16));

// Transform a slice to an array
fn clone_into_array<A, T>(slice: &[T]) -> A where A: Sized + Default + AsMut<[T]>, T: Clone {
  let mut a = Default::default();
  <A as AsMut<[T]>>::as_mut(&mut a).clone_from_slice(slice);
  a
}

fn parse_ipv6_on_v2(input: &[u8]) -> IResult<&[u8], ProxyAddr> {
  do_parse!(
    input,
    src_ip: take_16_bytes >>
    dest_ip: take_16_bytes >>
    src_port: be_u16 >>
    dest_port: be_u16 >>
    (
      ProxyAddr::Ipv6Addr {
        src_addr: SocketAddrV6::new(Ipv6Addr::from(clone_into_array::<[u8; 16], u8>(&src_ip[..])), src_port, 0 ,0),
        dst_addr: SocketAddrV6::new(Ipv6Addr::from(clone_into_array::<[u8; 16], u8>(&dest_ip[..])), dest_port, 0 ,0),
      }
    )
  )
}

#[cfg(test)]
mod test {

  use super::*;
  use std::net::{IpAddr, SocketAddr};
  use nom::Needed::Size;

  #[test]
  fn test_parse_proxy_protocol_v2_ipv4_addr_header() {
    let input = &[
      0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // MAGIC header
      0x20,                                                                   // Version 2 and command LOCAL
      0x11,                                                                   // family AF_UNIX with IPv4
      0x00, 0x0C,                                                             // address sizes = 12
      0x7D, 0x19, 0x0A, 0x01,                                                 // source address
      0x0A, 0x04, 0x05, 0x08,                                                 // destination address
      0x1F, 0x90,                                                             // source port
      0x10, 0x68,                                                             // destination port
    ];

    let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080);
    let dst_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200);
    let expected = ProxyProtocolHeader::V2(HeaderV2::new(Some((src_addr, dst_addr))));

    assert_eq!(Done(&[][..], expected), parse(input));
  }

  #[test]
  fn it_should_parse_proxy_protocol_v2_ipv6_addr_header() {
    let input = &[
      0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,                         // MAGIC header
      0x20,                                                                                           // Version 2 and command LOCAL
      0x21,                                                                                           // family AF_UNIX with IPv6
      0x00, 0x24,                                                                                     // address sizes = 36
      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // source address
      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, // destination address
      0x1F, 0x90,                                                                                     // source port
      0x10, 0x68,                                                                                     // destination port
    ];

    let src_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)), 8080);
    let dst_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 2)), 4200);
    let expected = ProxyProtocolHeader::V2(HeaderV2::new(Some((src_addr, dst_addr))));

    assert_eq!(Done(&[][..], expected), parse(input));
  }

  #[test]
  fn it_should_parse_proxy_protocol_v2_afunspec_header() {
    let input = &[
      0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // MAGIC header
      0x20,                                                                   // Version 2 and command LOCAL
      0x00,                                                                   // family AF_UNSPEC and transport protocol unknow
      0x00, 0x00,                                                             // address sizes = 0
    ];

    let expected = ProxyProtocolHeader::V2(HeaderV2::new(None));

    assert_eq!(Done(&[][..], expected), parse(input));
  }

  #[test]
  fn it_should_not_parse_proxy_protocol_v2_with_unknown_version() {
    let unknow_version = 0x30;

    let input = &[
      0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // MAGIC header
      unknow_version,                                                         // Version 2 and command LOCAL
    ];

    assert_eq!(Error(ErrorKind::Custom(INCORRECT_VERSION)), parse(input));
  }

  #[test]
  fn it_should_not_parse_proxy_protocol_with_unknown_family() {
    let unknow_family = 0x30;

    let input = &[
      0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // MAGIC header
      0x20,                                                                   // Version 2 and command LOCAL
      unknow_family,                                                          // family
      0x00, 0x00,                                                             // address sizes = 0
    ];

    assert_eq!(Error(ErrorKind::Custom(UNKNOWN_FAMILY)), parse(input));
  }

  #[test]
  fn it_should_not_parse_request_without_magic_header() {

    let input = &[
      0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, // INCORRECT MAGIC header
    ];

    assert_eq!(Error(ErrorKind::Tag), parse(input));
  }

  #[test]
  fn it_should_not_parse_proxy_protocol_v2_ipv4_addr_header_with_missing_data() {
    let input = &[
      0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, // MAGIC header
      0x20,                                                                   // Version 2 and command LOCAL
      0x11,                                                                   // family AF_UNIX with IPv4
    ];

    assert_eq!(Incomplete(Size(4)), parse(input));
  }
}