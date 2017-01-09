use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::sync::{Arc,Mutex};
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::mem;
use mio::*;
use mio::tcp::*;
use mio::timer::Timeout;
use mio_uds::UnixStream;
use std::io::{self,Read,Write,ErrorKind,BufReader};
use std::collections::HashMap;
use std::error::Error;
use slab::Slab;
use pool::{Pool,Checkout};
use std::net::{IpAddr,SocketAddr};
use std::str::{FromStr, from_utf8, from_utf8_unchecked};
use time::{precise_time_s, precise_time_ns};
use rand::random;
use openssl::ssl::{self, SslContext, SslContextBuilder, SslMethod,
                   Ssl, SslRef, SslStream, SniError};
use openssl::x509::{X509,X509FileType};
use openssl::dh::Dh;
use openssl::pkey::PKey;
use openssl::hash::MessageDigest;
use openssl::nid;
use nom::IResult;

use parser::http11::{HttpState,RequestState,ResponseState,RRequestLine,parse_request_until_stop,hostname_and_port};
use network::buffer::Buffer;
use network::buffer_queue::BufferQueue;
use network::{Backend,ClientResult,ServerMessage,ServerMessageStatus,ConnectionError,ProxyOrder,Protocol};
use network::proxy::{BackendConnectAction,Server,ProxyConfiguration,ProxyClient,
  Readiness,ListenToken,FrontToken,BackToken,Channel};
use messages::{self,Command,TlsFront,TlsProxyConfiguration};
use network::http::{self,DefaultAnswers};
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::trie::*;
use network::protocol::{ProtocolResult,TlsHandshake,Http,Pipe};

use command::CommandChannel;

type BackendToken = Token;

type ClientToken = Token;

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct TlsApp {
  pub app_id:           String,
  pub hostname:         String,
  pub path_begin:       String,
  pub cert_fingerprint: CertFingerprint,
}

pub enum State {
  Handshake(TlsHandshake),
  Http(Http<SslStream<TcpStream>>),
  WebSocket(Pipe<SslStream<TcpStream>>)
}

pub struct TlsClient {
  front:          Option<TcpStream>,
  front_token:    Option<Token>,
  front_timeout:  Option<Timeout>,
  back_timeout:   Option<Timeout>,
  protocol:       Option<State>,
  public_address: Option<IpAddr>,
  ssl:            Option<Ssl>,
  pool:           Weak<RefCell<Pool<BufferQueue>>>,
}

impl TlsClient {
  pub fn new(server_context: &str, ssl:Ssl, sock: TcpStream, pool: Weak<RefCell<Pool<BufferQueue>>>, public_address: Option<IpAddr>) -> Option<TlsClient> {
    //FIXME: we should not need to clone the socket. Maybe do the accept here instead of
    // in TlsHandshake?
    let s = sock.try_clone().unwrap();
    let handshake = TlsHandshake::new(server_context, ssl, s);
    Some(TlsClient {
      front:          Some(sock),
      front_token:    None,
      front_timeout:  None,
      back_timeout:   None,
      protocol:       Some(State::Handshake(handshake)),
      public_address: public_address,
      ssl:            None,
      pool:           pool,
    })
  }

  pub fn http(&mut self) -> Option<&mut Http<SslStream<TcpStream>>> {
    if let State::Http(ref mut http) = *self.protocol.as_mut().unwrap() {
      Some(http)
    } else {
      None
    }
  }

