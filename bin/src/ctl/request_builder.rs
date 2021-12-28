use sozu_command_lib::{
    certificate::{calculate_fingerprint, split_certificate_chain},
    command::{
        CommandRequest, CommandRequestData, CommandResponse, CommandResponseData, CommandStatus,
        FrontendFilters, RunState, WorkerInfo,
    },
    config::{Config, FileListenerProtocolConfig, Listener, ProxyProtocolConfig},
    proxy::{
        ActivateListener, AddCertificate, Backend, CertificateAndKey, CertificateFingerprint,
        Cluster, DeactivateListener, FilteredData, HttpFrontend, ListenerType,
        LoadBalancingAlgorithms, LoadBalancingParams, MetricsConfiguration, PathRule,
        ProxyRequestData, Query, QueryAnswer, QueryAnswerCertificate, QueryAnswerMetrics,
        QueryApplicationDomain, QueryApplicationType, QueryCertificateType, QueryMetricsType,
        RemoveBackend, RemoveCertificate, RemoveListener, ReplaceCertificate, Route, RulePosition,
        TcpFrontend, TcpListener, TlsVersion,
    },
};

use anyhow::{bail, Context};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
    process::exit,
    sync::mpsc,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use crate::{
    cli::{HttpFrontendCmd, LoggingLevel, MetricsCmd},
    ctl::{
        create_channel,
        display::{
            create_queried_application_table, print_frontend_list, print_json_response,
            print_query_answers,
        },
        CommandManager,
    },
};

impl CommandManager {
    pub fn add_backend(
        &mut self,
        cluster_id: &str,
        backend_id: &str,
        address: SocketAddr,
        sticky_id: Option<String>,
        backup: Option<bool>,
    ) -> Result<(), anyhow::Error> {
        self.order_command(ProxyRequestData::AddBackend(Backend {
            cluster_id: String::from(cluster_id),
            address: address,
            backend_id: String::from(backend_id),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: sticky_id,
            backup: backup,
        }))
    }

    pub fn remove_backend(
        &mut self,
        cluster_id: &str,
        backend_id: &str,
        address: SocketAddr,
    ) -> Result<(), anyhow::Error> {
        self.order_command(ProxyRequestData::RemoveBackend(RemoveBackend {
            cluster_id: String::from(cluster_id),
            address: address,
            backend_id: String::from(backend_id),
        }))
    }

    pub fn add_application(
        &mut self,
        cluster_id: &str,
        sticky_session: bool,
        https_redirect: bool,
        send_proxy: bool,
        expect_proxy: bool,
        load_balancing: LoadBalancingAlgorithms,
    ) -> Result<(), anyhow::Error> {
        let proxy_protocol = match (send_proxy, expect_proxy) {
            (true, true) => Some(ProxyProtocolConfig::RelayHeader),
            (true, false) => Some(ProxyProtocolConfig::SendHeader),
            (false, true) => Some(ProxyProtocolConfig::ExpectHeader),
            _ => None,
        };

        self.order_command(ProxyRequestData::AddCluster(Cluster {
            cluster_id: String::from(cluster_id),
            sticky_session,
            https_redirect,
            proxy_protocol,
            load_balancing,
            load_metric: None,
            answer_503: None,
        }))
    }

    pub fn remove_application(&mut self, cluster_id: &str) -> Result<(), anyhow::Error> {
        self.order_command(ProxyRequestData::RemoveCluster {
            cluster_id: String::from(cluster_id),
        })
    }

    pub fn add_tcp_frontend(
        &mut self,
        cluster_id: &str,
        address: SocketAddr,
    ) -> Result<(), anyhow::Error> {
        self.order_command(ProxyRequestData::AddTcpFrontend(TcpFrontend {
            cluster_id: String::from(cluster_id),
            address,
        }))
    }

    pub fn remove_tcp_frontend(
        &mut self,
        cluster_id: &str,
        address: SocketAddr,
    ) -> Result<(), anyhow::Error> {
        self.order_command(ProxyRequestData::RemoveTcpFrontend(TcpFrontend {
            cluster_id: String::from(cluster_id),
            address,
        }))
    }

    pub fn http_frontend_command(&mut self, cmd: HttpFrontendCmd) -> Result<(), anyhow::Error> {
        match cmd {
            HttpFrontendCmd::Add {
                hostname,
                path_begin,
                address,
                method,
                route,
            } => self.order_command(ProxyRequestData::AddHttpFrontend(HttpFrontend {
                route: route.into(),
                address,
                hostname: String::from(hostname),
                path: PathRule::Prefix(String::from(&path_begin.unwrap_or("".to_string()))),
                method: method.map(String::from),
                position: RulePosition::Tree,
            })),

            HttpFrontendCmd::Remove {
                hostname,
                path_begin,
                address,
                method,
                route,
            } => self.order_command(ProxyRequestData::RemoveHttpFrontend(HttpFrontend {
                route: route.into(),
                address,
                hostname: String::from(hostname),
                path: PathRule::Prefix(String::from(&path_begin.unwrap_or("".to_string()))),
                method: method.map(String::from),
                position: RulePosition::Tree,
            })),
        }
    }

