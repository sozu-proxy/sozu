#[cfg(feature = "use-openssl")]
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    os::unix::io::AsRawFd,
    rc::{Rc, Weak},
    str::from_utf8_unchecked,
    sync::{Arc, Mutex},
};
use std::{collections::hash_map::Entry, io::Read};

use anyhow::Context;
use foreign_types_shared::{ForeignType, ForeignTypeRef};
use mio::{net::*, unix::SourceFd, *};
use nom::HexDisplay;
use openssl::{
    dh::Dh,
    error::ErrorStack,
    nid,
    pkey::{PKey, Private},
    ssl::{
        self, select_next_proto, AlpnError, NameType, SniError, Ssl, SslAlert, SslContext,
        SslContextBuilder, SslMethod, SslOptions, SslRef, SslSessionCacheMode, SslStream,
        SslVersion,
    },
    x509::X509,
};
use rustls::{
    cipher_suite::{
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    },
    CipherSuite, ProtocolVersion, ServerConfig, ServerConnection, SupportedCipherSuite,
};
use rusty_ulid::Ulid;
use slab::Slab;
use time::{Duration, Instant};

use crate::{
    backends::BackendMap,
    buffer_queue::BufferQueue,
    pool::Pool,
    protocol::{
        h2::Http2,
        http::{
            answers::HttpAnswers,
            parser::{hostname_and_port, Method, RequestState},
            DefaultAnswerStatus,
        },
        openssl::TlsHandshake as OpensslHandshake,
        proxy_protocol::expect::ExpectProxyProtocol,
        rustls::TlsHandshake as RustlsHandshake,
        Http, Pipe, ProtocolResult, StickySession,
    },
    retry::RetryPolicy,
    router::Router,
    server::{
        push_event, ListenSession, ListenToken, ProxyChannel, Server, SessionManager, SessionToken,
        CONN_RETRIES,
    },
    socket::{server_bind, FrontRustls},
    sozu_command::{
        logging,
        proxy::{
            CertificateFingerprint, Cluster, HttpFrontend, HttpsListener, ProxyEvent, ProxyRequest,
            ProxyRequestOrder, ProxyResponse, ProxyResponseContent, ProxyResponseStatus, Query,
            QueryAnswer, QueryAnswerCertificate, QueryCertificateType, Route, TlsProvider,
            TlsVersion,
        },
        ready::Ready,
        scm_socket::ScmSocket,
    },
    timer::TimeoutContainer,
    tls::{
        CertificateResolver, GenericCertificateResolver, GenericCertificateResolverError,
        MutexWrappedCertificateResolver, ParsedCertificateAndKey,
    },
    util::UnwrapLog,
    AcceptError, Backend, BackendConnectAction, BackendConnectionStatus, ClusterId,
    ConnectionError, ListenerHandler, Protocol, ProxyConfiguration, ProxySession, Readiness,
    SessionMetrics, SessionResult,
};

//const SERVER_PROTOS: &'static [u8] = b"\x02h2\x08http/1.1";
//const SERVER_PROTOS: &'static [u8] = b"\x08http/1.1\x02h2";
const SERVER_PROTOS: &[u8] = b"\x08http/1.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsCluster {
    pub cluster_id: String,
    pub hostname: String,
    pub path_begin: String,
}

pub enum State {
    OpensslExpect(ExpectProxyProtocol<TcpStream>, Ssl),
    OpensslHandshake(OpensslHandshake),
    OpensslHttp(Http<SslStream<TcpStream>, Listener>),
    OpensslWebSocket(Pipe<SslStream<TcpStream>, Listener>),
    OpensslHttp2(Http2<SslStream<TcpStream>>),

    RustlsExpect(ExpectProxyProtocol<TcpStream>, ServerConnection),
    RustlsHandshake(RustlsHandshake),
    RustlsHttp(Http<FrontRustls, Listener>),
    RustlsWebSocket(Pipe<FrontRustls, Listener>),
    RustlsHttp2,
}

pub enum AlpnProtocols {
    H2,
    Http11,
}

pub struct Session {
    pub frontend_token: Token,
    pub backend: Option<Rc<RefCell<Backend>>>,
    pub back_connected: BackendConnectionStatus,
    state: Option<State>,
    pub public_address: SocketAddr,
    pool: Weak<RefCell<Pool>>,
    proxy: Rc<RefCell<Proxy>>,
    pub metrics: SessionMetrics,
    pub cluster_id: Option<String>,
    sticky_name: String,
    last_event: Instant,
    pub listener_token: Token,
    pub connection_attempt: u8,
    peer_address: Option<SocketAddr>,
    answers: Rc<RefCell<HttpAnswers>>,
    front_timeout: TimeoutContainer,
    frontend_timeout_duration: Duration,
    backend_timeout_duration: Duration,
    pub listener: Rc<RefCell<Listener>>,
}

pub enum TlsDetails {
    Openssl(Ssl),
    Rustls(ServerConnection),
}

impl Session {
    pub fn new(
        tls_details: TlsDetails,
        sock: TcpStream,
        token: Token,
        pool: Weak<RefCell<Pool>>,
        proxy: Rc<RefCell<Proxy>>,
        public_address: SocketAddr,
        expect_proxy: bool,
        sticky_name: String,
        answers: Rc<RefCell<HttpAnswers>>,
        listener_token: Token,
        wait_time: Duration,
        frontend_timeout_duration: Duration,
        backend_timeout_duration: Duration,
        request_timeout_duration: Duration,
        listener: Rc<RefCell<Listener>>,
    ) -> Session {
        let peer_address = if expect_proxy {
            // Will be defined later once the expect proxy header has been received and parsed
            None
        } else {
            sock.peer_addr().ok()
        };

        let request_id = Ulid::generate();
        let front_timeout = TimeoutContainer::new(request_timeout_duration, token);

        let state = match (tls_details, expect_proxy) {
            (TlsDetails::Openssl(details), true) => {
                trace!("starting in expect proxy state");
                gauge_add!("protocol.proxy.expect", 1);
                Some(State::OpensslExpect(
                    ExpectProxyProtocol::new(sock, token, request_id),
                    details,
                ))
            }
            (TlsDetails::Openssl(details), false) => {
                gauge_add!("protocol.tls.handshake", 1);
                Some(State::OpensslHandshake(OpensslHandshake::new(
                    details,
                    sock,
                    request_id,
                    peer_address,
                )))
            }
            (TlsDetails::Rustls(details), true) => {
                trace!("starting in expect proxy state");
                gauge_add!("protocol.proxy.expect", 1);
                Some(State::RustlsExpect(
                    ExpectProxyProtocol::new(sock, token, request_id),
                    details,
                ))
            }
            (TlsDetails::Rustls(details), false) => {
                gauge_add!("protocol.tls.handshake", 1);
                Some(State::RustlsHandshake(RustlsHandshake::new(
                    details, sock, request_id,
                )))
            }
        };

        let metrics = SessionMetrics::new(Some(wait_time));
        let mut session = Session {
            frontend_token: token,
            backend: None,
            back_connected: BackendConnectionStatus::NotConnected,
            state,
            public_address,
            pool,
            proxy,
            sticky_name,
            metrics,
            cluster_id: None,
            last_event: Instant::now(),
            listener_token,
            connection_attempt: 0,
            peer_address,
            answers,
            front_timeout,
            frontend_timeout_duration,
            backend_timeout_duration,
            listener,
        };

        session.front_readiness().interest = Ready::readable() | Ready::hup() | Ready::error();
        session
    }

    // pub fn http(&self) -> Option<&Http<SslStream<TcpStream>, Listener>> {
    //     self.state.as_ref().and_then(|protocol| {
    //         if let &State::OpensslHttp(ref http) = protocol {
    //             Some(http)
    //         } else {
    //             None
    //         }
    //     })
    // }

    // pub fn http_mut(&mut self) -> Option<&mut Http<SslStream<TcpStream>, Listener>> {
    //     self.state.as_mut().and_then(|protocol| {
    //         if let &mut State::OpensslHttp(ref mut http) = protocol {
    //             Some(http)
    //         } else {
    //             None
    //         }
    //     })
    // }

    pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Option<Rc<Vec<u8>>>) {
        self.state.as_mut().map(|protocol| match protocol {
            &mut State::OpensslHttp(ref mut http) => {
                http.set_answer(answer, buf);
            }
            &mut State::RustlsHttp(ref mut http) => {
                http.set_answer(answer, buf);
            }
            _ => (),
        });
    }

    pub fn upgrade(&mut self) -> bool {
        let state = unwrap_msg!(self.state.take());

        match state {
            State::OpensslExpect(expect, ssl) => {
                debug!("switching to TLS handshake");
                if let Some(ref addresses) = expect.addresses {
                    if let (Some(public_address), Some(session_address)) =
                        (addresses.destination(), addresses.source())
                    {
                        self.public_address = public_address;
                        self.peer_address = Some(session_address);

                        let ExpectProxyProtocol {
                            frontend,
                            readiness,
                            request_id,
                            ..
                        } = expect;
                        let mut tls =
                            OpensslHandshake::new(ssl, frontend, request_id, self.peer_address);
                        tls.readiness.event = readiness.event;

                        gauge_add!("protocol.proxy.expect", -1);
                        gauge_add!("protocol.tls.handshake", 1);
                        self.state = Some(State::OpensslHandshake(tls));
                        return true;
                    }
                }

                // currently, only happens in expect proxy protocol with AF_UNSPEC address
                //error!("failed to upgrade from expect");
                self.state = Some(State::OpensslExpect(expect, ssl));
                false
            }
            State::OpensslHandshake(handshake) => {
                let pool = self.pool.clone();
                let readiness = handshake.readiness.clone();

                handshake.stream.as_ref().map(|s| {
                    let ssl = s.ssl();
                    if let Some(version) = ssl.version2() {
                        incr!(openssl_version_str(version))
                    }
                    ssl.current_cipher().map(|c| incr!(c.name()));
                });

                let selected_protocol = {
                    let s = handshake
                        .stream
                        .as_ref()
                        .and_then(|s| s.ssl().selected_alpn_protocol());
                    trace!("selected: {}", unsafe {
                        from_utf8_unchecked(s.unwrap_or(&b""[..]))
                    });

                    match s {
                        Some(b"h2") => AlpnProtocols::H2,
                        Some(b"http/1.1") | None => AlpnProtocols::Http11,
                        Some(s) => {
                            error!("unknown alpn protocol: {:?}", s);
                            return false;
                        }
                    }
                };

                match selected_protocol {
                    AlpnProtocols::H2 => {
                        info!("got h2");
                        let mut http = Http2::new(
                            unwrap_msg!(handshake.stream),
                            self.frontend_token,
                            pool,
                            Some(self.public_address),
                            None,
                            self.sticky_name.clone(),
                            Protocol::HTTPS,
                        );

                        http.frontend.readiness = readiness;
                        http.frontend.readiness.interest =
                            Ready::readable() | Ready::hup() | Ready::error();

                        gauge_add!("protocol.tls.handshake", -1);
                        gauge_add!("protocol.http2s", 1);

                        self.state = Some(State::OpensslHttp2(http));
                        true
                    }
                    AlpnProtocols::Http11 => {
                        let backend_timeout_duration = self.backend_timeout_duration;
                        let mut http = Http::new(
                            unwrap_msg!(handshake.stream),
                            self.frontend_token,
                            handshake.request_id,
                            pool,
                            self.public_address,
                            self.peer_address,
                            self.sticky_name.clone(),
                            Protocol::HTTPS,
                            self.answers.clone(),
                            self.front_timeout.take(),
                            self.frontend_timeout_duration,
                            backend_timeout_duration,
                            self.listener.clone(),
                        );

                        http.front_readiness = readiness;
                        http.front_readiness.interest =
                            Ready::readable() | Ready::hup() | Ready::error();

                        gauge_add!("protocol.tls.handshake", -1);
                        gauge_add!("protocol.https", 1);

                        self.state = Some(State::OpensslHttp(http));
                        true
                    }
                }
            }
            State::OpensslHttp(mut http) => {
                debug!("https switching to wss");
                let front_token = self.frontend_token;
                let back_token = unwrap_msg!(http.back_token());
                let ws_context = http.websocket_context();

                let front_buf = match http.front_buf {
                    Some(buf) => buf.buffer,
                    None => {
                        if let Some(p) = self.pool.upgrade() {
                            if let Some(buf) = p.borrow_mut().checkout() {
                                buf
                            } else {
                                return false;
                            }
                        } else {
                            return false;
                        }
                    }
                };
                let back_buf = match http.back_buf {
                    Some(buf) => buf.buffer,
                    None => {
                        if let Some(p) = self.pool.upgrade() {
                            if let Some(buf) = p.borrow_mut().checkout() {
                                buf
                            } else {
                                return false;
                            }
                        } else {
                            return false;
                        }
                    }
                };

                gauge_add!("protocol.https", -1);
                gauge_add!("http.active_requests", -1);
                gauge_add!("websocket.active_requests", 1);
                gauge_add!("protocol.wss", 1);

                let mut pipe = Pipe::new(
                    http.frontend,
                    front_token,
                    http.request_id,
                    http.cluster_id,
                    http.backend_id,
                    Some(ws_context),
                    Some(unwrap_msg!(http.backend)),
                    front_buf,
                    back_buf,
                    http.session_address,
                    Protocol::HTTPS,
                    self.listener.clone(),
                );

                pipe.front_readiness.event = http.front_readiness.event;
                pipe.back_readiness.event = http.back_readiness.event;
                http.front_timeout
                    .set_duration(self.frontend_timeout_duration);
                http.back_timeout
                    .set_duration(self.backend_timeout_duration);
                pipe.front_timeout = Some(http.front_timeout);
                pipe.back_timeout = Some(http.back_timeout);
                pipe.set_back_token(back_token);

                self.state = Some(State::OpensslWebSocket(pipe));
                true
            }
            State::RustlsExpect(expect, ssl) => {
                debug!("switching to TLS handshake");
                if let Some(ref addresses) = expect.addresses {
                    if let (Some(public_address), Some(session_address)) =
                        (addresses.destination(), addresses.source())
                    {
                        self.public_address = public_address;
                        self.peer_address = Some(session_address);

                        let ExpectProxyProtocol {
                            frontend,
                            readiness,
                            request_id,
                            ..
                        } = expect;

                        let mut tls = RustlsHandshake::new(ssl, frontend, request_id);
                        tls.readiness.event = readiness.event;
                        tls.readiness.event.insert(Ready::readable());

                        gauge_add!("protocol.proxy.expect", -1);
                        gauge_add!("protocol.tls.handshake", 1);
                        self.state = Some(State::RustlsHandshake(tls));
                        return true;
                    }
                }

                // currently, only happens in expect proxy protocol with AF_UNSPEC address
                //error!("failed to upgrade from expect");
                self.state = Some(State::RustlsExpect(expect, ssl));
                false
            }
            State::RustlsHandshake(handshake) => {
                let front_buf = self.pool.upgrade().and_then(|p| p.borrow_mut().checkout());
                if front_buf.is_none() {
                    self.state = Some(State::RustlsHandshake(handshake));
                    return false;
                }

                let mut front_buf = front_buf.unwrap();
                if let Some(version) = handshake.session.protocol_version() {
                    incr!(rustls_version_str(version));
                };
                if let Some(cipher) = handshake.session.negotiated_cipher_suite() {
                    incr!(rustls_ciphersuite_str(cipher));
                };

                let front_stream = FrontRustls {
                    stream: handshake.stream,
                    session: handshake.session,
                };

                let readiness = handshake.readiness.clone();
                let mut http = Http::new(
                    front_stream,
                    self.frontend_token,
                    handshake.request_id,
                    self.pool.clone(),
                    self.public_address,
                    self.peer_address,
                    self.sticky_name.clone(),
                    Protocol::HTTPS,
                    self.answers.clone(),
                    self.front_timeout.take(),
                    self.frontend_timeout_duration,
                    self.backend_timeout_duration,
                    self.listener.clone(),
                );

                let res = http.frontend.session.reader().read(front_buf.space());
                match res {
                    Ok(sz) => {
                        //info!("rustls upgrade: there were {} bytes of plaintext available", sz);
                        front_buf.fill(sz);
                        count!("bytes_in", sz as i64);
                        self.metrics.bin += sz;
                    }
                    Err(e) => {
                        error!("read error: {:?}", e);
                    }
                }

                let sz = front_buf.available_data();
                let mut buf = BufferQueue::with_buffer(front_buf);
                buf.sliced_input(sz);

                gauge_add!("protocol.tls.handshake", -1);
                gauge_add!("protocol.https", 1);
                http.front_buf = Some(buf);
                http.front_readiness = readiness;
                http.front_readiness.interest = Ready::readable() | Ready::hup() | Ready::error();

                self.state = Some(State::RustlsHttp(http));
                true
            }
            State::RustlsHttp(mut http) => {
                debug!("https switching to wss");
                let front_token = self.frontend_token;
                let back_token = unwrap_msg!(http.back_token());
                let ws_context = http.websocket_context();

                let front_buf = match http.front_buf {
                    Some(buf) => buf.buffer,
                    None => {
                        let pool = match self.pool.upgrade() {
                            Some(p) => p,
                            None => return false,
                        };

                        let buffer = match pool.borrow_mut().checkout() {
                            Some(buf) => buf,
                            None => return false,
                        };
                        buffer
                    }
                };
                let back_buf = match http.back_buf {
                    Some(buf) => buf.buffer,
                    None => {
                        let pool = match self.pool.upgrade() {
                            Some(p) => p,
                            None => return false,
                        };

                        let buffer = match pool.borrow_mut().checkout() {
                            Some(buf) => buf,
                            None => return false,
                        };
                        buffer
                    }
                };

                let mut pipe = Pipe::new(
                    http.frontend,
                    front_token,
                    http.request_id,
                    http.cluster_id,
                    http.backend_id,
                    Some(ws_context),
                    http.backend,
                    front_buf,
                    back_buf,
                    http.session_address,
                    Protocol::HTTPS,
                    self.listener.clone(),
                );

                pipe.front_readiness.event = http.front_readiness.event;
                pipe.back_readiness.event = http.back_readiness.event;
                http.front_timeout
                    .set_duration(self.frontend_timeout_duration);
                http.back_timeout
                    .set_duration(self.backend_timeout_duration);
                pipe.front_timeout = Some(http.front_timeout);
                pipe.back_timeout = Some(http.back_timeout);
                pipe.set_back_token(back_token);
                pipe.set_cluster_id(self.cluster_id.clone());

                gauge_add!("protocol.https", -1);
                gauge_add!("protocol.wss", 1);
                gauge_add!("websocket.active_requests", 1);
                gauge_add!("http.active_requests", -1);
                self.state = Some(State::RustlsWebSocket(pipe));
                true
            }
            State::OpensslHttp2(_) | State::RustlsHttp2 => todo!(),
            State::OpensslWebSocket(_) | State::RustlsWebSocket(_) => {
                error!("Upgrade called on WSS, this should not happen");
                self.state = Some(state);
                true
            }
        }
    }

    fn front_hup(&mut self) -> SessionResult {
        match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslHttp(ref mut http) => http.front_hup(),
            State::OpensslWebSocket(ref mut pipe) => pipe.front_hup(&mut self.metrics),
            State::OpensslHttp2(ref mut http) => http.front_hup(),
            State::OpensslHandshake(_) => SessionResult::CloseSession,
            State::OpensslExpect(_, _) => SessionResult::CloseSession,

            State::RustlsHttp(ref mut http) => http.front_hup(),
            State::RustlsWebSocket(ref mut pipe) => pipe.front_hup(&mut self.metrics),
            State::RustlsHandshake(_) => SessionResult::CloseSession,
            State::RustlsExpect(_, _) => SessionResult::CloseSession,
            State::RustlsHttp2 => todo!(),
        }
    }

    fn back_hup(&mut self) -> SessionResult {
        match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslHttp(ref mut http) => http.back_hup(),
            State::OpensslWebSocket(ref mut pipe) => pipe.back_hup(&mut self.metrics),
            State::OpensslHttp2(ref mut http) => http.back_hup(),
            State::OpensslHandshake(_) => {
                error!("why a backend HUP event while still in frontend handshake?");
                SessionResult::CloseSession
            }
            State::OpensslExpect(_, _) => {
                error!("why a backend HUP event while still in frontend proxy protocol expect?");
                SessionResult::CloseSession
            }

            State::RustlsHttp(ref mut http) => http.back_hup(),
            State::RustlsWebSocket(ref mut pipe) => pipe.back_hup(&mut self.metrics),
            State::RustlsHandshake(_) => {
                error!("why a backend HUP event while still in frontend handshake?");
                SessionResult::CloseSession
            }
            State::RustlsExpect(_, _) => {
                error!("why a backend HUP event while still in frontend proxy protocol expect?");
                SessionResult::CloseSession
            }
            State::RustlsHttp2 => todo!(),
        }
    }

    fn log_context(&self) -> String {
        match unwrap_msg!(self.state.as_ref()) {
            &State::OpensslHttp(ref http) => http.log_context().to_string(),
            &State::RustlsHttp(ref http) => http.log_context().to_string(),
            _ => "".to_string(),
        }
    }

    fn readable(&mut self) -> SessionResult {
        let (upgrade, session_result) = match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslExpect(ref mut expect, _) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout (HTTPS OpenSSL upgrading from Expect)");
                }
                expect.readable(&mut self.metrics)
            }
            State::OpensslHandshake(ref mut handshake) => {
                if !self.front_timeout.reset() {
                    error!(
                        "could not reset front timeout (HTTPS OpenSSL upgrading from Handshake)"
                    );
                }
                handshake.readable(&mut self.metrics)
            }
            State::OpensslHttp(ref mut http) => {
                (ProtocolResult::Continue, http.readable(&mut self.metrics))
            }
            State::OpensslHttp2(ref mut http) => {
                (ProtocolResult::Continue, http.readable(&mut self.metrics))
            }
            State::OpensslWebSocket(ref mut pipe) => {
                (ProtocolResult::Continue, pipe.readable(&mut self.metrics))
            }

            State::RustlsExpect(ref mut expect, _) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout");
                }
                expect.readable(&mut self.metrics)
            }
            State::RustlsHandshake(ref mut handshake) => {
                if !self.front_timeout.reset() {
                    error!("could not reset front timeout");
                }
                handshake.readable()
            }
            State::RustlsHttp(ref mut http) => {
                (ProtocolResult::Continue, http.readable(&mut self.metrics))
            }
            State::RustlsWebSocket(ref mut pipe) => {
                (ProtocolResult::Continue, pipe.readable(&mut self.metrics))
            }
            State::RustlsHttp2 => todo!(),
        };

        if upgrade == ProtocolResult::Continue {
            session_result
        } else if self.upgrade() {
            match *unwrap_msg!(self.state.as_mut()) {
                State::OpensslHttp(ref mut http) => http.readable(&mut self.metrics),
                State::OpensslHttp2(ref mut http) => http.readable(&mut self.metrics),

                State::RustlsHttp(ref mut http) => http.readable(&mut self.metrics),
                State::RustlsHttp2 => todo!(),
                _ => session_result,
            }
        } else {
            SessionResult::CloseSession
        }
    }

    fn writable(&mut self) -> SessionResult {
        let (upgrade, session_result) = match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslExpect(_, _) | State::OpensslHandshake(_) => {
                return SessionResult::CloseSession
            }
            State::OpensslHttp(ref mut http) => {
                (ProtocolResult::Continue, http.writable(&mut self.metrics))
            }
            State::OpensslHttp2(ref mut http) => {
                (ProtocolResult::Continue, http.writable(&mut self.metrics))
            }
            State::OpensslWebSocket(ref mut pipe) => {
                (ProtocolResult::Continue, pipe.writable(&mut self.metrics))
            }

            State::RustlsExpect(_, _) => return SessionResult::CloseSession,
            State::RustlsHandshake(ref mut handshake) => handshake.writable(),
            State::RustlsHttp(ref mut http) => {
                (ProtocolResult::Continue, http.writable(&mut self.metrics))
            }
            State::RustlsWebSocket(ref mut pipe) => {
                (ProtocolResult::Continue, pipe.writable(&mut self.metrics))
            }
            State::RustlsHttp2 => todo!(),
        };

        if upgrade == ProtocolResult::Continue {
            return session_result;
        }
        if self.upgrade() {
            if (self.front_readiness().event & self.front_readiness().interest).is_writable() {
                return self.writable();
            }
            return SessionResult::Continue;
        }
        SessionResult::CloseSession
    }

    fn back_readable(&mut self) -> SessionResult {
        let (upgrade, session_result) = match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslExpect(_, _) => (ProtocolResult::Continue, SessionResult::CloseSession),
            State::OpensslHttp(ref mut http) => http.back_readable(&mut self.metrics),
            State::OpensslHttp2(ref mut http) => (
                ProtocolResult::Continue,
                http.back_readable(&mut self.metrics),
            ),
            State::OpensslHandshake(_) => (ProtocolResult::Continue, SessionResult::CloseSession),
            State::OpensslWebSocket(ref mut pipe) => (
                ProtocolResult::Continue,
                pipe.back_readable(&mut self.metrics),
            ),

            State::RustlsExpect(_, _) => return SessionResult::CloseSession,
            State::RustlsHttp(ref mut http) => http.back_readable(&mut self.metrics),
            State::RustlsHandshake(_) => (ProtocolResult::Continue, SessionResult::CloseSession),
            State::RustlsWebSocket(ref mut pipe) => (
                ProtocolResult::Continue,
                pipe.back_readable(&mut self.metrics),
            ),
            State::RustlsHttp2 => todo!(),
        };

        if upgrade == ProtocolResult::Continue {
            return session_result;
        }
        if !self.upgrade() {
            return SessionResult::CloseSession;
        }
        match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslWebSocket(ref mut pipe) => pipe.back_readable(&mut self.metrics),
            State::RustlsWebSocket(ref mut pipe) => pipe.back_readable(&mut self.metrics),
            _ => session_result,
        }
    }

    fn back_writable(&mut self) -> SessionResult {
        match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslExpect(_, _) | State::OpensslHandshake(_) => SessionResult::CloseSession,
            State::OpensslHttp(ref mut http) => http.back_writable(&mut self.metrics),
            State::OpensslHttp2(ref mut http) => http.back_writable(&mut self.metrics),
            State::OpensslWebSocket(ref mut pipe) => pipe.back_writable(&mut self.metrics),

            State::RustlsExpect(_, _) | State::RustlsHandshake(_) => SessionResult::CloseSession,
            State::RustlsHttp(ref mut http) => http.back_writable(&mut self.metrics),
            State::RustlsWebSocket(ref mut pipe) => pipe.back_writable(&mut self.metrics),
            State::RustlsHttp2 => todo!(),
        }
    }

    fn front_socket(&self) -> Option<&TcpStream> {
        match unwrap_msg!(self.state.as_ref()) {
            State::OpensslExpect(expect, _) => Some(expect.front_socket()),
            State::OpensslHandshake(ref handshake) => handshake.socket(),
            State::OpensslHttp(http) => Some(http.front_socket()),
            State::OpensslHttp2(http) => Some(http.front_socket()),
            State::OpensslWebSocket(pipe) => Some(pipe.front_socket()),

            State::RustlsExpect(expect, _) => Some(expect.front_socket()),
            State::RustlsHandshake(handshake) => Some(&handshake.stream),
            State::RustlsHttp(http) => Some(http.front_socket()),
            State::RustlsWebSocket(pipe) => Some(pipe.front_socket()),
            State::RustlsHttp2 => todo!(),
        }
    }

    fn back_socket(&self) -> Option<&TcpStream> {
        match *unwrap_msg!(self.state.as_ref()) {
            State::OpensslExpect(_, _) => None,
            State::OpensslHandshake(_) => None,
            State::OpensslHttp(ref http) => http.back_socket(),
            State::OpensslHttp2(ref http) => http.back_socket(),
            State::OpensslWebSocket(ref pipe) => pipe.back_socket(),

            State::RustlsExpect(_, _) => None,
            State::RustlsHandshake(_) => None,
            State::RustlsHttp(ref http) => http.back_socket(),
            State::RustlsWebSocket(ref pipe) => pipe.back_socket(),
            State::RustlsHttp2 => todo!(),
        }
    }

    fn back_token(&self) -> Option<Token> {
        match unwrap_msg!(self.state.as_ref()) {
            State::OpensslExpect(_, _) => None,
            State::OpensslHandshake(_) => None,
            State::OpensslHttp(ref http) => http.back_token(),
            State::OpensslHttp2(ref http) => http.back_token(),
            State::OpensslWebSocket(ref pipe) => pipe.back_token(),

            State::RustlsExpect(_, _) => None,
            State::RustlsHandshake(_) => None,
            State::RustlsHttp(ref http) => http.back_token(),
            State::RustlsWebSocket(ref pipe) => pipe.back_token(),
            State::RustlsHttp2 => todo!(),
        }
    }

    fn set_back_socket(&mut self, sock: TcpStream) {
        let backend = self.backend.clone();
        match unwrap_msg!(self.state.as_mut()) {
            State::OpensslHttp(ref mut http) => http.set_back_socket(sock, backend),
            State::RustlsHttp(ref mut http) => http.set_back_socket(sock, backend),
            _ => {
                // what to do when set_back_socket is called on the other states?
                todo!()
            }
        }
    }

    fn set_back_token(&mut self, token: Token) {
        match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslHttp(ref mut http) => http.set_back_token(token),
            State::OpensslHttp2(ref mut http) => http.set_back_token(token),
            State::OpensslWebSocket(ref mut pipe) => pipe.set_back_token(token),

            State::RustlsHttp(ref mut http) => http.set_back_token(token),
            State::RustlsHttp2 => todo!(),
            State::RustlsWebSocket(ref mut pipe) => pipe.set_back_token(token),

            _ => {}
        }
    }

    fn back_connected(&self) -> BackendConnectionStatus {
        self.back_connected
    }

    fn set_back_connected(&mut self, connected: BackendConnectionStatus) {
        let last = self.back_connected;
        self.back_connected = connected;

        if connected != BackendConnectionStatus::Connected {
            return;
        }

        gauge_add!("backend.connections", 1);
        gauge_add!(
            "connections_per_backend",
            1,
            self.cluster_id.as_deref(),
            self.metrics.backend_id.as_deref()
        );

        // the back timeout was of connect_timeout duration before,
        // now that we're connected, move to backend_timeout duration
        let timeout = self.backend_timeout_duration;
        match self.state.as_mut() {
            Some(State::OpensslHttp(ref mut http)) => {
                http.set_back_timeout(timeout);
                http.cancel_backend_timeout();
            }
            Some(State::RustlsHttp(ref mut http)) => {
                http.set_back_timeout(timeout);
                http.cancel_backend_timeout();
            }
            _ => {}
        }

        if let Some(backend) = &self.backend {
            let mut backend = backend.borrow_mut();

            if backend.retry_policy.is_down() {
                incr!(
                    "up",
                    self.cluster_id.as_deref(),
                    self.metrics.backend_id.as_deref()
                );

                info!(
                    "backend server {} at {} is up",
                    backend.backend_id, backend.address
                );

                push_event(ProxyEvent::BackendUp(
                    backend.backend_id.clone(),
                    backend.address,
                ));
            }

            if let BackendConnectionStatus::Connecting(start) = last {
                backend.set_connection_time(Instant::now() - start);
            }

            backend.active_requests += 1;
            backend.failures = 0;
            backend.retry_policy.succeed();
        }
    }

    fn metrics(&mut self) -> &mut SessionMetrics {
        &mut self.metrics
    }

    fn remove_backend(&mut self) {
        if let Some(backend) = self.backend.take() {
            match self.state.as_mut() {
                Some(State::OpensslHttp(ref mut http)) => http.clear_back_token(),
                Some(State::RustlsHttp(ref mut http)) => http.clear_back_token(),
                _ => {}
            }

            backend.borrow_mut().dec_connections();
        }
    }

    fn front_readiness(&mut self) -> &mut Readiness {
        match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslExpect(ref mut expect, _) => &mut expect.readiness,
            State::OpensslHandshake(ref mut handshake) => &mut handshake.readiness,
            State::OpensslHttp(ref mut http) => http.front_readiness(),
            State::OpensslHttp2(ref mut http) => http.front_readiness(),
            State::OpensslWebSocket(ref mut pipe) => &mut pipe.front_readiness,

            State::RustlsExpect(ref mut expect, _) => &mut expect.readiness,
            State::RustlsHandshake(ref mut handshake) => &mut handshake.readiness,
            State::RustlsHttp(ref mut http) => http.front_readiness(),
            State::RustlsWebSocket(ref mut pipe) => &mut pipe.front_readiness,
            State::RustlsHttp2 => todo!(),
        }
    }

    fn back_readiness(&mut self) -> Option<&mut Readiness> {
        match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslHttp(ref mut http) => Some(http.back_readiness()),
            State::OpensslHttp2(ref mut http) => Some(http.back_readiness()),
            State::OpensslWebSocket(ref mut pipe) => Some(&mut pipe.back_readiness),

            State::RustlsHttp(ref mut http) => Some(http.back_readiness()),
            State::RustlsHttp2 => todo!(),
            State::RustlsWebSocket(ref mut pipe) => Some(&mut pipe.back_readiness),

            _ => None,
        }
    }

    fn fail_backend_connection(&mut self) {
        self.backend.as_ref().map(|backend| {
            let mut backend = backend.borrow_mut();
            backend.failures += 1;

            let already_unavailable = backend.retry_policy.is_down();
            backend.retry_policy.fail();
            incr!(
                "connections.error",
                self.cluster_id.as_deref(),
                self.metrics.backend_id.as_deref()
            );
            if !already_unavailable && backend.retry_policy.is_down() {
                error!(
                    "backend server {} at {} is down",
                    backend.backend_id, backend.address
                );
                incr!(
                    "down",
                    self.cluster_id.as_deref(),
                    self.metrics.backend_id.as_deref()
                );

                push_event(ProxyEvent::BackendDown(
                    backend.backend_id.clone(),
                    backend.address,
                ));
            }
        });
    }

    fn reset_connection_attempt(&mut self) {
        self.connection_attempt = 0;
    }

    fn cancel_timeouts(&mut self) {
        self.front_timeout.cancel();

        match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslHttp(ref mut http) => http.cancel_timeouts(),
            State::OpensslWebSocket(ref mut pipe) => pipe.cancel_timeouts(),

            State::RustlsHttp(ref mut http) => http.cancel_timeouts(),
            State::RustlsWebSocket(ref mut pipe) => pipe.cancel_timeouts(),

            _ => {}
        }
    }

    fn cancel_backend_timeout(&mut self) {
        self.front_timeout.cancel();

        match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslHttp(ref mut http) => http.cancel_backend_timeout(),

            State::RustlsHttp(ref mut http) => http.cancel_backend_timeout(),

            _ => {}
        }
    }

    fn ready_inner(&mut self, session: Rc<RefCell<dyn ProxySession>>) -> SessionResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        if self.back_connected().is_connecting()
            && self
                .back_readiness()
                .map(|r| r.event != Ready::empty())
                .unwrap_or(false)
        {
            self.cancel_backend_timeout();

            let back_is_hup = self
                .back_readiness()
                .map(|r| r.event.is_hup())
                .unwrap_or(false);
            let back_is_alive = match self.state.as_mut() {
                Some(State::OpensslHttp(ref mut http)) => http.test_back_socket(),
                Some(State::RustlsHttp(ref mut http)) => http.test_back_socket(),
                _ => false,
            };
            if back_is_hup && !back_is_alive {
                //retry connecting the backend
                error!(
                    "{} error connecting to backend, trying again",
                    self.log_context()
                );
                self.connection_attempt += 1;
                self.fail_backend_connection();

                self.back_connected = BackendConnectionStatus::Connecting(Instant::now());

                // trigger a backend reconnection
                self.close_backend();
                match self.connect_to_backend(session.clone()) {
                    // reuse connection or send a default answer, we can continue
                    Ok(BackendConnectAction::Reuse) | Err(_) => {}
                    // New or Replace: stop here, we must wait for an event
                    _ => return SessionResult::Continue,
                }
            } else {
                self.metrics().backend_connected();
                self.reset_connection_attempt();
                self.set_back_connected(BackendConnectionStatus::Connected);
            }
        }

        if self.front_readiness().event.is_hup() {
            let order = self.front_hup();
            match order {
                SessionResult::CloseSession => {
                    return order;
                }
                _ => {
                    self.front_readiness().event.remove(Ready::hup());
                    return order;
                }
            }
        }

        let token = self.frontend_token;
        while counter < max_loop_iterations {
            let front_interest = self.front_readiness().interest & self.front_readiness().event;
            let back_interest = self
                .back_readiness()
                .map(|r| r.interest & r.event)
                .unwrap_or(Ready::empty());

            trace!(
                "PROXY\t{} {:?} {:?} -> {:?}",
                self.log_context(),
                token,
                self.front_readiness().clone(),
                self.back_readiness()
            );

            if front_interest == Ready::empty() && back_interest == Ready::empty() {
                break;
            }

            if self
                .back_readiness()
                .map(|r| r.event.is_hup())
                .unwrap_or(false)
                && self.front_readiness().interest.is_writable()
                && !self.front_readiness().event.is_writable()
            {
                break;
            }

            if front_interest.is_readable() {
                let order = self.readable();
                trace!("front readable\tinterpreting session order {:?}", order);

                match order {
                    SessionResult::ConnectBackend => {
                        match self.connect_to_backend(session.clone()) {
                            // reuse connection or send a default answer, we can continue
                            Ok(BackendConnectAction::Reuse) | Err(_) => {}
                            // New or Replace: stop here, we must wait for an event
                            _ => return SessionResult::Continue,
                        }
                    }
                    SessionResult::Continue => {}
                    order => return order,
                }
            }

            if back_interest.is_writable() {
                let order = self.back_writable();
                if order != SessionResult::Continue {
                    return order;
                }
            }

            if back_interest.is_readable() {
                let order = self.back_readable();
                if order != SessionResult::Continue {
                    return order;
                }
            }

            if front_interest.is_writable() {
                let order = self.writable();
                trace!("front writable\tinterpreting session order {:?}", order);
                if order != SessionResult::Continue {
                    return order;
                }
            }

            if back_interest.is_hup() {
                let order = self.back_hup();
                match order {
                    SessionResult::CloseSession | SessionResult::CloseBackend => {
                        return order;
                    }
                    SessionResult::Continue => {
                        // FIXME: do we have to fix something?
                        /*self.front_readiness().interest.insert(Ready::writable());
                        if ! self.front_readiness().event.is_writable() {
                          error!("got back socket HUP but there's still data, and front is not writable yet(openssl)[{:?} => {:?}]", self.frontend_token, self.back_token());
                          break;
                        }*/
                    }
                    _ => {
                        if let Some(r) = self.back_readiness() {
                            r.event.remove(Ready::hup())
                        }
                        return order;
                    }
                };
            }

            if front_interest.is_error() {
                error!(
                    "PROXY session {:?} front error, disconnecting",
                    self.frontend_token
                );
                self.front_readiness().interest = Ready::empty();
                self.back_readiness().map(|r| r.interest = Ready::empty());
                return SessionResult::CloseSession;
            }

            if back_interest.is_error() && self.back_hup() == SessionResult::CloseSession {
                self.front_readiness().interest = Ready::empty();
                self.back_readiness().map(|r| r.interest = Ready::empty());
                error!(
                    "PROXY session {:?} back error, disconnecting",
                    self.frontend_token
                );
                return SessionResult::CloseSession;
            }

            counter += 1;
        }

        if counter == max_loop_iterations {
            error!("PROXY\thandling session {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.frontend_token, max_loop_iterations);
            incr!("https_openssl.infinite_loop.error");

            let front_interest = self.front_readiness().filter_interest();
            let back_interest = self.back_readiness().map(|r| r.interest & r.event);

            let token = self.frontend_token;
            let back = self.back_readiness().cloned();
            error!(
                "PROXY\t{:?} readiness: {:?} -> {:?} | front: {:?} | back: {:?} ",
                token,
                self.front_readiness(),
                back,
                front_interest,
                back_interest
            );
            self.print_state();

            return SessionResult::CloseSession;
        }

        SessionResult::Continue
    }

    fn close_backend(&mut self) {
        if let Some(token) = self.back_token() {
            if let Some(fd) = self.back_socket().map(|stream| stream.as_raw_fd()) {
                let proxy = self.proxy.borrow();
                if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
                    error!("1error deregistering socket({:?}): {:?}", fd, e);
                }

                proxy.sessions.borrow_mut().slab.try_remove(token.0);
            }

            self.remove_backend();

            let back_connected = self.back_connected();
            if back_connected != BackendConnectionStatus::NotConnected {
                self.back_readiness().map(|r| r.event = Ready::empty());
                if let Some(sock) = self.back_socket() {
                    if let Err(e) = sock.shutdown(Shutdown::Both) {
                        if e.kind() != ErrorKind::NotConnected {
                            error!("error closing back socket({:?}): {:?}", sock, e);
                        }
                    }
                }
            }

            if back_connected == BackendConnectionStatus::Connected {
                gauge_add!("backend.connections", -1);
                gauge_add!(
                    "connections_per_backend",
                    -1,
                    self.cluster_id.as_deref(),
                    self.metrics.backend_id.as_deref()
                );
            }

            self.set_back_connected(BackendConnectionStatus::NotConnected);

            match self.state.as_mut() {
                Some(State::OpensslHttp(ref mut http)) => {
                    http.clear_back_token();
                    http.remove_backend();
                }
                Some(State::RustlsHttp(ref mut http)) => {
                    http.clear_back_token();
                    http.remove_backend();
                }
                _ => {}
            }
        }
    }

    fn check_circuit_breaker(&mut self) -> Result<(), ConnectionError> {
        if self.connection_attempt >= CONN_RETRIES {
            error!("{} max connection attempt reached", self.log_context());
            self.set_answer(DefaultAnswerStatus::Answer503, None);
            return Err(ConnectionError::NoBackendAvailable);
        }
        Ok(())
    }

    fn check_backend_connection(&mut self) -> bool {
        let is_valid_backend_socket = match self.state.as_ref() {
            Some(State::OpensslHttp(ref http)) => http.is_valid_backend_socket(),
            Some(State::RustlsHttp(ref http)) => http.is_valid_backend_socket(),
            _ => false,
        };

        if is_valid_backend_socket {
            //matched on keepalive
            self.metrics.backend_id = self.backend.as_ref().map(|i| i.borrow().backend_id.clone());
            self.metrics.backend_start();

            let backend = match self.state {
                Some(State::OpensslHttp(ref mut http)) => http.backend_data.as_mut(),
                Some(State::RustlsHttp(ref mut http)) => http.backend_data.as_mut(),
                _ => None,
            };
            backend.map(|backend| backend.borrow_mut().active_requests += 1);
            return true;
        }
        false
    }

    pub fn extract_route(&self) -> Result<(&str, &str, &Method), ConnectionError> {
        let request_state = match self.state.as_ref() {
            Some(State::OpensslHttp(ref http)) => http.request_state.as_ref(),
            Some(State::RustlsHttp(ref http)) => http.request_state.as_ref(),
            _ => None,
        };

        let host = request_state
            .and_then(|request_state| request_state.get_host())
            .ok_or(ConnectionError::NoHostGiven)?;

        let host: &str = match hostname_and_port(host.as_bytes()) {
            Ok((remaining_input, (hostname, port))) => {
                if remaining_input != &b""[..] {
                    error!(
                        "connect_to_backend: invalid remaining chars after hostname. Host: {}",
                        host
                    );
                    return Err(ConnectionError::InvalidHost);
                }

                // it is alright to call from_utf8_unchecked,
                // we already verified that there are only ascii
                // chars in there
                let hostname = unsafe { from_utf8_unchecked(hostname) };

                //FIXME: what if we don't use SNI?
                let servername = match self.state.as_ref() {
                    Some(State::OpensslHttp(ref http)) => {
                        http.frontend.ssl().servername(NameType::HOST_NAME)
                    }
                    Some(State::RustlsHttp(ref http)) => http.frontend.session.sni_hostname(),
                    _ => None,
                };
                let servername = servername.map(|name| name.to_string());

                if servername.as_deref() != Some(hostname) {
                    error!(
                        "TLS SNI hostname '{:?}' and Host header '{}' don't match",
                        servername, hostname
                    );
                    /*FIXME: deactivate this check for a temporary test
                    unwrap_msg!(session.http()).set_answer(DefaultAnswerStatus::Answer404, None);
                    */
                    return Err(ConnectionError::HostNotFound);
                }

                //FIXME: we should check that the port is right too

                if port == Some(&b"443"[..]) {
                    hostname
                } else {
                    host
                }
            }
            Err(_) => {
                error!("hostname parsing failed");
                return Err(ConnectionError::InvalidHost);
            }
        };

        let request_line = request_state
            .as_ref()
            .and_then(|r| r.get_request_line())
            .ok_or(ConnectionError::NoRequestLineGiven)?;

        Ok((host, &request_line.uri, &request_line.method))
    }

    pub fn backend_from_request(
        &mut self,
        cluster_id: &str,
        front_should_stick: bool,
    ) -> Result<TcpStream, ConnectionError> {
        let request_state = match self.state {
            Some(State::OpensslHttp(ref mut http)) => http.request_state.as_ref(),
            Some(State::RustlsHttp(ref mut http)) => http.request_state.as_ref(),
            _ => None,
        };

        let sticky_session =
            request_state.and_then(|request_state| request_state.get_sticky_session());

        let res = match (front_should_stick, sticky_session) {
            (true, Some(sticky_session)) => self
                .proxy
                .borrow()
                .backends
                .borrow_mut()
                .backend_from_sticky_session(cluster_id, sticky_session)
                .map_err(|e| {
                    debug!(
                        "Couldn't find a backend corresponding to sticky_session {} for cluster {}",
                        sticky_session, cluster_id
                    );
                    e
                }),
            _ => self
                .proxy
                .borrow()
                .backends
                .borrow_mut()
                .backend_from_cluster_id(cluster_id),
        };

        match res {
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer503, None);
                Err(e)
            }
            Ok((backend, conn)) => {
                if front_should_stick {
                    let sticky_name = self.proxy.borrow().listeners[&self.listener_token]
                        .borrow()
                        .config
                        .sticky_name
                        .clone();
                    let sticky_session = Some(StickySession::new(
                        backend
                            .borrow()
                            .sticky_id
                            .clone()
                            .unwrap_or(backend.borrow().backend_id.clone()),
                    ));

                    match self.state {
                        Some(State::OpensslHttp(ref mut http)) => {
                            http.sticky_session = sticky_session;
                            http.sticky_name = sticky_name;
                        }
                        Some(State::RustlsHttp(ref mut http)) => {
                            http.sticky_session = sticky_session;
                            http.sticky_name = sticky_name;
                        }
                        _ => {}
                    };
                }
                self.metrics.backend_id = Some(backend.borrow().backend_id.clone());
                self.metrics.backend_start();

                match self.state {
                    Some(State::OpensslHttp(ref mut http)) => {
                        http.set_backend_id(backend.borrow().backend_id.clone())
                    }
                    Some(State::RustlsHttp(ref mut http)) => {
                        http.set_backend_id(backend.borrow().backend_id.clone())
                    }
                    _ => {}
                }
                self.backend = Some(backend);

                Ok(conn)
            }
        }
    }

    fn cluster_id_from_request(&mut self) -> Result<String, ConnectionError> {
        let (host, uri, method) = match self.extract_route() {
            Ok(tuple) => tuple,
            Err(e) => {
                self.set_answer(DefaultAnswerStatus::Answer400, None);
                return Err(e);
            }
        };

        let route_res = self
            .proxy
            .borrow()
            .listeners
            .get(&self.listener_token)
            .as_ref()
            .and_then(|listener| listener.borrow().frontend_from_request(host, uri, method));

        match route_res {
            Some(Route::ClusterId(cluster_id)) => Ok(cluster_id),
            Some(Route::Deny) => {
                self.set_answer(DefaultAnswerStatus::Answer401, None);
                Err(ConnectionError::Unauthorized)
            }
            None => {
                self.set_answer(DefaultAnswerStatus::Answer404, None);
                Err(ConnectionError::HostNotFound)
            }
        }
    }

    fn connect_to_backend(
        &mut self,
        session_rc: Rc<RefCell<dyn ProxySession>>,
    ) -> Result<BackendConnectAction, ConnectionError> {
        let old_cluster_id = match &self.state {
            Some(State::OpensslHttp(http)) => http.cluster_id.clone(),
            Some(State::RustlsHttp(http)) => http.cluster_id.clone(),
            _ => None,
        };
        let old_back_token = self.back_token();

        self.check_circuit_breaker()?;

        let requested_cluster_id = self.cluster_id_from_request()?;

        let backend_is_connected = self.back_connected == BackendConnectionStatus::Connected;

        if old_cluster_id.as_ref() == Some(&requested_cluster_id) && backend_is_connected {
            let has_backend = self
                .backend
                .as_ref()
                .map(|backend| {
                    let backend = &(*backend.borrow());
                    self.proxy
                        .borrow()
                        .backends
                        .borrow()
                        .has_backend(&requested_cluster_id, backend)
                })
                .unwrap_or(false);

            if has_backend && self.check_backend_connection() {
                return Ok(BackendConnectAction::Reuse);
            } else if let Some(token) = self.back_token() {
                self.close_backend();

                //reset the back token here so we can remove it
                //from the slab after backend_from* fails
                self.set_back_token(token);
            }
        }

        //replacing with a connection to another cluster
        if old_cluster_id.is_some() && old_cluster_id.as_ref() != Some(&requested_cluster_id) {
            if let Some(token) = self.back_token() {
                self.close_backend();

                //reset the back token here so we can remove it
                //from the slab after backend_from* fails
                self.set_back_token(token);
            }
        }

        self.cluster_id = Some(requested_cluster_id.clone());
        match &mut self.state {
            Some(State::OpensslHttp(http)) => http.cluster_id = Some(requested_cluster_id.clone()),
            Some(State::RustlsHttp(http)) => http.cluster_id = Some(requested_cluster_id.clone()),
            _ => {}
        }

        let front_should_stick = self
            .proxy
            .borrow()
            .clusters
            .get(&requested_cluster_id)
            .map(|cluster| cluster.sticky_session)
            .unwrap_or(false);
        let mut socket = self.backend_from_request(&requested_cluster_id, front_should_stick)?;

        // we still want to use the new socket
        if let Err(e) = socket.set_nodelay(true) {
            error!(
                "error setting nodelay on back socket({:?}): {:?}",
                socket, e
            );
        }
        if let Some(r) = self.back_readiness() {
            r.interest = Ready::writable() | Ready::hup() | Ready::error();
        }

        let connect_timeout = Duration::seconds(i64::from(
            self.proxy
                .borrow()
                .listeners
                .get(&self.listener_token)
                .as_ref()
                .map(|listener| listener.borrow().config.connect_timeout)
                .unwrap(),
        ));

        self.back_connected = BackendConnectionStatus::Connecting(Instant::now());

        let action_result = match old_back_token {
            Some(back_token) => {
                self.set_back_token(back_token);
                if let Err(e) = self.proxy.borrow().registry.register(
                    &mut socket,
                    back_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    error!("error registering back socket({:?}): {:?}", socket, e);
                }

                Ok(BackendConnectAction::Replace)
            }
            None => {
                if self.proxy.borrow().sessions.borrow().slab.len()
                    >= self.proxy.borrow().sessions.borrow().slab_capacity()
                {
                    error!("not enough memory, cannot connect to backend");
                    self.set_answer(DefaultAnswerStatus::Answer503, None);
                    return Err(ConnectionError::TooManyConnections);
                }

                let back_token = {
                    let proxy = self.proxy.borrow();
                    let mut session_manager = proxy.sessions.borrow_mut();
                    let entry = session_manager.slab.vacant_entry();
                    let back_token = Token(entry.key());
                    let _entry = entry.insert(session_rc.clone());
                    back_token
                };

                if let Err(e) = self.proxy.borrow().registry.register(
                    &mut socket,
                    back_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    error!("error registering back socket({:?}): {:?}", socket, e);
                }

                self.set_back_token(back_token);
                Ok(BackendConnectAction::New)
            }
        };

        self.set_back_socket(socket);
        match &mut self.state {
            Some(State::OpensslHttp(http)) => http.set_back_timeout(connect_timeout),
            Some(State::RustlsHttp(http)) => http.set_back_timeout(connect_timeout),
            _ => {}
        }
        action_result
    }
}

