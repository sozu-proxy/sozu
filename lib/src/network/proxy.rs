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
use sozu_command::state::ConfigState;
use sozu_command::messages::{self,TcpFront,Order,Instance,MessageId,OrderMessageAnswer,OrderMessageAnswerData,OrderMessageStatus,OrderMessage,Topic,Query,QueryAnswer};

use network::buffer_queue::BufferQueue;
use network::{ClientResult,ConnectionError,
  SocketType,Protocol,RequiredEvents};
use network::{http,tls,tcp};
use network::metrics::METRICS;
use network::session::{BackToken,FrontToken,ListenToken,ProxyClient,ProxyConfiguration,Readiness,Session};

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

pub struct Server {
  pub poll:        Poll,
  shutting_down:   Option<MessageId>,
  accept_ready:    HashSet<ListenToken>,
  can_accept:      bool,
  channel:         ProxyChannel,
  queue:           VecDeque<OrderMessageAnswer>,
  http:            Option<Session<http::ServerConfiguration, http::Client>>,
  https:           Option<Session<tls::ServerConfiguration, tls::TlsClient>>,
  tcp:             Option<Session<tcp::ServerConfiguration, tcp::Client>>,
  config_state:    ConfigState,
}

impl Server {
  pub fn new_from_config(channel: ProxyChannel, config: Config, config_state: ConfigState) -> Self {
    let mut event_loop  = Poll::new().expect("could not create event loop");
    let pool = Rc::new(RefCell::new(
      Pool::with_capacity(2*config.max_buffers, 0, || BufferQueue::with_capacity(config.buffer_size))
    ));

    let max_connections = config.max_connections;
    let max_buffers     = config.max_buffers;
    let http_session = config.http.and_then(|conf| conf.to_http()).and_then(|http_conf| {
      let max_listeners = 1;
      http::ServerConfiguration::new(http_conf, &mut event_loop, 1 + max_listeners, pool.clone()).map(|configuration| {
        Session::new(1, max_connections, 0, configuration, &mut event_loop)
      }).ok()
    });

    let https_session = config.https.and_then(|conf| conf.to_tls()).and_then(|https_conf| {
      let max_listeners   = 1;
      tls::ServerConfiguration::new(https_conf, 6148914691236517205, &mut event_loop, 1 + max_listeners + 6148914691236517205, pool.clone()).map(|configuration| {
        Session::new(max_listeners, max_connections, 6148914691236517205, configuration, &mut event_loop)
      }).ok()
    });

    let tcp_session = config.tcp.map(|conf| {
      let configuration = tcp::ServerConfiguration::new(conf.max_listeners, 12297829382473034410, pool.clone());
      Session::new(conf.max_listeners, max_buffers, 12297829382473034410, configuration, &mut event_loop)
    });

    Server::new(event_loop, channel, http_session, https_session, tcp_session, Some(config_state))
  }

  pub fn new(poll: Poll, channel: ProxyChannel,
    http:  Option<Session<http::ServerConfiguration, http::Client>>,
    https: Option<Session<tls::ServerConfiguration, tls::TlsClient>>,
    tcp:  Option<Session<tcp::ServerConfiguration, tcp::Client>>,
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
    loop {
      self.poll.poll(&mut events, poll_timeout).expect("should be able to poll for events");

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
          //self.ready(event.token(), event.readiness());
          match proxy_type(event.token().0) {
            ProxyType::HTTP  => if let Some(mut http) = self.http.take() {
              http.ready(&mut self.poll, event.token(), event.readiness());
              self.http = Some(http);
            },
            ProxyType::HTTPS => if let Some(mut https) = self.https.take() {
              https.ready(&mut self.poll, event.token(), event.readiness());
              self.https = Some(https);
            },
            ProxyType::TCP   => if let Some(mut tcp) = self.tcp.take() {
              tcp.ready(&mut self.poll, event.token(), event.readiness());
              self.tcp = Some(tcp);
            },
          };
        }
      }

      if let Some(mut http) = self.http.take() {
        http.handle_remaining_readiness(&mut self.poll);
        self.http = Some(http);
      }
      if let Some(mut https) = self.https.take() {
        https.handle_remaining_readiness(&mut self.poll);
        self.https = Some(https);
      }
      if let Some(mut tcp) = self.tcp.take() {
        tcp.handle_remaining_readiness(&mut self.poll);
        self.tcp = Some(tcp);
      }

      METRICS.with(|metrics| {
        (*metrics.borrow_mut()).send_data();
      });

      if self.shutting_down.is_some() {
        info!("last client stopped, shutting down!");
        self.channel.write_message(&OrderMessageAnswer{ id: self.shutting_down.take().expect("should have shut down correctly"), status: OrderMessageStatus::Ok, data: None});
        self.channel.run();
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
            (*metrics.borrow()).dump_metrics_data()
          ))
        });
      });
      return;
    }

    if let Order::Query(ref query) = message.order {
      match query {
        &Query::Applications => {
          self.queue.push_back(OrderMessageAnswer {
            id:     message.id.clone(),
            status: OrderMessageStatus::Ok,
            data:   Some(OrderMessageAnswerData::Query(
              QueryAnswer::Applications(self.config_state.hash_state())
            ))
          });
        },
        &Query::Application(ref app_id) => {
          self.queue.push_back(OrderMessageAnswer {
            id:     message.id.clone(),
            status: OrderMessageStatus::Ok,
            data:   Some(OrderMessageAnswerData::Query(
              QueryAnswer::Application(self.config_state.application_state(app_id))
            ))
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
        self.queue.push_back(http.configuration().notify(&mut self.poll, message.clone()));
        self.http = Some(http);
      }
    }
    if topics.contains(&Topic::HttpsProxyConfig) {
      if let Some(mut https) = self.https.take() {
        self.queue.push_back(https.configuration().notify(&mut self.poll, message.clone()));
        self.https = Some(https);
      }
    }
    if topics.contains(&Topic::TcpProxyConfig) {
      if let Some(mut tcp) = self.tcp.take() {
        self.queue.push_back(tcp.configuration().notify(&mut self.poll, message));
        self.tcp = Some(tcp);
      }
    }
  }
}

fn proxy_type(token: usize) -> ProxyType {
  if token < 6148914691236517205 {
    ProxyType::HTTP
  } else if token < 12297829382473034410 {
    ProxyType::HTTPS
  } else {
    ProxyType::TCP
  }
}