    pub fn https_frontend_command(&mut self, cmd: HttpFrontendCmd) -> Result<(), anyhow::Error> {
        match cmd {
            HttpFrontendCmd::Add {
                hostname,
                path_begin,
                address,
                method,
                route,
            } => self.order_command(ProxyRequestData::AddHttpsFrontend(HttpFrontend {
                route: route.into(),
                address,
                hostname: String::from(hostname),
                path: PathRule::Prefix(String::from(path_begin.unwrap_or("".to_string()))),
                method: method.map(String::from),
                position: RulePosition::Tree,
            })),
            HttpFrontendCmd::Remove {
                hostname,
                path_begin,
                address,
                method,
                route,
            } => self.order_command(ProxyRequestData::RemoveHttpsFrontend(HttpFrontend {
                route: route.into(),
                address,
                hostname: String::from(hostname),
                path: PathRule::Prefix(String::from(path_begin.unwrap_or("".to_string()))),
                method: method.map(String::from),
                position: RulePosition::Tree,
            })),
        }
    }

    pub fn add_http_listener(
        &mut self,
        address: SocketAddr,
        public_address: Option<SocketAddr>,
        answer_404: Option<String>,
        answer_503: Option<String>,
        expect_proxy: bool,
        sticky_name: Option<String>,
    ) -> Result<(), anyhow::Error> {
        let mut listener = Listener::new(address, FileListenerProtocolConfig::Http);
        listener.public_address = public_address;
        listener.answer_404 = answer_404;
        listener.answer_503 = answer_503;
        listener.expect_proxy = Some(expect_proxy);
        if let Some(sticky_name) = sticky_name {
            listener.sticky_name = sticky_name;
        }

        match listener.to_http(None, None, None) {
            Some(conf) => self.order_command(ProxyRequestData::AddHttpListener(conf)),
            None => bail!("Error creating HTTP listener"),
        }
    }

    pub fn add_https_listener(
        &mut self,
        address: SocketAddr,
        public_address: Option<SocketAddr>,
        answer_404: Option<String>,
        answer_503: Option<String>,
        tls_versions: Vec<TlsVersion>,
        cipher_list: Option<String>,
        rustls_cipher_list: Vec<String>,
        expect_proxy: bool,
        sticky_name: Option<String>,
    ) -> Result<(), anyhow::Error> {
        let mut listener = Listener::new(address, FileListenerProtocolConfig::Https);
        listener.public_address = public_address;
        listener.answer_404 = answer_404;
        listener.answer_503 = answer_503;
        listener.expect_proxy = Some(expect_proxy);
        if let Some(sticky_name) = sticky_name {
            listener.sticky_name = sticky_name;
        }
        listener.cipher_list = cipher_list;
        listener.tls_versions = if tls_versions.len() == 0 {
            None
        } else {
            Some(tls_versions)
        };
        listener.rustls_cipher_list = if rustls_cipher_list.len() == 0 {
            None
        } else {
            Some(rustls_cipher_list)
        };

        match listener.to_tls(None, None, None) {
            Some(conf) => self.order_command(ProxyRequestData::AddHttpsListener(conf)),
            None => bail!("Error creating HTTPS listener"),
        }
    }

    pub fn add_tcp_listener(
        &mut self,
        address: SocketAddr,
        public_address: Option<SocketAddr>,
        expect_proxy: bool,
    ) -> Result<(), anyhow::Error> {
        self.order_command(ProxyRequestData::AddTcpListener(TcpListener {
            address,
            public_address,
            expect_proxy,
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
        }))
    }

    pub fn remove_listener(
        &mut self,
        address: SocketAddr,
        proxy: ListenerType,
    ) -> Result<(), anyhow::Error> {
        self.order_command(ProxyRequestData::RemoveListener(RemoveListener {
            address,
            proxy,
        }))
    }

    pub fn activate_listener(
        &mut self,
        address: SocketAddr,
        proxy: ListenerType,
    ) -> Result<(), anyhow::Error> {
        self.order_command(ProxyRequestData::ActivateListener(ActivateListener {
            address,
            proxy,
            from_scm: false,
        }))
    }

