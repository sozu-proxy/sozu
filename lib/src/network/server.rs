#![allow(dead_code, unused_must_use, unused_variables, unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::TryRecvError;
use std::net::{SocketAddr,Shutdown};
use std::rc::Rc;
use std::cell::RefCell;
use mio::net::*;
use mio::*;
use mio::unix::UnixReady;
use std::collections::{HashSet,HashMap,VecDeque};
use std::io::{self,Read,ErrorKind};
use std::os::unix::io::{AsRawFd,FromRawFd,IntoRawFd};
use nom::HexDisplay;
use std::error::Error;
use slab::Slab;
use std::io::Write;
use std::str::FromStr;
use std::marker::PhantomData;
use std::fmt::Debug;
use time::{self, SteadyTime, precise_time_ns};
use std::time::Duration;
use rand::random;
use mio_extras::timer::{Timer, Timeout};

use sozu_command::config::Config;
use sozu_command::channel::Channel;
use sozu_command::scm_socket::{Listeners,ScmSocket};
use sozu_command::state::{ConfigState,get_application_ids_by_domain};
use sozu_command::proxy::{self,TcpFront,ProxyRequestData,Backend,MessageId,ProxyResponse,
  ProxyResponseData,ProxyResponseStatus,ProxyRequest,Topic,Query,QueryAnswer,
  QueryApplicationType,TlsProvider,ListenerType,HttpsListener};

use network::buffer_queue::BufferQueue;
use network::{SessionResult,ConnectionError,Protocol,RequiredEvents,ProxySession,
  CloseResult,AcceptError,BackendConnectAction,ProxyConfiguration};
use network::{http,tcp,AppId};
use network::pool::Pool;
use network::metrics::METRICS;

const SERVER: Token = Token(0);
const DEFAULT_FRONT_TIMEOUT: u64 = 50000;
const DEFAULT_BACK_TIMEOUT:  u64 = 50000;

// Number of retries to perform on a server after a connection failure
pub const CONN_RETRIES: u8 = 3;

pub type ProxyChannel = Channel<ProxyResponse,ProxyRequest>;

#[derive(Debug,Clone,PartialEq)]
enum ProxyType {
  HTTP,
  HTTPS,
  TCP,
}

#[derive(PartialEq)]
pub enum ListenPortState {
  Available,
  InUse
}

#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct ListenToken(pub usize);
#[derive(Copy,Clone,Debug,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct SessionToken(pub usize);

impl From<usize> for ListenToken {
    fn from(val: usize) -> ListenToken {
        ListenToken(val)
    }
}

impl From<ListenToken> for usize {
    fn from(val: ListenToken) -> usize {
        val.0
    }
}

impl From<usize> for SessionToken {
    fn from(val: usize) -> SessionToken {
        SessionToken(val)
    }
}

impl From<SessionToken> for usize {
    fn from(val: SessionToken) -> usize {
        val.0
    }
}

pub struct ServerConfig {
  pub max_connections:          usize,
  pub front_timeout:            u32,
  pub zombie_check_interval:    u32,
  pub accept_queue_timeout:     u32,
}

impl ServerConfig {
  pub fn from_config(config: &Config) -> ServerConfig {
    ServerConfig {
      max_connections: config.max_connections,
      front_timeout: config.front_timeout,
      zombie_check_interval: config.zombie_check_interval,
      accept_queue_timeout: config.accept_queue_timeout,
    }
  }
}

impl Default for ServerConfig {
  fn default() -> ServerConfig {
    ServerConfig {
      max_connections: 10000,
      front_timeout: 60,
      zombie_check_interval: 30*60,
      accept_queue_timeout: 60,
    }
  }
}

pub struct Server {
  pub poll:        Poll,
  shutting_down:   Option<MessageId>,
  accept_ready:    HashSet<ListenToken>,
  can_accept:      bool,
  channel:         ProxyChannel,
  queue:           VecDeque<ProxyResponse>,
  http:            http::Proxy,
  https:           HttpsProvider,
  tcp:             tcp::Proxy,
  config_state:    ConfigState,
  scm:             ScmSocket,
  sessions:        Slab<Rc<RefCell<ProxySessionCast>>,SessionToken>,
  max_connections: usize,
  nb_connections:  usize,
  front_timeout:   time::Duration,
  timer:           Timer<Token>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  scm_listeners:   Option<Listeners>,
  zombie_check_interval: time::Duration,
  accept_queue:    VecDeque<(TcpStream, ListenToken, Protocol, SteadyTime)>,
  accept_queue_timeout: time::Duration,
  base_sessions_count: usize,
}

