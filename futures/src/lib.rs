#[macro_use]
extern crate log;
extern crate sozu_command_lib as sozu_command;

use bytes::BytesMut;
use futures::{SinkExt, TryStreamExt};
use sozu_command::command::{CommandRequest, CommandResponse, CommandStatus};
use std::io::{self, Error, ErrorKind};
use std::str::from_utf8;
use tokio::net::UnixStream;
use tokio_util::codec::{Decoder, Encoder, Framed};

pub struct CommandCodec;

impl Decoder for CommandCodec {
    type Item = CommandResponse;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<CommandResponse>, io::Error> {
        if let Some(pos) = (&buf[..]).iter().position(|&x| x == 0) {
            let res = if let Ok(s) = from_utf8(&buf[..pos]) {
                match serde_json::from_str(s) {
                    Ok(message) => Ok(Some(message)),
                    Err(e) => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("parse error: {:?}", e),
                    )),
                }
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    String::from("could not parse UTF-8 data"),
                ))
            };

            if pos < buf.len() {
                let _ = buf.split_to(pos + 1);
            }

            res
        } else {
            Ok(None)
        }
    }
}

impl Encoder<CommandRequest> for CommandCodec {
    type Error = io::Error;

    fn encode(&mut self, message: CommandRequest, buf: &mut BytesMut) -> Result<(), Self::Error> {
        match serde_json::to_string(&message) {
            Ok(data) => {
                trace!("encoded message: {}", data);
                buf.extend(data.as_bytes());
                buf.extend(&[0u8][..]);
                trace!("buffer content: {:?}", from_utf8(&buf[..]));
                Ok(())
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("serialization error: {:?}", e),
            )),
        }
    }
}

pub struct SozuCommandClient {
    transport: Framed<UnixStream, CommandCodec>,
}

impl SozuCommandClient {
    pub fn new(stream: UnixStream) -> SozuCommandClient {
        SozuCommandClient {
            transport: CommandCodec.framed(stream),
        }
    }

    pub async fn send(&mut self, message: CommandRequest) -> Result<CommandResponse, io::Error> {
        trace!("will send message: {:?}", message);

        let id = message.id.clone();
        self.transport.send(message).await?;

        loop {
            match self.transport.try_next().await? {
                None => {}
                Some(msg) => {
                    if msg.id != id {
                        return Err(Error::new(
                            ErrorKind::ConnectionAborted,
                            format!("could not send message"),
                        ));
                    }

                    if msg.status == CommandStatus::Processing {
                        info!("processing: {:?}", msg);
                    } else {
                        return Ok(msg);
                    }
                }
            }
        }
    }
}
