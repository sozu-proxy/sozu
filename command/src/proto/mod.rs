use std::collections::BTreeMap;

use command::{
    AggregatedMetrics, BackendMetrics, Bucket, FilteredHistogram, FilteredMetrics, Percentiles,
    filtered_metrics::Inner,
};
use prost::UnknownEnumValue;

/// Contains all types received by and sent from Sōzu
pub mod command;

/// Implementation of fmt::Display for the protobuf types, used in the CLI
pub mod display;

#[derive(thiserror::Error, Debug)]
pub enum DisplayError {
    #[error("Could not display content")]
    DisplayContent(String),
    #[error("Error while parsing response to JSON")]
    Json(serde_json::Error),
    #[error("got the wrong response content type: {0}")]
    WrongResponseType(String),
    #[error("Could not format the datetime to ISO 8601")]
    DateTime,
    #[error("unrecognized protobuf variant: {0}")]
    DecodeError(UnknownEnumValue),
}

// Simple helper to build ResponseContent from ContentType
impl From<command::response_content::ContentType> for command::ResponseContent {
    fn from(value: command::response_content::ContentType) -> Self {
        Self {
            content_type: Some(value),
        }
    }
}

// Simple helper to build Request from RequestType
impl From<command::request::RequestType> for command::Request {
    fn from(value: command::request::RequestType) -> Self {
        Self {
            request_type: Some(value),
        }
    }
}

impl std::fmt::Debug for command::Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut request = f.debug_struct("Request");
        match self.request_type.as_ref() {
            Some(request_type) => request.field("request_type", request_type),
            None => request.field("request_type", &"Unallowed"),
        };
        request.finish()
    }
}

impl std::fmt::Debug for command::request::RequestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use command::request::RequestType;

        match self {
            RequestType::AddCluster(value) => f.debug_tuple("AddCluster").field(value).finish(),
            RequestType::AddHttpFrontend(value) => {
                f.debug_tuple("AddHttpFrontend").field(value).finish()
            }
            RequestType::RemoveHttpFrontend(value) => {
                f.debug_tuple("RemoveHttpFrontend").field(value).finish()
            }
            RequestType::AddHttpsFrontend(value) => {
                f.debug_tuple("AddHttpsFrontend").field(value).finish()
            }
            RequestType::RemoveHttpsFrontend(value) => {
                f.debug_tuple("RemoveHttpsFrontend").field(value).finish()
            }
            RequestType::AddCertificate(value) => {
                f.debug_tuple("AddCertificate").field(value).finish()
            }
            RequestType::ReplaceCertificate(value) => {
                f.debug_tuple("ReplaceCertificate").field(value).finish()
            }
            RequestType::RemoveCertificate(value) => {
                f.debug_tuple("RemoveCertificate").field(value).finish()
            }
            RequestType::AddTcpFrontend(value) => {
                f.debug_tuple("AddTcpFrontend").field(value).finish()
            }
            RequestType::RemoveTcpFrontend(value) => {
                f.debug_tuple("RemoveTcpFrontend").field(value).finish()
            }
            RequestType::AddHttpListener(value) => {
                f.debug_tuple("AddHttpListener").field(value).finish()
            }
            RequestType::AddHttpsListener(value) => {
                f.debug_tuple("AddHttpsListener").field(value).finish()
            }
            RequestType::UpdateHttpListener(value) => {
                f.debug_tuple("UpdateHttpListener").field(value).finish()
            }
            RequestType::UpdateHttpsListener(value) => {
                f.debug_tuple("UpdateHttpsListener").field(value).finish()
            }
            RequestType::QueryCertificatesFromTheState(value) => f
                .debug_tuple("QueryCertificatesFromTheState")
                .field(value)
                .finish(),
            RequestType::QueryCertificatesFromWorkers(value) => f
                .debug_tuple("QueryCertificatesFromWorkers")
                .field(value)
                .finish(),
            other => f.write_str(display::format_request_type(other)),
        }
    }
}

impl std::fmt::Debug for command::CertificateAndKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let certificate_chain_len = self
            .certificate_chain
            .iter()
            .map(String::len)
            .fold(0usize, usize::saturating_add);

        f.debug_struct("CertificateAndKey")
            .field("certificate", &"[redacted]")
            .field("certificate_len", &self.certificate.len())
            .field("certificate_chain", &"[redacted]")
            .field("certificate_chain_count", &self.certificate_chain.len())
            .field("certificate_chain_len", &certificate_chain_len)
            .field("key", &"[redacted]")
            .field("key_len", &self.key.len())
            .field("versions_count", &self.versions.len())
            .field("names_count", &self.names.len())
            .field("names_len", &total_string_len(&self.names))
            .finish()
    }
}

impl std::fmt::Debug for command::RemoveCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoveCertificate")
            .field("address", &self.address)
            .field("fingerprint_len", &self.fingerprint.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for command::ReplaceCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplaceCertificate")
            .field("address", &self.address)
            .field("new_certificate", &self.new_certificate)
            .field("fingerprint_len", &self.old_fingerprint.len())
            .field("new_expired_at", &self.new_expired_at)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for command::Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (answers_count, answers_key_len, answers_value_len) =
            summarize_string_map(&self.answers);

        f.debug_struct("Cluster")
            .field("cluster_id_len", &self.cluster_id.len())
            .field("sticky_session", &self.sticky_session)
            .field("https_redirect", &self.https_redirect)
            .field("proxy_protocol", &self.proxy_protocol)
            .field("load_balancing", &self.load_balancing)
            .field("answer_503_len", &self.answer_503.as_ref().map(String::len))
            .field("load_metric", &self.load_metric)
            .field("http2", &self.http2)
            .field("answers_count", &answers_count)
            .field("answers_key_len", &answers_key_len)
            .field("answers_value_len", &answers_value_len)
            .field("https_redirect_port", &self.https_redirect_port)
            .field("authorized_hashes_count", &self.authorized_hashes.len())
            .field(
                "authorized_hashes_len",
                &total_string_len(&self.authorized_hashes),
            )
            .field(
                "www_authenticate_len",
                &self.www_authenticate.as_ref().map(String::len),
            )
            .field("max_connections_per_ip", &self.max_connections_per_ip)
            .field("retry_after", &self.retry_after)
            .field("health_check_present", &self.health_check.is_some())
            .field("udp_present", &self.udp.is_some())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for command::Header {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Header")
            .field("position", &self.position)
            .field("key_len", &self.key.len())
            .field("val_len", &self.val.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for command::RequestHttpFrontend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tags_len = self.tags.iter().fold(0usize, |total, (key, value)| {
            total.saturating_add(key.len()).saturating_add(value.len())
        });
        let headers_len = self.headers.iter().fold(0usize, |total, header| {
            total
                .saturating_add(header.key.len())
                .saturating_add(header.val.len())
        });

        f.debug_struct("RequestHttpFrontend")
            .field("cluster_id_len", &self.cluster_id.as_ref().map(String::len))
            .field("address", &self.address)
            .field("hostname_len", &self.hostname.len())
            .field("path_kind", &self.path.kind)
            .field("path_len", &self.path.value.len())
            .field("method_len", &self.method.as_ref().map(String::len))
            .field("position", &self.position)
            .field("tags_count", &self.tags.len())
            .field("tags_len", &tags_len)
            .field("redirect", &self.redirect)
            .field("required_auth", &self.required_auth)
            .field("redirect_scheme", &self.redirect_scheme)
            .field(
                "redirect_template_len",
                &self.redirect_template.as_ref().map(String::len),
            )
            .field(
                "rewrite_host_len",
                &self.rewrite_host.as_ref().map(String::len),
            )
            .field(
                "rewrite_path_len",
                &self.rewrite_path.as_ref().map(String::len),
            )
            .field("rewrite_port", &self.rewrite_port)
            .field("headers_count", &self.headers.len())
            .field("headers_len", &headers_len)
            .field("hsts", &self.hsts)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for command::RequestTcpFrontend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (tags_count, tags_key_len, tags_value_len) = summarize_string_map(&self.tags);

        f.debug_struct("RequestTcpFrontend")
            .field("cluster_id_len", &self.cluster_id.len())
            .field("address", &self.address)
            .field("tags_count", &tags_count)
            .field("tags_key_len", &tags_key_len)
            .field("tags_value_len", &tags_value_len)
            .field("sni_len", &self.sni.as_ref().map(String::len))
            .field("alpn_count", &self.alpn.len())
            .field("alpn_len", &total_string_len(&self.alpn))
            .finish_non_exhaustive()
    }
}

fn total_string_len<'a>(values: impl IntoIterator<Item = &'a String>) -> usize {
    values
        .into_iter()
        .map(String::len)
        .fold(0usize, usize::saturating_add)
}

fn summarize_http_answers(
    answers: Option<&command::CustomHttpAnswers>,
) -> (Option<&'static str>, Option<usize>, Option<usize>) {
    let Some(answers) = answers else {
        return (None, None, None);
    };
    let values = [
        answers.answer_301.as_ref(),
        answers.answer_400.as_ref(),
        answers.answer_401.as_ref(),
        answers.answer_404.as_ref(),
        answers.answer_408.as_ref(),
        answers.answer_413.as_ref(),
        answers.answer_421.as_ref(),
        answers.answer_429.as_ref(),
        answers.answer_502.as_ref(),
        answers.answer_503.as_ref(),
        answers.answer_504.as_ref(),
        answers.answer_507.as_ref(),
    ];

    (
        Some("[redacted]"),
        Some(values.iter().flatten().count()),
        Some(total_string_len(values.iter().flatten().copied())),
    )
}

