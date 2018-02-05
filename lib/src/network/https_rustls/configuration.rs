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
use std::os::unix::io::{AsRawFd};
use std::io::{self,Read,Write,ErrorKind,BufReader};
use std::collections::{HashMap,HashSet};
use std::error::Error;
use slab::{Slab,Entry,VacantEntry};
use pool::{Pool,Checkout};
use std::net::{IpAddr,SocketAddr};
use std::str::{FromStr, from_utf8, from_utf8_unchecked};
use time::{precise_time_s, precise_time_ns};
use rand::random;
use rustls::{ServerConfig, ServerSession, NoClientAuth};
use nom::IResult;

use sozu_command::buffer::Buffer;
use sozu_command::channel::Channel;
use sozu_command::scm_socket::ScmSocket;
use sozu_command::messages::{self,Application,CertFingerprint,CertificateAndKey,
  Order,HttpsFront,HttpsProxyConfiguration,OrderMessage, OrderMessageAnswer,
  OrderMessageStatus,AddCertificate,RemoveCertificate,ReplaceCertificate};
use sozu_command::certificate::split_certificate_chain;

use parser::http11::{HttpState,RequestState,ResponseState,RRequestLine,parse_request_until_stop,hostname_and_port};
use network::buffer_queue::BufferQueue;
use network::{AppId,Backend,ClientResult,ConnectionError,Protocol};
use network::backends::BackendMap;
use network::proxy::{Server,ProxyChannel};
use network::session::{BackendConnectAction,BackendConnectionStatus,ProxyClient,ProxyConfiguration,
  Readiness,ListenToken,FrontToken,BackToken,AcceptError,Session,SessionMetrics};
use network::http::{self,DefaultAnswers};
use network::socket::{SocketHandler,SocketResult,server_bind,FrontRustls};
use network::trie::*;
use network::protocol::{ProtocolResult,TlsHandshake,Http,Pipe,StickySession};
use network::protocol::http::DefaultAnswerStatus;
use network::retry::RetryPolicy;
use util::UnwrapLog;

use super::resolver::{CertificateResolver,CertificateResolverWrapper};
use super::client::TlsClient;

type BackendToken = Token;

type ClientToken = Token;

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct TlsApp {
  pub app_id:           String,
  pub hostname:         String,
  pub path_begin:       String,
  pub cert_fingerprint: CertFingerprint,
}

pub type HostName  = String;
pub type PathBegin = String;

pub struct ServerConfiguration {
  listener:        Option<TcpListener>,
  address:         SocketAddr,
  applications:    HashMap<AppId, Application>,
  instances:       BackendMap,
  fronts:          HashMap<HostName, Vec<TlsApp>>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  answers:         DefaultAnswers,
  config:          HttpsProxyConfiguration,
  base_token:      usize,
  ssl_config:      Arc<ServerConfig>,
  resolver:        Arc<CertificateResolverWrapper>,
}

impl ServerConfiguration {
  pub fn new(config: HttpsProxyConfiguration, base_token: usize, event_loop: &mut Poll, start_at: usize,
    pool: Rc<RefCell<Pool<BufferQueue>>>, tcp_listener: Option<TcpListener>) -> io::Result<(ServerConfiguration, HashSet<ListenToken>)> {

    let mut fronts   = HashMap::new();
    let default_name = config.default_name.as_ref().map(|name| name.clone()).unwrap_or(String::new());

    let listener = tcp_listener.or_else(|| server_bind(&config.front).map_err(|e| {
      error!("could not create listener {:?}: {:?}", fronts, e);
    }).ok());

    let mut listeners = HashSet::new();
    if let Some(ref sock) = listener {
      event_loop.register(sock, Token(base_token), Ready::readable(), PollOpt::edge());
      listeners.insert(ListenToken(0));
    }

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

    let mut server_config = ServerConfig::new(NoClientAuth::new());
    let resolver = Arc::new(CertificateResolverWrapper::new());
    server_config.cert_resolver = resolver.clone();

    Ok((ServerConfiguration {
      listener:        listener,
      address:         config.front.clone(),
      applications:    HashMap::new(),
      instances:       BackendMap::new(),
      fronts:          fronts,
      pool:            pool,
      answers:         default,
      base_token:      base_token,
      config:          config,
      ssl_config:      Arc::new(server_config),
      resolver:        resolver,
    }, listeners))
  }

  pub fn give_back_listener(&mut self) -> Option<TcpListener> {
    self.listener.take()
  }

  pub fn add_application(&mut self, application: Application, event_loop: &mut Poll) {
    self.applications.insert(application.app_id.clone(), application);
  }

  pub fn remove_application(&mut self, app_id: &str, event_loop: &mut Poll) {
    self.applications.remove(app_id);
  }

  pub fn add_https_front(&mut self, tls_front: HttpsFront, event_loop: &mut Poll) -> bool {
    if !(*self.resolver).add_front(&tls_front.fingerprint) {
      return false;
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
      if let Some(pos) = fronts.iter()
        .position(|f| &f.app_id == &front.app_id && &f.cert_fingerprint == &front.fingerprint) {

        let front = fronts.remove(pos);
        (*self.resolver).remove_front(&front.cert_fingerprint) 
      }
    }
  }

