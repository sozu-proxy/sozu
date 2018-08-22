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
use sozu_command::messages::{self,TcpFront,Order,Backend,MessageId,OrderMessageAnswer,
  OrderMessageAnswerData,OrderMessageStatus,OrderMessage,Topic,Query,QueryAnswer,
  QueryApplicationType,TlsProvider,ListenerType};
use sozu_command::messages::HttpsListener;

use network::buffer_queue::BufferQueue;
use network::{ClientResult,ConnectionError,Protocol,RequiredEvents,ProxyClient,
  CloseResult,AcceptError,BackendConnectAction,ProxyConfiguration};
use network::{http,tcp,AppId};
use network::pool::Pool;
use network::metrics::METRICS;

const SERVER: Token = Token(0);
const DEFAULT_FRONT_TIMEOUT: u64 = 50000;
const DEFAULT_BACK_TIMEOUT:  u64 = 50000;

pub type ProxyChannel = Channel<OrderMessageAnswer,OrderMessage>;

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
pub struct ClientToken(pub usize);

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

impl From<usize> for ClientToken {
    fn from(val: usize) -> ClientToken {
        ClientToken(val)
    }
}

impl From<ClientToken> for usize {
    fn from(val: ClientToken) -> usize {
        val.0
    }
}

pub struct Server {
  pub poll:        Poll,
  shutting_down:   Option<MessageId>,
  accept_ready:    HashSet<ListenToken>,
  can_accept:      bool,
  channel:         ProxyChannel,
  queue:           VecDeque<OrderMessageAnswer>,
  http:            http::ServerConfiguration,
  https:           HttpsProvider,
  tcp:             tcp::ServerConfiguration,
  config_state:    ConfigState,
  scm:             ScmSocket,
  clients:         Slab<Rc<RefCell<ProxyClientCast>>,ClientToken>,
  max_connections: usize,
  nb_connections:  usize,
  timer:           Timer<Token>,
  pool:            Rc<RefCell<Pool<BufferQueue>>>,
  scm_listeners:   Listeners,
}

