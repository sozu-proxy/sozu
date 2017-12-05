use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::sync::{Arc,Mutex};
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::mem;
use std::net::Shutdown;
use mio::*;
use mio::net::*;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
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
                   Ssl, SslOption, SslRef, SslStream, SniError};
use openssl::x509::X509;
use openssl::dh::Dh;
use openssl::pkey::PKey;
use openssl::hash::MessageDigest;
use openssl::nid;
use openssl::error::ErrorStack;
use nom::IResult;

use sozu_command::buffer::Buffer;
use sozu_command::channel::Channel;
use sozu_command::messages::{self,Application,CertFingerprint,CertificateAndKey,Order,HttpsFront,HttpsProxyConfiguration,OrderMessage,
  OrderMessageAnswer,OrderMessageStatus};

use parser::http11::{HttpState,RequestState,ResponseState,RRequestLine,parse_request_until_stop,hostname_and_port};
use network::buffer_queue::BufferQueue;
use network::{AppId,Backend,ClientResult,ConnectionError,Protocol};
use network::backends::BackendMap;
use network::proxy::{Server,ProxyChannel};
use network::session::{BackendConnectAction,BackendConnectionStatus,ProxyClient,ProxyConfiguration,
  Readiness,ListenToken,FrontToken,BackToken,AcceptError,Session,SessionMetrics};
use network::http::{self,DefaultAnswers};
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::trie::*;
use network::protocol::{ProtocolResult,TlsHandshake,Http,Pipe,StickySession};
use network::protocol::http::DefaultAnswerStatus;
use network::retry::RetryPolicy;
use util::UnwrapLog;


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
  instance:       Option<Rc<RefCell<Backend>>>,
  back_connected: BackendConnectionStatus,
  protocol:       Option<State>,
  public_address: Option<IpAddr>,
  ssl:            Option<Ssl>,
  pool:           Weak<RefCell<Pool<BufferQueue>>>,
  sticky_session: bool,
  metrics:        SessionMetrics,
}

impl TlsClient {
  pub fn new(ssl:Ssl, sock: TcpStream, pool: Weak<RefCell<Pool<BufferQueue>>>, public_address: Option<IpAddr>) -> TlsClient {
    //FIXME: we should not need to clone the socket. Maybe do the accept here instead of
    // in TlsHandshake?
    let s = sock.try_clone().expect("could not clone the socket");
    let handshake = TlsHandshake::new(ssl, s);
    TlsClient {
      front:          Some(sock),
      front_token:    None,
      instance:       None,
      back_connected: BackendConnectionStatus::NotConnected,
      protocol:       Some(State::Handshake(handshake)),
      public_address: public_address,
      ssl:            None,
      pool:           pool,
      sticky_session: false,
      metrics:        SessionMetrics::new(),
    }
  }

  pub fn http(&mut self) -> Option<&mut Http<SslStream<TcpStream>>> {
    self.protocol.as_mut().and_then(|protocol| {
      if let &mut State::Http(ref mut http) = protocol {
        Some(http)
      } else {
        None
      }
    })
  }

  pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: &[u8])  {
    self.protocol.as_mut().map(|protocol| {
      if let &mut State::Http(ref mut http) = protocol {
        http.set_answer(answer, buf);
      }
    });
  }

  pub fn upgrade(&mut self) -> bool {
    let protocol = unwrap_msg!(self.protocol.take());

    if let State::Handshake(handshake) = protocol {
      if let Some(pool) = self.pool.upgrade() {
        let mut p = pool.borrow_mut();

        if let (Some(front_buf), Some(back_buf)) = (p.checkout(), p.checkout()) {
          let mut http = Http::new(unwrap_msg!(handshake.stream), front_buf,
            back_buf, self.public_address.clone(), Protocol::HTTPS).unwrap();

          http.readiness = handshake.readiness;
          http.readiness.front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
          http.set_front_token(unwrap_msg!(self.front_token.as_ref()).clone());
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
      debug!("https switching to wss");
      let front_token = unwrap_msg!(http.front_token());
      let back_token  = unwrap_msg!(http.back_token());

      let mut pipe = Pipe::new(http.frontend, unwrap_msg!(http.backend),
        http.front_buf, http.back_buf, http.public_address).expect("could not create Pipe instance");

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
    unwrap_msg!(self.front.as_ref())
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    match unwrap_msg!(self.protocol.as_ref()) {
      &State::Handshake(ref handshake) => None,
      &State::Http(ref http)           => http.back_socket(),
      &State::WebSocket(ref pipe)      => pipe.back_socket(),
    }
  }

  fn front_token(&self)  -> Option<Token> {
    self.front_token
  }

  fn back_token(&self)   -> Option<Token> {
    if let &State::Http(ref http) = unwrap_msg!(self.protocol.as_ref()) {
      http.back_token()
    } else {
      None
    }
  }

  fn back_connected(&self)     -> BackendConnectionStatus {
    self.back_connected
  }

  fn set_back_connected(&mut self, connected: BackendConnectionStatus) {
    self.back_connected = connected;

    if connected == BackendConnectionStatus::Connected {
      self.instance.as_ref().map(|instance| {
        let ref mut backend = *instance.borrow_mut();
        backend.failures = 0;
        backend.retry_policy.succeed();
      });
    }
  }

  fn close(&mut self) {
    //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.token, *self.temp.front_buf, *self.temp.back_buf);
    self.http().map(|http| http.close());
  }

  fn log_context(&self)  -> String {
    if let &State::Http(ref http) = unwrap_msg!(self.protocol.as_ref()) {
      http.log_context()
    } else {
      "".to_string()
    }
  }

  fn set_back_socket(&mut self, sock:TcpStream) {
    unwrap_msg!(self.http()).set_back_socket(sock)
  }

  fn set_front_token(&mut self, token: Token) {
    self.front_token = Some(token);
    self.protocol.as_mut().map(|p| match *p {
      State::Http(ref mut http) => http.set_front_token(token),
      _                         => {}
    });
  }

  fn set_back_token(&mut self, token: Token) {
    unwrap_msg!(self.http()).set_back_token(token)
  }

  fn metrics(&mut self)        -> &mut SessionMetrics {
    &mut self.metrics
  }

  fn front_hup(&mut self)     -> ClientResult {
    self.http().map(|h| h.front_hup()).unwrap_or(ClientResult::CloseClient)
  }

  fn back_hup(&mut self)      -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)      => http.back_hup(),
      State::WebSocket(ref mut pipe) => pipe.back_hup(),
      State::Handshake(_)            => {
        error!("why a backend HUP event while still in frontend handshake?");
        ClientResult::CloseClient
      }
    }
  }

  fn readable(&mut self)      -> ClientResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Handshake(ref mut handshake) => handshake.readable(),
      State::Http(ref mut http)           => (ProtocolResult::Continue, http.readable(&mut self.metrics)),
      State::WebSocket(ref mut pipe)      => (ProtocolResult::Continue, pipe.readable(&mut self.metrics)),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else {
        if self.upgrade() {
        match *unwrap_msg!(self.protocol.as_mut()) {
          State::Http(ref mut http) => http.readable(&mut self.metrics),
          _ => result
        }
      } else {
        ClientResult::CloseClient
      }
    }
  }

  fn writable(&mut self)      -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Handshake(ref mut handshake) => ClientResult::CloseClient,
      State::Http(ref mut http)           => http.writable(&mut self.metrics),
      State::WebSocket(ref mut pipe)      => pipe.writable(&mut self.metrics),
    }
  }

  fn back_readable(&mut self) -> ClientResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Http(ref mut http)           => http.back_readable(&mut self.metrics),
      State::Handshake(ref mut handshake) => (ProtocolResult::Continue, ClientResult::CloseClient),
      State::WebSocket(ref mut pipe)      => (ProtocolResult::Continue, pipe.back_readable(&mut self.metrics)),
    };

    if upgrade == ProtocolResult::Continue {
      result
    } else {
      if self.upgrade() {
        match *unwrap_msg!(self.protocol.as_mut()) {
          State::WebSocket(ref mut pipe) => pipe.back_readable(&mut self.metrics),
          _ => result
        }
      } else {
        ClientResult::CloseBoth
      }
    }
  }

  fn back_writable(&mut self) -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Handshake(ref mut handshake) => ClientResult::CloseClient,
      State::Http(ref mut http)           => http.back_writable(&mut self.metrics),
      State::WebSocket(ref mut pipe)      => pipe.back_writable(&mut self.metrics),
    }
  }

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    unwrap_msg!(self.http()).remove_backend()
  }

  fn readiness(&mut self)      -> &mut Readiness {
    let r = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Handshake(ref mut handshake) => &mut handshake.readiness,
      State::Http(ref mut http)           => http.readiness(),
      State::WebSocket(ref mut pipe)      => &mut pipe.readiness,
    };
    //info!("current readiness: {:?}", r);
    r
  }

  fn protocol(&self)           -> Protocol {
    Protocol::HTTPS
  }
}

fn get_cert_common_name(cert: &X509) -> Option<String> {
    cert.subject_name().entries_by_nid(nid::COMMONNAME).next().and_then(|name| name.data().as_utf8().ok().map(|name| (&*name).to_string()))
}

pub type HostName  = String;
pub type PathBegin = String;
pub struct TlsData {
  context:     SslContext,
  certificate: Vec<u8>,
  refcount:    usize,
  initialized: bool,
}

pub struct ServerConfiguration {
  listener:        TcpListener,
  address:         SocketAddr,
  applications:    HashMap<AppId, Application>,
  instances:       BackendMap,
  fronts:          HashMap<HostName, Vec<TlsApp>>,
  domains:         Arc<Mutex<TrieNode<CertFingerprint>>>,
  default_context: TlsData,
  contexts:        Arc<Mutex<HashMap<CertFingerprint,TlsData>>>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  answers:         DefaultAnswers,
  config:          HttpsProxyConfiguration,
  base_token:      usize,
  ssl_options:     SslOption,
}

