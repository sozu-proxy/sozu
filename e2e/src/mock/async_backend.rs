use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::{fd::AsRawFd, unix::net::UnixStream},
    str::from_utf8_unchecked,
    thread,
};

use futures::channel::mpsc;

use crate::{
    BUFFER_SIZE,
    http_utils::http_ok_response,
    mock::aggregator::{Aggregator, SimpleAggregator},
    port_registry::bind_std_listener,
    sched::current_tid,
};

/// Handle to a detached thread where a Backend runs
/// (a thin wrapper around a TcpListener)
pub struct BackendHandle<T> {
    pub name: String,
    /// Allows to stop the backend within the thread
    pub stop_tx: mpsc::Sender<()>,
    /// Receives data from the backend on the thread
    pub aggregator_rx: mpsc::Receiver<T>,
    /// Kernel id of the backend thread, for
    /// [`run_queue_delay`](crate::sched::run_queue_delay).
    pub tid: Option<i32>,
    /// Wakes the backend thread out of `poll` to see `stop_tx`.
    waker: UnixStream,
}

/// Block until `listener`, one of `clients` or `waker` is readable.
///
/// The backend thread used to spin on non-blocking `accept`/`read`. A thread
/// that spins is always runnable, so on a loaded host it notices a request
/// only once the scheduler gives it its next time slice, and that wait lands
/// in whatever round trip a test is timing; it also takes a CPU from every
/// other thread for the whole test. Blocked in `poll`, the thread is woken as
/// soon as bytes arrive, and by `waker` when it is asked to stop (`stop_rx`
/// is a channel and cannot be polled). Every handler starts with a read, so a
/// pass with nothing readable did nothing but return, and skipping it changes
/// no reply. A client at EOF stays readable, so a backend whose peer closed
/// returns from `poll` at once, as it always looped.
fn wait_readable(listener: &TcpListener, clients: &[TcpStream], waker: &UnixStream) {
    let mut fds: Vec<libc::pollfd> = [listener.as_raw_fd(), waker.as_raw_fd()]
        .into_iter()
        .chain(clients.iter().map(AsRawFd::as_raw_fd))
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // SAFETY: `fds` is a valid array of `fds.len()` initialized `pollfd`s,
    // every descriptor is owned by a socket borrowed for this call. A failed
    // or interrupted poll only makes the caller run one pass early.
    unsafe {
        libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1);
    }
}

type RequestHandler<A> = Box<dyn Fn(&TcpStream, &str, A) -> A + Send + Sync>;

impl<A: Aggregator + Send + Sync + 'static> BackendHandle<A> {
    pub fn spawn_detached_backend<S: Into<String>>(
        name: S,
        address: SocketAddr,
        mut aggregator: A,
        handler: RequestHandler<A>,
    ) -> Self {
        let name = name.into();
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (mut aggregator_tx, aggregator_rx) = mpsc::channel::<A>(1);
        let listener = bind_std_listener(address, "async backend");
        let mut clients = Vec::new();
        let thread_name = name.to_owned();
        let (tid_tx, tid_rx) = std::sync::mpsc::sync_channel(1);
        let (waker, wake_rx) = UnixStream::pair().expect("could not create the backend waker");

        // The backend runs on this detached thread:
        // - accepts tcp connections
        // - calls handler on each live connections
        // - monitors stop_rx to stop itself
        thread::spawn(move || {
            let _ = tid_tx.send(current_tid());
            listener
                .set_nonblocking(true)
                .expect("could not set nonblocking on listener");
            loop {
                wait_readable(&listener, &clients, &wake_rx);
                let stream = listener.accept();
                match stream {
                    Ok(stream) => {
                        println!("{thread_name}: new connection");
                        stream
                            .0
                            .set_nonblocking(true)
                            .expect("cound not set nonblocking on client");
                        clients.push(stream.0);
                    }
                    Err(error) => {
                        if error.kind() != ErrorKind::WouldBlock {
                            println!("IO Error: {error:?}");
                        }
                    }
                }
                for client in &clients {
                    aggregator = handler(client, &thread_name, aggregator);
                }
                match stop_rx.try_recv() {
                    Ok(_) => break,
                    _ => continue,
                }
            }
            drop(listener);
            aggregator_tx
                .try_send(aggregator)
                .expect("could not send aggregator");
        });
        Self {
            name,
            stop_tx,
            aggregator_rx,
            tid: tid_rx.recv().ok().flatten(),
            waker,
        }
    }

    pub fn stop_and_get_aggregator(&mut self) -> Option<A> {
        self.stop_tx.try_send(()).expect("could not stop backend");
        // The byte is never read: the thread leaves its loop on this pass.
        let _ = (&self.waker).write_all(&[0]);
        loop {
            match self.aggregator_rx.try_recv() {
                Ok(aggregator) => return Some(aggregator),
                _ => continue,
            }
        }
    }
}

impl BackendHandle<SimpleAggregator> {
    /// This creates a callback that listens on a TcpStream
    /// and returns HTTP OK responses with the given content in the body
    /// it returns an updated aggregator
    pub fn http_handler<S: Into<String>>(content: S) -> RequestHandler<SimpleAggregator> {
        let content = content.into();
        Box::new(move |mut stream, backend_name, mut aggregator| {
            let mut buf = [0u8; BUFFER_SIZE];
            match stream.read(&mut buf) {
                Ok(0) => return aggregator,
                Ok(n) => {
                    println!("{backend_name} received {n}");
                    println!("{}", unsafe { from_utf8_unchecked(&buf) });
                }
                Err(_) => {
                    //println!("{} could not receive {}", content, error);
                    return aggregator;
                }
            }
            aggregator.requests_received += 1;
            let response = http_ok_response(&content);
            match stream.write_all(response.as_bytes()) {
                Ok(()) => {
                    aggregator.responses_sent += 1;
                }
                Err(_) => {
                    // Connection closed by proxy (e.g. after forwarding response)
                }
            }
            aggregator
        })
    }

    /// This creates a callback that listens on a TcpStream
    /// and returns the given content as raw bytes
    /// it returns an updated aggregator
    pub fn tcp_handler<S: Into<String>>(content: S) -> RequestHandler<SimpleAggregator> {
        let content: String = content.into();
        Box::new(move |mut stream, backend_name, mut aggregator| {
            let mut buf = [0u8; BUFFER_SIZE];
            match stream.read(&mut buf) {
                Ok(0) => return aggregator,
                Ok(n) => {
                    println!("{backend_name} received {n}");
                }
                Err(error) => {
                    println!("{content} could not receive {error}");
                    return aggregator;
                }
            }
            stream.write_all(content.as_bytes()).unwrap();
            aggregator.responses_sent += 1;
            aggregator
        })
    }
}
