#[macro_use] extern crate prettytable;
extern crate rand;
extern crate sozu_command_lib as sozu_command;
extern crate structopt;
#[macro_use] extern crate structopt_derive;
extern crate serde;
extern crate serde_json;
#[macro_use] extern crate serde_derive;

mod command;
mod cli;

use std::io;
use structopt::StructOpt;

use sozu_command::config::Config;
use sozu_command::channel::Channel;
use sozu_command::data::{ConfigMessage,ConfigMessageAnswer};

use command::{add_application,remove_application,dump_state,load_state,
  save_state, soft_stop, hard_stop, upgrade_master, status,metrics,
  remove_backend, add_backend, remove_http_frontend, add_http_frontend,
  remove_tcp_frontend, add_tcp_frontend, add_certificate, remove_certificate,
  replace_certificate, query_application, logging_filter, upgrade_worker};

use cli::*;

fn main() {
  let matches = App::from_args();

  let config_file = matches.config;

  let config  = Config::load_from_path(config_file.as_str()).expect("could not parse configuration file");
  let channel = create_channel(&config.command_socket_path()).expect("could not connect to the command unix socket");
  let timeout: u64 = matches.timeout.unwrap_or(config.ctl_command_timeout);

  match matches.cmd {
    SubCmd::Shutdown{ hard } => {
      if hard {
        hard_stop(channel, timeout);
      } else {
        soft_stop(channel);
      }
    },
    SubCmd::Upgrade { worker: None } => upgrade_master(channel, &config.command_socket_path()),
    SubCmd::Upgrade { worker: Some(id) } => { upgrade_worker(channel, timeout, id); },
    SubCmd::Status{ json } => status(channel, json),
    SubCmd::Metrics{ json } => metrics(channel, json),
    SubCmd::Logging{ level } => logging_filter(channel, timeout, &level),
    SubCmd::State{ cmd } => {
      match cmd {
        StateCmd::Save{ file } => save_state(channel, timeout, file),
        StateCmd::Load{ file } => load_state(channel, timeout, file),
        StateCmd::Dump{ json } => dump_state(channel, timeout, json),
      }
    },
    SubCmd::Application{ cmd } => {
      match cmd {
        ApplicationCmd::Add{ id, sticky_session, https_redirect, send_proxy, expect_proxy, load_balancing_policy } => add_application(channel, timeout, &id, sticky_session, https_redirect, send_proxy, expect_proxy, load_balancing_policy),
        ApplicationCmd::Remove{ id } => remove_application(channel, timeout, &id),
      }
    },
    SubCmd::Backend{ cmd } => {
      match cmd {
        BackendCmd::Add{ id, backend_id, ip, port, sticky_id, backup } => add_backend(channel, timeout, &id, &backend_id, &ip, port, sticky_id, backup),
        BackendCmd::Remove{ id, backend_id, ip, port } => remove_backend(channel, timeout, &id, &backend_id, &ip, port),
      }
    },
    SubCmd::Frontend{ cmd } => {
      match cmd {
        FrontendCmd::Http{ cmd } => match cmd {
          HttpFrontendCmd::Add{ id, hostname, path_begin, path_to_certificate } =>
            add_http_frontend(channel, timeout, &id, &hostname, &path_begin.unwrap_or("".to_string()), path_to_certificate),
          HttpFrontendCmd::Remove{ id, hostname, path_begin, path_to_certificate } =>
            remove_http_frontend(channel, timeout, &id, &hostname, &path_begin.unwrap_or("".to_string()), path_to_certificate),
        },
        FrontendCmd::Tcp { cmd } => match cmd {
          TcpFrontendCmd::Add{ id, ip_address, port } =>
            add_tcp_frontend(channel, timeout, &id, &ip_address, port),
          TcpFrontendCmd::Remove{ id, ip_address, port } =>
            remove_tcp_frontend(channel, timeout, &id, &ip_address, port),
        }
      }
    },
    SubCmd::Certificate{ cmd } => {
      match cmd {
        CertificateCmd::Add{ certificate, chain, key } => add_certificate(channel, timeout, &certificate, &chain, key.unwrap_or("missing key path".to_string()).as_str()),
        CertificateCmd::Remove{ certificate } => remove_certificate(channel, timeout, &certificate),
        CertificateCmd::Replace{ certificate, chain, key, old_certificate } => replace_certificate(channel, timeout, &certificate, &chain, &key.unwrap_or("missing key path".to_string()).as_str(), &old_certificate)
      }
    },
    SubCmd::Query{ cmd, json } => {
      match cmd {
        QueryCmd::Applications{ id, domain } => query_application(channel, json, id, domain),
      }
    },
  }
}

pub fn create_channel(path: &str) -> Result<Channel<ConfigMessage,ConfigMessageAnswer>,io::Error> {
  Channel::from_path(path, 10_000, 2_000_000)
    .and_then(|mut channel| {
      channel.set_nonblocking(false);
      Ok(channel)
    })
}
