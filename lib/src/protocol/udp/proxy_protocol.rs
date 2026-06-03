//! Pure PROXY-protocol-v2 **DGRAM** header builder.
//!
//! No reference proxy ships PPv2-over-UDP, but the v2 spec carries a DGRAM
//! encoding: the version+command byte (offset 12) is `0x21` (v2 + PROXY) and
//! the family+transport byte (offset 13) is `0x12` for UDP-over-IPv4 or `0x22`
//! for UDP-over-IPv6 (the low nibble `0x2` = `DGRAM`, vs `0x1` = `STREAM` used
//! by the TCP serializer in `protocol/proxy_protocol/header.rs`).
//!
//! This builds the header for a flow's first upstream datagram with the **real
//! (pre-NAT) client address** as the PPv2 source. The destination is the
//! backend address. The header is a cheap prefix the core prepends in place to
//! the owned payload [`Vec<u8>`].
//!
//! Layout (v2):
//! ```text
//!  0..12  signature  0D 0A 0D 0A 00 0D 0A 51 55 49 54 0A
//!  12     version+command  0x21  (v2 | PROXY)
//!  13     family+transport 0x12 (AF_INET|DGRAM) / 0x22 (AF_INET6|DGRAM)
//!  14..16 address length (u16 BE): 12 (v4) / 36 (v6)
//!  16..   src ip, dst ip, src port, dst port  (BE)
//! ```

use std::net::SocketAddr;

/// v2 signature (12 bytes) shared with the TCP serializer.
const PP2_SIGNATURE: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// v2 + PROXY command (offset 12).
const PP2_VER_CMD_PROXY: u8 = 0x21;

/// AF_INET + DGRAM (offset 13, IPv4).
const PP2_FAM_INET_DGRAM: u8 = 0x12;

/// AF_INET6 + DGRAM (offset 13, IPv6).
const PP2_FAM_INET6_DGRAM: u8 = 0x22;

/// Build the PPv2 DGRAM header bytes for a datagram whose real client source is
/// `client` and whose backend destination is `backend`.
///
/// When the two addresses are mixed-family (one v4, one v6 — which the UDP
/// datapath never produces, since the connected upstream socket matches the
/// backend family) this falls back to an `AF_UNSPEC` / `UNSPEC` header with a
/// zero-length address block, exactly as the TCP serializer does. The backend
/// must then treat the connection as un-proxied, per the spec's UNSPEC rule.
pub fn dgram_header(client: SocketAddr, backend: SocketAddr) -> Vec<u8> {
    match (client, backend) {
        (SocketAddr::V4(src), SocketAddr::V4(dst)) => {
            let mut h = Vec::with_capacity(16 + 12);
            h.extend_from_slice(&PP2_SIGNATURE);
            h.push(PP2_VER_CMD_PROXY);
            h.push(PP2_FAM_INET_DGRAM);
            h.extend_from_slice(&12u16.to_be_bytes());
            h.extend_from_slice(&src.ip().octets());
            h.extend_from_slice(&dst.ip().octets());
            h.extend_from_slice(&src.port().to_be_bytes());
            h.extend_from_slice(&dst.port().to_be_bytes());
            h
        }
        (SocketAddr::V6(src), SocketAddr::V6(dst)) => {
            let mut h = Vec::with_capacity(16 + 36);
            h.extend_from_slice(&PP2_SIGNATURE);
            h.push(PP2_VER_CMD_PROXY);
            h.push(PP2_FAM_INET6_DGRAM);
            h.extend_from_slice(&36u16.to_be_bytes());
            h.extend_from_slice(&src.ip().octets());
            h.extend_from_slice(&dst.ip().octets());
            h.extend_from_slice(&src.port().to_be_bytes());
            h.extend_from_slice(&dst.port().to_be_bytes());
            h
        }
        _ => {
            // Mixed family: emit AF_UNSPEC with no address block.
            let mut h = Vec::with_capacity(16);
            h.extend_from_slice(&PP2_SIGNATURE);
            h.push(PP2_VER_CMD_PROXY);
            h.push(0x00); // AF_UNSPEC | UNSPEC
            h.extend_from_slice(&0u16.to_be_bytes());
            h
        }
    }
}

/// Prepend the PPv2 DGRAM header to an owned payload in place, returning the
/// header length so the shell can account for the prefix bytes in metrics/logs.
pub fn prepend_dgram_header(
    payload: &mut Vec<u8>,
    client: SocketAddr,
    backend: SocketAddr,
) -> usize {
    let header = dgram_header(client, backend);
    let header_len = header.len();
    // Single splice: reserve, shift the payload right, copy the header in front.
    let mut prefixed = Vec::with_capacity(header_len + payload.len());
    prefixed.extend_from_slice(&header);
    prefixed.append(payload);
    *payload = prefixed;
    header_len
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn dgram_header_ipv4_exact_bytes() {
        // client 125.25.10.1:8080  →  backend 10.4.5.8:4200
        let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080);
        let backend = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200);
        let header = dgram_header(client, backend);
        let expected: [u8; 28] = [
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // signature
            0x21, // v2 + PROXY
            0x12, // AF_INET | DGRAM
            0x00, 0x0C, // address length = 12
            0x7D, 0x19, 0x0A, 0x01, // source 125.25.10.1
            0x0A, 0x04, 0x05, 0x08, // dest 10.4.5.8
            0x1F, 0x90, // source port 8080
            0x10, 0x68, // dest port 4200
        ];
        assert_eq!(&expected[..], &header[..]);
        assert_eq!(header.len(), 28);
    }

    #[test]
    fn dgram_header_ipv6_exact_bytes() {
        // client ::1:8080  →  backend ::1:4200
        let client = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)), 8080);
        let backend = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)), 4200);
        let header = dgram_header(client, backend);
        let expected: [u8; 52] = [
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // signature
            0x21, // v2 + PROXY
            0x22, // AF_INET6 | DGRAM
            0x00, 0x24, // address length = 36
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, // source ::1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, // dest ::1
            0x1F, 0x90, // source port 8080
            0x10, 0x68, // dest port 4200
        ];
        assert_eq!(&expected[..], &header[..]);
        assert_eq!(header.len(), 52);
    }

    #[test]
    fn prepend_shifts_payload() {
        let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 1234);
        let backend = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)), 5678);
        let mut payload = b"DNSQUERY".to_vec();
        let n = prepend_dgram_header(&mut payload, client, backend);
        assert_eq!(n, 28);
        assert_eq!(&payload[..12], &PP2_SIGNATURE);
        assert_eq!(payload[12], PP2_VER_CMD_PROXY);
        assert_eq!(payload[13], PP2_FAM_INET_DGRAM);
        assert_eq!(&payload[28..], b"DNSQUERY");
    }

    #[test]
    fn mixed_family_emits_unspec() {
        let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 1);
        let backend = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 2);
        let header = dgram_header(client, backend);
        assert_eq!(header.len(), 16);
        assert_eq!(header[12], PP2_VER_CMD_PROXY);
        assert_eq!(header[13], 0x00); // AF_UNSPEC
        assert_eq!(&header[14..16], &[0x00, 0x00]); // zero address length
    }
}
