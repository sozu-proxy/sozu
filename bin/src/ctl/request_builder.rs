use std::net::SocketAddr;

use anyhow::{bail, Context};

use sozu_command_lib::{
    certificate::{calculate_fingerprint, split_certificate_chain},
    command::{CommandRequestOrder, FrontendFilters},
    config::{Config, FileListenerProtocolConfig, Listener, ProxyProtocolConfig},
    proxy::{
        ActivateListener, AddCertificate, Backend, CertificateAndKey, CertificateFingerprint,
        Cluster, DeactivateListener, HttpFrontend, ListenerType, LoadBalancingParams,
        MetricsConfiguration, PathRule, ProxyRequestOrder, RemoveBackend, RemoveCertificate,
        RemoveListener, ReplaceCertificate, RulePosition, TcpFrontend, TcpListenerConfig,
        TlsVersion,
    },
};

use crate::{
    cli::{
        BackendCmd, ClusterCmd, HttpFrontendCmd, HttpListenerCmd, HttpsListenerCmd, LoggingLevel,
        MetricsCmd, TcpFrontendCmd, TcpListenerCmd,
    },
    ctl::CommandManager,
};

impl CommandManager {
    pub fn save_state(&mut self, path: String) -> anyhow::Result<()> {
        println!("Loading the state to file {path}");

        self.order_command(CommandRequestOrder::SaveState { path })
    }

    pub fn load_state(&mut self, path: String) -> anyhow::Result<()> {
        println!("Loading the state on path {path}");

        self.order_command(CommandRequestOrder::LoadState { path })
    }

    pub fn dump_state(&mut self, json: bool) -> anyhow::Result<()> {
        println!("Dumping the state, json={json}");

        self.order_command_with_json(CommandRequestOrder::DumpState, json)
    }