fn summarize_string_map(
    values: &std::collections::BTreeMap<String, String>,
) -> (usize, usize, usize) {
    (
        values.len(),
        total_string_len(values.keys()),
        total_string_len(values.values()),
    )
}

impl std::fmt::Debug for command::HttpListenerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (http_answers, http_answers_count, http_answers_len) =
            summarize_http_answers(self.http_answers.as_ref());
        let (answers_count, answers_key_len, answers_value_len) =
            summarize_string_map(&self.answers);

        f.debug_struct("HttpListenerConfig")
            .field("address", &self.address)
            .field("public_address", &self.public_address)
            .field("expect_proxy", &self.expect_proxy)
            .field("sticky_name_len", &self.sticky_name.len())
            .field("front_timeout", &self.front_timeout)
            .field("back_timeout", &self.back_timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("active", &self.active)
            .field("http_answers", &http_answers)
            .field("http_answers_count", &http_answers_count)
            .field("http_answers_len", &http_answers_len)
            .field(
                "sozu_id_header_len",
                &self.sozu_id_header.as_ref().map(String::len),
            )
            .field("answers_count", &answers_count)
            .field("answers_key_len", &answers_key_len)
            .field("answers_value_len", &answers_value_len)
            .field("elide_x_real_ip", &self.elide_x_real_ip)
            .field("send_x_real_ip", &self.send_x_real_ip)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for command::UpdateHttpListenerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (http_answers, http_answers_count, http_answers_len) =
            summarize_http_answers(self.http_answers.as_ref());
        let (answers_count, answers_key_len, answers_value_len) =
            summarize_string_map(&self.answers);

        f.debug_struct("UpdateHttpListenerConfig")
            .field("address", &self.address)
            .field("public_address", &self.public_address)
            .field(
                "sticky_name_len",
                &self.sticky_name.as_ref().map(String::len),
            )
            .field("http_answers", &http_answers)
            .field("http_answers_count", &http_answers_count)
            .field("http_answers_len", &http_answers_len)
            .field(
                "sozu_id_header_len",
                &self.sozu_id_header.as_ref().map(String::len),
            )
            .field("answers_count", &answers_count)
            .field("answers_key_len", &answers_key_len)
            .field("answers_value_len", &answers_value_len)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for command::UpdateHttpsListenerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (http_answers, http_answers_count, http_answers_len) =
            summarize_http_answers(self.http_answers.as_ref());
        let alpn_protocols_count = self
            .alpn_protocols
            .as_ref()
            .map(|protocols| protocols.values.len());
        let alpn_protocols_len = self
            .alpn_protocols
            .as_ref()
            .map(|protocols| total_string_len(&protocols.values));
        let (answers_count, answers_key_len, answers_value_len) =
            summarize_string_map(&self.answers);

        f.debug_struct("UpdateHttpsListenerConfig")
            .field("address", &self.address)
            .field("public_address", &self.public_address)
            .field(
                "sticky_name_len",
                &self.sticky_name.as_ref().map(String::len),
            )
            .field("http_answers", &http_answers)
            .field("http_answers_count", &http_answers_count)
            .field("http_answers_len", &http_answers_len)
            .field("alpn_protocols_count", &alpn_protocols_count)
            .field("alpn_protocols_len", &alpn_protocols_len)
            .field(
                "sozu_id_header_len",
                &self.sozu_id_header.as_ref().map(String::len),
            )
            .field("answers_count", &answers_count)
            .field("answers_key_len", &answers_key_len)
            .field("answers_value_len", &answers_value_len)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for command::HttpsListenerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let certificate = self.certificate.as_ref().map(|_| "[redacted]");
        let certificate_len = self.certificate.as_ref().map(String::len);
        let certificate_chain_len = total_string_len(&self.certificate_chain);
        let key = self.key.as_ref().map(|_| "[redacted]");
        let key_len = self.key.as_ref().map(String::len);
        let (http_answers, http_answers_count, http_answers_len) =
            summarize_http_answers(self.http_answers.as_ref());
        let sozu_id_header_len = self.sozu_id_header.as_ref().map(String::len);
        let (answers_count, answers_key_len, answers_value_len) =
            summarize_string_map(&self.answers);

        f.debug_struct("HttpsListenerConfig")
            .field("address", &self.address)
            .field("public_address", &self.public_address)
            .field("expect_proxy", &self.expect_proxy)
            .field("sticky_name_len", &self.sticky_name.len())
            .field("front_timeout", &self.front_timeout)
            .field("back_timeout", &self.back_timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("active", &self.active)
            .field("versions_count", &self.versions.len())
            .field("cipher_list_count", &self.cipher_list.len())
            .field("cipher_list_len", &total_string_len(&self.cipher_list))
            .field("cipher_suites_count", &self.cipher_suites.len())
            .field("cipher_suites_len", &total_string_len(&self.cipher_suites))
            .field(
                "signature_algorithms_count",
                &self.signature_algorithms.len(),
            )
            .field(
                "signature_algorithms_len",
                &total_string_len(&self.signature_algorithms),
            )
            .field("groups_list_count", &self.groups_list.len())
            .field("groups_list_len", &total_string_len(&self.groups_list))
            .field("certificate", &certificate)
            .field("certificate_len", &certificate_len)
            .field("certificate_chain", &"[redacted]")
            .field("certificate_chain_count", &self.certificate_chain.len())
            .field("certificate_chain_len", &certificate_chain_len)
            .field("key", &key)
            .field("key_len", &key_len)
            .field("send_tls13_tickets", &self.send_tls13_tickets)
            .field("http_answers", &http_answers)
            .field("http_answers_count", &http_answers_count)
            .field("http_answers_len", &http_answers_len)
            .field("alpn_protocols_count", &self.alpn_protocols.len())
            .field(
                "alpn_protocols_len",
                &total_string_len(&self.alpn_protocols),
            )
            .field(
                "h2_max_rst_stream_per_window",
                &self.h2_max_rst_stream_per_window,
            )
            .field("h2_max_ping_per_window", &self.h2_max_ping_per_window)
            .field(
                "h2_max_settings_per_window",
                &self.h2_max_settings_per_window,
            )
            .field(
                "h2_max_empty_data_per_window",
                &self.h2_max_empty_data_per_window,
            )
            .field(
                "h2_max_continuation_frames",
                &self.h2_max_continuation_frames,
            )
            .field("h2_max_glitch_count", &self.h2_max_glitch_count)
            .field(
                "h2_initial_connection_window",
                &self.h2_initial_connection_window,
            )
            .field("h2_max_concurrent_streams", &self.h2_max_concurrent_streams)
            .field("h2_stream_shrink_ratio", &self.h2_stream_shrink_ratio)
            .field(
                "h2_max_rst_stream_lifetime",
                &self.h2_max_rst_stream_lifetime,
            )
            .field(
                "h2_max_rst_stream_abusive_lifetime",
                &self.h2_max_rst_stream_abusive_lifetime,
            )
            .field(
                "h2_max_rst_stream_emitted_lifetime",
                &self.h2_max_rst_stream_emitted_lifetime,
            )
            .field("h2_max_header_list_size", &self.h2_max_header_list_size)
            .field("strict_sni_binding", &self.strict_sni_binding)
            .field("disable_http11", &self.disable_http11)
            .field(
                "h2_stream_idle_timeout_seconds",
                &self.h2_stream_idle_timeout_seconds,
            )
            .field("h2_max_header_table_size", &self.h2_max_header_table_size)
            .field(
                "h2_graceful_shutdown_deadline_seconds",
                &self.h2_graceful_shutdown_deadline_seconds,
            )
            .field(
                "h2_max_window_update_stream0_per_window",
                &self.h2_max_window_update_stream0_per_window,
            )
            .field("sozu_id_header_len", &sozu_id_header_len)
            .field("answers_count", &answers_count)
            .field("answers_key_len", &answers_key_len)
            .field("answers_value_len", &answers_value_len)
            .field("elide_x_real_ip", &self.elide_x_real_ip)
            .field("send_x_real_ip", &self.send_x_real_ip)
            .field("hsts", &self.hsts)
            .field("h2_max_header_fields", &self.h2_max_header_fields)
            .finish()
    }
}

impl std::fmt::Debug for command::QueryCertificatesFilters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryCertificatesFilters")
            .field("domain_len", &self.domain.as_ref().map(String::len))
            .field(
                "fingerprint_len",
                &self.fingerprint.as_ref().map(String::len),
            )
            .finish()
    }
}

impl std::fmt::Debug for command::CertificateSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificateSummary")
            .field("domain_len", &self.domain.len())
            .field("fingerprint_len", &self.fingerprint.len())
            .finish()
    }
}