impl ProxySession for Session {
    fn close(&mut self) {
        //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.frontend_token, *self.temp.front_buf, *self.temp.back_buf);
        match &mut self.state {
            Some(State::OpensslHttp(http)) => http.close(),
            Some(State::RustlsHttp(http)) => http.close(),
            _ => (),
        }

        self.metrics.service_stop();
        self.cancel_timeouts();
        if let Some(front_socket) = self.front_socket() {
            if let Err(e) = front_socket.shutdown(Shutdown::Both) {
                if e.kind() != ErrorKind::NotConnected {
                    error!("error closing front socket({:?}): {:?}", front_socket, e);
                }
            }
        }

        self.close_backend();

        match self.state {
            Some(State::OpensslHttp(ref mut http)) => {
                //if the state was initial, the connection was already reset
                if http.request_state != Some(RequestState::Initial) {
                    gauge_add!("http.active_requests", -1);

                    if let Some(b) = http.backend_data.as_mut() {
                        let mut backend = b.borrow_mut();
                        backend.active_requests = backend.active_requests.saturating_sub(1);
                    }
                }
            }
            Some(State::OpensslWebSocket(_)) => {
                if let Some(backend) = self.backend.as_mut() {
                    let mut backend = backend.borrow_mut();
                    backend.active_requests = backend.active_requests.saturating_sub(1);
                }
            }
            Some(State::OpensslExpect(_, _)) => gauge_add!("protocol.proxy.expect", -1),
            Some(State::OpensslHandshake(_)) => gauge_add!("protocol.tls.handshake", -1),
            Some(State::OpensslHttp2(_)) => gauge_add!("protocol.http2s", -1),

            Some(State::RustlsHttp(ref mut http)) => {
                //if the state was initial, the connection was already reset
                if http.request_state != Some(RequestState::Initial) {
                    gauge_add!("http.active_requests", -1);

                    if let Some(b) = http.backend_data.as_mut() {
                        let mut backend = b.borrow_mut();
                        backend.active_requests = backend.active_requests.saturating_sub(1);
                    }
                }
            }
            Some(State::RustlsWebSocket(_)) => {
                if let Some(backend) = self.backend.as_mut() {
                    let mut backend = backend.borrow_mut();
                    backend.active_requests = backend.active_requests.saturating_sub(1);
                }
            }
            Some(State::RustlsExpect(_, _)) => gauge_add!("protocol.proxy.expect", -1),
            Some(State::RustlsHandshake(_)) => gauge_add!("protocol.tls.handshake", -1),
            Some(State::RustlsHttp2) => todo!(),

            None => {}
        }

        if let Some(fd) = self.front_socket().map(|stream| stream.as_raw_fd()) {
            let proxy = self.proxy.borrow();
            if let Err(e) = proxy.registry.deregister(&mut SourceFd(&fd)) {
                error!("1error deregistering socket({:?}): {:?}", fd, e);
            }
        }
    }

