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
    let (i, _) = tag(&PROTOCOL_SIGNATURE_V2[..])(i)?;
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
    let (data_rest, parsed_addr) = parse_addr_v2(family)(data)?;
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

    // HAProxy PROXY protocol specification, §2.2: `LOCAL` describes a
    // connection the upstream proxy originated on its own behalf (typically a
    // health check), and the receiver must use the real socket endpoints and
    // ignore the header's address block. Nothing on the wire ties `LOCAL` to
    // `AF_UNSPEC` -- only HAProxy's own emitter happens to pair them -- so a
    // crafted peer can send `LOCAL` with a fully populated `AF_INET` /
    // `AF_INET6` block. `parse_addr_v2` switches on the family nibble alone,
    // so that block used to reach every consumer of `HeaderV2::addr`. There are
    // six of them, not three. Four are TCP-side and read this `ProxyAddr`
    // directly: `expect.rs`, `relay.rs`, `tcp_preread/mod.rs`, and
    // `TcpSession::effective_session_address` (`lib/src/tcp.rs`), which reads
    // `ExpectProxyProtocol::addresses` / `RelayProxyProtocol::addresses` itself
    // rather than through `into_pipe` and feeds the raw-TCP
    // `max_connections_per_ip` gate in `TcpSession::connect_to_backend`
    // (`lib/src/tcp.rs`). The other two are the HTTP and HTTPS expect upgrades,
    // which read the same value back out of `ExpectProxyProtocol::addresses` in
    // `HttpSession::upgrade_expect` (`lib/src/http.rs`) and
    // `HttpsSession::upgrade` (`lib/src/https.rs`). All six attribute
    // `ProxyAddr::source()` to the client: any peer could therefore forge the
    // source address Sōzu records in its access logs, injects as `X-Real-IP`,
    // and counts against `max_connections_per_ip`.
    //
    // Discarding it here rather than at each of those six keeps one decision
    // in one place: none of them reads `command` or `family`, none
    // re-serializes the parsed header (`relay.rs` forwards the buffered bytes
    // verbatim), and all six obtain the address exclusively through this
    // `ProxyAddr`.
    //
    // `AfUnspec` is a variant all six already handle, but they do NOT handle
    // it alike, and that split is the operator-visible behaviour:
    //
    // * the four TCP-side consumers fall back to the front socket's
    //   `peer_addr`, so the session proceeds, attributed to the real peer;
    // * the HTTP and HTTPS expect upgrades require BOTH endpoints and get
    //   neither -- `AfUnspec` returns `None` from `ProxyAddr::source()` and
    //   `ProxyAddr::destination()` alike
    //   (`lib/src/protocol/proxy_protocol/header.rs`) -- so `upgrade_expect`
    //   returns `None` and `HttpSession::upgrade` reports `SessionIsToBeClosed`
    //   (`lib/src/http.rs`). Such a session is closed at the expect stage, not
    //   re-attributed to `peer_addr`.
    //
    // That close is not a regression for legitimate traffic: HAProxy pairs
    // `LOCAL` with `AF_UNSPEC`, which already parsed to `AfUnspec`, so those
    // sessions already closed. Only the forged populated block changes, from
    // "upgrade with attacker-chosen addresses" to "close".
    //
    // The block is still parsed above, not skipped: a `LOCAL` header with a
    // malformed or unknown family must keep being rejected, and its declared
    // length must keep delimiting the header, so framing is unchanged.
    // Matched exhaustively on purpose: a future command must make an explicit
    // attribution decision rather than inherit `PROXY`'s by default.
    let addr = match command {
        Command::Local => ProxyAddr::AfUnspec,
        Command::Proxy => parsed_addr,
    };

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

    /// A `LOCAL` header describes a connection the upstream proxy originated
    /// itself (typically a health check); the HAProxy PROXY protocol
    /// specification §2.2 requires the receiver to discard its address block.
    /// Nothing on the wire forces a `LOCAL` header to carry `AF_UNSPEC` --
    /// only HAProxy's own emitter happens to pair the two -- so a crafted peer
    /// may send `LOCAL` with a fully populated `AF_INET` block. Before the fix
    /// `parse_addr_v2` switched on the family nibble alone and handed that
    /// block back as a real address pair, letting any peer forge the source
    /// address Sōzu attributes to the client (access logs, `X-Real-IP`,
    /// `max_connections_per_ip`).
    ///
    /// The block is still parsed -- a malformed one must still be rejected,
    /// see `it_should_not_parse_proxy_protocol_with_unknown_family` -- but its
    /// contents are dropped.
    ///
    /// To SEE THIS RED: in [`super::parse_v2_header`], replace the
    /// `Command::Local => ProxyAddr::AfUnspec` arm of the `addr` binding with
    /// `parsed_addr` (the pre-fix behaviour) -- the parser then returns the
    /// forged `125.25.10.1:8080` instead of `AfUnspec`.
    #[test]
    fn local_command_discards_a_populated_ipv4_address_block() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x20, // Version 2 and command LOCAL
            0x11, // family AF_INET over STREAM
            0x00, 0x0C, // address sizes = 12
            0x7D, 0x19, 0x0A, 0x01, // forged source address
            0x0A, 0x04, 0x05, 0x08, // forged destination address
            0x1F, 0x90, // forged source port
            0x10, 0x68, // forged destination port
        ];

        // The family byte is reported as it was read off the wire; only the
        // address block it describes is discarded.
        let expected = HeaderV2 {
            command: Command::Local,
            family: 0x11,
            addr: ProxyAddr::AfUnspec,
        };

        assert_eq!(Ok((&[][..], expected)), parse_v2_header(input));

        let (_, header) = parse_v2_header(input).expect("the header itself stays well-formed");
        assert_eq!(
            header.addr.source(),
            None,
            "a LOCAL header must attribute no source address, whatever its address block holds"
        );
        assert_eq!(
            header.addr.destination(),
            None,
            "a LOCAL header must attribute no destination address either"
        );
    }

    /// Same crafted block behind the `PROXY` command: the fix must not
    /// silently disable PROXY-protocol address attribution.
    ///
    /// To SEE THIS RED: make the `addr` binding in [`super::parse_v2_header`]
    /// unconditionally `ProxyAddr::AfUnspec`.
    #[test]
    fn proxy_command_still_honours_the_same_address_block() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x21, // Version 2 and command PROXY
            0x11, // family AF_INET over STREAM
            0x00, 0x0C, // address sizes = 12
            0x7D, 0x19, 0x0A, 0x01, // source address
            0x0A, 0x04, 0x05, 0x08, // destination address
            0x1F, 0x90, // source port
            0x10, 0x68, // destination port
        ];

        let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080);
        let (_, header) = parse_v2_header(input).expect("a PROXY header must still parse");
        assert_eq!(
            header.addr.source(),
            Some(src_addr),
            "a PROXY header still carries the encapsulated client address"
        );
    }

    /// The IPv6 half of `local_command_discards_a_populated_ipv4_address_block`.
    ///
    /// To SEE THIS RED: same mutation -- restore `parsed_addr` unconditionally
    /// in [`super::parse_v2_header`].
    #[test]
    fn local_command_discards_a_populated_ipv6_address_block() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x20, // Version 2 and command LOCAL
            0x21, // family AF_INET6 over STREAM
            0x00, 0x24, // address sizes = 36
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, // forged source address
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x02, // forged destination address
            0x1F, 0x90, // forged source port
            0x10, 0x68, // forged destination port
        ];

        let expected = HeaderV2 {
            command: Command::Local,
            family: 0x21,
            addr: ProxyAddr::AfUnspec,
        };

        assert_eq!(Ok((&[][..], expected)), parse_v2_header(input));
    }

    #[test]
    fn test_parse_proxy_protocol_v2_proxy_ipv4_addr_header() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x21, // Version 2 and command PROXY
            0x11, // family AF_INET over STREAM
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
            0x21, // Version 2 and command PROXY
            0x21, // family AF_INET6 over STREAM
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
        // Driven with the PROXY command: a LOCAL header no longer yields an
        // address pair at all (see
        // `local_command_discards_a_populated_ipv6_address_block`), so this
        // test would otherwise stop covering IPv6 address-block decoding.
        let expected = HeaderV2::new(Command::Proxy, src_addr, dst_addr);

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
            0x11, // family AF_INET over STREAM
        ];

        assert_eq!(Err(Err::Incomplete(Needed::new(2))), parse_v2_header(input));
    }

    #[test]
    fn it_should_not_parse_proxy_protocol_v2_with_invalid_length() {
        let input = &[
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // MAGIC header
            0x20, // Version 2 and command LOCAL
            0x21, // family AF_INET6 over STREAM
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
