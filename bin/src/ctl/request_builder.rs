use std::{collections::BTreeMap, fs::File, io::Read as IoRead, path::PathBuf};

use sozu_command_lib::{
    certificate::{
        decode_fingerprint, get_fingerprint_from_certificate_path, load_full_certificate,
    },
    config::{ListenerBuilder, validate_health_check_config},
    proto::command::{
        ActivateListener, AddBackend, AddCertificate, AlpnProtocols, Cluster, CountRequests,
        CustomHttpAnswers, DeactivateListener, FrontendFilters, HardStop, HealthCheckConfig,
        ListListeners, ListenerType, LoadBalancingParams, MetricsConfiguration, PathRule,
        ProxyProtocolConfig, QueryCertificatesFilters, QueryClusterByDomain, QueryClustersHashes,
        QueryHealthChecks, QueryMaxConnectionsPerIp, RemoveBackend, RemoveCertificate,
        RemoveListener, ReplaceCertificate, RequestHttpFrontend, RequestTcpFrontend, RulePosition,
        SetHealthCheck, SocketAddress, SoftStop, Status, SubscribeEvents, TlsVersion,
        UpdateHttpListenerConfig, UpdateHttpsListenerConfig, UpdateTcpListenerConfig,
        request::RequestType, response_content::ContentType,
    },
};

use super::CtlError;
use crate::{
    cli::{
        BackendCmd, ClusterCmd, ClusterH2Cmd, ConnectionLimitCmd, HealthCheckCmd, HttpFrontendCmd,
        HttpListenerCmd, HttpsListenerCmd, MetricsCmd, TcpFrontendCmd, TcpListenerCmd,
    },
    ctl::CommandManager,
};

impl CommandManager {
    pub fn save_state(&mut self, path: String) -> Result<(), CtlError> {
        debug!("Saving the state to file {}", path);

        let current_directory =
            std::env::current_dir().map_err(|err| CtlError::ResolvePath(path.to_owned(), err))?;

        let absolute_path = current_directory.join(&path);
        self.send_request(
            RequestType::SaveState(String::from(absolute_path.to_string_lossy())).into(),
        )
    }

    pub fn load_state(&mut self, path: String) -> Result<(), CtlError> {
        debug!("Loading the state on path {}", path);

        self.send_request(RequestType::LoadState(path).into())
    }

    pub fn count_requests(&mut self) -> Result<(), CtlError> {
        self.send_request(RequestType::CountRequests(CountRequests {}).into())
    }

    pub fn soft_stop(&mut self) -> Result<(), CtlError> {
        debug!("shutting down proxy softly");

        self.send_request(RequestType::SoftStop(SoftStop {}).into())
    }

    pub fn hard_stop(&mut self) -> Result<(), CtlError> {
        debug!("shutting down proxy the hard way");

        self.send_request(RequestType::HardStop(HardStop {}).into())
    }

    pub fn status(&mut self) -> Result<(), CtlError> {
        debug!("Requesting status…");

        self.send_request(RequestType::Status(Status {}).into())
    }

    pub fn configure_metrics(&mut self, cmd: MetricsCmd) -> Result<(), CtlError> {
        debug!("Configuring metrics: {:?}", cmd);

        let configuration = match cmd {
            MetricsCmd::Enable => MetricsConfiguration::Enabled,
            MetricsCmd::Disable => MetricsConfiguration::Disabled,
            MetricsCmd::Clear => MetricsConfiguration::Clear,
            _ => return Ok(()), // completely unlikely
        };

        self.send_request(RequestType::ConfigureMetrics(configuration as i32).into())
    }

    pub fn reload_configuration(&mut self, path: Option<String>) -> Result<(), CtlError> {
        debug!("Reloading configuration…");
        self.send_request(RequestType::ReloadConfiguration(path.unwrap_or_default()).into())
    }