impl std::fmt::Debug for command::CertificatesByAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let domain_len = self
            .certificate_summaries
            .iter()
            .map(|summary| summary.domain.len())
            .fold(0usize, usize::saturating_add);
        let fingerprint_len = self
            .certificate_summaries
            .iter()
            .map(|summary| summary.fingerprint.len())
            .fold(0usize, usize::saturating_add);

        f.debug_struct("CertificatesByAddress")
            .field("address", &self.address)
            .field("certificates_count", &self.certificate_summaries.len())
            .field("domain_len", &domain_len)
            .field("fingerprint_len", &fingerprint_len)
            .finish()
    }
}

impl std::fmt::Debug for command::ListOfCertificatesByAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let certificates_count = self
            .certificates
            .iter()
            .map(|entry| entry.certificate_summaries.len())
            .fold(0usize, usize::saturating_add);
        let domain_len = self
            .certificates
            .iter()
            .flat_map(|entry| &entry.certificate_summaries)
            .map(|summary| summary.domain.len())
            .fold(0usize, usize::saturating_add);
        let fingerprint_len = self
            .certificates
            .iter()
            .flat_map(|entry| &entry.certificate_summaries)
            .map(|summary| summary.fingerprint.len())
            .fold(0usize, usize::saturating_add);

        f.debug_struct("ListOfCertificatesByAddress")
            .field("listeners_count", &self.certificates.len())
            .field("certificates_count", &certificates_count)
            .field("domain_len", &domain_len)
            .field("fingerprint_len", &fingerprint_len)
            .finish()
    }
}

impl std::fmt::Debug for command::CertificatesWithFingerprints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fingerprint_len = total_string_len(self.certs.keys());
        let names_count = self
            .certs
            .values()
            .map(|certificate| certificate.names.len())
            .fold(0usize, usize::saturating_add);
        let names_len = self
            .certs
            .values()
            .flat_map(|certificate| &certificate.names)
            .map(String::len)
            .fold(0usize, usize::saturating_add);

        f.debug_struct("CertificatesWithFingerprints")
            .field("certificates_count", &self.certs.len())
            .field("fingerprint_len", &fingerprint_len)
            .field("names_count", &names_count)
            .field("names_len", &names_len)
            .finish()
    }
}

fn response_content_kind(content: Option<&command::ResponseContent>) -> Option<&'static str> {
    use command::response_content::ContentType;

    content
        .and_then(|response| response.content_type.as_ref())
        .map(|content_type| match content_type {
            ContentType::Workers(_) => "workers",
            ContentType::Metrics(_) => "metrics",
            ContentType::WorkerResponses(_) => "worker_responses",
            ContentType::Event(_) => "event",
            ContentType::FrontendList(_) => "frontend_list",
            ContentType::ListenersList(_) => "listeners_list",
            ContentType::WorkerMetrics(_) => "worker_metrics",
            ContentType::AvailableMetrics(_) => "available_metrics",
            ContentType::Clusters(_) => "clusters",
            ContentType::ClusterHashes(_) => "cluster_hashes",
            ContentType::CertificatesByAddress(_) => "certificates_by_address",
            ContentType::CertificatesWithFingerprints(_) => "certificates_with_fingerprints",
            ContentType::RequestCounts(_) => "request_counts",
            ContentType::MaxConnectionsPerIpLimit(_) => "max_connections_per_ip_limit",
            ContentType::HealthChecksList(_) => "health_checks_list",
            ContentType::MetricDetailStatus(_) => "metric_detail_status",
            ContentType::WorkerMetricDetailStatus(_) => "worker_metric_detail_status",
        })
}

impl std::fmt::Debug for command::ResponseContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseContent")
            .field("content_type", &response_content_kind(Some(self)))
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for command::WorkerResponses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerResponses")
            .field("workers_count", &self.map.len())
            .field("worker_ids_len", &total_string_len(self.map.keys()))
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for command::WorkerRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerRequest")
            .field("id_len", &self.id.len())
            .field("content", &self.content)
            .finish()
    }
}

impl std::fmt::Debug for command::WorkerResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerResponse")
            .field("id_len", &self.id.len())
            .field("status", &self.status)
            .field("message_len", &self.message.len())
            .field(
                "content_type",
                &response_content_kind(self.content.as_ref()),
            )
            .finish()
    }
}

impl AggregatedMetrics {
    /// Merge metrics that were received from several workers
    ///
    /// Each worker gather the same kind of metrics,
    /// for its own proxying logic, and for the same clusters with their backends.
    /// This means we have to reduce each metric from N instances to 1.
    pub fn merge_metrics(&mut self) {
        // avoid copying the worker metrics, by taking them
        let workers = std::mem::take(&mut self.workers);

        for (_worker_id, worker) in workers {
            for (metric_name, new_value) in worker.proxy {
                if new_value.is_mergeable() {
                    self.proxying
                        .entry(metric_name)
                        .and_modify(|old_value| old_value.merge(&new_value))
                        .or_insert(new_value);
                }
            }

            for (cluster_id, mut cluster_metrics) in worker.clusters {
                for (metric_name, new_value) in cluster_metrics.cluster {
                    if new_value.is_mergeable() {
                        let cluster = self.clusters.entry(cluster_id.to_owned()).or_default();

                        cluster
                            .cluster
                            .entry(metric_name)
                            .and_modify(|old_value| old_value.merge(&new_value))
                            .or_insert(new_value);
                    }
                }

                for backend in cluster_metrics.backends.drain(..) {
                    for (metric_name, new_value) in backend.metrics {
                        if new_value.is_mergeable() {
                            let cluster = self.clusters.entry(cluster_id.to_owned()).or_default();

                            let found_backend = cluster
                                .backends
                                .iter_mut()
                                .find(|present| present.backend_id == backend.backend_id);

                            if let Some(existing_backend) = found_backend {
                                let _ = existing_backend
                                    .metrics
                                    .entry(metric_name)
                                    .and_modify(|old_value| old_value.merge(&new_value))
                                    .or_insert(new_value);
                            } else {
                                cluster.backends.push(BackendMetrics {
                                    backend_id: backend.backend_id.clone(),
                                    metrics: BTreeMap::from([(metric_name, new_value)]),
                                });
                            };
                        }
                    }
                }
            }
        }
    }
}

impl FilteredMetrics {
    pub fn merge(&mut self, right: &Self) {
        match (&self.inner, &right.inner) {
            (Some(Inner::Gauge(a)), Some(Inner::Gauge(b))) => {
                *self = Self {
                    inner: Some(Inner::Gauge(a + b)),
                };
            }
            (Some(Inner::Count(a)), Some(Inner::Count(b))) => {
                *self = Self {
                    inner: Some(Inner::Count(a + b)),
                };
            }
            (Some(Inner::Histogram(a)), Some(Inner::Histogram(b))) => {
                let longest_len = a.buckets.len().max(b.buckets.len());

                let mut a_count = 0;
                let mut b_count = 0;
                let buckets = (0..longest_len)
                    .map(|i| {
                        if let Some(a_bucket) = a.buckets.get(i) {
                            a_count = a_bucket.count;
                        }
                        if let Some(b_bucket) = b.buckets.get(i) {
                            b_count = b_bucket.count;
                        }
                        Bucket {
                            le: (1 << i) - 1, // the bucket less-or-equal limits are normalized: 0, 1, 3, 7, 15, ...
                            count: a_count + b_count,
                        }
                    })
                    .collect();

                *self = Self {
                    inner: Some(Inner::Histogram(FilteredHistogram {
                        count: a.count + b.count,
                        sum: a.sum + b.sum,
                        buckets,
                    })),
                };
            }
            (Some(Inner::Percentiles(a)), Some(Inner::Percentiles(b))) => {
                // You cannot statistically merge two percentile summaries
                // without the underlying samples. The companion
                // `<name>_histogram` Inner::Histogram value is the source
                // of truth for accurate aggregation and merges correctly
                // above. We still propagate the percentile shape so legacy
                // consumers reading it observe at least the worst-case
                // upper bound across workers — element-wise max preserves
                // the "is anyone slow?" intent. `samples` and `sum` add so
                // the totals reflect cross-worker volume.
                *self = Self {
                    inner: Some(Inner::Percentiles(Percentiles {
                        samples: a.samples + b.samples,
                        p_50: a.p_50.max(b.p_50),
                        p_90: a.p_90.max(b.p_90),
                        p_99: a.p_99.max(b.p_99),
                        p_99_9: a.p_99_9.max(b.p_99_9),
                        p_99_99: a.p_99_99.max(b.p_99_99),
                        p_99_999: a.p_99_999.max(b.p_99_999),
                        p_100: a.p_100.max(b.p_100),
                        sum: a.sum + b.sum,
                    })),
                };
            }
            _ => {}
        }
    }