impl Server {
  pub fn new_from_config(channel: ProxyChannel, scm: ScmSocket, config: Config, config_state: ConfigState) -> Self {
    let event_loop  = Poll::new().expect("could not create event loop");
    let pool = Rc::new(RefCell::new(
      Pool::with_capacity(2*config.max_buffers, 0, || BufferQueue::with_capacity(config.buffer_size))
    ));

    //FIXME: we will use a few entries for the channel, metrics socket and the listeners
    //FIXME: for HTTP/2, we will have more than 2 entries per session
    let mut sessions: Slab<Rc<RefCell<ProxySessionCast>>,SessionToken> = Slab::with_capacity(10+2*config.max_connections);
    {
      let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
      trace!("taking token {:?} for channel", entry.index());
      entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::Channel })));
    }
    {
      let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
      trace!("taking token {:?} for metrics", entry.index());
      entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::Timer })));
    }
    {
      let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
      trace!("taking token {:?} for metrics", entry.index());
      entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::Metrics })));
    }

    let use_openssl = config.tls_provider == TlsProvider::Openssl;
    let https = HttpsProvider::new(use_openssl, pool.clone());

    let server_config = ServerConfig::from_config(&config);
    Server::new(event_loop, channel, scm, sessions, pool, None, Some(https), None, server_config, Some(config_state))
  }

  pub fn new(poll: Poll, channel: ProxyChannel, scm: ScmSocket,
    sessions: Slab<Rc<RefCell<ProxySessionCast>>,SessionToken>,
    pool: Rc<RefCell<Pool<BufferQueue>>>,
    http: Option<http::Proxy>,
    https: Option<HttpsProvider>,
    tcp:  Option<tcp::Proxy>,
    server_config: ServerConfig,
    config_state: Option<ConfigState>) -> Self {

    poll.register(
      &channel,
      Token(0),
      Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
      PollOpt::edge()
    ).expect("should register the channel");

    let timer = Timer::default();
    poll.register(
      &timer,
      Token(1),
      Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
      PollOpt::edge()
    ).expect("should register the timer");

    METRICS.with(|metrics| {
      if let Some(sock) = (*metrics.borrow()).socket() {
        poll.register(sock, Token(2), Ready::writable(), PollOpt::edge()).expect("should register the metrics socket");
      }
    });

    let base_sessions_count = sessions.len();

    let mut server = Server {
      poll,
      shutting_down:   None,
      accept_ready:    HashSet::new(),
      can_accept:      true,
      channel,
      queue:           VecDeque::new(),
      http:            http.unwrap_or(http::Proxy::new(pool.clone())),
      https:           https.unwrap_or(HttpsProvider::new(false, pool.clone())),
      tcp:             tcp.unwrap_or(tcp::Proxy::new()),
      config_state:    ConfigState::new(),
      scm,
      sessions,
      max_connections: server_config.max_connections,
      nb_connections:  0,
      scm_listeners:   None,
      timer,
      pool,
      front_timeout: time::Duration::seconds(i64::from(server_config.front_timeout)),
      zombie_check_interval: time::Duration::seconds(i64::from(server_config.zombie_check_interval)),
      accept_queue:    VecDeque::new(),
      accept_queue_timeout: time::Duration::seconds(i64::from(server_config.accept_queue_timeout)),
      base_sessions_count,
    };

    // initialize the worker with the state we got from a file
    if let Some(state) = config_state {
      let mut counter = 0usize;

      for order in state.generate_orders() {
        let id = format!("INIT-{}", counter);
        let message = ProxyRequest {
          id:    id,
          order: order,
        };

        trace!("generating initial config order: {:#?}", message);
        server.notify_proxys(message);

        counter += 1;
      }
      // do not send back answers to the initialization messages
      server.queue.clear();
    }

    info!("will try to receive listeners");
    server.scm.set_blocking(true);
    let listeners = server.scm.receive_listeners();
    server.scm.set_blocking(false);
    info!("received listeners: {:?}", listeners);
    server.scm_listeners = listeners;

    server
  }
}

impl Server {
  pub fn run(&mut self) {
    //FIXME: make those parameters configurable?
    let mut events = Events::with_capacity(1024);
    let poll_timeout = Some(Duration::from_millis(1000));
    let max_poll_errors = 10000;
    let mut current_poll_errors = 0;
    let mut last_zombie_check = SteadyTime::now();
    let mut last_sessions_len = self.sessions.len();

    loop {
      if current_poll_errors == max_poll_errors {
        error!("Something is going very wrong. Last {} poll() calls failed, crashing..", current_poll_errors);
        panic!("poll() calls failed {} times in a row", current_poll_errors);
      }

      if let Err(error) = self.poll.poll(&mut events, poll_timeout) {
        error!("Error while polling events: {:?}", error);
        current_poll_errors += 1;
        continue;
      } else {
        current_poll_errors = 0;
      }

      for event in events.iter() {
        if event.token() == Token(0) {
          let kind = event.readiness();
          if UnixReady::from(kind).is_error() {
            error!("error reading from command channel");
            continue;
          }
          if UnixReady::from(kind).is_hup() {
            error!("command channel was closed");
            continue;
          }
          self.channel.handle_events(kind);

          // loop here because iterations has borrow issues
          loop {
            if !self.queue.is_empty() {
              self.channel.interest.insert(Ready::writable());
            }

            //trace!("WORKER[{}] channel readiness={:?}, interest={:?}, queue={} elements",
            //  line!(), self.channel.readiness, self.channel.interest, self.queue.len());
            if self.channel.readiness() == Ready::empty() {
              break;
            }

            if self.channel.readiness().is_readable() {
              self.channel.readable();

              loop {
                let msg = self.channel.read_message();

                // if the message was too large, we grow the buffer and retry to read if possible
                if msg.is_none() {
                  if (self.channel.interest & self.channel.readiness).is_readable() {
                    self.channel.readable();
                    continue;
                  } else {
                    break;
                  }
                }

                let msg = msg.expect("the message should be valid");
                if let ProxyRequestData::HardStop = msg.order {
                  let id_msg = msg.id.clone();
                  self.notify(msg);
                  self.channel.write_message(&ProxyResponse{ id: id_msg, status: ProxyResponseStatus::Ok, data: None});
                  self.channel.run();
                  return;
                } else if let ProxyRequestData::SoftStop = msg.order {
                  self.shutting_down = Some(msg.id.clone());
                  last_sessions_len = self.sessions.len();
                  self.notify(msg);
                } else if let ProxyRequestData::ReturnListenSockets = msg.order {
                  info!("received ReturnListenSockets order");
                  self.return_listen_sockets();
                } else {
                  self.notify(msg);
                }

              }
            }

            if !self.queue.is_empty() {
              self.channel.interest.insert(Ready::writable());
            }
            if self.channel.readiness.is_writable() {

              loop {

                if let Some(msg) = self.queue.pop_front() {
                  if !self.channel.write_message(&msg) {
                    self.queue.push_front(msg);
                  }
                }

                if self.channel.back_buf.available_data() > 0 {
                  self.channel.writable();
                }

                if !self.channel.readiness.is_writable() {
                  break;
                }

                if self.channel.back_buf.available_data() == 0 && self.queue.len() == 0 {
                  break;
                }
              }
            }
          }

        } else if event.token() == Token(1) {
          while let Some(t) = self.timer.poll() {
            self.timeout(t);
          }
        } else if event.token() == Token(2) {
          METRICS.with(|metrics| {
            (*metrics.borrow_mut()).writable();
          });
        } else {
          self.ready(event.token(), event.readiness());
        }
      }

      self.handle_remaining_readiness();
      self.create_sessions();

      let now = SteadyTime::now();
      if now - last_zombie_check > self.zombie_check_interval {
        info!("zombie check");
        last_zombie_check = now;

        let mut tokens = HashSet::new();
        let mut frontend_tokens = HashSet::new();

        let mut count = 0;
        let duration = self.zombie_check_interval.clone();
        for session in self.sessions.iter_mut().filter(|c| {
          now - c.borrow().last_event() > duration
        }) {
          let t = session.borrow().tokens();
          if !frontend_tokens.contains(&t[0]) {
            session.borrow().print_state();

            frontend_tokens.insert(t[0]);
            for tk in t.into_iter() {
              tokens.insert(tk);
            }

            count += 1;
          }
        }

        for tk in frontend_tokens.iter() {
          let cl = self.to_session(*tk);
          self.close_session(cl);
        }

        if count > 0 {
          count!("zombies", count);

          let mut remaining = 0;
          for tk in tokens.into_iter() {
            let cl = self.to_session(tk);
            if self.sessions.remove(cl).is_some() {
              remaining += 1;
            }
          }
          info!("removing {} zombies ({} remaining tokens after close)", count, remaining);
        }
      }

      gauge!("client.connections", self.nb_connections);
      gauge!("slab.count", self.sessions.len());
      METRICS.with(|metrics| {
        (*metrics.borrow_mut()).send_data();
      });

      if self.shutting_down.is_some() {
        let count = self.sessions.len();
        if count <= self.base_sessions_count {
          info!("last session stopped, shutting down!");
          self.channel.run();
          self.channel.set_blocking(true);
          self.channel.write_message(&ProxyResponse{ id: self.shutting_down.take().expect("should have shut down correctly"), status: ProxyResponseStatus::Ok, data: None});
          return;
        } else if count < last_sessions_len {
          info!("shutting down, {} slab elements remaining (base: {})",
            count - self.base_sessions_count, self.base_sessions_count);
          last_sessions_len = count;
        }
      }
    }
  }

