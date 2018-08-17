use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::sync::{Arc,Mutex};
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::mem;
use std::net::Shutdown;
use std::os::unix::io::RawFd;
use std::os::unix::io::{FromRawFd,AsRawFd};
use mio::*;
use mio::net::*;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
use std::io::{self,Read,Write,ErrorKind,BufReader};
use std::collections::{HashMap,HashSet};
use std::error::Error;
use slab::{Slab,Entry,VacantEntry};
use std::net::{IpAddr,SocketAddr};
use std::str::{FromStr, from_utf8, from_utf8_unchecked};
use time::{precise_time_s, precise_time_ns, SteadyTime, Duration};
use rand::random;
use openssl::ssl::{self, SslContext, SslContextBuilder, SslMethod, SslAlert,
                   Ssl, SslOptions, SslRef, SslStream, SniError, NameType};
use openssl::x509::X509;
use openssl::dh::Dh;
use openssl::pkey::PKey;
use openssl::hash::MessageDigest;
use openssl::nid;
use openssl::error::ErrorStack;
use openssl::ssl::SslVersion;
use nom::IResult;
use mio_extras::timer::{Timer,Timeout};

use sozu_command::config::LoadBalancingAlgorithms;
use sozu_command::buffer::Buffer;
use sozu_command::channel::Channel;
use sozu_command::scm_socket::ScmSocket;
use sozu_command::messages::{self,Application,CertFingerprint,CertificateAndKey,
  Order,HttpsFront,HttpsListener,OrderMessage,OrderMessageAnswer,
  OrderMessageStatus,LoadBalancingParams,TlsVersion};
use sozu_command::logging;

use parser::http11::{HttpState,RequestState,ResponseState,RRequestLine,parse_request_until_stop,hostname_and_port};
use network::pool::{Pool,Checkout};
use network::buffer_queue::BufferQueue;
use network::{AppId,Backend,ClientResult,ConnectionError,Protocol,Readiness,SessionMetrics,
  ProxyClient,ProxyConfiguration,AcceptError,BackendConnectAction,BackendConnectionStatus,
  CloseResult};
use network::backends::BackendMap;
use network::proxy::{Server,ProxyChannel,ListenToken,ListenPortState,ClientToken,ListenClient};
use network::http::{self,DefaultAnswers};
use network::socket::{SocketHandler,SocketResult,server_bind};
use network::trie::*;
use network::protocol::{ProtocolResult,Http,Pipe,StickySession};
use network::protocol::openssl::TlsHandshake;
use network::protocol::http::DefaultAnswerStatus;
use network::protocol::proxy_protocol::expect::ExpectProxyProtocol;
use network::retry::RetryPolicy;
use network::tcp;
use util::UnwrapLog;
use network::https_rustls;

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct TlsApp {
  pub app_id:           String,
  pub hostname:         String,
  pub path_begin:       String,
  pub cert_fingerprint: CertFingerprint,
}

pub enum State {
  Expect(ExpectProxyProtocol<TcpStream>, Ssl),
  Handshake(TlsHandshake),
  Http(Http<SslStream<TcpStream>>),
  WebSocket(Pipe<SslStream<TcpStream>>)
}

pub struct TlsClient {
  frontend_token: Token,
  backend:          Option<Rc<RefCell<Backend>>>,
  back_connected:   BackendConnectionStatus,
  protocol:         Option<State>,
  public_address:   Option<IpAddr>,
  ssl:              Option<Ssl>,
  pool:             Weak<RefCell<Pool<BufferQueue>>>,
  sticky_name:      String,
  metrics:          SessionMetrics,
  pub app_id:       Option<String>,
  timeout:          Timeout,
  last_event:       SteadyTime,
  pub listen_token: Token,
}

impl TlsClient {
  pub fn new(ssl:Ssl, sock: TcpStream, token: Token, pool: Weak<RefCell<Pool<BufferQueue>>>, public_address: Option<IpAddr>,
    expect_proxy: bool, sticky_name: String, timeout: Timeout, listen_token: Token) -> TlsClient {
    let protocol = if expect_proxy {
      trace!("starting in expect proxy state");
      gauge_add!("protocol.proxy.expect", 1);
      Some(State::Expect(ExpectProxyProtocol::new(sock, token), ssl))
    } else {
      gauge_add!("protocol.tls.handshake", 1);
      Some(State::Handshake(TlsHandshake::new(ssl, sock)))
    };

    let mut client = TlsClient {
      frontend_token: token,
      backend:        None,
      back_connected: BackendConnectionStatus::NotConnected,
      protocol:       protocol,
      public_address: public_address,
      ssl:            None,
      pool:           pool,
      sticky_name:    sticky_name,
      metrics:        SessionMetrics::new(),
      app_id:         None,
      timeout,
      last_event:     SteadyTime::now(),
      listen_token,
    };
    client.readiness().front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();

    client
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

  pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Rc<Vec<u8>>)  {
    self.protocol.as_mut().map(|protocol| {
      if let &mut State::Http(ref mut http) = protocol {
        http.set_answer(answer, buf);
      }
    });
  }