    pub fn deactivate_listener(
        &mut self,
        address: SocketAddr,
        proxy: ListenerType,
    ) -> Result<(), anyhow::Error> {
        self.order_command(ProxyRequestData::DeactivateListener(DeactivateListener {
            address,
            proxy,
            to_scm: false,
        }))
    }

    pub fn logging_filter(&mut self, filter: &LoggingLevel) -> Result<(), anyhow::Error> {
        self.order_command(ProxyRequestData::Logging(filter.to_string().to_lowercase()))
    }

    pub fn add_certificate(
        &mut self,
        address: SocketAddr,
        certificate_path: &str,
        certificate_chain_path: &str,
        key_path: &str,
        versions: Vec<TlsVersion>,
    ) -> Result<(), anyhow::Error> {
        if let Some(new_certificate) =
            load_full_certificate(certificate_path, certificate_chain_path, key_path, versions)?
        {
            self.order_command(ProxyRequestData::AddCertificate(AddCertificate {
                address,
                certificate: new_certificate,
                names: vec![],
                expired_at: None,
            }))?;
        }
        Ok(())
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
    ) -> Result<(), anyhow::Error> {
        if old_certificate_path.is_some() && old_fingerprint.is_some() {
            bail!("Error: Either provide the old certificate's path or its fingerprint");
        }

        if old_certificate_path.is_none() && old_fingerprint.is_none() {
            bail!("Error: Either provide the old certificate's path or its fingerprint");
        }

        if let Some(new_certificate) = load_full_certificate(
            new_certificate_path,
            new_certificate_chain_path,
            new_key_path,
            versions,
        )? {
            if let Some(old_fingerprint) = old_fingerprint.and_then(|s| {
        match hex::decode(s) {
            Ok(v) => Some(CertificateFingerprint(v)),
            Err(e) => {
                eprintln!("Error decoding the certificate fingerprint (expected hexadecimal data): {:?}", e);
                None
            }
        }
    }).or(old_certificate_path.and_then(get_certificate_fingerprint)) {
     self.order_command( ProxyRequestData::ReplaceCertificate(ReplaceCertificate {
        address,
        new_certificate,
        old_fingerprint,
        new_names: vec![],
        new_expired_at: None,
      }))?;
    }
        }
        Ok(())
    }

    pub fn remove_certificate(
        &mut self,
        address: SocketAddr,
        certificate_path: Option<&str>,
        fingerprint: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        if certificate_path.is_some() && fingerprint.is_some() {
            bail!("Error: Either provide the certificate's path or its fingerprint");
        }

        if certificate_path.is_none() && fingerprint.is_none() {
            bail!("Error: Either provide the certificate's path or its fingerprint");
        }

        if let Some(fingerprint) = fingerprint
            .and_then(|s| match hex::decode(s) {
                Ok(v) => Some(CertificateFingerprint(v)),
                Err(e) => {
                    eprintln!(
                    "Error decoding the certificate fingerprint (expected hexadecimal data): {:?}",
                    e
                );
                    None
                }
            })
            .or(certificate_path.and_then(get_certificate_fingerprint))
        {
            self.order_command(ProxyRequestData::RemoveCertificate(RemoveCertificate {
                address,
                fingerprint,
            }))?
        }
        Ok(())
    }
}

fn get_certificate_fingerprint(certificate_path: &str) -> Option<CertificateFingerprint> {
    match Config::load_file_bytes(certificate_path) {
        Ok(data) => match calculate_fingerprint(&data) {
            Some(fingerprint) => Some(CertificateFingerprint(fingerprint)),
            None => {
                eprintln!("could not calculate finrprint for certificate");
                exit(1);
            }
        },
        Err(e) => {
            eprintln!("could not load file: {:?}", e);
            exit(1);
        }
    }
}

fn load_full_certificate(
    certificate_path: &str,
    certificate_chain_path: &str,
    key_path: &str,
    versions: Vec<TlsVersion>,
) -> Result<Option<CertificateAndKey>, anyhow::Error> {
    match Config::load_file(certificate_path) {
        Err(e) => {
            bail!("could not load certificate: {:?}", e);
        }
        Ok(certificate) => {
            match Config::load_file(certificate_chain_path).map(split_certificate_chain) {
                Err(e) => {
                    bail!("could not load certificate chain: {:?}", e);
                }
                Ok(certificate_chain) => match Config::load_file(key_path) {
                    Err(e) => {
                        bail!("could not load key: {:?}", e);
                    }
                    Ok(key) => Ok(Some(CertificateAndKey {
                        certificate,
                        certificate_chain,
                        key,
                        versions,
                    })),
                },
            }
        }
    }
}
