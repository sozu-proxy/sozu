use std::{
    io::{self, ErrorKind, Read, Write},
    net::SocketAddr,
};

use mio::net::{TcpListener, TcpStream};
#[cfg(feature = "use-openssl")]
use openssl::ssl::{ErrorCode, SslStream, SslVersion};
use rustls::{ProtocolVersion, ServerConnection};
use socket2::{Domain, Protocol, Socket, Type};

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum SocketResult {
    Continue,
    Closed,
    WouldBlock,
    Error,
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum TransportProtocol {
    Tcp,
    Ssl2,
    Ssl3,
    Tls1_0,
    Tls1_1,
    Tls1_2,
    Tls1_3,
}

pub trait SocketHandler {
    fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult);
    fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult);
    fn socket_write_vectored(&mut self, _buf: &[std::io::IoSlice]) -> (usize, SocketResult) {
        unimplemented!()
    }
    fn has_vectored_writes(&self) -> bool {
        false
    }
    fn socket_ref(&self) -> &TcpStream;
    fn socket_mut(&mut self) -> &mut TcpStream;
    fn protocol(&self) -> TransportProtocol;
    fn read_error(&self);
    fn write_error(&self);
}

impl SocketHandler for TcpStream {
    fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
        let mut size = 0usize;
        loop {
            if size == buf.len() {
                return (size, SocketResult::Continue);
            }
            match self.read(&mut buf[size..]) {
                Ok(0) => return (size, SocketResult::Closed),
                Ok(sz) => size += sz,
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => return (size, SocketResult::WouldBlock),
                    ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe => return (size, SocketResult::Closed),
                    _ => {
                        error!("SOCKET\tsocket_read error={:?}", e);
                        return (size, SocketResult::Error);
                    }
                },
            }
        }
    }

    fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
        let mut size = 0usize;
        loop {
            if size == buf.len() {
                return (size, SocketResult::Continue);
            }
            match self.write(&buf[size..]) {
                Ok(0) => return (size, SocketResult::Continue),
                Ok(sz) => size += sz,
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => return (size, SocketResult::WouldBlock),
                    ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::ConnectionRefused => {
                        incr!("tcp.write.error");
                        return (size, SocketResult::Closed);
                    }
                    _ => {
                        //FIXME: timeout and other common errors should be sent up
                        error!("SOCKET\tsocket_write error={:?}", e);
                        incr!("tcp.write.error");
                        return (size, SocketResult::Error);
                    }
                },
            }
        }
    }

    fn socket_write_vectored(&mut self, bufs: &[std::io::IoSlice]) -> (usize, SocketResult) {
        match self.write_vectored(bufs) {
            Ok(sz) => (sz, SocketResult::Continue),
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock => (0, SocketResult::WouldBlock),
                ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe
                | ErrorKind::ConnectionRefused => {
                    incr!("tcp.write.error");
                    (0, SocketResult::Closed)
                }
                _ => {
                    //FIXME: timeout and other common errors should be sent up
                    error!("SOCKET\tsocket_write error={:?}", e);
                    incr!("tcp.write.error");
                    (0, SocketResult::Error)
                }
            },
        }
    }

    fn has_vectored_writes(&self) -> bool {
        true
    }

    fn socket_ref(&self) -> &TcpStream {
        self
    }

    fn socket_mut(&mut self) -> &mut TcpStream {
        self
    }

    fn protocol(&self) -> TransportProtocol {
        TransportProtocol::Tcp
    }

    fn read_error(&self) {
        incr!("tcp.read.error");
    }

    fn write_error(&self) {
        incr!("tcp.write.error");
    }
}