  pub fn upgrade(&mut self) -> bool {
    let protocol = unwrap_msg!(self.protocol.take());

    if let State::Expect(expect, ssl) = protocol {
      debug!("switching to TLS handshake");
      if let Some(ref addresses) = expect.addresses {
        if let (Some(public_address), Some(client_address)) = (addresses.destination(), addresses.source()) {
          self.public_address = Some(public_address.ip());

          let ExpectProxyProtocol {
            frontend, frontend_token, readiness, .. } = expect;
          let mut tls = TlsHandshake::new(ssl, frontend);
          tls.readiness.front_readiness = readiness.front_readiness;
          tls.readiness.back_readiness  = readiness.back_readiness;

          gauge_add!("protocol.proxy.expect", -1);
          gauge_add!("protocol.tls.handshake", 1);
          self.protocol = Some(State::Handshake(tls));
          return true;
        }
      }

      error!("failed to upgrade from expect");
      self.protocol = Some(State::Expect(expect, ssl));
      false

    } else if let State::Handshake(handshake) = protocol {
      let pool = self.pool.clone();
      let readiness = handshake.readiness.clone();

      handshake.stream.as_ref().map(|s| {
        let ssl = s.ssl();
        ssl.version2().map(|version| {
          incr!(version_str(version));
        });
        ssl.current_cipher().map(|c| incr!(c.name()));
      });

      let http = Http::new(unwrap_msg!(handshake.stream), self.frontend_token.clone(), pool,
        self.public_address.clone(), None, self.sticky_name.clone(), Protocol::HTTPS).map(|mut http| {

        http.readiness = readiness;
        http.readiness.front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
        State::Http(http)
      });

      if http.is_none() {
        error!("could not upgrade to HTTP");
        //we cannot put back the protocol since we moved the stream
        //self.protocol = Some(State::Handshake(handshake));
        return false;
      }
      gauge_add!("protocol.tls.handshake", -1);
      gauge_add!("protocol.https", 1);

      self.ssl = handshake.ssl;
      self.protocol = http;
      return true;
    } else if let State::Http(http) = protocol {
      debug!("https switching to wss");
      let front_token = self.frontend_token;
      let back_token  = unwrap_msg!(http.back_token());

      let front_buf = match http.front_buf {
        Some(buf) => buf,
        None => if let Some(p) = self.pool.upgrade() {
          if let Some(buf) = p.borrow_mut().checkout() {
            buf
          } else {
            return false;
          }
        } else {
          return false;
        }
      };
      let back_buf = match http.back_buf {
        Some(buf) => buf,
        None => if let Some(p) = self.pool.upgrade() {
          if let Some(buf) = p.borrow_mut().checkout() {
            buf
          } else {
            return false;
          }
        } else {
          return false;
        }
      };

      gauge_add!("protocol.https", -1);
      gauge_add!("protocol.wss", 1);

      let mut pipe = Pipe::new(http.frontend, front_token, Some(unwrap_msg!(http.backend)),
        front_buf, back_buf, http.public_address);

      pipe.readiness.front_readiness = http.readiness.front_readiness;
      pipe.readiness.back_readiness  = http.readiness.back_readiness;
      pipe.set_back_token(back_token);

      self.protocol = Some(State::WebSocket(pipe));
      true
    } else {
      self.protocol = Some(protocol);
      true
    }
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
      },
      State::Expect(_,_)             => {
        error!("why a backend HUP event while still in frontend proxy protocol expect?");
        ClientResult::CloseClient
      }
    }
  }

  fn log_context(&self)  -> String {
    if let &State::Http(ref http) = unwrap_msg!(self.protocol.as_ref()) {
      http.log_context()
    } else {
      "".to_string()
    }
  }

  fn readable(&mut self)      -> ClientResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(ref mut expect,_)     => expect.readable(&mut self.metrics),
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
      State::Expect(_,_)                  => ClientResult::CloseClient,
      State::Handshake(ref mut handshake) => ClientResult::CloseClient,
      State::Http(ref mut http)           => http.writable(&mut self.metrics),
      State::WebSocket(ref mut pipe)      => pipe.writable(&mut self.metrics),
    }
  }

  fn back_readable(&mut self) -> ClientResult {
    let (upgrade, result) = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(_,_)                  => (ProtocolResult::Continue, ClientResult::CloseClient),
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
        ClientResult::CloseClient
      }
    }
  }

  fn back_writable(&mut self) -> ClientResult {
    match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(_,_)                  => ClientResult::CloseClient,
      State::Handshake(ref mut handshake) => ClientResult::CloseClient,
      State::Http(ref mut http)           => http.back_writable(&mut self.metrics),
      State::WebSocket(ref mut pipe)      => pipe.back_writable(&mut self.metrics),
    }
  }

  fn front_socket(&self) -> Option<&TcpStream> {
    match unwrap_msg!(self.protocol.as_ref()) {
      &State::Expect(ref expect,_)     => Some(expect.front_socket()),
      &State::Handshake(ref handshake) => handshake.socket(),
      &State::Http(ref http)           => Some(http.front_socket()),
      &State::WebSocket(ref pipe)      => Some(pipe.front_socket()),
    }
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    match unwrap_msg!(self.protocol.as_ref()) {
      &State::Expect(_,_)              => None,
      &State::Handshake(ref handshake) => None,
      &State::Http(ref http)           => http.back_socket(),
      &State::WebSocket(ref pipe)      => pipe.back_socket(),
    }
  }

  fn back_token(&self)   -> Option<Token> {
    if let &State::Http(ref http) = unwrap_msg!(self.protocol.as_ref()) {
      http.back_token()
    } else {
      None
    }
  }

  fn set_back_socket(&mut self, sock:TcpStream) {
    unwrap_msg!(self.http()).set_back_socket(sock)
  }

  fn set_back_token(&mut self, token: Token) {
    unwrap_msg!(self.http()).set_back_token(token)
  }

  fn back_connected(&self)     -> BackendConnectionStatus {
    self.back_connected
  }

  fn set_back_connected(&mut self, connected: BackendConnectionStatus) {
    self.back_connected = connected;

    if connected == BackendConnectionStatus::Connected {
      gauge_add!("backend.connections", 1);
      self.backend.as_ref().map(|backend| {
        let ref mut backend = *backend.borrow_mut();
        backend.failures = 0;
        backend.retry_policy.succeed();
      });
    }
  }

  fn metrics(&mut self)        -> &mut SessionMetrics {
    &mut self.metrics
  }

  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    let addr:Option<SocketAddr> = self.back_socket().and_then(|sock| sock.peer_addr().ok());
    (self.app_id.clone(), addr)
  }

  fn readiness(&mut self)      -> &mut Readiness {
    let r = match *unwrap_msg!(self.protocol.as_mut()) {
      State::Expect(ref mut expect, _)    => &mut expect.readiness,
      State::Handshake(ref mut handshake) => &mut handshake.readiness,
      State::Http(ref mut http)           => http.readiness(),
      State::WebSocket(ref mut pipe)      => &mut pipe.readiness,
    };
    //info!("current readiness: {:?}", r);
    r
  }
}

impl ProxyClient for TlsClient {

