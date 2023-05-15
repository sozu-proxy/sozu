use std::time::Duration;

use anyhow::Context;
use sozu_command_lib::{
    channel::Channel,
    config::Config,
    proto::command::{Request, Response},
};

use crate::{
    cli::{self, *},
    get_config_file_path, load_configuration, util,
};

mod command;
/// TODO: just create a display() method on sozu_command_lib::Response and put everything in there
mod display;
mod request_builder;

pub struct CommandManager {
    channel: Channel<Request, Response>,
    timeout: Duration,
    config: Config,
}

pub fn ctl(args: cli::Args) -> anyhow::Result<()> {
    let config_file_path = get_config_file_path(&args)?;
    let config = load_configuration(config_file_path)?;

    util::setup_logging(&config, "CTL");

    // If the command is `config check` then exit because if we are here, the configuration is valid
    if let SubCmd::Config {
        cmd: ConfigCmd::Check,
    } = args.cmd
    {
        println!("Configuration file is valid");
        std::process::exit(0);
    }

    let channel = create_channel(&config).with_context(|| {
        "could not connect to the command unix socket. Are you sure the proxy is up?"
    })?;

    let timeout = Duration::from_millis(args.timeout.unwrap_or(config.ctl_command_timeout));

    let mut command_manager = CommandManager {
        channel,
        timeout,
        config,
    };

    command_manager.handle_command(args.cmd)
}

impl CommandManager {
    fn handle_command(&mut self, command: SubCmd) -> anyhow::Result<()> {
        match command {
            SubCmd::Shutdown { hard } => {
                if hard {
                    self.hard_stop()
                } else {
                    self.soft_stop()
                }
            }
            SubCmd::Upgrade { worker } => match worker {
                None => self.upgrade_main(),
                Some(worker_id) => self.upgrade_worker(worker_id),
            },
            SubCmd::Status { json } => self.status(json),
            SubCmd::Metrics { cmd, json } => match cmd {
                MetricsCmd::Get {
                    list,
                    refresh,
                    names,
                    clusters,
                    backends,
                } => self.get_metrics(json, list, refresh, names, clusters, backends),
                _ => self.configure_metrics(cmd),
            },
            SubCmd::Logging { level } => self.logging_filter(&level),
            SubCmd::State { cmd } => match cmd {
                StateCmd::Save { file } => self.save_state(file),
                StateCmd::Load { file } => self.load_state(file),
            },
            SubCmd::Reload { file, json } => self.reload_configuration(file, json),
            SubCmd::Cluster { cmd, json } => self.cluster_command(cmd, json),
            SubCmd::Backend { cmd } => self.backend_command(cmd),
            SubCmd::Frontend { cmd } => match cmd {
                FrontendCmd::Http { cmd } => self.http_frontend_command(cmd),
                FrontendCmd::Https { cmd } => self.https_frontend_command(cmd),
                FrontendCmd::Tcp { cmd } => self.tcp_frontend_command(cmd),
                FrontendCmd::List {
                    http,
                    https,
                    tcp,
                    domain,
                } => self.list_frontends(http, https, tcp, domain),
            },
            SubCmd::Listener { cmd } => match cmd {
                ListenerCmd::Http { cmd } => self.http_listener_command(cmd),
                ListenerCmd::Https { cmd } => self.https_listener_command(cmd),
                ListenerCmd::Tcp { cmd } => self.tcp_listener_command(cmd),
                ListenerCmd::List => self.list_listeners(),
            },
            SubCmd::Certificate { cmd, json } => match cmd {
                CertificateCmd::Get {
                    fingerprint,
                    domain,
                } => self.query_certificates(json, fingerprint, domain),
                CertificateCmd::Add {
                    certificate,
                    chain,
                    key,
                    address,
                    tls_versions,
                } => self.add_certificate(
                    address.to_string(),
                    &certificate,
                    &chain,
                    &key,
                    tls_versions,
                ),
                CertificateCmd::Remove {
                    certificate,
                    address,
                    fingerprint,
                } => self.remove_certificate(
                    address.to_string(),
                    certificate.as_deref(),
                    fingerprint.as_deref(),
                ),
                CertificateCmd::Replace {
                    certificate,
                    chain,
                    key,
                    old_certificate,
                    address,
                    old_fingerprint,
                    tls_versions,
                } => self.replace_certificate(
                    address.to_string(),
                    &certificate,
                    &chain,
                    &key,
                    old_certificate.as_deref(),
                    old_fingerprint.as_deref(),
                    tls_versions,
                ),
            },
            SubCmd::Config { cmd: _ } => Ok(()), // noop, handled at the beginning of the method
            SubCmd::Events => self.events(),
            rest => {
                panic!("that command should have been handled earlier: {rest:x?}")
            }
        }
    }
}

/// creates a blocking channel
pub fn create_channel(config: &Config) -> anyhow::Result<Channel<Request, Response>> {
    let mut channel = Channel::from_path(
        &config.command_socket_path()?,
        config.command_buffer_size,
        config.max_command_buffer_size,
    )
    .with_context(|| "Could not create Channel from the given path")?;

    channel
        .blocking()
        .with_context(|| "Could not block the channel used to communicate with Sōzu")?;
    Ok(channel)
}
