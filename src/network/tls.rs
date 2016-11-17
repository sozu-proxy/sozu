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
use std::net::SocketAddr;
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
use network::{Backend,ClientResult,ServerMessage,ServerMessageType,ConnectionError,ProxyOrder};
use network::proxy::{BackendConnectAction,Server,ProxyConfiguration,ProxyClient,Readiness,ListenToken,FrontToken,BackToken};
use messages::{self,Command,TlsFront,TlsProxyConfiguration};
use network::http::{self,DefaultAnswers};
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::trie::*;

type BackendToken = Token;

type ClientToken = Token;

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct TlsApp {
  pub app_id:           String,
  pub hostname:         String,
  pub path_begin:       String,
  pub cert_fingerprint: CertFingerprint,
}

pub enum TlsState {
  Initial,
  Handshake,
  Established,
  Error,
}

pub struct TemporaryState {
  server_context: String,
  front_buf:      Checkout<BufferQueue>,
  back_buf:       Checkout<BufferQueue>,
}

pub struct TlsClient {
  front: Option<TcpStream>,
  front_token: Option<Token>,
  front_timeout: Option<Timeout>,
  temp: Option<TemporaryState>,
  ssl: Option<Ssl>,
  mid: Option<MidHandshakeSslStream<TcpStream>>,
  http:  Option<http::Client<SslStream<TcpStream>>>,
  state: TlsState,
  handshake_readiness: Readiness,
}

impl TlsClient {
  pub fn new(server_context: &str, ssl:Ssl, sock: TcpStream, front_buf: Checkout<BufferQueue>, back_buf: Checkout<BufferQueue>) -> Option<TlsClient> {
    Some(TlsClient {
      front: Some(sock),
      front_token: None,
      front_timeout: None,
      temp: Some(TemporaryState {
        server_context: String::from(server_context),
        front_buf: front_buf,
        back_buf: back_buf,
      }),
      ssl: Some(ssl),
      mid: None,
      http:  None,
      state: TlsState::Initial,
      handshake_readiness: Readiness {
        front_interest:  Ready::readable() | Ready::hup() | Ready::error(),
        back_interest:   Ready::none(),
        front_readiness: Ready::none(),
        back_readiness:  Ready::none(),
      }
    })
  }
}

impl ProxyClient for TlsClient {
  fn front_socket(&self) -> &TcpStream {
    if let Some(ref state) = self.http.as_ref() {
      state.front_socket()
    } else {
      self.front.as_ref().unwrap()
    }
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    if let Some(ref http) = self.http.as_ref() {
      http.back_socket()
    } else {
      None
    }
  }

  fn front_token(&self)  -> Option<Token> {
    if let Some(ref state) = self.http.as_ref() {
      state.front_token()
    } else {
      self.front_token
    }
  }

  fn back_token(&self)   -> Option<Token> {
    if let Some(ref http) = self.http.as_ref() {
      http.back_token()
    } else {
      None
    }
  }

  fn close(&mut self) {
    //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.token, *self.temp.front_buf, *self.temp.back_buf);
    self.http.as_mut().map(|http| http.close());
  }

  fn log_context(&self)  -> String {
    self.http.as_ref().unwrap().log_context()
  }

  fn set_back_socket(&mut self, sock:TcpStream) {
    self.http.as_mut().unwrap().set_back_socket(sock)
  }

  fn set_front_token(&mut self, token: Token) {
    if let Some(ref mut state) = self.http.as_mut() {
      state.set_front_token(token)
    } else {
      self.front_token = Some(token)
    }
  }

  fn set_back_token(&mut self, token: Token) {
    self.http.as_mut().unwrap().set_back_token(token)
  }

  fn front_timeout(&mut self) -> Option<Timeout> {
    if let Some(ref mut state) = self.http.as_mut() {
      state.front_timeout()
    } else {
      self.front_timeout.clone()
    }
  }

  fn back_timeout(&mut self)  -> Option<Timeout> {
    self.http.as_mut().unwrap().back_timeout()
  }

  fn set_front_timeout(&mut self, timeout: Timeout) {
    if let Some(ref mut state) = self.http.as_mut() {
      state.set_front_timeout(timeout)
    } else {
      self.front_timeout = Some(timeout)
    }
  }

  fn set_back_timeout(&mut self, timeout: Timeout) {
    self.http.as_mut().unwrap().set_back_timeout(timeout)
  }

  fn set_tokens(&mut self, token: Token, backend: Token) {
    self.http.as_mut().unwrap().set_tokens(token, backend)
  }

