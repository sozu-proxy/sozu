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
use std::os::unix::io::{AsRawFd,FromRawFd};
use nom::HexDisplay;
use std::error::Error;
use slab::Slab;
use pool::Pool;
use std::io::Write;
use std::str::FromStr;
use std::marker::PhantomData;
use std::fmt::Debug;
use time::precise_time_ns;
use std::time::Duration;
use rand::random;

use sozu_command::config::Config;
use sozu_command::channel::Channel;
use sozu_command::scm_socket::{Listeners,ScmSocket};
use sozu_command::state::{ConfigState,get_application_ids_by_domain};
use sozu_command::messages::{self,TcpFront,Order,Instance,MessageId,OrderMessageAnswer,OrderMessageAnswerData,OrderMessageStatus,OrderMessage,Topic,Query,QueryAnswer,QueryApplicationType};

use network::buffer_queue::BufferQueue;
use network::{ClientResult,ConnectionError,Protocol,RequiredEvents,ProxyClient,ProxyConfiguration,
  CloseResult,AcceptError,BackendConnectAction};
use network::{http,https,tcp};
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
  http:            Option<http::ServerConfiguration>,
  https:           Option<https::ServerConfiguration>,
  tcp:             Option<tcp::ServerConfiguration>,
  config_state:    ConfigState,
  scm:             ScmSocket,
  clients:         Slab<Rc<RefCell<ProxyClient>>,ClientToken>,
}

