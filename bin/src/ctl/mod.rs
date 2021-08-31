mod command;

use crate::cli;
use crate::util;
use crate::{get_config_file_path, load_configuration};
use sozu_command::channel::Channel;
use sozu_command::command::{CommandRequest, CommandResponse};
use sozu_command::config::Config;
use sozu_command::proxy::ListenerType;
use std::io;

use self::command::{
    activate_listener, add_application, add_backend, add_certificate, add_http_frontend,
    add_http_listener, add_https_listener, add_tcp_frontend, add_tcp_listener, deactivate_listener,
    dump_state, events, hard_stop, load_state, logging_filter, metrics, query_application,
    query_certificate, query_metrics, reload_configuration, remove_application, remove_backend,
    remove_certificate, remove_http_frontend, remove_listener, remove_tcp_frontend,
    replace_certificate, save_state, soft_stop, status, upgrade_main, upgrade_worker,
};
use crate::cli::*;

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

    let channel = create_channel(&config).expect("could not connect to the command unix socket");
    let timeout: u64 = matches.timeout.unwrap_or(config.ctl_command_timeout);

    match matches.cmd {
        SubCmd::Shutdown { hard, worker } => {
            if hard {
                hard_stop(channel, worker, timeout)
            } else {
                soft_stop(channel, worker)
            }
        }
        SubCmd::Upgrade { worker: None } => upgrade_main(channel, &config),
        SubCmd::Upgrade { worker: Some(id) } => {
            upgrade_worker(channel, timeout, id)?;
            Ok(())
        }
        SubCmd::Status { json } => status(channel, json),
        SubCmd::Metrics { cmd } => metrics(channel, cmd),
        SubCmd::Logging { level } => logging_filter(channel, timeout, &level),
        SubCmd::State { cmd } => match cmd {
            StateCmd::Save { file } => save_state(channel, timeout, file),
            StateCmd::Load { file } => load_state(channel, timeout, file),
            StateCmd::Dump { json } => dump_state(channel, timeout, json),
        },
        SubCmd::Reload { file, json } => reload_configuration(channel, file, json),
        SubCmd::Application { cmd } => match cmd {
            ApplicationCmd::Add {
                id,
                sticky_session,
                https_redirect,
                send_proxy,
                expect_proxy,
                load_balancing_policy,
            } => add_application(
                channel,
                timeout,
                &id,
                sticky_session,
                https_redirect,
                send_proxy,
                expect_proxy,
                load_balancing_policy,
            ),
            ApplicationCmd::Remove { id } => remove_application(channel, timeout, &id),
        },
        SubCmd::Backend { cmd } => match cmd {
            BackendCmd::Add {
                id,
                backend_id,
                address,
                sticky_id,
                backup,
            } => add_backend(
                channel,
                timeout,
                &id,
                &backend_id,
                address,
                sticky_id,
                backup,
            ),
            BackendCmd::Remove {
                id,
                backend_id,
                address,
            } => remove_backend(channel, timeout, &id, &backend_id, address),
        },
        SubCmd::Frontend { cmd } => match cmd {
            FrontendCmd::Http { cmd } => match cmd {
                HttpFrontendCmd::Add {
                    hostname,
                    path_begin,
                    address,
                    method,
                    route,
                } => add_http_frontend(
                    channel,
                    timeout,
                    route.into(),
                    address,
                    &hostname,
                    &path_begin.unwrap_or("".to_string()),
                    method.as_deref(),
                    false,
                ),
                HttpFrontendCmd::Remove {
                    hostname,
                    path_begin,
                    address,
                    method,
                    route,
                } => remove_http_frontend(
                    channel,
                    timeout,
                    route.into(),
                    address,
                    &hostname,
                    &path_begin.unwrap_or("".to_string()),
                    method.as_deref(),
                    false,
                ),
            },
            FrontendCmd::Https { cmd } => match cmd {
                HttpFrontendCmd::Add {
                    hostname,
                    path_begin,
                    address,
                    method,
                    route,
                } => add_http_frontend(
                    channel,
                    timeout,
                    route.into(),
                    address,
                    &hostname,
                    &path_begin.unwrap_or("".to_string()),
                    method.as_deref(),
                    true,
                ),
                HttpFrontendCmd::Remove {
                    hostname,
                    path_begin,
                    address,
                    method,
                    route,
                } => remove_http_frontend(
                    channel,
                    timeout,
                    route.into(),
                    address,
                    &hostname,
                    &path_begin.unwrap_or("".to_string()),
                    method.as_deref(),
                    true,
                ),
            },
            FrontendCmd::Tcp { cmd } => match cmd {
                TcpFrontendCmd::Add { id, address } => {
                    add_tcp_frontend(channel, timeout, &id, address)
                }
                TcpFrontendCmd::Remove { id, address } => {
                    remove_tcp_frontend(channel, timeout, &id, address)
                }
            },
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
                } => add_http_listener(
                    channel,
                    timeout,
                    address,
                    public_address,
                    answer_404,
                    answer_503,
                    expect_proxy,
                    sticky_name,
                ),
                HttpListenerCmd::Remove { address } => {
                    remove_listener(channel, timeout, address, ListenerType::HTTP)
                }
                HttpListenerCmd::Activate { address } => {
                    activate_listener(channel, timeout, address, ListenerType::HTTP)
                }
                HttpListenerCmd::Deactivate { address } => {
                    deactivate_listener(channel, timeout, address, ListenerType::HTTP)
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
                } => add_https_listener(
                    channel,
                    timeout,
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
                    remove_listener(channel, timeout, address, ListenerType::HTTPS)
                }
                HttpsListenerCmd::Activate { address } => {
                    activate_listener(channel, timeout, address, ListenerType::HTTPS)
                }
                HttpsListenerCmd::Deactivate { address } => {
                    deactivate_listener(channel, timeout, address, ListenerType::HTTPS)
                }
            },
            ListenerCmd::Tcp { cmd } => match cmd {
                TcpListenerCmd::Add {
                    address,
                    public_address,
                    expect_proxy,
                } => add_tcp_listener(channel, timeout, address, public_address, expect_proxy),
                TcpListenerCmd::Remove { address } => {
                    remove_listener(channel, timeout, address, ListenerType::TCP)
                }
                TcpListenerCmd::Activate { address } => {
                    activate_listener(channel, timeout, address, ListenerType::TCP)
                }
                TcpListenerCmd::Deactivate { address } => {
                    deactivate_listener(channel, timeout, address, ListenerType::TCP)
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
            } => add_certificate(
                channel,
                timeout,
                address,
                &certificate,
                &chain,
                &key,
                tls_versions,
            ),
            CertificateCmd::Remove {
                certificate,
                address,
                fingerprint,
            } => remove_certificate(
                channel,
                timeout,
                address,
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
            } => replace_certificate(
                channel,
                timeout,
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
            QueryCmd::Applications { id, domain } => query_application(channel, json, id, domain),
            QueryCmd::Certificates {
                fingerprint,
                domain,
            } => query_certificate(channel, json, fingerprint, domain),
            QueryCmd::Metrics {
                list,
                refresh,
                names,
                clusters,
                backends,
            } => query_metrics(channel, json, list, refresh, names, clusters, backends),
        },
        SubCmd::Config { cmd: _ } => Ok(()), // noop, handled at the beginning of the method
        SubCmd::Events => events(channel),
        rest => {
            panic!("that command should have been handled earlier: {:x?}", rest)
        }
    }
}

pub fn create_channel(
    config: &Config,
) -> Result<Channel<CommandRequest, CommandResponse>, io::Error> {
    Channel::from_path(
        &config.command_socket_path(),
        config.command_buffer_size,
        config.max_command_buffer_size,
    )
    .and_then(|mut channel| {
        channel.set_nonblocking(false);
        Ok(channel)
    })
}