  fn notify(&mut self, message: ProxyRequest) {
    if let ProxyRequestData::Metrics = message.order {
      let q = &mut self.queue;
      //let id = message.id.clone();
      let msg = METRICS.with(|metrics| {
        q.push_back(ProxyResponse {
          id:     message.id.clone(),
          status: ProxyResponseStatus::Ok,
          data:   Some(ProxyResponseData::Metrics(
            (*metrics.borrow_mut()).dump_metrics_data()
          ))
        });
      });
      return;
    }

    if let ProxyRequestData::Query(ref query) = message.order {
      match query {
        &Query::ApplicationsHashes => {
          self.queue.push_back(ProxyResponse {
            id:     message.id.clone(),
            status: ProxyResponseStatus::Ok,
            data:   Some(ProxyResponseData::Query(
              QueryAnswer::ApplicationsHashes(self.config_state.hash_state())
            ))
          });
        },
        &Query::Applications(ref query_type) => {
          let answer = match query_type {
            &QueryApplicationType::AppId(ref app_id) => {
              QueryAnswer::Applications(vec!(self.config_state.application_state(app_id)))
            },
            &QueryApplicationType::Domain(ref domain) => {
              let app_ids = get_application_ids_by_domain(&self.config_state, domain.hostname.clone(), domain.path_begin.clone());
              let answer = app_ids.iter().map(|ref app_id| self.config_state.application_state(app_id)).collect();

              QueryAnswer::Applications(answer)
            }
          };

          self.queue.push_back(ProxyResponse {
            id:     message.id.clone(),
            status: ProxyResponseStatus::Ok,
            data:   Some(ProxyResponseData::Query(answer))
          });
        }
      }
      return
    }

    self.notify_proxys(message);
  }

  pub fn notify_proxys(&mut self, message: ProxyRequest) {
    self.config_state.handle_order(&message.order);

    let topics = message.order.get_topics();

    if topics.contains(&Topic::HttpProxyConfig) {
      match message {
        // special case for AddHttpListener because we need to register a listener
        ProxyRequest { ref id, order: ProxyRequestData::AddHttpListener(ref listener) } => {
          debug!("{} add http listener {:?}", id, listener);
          /*FIXME
          if self.listen_port_state(&tcp_front.port) == ListenPortState::InUse {
            error!("Couldn't add TCP front {:?}: port already in use", tcp_front);
            self.queue.push_back(ProxyResponse {
              id,
              status: ProxyResponseStatus::Error(String::from("Couldn't add TCP front: port already in use")),
              data: None
            });
            return;
          }*/

          let entry = self.sessions.vacant_entry();

          if entry.is_none() {
            self.queue.push_back(ProxyResponse {
              id: id.to_string(),
              status: ProxyResponseStatus::Error(String::from("session list is full, cannot add a listener")),
              data: None
            });
            return;
          }

          let entry = entry.unwrap();

          let token = Token(entry.index().0);

          let front = listener.front.clone();
          let status = if let Some(token) = self.http.add_listener(listener.clone(), self.pool.clone(), token) {
            entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
            self.base_sessions_count += 1;
            ProxyResponseStatus::Ok
          } else {
            error!("Couldn't add HTTP listener");
            ProxyResponseStatus::Error(String::from("cannot add HTTP listener"))
          };

          let answer = ProxyResponse { id: id.to_string(), status, data: None };
          self.queue.push_back(answer);
        },
        ProxyRequest { ref id, order: ProxyRequestData::RemoveListener(ref remove) } => {
          if remove.proxy == ListenerType::HTTP {
            debug!("{} remove http listener {:?}", id, remove);
            self.base_sessions_count -= 1;
            self.queue.push_back(self.http.notify(&mut self.poll, ProxyRequest {
              id: id.to_string(),
              order: ProxyRequestData::RemoveListener(remove.clone())
            }));
          }
        },
        ProxyRequest { ref id, order: ProxyRequestData::ActivateListener(ref activate) } => {
          if activate.proxy == ListenerType::HTTP {
            debug!("{} activate http listener {:?}", id, activate);
            let listener = self.scm_listeners.as_mut().and_then(|s| s.get_http(&activate.front))
              .map(|fd| unsafe { TcpListener::from_raw_fd(fd) });
            let status = match self.http.activate_listener(&mut self.poll, &activate.front, listener) {
              Some(token) => {
                self.accept(ListenToken(token.0), Protocol::HTTPListen);
                ProxyResponseStatus::Ok
              },
              None => {
                error!("Couldn't activate HTTP listener");
                ProxyResponseStatus::Error(String::from("cannot activate HTTP listener"))
              }
            };

            let answer = ProxyResponse { id: id.to_string(), status, data: None };
            self.queue.push_back(answer);
          }
        },
        ref m => self.queue.push_back(self.http.notify(&mut self.poll, m.clone())),
      }
    }
    if topics.contains(&Topic::HttpsProxyConfig) {
      match message {
        // special case for AddHttpListener because we need to register a listener
        ProxyRequest { ref id, order: ProxyRequestData::AddHttpsListener(ref listener) } => {
          debug!("{} add https listener {:?}", id, listener);
          /*FIXME
          if self.listen_port_state(&tcp_front.port) == ListenPortState::InUse {
            error!("Couldn't add TCP front {:?}: port already in use", tcp_front);
            self.queue.push_back(ProxyResponse {
              id,
              status: ProxyResponseStatus::Error(String::from("Couldn't add TCP front: port already in use")),
              data: None
            });
            return;
          }*/

          let entry = self.sessions.vacant_entry();

          if entry.is_none() {
            self.queue.push_back(ProxyResponse {
              id: id.to_string(),
              status: ProxyResponseStatus::Error(String::from("session list is full, cannot add a listener")),
              data: None
            });
            return;
          }

          let entry = entry.unwrap();

          let token = Token(entry.index().0);

          let front = listener.front.clone();
          let status = if let Some(token) = self.https.add_listener(listener.clone(), self.pool.clone(), token) {
            entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPSListen })));
            self.base_sessions_count += 1;
            ProxyResponseStatus::Ok
          } else {
            error!("Couldn't add HTTPS listener");
            ProxyResponseStatus::Error(String::from("cannot add HTTPS listener"))
          };