    fn is_mergeable(&self) -> bool {
        match &self.inner {
            Some(Inner::Gauge(_))
            | Some(Inner::Count(_))
            | Some(Inner::Histogram(_))
            | Some(Inner::Percentiles(_)) => true,
            // Inner::Time and Inner::Timeserie are never used in Sōzu
            Some(Inner::Time(_)) | Some(Inner::TimeSerie(_)) | None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        net::SocketAddr,
    };

    use crate::{certificate::Fingerprint, state::ConfigState};

    use super::AggregatedMetrics;
    use super::command::{
        AddCertificate, AlpnProtocols, Bucket, CertificateAndKey, CertificateSummary,
        CertificatesByAddress, CertificatesWithFingerprints, Cluster, ClusterMetrics,
        CustomHttpAnswers, FilteredHistogram, FilteredMetrics, Header, HeaderPosition,
        HttpListenerConfig, HttpsListenerConfig, ListOfCertificatesByAddress, PathRule,
        PathRuleKind, Percentiles, QueryCertificatesFilters, RedirectPolicy, RedirectScheme,
        RemoveCertificate, ReplaceCertificate, Request, RequestHttpFrontend, RequestTcpFrontend,
        ResponseContent, ResponseStatus, RulePosition, TlsVersion, UpdateHttpListenerConfig,
        UpdateHttpsListenerConfig, WorkerMetrics, WorkerRequest, WorkerResponse, WorkerResponses,
        filtered_metrics::Inner, request::RequestType, response_content::ContentType,
    };

    #[test]
    fn request_debug_redacts_string_variant_payloads_directly_and_when_nested() {
        const PATH_SECRET: &str = "REQUEST_SAVE_STATE_PATH_SECRET_SENTINEL";

        let request_type = RequestType::SaveState(format!("{PATH_SECRET}{}", "x".repeat(4096)));
        let direct_output = format!("{request_type:?}");
        let request = Request::from(request_type);
        let worker_request = WorkerRequest {
            id: "safe-request-id".to_owned(),
            content: request.clone(),
        };
        let outputs = [
            direct_output,
            format!("{request:?}"),
            format!("{worker_request:?}"),
        ];

        for output in outputs {
            assert!(
                !output.contains(PATH_SECRET),
                "Request Debug leaked its string variant payload: {output}"
            );
            assert!(
                output.contains("SaveState"),
                "Request Debug omitted the bounded request kind: {output}"
            );
            assert!(
                output.len() <= 256,
                "Request Debug output is not bounded: {} bytes",
                output.len()
            );
        }
    }

    #[test]
    fn listener_and_patch_debug_redacts_user_controlled_material() {
        const HTTP_STICKY_SECRET: &str = "HTTP_LISTENER_STICKY_SECRET_SENTINEL";
        const HTTP_LEGACY_ANSWER_SECRET: &str = "HTTP_LISTENER_LEGACY_ANSWER_SECRET_SENTINEL";
        const HTTP_HEADER_SECRET: &str = "HTTP_LISTENER_HEADER_SECRET_SENTINEL";
        const HTTP_ANSWER_KEY_SECRET: &str = "HTTP_LISTENER_ANSWER_KEY_SECRET_SENTINEL";
        const HTTP_ANSWER_BODY_SECRET: &str = "HTTP_LISTENER_ANSWER_BODY_SECRET_SENTINEL";
        const HTTP_PATCH_STICKY_SECRET: &str = "HTTP_PATCH_STICKY_SECRET_SENTINEL";
        const HTTP_PATCH_LEGACY_ANSWER_SECRET: &str = "HTTP_PATCH_LEGACY_ANSWER_SECRET_SENTINEL";
        const HTTP_PATCH_HEADER_SECRET: &str = "HTTP_PATCH_HEADER_SECRET_SENTINEL";
        const HTTP_PATCH_ANSWER_KEY_SECRET: &str = "HTTP_PATCH_ANSWER_KEY_SECRET_SENTINEL";
        const HTTP_PATCH_ANSWER_BODY_SECRET: &str = "HTTP_PATCH_ANSWER_BODY_SECRET_SENTINEL";
        const HTTPS_PATCH_STICKY_SECRET: &str = "HTTPS_PATCH_STICKY_SECRET_SENTINEL";
        const HTTPS_PATCH_LEGACY_ANSWER_SECRET: &str = "HTTPS_PATCH_LEGACY_ANSWER_SECRET_SENTINEL";
        const HTTPS_PATCH_ALPN_SECRET: &str = "HTTPS_PATCH_ALPN_SECRET_SENTINEL";
        const HTTPS_PATCH_HEADER_SECRET: &str = "HTTPS_PATCH_HEADER_SECRET_SENTINEL";
        const HTTPS_PATCH_ANSWER_KEY_SECRET: &str = "HTTPS_PATCH_ANSWER_KEY_SECRET_SENTINEL";
        const HTTPS_PATCH_ANSWER_BODY_SECRET: &str = "HTTPS_PATCH_ANSWER_BODY_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let http_address: SocketAddr = "127.0.0.1:8080"
            .parse()
            .expect("test HTTP listener address must parse");
        let http_listener = HttpListenerConfig {
            address: http_address.into(),
            sticky_name: long_value(HTTP_STICKY_SECRET),
            http_answers: Some(CustomHttpAnswers {
                answer_503: Some(long_value(HTTP_LEGACY_ANSWER_SECRET)),
                ..Default::default()
            }),
            sozu_id_header: Some(long_value(HTTP_HEADER_SECRET)),
            answers: BTreeMap::from([(
                long_value(HTTP_ANSWER_KEY_SECRET),
                long_value(HTTP_ANSWER_BODY_SECRET),
            )]),
            ..Default::default()
        };
        let http_request = Request::from(RequestType::AddHttpListener(http_listener.clone()));
        let http_worker_request = WorkerRequest {
            id: "safe-add-http-listener-id".to_owned(),
            content: http_request.clone(),
        };

        let http_patch = UpdateHttpListenerConfig {
            address: http_address.into(),
            sticky_name: Some(long_value(HTTP_PATCH_STICKY_SECRET)),
            http_answers: Some(CustomHttpAnswers {
                answer_503: Some(long_value(HTTP_PATCH_LEGACY_ANSWER_SECRET)),
                ..Default::default()
            }),
            sozu_id_header: Some(long_value(HTTP_PATCH_HEADER_SECRET)),
            answers: BTreeMap::from([(
                long_value(HTTP_PATCH_ANSWER_KEY_SECRET),
                long_value(HTTP_PATCH_ANSWER_BODY_SECRET),
            )]),
            ..Default::default()
        };
        let http_patch_request = Request::from(RequestType::UpdateHttpListener(http_patch.clone()));
        let http_patch_worker_request = WorkerRequest {
            id: "safe-update-http-listener-id".to_owned(),
            content: http_patch_request.clone(),
        };

        let https_address: SocketAddr = "127.0.0.1:8443"
            .parse()
            .expect("test HTTPS listener address must parse");
        let https_patch = UpdateHttpsListenerConfig {
            address: https_address.into(),
            sticky_name: Some(long_value(HTTPS_PATCH_STICKY_SECRET)),
            http_answers: Some(CustomHttpAnswers {
                answer_503: Some(long_value(HTTPS_PATCH_LEGACY_ANSWER_SECRET)),
                ..Default::default()
            }),
            alpn_protocols: Some(AlpnProtocols {
                values: vec![long_value(HTTPS_PATCH_ALPN_SECRET)],
            }),
            sozu_id_header: Some(long_value(HTTPS_PATCH_HEADER_SECRET)),
            answers: BTreeMap::from([(
                long_value(HTTPS_PATCH_ANSWER_KEY_SECRET),
                long_value(HTTPS_PATCH_ANSWER_BODY_SECRET),
            )]),
            ..Default::default()
        };
        let https_patch_request =
            Request::from(RequestType::UpdateHttpsListener(https_patch.clone()));
        let https_patch_worker_request = WorkerRequest {
            id: "safe-update-https-listener-id".to_owned(),
            content: https_patch_request.clone(),
        };

        let debug_outputs = [
            ("HttpListenerConfig", format!("{http_listener:?}")),
            ("AddHttpListener Request", format!("{http_request:?}")),
            (
                "AddHttpListener WorkerRequest",
                format!("{http_worker_request:?}"),
            ),
            ("UpdateHttpListenerConfig", format!("{http_patch:?}")),
            (
                "UpdateHttpListener Request",
                format!("{http_patch_request:?}"),
            ),
            (
                "UpdateHttpListener WorkerRequest",
                format!("{http_patch_worker_request:?}"),
            ),
            ("UpdateHttpsListenerConfig", format!("{https_patch:?}")),
            (
                "UpdateHttpsListener Request",
                format!("{https_patch_request:?}"),
            ),
            (
                "UpdateHttpsListener WorkerRequest",
                format!("{https_patch_worker_request:?}"),
            ),
        ];
        let secrets = [
            HTTP_STICKY_SECRET,
            HTTP_LEGACY_ANSWER_SECRET,
            HTTP_HEADER_SECRET,
            HTTP_ANSWER_KEY_SECRET,
            HTTP_ANSWER_BODY_SECRET,
            HTTP_PATCH_STICKY_SECRET,
            HTTP_PATCH_LEGACY_ANSWER_SECRET,
            HTTP_PATCH_HEADER_SECRET,
            HTTP_PATCH_ANSWER_KEY_SECRET,
            HTTP_PATCH_ANSWER_BODY_SECRET,
            HTTPS_PATCH_STICKY_SECRET,
            HTTPS_PATCH_LEGACY_ANSWER_SECRET,
            HTTPS_PATCH_ALPN_SECRET,
            HTTPS_PATCH_HEADER_SECRET,
            HTTPS_PATCH_ANSWER_KEY_SECRET,
            HTTPS_PATCH_ANSWER_BODY_SECRET,
        ];
        let mut leaks = Vec::new();
        for (label, output) in &debug_outputs {
            for secret in secrets {
                if output.contains(secret) {
                    leaks.push(format!("{label}:{secret}"));
                }
            }
        }
        assert!(
            leaks.is_empty(),
            "listener Debug leaked secret markers through {}",
            leaks.join(", ")
        );

        let expected_metadata = [
            (0, "sticky_name_len: 4132"),
            (0, "http_answers_count: Some(1)"),
            (0, "answers_count: 1"),
            (3, "sticky_name_len: Some(4129)"),
            (3, "http_answers_count: Some(1)"),
            (3, "answers_count: 1"),
            (6, "sticky_name_len: Some(4130)"),
            (6, "http_answers_count: Some(1)"),
            (6, "alpn_protocols_count: Some(1)"),
            (6, "answers_count: 1"),
        ];
        for (output_index, safe_metadata) in expected_metadata {
            assert!(
                debug_outputs[output_index].1.contains(safe_metadata),
                "{} Debug omitted safe metadata {safe_metadata}: {}",
                debug_outputs[output_index].0,
                debug_outputs[output_index].1,
            );
        }
        for (label, output) in debug_outputs {
            assert!(
                output.len() <= 4096,
                "{label} Debug output is not bounded: {} bytes",
                output.len()
            );
        }
    }

    #[test]
    fn https_listener_debug_redacts_user_controlled_material() {
        const STICKY_NAME_SECRET: &str = "HTTPS_LISTENER_STICKY_NAME_SECRET_SENTINEL";
        const CIPHER_LIST_SECRET: &str = "HTTPS_LISTENER_CIPHER_LIST_SECRET_SENTINEL";
        const CIPHER_SUITES_SECRET: &str = "HTTPS_LISTENER_CIPHER_SUITES_SECRET_SENTINEL";
        const SIGNATURE_ALGORITHMS_SECRET: &str =
            "HTTPS_LISTENER_SIGNATURE_ALGORITHMS_SECRET_SENTINEL";
        const GROUPS_LIST_SECRET: &str = "HTTPS_LISTENER_GROUPS_LIST_SECRET_SENTINEL";
        const CERTIFICATE_SECRET: &str = "HTTPS_LISTENER_CERTIFICATE_SECRET_SENTINEL";
        const CHAIN_SECRET: &str = "HTTPS_LISTENER_CHAIN_SECRET_SENTINEL";
        const KEY_SECRET: &str = "HTTPS_LISTENER_KEY_SECRET_SENTINEL";
        const HTTP_ANSWER_SECRET: &str = "HTTPS_LISTENER_HTTP_ANSWER_SECRET_SENTINEL";
        const ALPN_SECRET: &str = "HTTPS_LISTENER_ALPN_SECRET_SENTINEL";
        const SOZU_ID_HEADER_SECRET: &str = "HTTPS_LISTENER_SOZU_ID_HEADER_SECRET_SENTINEL";
        const ANSWER_KEY_SECRET: &str = "HTTPS_LISTENER_ANSWER_KEY_SECRET_SENTINEL";
        const ANSWER_VALUE_SECRET: &str = "HTTPS_LISTENER_ANSWER_VALUE_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let address: SocketAddr = "127.0.0.1:8443"
            .parse()
            .expect("test HTTPS listener address must parse");
        let public_address: SocketAddr = "203.0.113.1:443"
            .parse()
            .expect("test public HTTPS listener address must parse");
        let listener = HttpsListenerConfig {
            address: address.into(),
            public_address: Some(public_address.into()),
            sticky_name: long_value(STICKY_NAME_SECRET),
            versions: vec![TlsVersion::TlsV13 as i32],
            cipher_list: vec![long_value(CIPHER_LIST_SECRET)],
            cipher_suites: vec![long_value(CIPHER_SUITES_SECRET)],
            signature_algorithms: vec![long_value(SIGNATURE_ALGORITHMS_SECRET)],
            groups_list: vec![long_value(GROUPS_LIST_SECRET)],
            certificate: Some(long_value(CERTIFICATE_SECRET)),
            certificate_chain: vec![long_value(CHAIN_SECRET)],
            key: Some(long_value(KEY_SECRET)),
            http_answers: Some(CustomHttpAnswers {
                answer_404: Some(long_value(HTTP_ANSWER_SECRET)),
                ..Default::default()
            }),
            alpn_protocols: vec![long_value(ALPN_SECRET)],
            sozu_id_header: Some(long_value(SOZU_ID_HEADER_SECRET)),
            answers: BTreeMap::from([(
                long_value(ANSWER_KEY_SECRET),
                long_value(ANSWER_VALUE_SECRET),
            )]),
            ..Default::default()
        };
        let add_https_listener = RequestType::AddHttpsListener(listener.clone());
        let request = Request::from(add_https_listener.clone());
        let worker_request = WorkerRequest {
            id: "safe-add-https-listener-id".to_owned(),
            content: request.clone(),
        };
        let state = ConfigState {
            https_listeners: BTreeMap::from([(address, listener.clone())]),
            ..Default::default()
        };
        let debug_outputs = [
            ("HttpsListenerConfig", format!("{listener:?}")),
            (
                "RequestType::AddHttpsListener",
                format!("{add_https_listener:?}"),
            ),
            ("Request", format!("{request:?}")),
            ("WorkerRequest", format!("{worker_request:?}")),
            ("ConfigState", format!("{state:?}")),
        ];
        let secrets = [
            STICKY_NAME_SECRET,
            CIPHER_LIST_SECRET,
            CIPHER_SUITES_SECRET,
            SIGNATURE_ALGORITHMS_SECRET,
            GROUPS_LIST_SECRET,
            CERTIFICATE_SECRET,
            CHAIN_SECRET,
            KEY_SECRET,
            HTTP_ANSWER_SECRET,
            ALPN_SECRET,
            SOZU_ID_HEADER_SECRET,
            ANSWER_KEY_SECRET,
            ANSWER_VALUE_SECRET,
        ];
        let mut leaks = Vec::new();
        for (label, output) in &debug_outputs {
            for secret in secrets {
                if output.contains(secret) {
                    leaks.push(format!("{label}:{secret}"));
                }
            }
        }
        assert!(
            leaks.is_empty(),
            "HTTPS listener Debug leaked secret markers through {}",
            leaks.join(", ")
        );

        let direct_output = &debug_outputs[0].1;
        let expected_metadata = [
            format!("sticky_name_len: {}", long_value(STICKY_NAME_SECRET).len()),
            "versions_count: 1".to_owned(),
            "cipher_list_count: 1".to_owned(),
            format!("cipher_list_len: {}", long_value(CIPHER_LIST_SECRET).len()),
            "cipher_suites_count: 1".to_owned(),
            "signature_algorithms_count: 1".to_owned(),
            "groups_list_count: 1".to_owned(),
            "certificate: Some(\"[redacted]\")".to_owned(),
            format!(
                "certificate_len: Some({})",
                long_value(CERTIFICATE_SECRET).len()
            ),
            "certificate_chain: \"[redacted]\"".to_owned(),
            "certificate_chain_count: 1".to_owned(),
            "key: Some(\"[redacted]\")".to_owned(),
            "http_answers_count: Some(1)".to_owned(),
            "alpn_protocols_count: 1".to_owned(),
            format!(
                "sozu_id_header_len: Some({})",
                long_value(SOZU_ID_HEADER_SECRET).len()
            ),
            "answers_count: 1".to_owned(),
        ];
        for safe_metadata in expected_metadata {
            assert!(
                direct_output.contains(&safe_metadata),
                "HttpsListenerConfig Debug omitted safe metadata {safe_metadata}: {direct_output}"
            );
        }
        for (label, output) in debug_outputs {
            assert!(
                output.len() <= 4096,
                "{label} Debug output is not bounded: {} bytes",
                output.len()
            );
        }
    }

    #[test]
    fn header_debug_redacts_key_and_value() {
        const KEY_SECRET: &str = "HEADER_KEY_SECRET_SENTINEL";
        const VALUE_SECRET: &str = "HEADER_VALUE_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let header = Header {
            position: HeaderPosition::Unspecified as i32,
            key: long_value(KEY_SECRET),
            val: long_value(VALUE_SECRET),
        };
        let output = format!("{header:?}");

        for secret in [KEY_SECRET, VALUE_SECRET] {
            assert!(
                !output.contains(secret),
                "Header Debug leaked user-controlled marker {secret}"
            );
        }
        for metadata in [
            "position: 0".to_owned(),
            format!("key_len: {}", long_value(KEY_SECRET).len()),
            format!("val_len: {}", long_value(VALUE_SECRET).len()),
        ] {
            assert!(
                output.contains(&metadata),
                "Header Debug omitted safe metadata {metadata}: {output}"
            );
        }
        assert!(
            output.len() <= 256,
            "Header Debug output is not bounded: {} bytes",
            output.len()
        );
    }