    fn timeout(&mut self, token: Token) {
        let session_result = match *unwrap_msg!(self.state.as_mut()) {
            State::OpensslExpect(_, _) => {
                if token == self.frontend_token {
                    self.front_timeout.triggered();
                }
                SessionResult::CloseSession
            }
            State::OpensslHandshake(_) => {
                if token == self.frontend_token {
                    self.front_timeout.triggered();
                }
                SessionResult::CloseSession
            }
            State::OpensslWebSocket(ref mut pipe) => pipe.timeout(token, &mut self.metrics),
            State::OpensslHttp(ref mut http) => http.timeout(token, &mut self.metrics),
            State::OpensslHttp2(_) => todo!(),

            State::RustlsExpect(_, _) => {
                if token == self.frontend_token {
                    self.front_timeout.triggered();
                }
                SessionResult::CloseSession
            }
            State::RustlsHandshake(_) => {
                if token == self.frontend_token {
                    self.front_timeout.triggered();
                }
                SessionResult::CloseSession
            }
            State::RustlsWebSocket(ref mut pipe) => pipe.timeout(token, &mut self.metrics),
            State::RustlsHttp(ref mut http) => http.timeout(token, &mut self.metrics),
            State::RustlsHttp2 => todo!(),
        };

        if session_result == SessionResult::CloseSession {
            self.close();
        }
    }