#[cfg(feature = "use-openssl")]
impl SocketHandler for SslStream<TcpStream> {
    fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
        let mut size = 0usize;
        loop {
            if size == buf.len() {
                return (size, SocketResult::Continue);
            }
            match self.ssl_read(&mut buf[size..]) {
                Ok(0) => return (size, SocketResult::Continue),
                Ok(sz) => size += sz,
                Err(e) => match e.code() {
                    ErrorCode::WANT_READ => return (size, SocketResult::WouldBlock),
                    ErrorCode::WANT_WRITE => return (size, SocketResult::WouldBlock),
                    ErrorCode::SSL => {
                        debug!(
                            "SOCKET-TLS\treadable TLS socket SSL error: {:?} -> {:?}",
                            e,
                            e.ssl_error()
                        );
                        return (size, SocketResult::Error);
                    }
                    ErrorCode::SYSCALL => return (size, SocketResult::Error),
                    ErrorCode::ZERO_RETURN => return (size, SocketResult::Closed),
                    _ => {
                        debug!(
                            "SOCKET-TLS\treadable TLS socket error={:?} -> {:?}",
                            e,
                            e.ssl_error()
                        );
                        return (size, SocketResult::Error);
                    }
                },
            }
        }
    }

    fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
        let mut size = 0usize;
        loop {
            if size == buf.len() {
                return (size, SocketResult::Continue);
            }
            match self.ssl_write(&buf[size..]) {
                Ok(0) => return (size, SocketResult::Continue),
                Ok(sz) => size += sz,
                Err(e) => match e.code() {
                    ErrorCode::WANT_READ => return (size, SocketResult::WouldBlock),
                    ErrorCode::WANT_WRITE => return (size, SocketResult::WouldBlock),
                    ErrorCode::SSL => {
                        debug!(
                            "SOCKET-TLS\twritable TLS socket SSL error: {:?} -> {:?}",
                            e,
                            e.ssl_error()
                        );
                        return (size, SocketResult::Error);
                    }
                    ErrorCode::SYSCALL => return (size, SocketResult::Error),
                    ErrorCode::ZERO_RETURN => return (size, SocketResult::Closed),
                    _ => {
                        debug!(
                            "SOCKET-TLS\twritable TLS socket error={:?} -> {:?}",
                            e,
                            e.ssl_error()
                        );
                        return (size, SocketResult::Error);
                    }
                },
            }
        }
    }

    fn socket_ref(&self) -> &TcpStream {
        self.get_ref()
    }

    fn socket_mut(&mut self) -> &mut TcpStream {
        self.get_mut()
    }

    fn protocol(&self) -> TransportProtocol {
        self.ssl()
            .version2()
            .map(|version| match version {
                SslVersion::SSL3 => TransportProtocol::Ssl3,
                SslVersion::TLS1 => TransportProtocol::Tls1_0,
                SslVersion::TLS1_1 => TransportProtocol::Tls1_1,
                SslVersion::TLS1_2 => TransportProtocol::Tls1_2,
                _ => TransportProtocol::Tls1_3,
            })
            .unwrap_or(TransportProtocol::Tcp)
    }

    fn read_error(&self) {
        incr!("openssl.read.error");
    }

    fn write_error(&self) {
        incr!("openssl.write.error");
    }
}

pub struct FrontRustls {
    pub stream: TcpStream,
    pub session: ServerConnection,
}

impl SocketHandler for FrontRustls {
    fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
        let mut size = 0usize;
        let mut can_read = true;
        let mut is_error = false;
        let mut is_closed = false;

        loop {
            if size == buf.len() {
                break;
            }

            if !can_read | is_error | is_closed {
                break;
            }

            match self.session.read_tls(&mut self.stream) {
                Ok(0) => {
                    can_read = false;
                    is_closed = true;
                }
                Ok(_sz) => {}
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => {
                        can_read = false;
                    }
                    ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe => {
                        is_closed = true;
                    }
                    _ => {
                        error!("could not read TLS stream from socket: {:?}", e);
                        is_error = true;
                        break;
                    }
                },
            }

            if let Err(e) = self.session.process_new_packets() {
                error!("could not process read TLS packets: {:?}", e);
                is_error = true;
                break;
            }