    #[test]
    fn http_frontend_request_debug_redacts_user_controlled_material() {
        const CLUSTER_SECRET: &str = "HTTP_FRONTEND_CLUSTER_SECRET_SENTINEL";
        const HOSTNAME_SECRET: &str = "HTTP_FRONTEND_HOSTNAME_SECRET_SENTINEL";
        const PATH_SECRET: &str = "HTTP_FRONTEND_PATH_SECRET_SENTINEL";
        const METHOD_SECRET: &str = "HTTP_FRONTEND_METHOD_SECRET_SENTINEL";
        const TAG_KEY_SECRET: &str = "HTTP_FRONTEND_TAG_KEY_SECRET_SENTINEL";
        const TAG_VALUE_SECRET: &str = "HTTP_FRONTEND_TAG_VALUE_SECRET_SENTINEL";
        const REDIRECT_SECRET: &str = "HTTP_FRONTEND_REDIRECT_SECRET_SENTINEL";
        const REWRITE_HOST_SECRET: &str = "HTTP_FRONTEND_REWRITE_HOST_SECRET_SENTINEL";
        const REWRITE_PATH_SECRET: &str = "HTTP_FRONTEND_REWRITE_PATH_SECRET_SENTINEL";
        const HEADER_KEY_SECRET: &str = "HTTP_FRONTEND_HEADER_KEY_SECRET_SENTINEL";
        const HEADER_VALUE_SECRET: &str = "HTTP_FRONTEND_HEADER_VALUE_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let frontend = RequestHttpFrontend {
            cluster_id: Some(long_value(CLUSTER_SECRET)),
            address: Default::default(),
            hostname: long_value(HOSTNAME_SECRET),
            path: PathRule {
                value: long_value(PATH_SECRET),
                kind: PathRuleKind::Prefix as i32,
            },
            method: Some(long_value(METHOD_SECRET)),
            position: RulePosition::Tree as i32,
            tags: BTreeMap::from([(long_value(TAG_KEY_SECRET), long_value(TAG_VALUE_SECRET))]),
            redirect: Some(RedirectPolicy::Forward as i32),
            required_auth: Some(true),
            redirect_scheme: Some(RedirectScheme::UseSame as i32),
            redirect_template: Some(long_value(REDIRECT_SECRET)),
            rewrite_host: Some(long_value(REWRITE_HOST_SECRET)),
            rewrite_path: Some(long_value(REWRITE_PATH_SECRET)),
            rewrite_port: Some(8443),
            headers: vec![Header {
                position: HeaderPosition::Request as i32,
                key: long_value(HEADER_KEY_SECRET),
                val: long_value(HEADER_VALUE_SECRET),
            }],
            hsts: None,
        };
        let request_type = RequestType::AddHttpsFrontend(frontend.clone());
        let request = Request::from(request_type.clone());
        let worker_request = WorkerRequest {
            id: "safe-http-frontend-request-id".to_owned(),
            content: request.clone(),
        };
        let debug_outputs = [
            ("RequestHttpFrontend", format!("{frontend:?}")),
            ("RequestType", format!("{request_type:?}")),
            ("Request", format!("{request:?}")),
            ("WorkerRequest", format!("{worker_request:?}")),
        ];
        let secrets = [
            CLUSTER_SECRET,
            HOSTNAME_SECRET,
            PATH_SECRET,
            METHOD_SECRET,
            TAG_KEY_SECRET,
            TAG_VALUE_SECRET,
            REDIRECT_SECRET,
            REWRITE_HOST_SECRET,
            REWRITE_PATH_SECRET,
            HEADER_KEY_SECRET,
            HEADER_VALUE_SECRET,
        ];