    pub fn soft_stop(&mut self, worker_id: Option<u32>) -> anyhow::Result<()> {
        println!("shutting down proxy softly");

        self.order_command_with_worker_id(
            CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::SoftStop)),
            worker_id,
            false,
        )
    }

    pub fn hard_stop(&mut self, worker_id: Option<u32>) -> anyhow::Result<()> {
        println!("shutting down proxy the hard way");

        self.order_command_with_worker_id(
            CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::HardStop)),
            worker_id,
            false,
        )
    }
    /*
    pub fn upgrade_worker(&mut self, worker_id: u32) -> anyhow::Result<()> {
        println!("upgrading worker {}", worker_id);

        self.order_command_with_worker_id(
            CommandRequestOrder::UpgradeWorker(worker_id),
            Some(worker_id),
            false,
        )
    }
    */

    pub fn status(&mut self, json: bool) -> anyhow::Result<()> {
        println!("Requesting status…");

        self.order_command_with_json(CommandRequestOrder::Status, json)
    }

    pub fn configure_metrics(&mut self, cmd: MetricsCmd) -> anyhow::Result<()> {
        println!("Configuring metrics: {cmd:?}");

        let configuration = match cmd {
            MetricsCmd::Enable => MetricsConfiguration::Enabled(true),
            MetricsCmd::Disable => MetricsConfiguration::Enabled(false),
            MetricsCmd::Clear => MetricsConfiguration::Clear,
            _ => bail!("The command passed to the configure_metrics function is wrong."),
        };

        self.order_command(CommandRequestOrder::Proxy(Box::new(
            ProxyRequestOrder::ConfigureMetrics(configuration),
        )))
    }

    pub fn reload_configuration(&mut self, path: Option<String>, json: bool) -> anyhow::Result<()> {
        println!("Reloading configuration…");

        self.order_command_with_json(CommandRequestOrder::ReloadConfiguration { path }, json)
    }

    pub fn list_frontends(
        &mut self,
        http: bool,
        https: bool,
        tcp: bool,
        domain: Option<String>,
    ) -> anyhow::Result<()> {
        println!("Listing frontends");

        self.order_command(CommandRequestOrder::ListFrontends(FrontendFilters {
            http,
            https,
            tcp,
            domain,
        }))
    }

    pub fn events(&mut self) -> anyhow::Result<()> {
        self.order_command(CommandRequestOrder::SubscribeEvents)
    }

    pub fn backend_command(&mut self, cmd: BackendCmd) -> anyhow::Result<()> {
        match cmd {
            BackendCmd::Add {
                id,
                backend_id,
                address,
                sticky_id,
                backup,
            } => self.order_command(CommandRequestOrder::Proxy(Box::new(
                ProxyRequestOrder::AddBackend(Backend {
                    cluster_id: id,
                    address,
                    backend_id,
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    sticky_id,
                    backup,
                }),
            ))),
            BackendCmd::Remove {
                id,
                backend_id,
                address,
            } => self.order_command(CommandRequestOrder::Proxy(Box::new(
                ProxyRequestOrder::RemoveBackend(RemoveBackend {
                    cluster_id: id,
                    address,
                    backend_id,
                }),
            ))),
        }
    }

    pub fn cluster_command(&mut self, cmd: ClusterCmd) -> anyhow::Result<()> {
        match cmd {
            ClusterCmd::Add {
                id,
                sticky_session,
                https_redirect,
                send_proxy,
                expect_proxy,
                load_balancing_policy,
            } => {
                let proxy_protocol = match (send_proxy, expect_proxy) {
                    (true, true) => Some(ProxyProtocolConfig::RelayHeader),
                    (true, false) => Some(ProxyProtocolConfig::SendHeader),
                    (false, true) => Some(ProxyProtocolConfig::ExpectHeader),
                    _ => None,
                };
                self.order_command(CommandRequestOrder::Proxy(Box::new(
                    ProxyRequestOrder::AddCluster(Cluster {
                        cluster_id: id,
                        sticky_session,
                        https_redirect,
                        proxy_protocol,
                        load_balancing: load_balancing_policy,
                        load_metric: None,
                        answer_503: None,
                    }),
                )))
            }
            ClusterCmd::Remove { id } => self.order_command(CommandRequestOrder::Proxy(Box::new(
                ProxyRequestOrder::RemoveCluster { cluster_id: id },
            ))),
        }
    }

    pub fn tcp_frontend_command(&mut self, cmd: TcpFrontendCmd) -> anyhow::Result<()> {
        match cmd {
            TcpFrontendCmd::Add { id, address, tags } => {
                self.order_command(CommandRequestOrder::Proxy(Box::new(
                    ProxyRequestOrder::AddTcpFrontend(TcpFrontend {
                        cluster_id: id,
                        address,
                        tags,
                    }),
                )))
            }
            TcpFrontendCmd::Remove { id, address } => {
                self.order_command(CommandRequestOrder::Proxy(Box::new(
                    ProxyRequestOrder::RemoveTcpFrontend(TcpFrontend {
                        cluster_id: id,
                        address,
                        tags: None,
                    }),
                )))
            }
        }
    }

    pub fn http_frontend_command(&mut self, cmd: HttpFrontendCmd) -> anyhow::Result<()> {
        match cmd {
            HttpFrontendCmd::Add {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                route,
                tags,
            } => self.order_command(CommandRequestOrder::Proxy(Box::new(
                ProxyRequestOrder::AddHttpFrontend(HttpFrontend {
                    route: route.into(),
                    address,
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    position: RulePosition::Tree,
                    tags,
                }),
            ))),

            HttpFrontendCmd::Remove {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                route,
            } => self.order_command(CommandRequestOrder::Proxy(Box::new(
                ProxyRequestOrder::RemoveHttpFrontend(HttpFrontend {
                    route: route.into(),
                    address,
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    position: RulePosition::Tree,
                    tags: None,
                }),
            ))),
        }
    }

    pub fn https_frontend_command(&mut self, cmd: HttpFrontendCmd) -> anyhow::Result<()> {
        match cmd {
            HttpFrontendCmd::Add {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                route,
                tags,
            } => self.order_command(CommandRequestOrder::Proxy(Box::new(
                ProxyRequestOrder::AddHttpsFrontend(HttpFrontend {
                    route: route.into(),
                    address,
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    position: RulePosition::Tree,
                    tags,
                }),
            ))),
            HttpFrontendCmd::Remove {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                route,
            } => self.order_command(CommandRequestOrder::Proxy(Box::new(
                ProxyRequestOrder::RemoveHttpsFrontend(HttpFrontend {
                    route: route.into(),
                    address,
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    position: RulePosition::Tree,
                    tags: None,
                }),
            ))),
        }
    }

    pub fn https_listener_command(&mut self, cmd: HttpsListenerCmd) -> anyhow::Result<()> {
        match cmd {
            HttpsListenerCmd::Add {
                address,
                public_address,
                answer_404,
                answer_503,
                tls_versions,
                cipher_list,
                expect_proxy,
                sticky_name,
                front_timeout,
                back_timeout,
                request_timeout,
                connect_timeout,
            } => {
                let mut listener = Listener::new(address, FileListenerProtocolConfig::Https);
                listener.public_address = public_address;
                listener.answer_404 = answer_404;
                listener.answer_503 = answer_503;
                listener.expect_proxy = Some(expect_proxy);
                if let Some(sticky_name) = sticky_name {
                    listener.sticky_name = sticky_name;
                }
                listener.cipher_list = cipher_list;
                listener.tls_versions = if tls_versions.is_empty() {
                    None
                } else {
                    Some(tls_versions)
                };
                let https_listener = listener
                    .to_tls(
                        front_timeout,
                        back_timeout,
                        connect_timeout,
                        request_timeout,
                    )
                    .with_context(|| "Error creating HTTPS listener")?;
                self.order_command(CommandRequestOrder::Proxy(Box::new(
                    ProxyRequestOrder::AddHttpsListener(https_listener),
                )))
            }
            HttpsListenerCmd::Remove { address } => {
                self.remove_listener(address, ListenerType::HTTPS)
            }
            HttpsListenerCmd::Activate { address } => {
                self.activate_listener(address, ListenerType::HTTPS)
            }
            HttpsListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address, ListenerType::HTTPS)
            }
        }
    }

    pub fn http_listener_command(&mut self, cmd: HttpListenerCmd) -> anyhow::Result<()> {
        match cmd {
            HttpListenerCmd::Add {
                address,
                public_address,
                answer_404,
                answer_503,
                expect_proxy,
                sticky_name,
                front_timeout,
                back_timeout,
                request_timeout,
                connect_timeout,
            } => {
                let mut listener = Listener::new(address, FileListenerProtocolConfig::Http);
                listener.public_address = public_address;
                listener.answer_404 = answer_404;
                listener.answer_503 = answer_503;
                listener.expect_proxy = Some(expect_proxy);
                if let Some(sticky_name) = sticky_name {
                    listener.sticky_name = sticky_name;
                }

                let http_listener = listener
                    .to_http(
                        front_timeout,
                        back_timeout,
                        connect_timeout,
                        request_timeout,
                    )
                    .with_context(|| "Error creating HTTP listener")?;
                self.order_command(CommandRequestOrder::Proxy(Box::new(
                    ProxyRequestOrder::AddHttpListener(http_listener),
                )))
            }
            HttpListenerCmd::Remove { address } => {
                self.remove_listener(address, ListenerType::HTTP)
            }
            HttpListenerCmd::Activate { address } => {
                self.activate_listener(address, ListenerType::HTTP)
            }
            HttpListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address, ListenerType::HTTP)
            }
        }
    }

    pub fn tcp_listener_command(&mut self, cmd: TcpListenerCmd) -> anyhow::Result<()> {
        match cmd {
            TcpListenerCmd::Add {
                address,
                public_address,
                expect_proxy,
            } => self.order_command(CommandRequestOrder::Proxy(Box::new(
                ProxyRequestOrder::AddTcpListener(TcpListenerConfig {
                    address,
                    public_address,
                    expect_proxy,
                    front_timeout: 60,
                    back_timeout: 30,
                    connect_timeout: 3,
                }),
            ))),
            TcpListenerCmd::Remove { address } => self.remove_listener(address, ListenerType::TCP),
            TcpListenerCmd::Activate { address } => {
                self.activate_listener(address, ListenerType::TCP)
            }
            TcpListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address, ListenerType::TCP)
            }
        }
    }

    pub fn list_listeners(&mut self) -> anyhow::Result<()> {
        self.order_command(CommandRequestOrder::ListListeners)
    }

    pub fn remove_listener(
        &mut self,
        address: SocketAddr,
        proxy: ListenerType,
    ) -> anyhow::Result<()> {
        self.order_command(CommandRequestOrder::Proxy(Box::new(
            ProxyRequestOrder::RemoveListener(RemoveListener { address, proxy }),
        )))
    }

    pub fn activate_listener(
        &mut self,
        address: SocketAddr,
        proxy: ListenerType,
    ) -> anyhow::Result<()> {
        self.order_command(CommandRequestOrder::Proxy(Box::new(
            ProxyRequestOrder::ActivateListener(ActivateListener {
                address,
                proxy,
                from_scm: false,
            }),
        )))
    }

    pub fn deactivate_listener(
        &mut self,
        address: SocketAddr,
        proxy: ListenerType,
    ) -> anyhow::Result<()> {
        self.order_command(CommandRequestOrder::Proxy(Box::new(
            ProxyRequestOrder::DeactivateListener(DeactivateListener {
                address,
                proxy,
                to_scm: false,
            }),
        )))
    }

    pub fn logging_filter(&mut self, filter: &LoggingLevel) -> anyhow::Result<()> {
        self.order_command(CommandRequestOrder::Proxy(Box::new(
            ProxyRequestOrder::Logging(filter.to_string().to_lowercase()),
        )))
    }

    pub fn add_certificate(
        &mut self,
        address: SocketAddr,
        certificate_path: &str,
        certificate_chain_path: &str,
        key_path: &str,
        versions: Vec<TlsVersion>,
    ) -> anyhow::Result<()> {
        let new_certificate =
            load_full_certificate(certificate_path, certificate_chain_path, key_path, versions)
                .with_context(|| "Could not load the full certificate")?;

        self.order_command(CommandRequestOrder::Proxy(Box::new(
            ProxyRequestOrder::AddCertificate(AddCertificate {
                address,
                certificate: new_certificate,
                names: vec![],
                expired_at: None,
            }),
        )))
    }

    pub fn replace_certificate(
        &mut self,
        address: SocketAddr,
        new_certificate_path: &str,
        new_certificate_chain_path: &str,
        new_key_path: &str,
        old_certificate_path: Option<&str>,
        old_fingerprint: Option<&str>,
        versions: Vec<TlsVersion>,
    ) -> anyhow::Result<()> {
        let old_fingerprint = match (old_certificate_path, old_fingerprint) {
            (None, None) | (Some(_), Some(_)) => {
                bail!("Error: Please provide either one, the old certificate's path OR its fingerprint")
            }
            (Some(old_certificate_path), None) => {
                get_fingerprint_from_certificate_path(old_certificate_path).with_context(|| {
                    "Could not retrieve the fingerprint from the given certificate path"
                })?
            }
            (None, Some(fingerprint)) => decode_fingerprint(fingerprint)
                .with_context(|| "Error decoding the given fingerprint")?,
        };

        let new_certificate = load_full_certificate(
            new_certificate_path,
            new_certificate_chain_path,
            new_key_path,
            versions,
        )
        .with_context(|| "Could not load the full certificate")?;

        self.order_command(CommandRequestOrder::Proxy(Box::new(
            ProxyRequestOrder::ReplaceCertificate(ReplaceCertificate {
                address,
                new_certificate,
                old_fingerprint,
                new_names: vec![],
                new_expired_at: None,
            }),
        )))?;

        Ok(())
    }

    pub fn remove_certificate(
        &mut self,
        address: SocketAddr,
        certificate_path: Option<&str>,
        fingerprint: Option<&str>,
    ) -> anyhow::Result<()> {
        let fingerprint = match (certificate_path, fingerprint) {
            (None, None) | (Some(_), Some(_)) => {
                bail!("Error: Please provide either one, the path OR the fingerprint of the certificate")
            }
            (Some(certificate_path), None) => {
                get_fingerprint_from_certificate_path(certificate_path).with_context(|| {
                    "Could not retrieve the finger print from the given certificate path"
                })?
            }
            (None, Some(fingerprint)) => decode_fingerprint(fingerprint)
                .with_context(|| "Error decoding the given fingerprint")?,
        };

        self.order_command(CommandRequestOrder::Proxy(Box::new(
            ProxyRequestOrder::RemoveCertificate(RemoveCertificate {
                address,
                fingerprint,
            }),
        )))
    }
}