  pub fn upgrade(&mut self) -> bool {
    let protocol = self.protocol.take().unwrap();

    if let State::Handshake(handshake) = protocol {
      if let Some(pool) = self.pool.upgrade() {
        let mut p = pool.borrow_mut();

        if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
          let mut http = Http::new(&handshake.server_context, handshake.stream.unwrap(), front_buf,
            back_buf, self.public_address.clone()).unwrap();

          http.readiness = handshake.readiness;
          http.readiness.front_interest = Ready::readable() | Ready::hup() | Ready::error();
          http.set_front_token(self.front_token.as_ref().unwrap().clone());
          self.ssl = handshake.ssl;
          self.protocol = Some(State::Http(http));
          return true;
        } else {
          error!("could not get buffers");
          //FIXME: must return an error and stop the connection here
        }
      }
      false
    } else if let State::Http(http) = protocol {
      info!("https switching to wss");
      let front_token = http.front_token().unwrap();
      let back_token  = http.back_token().unwrap();

      let mut pipe = Pipe::new(&http.server_context, http.frontend, http.backend.unwrap(),
        http.front_buf, http.back_buf, http.public_address).unwrap();

      pipe.readiness.front_readiness = http.readiness.front_readiness;
      pipe.readiness.back_readiness  = http.readiness.back_readiness;
      pipe.set_front_token(front_token);
      pipe.set_back_token(back_token);

      self.protocol = Some(State::WebSocket(pipe));
      true
    } else {
      self.protocol = Some(protocol);
      true
    }
  }
}

impl ProxyClient for TlsClient {
  fn front_socket(&self) -> &TcpStream {
    self.front.as_ref().unwrap()
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    match *self.protocol.as_ref().unwrap() {
      State::Handshake(ref handshake) => None,
      State::Http(ref http)           => http.back_socket(),
      State::WebSocket(ref pipe)      => pipe.back_socket(),
    }
  }

  fn front_token(&self)  -> Option<Token> {
    self.front_token
  }

  fn back_token(&self)   -> Option<Token> {
    if let State::Http(ref http) = *self.protocol.as_ref().unwrap() {
      http.back_token()
    } else {
      None
    }
  }

  fn close(&mut self) {
    //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.token, *self.temp.front_buf, *self.temp.back_buf);
    self.http().map(|http| http.close());
  }

  fn log_context(&self)  -> String {
    if let State::Http(ref http) = *self.protocol.as_ref().unwrap() {
      http.log_context()
    } else {
      "".to_string()
    }
  }

  fn set_back_socket(&mut self, sock:TcpStream) {
    self.http().unwrap().set_back_socket(sock)
  }

  fn set_front_token(&mut self, token: Token) {
    self.front_token = Some(token);
    self.protocol.as_mut().map(|p| match *p {
      State::Http(ref mut http) => http.set_front_token(token),
      _                         => {}
    });
  }

  fn set_back_token(&mut self, token: Token) {
    self.http().unwrap().set_back_token(token)
  }

  fn front_timeout(&mut self) -> Option<Timeout> {
    self.front_timeout.clone()
  }

  fn back_timeout(&mut self)  -> Option<Timeout> {
    self.back_timeout.clone()
  }

  fn set_front_timeout(&mut self, timeout: Timeout) {
    self.front_timeout = Some(timeout)
  }

  fn set_back_timeout(&mut self, timeout: Timeout) {
    self.back_timeout = Some(timeout);
  }

  fn front_hup(&mut self)     -> ClientResult {
    self.http().unwrap().front_hup()
  }

  fn back_hup(&mut self)      -> ClientResult {
    self.http().unwrap().back_hup()
  }

