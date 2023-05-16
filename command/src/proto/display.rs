use std::fmt::{Display, Formatter};

use super::command::CertificateWithNames;

impl Display for CertificateWithNames {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut hostnames = String::new();
        for name in &self.names {
            hostnames.push_str(&name);
            hostnames.push_str(", ")
        }
        write!(
            f,
            "\thostnames: {}\n\tcertificate: {}",
            hostnames, self.certificate
        )
    }
}
