#![allow(unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::sync::{Arc,Mutex};
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::mem;
use mio::*;
use mio::tcp::*;
use mio::timer::Timeout;
use std::io::{self,Read,Write,ErrorKind,BufReader};
use bytes::{Buf,ByteBuf,MutByteBuf};
use bytes::buf::MutBuf;
use std::collections::HashMap;
use std::error::Error;
use slab::Slab;
use pool::{Pool,Checkout};
use std::net::{IpAddr,SocketAddr};
use std::str::{FromStr, from_utf8, from_utf8_unchecked};
use time::{precise_time_s, precise_time_ns};
use rand::random;
use openssl::ssl::{self,HandshakeError,MidHandshakeSslStream,
                   SslContext, SslContextOptions, SslMethod,
                   Ssl, SslRef, SslStream, SniError};
use openssl::x509::{X509,X509FileType};
use openssl::dh::DH;
use openssl::crypto::pkey::PKey;
use openssl::crypto::hash::Type;
use openssl::nid::Nid;
use nom::IResult;

use parser::http11::{HttpState,RequestState,ResponseState,RRequestLine,parse_request_until_stop,hostname_and_port};
use network::buffer::Buffer;
use network::buffer_queue::BufferQueue;
use network::{Backend,ClientResult,ServerMessage,ServerMessageType,ConnectionError,ProxyOrder,Protocol};
use network::proxy::{BackendConnectAction,Server,ProxyConfiguration,ProxyClient,Readiness,ListenToken,FrontToken,BackToken};
use messages::{self,Command,TlsFront,TlsProxyConfiguration};
use network::http::{self,DefaultAnswers};
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::trie::*;
use network::protocol::ProtocolResult;

pub enum TlsState {
  Initial,
  Handshake,
  Established,
  Error,
}

pub struct TlsHandshake {
  pub readiness:       Readiness,
  pub front_token:     Option<Token>,
  pub front:           Option<TcpStream>,
  pub front_buf:       Checkout<BufferQueue>,
  pub back_buf:        Checkout<BufferQueue>,
  pub ssl:             Option<Ssl>,
  pub stream:          Option<SslStream<TcpStream>>,
  pub server_context:  String,
  mid:                 Option<MidHandshakeSslStream<TcpStream>>,
  state:               TlsState,
}

impl TlsHandshake {
  pub fn new(server_context: &str, ssl:Ssl, sock: TcpStream, front_buf: Checkout<BufferQueue>, back_buf: Checkout<BufferQueue>) -> TlsHandshake {
    TlsHandshake {
      front:          Some(sock),
      front_token:    None,
      server_context: String::from(server_context),
      front_buf:      front_buf,
      back_buf:       back_buf,
      ssl:            Some(ssl),
      mid:            None,
      stream:         None,
      state:          TlsState::Initial,
      readiness:      Readiness {
                        front_interest:  Ready::readable() | Ready::hup() | Ready::error(),
                        back_interest:   Ready::none(),
                        front_readiness: Ready::none(),
                        back_readiness:  Ready::none(),
      },
    }
  }

  pub fn readable(&mut self) -> (ProtocolResult,ClientResult) {
    info!("handshake readable");
    match self.state {
      TlsState::Error   => return (ProtocolResult::Continue, ClientResult::CloseClient),
      TlsState::Initial => {
        let ssl     = self.ssl.take().unwrap();
        //let sock    = self.front.as_ref().map(|f| f.try_clone().unwrap()).unwrap();
        let sock    = self.front.take().unwrap();
        let version = ssl.version();
        match SslStream::accept(ssl, sock) {
          Ok(stream) => {
            /*
            let temp   = self.temp.take().unwrap();
            self.http  = http::Client::new(&temp.server_context, stream, temp.front_buf, temp.back_buf, self.public_address);
            self.readiness().front_interest = Ready::readable() | Ready::hup() | Ready::error();
            self.state = TlsState::Established;
            */
    info!("handshake readable: accept");
            self.stream = Some(stream);
            return (ProtocolResult::Upgrade, ClientResult::Continue);
          },
          Err(HandshakeError::Failure(e)) => {
            println!("accept: handshake failed: {:?}", e);
            println!("version: {:?}", version);
            self.state = TlsState::Error;
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          },
          Err(HandshakeError::Interrupted(mid)) => {
    info!("handshake readable: Interrupted");
            self.state = TlsState::Handshake;
            self.mid = Some(mid);
            self.readiness.front_readiness.remove(Ready::readable());
            return (ProtocolResult::Continue, ClientResult::Continue);
          }
        }
      },
      TlsState::Handshake => {
    info!("handshake readable: mid");
        let mid = self.mid.take().unwrap();
        let version = mid.ssl().version();
        match mid.handshake() {
          Ok(stream) => {
            /*
            let temp   = self.temp.take().unwrap();
            self.http  = http::Client::new(&temp.server_context, stream, temp.front_buf, temp.back_buf, self.public_address);
            self.readiness().front_interest = Ready::readable() | Ready::hup() | Ready::error();
            self.state = TlsState::Established;
            */
    info!("handshake readable: accepted");
            self.stream = Some(stream);
            return (ProtocolResult::Upgrade, ClientResult::Continue);
          },
          Err(HandshakeError::Failure(e)) => {
            println!("mid handshake failed: {:?}", e);
            println!("version: {:?}", version);
            self.state = TlsState::Error;
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          },
          Err(HandshakeError::Interrupted(new_mid)) => {
    info!("handshake readable: interrupted");
            self.state = TlsState::Handshake;
            self.mid = Some(new_mid);
            self.readiness.front_readiness.remove(Ready::readable());
            return (ProtocolResult::Continue, ClientResult::Continue);
          }
        }
      },
      TlsState::Established => {
    info!("handshake readable: established");
        //FIXME: CHANGE STATE
        return (ProtocolResult::Upgrade, ClientResult::Continue);
      }
    }

  }

  fn protocol(&self)           -> Protocol {
    Protocol::TLS
  }
}