  fn front_hup(&mut self)     -> ClientResult {
    self.http.as_mut().unwrap().front_hup()
  }

  fn back_hup(&mut self)      -> ClientResult {
    self.http.as_mut().unwrap().back_hup()
  }

  fn readable(&mut self)      -> ClientResult {
    match self.state {
      TlsState::Error   => return ClientResult::CloseClient,
      TlsState::Initial => {
        let ssl     = self.ssl.take().unwrap();
        let sock    = self.front.as_ref().map(|f| f.try_clone().unwrap()).unwrap();
        let version = ssl.version();
        match SslStream::accept(ssl, sock) {
          Ok(stream) => {
            let temp   = self.temp.take().unwrap();
            self.http  = http::Client::new(&temp.server_context, stream, temp.front_buf, temp.back_buf);
            let front_token = self.front_token.take().unwrap();
            self.set_front_token(front_token);
            if let Some(timeout) = self.front_timeout.take() {
              self.set_front_timeout(timeout);
            }
            self.readiness().front_interest = Ready::readable() | Ready::hup() | Ready::error();
            self.state = TlsState::Established;
          },
          Err(HandshakeError::Failure(e)) => {
            println!("accept: handshake failed: {:?}", e);
            println!("version: {:?}", version);
            self.state = TlsState::Error;
            return ClientResult::CloseClient;
          },
          Err(HandshakeError::Interrupted(mid)) => {
            self.state = TlsState::Handshake;
            self.mid = Some(mid);
            return ClientResult::Continue;
          }
        }
      },
      TlsState::Handshake => {
        let mid = self.mid.take().unwrap();
        let version = mid.ssl().version();
        match mid.handshake() {
          Ok(stream) => {
            let temp   = self.temp.take().unwrap();
            self.http  = http::Client::new(&temp.server_context, stream, temp.front_buf, temp.back_buf);
            let front_token = self.front_token.take().unwrap();
            self.set_front_token(front_token);
            if let Some(timeout) = self.front_timeout.take() {
              self.set_front_timeout(timeout);
            }
            self.readiness().front_interest = Ready::readable() | Ready::hup() | Ready::error();
            self.state = TlsState::Established;
          },
          Err(HandshakeError::Failure(e)) => {
            println!("mid handshake failed: {:?}", e);
            println!("version: {:?}", version);
            self.state = TlsState::Error;
            return ClientResult::CloseClient;
          },
          Err(HandshakeError::Interrupted(new_mid)) => {
            self.state = TlsState::Handshake;
            self.mid = Some(new_mid);
            return ClientResult::Continue;
          }
        }
      },
      TlsState::Established => {}
    }

    //execute the main readable() method after handshake was done
    self.http.as_mut().unwrap().readable()
  }

  fn writable(&mut self)      -> ClientResult {
    self.http.as_mut().unwrap().writable()
  }

  fn back_readable(&mut self) -> ClientResult {
    self.http.as_mut().unwrap().back_readable()
  }

  fn back_writable(&mut self) -> ClientResult {
    self.http.as_mut().unwrap().back_writable()
  }

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    self.http.as_mut().unwrap().remove_backend()
  }

  fn readiness(&mut self)      -> &mut Readiness {
    if let Some(state) = self.http.as_mut() {
      state.readiness()
    } else {
      &mut self.handshake_readiness
    }
  }
}

pub type AppId     = String;
pub type HostName  = String;
pub type PathBegin = String;
//maybe not the most efficient type for that
pub type CertFingerprint = Vec<u8>;
pub struct TlsData {
  context:     SslContext,
  certificate: Vec<u8>,
  refcount:    u8,
}

pub struct ServerConfiguration<Tx> {
  listener:        TcpListener,
  address:         SocketAddr,
  instances:       HashMap<AppId, Vec<Backend>>,
  fronts:          HashMap<HostName, Vec<TlsApp>>,
  domains:         Arc<Mutex<TrieNode<CertFingerprint>>>,
  default_cert:    String,
  default_context: TlsData,
  contexts:        Arc<Mutex<HashMap<CertFingerprint,TlsData>>>,
  tx:              Tx,
  pool:            Pool<BufferQueue>,
  answers:         DefaultAnswers,
  front_timeout:   u64,
  back_timeout:    u64,
  config:          TlsProxyConfiguration,
}