impl Server {
  pub fn new_from_config(channel: ProxyChannel, scm: ScmSocket, config: Config, config_state: ConfigState) -> Self {
    let event_loop  = Poll::new().expect("could not create event loop");
    let pool = Rc::new(RefCell::new(
      Pool::with_capacity(2*config.max_buffers, 0, || BufferQueue::with_capacity(config.buffer_size))
    ));

    //FIXME: we will use a few entries for the channel, metrics socket and the listeners
    //FIXME: for HTTP/2, we will have more than 2 entries per client
    let mut clients: Slab<Rc<RefCell<ProxyClientCast>>,ClientToken> = Slab::with_capacity(10+2*config.max_connections);
    {
      let entry = clients.vacant_entry().expect("client list should have enough room at startup");
      trace!("taking token {:?} for channel", entry.index());
      entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::Channel })));
    }
    {
      let entry = clients.vacant_entry().expect("client list should have enough room at startup");
      trace!("taking token {:?} for metrics", entry.index());
      entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::Timer })));
    }
    {
      let entry = clients.vacant_entry().expect("client list should have enough room at startup");
      trace!("taking token {:?} for metrics", entry.index());
      entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::Metrics })));
    }

    let use_openssl = config.tls_provider == TlsProvider::Openssl;
    let https = HttpsProvider::new(use_openssl, pool.clone());

    Server::new(event_loop, channel, scm, clients, pool, None, Some(https), None, Some(config_state),
      config.max_connections)
  }

  pub fn new(poll: Poll, channel: ProxyChannel, scm: ScmSocket,
    clients: Slab<Rc<RefCell<ProxyClientCast>>,ClientToken>,
    pool: Rc<RefCell<Pool<BufferQueue>>>,
    http: Option<http::ServerConfiguration>,
    https: Option<HttpsProvider>,
    tcp:  Option<tcp::ServerConfiguration>,
    config_state: Option<ConfigState>,
    max_connections: usize) -> Self {

    info!("will try to receive listeners");
    scm.set_blocking(false);
    let listeners = scm.receive_listeners();
    scm.set_blocking(true);
    info!("received listeners: {:?}", listeners);
    let scm_listeners = listeners.unwrap_or(Listeners {
      http: Vec::new(),
      tls:  Vec::new(),
      tcp:  Vec::new(),
    });

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

    let mut server = Server {
      poll:            poll,
      shutting_down:   None,
      accept_ready:    HashSet::new(),
      can_accept:      true,
      channel:         channel,
      queue:           VecDeque::new(),
      http:            http.unwrap_or(http::ServerConfiguration::new(pool.clone())),
      https:           https.unwrap_or(HttpsProvider::new(false, pool.clone())),
      tcp:             tcp.unwrap_or(tcp::ServerConfiguration::new()),
      config_state:    ConfigState::new(),
      scm:             scm,
      clients:         clients,
      max_connections: max_connections,
      nb_connections:  0,
      timer,
      pool,
      scm_listeners,
    };

    // initialize the worker with the state we got from a file
    if let Some(state) = config_state {
      let mut counter = 0usize;

      for order in state.generate_orders() {
        let id = format!("INIT-{}", counter);
        let message = OrderMessage {
          id:    id,
          order: order,
        };

        trace!("generating initial config order: {:#?}", message);
        server.notify_sessions(message);

        counter += 1;
      }
      // do not send back answers to the initialization messages
      server.queue.clear();
    }

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
                if let Order::HardStop = msg.order {
                  let id_msg = msg.id.clone();
                  self.notify(msg);
                  self.channel.write_message(&OrderMessageAnswer{ id: id_msg, status: OrderMessageStatus::Ok, data: None});
                  self.channel.run();
                  return;
                } else if let Order::SoftStop = msg.order {
                  self.shutting_down = Some(msg.id.clone());
                  self.notify(msg);
                } else if let Order::ReturnListenSockets = msg.order {
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

      let now = SteadyTime::now();
      let dur = time::Duration::minutes(10);
      if now - last_zombie_check > dur {
        info!("zombie check");
        last_zombie_check = now;

        let mut tokens = HashSet::new();
        let mut frontend_tokens = HashSet::new();

        let mut count = 0;
        for client in self.clients.iter_mut().filter(|c| {
          now - c.borrow().last_event() > dur
        }) {
          let t = client.borrow().tokens();
          if !frontend_tokens.contains(&t[0]) {
            client.borrow().print_state();

            frontend_tokens.insert(t[0]);
            for tk in t.into_iter() {
              tokens.insert(tk);
            }

            count += 1;
          }
        }

        for tk in frontend_tokens.iter() {
          let cl = self.to_client(*tk);
          self.close_client(cl);
        }

        if count > 0 {
          count!("zombies", count);

          let mut remaining = 0;
          for tk in tokens.into_iter() {
            let cl = self.to_client(tk);
            if self.clients.remove(cl).is_some() {
              remaining += 1;
            }
          }
          info!("removing {} zombies ({} remaining tokens after close)", count, remaining);
        }
      }

      gauge!("slab.count", self.clients.len());
      METRICS.with(|metrics| {
        (*metrics.borrow_mut()).send_data();
      });

      if self.shutting_down.is_some() {
        info!("last client stopped, shutting down!");
        self.channel.run();
        self.channel.set_blocking(true);
        self.channel.write_message(&OrderMessageAnswer{ id: self.shutting_down.take().expect("should have shut down correctly"), status: OrderMessageStatus::Ok, data: None});
        return;
      }
    }
  }

  fn notify(&mut self, message: OrderMessage) {
    if let Order::Metrics = message.order {
      let q = &mut self.queue;
      //let id = message.id.clone();
      let msg = METRICS.with(|metrics| {
        q.push_back(OrderMessageAnswer {
          id:     message.id.clone(),
          status: OrderMessageStatus::Ok,
          data:   Some(OrderMessageAnswerData::Metrics(
            (*metrics.borrow_mut()).dump_metrics_data()
          ))
        });
      });
      return;
    }

    if let Order::Query(ref query) = message.order {
      match query {
        &Query::ApplicationsHashes => {
          self.queue.push_back(OrderMessageAnswer {
            id:     message.id.clone(),
            status: OrderMessageStatus::Ok,
            data:   Some(OrderMessageAnswerData::Query(
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

          self.queue.push_back(OrderMessageAnswer {
            id:     message.id.clone(),
            status: OrderMessageStatus::Ok,
            data:   Some(OrderMessageAnswerData::Query(answer))
          });
        }
      }
      return
    }

    self.notify_sessions(message);
  }

  pub fn notify_sessions(&mut self, message: OrderMessage) {
    self.config_state.handle_order(&message.order);

    let topics = message.order.get_topics();

    if topics.contains(&Topic::HttpProxyConfig) {
      match message {
        // special case for AddHttpListener because we need to register a listener
        OrderMessage { ref id, order: Order::AddHttpListener(ref listener) } => {
          /*FIXME
          if self.listen_port_state(&tcp_front.port) == ListenPortState::InUse {
            error!("Couldn't add TCP front {:?}: port already in use", tcp_front);
            self.queue.push_back(OrderMessageAnswer {
              id,
              status: OrderMessageStatus::Error(String::from("Couldn't add TCP front: port already in use")),
              data: None
            });
            return;
          }*/

          let entry = self.clients.vacant_entry();

          if entry.is_none() {
            self.queue.push_back(OrderMessageAnswer {
              id: id.to_string(),
              status: OrderMessageStatus::Error(String::from("client list is full, cannot add a listener")),
              data: None
            });
            return;
          }

          let entry = entry.unwrap();

          let token = Token(entry.index().0);

          let front = listener.front.clone();
          let status = if let Some(token) = self.http.add_listener(listener.clone(), self.pool.clone(), token) {
            entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
            OrderMessageStatus::Ok
          } else {
            error!("Couldn't add HTTP listener");
            OrderMessageStatus::Error(String::from("cannot add HTTP listener"))
          };

          let answer = OrderMessageAnswer { id: id.to_string(), status, data: None };
          self.queue.push_back(answer);
        },
        OrderMessage { ref id, order: Order::RemoveListener(ref remove) } => {
          if remove.proxy == ListenerType::HTTP {
            self.queue.push_back(self.http.notify(&mut self.poll, OrderMessage {
              id: id.to_string(),
              order: Order::RemoveListener(remove.clone())
            }));
          }
        },
        OrderMessage { ref id, order: Order::ActivateListener(ref activate) } => {
          if activate.proxy == ListenerType::HTTP {
            let listener = self.scm_listeners.get_http(&activate.front).map(|fd| unsafe { TcpListener::from_raw_fd(fd) });
            let status = match self.http.activate_listener(&mut self.poll, &activate.front, listener) {
              Some(token) => {
                self.accept(ListenToken(token.0), Protocol::HTTPListen);
                OrderMessageStatus::Ok
              },
              None => {
                error!("Couldn't activate HTTP listener");
                OrderMessageStatus::Error(String::from("cannot activate HTTP listener"))
              }
            };

            let answer = OrderMessageAnswer { id: id.to_string(), status, data: None };
            self.queue.push_back(answer);
          }
        },
        ref m => self.queue.push_back(self.http.notify(&mut self.poll, m.clone())),
      }
    }
    if topics.contains(&Topic::HttpsProxyConfig) {
      match message {
        // special case for AddHttpListener because we need to register a listener
        OrderMessage { ref id, order: Order::AddHttpsListener(ref listener) } => {
          /*FIXME
          if self.listen_port_state(&tcp_front.port) == ListenPortState::InUse {
            error!("Couldn't add TCP front {:?}: port already in use", tcp_front);
            self.queue.push_back(OrderMessageAnswer {
              id,
              status: OrderMessageStatus::Error(String::from("Couldn't add TCP front: port already in use")),
              data: None
            });
            return;
          }*/

          let entry = self.clients.vacant_entry();

          if entry.is_none() {
            self.queue.push_back(OrderMessageAnswer {
              id: id.to_string(),
              status: OrderMessageStatus::Error(String::from("client list is full, cannot add a listener")),
              data: None
            });
            return;
          }

          let entry = entry.unwrap();

          let token = Token(entry.index().0);

          let front = listener.front.clone();
          let status = if let Some(token) = self.https.add_listener(listener.clone(), self.pool.clone(), token) {
            entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPSListen })));
            OrderMessageStatus::Ok
          } else {
            error!("Couldn't add HTTPS listener");
            OrderMessageStatus::Error(String::from("cannot add HTTPS listener"))
          };

          let answer = OrderMessageAnswer { id: id.to_string(), status, data: None };
          self.queue.push_back(answer);
        },
        OrderMessage { ref id, order: Order::RemoveListener(ref remove) } => {
          if remove.proxy == ListenerType::HTTPS {
            self.queue.push_back(self.https.notify(&mut self.poll, OrderMessage {
              id: id.to_string(),
              order: Order::RemoveListener(remove.clone())
            }));
          }
        },
        OrderMessage { ref id, order: Order::ActivateListener(ref activate) } => {
          if activate.proxy == ListenerType::HTTPS {
            let listener = self.scm_listeners.get_https(&activate.front).map(|fd| unsafe { TcpListener::from_raw_fd(fd) });
            let status = match self.https.activate_listener(&mut self.poll, &activate.front, listener) {
              Some(token) => {
                self.accept(ListenToken(token.0), Protocol::HTTPSListen);
                OrderMessageStatus::Ok
              },
              None => {
                error!("Couldn't activate HTTPS listener");
                OrderMessageStatus::Error(String::from("cannot activate HTTPS listener"))
              }
            };

            let answer = OrderMessageAnswer { id: id.to_string(), status, data: None };
            self.queue.push_back(answer);
          }
        },
        ref m => self.queue.push_back(self.https.notify(&mut self.poll, m.clone())),
      }
    }
    if topics.contains(&Topic::TcpProxyConfig) {
      match message {
        // special case for AddTcpFront because we need to register a listener
        OrderMessage { id, order: Order::AddTcpListener(listener) } => {
          /*if self.listen_port_state(&tcp_front.port) == ListenPortState::InUse {
            error!("Couldn't add TCP front {:?}: port already in use", tcp_front);
            self.queue.push_back(OrderMessageAnswer {
              id,
              status: OrderMessageStatus::Error(String::from("Couldn't add TCP front: port already in use")),
              data: None
            });
            return;
          }
          */

          let entry = self.clients.vacant_entry();

          if entry.is_none() {
            self.queue.push_back(OrderMessageAnswer {
              id,
              status: OrderMessageStatus::Error(String::from("client list is full, cannot add a listener")),
              data: None
            });
            return;
          }

          let entry = entry.unwrap();

          let token = Token(entry.index().0);

          let front = listener.front.clone();
          let status = if let Some(token) = self.tcp.add_listener(listener.clone(), self.pool.clone(), token) {
            entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::TCPListen })));
            OrderMessageStatus::Ok
          } else {
            error!("Couldn't add TCP listener");
            OrderMessageStatus::Error(String::from("cannot add TCP listener"))
          };
          let answer = OrderMessageAnswer { id, status, data: None };
          self.queue.push_back(answer);
        },
        OrderMessage { ref id, order: Order::RemoveListener(ref remove) } => {
          if remove.proxy == ListenerType::TCP {
            self.queue.push_back(self.tcp.notify(&mut self.poll, OrderMessage {
              id: id.to_string(),
              order: Order::RemoveListener(remove.clone())
            }));
          }
        },
        OrderMessage { ref id, order: Order::ActivateListener(ref activate) } => {
          if activate.proxy == ListenerType::TCP {
            let listener = self.scm_listeners.get_tcp(&activate.front).map(|fd| unsafe { TcpListener::from_raw_fd(fd) });
            let status = match self.tcp.activate_listener(&mut self.poll, &activate.front, listener) {
              Some(token) => {
                self.accept(ListenToken(token.0), Protocol::TCPListen);
                OrderMessageStatus::Ok
              },
              None => {
                error!("Couldn't activate TCP listener");
                OrderMessageStatus::Error(String::from("cannot activate TCP listener"))
              }
            };

            let answer = OrderMessageAnswer { id: id.to_string(), status, data: None };
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

  pub fn to_client(&self, token: Token) -> ClientToken {
    ClientToken(token.0)
  }

  pub fn from_client(&self, token: ClientToken) -> Token {
    Token(token.0)
  }

  pub fn close_client(&mut self, token: ClientToken) {
    if self.clients.contains(token) {
      let client = self.clients.remove(token).expect("client shoud be there");
      client.borrow().cancel_timeouts(&mut self.timer);
      let CloseResult { tokens, backends } = client.borrow_mut().close(&mut self.poll);

      for tk in tokens.into_iter() {
        let cl = self.to_client(tk);
        self.clients.remove(cl);
      }

      let protocol = { client.borrow().protocol() };

      match protocol {
        Protocol::TCP   => {
          for (app_id, address) in backends.into_iter() {
            self.tcp.close_backend(app_id, &address);
          }
        },
        Protocol::HTTP   => {
          for (app_id, address) in backends.into_iter() {
            self.http.close_backend(app_id, &address);
          }
        },
        Protocol::HTTPS   => {
          for (app_id, address) in backends.into_iter() {
            self.https.close_backend(app_id, &address);
          }
        },
        _ => {}
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

  pub fn accept(&mut self, token: ListenToken, protocol: Protocol) -> bool {
    let res = {

      if self.nb_connections == self.max_connections {
        error!("max number of client connection reached, flushing the accept queue");
        Err(AcceptError::TooManyClients)
      } else {
        //FIXME: we must handle separately the client limit since the clients slab also has entries for listeners and backends
        match self.clients.vacant_entry() {
          None => {
            error!("not enough memory to accept another client, flushing the accept queue");
            error!("nb_connections: {}, max_connections: {}", self.nb_connections, self.max_connections);
            //FIXME: should accept in a loop and close connections here instead of letting them wait
            Err(AcceptError::TooManyClients)
          },
          Some(entry) => {
            let client_token = Token(entry.index().0);
            let index = entry.index();
            let timeout = self.timer.set_timeout(Duration::from_secs(60), client_token);
            match protocol {
              Protocol::TCPListen   => {
                let res1 = self.tcp.accept(token, &mut self.poll, client_token, timeout).map(|(client, should_connect)| {
                  entry.insert(client);
                  (index, should_connect)
                });
                res1
              },
              Protocol::HTTPListen  => {
                let res1 = self.http.accept(token, &mut self.poll, client_token, timeout).map(|(client, should_connect)| {
                  entry.insert(client);
                  (index, should_connect)
                });
                res1
              },
              Protocol::HTTPSListen => {
                let res1 = self.https.accept(token, &mut self.poll, client_token, timeout).map(|(client, should_connect)| {
                  entry.insert(client);
                  (index, should_connect)
                });
                res1
              },
              _ => panic!("should not call accept() on a HTTP, HTTPS or TCP client"),
            }
          }
        }
      }
    };

    match res {
      Err(AcceptError::IoError) => {
        //FIXME: do we stop accepting?
        false
      },
      Err(AcceptError::TooManyClients) => {
        self.accept_flush();
        self.can_accept = false;
        false
      },
      Err(AcceptError::WouldBlock) => {
        self.accept_ready.remove(&token);
        false
      },
      Ok((client_token, should_connect)) => {
        self.nb_connections += 1;
        assert!(self.nb_connections <= self.max_connections);
        gauge!("client.connections", self.nb_connections);

        if should_connect {
           self.connect_to_backend(client_token);
        }

        true
      }
    }
  }

  pub fn accept_flush(&mut self) {
    let mut counter = self.tcp.accept_flush();
    counter += self.http.accept_flush();
    counter += self.https.accept_flush();
    error!("flushed {} new connections", counter);
  }

  pub fn connect_to_backend(&mut self, token: ClientToken) {
    if ! self.clients.contains(token) {
      error!("invalid token in connect_to_backend");
      return;
    }

    let (protocol, res) = {
      let cl = self.clients[token].clone();
      let cl2: Rc<RefCell<ProxyClientCast>> = self.clients[token].clone();
      let protocol = { cl.borrow().protocol() };
      let entry = self.clients.vacant_entry();
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
          let client: &mut tcp::Client = b.as_tcp() ;
          let r = self.tcp.connect_to_backend(&mut self.poll, client, back_token);

          (Protocol::TCP, r)
        },
        Protocol::HTTP  => {
          (Protocol::HTTP, {
            let mut b = cl2.borrow_mut();
            let client: &mut http::Client = b.as_http() ;
            let r = self.http.connect_to_backend(&mut self.poll, client, back_token);
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
          self.close_client(token);
        }
      },
      _ => self.close_client(token),
    }
  }

  pub fn interpret_client_order(&mut self, token: ClientToken, order: ClientResult) {
    //trace!("INTERPRET ORDER: {:?}", order);
    match order {
      ClientResult::CloseClient     => self.close_client(token),
      ClientResult::CloseBackend(opt) => {
        if let Some(token) = opt {
          let cl = self.to_client(token);
          if let Some(client) = self.clients.remove(cl) {
            let protocol = client.borrow().protocol();
            let res = client.borrow_mut().close_backend(token, &mut self.poll);
            if let Some((app_id, address)) = res {
              match protocol {
                Protocol::TCP   => {
                  Some(self.tcp.close_backend(app_id, &address))
                },
                Protocol::HTTP  => {
                  Some(self.http.close_backend(app_id, &address))
                },
                Protocol::HTTPS => {
                  Some(self.https.close_backend(app_id, &address))
                },
                _ => {
                  panic!("should not call interpret_client_order on a listen socket");
                }
              };
            }
          }
        }
      },
      ClientResult::ReconnectBackend(main_token, backend_token)  => {
        if let Some(t) = backend_token {
          let cl = self.to_client(t);
          let _ = self.clients.remove(cl);
        }

        if let Some(t) = main_token {
          let tok = self.to_client(t);
          self.connect_to_backend(tok)
        }
      },
      ClientResult::ConnectBackend  => self.connect_to_backend(token),
      ClientResult::Continue        => {}
    }
  }

  pub fn ready(&mut self, token: Token, events: Ready) {
    trace!("PROXY\t{:?} got events: {:?}", token, events);

    let mut client_token = ClientToken(token.0);
    if self.clients.contains(client_token) {
      //info!("clients contains {:?}", client_token);
      let protocol = self.clients[client_token].borrow().protocol();
      //info!("protocol: {:?}", protocol);
      match protocol {
        Protocol::HTTPListen | Protocol::HTTPSListen | Protocol::TCPListen => {
          //info!("PROTOCOL IS LISTEN");
          if events.is_readable() {
            self.accept_ready.insert(ListenToken(token.0));
            if self.can_accept {
              loop {
                //info!("will accept");
                if !self.accept(ListenToken(token.0), protocol) {
                  break;
                }
              }
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

      self.clients[client_token].borrow_mut().process_events(token, events);

      loop {
        //self.client_ready(poll, client_token, events);
        if !self.clients.contains(client_token) {
          break;
        }

        let order = self.clients[client_token].borrow_mut().ready();
        trace!("client[{:?} -> {:?}] got events {:?} and returned order {:?}", client_token, self.from_client(client_token), events, order);
        //FIXME: the CloseBackend message might not mean we have nothing else to do
        //with that client
        let is_connect = match order {
          ClientResult::ConnectBackend | ClientResult::ReconnectBackend(_,_) => true,
          _ => false,
        };

        // if we got ReconnectBackend, that means the current client_token
        // corresponds to an entry that will be removed in interpret_client_order
        // so we ask for the "main" token, ie the one for the front socket
        if let ClientResult::ReconnectBackend(Some(t), _) = order {
          client_token = self.to_client(t);
        }

        self.interpret_client_order(client_token, order);

        // if we had to connect to a backend server, go back to the loop
        // I'm not sure we would have anything to do right away, though,
        // so maybe we can just stop there for that client?
        // also the events would change?
        if !is_connect {
          break;
        }
      }
    }
  }

  pub fn timeout(&mut self, token: Token) {
    trace!("PROXY\t{:?} got timeout", token);

    let client_token = ClientToken(token.0);
    if self.clients.contains(client_token) {
      let order = self.clients[client_token].borrow_mut().timeout(token, &mut self.timer);
      self.interpret_client_order(client_token, order);
    }
  }

  pub fn handle_remaining_readiness(&mut self) {
    // try to accept again after handling all client events,
    // since we might have released a few client slots
    if self.can_accept && !self.accept_ready.is_empty() {
      loop {
        if let Some(token) = self.accept_ready.iter().next().map(|token| ListenToken(token.0)) {
          let protocol = self.clients[ClientToken(token.0)].borrow().protocol();
          if !self.accept(token, protocol) {
            if !self.can_accept || self.accept_ready.is_empty() {
              break;
            }
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

pub struct ListenClient {
  pub protocol: Protocol,
}

impl ProxyClient for ListenClient {
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

  fn ready(&mut self) -> ClientResult {
    ClientResult::Continue
  }

  fn process_events(&mut self, token: Token, events: Ready) {}

  fn close(&mut self, poll: &mut Poll) -> CloseResult {
    CloseResult::default()
  }

  fn close_backend(&mut self, token: Token, poll: &mut Poll) -> Option<(String, SocketAddr)> {
    None
  }

  fn timeout(&self, token: Token, timer: &mut Timer<Token>) -> ClientResult {
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
  Openssl(https_openssl::ServerConfiguration),
  Rustls(https_rustls::configuration::ServerConfiguration),
}

#[cfg(not(feature = "use-openssl"))]
pub enum HttpsProvider {
  Rustls(https_rustls::configuration::ServerConfiguration),
}

#[cfg(feature = "use-openssl")]
impl HttpsProvider {
  pub fn new(use_openssl: bool, pool: Rc<RefCell<Pool<BufferQueue>>>) -> HttpsProvider {
    if use_openssl {
      HttpsProvider::Openssl(https_openssl::ServerConfiguration::new(pool))
    } else {
      HttpsProvider::Rustls(https_rustls::configuration::ServerConfiguration::new(pool))
    }
  }

  pub fn notify(&mut self, event_loop: &mut Poll, message: OrderMessage) -> OrderMessageAnswer {
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

  pub fn accept(&mut self, token: ListenToken, poll: &mut Poll, client_token: Token, timeout: Timeout) -> Result<(Rc<RefCell<ProxyClientCast>>,bool), AcceptError> {
    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => rustls.accept(token, poll, client_token, timeout).map(|(r,b)| {
        (r as Rc<RefCell<ProxyClientCast>>, b)
      }),
      &mut HttpsProvider::Openssl(ref mut openssl) => openssl.accept(token, poll, client_token, timeout).map(|(r,b)| {
        (r as Rc<RefCell<ProxyClientCast>>, b)
      }),
    }
  }

  pub fn close_backend(&mut self, app_id: AppId, addr: &SocketAddr) {
    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => rustls.close_backend(app_id, addr),
      &mut HttpsProvider::Openssl(ref mut openssl) => openssl.close_backend(app_id, addr),
    }
  }

  pub fn accept_flush(&mut self) -> usize {
    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => rustls.accept_flush(),
      &mut HttpsProvider::Openssl(ref mut openssl) => openssl.accept_flush(),
    }
  }

  pub fn connect_to_backend(&mut self, poll: &mut Poll,  proxy_client: Rc<RefCell<ProxyClientCast>>, back_token: Token)
    -> Result<BackendConnectAction, ConnectionError> {

    match self {
      &mut HttpsProvider::Rustls(ref mut rustls)   => {
        let mut b = proxy_client.borrow_mut();
        let client: &mut https_rustls::client::TlsClient = b.as_https_rustls();
        rustls.connect_to_backend(poll, client, back_token)
      },
      &mut HttpsProvider::Openssl(ref mut openssl) => {
        let mut b = proxy_client.borrow_mut();
        let client: &mut https_openssl::TlsClient = b.as_https_openssl();
        openssl.connect_to_backend(poll, client, back_token)
      }
    }

  }
}

use network::https_rustls::client::TlsClient;
#[cfg(not(feature = "use-openssl"))]
impl HttpsProvider {
  pub fn new(use_openssl: bool, pool: Rc<RefCell<Pool<BufferQueue>>>) -> HttpsProvider {
    if use_openssl {
      error!("the openssl provider is not compiled, continuing with the rustls provider");
    }

    let configuration = https_rustls::configuration::ServerConfiguration::new(pool);
    HttpsProvider::Rustls(configuration)
  }

  pub fn notify(&mut self, event_loop: &mut Poll, message: OrderMessage) -> OrderMessageAnswer {
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

  pub fn accept(&mut self, token: ListenToken, poll: &mut Poll, client_token: Token, timeout: Timeout) -> Result<(Rc<RefCell<TlsClient>>,bool), AcceptError> {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;
    rustls.accept(token, poll, client_token, timeout)
  }

  pub fn close_backend(&mut self, app_id: AppId, addr: &SocketAddr) {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;
    rustls.close_backend(app_id, addr);
  }

  pub fn accept_flush(&mut self) -> usize {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;
    rustls.accept_flush()
  }

  pub fn connect_to_backend(&mut self, poll: &mut Poll,  proxy_client: Rc<RefCell<ProxyClientCast>>, back_token: Token)
    -> Result<BackendConnectAction, ConnectionError> {
    let &mut HttpsProvider::Rustls(ref mut rustls) = self;

    let mut b = proxy_client.borrow_mut();
    let client: &mut https_rustls::client::TlsClient = b.as_https_rustls() ;
    rustls.connect_to_backend(poll, client, back_token)
  }
}

#[cfg(not(feature = "use-openssl"))]
/// this trait is used to work around the fact that we need to transform
/// the clients (that are all stored as a ProxyClient in the slab) back
/// to their original type to call the `connect_to_backend` method of
/// their corresponding `ServerConfiguration`.
/// If we find a way to make `connect_to_backend` work wothout transforming
/// back to the type (example: storing a reference to the configuration
/// in the client and making `connect_to_backend` a method of `ProxyClient`?),
/// we'll be able to remove all those ugly casts and panics
pub trait ProxyClientCast: ProxyClient {
  fn as_tcp(&mut self) -> &mut tcp::Client;
  fn as_http(&mut self) -> &mut http::Client;
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient;
}

#[cfg(feature = "use-openssl")]
pub trait ProxyClientCast: ProxyClient {
  fn as_tcp(&mut self) -> &mut tcp::Client;
  fn as_http(&mut self) -> &mut http::Client;
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient;
  fn as_https_openssl(&mut self) -> &mut https_openssl::TlsClient;
}

#[cfg(not(feature = "use-openssl"))]
impl ProxyClientCast for ListenClient {
  fn as_http(&mut self) -> &mut http::Client { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Client { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient { panic!() }
}

#[cfg(feature = "use-openssl")]
impl ProxyClientCast for ListenClient {
  fn as_http(&mut self) -> &mut http::Client { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Client { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient { panic!() }
  fn as_https_openssl(&mut self) -> &mut https_openssl::TlsClient { panic!() }
}

#[cfg(not(feature = "use-openssl"))]
impl ProxyClientCast for http::Client {
  fn as_http(&mut self) -> &mut http::Client { self }
  fn as_tcp(&mut self) -> &mut tcp::Client { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient { panic!() }
}

#[cfg(feature = "use-openssl")]
impl ProxyClientCast for http::Client {
  fn as_http(&mut self) -> &mut http::Client { self }
  fn as_tcp(&mut self) -> &mut tcp::Client { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient { panic!() }
  fn as_https_openssl(&mut self) -> &mut https_openssl::TlsClient { panic!() }
}

#[cfg(not(feature = "use-openssl"))]
impl ProxyClientCast for tcp::Client {
  fn as_http(&mut self) -> &mut http::Client { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Client { self }
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient { panic!() }
}

#[cfg(feature = "use-openssl")]
impl ProxyClientCast for tcp::Client {
  fn as_http(&mut self) -> &mut http::Client { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Client { self }
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient { panic!() }
  fn as_https_openssl(&mut self) -> &mut https_openssl::TlsClient { panic!() }
}

#[cfg(not(feature = "use-openssl"))]
impl ProxyClientCast for https_rustls::client::TlsClient {
  fn as_http(&mut self) -> &mut http::Client { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Client { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient { self }
}

#[cfg(feature = "use-openssl")]
impl ProxyClientCast for https_rustls::client::TlsClient {
  fn as_http(&mut self) -> &mut http::Client { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Client { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient { self }
  fn as_https_openssl(&mut self) -> &mut https_openssl::TlsClient { panic!() }
}

#[cfg(feature = "use-openssl")]
impl ProxyClientCast for https_openssl::TlsClient {
  fn as_http(&mut self) -> &mut http::Client { panic!() }
  fn as_tcp(&mut self) -> &mut tcp::Client { panic!() }
  fn as_https_rustls(&mut self) -> &mut https_rustls::client::TlsClient { panic!() }
  fn as_https_openssl(&mut self) -> &mut https_openssl::TlsClient { self }
}

