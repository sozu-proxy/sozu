use std::sync::mpsc::{channel};
use std::thread::{self,JoinHandle};
use sozu::network::{self,ProxyOrder};
use mio::channel;

use command::Listener;
use command::data::ListenerType;
use config::ListenerConfig;

pub fn start_workers(tag: &str, ls: &ListenerConfig) -> Option<(JoinHandle<()>, Vec<Listener>)> {
  match ls.listener_type {
    ListenerType::HTTP => {
      //FIXME: make safer
      if let Some(conf) = ls.to_http() {
        let mut http_listeners = Vec::new();

        for index in 1..ls.worker_count.unwrap_or(1) {
          let (sender, receiver) = channel::<network::ServerMessage>();
          let (tx, rx) = channel::channel::<ProxyOrder>();
          let config = conf.clone();
          let t = format!("{}-{}", tag, index);
          thread::spawn(move || {
            network::http::start_listener(t, config, sender, rx);
          });
          let l =  Listener::new(tag.to_string(), index as u8, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
          http_listeners.push(l);
        }

        let (sender, receiver) = channel::<network::ServerMessage>();
        let (tx, rx) = channel::channel::<ProxyOrder>();
        let t = format!("{}-{}", tag, 0);
        //FIXME: keep this to get a join guard
        let jg = thread::spawn(move || {
          network::http::start_listener(t, conf, sender, rx);
        });

        let l =  Listener::new(tag.to_string(), 0, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
        http_listeners.push(l);
        Some((jg, http_listeners))
      } else {
        None
      }
    },
    ListenerType::HTTPS => {
      if let Some(conf) = ls.to_tls() {
        let mut tls_listeners = Vec::new();

        for index in 1..ls.worker_count.unwrap_or(1) {
          let (sender, receiver) = channel::<network::ServerMessage>();
          let (tx, rx) = channel::channel::<ProxyOrder>();
          let config = conf.clone();
          let t = format!("{}-{}", tag, index);
          thread::spawn(move || {
            network::tls::start_listener(t, config, sender, rx);
          });

          let l =  Listener::new(tag.to_string(), index as u8, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
          tls_listeners.push(l);
        }

        let (sender, receiver) = channel::<network::ServerMessage>();
        let (tx, rx) = channel::channel::<ProxyOrder>();
          let t = format!("{}-{}", tag, 0);
        //FIXME: keep this to get a join guard
        let jg = thread::spawn(move || {
          network::tls::start_listener(t, conf, sender, rx);
        });

        let l =  Listener::new(tag.to_string(), 0, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
        tls_listeners.push(l);
        Some((jg, tls_listeners))
      } else {
        None
      }
    },
    _ => unimplemented!()
  }
}
