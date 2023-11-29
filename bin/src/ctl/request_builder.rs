use std::collections::BTreeMap;

use anyhow::{bail, Context};

use sozu_command_lib::{
    certificate::{calculate_fingerprint, split_certificate_chain, Fingerprint},
    config::{Config, ListenerBuilder},
    proto::command::{
        request::RequestType, ActivateListener, AddBackend, AddCertificate, CertificateAndKey,
        Cluster, CountRequests, DeactivateListener, FrontendFilters, HardStop, ListListeners,
        ListenerType, LoadBalancingParams, MetricsConfiguration, PathRule, ProxyProtocolConfig,
        RemoveBackend, RemoveCertificate, RemoveListener, ReplaceCertificate, RequestHttpFrontend,
        RequestTcpFrontend, RulePosition, SoftStop, Status, SubscribeEvents, TlsVersion,
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
        debug!("Saving the state to file {}", path);

        self.send_request(RequestType::SaveState(path).into())
    }

    pub fn load_state(&mut self, path: String) -> anyhow::Result<()> {
        debug!("Loading the state on path {}", path);

        self.send_request(RequestType::LoadState(path).into())
    }

    pub fn count_requests(&mut self) -> anyhow::Result<()> {
        self.send_request(RequestType::CountRequests(CountRequests {}).into())
    }

    pub fn soft_stop(&mut self) -> anyhow::Result<()> {
        debug!("shutting down proxy softly");

        self.send_request_to_workers(RequestType::SoftStop(SoftStop {}).into(), false)
    }

    pub fn hard_stop(&mut self) -> anyhow::Result<()> {
        debug!("shutting down proxy the hard way");

        self.send_request_to_workers(RequestType::HardStop(HardStop {}).into(), false)
    }

    pub fn status(&mut self, json: bool) -> anyhow::Result<()> {
        debug!("Requesting status…");

        self.send_request_to_workers(RequestType::Status(Status {}).into(), json)
    }

    pub fn configure_metrics(&mut self, cmd: MetricsCmd) -> anyhow::Result<()> {
        debug!("Configuring metrics: {:?}", cmd);

        let configuration = match cmd {
            MetricsCmd::Enable => MetricsConfiguration::Enabled,
            MetricsCmd::Disable => MetricsConfiguration::Disabled,
            MetricsCmd::Clear => MetricsConfiguration::Clear,
            _ => bail!("The command passed to the configure_metrics function is wrong."),
        };

        self.send_request(RequestType::ConfigureMetrics(configuration as i32).into())
    }

    pub fn reload_configuration(&mut self, path: Option<String>, json: bool) -> anyhow::Result<()> {
        debug!("Reloading configuration…");
        let path = match path {
            Some(p) => p,
            None => String::new(),
        };
        self.send_request_to_workers(RequestType::ReloadConfiguration(path).into(), json)
    }

    pub fn list_frontends(
        &mut self,
        http: bool,
        https: bool,
        tcp: bool,
        domain: Option<String>,
    ) -> anyhow::Result<()> {
        debug!("Listing frontends");

        self.send_request(
            RequestType::ListFrontends(FrontendFilters {
                http,
                https,
                tcp,
                domain,
            })
            .into(),
        )
    }

    pub fn events(&mut self) -> anyhow::Result<()> {
        self.send_request(RequestType::SubscribeEvents(SubscribeEvents {}).into())
    }

    pub fn backend_command(&mut self, cmd: BackendCmd) -> anyhow::Result<()> {
        match cmd {
            BackendCmd::Add {
                id,
                backend_id,
                address,
                sticky_id,
                backup,
            } => self.send_request(
                RequestType::AddBackend(AddBackend {
                    cluster_id: id,
                    address: address.to_string(),
                    backend_id,
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    sticky_id,
                    backup,
                })
                .into(),
            ),
            BackendCmd::Remove {
                id,
                backend_id,
                address,
            } => self.send_request(
                RequestType::RemoveBackend(RemoveBackend {
                    cluster_id: id,
                    address: address.to_string(),
                    backend_id,
                })
                .into(),
            ),
        }
    }

    pub fn cluster_command(&mut self, cmd: ClusterCmd, json: bool) -> anyhow::Result<()> {
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
                self.send_request(
                    RequestType::AddCluster(Cluster {
                        cluster_id: id,
                        sticky_session,
                        https_redirect,
                        proxy_protocol: proxy_protocol.map(|pp| pp as i32),
                        load_balancing: load_balancing_policy as i32,
                        ..Default::default()
                    })
                    .into(),
                )
            }
            ClusterCmd::Remove { id } => self.send_request(RequestType::RemoveCluster(id).into()),
            ClusterCmd::List { id, domain } => self.query_cluster(json, id, domain),
        }
    }

    pub fn tcp_frontend_command(&mut self, cmd: TcpFrontendCmd) -> anyhow::Result<()> {
        match cmd {
            TcpFrontendCmd::Add { id, address, tags } => self.send_request(
                RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: id,
                    address: address.to_string(),
                    tags: tags.unwrap_or(BTreeMap::new()),
                })
                .into(),
            ),
            TcpFrontendCmd::Remove { id, address } => self.send_request(
                RequestType::RemoveTcpFrontend(RequestTcpFrontend {
                    cluster_id: id,
                    address: address.to_string(),
                    ..Default::default()
                })
                .into(),
            ),
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
                cluster_id: route,
                tags,
                h2,
            } => self.send_request(
                RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: route.into(),
                    address: address.to_string(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    position: RulePosition::Tree.into(),
                    tags: match tags {
                        Some(tags) => tags,
                        None => BTreeMap::new(),
                    },
                    h2: h2.unwrap_or(false),
                })
                .into(),
            ),
            HttpFrontendCmd::Remove {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                cluster_id: route,
            } => self.send_request(
                RequestType::RemoveHttpFrontend(RequestHttpFrontend {
                    cluster_id: route.into(),
                    address: address.to_string(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    ..Default::default()
                })
                .into(),
            ),
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
                cluster_id: route,
                tags,
                h2,
            } => self.send_request(
                RequestType::AddHttpsFrontend(RequestHttpFrontend {
                    cluster_id: route.into(),
                    address: address.to_string(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    position: RulePosition::Tree.into(),
                    tags: match tags {
                        Some(tags) => tags,
                        None => BTreeMap::new(),
                    },
                    h2: h2.unwrap_or(false),
                })
                .into(),
            ),
            HttpFrontendCmd::Remove {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                cluster_id: route,
            } => self.send_request(
                RequestType::RemoveHttpsFrontend(RequestHttpFrontend {
                    cluster_id: route.into(),
                    address: address.to_string(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    ..Default::default()
                })
                .into(),
            ),
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
                let https_listener = ListenerBuilder::new_https(address)
                    .with_public_address(public_address)
                    .with_answer_404_path(answer_404)
                    .with_answer_503_path(answer_503)
                    .with_tls_versions(tls_versions)
                    .with_cipher_list(cipher_list)
                    .with_expect_proxy(expect_proxy)
                    .with_sticky_name(sticky_name)
                    .with_front_timeout(front_timeout)
                    .with_back_timeout(back_timeout)
                    .with_request_timeout(request_timeout)
                    .with_connect_timeout(connect_timeout)
                    .to_tls(Some(&self.config))
                    .with_context(|| "Error creating HTTPS listener")?;

                self.send_request(RequestType::AddHttpsListener(https_listener).into())
            }
            HttpsListenerCmd::Remove { address } => {
                self.remove_listener(address.to_string(), ListenerType::Https)
            }
            HttpsListenerCmd::Activate { address } => {
                self.activate_listener(address.to_string(), ListenerType::Https)
            }
            HttpsListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address.to_string(), ListenerType::Https)
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
                let http_listener = ListenerBuilder::new_http(address)
                    .with_public_address(public_address)
                    .with_answer_404_path(answer_404)
                    .with_answer_503_path(answer_503)
                    .with_expect_proxy(expect_proxy)
                    .with_sticky_name(sticky_name)
                    .with_front_timeout(front_timeout)
                    .with_request_timeout(request_timeout)
                    .with_back_timeout(back_timeout)
                    .with_connect_timeout(connect_timeout)
                    .to_http(Some(&self.config))
                    .with_context(|| "Error creating HTTP listener")?;
                self.send_request(RequestType::AddHttpListener(http_listener).into())
            }
            HttpListenerCmd::Remove { address } => {
                self.remove_listener(address.to_string(), ListenerType::Http)
            }
            HttpListenerCmd::Activate { address } => {
                self.activate_listener(address.to_string(), ListenerType::Http)
            }
            HttpListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address.to_string(), ListenerType::Http)
            }
        }
    }

    pub fn tcp_listener_command(&mut self, cmd: TcpListenerCmd) -> anyhow::Result<()> {
        match cmd {
            TcpListenerCmd::Add {
                address,
                public_address,
                expect_proxy,
            } => {
                let listener = ListenerBuilder::new_tcp(address)
                    .with_public_address(public_address)
                    .with_expect_proxy(expect_proxy)
                    .to_tcp(Some(&self.config))
                    .with_context(|| "Could not create TCP listener")?;

                self.send_request(RequestType::AddTcpListener(listener).into())
            }
            TcpListenerCmd::Remove { address } => {
                self.remove_listener(address.to_string(), ListenerType::Tcp)
            }
            TcpListenerCmd::Activate { address } => {
                self.activate_listener(address.to_string(), ListenerType::Tcp)
            }
            TcpListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address.to_string(), ListenerType::Tcp)
            }
        }
    }

    pub fn list_listeners(&mut self) -> anyhow::Result<()> {
        self.send_request(RequestType::ListListeners(ListListeners {}).into())
    }

    pub fn remove_listener(
        &mut self,
        address: String,
        listener_type: ListenerType,
    ) -> anyhow::Result<()> {
        self.send_request(
            RequestType::RemoveListener(RemoveListener {
                address: address.parse().with_context(|| "wrong socket address")?,
                proxy: listener_type.into(),
            })
            .into(),
        )
    }

    pub fn activate_listener(
        &mut self,
        address: String,
        listener_type: ListenerType,
    ) -> anyhow::Result<()> {
        self.send_request(
            RequestType::ActivateListener(ActivateListener {
                address: address.parse().with_context(|| "wrong socket address")?,
                proxy: listener_type.into(),
                from_scm: false,
            })
            .into(),
        )
    }

    pub fn deactivate_listener(
        &mut self,
        address: String,
        listener_type: ListenerType,
    ) -> anyhow::Result<()> {
        self.send_request(
            RequestType::DeactivateListener(DeactivateListener {
                address: address.parse().with_context(|| "wrong socket address")?,
                proxy: listener_type.into(),
                to_scm: false,
            })
            .into(),
        )
    }

    pub fn logging_filter(&mut self, filter: &LoggingLevel) -> anyhow::Result<()> {
        self.send_request(RequestType::Logging(filter.to_string().to_lowercase()).into())
    }

    pub fn add_certificate(
        &mut self,
        address: String,
        certificate_path: &str,
        certificate_chain_path: &str,
        key_path: &str,
        versions: Vec<TlsVersion>,
    ) -> anyhow::Result<()> {
        let new_certificate = load_full_certificate(
            certificate_path,
            certificate_chain_path,
            key_path,
            versions,
            vec![],
        )
        .with_context(|| "Could not load the full certificate")?;

        self.send_request(
            RequestType::AddCertificate(AddCertificate {
                address,
                certificate: new_certificate,
                expired_at: None,
            })
            .into(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn replace_certificate(
        &mut self,
        address: String,
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
            vec![],
        )
        .with_context(|| "Could not load the full certificate")?;

        self.send_request(
            RequestType::ReplaceCertificate(ReplaceCertificate {
                address,
                new_certificate,
                old_fingerprint: old_fingerprint.to_string(),
                new_expired_at: None,
            })
            .into(),
        )?;

        Ok(())
    }

    pub fn remove_certificate(
        &mut self,
        address: String,
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

        self.send_request(
            RequestType::RemoveCertificate(RemoveCertificate {
                address,
                fingerprint: fingerprint.to_string(),
            })
            .into(),
        )
    }
}

fn get_fingerprint_from_certificate_path(certificate_path: &str) -> anyhow::Result<Fingerprint> {
    let bytes = Config::load_file_bytes(certificate_path)
        .with_context(|| format!("could not load certificate file on path {certificate_path}"))?;

    let parsed_bytes = calculate_fingerprint(&bytes).with_context(|| {
        format!("could not calculate fingerprint for the certificate at {certificate_path}")
    })?;

    Ok(Fingerprint(parsed_bytes))
}

fn decode_fingerprint(fingerprint: &str) -> anyhow::Result<Fingerprint> {
    let bytes = hex::decode(fingerprint)
        .with_context(|| "Failed at decoding the string (expected hexadecimal data)")?;
    Ok(Fingerprint(bytes))
}

fn load_full_certificate(
    certificate_path: &str,
    certificate_chain_path: &str,
    key_path: &str,
    versions: Vec<TlsVersion>,
    names: Vec<String>,
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

    let versions = versions.iter().map(|v| *v as i32).collect();

    Ok(CertificateAndKey {
        certificate,
        certificate_chain,
        key,
        versions,
        names,
    })
}
