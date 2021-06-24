use std::sync::Arc;
use std::rc::Rc;
use std::cell::RefCell;
use mio::*;
use mio::net::*;
use std::os::unix::io::{AsRawFd};
use std::io::ErrorKind;
use std::collections::HashMap;
use slab::Slab;
use std::net::SocketAddr;
use std::str::from_utf8_unchecked;
use rustls::{ServerConfig, ServerSession, NoClientAuth, ProtocolVersion,
  ALL_CIPHERSUITES};
use time::{Duration, Instant};

use sozu_command::scm_socket::ScmSocket;
use sozu_command::proxy::{Application,
  ProxyRequestData,HttpFront,HttpsListener,ProxyRequest,ProxyResponse,
  ProxyResponseStatus,AddCertificate,RemoveCertificate,ReplaceCertificate,
  TlsVersion,ProxyResponseData,Query, QueryCertificateType,QueryAnswer,
  QueryAnswerCertificate};
use sozu_command::logging;
use sozu_command::ready::Ready;

use protocol::http::{parser::{RRequestLine,hostname_and_port}, answers::HttpAnswers};
use pool::Pool;
use {AppId,ConnectionError,Protocol,
  ProxySession,ProxyConfiguration,AcceptError,BackendConnectAction,BackendConnectionStatus};
use backends::BackendMap;
use server::{Server,ProxyChannel,ListenToken,ListenPortState,SessionToken,ListenSession,CONN_RETRIES};
use socket::server_bind;
use trie::*;
use protocol::StickySession;
use protocol::http::DefaultAnswerStatus;
use util::UnwrapLog;

use super::resolver::CertificateResolverWrapper;
use super::session::Session;

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct TlsApp {
  pub app_id:           String,
  pub hostname:         String,
  pub path_begin:       String,
}

pub type HostName  = String;
pub type PathBegin = String;

pub struct Listener {
  listener:   Option<TcpListener>,
  address:    SocketAddr,
  fronts:     TrieNode<Vec<TlsApp>>,
  answers:    Rc<RefCell<HttpAnswers>>,
  config:     HttpsListener,
  ssl_config: Arc<ServerConfig>,
  resolver:   Arc<CertificateResolverWrapper>,
  pub token:  Token,
  active:     bool,
}

impl Listener {
  pub fn new(config: HttpsListener, token: Token) -> Listener {

    let mut server_config = ServerConfig::new(NoClientAuth::new());
    server_config.versions = config.versions.iter().map(|version| {
      match version {
        TlsVersion::SSLv2   => ProtocolVersion::SSLv2,
        TlsVersion::SSLv3   => ProtocolVersion::SSLv3,
        TlsVersion::TLSv1_0 => ProtocolVersion::TLSv1_0,
        TlsVersion::TLSv1_1 => ProtocolVersion::TLSv1_1,
        TlsVersion::TLSv1_2 => ProtocolVersion::TLSv1_2,
        TlsVersion::TLSv1_3 => ProtocolVersion::TLSv1_3,
      }
    }).collect();

    let resolver = Arc::new(CertificateResolverWrapper::new());
    server_config.cert_resolver = resolver.clone();

    //FIXME: we should have another way than indexes in ALL_CIPHERSUITES,
    //but rustls does not export the static SupportedCipherSuite instances yet
    if !config.rustls_cipher_list.is_empty() {
      let mut ciphers = Vec::new();
      for cipher in config.rustls_cipher_list.iter() {
        match cipher.as_str() {
          "TLS13_CHACHA20_POLY1305_SHA256" => ciphers.push(ALL_CIPHERSUITES[0]),
          "TLS13_AES_256_GCM_SHA384" => ciphers.push(ALL_CIPHERSUITES[1]),
          "TLS13_AES_128_GCM_SHA256" => ciphers.push(ALL_CIPHERSUITES[2]),
          "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => ciphers.push(ALL_CIPHERSUITES[3]),
          "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => ciphers.push(ALL_CIPHERSUITES[4]),
          "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384" => ciphers.push(ALL_CIPHERSUITES[5]),
          "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => ciphers.push(ALL_CIPHERSUITES[6]),
          "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" => ciphers.push(ALL_CIPHERSUITES[7]),
          "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => ciphers.push(ALL_CIPHERSUITES[8]),
          s => error!("unknown cipher: {:?}", s),
        }
      }
      server_config.ciphersuites = ciphers;
    }

    Listener {
      address:    config.front.clone(),
      fronts:     TrieNode::root(),
      answers:    Rc::new(RefCell::new(HttpAnswers::new(&config.answer_404, &config.answer_503))),
      ssl_config: Arc::new(server_config),
      listener: None,
      config,
      resolver,
      token,
      active: false,
    }
  }