    pub fn list_frontends(
        &mut self,
        http: bool,
        https: bool,
        tcp: bool,
        domain: Option<String>,
    ) -> Result<(), CtlError> {
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

    pub fn events(&mut self) -> Result<(), CtlError> {
        self.send_request_no_timeout(RequestType::SubscribeEvents(SubscribeEvents {}).into())
    }

    pub fn backend_command(&mut self, cmd: BackendCmd) -> Result<(), CtlError> {
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
                    address: address.into(),
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
                    address: address.into(),
                    backend_id,
                })
                .into(),
            ),
        }
    }

    pub fn cluster_command(&mut self, cmd: ClusterCmd) -> Result<(), CtlError> {
        match cmd {
            ClusterCmd::Add {
                id,
                sticky_session,
                https_redirect,
                send_proxy,
                expect_proxy,
                load_balancing_policy,
                http2,
                https_redirect_port,
                www_authenticate,
                authorized_hash,
                answer,
            } => {
                let proxy_protocol = match (send_proxy, expect_proxy) {
                    (true, true) => Some(ProxyProtocolConfig::RelayHeader),
                    (true, false) => Some(ProxyProtocolConfig::SendHeader),
                    (false, true) => Some(ProxyProtocolConfig::ExpectHeader),
                    _ => None,
                };

                // Validate every authorized hash matches `<user>:<hex64>`
                // before sending so a typo (missing colon, lowercase off,
                // non-hex chars) surfaces here with a friendly message
                // rather than silently rejecting traffic on the worker.
                for hash in &authorized_hash {
                    if !looks_like_authorized_hash(hash) {
                        return Err(CtlError::ArgsNeeded(
                            "valid `username:hex(sha256(password))`".to_string(),
                            format!(
                                "got {hash:?}; produce one with: \
                                 printf '<password>' | sha256sum"
                            ),
                        ));
                    }
                }

                // The proto carries `https_redirect_port` as `u32` (matches
                // the rewrite_port shape on the frontend) but real TCP
                // ports are 16-bit. Reject out-of-range values up front so
                // a typo doesn't render `Location: https://host:70000/...`
                // on the wire.
                if let Some(port) = https_redirect_port {
                    if port == 0 || port > u16::MAX as u32 {
                        return Err(CtlError::ArgsNeeded(
                            "TCP port in 1..=65535".to_string(),
                            format!("got https_redirect_port={port}"),
                        ));
                    }
                }

                // Resolve each `--answer code=value` entry. The
                // right-hand side is the literal template body by
                // default; `file://<path>` opts into reading the body
                // off disk. Funnels through the canonical
                // `resolve_answer_source` helper so the CLI and TOML
                // loader stay in sync.
                //
                // Cluster-level entries override the listener-level
                // `answers` map (which itself is the global default for
                // every status code on that listener). Both layers
                // accept the same `<body> | file://<path>` value form.
                let mut answers_map = std::collections::BTreeMap::new();
                for entry in &answer {
                    let (code, value) = match entry.split_once('=') {
                        Some((c, v)) if !v.is_empty() => (c, v),
                        _ => {
                            return Err(CtlError::ArgsNeeded(
                                "<code>=<body>|file://<path>".to_string(),
                                format!("got {entry:?}"),
                            ));
                        }
                    };
                    let body =
                        sozu_command_lib::config::resolve_answer_source(value).map_err(|e| {
                            CtlError::ArgsNeeded(
                                "literal body or readable file://<path>".to_string(),
                                format!("{value:?}: {e}"),
                            )
                        })?;
                    answers_map.insert(code.to_owned(), body);
                }

                self.send_request(
                    RequestType::AddCluster(Cluster {
                        cluster_id: id,
                        sticky_session,
                        https_redirect,
                        proxy_protocol: proxy_protocol.map(|pp| pp as i32),
                        load_balancing: load_balancing_policy as i32,
                        http2: if http2 { Some(true) } else { None },
                        answers: answers_map,
                        https_redirect_port,
                        authorized_hashes: authorized_hash,
                        www_authenticate,
                        ..Default::default()
                    })
                    .into(),
                )
            }
            ClusterCmd::Remove { id } => self.send_request(RequestType::RemoveCluster(id).into()),
            ClusterCmd::H2 { cmd } => self.cluster_h2_command(cmd),
            ClusterCmd::HealthCheck { cmd } => self.health_check_command(cmd),
            ClusterCmd::List {
                id: cluster_id,
                domain,
            } => {
                if cluster_id.is_some() && domain.is_some() {
                    return Err(CtlError::ArgsNeeded(
                        "a cluster id".to_string(),
                        "a domain name".to_string(),
                    ));
                }

                let request = if let Some(ref cluster_id) = cluster_id {
                    RequestType::QueryClusterById(cluster_id.to_string()).into()
                } else if let Some(ref domain) = domain {
                    let splitted: Vec<String> =
                        domain.splitn(2, '/').map(|elem| elem.to_string()).collect();

                    if splitted.is_empty() {
                        return Err(CtlError::NeedClusterDomain)?;
                    }

                    let query_domain = QueryClusterByDomain {
                        hostname: splitted.first().ok_or(CtlError::NeedClusterDomain)?.clone(),
                        path: splitted.get(1).cloned().map(|path| format!("/{path}")), // We add the / again because of the splitn removing it
                    };

                    RequestType::QueryClustersByDomain(query_domain).into()
                } else {
                    RequestType::QueryClustersHashes(QueryClustersHashes {}).into()
                };

                self.send_request(request)
            }
        }
    }

    pub fn cluster_h2_command(&mut self, cmd: ClusterH2Cmd) -> Result<(), CtlError> {
        let (cluster_id, enable) = match cmd {
            ClusterH2Cmd::Enable { id } => (id, true),
            ClusterH2Cmd::Disable { id } => (id, false),
        };

        let request = RequestType::QueryClusterById(cluster_id.clone()).into();
        let response = self.send_request_get_response(request, true)?;

        let cluster = response
            .content
            .and_then(|content| content.content_type)
            .and_then(|content_type| find_cluster_configuration(content_type, &cluster_id))
            .ok_or_else(|| {
                CtlError::ArgsNeeded("cluster not found".to_owned(), cluster_id.clone())
            })?;

        let updated = Cluster {
            http2: Some(enable),
            ..cluster
        };

        self.send_request(RequestType::AddCluster(updated).into())
    }

    pub fn health_check_command(&mut self, cmd: HealthCheckCmd) -> Result<(), CtlError> {
        match cmd {
            HealthCheckCmd::Set {
                id,
                uri,
                interval,
                timeout,
                healthy_threshold,
                unhealthy_threshold,
                expected_status,
            } => {
                let config = HealthCheckConfig {
                    uri,
                    interval,
                    timeout,
                    healthy_threshold,
                    unhealthy_threshold,
                    expected_status,
                };
                if let Err(reason) = validate_health_check_config(&config) {
                    return Err(CtlError::Failure(reason.to_owned()));
                }
                if config.timeout >= config.interval {
                    warn!(
                        "health check timeout ({}s) >= interval ({}s), checks may overlap",
                        config.timeout, config.interval
                    );
                }
                self.send_request(
                    RequestType::SetHealthCheck(SetHealthCheck {
                        cluster_id: id,
                        config,
                    })
                    .into(),
                )
            }
            HealthCheckCmd::Remove { id } => {
                self.send_request(RequestType::RemoveHealthCheck(id).into())
            }
            HealthCheckCmd::List { id } => self.send_request(
                RequestType::QueryHealthChecks(QueryHealthChecks { cluster_id: id }).into(),
            ),
        }
    }

    pub fn tcp_frontend_command(&mut self, cmd: TcpFrontendCmd) -> Result<(), CtlError> {
        match cmd {
            TcpFrontendCmd::Add { id, address, tags } => self.send_request(
                RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: id,
                    address: address.into(),
                    tags: tags.unwrap_or(BTreeMap::new()),
                })
                .into(),
            ),
            TcpFrontendCmd::Remove { id, address } => self.send_request(
                RequestType::RemoveTcpFrontend(RequestTcpFrontend {
                    cluster_id: id,
                    address: address.into(),
                    ..Default::default()
                })
                .into(),
            ),
        }
    }

    pub fn http_frontend_command(&mut self, cmd: HttpFrontendCmd) -> Result<(), CtlError> {
        match cmd {
            HttpFrontendCmd::Add { .. } => {
                let frontend = build_http_frontend_add(cmd)?;
                self.send_request(RequestType::AddHttpFrontend(frontend).into())
            }
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
                    address: address.into(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method,
                    ..Default::default()
                })
                .into(),
            ),
        }
    }

    pub fn https_frontend_command(&mut self, cmd: HttpFrontendCmd) -> Result<(), CtlError> {
        match cmd {
            HttpFrontendCmd::Add { .. } => {
                let frontend = build_http_frontend_add(cmd)?;
                self.send_request(RequestType::AddHttpsFrontend(frontend).into())
            }
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
                    address: address.into(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method,
                    ..Default::default()
                })
                .into(),
            ),
        }
    }

    pub fn https_listener_command(&mut self, cmd: HttpsListenerCmd) -> Result<(), CtlError> {
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
                let https_listener = ListenerBuilder::new_https(address.into())
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
                    .map_err(CtlError::CreateListener)?;

                self.send_request(RequestType::AddHttpsListener(https_listener).into())
            }
            HttpsListenerCmd::Remove { address } => {
                self.remove_listener(address.into(), ListenerType::Https)
            }
            HttpsListenerCmd::Activate { address } => {
                self.activate_listener(address.into(), ListenerType::Https)
            }
            HttpsListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address.into(), ListenerType::Https)
            }
            HttpsListenerCmd::Update {
                address,
                public_address,
                sticky_name,
                front_timeout,
                back_timeout,
                connect_timeout,
                request_timeout,
                expect_proxy,
                no_expect_proxy,
                strict_sni_binding,
                no_strict_sni_binding,
                disable_http11,
                enable_http11,
                alpn_protocols,
                reset_alpn,
                h2_max_rst_stream_per_window,
                h2_max_ping_per_window,
                h2_max_settings_per_window,
                h2_max_empty_data_per_window,
                h2_max_continuation_frames,
                h2_max_glitch_count,
                h2_initial_connection_window,
                h2_max_concurrent_streams,
                h2_stream_shrink_ratio,
                h2_max_rst_stream_lifetime,
                h2_max_rst_stream_abusive_lifetime,
                h2_max_rst_stream_emitted_lifetime,
                h2_max_header_list_size,
                h2_max_header_table_size,
                h2_stream_idle_timeout_seconds,
                h2_graceful_shutdown_deadline_seconds,
                h2_max_window_update_stream0_per_window,
                sozu_id_header,
                answer_301,
                answer_401,
                answer_404,
                answer_408,
                answer_413,
                answer_421,
                answer_429,
                answer_502,
                answer_503,
                answer_504,
                answer_507,
                hsts_max_age,
                hsts_include_subdomains,
                hsts_preload,
                hsts_disabled,
                hsts_force_replace_backend,
            } => self.update_https_listener_command(
                address,
                public_address,
                sticky_name,
                front_timeout,
                back_timeout,
                connect_timeout,
                request_timeout,
                expect_proxy,
                no_expect_proxy,
                strict_sni_binding,
                no_strict_sni_binding,
                disable_http11,
                enable_http11,
                alpn_protocols,
                reset_alpn,
                h2_max_rst_stream_per_window,
                h2_max_ping_per_window,
                h2_max_settings_per_window,
                h2_max_empty_data_per_window,
                h2_max_continuation_frames,
                h2_max_glitch_count,
                h2_initial_connection_window,
                h2_max_concurrent_streams,
                h2_stream_shrink_ratio,
                h2_max_rst_stream_lifetime,
                h2_max_rst_stream_abusive_lifetime,
                h2_max_rst_stream_emitted_lifetime,
                h2_max_header_list_size,
                h2_max_header_table_size,
                h2_stream_idle_timeout_seconds,
                h2_graceful_shutdown_deadline_seconds,
                h2_max_window_update_stream0_per_window,
                sozu_id_header,
                answer_301,
                answer_401,
                answer_404,
                answer_408,
                answer_413,
                answer_421,
                answer_429,
                answer_502,
                answer_503,
                answer_504,
                answer_507,
                hsts_max_age,
                hsts_include_subdomains,
                hsts_preload,
                hsts_disabled,
                hsts_force_replace_backend,
            ),
        }
    }

    pub fn http_listener_command(&mut self, cmd: HttpListenerCmd) -> Result<(), CtlError> {
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
                let http_listener = ListenerBuilder::new_http(address.into())
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
                    .map_err(CtlError::CreateListener)?;

                self.send_request(RequestType::AddHttpListener(http_listener).into())
            }
            HttpListenerCmd::Remove { address } => {
                self.remove_listener(address.into(), ListenerType::Http)
            }
            HttpListenerCmd::Activate { address } => {
                self.activate_listener(address.into(), ListenerType::Http)
            }
            HttpListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address.into(), ListenerType::Http)
            }
            HttpListenerCmd::Update {
                address,
                public_address,
                sticky_name,
                front_timeout,
                back_timeout,
                connect_timeout,
                request_timeout,
                expect_proxy,
                no_expect_proxy,
                h2_max_rst_stream_per_window,
                h2_max_ping_per_window,
                h2_max_settings_per_window,
                h2_max_empty_data_per_window,
                h2_max_continuation_frames,
                h2_max_glitch_count,
                h2_initial_connection_window,
                h2_max_concurrent_streams,
                h2_stream_shrink_ratio,
                h2_max_rst_stream_lifetime,
                h2_max_rst_stream_abusive_lifetime,
                h2_max_rst_stream_emitted_lifetime,
                h2_max_header_list_size,
                h2_max_header_table_size,
                h2_stream_idle_timeout_seconds,
                h2_graceful_shutdown_deadline_seconds,
                h2_max_window_update_stream0_per_window,
                sozu_id_header,
                answer_301,
                answer_401,
                answer_404,
                answer_408,
                answer_413,
                answer_421,
                answer_429,
                answer_502,
                answer_503,
                answer_504,
                answer_507,
            } => self.update_http_listener_command(
                address,
                public_address,
                sticky_name,
                front_timeout,
                back_timeout,
                connect_timeout,
                request_timeout,
                expect_proxy,
                no_expect_proxy,
                h2_max_rst_stream_per_window,
                h2_max_ping_per_window,
                h2_max_settings_per_window,
                h2_max_empty_data_per_window,
                h2_max_continuation_frames,
                h2_max_glitch_count,
                h2_initial_connection_window,
                h2_max_concurrent_streams,
                h2_stream_shrink_ratio,
                h2_max_rst_stream_lifetime,
                h2_max_rst_stream_abusive_lifetime,
                h2_max_rst_stream_emitted_lifetime,
                h2_max_header_list_size,
                h2_max_header_table_size,
                h2_stream_idle_timeout_seconds,
                h2_graceful_shutdown_deadline_seconds,
                h2_max_window_update_stream0_per_window,
                sozu_id_header,
                answer_301,
                answer_401,
                answer_404,
                answer_408,
                answer_413,
                answer_421,
                answer_429,
                answer_502,
                answer_503,
                answer_504,
                answer_507,
            ),
        }
    }

    pub fn tcp_listener_command(&mut self, cmd: TcpListenerCmd) -> Result<(), CtlError> {
        match cmd {
            TcpListenerCmd::Add {
                address,
                public_address,
                expect_proxy,
            } => {
                let listener = ListenerBuilder::new_tcp(address.into())
                    .with_public_address(public_address)
                    .with_expect_proxy(expect_proxy)
                    .to_tcp(Some(&self.config))
                    .map_err(CtlError::CreateListener)?;

                self.send_request(RequestType::AddTcpListener(listener).into())
            }
            TcpListenerCmd::Remove { address } => {
                self.remove_listener(address.into(), ListenerType::Tcp)
            }
            TcpListenerCmd::Activate { address } => {
                self.activate_listener(address.into(), ListenerType::Tcp)
            }
            TcpListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address.into(), ListenerType::Tcp)
            }
            TcpListenerCmd::Update {
                address,
                public_address,
                front_timeout,
                back_timeout,
                connect_timeout,
                expect_proxy,
                no_expect_proxy,
            } => self.update_tcp_listener_command(
                address,
                public_address,
                front_timeout,
                back_timeout,
                connect_timeout,
                expect_proxy,
                no_expect_proxy,
            ),
        }
    }

    pub fn list_listeners(&mut self) -> Result<(), CtlError> {
        self.send_request(RequestType::ListListeners(ListListeners {}).into())
    }

    /// Patch a running HTTP listener in place. Only `Some` fields in the patch
    /// are applied; `None` fields preserve the listener's current value.
    #[allow(clippy::too_many_arguments)]
    pub fn update_http_listener_command(
        &mut self,
        address: std::net::SocketAddr,
        public_address: Option<std::net::SocketAddr>,
        sticky_name: Option<String>,
        front_timeout: Option<u32>,
        back_timeout: Option<u32>,
        connect_timeout: Option<u32>,
        request_timeout: Option<u32>,
        expect_proxy_flag: bool,
        no_expect_proxy_flag: bool,
        h2_max_rst_stream_per_window: Option<u32>,
        h2_max_ping_per_window: Option<u32>,
        h2_max_settings_per_window: Option<u32>,
        h2_max_empty_data_per_window: Option<u32>,
        h2_max_continuation_frames: Option<u32>,
        h2_max_glitch_count: Option<u32>,
        h2_initial_connection_window: Option<u32>,
        h2_max_concurrent_streams: Option<u32>,
        h2_stream_shrink_ratio: Option<u32>,
        h2_max_rst_stream_lifetime: Option<u64>,
        h2_max_rst_stream_abusive_lifetime: Option<u64>,
        h2_max_rst_stream_emitted_lifetime: Option<u64>,
        h2_max_header_list_size: Option<u32>,
        h2_max_header_table_size: Option<u32>,
        h2_stream_idle_timeout_seconds: Option<u32>,
        h2_graceful_shutdown_deadline_seconds: Option<u32>,
        h2_max_window_update_stream0_per_window: Option<u32>,
        sozu_id_header: Option<String>,
        answer_301: Option<PathBuf>,
        answer_401: Option<PathBuf>,
        answer_404: Option<PathBuf>,
        answer_408: Option<PathBuf>,
        answer_413: Option<PathBuf>,
        answer_421: Option<PathBuf>,
        answer_429: Option<PathBuf>,
        answer_502: Option<PathBuf>,
        answer_503: Option<PathBuf>,
        answer_504: Option<PathBuf>,
        answer_507: Option<PathBuf>,
    ) -> Result<(), CtlError> {
        let expect_proxy = if expect_proxy_flag {
            Some(true)
        } else if no_expect_proxy_flag {
            Some(false)
        } else {
            None
        };

        let http_answers = build_http_answers(
            answer_301, answer_401, answer_404, answer_408, answer_413, answer_421, answer_429,
            answer_502, answer_503, answer_504, answer_507,
        )?;

        let patch = UpdateHttpListenerConfig {
            address: address.into(),
            public_address: public_address.map(|a| a.into()),
            expect_proxy,
            sticky_name,
            front_timeout,
            back_timeout,
            connect_timeout,
            request_timeout,
            http_answers,
            h2_max_rst_stream_per_window,
            h2_max_ping_per_window,
            h2_max_settings_per_window,
            h2_max_empty_data_per_window,
            h2_max_continuation_frames,
            h2_max_glitch_count,
            h2_initial_connection_window,
            h2_max_concurrent_streams,
            h2_stream_shrink_ratio,
            h2_max_rst_stream_lifetime,
            h2_max_rst_stream_abusive_lifetime,
            h2_max_rst_stream_emitted_lifetime,
            h2_max_header_list_size,
            h2_max_header_table_size,
            h2_stream_idle_timeout_seconds,
            h2_graceful_shutdown_deadline_seconds,
            h2_max_window_update_stream0_per_window,
            sozu_id_header,
            ..Default::default()
        };
        self.send_request(RequestType::UpdateHttpListener(patch).into())
    }

    /// Patch a running HTTPS listener in place. Only `Some` fields in the patch
    /// are applied; `None` fields preserve the listener's current value.
    #[allow(clippy::too_many_arguments)]
    pub fn update_https_listener_command(
        &mut self,
        address: std::net::SocketAddr,
        public_address: Option<std::net::SocketAddr>,
        sticky_name: Option<String>,
        front_timeout: Option<u32>,
        back_timeout: Option<u32>,
        connect_timeout: Option<u32>,
        request_timeout: Option<u32>,
        expect_proxy_flag: bool,
        no_expect_proxy_flag: bool,
        strict_sni_binding_flag: bool,
        no_strict_sni_binding_flag: bool,
        disable_http11_flag: bool,
        enable_http11_flag: bool,
        alpn_protocols: Option<Vec<String>>,
        reset_alpn: bool,
        h2_max_rst_stream_per_window: Option<u32>,
        h2_max_ping_per_window: Option<u32>,
        h2_max_settings_per_window: Option<u32>,
        h2_max_empty_data_per_window: Option<u32>,
        h2_max_continuation_frames: Option<u32>,
        h2_max_glitch_count: Option<u32>,
        h2_initial_connection_window: Option<u32>,
        h2_max_concurrent_streams: Option<u32>,
        h2_stream_shrink_ratio: Option<u32>,
        h2_max_rst_stream_lifetime: Option<u64>,
        h2_max_rst_stream_abusive_lifetime: Option<u64>,
        h2_max_rst_stream_emitted_lifetime: Option<u64>,
        h2_max_header_list_size: Option<u32>,
        h2_max_header_table_size: Option<u32>,
        h2_stream_idle_timeout_seconds: Option<u32>,
        h2_graceful_shutdown_deadline_seconds: Option<u32>,
        h2_max_window_update_stream0_per_window: Option<u32>,
        sozu_id_header: Option<String>,
        answer_301: Option<PathBuf>,
        answer_401: Option<PathBuf>,
        answer_404: Option<PathBuf>,
        answer_408: Option<PathBuf>,
        answer_413: Option<PathBuf>,
        answer_421: Option<PathBuf>,
        answer_429: Option<PathBuf>,
        answer_502: Option<PathBuf>,
        answer_503: Option<PathBuf>,
        answer_504: Option<PathBuf>,
        answer_507: Option<PathBuf>,
        hsts_max_age: Option<u32>,
        hsts_include_subdomains: bool,
        hsts_preload: bool,
        hsts_disabled: bool,
        hsts_force_replace_backend: bool,
    ) -> Result<(), CtlError> {
        let expect_proxy = if expect_proxy_flag {
            Some(true)
        } else if no_expect_proxy_flag {
            Some(false)
        } else {
            None
        };

        let strict_sni_binding = if strict_sni_binding_flag {
            Some(true)
        } else if no_strict_sni_binding_flag {
            Some(false)
        } else {
            None
        };

        let disable_http11 = if disable_http11_flag {
            Some(true)
        } else if enable_http11_flag {
            Some(false)
        } else {
            None
        };

        // `--reset-alpn` ⇒ Some(AlpnProtocols { values: [] }) = "reset to default"
        // `--alpn-protocols h2,http/1.1` ⇒ Some(AlpnProtocols { values: [...] })
        // neither ⇒ None = "preserve current value"
        let alpn_protocols_patch = if reset_alpn {
            Some(AlpnProtocols { values: vec![] })
        } else {
            alpn_protocols.map(|values| AlpnProtocols { values })
        };

        let http_answers = build_http_answers(
            answer_301, answer_401, answer_404, answer_408, answer_413, answer_421, answer_429,
            answer_502, answer_503, answer_504, answer_507,
        )?;

        // Reuses the same builder as `frontend https add` so the
        // mutual-exclusion rules and the canonical
        // `DEFAULT_HSTS_MAX_AGE` substitution stay in lock-step
        // between the two CLI surfaces.
        let hsts = build_hsts_from_cli(
            hsts_max_age,
            hsts_include_subdomains,
            hsts_preload,
            hsts_disabled,
            hsts_force_replace_backend,
        )?;

        let patch = UpdateHttpsListenerConfig {
            address: address.into(),
            public_address: public_address.map(|a| a.into()),
            expect_proxy,
            sticky_name,
            front_timeout,
            back_timeout,
            connect_timeout,
            request_timeout,
            http_answers,
            alpn_protocols: alpn_protocols_patch,
            strict_sni_binding,
            disable_http11,
            h2_max_rst_stream_per_window,
            h2_max_ping_per_window,
            h2_max_settings_per_window,
            h2_max_empty_data_per_window,
            h2_max_continuation_frames,
            h2_max_glitch_count,
            h2_initial_connection_window,
            h2_max_concurrent_streams,
            h2_stream_shrink_ratio,
            h2_max_rst_stream_lifetime,
            h2_max_rst_stream_abusive_lifetime,
            h2_max_rst_stream_emitted_lifetime,
            h2_max_header_list_size,
            h2_max_header_table_size,
            h2_stream_idle_timeout_seconds,
            h2_graceful_shutdown_deadline_seconds,
            h2_max_window_update_stream0_per_window,
            sozu_id_header,
            hsts,
            ..Default::default()
        };
        self.send_request(RequestType::UpdateHttpsListener(patch).into())
    }

    /// Patch a running TCP listener in place. Only `Some` fields in the patch
    /// are applied; `None` fields preserve the listener's current value.
    pub fn update_tcp_listener_command(
        &mut self,
        address: std::net::SocketAddr,
        public_address: Option<std::net::SocketAddr>,
        front_timeout: Option<u32>,
        back_timeout: Option<u32>,
        connect_timeout: Option<u32>,
        expect_proxy_flag: bool,
        no_expect_proxy_flag: bool,
    ) -> Result<(), CtlError> {
        let expect_proxy = if expect_proxy_flag {
            Some(true)
        } else if no_expect_proxy_flag {
            Some(false)
        } else {
            None
        };

        let patch = UpdateTcpListenerConfig {
            address: address.into(),
            public_address: public_address.map(|a| a.into()),
            expect_proxy,
            front_timeout,
            back_timeout,
            connect_timeout,
        };
        self.send_request(RequestType::UpdateTcpListener(patch).into())
    }

    pub fn remove_listener(
        &mut self,
        address: SocketAddress,
        listener_type: ListenerType,
    ) -> Result<(), CtlError> {
        self.send_request(
            RequestType::RemoveListener(RemoveListener {
                address,
                proxy: listener_type.into(),
            })
            .into(),
        )
    }

    pub fn activate_listener(
        &mut self,
        address: SocketAddress,
        listener_type: ListenerType,
    ) -> Result<(), CtlError> {
        self.send_request(
            RequestType::ActivateListener(ActivateListener {
                address,
                proxy: listener_type.into(),
                from_scm: false,
            })
            .into(),
        )
    }

    pub fn deactivate_listener(
        &mut self,
        address: SocketAddress,
        listener_type: ListenerType,
    ) -> Result<(), CtlError> {
        self.send_request(
            RequestType::DeactivateListener(DeactivateListener {
                address,
                proxy: listener_type.into(),
                to_scm: false,
            })
            .into(),
        )
    }

    pub fn logging_filter(&mut self, filter: String) -> Result<(), CtlError> {
        self.send_request(RequestType::Logging(filter).into())
    }

    pub fn add_certificate(
        &mut self,
        address: SocketAddress,
        certificate_path: &str,
        certificate_chain_path: &str,
        key_path: &str,
        versions: Vec<TlsVersion>,
    ) -> Result<(), CtlError> {
        let new_certificate = load_full_certificate(
            certificate_path,
            certificate_chain_path,
            key_path,
            versions,
            vec![],
        )
        .map_err(CtlError::LoadCertificate)?;

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
        address: SocketAddress,
        new_certificate_path: &str,
        new_certificate_chain_path: &str,
        new_key_path: &str,
        old_certificate_path: Option<&str>,
        old_fingerprint: Option<&str>,
        versions: Vec<TlsVersion>,
    ) -> Result<(), CtlError> {
        let old_fingerprint = match (old_certificate_path, old_fingerprint) {
            (None, None) | (Some(_), Some(_)) => {
                return Err(CtlError::ArgsNeeded(
                    "the path to the old certificate".to_string(),
                    "the path to the old fingerprint".to_string(),
                ));
            }
            (Some(old_certificate_path), None) => {
                get_fingerprint_from_certificate_path(old_certificate_path)
                    .map_err(CtlError::GetFingerprint)?
            }
            (None, Some(fingerprint)) => {
                decode_fingerprint(fingerprint).map_err(CtlError::DecodeFingerprint)?
            }
        };

        let new_certificate = load_full_certificate(
            new_certificate_path,
            new_certificate_chain_path,
            new_key_path,
            versions,
            vec![],
        )
        .map_err(CtlError::LoadCertificate)?;

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
        address: SocketAddress,
        certificate_path: Option<&str>,
        fingerprint: Option<&str>,
    ) -> Result<(), CtlError> {
        let fingerprint = match (certificate_path, fingerprint) {
            (None, None) | (Some(_), Some(_)) => {
                return Err(CtlError::ArgsNeeded(
                    "the path to the certificate".to_string(),
                    "the fingerprint of the certificate".to_string(),
                ));
            }
            (Some(certificate_path), None) => {
                get_fingerprint_from_certificate_path(certificate_path)
                    .map_err(CtlError::GetFingerprint)?
            }
            (None, Some(fingerprint)) => {
                decode_fingerprint(fingerprint).map_err(CtlError::DecodeFingerprint)?
            }
        };

        self.send_request(
            RequestType::RemoveCertificate(RemoveCertificate {
                address,
                fingerprint: fingerprint.to_string(),
            })
            .into(),
        )
    }

    pub fn query_certificates(
        &mut self,
        fingerprint: Option<String>,
        domain: Option<String>,
        query_workers: bool,
    ) -> Result<(), CtlError> {
        let filters = QueryCertificatesFilters {
            domain,
            fingerprint,
        };

        if query_workers {
            self.send_request(RequestType::QueryCertificatesFromWorkers(filters).into())
        } else {
            self.send_request(RequestType::QueryCertificatesFromTheState(filters).into())
        }
    }

    pub fn upgrade_worker(&mut self, worker_id: u32) -> Result<(), CtlError> {
        debug!("upgrading worker {}", worker_id);
        self.send_request(RequestType::UpgradeWorker(worker_id).into())
    }

    /// Drives `sozu connection-limit {set|remove|show}` from the CLI to
    /// the worker via the command socket. The setter is non-sticky:
    /// workers reset to the TOML-configured value on restart, so
    /// operators must mirror the change in the config to make it
    /// durable. The query path returns the live in-memory value.
    pub fn connection_limit_command(&mut self, cmd: ConnectionLimitCmd) -> Result<(), CtlError> {
        match cmd {
            ConnectionLimitCmd::Set { limit } => {
                self.send_request(RequestType::SetMaxConnectionsPerIp(limit).into())
            }
            ConnectionLimitCmd::Remove => {
                self.send_request(RequestType::SetMaxConnectionsPerIp(0).into())
            }
            ConnectionLimitCmd::Show => self.send_request(
                RequestType::QueryMaxConnectionsPerIp(QueryMaxConnectionsPerIp {}).into(),
            ),
        }
    }
}