  fn readable(&mut self)      -> ClientResult {
    let (upgrade, result) = match *self.protocol.as_mut().unwrap() {
      State::Handshake(ref mut handshake) => handshake.readable(),
      State::Http(ref mut http)           => (ProtocolResult::Continue, http.readable()),
      State::WebSocket(ref mut pipe)      => (ProtocolResult::Continue, pipe.readable()),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else {
        if self.upgrade() {
        match *self.protocol.as_mut().unwrap() {
          State::Http(ref mut http) => http.readable(),
          _ => result
        }
      } else {
        ClientResult::CloseClient
      }
    }
  }

  fn writable(&mut self)      -> ClientResult {
    match *self.protocol.as_mut().unwrap() {
      State::Handshake(ref mut handshake) => ClientResult::CloseClient,
      State::Http(ref mut http)           => http.writable(),
      State::WebSocket(ref mut pipe)      => pipe.writable(),
    }
  }

  fn back_readable(&mut self) -> ClientResult {
    let (upgrade, result) = match *self.protocol.as_mut().unwrap() {
      State::Http(ref mut http)           => http.back_readable(),
      State::Handshake(ref mut handshake) => (ProtocolResult::Continue, ClientResult::CloseClient),
      State::WebSocket(ref mut pipe)      => (ProtocolResult::Continue, pipe.back_readable()),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else {
      if self.upgrade() {
      match *self.protocol.as_mut().unwrap() {
        State::WebSocket(ref mut pipe) => pipe.back_readable(),
        _ => result
      }
      } else {
        ClientResult::CloseBothFailure
      }
    }
  }

  fn back_writable(&mut self) -> ClientResult {
    //self.http().unwrap().back_writable()
    match *self.protocol.as_mut().unwrap() {
      State::Handshake(ref mut handshake) => ClientResult::CloseClient,
      State::Http(ref mut http)           => http.back_writable(),
      State::WebSocket(ref mut pipe)      => pipe.back_writable(),
    }
  }

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    self.http().unwrap().remove_backend()
  }

  fn readiness(&mut self)      -> &mut Readiness {
    let r = match *self.protocol.as_mut().unwrap() {
      State::Handshake(ref mut handshake) => &mut handshake.readiness,
      State::Http(ref mut http)           => http.readiness(),
      State::WebSocket(ref mut pipe)      => &mut pipe.readiness,
    };
    //info!("current readiness: {:?}", r);
    r
  }

  fn protocol(&self)           -> Protocol {
    Protocol::TLS
  }
}

fn get_cert_common_name(cert: &X509) -> Option<String> {
    cert.subject_name().entries_by_nid(nid::COMMONNAME).next().and_then(|name| name.data().as_utf8().ok().map(|name| String::from(&*name)))
}

pub type AppId     = String;
pub type HostName  = String;
pub type PathBegin = String;
//maybe not the most efficient type for that
pub type CertFingerprint = Vec<u8>;
pub struct TlsData {
  context:     SslContext,
  certificate: Vec<u8>,
  refcount:    usize,
}

pub struct ServerConfiguration {
  listener:        TcpListener,
  address:         SocketAddr,
  instances:       HashMap<AppId, Vec<Backend>>,
  fronts:          HashMap<HostName, Vec<TlsApp>>,
  domains:         Arc<Mutex<TrieNode<CertFingerprint>>>,
  default_context: TlsData,
  contexts:        Arc<Mutex<HashMap<CertFingerprint,TlsData>>>,
  channel:         Channel,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  answers:         DefaultAnswers,
  front_timeout:   u64,
  back_timeout:    u64,
  config:          TlsProxyConfiguration,
  tag:             String,
}

impl ServerConfiguration {
  pub fn new(tag: String, config: TlsProxyConfiguration, channel: Channel, event_loop: &mut Poll, start_at: usize) -> io::Result<ServerConfiguration> {
    let contexts:HashMap<CertFingerprint,TlsData> = HashMap::new();
    let mut domains  = TrieNode::root();
    let mut fronts   = HashMap::new();
    let rc_ctx       = Arc::new(Mutex::new(contexts));
    let ref_ctx      = rc_ctx.clone();
    let rc_domains   = Arc::new(Mutex::new(domains));
    let ref_domains  = rc_domains.clone();
    let cl_tag       = tag.clone();
    let default_name = config.default_name.as_ref().map(|name| name.clone()).unwrap_or(String::new());

    let (fingerprint, mut tls_data, names):(Vec<u8>,TlsData, Vec<String>) = Self::create_default_context(&config, ref_ctx, ref_domains, cl_tag, default_name).unwrap();
    let cert = X509::from_pem(&tls_data.certificate).unwrap();

    let common_name: Option<String> = get_cert_common_name(&cert);
    info!("{}\tgot common name: {:?}", &tag, common_name);

    let app = TlsApp {
      app_id:           config.default_app_id.clone().unwrap_or(String::new()),
      hostname:         config.default_name.clone().unwrap_or(String::new()),
      path_begin:       String::new(),
      cert_fingerprint: fingerprint.clone(),
    };
    fronts.insert(config.default_name.clone().unwrap_or(String::from("")), vec![app]);

    match server_bind(&config.front) {
      Ok(listener) => {
        event_loop.register(&listener, Token(start_at), Ready::readable(), PollOpt::level());
        let default = DefaultAnswers {
          NotFound: Vec::from(if config.answer_404.len() > 0 {
              config.answer_404.as_bytes()
            } else {
              &b"HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
            }),
          ServiceUnavailable: Vec::from(if config.answer_503.len() > 0 {
              config.answer_503.as_bytes()
            } else {
              &b"HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
            }),
        };

        Ok(ServerConfiguration {
          listener:        listener,
          address:         config.front.clone(),
          instances:       HashMap::new(),
          fronts:          fronts,
          domains:         rc_domains,
          default_context: tls_data,
          contexts:        rc_ctx,
          channel:         channel,
          pool:            Rc::new(RefCell::new(
                             Pool::with_capacity(2*config.max_connections, 0, || BufferQueue::with_capacity(config.buffer_size))
          )),
          front_timeout:   50000,
          back_timeout:    50000,
          answers:         default,
          config:          config,
          tag:             tag,
        })
      },
      Err(e) => {
        error!("{}\tcould not create listener {:?}: {:?}", tag, config.front, e);
        Err(e)
      }
    }
  }