  pub fn activate(&mut self, event_loop: &mut Poll, tcp_listener: Option<TcpListener>) -> Option<Token> {
    if self.active {
      return Some(self.token);
    }

    let mut listener = tcp_listener.or_else(|| server_bind(&self.config.front).map_err(|e| {
      error!("could not create listener {:?}: {:?}", self.config.front, e);
    }).ok());


    if let Some(ref mut sock) = listener {
      if let Err(e) = event_loop.registry().register(
          sock,
          self.token, Interest::READABLE
        ) {
        error!("error registering listen socket: {:?}", e);
      }
    } else {
      return None;
    }

    self.listener = listener;
    self.active = true;
    Some(self.token)
  }

  pub fn add_https_front(&mut self, tls_front: HttpFront) -> bool {
    //FIXME: should clone he hostname then do a into() here
    let app = TlsApp {
      app_id:           tls_front.app_id.clone(),
      hostname:         tls_front.hostname.clone(),
      path_begin:       tls_front.path_begin.clone(),
    };

    if let Some((_,fronts)) = self.fronts.domain_lookup_mut(&tls_front.hostname.as_bytes(), false) {
        if ! fronts.contains(&app) {
          fronts.push(app.clone());
        }
    }
    if self.fronts.domain_lookup(&tls_front.hostname.as_bytes(), false).is_none() {
      self.fronts.domain_insert(tls_front.hostname.into_bytes(), vec![app]);
    }

    true
  }

  pub fn remove_https_front(&mut self, front: HttpFront) {
    debug!("removing tls_front {:?}", front);

    let should_delete = {
      let fronts_opt = self.fronts.domain_lookup_mut(front.hostname.as_bytes(), false);
      if let Some((_, fronts)) = fronts_opt {
        if let Some(pos) = fronts.iter()
          .position(|f| {
            f.app_id == front.app_id &&
            f.hostname == front.hostname &&
            f.path_begin == front.path_begin
          }) {

          let _front = fronts.remove(pos);
        }
      }

      fronts_opt.as_ref().map(|(_,fronts)| fronts.is_empty()).unwrap_or(false)
    };

    if should_delete {
      self.fronts.domain_remove(&front.hostname.into());
    }
  }

  pub fn add_certificate(&mut self, add_certificate: AddCertificate) -> bool {
    (*self.resolver).add_certificate(add_certificate).is_some()
  }

  // FIXME: return an error if the cert is still in use
  pub fn remove_certificate(&mut self, remove_certificate: RemoveCertificate) {
    debug!("removing certificate {:?}", remove_certificate);
    (*self.resolver).remove_certificate(remove_certificate)
  }

  pub fn replace_certificate(&mut self, replace_certificate: ReplaceCertificate) {
    debug!("replacing certificate {:?}", replace_certificate);
    let ReplaceCertificate { front, new_certificate, old_fingerprint, old_names, new_names } = replace_certificate;
    let remove = RemoveCertificate {
      front,
      fingerprint: old_fingerprint,
      names: old_names,
    };
    let add = AddCertificate {
      front,
      certificate: new_certificate,
      names: new_names,
    };

    //FIXME: handle results
    (*self.resolver).remove_certificate(remove);
    (*self.resolver).add_certificate(add);
  }

  fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {

    if let Some(ref listener) = self.listener.as_ref() {
      listener.accept().map_err(|e| {
        match e.kind() {
          ErrorKind::WouldBlock => AcceptError::WouldBlock,
          _ => {
            error!("accept() IO error: {:?}", e);
            AcceptError::IoError
          }
        }
      }).map(|(frontend_sock, _)| frontend_sock)
    } else {
      Err(AcceptError::IoError)
    }
  }