          let answer = ProxyResponse { id: id.to_string(), status, data: None };
          self.queue.push_back(answer);
        },
        ProxyRequest { ref id, order: ProxyRequestData::RemoveListener(ref remove) } => {
          if remove.proxy == ListenerType::HTTPS {
            debug!("{} remove https listener {:?}", id, remove);
            self.base_sessions_count -= 1;
            self.queue.push_back(self.https.notify(&mut self.poll, ProxyRequest {
              id: id.to_string(),
              order: ProxyRequestData::RemoveListener(remove.clone())
            }));
          }
        },
        ProxyRequest { ref id, order: ProxyRequestData::ActivateListener(ref activate) } => {
          if activate.proxy == ListenerType::HTTPS {
            debug!("{} activate https listener {:?}", id, activate);
            let listener = self.scm_listeners.as_mut().and_then(|s| s.get_https(&activate.front))
              .map(|fd| unsafe { TcpListener::from_raw_fd(fd) });
            let status = match self.https.activate_listener(&mut self.poll, &activate.front, listener) {
              Some(token) => {
                self.accept(ListenToken(token.0), Protocol::HTTPSListen);
                ProxyResponseStatus::Ok
              },
              None => {
                error!("Couldn't activate HTTPS listener");
                ProxyResponseStatus::Error(String::from("cannot activate HTTPS listener"))
              }
            };

            let answer = ProxyResponse { id: id.to_string(), status, data: None };
            self.queue.push_back(answer);
          }
        },
        ref m => self.queue.push_back(self.https.notify(&mut self.poll, m.clone())),
      }
    }
    if topics.contains(&Topic::TcpProxyConfig) {
      match message {
        // special case for AddTcpFront because we need to register a listener
        ProxyRequest { id, order: ProxyRequestData::AddTcpListener(listener) } => {
          debug!("{} add tcp listener {:?}", id, listener);
          /*if self.listen_port_state(&tcp_front.port) == ListenPortState::InUse {
            error!("Couldn't add TCP front {:?}: port already in use", tcp_front);
            self.queue.push_back(ProxyResponse {
              id,
              status: ProxyResponseStatus::Error(String::from("Couldn't add TCP front: port already in use")),
              data: None
            });
            return;
          }
          */

          let entry = self.sessions.vacant_entry();

          if entry.is_none() {
            self.queue.push_back(ProxyResponse {
              id,
              status: ProxyResponseStatus::Error(String::from("session list is full, cannot add a listener")),
              data: None
            });
            return;
          }

          let entry = entry.unwrap();

          let token = Token(entry.index().0);

          let front = listener.front.clone();
          let status = if let Some(token) = self.tcp.add_listener(listener.clone(), self.pool.clone(), token) {
            entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::TCPListen })));
            self.base_sessions_count += 1;
            ProxyResponseStatus::Ok
          } else {
            error!("Couldn't add TCP listener");
            ProxyResponseStatus::Error(String::from("cannot add TCP listener"))
          };
          let answer = ProxyResponse { id, status, data: None };
          self.queue.push_back(answer);
        },
        ProxyRequest { ref id, order: ProxyRequestData::RemoveListener(ref remove) } => {
          if remove.proxy == ListenerType::TCP {
            debug!("{} remove tcp listener {:?}", id, remove);
            self.base_sessions_count -= 1;
            self.queue.push_back(self.tcp.notify(&mut self.poll, ProxyRequest {
              id: id.to_string(),
              order: ProxyRequestData::RemoveListener(remove.clone())
            }));
          }
        },
        ProxyRequest { ref id, order: ProxyRequestData::ActivateListener(ref activate) } => {
          if activate.proxy == ListenerType::TCP {
            debug!("{} activate tcp listener {:?}", id, activate);
            let listener = self.scm_listeners.as_mut().and_then(|s| s.get_tcp(&activate.front))
              .map(|fd| unsafe { TcpListener::from_raw_fd(fd) });
            let status = match self.tcp.activate_listener(&mut self.poll, &activate.front, listener) {
              Some(token) => {
                self.accept(ListenToken(token.0), Protocol::TCPListen);
                ProxyResponseStatus::Ok
              },
              None => {
                error!("Couldn't activate TCP listener");
                ProxyResponseStatus::Error(String::from("cannot activate TCP listener"))
              }
            };

            let answer = ProxyResponse { id: id.to_string(), status, data: None };
            self.queue.push_back(answer);
          }
        },
        m => self.queue.push_back(self.tcp.notify(&mut self.poll, m)),
      }
    }
  }

  pub fn return_listen_sockets(&mut self) {
    self.scm.set_blocking(false);

    let http_listeners = self.http.give_back_listeners();
    for &(_, ref sock) in http_listeners.iter() {
      self.poll.deregister(sock);
    }

    let https_listeners = self.https.give_back_listeners();
    for &(_, ref sock) in https_listeners.iter() {
      self.poll.deregister(sock);
    }

    let tcp_listeners = self.tcp.give_back_listeners();
    for &(_, ref sock) in tcp_listeners.iter() {
      self.poll.deregister(sock);
    }

    let listeners = Listeners {
      http: http_listeners.into_iter().map(|(addr, listener)|  (addr, listener.into_raw_fd())).collect(),
      tls:  https_listeners.into_iter().map(|(addr, listener)| (addr, listener.into_raw_fd())).collect(),
      tcp:  tcp_listeners.into_iter().map(|(addr, listener)|   (addr, listener.into_raw_fd())).collect(),
    };
    info!("sending default listeners: {:?}", listeners);
    let res = self.scm.send_listeners(listeners);

    self.scm.set_blocking(true);

    info!("sent default listeners: {:?}", res);
  }

  pub fn to_session(&self, token: Token) -> SessionToken {
    SessionToken(token.0)
  }

  pub fn from_session(&self, token: SessionToken) -> Token {
    Token(token.0)
  }

  pub fn close_session(&mut self, token: SessionToken) {
    if self.sessions.contains(token) {
      let session = self.sessions.remove(token).expect("session shoud be there");
      session.borrow().cancel_timeouts(&mut self.timer);
      let CloseResult { tokens } = session.borrow_mut().close(&mut self.poll);

      for tk in tokens.into_iter() {
        let cl = self.to_session(tk);
        self.sessions.remove(cl);
      }

      assert!(self.nb_connections != 0);
      self.nb_connections -= 1;
      gauge!("client.connections", self.nb_connections);
    }

    // do not be ready to accept right away, wait until we get back to 10% capacity
    if !self.can_accept && self.nb_connections < self.max_connections * 90 / 100 {
      debug!("nb_connections = {}, max_connections = {}, starting to accept again", self.nb_connections, self.max_connections);
      self.can_accept = true;
    }
  }

  pub fn create_session_tcp(&mut self, token: ListenToken, socket: TcpStream) -> bool {
    if self.nb_connections == self.max_connections {
      error!("max number of session connection reached, flushing the accept queue");
      self.can_accept = false;
      return false;
    }

    //FIXME: we must handle separately the session limit since the sessions slab also has entries for listeners and backends
    let index = match self.sessions.vacant_entry() {
      None => {
        error!("not enough memory to accept another session, flushing the accept queue");
        error!("nb_connections: {}, max_connections: {}", self.nb_connections, self.max_connections);
        self.can_accept = false;

        return false;
      },
      Some(entry) => {
        let session_token = Token(entry.index().0);
        let index = entry.index();
        let timeout = self.timer.set_timeout(self.front_timeout.to_std().unwrap(), session_token);
        match self.tcp.create_session(socket, token, &mut self.poll, session_token, timeout) {
          Ok((session, should_connect)) => {
            entry.insert(session);
            self.nb_connections += 1;
            assert!(self.nb_connections <= self.max_connections);
            gauge!("client.connections", self.nb_connections);

            if should_connect {
              index
            } else {
              return true;
            }
          },
          Err(AcceptError::IoError) => {
            //FIXME: do we stop accepting?
            return false;
          },
          Err(AcceptError::WouldBlock) => {
            self.accept_ready.remove(&token);
            return false;
          },
          Err(AcceptError::TooManySessions) => {
            error!("max number of session connection reached, flushing the accept queue");
            self.can_accept = false;
            return false;
          }
        }
      }
    };

    self.connect_to_backend(index);
    true
  }

  pub fn create_session_http(&mut self, token: ListenToken, socket: TcpStream) -> bool {
    if self.nb_connections == self.max_connections {
      error!("max number of session connection reached, flushing the accept queue");
      self.can_accept = false;
      return false;
    }

    //FIXME: we must handle separately the session limit since the sessions slab also has entries for listeners and backends
    match self.sessions.vacant_entry() {
      None => {
        error!("not enough memory to accept another session, flushing the accept queue");
        error!("nb_connections: {}, max_connections: {}", self.nb_connections, self.max_connections);
        self.can_accept = false;
        return false;
      },
      Some(entry) => {
        let session_token = Token(entry.index().0);
        let index = entry.index();
        let timeout = self.timer.set_timeout(self.front_timeout.to_std().unwrap(), session_token);
        match self.http.create_session(socket, token, &mut self.poll, session_token, timeout) {
          Ok((session, should_connect)) => {
            entry.insert(session);
            self.nb_connections += 1;
            assert!(self.nb_connections <= self.max_connections);
            gauge!("client.connections", self.nb_connections);
            true
          },
          Err(AcceptError::IoError) => {
            //FIXME: do we stop accepting?
            false
          },
          Err(AcceptError::WouldBlock) => {
            self.accept_ready.remove(&token);
            false
          },
          Err(AcceptError::TooManySessions) => {
            error!("max number of session connection reached, flushing the accept queue");
            self.can_accept = false;
            false
          }
        }
      }
    }
  }

  pub fn create_session_https(&mut self, token: ListenToken, socket: TcpStream) -> bool {
    if self.nb_connections == self.max_connections {
      error!("max number of session connection reached, flushing the accept queue");
      self.can_accept = false;
      return false;
    }

    //FIXME: we must handle separately the session limit since the sessions slab also has entries for listeners and backends
    match self.sessions.vacant_entry() {
      None => {
        error!("not enough memory to accept another session, flushing the accept queue");
        error!("nb_connections: {}, max_connections: {}", self.nb_connections, self.max_connections);
        self.can_accept = false;
        false
      },
      Some(entry) => {
        let session_token = Token(entry.index().0);
        let index = entry.index();
        let timeout = self.timer.set_timeout(self.front_timeout.to_std().unwrap(), session_token);
        match self.https.create_session(socket, token, &mut self.poll, session_token, timeout) {
          Ok((session, should_connect)) => {
            entry.insert(session);
            self.nb_connections += 1;
            assert!(self.nb_connections <= self.max_connections);
            gauge!("client.connections", self.nb_connections);
            true
          },
          Err(AcceptError::IoError) => {
            //FIXME: do we stop accepting?
            false
          },
          Err(AcceptError::WouldBlock) => {
            self.accept_ready.remove(&token);
            false
          },
          Err(AcceptError::TooManySessions) => {
            error!("max number of session connection reached, flushing the accept queue");
            self.can_accept = false;
            false
          }
        }
      }
    }
  }

  pub fn accept(&mut self, token: ListenToken, protocol: Protocol) {
    match protocol {
      Protocol::TCPListen   => {
        loop {
          match self.tcp.accept(token) {
            Ok(sock) => self.accept_queue.push_back((sock, token, Protocol::TCPListen, SteadyTime::now())),
            Err(AcceptError::WouldBlock) => {
              self.accept_ready.remove(&token);
              break
            },
            Err(other) => {
              error!("error accepting TCP sockets: {:?}", other);
              self.accept_ready.remove(&token);
              break;
            }
          }
        }
      },
      Protocol::HTTPListen  => {
        loop {
          match self.http.accept(token) {
            Ok(sock) => self.accept_queue.push_back((sock, token, Protocol::HTTPListen, SteadyTime::now())),
            Err(AcceptError::WouldBlock) => {
              self.accept_ready.remove(&token);
              break
            },
            Err(other) => {
              error!("error accepting HTTP sockets: {:?}", other);
              self.accept_ready.remove(&token);
              break;
            }
          }
        }
      },
      Protocol::HTTPSListen => {
        loop {
          match self.https.accept(token) {
            Ok(sock) => self.accept_queue.push_back((sock, token, Protocol::HTTPSListen, SteadyTime::now())),
            Err(AcceptError::WouldBlock) => {
              self.accept_ready.remove(&token);
              break
            },
            Err(other) => {
              error!("error accepting HTTPS sockets: {:?}", other);
              self.accept_ready.remove(&token);
              break;
            }
          }
        }
      },
      _ => panic!("should not call accept() on a HTTP, HTTPS or TCP session"),
    }

    gauge!("accept_queue.count", self.accept_queue.len());
  }

  pub fn create_sessions(&mut self) {
    loop {
      if let Some((sock, token, protocol, timestamp)) = self.accept_queue.pop_back() {
        let delay = SteadyTime::now() - timestamp;
        time!("accept_queue.wait_time", delay.num_milliseconds());
        if delay > self.accept_queue_timeout {
          incr!("accept_queue.timeout");
          continue;
        }
        //FIXME: check the timestamp
        match protocol {
          Protocol::TCPListen   => {
            if !self.create_session_tcp(token, sock) {
              break;
            }
          },
          Protocol::HTTPListen  => {
            if !self.create_session_http(token, sock) {
              break;
            }
          },
          Protocol::HTTPSListen => {
            if !self.create_session_https(token, sock) {
              break;
            }
          },
          _ => panic!("should not call accept() on a HTTP, HTTPS or TCP session"),
        }
      } else {
        break;
      }
    }

    gauge!("accept_queue.count", self.accept_queue.len());
  }

  pub fn connect_to_backend(&mut self, token: SessionToken) {
    if ! self.sessions.contains(token) {
      error!("invalid token in connect_to_backend");
      return;
    }

    let (protocol, res) = {
      let cl = self.sessions[token].clone();
      let cl2: Rc<RefCell<ProxySessionCast>> = self.sessions[token].clone();
      let protocol = { cl.borrow().protocol() };
      let entry = self.sessions.vacant_entry();
      if entry.is_none() {
        error!("not enough memory, cannot connect to backend");
        return;
      }
      let entry = entry.unwrap();
      let entry = entry.insert(cl);
      let back_token = Token(entry.index().0);

      let (protocol, res) = match protocol {
        Protocol::TCP   => {
          let mut b = cl2.borrow_mut();
          let session: &mut tcp::Session = b.as_tcp() ;
          let r = self.tcp.connect_to_backend(&mut self.poll, session, back_token);

          (Protocol::TCP, r)
        },
        Protocol::HTTP  => {
          (Protocol::HTTP, {
            let mut b = cl2.borrow_mut();
            let session: &mut http::Session = b.as_http() ;
            let r = self.http.connect_to_backend(&mut self.poll, session, back_token);
            r
          })
        },
        Protocol::HTTPS => {
          (Protocol::HTTPS, {
            let r = self.https.connect_to_backend(&mut self.poll, cl2, back_token);
            r
          })
        },
        _ => {
          panic!("should not call connect_to_backend on listeners");
        },
      };

      if res != Ok(BackendConnectAction::New) {
        entry.remove();
      }
      (protocol, res)
    };

    match res {
      Ok(BackendConnectAction::Reuse) => {
        debug!("keepalive, reusing backend connection");
      }
      Ok(BackendConnectAction::Replace) => {
      },
      Ok(BackendConnectAction::New) => {
      },
      Err(ConnectionError::HostNotFound) | Err(ConnectionError::NoBackendAvailable) |
        Err(ConnectionError::HttpsRedirect) | Err(ConnectionError::InvalidHost) => {
        if protocol == Protocol::TCP {
          self.close_session(token);
        }
      },
      _ => self.close_session(token),
    }
  }

  pub fn interpret_session_order(&mut self, token: SessionToken, order: SessionResult) {
    //trace!("INTERPRET ORDER: {:?}", order);
    match order {
      SessionResult::CloseSession     => self.close_session(token),
      SessionResult::CloseBackend(opt) => {
        if let Some(token) = opt {
          let cl = self.to_session(token);
          if let Some(session) = self.sessions.remove(cl) {
            session.borrow_mut().close_backend(token, &mut self.poll);
          }
        }
      },
      SessionResult::ReconnectBackend(main_token, backend_token)  => {
        if let Some(t) = backend_token {
          let cl = self.to_session(t);
          if let Some(session) = self.sessions.remove(cl) {
            session.borrow_mut().close_backend(t, &mut self.poll);
          }
        }

        if let Some(t) = main_token {
          let tok = self.to_session(t);
          self.connect_to_backend(tok)
        }
      },
      SessionResult::ConnectBackend  => self.connect_to_backend(token),
      SessionResult::Continue        => {}
    }
  }

  pub fn ready(&mut self, token: Token, events: Ready) {
    trace!("PROXY\t{:?} got events: {:?}", token, events);

    let mut session_token = SessionToken(token.0);
    if self.sessions.contains(session_token) {
      //info!("sessions contains {:?}", session_token);
      let protocol = self.sessions[session_token].borrow().protocol();
      //info!("protocol: {:?}", protocol);
      match protocol {
        Protocol::HTTPListen | Protocol::HTTPSListen | Protocol::TCPListen => {
          //info!("PROTOCOL IS LISTEN");
          if events.is_readable() {
            self.accept_ready.insert(ListenToken(token.0));
            if self.can_accept {
              self.accept(ListenToken(token.0), protocol);
            }
            return;
          }

          if events.is_writable() {
            error!("received writable for listener {:?}, this should not happen", token);
            return;
          }

          if UnixReady::from(events).is_hup() {
            error!("should not happen: server {:?} closed", token);
            return;
          }

          unreachable!();

        },
        _ => {}
      }

      self.sessions[session_token].borrow_mut().process_events(token, events);

      loop {
        //self.session_ready(poll, session_token, events);
        if !self.sessions.contains(session_token) {
          break;
        }

        let order = self.sessions[session_token].borrow_mut().ready();
        trace!("session[{:?} -> {:?}] got events {:?} and returned order {:?}", session_token, self.from_session(session_token), events, order);
        //FIXME: the CloseBackend message might not mean we have nothing else to do
        //with that session
        let is_connect = match order {
          SessionResult::ConnectBackend | SessionResult::ReconnectBackend(_,_) => true,
          _ => false,
        };

        // if we got ReconnectBackend, that means the current session_token
        // corresponds to an entry that will be removed in interpret_session_order
        // so we ask for the "main" token, ie the one for the front socket
        if let SessionResult::ReconnectBackend(Some(t), _) = order {
          session_token = self.to_session(t);
        }

        self.interpret_session_order(session_token, order);

        // if we had to connect to a backend server, go back to the loop
        // I'm not sure we would have anything to do right away, though,
        // so maybe we can just stop there for that session?
        // also the events would change?
        if !is_connect {
          break;
        }
      }
    }
  }

  pub fn timeout(&mut self, token: Token) {
    trace!("PROXY\t{:?} got timeout", token);

    let session_token = SessionToken(token.0);
    if self.sessions.contains(session_token) {
      let order = self.sessions[session_token].borrow_mut().timeout(token, &mut self.timer, &self.front_timeout);
      self.interpret_session_order(session_token, order);
    }
  }

  pub fn handle_remaining_readiness(&mut self) {
    // try to accept again after handling all session events,
    // since we might have released a few session slots
    if self.can_accept && !self.accept_ready.is_empty() {
      loop {
        if let Some(token) = self.accept_ready.iter().next().map(|token| ListenToken(token.0)) {
          let protocol = self.sessions[SessionToken(token.0)].borrow().protocol();
          self.accept(token, protocol);
          if !self.can_accept || self.accept_ready.is_empty() {
            break;
          }
        } else {
          // we don't have any more elements to loop over
          break;
        }
      }
    }
  }

  fn listen_port_state(&self, port: &u16) -> ListenPortState {
    let http_bound = self.http.listen_port_state(&port);

    if http_bound == ListenPortState::Available {
      let https_bound = match self.https {
        #[cfg(feature = "use-openssl")]
        HttpsProvider::Openssl(ref openssl) => openssl.listen_port_state(&port),
        HttpsProvider::Rustls(ref rustls) => rustls.listen_port_state(&port),
      };

      if https_bound == ListenPortState::Available {
        return self.tcp.listen_port_state(&port);
      }
    }

    ListenPortState::InUse
  }
}

