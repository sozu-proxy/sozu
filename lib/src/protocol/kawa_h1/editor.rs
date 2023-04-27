use std::{
    net::{IpAddr, SocketAddr},
    str::{from_utf8, from_utf8_unchecked},
};

use rusty_ulid::Ulid;

use crate::{
    pool::Checkout,
    protocol::http::{parser::compare_no_case, GenericHttpStream},
    Protocol,
};

#[derive(Debug)]
pub struct HttpContext {
    pub closing: bool,
    pub id: Ulid,
    pub keep_alive_backend: bool,
    pub keep_alive_frontend: bool,
    pub protocol: Protocol,
    pub public_address: SocketAddr,
    pub session_address: Option<SocketAddr>,
    pub sticky_name: String,
    pub sticky_session: Option<String>,
    pub sticky_session_found: Option<String>,
}

impl kawa::h1::ParserCallbacks<Checkout> for HttpContext {
    fn on_headers(&mut self, stream: &mut GenericHttpStream) {
        match stream.kind {
            kawa::Kind::Request => self.on_request_headers(stream),
            kawa::Kind::Response => self.on_response_headers(stream),
        }
    }
}

impl HttpContext {
    fn on_request_headers(&mut self, request: &mut GenericHttpStream) {
        println!("REQUEST CALLBACK!!!!!!!!!!!!!!!!!!!!");
        println!("{self:?}");
        let buf = &mut request.storage.mut_buffer();

        let public_ip = self.public_address.ip();
        let public_port = self.public_address.port();
        let proto = match self.protocol {
            Protocol::HTTP => "http",
            Protocol::HTTPS => "https",
            _ => unreachable!(),
        };

        for cookie in &mut request.detached.jar {
            let key = cookie.key.data(buf);
            if key == self.sticky_name.as_bytes() {
                let val = cookie.val.data(buf);
                self.sticky_session_found = from_utf8(val).ok().map(|val| val.to_string());
                cookie.elide();
            }
        }

        let mut x_for = None;
        let mut forwarded = None;
        for block in &mut request.blocks {
            match block {
                kawa::Block::Header(header) if !header.is_elided() => {
                    let key = header.key.data(buf);
                    if compare_no_case(key, b"connection") {
                        if self.closing {
                            header.val = kawa::Store::Static(b"close");
                        } else {
                            let val = header.val.data(buf);
                            self.keep_alive_frontend &= !compare_no_case(val, b"close");
                        }
                    } else if compare_no_case(key, b"X-Forwarded-Proto") {
                        header.val = kawa::Store::Static(proto.as_bytes());
                    } else if compare_no_case(key, b"X-Forwarded-Port") {
                        header.val = kawa::Store::from_string(public_port.to_string());
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
                header.val = kawa::Store::from_string(format!(
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
                header.val = kawa::Store::from_string(new_value);
            }

            if !has_x_for {
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"X-Forwarded-For"),
                    val: kawa::Store::from_string(peer_ip.to_string()),
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
                request.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"Forwarded"),
                    val: kawa::Store::from_string(value),
                }));
            }
        }

        request.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Sozu-Id"),
            val: kawa::Store::from_string(self.id.to_string()),
        }));
    }

    fn on_response_headers(&mut self, response: &mut GenericHttpStream) {
        println!("RESPONSE CALLBACK!!!!!!!!!!!!!!!!!!!!");
        println!("{self:?}");
        let buf = &mut response.storage.mut_buffer();

        for block in &mut response.blocks {
            match block {
                kawa::Block::Header(header) if !header.is_elided() => {
                    let key = header.key.data(buf);
                    if compare_no_case(key, b"connection") {
                        if self.closing {
                            header.val = kawa::Store::Static(b"close");
                        } else {
                            let val = header.val.data(buf);
                            self.keep_alive_backend &= !compare_no_case(val, b"close");
                        }
                    }
                }
                _ => {}
            }
        }

        if let Some(sticky_session) = &self.sticky_session {
            if self.sticky_session != self.sticky_session_found {
                response.push_block(kawa::Block::Header(kawa::Pair {
                    key: kawa::Store::Static(b"Set-Cookie"),
                    val: kawa::Store::from_string(format!(
                        "{}={}; Path=/",
                        self.sticky_name, sticky_session
                    )),
                }));
            }
        }

        response.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Sozu-Id"),
            val: kawa::Store::from_string(self.id.to_string()),
        }));
    }
}