fn find_cluster_configuration(content_type: ContentType, cluster_id: &str) -> Option<Cluster> {
    match content_type {
        ContentType::Clusters(infos) => infos
            .vec
            .into_iter()
            .find(|info| {
                info.configuration
                    .as_ref()
                    .is_some_and(|cluster| cluster.cluster_id == cluster_id)
            })
            .and_then(|info| info.configuration),
        ContentType::WorkerResponses(mut worker_responses) => {
            if let Some(content) = worker_responses.map.remove("main") {
                return content
                    .content_type
                    .and_then(|content_type| find_cluster_configuration(content_type, cluster_id));
            }

            worker_responses.map.into_values().find_map(|content| {
                content
                    .content_type
                    .and_then(|content_type| find_cluster_configuration(content_type, cluster_id))
            })
        }
        _ => None,
    }
}

/// Build a [`RequestHttpFrontend`] from the policy fields collected by
/// `HttpFrontendCmd::Add`. The same builder feeds both
/// `RequestType::AddHttpFrontend` and `RequestType::AddHttpsFrontend` so
/// the two `frontend {http,https} add` paths stay in lock-step.
///
/// Validates each policy field up front and returns a typed
/// [`CtlError::ArgsNeeded`] on a malformed input rather than letting a
/// silent default reach the worker.
fn build_http_frontend_add(cmd: HttpFrontendCmd) -> Result<RequestHttpFrontend, CtlError> {
    let HttpFrontendCmd::Add {
        hostname,
        path_prefix,
        path_regex,
        path_equals,
        address,
        method,
        cluster_id: route,
        tags,
        redirect,
        redirect_scheme,
        redirect_template,
        rewrite_host,
        rewrite_path,
        rewrite_port,
        required_auth,
        header,
        hsts_max_age,
        hsts_include_subdomains,
        hsts_preload,
        hsts_disabled,
        hsts_force_replace_backend,
    } = cmd
    else {
        return Err(CtlError::ArgsNeeded(
            "HttpFrontendCmd::Add".to_owned(),
            "got non-Add variant — should be unreachable".to_owned(),
        ));
    };

    // Map `--redirect <forward|permanent|unauthorized>` onto the proto
    // enum. Unknown / mistyped value surfaces a typed error here.
    let redirect_proto = match redirect.as_deref() {
        None => None,
        Some(s) => Some(match s.to_ascii_lowercase().as_str() {
            "forward" => sozu_command_lib::proto::command::RedirectPolicy::Forward as i32,
            "permanent" => sozu_command_lib::proto::command::RedirectPolicy::Permanent as i32,
            "unauthorized" => sozu_command_lib::proto::command::RedirectPolicy::Unauthorized as i32,
            other => {
                return Err(CtlError::ArgsNeeded(
                    "redirect in {forward, permanent, unauthorized}".to_owned(),
                    format!("got --redirect={other:?}"),
                ));
            }
        }),
    };

    let redirect_scheme_proto = match redirect_scheme.as_deref() {
        None => None,
        Some(s) => Some(match s.to_ascii_lowercase().as_str() {
            "use-same" | "use_same" => {
                sozu_command_lib::proto::command::RedirectScheme::UseSame as i32
            }
            "use-http" | "use_http" => {
                sozu_command_lib::proto::command::RedirectScheme::UseHttp as i32
            }
            "use-https" | "use_https" => {
                sozu_command_lib::proto::command::RedirectScheme::UseHttps as i32
            }
            other => {
                return Err(CtlError::ArgsNeeded(
                    "redirect-scheme in {use-same, use-http, use-https}".to_owned(),
                    format!("got --redirect-scheme={other:?}"),
                ));
            }
        }),
    };

    if let Some(port) = rewrite_port {
        if port == 0 || port > u16::MAX as u32 {
            return Err(CtlError::ArgsNeeded(
                "TCP port in 1..=65535".to_owned(),
                format!("got rewrite_port={port}"),
            ));
        }
    }

    // Each `--header` entry is `<position>=<key>=<value>`. Split on the
    // first two `=` so the value may contain further `=` bytes (common
    // in cookie / quoted strings). Empty value preserves HAProxy
    // `del-header` parity.
    let mut headers_proto = Vec::with_capacity(header.len());
    for (index, raw) in header.iter().enumerate() {
        let (position, rest) = raw.split_once('=').ok_or_else(|| {
            CtlError::ArgsNeeded(
                "<position>=<name>=<value>".to_owned(),
                format!("--header[{index}] = {raw:?} (missing first `=`)"),
            )
        })?;
        let (key, val) = rest.split_once('=').ok_or_else(|| {
            CtlError::ArgsNeeded(
                "<position>=<name>=<value>".to_owned(),
                format!("--header[{index}] = {raw:?} (missing second `=`)"),
            )
        })?;
        let position_proto = match position.to_ascii_lowercase().as_str() {
            "request" => sozu_command_lib::proto::command::HeaderPosition::Request as i32,
            "response" => sozu_command_lib::proto::command::HeaderPosition::Response as i32,
            "both" => sozu_command_lib::proto::command::HeaderPosition::Both as i32,
            other => {
                return Err(CtlError::ArgsNeeded(
                    "header position in {request, response, both}".to_owned(),
                    format!("--header[{index}] position={other:?}"),
                ));
            }
        };
        // Reject CRLF / NUL / other C0 controls. CRLF in the value would
        // let an operator (or a misuse via piped CLI args) splice
        // arbitrary header / request lines into the H1 wire on the
        // backend side (CWE-113 / request smuggling). The H2 emission
        // path filters values at runtime; the H1 path serialises raw,
        // so we reject at CLI parse time as a defense in depth and to
        // give a clear error.
        //
        // The KEY check is stricter than the value check — RFC 9110
        // §5.1 field names follow the `token` grammar (no HTAB, no SP,
        // no C0 controls); reusing the value-side predicate would let
        // `Host\t` slip through and produce an invalid header line on
        // the wire (security review follow-up on `da845c71`).
        if !is_valid_header_name(key.as_bytes()) {
            return Err(CtlError::ArgsNeeded(
                "header key matching RFC 9110 token grammar (alphanumeric or one of !#$%&'*+-.^_`|~)".to_owned(),
                format!("--header[{index}] key={key:?}"),
            ));
        }
        if header_value_has_control_byte(val.as_bytes()) {
            return Err(CtlError::ArgsNeeded(
                "header value without control characters (NUL / CR / LF / other C0)".to_owned(),
                format!("--header[{index}] val={val:?}"),
            ));
        }
        headers_proto.push(sozu_command_lib::proto::command::Header {
            position: position_proto,
            key: key.to_owned(),
            val: val.to_owned(),
        });
    }

    // Build the typed HSTS block from the four `--hsts-*` flags. Layering:
    // - `--hsts-disabled` is mutually exclusive with the enabling flags;
    //   if combined we error rather than silently picking one (operator
    //   intent is ambiguous).
    // - any of `--hsts-max-age`, `--hsts-include-subdomains`,
    //   `--hsts-preload` flips the frontend into "explicit enable"; the
    //   worker substitutes `DEFAULT_HSTS_MAX_AGE = 31_536_000` if
    //   `--hsts-max-age` is omitted (matching the TOML semantics in
    //   `command/src/config.rs::FileHstsConfig::to_proto`).
    // - none of the flags = `None` = inherit listener default.
    let hsts_proto = build_hsts_from_cli(
        hsts_max_age,
        hsts_include_subdomains,
        hsts_preload,
        hsts_disabled,
        hsts_force_replace_backend,
    )?;

    Ok(RequestHttpFrontend {
        cluster_id: route.into(),
        address: address.into(),
        hostname,
        path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
        method,
        position: RulePosition::Tree.into(),
        tags: tags.unwrap_or_default(),
        redirect: redirect_proto,
        redirect_scheme: redirect_scheme_proto,
        redirect_template,
        rewrite_host,
        rewrite_path,
        rewrite_port,
        required_auth: if required_auth { Some(true) } else { None },
        headers: headers_proto,
        hsts: hsts_proto,
    })
}