pub struct ListenSession {
  pub protocol: Protocol,
}

impl ProxySession for ListenSession {
  fn last_event(&self) -> SteadyTime {
    SteadyTime::now()
  }

  fn print_state(&self) {}

  fn tokens(&self) -> Vec<Token> {
    Vec::new()
  }

  fn protocol(&self) -> Protocol {
    self.protocol
  }

  fn ready(&mut self) -> SessionResult {
    SessionResult::Continue
  }

  fn process_events(&mut self, token: Token, events: Ready) {}

  fn close(&mut self, poll: &mut Poll) -> CloseResult {
    CloseResult::default()
  }

  fn close_backend(&mut self, token: Token, poll: &mut Poll) {
  }

  fn timeout(&self, token: Token, timer: &mut Timer<Token>, front_timeout: &time::Duration) -> SessionResult {
    unimplemented!();
  }

  fn cancel_timeouts(&self, timer: &mut Timer<Token>) {
    unimplemented!();
  }

}

#[cfg(feature = "use-openssl")]
use network::https_openssl;

use network::https_rustls;

#[cfg(feature = "use-openssl")]
pub enum HttpsProvider {
  Openssl(https_openssl::Proxy),
  Rustls(https_rustls::configuration::Proxy),
}

#[cfg(not(feature = "use-openssl"))]
pub enum HttpsProvider {
  Rustls(https_rustls::configuration::Proxy),
}