            while !self.session.wants_read() {
                match self.session.reader().read(&mut buf[size..]) {
                    Ok(0) => break,
                    Ok(sz) => size += sz,
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            break;
                        }
                        ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe => {
                            is_closed = true;
                            break;
                        }
                        _ => {
                            error!("could not read data from TLS stream: {:?}", e);
                            is_error = true;
                            break;
                        }
                    },
                }
            }
        }

        if is_error {
            (size, SocketResult::Error)
        } else if is_closed {
            (size, SocketResult::Closed)
        } else if !can_read {
            (size, SocketResult::WouldBlock)
        } else {
            (size, SocketResult::Continue)
        }
    }

    fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
        let mut buffered_size = 0usize;
        let mut can_write = true;
        let mut is_error = false;
        let mut is_closed = false;

        loop {
            if buffered_size == buf.len() {
                break;
            }

            if !can_write | is_error | is_closed {
                break;
            }

            match self.session.writer().write(&buf[buffered_size..]) {
                Ok(0) => {
                    break;
                }
                Ok(sz) => {
                    buffered_size += sz;
                }
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => {
                        // we don't need to do anything, the session will return false in wants_write?
                        //error!("rustls socket_write wouldblock");
                    }
                    ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe => {
                        //FIXME: this should probably not happen here
                        incr!("rustls.write.error");
                        is_closed = true;
                        break;
                    }
                    _ => {
                        error!("could not write data to TLS stream: {:?}", e);
                        incr!("rustls.write.error");
                        is_error = true;
                        break;
                    }
                },
            }

            loop {
                match self.session.write_tls(&mut self.stream) {
                    Ok(0) => {
                        //can_write = false;
                        break;
                    }
                    Ok(_sz) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => can_write = false,
                        ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe => {
                            incr!("rustls.write.error");
                            is_closed = true;
                            break;
                        }
                        _ => {
                            error!("could not write TLS stream to socket: {:?}", e);
                            incr!("rustls.write.error");
                            is_error = true;
                            break;
                        }
                    },
                }
            }
        }

        if is_error {
            (buffered_size, SocketResult::Error)
        } else if is_closed {
            (buffered_size, SocketResult::Closed)
        } else if !can_write {
            (buffered_size, SocketResult::WouldBlock)
        } else {
            (buffered_size, SocketResult::Continue)
        }
    }

    fn socket_ref(&self) -> &TcpStream {
        &self.stream
    }

    fn socket_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    fn protocol(&self) -> TransportProtocol {
        self.session
            .protocol_version()
            .map(|version| match version {
                ProtocolVersion::SSLv2 => TransportProtocol::Ssl2,
                ProtocolVersion::SSLv3 => TransportProtocol::Ssl3,
                ProtocolVersion::TLSv1_0 => TransportProtocol::Tls1_0,
                ProtocolVersion::TLSv1_1 => TransportProtocol::Tls1_1,
                ProtocolVersion::TLSv1_2 => TransportProtocol::Tls1_2,
                ProtocolVersion::TLSv1_3 => TransportProtocol::Tls1_3,
                _ => TransportProtocol::Tls1_3,
            })
            .unwrap_or(TransportProtocol::Tcp)
    }

    fn read_error(&self) {
        incr!("rustls.read.error");
    }

    fn write_error(&self) {
        incr!("rustls.write.error");
    }
}

pub fn server_bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;

    // set so_reuseaddr, but only on unix (mirrors what libstd does)
    if cfg!(unix) {
        sock.set_reuse_address(true)?;
    }

    sock.set_reuse_port(true)?;

    // bind the socket
    let addr = addr.into();
    sock.bind(&addr)?;

    sock.set_nonblocking(true)?;

    // listen
    // FIXME: make the backlog configurable?
    sock.listen(1024)?;

    Ok(TcpListener::from_std(sock.into()))
}