impl<Tx: messages::Sender<ServerMessage>> ServerConfiguration<Tx> {
  pub fn new(config: TlsProxyConfiguration, tx: Tx, event_loop: &mut Poll, start_at: usize) -> io::Result<ServerConfiguration<Tx>> {
    let mut contexts:HashMap<CertFingerprint,TlsData> = HashMap::new();
    let mut domains  = TrieNode::root();

    /*let ctx = SslContext::new(SslMethod::Sslv23);
    if let Err(e) = ctx {
      return Err(io::Error::new(io::ErrorKind::Other, e.description()));
    }

    let mut context = ctx.unwrap();

    context.set_cipher_list(&config.cipher_list);
    context.set_cipher_list("ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
                               ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
                               ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
                               DHE-RSA-AES128-GCM-SHA256:DHE-RSA-AES256-GCM-SHA384:\
                               ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA256:\
                               ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES256-SHA384:\
                               ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA384:\
                               ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:DHE-RSA-AES128-SHA256:\
                               DHE-RSA-AES128-SHA:DHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA:\
                               ECDHE-ECDSA-DES-CBC3-SHA:ECDHE-RSA-DES-CBC3-SHA:\
                               EDH-RSA-DES-CBC3-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:\
                               AES128-SHA256:AES256-SHA256:AES128-SHA:AES256-SHA:DES-CBC3-SHA:\
                               !DSS");
    //context.set_options(ssl::SSL_OP_CIPHER_SERVER_PREFERENCE | ssl::SSL_OP_NO_COMPRESSION |
    //                                      ssl::SSL_OP_NO_TICKET | ssl::SSL_OP_NO_SSLV2);// |
                                          //ssl::SSL_OP_NO_SSLV3 | ssl::SSL_OP_NO_TLSV1);
    //if let Some(tls_options) = SslContextOptions::from_bits(config.options) {
    //  context.set_options(tls_options);
    //}

    match DH::get_2048_256() {
      Ok(dh) => context.set_tmp_dh(&dh),
      Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e.description()))
    };

    context.set_ecdh_auto(true);

    //FIXME: get the default cert and key from the configuration
    context.set_certificate_file("assets/certificate.pem", X509FileType::PEM);
    context.set_private_key_file("assets/key.pem", X509FileType::PEM);
    */
    let (fingerprint, mut tls_data):(Vec<u8>,TlsData) = Self::create_default_context(&config.cipher_list).unwrap();

    let rc_ctx = Arc::new(Mutex::new(contexts));
    let ref_ctx = rc_ctx.clone();
    let rc_domains  = Arc::new(Mutex::new(domains));
    let ref_domains = rc_domains.clone();

    tls_data.context.set_servername_callback(move |ssl: &mut SslRef| {
      let contexts = ref_ctx.lock().unwrap();
      let domains  = ref_domains.lock().unwrap();

      info!("TLS ref: {:?}", ssl);
      if let Some(servername) = ssl.servername() {
        info!("TLS\tlooking for fingerprint for {:?}", servername);
        if let Some(kv) = domains.domain_lookup(servername.as_bytes()) {
          info!("TLS\tlooking for context for {:?} with fingerprint {:?}", servername, kv.0);
          if let Some(ref tls_data) = contexts.get(&kv.0) {
            info!("TLS\tfound context for {:?}", servername);
            let context: &SslContext = &tls_data.context;
            if let Ok(()) = ssl.set_ssl_context(context) {
              info!("TLS\tservername is now {:?}", ssl.servername());
              return Ok(());
            } else {
              error!("no context found for {:?}", servername);
            }
          }
        }
      } else {
        error!("got no server name from ssl");
      }
      Err(SniError::Fatal(0))
    });

    /*
    {
      let mut ctxts = rc_ctx.lock().unwrap();
      ctxts.insert("lolcatho.st", context);
    }
    */


    match server_bind(&config.front) {
      Ok(listener) => {
        event_loop.register(&listener, Token(start_at), Ready::readable(), PollOpt::level());
        Ok(ServerConfiguration {
          listener:        listener,
          address:         config.front.clone(),
          instances:       HashMap::new(),
          fronts:          HashMap::new(),
          domains:         rc_domains,
          default_cert:    String::from("lolcatho.st"),
          default_context: tls_data,
          contexts:        rc_ctx,
          tx:              tx,
          pool:            Pool::with_capacity(2*config.max_connections, 0, || BufferQueue::with_capacity(config.buffer_size)),
          front_timeout:   50000,
          back_timeout:    50000,
          answers:         DefaultAnswers {
            NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]),
            ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]),
          },
          config:          config,
        })
      },
      Err(e) => {
        error!("TLS\tcould not create listener {:?}: {:?}", config.front, e);
        Err(e)
      }
    }
  }

  pub fn create_default_context(cipher_list: &str) -> Option<(CertFingerprint,TlsData)> {
    let ctx = SslContext::new(SslMethod::Sslv23);
    if let Err(e) = ctx {
      //return Err(io::Error::new(io::ErrorKind::Other, e.description()));
      return None
    }

    let mut context = ctx.unwrap();

    context.set_cipher_list(cipher_list);
    context.set_cipher_list("ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
                               ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
                               ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
                               DHE-RSA-AES128-GCM-SHA256:DHE-RSA-AES256-GCM-SHA384:\
                               ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA256:\
                               ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES256-SHA384:\
                               ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA384:\
                               ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:DHE-RSA-AES128-SHA256:\
                               DHE-RSA-AES128-SHA:DHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA:\
                               ECDHE-ECDSA-DES-CBC3-SHA:ECDHE-RSA-DES-CBC3-SHA:\
                               EDH-RSA-DES-CBC3-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:\
                               AES128-SHA256:AES256-SHA256:AES128-SHA:AES256-SHA:DES-CBC3-SHA:\
                               !DSS");
    //context.set_options(ssl::SSL_OP_CIPHER_SERVER_PREFERENCE | ssl::SSL_OP_NO_COMPRESSION |
    //                                      ssl::SSL_OP_NO_TICKET | ssl::SSL_OP_NO_SSLV2);// |
                                          //ssl::SSL_OP_NO_SSLV3 | ssl::SSL_OP_NO_TLSV1);
    /*if let Some(tls_options) = SslContextOptions::from_bits(config.options) {
      context.set_options(tls_options);
    }*/

    match DH::get_2048_256() {
      Ok(dh) => context.set_tmp_dh(&dh),
      Err(e) => {
        //return Err(io::Error::new(io::ErrorKind::Other, e.description()))
        return None
      }
    };

    context.set_ecdh_auto(true);

    //FIXME: get the default cert and key from the configuration
    //context.set_certificate_file("assets/certificate.pem", X509FileType::PEM);
    //context.set_private_key_file("assets/key.pem", X509FileType::PEM);
    let cert_read = include_bytes!("../../assets/certificate.pem");
    let key_read  = include_bytes!("../../assets/key.pem");
    if let (Ok(cert), Ok(key)) = (X509::from_pem(&cert_read[..]), PKey::private_key_from_pem(&key_read[..])) {
      if let Ok(fingerprint) = cert.fingerprint(Type::SHA256) {
        context.set_certificate(&cert);
        context.set_private_key(&key);

        let tls_data = TlsData {
          context:     context,
          certificate: cert_read.to_vec(),
          refcount:    0,
        };
        Some((fingerprint, tls_data))
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn add_http_front(&mut self, http_front: TlsFront, event_loop: &mut Poll) -> bool {
    //FIXME: insert some error management with a Result here
    let c = SslContext::new(SslMethod::Sslv23);
    if c.is_err() { return false; }
    let mut ctx = c.unwrap();

    let mut cert_read  = &http_front.certificate.as_bytes()[..];
    let mut key_read   = &http_front.key.as_bytes()[..];
    let cert_chain: Vec<X509> = http_front.certificate_chain.iter().filter_map(|c| {
      X509::from_pem(c.as_bytes()).ok()
    }).collect();

    if let (Ok(cert), Ok(key)) = (X509::from_pem(&mut cert_read), PKey::private_key_from_pem(&mut key_read)) {
      //FIXME: would need more logs here

      //FIXME
      let fingerprint = cert.fingerprint(Type::SHA256).unwrap();
      let common_name: Option<String> = cert.subject_name().text_by_nid(Nid::CN).map(|name| String::from(&*name));
      info!("got common name: {:?}", common_name);

      let names: Vec<String> = cert.subject_alt_names().map(|names| {
        names.iter().filter_map(|general_name|
          general_name.dnsname().map(|name| String::from(name))
        ).collect()
      }).unwrap_or(vec!());
      info!("got subject alt names: {:?}", names);

      ctx.set_certificate(&cert);
      ctx.set_private_key(&key);
      cert_chain.iter().map(|ref cert| ctx.add_extra_chain_cert(cert));

      let tls_data = TlsData {
        context:     ctx,
        certificate: cert_read.to_vec(),
        refcount:    1,
      };
      // if the name or the fingerprint are already used,
      // those insertions should fail, because it would be
      // from the same certificate
      // Add a refcount?
      //FIXME: this is blocking
      //this lock is only obtained from this thread, so is it alright?
      {
        let mut contexts = self.contexts.lock().unwrap();

        if contexts.contains_key(&fingerprint) {
          contexts.get_mut(&fingerprint).map(|data| {
            data.refcount += 1;
          });
        }  else {
          contexts.insert(fingerprint.clone(), tls_data);
        }
      }
      {
        let mut domains = self.domains.lock().unwrap();
        if let Some(name) = common_name {
          domains.domain_insert(name.into_bytes(), fingerprint.clone());
        }
        for name in names {
          domains.domain_insert(name.into_bytes(), fingerprint.clone());
        }
      }

      let app = TlsApp {
        app_id:           http_front.app_id.clone(),
        hostname:         http_front.hostname.clone(),
        path_begin:       http_front.path_begin.clone(),
        cert_fingerprint: fingerprint.clone(),
      };

      if let Some(fronts) = self.fronts.get_mut(&http_front.hostname) {
          fronts.push(app.clone());
      }
      if self.fronts.get(&http_front.hostname).is_none() {
        self.fronts.insert(http_front.hostname, vec![app]);
      }

      true
    } else {
      false
    }
  }

  pub fn remove_http_front(&mut self, front: TlsFront, event_loop: &mut Poll) {
    info!("TLS\tremoving http_front {:?}", front);

    if let Some(fronts) = self.fronts.get_mut(&front.hostname) {
      if let Some(pos) = fronts.iter().position(|f| &f.app_id == &front.app_id) {
        let front = fronts.remove(pos);

        {
          let mut contexts = self.contexts.lock().unwrap();
          let mut domains  = self.domains.lock().unwrap();
          let must_delete = contexts.get_mut(&front.cert_fingerprint).map(|tls_data| {
            tls_data.refcount -= 1;
            tls_data.refcount == 0
          });

          if must_delete == Some(true) {
            if let Some(data) = contexts.remove(&front.cert_fingerprint) {
              if let Ok(cert) = X509::from_pem(&data.certificate) {
                let common_name: Option<String> = cert.subject_name().text_by_nid(Nid::CN).map(|name| String::from(&*name));
                //info!("got common name: {:?}", common_name);
                if let Some(name) = common_name {
                  domains.domain_remove(&name.into_bytes());
                }

                let names: Vec<String> = cert.subject_alt_names().map(|names| {
                  names.iter().filter_map(|general_name|
                                          general_name.dnsname().map(|name| String::from(name))
                                         ).collect()
                }).unwrap_or(vec!());
                for name in names {
                  domains.domain_remove(&name.into_bytes());
                }
              }
            }
          }
        }
      }
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) {
    if let Some(addrs) = self.instances.get_mut(app_id) {
      let backend = Backend::new(*instance_address);
      addrs.push(backend);
    }

    if self.instances.get(app_id).is_none() {
      let backend = Backend::new(*instance_address);
      self.instances.insert(String::from(app_id), vec![backend]);
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) {
      if let Some(instances) = self.instances.get_mut(app_id) {
        instances.retain(|backend| &backend.address != instance_address);
      } else {
        error!("TLS\tInstance was already removed");
      }
  }

  // ToDo factor out with http.rs
  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<&TlsApp> {
    if let Some(http_fronts) = self.fronts.get(host) {
      let matching_fronts = http_fronts.iter().filter(|f| uri.starts_with(&f.path_begin)); // ToDo match on uri
      let mut front = None;

      for f in matching_fronts {
        if front.is_none() {
          front = Some(f);
        }

        if let Some(ff) = front {
          if f.path_begin.len() > ff.path_begin.len() {
            front = Some(f)
          }
        }
      }
      front
    } else {
      None
    }
  }

  pub fn backend_from_request(&mut self, client: &mut TlsClient, host: &str, uri: &str) -> Result<TcpStream,ConnectionError> {
    trace!("TLS\tlooking for backend for host: {}", host);
    let real_host = if let Some(h) = host.split(":").next() {
      h
    } else {
      host
    };
    trace!("TLS\tlooking for backend for real host: {}", host);

    if let Some(app_id) = self.frontend_from_request(real_host, uri).map(|ref front| front.app_id.clone()) {
      client.http.as_mut().unwrap().app_id = Some(app_id.clone());
      // ToDo round-robin on instances
      if let Some(ref mut app_instances) = self.instances.get_mut(&app_id) {
        if app_instances.len() == 0 {
          client.http.as_mut().unwrap().set_answer(&self.answers.ServiceUnavailable);
          return Err(ConnectionError::NoBackendAvailable);
        }
        let rnd = random::<usize>();
        let mut instances:Vec<&mut Backend> = app_instances.iter_mut().filter(|backend| backend.can_open()).collect();
        let idx = rnd % instances.len();
        info!("{}\tConnecting {} -> {:?}", client.http.as_mut().unwrap().log_context(), host, instances.get(idx).map(|backend| (backend.address, backend.active_connections)));
        instances.get_mut(idx).ok_or(ConnectionError::NoBackendAvailable).and_then(|ref mut backend| {
          let conn =  TcpStream::connect(&backend.address).map_err(|_| ConnectionError::NoBackendAvailable);
          if conn.is_ok() {
             backend.inc_connections();
          }
          conn
         })
      } else {
        Err(ConnectionError::NoBackendAvailable)
      }
    } else {
      Err(ConnectionError::HostNotFound)
    }
  }
}

impl<Tx: messages::Sender<ServerMessage>> ProxyConfiguration<TlsClient> for ServerConfiguration<Tx> {
  fn accept(&mut self, token: ListenToken) -> Option<(TlsClient,bool)> {
    if let (Some(front_buf), Some(back_buf)) = (self.pool.checkout(), self.pool.checkout()) {
      let accepted = self.listener.accept();

      if let Ok((frontend_sock, _)) = accepted {
        frontend_sock.set_nodelay(true);
        if let Ok(ssl) = Ssl::new(&self.default_context.context) {
          if let Some(c) = TlsClient::new("TLS", ssl, frontend_sock, front_buf, back_buf) {
            return Some((c, false))
          }
        } else {
          error!("TLS\tcould not create ssl context");
        }
      } else {
        error!("TLS\tcould not accept connection: {:?}", accepted);
      }
    } else {
      error!("TLS\tcould not get buffers from pool");
    }
    None
  }

  fn connect_to_backend(&mut self, event_loop: &mut Poll, client: &mut TlsClient) -> Result<BackendConnectAction,ConnectionError> {
    let h = try!(client.http.as_mut().unwrap().state().get_host().ok_or(ConnectionError::NoHostGiven));

    let host: &str = if let IResult::Done(i, (hostname, port)) = hostname_and_port(h.as_bytes()) {
      if i != &b""[..] {
        error!("invalid remaining chars after hostname");
        return Err(ConnectionError::ToBeDefined);
      }

      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
      let hostname_str =  unsafe { from_utf8_unchecked(hostname) };

      //FIXME: what if we don't use SNI?
      let servername: Option<String> = client.http.as_ref().unwrap().frontend.ssl().servername();
      if servername.as_ref().map(|s| s.as_str()) != Some(hostname_str) {
        error!("TLS SNI hostname and Host header don't match");
        return Err(ConnectionError::HostNotFound);
      }

      //FIXME: we should check that the port is right too

      if port == Some(&b"443"[..]) {
        hostname_str
      } else {
        &h
      }
    } else {
      error!("hostname parsing failed");
      return Err(ConnectionError::ToBeDefined);
    };

    let rl:RRequestLine = try!(client.http.as_mut().unwrap().state().get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
    let conn   = try!(client.http.as_mut().unwrap().state().get_front_keep_alive().ok_or(ConnectionError::ToBeDefined));
    let conn   = self.backend_from_request(client, &host, &rl.uri);

    match conn {
      Ok(socket) => {
        let req_state = client.http.as_mut().unwrap().state().request.clone();
        let req_header_end = client.http.as_mut().unwrap().state().req_header_end;
        let res_header_end = client.http.as_mut().unwrap().state().res_header_end;
        let added_req_header = client.http.as_mut().unwrap().state().added_req_header.clone();
        let added_res_header = client.http.as_mut().unwrap().state().added_res_header.clone();
        // FIXME: is this still needed?
        client.http.as_mut().unwrap().set_state(HttpState {
          req_header_end: req_header_end,
          res_header_end: res_header_end,
          request:  req_state,
          response: Some(ResponseState::Initial),
          added_req_header: added_req_header,
          added_res_header: added_res_header,
        });

        client.set_back_socket(socket);
        //FIXME: implement keepalive
        Ok(BackendConnectAction::New)
      },
      Err(ConnectionError::NoBackendAvailable) => {
        client.http.as_mut().unwrap().set_answer(&self.answers.ServiceUnavailable);
        Err(ConnectionError::NoBackendAvailable)
      },
      Err(ConnectionError::HostNotFound) => {
        client.http.as_mut().unwrap().set_answer(&self.answers.NotFound);
        Err(ConnectionError::HostNotFound)
      },
      e => panic!(e)
    }
  }

  fn notify(&mut self, event_loop: &mut Poll, message: ProxyOrder) {
    trace!("TLS\t{} notified", message);
    match message {
      ProxyOrder::Command(id, Command::AddTlsFront(front)) => {
        info!("TLS\t{} add front {:?}", id, front);
          self.add_http_front(front, event_loop);
          self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::AddedFront});
      },
      ProxyOrder::Command(id, Command::RemoveTlsFront(front)) => {
        info!("TLS\t{} remove front {:?}", id, front);
        self.remove_http_front(front, event_loop);
        self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::RemovedFront});
      },
      ProxyOrder::Command(id, Command::AddInstance(instance)) => {
        info!("TLS\t{} add instance {:?}", id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &addr, event_loop);
          self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::AddedInstance});
        } else {
          self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("cannot parse the address"))});
        }
      },
      ProxyOrder::Command(id, Command::RemoveInstance(instance)) => {
        info!("TLS\t{} remove instance {:?}", id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_instance(&instance.app_id, &addr, event_loop);
          self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::RemovedInstance});
        } else {
          self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("cannot parse the address"))});
        }
      },
      ProxyOrder::Command(id, Command::HttpProxy(configuration)) => {
        info!("TLS\t{} modifying proxy configuration: {:?}", id, configuration);
        self.front_timeout = configuration.front_timeout;
        self.back_timeout  = configuration.back_timeout;
        self.answers = DefaultAnswers {
          NotFound:           configuration.answer_404.into_bytes(),
          ServiceUnavailable: configuration.answer_503.into_bytes(),
        };
      },
      ProxyOrder::Stop(id)                   => {
        info!("HTTP\t{} shutdown", id);
        //FIXME: handle shutdown
        //event_loop.shutdown();
        self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::Stopped});
      },
      ProxyOrder::Command(id, msg) => {
        error!("TLS\t{} unsupported message, ignoring {:?}", id, msg);
        self.tx.send_message(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("unsupported message"))});
      }
    }
  }

  fn close_backend(&mut self, app_id: AppId, addr: &SocketAddr) {
    if let Some(app_instances) = self.instances.get_mut(&app_id) {
      if let Some(ref mut backend) = app_instances.iter_mut().find(|backend| &backend.address == addr) {
        backend.dec_connections();
      }
    }
  }

  fn front_timeout(&self) -> u64 {
    self.front_timeout
  }

  fn back_timeout(&self)  -> u64 {
    self.back_timeout
  }
}

