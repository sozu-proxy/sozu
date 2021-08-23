use nom::{
    bytes::streaming::{tag, take},
    error::{Error, ErrorKind, ParseError},
    number::streaming::{be_u16, be_u8},
    Err, IResult,
};

use std::convert::From;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

use crate::protocol::proxy_protocol::header::*;

const PROTOCOL_SIGNATURE_V2: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

fn parse_command(i: &[u8]) -> IResult<&[u8], Command> {
    let i2 = i;
    let (i, cmd) = be_u8(i)?;
    match cmd {
        0x20 => Ok((i, Command::Local)),
        0x21 => Ok((i, Command::Proxy)),
        _ => Err(Err::Error(Error::from_error_kind(i2, ErrorKind::Switch))),
    }
}

pub fn parse_v2_header(i: &[u8]) -> IResult<&[u8], HeaderV2> {
    let (i, _) = tag(&PROTOCOL_SIGNATURE_V2)(i)?;
    let (i, command) = parse_command(i)?;
    let (i, family) = be_u8(i)?;
    let (i, len) = be_u16(i)?;
    let (i, data) = take(len)(i)?;
    let (_, addr) = parse_addr_v2(family)(data)?;

    Ok((
        i,
        (HeaderV2 {
            command,
            family,
            addr,
        }),
    ))
}

fn parse_addr_v2(family: u8) -> impl Fn(&[u8]) -> IResult<&[u8], ProxyAddr> {
    move |i: &[u8]| match (family >> 4) & 0x0f {
        0x00 => Ok((i, ProxyAddr::AfUnspec)),
        0x01 => parse_ipv4_on_v2(i),
        0x02 => parse_ipv6_on_v2(i),
        _ => Err(Err::Error(Error::from_error_kind(i, ErrorKind::Switch))),
    }
}

fn parse_ipv4_on_v2(i: &[u8]) -> IResult<&[u8], ProxyAddr> {
    let (i, src_ip) = take(4u8)(i)?;
    let (i, dest_ip) = take(4u8)(i)?;
    let (i, src_port) = be_u16(i)?;
    let (i, dest_port) = be_u16(i)?;

    Ok((
        i,
        ProxyAddr::Ipv4Addr {
            src_addr: SocketAddrV4::new(
                Ipv4Addr::new(src_ip[0], src_ip[1], src_ip[2], src_ip[3]),
                src_port,
            ),
            dst_addr: SocketAddrV4::new(
                Ipv4Addr::new(dest_ip[0], dest_ip[1], dest_ip[2], dest_ip[3]),
                dest_port,
            ),
        },
    ))
}

fn parse_ipv6_on_v2(i: &[u8]) -> IResult<&[u8], ProxyAddr> {
    let (i, src_ip) = take(16u8)(i)?;
    let (i, dest_ip) = take(16u8)(i)?;
    let (i, src_port) = be_u16(i)?;
    let (i, dest_port) = be_u16(i)?;

    Ok((
        i,
        ProxyAddr::Ipv6Addr {
            src_addr: SocketAddrV6::new(slice_to_ipv6(&src_ip[..]), src_port, 0, 0),
            dst_addr: SocketAddrV6::new(slice_to_ipv6(&dest_ip[..]), dest_port, 0, 0),
        },
    ))
}

// assumes the slice has 16 bytes
pub fn slice_to_ipv6(sl: &[u8]) -> Ipv6Addr {
    let mut arr: [u8; 16] = [0; 16];
    arr.clone_from_slice(sl);
    Ipv6Addr::from(arr)
}

#[cfg(test)]
mod test {

    use super::*;
    use nom::Err;
    use nom::Needed;
    use std::net::{IpAddr, SocketAddr};