  pub fn create_default_context(config: &TlsProxyConfiguration, ref_ctx: Arc<Mutex<HashMap<CertFingerprint,TlsData>>>, ref_domains: Arc<Mutex<TrieNode<CertFingerprint>>>, tag: String, default_name: String) -> Option<(CertFingerprint,TlsData,Vec<String>)> {
    let ctx = SslContext::builder(SslMethod::tls());
    if let Err(e) = ctx {
      //return Err(io::Error::new(io::ErrorKind::Other, e.description()));
      return None
    }

    let mut context = ctx.unwrap();

    let mut options = context.options();
    options.insert(ssl::SSL_OP_NO_SSLV2);
    options.insert(ssl::SSL_OP_NO_SSLV3);
    options.insert(ssl::SSL_OP_NO_TLSV1);
    options.insert(ssl::SSL_OP_NO_COMPRESSION);
    options.insert(ssl::SSL_OP_NO_TICKET);
    options.insert(ssl::SSL_OP_CIPHER_SERVER_PREFERENCE);
    let opt = context.set_options(options);

    context.set_cipher_list(&config.cipher_list);

    match Dh::get_2048_256() {
      Ok(dh) => context.set_tmp_dh(&dh),
      Err(e) => {
        //return Err(io::Error::new(io::ErrorKind::Other, e.description()))
        return None
      }
    };

    context.set_ecdh_auto(true);

    //FIXME: get the default cert and key from the configuration
    let cert_read = config.default_certificate.as_ref().map(|vec| &vec[..]).unwrap_or(&include_bytes!("../../../assets/certificate.pem")[..]);
    let key_read = config.default_key.as_ref().map(|vec| &vec[..]).unwrap_or(&include_bytes!("../../../assets/key.pem")[..]);
    if let Some(path) = config.default_certificate_chain.as_ref() {
      context.set_certificate_chain_file(path, X509FileType::PEM);
    }

    if let (Ok(cert), Ok(key)) = (X509::from_pem(&cert_read[..]), PKey::private_key_from_pem(&key_read[..])) {
      if let Ok(fingerprint) = cert.fingerprint(MessageDigest::sha256()) {
        context.set_certificate(&cert);
        context.set_private_key(&key);


        let mut names: Vec<String> = cert.subject_alt_names().map(|names| {
          names.iter().filter_map(|general_name|
            general_name.dnsname().map(|name| String::from(name))
          ).collect()
        }).unwrap_or(vec!());

        info!("{}\tgot subject alt names: {:?}", &tag, names);
        {
          let mut domains = ref_domains.lock().unwrap();
          for name in &names {
            domains.domain_insert(name.clone().into_bytes(), fingerprint.clone());
          }
        }

        if let Some(common_name) = get_cert_common_name(&cert) {
        info!("got common name: {:?}", common_name);
          names.push(common_name);
        }

        context.set_servername_callback(move |ssl: &mut SslRef| {
          let contexts = ref_ctx.lock().unwrap();
          let domains  = ref_domains.lock().unwrap();

          info!("{}\tref: {:?}", tag, ssl);
          if let Some(servername) = ssl.servername() {
            info!("checking servername: {}", servername);
            if &servername == &default_name {
              return Ok(());
            }
            info!("{}\tlooking for fingerprint for {:?}", tag, servername);
            if let Some(kv) = domains.domain_lookup(servername.as_bytes()) {
              info!("{}\tlooking for context for {:?} with fingerprint {:?}", tag, servername, kv.1);
              if let Some(ref tls_data) = contexts.get(&kv.1) {
                info!("{}\tfound context for {:?}", tag, servername);
                let context: &SslContext = &tls_data.context;
                if let Ok(()) = ssl.set_ssl_context(context) {
                  info!("{}\tservername is now {:?}", tag, ssl.servername());
                  return Ok(());
                } else {
                  error!("{}\tno context found for {:?}", tag, servername);
                }
              }
            }
          } else {
            error!("{}\tgot no server name from ssl, answering with default one", tag);
          }
          //answer ok because we use the default certificate
          Ok(())
        });

        let tls_data = TlsData {
          context:     context.build(),
          certificate: cert_read.to_vec(),
          refcount:    1,
        };
        Some((fingerprint, tls_data, names))
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn add_http_front(&mut self, http_front: TlsFront, event_loop: &mut Poll) -> bool {
    //FIXME: insert some error management with a Result here
    let c = SslContext::builder(SslMethod::tls());
    if c.is_err() { return false; }
    let mut ctx = c.unwrap();
    let mut options = ctx.options();
    options.insert(ssl::SSL_OP_NO_SSLV2);
    options.insert(ssl::SSL_OP_NO_SSLV3);
    options.insert(ssl::SSL_OP_NO_TLSV1);
    options.insert(ssl::SSL_OP_NO_COMPRESSION);
    options.insert(ssl::SSL_OP_NO_TICKET);
    options.insert(ssl::SSL_OP_CIPHER_SERVER_PREFERENCE);
    let opt = ctx.set_options(options);

    match Dh::get_2048_256() {
      Ok(dh) => ctx.set_tmp_dh(&dh),
      Err(e) => {
        return false;
      }
    };

    ctx.set_ecdh_auto(true);

    let mut cert_read  = &http_front.certificate.as_bytes()[..];
    let mut key_read   = &http_front.key.as_bytes()[..];
    let cert_chain: Vec<X509> = http_front.certificate_chain.iter().filter_map(|c| {
      X509::from_pem(c.as_bytes()).ok()
    }).collect();

    if let (Ok(cert), Ok(key)) = (X509::from_pem(&mut cert_read), PKey::private_key_from_pem(&mut key_read)) {
      //FIXME: would need more logs here

      //FIXME
      let fingerprint = cert.fingerprint(MessageDigest::sha256()).unwrap();
      let common_name: Option<String> = get_cert_common_name(&cert);
      info!("{}\tgot common name: {:?}", self.tag, common_name);

      let names: Vec<String> = cert.subject_alt_names().map(|names| {
        names.iter().filter_map(|general_name|
          general_name.dnsname().map(|name| String::from(name))
        ).collect()
      }).unwrap_or(vec!());
      info!("{}\tgot subject alt names: {:?}", self.tag, names);

      ctx.set_certificate(&cert);
      ctx.set_private_key(&key);
      cert_chain.iter().map(|ref cert| ctx.add_extra_chain_cert(cert));

      let tls_data = TlsData {
        context:     ctx.build(),
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
          if ! fronts.contains(&app) {
            fronts.push(app.clone());
          }
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
    info!("{}\tremoving http_front {:?}", self.tag, front);

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
                let common_name: Option<String> = get_cert_common_name(&cert);
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
      if !addrs.contains(&backend) {
        addrs.push(backend);
      }
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
        error!("{}\tInstance was already removed", self.tag);
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
    trace!("{}\tlooking for backend for host: {}", self.tag, host);
    let real_host = if let Some(h) = host.split(":").next() {
      h
    } else {
      host
    };
    trace!("{}\tlooking for backend for real host: {}", self.tag, real_host);

    if let Some(app_id) = self.frontend_from_request(real_host, uri).map(|ref front| front.app_id.clone()) {
      client.http().unwrap().app_id = Some(app_id.clone());
      // ToDo round-robin on instances
      if let Some(ref mut app_instances) = self.instances.get_mut(&app_id) {
        if app_instances.len() == 0 {
          client.http().unwrap().set_answer(&self.answers.ServiceUnavailable);
          return Err(ConnectionError::NoBackendAvailable);
        }
        let rnd = random::<usize>();
        let mut instances:Vec<&mut Backend> = app_instances.iter_mut().filter(|backend| backend.can_open()).collect();
        let idx = rnd % instances.len();
        info!("{}\tConnecting {} -> {:?}", client.http().unwrap().log_context(), host, instances.get(idx).map(|backend| (backend.address, backend.active_connections)));
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

impl ProxyConfiguration<TlsClient> for ServerConfiguration {
  fn accept(&mut self, token: ListenToken) -> Option<(TlsClient,bool)> {
    let accepted = self.listener.accept();

    if let Ok((frontend_sock, _)) = accepted {
      frontend_sock.set_nodelay(true);
      if let Ok(ssl) = Ssl::new(&self.default_context.context) {
        if let Some(c) = TlsClient::new("TLS", ssl, frontend_sock, Rc::downgrade(&self.pool), self.config.public_address) {
          return Some((c, false))
        }
      } else {
        error!("{}\tcould not create ssl context", self.tag);
      }
    } else {
      error!("{}\tcould not accept connection: {:?}", self.tag, accepted);
    }
    None
  }

  fn connect_to_backend(&mut self, event_loop: &mut Poll, client: &mut TlsClient) -> Result<BackendConnectAction,ConnectionError> {
    let h = try!(client.http().unwrap().state().get_host().ok_or(ConnectionError::NoHostGiven));

    let host: &str = if let IResult::Done(i, (hostname, port)) = hostname_and_port(h.as_bytes()) {
      if i != &b""[..] {
        error!("{}\tinvalid remaining chars after hostname", self.tag);
        return Err(ConnectionError::ToBeDefined);
      }

      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
      let hostname_str =  unsafe { from_utf8_unchecked(hostname) };

      //FIXME: what if we don't use SNI?
      let servername: Option<String> = client.http().unwrap().frontend.ssl().servername();
      if servername.as_ref().map(|s| s.as_str()) != Some(hostname_str) {
        error!("{}\tTLS SNI hostname and Host header don't match", self.tag);
        return Err(ConnectionError::HostNotFound);
      }

      //FIXME: we should check that the port is right too

      if port == Some(&b"443"[..]) {
        hostname_str
      } else {
        &h
      }
    } else {
      error!("{}\thostname parsing failed", self.tag);
      return Err(ConnectionError::ToBeDefined);
    };

    let rl:RRequestLine = try!(client.http().unwrap().state().get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
    let conn   = try!(client.http().unwrap().state().get_front_keep_alive().ok_or(ConnectionError::ToBeDefined));
    let conn   = self.backend_from_request(client, &host, &rl.uri);

    match conn {
      Ok(socket) => {
        let req_state = client.http().unwrap().state().request.clone();
        let req_header_end = client.http().unwrap().state().req_header_end;
        let res_header_end = client.http().unwrap().state().res_header_end;
        let added_req_header = client.http().unwrap().state().added_req_header.clone();
        let added_res_header = client.http().unwrap().state().added_res_header.clone();
        // FIXME: is this still needed?
        client.http().unwrap().set_state(HttpState {
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
        client.http().unwrap().set_answer(&self.answers.ServiceUnavailable);
        Err(ConnectionError::NoBackendAvailable)
      },
      Err(ConnectionError::HostNotFound) => {
        client.http().unwrap().set_answer(&self.answers.NotFound);
        Err(ConnectionError::HostNotFound)
      },
      e => panic!(e)
    }
  }

  fn notify(&mut self, event_loop: &mut Poll, message: ProxyOrder) {
    trace!("{}\t{} notified", self.tag, message);
    match message.command {
      Command::AddTlsFront(front) => {
        //info!("TLS\t{} add front {:?}", id, front);
          self.add_http_front(front, event_loop);
          self.channel.write_message(&ServerMessage{ id: message.id, status: ServerMessageStatus::Ok});
      },
      Command::RemoveTlsFront(front) => {
        //info!("TLS\t{} remove front {:?}", id, front);
        self.remove_http_front(front, event_loop);
        self.channel.write_message(&ServerMessage{ id: message.id, status: ServerMessageStatus::Ok});
      },
      Command::AddInstance(instance) => {
        info!("{}\t{} add instance {:?}", self.tag, message.id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &addr, event_loop);
          self.channel.write_message(&ServerMessage{ id: message.id, status: ServerMessageStatus::Ok});
        } else {
          self.channel.write_message(&ServerMessage{ id: message.id, status: ServerMessageStatus::Error(String::from("cannot parse the address"))});
        }
      },
      Command::RemoveInstance(instance) => {
        info!("{}\t{} remove instance {:?}", self.tag, message.id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_instance(&instance.app_id, &addr, event_loop);
          self.channel.write_message(&ServerMessage{ id: message.id, status: ServerMessageStatus::Ok});
        } else {
          self.channel.write_message(&ServerMessage{ id: message.id, status: ServerMessageStatus::Error(String::from("cannot parse the address"))});
        }
      },
      Command::HttpProxy(configuration) => {
        info!("{}\t{} modifying proxy configuration: {:?}", self.tag, message.id, configuration);
        self.front_timeout = configuration.front_timeout;
        self.back_timeout  = configuration.back_timeout;
        self.answers = DefaultAnswers {
          NotFound:           configuration.answer_404.into_bytes(),
          ServiceUnavailable: configuration.answer_503.into_bytes(),
        };
        self.channel.write_message(&ServerMessage{ id: message.id, status: ServerMessageStatus::Ok});
      },
      Command::SoftStop => {
        info!("{}\t{} processing soft shutdown", self.tag, message.id);
        event_loop.deregister(&self.listener);
        self.channel.write_message(&ServerMessage{ id: message.id, status: ServerMessageStatus::Processing});
      },
      Command::HardStop => {
        info!("{}\t{} hard shutdown", self.tag, message.id);
        self.channel.write_message(&ServerMessage{ id: message.id, status: ServerMessageStatus::Ok});
      },
      command => {
        error!("{}\t{} unsupported message, ignoring {:?}", self.tag, message.id, command);
        self.channel.write_message(&ServerMessage{ id: message.id, status: ServerMessageStatus::Error(String::from("unsupported message"))});
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

  fn channel(&mut self) -> &mut Channel {
    &mut self.channel
  }
}

pub type TlsServer = Server<ServerConfiguration,TlsClient>;

pub fn start_listener(tag: String, config: TlsProxyConfiguration, channel: Channel) {
  let mut event_loop  = Poll::new().unwrap();
  let max_connections = config.max_connections;
  let max_listeners   = 1;

  // start at max_listeners + 1 because token(0) is the channel, and token(1) is the timer
  let configuration = ServerConfiguration::new(tag.clone(), config, channel, &mut event_loop, 1 + max_listeners).unwrap();
  let mut server = TlsServer::new(max_listeners, max_connections, configuration, event_loop);

  info!("{}\tstarting event loop", &tag);
  server.run();
  //event_loop.run(&mut server).unwrap();
  info!("{}\tending event loop", &tag);
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

    let context    = SslContext::builder(SslMethod::dtls()).unwrap();
    let (command, channel) = CommandChannel::generate(1000, 10000).expect("should create a channel");

    let tls_data = TlsData {
      context:     context.build(),
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
      default_context: tls_data,
      contexts: rc_ctx,
      channel:   channel,
      pool:      Rc::new(RefCell::new(Pool::with_capacity(1, 0, || BufferQueue::with_capacity(16384)))),
      front_timeout: 5000,
      back_timeout:  5000,
      answers:   DefaultAnswers {
        NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\n\r\n"[..]),
        ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\n\r\n"[..]),
      },
      config: Default::default(),
      tag:    String::from("TLS"),
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