    fn protocol(&self) -> Protocol {
        Protocol::HTTPS
    }

    fn process_events(&mut self, token: Token, events: Ready) {
        trace!(
            "token {:?} got event {}",
            token,
            super::ready_to_string(events)
        );
        self.last_event = Instant::now();
        self.metrics.wait_start();

        if self.frontend_token == token {
            self.front_readiness().event = self.front_readiness().event | events;
        } else if self.back_token() == Some(token) {
            self.back_readiness().map(|r| r.event |= events);
        }
    }

    fn ready(&mut self, session: Rc<RefCell<dyn ProxySession>>) {
        self.metrics().service_start();

        match self.ready_inner(session) {
            SessionResult::CloseSession => self.close(),
            SessionResult::CloseBackend => self.close_backend(),
            _ => {}
        }

        self.metrics().service_stop();
    }

    fn shutting_down(&mut self) {
        let session_result = match &mut self.state {
            Some(State::OpensslHttp(http)) => http.shutting_down(),
            Some(State::OpensslHandshake(_)) => SessionResult::Continue,
            Some(State::RustlsHttp(http)) => http.shutting_down(),
            Some(State::RustlsHandshake(_)) => SessionResult::Continue,
            _ => SessionResult::CloseSession,
        };

        if session_result == SessionResult::CloseSession {
            self.close();
            self.proxy
                .borrow()
                .sessions
                .borrow_mut()
                .slab
                .try_remove(self.frontend_token.0);
        }
    }

    fn last_event(&self) -> Instant {
        self.last_event
    }

    fn print_state(&self) {
        let protocol: String = match &self.state {
            Some(State::OpensslExpect(_, _)) => String::from("Expect"),
            Some(State::OpensslHandshake(_)) => String::from("Handshake"),
            Some(State::OpensslHttp(h)) => h.print_state("HTTPS"),
            Some(State::OpensslHttp2(h)) => format!("HTTP2S: {:?}", h.state),
            Some(State::OpensslWebSocket(_)) => String::from("WSS"),

            Some(State::RustlsExpect(_, _)) => String::from("Expect"),
            Some(State::RustlsHandshake(_)) => String::from("Handshake"),
            Some(State::RustlsHttp(h)) => h.print_state("HTTPS"),
            Some(State::RustlsHttp2) => todo!(),
            Some(State::RustlsWebSocket(_)) => String::from("WSS"),

            None => String::from("None"),
        };

        let front_readiness = match *unwrap_msg!(self.state.as_ref()) {
            State::OpensslExpect(ref expect, _) => &expect.readiness,
            State::OpensslHandshake(ref handshake) => &handshake.readiness,
            State::OpensslHttp(ref http) => &http.front_readiness,
            State::OpensslHttp2(ref http) => &http.frontend.readiness,
            State::OpensslWebSocket(ref pipe) => &pipe.front_readiness,

            State::RustlsExpect(ref expect, _) => &expect.readiness,
            State::RustlsHandshake(ref handshake) => &handshake.readiness,
            State::RustlsHttp(ref http) => &http.front_readiness,
            State::RustlsHttp2 => todo!(),
            State::RustlsWebSocket(ref pipe) => &pipe.front_readiness,
        };
        let back_readiness = match *unwrap_msg!(self.state.as_ref()) {
            State::OpensslHttp(ref http) => Some(&http.back_readiness),
            State::OpensslHttp2(ref http) => Some(&http.back_readiness),
            State::OpensslWebSocket(ref pipe) => Some(&pipe.back_readiness),

            State::RustlsHttp(ref http) => Some(&http.back_readiness),
            State::RustlsHttp2 => todo!(),
            State::RustlsWebSocket(ref pipe) => Some(&pipe.back_readiness),
            _ => None,
        };

        error!(
            "zombie session[{:?} => {:?}], state => readiness: {:?} -> {:?}, protocol: {}, cluster_id: {:?}, back_connected: {:?}, metrics: {:?}",
            self.frontend_token, self.back_token(), front_readiness, back_readiness, protocol, self.cluster_id, self.back_connected, self.metrics
        );
    }

    fn tokens(&self) -> Vec<Token> {
        let mut tokens = vec![self.frontend_token];
        if let Some(token) = self.back_token() {
            tokens.push(token)
        }
        tokens
    }
}

pub type HostName = String;
pub type PathBegin = String;

#[derive(thiserror::Error, Debug)]
pub enum ListenerError {
    #[error("failed to acquire the lock, {0}")]
    LockError(String),
    #[error("failed to handle certificate request, got a resolver error, {0}")]
    ResolverError(GenericCertificateResolverError),
    #[error("failed to parse pem, {0}")]
    PemParseError(String),
    #[error("failed to build openssl context, {0}")]
    BuildOpenSslError(String),
    #[error("failed to build rustls context, {0}")]
    BuildRustlsError(String),
}

enum TlsResolverDetails {
    Openssl {
        // we add and remove contexts but we don't make use of them
        contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
        default_context: SslContext,
        _ssl_options: SslOptions,
    },
    Rustls {
        ssl_config: Arc<ServerConfig>,
    },
}

pub struct Listener {
    listener: Option<TcpListener>,
    address: SocketAddr,
    fronts: Router,
    answers: Rc<RefCell<HttpAnswers>>,
    config: HttpsListener,
    resolver: Arc<MutexWrappedCertificateResolver>,
    // resolver: Arc<Mutex<GenericCertificateResolver>>,
    tls_resolver_details: TlsResolverDetails,
    pub token: Token,
    active: bool,
    tags: BTreeMap<String, BTreeMap<String, String>>,
}

impl ListenerHandler for Listener {
    fn get_addr(&self) -> &SocketAddr {
        &self.address
    }

    fn get_tags(&self, key: &str) -> Option<&BTreeMap<String, String>> {
        self.tags.get(key)
    }

    fn set_tags(&mut self, key: String, tags: Option<BTreeMap<String, String>>) {
        match tags {
            Some(tags) => self.tags.insert(key, tags),
            None => self.tags.remove(&key),
        };
    }
}

impl CertificateResolver for Listener {
    type Error = ListenerError;

    fn get_certificate(
        &self,
        fingerprint: &CertificateFingerprint,
    ) -> Option<ParsedCertificateAndKey> {
        let resolver = self
            .resolver
            .0
            .lock()
            .map_err(|err| ListenerError::LockError(err.to_string()))
            .ok()?;

        resolver.get_certificate(fingerprint)
    }

    fn add_certificate(
        &mut self,
        opts: &sozu_command_lib::proxy::AddCertificate,
    ) -> Result<CertificateFingerprint, Self::Error> {
        let fingerprint = {
            let mut resolver = self
                .resolver
                .0
                .lock()
                .map_err(|err| ListenerError::LockError(err.to_string()))?;

            resolver
                .add_certificate(opts)
                .map_err(ListenerError::ResolverError)?
        };

        match &self.tls_resolver_details {
            TlsResolverDetails::Openssl {
                contexts,
                default_context: _,
                _ssl_options,
            } => {
                let certificate = X509::from_pem(opts.certificate.certificate.as_bytes())
                    .map_err(|err| ListenerError::PemParseError(err.to_string()))?;
                let key = PKey::private_key_from_pem(opts.certificate.key.as_bytes())
                    .map_err(|err| ListenerError::PemParseError(err.to_string()))?;

                let mut chain = vec![];
                for certificate in &opts.certificate.certificate_chain {
                    chain.push(
                        X509::from_pem(certificate.as_bytes())
                            .map_err(|err| ListenerError::PemParseError(err.to_string()))?,
                    );
                }

                let versions = if !opts.certificate.versions.is_empty() {
                    &opts.certificate.versions
                } else {
                    &self.config.versions
                };

                let (context, _) = Self::create_openssl_context(
                    versions,
                    &self.config.cipher_list,
                    &self.config.cipher_suites,
                    &self.config.signature_algorithms,
                    &self.config.groups_list,
                    &certificate,
                    &key,
                    &chain,
                    self.resolver.to_owned(),
                    contexts.to_owned(),
                )?;

                let mut contexts = contexts
                    .lock()
                    .map_err(|err| ListenerError::LockError(err.to_string()))?;

                contexts.insert(fingerprint.to_owned(), context);
            }
            _ => {}
        }
        Ok(fingerprint)
    }

    fn remove_certificate(
        &mut self,
        opts: &sozu_command_lib::proxy::RemoveCertificate,
    ) -> Result<(), Self::Error> {
        {
            let mut resolver = self
                .resolver
                .0
                .lock()
                .map_err(|err| ListenerError::LockError(err.to_string()))?;

            resolver
                .remove_certificate(opts)
                .map_err(ListenerError::ResolverError)?;
        }

        if let TlsResolverDetails::Openssl {
            contexts,
            default_context: _,
            _ssl_options,
        } = &self.tls_resolver_details
        {
            let mut contexts = contexts
                .lock()
                .map_err(|err| ListenerError::LockError(err.to_string()))?;

            contexts.remove(&opts.fingerprint);
        }
        Ok(())
    }
}