pub type TlsServer<Tx,Rx> = Server<ServerConfiguration<Tx>,TlsClient,Rx>;

pub fn start_listener<Tx,Rx>(config: TlsProxyConfiguration, tx: Tx, mut event_loop: Poll, receiver: Rx)
  where Tx: messages::Sender<ServerMessage>,
        Rx: Evented+messages::Receiver<ProxyOrder> {

  let max_connections = config.max_connections;
  let max_listeners   = 1;
  // start at max_listeners + 1 because token(0) is the channel, and token(1) is the timer
  let configuration = ServerConfiguration::new(config, tx, &mut event_loop, 1 + max_listeners).unwrap();
  let mut server = TlsServer::new(max_listeners, max_connections, configuration, event_loop, receiver);

  info!("TLS\tstarting event loop");
  server.run();
  //event_loop.run(&mut server).unwrap();
  info!("TLS\tending event loop");
}

#[cfg(test)]
mod tests {
  extern crate tiny_http;
  use super::*;
  use std::collections::HashMap;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::mpsc::channel;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::time::Duration;
  use std::rc::{Rc,Weak};
  use std::sync::{Arc,Mutex};
  use std::cell::RefCell;
  use messages::{Command,TlsFront,Instance};
  use slab::Slab;
  use pool::Pool;
  use network::buffer::Buffer;
  use network::buffer_queue::BufferQueue;
  use network::{ProxyOrder,ServerMessage};
  use network::http::DefaultAnswers;
  use network::trie::TrieNode;
  use openssl::ssl::{SslContext, SslMethod, Ssl, SslStream};
  use openssl::x509::X509;

