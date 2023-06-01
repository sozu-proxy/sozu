use std::fmt::{Display, Formatter};

use crate::proto::command::TlsVersion;

use super::command::{
    request::RequestType, CertificateAndKey, CertificateSummary, QueryCertificatesFilters,
};

impl Display for CertificateAndKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let versions = self.versions.iter().fold(String::new(), |acc, tls_v| {
            acc + " "
                + match TlsVersion::from_i32(*tls_v) {
                    Some(v) => v.as_str_name(),
                    None => "",
                }
        });
        write!(
            f,
            "\tcertificate: {}\n\tcertificate_chain: {:?}\n\tkey: {}\n\tTLS versions: {}\n\tnames: {:?}",
            self.certificate, self.certificate_chain, self.key, versions,
            concatenate_vector(&self.names)
        )
    }
}

impl Display for CertificateSummary {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:\t{}", self.fingerprint, self.domain)
    }
}

impl Display for QueryCertificatesFilters {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(d) = self.domain.clone() {
            write!(f, "domain:{}", d)
        } else if let Some(fp) = self.fingerprint.clone() {
            write!(f, "domain:{}", fp)
        } else {
            write!(f, "all certificates")
        }
    }
}

pub fn concatenate_vector(vec: &Vec<String>) -> String {
    let mut vec = vec.clone();
    let mut concatenated = match vec.pop() {
        Some(s) => s,
        None => return String::from("empty"),
    };
    for s in vec {
        concatenated.push_str(&s);
        concatenated.push_str(", ");
    }
    concatenated
}

pub fn format_request_type(request_type: &RequestType) -> String {
    match request_type {
        RequestType::SaveState(_) => "SaveState".to_owned(),
        RequestType::LoadState(_) => "LoadState".to_owned(),
        RequestType::CountRequests(_) => "CountRequests".to_owned(),
        RequestType::ListWorkers(_) => "ListWorkers".to_owned(),
        RequestType::ListFrontends(_) => "ListFrontends".to_owned(),
        RequestType::ListListeners(_) => "ListListeners".to_owned(),
        RequestType::LaunchWorker(_) => "LaunchWorker".to_owned(),
        RequestType::UpgradeMain(_) => "UpgradeMain".to_owned(),
        RequestType::UpgradeWorker(_) => "UpgradeWorker".to_owned(),
        RequestType::SubscribeEvents(_) => "SubscribeEvents".to_owned(),
        RequestType::ReloadConfiguration(_) => "ReloadConfiguration".to_owned(),
        RequestType::Status(_) => "Status".to_owned(),
        RequestType::AddCluster(_) => "AddCluster".to_owned(),
        RequestType::RemoveCluster(_) => "RemoveCluster".to_owned(),
        RequestType::AddHttpFrontend(_) => "AddHttpFrontend".to_owned(),
        RequestType::RemoveHttpFrontend(_) => "RemoveHttpFrontend".to_owned(),
        RequestType::AddHttpsFrontend(_) => "AddHttpsFrontend".to_owned(),
        RequestType::RemoveHttpsFrontend(_) => "RemoveHttpsFrontend".to_owned(),
        RequestType::AddCertificate(_) => "AddCertificate".to_owned(),
        RequestType::ReplaceCertificate(_) => "ReplaceCertificate".to_owned(),
        RequestType::RemoveCertificate(_) => "RemoveCertificate".to_owned(),
        RequestType::AddTcpFrontend(_) => "AddTcpFrontend".to_owned(),
        RequestType::RemoveTcpFrontend(_) => "RemoveTcpFrontend".to_owned(),
        RequestType::AddBackend(_) => "AddBackend".to_owned(),
        RequestType::RemoveBackend(_) => "RemoveBackend".to_owned(),
        RequestType::AddHttpListener(_) => "AddHttpListener".to_owned(),
        RequestType::AddHttpsListener(_) => "AddHttpsListener".to_owned(),
        RequestType::AddTcpListener(_) => "AddTcpListener".to_owned(),
        RequestType::RemoveListener(_) => "RemoveListener".to_owned(),
        RequestType::ActivateListener(_) => "ActivateListener".to_owned(),
        RequestType::DeactivateListener(_) => "DeactivateListener".to_owned(),
        RequestType::QueryClusterById(_) => "QueryClusterById".to_owned(),
        RequestType::QueryClustersByDomain(_) => "QueryClustersByDomain".to_owned(),
        RequestType::QueryClustersHashes(_) => "QueryClustersHashes".to_owned(),
        RequestType::QueryMetrics(_) => "QueryMetrics".to_owned(),
        RequestType::SoftStop(_) => "SoftStop".to_owned(),
        RequestType::HardStop(_) => "HardStop".to_owned(),
        RequestType::ConfigureMetrics(_) => "ConfigureMetrics".to_owned(),
        RequestType::Logging(_) => "Logging".to_owned(),
        RequestType::ReturnListenSockets(_) => "ReturnListenSockets".to_owned(),
        RequestType::QueryCertificatesFromTheState(_) => "QueryCertificatesFromTheState".to_owned(),
        RequestType::QueryCertificatesFromWorkers(_) => "QueryCertificatesFromWorkers".to_owned(),
    }
}