impl Listener {
    pub fn try_new(
        config: HttpsListener,
        token: Token,
        tls_provider: TlsProvider,
    ) -> Result<Listener, ListenerError> {
        let resolver = Arc::new(MutexWrappedCertificateResolver::new());

        let tls_resolver_details = match tls_provider {
            TlsProvider::Openssl => {
                let contexts = Arc::new(Mutex::new(HashMap::new()));
                let (default_context, _ssl_options): (SslContext, SslOptions) =
                    Self::create_default_openssl_context(
                        &config,
                        resolver.to_owned(),
                        contexts.to_owned(),
                    )?;
                TlsResolverDetails::Openssl {
                    contexts,
                    default_context,
                    _ssl_options,
                }
            }
            TlsProvider::Rustls => {
                let server_config = Self::create_rustls_context(&config, resolver.to_owned())?;
                TlsResolverDetails::Rustls {
                    ssl_config: Arc::new(server_config),
                }
            }
        };

        Ok(Listener {
            listener: None,
            address: config.address,
            resolver,
            tls_resolver_details,
            active: false,
            fronts: Router::new(),
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                &config.answer_404,
                &config.answer_503,
            ))),
            config,
            token,
            tags: BTreeMap::new(),
        })
    }

    pub fn activate(
        &mut self,
        registry: &Registry,
        tcp_listener: Option<TcpListener>,
    ) -> Option<Token> {
        if self.active {
            return Some(self.token);
        }

        let mut listener = tcp_listener.or_else(|| {
            server_bind(self.config.address)
                .map_err(|e| {
                    error!(
                        "could not create listener {:?}: {:?}",
                        self.config.address, e
                    );
                })
                .ok()
        });

        if let Some(ref mut sock) = listener {
            if let Err(e) = registry.register(sock, self.token, Interest::READABLE) {
                error!("cannot register listen socket({:?}): {:?}", sock, e);
            }
        } else {
            return None;
        }

        self.listener = listener;
        self.active = true;
        Some(self.token)
    }

    pub fn create_default_openssl_context(
        config: &HttpsListener,
        resolver: Arc<MutexWrappedCertificateResolver>,
        contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
    ) -> Result<(SslContext, SslOptions), ListenerError> {
        let cert_read = config
            .certificate
            .as_ref()
            .map(|c| c.as_bytes())
            .unwrap_or_else(|| include_bytes!("../assets/certificate.pem"));

        let key_read = config
            .key
            .as_ref()
            .map(|c| c.as_bytes())
            .unwrap_or_else(|| include_bytes!("../assets/key.pem"));

        let mut chain = vec![];
        for certificate in &config.certificate_chain {
            chain.push(
                X509::from_pem(certificate.as_bytes())
                    .map_err(|err| ListenerError::PemParseError(err.to_string()))?,
            );
        }

        let certificate = X509::from_pem(cert_read)
            .map_err(|err| ListenerError::PemParseError(err.to_string()))?;

        let key = PKey::private_key_from_pem(key_read)
            .map_err(|err| ListenerError::PemParseError(err.to_string()))?;

        Self::create_openssl_context(
            &config.versions,
            &config.cipher_list,
            &config.cipher_suites,
            &config.signature_algorithms,
            &config.groups_list,
            &certificate,
            &key,
            &chain[..],
            resolver,
            contexts,
        )
    }

    pub fn create_openssl_context(
        tls_versions: &[TlsVersion],
        cipher_list: &Vec<String>,
        cipher_suites: &Vec<String>,
        signature_algorithms: &Vec<String>,
        groups_list: &Vec<String>,
        cert: &X509,
        key: &PKey<Private>,
        cert_chain: &[X509],
        resolver: Arc<MutexWrappedCertificateResolver>,
        contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
    ) -> Result<(SslContext, SslOptions), ListenerError> {
        let mut context = SslContext::builder(SslMethod::tls())
            .map_err(|err| ListenerError::BuildOpenSslError(err.to_string()))?;

        // FIXME: maybe activate ssl::SslMode::RELEASE_BUFFERS to save some memory?
        let mode = ssl::SslMode::ENABLE_PARTIAL_WRITE | ssl::SslMode::ACCEPT_MOVING_WRITE_BUFFER;

        context.set_mode(mode);

        let mut ssl_options = SslOptions::CIPHER_SERVER_PREFERENCE
            | SslOptions::NO_COMPRESSION
            | SslOptions::NO_TICKET;

        let mut versions = SslOptions::NO_SSLV2
            | SslOptions::NO_SSLV3
            | SslOptions::NO_TLSV1
            | SslOptions::NO_TLSV1_1
            | SslOptions::NO_TLSV1_2;

        // TLSv1.3 is not implemented before OpenSSL v1.1.0
        if cfg!(any(ossl11x, ossl30x)) {
            versions |= SslOptions::NO_TLSV1_3;
        }

        for version in tls_versions.iter() {
            match version {
                TlsVersion::SSLv2 => versions.remove(SslOptions::NO_SSLV2),
                TlsVersion::SSLv3 => versions.remove(SslOptions::NO_SSLV3),
                TlsVersion::TLSv1_0 => versions.remove(SslOptions::NO_TLSV1),
                TlsVersion::TLSv1_1 => versions.remove(SslOptions::NO_TLSV1_1),
                TlsVersion::TLSv1_2 => versions.remove(SslOptions::NO_TLSV1_2),
                #[cfg(any(ossl11x, ossl30x))]
                TlsVersion::TLSv1_3 => versions.remove(SslOptions::NO_TLSV1_3),
                #[cfg(ssl10x)]
                TlsVersion::TLSv1_3 => {}
            };
        }

        ssl_options.insert(versions);
        trace!("parsed tls options: {:?}", ssl_options);

        context.set_options(ssl_options);
        context.set_session_cache_size(1);
        context.set_session_cache_mode(SslSessionCacheMode::OFF);

        if let Err(e) = setup_curves(&mut context) {
            error!("could not setup curves for openssl: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }

        if let Err(e) = setup_dh(&mut context) {
            error!("could not setup DH for openssl: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }

        if let Err(e) = context.set_cipher_list(&cipher_list.join(":")) {
            error!("could not set context cipher list: {:?}", e);
            // verify the cipher_list and tls_provider are compatible
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }

        if cfg!(any(ossl11x, ossl30x)) {
            if let Err(e) = context.set_ciphersuites(&cipher_suites.join(":")) {
                error!("could not set context cipher suites: {:?}", e);
                return Err(ListenerError::BuildOpenSslError(e.to_string()));
            }

            if let Err(e) = context.set_groups_list(&groups_list.join(":")) {
                if !e.errors().is_empty() {
                    error!("could not set context groups list: {:?}", e);
                    return Err(ListenerError::BuildOpenSslError(e.to_string()));
                }
            }
        }

        if cfg!(any(ossl102, ossl11x, ossl30x)) {
            if let Err(e) = context.set_sigalgs_list(&signature_algorithms.join(":")) {
                error!("could not set context signature algorithms: {:?}", e);
                return Err(ListenerError::BuildOpenSslError(e.to_string()));
            }
        }

        if let Err(e) = context.set_certificate(cert) {
            error!("error adding certificate to context: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }

        if let Err(e) = context.set_private_key(key) {
            error!("error adding private key to context: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }

        if let Err(e) = context.set_alpn_protos(SERVER_PROTOS) {
            error!("could not set ALPN protocols: {:?}", e);
            return Err(ListenerError::BuildOpenSslError(e.to_string()));
        }

        context.set_alpn_select_callback(move |_ssl: &mut SslRef, client_protocols: &[u8]| {
            debug!("got protocols list from client: {:?}", unsafe {
                from_utf8_unchecked(client_protocols)
            });
            match select_next_proto(SERVER_PROTOS, client_protocols) {
                None => Err(AlpnError::ALERT_FATAL),
                Some(selected) => {
                    debug!("selected protocol with ALPN: {:?}", unsafe {
                        from_utf8_unchecked(selected)
                    });
                    Ok(selected)
                }
            }
        });

        for cert in cert_chain {
            if let Err(e) = context.add_extra_chain_cert(cert.clone()) {
                error!("error adding chain certificate to context: {:?}", e);
                return Err(ListenerError::BuildOpenSslError(e.to_string()));
            }
        }

        context.set_client_hello_callback(Self::create_client_hello_callback(resolver, contexts));

        Ok((context.build(), ssl_options))
    }

    #[allow(dead_code)]
    fn create_servername_callback(
        resolver: Arc<Mutex<GenericCertificateResolver>>,
        contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
    ) -> impl Fn(&mut SslRef, &mut SslAlert) -> Result<(), SniError> + 'static + Sync + Send {
        move |ssl: &mut SslRef, alert: &mut SslAlert| {
            let resolver = unwrap_msg!(resolver.lock());
            let contexts = unwrap_msg!(contexts.lock());

            trace!("ref: {:?}", ssl);
            if let Some(servername) = ssl.servername(NameType::HOST_NAME).map(|s| s.to_string()) {
                debug!("looking for fingerprint for {:?}", servername);
                if let Some(kv) = resolver.domain_lookup(servername.as_bytes(), true) {
                    debug!(
                        "looking for context for {:?} with fingerprint {:?}",
                        servername, kv.1
                    );
                    if let Some(context) = contexts.get(&kv.1) {
                        debug!("found context for {:?}", servername);

                        if let Ok(()) = ssl.set_ssl_context(context) {
                            debug!(
                                "servername is now {:?}",
                                ssl.servername(NameType::HOST_NAME)
                            );

                            return Ok(());
                        } else {
                            error!("could not set context for {:?}", servername);
                        }
                    } else {
                        error!("no context found for {:?}", servername);
                    }
                } else {
                    //error!("unrecognized server name: {}", servername);
                }
            } else {
                //error!("no server name information found");
            }

            incr!("openssl.sni.error");
            *alert = SslAlert::UNRECOGNIZED_NAME;
            Err(SniError::ALERT_FATAL)
        }
    }

    fn create_client_hello_callback(
        resolver: Arc<MutexWrappedCertificateResolver>,
        contexts: Arc<Mutex<HashMap<CertificateFingerprint, SslContext>>>,
    ) -> impl Fn(&mut SslRef, &mut SslAlert) -> Result<ssl::ClientHelloResponse, ErrorStack>
           + 'static
           + Sync
           + Send {
        move |ssl: &mut SslRef, alert: &mut SslAlert| {
            debug!(
                "client hello callback: TLS version = {:?}",
                ssl.version_str()
            );

            let server_name_opt = get_server_name(ssl).and_then(parse_sni_name_list);
            // no SNI extension, use the default context
            if server_name_opt.is_none() {
                *alert = SslAlert::UNRECOGNIZED_NAME;
                return Ok(ssl::ClientHelloResponse::SUCCESS);
            }

            let mut sni_names = server_name_opt.unwrap();
            let resolver = unwrap_msg!(resolver.0.lock());
            let contexts = unwrap_msg!(contexts.lock());

            while !sni_names.is_empty() {
                debug!("name list:\n{}", sni_names.to_hex(16));
                match parse_sni_name(sni_names) {
                    None => break,
                    Some((i, servername)) => {
                        debug!("parsed name:\n{}", servername.to_hex(16));
                        sni_names = i;

                        if let Some(kv) = resolver.domain_lookup(servername, true) {
                            debug!(
                                "looking for context for {:?} with fingerprint {:?}",
                                servername, kv.1
                            );
                            if let Some(context) = contexts.get(&kv.1) {
                                debug!("found context for {:?}", servername);

                                if let Ok(()) = ssl.set_ssl_context(context) {
                                    ssl_set_options(ssl, context);

                                    return Ok(ssl::ClientHelloResponse::SUCCESS);
                                } else {
                                    error!("could not set context for {:?}", servername);
                                }
                            } else {
                                error!("no context found for {:?}", servername);
                            }
                        } else {
                            error!(
                                "unrecognized server name: {:?}",
                                std::str::from_utf8(servername)
                            );
                        }
                    }
                }
            }

            *alert = SslAlert::UNRECOGNIZED_NAME;
            Ok::<ssl::ClientHelloResponse, ErrorStack>(ssl::ClientHelloResponse::SUCCESS)
        }
    }

    pub fn create_rustls_context(
        config: &HttpsListener,
        resolver: Arc<MutexWrappedCertificateResolver>,
    ) -> Result<ServerConfig, ListenerError> {
        let server_config = ServerConfig::builder();
        let server_config = if !config.cipher_list.is_empty() {
            let mut ciphers = Vec::new();
            for cipher in config.cipher_list.iter() {
                match cipher.as_str() {
                    "TLS13_CHACHA20_POLY1305_SHA256" => {
                        ciphers.push(TLS13_CHACHA20_POLY1305_SHA256)
                    }
                    "TLS13_AES_256_GCM_SHA384" => ciphers.push(TLS13_AES_256_GCM_SHA384),
                    "TLS13_AES_128_GCM_SHA256" => ciphers.push(TLS13_AES_128_GCM_SHA256),
                    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => {
                        ciphers.push(TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256)
                    }
                    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => {
                        ciphers.push(TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256)
                    }
                    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384" => {
                        ciphers.push(TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384)
                    }
                    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => {
                        ciphers.push(TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256)
                    }
                    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" => {
                        ciphers.push(TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384)
                    }
                    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => {
                        ciphers.push(TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256)
                    }
                    s => error!("unknown cipher: {:?}", s),
                }
            }
            server_config.with_cipher_suites(&ciphers[..])
        } else {
            server_config.with_safe_default_cipher_suites()
        };
        let server_config = server_config.with_safe_default_kx_groups();
        let mut versions = Vec::new();
        for version in config.versions.iter() {
            match version {
                TlsVersion::TLSv1_2 => versions.push(&rustls::version::TLS12),
                TlsVersion::TLSv1_3 => versions.push(&rustls::version::TLS13),
                s => error!("unsupported TLS version: {:?}", s),
            }
        }
        let server_config = server_config
            .with_protocol_versions(&versions[..])
            .map_err(|err| ListenerError::BuildRustlsError(err.to_string()))?;
        let server_config = server_config.with_no_client_auth();
        Ok(server_config.with_cert_resolver(resolver))
    }

    pub fn add_https_front(&mut self, tls_front: HttpFrontend) -> bool {
        self.fronts.add_http_front(tls_front)
    }

    pub fn remove_https_front(&mut self, tls_front: HttpFrontend) -> bool {
        debug!("removing tls_front {:?}", tls_front);
        self.fronts.remove_http_front(tls_front)
    }

    // ToDo factor out with http.rs
    pub fn frontend_from_request(&self, host: &str, uri: &str, method: &Method) -> Option<Route> {
        let host: &str = if let Ok((i, (hostname, _))) = hostname_and_port(host.as_bytes()) {
            if i != &b""[..] {
                error!(
                    "frontend_from_request: invalid remaining chars after hostname. Host: {}",
                    host
                );
                return None;
            }

            // it is alright to call from_utf8_unchecked,
            // we already verified that there are only ascii
            // chars in there
            unsafe { from_utf8_unchecked(hostname) }
        } else {
            error!("hostname parsing failed for: '{}'", host);
            return None;
        };

        self.fronts.lookup(host.as_bytes(), uri.as_bytes(), method)
    }

    fn accept(&mut self) -> Result<TcpStream, AcceptError> {
        if let Some(ref sock) = self.listener {
            sock.accept()
                .map_err(|e| match e.kind() {
                    ErrorKind::WouldBlock => AcceptError::WouldBlock,
                    _ => {
                        error!("accept() IO error: {:?}", e);
                        AcceptError::IoError
                    }
                })
                .map(|(sock, _)| sock)
        } else {
            error!("cannot accept connections, no listening socket available");
            Err(AcceptError::IoError)
        }
    }
}

pub struct Proxy {
    listeners: HashMap<Token, Rc<RefCell<Listener>>>,
    clusters: HashMap<ClusterId, Cluster>,
    backends: Rc<RefCell<BackendMap>>,
    pool: Rc<RefCell<Pool>>,
    registry: Registry,
    sessions: Rc<RefCell<SessionManager>>,
    tls_provider: TlsProvider,
}

impl Proxy {
    pub fn new(
        registry: Registry,
        sessions: Rc<RefCell<SessionManager>>,
        pool: Rc<RefCell<Pool>>,
        backends: Rc<RefCell<BackendMap>>,
        tls_provider: TlsProvider,
    ) -> Proxy {
        Proxy {
            listeners: HashMap::new(),
            clusters: HashMap::new(),
            backends,
            pool,
            registry,
            sessions,
            tls_provider,
        }
    }

    pub fn add_listener(&mut self, config: HttpsListener, token: Token) -> Option<Token> {
        match self.listeners.entry(token) {
            Entry::Vacant(entry) => {
                entry.insert(Rc::new(RefCell::new(
                    Listener::try_new(config, token, self.tls_provider).ok()?,
                )));
                Some(token)
            }
            _ => None,
        }
    }

    pub fn remove_listener(&mut self, address: SocketAddr) -> bool {
        let len = self.listeners.len();

        self.listeners
            .retain(|_, listener| listener.borrow().address != address);
        self.listeners.len() < len
    }

    pub fn activate_listener(
        &mut self,
        addr: &SocketAddr,
        tcp_listener: Option<TcpListener>,
    ) -> Option<Token> {
        self.listeners
            .values()
            .find(|listener| listener.borrow().address == *addr)
            .and_then(|listener| listener.borrow_mut().activate(&self.registry, tcp_listener))
    }

    pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
        self.listeners
            .values()
            .filter_map(|listener| {
                let mut owned = listener.borrow_mut();
                if let Some(listener) = owned.listener.take() {
                    return Some((owned.address, listener));
                }

                None
            })
            .collect()
    }

    pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, TcpListener)> {
        self.listeners
            .values()
            .find(|listener| listener.borrow().address == address)
            .and_then(|listener| {
                let mut owned = listener.borrow_mut();

                owned
                    .listener
                    .take()
                    .map(|listener| (owned.token, listener))
            })
    }

    pub fn add_cluster(&mut self, mut cluster: Cluster) {
        if let Some(answer_503) = cluster.answer_503.take() {
            for listener in self.listeners.values() {
                listener
                    .borrow()
                    .answers
                    .borrow_mut()
                    .add_custom_answer(&cluster.cluster_id, &answer_503);
            }
        }

        self.clusters.insert(cluster.cluster_id.clone(), cluster);
    }

    pub fn remove_cluster(&mut self, cluster_id: &str) {
        self.clusters.remove(cluster_id);
        for listener in self.listeners.values() {
            listener
                .borrow()
                .answers
                .borrow_mut()
                .remove_custom_answer(cluster_id);
        }
    }
}

impl ProxyConfiguration<Session> for Proxy {
    fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
        match self.listeners.get(&Token(token.0)) {
            Some(listener) => listener.borrow_mut().accept(),
            None => Err(AcceptError::IoError),
        }
    }

    fn create_session(
        &mut self,
        mut frontend_sock: TcpStream,
        token: ListenToken,
        wait_time: Duration,
        proxy: Rc<RefCell<Self>>,
    ) -> Result<(), AcceptError> {
        let listener = self
            .listeners
            .get(&Token(token.0))
            .ok_or_else(|| AcceptError::IoError)?;
        if let Err(e) = frontend_sock.set_nodelay(true) {
            error!(
                "error setting nodelay on front socket({:?}): {:?}",
                frontend_sock, e
            );
        }

        let owned = listener.borrow();
        let tls_details = match &owned.tls_resolver_details {
            TlsResolverDetails::Openssl {
                contexts: _,
                default_context,
                _ssl_options,
            } => {
                let ssl = Ssl::new(&default_context).map_err(|ssl_creation_error| {
                    error!("could not create ssl context: {}", ssl_creation_error);
                    AcceptError::IoError
                })?;
                TlsDetails::Openssl(ssl)
            }
            TlsResolverDetails::Rustls { ssl_config } => {
                let session = ServerConnection::new(ssl_config.clone()).map_err(|e| {
                    error!("failed to create server session: {:?}", e);
                    AcceptError::IoError
                })?;
                TlsDetails::Rustls(session)
            }
        };

        let mut session_manager = self.sessions.borrow_mut();
        let entry = session_manager.slab.vacant_entry();
        let session_token = Token(entry.key());

        self.registry
            .register(
                &mut frontend_sock,
                session_token,
                Interest::READABLE | Interest::WRITABLE,
            )
            .map_err(|register_error| {
                error!(
                    "error registering front socket({:?}): {:?}",
                    frontend_sock, register_error
                );
                AcceptError::RegisterError
            })?;

        let session = Rc::new(RefCell::new(Session::new(
            tls_details,
            frontend_sock,
            session_token,
            Rc::downgrade(&self.pool),
            proxy,
            owned.config.public_address.unwrap_or(owned.config.address),
            owned.config.expect_proxy,
            owned.config.sticky_name.clone(),
            owned.answers.clone(),
            Token(token.0),
            wait_time,
            Duration::seconds(owned.config.front_timeout as i64),
            Duration::seconds(owned.config.back_timeout as i64),
            Duration::seconds(owned.config.request_timeout as i64),
            listener.clone(),
        )));
        entry.insert(session);

        session_manager.incr();
        Ok(())
    }

    fn notify(&mut self, message: ProxyRequest) -> ProxyResponse {
        //trace!("{} notified", message);
        match message.order {
            ProxyRequestOrder::AddCluster(cluster) => {
                debug!("{} add cluster {:?}", message.id, cluster);
                self.add_cluster(cluster);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::RemoveCluster { cluster_id } => {
                debug!("{} remove cluster {:?}", message.id, cluster_id);
                self.remove_cluster(&cluster_id);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::AddHttpsFrontend(front) => {
                //info!("HTTPS\t{} add front {:?}", id, front);
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == front.address)
                {
                    let mut owned = listener.borrow_mut();
                    owned.set_tags(front.hostname.to_owned(), front.tags.to_owned());
                    owned.add_https_front(front);
                    ProxyResponse::ok(message.id)
                } else {
                    ProxyResponse::error(
                        message.id,
                        format!("adding front {:?} to unknown listener", front),
                    )
                }
            }
            ProxyRequestOrder::RemoveHttpsFrontend(front) => {
                //info!("HTTPS\t{} remove front {:?}", id, front);
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow_mut().address == front.address)
                {
                    let mut owned = listener.borrow_mut();
                    owned.set_tags(front.hostname.to_owned(), None);
                    owned.remove_https_front(front);
                    ProxyResponse::ok(message.id)
                } else {
                    ProxyResponse::error(
                        message.id,
                        format!("No listener found for the frontend {:?}", front),
                    )
                }
            }
            ProxyRequestOrder::AddCertificate(add_certificate) => {
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == add_certificate.address)
                {
                    //info!("HTTPS\t{} add certificate: {:?}", id, certificate_and_key);
                    match listener.borrow_mut().add_certificate(&add_certificate) {
                        Ok(_) => ProxyResponse::ok(message.id),
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    error!("adding certificate to unknown listener");
                    ProxyResponse::error(message.id, "unsupported message")
                }
            }
            ProxyRequestOrder::RemoveCertificate(remove_certificate) => {
                //FIXME: should return an error if certificate still has fronts referencing it
                if let Some(listener) = self
                    .listeners
                    .values()
                    .find(|l| l.borrow().address == remove_certificate.address)
                {
                    match listener
                        .borrow_mut()
                        .remove_certificate(&remove_certificate)
                    {
                        Ok(_) => ProxyResponse::ok(message.id),
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    error!("removing certificate to unknown listener");
                    ProxyResponse::error(message.id, "unsupported message")
                }
            }
            ProxyRequestOrder::ReplaceCertificate(replace) => {
                //FIXME: should return an error if certificate still has fronts referencing it
                for listener in &self.listeners {
                    error!("LISTENER: {:?}", listener.1.borrow().address);
                }
                if let Some(listener) = self
                    .listeners
                    .values_mut()
                    .find(|l| l.borrow().address == replace.address)
                {
                    match listener.borrow_mut().replace_certificate(&replace) {
                        Ok(_) => ProxyResponse::ok(message.id),
                        Err(err) => ProxyResponse::error(message.id, err),
                    }
                } else {
                    error!("replacing certificate to unknown listener");
                    ProxyResponse::error(message.id, "unsupported message")
                }
            }
            ProxyRequestOrder::RemoveListener(remove) => {
                debug!("removing HTTPS listener at address {:?}", remove.address);
                if !self.remove_listener(remove.address) {
                    ProxyResponse::error(
                        message.id,
                        format!(
                            "no HTTPS listener to remove at address {:?}",
                            remove.address
                        ),
                    )
                } else {
                    ProxyResponse::ok(message.id)
                }
            }
            ProxyRequestOrder::SoftStop => {
                info!("{} processing soft shutdown", message.id);
                let listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, listener) in listeners.iter() {
                    listener.borrow_mut().listener.take().map(|mut sock| {
                        if let Err(e) = self.registry.deregister(&mut sock) {
                            error!("error deregistering listen socket({:?}): {:?}", sock, e);
                        }
                    });
                }
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::HardStop => {
                info!("{} hard shutdown", message.id);
                let listeners: HashMap<_, _> = self.listeners.drain().collect();
                for (_, listener) in listeners.iter() {
                    listener.borrow_mut().listener.take().map(|mut sock| {
                        if let Err(e) = self.registry.deregister(&mut sock) {
                            error!("error dereginstering listen socket({:?}): {:?}", sock, e);
                        }
                    });
                }
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::Status => {
                debug!("{} status", message.id);
                ProxyResponse::ok(message.id)
            }
            ProxyRequestOrder::Logging(logging_filter) => {
                debug!(
                    "{} changing logging filter to {}",
                    message.id, logging_filter
                );
                logging::LOGGER.with(|l| {
                    let directives = logging::parse_logging_spec(&logging_filter);
                    l.borrow_mut().set_directives(directives);
                });
                ProxyResponse::processing(message.id)
            }
            ProxyRequestOrder::Query(Query::Certificates(QueryCertificateType::All)) => {
                let res = self
                    .listeners
                    .values()
                    .map(|listener| {
                        let owned = listener.borrow();
                        let resolver = unwrap_msg!(owned.resolver.0.lock());
                        let res = resolver
                            .domains
                            .to_hashmap()
                            .drain()
                            .map(|(k, v)| (String::from_utf8(k).unwrap(), v.0))
                            .collect();

                        (owned.address, res)
                    })
                    .collect::<HashMap<_, _>>();

                info!("got Certificates::All query, answering with {:?}", res);

                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    content: Some(ProxyResponseContent::Query(QueryAnswer::Certificates(
                        QueryAnswerCertificate::All(res),
                    ))),
                }
            }
            ProxyRequestOrder::Query(Query::Certificates(QueryCertificateType::Domain(d))) => {
                let res = self
                    .listeners
                    .values()
                    .map(|listener| {
                        let owned = listener.borrow();
                        let resolver = unwrap_msg!(owned.resolver.0.lock());
                        (
                            owned.address,
                            resolver.domain_lookup(d.as_bytes(), true).map(|(k, v)| {
                                (String::from_utf8(k.to_vec()).unwrap(), v.0.clone())
                            }),
                        )
                    })
                    .collect::<HashMap<_, _>>();

                info!(
                    "got Certificates::Domain({}) query, answering with {:?}",
                    d, res
                );

                ProxyResponse {
                    id: message.id,
                    status: ProxyResponseStatus::Ok,
                    content: Some(ProxyResponseContent::Query(QueryAnswer::Certificates(
                        QueryAnswerCertificate::Domain(res),
                    ))),
                }
            }
            command => {
                error!(
                    "{} unsupported message for OpenSSL proxy, ignoring {:?}",
                    message.id, command
                );
                ProxyResponse::error(message.id, "unsupported message")
            }
        }
    }
}

#[cfg(ossl102)]
fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    match Dh::get_2048_256() {
        Ok(dh) => ctx.set_tmp_dh(&dh),
        Err(e) => return Err(e),
    };
    ctx.set_ecdh_auto(true)
}

#[cfg(any(ossl101, ossl11x, ossl30x))]
fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    use openssl::ec::EcKey;

    let curve = EcKey::from_curve_name(nid::Nid::X9_62_PRIME256V1)?;
    ctx.set_tmp_ecdh(&curve)
}