  /*
  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let (sender, jg) = start_listener(front, 10, 10, tx.clone());
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:1024"), path_begin: String::from("/") };
    sender.send(ProxyOrder::Command(Command::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1025 };
    sender.send(ProxyOrder::Command(Command::AddInstance(instance)));
    println!("test received: {:?}", rx.recv());
    println!("test received: {:?}", rx.recv());
    thread::sleep_ms(300);

    let mut client = TcpStream::connect(("127.0.0.1", 1024)).unwrap();
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));
    thread::sleep_ms(100);
    let mut w  = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1024\r\nConnection: Close\r\n\r\n"[..]);
    println!("http client write: {:?}", w);
    let mut buffer = [0;4096];
    thread::sleep_ms(500);
    let mut r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        // Read the Response.
        println!("read response");

        println!("Response: {}", str::from_utf8(&buffer[..]).unwrap());

        //thread::sleep_ms(300);
        //assert_eq!(&body, &"Hello World!"[..]);
        assert_eq!(sz, 154);
        //assert!(false);
      }
    }
  }

  use self::tiny_http::{ServerBuilder, Response};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    thread::spawn(move|| {
      let server = ServerBuilder::new().with_port(1025).build().unwrap();
      println!("starting web server");

      for request in server.incoming_requests() {
        println!("backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
          request.method(),
          request.url(),
          request.headers()
        );

        let response = Response::from_string("hello world");
        request.respond(response);
        println!("backend web server sent response");
      }
    });
  }
*/