/// Combine the four `--hsts-*` CLI flags into an `Option<HstsConfig>`.
/// `None` = inherit listener default; `Some(HstsConfig { enabled: Some(false), .. })`
/// = explicit disable (suppresses listener default); `Some(HstsConfig { enabled: Some(true), .. })`
/// = explicit enable with the specified knobs (the helper substitutes the
/// canonical `DEFAULT_HSTS_MAX_AGE = 31_536_000` here so the IPC payload
/// is self-contained — the worker no longer needs to apply a default for
/// CLI-built frontends; only the TOML loader path still substitutes there
/// because `FileHstsConfig` carries the typed `Option<u32>` shape).
/// `--hsts-disabled` mutually excludes the three enabling flags; the
/// function returns a typed `CtlError::ArgsNeeded` rather than silently
/// picking an interpretation.
fn build_hsts_from_cli(
    max_age: Option<u32>,
    include_subdomains: bool,
    preload: bool,
    disabled: bool,
    force_replace_backend: bool,
) -> Result<Option<sozu_command_lib::proto::command::HstsConfig>, CtlError> {
    use sozu_command_lib::proto::command::HstsConfig;

    let any_enabling = max_age.is_some() || include_subdomains || preload || force_replace_backend;
    if disabled && any_enabling {
        return Err(CtlError::ArgsNeeded(
            "either --hsts-disabled OR (--hsts-max-age | --hsts-include-subdomains | --hsts-preload | --hsts-force-replace-backend) — not both"
                .to_owned(),
            "got --hsts-disabled together with one of the enabling flags".to_owned(),
        ));
    }
    if disabled {
        return Ok(Some(HstsConfig {
            enabled: Some(false),
            max_age: None,
            include_subdomains: None,
            preload: None,
            force_replace_backend: None,
        }));
    }
    if !any_enabling {
        return Ok(None);
    }
    Ok(Some(HstsConfig {
        enabled: Some(true),
        // Substitute the canonical default at CLI-build time so the
        // resulting IPC payload renders end-to-end without the worker
        // having to re-apply a default. Mirrors the TOML loader's
        // substitution in `FileHstsConfig::to_proto`.
        max_age: max_age.or(Some(sozu_command_lib::config::DEFAULT_HSTS_MAX_AGE)),
        include_subdomains: if include_subdomains { Some(true) } else { None },
        preload: if preload { Some(true) } else { None },
        force_replace_backend: if force_replace_backend {
            Some(true)
        } else {
            None
        },
    }))
}