  // ToDo factor out with http.rs
  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<&TlsApp> {
    let host: &str = if let Ok((i, (hostname, _))) = hostname_and_port(host.as_bytes()) {
      if i != &b""[..] {
        error!("invalid remaining chars after hostname");
        return None;
      }

      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
      unsafe { from_utf8_unchecked(hostname) }
    } else {
      error!("hostname parsing failed for: '{}'", host);
      return None;
    };

    if let Some((_,http_fronts)) = self.fronts.domain_lookup(host.as_bytes(), true) {
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

}

pub struct Proxy {
  listeners:      HashMap<Token, Listener>,
  applications:   HashMap<AppId, Application>,
  backends:       Rc<RefCell<BackendMap>>,
  pool:           Rc<RefCell<Pool>>,
}

impl Proxy {
  pub fn new(pool: Rc<RefCell<Pool>>, backends: Rc<RefCell<BackendMap>>) -> Proxy {
    Proxy {
      listeners : HashMap::new(),
      applications: HashMap::new(),
      backends,
      pool,
    }
  }

  pub fn add_listener(&mut self, config: HttpsListener, token: Token) -> Option<Token> {
    if self.listeners.contains_key(&token) {
      None
    } else {
      let listener = Listener::new(config, token);
      self.listeners.insert(listener.token, listener);
      Some(token)
    }
  }

  pub fn remove_listener(&mut self, address: SocketAddr) -> bool {
    let len = self.listeners.len();
    self.listeners.retain(|_, l| l.address != address);

    self.listeners.len() < len
  }

  pub fn activate_listener(&mut self, event_loop: &mut Poll, addr: &SocketAddr, tcp_listener: Option<TcpListener>) -> Option<Token> {
    for listener in self.listeners.values_mut() {
      if &listener.address == addr {
        return listener.activate(event_loop, tcp_listener);
      }
    }
    None
  }

  pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
    self.listeners.values_mut().filter(|l| l.listener.is_some()).map(|l| {
      (l.address, l.listener.take().unwrap())
    }).collect()
  }

  pub fn give_back_listener(&mut self, address: SocketAddr) -> Option<(Token, TcpListener)> {
    self.listeners.values_mut().find(|l| l.address == address).and_then(|l| {
      l.listener.take().map(|sock| (l.token, sock))
    })
  }

  pub fn add_application(&mut self, mut application: Application) {
    if let Some(answer_503) = application.answer_503.take() {
      for l in self.listeners.values_mut() {
        l.answers.borrow_mut().add_custom_answer(&application.app_id, &answer_503);
      }
    }

    self.applications.insert(application.app_id.clone(), application);
  }

  pub fn remove_application(&mut self, app_id: &str) {
    self.applications.remove(app_id);

    for l in self.listeners.values_mut() {
      l.answers.borrow_mut().remove_custom_answer(app_id);
    }
  }

  pub fn backend_from_request(&mut self, session: &mut Session, app_id: &str,
  front_should_stick: bool) -> Result<TcpStream,ConnectionError> {
    session.http_mut().map(|h| h.set_app_id(String::from(app_id)));

    let sticky_session = session.http()
      .and_then(|http| http.request.as_ref())
      .and_then(|r| r.get_sticky_session());

    let res = match (front_should_stick, sticky_session) {
      (true, Some(sticky_session)) => {
        self.backends.borrow_mut().backend_from_sticky_session(app_id, &sticky_session)
          .map_err(|e| {
            debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
            e
          })
      },
      _ => self.backends.borrow_mut().backend_from_app_id(app_id),
    };

    match res {
      Err(e) => {
        session.set_answer(DefaultAnswerStatus::Answer503, None);
        Err(e)
      },
      Ok((backend, conn))  => {
        if front_should_stick {
          let sticky_name = self.listeners[&session.listen_token].config.sticky_name.clone();
          session.http_mut().map(|http| {
            http.sticky_session =
              Some(StickySession::new(backend.borrow().sticky_id.clone().unwrap_or(backend.borrow().backend_id.clone())));
            http.sticky_name = sticky_name;
          });
        }
        session.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        session.metrics.backend_start();

        session.http_mut().map(|http| {
          http.set_backend_id(backend.borrow().backend_id.clone());
        });

        session.backend = Some(backend);

        Ok(conn)
      }
    }
  }

  fn app_id_from_request(&mut self, session: &mut Session) -> Result<String, ConnectionError> {
    let listen_token = session.listen_token;

    let h = session.http().and_then(|h| h.request.as_ref()).and_then(|r| r.get_host()).ok_or(ConnectionError::NoHostGiven)?;

    let host: &str = if let Ok((i, (hostname, port))) = hostname_and_port(h.as_bytes()) {
      if i != &b""[..] {
        error!("invalid remaining chars after hostname");
        session.set_answer(DefaultAnswerStatus::Answer400, None);
        return Err(ConnectionError::InvalidHost);
      }

      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
      let hostname_str =  unsafe { from_utf8_unchecked(hostname) };

      //FIXME: what if we don't use SNI?
      let servername: Option<String> = session.http()
        .and_then(|h| h.frontend.session.get_sni_hostname()).map(|s| s.to_string());
      if servername.as_ref().map(|s| s.as_str()) != Some(hostname_str) {
        error!("TLS SNI hostname '{:?}' and Host header '{}' don't match", servername, hostname_str);
        unwrap_msg!(session.http_mut()).set_answer(DefaultAnswerStatus::Answer404, None);
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
      session.set_answer(DefaultAnswerStatus::Answer400, None);
      return Err(ConnectionError::InvalidHost);
    };

    let rl:&RRequestLine = session.http()
      .and_then(|h| h.request.as_ref()).and_then(|r| r.get_request_line())
      .ok_or(ConnectionError::NoRequestLineGiven)?;
    match self.listeners.get(&listen_token).as_ref()
      .and_then(|l| l.frontend_from_request(&host, &rl.uri))
      .map(|ref front| front.app_id.clone()) {
      Some(app_id) => Ok(app_id),
      None => {
        session.set_answer(DefaultAnswerStatus::Answer404, None);
        Err(ConnectionError::HostNotFound)
      }
    }
  }

  fn check_circuit_breaker(&mut self, session: &mut Session) -> Result<(), ConnectionError> {
    if session.connection_attempt == CONN_RETRIES {
      error!("{} max connection attempt reached", session.log_context());
      session.set_answer(DefaultAnswerStatus::Answer503, None);
      Err(ConnectionError::NoBackendAvailable)
    } else {
      Ok(())
    }
  }

}

impl ProxyConfiguration<Session> for Proxy {
  fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
    self.listeners.get_mut(&Token(token.0)).unwrap().accept(token)
  }

  fn create_session(&mut self, mut frontend_sock: TcpStream, token: ListenToken,
    poll: &mut Poll, session_token: Token, wait_time: Duration)
    -> Result<(Rc<RefCell<Session>>, bool), AcceptError> {
      if let Some(ref listener) = self.listeners.get(&Token(token.0)) {
        if let Err(e) = frontend_sock.set_nodelay(true) {
          error!("error setting nodelay on front socket({:?}): {:?}", frontend_sock, e);
        }

        if let Err(e) = poll.registry().register(
          &mut frontend_sock,
          session_token,
          Interest::READABLE | Interest::WRITABLE
        ) {
          error!("error registering fron socket({:?}): {:?}", frontend_sock, e);
        }

        let session = ServerSession::new(&listener.ssl_config);
        let c = Session::new(session, frontend_sock, session_token, Rc::downgrade(&self.pool),
          listener.config.public_address.unwrap_or(listener.config.front),
          listener.config.expect_proxy, listener.config.sticky_name.clone(),
          listener.answers.clone(), Token(token.0), wait_time,
          Duration::seconds(listener.config.front_timeout as i64),
          Duration::seconds(listener.config.back_timeout as i64),
          Duration::seconds(listener.config.request_timeout as i64),
          );

        Ok((Rc::new(RefCell::new(c)), false))
      } else {
        //FIXME
        Err(AcceptError::IoError)
      }
    }

  fn connect_to_backend(&mut self, poll: &mut Poll,  session: &mut Session, back_token: Token) -> Result<BackendConnectAction,ConnectionError> {
    let old_app_id = session.http().and_then(|ref http| http.app_id.clone());
    let old_back_token = session.back_token();

    self.check_circuit_breaker(session)?;

    let app_id = self.app_id_from_request(session)?;

    if (session.http().and_then(|h| h.app_id.as_ref()) == Some(&app_id)) && session.back_connected == BackendConnectionStatus::Connected {
      let has_backend = session.backend.as_ref().map(|backend| {
         let ref backend = *backend.borrow();
         self.backends.borrow().has_backend(&app_id, backend)
        }).unwrap_or(false);

      let is_valid_backend_socket = has_backend &&
        session.http_mut().map(|h| h.is_valid_backend_socket()).unwrap_or(false);

      if is_valid_backend_socket {
        //matched on keepalive
        session.metrics.backend_id = session.backend.as_ref().map(|i| i.borrow().backend_id.clone());
        session.metrics.backend_start();
        session.http_mut().map(|h| h.backend_data.as_mut().map(|b| b.borrow_mut().active_requests += 1));
        return Ok(BackendConnectAction::Reuse);
      } else if let Some(token) = session.back_token() {
        session.close_backend(token, poll);

        //reset the back token here so we can remove it
        //from the slab after backend_from* fails
        session.set_back_token(token);
      }
    }

    if old_app_id.is_some() && old_app_id.as_ref() != Some(&app_id) {
      if let Some(token) = session.back_token() {
        session.close_backend(token, poll);

        //reset the back token here so we can remove it
        //from the slab after backend_from* fails
        session.set_back_token(token);
      }
    }

    session.app_id = Some(app_id.clone());

    let front_should_stick = self.applications.get(&app_id).map(|ref app| app.sticky_session).unwrap_or(false);
    let mut socket = self.backend_from_request(session, &app_id, front_should_stick)?;

    session.http_mut().map(|http| {
      http.app_id = Some(app_id.clone());
    });

    // we still want to use the new socket
    if let Err(e) = socket.set_nodelay(true) {
      error!("error setting nodelay on back socket: {:?}", e);
    }
    session.back_readiness().map(|r| {
      r.interest  = Ready::writable() | Ready::hup() | Ready::error();
    });

    let connect_timeout = time::Duration::seconds(i64::from(self.listeners.get(&session.listen_token).as_ref().map(|l| l.config.connect_timeout).unwrap()));

    session.back_connected = BackendConnectionStatus::Connecting(Instant::now());
    if let Some(back_token) = old_back_token {
      session.set_back_token(back_token);
      if let Err(e) = poll.registry().register(
        &mut socket,
        back_token,
        Interest::READABLE | Interest::WRITABLE
      ) {
        error!("error registering back socket: {:?}", e);
      }

      session.set_back_socket(socket);
      session.http_mut().map(|h| h.set_back_timeout(connect_timeout));
      Ok(BackendConnectAction::Replace)
    } else {
      if let Err(e) = poll.registry().register(
        &mut socket,
        back_token,
        Interest::READABLE | Interest::WRITABLE
      ) {
        error!("error registering back socket: {:?}", e);
      }

      session.set_back_socket(socket);
      session.set_back_token(back_token);
      session.http_mut().map(|h| h.set_back_timeout(connect_timeout));
      Ok(BackendConnectAction::New)
    }
  }

  fn notify(&mut self, event_loop: &mut Poll, message: ProxyRequest) -> ProxyResponse {
    //info!("{} notified", message);
    match message.order {
      ProxyRequestData::AddApplication(application) => {
        debug!("{} add application {:?}", message.id, application);
        self.add_application(application);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::RemoveApplication(application) => {
        debug!("{} remove application {:?}", message.id, application);
        self.remove_application(&application);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::AddHttpsFront(front) => {
        //info!("HTTPS\t{} add front {:?}", id, front);
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == front.address) {
          listener.add_https_front(front);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!("unknown listener: {:?}", front.address)
        }
      },
      ProxyRequestData::RemoveHttpsFront(front) => {
        //info!("HTTPS\t{} remove front {:?}", id, front);
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == front.address) {
          listener.remove_https_front(front);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!("unknown listener: {:?}", front.address)
        }
      },
      ProxyRequestData::AddCertificate(add_certificate) => {
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == add_certificate.front) {
          listener.add_certificate(add_certificate);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!()
        }
      },
      ProxyRequestData::RemoveCertificate(remove_certificate) => {
        //FIXME: should return an error if certificate still has fronts referencing it
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == remove_certificate.front) {
          listener.remove_certificate(remove_certificate);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!()
        }
      },
      ProxyRequestData::ReplaceCertificate(replace_certificate) => {
        //FIXME: should return an error if certificate still has fronts referencing it
        if let Some(listener) = self.listeners.values_mut().find(|l| l.address == replace_certificate.front) {
          listener.replace_certificate(replace_certificate);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!()
        }
      },
      ProxyRequestData::RemoveListener(remove) => {
        debug!("removing HTTPS listener at address: {:?}", remove.front);
        if !self.remove_listener(remove.front) {
          ProxyResponse {
              id: message.id,
              status: ProxyResponseStatus::Error(format!("no HTTPS listener to remove at address {:?}", remove.front)),
              data: None
          }
        } else {
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        }
      },
      ProxyRequestData::SoftStop => {
        info!("{} processing soft shutdown", message.id);
        for (_, l) in self.listeners.iter_mut() {
          l.listener.take().map(|mut sock| {
            if let Err(e) = event_loop.registry().deregister(&mut sock) {
              error!("error deregistering listen socket: {:?}", e);
            }
          });
        }
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Processing, data: None }
      },
      ProxyRequestData::HardStop => {
        info!("{} hard shutdown", message.id);
        for (_, mut l) in self.listeners.drain() {
          l.listener.take().map(|mut sock| {
            if let Err(e) = event_loop.registry().deregister(&mut sock) {
              error!("error deregistering listen socket: {:?}", e);
            }
          });
        }
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Processing, data: None }
      },
      ProxyRequestData::Status => {
        debug!("{} status", message.id);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::Logging(logging_filter) => {
        debug!("{} changing logging filter to {}", message.id, logging_filter);
        logging::LOGGER.with(|l| {
          let directives = logging::parse_logging_spec(&logging_filter);
          l.borrow_mut().set_directives(directives);
        });
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::Query(Query::Certificates(QueryCertificateType::All)) => {
        let res = self.listeners.iter().map(|(_addr, listener)| {
          let mut domains = (&unwrap_msg!(listener.resolver.0.lock()).domains).to_hashmap();
          let res = domains.drain().map(|(k, v)| {
            (String::from_utf8(k).unwrap(), v.0)
          }).collect();

          (listener.address, res)
        }).collect::<HashMap<_,_>>();

        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok,
          data: Some(ProxyResponseData::Query(QueryAnswer::Certificates(QueryAnswerCertificate::All(res)))) }
      },
      ProxyRequestData::Query(Query::Certificates(QueryCertificateType::Domain(d))) => {
        let res = self.listeners.iter().map(|(_addr, listener)| {
          let domains  = &unwrap_msg!(listener.resolver.0.lock()).domains;
          (listener.address, domains.domain_lookup(d.as_bytes(), true).map(|(k, v)| {
            (String::from_utf8(k.to_vec()).unwrap(), v.0.clone())
          }))
        }).collect::<HashMap<_,_>>();

        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok,
          data: Some(ProxyResponseData::Query(QueryAnswer::Certificates(QueryAnswerCertificate::Domain(res)))) }
      },
      command => {
        error!("{} unsupported message for rustls proxy, ignoring {:?}", message.id, command);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Error(String::from("unsupported message")), data: None }
      }
    }
  }

  fn listen_port_state(&self, port: &u16) -> ListenPortState {
    fixme!();
    ListenPortState::Available
    //if port == &self.address.port() { ListenPortState::InUse } else { ListenPortState::Available }
  }
}

