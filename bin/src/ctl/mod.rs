mod command;

use crate::{
    cli::{self, *},
    get_config_file_path, load_configuration, util,
};
use anyhow::Context;
use sozu_command_lib::{
    channel::Channel,
    command::{CommandRequest, CommandResponse},
    config::Config,
    proxy::ListenerType,
};
use std::time::Duration;

pub struct CommandManager {
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: Duration,
    config: Config,
}

pub fn ctl(matches: cli::Sozu) -> Result<(), anyhow::Error> {
    let config_file_path = get_config_file_path(&matches)?;
    let config = load_configuration(config_file_path)?;

    util::setup_logging(&config);

    // If the command is `config check` then exit because if we are here, the configuration is valid
    if let SubCmd::Config {
        cmd: ConfigCmd::Check {},
    } = matches.cmd
    {
        println!("Configuration file is valid");
        std::process::exit(0);
    }

    let channel = create_channel(&config).with_context(|| {
        "could not connect to the command unix socket. Are you sure the proxy is up?"
    })?;

    let timeout = Duration::from_millis(matches.timeout.unwrap_or(config.ctl_command_timeout));

    let mut command_manager = CommandManager {
        channel,
        timeout,
        config,
    };
    command_manager.handle_command(matches.cmd)
}

impl CommandManager {
    fn handle_command(&mut self, command: SubCmd) -> anyhow::Result<()> {
        match command {
            SubCmd::Shutdown { hard, worker } => {
                if hard {
                    self.hard_stop(worker)
                } else {
                    self.soft_stop(worker)
                }
            }
            SubCmd::Upgrade { worker: None } => self.upgrade_main(),
            SubCmd::Upgrade { worker: Some(id) } => self.upgrade_worker(id),
            SubCmd::Status { json } => self.status(json),
            SubCmd::Metrics { cmd } => self.metrics(cmd),
            SubCmd::Logging { level } => self.logging_filter(&level),
            SubCmd::State { cmd } => match cmd {
                StateCmd::Save { file } => self.save_state(file),
                StateCmd::Load { file } => self.load_state(file),
                StateCmd::Dump { json } => self.dump_state(json),
            },
            SubCmd::Reload { file, json } => self.reload_configuration(file, json),
            SubCmd::Application { cmd } => match cmd {
                ApplicationCmd::Add {
                    id,
                    sticky_session,
                    https_redirect,
                    send_proxy,
                    expect_proxy,
                    load_balancing_policy,
                } => self.add_application(
                    &id,
                    sticky_session,
                    https_redirect,
                    send_proxy,
                    expect_proxy,
                    load_balancing_policy,
                ),
                ApplicationCmd::Remove { id } => self.remove_application(&id),
            },
            SubCmd::Backend { cmd } => match cmd {
                BackendCmd::Add {
                    id,
                    backend_id,
                    address,
                    sticky_id,
                    backup,
                } => self.add_backend(&id, &backend_id, address, sticky_id, backup),
                BackendCmd::Remove {
                    id,
                    backend_id,
                    address,
                } => self.remove_backend(&id, &backend_id, address),
            },
            SubCmd::Frontend { cmd } => match cmd {
                FrontendCmd::Http { cmd } => self.http_frontend_command(cmd),
                FrontendCmd::Https { cmd } => self.https_frontend_command(cmd),
                FrontendCmd::Tcp { cmd } => match cmd {
                    TcpFrontendCmd::Add { id, address } => self.add_tcp_frontend(&id, address),
                    TcpFrontendCmd::Remove { id, address } => {
                        self.remove_tcp_frontend(&id, address)
                    }
                },
                FrontendCmd::List {
                    http,
                    https,
                    tcp,
                    domain,
                } => self.list_frontends(http, https, tcp, domain),
            },
            SubCmd::Listener { cmd } => match cmd {
                ListenerCmd::Http { cmd } => match cmd {
                    HttpListenerCmd::Add {
                        address,
                        public_address,
                        answer_404,
                        answer_503,
                        expect_proxy,
                        sticky_name,
                    } => self.add_http_listener(
                        address,
                        public_address,
                        answer_404,
                        answer_503,
                        expect_proxy,
                        sticky_name,
                    ),
                    HttpListenerCmd::Remove { address } => {
                        self.remove_listener(address, ListenerType::HTTP)
                    }
                    HttpListenerCmd::Activate { address } => {
                        self.activate_listener(address, ListenerType::HTTP)
                    }
                    HttpListenerCmd::Deactivate { address } => {
                        self.deactivate_listener(address, ListenerType::HTTP)
                    }
                },
                ListenerCmd::Https { cmd } => match cmd {
                    HttpsListenerCmd::Add {
                        address,
                        public_address,
                        answer_404,
                        answer_503,
                        tls_versions,
                        cipher_list,
                        rustls_cipher_list,
                        expect_proxy,
                        sticky_name,
                    } => self.add_https_listener(
                        address,
                        public_address,
                        answer_404,
                        answer_503,
                        tls_versions,
                        cipher_list,
                        rustls_cipher_list,
                        expect_proxy,
                        sticky_name,
                    ),
                    HttpsListenerCmd::Remove { address } => {
                        self.remove_listener(address, ListenerType::HTTPS)
                    }
                    HttpsListenerCmd::Activate { address } => {
                        self.activate_listener(address, ListenerType::HTTPS)
                    }
                    HttpsListenerCmd::Deactivate { address } => {
                        self.deactivate_listener(address, ListenerType::HTTPS)
                    }
                },
                ListenerCmd::Tcp { cmd } => match cmd {
                    TcpListenerCmd::Add {
                        address,
                        public_address,
                        expect_proxy,
                    } => self.add_tcp_listener(address, public_address, expect_proxy),
                    TcpListenerCmd::Remove { address } => {
                        self.remove_listener(address, ListenerType::TCP)
                    }
                    TcpListenerCmd::Activate { address } => {
                        self.activate_listener(address, ListenerType::TCP)
                    }
                    TcpListenerCmd::Deactivate { address } => {
                        self.deactivate_listener(address, ListenerType::TCP)
                    }
                },
            },
            SubCmd::Certificate { cmd } => match cmd {
                CertificateCmd::Add {
                    certificate,
                    chain,
                    key,
                    address,
                    tls_versions,
                } => self.add_certificate(address, &certificate, &chain, &key, tls_versions),
                CertificateCmd::Remove {
                    certificate,
                    address,
                    fingerprint,
                } => {
                    self.remove_certificate(address, certificate.as_deref(), fingerprint.as_deref())
                }
                CertificateCmd::Replace {
                    certificate,
                    chain,
                    key,
                    old_certificate,
                    address,
                    old_fingerprint,
                    tls_versions,
                } => self.replace_certificate(
                    address,
                    &certificate,
                    &chain,
                    &key,
                    old_certificate.as_deref(),
                    old_fingerprint.as_deref(),
                    tls_versions,
                ),
            },
            SubCmd::Query { cmd, json } => match cmd {
                QueryCmd::Applications { id, domain } => self.query_application(json, id, domain),
                QueryCmd::Certificates {
                    fingerprint,
                    domain,
                } => self.query_certificate(json, fingerprint, domain),
                QueryCmd::Metrics {
                    list,
                    refresh,
                    names,
                    clusters,
                    backends,
                } => self.query_metrics(json, list, refresh, names, clusters, backends),
            },
            SubCmd::Config { cmd: _ } => Ok(()), // noop, handled at the beginning of the method
            SubCmd::Events => self.events(),
            rest => {
                panic!("that command should have been handled earlier: {:x?}", rest)
            }
        }
    }
}

pub fn create_channel(config: &Config) -> anyhow::Result<Channel<CommandRequest, CommandResponse>> {
    let mut channel = Channel::from_path(
        &config.command_socket_path()?,
        config.command_buffer_size,
        config.max_command_buffer_size,
    )
    .with_context(|| "Could not create Channel from the given path")?;

    channel.set_nonblocking(false);
    Ok(channel)
}