#[cfg(test)]
mod hsts_cli_tests {
    use super::*;
    use sozu_command_lib::config::DEFAULT_HSTS_MAX_AGE;

    #[test]
    fn no_flags_returns_none() {
        // No `--hsts-*` flags = inherit listener default.
        assert!(matches!(
            build_hsts_from_cli(None, false, false, false, false),
            Ok(None)
        ));
    }

    #[test]
    fn disabled_only_returns_some_disabled() {
        let out = build_hsts_from_cli(None, false, false, true, false)
            .expect("should validate")
            .expect("should be Some");
        assert_eq!(out.enabled, Some(false));
        assert_eq!(out.max_age, None);
        assert_eq!(out.include_subdomains, None);
        assert_eq!(out.preload, None);
        assert_eq!(out.force_replace_backend, None);
    }

    #[test]
    fn partial_enabling_substitutes_default_max_age() {
        // Operator opted into HSTS via --hsts-include-subdomains alone;
        // helper must substitute the canonical default so the worker
        // does not see a `max_age = None` and silently no-op the render.
        let out = build_hsts_from_cli(None, true, false, false, false)
            .expect("should validate")
            .expect("should be Some");
        assert_eq!(out.enabled, Some(true));
        assert_eq!(out.max_age, Some(DEFAULT_HSTS_MAX_AGE));
        assert_eq!(out.include_subdomains, Some(true));
        assert_eq!(out.force_replace_backend, None);
    }