  fn close(&mut self, poll: &mut Poll) -> CloseResult {
    //println!("TLS closing[{:?}] temp->front: {:?}, temp->back: {:?}", self.frontend_token, *self.temp.front_buf, *self.temp.back_buf);
    self.http().map(|http| http.close());
    self.metrics.service_stop();
    if let Some(front_socket) = self.front_socket() {
      front_socket.shutdown(Shutdown::Both);
      poll.deregister(front_socket);
    }

    let mut result = CloseResult::default();

    if let Some(tk) = self.back_token() {
      result.tokens.push(tk)
    }

    if let (Some(app_id), Some(addr)) = self.remove_backend() {
      result.backends.push((app_id, addr.clone()));
    }

    let back_connected = self.back_connected();
    if back_connected != BackendConnectionStatus::NotConnected {
      if let Some(sock) = self.back_socket() {
        sock.shutdown(Shutdown::Both);
        poll.deregister(sock);
      }
    }

    if back_connected == BackendConnectionStatus::Connected {
      gauge_add!("backend.connections", -1);
    }

    self.set_back_connected(BackendConnectionStatus::NotConnected);

    if let Some(State::Http(ref http)) = self.protocol {
      //if the state was initial, the connection was already reset
      if unwrap_msg!(http.state.as_ref()).request != Some(RequestState::Initial) {
        gauge_add!("http.active_requests", -1);
      }
    }

    match self.protocol {
      Some(State::Expect(_,_)) => gauge_add!("protocol.proxy.expect", -1),
      Some(State::Handshake(_)) => gauge_add!("protocol.tls.handshake", -1),
      Some(State::Http(_)) => gauge_add!("protocol.https", -1),
      Some(State::WebSocket(_)) => gauge_add!("protocol.wss", -1),
      None => {}
    }

    result.tokens.push(self.frontend_token);

    result
  }

  fn timeout(&self, token: Token, timer: &mut Timer<Token>) -> ClientResult {
    if self.frontend_token == token {
      let dur = SteadyTime::now() - self.last_event;
      let keepalive_timeout = Duration::seconds(60);
      if dur < keepalive_timeout {
        timer.set_timeout((keepalive_timeout - dur).to_std().unwrap(), token);
        ClientResult::Continue
      } else {
        ClientResult::CloseClient
      }
    } else {
      // invalid token, obsolete timeout triggered
      ClientResult::Continue
    }
  }

  fn cancel_timeouts(&self, timer: &mut Timer<Token>) {
    timer.cancel_timeout(&self.timeout);
  }

  fn close_backend(&mut self, _: Token, poll: &mut Poll) -> Option<(String,SocketAddr)> {
    let mut res = None;
    if let (Some(app_id), Some(addr)) = self.remove_backend() {
      res = Some((app_id, addr.clone()));
    }

    let back_connected = self.back_connected();
    if back_connected != BackendConnectionStatus::NotConnected {
      if let Some(sock) = self.back_socket() {
        sock.shutdown(Shutdown::Both);
        poll.deregister(sock);
      }
    }

    if back_connected == BackendConnectionStatus::Connected {
      gauge_add!("backend.connections", -1);
    }

    self.set_back_connected(BackendConnectionStatus::NotConnected);

    res
  }

  fn protocol(&self) -> Protocol {
    Protocol::HTTPS
  }

  fn process_events(&mut self, token: Token, events: Ready) {
    trace!("token {:?} got event {}", token, super::unix_ready_to_string(UnixReady::from(events)));
    self.last_event = SteadyTime::now();

    if self.frontend_token == token {
      self.readiness().front_readiness = self.readiness().front_readiness | UnixReady::from(events);
    } else if self.back_token() == Some(token) {
      self.readiness().back_readiness = self.readiness().back_readiness | UnixReady::from(events);
    }
  }

  fn ready(&mut self) -> ClientResult {
    let mut counter = 0;
    let max_loop_iterations = 100000;

    self.metrics().service_start();

    if self.back_connected() == BackendConnectionStatus::Connecting {
      if self.readiness().back_readiness.is_hup() {
        //retry connecting the backend
        //FIXME: there should probably be a circuit breaker per client too
        error!("{} error connecting to backend, trying again", self.log_context());
        self.metrics().service_stop();
        let backend_token = self.back_token();
        self.http().map(|h| h.remove_backend());
        return ClientResult::ReconnectBackend(Some(self.frontend_token), backend_token);
      } else if self.readiness().back_readiness != UnixReady::from(Ready::empty()) {
        self.set_back_connected(BackendConnectionStatus::Connected);
      }
    }

    if self.readiness().front_readiness.is_hup() {
      let order = self.front_hup();
      match order {
        ClientResult::CloseClient => {
          return order;
        },
        _ => {
          self.readiness().front_readiness.remove(UnixReady::hup());
          return order;
        }
      }
    }

    let token = self.frontend_token.clone();
    while counter < max_loop_iterations {
      let front_interest = self.readiness().front_interest & self.readiness().front_readiness;
      let back_interest  = self.readiness().back_interest & self.readiness().back_readiness;

      trace!("PROXY\t{} {:?} {:?}", self.log_context(), token, self.readiness());

      if front_interest == UnixReady::from(Ready::empty()) && back_interest == UnixReady::from(Ready::empty()) {
        break;
      }

      if self.readiness().back_readiness.is_hup() && self.readiness().front_interest.is_writable() &&
        ! self.readiness().front_readiness.is_writable() {
        break;
      }

      if front_interest.is_readable() {
        let order = self.readable();
        trace!("front readable\tinterpreting client order {:?}", order);

        if order != ClientResult::Continue {
          return order;
        }
      }

      if back_interest.is_writable() {
        let order = self.back_writable();
        if order != ClientResult::Continue {
          return order;
        }
      }

      if back_interest.is_readable() {
        let order = self.back_readable();
        if order != ClientResult::Continue {
          return order;
        }
      }

      if front_interest.is_writable() {
        let order = self.writable();
        trace!("front writable\tinterpreting client order {:?}", order);
        if order != ClientResult::Continue {
          return order;
        }
      }

      if back_interest.is_hup() {
        let order = self.back_hup();
        match order {
          ClientResult::CloseClient => {
            return order;
          },
          ClientResult::Continue => {
            /*self.readiness().front_interest.insert(Ready::writable());
            if ! self.readiness().front_readiness.is_writable() {
              error!("got back socket HUP but there's still data, and front is not writable yet(openssl)[{:?} => {:?}]", self.frontend_token, self.back_token());
              break;
            }*/
          },
          _ => {
            self.readiness().back_readiness.remove(UnixReady::hup());
            return order;
          }
        };
      }

      if front_interest.is_error() || back_interest.is_error() {
        if front_interest.is_error() {
          error!("PROXY client {:?} front error, disconnecting", self.frontend_token);
        } else {
          error!("PROXY client {:?} back error, disconnecting", self.frontend_token);
        }

        self.readiness().front_interest = UnixReady::from(Ready::empty());
        self.readiness().back_interest  = UnixReady::from(Ready::empty());
        return ClientResult::CloseClient;
      }

      counter += 1;
    }

    if counter == max_loop_iterations {
      error!("PROXY\thandling client {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection", self.frontend_token, max_loop_iterations);

      let front_interest = self.readiness().front_interest & self.readiness().front_readiness;
      let back_interest  = self.readiness().back_interest & self.readiness().back_readiness;

      let token = self.frontend_token.clone();
      error!("PROXY\t{:?} readiness: {:?} | front: {:?} | back: {:?} ", token,
        self.readiness(), front_interest, back_interest);

      return ClientResult::CloseClient;
    }

    ClientResult::Continue
  }