    #[test]
    fn test_parse_proxy_protocol_v2_local_ipv4_addr_header() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x20, // Version 2 and command LOCAL
            0x11, // family AF_UNIX with IPv4
            0x00, 0x0C, // address sizes = 12
            0x7D, 0x19, 0x0A, 0x01, // source address
            0x0A, 0x04, 0x05, 0x08, // destination address
            0x1F, 0x90, // source port
            0x10, 0x68, // destination port
        ];

        let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080);
        let dst_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200);
        let expected = HeaderV2::new(Command::Local, src_addr, dst_addr);

        assert_eq!(Ok((&[][..], expected)), parse_v2_header(input));
    }

    #[test]
    fn test_parse_proxy_protocol_v2_proxy_ipv4_addr_header() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x21, // Version 2 and command LOCAL
            0x11, // family AF_UNIX with IPv4
            0x00, 0x0C, // address sizes = 12
            0x7D, 0x19, 0x0A, 0x01, // source address
            0x0A, 0x04, 0x05, 0x08, // destination address
            0x1F, 0x90, // source port
            0x10, 0x68, // destination port
        ];

        let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080);
        let dst_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200);
        let expected = HeaderV2::new(Command::Proxy, src_addr, dst_addr);

        assert_eq!(Ok((&[][..], expected)), parse_v2_header(input));
    }

    #[test]
    fn it_should_parse_proxy_protocol_v2_ipv6_addr_header() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x20, // Version 2 and command LOCAL
            0x21, // family AF_UNIX with IPv6
            0x00, 0x24, // address sizes = 36
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, // source address
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x02, // destination address
            0x1F, 0x90, // source port
            0x10, 0x68, // destination port
        ];

        let src_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)), 8080);
        let dst_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 2)), 4200);
        let expected = HeaderV2::new(Command::Local, src_addr, dst_addr);

        assert_eq!(Ok((&[][..], expected)), parse_v2_header(input));
    }

    #[test]
    fn it_should_parse_proxy_protocol_v2_afunspec_header() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x20, // Version 2 and command LOCAL
            0x00, // family AF_UNSPEC and transport protocol unknow
            0x00, 0x00, // address sizes = 0
        ];

        let expected = HeaderV2 {
            command: Command::Local,
            family: 0,
            addr: ProxyAddr::AfUnspec,
        };

        assert_eq!(Ok((&[][..], expected)), parse_v2_header(input));
    }

    #[test]
    fn it_should_not_parse_proxy_protocol_v2_with_unknown_version() {
        let unknow_version = 0x30;

        let input = &[
            0x0D,
            0x0A,
            0x0D,
            0x0A,
            0x00,
            0x0D,
            0x0A,
            0x51,
            0x55,
            0x49,
            0x54,
            0x0A,           // MAGIC header
            unknow_version, // invalid version
        ];

        assert!(parse_v2_header(input).is_err());
    }

    #[test]
    fn it_should_not_parse_proxy_protocol_v2_with_unknown_command() {
        let unknow_command = 0x23;

        let input = &[
            0x0D,
            0x0A,
            0x0D,
            0x0A,
            0x00,
            0x0D,
            0x0A,
            0x51,
            0x55,
            0x49,
            0x54,
            0x0A,           // MAGIC header
            unknow_command, // Version 2 and invalid command
        ];

        assert!(parse_v2_header(input).is_err());
    }

    #[test]
    fn it_should_not_parse_proxy_protocol_with_unknown_family() {
        let unknow_family = 0x30;

        let input = &[
            0x0D,
            0x0A,
            0x0D,
            0x0A,
            0x00,
            0x0D,
            0x0A,
            0x51,
            0x55,
            0x49,
            0x54,
            0x0A,          // MAGIC header
            0x20,          // Version 2 and command LOCAL
            unknow_family, // family
            0x00,
            0x00, // address sizes = 0
        ];

        assert!(parse_v2_header(input).is_err());
    }

    #[test]
    fn it_should_not_parse_request_without_magic_header() {
        let input = &[
            0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D, 0x0D,
            0x0D, // INCORRECT MAGIC header
        ];

        assert!(parse_v2_header(input).is_err());
    }

    #[test]
    fn it_should_not_parse_proxy_protocol_v2_ipv4_addr_header_with_missing_data() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x20, // Version 2 and command LOCAL
            0x11, // family AF_UNIX with IPv4
        ];

        assert_eq!(Err(Err::Incomplete(Needed::new(2))), parse_v2_header(input));
    }

    #[test]
    fn it_should_not_parse_proxy_protocol_v2_with_invalid_length() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x20, // Version 2 and command LOCAL
            0x21, // family AF_UNIX with IPv6
            0x00, 0x10, // address sizes = 36
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, // source address
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x02, // destination address
            0x1F, 0x90, // source port
            0x10, 0x68, // destination port
        ];

        assert_eq!(
            Err(Err::Incomplete(Needed::new(16))),
            parse_v2_header(input)
        );
    }
}