#[cfg(all(not(ossl101), not(ossl102), not(ossl11x), not(ossl30x)))]
fn setup_curves(_: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    compile_error!("unsupported openssl version, please open an issue");
    Ok(())
}

fn setup_dh(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
    let dh = Dh::params_from_pem(
        b"
-----BEGIN DH PARAMETERS-----
MIIBCAKCAQEAm7EQIiluUX20jnC3NGhikNVGH7MSlRIgTpkUlCyWrmlJci+Pu54Q
YzOZw3tw4g84+ipTzpdGuUvZxNd4Y4ZUp/XsdZgnWMfV+y9OuyYAKWGPTtEO/HUy
6buhvi5qahIXdxUm0dReFuOTrDOphNjHXggU8Iwey3U54aL+KVqLMAP+Tev8jrQN
A0mv4X8dmdvCv7LEZPVYFJb3Uprwd60ThSl3NKNTMDnwnRtXpIrSZIZcJh5IER7S
/IexUuBsopcTVIcHfLyaECIbMKnZfyrkAJfHqRWEpPOzk1euVw0hw3SkDJREfB21
yD0TrUjkXyjV/zczIYiYSROg9OE5UgYqswIBAg==
-----END DH PARAMETERS-----
",
    )?;
    ctx.set_tmp_dh(&dh)
}

fn openssl_version_str(version: SslVersion) -> &'static str {
    match version {
        SslVersion::SSL3 => "tls.version.SSLv3",
        SslVersion::TLS1 => "tls.version.TLSv1_0",
        SslVersion::TLS1_1 => "tls.version.TLSv1_1",
        SslVersion::TLS1_2 => "tls.version.TLSv1_2",
        //SslVersion::TLS1_3 => "tls.version.TLSv1_3",
        _ => "tls.version.TLSv1_3",
        //_ => "tls.version.Unknown",
    }
}