fn get_fingerprint_from_certificate_path(
    certificate_path: &str,
) -> anyhow::Result<CertificateFingerprint> {
    let bytes = Config::load_file_bytes(certificate_path)
        .with_context(|| format!("could not load certificate file on path {certificate_path}"))?;

    let parsed_bytes = calculate_fingerprint(&bytes).with_context(|| {
        format!("could not calculate fingerprint for the certificate at {certificate_path}")
    })?;

    Ok(CertificateFingerprint(parsed_bytes))
}

fn decode_fingerprint(fingerprint: &str) -> anyhow::Result<CertificateFingerprint> {
    let bytes = hex::decode(fingerprint)
        .with_context(|| "Failed at decoding the string (expected hexadecimal data)")?;
    Ok(CertificateFingerprint(bytes))
}

fn load_full_certificate(
    certificate_path: &str,
    certificate_chain_path: &str,
    key_path: &str,
    versions: Vec<TlsVersion>,
) -> Result<CertificateAndKey, anyhow::Error> {
    let certificate = Config::load_file(certificate_path)
        .with_context(|| format!("Could not load certificate file on path {certificate_path}"))?;

    let certificate_chain = Config::load_file(certificate_chain_path)
        .map(split_certificate_chain)
        .with_context(|| {
            format!("could not load certificate chain on path: {certificate_chain_path}")
        })?;

    let key = Config::load_file(key_path)
        .with_context(|| format!("Could not load key file on path {key_path}"))?;

    Ok(CertificateAndKey {
        certificate,
        certificate_chain,
        key,
        versions,
    })
}
