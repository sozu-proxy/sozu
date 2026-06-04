//! PROXY-v2 wire parser.
//!
//! Decodes the 12-byte PROXY-v2 signature plus the variable address block
//! defined by the HAProxy PROXY protocol specification. Returns a
//! `HeaderV2` to the caller (`expect.rs` / `relay.rs`); rejects malformed
//! framing through `nom::Err` so the session can close cleanly. No I/O.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

use nom::{
    Err, IResult,
    bytes::streaming::{tag, take},
    error::{Error, ErrorKind, ParseError},
    number::streaming::{be_u8, be_u16},
};

use crate::protocol::proxy_protocol::header::{Command, HeaderV2, ProxyAddr};

const PROTOCOL_SIGNATURE_V2: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

fn parse_command(i: &[u8]) -> IResult<&[u8], Command> {
    let i2 = i;
    let (i, cmd) = be_u8(i)?;
    // `be_u8` consumes exactly one byte on success; the remainder must be one
    // shorter than the input we were given.
    debug_assert_eq!(
        i.len() + 1,
        i2.len(),
        "command byte parse consumes exactly one byte"
    );
    match cmd {
        0x20 => Ok((i, Command::Local)),
        0x21 => Ok((i, Command::Proxy)),
        _ => Err(Err::Error(Error::from_error_kind(i2, ErrorKind::Switch))),
    }
}

pub fn parse_v2_header(i: &[u8]) -> IResult<&[u8], HeaderV2> {
    let input_len = i.len();
    let (i, _) = tag(&PROTOCOL_SIGNATURE_V2)(i)?;
    let (i, command) = parse_command(i)?;
    let (i, family) = be_u8(i)?;
    let (i, len) = be_u16(i)?;
    let (i, data) = take(len)(i)?;
    // The length field gates exactly `len` address bytes; `take` already
    // enforces availability, so `data` is precisely that many bytes.
    debug_assert_eq!(
        data.len(),
        len as usize,
        "address block must be exactly the declared length"
    );
    let (data_rest, addr) = parse_addr_v2(family)(data)?;
    // The address parser may not consume every advertised byte (TLVs / over-
    // long length fields are tolerated by leaving a tail), but it must never
    // read past the block it was handed.
    debug_assert!(
        data_rest.len() <= data.len(),
        "address parser cannot grow its input"
    );

    // Postcondition: a parser never grows its input — the unconsumed remainder
    // is no longer than the original, and the consumed prefix (the full v2
    // header) is the 16-byte fixed part plus the declared address length.
    debug_assert!(i.len() <= input_len, "parser cannot grow its input");
    debug_assert_eq!(
        input_len - i.len(),
        PROTOCOL_SIGNATURE_V2.len() + 1 + 1 + 2 + len as usize,
        "consumed header length must reconcile with the signature, fixed fields, and declared address length"
    );
    // The family byte is in the enumerated set the address parser accepts: a
    // rejected family would have short-circuited via `?` above, so on this
    // success path the high nibble is AF_UNSPEC/INET/INET6.
    debug_assert!(
        matches!((family >> 4) & 0x0f, 0x00..=0x02),
        "accepted family nibble must be AF_UNSPEC, AF_INET, or AF_INET6"
    );

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
    let in_len = i.len();
    let (i, src_ip) = take(4u8)(i)?;
    let (i, dest_ip) = take(4u8)(i)?;
    let (i, src_port) = be_u16(i)?;
    let (i, dest_port) = be_u16(i)?;
    // An IPv4 v2 address block is fixed at 4 + 4 + 2 + 2 = 12 bytes; the parser
    // must have consumed exactly that on success and never grown its input.
    debug_assert_eq!(src_ip.len(), 4, "IPv4 source address is 4 bytes");
    debug_assert_eq!(dest_ip.len(), 4, "IPv4 destination address is 4 bytes");
    debug_assert!(i.len() <= in_len, "parser cannot grow its input");
    debug_assert_eq!(
        in_len - i.len(),
        12,
        "IPv4 v2 address block is exactly 12 bytes"
    );

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
    let in_len = i.len();
    let (i, src_ip) = take(16u8)(i)?;
    let (i, dest_ip) = take(16u8)(i)?;
    let (i, src_port) = be_u16(i)?;
    let (i, dest_port) = be_u16(i)?;
    // An IPv6 v2 address block is fixed at 16 + 16 + 2 + 2 = 36 bytes; `take`
    // guarantees the slice widths fed to `slice_to_ipv6`, so its 16-byte
    // precondition holds on this path.
    debug_assert_eq!(src_ip.len(), 16, "IPv6 source address is 16 bytes");
    debug_assert_eq!(dest_ip.len(), 16, "IPv6 destination address is 16 bytes");
    debug_assert!(i.len() <= in_len, "parser cannot grow its input");
    debug_assert_eq!(
        in_len - i.len(),
        36,
        "IPv6 v2 address block is exactly 36 bytes"
    );

    Ok((
        i,
        ProxyAddr::Ipv6Addr {
            src_addr: SocketAddrV6::new(slice_to_ipv6(src_ip), src_port, 0, 0),
            dst_addr: SocketAddrV6::new(slice_to_ipv6(dest_ip), dest_port, 0, 0),
        },
    ))
}

// assumes the slice has 16 bytes
pub fn slice_to_ipv6(sl: &[u8]) -> Ipv6Addr {
    // Precondition (internal invariant): every caller hands a slice sized by a
    // `take(16)`, so a wrong length here is a logic bug, not attacker input.
    // `clone_from_slice` below would itself panic on a mismatch; the
    // debug_assert names the contract loudly in debug/test/fuzz builds.
    debug_assert_eq!(sl.len(), 16, "slice_to_ipv6 requires exactly 16 bytes");
    let mut arr: [u8; 16] = [0; 16];
    arr.clone_from_slice(sl);
    Ipv6Addr::from(arr)
}

#[cfg(test)]
mod test {

    use std::net::{IpAddr, SocketAddr};

    use nom::{Err, Needed};

    use super::*;

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
            0x00, // family AF_UNSPEC and transport protocol unknown
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
