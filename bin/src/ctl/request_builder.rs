use std::{collections::BTreeMap, fs::File, io::Read as IoRead, path::PathBuf};

use sozu_command_lib::{
    certificate::{
        decode_fingerprint, get_fingerprint_from_certificate_path, load_full_certificate,
    },
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddBackend, AddCertificate, AlpnProtocols, Cluster, CountRequests,
        CustomHttpAnswers, DeactivateListener, FrontendFilters, HardStop, ListListeners,
        ListenerType, LoadBalancingParams, MetricsConfiguration, PathRule, ProxyProtocolConfig,
        QueryCertificatesFilters, QueryClusterByDomain, QueryClustersHashes, RemoveBackend,
        RemoveCertificate, RemoveListener, ReplaceCertificate, RequestHttpFrontend,
        RequestTcpFrontend, RulePosition, SocketAddress, SoftStop, Status, SubscribeEvents,
        TlsVersion, UpdateHttpListenerConfig, UpdateHttpsListenerConfig, UpdateTcpListenerConfig,
        request::RequestType, response_content,
    },
};

use super::CtlError;
use crate::{
    cli::{
        BackendCmd, ClusterCmd, ClusterH2Cmd, HttpFrontendCmd, HttpListenerCmd, HttpsListenerCmd,
        MetricsCmd, TcpFrontendCmd, TcpListenerCmd,
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
                        http2: if http2 { Some(true) } else { None },
                        ..Default::default()
                    })
                    .into(),
                )
            }
            ClusterCmd::Remove { id } => self.send_request(RequestType::RemoveCluster(id).into()),
            ClusterCmd::H2 { cmd } => self.cluster_h2_command(cmd),
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
            .and_then(|c| c.content_type)
            .and_then(|ct| match ct {
                response_content::ContentType::Clusters(infos) => infos
                    .vec
                    .into_iter()
                    .find(|info| {
                        info.configuration
                            .as_ref()
                            .is_some_and(|c| c.cluster_id == cluster_id)
                    })
                    .and_then(|info| info.configuration),
                _ => None,
            })
            .ok_or_else(|| {
                CtlError::ArgsNeeded("cluster not found".to_owned(), cluster_id.clone())
            })?;

        let updated = Cluster {
            http2: Some(enable),
            ..cluster
        };

        self.send_request(RequestType::AddCluster(updated).into())
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
            HttpFrontendCmd::Add {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                cluster_id: route,
                tags,
            } => self.send_request(
                RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: route.into(),
                    address: address.into(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method,
                    position: RulePosition::Tree.into(),
                    tags: tags.unwrap_or_default(),
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
            HttpFrontendCmd::Add {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                cluster_id: route,
                tags,
            } => self.send_request(
                RequestType::AddHttpsFrontend(RequestHttpFrontend {
                    cluster_id: route.into(),
                    address: address.into(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method,
                    position: RulePosition::Tree.into(),
                    tags: tags.unwrap_or_default(),
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
                answer_502,
                answer_503,
                answer_504,
                answer_507,
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
                answer_502,
                answer_503,
                answer_504,
                answer_507,
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
            answer_301, answer_401, answer_404, answer_408, answer_413, answer_421, answer_502,
            answer_503, answer_504, answer_507,
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
            answer_301, answer_401, answer_404, answer_408, answer_413, answer_421, answer_502,
            answer_503, answer_504, answer_507,
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
        answer_502: read_answer_path(&answer_502)?,
        answer_503: read_answer_path(&answer_503)?,
        answer_504: read_answer_path(&answer_504)?,
        answer_507: read_answer_path(&answer_507)?,
    }))
}
