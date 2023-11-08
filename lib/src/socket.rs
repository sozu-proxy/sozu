use std::{
    io::{ErrorKind, Read, Write},
    net::SocketAddr,
};

use mio::net::{TcpListener, TcpStream};
use rustls::{ProtocolVersion, ServerConnection};
use socket2::{Domain, Protocol, Socket, Type};

#[derive(thiserror::Error, Debug)]
pub enum ServerBindError {
    #[error("could not set bind to socket: {0}")]
    BindError(std::io::Error),
    #[error("could not listen on socket: {0}")]
    Listen(std::io::Error),
    #[error("could not set socket to nonblocking: {0}")]
    SetNonBlocking(std::io::Error),
    #[error("could not set reuse address: {0}")]
    SetReuseAddress(std::io::Error),
    #[error("could not set reuse address: {0}")]
    SetReusePort(std::io::Error),
    #[error("Could not create socket: {0}")]
    SocketCreationError(std::io::Error),
    #[error("Invalid socket address '{address}': {error}")]
    InvalidSocketAddress { address: String, error: String },
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum SocketResult {
    Continue,
    Closed,
    WouldBlock,
    Error,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
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
                    // https://github.com/rustls/rustls/blob/main/rustls/src/conn.rs#L482-L500,
                    ErrorKind::Other => {
                        warn!("rustls buffer is full, we will consume it, before processing new incoming packets, to mitigate this issue, you could try to increase the buffer size, {:?}", e);
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
                    Ok(sz) => {
                        size += sz;
                        can_read = true;
                    }
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

    fn socket_write_vectored(&mut self, bufs: &[std::io::IoSlice]) -> (usize, SocketResult) {
        let mut buffered_size = 0usize;
        let mut can_write = true;
        let mut is_error = false;
        let mut is_closed = false;

        match self.session.writer().write_vectored(&bufs) {
            Ok(0) => {}
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
                }
                _ => {
                    error!("could not write data to TLS stream: {:?}", e);
                    incr!("rustls.write.error");
                    is_error = true;
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

pub fn server_bind(addr: String) -> Result<TcpListener, ServerBindError> {
    let address = addr.parse::<SocketAddr>().map_err(|parse_error| {
        ServerBindError::InvalidSocketAddress {
            address: addr.clone(),
            error: parse_error.to_string(),
        }
    })?;

    let sock = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )
    .map_err(ServerBindError::SocketCreationError)?;

    // set so_reuseaddr, but only on unix (mirrors what libstd does)
    if cfg!(unix) {
        sock.set_reuse_address(true)
            .map_err(ServerBindError::SetReuseAddress)?;
    }

    sock.set_reuse_port(true)
        .map_err(ServerBindError::SetReusePort)?;

    // bind the socket
    let addr = address.into();
    sock.bind(&addr).map_err(ServerBindError::BindError)?;

    sock.set_nonblocking(true)
        .map_err(ServerBindError::SetNonBlocking)?;

    // listen
    // FIXME: make the backlog configurable?
    sock.listen(1024).map_err(ServerBindError::Listen)?;

    Ok(TcpListener::from_std(sock.into()))
}

/// Socket statistics
pub mod stats {
    use std::os::fd::AsRawFd;
    use time::Duration;

    use internal::{TcpInfo, OPT_LEVEL, OPT_NAME};

    /// Round trip time for a TCP socket
    pub fn socket_rtt<A: AsRawFd>(socket: &A) -> Option<Duration> {
        socket_info(socket.as_raw_fd()).map(|info| Duration::microseconds(info.rtt() as i64))
    }

    #[cfg(unix)]
    pub fn socket_info(fd: libc::c_int) -> Option<TcpInfo> {
        let mut tcp_info: TcpInfo = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<TcpInfo>() as libc::socklen_t;
        let status = unsafe {
            libc::getsockopt(
                fd,
                OPT_LEVEL,
                OPT_NAME,
                &mut tcp_info as *mut _ as *mut _,
                &mut len,
            )
        };
        if status != 0 {
            None
        } else {
            Some(tcp_info)
        }
    }
    #[cfg(not(unix))]
    pub fn socketinfo(fd: libc::c_int) -> Option<TcpInfo> {
        None
    }

    #[cfg(unix)]
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    mod internal {
        pub const OPT_LEVEL: libc::c_int = libc::SOL_TCP;
        pub const OPT_NAME: libc::c_int = libc::TCP_INFO;

        #[derive(Clone, Debug)]
        #[repr(C)]
        pub struct TcpInfo {
            // State
            tcpi_state: u8,
            tcpi_ca_state: u8,
            tcpi_retransmits: u8,
            tcpi_probes: u8,
            tcpi_backoff: u8,
            tcpi_options: u8,
            tcpi_snd_rcv_wscale: u8, // 4bits|4bits

            tcpi_rto: u32,
            tcpi_ato: u32,
            tcpi_snd_mss: u32,
            tcpi_rcv_mss: u32,

            tcpi_unacked: u32,
            tcpi_sacked: u32,
            tcpi_lost: u32,
            tcpi_retrans: u32,
            tcpi_fackets: u32,

            // Times
            tcpi_last_data_sent: u32,
            tcpi_last_ack_sent: u32, // Not remembered
            tcpi_last_data_recv: u32,
            tcpi_last_ack_recv: u32,

            // Metrics
            tcpi_pmtu: u32,
            tcpi_rcv_ssthresh: u32,
            tcpi_rtt: u32,
            tcpi_rttvar: u32,
            tcpi_snd_ssthresh: u32,
            tcpi_snd_cwnd: u32,
            tcpi_advmss: u32,
            tcpi_reordering: u32,
        }
        impl TcpInfo {
            pub fn rtt(&self) -> u32 {
                self.tcpi_rtt
            }
        }
    }

    #[cfg(unix)]
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    mod internal {
        pub const OPT_LEVEL: libc::c_int = libc::IPPROTO_TCP;
        pub const OPT_NAME: libc::c_int = 0x106;

        #[derive(Clone, Debug)]
        #[repr(C)]
        pub struct TcpInfo {
            tcpi_state: u8,
            tcpi_snd_wscale: u8,
            tcpi_rcv_wscale: u8,
            __pad1: u8,
            tcpi_options: u32,
            tcpi_flags: u32,
            tcpi_rto: u32,
            tcpi_maxseg: u32,
            tcpi_snd_ssthresh: u32,
            tcpi_snd_cwnd: u32,
            tcpi_snd_wnd: u32,
            tcpi_snd_sbbytes: u32,
            tcpi_rcv_wnd: u32,
            tcpi_rttcur: u32,
            tcpi_srtt: u32,
            tcpi_rttvar: u32,
            tcpi_tfo: u32,
            tcpi_txpackets: u64,
            tcpi_txbytes: u64,
            tcpi_txretransmitbytes: u64,
            tcpi_rxpackets: u64,
            tcpi_rxbytes: u64,
            tcpi_rxoutoforderbytes: u64,
            tcpi_txretransmitpackets: u64,
        }
        impl TcpInfo {
            pub fn rtt(&self) -> u32 {
                // tcpi_srtt is in milliseconds not microseconds
                self.tcpi_srtt * 1000
            }
        }
    }

    #[cfg(not(unix))]
    #[derive(Clone, Debug)]
    struct TcpInfo {}

    #[test]
    #[serial_test::serial]
    fn test_rtt() {
        let sock = std::net::TcpStream::connect("google.com:80").unwrap();
        let fd = sock.as_raw_fd();
        let info = socket_info(fd);
        assert!(info.is_some());
        println!("{:#?}", info);
        println!("rtt: {}", crate::logs::LogDuration(socket_rtt(&sock)));
    }
}
