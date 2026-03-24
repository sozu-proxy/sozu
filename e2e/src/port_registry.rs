use std::{
    collections::{HashMap, HashSet},
    net::{SocketAddr, TcpListener},
    os::fd::{IntoRawFd, RawFd},
    sync::{Mutex, OnceLock},
};

use sozu_command_lib::scm_socket::Listeners;

// Keep test allocations in a high user-space range. We still probe with bind()
// before handing a port out, so overlap with the OS ephemeral range is safe.
const PORT_SEARCH_START: u16 = 20_000;
const PORT_SEARCH_END: u16 = 65_000;

struct PortRegistry {
    next_port: u16,
    issued_ports: HashSet<u16>,
    pending_listeners: HashMap<SocketAddr, TcpListener>,
}

impl PortRegistry {
    fn new() -> Self {
        Self {
            next_port: PORT_SEARCH_START,
            issued_ports: HashSet::new(),
            pending_listeners: HashMap::new(),
        }
    }

    fn reserve_listener_port(&mut self) -> u16 {
        let search_span = usize::from(PORT_SEARCH_END - PORT_SEARCH_START) + 1;

        for _ in 0..search_span {
            let candidate = self.next_port;
            self.advance_cursor();

            if self.issued_ports.contains(&candidate) {
                continue;
            }

            let address = SocketAddr::from(([127, 0, 0, 1], candidate));
            let listener = match TcpListener::bind(address) {
                Ok(listener) => listener,
                Err(_) => continue,
            };

            self.pending_listeners.insert(address, listener);
            return candidate;
        }

        panic!(
            "could not allocate a free localhost port in range {PORT_SEARCH_START}..={PORT_SEARCH_END}"
        );
    }

    fn issue_unbound_port(&mut self) -> u16 {
        let search_span = usize::from(PORT_SEARCH_END - PORT_SEARCH_START) + 1;

        for _ in 0..search_span {
            let candidate = self.next_port;
            self.advance_cursor();

            if self.issued_ports.contains(&candidate) {
                continue;
            }

            let address = SocketAddr::from(([127, 0, 0, 1], candidate));
            let listener = match TcpListener::bind(address) {
                Ok(listener) => listener,
                Err(_) => continue,
            };

            drop(listener);
            self.issued_ports.insert(candidate);
            return candidate;
        }

        panic!(
            "could not allocate a free localhost port in range {PORT_SEARCH_START}..={PORT_SEARCH_END}"
        );
    }

    fn take_listener(&mut self, address: SocketAddr) -> Option<TcpListener> {
        self.pending_listeners.remove(&address)
    }

    fn advance_cursor(&mut self) {
        self.next_port = if self.next_port >= PORT_SEARCH_END {
            PORT_SEARCH_START
        } else {
            self.next_port + 1
        };
    }
}

fn registry() -> &'static Mutex<PortRegistry> {
    static PORT_REGISTRY: OnceLock<Mutex<PortRegistry>> = OnceLock::new();

    PORT_REGISTRY.get_or_init(|| Mutex::new(PortRegistry::new()))
}

pub(crate) fn provide_port() -> u16 {
    registry()
        .lock()
        .expect("port registry lock poisoned")
        .reserve_listener_port()
}

pub(crate) fn provide_unbound_port() -> u16 {
    registry()
        .lock()
        .expect("port registry lock poisoned")
        .issue_unbound_port()
}

pub(crate) fn take_reserved_listener(address: SocketAddr) -> Option<TcpListener> {
    registry()
        .lock()
        .expect("port registry lock poisoned")
        .take_listener(address)
}

pub(crate) fn bind_std_listener(address: SocketAddr, context: &str) -> TcpListener {
    take_reserved_listener(address).unwrap_or_else(|| {
        TcpListener::bind(address)
            .unwrap_or_else(|error| panic!("{context}: could not bind {address}: {error}"))
    })
}

pub(crate) fn bind_tokio_listener(address: SocketAddr, context: &str) -> tokio::net::TcpListener {
    if let Some(listener) = take_reserved_listener(address) {
        listener.set_nonblocking(true).unwrap_or_else(|error| {
            panic!("{context}: could not set nonblocking {address}: {error}")
        });
        tokio::net::TcpListener::from_std(listener).unwrap_or_else(|error| {
            panic!("{context}: could not adopt reserved listener {address}: {error}")
        })
    } else {
        let std_listener = TcpListener::bind(address)
            .unwrap_or_else(|error| panic!("{context}: could not bind {address}: {error}"));
        std_listener.set_nonblocking(true).unwrap_or_else(|error| {
            panic!("{context}: could not set nonblocking {address}: {error}")
        });
        tokio::net::TcpListener::from_std(std_listener).unwrap_or_else(|error| {
            panic!("{context}: could not create tokio listener {address}: {error}")
        })
    }
}

pub(crate) fn attach_reserved_http_listener(listeners: &mut Listeners, address: SocketAddr) {
    listeners.http.push((
        address,
        into_listener_fd(address, "http listener reservation"),
    ));
}

pub(crate) fn attach_reserved_https_listener(listeners: &mut Listeners, address: SocketAddr) {
    listeners.tls.push((
        address,
        into_listener_fd(address, "https listener reservation"),
    ));
}

pub(crate) fn attach_reserved_tcp_listener(listeners: &mut Listeners, address: SocketAddr) {
    listeners.tcp.push((
        address,
        into_listener_fd(address, "tcp listener reservation"),
    ));
}

fn into_listener_fd(address: SocketAddr, context: &str) -> RawFd {
    let listener = bind_std_listener(address, context);
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|error| panic!("{context}: could not set nonblocking {address}: {error}"));
    listener.into_raw_fd()
}