  pub fn add_certificate(&mut self, add_certificate: AddCertificate, event_loop: &mut Poll) -> bool {
    (*self.resolver).add_certificate(add_certificate).is_some()
  }

  // FIXME: return an error if the cert is still in use
  pub fn remove_certificate(&mut self, remove_certificate: RemoveCertificate, event_loop: &mut Poll) {
    debug!("removing certificate {:?}", remove_certificate);
    (*self.resolver).remove_certificate(remove_certificate)
  }

  pub fn replace_certificate(&mut self, replace_certificate: ReplaceCertificate, event_loop: &mut Poll) {
    debug!("replacing certificate {:?}", replace_certificate);
    let ReplaceCertificate { new_certificate, old_fingerprint, old_names, new_names } = replace_certificate;
    let remove = RemoveCertificate {
      fingerprint: old_fingerprint,
      names: old_names,
    };
    let add = AddCertificate {
      certificate: new_certificate,
      names: new_names,
    };

    //FIXME: handle results
    (*self.resolver).remove_certificate(remove);
    (*self.resolver).add_certificate(add);
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
  fn accept(&mut self, token: ListenToken, poll: &mut Poll, entry: VacantEntry<TlsClient, FrontToken>,
           client_token: Token) -> Result<(FrontToken,bool), AcceptError> {
    if let Some(ref listener) = self.listener.as_ref() {
      listener.accept().map_err(|e| {
        match e.kind() {
          ErrorKind::WouldBlock => AcceptError::WouldBlock,
          other => {
            error!("accept() IO error: {:?}", e);
            AcceptError::IoError
          }
        }
      }).map(|(frontend_sock, _)| {
        frontend_sock.set_nodelay(true);
        let session = ServerSession::new(&self.ssl_config);
        let mut c = TlsClient::new(session, frontend_sock, Rc::downgrade(&self.pool), self.config.public_address);

        c.readiness().front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
        c.set_front_token(client_token);
        poll.register(
          c.front_socket(),
          client_token,
          Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
          PollOpt::edge()
        );

        let index = entry.index();
        entry.insert(c);

        (index, false)
      })
    } else {
      Err(AcceptError::IoError)
    }
  }

  fn connect_to_backend(&mut self, poll: &mut Poll, client: &mut TlsClient, entry: Entry<FrontToken, BackToken>, back_token: Token) -> Result<BackendConnectAction,ConnectionError> {
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
      let servername: Option<String> = unwrap_msg!(client.http()).frontend.session.get_sni_hostname().map(|s| s.to_string());
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
            poll.deregister(*sock);
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
          poll.deregister(*sock);
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
              poll.deregister(*sock);
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

          if old_app_id == new_app_id {
            entry.remove();

            poll.register(
              &socket,
              client.back_token().expect("FIXME"),
              Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
              PollOpt::edge()
            );

            client.set_back_socket(socket);
            Ok(BackendConnectAction::Replace)
          } else {
            poll.register(
              &socket,
              back_token,
              Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
              PollOpt::edge()
            );

            client.set_back_socket(socket);
            client.set_back_token(back_token);
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
    info!("{} notified", message);
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
      Order::AddCertificate(add_certificate) => {
        self.add_certificate(add_certificate, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveCertificate(remove_certificate) => {
        //info!("TLS\t{} remove certificate with fingerprint {:?}", id, fingerprint);
        self.remove_certificate(remove_certificate, event_loop);
        //FIXME: should return an error if certificate still has fronts referencing it
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::ReplaceCertificate(replace_certificate) => {
        //info!("TLS\t{} remove certificate with fingerprint {:?}", id, fingerprint);
        self.replace_certificate(replace_certificate, event_loop);
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
        self.listener.as_ref().map(|sock| {
          event_loop.deregister(sock);
        });
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Processing, data: None }
      },
      Order::HardStop => {
        info!("{} hard shutdown", message.id);
        self.listener.as_ref().map(|sock| {
          event_loop.deregister(sock);
        });
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

pub type RustlsServer = Session<ServerConfiguration,TlsClient>;

pub fn start(config: HttpsProxyConfiguration, channel: ProxyChannel, max_buffers: usize, buffer_size: usize) {
  let mut event_loop  = Poll::new().expect("could not create event loop");
  let max_listeners   = 1;

  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(2*max_buffers, 0, || BufferQueue::with_capacity(buffer_size))
  ));

  // start at max_listeners + 1 because token(0) is the channel, and token(1) is the timer
  if let Ok((configuration, listeners)) = ServerConfiguration::new(config, 6148914691236517205, &mut event_loop, 1 + max_listeners, pool, None) {
    let session = Session::new(max_listeners, max_buffers, 6148914691236517205, configuration, listeners, &mut event_loop);
    let (scm_server, scm_client) = UnixStream::pair().unwrap();
    let mut server  = Server::new(event_loop, channel, ScmSocket::new(scm_server.as_raw_fd()),
      None, Some(session), None, None);

    info!("starting event loop");
    server.run();
    info!("ending event loop");
  }
}

