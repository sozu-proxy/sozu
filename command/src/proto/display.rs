use std::fmt::{Display, Formatter};

use crate::proto::command::TlsVersion;

use super::command::{CertificateAndKey, CertificateWithNames};

impl Display for CertificateWithNames {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "\thostnames: {}\n\tcertificate: {}",
            concatenate_vector(&self.names),
            self.certificate
        )
    }
}

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

fn concatenate_vector(vec: &Vec<String>) -> String {
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