    #[test]
    fn explicit_max_age_kept() {
        let out = build_hsts_from_cli(Some(63_072_000), true, true, false, false)
            .expect("should validate")
            .expect("should be Some");
        assert_eq!(out.max_age, Some(63_072_000));
        assert_eq!(out.preload, Some(true));
    }

    #[test]
    fn force_replace_backend_alone_enables_with_default_max_age() {
        // Setting --hsts-force-replace-backend on its own is also an
        // enabling flag — operator wants override semantics; the
        // canonical default max-age applies.
        let out = build_hsts_from_cli(None, false, false, false, true)
            .expect("should validate")
            .expect("should be Some");
        assert_eq!(out.enabled, Some(true));
        assert_eq!(out.max_age, Some(DEFAULT_HSTS_MAX_AGE));
        assert_eq!(out.force_replace_backend, Some(true));
    }

    #[test]
    fn disabled_with_force_replace_returns_args_needed() {
        match build_hsts_from_cli(None, false, false, true, true).unwrap_err() {
            CtlError::ArgsNeeded(_, _) => {}
            other => panic!("expected ArgsNeeded, got {other:?}"),
        }
    }

    #[test]
    fn disabled_with_enabling_flags_returns_args_needed() {
        match build_hsts_from_cli(Some(31_536_000), false, false, true, false).unwrap_err() {
            CtlError::ArgsNeeded(_, _) => {}
            other => panic!("expected ArgsNeeded, got {other:?}"),
        }
    }
}