#[cfg(feature = "use-openssl")]
impl HttpsProvider {
  pub fn new(use_openssl: bool, pool: Rc<RefCell<Pool<BufferQueue>>>) -> HttpsProvider {
    if use_openssl {
      HttpsProvider::Openssl(https_openssl::Proxy::new(pool))
    } else {
      HttpsProvider::Rustls(https_rustls::configuration::Proxy::new(pool))
    }
  }

  pub fn notify(&mut self, event_loop: &mut Poll, message: ProxyRequest) -> ProxyResponse {
    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => rustls.notify(event_loop, message),
      &mut HttpsProvider::Openssl(ref mut openssl) => openssl.notify(event_loop, message),
    }
  }

  pub fn add_listener(&mut self, config: HttpsListener, pool: Rc<RefCell<Pool<BufferQueue>>>, token: Token) -> Option<Token> {

    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => rustls.add_listener(config, pool, token),
      &mut HttpsProvider::Openssl(ref mut openssl) => openssl.add_listener(config, pool, token),
    }
  }

  pub fn activate_listener(&mut self, event_loop: &mut Poll, addr: &SocketAddr, tcp_listener: Option<TcpListener>) -> Option<Token> {

    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => rustls.activate_listener(event_loop, addr, tcp_listener),
      &mut HttpsProvider::Openssl(ref mut openssl) => openssl.activate_listener(event_loop, addr, tcp_listener),
    }
  }

  pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr,TcpListener)> {
    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => rustls.give_back_listeners(),
      &mut HttpsProvider::Openssl(ref mut openssl) => openssl.give_back_listeners(),
    }
  }

  pub fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => rustls.accept(token),
      &mut HttpsProvider::Openssl(ref mut openssl) => openssl.accept(token),
    }
  }

  pub fn create_session(&mut self, frontend_sock: TcpStream, token: ListenToken, poll: &mut Poll, session_token: Token, timeout: Timeout) -> Result<(Rc<RefCell<ProxySessionCast>>,bool), AcceptError> {
    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => rustls.create_session(frontend_sock, token, poll, session_token, timeout).map(|(r,b)| {
        (r as Rc<RefCell<ProxySessionCast>>, b)
      }),
      &mut HttpsProvider::Openssl(ref mut openssl) => openssl.create_session(frontend_sock, token, poll, session_token, timeout).map(|(r,b)| {
        (r as Rc<RefCell<ProxySessionCast>>, b)
      }),
    }
  }

  pub fn connect_to_backend(&mut self, poll: &mut Poll,  proxy_session: Rc<RefCell<ProxySessionCast>>, back_token: Token)
    -> Result<BackendConnectAction, ConnectionError> {

    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => {
        let mut b = proxy_session.borrow_mut();
        let session: &mut https_rustls::session::Session = b.as_https_rustls();
        rustls.connect_to_backend(poll, session, back_token)
      },
      &mut HttpsProvider::Openssl(ref mut openssl) => {
        let mut b = proxy_session.borrow_mut();
        let session: &mut https_openssl::Session = b.as_https_openssl();
        openssl.connect_to_backend(poll, session, back_token)
      }
    }

  }
}