  fn last_event(&self) -> SteadyTime {
    self.last_event
  }

  fn print_state(&self) {
    let p:String = match &self.protocol {
      Some(State::Expect(_,_))  => String::from("Expect"),
      Some(State::Handshake(_)) => String::from("Handshake"),
      Some(State::Http(h))      => format!("HTTPS: {:?}", h.state),
      Some(State::WebSocket(_)) => String::from("WSS"),
      None                      => String::from("None"),
    };

    let r = match *unwrap_msg!(self.protocol.as_ref()) {
      State::Expect(ref expect, _)    => &expect.readiness,
      State::Handshake(ref handshake) => &handshake.readiness,
      State::Http(ref http)           => &http.readiness,
      State::WebSocket(ref pipe)      => &pipe.readiness,
    };

    error!("zombie client[{:?} => {:?}], state => readiness: {:?}, protocol: {}, app_id: {:?}, back_connected: {:?}, metrics: {:?}",
      self.frontend_token, self.back_token(), r, p, self.app_id, self.back_connected, self.metrics);
  }

  fn tokens(&self) -> Vec<Token> {
    let mut v = vec![self.frontend_token];
    if let Some(tk) = self.back_token() {
      v.push(tk)
    }

    v
  }
}

fn get_cert_common_name(cert: &X509) -> Option<String> {
    cert.subject_name().entries_by_nid(nid::Nid::COMMONNAME).next().and_then(|name| name.data().as_utf8().ok().map(|name| (&*name).to_string()))
}

pub type HostName  = String;
pub type PathBegin = String;
pub struct TlsData {
  context:     SslContext,
  certificate: Vec<u8>,
  refcount:    usize,
  initialized: bool,
}

pub struct Listener {
  listener:        Option<TcpListener>,
  address:         SocketAddr,
  fronts:          TrieNode<Vec<TlsApp>>,
  domains:         Arc<Mutex<TrieNode<CertFingerprint>>>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  contexts:        Arc<Mutex<HashMap<CertFingerprint,TlsData>>>,
  default_context: SslContext,
  answers:         DefaultAnswers,
  config:          HttpsListener,
  ssl_options:     SslOptions,
  pub token:       Token,
  active:          bool,
}

impl Listener {
  pub fn new(config: HttpsListener, pool: Rc<RefCell<Pool<BufferQueue>>>, token: Token) -> Listener {

    let contexts:HashMap<CertFingerprint,TlsData> = HashMap::new();
    let domains      = TrieNode::root();
    let fronts       = TrieNode::root();
    let rc_ctx       = Arc::new(Mutex::new(contexts));
    let ref_ctx      = rc_ctx.clone();
    let rc_domains   = Arc::new(Mutex::new(domains));
    let ref_domains  = rc_domains.clone();

    let (default_context, ssl_options):(SslContext, SslOptions) =
      Self::create_default_context(&config, ref_ctx, ref_domains).expect("could not create default context");

    let default = DefaultAnswers {
      NotFound: Rc::new(Vec::from(config.answer_404.as_bytes())),
      ServiceUnavailable: Rc::new(Vec::from(config.answer_503.as_bytes())),
      BadRequest: Rc::new(Vec::from(
        &b"HTTP/1.1 400 Bad Request\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
      )),
    };

    Listener {
      listener:        None,
      address:         config.front.clone(),
      domains:         rc_domains,
      default_context: default_context,
      contexts:        rc_ctx,
      answers:         default,
      active:          false,
      fronts,
      pool,
      config,
      ssl_options,
      token,
    }
  }

  pub fn activate(&mut self, event_loop: &mut Poll, tcp_listener: Option<TcpListener>) -> Option<Token> {
    if self.active {
      return None;
    }

    let listener = tcp_listener.or_else(|| server_bind(&self.config.front).map_err(|e| {
      error!("could not create listener {:?}: {:?}", self.config.front, e);
    }).ok());


    if let Some(ref sock) = listener {
      event_loop.register(sock, self.token, Ready::readable(), PollOpt::edge());
    } else {
      return None;
    }

    self.listener = listener;
    self.active = true;
    Some(self.token)
  }

