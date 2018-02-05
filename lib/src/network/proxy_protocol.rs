use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::fmt;


pub enum ProxyProtocolHeader {
    V1(HeaderV1),
    V2(HeaderV2),
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
#[derive(Debug)]
pub struct HeaderV1 {
    protocol: ProtocolSupportedV1,
    addr_src: Option<SocketAddr>,
    addr_dst: Option<SocketAddr>,
}

const PROXY_PROTO_IDENTIFIER: &'static str = "PROXY";

impl HeaderV1 {

    pub fn new (protocol: ProtocolSupportedV1, addr_src: Option<SocketAddr>, addr_dst: Option<SocketAddr>) -> Self {
        HeaderV1 {
            protocol,
            addr_src,
            addr_dst,
        }
    }

    // Use this method to writte the header in the backend socket
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
                self.addr_src.unwrap().ip(),
                self.addr_dst.unwrap().ip(),
                self.addr_src.unwrap().port(),
                self.addr_dst.unwrap().port(),
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
        let addr_src = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 80));
        let addr_dst = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 80));
        let header = HeaderV1::new(ProtocolSupportedV1::TCP4, addr_src, addr_dst);

        let header_to_cmp = "PROXY TCP4 127.0.0.1 127.0.0.1 80 80\r\n".as_bytes();

        assert_eq!(header_to_cmp, &header.into_bytes()[..]);
    }

    #[test]
    fn it_should_return_a_correct_header_with_ipv6() {
        let addr_src = Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0xffff)), 80));
        let addr_dst = Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0xffff)), 80));
        let header = HeaderV1::new(ProtocolSupportedV1::TCP6, addr_src, addr_dst);

        let header_to_cmp = "PROXY TCP6 ::0.0.255.255 ::0.0.255.255 80 80\r\n".as_bytes();

        assert_eq!(header_to_cmp, &header.into_bytes()[..]);
    }

    #[test]
    fn it_should_return_the_an_unknown_header() {
        let header = HeaderV1::new(ProtocolSupportedV1::UNKNOWN, None, None);

        let header_to_cmp = "PROXY UNKNOWN\r\n".as_bytes();

        assert_eq!(header_to_cmp, &header.into_bytes()[..]);
    }
}


pub struct HeaderV2 {
    signature: [u8; 12], // hex 0D 0A 0D 0A 00 0D 0A 51 55 49 54 0A
    ver_and_cmd: u8,     // protocol version and command
    family: u8,          // protocol family and address
    len: u16,            // number of following bytes part of the header
    addr: AddrV2,
}

impl HeaderV2 {
    fn new(ver_and_cmd: u8, family: u8, addr: AddrV2) -> Self {
        HeaderV2 { 
            signature: [0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A],
            ver_and_cmd,
            family,
            len: 0,
            addr,
        }
   }
}

pub enum AddrV2 {
    // len = 12
    TCP_UDP_IPV4 {
        addr_src: SocketAddrV4,
        addr_dst: SocketAddrV4,
    },
    // len = 36
    TCP_UDP_IPV6 {
        addr_src: SocketAddrV6,
        addr_dst: SocketAddrV6,
    },
    // len = 216
    AF_UNIX {
        src_addr: [u8; 108],
        dest_addr: [u8; 108],
    },
    AF_UNSPEC
}