use network::https_rustls::session::Session;
#[cfg(not(feature = "use-openssl"))]
impl HttpsProvider {
  pub fn new(use_openssl: bool, pool: Rc<RefCell<Pool<BufferQueue>>>) -> HttpsProvider {
    if use_openssl {
      error!("the openssl provider is not compiled, continuing with the rustls provider");
    }

    let configuration = https_rustls::configuration::Proxy::new(pool);
    HttpsProvider::Rustls(configuration)
  }

  pub fn notify(&mut self, event_loop: &mut Poll, message: ProxyRequest) -> ProxyResponse {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;
    rustls.notify(event_loop, message)
  }

  pub fn add_listener(&mut self, config: HttpsListener, pool: Rc<RefCell<Pool<BufferQueue>>>, token: Token) -> Option<Token> {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;
    rustls.add_listener(config, pool, token)
  }

  pub fn activate_listener(&mut self, event_loop: &mut Poll, addr: &SocketAddr, tcp_listener: Option<TcpListener>) -> Option<Token> {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;

    rustls.activate_listener(event_loop, addr, tcp_listener)
  }


  pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;
    rustls.give_back_listeners()
  }

  pub fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;
    rustls.accept(token)
  }

  pub fn create_session(&mut self, frontend_sock: TcpStream, token: ListenToken, poll: &mut Poll, session_token: Token, timeout: Timeout) -> Result<(Rc<RefCell<Session>>,bool), AcceptError> {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;
    rustls.create_session(frontend_sock, token, poll, session_token, timeout)
  }

  pub fn connect_to_backend(&mut self, poll: &mut Poll,  proxy_session: Rc<RefCell<ProxySessionCast>>, back_token: Token)
    -> Result<BackendConnectAction, ConnectionError> {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;

    let mut b = proxy_session.borrow_mut();
    let session: &mut https_rustls::session::Session = b.as_https_rustls() ;
    rustls.connect_to_backend(poll, session, back_token)
  }
}