  pub fn create_default_context(config: &HttpsListener, ref_ctx: Arc<Mutex<HashMap<CertFingerprint,TlsData>>>,
    ref_domains: Arc<Mutex<TrieNode<CertFingerprint>>>) -> Option<(SslContext, SslOptions)> {
    let ctx = SslContext::builder(SslMethod::tls());
    if let Err(e) = ctx {
      return None
    }

    let mut context = ctx.expect("should have built a correct SSL context");

    let mut mode = ssl::SslMode::ENABLE_PARTIAL_WRITE;
    mode.insert(ssl::SslMode::ACCEPT_MOVING_WRITE_BUFFER);
    //FIXME: maybe activate ssl::SslMode::RELEASE_BUFFERS to save some memory?
    context.set_mode(mode);


    let mut ssl_options = ssl::SslOptions::CIPHER_SERVER_PREFERENCE | ssl::SslOptions::NO_COMPRESSION | ssl::SslOptions::NO_TICKET;
    let mut versions = ssl::SslOptions::NO_SSLV2 |
      ssl::SslOptions::NO_SSLV3 | ssl::SslOptions::NO_TLSV1 |
      ssl::SslOptions::NO_TLSV1_1 | ssl::SslOptions::NO_TLSV1_2;

    for version in config.versions.iter() {
      match version {
        TlsVersion::SSLv2   => versions.remove(ssl::SslOptions::NO_SSLV2),
        TlsVersion::SSLv3   => versions.remove(ssl::SslOptions::NO_SSLV3),
        TlsVersion::TLSv1_0 => versions.remove(ssl::SslOptions::NO_TLSV1),
        TlsVersion::TLSv1_1 => versions.remove(ssl::SslOptions::NO_TLSV1_1),
        TlsVersion::TLSv1_2 => versions.remove(ssl::SslOptions::NO_TLSV1_2),
        s         => error!("unrecognized TLS version: {:?}", s)
      };
    }

    ssl_options.insert(versions);
    trace!("parsed tls options: {:?}", ssl_options);

    let opt = context.set_options(ssl_options);

    context.set_cipher_list(&config.cipher_list);

    if let Err(e) = setup_curves(&mut context) {
      error!("could not setup curves for openssl");
    }

    context.set_servername_callback(move |ssl: &mut SslRef, alert: &mut SslAlert| {
      let contexts = unwrap_msg!(ref_ctx.lock());
      let domains  = unwrap_msg!(ref_domains.lock());

      trace!("ref: {:?}", ssl);
      if let Some(servername) = ssl.servername(NameType::HOST_NAME).map(|s| s.to_string()) {
        debug!("looking for fingerprint for {:?}", servername);
        if let Some(kv) = domains.domain_lookup(servername.as_bytes()) {
          debug!("looking for context for {:?} with fingerprint {:?}", servername, kv.1);
          if let Some(ref tls_data) = contexts.get(&kv.1) {
            debug!("found context for {:?}", servername);
            if !tls_data.initialized {
              //FIXME: couldn't we skip to the next cert?
              error!("no application is using that certificate (looking up {})", servername);
              *alert = SslAlert::UNRECOGNIZED_NAME;
              return Err(SniError::ALERT_FATAL);
            }
            let context: &SslContext = &tls_data.context;
            if let Ok(()) = ssl.set_ssl_context(context) {
              debug!("servername is now {:?}", ssl.servername(NameType::HOST_NAME));
              return Ok(());
            } else {
              error!("no context found for {:?}", servername);
              *alert = SslAlert::UNRECOGNIZED_NAME;
            }
          }
        }
      }
      *alert = SslAlert::UNRECOGNIZED_NAME;
      return Err(SniError::ALERT_FATAL);
    });

    Some((context.build(), ssl_options))
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

    if let Some((_, ref mut fronts)) = self.fronts.domain_lookup_mut(&tls_front.hostname.clone().into_bytes()) {
        if ! fronts.contains(&app) {
          fronts.push(app.clone());
        }
    }
    if self.fronts.domain_lookup(&tls_front.hostname.clone().into_bytes()).is_none() {
      self.fronts.domain_insert(tls_front.hostname.into_bytes(), vec![app]);
    }

    true
  }