impl ServerConfiguration {
  pub fn new(config: HttpsProxyConfiguration, base_token: usize, event_loop: &mut Poll, start_at: usize,
    pool: Rc<RefCell<Pool<BufferQueue>>>) -> io::Result<ServerConfiguration> {

    let contexts:HashMap<CertFingerprint,TlsData> = HashMap::new();
    let     domains  = TrieNode::root();
    let mut fronts   = HashMap::new();
    let rc_ctx       = Arc::new(Mutex::new(contexts));
    let ref_ctx      = rc_ctx.clone();
    let rc_domains   = Arc::new(Mutex::new(domains));
    let ref_domains  = rc_domains.clone();
    let default_name = config.default_name.as_ref().map(|name| name.clone()).unwrap_or(String::new());

    let (fingerprint, tls_data, names, ssl_options):(CertFingerprint,TlsData, Vec<String>, SslOption) =
      Self::create_default_context(&config, ref_ctx, ref_domains, default_name).expect("could not create default context");
    let cert = try!(X509::from_pem(&tls_data.certificate));

    let common_name: Option<String> = get_cert_common_name(&cert);
    debug!("got common name for default cert: {:?}", common_name);

    let app = TlsApp {
      app_id:           config.default_app_id.clone().unwrap_or(String::new()),
      hostname:         config.default_name.clone().unwrap_or(String::new()),
      path_begin:       String::new(),
      cert_fingerprint: fingerprint,
    };
    fronts.insert(config.default_name.clone().unwrap_or(String::from("")), vec![app]);

    match server_bind(&config.front) {
      Ok(listener) => {
        event_loop.register(&listener, Token(base_token), Ready::readable(), PollOpt::edge());
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
          applications:    HashMap::new(),
          instances:       BackendMap::new(),
          fronts:          fronts,
          domains:         rc_domains,
          default_context: tls_data,
          contexts:        rc_ctx,
          pool:            pool,
          answers:         default,
          base_token:      base_token,
          config:          config,
          ssl_options:     ssl_options,
        })
      },
      Err(e) => {
        let formatted_err = format!("could not create listener {:?}: {:?}", fronts, e);
        error!("{}", formatted_err);
        //FIXME: send message if we could not create the listener
        //channel.write_message(&OrderMessageAnswer{id: String::from("listener_failed"), status: OrderMessageStatus::Error(formatted_err)});
        //channel.run();
        Err(e)
      }
    }
  }

  pub fn create_default_context(config: &HttpsProxyConfiguration, ref_ctx: Arc<Mutex<HashMap<CertFingerprint,TlsData>>>, ref_domains: Arc<Mutex<TrieNode<CertFingerprint>>>, default_name: String) -> Option<(CertFingerprint,TlsData,Vec<String>, SslOption)> {
    let ctx = SslContext::builder(SslMethod::tls());
    if let Err(e) = ctx {
      //return Err(io::Error::new(io::ErrorKind::Other, e.description()));
      return None
    }

    let mut context = ctx.expect("should have built a correct SSL context");

    let mut mode = ssl::SSL_MODE_ENABLE_PARTIAL_WRITE;
    mode.insert(ssl::SSL_MODE_ACCEPT_MOVING_WRITE_BUFFER);
    //FIXME: maybe activate ssl::SSL_MODE_RELEASE_BUFFERS to save some memory?
    context.set_mode(mode);


    let mut ssl_options = ssl::SSL_OP_CIPHER_SERVER_PREFERENCE | ssl::SSL_OP_NO_COMPRESSION | ssl::SSL_OP_NO_TICKET;
    let mut versions = ssl::SSL_OP_NO_SSLV2 |
      ssl::SSL_OP_NO_SSLV3 | ssl::SSL_OP_NO_TLSV1 |
      ssl::SSL_OP_NO_TLSV1_1 | ssl::SSL_OP_NO_TLSV1_2;

    for version in config.versions.iter() {
      match version.as_str() {
        "SSLv2"   => versions.remove(ssl::SSL_OP_NO_SSLV2),
        "SSLv3"   => versions.remove(ssl::SSL_OP_NO_SSLV3),
        "TLSv1"   => versions.remove(ssl::SSL_OP_NO_TLSV1),
        "TLSv1.1" => versions.remove(ssl::SSL_OP_NO_TLSV1_1),
        "TLSv1.2" => versions.remove(ssl::SSL_OP_NO_TLSV1_2),
        s         => error!("unrecognized TLS version: {}", s)
      };
    }

    ssl_options.insert(versions);
    trace!("parsed tls options: {:?}", ssl_options);

    let opt = context.set_options(ssl_options);

    context.set_cipher_list(&config.cipher_list);

    if let Err(e) = setup_curves(&mut context) {
      error!("could not setup curves for openssl");
    }

    //FIXME: get the default cert and key from the configuration
    let cert_read = config.default_certificate.as_ref().map(|vec| &vec[..]).unwrap_or(&include_bytes!("../../assets/certificate.pem")[..]);
    let key_read = config.default_key.as_ref().map(|vec| &vec[..]).unwrap_or(&include_bytes!("../../assets/key.pem")[..]);
    if let Some(path) = config.default_certificate_chain.as_ref() {
      context.set_certificate_chain_file(path);
    }

    if let (Ok(cert), Ok(key)) = (X509::from_pem(&cert_read[..]), PKey::private_key_from_pem(&key_read[..])) {
      if let Ok(fingerprint) = cert.fingerprint(MessageDigest::sha256()).map(|v| CertFingerprint(v)) {
        context.set_certificate(&cert);
        context.set_private_key(&key);


        let mut names: Vec<String> = cert.subject_alt_names().map(|names| {
          names.iter().filter_map(|general_name|
            general_name.dnsname().map(|name| String::from(name))
          ).collect()
        }).unwrap_or(vec!());

        debug!("got subject alt names: {:?}", names);
        {
          let mut domains = unwrap_msg!(ref_domains.lock());
          for name in &names {
            domains.domain_insert(name.clone().into_bytes(), fingerprint.clone());
          }
        }

        if let Some(common_name) = get_cert_common_name(&cert) {
        debug!("got common name: {:?}", common_name);
          names.push(common_name);
        }

        context.set_servername_callback(move |ssl: &mut SslRef| {
          let contexts = unwrap_msg!(ref_ctx.lock());
          let domains  = unwrap_msg!(ref_domains.lock());

          trace!("ref: {:?}", ssl);
          if let Some(servername) = ssl.servername().map(|s| s.to_string()) {
            debug!("checking servername: {}", servername);
            if &servername == &default_name {
              return Ok(());
            }
            debug!("looking for fingerprint for {:?}", servername);
            if let Some(kv) = domains.domain_lookup(servername.as_bytes()) {
              debug!("looking for context for {:?} with fingerprint {:?}", servername, kv.1);
              if let Some(ref tls_data) = contexts.get(&kv.1) {
                debug!("found context for {:?}", servername);
                if !tls_data.initialized {
                  //FIXME: couldn't we skip to the next cert?
                  error!("no application is using that certificate (looking up {})", servername);
                  return Ok(());
                }
                let context: &SslContext = &tls_data.context;
                if let Ok(()) = ssl.set_ssl_context(context) {
                  debug!("servername is now {:?}", ssl.servername());
                  return Ok(());
                } else {
                  error!("no context found for {:?}", servername);
                }
              }
            }
          } else {
            error!("got no server name from ssl, answering with default one");
          }
          //answer ok because we use the default certificate
          Ok(())
        });

        let tls_data = TlsData {
          context:     context.build(),
          certificate: cert_read.to_vec(),
          refcount:    1,
          initialized: true,
        };
        Some((fingerprint, tls_data, names, ssl_options))
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn add_application(&mut self, application: Application, event_loop: &mut Poll) {
    self.applications.insert(application.app_id.clone(), application);
  }

  pub fn remove_application(&mut self, app_id: &str, event_loop: &mut Poll) {
    self.applications.remove(app_id);
  }

  pub fn add_https_front(&mut self, tls_front: HttpsFront, event_loop: &mut Poll) -> bool {
    {
      let mut contexts = unwrap_msg!(self.contexts.lock());

      if contexts.contains_key(&tls_front.fingerprint) {
        contexts.get_mut(&tls_front.fingerprint).map(|data| {
          data.refcount    += 1;
          data.initialized  = true;
        });
      } else {
        //FIXME return error here, no available certificate
        return false
      }
    }

    //FIXME: should clone he hostname then do a into() here
    let app = TlsApp {
      app_id:           tls_front.app_id.clone(),
      hostname:         tls_front.hostname.clone(),
      path_begin:       tls_front.path_begin.clone(),
      cert_fingerprint: tls_front.fingerprint.clone(),
    };

    if let Some(fronts) = self.fronts.get_mut(&tls_front.hostname) {
        if ! fronts.contains(&app) {
          fronts.push(app.clone());
        }
    }
    if self.fronts.get(&tls_front.hostname).is_none() {
      self.fronts.insert(tls_front.hostname, vec![app]);
    }

    true
  }

  pub fn remove_https_front(&mut self, front: HttpsFront, event_loop: &mut Poll) {
    debug!("removing tls_front {:?}", front);

    if let Some(fronts) = self.fronts.get_mut(&front.hostname) {
      if let Some(pos) = fronts.iter().position(|f| &f.app_id == &front.app_id && &f.cert_fingerprint == &front.fingerprint) {
        let front = fronts.remove(pos);

        {
          let mut contexts = unwrap_msg!(self.contexts.lock());
          let domains  = unwrap_msg!(self.domains.lock());
          let must_delete = contexts.get_mut(&front.cert_fingerprint).map(|tls_data| {
            if tls_data.refcount > 0 {
              tls_data.refcount -= 1;
            }
            tls_data.refcount == 0
          });
        }
      }
    }
  }

  pub fn add_certificate(&mut self, certificate_and_key: CertificateAndKey, event_loop: &mut Poll) -> bool {
    //FIXME: insert some error management with a Result here
    let c = SslContext::builder(SslMethod::tls());
    if c.is_err() { return false; }
    let mut ctx = c.expect("should have built a correct SSL context");
    let opt = ctx.set_options(self.ssl_options);

    if let Err(e) = setup_curves(&mut ctx) {
      error!("could not setup curves for openssl");
    }

    let mut cert_read  = &certificate_and_key.certificate.as_bytes()[..];
    let mut key_read   = &certificate_and_key.key.as_bytes()[..];
    let cert_chain: Vec<X509> = certificate_and_key.certificate_chain.iter().filter_map(|c| {
      X509::from_pem(c.as_bytes()).ok()
    }).collect();

    if let (Ok(cert), Ok(key)) = (X509::from_pem(&mut cert_read), PKey::private_key_from_pem(&mut key_read)) {
      //FIXME: would need more logs here

      //FIXME
      let fingerprint = CertFingerprint(unwrap_msg!(cert.fingerprint(MessageDigest::sha256())));
      let common_name: Option<String> = get_cert_common_name(&cert);
      debug!("got common name: {:?}", common_name);

      let names: Vec<String> = cert.subject_alt_names().map(|names| {
        names.iter().filter_map(|general_name|
          general_name.dnsname().map(|name| String::from(name))
        ).collect()
      }).unwrap_or(vec!());
      debug!("got subject alt names: {:?}", names);

      ctx.set_certificate(&cert);
      ctx.set_private_key(&key);
      for cert in cert_chain {
        ctx.add_extra_chain_cert(cert);
      }

      let tls_data = TlsData {
        context:     ctx.build(),
        certificate: cert_read.to_vec(),
        refcount:    0,
        initialized: false,
      };

      // if the name or the fingerprint are already used,
      // those insertions should fail, because it would be
      // from the same certificate
      // Add a refcount?
      //FIXME: this is blocking
      //this lock is only obtained from this thread, so is it alright?
      {
        let mut contexts = unwrap_msg!(self.contexts.lock());
        if !contexts.contains_key(&fingerprint) {
          contexts.insert(fingerprint.clone(), tls_data);
        }
      }
      {
        let mut domains = unwrap_msg!(self.domains.lock());
        if let Some(name) = common_name {
          domains.domain_insert(name.into_bytes(), fingerprint.clone());
        }
        for name in names {
          domains.domain_insert(name.into_bytes(), fingerprint.clone());
        }
      }

      true
    } else {
      false
    }
  }

  // FIXME: return an error if the cert is still in use
  pub fn remove_certificate(&mut self, fingerprint: CertFingerprint, event_loop: &mut Poll) {
    debug!("removing certificate {:?}", fingerprint);
    let mut contexts = unwrap_msg!(self.contexts.lock());
    let mut domains  = unwrap_msg!(self.domains.lock());
    let must_delete = contexts.get_mut(&fingerprint).map(|tls_data| {
      if tls_data.refcount > 0 { tls_data.refcount -= 1; }
      tls_data.refcount == 0 || !tls_data.initialized
    });

    if must_delete == Some(true) {
      if let Some(data) = contexts.remove(&fingerprint) {
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

  pub fn add_instance(&mut self, app_id: &str, instance_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) {
    self.instances.add_instance(app_id, instance_id, instance_address);
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut Poll) {
    self.instances.remove_instance(app_id, instance_address);
  }

  // ToDo factor out with http.rs
  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<&TlsApp> {
    let host: &str = if let IResult::Done(i, (hostname, port)) = hostname_and_port(host.as_bytes()) {
      if i != &b""[..] {
        error!("invalid remaining chars after hostname");
        return None;
      }

      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
      unsafe { from_utf8_unchecked(hostname) }
    } else {
      error!("hostname parsing failed");
      return None;
    };

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

  pub fn backend_from_request(&mut self, client: &mut TlsClient, host: &str, uri: &str, front_should_stick: bool) -> Result<TcpStream,ConnectionError> {
    trace!("looking for backend for host: {}", host);
    let real_host = if let Some(h) = host.split(":").next() {
      h
    } else {
      host
    };
    trace!("looking for backend for real host: {}", real_host);

    if let Some(app_id) = self.frontend_from_request(real_host, uri).map(|ref front| front.app_id.clone()) {
      client.http().map(|h| h.set_app_id(app_id.clone()));

      match self.instances.backend_from_app_id(&app_id) {
        Err(e) => {
          client.set_answer(DefaultAnswerStatus::Answer503, &self.answers.ServiceUnavailable);
          Err(e)
        },
        Ok((backend, conn))  => {
          client.back_connected = BackendConnectionStatus::Connecting;
          if front_should_stick {
            client.http().map(|http| http.sticky_session = Some(StickySession::new(backend.borrow().id.clone())));
          }
          client.metrics.backend_id = Some(backend.borrow().instance_id.clone());
          client.metrics.backend_start();
          client.instance = Some(backend);

          Ok(conn)
        }
      }
    } else {
      Err(ConnectionError::HostNotFound)
    }
  }

  pub fn backend_from_sticky_session(&mut self, client: &mut TlsClient, app_id: &str, sticky_session: u32) -> Result<TcpStream,ConnectionError> {
    client.http().map(|h| h.set_app_id(String::from(app_id)));

    match self.instances.backend_from_sticky_session(app_id, sticky_session) {
      Err(e) => {
        debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
        client.set_answer(DefaultAnswerStatus::Answer503, &self.answers.ServiceUnavailable);
        Err(e)
      },
      Ok((backend, conn))  => {
        client.back_connected = BackendConnectionStatus::Connecting;
        client.http().map(|http| http.sticky_session = Some(StickySession::new(backend.borrow().id.clone())));
        client.metrics.backend_id = Some(backend.borrow().instance_id.clone());
        client.metrics.backend_start();
        client.instance = Some(backend);

        Ok(conn)
      }
    }
  }
}

impl ProxyConfiguration<TlsClient> for ServerConfiguration {
  fn accept(&mut self, token: ListenToken) -> Result<(TlsClient,bool), AcceptError> {
    self.listener.accept().map_err(|e| {
      match e.kind() {
        ErrorKind::WouldBlock => AcceptError::WouldBlock,
        other => {
          error!("accept() IO error: {:?}", e);
          AcceptError::IoError
        }
      }
    }).and_then(|(frontend_sock, _)| {
      frontend_sock.set_nodelay(true);
      if let Ok(ssl) = Ssl::new(&self.default_context.context) {
        let c = TlsClient::new(ssl, frontend_sock, Rc::downgrade(&self.pool), self.config.public_address);
        return Ok((c, false))
      } else {
        error!("could not create ssl context");
        Err(AcceptError::IoError)
      }
    })
  }

  fn connect_to_backend(&mut self, event_loop: &mut Poll, client: &mut TlsClient) -> Result<BackendConnectAction,ConnectionError> {
    let h = try!(unwrap_msg!(client.http()).state().get_host().ok_or(ConnectionError::NoHostGiven));

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
      let servername: Option<String> = unwrap_msg!(client.http()).frontend.ssl().servername().map(|s| s.to_string());
      if servername.as_ref().map(|s| s.as_str()) != Some(hostname_str) {
        error!("TLS SNI hostname '{:?}' and Host header '{}' don't match", servername, hostname_str);
        unwrap_msg!(client.http()).set_answer(DefaultAnswerStatus::Answer404, &self.answers.NotFound);
        client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
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

    let rl:RRequestLine = try!(unwrap_msg!(client.http()).state().get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
    if let Some(app_id) = self.frontend_from_request(&host, &rl.uri).map(|ref front| front.app_id.clone()) {

      let front_should_stick = self.applications.get(&app_id).map(|ref app| app.sticky_session).unwrap_or(false);

      if (client.http().map(|h| h.app_id.as_ref()).unwrap_or(None) == Some(&app_id)) && client.back_connected == BackendConnectionStatus::Connected {
        if client.instance.as_ref().map(|instance| {
           let ref backend = *instance.borrow();
           self.instances.has_backend(&app_id, backend)
        }).unwrap_or(false) {
          //matched on keepalive
          client.metrics.backend_id = client.instance.as_ref().map(|i| i.borrow().instance_id.clone());
          client.metrics.backend_start();
          return Ok(BackendConnectAction::Reuse);
        } else {
          client.instance = None;
          client.back_connected = BackendConnectionStatus::NotConnected;
          //client.readiness().back_interest  = UnixReady::from(Ready::empty());
          client.readiness().back_readiness = UnixReady::from(Ready::empty());
          client.back_socket().as_ref().map(|sock| {
            event_loop.deregister(*sock);
            sock.shutdown(Shutdown::Both);
          });
        }
      }

      // circuit breaker
      if client.back_connected == BackendConnectionStatus::Connecting {
        client.instance.as_ref().map(|instance| {
          let ref mut backend = *instance.borrow_mut();
          backend.dec_connections();
          backend.failures += 1;
          backend.retry_policy.fail();
        });

        client.instance = None;
        client.back_connected = BackendConnectionStatus::NotConnected;
        client.readiness().back_interest  = UnixReady::from(Ready::empty());
        client.readiness().back_readiness = UnixReady::from(Ready::empty());
        client.back_socket().as_ref().map(|sock| {
          event_loop.deregister(*sock);
          sock.shutdown(Shutdown::Both);
        });
      }

      let old_app_id = client.http().and_then(|ref http| http.app_id.clone());

      let conn   = try!(unwrap_msg!(client.http()).state().get_front_keep_alive().ok_or(ConnectionError::ToBeDefined));
      let sticky_session = client.http().unwrap().state.as_ref().unwrap().get_request_sticky_session();
      let conn = match (front_should_stick, sticky_session) {
        (true, Some(session)) => self.backend_from_sticky_session(client, &app_id, session),
        _ => self.backend_from_request(client, &host, &rl.uri, front_should_stick),
      };

      match conn {
        Ok(socket) => {
          let new_app_id = client.http().and_then(|ref http| http.app_id.clone());

          //deregister back socket if it is the wrong one or if it was not connecting
          if old_app_id.is_some() && old_app_id != new_app_id {
            client.instance = None;
            client.back_connected = BackendConnectionStatus::NotConnected;
            client.readiness().back_readiness = UnixReady::from(Ready::empty());
            client.back_socket().as_ref().map(|sock| {
              event_loop.deregister(*sock);
              sock.shutdown(Shutdown::Both);
            });
            // we still want to use the new socket
            client.readiness().back_interest  = UnixReady::from(Ready::writable());
          }

          let req_state = unwrap_msg!(client.http()).state().request.clone();
          let req_header_end = unwrap_msg!(client.http()).state().req_header_end;
          let res_header_end = unwrap_msg!(client.http()).state().res_header_end;
          let added_req_header = unwrap_msg!(client.http()).state().added_req_header.clone();
          let added_res_header = unwrap_msg!(client.http()).state().added_res_header.clone();
          // FIXME: is this still needed?
          unwrap_msg!(client.http()).set_state(HttpState {
            req_header_end: req_header_end,
            res_header_end: res_header_end,
            request:  req_state,
            response: Some(ResponseState::Initial),
            added_req_header: added_req_header,
            added_res_header: added_res_header,
          });

          socket.set_nodelay(true);
          client.set_back_socket(socket);

          if old_app_id == new_app_id {
            Ok(BackendConnectAction::Replace)
          } else {
            Ok(BackendConnectAction::New)
          }
        },
        Err(ConnectionError::NoBackendAvailable) => {
          unwrap_msg!(client.http()).set_answer(DefaultAnswerStatus::Answer503, &self.answers.ServiceUnavailable);
          client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
          Err(ConnectionError::NoBackendAvailable)
        },
        Err(ConnectionError::HostNotFound) => {
          unwrap_msg!(client.http()).set_answer(DefaultAnswerStatus::Answer404, &self.answers.NotFound);
          client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
          Err(ConnectionError::HostNotFound)
        },
        e => panic!(e)
      }
    } else {
      unwrap_msg!(client.http()).set_answer(DefaultAnswerStatus::Answer404, &self.answers.NotFound);
      client.readiness().front_interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
      Err(ConnectionError::HostNotFound)
    }
  }

  fn notify(&mut self, event_loop: &mut Poll, message: OrderMessage) -> OrderMessageAnswer {
    //trace!("{} notified", message);
    match message.order {
      Order::AddApplication(application) => {
        debug!("{} add application {:?}", message.id, application);
        self.add_application(application, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveApplication(application) => {
        debug!("{} remove application {:?}", message.id, application);
        remove_app_metrics!(&application);
        self.remove_application(&application, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::AddHttpsFront(front) => {
        //info!("HTTPS\t{} add front {:?}", id, front);
        self.add_https_front(front, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveHttpsFront(front) => {
        //info!("HTTPS\t{} remove front {:?}", id, front);
        self.remove_https_front(front, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::AddCertificate(certificate_and_key) => {
        //info!("HTTPS\t{} add certificate: {:?}", id, certificate_and_key);
          self.add_certificate(certificate_and_key, event_loop);
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveCertificate(fingerprint) => {
        //info!("TLS\t{} remove certificate with fingerprint {:?}", id, fingerprint);
        self.remove_certificate(fingerprint, event_loop);
        //FIXME: should return an error if certificate still has fronts referencing it
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::AddInstance(instance) => {
        debug!("{} add instance {:?}", message.id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &instance.instance_id, &addr, event_loop);
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot parse the address")), data: None }
        }
      },
      Order::RemoveInstance(instance) => {
        debug!("{} remove instance {:?}", message.id, instance);
        remove_backend_metrics!(&instance.instance_id);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_instance(&instance.app_id, &addr, event_loop);
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("cannot parse the address")), data: None }
        }
      },
      Order::HttpProxy(configuration) => {
        debug!("{} modifying proxy configuration: {:?}", message.id, configuration);
        self.answers = DefaultAnswers {
          NotFound:           configuration.answer_404.into_bytes(),
          ServiceUnavailable: configuration.answer_503.into_bytes(),
        };
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::SoftStop => {
        info!("{} processing soft shutdown", message.id);
        event_loop.deregister(&self.listener);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Processing, data: None }
      },
      Order::HardStop => {
        info!("{} hard shutdown", message.id);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::Status => {
        debug!("{} status", message.id);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::Logging(logging_filter) => {
        debug!("{} changing logging filter to {}", message.id, logging_filter);
        ::logging::LOGGER.with(|l| {
          let directives = ::logging::parse_logging_spec(&logging_filter);
          l.borrow_mut().set_directives(directives);
        });
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      command => {
        error!("{} unsupported message, ignoring {:?}", message.id, command);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Error(String::from("unsupported message")), data: None }
      }
    }
  }

  fn close_backend(&mut self, app_id: AppId, addr: &SocketAddr) {
    self.instances.close_backend_connection(&app_id, &addr);
  }
}


#[cfg(ossl101)]
pub fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
  use openssl::ec::EcKey;
  use openssl::nid;

  let curve = try!(EcKey::from_curve_name(nid::X9_62_PRIME256V1));
  ctx.set_tmp_ecdh(&curve)
}

#[cfg(ossl102)]
fn setup_curves(ctx: &mut SslContextBuilder) -> Result<(), ErrorStack> {
  match Dh::get_2048_256() {
    Ok(dh) => ctx.set_tmp_dh(&dh),
    Err(e) => {
      return Err(e)
    }
  };
  ctx.set_ecdh_auto(true)
}

#[cfg(ossl110)]
fn setup_curves(_: &mut SslContextBuilder) -> Result<(), ErrorStack> {
  Ok(())
}

#[cfg(all(not(ossl101), not(ossl102), not(ossl110)))]
fn setup_curves(_: &mut SslContextBuilder) -> Result<(), ErrorStack> {
  Ok(())
}

pub type TlsServer = Session<ServerConfiguration,TlsClient>;

pub fn start(config: HttpsProxyConfiguration, channel: ProxyChannel, max_buffers: usize, buffer_size: usize) {
  let mut event_loop  = Poll::new().expect("could not create event loop");
  let max_listeners   = 1;

  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(2*max_buffers, 0, || BufferQueue::with_capacity(buffer_size))
  ));

  // start at max_listeners + 1 because token(0) is the channel, and token(1) is the timer
  if let Ok(configuration) = ServerConfiguration::new(config, 6148914691236517205, &mut event_loop, 1 + max_listeners, pool) {
    let session = Session::new(max_listeners, max_buffers, 6148914691236517205, configuration, &mut event_loop);
    let mut server  = Server::new(event_loop, channel, None, Some(session), None, None);

    info!("starting event loop");
    server.run();
    info!("ending event loop");
  }
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
  use slab::Slab;
  use pool::Pool;
  use sozu_command::buffer::Buffer;
  use network::buffer_queue::BufferQueue;
  use network::http::DefaultAnswers;
  use network::trie::TrieNode;
  use sozu_command::messages::{Order,HttpsFront,Instance,OrderMessage,OrderMessageAnswer};
  use openssl::ssl::{SslContext, SslMethod, Ssl, SslStream};
  use openssl::x509::X509;

  /*
  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").expect("could not parse address");
    let (tx,rx) = channel::<OrderMessageAnswer>();
    let (sender, jg) = start_listener(front, 10, 10, tx.clone());
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:1024"), path_begin: String::from("/") };
    sender.send(OrderMessage::Order(Order::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1025 };
    sender.send(OrderMessage::Order(Order::AddInstance(instance)));
    println!("test received: {:?}", rx.recv());
    println!("test received: {:?}", rx.recv());
    thread::sleep_ms(300);

    let mut client = TcpStream::connect(("127.0.0.1", 1024)).expect("could not parse address");
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

        println!("Response: {}", str::from_utf8(&buffer[..]).expect("could not make string from buffer"));

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
      let server = ServerBuilder::new().with_port(1025).build().expect("could not create server");
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

  use mio::net;
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
        cert_fingerprint: CertFingerprint(vec!()),
      },
      TlsApp {
        app_id: app_id2, hostname: "lolcatho.st".to_owned(), path_begin: uri2,
        cert_fingerprint: CertFingerprint(vec!()),
      },
      TlsApp {
        app_id: app_id3, hostname: "lolcatho.st".to_owned(), path_begin: uri3,
        cert_fingerprint: CertFingerprint(vec!()),
      }
    ]);
    fronts.insert("other.domain".to_owned(), vec![
      TlsApp {
        app_id: "app_1".to_owned(), hostname: "other.domain".to_owned(), path_begin: "/test".to_owned(),
        cert_fingerprint: CertFingerprint(vec!()),
      },
    ]);

    let contexts   = HashMap::new();
    let rc_ctx     = Arc::new(Mutex::new(contexts));
    let domains    = TrieNode::root();
    let rc_domains = Arc::new(Mutex::new(domains));

    let context    = SslContext::builder(SslMethod::tls()).expect("could not create a SslContextBuilder");

    let tls_data = TlsData {
      context:     context.build(),
      certificate: vec!(),
      refcount:    0,
      initialized: false,
    };

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1032").expect("test address 127.0.0.1:1032 should be parsed");
    let listener = net::TcpListener::bind(&front).expect("test address 127.0.0.1:1032 should be available");
    let server_config = ServerConfiguration {
      listener:  listener,
      address:   front,
      applications: HashMap::new(),
      instances: BackendMap::new(),
      fronts:    fronts,
      domains:   rc_domains,
      default_context: tls_data,
      contexts: rc_ctx,
      pool:      Rc::new(RefCell::new(Pool::with_capacity(1, 0, || BufferQueue::with_capacity(16384)))),
      base_token:    6148914691236517205,
      answers:   DefaultAnswers {
        NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\n\r\n"[..]),
        ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\n\r\n"[..]),
      },
      config: Default::default(),
      ssl_options: ssl::SSL_OP_CIPHER_SERVER_PREFERENCE | ssl::SSL_OP_NO_COMPRESSION | ssl::SSL_OP_NO_TICKET |
        ssl::SSL_OP_NO_SSLV2 | ssl::SSL_OP_NO_SSLV3 | ssl::SSL_OP_NO_TLSV1 | ssl::SSL_OP_NO_TLSV1_1,
    };

    println!("TEST {}", line!());
    let frontend1 = server_config.frontend_from_request("lolcatho.st", "/");
    assert_eq!(frontend1.expect("should find a frontend").app_id, "app_1");
    println!("TEST {}", line!());
    let frontend2 = server_config.frontend_from_request("lolcatho.st", "/test");
    assert_eq!(frontend2.expect("should find a frontend").app_id, "app_1");
    println!("TEST {}", line!());
    let frontend3 = server_config.frontend_from_request("lolcatho.st", "/yolo/test");
    assert_eq!(frontend3.expect("should find a frontend").app_id, "app_2");
    println!("TEST {}", line!());
    let frontend4 = server_config.frontend_from_request("lolcatho.st", "/yolo/swag");
    assert_eq!(frontend4.expect("should find a frontend").app_id, "app_3");
    println!("TEST {}", line!());
    let frontend5 = server_config.frontend_from_request("domain", "/");
    assert_eq!(frontend5, None);
   // assert!(false);
  }
}