impl Server {
  pub fn new_from_config(channel: ProxyChannel, scm: ScmSocket, config: Config, config_state: ConfigState) -> Self {
    let mut event_loop  = Poll::new().expect("could not create event loop");
    let pool = Rc::new(RefCell::new(
      Pool::with_capacity(2*config.max_buffers, 0, || BufferQueue::with_capacity(config.buffer_size))
    ));

    info!("will try to receive listeners");
    scm.set_blocking(false);
    let listeners = scm.receive_listeners();
    scm.set_blocking(true);
    info!("received listeners: {:#?}", listeners);
    let mut listeners = listeners.unwrap_or(Listeners {
      http: None,
      tls:  None,
      tcp:  Vec::new(),
    });

    let mut clients: Slab<Rc<RefCell<ProxyClient>>,ClientToken> = Slab::with_capacity(10000);
    {
      let entry = clients.vacant_entry().expect("client list should have enough room at startup");
      info!("taking token {:?} for channel", entry.index());
      entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
    }
    {
      let entry = clients.vacant_entry().expect("client list should have enough room at startup");
      info!("taking token {:?} for metrics", entry.index());
      entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
    }

    let max_connections = config.max_connections;
    let max_buffers     = config.max_buffers;
    let http_session = config.http.and_then(|conf| conf.to_http()).map(|http_conf| {
      let entry = clients.vacant_entry().expect("client list should have enough room at startup");
      let token = Token(entry.index().0);
      let (configuration, listener_tokens) = http::ServerConfiguration::new(http_conf, &mut event_loop,
        pool.clone(), listeners.http.map(|fd| unsafe { TcpListener::from_raw_fd(fd) }), token);

      if listener_tokens.len() == 1 {
        let e = entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPListen })));
        info!("inserting http listener at token: {:?}", e.index());
      }
      configuration
    });

    let https_session = config.https.and_then(|conf| conf.to_tls()).and_then(|https_conf| {
      let entry = clients.vacant_entry().expect("client list should have enough room at startup");
      let token = Token(entry.index().0);
      https::ServerConfiguration::new(https_conf, &mut event_loop, pool.clone(),
      listeners.tls.map(|fd| unsafe { TcpListener::from_raw_fd(fd) }), token
      ).map(|(configuration, listener_tokens)| {
        if listener_tokens.len() == 1 {
          entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::HTTPSListen })));
        }
        configuration
      }).ok()
    });

    let tcp_listeners: Vec<(String, TcpListener)> = listeners.tcp.drain(..).map(|(app_id, fd)| {
      (app_id, unsafe { TcpListener::from_raw_fd(fd) })
    }).collect();

    let mut tokens = Vec::new();
    for _ in 0..tcp_listeners.len() {
      let entry = clients.vacant_entry().expect("client list should have enough room at startup");
      let token = Token(entry.index().0);
      entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::TCPListen })));
      tokens.push(token);
    }

    let tcp_tokens: HashSet<Token> = tokens.iter().cloned().collect();

    let tcp_session = config.tcp.map(|conf| {
      let (configuration, listener_tokens) = tcp::ServerConfiguration::new(conf.max_listeners,
      &mut event_loop, pool.clone(), tcp_listeners, tokens);

      let to_remove:Vec<Token> = tcp_tokens.difference(&listener_tokens).cloned().collect();
      for token in to_remove.into_iter() {
        clients.remove(ClientToken(token.0));
      }
      configuration
    });

    Server::new(event_loop, channel, scm, clients, http_session, https_session, tcp_session, Some(config_state))
  }

  pub fn new(poll: Poll, channel: ProxyChannel, scm: ScmSocket,
    clients: Slab<Rc<RefCell<ProxyClient>>,ClientToken>,
    http:  Option<http::ServerConfiguration>,
    https: Option<https::ServerConfiguration>,
    tcp:  Option<tcp::ServerConfiguration>,
    config_state: Option<ConfigState>) -> Self {

    poll.register(
      &channel,
      Token(0),
      Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
      PollOpt::edge()
    ).expect("should register the channel");

    METRICS.with(|metrics| {
      if let Some(sock) = (*metrics.borrow()).socket() {
        poll.register(sock, Token(1), Ready::writable(), PollOpt::edge()).expect("should register the metrics socket");
      } else {
        error!("could not register metrics socket");
      }
    });

    let mut server = Server {
      poll:            poll,
      shutting_down:   None,
      accept_ready:    HashSet::new(),
      can_accept:      true,
      channel:         channel,
      queue:           VecDeque::new(),
      http:            http,
      https:           https,
      tcp:             tcp,
      config_state:    ConfigState::new(),
      scm:             scm,
      clients:         clients,
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

//type Timeout = usize;

impl Server {
  pub fn run(&mut self) {
    //FIXME: make those parameters configurable?
    let mut events = Events::with_capacity(1024);
    let poll_timeout = Some(Duration::from_millis(1000));
    let max_poll_errors = 10000;
    let mut current_poll_errors = 0;
    loop {
      if current_poll_errors == max_poll_errors {
        error!("Something is going very wrong. Last {} poll() calls failed, crashing..", current_poll_errors);
        panic!(format!("poll() calls failed {} times in a row", current_poll_errors));
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
          METRICS.with(|metrics| {
            (*metrics.borrow_mut()).writable();
          });
        } else {
          self.ready(event.token(), event.readiness());
        }
      }

      self.handle_remaining_readiness();

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
      if let Some(mut http) = self.http.take() {
        self.queue.push_back(http.notify(&mut self.poll, message.clone()));
        self.http = Some(http);
      }
    }
    if topics.contains(&Topic::HttpsProxyConfig) {
      if let Some(mut https) = self.https.take() {
        self.queue.push_back(https.notify(&mut self.poll, message.clone()));
        self.https = Some(https);
      }
    }
    if topics.contains(&Topic::TcpProxyConfig) {
      if let Some(mut tcp) = self.tcp.take() {
        match message {
          // special case for AddTcpFront because we need to register a listener
          OrderMessage { id, order: Order::AddTcpFront(tcp_front) } => {
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
            entry.insert(Rc::new(RefCell::new(ListenClient { protocol: Protocol::TCPListen })));

            let addr_string = tcp_front.ip_address + ":" + &tcp_front.port.to_string();

            let status = if let Ok(front) = addr_string.parse() {
               if let Some(token) = tcp.add_tcp_front(&tcp_front.app_id, &front, &mut self.poll, token) {
                 OrderMessageStatus::Ok
               } else {
                 error!("Couldn't add tcp front");
                 OrderMessageStatus::Error(String::from("cannot add tcp front"))
               }
            } else {
              error!("Couldn't parse tcp front address");
              OrderMessageStatus::Error(String::from("cannot parse the address"))
            };
            let answer = OrderMessageAnswer { id, status, data: None };
            self.queue.push_back(answer);
          },
          m => self.queue.push_back(tcp.notify(&mut self.poll, m)),
        }
        self.tcp = Some(tcp);
      }
    }
  }

  pub fn return_listen_sockets(&mut self) {
    self.scm.set_blocking(false);

    let http_listener = self.http.as_mut()
      .and_then(|http| http.give_back_listener());
    if let Some(ref sock) = http_listener {
      self.poll.deregister(sock);
    }

    let https_listener = self.https.as_mut()
      .and_then(|https| https.give_back_listener());
    if let Some(ref sock) = https_listener {
      self.poll.deregister(sock);
    }

    let tcp_listeners = self.tcp.as_mut()
      .map(|tcp| tcp.give_back_listeners()).unwrap_or(Vec::new());

    for &(_, ref sock) in tcp_listeners.iter() {
      self.poll.deregister(sock);
    }

    let res = self.scm.send_listeners(Listeners {
      http: http_listener.map(|listener| listener.as_raw_fd()),
      tls:  https_listener.map(|listener| listener.as_raw_fd()),
      tcp:  tcp_listeners.into_iter().map(|(app_id, listener)| (app_id, listener.as_raw_fd())).collect(),
    });

    self.scm.set_blocking(true);

    info!("sent default listeners: {:?}", res);

  }

  pub fn to_client(&self, token: Token) -> ClientToken {
    ClientToken(token.0)
  }

  pub fn from_client(&self, token: ClientToken) -> Token {
    Token(token.0)
  }

  pub fn from_client_add(&self) -> usize {
    0
  }

  pub fn close_client(&mut self, token: ClientToken) {
    if self.clients.contains(token) {
      let client = self.clients.remove(token).expect("client shoud be there");
      let CloseResult { tokens, backends } = client.borrow_mut().close(&mut self.poll);

      for tk in tokens.into_iter() {
        let cl = self.to_client(tk);
        self.clients.remove(cl);
      }

      let protocol = { client.borrow().protocol() };

      match protocol {
        Protocol::TCP   => {
          if let Some(mut tcp) = self.tcp.take() {
            for (app_id, address) in backends.into_iter() {
              tcp.close_backend(app_id, &address);
            }

            self.tcp = Some(tcp);
          }
        },
        Protocol::HTTP   => {
          if let Some(mut http) = self.http.take() {
            for (app_id, address) in backends.into_iter() {
              http.close_backend(app_id, &address);
            }

            self.http = Some(http);
          }
        },
        Protocol::HTTPS   => {
          if let Some(mut https) = self.https.take() {
              for (app_id, address) in backends.into_iter() {
                https.close_backend(app_id, &address);
              }

            self.https = Some(https);
          }
        },
        _ => {}
      }

      decr!("client.connections");
    }

    self.can_accept = true;
  }

  pub fn accept(&mut self, token: ListenToken, protocol: Protocol) -> bool {
    let add = self.from_client_add();
    let res = {

      //FIXME: we must handle separately the client limit since the clients slab also has entries for listeners and backends
      match self.clients.vacant_entry() {
        None => {
          error!("not enough memory to accept another client");
          //FIXME: should accept in a loop and close connections here instead of letting them wait
          Err(AcceptError::TooManyClients)
        },
        Some(entry) => {
          let client_token = Token(entry.index().0 + add);
          let index = entry.index();
          let res = match protocol {
            Protocol::TCPListen   => {
              let mut tcp = self.tcp.take();
              if let Some(mut configuration) = tcp {
                let res1 = configuration.accept(token, &mut self.poll, client_token).map(|(client, should_connect)| {
                  entry.insert(client);
                  (index, should_connect)
                });
                self.tcp = Some(configuration);
                Some(res1)
              } else {
                None
              }
            },
            Protocol::HTTPListen  => {
              let mut http = self.http.take();
              if let Some(mut configuration) = http {
                let res1 = configuration.accept(token, &mut self.poll, client_token).map(|(client, should_connect)| {
                  entry.insert(client);
                  (index, should_connect)
                });
                self.http = Some(configuration);
                Some(res1)
              } else {
                None
              }
            },
            Protocol::HTTPSListen => {
              let mut https = self.https.take();
              if let Some(mut configuration) = https {
                let res1 = configuration.accept(token, &mut self.poll, client_token).map(|(client, should_connect)| {
                  entry.insert(client);
                  (index, should_connect)
                });
                self.https = Some(configuration);
                Some(res1)
              } else {
                None
              }
            },
            _ => panic!("should not call accept() on a HTTP, HTTPS or TCP client"),
          };
          res.expect("FIXME")
        }
      }
    };

    match res {
      Err(AcceptError::IoError) => {
        //FIXME: do we stop accepting?
        false
      },
      Err(AcceptError::TooManyClients) => {
        self.can_accept = false;
        false
      },
      Err(AcceptError::WouldBlock) => {
        self.accept_ready.remove(&token);
        false
      },
      Ok((client_token, should_connect)) => {
        if should_connect {
           self.connect_to_backend(client_token);
        }

        true
      }
    }
  }

  pub fn connect_to_backend(&mut self, token: ClientToken) {
    let add = self.from_client_add();
    let res = {
      let cl = self.clients[token].clone();
      let cl2: Rc<RefCell<ProxyClient>> = self.clients[token].clone();
      let protocol = { cl.borrow().protocol() };
      let entry = self.clients.vacant_entry().expect("FIXME");
      let entry = entry.insert(cl);
      let back_token = Token(entry.index().0 + add);

      let res = match protocol {
        Protocol::TCP   => {
          if let Some(mut tcp) = self.tcp.take() {
            let mut b = cl2.borrow_mut();
            let client: &mut tcp::Client = b.as_tcp() ;
            let r = tcp.connect_to_backend(&mut self.poll, client, back_token);
            self.tcp = Some(tcp);
            r
          } else {
            Err(ConnectionError::HostNotFound)
          }
        },
        Protocol::HTTP  => {
          if let Some(mut http) = self.http.take() {
            let mut b = cl2.borrow_mut();
            let client: &mut http::Client = b.as_http() ;
            let r = http.connect_to_backend(&mut self.poll, client, back_token);
            self.http = Some(http);
            r
          } else {
            Err(ConnectionError::HostNotFound)
          }
        },
        Protocol::HTTPS => {
          if let Some(mut https) = self.https.take() {
            let mut b = cl2.borrow_mut();
            let client: &mut https::TlsClient = b.as_https() ;
            let r = https.connect_to_backend(&mut self.poll, client, back_token);
            self.https = Some(https);
            r
          } else {
            Err(ConnectionError::HostNotFound)
          }
        },
        _ => {
          panic!("should not call connect_to_backend on listeners");
          Err(ConnectionError::HostNotFound)
        },
      };
 
      res.map(|action| {
        if action == BackendConnectAction::Replace {
          entry.remove();
        }
        action
      })
    };

    match res {
      Ok(BackendConnectAction::Reuse) => {
        debug!("keepalive, reusing backend connection");
      }
      Ok(BackendConnectAction::Replace) => {
      },
      Ok(BackendConnectAction::New) => {
      },
      Err(ConnectionError::HostNotFound) | Err(ConnectionError::NoBackendAvailable) | Err(ConnectionError::HttpsRedirect) => {
      },
      _ => self.close_client(token),
    }
  }

  pub fn interpret_client_order(&mut self, token: ClientToken, order: ClientResult) {
    //println!("INTERPRET ORDER: {:?}", order);
    match order {
      ClientResult::CloseClient     => self.close_client(token),
      //FIXME: we do not deregister in close_backend
      ClientResult::CloseBackend(opt) => {
        if let Some(token) = opt {
          let cl = self.to_client(token);
          if let Some(client) = self.clients.remove(cl) {
            let protocol = client.borrow().protocol();
            let res = client.borrow_mut().close_backend(token, &mut self.poll);
            if let Some((app_id, address)) = res {
              match protocol {
                Protocol::TCP   => self.tcp.as_mut().map(|c| {
                  c.close_backend(app_id, &address);
                }),
                Protocol::HTTP  => self.http.as_mut().map(|c| {
                  c.close_backend(app_id, &address);
                }),
                Protocol::HTTPS => self.https.as_mut().map(|c| {
                  c.close_backend(app_id, &address);
                }),
                _ => {
                  panic!("should not call interpret_client_order on a listen socket");
                }
              };
            }
          }
        }
      },
      ClientResult::ConnectBackend  => self.connect_to_backend(token),
      ClientResult::Continue        => {}
    }
  }

  pub fn ready(&mut self, token: Token, events: Ready) {
    //info!("PROXY\t{:?} got events: {:?}", token, events);

    let client_token = ClientToken(token.0);
    if self.clients.contains(client_token) {
      info!("clients contains {:?}", client_token);
      let protocol = self.clients[client_token].borrow().protocol();
      info!("protocol: {:?}", protocol);
      match protocol {
        Protocol::HTTPListen | Protocol::HTTPSListen | Protocol::TCPListen => {
          info!("PROTOCOL IS LISTEN");
          if events.is_readable() {
            self.accept_ready.insert(ListenToken(token.0));
            loop {
              info!("will accept");
              if !self.accept(ListenToken(token.0), protocol) {
                break;
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
    }

    loop {
    //self.client_ready(poll, client_token, events);
      let order = self.clients[client_token].borrow_mut().ready();
      //info!("client[{:?} -> {:?}] got events {:?} and returned order {:?}", client_token, self.from_client(client_token), events, order);
      //FIXME: the CloseBackend message might not mean we have nothing else to do
      //with that client
      let is_connect = order == ClientResult::ConnectBackend;
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

  pub fn handle_remaining_readiness(&mut self) {
    // try to accept again after handling all client events,
    // since we might have released a few client slots
    if !self.accept_ready.is_empty() && self.can_accept {
      loop {
        if let Some(token) = self.accept_ready.iter().next().map(|token| ListenToken(token.0)) {
          let protocol = self.clients[ClientToken(token.0)].borrow().protocol();
          if !self.accept(token, protocol) {
            if !self.accept_ready.is_empty() && self.can_accept {
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
}

pub struct ListenClient {
  pub protocol: Protocol,
}

impl ProxyClient for ListenClient {
  fn protocol(&self) -> Protocol {
    self.protocol
  }

  fn as_http(&mut self) ->  &mut http::Client {
    panic!();
  }

  fn as_tcp(&mut self) -> &mut tcp::Client {
    panic!();
  }

  fn as_https(&mut self) -> &mut https::TlsClient {
    panic!();
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
}