  pub fn remove_https_front(&mut self, front: HttpsFront, event_loop: &mut Poll) {
    debug!("removing tls_front {:?}", front);

    let should_delete = {
      let fronts_opt = self.fronts.domain_lookup_mut(front.hostname.as_bytes());
      if let Some((_, fronts)) = fronts_opt {
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

      fronts_opt.as_ref().map(|(_,fronts)| fronts.len() == 0).unwrap_or(false)
    };

    if should_delete {
      self.fronts.domain_remove(&front.hostname.into());
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

  // ToDo factor out with http.rs
  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<&TlsApp> {
    let host: &str = if let IResult::Done(i, (hostname, port)) = hostname_and_port(host.as_bytes()) {
      if i != &b""[..] {
        error!("frontend_from_request: invalid remaining chars after hostname. Host: {}", host);
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

    if let Some((_, http_fronts)) = self.fronts.domain_lookup(host.as_bytes()) {
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

  fn accept(&mut self, token: ListenToken, poll: &mut Poll, client_token: Token, timeout: Timeout)
    -> Result<(Rc<RefCell<TlsClient>>,bool), AcceptError> {
    if let Some(ref sock) = self.listener {
      sock.accept().map_err(|e| {
        match e.kind() {
          ErrorKind::WouldBlock => AcceptError::WouldBlock,
          other => {
            error!("accept() IO error: {:?}", e);
            AcceptError::IoError
          }
        }
      }).and_then(|(frontend_sock, _)| {
        frontend_sock.set_nodelay(true);
        if let Ok(ssl) = Ssl::new(&self.default_context) {
          poll.register(
            &frontend_sock,
            client_token,
            Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
            PollOpt::edge()
          );
          let c = TlsClient::new(ssl, frontend_sock, client_token, Rc::downgrade(&self.pool),
            self.config.public_address, self.config.expect_proxy, self.config.sticky_name.clone(), timeout, Token(token.0));

          Ok((Rc::new(RefCell::new(c)), false))
        } else {
          error!("could not create ssl context");
          Err(AcceptError::IoError)
        }
      })
    } else {
      error!("cannot accept connections, no listening socket available");
      Err(AcceptError::IoError)
    }
  }

  fn accept_flush(&mut self) -> usize {
    let mut counter = 0;
    if let Some(ref sock) = self.listener {
      while sock.accept().is_ok() {
        counter += 1;
      }
    }
    counter
  }

}

pub struct ServerConfiguration {
  listeners:    HashMap<Token, Listener>,
  applications: HashMap<AppId, Application>,
  backends:     BackendMap,
  pool:         Rc<RefCell<Pool<BufferQueue>>>,
}

impl ServerConfiguration {
  pub fn new(pool: Rc<RefCell<Pool<BufferQueue>>>) -> ServerConfiguration {
    ServerConfiguration {
      listeners : HashMap::new(),
      applications: HashMap::new(),
      backends: BackendMap::new(),
      pool,
    }
  }

  pub fn add_listener(&mut self, config: HttpsListener, pool: Rc<RefCell<Pool<BufferQueue>>>, token: Token) -> Option<Token> {
    if self.listeners.contains_key(&token) {
      None
    } else {
      let listener = Listener::new(config, pool, token);
      self.listeners.insert(listener.token.clone(), listener);
      Some(token)
    }
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
      (l.address.clone(), l.listener.take().unwrap())
    }).collect()
  }

  pub fn add_application(&mut self, application: Application, event_loop: &mut Poll) {
    let app_id = &application.app_id.clone();
    let lb_alg = application.load_balancing_policy;

    self.applications.insert(application.app_id.clone(), application);
    self.backends.set_load_balancing_policy_for_app(app_id, lb_alg);
  }

  pub fn remove_application(&mut self, app_id: &str, event_loop: &mut Poll) {
    self.applications.remove(app_id);
  }


  pub fn add_backend(&mut self, app_id: &str, backend: Backend, event_loop: &mut Poll) {
    self.backends.add_backend(app_id, backend);
  }

  pub fn remove_backend(&mut self, app_id: &str, backend_address: &SocketAddr, event_loop: &mut Poll) {
    self.backends.remove_backend(app_id, backend_address);
  }


  pub fn backend_from_request(&mut self, client: &mut TlsClient, host: &str, uri: &str, front_should_stick: bool) -> Result<TcpStream,ConnectionError> {
    trace!("looking for backend for host: {}", host);
    let real_host = if let Some(h) = host.split(":").next() {
      h
    } else {
      host
    };
    trace!("looking for backend for real host: {}", real_host);

    if let Some(app_id) = self.listeners[&client.listen_token].frontend_from_request(real_host, uri).map(|ref front| front.app_id.clone()) {
      client.http().map(|h| h.set_app_id(app_id.clone()));

      match self.backends.backend_from_app_id(&app_id) {
        Err(e) => {
          let answer = self.listeners[&client.listen_token].answers.ServiceUnavailable.clone();
          client.set_answer(DefaultAnswerStatus::Answer503, answer);
          Err(e)
        },
        Ok((backend, conn))  => {
          client.back_connected = BackendConnectionStatus::Connecting;
          if front_should_stick {
            let sticky_name = self.listeners[&client.listen_token].config.sticky_name.clone();
            client.http().map(|http| {
              http.sticky_session =
                Some(StickySession::new(backend.borrow().sticky_id.clone().unwrap_or(backend.borrow().backend_id.clone())));
              http.sticky_name = sticky_name;
            });
          }
          client.metrics.backend_id = Some(backend.borrow().backend_id.clone());
          client.metrics.backend_start();
          client.backend = Some(backend);

          Ok(conn)
        }
      }
    } else {
      Err(ConnectionError::HostNotFound)
    }
  }

  pub fn backend_from_sticky_session(&mut self, client: &mut TlsClient, app_id: &str, sticky_session: String) -> Result<TcpStream,ConnectionError> {
    client.http().map(|h| h.set_app_id(String::from(app_id)));

    match self.backends.backend_from_sticky_session(app_id, &sticky_session) {
      Err(e) => {
        debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
        let answer = self.listeners[&client.listen_token].answers.ServiceUnavailable.clone();
        client.set_answer(DefaultAnswerStatus::Answer503, answer);
        Err(e)
      },
      Ok((backend, conn))  => {
        client.back_connected = BackendConnectionStatus::Connecting;
        let sticky_name = self.listeners[&client.listen_token].config.sticky_name.clone();
        client.http().map(|http| {
          http.sticky_session = Some(StickySession::new(backend.borrow().sticky_id.clone().unwrap_or(sticky_session.clone())));
          http.sticky_name = sticky_name;
        });
        client.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        client.metrics.backend_start();
        client.backend = Some(backend);

        Ok(conn)
      }
    }
  }
}

impl ProxyConfiguration<TlsClient> for ServerConfiguration {
  fn accept(&mut self, token: ListenToken, poll: &mut Poll, client_token: Token, timeout: Timeout)
    -> Result<(Rc<RefCell<TlsClient>>,bool), AcceptError> {

    self.listeners.get_mut(&Token(token.0)).unwrap().accept(token, poll, client_token, timeout)
  }

  fn accept_flush(&mut self) -> usize {
    unimplemented!()
    /*
    let mut counter = 0;
    if let Some(ref sock) = self.listener {
      while sock.accept().is_ok() {
        counter += 1;
      }
    }
    counter
      */
  }

  fn connect_to_backend(&mut self, poll: &mut Poll,  client: &mut TlsClient, back_token: Token) -> Result<BackendConnectAction,ConnectionError> {
    let h = try!(unwrap_msg!(client.http()).state().get_host().ok_or(ConnectionError::NoHostGiven));

    let host: &str = if let IResult::Done(i, (hostname, port)) = hostname_and_port(h.as_bytes()) {
      if i != &b""[..] {
        error!("connect_to_backend: invalid remaining chars after hostname. Host: {}", h);
        let answer = self.listeners[&client.listen_token].answers.BadRequest.clone();
        client.set_answer(DefaultAnswerStatus::Answer400, answer);
        return Err(ConnectionError::InvalidHost);
      }

      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
      let hostname_str =  unsafe { from_utf8_unchecked(hostname) };

      //FIXME: what if we don't use SNI?
      let servername: Option<String> = unwrap_msg!(client.http()).frontend.ssl().servername(NameType::HOST_NAME).map(|s| s.to_string());
      if servername.as_ref().map(|s| s.as_str()) != Some(hostname_str) {
        error!("TLS SNI hostname '{:?}' and Host header '{}' don't match", servername, hostname_str);
        let answer = self.listeners[&client.listen_token].answers.NotFound.clone();
        unwrap_msg!(client.http()).set_answer(DefaultAnswerStatus::Answer404, answer);
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
      let answer = self.listeners[&client.listen_token].answers.BadRequest.clone();
      client.set_answer(DefaultAnswerStatus::Answer400, answer);
      return Err(ConnectionError::InvalidHost);
    };

    let rl:RRequestLine = try!(unwrap_msg!(client.http()).state().get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
    if let Some(app_id) = self.listeners[&client.listen_token].frontend_from_request(&host, &rl.uri).map(|ref front| front.app_id.clone()) {

      let front_should_stick = self.applications.get(&app_id).map(|ref app| app.sticky_session).unwrap_or(false);

      if (client.http().map(|h| h.app_id.as_ref()).unwrap_or(None) == Some(&app_id)) && client.back_connected == BackendConnectionStatus::Connected {
        if client.backend.as_ref().map(|backend| {
           let ref backend = *backend.borrow();
           self.backends.has_backend(&app_id, backend)
        }).unwrap_or(false) {
          //matched on keepalive
          client.metrics.backend_id = client.backend.as_ref().map(|i| i.borrow().backend_id.clone());
          client.metrics.backend_start();
          return Ok(BackendConnectAction::Reuse);
        } else {
          client.backend = None;
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
        client.backend.as_ref().map(|backend| {
          let ref mut backend = *backend.borrow_mut();
          backend.dec_connections();
          backend.failures += 1;

          let already_unavailable = backend.retry_policy.is_down();
          backend.retry_policy.fail();
          incr!("backend.connections.error");
          if !already_unavailable && backend.retry_policy.is_down() {
            incr!("backend.down");
          }
        });

        client.backend = None;
        client.back_connected = BackendConnectionStatus::NotConnected;
        client.readiness().back_interest  = UnixReady::from(Ready::empty());
        client.readiness().back_readiness = UnixReady::from(Ready::empty());
        client.back_socket().as_ref().map(|sock| {
          poll.deregister(*sock);
          sock.shutdown(Shutdown::Both);
        });
      }

      let old_app_id = client.http().and_then(|ref http| http.app_id.clone());
      client.app_id = Some(app_id.clone());

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
            client.backend = None;
            client.back_connected = BackendConnectionStatus::NotConnected;
            client.readiness().back_readiness = UnixReady::from(Ready::empty());
            client.back_socket().as_ref().map(|sock| {
              poll.deregister(*sock);
              sock.shutdown(Shutdown::Both);
            });
          }

          // we still want to use the new socket
          client.readiness().back_interest  = UnixReady::from(Ready::writable());

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

          if client.back_token().is_some() {
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
          let answer = self.listeners[&client.listen_token].answers.ServiceUnavailable.clone();
          unwrap_msg!(client.http()).set_answer(DefaultAnswerStatus::Answer503, answer);
          Err(ConnectionError::NoBackendAvailable)
        },
        Err(ConnectionError::HostNotFound) => {
          let answer = self.listeners[&client.listen_token].answers.NotFound.clone();
          unwrap_msg!(client.http()).set_answer(DefaultAnswerStatus::Answer404, answer);
          Err(ConnectionError::HostNotFound)
        },
        e => panic!(e)
      }
    } else {
      let answer = self.listeners[&client.listen_token].answers.NotFound.clone();
      unwrap_msg!(client.http()).set_answer(DefaultAnswerStatus::Answer404, answer);
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
        self.remove_application(&application, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::AddHttpsFront(front) => {
        //info!("HTTPS\t{} add front {:?}", id, front);
        if let Some(mut listener) = self.listeners.values_mut().find(|l| l.address == front.address) {
          listener.add_https_front(front, event_loop);
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          panic!();
        }
      },
      Order::RemoveHttpsFront(front) => {
        //info!("HTTPS\t{} remove front {:?}", id, front);
        if let Some(mut listener) = self.listeners.values_mut().find(|l| l.address == front.address) {
          listener.remove_https_front(front, event_loop);
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          panic!();
        }
      },
      Order::AddCertificate(add_certificate) => {
        if let Some(mut listener) = self.listeners.values_mut().find(|l| l.address == add_certificate.front) {
          //info!("HTTPS\t{} add certificate: {:?}", id, certificate_and_key);
          listener.add_certificate(add_certificate.certificate, event_loop);
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          panic!();
        }
      },
      Order::RemoveCertificate(remove_certificate) => {
        if let Some(mut listener) = self.listeners.values_mut().find(|l| l.address == remove_certificate.front) {
          //info!("TLS\t{} remove certificate with fingerprint {:?}", id, fingerprint);
          listener.remove_certificate(remove_certificate.fingerprint, event_loop);
          //FIXME: should return an error if certificate still has fronts referencing it
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          panic!();
        }
      },
      Order::ReplaceCertificate(replace) => {
        if let Some(mut listener) = self.listeners.values_mut().find(|l| l.address == replace.front) {
          //info!("TLS\t{} replace certificate of fingerprint {:?} with {:?}", id,
          //  replace.old_fingerprint, replace.new_certificate);
          listener.remove_certificate(replace.old_fingerprint, event_loop);
          listener.add_certificate(replace.new_certificate, event_loop);
          //FIXME: should return an error if certificate still has fronts referencing it
          OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
        } else {
          panic!();
        }
      },
      Order::AddBackend(backend) => {
        debug!("{} add backend {:?}", message.id, backend);
        let new_backend = Backend::new(&backend.backend_id, backend.address.clone(), backend.sticky_id.clone(), backend.load_balancing_parameters, backend.backup);
        self.add_backend(&backend.app_id, new_backend, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveBackend(backend) => {
        debug!("{} remove backend {:?}", message.id, backend);
        self.remove_backend(&backend.app_id, &backend.address, event_loop);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::RemoveListener(remove) => {
        info!("removing https listener att address: {:?}", remove.front);
        fixme!();
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::SoftStop => {
        info!("{} processing soft shutdown", message.id);
        self.listeners.drain().map(|(token, mut l)| {
          l.listener.take().map(|sock| {
            event_loop.deregister(&sock);
          })
        });
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Processing, data: None }
      },
      Order::HardStop => {
        info!("{} hard shutdown", message.id);
        self.listeners.drain().map(|(token, mut l)| {
          l.listener.take().map(|sock| {
            event_loop.deregister(&sock);
          })
        });
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Processing, data: None }
      },
      Order::Status => {
        debug!("{} status", message.id);
        OrderMessageAnswer{ id: message.id, status: OrderMessageStatus::Ok, data: None }
      },
      Order::Logging(logging_filter) => {
        debug!("{} changing logging filter to {}", message.id, logging_filter);
        logging::LOGGER.with(|l| {
          let directives = logging::parse_logging_spec(&logging_filter);
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
    self.backends.close_backend_connection(&app_id, &addr);
  }

  fn listen_port_state(&self, port: &u16) -> ListenPortState {
    fixme!();
    ListenPortState::Available
    //if port == &self.address.port() { ListenPortState::InUse } else { ListenPortState::Available }
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

use network::proxy::HttpsProvider;
pub fn start(config: HttpsListener, channel: ProxyChannel, max_buffers: usize, buffer_size: usize) {
  use network::proxy::ProxyClientCast;

  let mut event_loop  = Poll::new().expect("could not create event loop");
  let max_listeners   = 1;

  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(2*max_buffers, 0, || BufferQueue::with_capacity(buffer_size))
  ));

  let mut clients: Slab<Rc<RefCell<ProxyClientCast>>,ClientToken> = Slab::with_capacity(max_buffers);
  {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    info!("taking token {:?} for channel", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    info!("taking token {:?} for timer", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    info!("taking token {:?} for metrics", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
  }

  let token = {
    let entry = clients.vacant_entry().expect("client list should have enough room at startup");
    let e = entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
    Token(e.index().0)
  };

  let mut configuration = ServerConfiguration::new(pool.clone());
  let front = config.front.clone();
  if configuration.add_listener(config, pool.clone(), token).is_some() {
    if configuration.activate_listener(&mut event_loop, &front, None).is_some() {

      let (scm_server, scm_client) = UnixStream::pair().unwrap();
      let mut server  = Server::new(event_loop, channel, ScmSocket::new(scm_server.as_raw_fd()),
        clients, pool, None, Some(HttpsProvider::Openssl(configuration)), None, None, max_buffers);

      info!("starting event loop");
      server.run();
      info!("ending event loop");
    }
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
  use network::pool::Pool;
  use sozu_command::buffer::Buffer;
  use network::buffer_queue::BufferQueue;
  use network::http::DefaultAnswers;
  use network::trie::TrieNode;
  use sozu_command::messages::{Order,HttpsFront,Backend,OrderMessage,OrderMessageAnswer};
  use openssl::ssl::{SslContext, SslMethod, Ssl, SslStream};
  use openssl::x509::X509;
  use mio::net;

  #[test]
  fn frontend_from_request_test() {
    let app_id1 = "app_1".to_owned();
    let app_id2 = "app_2".to_owned();
    let app_id3 = "app_3".to_owned();
    let uri1 = "/".to_owned();
    let uri2 = "/yolo".to_owned();
    let uri3 = "/yolo/swag".to_owned();

    let mut fronts = TrieNode::root();
    fronts.domain_insert(Vec::from(&b"lolcatho.st"[..]), vec![
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
    fronts.domain_insert(Vec::from(&b"other.domain"[..]), vec![
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

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1032").expect("test address 127.0.0.1:1032 should be parsed");
    let listener = Listener {
      listener:  None,
      address:   front,
      fronts:    fronts,
      domains:   rc_domains,
      default_context: context.build(),
      contexts: rc_ctx,
      pool:      Rc::new(RefCell::new(Pool::with_capacity(1, 0, || BufferQueue::with_capacity(16384)))),
      answers:   DefaultAnswers {
        NotFound: Rc::new(Vec::from(&b"HTTP/1.1 404 Not Found\r\n\r\n"[..])),
        ServiceUnavailable: Rc::new(Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\n\r\n"[..])),
        BadRequest: Rc::new(Vec::from(&b"HTTP/1.1 400 Bad Request\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..])),
      },
      config: Default::default(),
      ssl_options: ssl::SslOptions::CIPHER_SERVER_PREFERENCE | ssl::SslOptions::NO_COMPRESSION | ssl::SslOptions::NO_TICKET |
        ssl::SslOptions::NO_SSLV2 | ssl::SslOptions::NO_SSLV3 | ssl::SslOptions::NO_TLSV1 | ssl::SslOptions::NO_TLSV1_1,
      token: Token(0),
      active: true,
    };


    println!("TEST {}", line!());
    let frontend1 = listener.frontend_from_request("lolcatho.st", "/");
    assert_eq!(frontend1.expect("should find a frontend").app_id, "app_1");
    println!("TEST {}", line!());
    let frontend2 = listener.frontend_from_request("lolcatho.st", "/test");
    assert_eq!(frontend2.expect("should find a frontend").app_id, "app_1");
    println!("TEST {}", line!());
    let frontend3 = listener.frontend_from_request("lolcatho.st", "/yolo/test");
    assert_eq!(frontend3.expect("should find a frontend").app_id, "app_2");
    println!("TEST {}", line!());
    let frontend4 = listener.frontend_from_request("lolcatho.st", "/yolo/swag");
    assert_eq!(frontend4.expect("should find a frontend").app_id, "app_3");
    println!("TEST {}", line!());
    let frontend5 = listener.frontend_from_request("domain", "/");
    assert_eq!(frontend5, None);
   // assert!(false);
  }
}

#[cfg(not(ossl111))]
fn version_str(version: SslVersion) -> &'static str {
  match version {
    SslVersion::SSL3 => "tls.version.SSLv3",
    SslVersion::TLS1 => "tls.version.TLSv1_0",
    SslVersion::TLS1_1 => "tls.version.TLSv1_1",
    SslVersion::TLS1_2 => "tls.version.TLSv1_2",
    _ => "tls.version.Unknown",
  }
}

#[cfg(ossl111)]
fn version_str(version: SslVersion) -> &'static str {
  match version {
    SslVersion::SSL3 => "tls.version.SSLv3",
    SslVersion::TLS1 => "tls.version.TLSv1_0",
    SslVersion::TLS1_1 => "tls.version.TLSv1_1",
    SslVersion::TLS1_2 => "tls.version.TLSv1_2",
    SslVersion::TLS1_3 => "tls.version.TLSv1_3",
    _ => "tls.version.Unknown",
  }
}