fn rustls_version_str(version: ProtocolVersion) -> &'static str {
    match version {
        ProtocolVersion::SSLv2 => "tls.version.SSLv2",
        ProtocolVersion::SSLv3 => "tls.version.SSLv3",
        ProtocolVersion::TLSv1_0 => "tls.version.TLSv1_0",
        ProtocolVersion::TLSv1_1 => "tls.version.TLSv1_1",
        ProtocolVersion::TLSv1_2 => "tls.version.TLSv1_2",
        ProtocolVersion::TLSv1_3 => "tls.version.TLSv1_3",
        ProtocolVersion::DTLSv1_0 => "tls.version.DTLSv1_0",
        ProtocolVersion::DTLSv1_2 => "tls.version.DTLSv1_2",
        ProtocolVersion::DTLSv1_3 => "tls.version.DTLSv1_3",
        ProtocolVersion::Unknown(_) => "tls.version.Unknown",
    }
}

fn rustls_ciphersuite_str(cipher: SupportedCipherSuite) -> &'static str {
    match cipher.suite() {
        CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 => {
            "tls.cipher.TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256"
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => {
            "tls.cipher.TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256"
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => {
            "tls.cipher.TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"
        }
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 => {
            "tls.cipher.TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384"
        }
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 => {
            "tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256"
        }
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 => {
            "tls.cipher.TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384"
        }
        CipherSuite::TLS13_CHACHA20_POLY1305_SHA256 => "tls.cipher.TLS13_CHACHA20_POLY1305_SHA256",
        CipherSuite::TLS13_AES_256_GCM_SHA384 => "tls.cipher.TLS13_AES_256_GCM_SHA384",
        CipherSuite::TLS13_AES_128_GCM_SHA256 => "tls.cipher.TLS13_AES_128_GCM_SHA256",
        _ => "tls.cipher.Unsupported",
    }
}

/// this function is not used, but is available for example and testing purposes
pub fn start(
    config: HttpsListener,
    channel: ProxyChannel,
    max_buffers: usize,
    buffer_size: usize,
    tls_provider: TlsProvider,
) -> anyhow::Result<()> {
    use crate::server;

    let event_loop = Poll::new().with_context(|| "could not create event loop")?;

    let pool = Rc::new(RefCell::new(Pool::with_capacity(
        1,
        max_buffers,
        buffer_size,
    )));
    let backends = Rc::new(RefCell::new(BackendMap::new()));

    let mut sessions: Slab<Rc<RefCell<dyn ProxySession>>> = Slab::with_capacity(max_buffers);
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for channel", SessionToken(entry.key()));
        entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
    }
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for timer", SessionToken(entry.key()));
        entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
    }
    {
        let entry = sessions.vacant_entry();
        info!("taking token {:?} for metrics", SessionToken(entry.key()));
        entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
    }

    let token = {
        let entry = sessions.vacant_entry();
        let key = entry.key();
        let _e = entry.insert(Rc::new(RefCell::new(ListenSession {
            protocol: Protocol::HTTPListen,
        })));
        Token(key)
    };

    let sessions = SessionManager::new(sessions, max_buffers);
    let registry = event_loop
        .registry()
        .try_clone()
        .with_context(|| "Failed at creating a registry")?;
    let mut proxy = Proxy::new(
        registry,
        sessions.clone(),
        pool.clone(),
        backends.clone(),
        tls_provider,
    );
    let address = config.address;
    if proxy.add_listener(config, token).is_some()
        && proxy.activate_listener(&address, None).is_some()
    {
        let (scm_server, _scm_client) =
            UnixStream::pair().with_context(|| "Failed at creating scm stream sockets")?;
        let mut server_config: server::ServerConfig = Default::default();
        server_config.max_connections = max_buffers;
        let mut server = Server::new(
            event_loop,
            channel,
            ScmSocket::new(scm_server.as_raw_fd()),
            sessions,
            pool,
            backends,
            None,
            Some(proxy),
            None,
            server_config,
            None,
            false,
            tls_provider,
        )
        .with_context(|| "Failed to create server")?;

        info!("starting event loop");
        server.run();
        info!("ending event loop");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate tiny_http;
    use super::*;
    use crate::router::{trie::TrieNode, MethodRule, PathRule, Router};
    use crate::sozu_command::proxy::Route;
    use openssl::ssl::{SslContext, SslMethod};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::rc::Rc;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    /*
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn size_test() {
      assert_size!(ExpectProxyProtocol<mio::net::TcpStream>, 520);
      assert_size!(TlsHandshake, 240);
      assert_size!(Http<SslStream<mio::net::TcpStream>>, 1232);
      assert_size!(Pipe<SslStream<mio::net::TcpStream>>, 272);
      assert_size!(State, 1240);
      // fails depending on the platform?
      assert_size!(Session, 1672);

      assert_size!(SslStream<mio::net::TcpStream>, 16);
      assert_size!(Ssl, 8);
    }
    */

    #[test]
    fn frontend_from_request_test_openssl() {
        frontend_from_request_test(TlsProvider::Openssl);
    }
    #[test]
    fn frontend_from_request_test_rustls() {
        frontend_from_request_test(TlsProvider::Rustls);
    }
    fn frontend_from_request_test(tls_provider: TlsProvider) {
        let cluster_id1 = "cluster_1".to_owned();
        let cluster_id2 = "cluster_2".to_owned();
        let cluster_id3 = "cluster_3".to_owned();
        let uri1 = "/".to_owned();
        let uri2 = "/yolo".to_owned();
        let uri3 = "/yolo/swag".to_owned();

        let mut fronts = Router::new();
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            PathRule::Prefix(uri1),
            MethodRule::new(None),
            Route::ClusterId(cluster_id1.clone())
        ));
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            PathRule::Prefix(uri2),
            MethodRule::new(None),
            Route::ClusterId(cluster_id2)
        ));
        assert!(fronts.add_tree_rule(
            "lolcatho.st".as_bytes(),
            PathRule::Prefix(uri3),
            MethodRule::new(None),
            Route::ClusterId(cluster_id3)
        ));
        assert!(fronts.add_tree_rule(
            "other.domain".as_bytes(),
            PathRule::Prefix("test".to_string()),
            MethodRule::new(None),
            Route::ClusterId(cluster_id1)
        ));

        let address: SocketAddr = FromStr::from_str("127.0.0.1:1032")
            .expect("test address 127.0.0.1:1032 should be parsed");
        let resolver = Arc::new(MutexWrappedCertificateResolver::new());

        let tls_resolver_details = match tls_provider {
            TlsProvider::Openssl => {
                let context = SslContext::builder(SslMethod::tls())
                    .expect("could not create a SslContextBuilder");

                TlsResolverDetails::Openssl {
                    contexts: Arc::new(Mutex::new(HashMap::new())),
                    default_context: context.build(),
                    _ssl_options: SslOptions::CIPHER_SERVER_PREFERENCE
                        | SslOptions::NO_COMPRESSION
                        | SslOptions::NO_TICKET
                        | SslOptions::NO_SSLV2
                        | SslOptions::NO_SSLV3
                        | SslOptions::NO_TLSV1
                        | SslOptions::NO_TLSV1_1,
                }
            }
            TlsProvider::Rustls => {
                let server_config = ServerConfig::builder()
                    .with_safe_default_cipher_suites()
                    .with_safe_default_kx_groups()
                    .with_protocol_versions(&[])
                    .map_err(|err| ListenerError::BuildRustlsError(err.to_string()))
                    .expect("could not create Rustls server config")
                    .with_no_client_auth()
                    .with_cert_resolver(resolver.clone());

                TlsResolverDetails::Rustls {
                    ssl_config: Arc::new(server_config),
                }
            }
        };
        let listener = Listener {
            listener: None,
            address,
            fronts,
            tls_resolver_details,
            resolver,
            answers: Rc::new(RefCell::new(HttpAnswers::new(
                "HTTP/1.1 404 Not Found\r\n\r\n",
                "HTTP/1.1 503 Service Unavailable\r\n\r\n",
            ))),
            config: Default::default(),
            token: Token(0),
            active: true,
            tags: BTreeMap::new(),
        };

        println!("TEST {}", line!());
        let frontend1 = listener.frontend_from_request("lolcatho.st", "/", &Method::Get);
        assert_eq!(
            frontend1.expect("should find a frontend"),
            Route::ClusterId("cluster_1".to_string())
        );
        println!("TEST {}", line!());
        let frontend2 = listener.frontend_from_request("lolcatho.st", "/test", &Method::Get);
        assert_eq!(
            frontend2.expect("should find a frontend"),
            Route::ClusterId("cluster_1".to_string())
        );
        println!("TEST {}", line!());
        let frontend3 = listener.frontend_from_request("lolcatho.st", "/yolo/test", &Method::Get);
        assert_eq!(
            frontend3.expect("should find a frontend"),
            Route::ClusterId("cluster_2".to_string())
        );
        println!("TEST {}", line!());
        let frontend4 = listener.frontend_from_request("lolcatho.st", "/yolo/swag", &Method::Get);
        assert_eq!(
            frontend4.expect("should find a frontend"),
            Route::ClusterId("cluster_3".to_string())
        );
        println!("TEST {}", line!());
        let frontend5 = listener.frontend_from_request("domain", "/", &Method::Get);
        assert_eq!(frontend5, None);
        // assert!(false);
    }

    #[test]
    fn wildcard_certificate_names() {
        let mut trie = TrieNode::root();

        trie.domain_insert("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8);
        trie.domain_insert("*.clever-cloud.com".as_bytes().to_vec(), 2u8);
        trie.domain_insert("services.clever-cloud.com".as_bytes().to_vec(), 0u8);
        trie.domain_insert(
            "abprefix.services.clever-cloud.com".as_bytes().to_vec(),
            3u8,
        );
        trie.domain_insert(
            "cdprefix.services.clever-cloud.com".as_bytes().to_vec(),
            4u8,
        );

        let res = trie.domain_lookup(b"test.services.clever-cloud.com", true);
        println!("query result: {:?}", res);

        assert_eq!(
            trie.domain_lookup(b"pgstudio.services.clever-cloud.com", true),
            Some(&("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8))
        );
        assert_eq!(
            trie.domain_lookup(b"test-prefix.services.clever-cloud.com", true),
            Some(&("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8))
        );
    }

    #[test]
    fn wildcard_with_subdomains() {
        let mut trie = TrieNode::root();

        trie.domain_insert("*.test.example.com".as_bytes().to_vec(), 1u8);
        trie.domain_insert("hello.sub.test.example.com".as_bytes().to_vec(), 2u8);

        let res = trie.domain_lookup(b"sub.test.example.com", true);
        println!("query result: {:?}", res);

        assert_eq!(
            trie.domain_lookup(b"sub.test.example.com", true),
            Some(&("*.test.example.com".as_bytes().to_vec(), 1u8))
        );
        assert_eq!(
            trie.domain_lookup(b"hello.sub.test.example.com", true),
            Some(&("hello.sub.test.example.com".as_bytes().to_vec(), 2u8))
        );

        // now try in a different order
        let mut trie = TrieNode::root();

        trie.domain_insert("hello.sub.test.example.com".as_bytes().to_vec(), 2u8);
        trie.domain_insert("*.test.example.com".as_bytes().to_vec(), 1u8);

        let res = trie.domain_lookup(b"sub.test.example.com", true);
        println!("query result: {:?}", res);

        assert_eq!(
            trie.domain_lookup(b"sub.test.example.com", true),
            Some(&("*.test.example.com".as_bytes().to_vec(), 1u8))
        );
        assert_eq!(
            trie.domain_lookup(b"hello.sub.test.example.com", true),
            Some(&("hello.sub.test.example.com".as_bytes().to_vec(), 2u8))
        );
    }
}

extern "C" {
    pub fn SSL_set_options(ssl: *mut openssl_sys::SSL, op: libc::c_ulong) -> libc::c_ulong;
}

fn get_server_name<'a, 'b>(ssl: &'a SslRef) -> Option<&'b [u8]> {
    unsafe {
        let mut out: *const libc::c_uchar = std::ptr::null();
        let mut outlen: libc::size_t = 0;

        let res = openssl_sys::SSL_client_hello_get0_ext(
            ssl.as_ptr(),
            0, // TLSEXT_TYPE_server_name = 0
            &mut out as *mut _,
            &mut outlen as *mut _,
        );

        if res == 0 {
            return None;
        }

        let sl = std::slice::from_raw_parts(out, outlen);

        Some(sl)
    }
}

fn ssl_set_options(ssl: &mut SslRef, context: &SslContext) {
    unsafe {
        let options = openssl_sys::SSL_CTX_get_options(context.as_ptr());
        SSL_set_options(ssl.as_ptr(), options);
    }
}

fn parse_sni_name_list(i: &[u8]) -> Option<&[u8]> {
    use nom::{multi::length_data, number::complete::be_u16};

    if i.is_empty() {
        return None;
    }

    match length_data::<_, _, (), _>(be_u16)(i).ok() {
        None => None,
        Some((i, o)) => {
            if !i.is_empty() {
                None
            } else {
                Some(o)
            }
        }
    }
}

fn parse_sni_name(i: &[u8]) -> Option<(&[u8], &[u8])> {
    use nom::{
        bytes::complete::tag, multi::length_data, number::complete::be_u16, sequence::preceded,
    };

    if i.is_empty() {
        return None;
    }

    preceded::<_, _, _, (), _, _>(
        tag([0x00]), // SNIType for hostname is 0
        length_data(be_u16),
    )(i)
    .ok()
}
