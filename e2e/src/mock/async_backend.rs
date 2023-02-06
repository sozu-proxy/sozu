use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    str::from_utf8_unchecked,
    thread,
};

use futures::channel::mpsc;

use crate::{
    http_utils::http_ok_response,
    mock::aggregator::{Aggregator, SimpleAggregator},
    BUFFER_SIZE,
};

/// Handle to a detached thread where a Backend runs
/// (a thin wrapper around a TcpListener)
pub struct BackendHandle<T> {
    pub name: String,
    /// Allows to stop the backend within the thread
    pub stop_tx: mpsc::Sender<()>,
    /// Receives data from the backend on the thread
    pub aggregator_rx: mpsc::Receiver<T>,
}

impl<A: Aggregator + Send + Sync + 'static> BackendHandle<A> {
    pub fn spawn_detached_backend<S: Into<String>>(
        name: S,
        address: SocketAddr,
        mut aggregator: A,
        handler: Box<dyn Fn(&TcpStream, &String, A) -> A + Send + Sync>,
    ) -> Self {
        let name = name.into();
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (mut aggregator_tx, aggregator_rx) = mpsc::channel::<A>(1);
        let listener = TcpListener::bind(address).expect("could not bind");
        let mut clients = Vec::new();
        let thread_name = name.to_owned();

        // The backend runs on this detached thread:
        // - accepts tcp connections
        // - calls handler on each live connections
        // - monitors stop_rx to stop itself
        thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("could not set nonblocking on listener");
            loop {
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
                match stop_rx.try_next() {
                    Ok(Some(_)) => break,
                    _ => continue,
                }
            }
            aggregator_tx
                .try_send(aggregator)
                .expect("could not send aggregator");
        });
        Self {
            name,
            stop_tx,
            aggregator_rx,
        }
    }

    pub fn stop_and_get_aggregator(&mut self) -> Option<A> {
        self.stop_tx.try_send(()).expect("could not stop backend");
        loop {
            match self.aggregator_rx.try_next() {
                Ok(Some(aggregator)) => return Some(aggregator),
                _ => continue,
            }
        }
    }
}

impl BackendHandle<SimpleAggregator> {
    /// This creates a callback that listens on a TcpStream
    /// and returns HTTP OK responses with the given content in the body
    /// it returns an updated aggregator
    pub fn http_handler<S: Into<String>>(
        content: S,
    ) -> Box<dyn Fn(&TcpStream, &String, SimpleAggregator) -> SimpleAggregator + Send + Sync> {
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
            stream.write_all(response.as_bytes()).unwrap();
            aggregator.responses_sent += 1;
            aggregator
        })
    }

    /// This creates a callback that listens on a TcpStream
    /// and returns the given content as raw bytes
    /// it returns an updated aggregator
    pub fn tcp_handler<S: Into<String>>(
        content: String,
    ) -> Box<dyn Fn(&TcpStream, &String, SimpleAggregator) -> SimpleAggregator + Send + Sync> {
        let content: String = content;
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