        for (label, output) in &debug_outputs {
            for secret in secrets {
                assert!(
                    !output.contains(secret),
                    "{label} Debug leaked user-controlled marker {secret}: {output}"
                );
            }
            assert!(
                output.len() <= 2048,
                "{label} Debug output is not bounded: {} bytes",
                output.len()
            );
        }

        let direct_output = &debug_outputs[0].1;
        let expected_metadata = [
            format!("cluster_id_len: Some({})", long_value(CLUSTER_SECRET).len()),
            format!("hostname_len: {}", long_value(HOSTNAME_SECRET).len()),
            format!("path_len: {}", long_value(PATH_SECRET).len()),
            format!("method_len: Some({})", long_value(METHOD_SECRET).len()),
            "tags_count: 1".to_owned(),
            "headers_count: 1".to_owned(),
        ];
        for safe_metadata in expected_metadata {
            assert!(
                direct_output.contains(&safe_metadata),
                "RequestHttpFrontend Debug omitted safe metadata {safe_metadata}: {direct_output}"
            );
        }
    }

    #[test]
    fn certificate_query_request_debug_redacts_filters_across_command_wrappers() {
        const DOMAIN_SECRET: &str = "QUERY_REQUEST_DOMAIN_SECRET_SENTINEL";
        const FINGERPRINT_SECRET: &str = "QUERY_REQUEST_FINGERPRINT_SECRET_SENTINEL";
        const REQUEST_ID_SECRET: &str = "QUERY_REQUEST_ID_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let filters = QueryCertificatesFilters {
            domain: Some(long_value(DOMAIN_SECRET)),
            fingerprint: Some(long_value(FINGERPRINT_SECRET)),
        };
        let request_type = RequestType::QueryCertificatesFromWorkers(filters.clone());
        let request = Request::from(request_type.clone());
        let worker_request = WorkerRequest {
            id: long_value(REQUEST_ID_SECRET),
            content: request.clone(),
        };
        let outputs = [
            ("QueryCertificatesFilters", format!("{filters:?}")),
            ("RequestType", format!("{request_type:?}")),
            ("Request", format!("{request:?}")),
            ("WorkerRequest", format!("{worker_request:?}")),
        ];

        for (label, output) in &outputs {
            for secret in [DOMAIN_SECRET, FINGERPRINT_SECRET, REQUEST_ID_SECRET] {
                assert!(
                    !output.contains(secret),
                    "{label} Debug leaked certificate-query marker {secret}: {output}"
                );
            }
            assert!(
                output.len() <= 1024,
                "{label} Debug output is not bounded: {} bytes",
                output.len()
            );
        }

        let filter_output = &outputs[0].1;
        for metadata in [
            format!("domain_len: Some({})", long_value(DOMAIN_SECRET).len()),
            format!(
                "fingerprint_len: Some({})",
                long_value(FINGERPRINT_SECRET).len()
            ),
        ] {
            assert!(
                filter_output.contains(&metadata),
                "QueryCertificatesFilters Debug omitted bounded metadata {metadata}: {filter_output}"
            );
        }
        assert!(
            outputs[3]
                .1
                .contains(&format!("id_len: {}", long_value(REQUEST_ID_SECRET).len())),
            "WorkerRequest Debug omitted the bounded request-id length: {}",
            outputs[3].1
        );
    }

    #[test]
    fn certificate_query_response_debug_redacts_results_across_worker_wrappers() {
        const DOMAIN_SECRET: &str = "QUERY_RESPONSE_DOMAIN_SECRET_SENTINEL";
        const FINGERPRINT_SECRET: &str = "QUERY_RESPONSE_FINGERPRINT_SECRET_SENTINEL";
        const MAP_KEY_SECRET: &str = "QUERY_RESPONSE_MAP_KEY_SECRET_SENTINEL";
        const CERTIFICATE_SECRET: &str = "QUERY_RESPONSE_CERTIFICATE_SECRET_SENTINEL";
        const KEY_SECRET: &str = "QUERY_RESPONSE_KEY_SECRET_SENTINEL";
        const NAME_SECRET: &str = "QUERY_RESPONSE_NAME_SECRET_SENTINEL";
        const WORKER_ID_SECRET: &str = "QUERY_RESPONSE_WORKER_ID_SECRET_SENTINEL";
        const RESPONSE_ID_SECRET: &str = "QUERY_RESPONSE_ID_SECRET_SENTINEL";
        const MESSAGE_SECRET: &str = "QUERY_RESPONSE_MESSAGE_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let summary = CertificateSummary {
            domain: long_value(DOMAIN_SECRET),
            fingerprint: long_value(FINGERPRINT_SECRET),
        };
        let by_address = CertificatesByAddress {
            address: Default::default(),
            certificate_summaries: vec![summary.clone()],
        };
        let list = ListOfCertificatesByAddress {
            certificates: vec![by_address.clone()],
        };
        let worker_content =
            ResponseContent::from(ContentType::CertificatesByAddress(list.clone()));
        let worker_response = WorkerResponse {
            id: long_value(RESPONSE_ID_SECRET),
            status: ResponseStatus::Ok as i32,
            message: long_value(MESSAGE_SECRET),
            content: Some(worker_content.clone()),
        };
        let worker_responses = WorkerResponses {
            map: BTreeMap::from([(long_value(WORKER_ID_SECRET), worker_content.clone())]),
        };
        let gathered_content =
            ResponseContent::from(ContentType::WorkerResponses(worker_responses.clone()));
        let main_certificates = CertificatesWithFingerprints {
            certs: BTreeMap::from([(
                long_value(MAP_KEY_SECRET),
                CertificateAndKey {
                    certificate: long_value(CERTIFICATE_SECRET),
                    certificate_chain: Vec::new(),
                    key: long_value(KEY_SECRET),
                    versions: Vec::new(),
                    names: vec![long_value(NAME_SECRET)],
                },
            )]),
        };
        let main_content = ResponseContent::from(ContentType::CertificatesWithFingerprints(
            main_certificates.clone(),
        ));
        let outputs = [
            ("CertificateSummary", format!("{summary:?}")),
            ("CertificatesByAddress", format!("{by_address:?}")),
            ("ListOfCertificatesByAddress", format!("{list:?}")),
            ("worker ResponseContent", format!("{worker_content:?}")),
            ("WorkerResponse", format!("{worker_response:?}")),
            ("WorkerResponses", format!("{worker_responses:?}")),
            ("gathered ResponseContent", format!("{gathered_content:?}")),
            (
                "CertificatesWithFingerprints",
                format!("{main_certificates:?}"),
            ),
            ("main ResponseContent", format!("{main_content:?}")),
        ];
        let secrets = [
            DOMAIN_SECRET,
            FINGERPRINT_SECRET,
            MAP_KEY_SECRET,
            CERTIFICATE_SECRET,
            KEY_SECRET,
            NAME_SECRET,
            WORKER_ID_SECRET,
            RESPONSE_ID_SECRET,
            MESSAGE_SECRET,
        ];

        for (label, output) in &outputs {
            for secret in secrets {
                assert!(
                    !output.contains(secret),
                    "{label} Debug leaked certificate-query marker {secret}: {output}"
                );
            }
            assert!(
                output.len() <= 1024,
                "{label} Debug output is not bounded: {} bytes",
                output.len()
            );
        }

        for metadata in ["listeners_count: 1", "certificates_count: 1"] {
            assert!(
                outputs[2].1.contains(metadata),
                "ListOfCertificatesByAddress Debug omitted bounded metadata {metadata}: {}",
                outputs[2].1
            );
        }
        for metadata in ["id_len:", "message_len:", "content_type:"] {
            assert!(
                outputs[4].1.contains(metadata),
                "WorkerResponse Debug omitted bounded metadata {metadata}: {}",
                outputs[4].1
            );
        }
    }

    #[test]
    fn certificate_debug_redacts_pem_material() {
        const CERTIFICATE_SECRET: &str = "CERTIFICATE_SECRET_SENTINEL";
        const CHAIN_SECRET: &str = "CHAIN_SECRET_SENTINEL";
        const KEY_SECRET: &str = "KEY_SECRET_SENTINEL";
        const NAME_SECRET: &str = "CERTIFICATE_NAME_SECRET_SENTINEL";

        let certificate = CertificateAndKey {
            certificate: CERTIFICATE_SECRET.to_owned(),
            certificate_chain: vec![CHAIN_SECRET.to_owned()],
            key: KEY_SECRET.to_owned(),
            versions: vec![i32::MIN; 1_024],
            names: vec![format!("{NAME_SECRET}{}", "x".repeat(4096))],
        };
        let add_certificate = AddCertificate {
            address: Default::default(),
            certificate: certificate.clone(),
            expired_at: None,
        };
        let replace_certificate = ReplaceCertificate {
            address: Default::default(),
            new_certificate: certificate.clone(),
            old_fingerprint: "safe-old-fingerprint".to_owned(),
            new_expired_at: None,
        };
        let add_request = Request::from(RequestType::AddCertificate(add_certificate.clone()));
        let replace_request =
            Request::from(RequestType::ReplaceCertificate(replace_certificate.clone()));
        let add_worker_request = WorkerRequest {
            id: "safe-add-request-id".to_owned(),
            content: add_request.clone(),
        };
        let replace_worker_request = WorkerRequest {
            id: "safe-replace-request-id".to_owned(),
            content: replace_request.clone(),
        };
        let state_address: SocketAddr = "127.0.0.1:443"
            .parse()
            .expect("test certificate address must parse");
        let state = ConfigState {
            certificates: HashMap::from([(
                state_address,
                HashMap::from([(Fingerprint(vec![0; 32]), certificate.clone())]),
            )]),
            ..Default::default()
        };

        let debug_outputs = [
            format!("{certificate:?}"),
            format!("{add_certificate:?}"),
            format!("{replace_certificate:?}"),
            format!("{add_request:?}"),
            format!("{replace_request:?}"),
            format!("{add_worker_request:?}"),
            format!("{replace_worker_request:?}"),
        ];
        let state_debug = format!("{state:?}");

        for output in debug_outputs {
            for secret in [CERTIFICATE_SECRET, CHAIN_SECRET, KEY_SECRET, NAME_SECRET] {
                assert!(
                    !output.contains(secret),
                    "debug output leaked secret marker {secret}: {output}"
                );
            }
            assert!(
                output.contains("[redacted]"),
                "debug output must make redaction explicit: {output}"
            );
            assert!(
                output.contains("names_count: 1") && output.contains("names_len: 4128"),
                "debug output must retain bounded certificate-name metadata: {output}"
            );
            assert!(
                output.contains("versions_count: 1024"),
                "debug output must retain bounded TLS-version metadata: {output}"
            );
            assert!(
                output.len() <= 4096,
                "certificate Debug output is not bounded: {} bytes",
                output.len()
            );
        }

        for secret in [CERTIFICATE_SECRET, CHAIN_SECRET, KEY_SECRET, NAME_SECRET] {
            assert!(
                !state_debug.contains(secret),
                "ConfigState Debug leaked secret marker {secret}: {state_debug}"
            );
        }
        assert!(
            state_debug.contains("certificate_addresses_count: 1")
                && state_debug.contains("certificates_count: 1"),
            "ConfigState Debug must retain count-only certificate metadata: {state_debug}"
        );
        assert!(
            state_debug.len() <= 4096,
            "ConfigState Debug output is not bounded: {} bytes",
            state_debug.len()
        );
    }

    #[test]
    fn certificate_fingerprint_control_debug_is_redacted_across_wrappers() {
        const FINGERPRINT_SECRET: &str = "CERTIFICATE_CONTROL_FINGERPRINT_SECRET_SENTINEL";

        let fingerprint = format!("{FINGERPRINT_SECRET}{}", "f".repeat(4096));
        let remove = RemoveCertificate {
            address: Default::default(),
            fingerprint: fingerprint.clone(),
        };
        let replace = ReplaceCertificate {
            address: Default::default(),
            new_certificate: CertificateAndKey::default(),
            old_fingerprint: fingerprint.clone(),
            new_expired_at: None,
        };
        let remove_request = Request::from(RequestType::RemoveCertificate(remove.clone()));
        let replace_request = Request::from(RequestType::ReplaceCertificate(replace.clone()));
        let outputs = [
            ("RemoveCertificate", format!("{remove:?}")),
            ("ReplaceCertificate", format!("{replace:?}")),
            ("remove Request", format!("{remove_request:?}")),
            ("replace Request", format!("{replace_request:?}")),
            (
                "remove WorkerRequest",
                format!(
                    "{:?}",
                    WorkerRequest {
                        id: "remove-certificate-test".to_owned(),
                        content: remove_request,
                    }
                ),
            ),
            (
                "replace WorkerRequest",
                format!(
                    "{:?}",
                    WorkerRequest {
                        id: "replace-certificate-test".to_owned(),
                        content: replace_request,
                    }
                ),
            ),
        ];

        for (label, output) in outputs {
            assert!(
                !output.contains(FINGERPRINT_SECRET),
                "{label} Debug leaked the certificate fingerprint: {output}"
            );
            assert!(
                output.contains(&format!("fingerprint_len: {}", fingerprint.len())),
                "{label} Debug omitted bounded fingerprint metadata: {output}"
            );
            assert!(
                output.len() <= 1024,
                "{label} Debug output is not bounded: {} bytes",
                output.len()
            );
        }
    }

    #[test]
    fn cluster_debug_redacts_answers_and_authorization_data_across_wrappers() {
        const CLUSTER_SECRET: &str = "CLUSTER_ID_SECRET_SENTINEL";
        const LEGACY_ANSWER_SECRET: &str = "CLUSTER_LEGACY_ANSWER_SECRET_SENTINEL";
        const ANSWER_KEY_SECRET: &str = "CLUSTER_ANSWER_KEY_SECRET_SENTINEL";
        const ANSWER_BODY_SECRET: &str = "CLUSTER_ANSWER_BODY_SECRET_SENTINEL";
        const AUTH_HASH_SECRET: &str = "CLUSTER_AUTH_HASH_SECRET_SENTINEL";
        const REALM_SECRET: &str = "CLUSTER_AUTH_REALM_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let cluster = Cluster {
            cluster_id: long_value(CLUSTER_SECRET),
            answer_503: Some(long_value(LEGACY_ANSWER_SECRET)),
            answers: BTreeMap::from([(
                long_value(ANSWER_KEY_SECRET),
                long_value(ANSWER_BODY_SECRET),
            )]),
            authorized_hashes: vec![long_value(AUTH_HASH_SECRET)],
            www_authenticate: Some(long_value(REALM_SECRET)),
            ..Default::default()
        };
        let request = Request::from(RequestType::AddCluster(cluster.clone()));
        let outputs = [
            ("Cluster", format!("{cluster:?}")),
            ("Request", format!("{request:?}")),
            (
                "WorkerRequest",
                format!(
                    "{:?}",
                    WorkerRequest {
                        id: "cluster-redaction-test".to_owned(),
                        content: request,
                    }
                ),
            ),
        ];

        for (label, output) in outputs {
            for secret in [
                CLUSTER_SECRET,
                LEGACY_ANSWER_SECRET,
                ANSWER_KEY_SECRET,
                ANSWER_BODY_SECRET,
                AUTH_HASH_SECRET,
                REALM_SECRET,
            ] {
                assert!(
                    !output.contains(secret),
                    "{label} Debug leaked cluster marker {secret}: {output}"
                );
            }
            for metadata in [
                "cluster_id_len:",
                "answers_count: 1",
                "authorized_hashes_count: 1",
            ] {
                assert!(
                    output.contains(metadata),
                    "{label} Debug omitted bounded cluster metadata {metadata}: {output}"
                );
            }
            assert!(
                output.len() <= 2048,
                "{label} Debug output is not bounded: {} bytes",
                output.len()
            );
        }
    }

    #[test]
    fn tcp_frontend_debug_redacts_stored_sni_routing_data_across_wrappers() {
        const CLUSTER_SECRET: &str = "TCP_FRONTEND_CLUSTER_SECRET_SENTINEL";
        const TAG_KEY_SECRET: &str = "TCP_FRONTEND_TAG_KEY_SECRET_SENTINEL";
        const TAG_VALUE_SECRET: &str = "TCP_FRONTEND_TAG_VALUE_SECRET_SENTINEL";
        const SNI_SECRET: &str = "TCP_FRONTEND_SNI_SECRET_SENTINEL";
        const ALPN_SECRET: &str = "TCP_FRONTEND_ALPN_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let frontend = RequestTcpFrontend {
            cluster_id: long_value(CLUSTER_SECRET),
            address: Default::default(),
            tags: BTreeMap::from([(long_value(TAG_KEY_SECRET), long_value(TAG_VALUE_SECRET))]),
            sni: Some(long_value(SNI_SECRET)),
            alpn: vec![long_value(ALPN_SECRET)],
        };
        let request = Request::from(RequestType::AddTcpFrontend(frontend.clone()));
        let outputs = [
            ("RequestTcpFrontend", format!("{frontend:?}")),
            ("Request", format!("{request:?}")),
            (
                "WorkerRequest",
                format!(
                    "{:?}",
                    WorkerRequest {
                        id: "tcp-frontend-redaction-test".to_owned(),
                        content: request,
                    }
                ),
            ),
        ];

        for (label, output) in outputs {
            for secret in [
                CLUSTER_SECRET,
                TAG_KEY_SECRET,
                TAG_VALUE_SECRET,
                SNI_SECRET,
                ALPN_SECRET,
            ] {
                assert!(
                    !output.contains(secret),
                    "{label} Debug leaked TCP frontend marker {secret}: {output}"
                );
            }
            for metadata in [
                "cluster_id_len:",
                "tags_count: 1",
                "sni_len: Some(",
                "alpn_count: 1",
            ] {
                assert!(
                    output.contains(metadata),
                    "{label} Debug omitted bounded TCP frontend metadata {metadata}: {output}"
                );
            }
            assert!(
                output.len() <= 1024,
                "{label} Debug output is not bounded: {} bytes",
                output.len()
            );
        }
    }

    #[test]
    fn merge_relocates_single_worker_to_top_level() {
        // Regression: a one-worker fleet must populate `clusters` and
        // `proxying` so CLI/TUI consumers reading those maps see the
        // worker's data. `std::mem::take(&mut self.workers)` empties the
        // per-worker map after relocation, which is the documented
        // contract when the caller asked for the merged shape.
        let mut worker = WorkerMetrics {
            proxy: BTreeMap::new(),
            clusters: BTreeMap::new(),
        };
        worker.proxy.insert(
            "requests".to_owned(),
            FilteredMetrics {
                inner: Some(Inner::Count(42)),
            },
        );
        let mut cluster = ClusterMetrics {
            cluster: BTreeMap::new(),
            backends: Vec::new(),
        };
        cluster.cluster.insert(
            "requests".to_owned(),
            FilteredMetrics {
                inner: Some(Inner::Count(7)),
            },
        );
        worker.clusters.insert("cluster-a".to_owned(), cluster);

        let mut agg = AggregatedMetrics {
            main: BTreeMap::new(),
            workers: BTreeMap::from([("0".to_owned(), worker)]),
            clusters: BTreeMap::new(),
            proxying: BTreeMap::new(),
        };

        agg.merge_metrics();

        assert!(
            agg.workers.is_empty(),
            "merge takes ownership of the per-worker map"
        );
        assert_eq!(
            agg.proxying.get("requests"),
            Some(&FilteredMetrics {
                inner: Some(Inner::Count(42)),
            }),
            "single worker's proxy counter must surface in proxying"
        );
        let cluster_a = agg
            .clusters
            .get("cluster-a")
            .expect("cluster row must surface in top-level clusters");
        assert_eq!(
            cluster_a.cluster.get("requests"),
            Some(&FilteredMetrics {
                inner: Some(Inner::Count(7)),
            })
        );
    }

    #[test]
    fn merge_counts_and_gauges() {
        let mut gauge_a = FilteredMetrics {
            inner: Some(Inner::Gauge(4)),
        };
        let gauge_b = FilteredMetrics {
            inner: Some(Inner::Gauge(4)),
        };

        gauge_a.merge(&gauge_b);

        assert_eq!(
            gauge_a,
            FilteredMetrics {
                inner: Some(Inner::Gauge(8)),
            }
        );

        let mut count_a = FilteredMetrics {
            inner: Some(Inner::Count(3)),
        };
        let count_b = FilteredMetrics {
            inner: Some(Inner::Count(3)),
        };

        count_a.merge(&count_b);

        assert_eq!(
            count_a,
            FilteredMetrics {
                inner: Some(Inner::Count(6)),
            }
        );
    }

    #[test]
    fn merge_percentiles_takes_max_per_quantile() {
        // Multi-worker percentile aggregation propagates the worst-case
        // quantile across workers and accumulates samples + sum so the
        // surfaced summary remains the "is anyone slow?" upper bound.
        let mut left = FilteredMetrics {
            inner: Some(Inner::Percentiles(Percentiles {
                samples: 100,
                p_50: 5,
                p_90: 20,
                p_99: 100,
                p_99_9: 200,
                p_99_99: 250,
                p_99_999: 300,
                p_100: 400,
                sum: 12_000,
            })),
        };
        let right = FilteredMetrics {
            inner: Some(Inner::Percentiles(Percentiles {
                samples: 50,
                p_50: 7,
                p_90: 15,
                p_99: 80,
                p_99_9: 240,
                p_99_99: 245,
                p_99_999: 290,
                p_100: 380,
                sum: 6_000,
            })),
        };
        left.merge(&right);
        assert_eq!(
            left,
            FilteredMetrics {
                inner: Some(Inner::Percentiles(Percentiles {
                    samples: 150,
                    p_50: 7,
                    p_90: 20,
                    p_99: 100,
                    p_99_9: 240,
                    p_99_99: 250,
                    p_99_999: 300,
                    p_100: 400,
                    sum: 18_000,
                })),
            }
        );
    }

    #[test]
    fn merge_histograms() {
        let mut histogram_a = FilteredMetrics {
            inner: Some(Inner::Histogram(FilteredHistogram {
                sum: 95,
                count: 30,
                buckets: vec![
                    Bucket { le: 0, count: 1 },
                    Bucket { le: 1, count: 2 },
                    Bucket { le: 3, count: 10 },
                    Bucket { le: 7, count: 25 },
                    Bucket { le: 15, count: 27 },
                    Bucket { le: 31, count: 30 },
                ],
            })),
        };

        let histogram_b = FilteredMetrics {
            inner: Some(Inner::Histogram(FilteredHistogram {
                sum: 82,
                count: 40,
                buckets: vec![
                    Bucket { le: 0, count: 0 },
                    Bucket { le: 1, count: 0 },
                    Bucket { le: 3, count: 12 },
                    Bucket { le: 7, count: 30 },
                    Bucket { le: 15, count: 40 },
                    // note: there is no bucket for "le: 31"
                ],
            })),
        };

        histogram_a.merge(&histogram_b);

        let merged_histogram = FilteredMetrics {
            inner: Some(Inner::Histogram(FilteredHistogram {
                sum: 177,
                count: 70,
                buckets: vec![
                    Bucket { le: 0, count: 1 },
                    Bucket { le: 1, count: 2 },
                    Bucket { le: 3, count: 22 },
                    Bucket { le: 7, count: 55 },
                    Bucket { le: 15, count: 67 },
                    Bucket { le: 31, count: 70 }, // note: the total count of histogram b is added, even though histogram b has no bucket
                ],
            })),
        };

        assert_eq!(histogram_a, merged_histogram);
    }
}
