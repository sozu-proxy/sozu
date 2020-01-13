#[macro_use] extern crate log;
extern crate futures;
extern crate tokio_codec;
extern crate tokio_uds;
extern crate bytes;
extern crate serde;

extern crate serde_json;
extern crate sozu_command_lib as sozu_command;

use bytes::BytesMut;
use std::io::{self, Error, ErrorKind};
use std::sync::{Arc,Mutex};
use std::collections::hash_map::HashMap;
use futures::{Async, Sink, Stream, Future, future};
use tokio_uds::UnixStream;
use tokio_codec::{Decoder, Encoder, Framed};
use std::str::from_utf8;
use sozu_command::command::{CommandRequest,CommandResponse,CommandStatus};

pub struct CommandCodec;

impl Decoder for CommandCodec {
    type Item  = CommandResponse;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<CommandResponse>, io::Error> {
        if let Some(pos) = (&buf[..]).iter().position(|&x| x == 0) {
            let res = if let Ok(s) = from_utf8(&buf[..pos]) {
                match serde_json::from_str(s) {
                    Ok(message) => Ok(Some(message)),
                    Err(e) => {
                        Err(io::Error::new(io::ErrorKind::Other, format!("parse error: {:?}", e)))
                    }
                }
            } else {
                Err(io::Error::new(io::ErrorKind::InvalidData, String::from("could not parse UTF-8 data")))
            };

            if pos < buf.len() {
              buf.split_to(pos+1);
            }

            res
        } else {
            Ok(None)
        }
    }
}

impl Encoder for CommandCodec {
    type Item = CommandRequest;
    type Error = io::Error;

    fn encode(&mut self, message: CommandRequest, buf: &mut BytesMut) -> Result<(), Self::Error> {
        match serde_json::to_string(&message) {
        Ok(data) => {
            trace!("encoded message: {}", data);
            buf.extend(data.as_bytes());
            buf.extend(&[0u8][..]);
            trace!("buffer content: {:?}", from_utf8(&buf[..]));
            Ok(()) },
        Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("serialization error: {:?}", e)))
        }
    }
}

pub struct SozuCommandTransport {
    upstream: Framed<UnixStream,CommandCodec>,
    received: HashMap<String,CommandResponse>,
}

impl SozuCommandTransport {
    pub fn new(stream: UnixStream) -> SozuCommandTransport {
        SozuCommandTransport {
            upstream: CommandCodec.framed(stream),
            received: HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct SozuCommandClient {
    transport: Arc<Mutex<SozuCommandTransport>>,
}

impl SozuCommandClient {
    pub fn new(stream: UnixStream) -> SozuCommandClient {
        SozuCommandClient {
            transport: Arc::new(Mutex::new(SozuCommandTransport::new(stream))),
        }
    }

    pub fn send(&mut self, message: CommandRequest)  -> Box<dyn Future<Item = CommandResponse, Error = io::Error> + Send + 'static> {
        trace!("will send message: {:?}", message);
        let tr  = self.transport.clone();
        let tr2 = self.transport.clone();

        if let Ok(mut transport) = self.transport.lock() {
            let id = message.id.clone();
            let res = transport.upstream.start_send(message);
            trace!("start_send result: {:?}", res);

            Box::new(future::poll_fn(move || {
                if let Ok(mut transport) = tr2.try_lock() {
                    let res = transport.upstream.poll_complete();
                    res
                } else {
                    Err(Error::new(ErrorKind::ConnectionAborted, format!("could not send message")))
                }
            }).and_then(|_| {
                future::poll_fn(move || {
                    trace!("polling for id = {}", id);
                    if let Ok(mut transport) = tr.try_lock() {
                        if let Some(message) = transport.received.remove(&id) {
                            if message.status == CommandStatus::Processing {
                                info!("processing: {:?}", message);
                            } else {
                                return Ok(Async::Ready(message))
                            }
                        }

                        let value = match transport.upstream.poll() {
                            Ok(Async::Ready(t)) => t,
                            Ok(Async::NotReady) => {
                                trace!("upstream poll gave NotReady");
                                return Ok(Async::NotReady);
                            },
                            Err(e) => {
                                trace!("upstream poll gave error: {:?}", e);
                                return Err(From::from(e));
                            },
                        };

                        if let Some(message) = value {
                            trace!("upstream poll gave message: {:?}", message);
                            if message.id != id {
                                transport.received.insert(message.id.clone(), message);
                                Ok(Async::NotReady)
                            } else {
                                if message.status == CommandStatus::Processing {
                                    info!("processing: {:?}", message);
                                    Ok(Async::NotReady)
                                } else {
                                    Ok(Async::Ready(message))
                                }
                            }
                        } else {
                            trace!("upstream poll gave Ready(None)");
                            Ok(Async::NotReady)
                        }
                    } else {
                        //FIXME: if we're there, it means the mutex failed
                        Err(
                            Error::new(ErrorKind::ConnectionAborted, format!("could not send message"))
                        )
                    }

                })
            }))
        } else {
            //FIXME: if we're there, it means the mutex failed
            Box::new(future::err(
                Error::new(ErrorKind::ConnectionAborted, format!("could not send message"))
            ))
        }
    }
}