/// Reject NUL / CR / LF / other C0 controls in a header value. HTAB
/// (`\x09`) is permitted per RFC 9110 §5.5 and matches the runtime
/// filter at `lib::protocol::mux::converter::call`.
fn header_value_has_control_byte(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|&b| matches!(b, 0x00..=0x08 | 0x0A..=0x1F | 0x7F))
}

/// Header field names follow the RFC 9110 §5.1 `token` grammar: a
/// non-empty sequence of `tchar` bytes (alphanumeric plus a closed
/// punctuation list). HTAB and SP are NOT tchar — they belong to
/// field-VALUE grammar. Reusing `header_value_has_control_byte` for
/// keys would let `Host\t` slip through and produce an invalid header
/// line on the H1 backend wire.
fn is_valid_header_name(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    bytes.iter().all(|&b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
    })
}

/// Validate that `s` matches the canonical `<user>:<hex(sha256)>` form
/// the worker stores in `Cluster.authorized_hashes`. Equivalent to the
/// regex `^[A-Za-z0-9_\-]+:[0-9a-f]{64}$`; inlined as bytes so we don't
/// pull `regex` in here for a one-line validator.
pub(crate) fn looks_like_authorized_hash(s: &str) -> bool {
    let Some((user, hex)) = s.split_once(':') else {
        return false;
    };
    !user.is_empty()
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Load the content of an HTTP-answer file path. Returns `None` if the path is
/// `None`. Mirrors the logic in `command/src/config.rs::read_http_answer_file`.
fn read_answer_path(path: &Option<PathBuf>) -> Result<Option<String>, CtlError> {
    let Some(path) = path else { return Ok(None) };
    let mut content = String::new();
    File::open(path)
        .and_then(|mut f| f.read_to_string(&mut content))
        .map_err(|io_error| CtlError::ResolvePath(path.display().to_string(), io_error))?;
    Ok(Some(content))
}

/// Build a [`CustomHttpAnswers`] proto from optional file-path arguments.
/// Returns `None` when every path argument is `None` (no-op on the listener).
#[allow(clippy::too_many_arguments)]
fn build_http_answers(
    answer_301: Option<PathBuf>,
    answer_401: Option<PathBuf>,
    answer_404: Option<PathBuf>,
    answer_408: Option<PathBuf>,
    answer_413: Option<PathBuf>,
    answer_421: Option<PathBuf>,
    answer_429: Option<PathBuf>,
    answer_502: Option<PathBuf>,
    answer_503: Option<PathBuf>,
    answer_504: Option<PathBuf>,
    answer_507: Option<PathBuf>,
) -> Result<Option<CustomHttpAnswers>, CtlError> {
    // Only produce the sub-message when at least one path was given.
    if answer_301.is_none()
        && answer_401.is_none()
        && answer_404.is_none()
        && answer_408.is_none()
        && answer_413.is_none()
        && answer_421.is_none()
        && answer_429.is_none()
        && answer_502.is_none()
        && answer_503.is_none()
        && answer_504.is_none()
        && answer_507.is_none()
    {
        return Ok(None);
    }
    Ok(Some(CustomHttpAnswers {
        answer_301: read_answer_path(&answer_301)?,
        answer_400: None, // not exposed via the CLI update verb
        answer_401: read_answer_path(&answer_401)?,
        answer_404: read_answer_path(&answer_404)?,
        answer_408: read_answer_path(&answer_408)?,
        answer_413: read_answer_path(&answer_413)?,
        answer_421: read_answer_path(&answer_421)?,
        answer_429: read_answer_path(&answer_429)?,
        answer_502: read_answer_path(&answer_502)?,
        answer_503: read_answer_path(&answer_503)?,
        answer_504: read_answer_path(&answer_504)?,
        answer_507: read_answer_path(&answer_507)?,
    }))
}

#[cfg(test)]
mod tests {
    use sozu_command_lib::proto::command::{
        ClusterInformation, ClusterInformations, LoadBalancingAlgorithms, ResponseContent,
        WorkerResponses,
    };

    use super::*;

    fn cluster(cluster_id: &str, sticky_session: bool) -> Cluster {
        Cluster {
            cluster_id: cluster_id.to_owned(),
            sticky_session,
            https_redirect: false,
            load_balancing: LoadBalancingAlgorithms::RoundRobin as i32,
            ..Default::default()
        }
    }

    fn clusters_response(clusters: Vec<Cluster>) -> ContentType {
        ContentType::Clusters(ClusterInformations {
            vec: clusters
                .into_iter()
                .map(|cluster| ClusterInformation {
                    configuration: Some(cluster),
                    ..Default::default()
                })
                .collect(),
        })
    }

    fn response_content(content_type: ContentType) -> ResponseContent {
        ResponseContent {
            content_type: Some(content_type),
        }
    }

    #[test]
    fn finds_cluster_configuration_from_direct_cluster_response() {
        let found = find_cluster_configuration(
            clusters_response(vec![cluster("other", false), cluster("target", true)]),
            "target",
        );

        assert_eq!(found.map(|cluster| cluster.sticky_session), Some(true));
    }

    #[test]
    fn finds_cluster_configuration_from_main_worker_response() {
        let mut map = BTreeMap::new();
        map.insert(
            "0".to_owned(),
            response_content(clusters_response(vec![cluster("target", true)])),
        );
        map.insert(
            "main".to_owned(),
            response_content(clusters_response(vec![cluster("target", false)])),
        );

        let found = find_cluster_configuration(
            ContentType::WorkerResponses(WorkerResponses { map }),
            "target",
        );

        assert_eq!(found.map(|cluster| cluster.sticky_session), Some(false));
    }

    #[test]
    fn falls_back_to_worker_cluster_response_when_main_is_absent() {
        let mut map = BTreeMap::new();
        map.insert(
            "0".to_owned(),
            response_content(clusters_response(vec![cluster("target", true)])),
        );

        let found = find_cluster_configuration(
            ContentType::WorkerResponses(WorkerResponses { map }),
            "target",
        );

        assert_eq!(
            found.map(|cluster| cluster.cluster_id),
            Some("target".to_owned())
        );
    }

    #[test]
    fn does_not_fall_back_to_worker_when_main_is_present() {
        let mut map = BTreeMap::new();
        map.insert(
            "0".to_owned(),
            response_content(clusters_response(vec![cluster("target", true)])),
        );
        map.insert(
            "main".to_owned(),
            response_content(clusters_response(vec![cluster("other", false)])),
        );

        let found = find_cluster_configuration(
            ContentType::WorkerResponses(WorkerResponses { map }),
            "target",
        );

        assert!(found.is_none());
    }

    #[test]
    fn returns_none_when_cluster_is_missing() {
        let found =
            find_cluster_configuration(clusters_response(vec![cluster("other", false)]), "target");

        assert!(found.is_none());
    }

    #[test]
    fn accepts_canonical_user_hex64() {
        assert!(looks_like_authorized_hash(
            "admin:2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"
        ));
    }

    #[test]
    fn rejects_missing_colon() {
        assert!(!looks_like_authorized_hash("admin"));
    }

    #[test]
    fn rejects_short_hex_tail() {
        assert!(!looks_like_authorized_hash("admin:deadbeef"));
    }

    #[test]
    fn rejects_uppercase_hex() {
        assert!(!looks_like_authorized_hash(
            "admin:2BB80D537B1DA3E38BD30361AA855686BDE0EACD7162FEF6A25FE97BF527A25B"
        ));
    }

    #[test]
    fn rejects_empty_username() {
        assert!(!looks_like_authorized_hash(
            ":2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"
        ));
    }

    #[test]
    fn rejects_non_alnum_username() {
        assert!(!looks_like_authorized_hash(
            "admin user:2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"
        ));
    }
}
