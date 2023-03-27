use std::{
    net::{IpAddr, SocketAddr},
    str::from_utf8_unchecked,
};

use rusty_ulid::Ulid;

use crate::{
    pool::Checkout,
    protocol::http::{parser::compare_no_case, SozuHtx},
    Protocol,
};

pub struct RequestContext {
    pub closing: bool,
    pub id: Ulid,
    pub protocol: Protocol,
    pub public_address: SocketAddr,
    pub session_address: Option<SocketAddr>,
}

pub struct ResponseContext {
    pub closing: bool,
    pub id: Ulid,
}

impl htx::h1::ParserCallbacks<Checkout> for RequestContext {
    fn on_headers(&mut self, request: &mut SozuHtx) {
        println!("REQUEST CALLBACK!!!!!!!!!!!!!!!!!!!!");
        let buf = &mut request.storage.mut_buffer();

        let public_ip = self.public_address.ip();
        let public_port = self.public_address.port();
        let proto = match self.protocol {
            Protocol::HTTP => "http",
            Protocol::HTTPS => "https",
            _ => unreachable!(),
        };

        let mut x_for = None;
        let mut forwarded = None;
        for block in &mut request.blocks {
            match block {
                htx::HtxBlock::Header(header) if !header.is_elided() => {
                    let key = header.key.data(buf);
                    if compare_no_case(key, b"connection") {
                        if self.closing {
                            header.val = htx::Store::Static(b"close");
                        }
                    } else if compare_no_case(key, b"X-Forwarded-Proto") {
                        header.val = htx::Store::Static(proto.as_bytes());
                    } else if compare_no_case(key, b"X-Forwarded-Port") {
                        header.val = htx::Store::from_string(public_port.to_string());
                    } else if compare_no_case(key, b"X-Forwarded-For") {
                        x_for = Some(header);
                    } else if compare_no_case(key, b"Forwarded") {
                        forwarded = Some(header);
                    }
                }
                _ => {}
            }
        }
        if let Some(peer_addr) = self.session_address {
            let peer_ip = peer_addr.ip();
            let peer_port = peer_addr.port();
            let has_x_for = x_for.is_some();
            let has_forwarded = forwarded.is_some();

            if let Some(header) = x_for {
                header.val = htx::Store::from_string(format!(
                    "{}, {}",
                    unsafe { from_utf8_unchecked(header.val.data(buf)) },
                    peer_ip.to_string()
                ));
            }
            if let Some(header) = &mut forwarded {
                let value = unsafe { from_utf8_unchecked(header.val.data(buf)) };
                let new_value = match (peer_ip, public_ip) {
                    (IpAddr::V4(_), IpAddr::V4(_)) => {
                        format!("{value}, proto={proto};for={peer_ip}:{peer_port};by={public_ip}")
                    }
                    (IpAddr::V4(_), IpAddr::V6(_)) => {
                        format!(
                            "{value}, proto={proto};for={peer_ip}:{peer_port};by=\"{public_ip}\""
                        )
                    }
                    (IpAddr::V6(_), IpAddr::V4(_)) => {
                        format!(
                            "{value}, proto={proto};for=\"{peer_ip}:{peer_port}\";by={public_ip}"
                        )
                    }
                    (IpAddr::V6(_), IpAddr::V6(_)) => {
                        format!(
                        "{value}, proto={proto};for=\"{peer_ip}:{peer_port}\";by=\"{public_ip}\""
                    )
                    }
                };
                header.val = htx::Store::from_string(new_value);
            }

            if !has_x_for {
                request.push_block(htx::HtxBlock::Header(htx::Header {
                    key: htx::Store::Static(b"X-Forwarded-For"),
                    val: htx::Store::from_string(peer_ip.to_string()),
                }));
            }
            if !has_forwarded {
                let value = match (peer_ip, public_ip) {
                    (IpAddr::V4(_), IpAddr::V4(_)) => {
                        format!("proto={proto};for={peer_ip}:{peer_port};by={public_ip}")
                    }
                    (IpAddr::V4(_), IpAddr::V6(_)) => {
                        format!("proto={proto};for={peer_ip}:{peer_port};by=\"{public_ip}")
                    }
                    (IpAddr::V6(_), IpAddr::V4(_)) => {
                        format!("proto={proto};for=\"{peer_ip}:{peer_port}\";by={public_ip}")
                    }
                    (IpAddr::V6(_), IpAddr::V6(_)) => {
                        format!("proto={proto};for=\"{peer_ip}:{peer_port}\";by=\"{public_ip}\"")
                    }
                };
                request.push_block(htx::HtxBlock::Header(htx::Header {
                    key: htx::Store::Static(b"Forwarded"),
                    val: htx::Store::from_string(value),
                }));
            }
        }

        request.push_block(htx::HtxBlock::Header(htx::Header {
            key: htx::Store::Static(b"Sozu-Id"),
            val: htx::Store::from_string(self.id.to_string()),
        }));
    }
}

impl htx::h1::ParserCallbacks<Checkout> for ResponseContext {
    fn on_headers(&mut self, response: &mut SozuHtx) {
        println!("RESPONSE CALLBACK!!!!!!!!!!!!!!!!!!!!");
        let buf = &mut response.storage.mut_buffer();

        if self.closing {
            for block in &mut response.blocks {
                match block {
                    htx::HtxBlock::Header(header) if !header.is_elided() => {
                        let key = header.key.data(buf);
                        if compare_no_case(key, b"connection") {
                            header.val = htx::Store::Static(b"close");
                        }
                    }
                    _ => {}
                }
            }
        }

        response.push_block(htx::HtxBlock::Header(htx::Header {
            key: htx::Store::Static(b"Sozu-Id"),
            val: htx::Store::from_string(self.id.to_string()),
        }));
    }
}
