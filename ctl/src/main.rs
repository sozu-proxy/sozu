#[macro_use] extern crate prettytable;
extern crate rand;
extern crate sozu_command_lib as sozu_command;
extern crate structopt;
#[macro_use] extern crate structopt_derive;
extern crate serde;
extern crate serde_json;
#[macro_use] extern crate serde_derive;
extern crate hex;

mod command;
mod cli;

use std::io;
use structopt::StructOpt;

use sozu_command::config::Config;
use sozu_command::channel::Channel;
use sozu_command::command::{CommandRequest,CommandResponse};
use sozu_command::proxy::ListenerType;

use command::{add_application,remove_application,dump_state,load_state,
  save_state, soft_stop, hard_stop, upgrade_master, status,metrics,
  remove_backend, add_backend, remove_http_frontend, add_http_frontend,
  remove_tcp_frontend, add_tcp_frontend, add_certificate, remove_certificate,
  replace_certificate, query_application, logging_filter, upgrade_worker,
  events, query_certificate, add_tcp_listener, add_http_listener, add_https_listener,
  remove_listener, activate_listener, deactivate_listener};

use cli::*;

fn main() {
  let matches = App::from_args();

  let config_file = matches.config.or(option_env!("SOZU_CONFIG").map(|s| s.to_string())).expect("missing --config <configuration file> option");

  let config  = Config::load_from_path(config_file.as_str()).expect("could not parse configuration file");

  // If the command is `config check` then exit because if we are here, the configuration is valid
  if let SubCmd::Config{ cmd: ConfigCmd::Check{} } = matches.cmd {
    println!("Configuration file is valid");
    std::process::exit(0);
  }

  let channel = create_channel(&config).expect("could not connect to the command unix socket");
  let timeout: u64 = matches.timeout.unwrap_or(config.ctl_command_timeout);

  match matches.cmd {
    SubCmd::Shutdown{ hard, worker} => {
      if hard {
        hard_stop(channel, worker, timeout);
      } else {
        soft_stop(channel, worker);
      }
    },
    SubCmd::Upgrade { worker: None } => upgrade_master(channel, &config),
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
        BackendCmd::Add{ id, backend_id, address, sticky_id, backup } => add_backend(channel, timeout, &id, &backend_id, address, sticky_id, backup),
        BackendCmd::Remove{ id, backend_id, address } => remove_backend(channel, timeout, &id, &backend_id, address),
      }
    },
    SubCmd::Frontend{ cmd } => {
      match cmd {
        FrontendCmd::Http{ cmd } => match cmd {
          HttpFrontendCmd::Add{ id, hostname, path_begin, address } => {
            add_http_frontend(channel, timeout, &id, address, &hostname, &path_begin.unwrap_or("".to_string()), false)
          },
          HttpFrontendCmd::Remove{ id, hostname, path_begin, address } => {
            remove_http_frontend(channel, timeout, &id, address, &hostname, &path_begin.unwrap_or("".to_string()), false)
          },
        },
        FrontendCmd::Https{ cmd } => match cmd {
          HttpFrontendCmd::Add{ id, hostname, path_begin, address } => {
            add_http_frontend(channel, timeout, &id, address, &hostname, &path_begin.unwrap_or("".to_string()), true)
          },
          HttpFrontendCmd::Remove{ id, hostname, path_begin, address } => {
            remove_http_frontend(channel, timeout, &id, address, &hostname, &path_begin.unwrap_or("".to_string()), true)
          },
        },
        FrontendCmd::Tcp { cmd } => match cmd {
          TcpFrontendCmd::Add{ id, address } =>
            add_tcp_frontend(channel, timeout, &id, address),
          TcpFrontendCmd::Remove{ id, address } =>
            remove_tcp_frontend(channel, timeout, &id, address),
        }
      }
    },
    SubCmd::Listener{ cmd } => {
      match cmd {
        ListenerCmd::Http { cmd } => match cmd {
          HttpListenerCmd::Add { address, public_address, answer_404, answer_503, expect_proxy, sticky_name } => {
            add_http_listener(channel, timeout, address, public_address, answer_404, answer_503, expect_proxy, sticky_name)
          },
          HttpListenerCmd::Remove { address } => remove_listener(channel, timeout, address, ListenerType::HTTP),
          HttpListenerCmd::Activate{ address } => activate_listener(channel, timeout, address, ListenerType::HTTP),
          HttpListenerCmd::Deactivate{ address } => deactivate_listener(channel, timeout, address, ListenerType::HTTP)
        },
        ListenerCmd::Https { cmd } => match cmd {
          HttpsListenerCmd::Add { address, public_address, answer_404, answer_503, tls_versions, cipher_list,
            rustls_cipher_list, expect_proxy, sticky_name } => {
            add_https_listener(channel, timeout, address, public_address, answer_404, answer_503, tls_versions, cipher_list, rustls_cipher_list, expect_proxy, sticky_name)
          },
          HttpsListenerCmd::Remove { address } => remove_listener(channel, timeout, address, ListenerType::HTTPS),
          HttpsListenerCmd::Activate{ address } => activate_listener(channel, timeout, address, ListenerType::HTTPS),
          HttpsListenerCmd::Deactivate{ address } => deactivate_listener(channel, timeout, address, ListenerType::HTTPS),
        },
        ListenerCmd::Tcp { cmd } => match cmd {
          TcpListenerCmd::Add{ address, public_address, expect_proxy } => add_tcp_listener(channel, timeout, address, public_address, expect_proxy),
          TcpListenerCmd::Remove{ address } => remove_listener(channel, timeout, address, ListenerType::TCP),
          TcpListenerCmd::Activate{ address } => activate_listener(channel, timeout, address, ListenerType::TCP),
          TcpListenerCmd::Deactivate{ address } => deactivate_listener(channel, timeout, address, ListenerType::TCP)
        }
      }
    }
    SubCmd::Certificate{ cmd } => {
      match cmd {
        CertificateCmd::Add{ certificate, chain, key, address } => {
          add_certificate(channel, timeout, address, &certificate, &chain, &key)
        },
        CertificateCmd::Remove{ certificate, address } => {
          remove_certificate(channel, timeout, address, &certificate)
        },
        CertificateCmd::Replace{ certificate, chain, key, old_certificate, address } => {
          replace_certificate(channel, timeout, address, &certificate, &chain, &key, &old_certificate)
        },
      }
    },
    SubCmd::Query{ cmd, json } => {
      match cmd {
        QueryCmd::Applications{ id, domain } => query_application(channel, json, id, domain),
        QueryCmd::Certificates{ fingerprint, domain } => query_certificate(channel, json, fingerprint, domain),
      }
    },
    SubCmd::Config{ cmd: _ } => {}, // noop, handled at the beginning of the method
    SubCmd::Events => events(channel),
  }
}

pub fn create_channel(config: &Config) -> Result<Channel<CommandRequest,CommandResponse>,io::Error> {
  Channel::from_path(&config.command_socket_path(), config.command_buffer_size, config.max_command_buffer_size)
    .and_then(|mut channel| {
      channel.set_nonblocking(false);
      Ok(channel)
    })
}
