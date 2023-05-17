use std::fmt::{Display, Formatter};

use crate::proto::command::TlsVersion;

use super::command::{CertificateAndKey, CertificateSummary, QueryCertificatesFilters};

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
