mod command;
mod request_builder;

use std::time::Duration;

use sozu_command_lib::{
    certificate::CertificateError,
    channel::{Channel, ChannelError},
    config::{Config, ConfigError},
    logging::setup_logging_with_config,
    proto::{
        command::{Request, Response},
        DisplayError,
    },
};

use crate::{
    cli::{self, *},
    util::{get_config_file_path, UtilError},
};

#[derive(thiserror::Error, Debug)]
pub enum CtlError {
    #[error("failed to get config: {0}")]
    GetConfig(UtilError),
    #[error("failed to load config: {0}")]
    LoadConfig(ConfigError),
    #[error("could not create channel to Sōzu. Are you sure the proxy is up?: {0}")]
    CreateChannel(ChannelError),
    #[error("failed to find the path of the command socket: {0}")]
    GetCommandSocketPath(ConfigError),
    #[error("failed to block channel to Sōzu: {0}")]
    BlockChannel(ChannelError),
    #[error("could not display response: {0}")]
    Display(DisplayError),
    #[error("could not read message on a blocking channel: {0}")]
    ReadBlocking(ChannelError),
    #[error("Request failed: {0}")]
    Failure(String),
    #[error("could not write request on channel: {0}")]
    WriteRequest(ChannelError),
    #[error("could not get certificate fingerprint")]
    GetFingerprint(CertificateError),
    #[error("could not decode fingerprint")]
    DecodeFingerprint(CertificateError),
    #[error("Please provide either one, {0} OR {1}")]
    ArgsNeeded(String, String),
    #[error("could not load certificate")]
    LoadCertificate(CertificateError),
    #[error("wrong input to create listener")]
    CreateListener(ConfigError),
    #[error("domain can not be empty")]
    NeedClusterDomain,
    #[error("wrong response from Sōzu: {0:?}")]
    WrongResponse(Response),
}

pub struct CommandManager {
    channel: Channel<Request, Response>,
    timeout: Duration,
    config: Config,
    /// wether to display the response in JSON
    json: bool,
}

pub fn ctl(args: cli::Args) -> Result<(), CtlError> {
    let config_path = get_config_file_path(&args).map_err(CtlError::GetConfig)?;

    let config = Config::load_from_path(config_path).map_err(CtlError::LoadConfig)?;

    // prevent logging for json responses for a clean output
    if !args.json {
        setup_logging_with_config(&config, "CTL");
    }

    // If the command is `config check` then exit because if we are here, the configuration is valid
    if let SubCmd::Config {
        cmd: ConfigCmd::Check,
    } = args.cmd
    {
        println!("Configuration file is valid");
        std::process::exit(0);
    }

    let channel = create_channel(&config)?;

    let timeout = Duration::from_millis(args.timeout.unwrap_or(config.ctl_command_timeout));
    if !args.json {
        debug!("applying timeout {:?}", timeout);
    }

    let mut command_manager = CommandManager {
        channel,
        timeout,
        config,
        json: args.json,
    };

    command_manager.handle_command(args.cmd)
}

impl CommandManager {
    fn handle_command(&mut self, command: SubCmd) -> Result<(), CtlError> {
        debug!("Executing command {:?}", command);
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
            SubCmd::Status {} => self.status(),
            SubCmd::Metrics { cmd } => match cmd {
                MetricsCmd::Get {
                    list,
                    refresh,
                    names,
                    clusters,
                    backends,
                    no_clusters,
                } => self.get_metrics(list, refresh, names, clusters, backends, no_clusters),
                _ => self.configure_metrics(cmd),
            },
            SubCmd::Logging { filter } => self.logging_filter(filter),
            SubCmd::State { cmd } => match cmd {
                StateCmd::Save { file } => self.save_state(file),
                StateCmd::Load { file } => self.load_state(file),
                StateCmd::Stats => self.count_requests(),
            },
            SubCmd::Reload { file } => self.reload_configuration(file),
            SubCmd::Cluster { cmd } => self.cluster_command(cmd),
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
            SubCmd::Certificate { cmd } => match cmd {
                CertificateCmd::Add {
                    certificate,
                    chain,
                    key,
                    address,
                    tls_versions,
                } => self.add_certificate(address.into(), &certificate, &chain, &key, tls_versions),
                CertificateCmd::Remove {
                    certificate,
                    address,
                    fingerprint,
                } => self.remove_certificate(
                    address.into(),
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
                    address.into(),
                    &certificate,
                    &chain,
                    &key,
                    old_certificate.as_deref(),
                    old_fingerprint.as_deref(),
                    tls_versions,
                ),
                CertificateCmd::List {
                    fingerprint,
                    domain,
                    query_workers,
                } => self.query_certificates(fingerprint, domain, query_workers),
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
pub fn create_channel(config: &Config) -> Result<Channel<Request, Response>, CtlError> {
    let command_socket_path = &config
        .command_socket_path()
        .map_err(CtlError::GetCommandSocketPath)?;

    let mut channel = Channel::from_path(
        command_socket_path,
        config.command_buffer_size,
        config.max_command_buffer_size,
    )
    .map_err(CtlError::CreateChannel)?;

    channel.blocking().map_err(CtlError::BlockChannel)?;
    Ok(channel)
}