  use mio::tcp;
  #[test]
  fn frontend_from_request_test() {
    let app_id1 = "app_1".to_owned();
    let app_id2 = "app_2".to_owned();
    let app_id3 = "app_3".to_owned();
    let uri1 = "/".to_owned();
    let uri2 = "/yolo".to_owned();
    let uri3 = "/yolo/swag".to_owned();

    let mut fronts = HashMap::new();
    fronts.insert("lolcatho.st".to_owned(), vec![
      TlsApp {
        app_id: app_id1, hostname: "lolcatho.st".to_owned(), path_begin: uri1,
        cert_fingerprint: vec!()
      },
      TlsApp {
        app_id: app_id2, hostname: "lolcatho.st".to_owned(), path_begin: uri2,
        cert_fingerprint: vec!()
      },
      TlsApp {
        app_id: app_id3, hostname: "lolcatho.st".to_owned(), path_begin: uri3,
        cert_fingerprint: vec!()
      }
    ]);
    fronts.insert("other.domain".to_owned(), vec![
      TlsApp {
        app_id: "app_1".to_owned(), hostname: "other.domain".to_owned(), path_begin: "/test".to_owned(),
        cert_fingerprint: vec!()
      },
    ]);

    let contexts   = HashMap::new();
    let rc_ctx     = Arc::new(Mutex::new(contexts));
    let domains    = TrieNode::root();
    let rc_domains = Arc::new(Mutex::new(domains));

    let context    = SslContext::new(SslMethod::Tlsv1).unwrap();
    let (tx,rx)    = channel::<ServerMessage>();

    let tls_data = TlsData {
      context:     context,
      certificate: vec!(),
      refcount:    0,
    };

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1032").expect("test address 127.0.0.1:1032 should be parsed");
    let listener = tcp::TcpListener::bind(&front).expect("test address 127.0.0.1:1032 should be available");
    let server_config = ServerConfiguration {
      listener:  listener,
      address:   front,
      instances: HashMap::new(),
      fronts:    fronts,
      domains:   rc_domains,
      default_cert: "".to_owned(),
      default_context: tls_data,
      contexts: rc_ctx,
      tx:        tx,
      pool:      Pool::with_capacity(1, 0, || BufferQueue::with_capacity(12000)),
      front_timeout: 5000,
      back_timeout:  5000,
      answers:   DefaultAnswers {
        NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\n\r\n"[..]),
        ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\n\r\n"[..]),
      },
      config: Default::default()
    };

    println!("TEST {}", line!());
    let frontend1 = server_config.frontend_from_request("lolcatho.st", "/");
    assert_eq!(frontend1.unwrap().app_id, "app_1");
    println!("TEST {}", line!());
    let frontend2 = server_config.frontend_from_request("lolcatho.st", "/test");
    assert_eq!(frontend2.unwrap().app_id, "app_1");
    println!("TEST {}", line!());
    let frontend3 = server_config.frontend_from_request("lolcatho.st", "/yolo/test");
    assert_eq!(frontend3.unwrap().app_id, "app_2");
    println!("TEST {}", line!());
    let frontend4 = server_config.frontend_from_request("lolcatho.st", "/yolo/swag");
    assert_eq!(frontend4.unwrap().app_id, "app_3");
    println!("TEST {}", line!());
    let frontend5 = server_config.frontend_from_request("domain", "/");
    assert_eq!(frontend5, None);
   // assert!(false);
  }
}