#[cfg(not(feature = "use-openssl"))]
/// this trait is used to work around the fact that we need to transform
/// the sessions (that are all stored as a ProxySession in the slab) back
/// to their original type to call the `connect_to_backend` method of
/// their corresponding `Proxy`.
/// If we find a way to make `connect_to_backend` work wothout transforming
/// back to the type (example: storing a reference to the configuration
/// in the session and making `connect_to_backend` a method of `ProxySession`?),
/// we'll be able to remove all those ugly casts and panics
pub trait ProxySessionCast: ProxySession {
  fn as_tcp(&mut self) -> &mut tcp::Session;
  fn as_http(&mut self) -> &mut http::Session;
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session;
}

#[cfg(feature = "use-openssl")]
pub trait ProxySessionCast: ProxySession {
  fn as_tcp(&mut self) -> &mut tcp::Session;
  fn as_http(&mut self) -> &mut http::Session;
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session;
  fn as_https_openssl(&mut self) -> &mut https_openssl::Session;
}

#[cfg(not(feature = "use-openssl"))]
impl ProxySessionCast for ListenSession {
  fn as_http(&mut self) -> &mut http::Session { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Session { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session { panic!() }
}

#[cfg(feature = "use-openssl")]
impl ProxySessionCast for ListenSession {
  fn as_http(&mut self) -> &mut http::Session { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Session { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session { panic!() }
  fn as_https_openssl(&mut self) -> &mut https_openssl::Session { panic!() }
}

#[cfg(not(feature = "use-openssl"))]
impl ProxySessionCast for http::Session {
  fn as_http(&mut self) -> &mut http::Session { self }
  fn as_tcp(&mut self) -> &mut tcp::Session { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session { panic!() }
}

#[cfg(feature = "use-openssl")]
impl ProxySessionCast for http::Session {
  fn as_http(&mut self) -> &mut http::Session { self }
  fn as_tcp(&mut self) -> &mut tcp::Session { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session { panic!() }
  fn as_https_openssl(&mut self) -> &mut https_openssl::Session { panic!() }
}

#[cfg(not(feature = "use-openssl"))]
impl ProxySessionCast for tcp::Session {
  fn as_http(&mut self) -> &mut http::Session { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Session { self }
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session { panic!() }
}

#[cfg(feature = "use-openssl")]
impl ProxySessionCast for tcp::Session {
  fn as_http(&mut self) -> &mut http::Session { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Session { self }
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session { panic!() }
  fn as_https_openssl(&mut self) -> &mut https_openssl::Session { panic!() }
}

#[cfg(not(feature = "use-openssl"))]
impl ProxySessionCast for https_rustls::session::Session {
  fn as_http(&mut self) -> &mut http::Session { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Session { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session { self }
}

#[cfg(feature = "use-openssl")]
impl ProxySessionCast for https_rustls::session::Session {
  fn as_http(&mut self) -> &mut http::Session { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Session { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session { self }
  fn as_https_openssl(&mut self) -> &mut https_openssl::Session { panic!() }
}

#[cfg(feature = "use-openssl")]
impl ProxySessionCast for https_openssl::Session {
  fn as_http(&mut self) -> &mut http::Session { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Session { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::session::Session { panic!() }
  fn as_https_openssl(&mut self) -> &mut https_openssl::Session { self }
}