use server::HttpsProvider;
pub fn start(config: HttpsListener, channel: ProxyChannel, max_buffers: usize, buffer_size: usize) {
  use server::{self,ProxySessionCast};

  let mut event_loop  = Poll::new().expect("could not create event loop");

  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(1, max_buffers, buffer_size)
  ));
  let backends = Rc::new(RefCell::new(BackendMap::new()));

  let mut sessions: Slab<Rc<RefCell<dyn ProxySessionCast>>> = Slab::with_capacity(max_buffers);
  {
    let entry = sessions.vacant_entry();
    info!("taking token {:?} for channel", SessionToken(entry.key()));
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = sessions.vacant_entry();
    info!("taking token {:?} for timer", SessionToken(entry.key()));
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = sessions.vacant_entry();
    info!("taking token {:?} for metrics", SessionToken(entry.key()));
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }

  let token = {
    let entry = sessions.vacant_entry();
    let key = entry.key();
    let _e = entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
    Token(key)
  };

  let front = config.front.clone();
  let mut configuration = Proxy::new(pool.clone(), backends.clone());
  if configuration.add_listener(config, token).is_some() &&
    configuration.activate_listener(&mut event_loop, &front, None).is_some() {
      let (scm_server, _scm_client) = UnixStream::pair().unwrap();
      let mut server_config: server::ServerConfig = Default::default();
      server_config.max_connections = max_buffers;
      let mut server  = Server::new(event_loop, channel,ScmSocket::new(scm_server.as_raw_fd()),
          sessions, pool, backends, None, Some(HttpsProvider::Rustls(configuration)),
          None, server_config, None, false);

      info!("starting event loop");
      server.run();
      info!("ending event loop");
  }
}

