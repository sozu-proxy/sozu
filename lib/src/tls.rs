//! A unified certificate resolver for rustls.
//!
//! Persists certificates in the Rustls
//! [`CertifiedKey` format](https://docs.rs/rustls/latest/rustls/sign/struct.CertifiedKey.html),
//! exposes them to the HTTPS listener for TLS handshakes.
#[cfg(test)]
use std::collections::HashSet;
use std::{
    collections::HashMap,
    fmt,
    str::FromStr,
    sync::{Arc, LazyLock, Mutex},
};

use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

use crate::crypto::any_supported_type;
use sha2::{Digest, Sha256};
use sozu_command::{
    certificate::{
        CertificateError, Fingerprint, get_cn_and_san_attributes, parse_pem, parse_x509,
        split_certificate_chain,
    },
    logging::ansi_palette,
    proto::command::{AddCertificate, CertificateAndKey, ReplaceCertificate, SocketAddress},
};

use crate::metrics::names;
use crate::router::pattern_trie::{Key, KeyValue, TrieNode};

/// Module-level prefix used on every log line emitted from this module.
/// Produces a bold bright-white `TLS-RESOLVER` label (uniform across every
/// protocol) when the logger is in colored mode. The certificate resolver
/// runs at listener scope -- it has no per-session state -- so this is the
/// only macro the module needs. `RUSTLS` covers the protocol-side logs in
/// `lib/src/protocol/rustls.rs`; `TLS-RESOLVER` is intentionally distinct so
/// operators can tell handshake failures (RUSTLS) apart from cert-store
/// management noise (TLS-RESOLVER).
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!(
            "{open}TLS-RESOLVER{reset}\t >>>",
            open = open,
            reset = reset
        )
    }};
}

// -----------------------------------------------------------------------------
// Default ParsedCertificateAndKey

static DEFAULT_CERTIFICATE: LazyLock<Option<Arc<CertifiedKey>>> = LazyLock::new(|| {
    let add = AddCertificate {
        certificate: CertificateAndKey {
            certificate: include_str!("../assets/certificate.pem").to_string(),
            certificate_chain: vec![include_str!("../assets/certificate_chain.pem").to_string()],
            key: include_str!("../assets/key.pem").to_string(),
            versions: vec![],
            names: vec![],
        },
        address: SocketAddress::new_v4(0, 0, 0, 0, 8080), // not used anyway
        expired_at: None,
    };
    CertifiedKeyWrapper::try_from(&add).ok().map(|c| c.inner)
});

#[derive(thiserror::Error, Debug)]
pub enum CertificateResolverError {
    #[error("failed to get common name and subject alternate names from pem, {0}")]
    InvalidCommonNameAndSubjectAlternateNames(CertificateError),
    #[error("invalid private key: {0}")]
    InvalidPrivateKey(String),
    #[error("empty key")]
    EmptyKeys,
    #[error("error parsing x509 cert from bytes: {0}")]
    ParseX509(CertificateError),
    #[error("error parsing pem formated certificate from bytes: {0}")]
    ParsePem(CertificateError),
    #[error("error parsing overriding names in new certificate: {0}")]
    ParseOverridingNames(CertificateError),
}

/// A wrapper around the Rustls
/// [`CertifiedKey` type](https://docs.rs/rustls/latest/rustls/sign/struct.CertifiedKey.html),
/// stored and returned by the certificate resolver.
#[derive(Clone, Debug)]
pub struct CertifiedKeyWrapper {
    inner: Arc<CertifiedKey>,
    /// domain names, override what can be found in the cert
    names: Vec<String>,
    expiration: i64,
    fingerprint: Fingerprint,
}

/// Convert an AddCertificate request into the Rustls format.
/// Support RSA and ECDSA certificates.
impl TryFrom<&AddCertificate> for CertifiedKeyWrapper {
    type Error = CertificateResolverError;

    fn try_from(add: &AddCertificate) -> Result<Self, Self::Error> {
        let cert = add.certificate.clone();

        let pem =
            parse_pem(cert.certificate.as_bytes()).map_err(CertificateResolverError::ParsePem)?;

        let x509 = parse_x509(&pem.contents).map_err(CertificateResolverError::ParseX509)?;

        let overriding_names = if add.certificate.names.is_empty() {
            get_cn_and_san_attributes(&x509)
        } else {
            add.certificate.names.clone()
        };

        let expiration = add
            .expired_at
            .unwrap_or(x509.validity().not_after.timestamp());

        let fingerprint = Fingerprint(Sha256::digest(&pem.contents).iter().cloned().collect());

        // The leaf is at index 0; chain entries follow. ACME clients
        // emitting `fullchain.pem` (Certbot default, lego, acme.sh)
        // place the leaf at the start, which would store
        // `[leaf, leaf, intermediate, root]` and fail strict
        // validators (Node.js `UNABLE_TO_VERIFY_LEAF_SIGNATURE`).
        // Each entry is split through `split_certificate_chain` so a
        // single multi-PEM string fans out (`parse_pem` would
        // otherwise stop at the first block); each split entry is
        // dedup'd against the leaf's DER bytes.
        let leaf_der = pem.contents;
        let mut chain = vec![CertificateDer::from(leaf_der.to_owned())];
        let mut dropped_duplicates = 0usize;
        for cert in &cert.certificate_chain {
            for split_pem in split_certificate_chain(cert.to_owned()) {
                let chain_link = parse_pem(split_pem.as_bytes())
                    .map_err(CertificateResolverError::ParsePem)?
                    .contents;

                if chain_link == leaf_der {
                    dropped_duplicates += 1;
                    continue;
                }
                chain.push(CertificateDer::from(chain_link));
            }
        }
        if dropped_duplicates > 0 {
            debug!(
                "{} dropped {} duplicate leaf certificate(s) from the supplied chain",
                log_module_context!(),
                dropped_duplicates
            );
        }

        // Parse the PEM-encoded private key into a `PrivateKeyDer` via
        // `rustls-pki-types`'s `PemObject` trait. `from_pem_slice` accepts
        // PKCS1 / PKCS8 / SEC1 key formats the same way the old
        // `rustls-pemfile::read_one` + per-variant `From::from` chain did,
        // and folds the empty-input / no-PEM-object / unsupported-format
        // cases into a single `Err` we surface as `EmptyKeys` (the
        // existing variant covers any failure to extract a key from the
        // supplied PEM blob).
        let private_key = PrivateKeyDer::from_pem_slice(cert.key.as_bytes())
            .map_err(|_| CertificateResolverError::EmptyKeys)?;

        match any_supported_type(&private_key) {
            Ok(signing_key) => {
                let stored_certificate = CertifiedKeyWrapper {
                    inner: Arc::new(CertifiedKey::new(chain, signing_key)),
                    names: overriding_names,
                    expiration,
                    fingerprint,
                };
                Ok(stored_certificate)
            }
            Err(sign_error) => Err(CertificateResolverError::InvalidPrivateKey(
                sign_error.to_string(),
            )),
        }
    }
}

/// Parses and stores TLS certificates, makes them available to Rustls for TLS handshakes
///
/// the `domains` TrieNode is an addressing system to resolve a certificate
/// for a given domain name.
/// Certificates are stored in a hashmap that may contain unreachable certificates if
/// no domain name points to it.
#[derive(Default, Debug)]
pub struct CertificateResolver {
    /// routing one domain name to one certificate for fast resolving
    pub domains: TrieNode<Fingerprint>,
    /// a storage map: fingerprint -> stored_certificate
    certificates: HashMap<Fingerprint, CertifiedKeyWrapper>,
    /// maps each domain name to several compatible certificates, sorted by expiration date
    /// map of domain_name -> all fingerprints (and expiration) linked to this domain name
    //  the vector of (fingerprint, expiration) is sorted by expiration
    name_fingerprint_idx: HashMap<String, Vec<(Fingerprint, i64)>>,
}

impl CertificateResolver {
    /// return the certificate in the Rustls-usable form
    pub fn get_certificate(&self, fingerprint: &Fingerprint) -> Option<CertifiedKeyWrapper> {
        self.certificates.get(fingerprint).map(ToOwned::to_owned)
    }

    /// Recompute the aggregate `tls.cert.min_expires_at_seconds` gauge across
    /// every certificate currently loaded. Per-SNI granularity would explode
    /// statsd key cardinality (the resolver can easily hold tens of thousands
    /// of names on a public endpoint) and the existing `gauge!` macro has no
    /// label support, so we expose a single absolute unix-seconds reading of
    /// the soonest-expiring cert. Dashboards alert on this as the "next cert
    /// to rotate" deadline; operators query per-cert detail through the
    /// command API. `x509` timestamps are signed but `set_gauge` takes a
    /// `usize`, so we clamp already-expired certs to 0 (which is still a
    /// monotonic "panic now" signal to any alerting rule).
    ///
    /// Called from `add_certificate` / `remove_certificate` — i.e. only when
    /// the cert set actually changes, never on the hot TLS handshake path.
    fn publish_min_expiration_gauge(&self) {
        let Some(min_expiration) = self.certificates.values().map(|c| c.expiration).min() else {
            // SECURITY: an empty resolver is not "every cert just
            // expired"; it is "no cert
            // has been loaded yet" — typical at process boot before the
            // first AddCertificate request lands. Writing 0 here pages
            // SOC tooling on every restart with the same alert as a real
            // expired-cert event. Skip the emit so the gauge reflects the
            // last known good state instead of being clobbered to 0.
            return;
        };
        let clamped = min_expiration.max(0) as usize;
        gauge!(names::tls::CERT_MIN_EXPIRES_AT_SECONDS, clamped);
    }

    /// persist a certificate, after ensuring validity, and checking if it can replace another certificate.
    /// return the certificate fingerprint regardless of having inserted it or not
    pub fn add_certificate(
        &mut self,
        add: &AddCertificate,
    ) -> Result<Fingerprint, CertificateResolverError> {
        let cert_to_add = CertifiedKeyWrapper::try_from(add)?;

        trace!(
            "{} adding certificate {:?}",
            log_module_context!(),
            cert_to_add
        );

        if self.certificates.contains_key(&cert_to_add.fingerprint) {
            return Ok(cert_to_add.fingerprint);
        }

        for new_name in &cert_to_add.names {
            let fingerprints_for_this_name = self
                .name_fingerprint_idx
                .entry(new_name.to_owned())
                .or_default();

            fingerprints_for_this_name
                .push((cert_to_add.fingerprint.clone(), cert_to_add.expiration));

            // sort expiration ascending (longest-lived to the right)
            fingerprints_for_this_name.sort_by_key(|t| t.1);

            let longest_lived_cert = match fingerprints_for_this_name.last() {
                Some(cert) => cert,
                None => {
                    error!(
                        "{} no fingerprint for this name, this should not happen",
                        log_module_context!()
                    );
                    continue;
                }
            };

            // update the longest lived certificate in the TriNode
            self.domains.remove(&new_name.to_owned().into_bytes());
            self.domains.insert(
                new_name.to_owned().into_bytes(),
                longest_lived_cert.0.to_owned(),
            );
        }

        self.certificates
            .insert(cert_to_add.fingerprint.to_owned(), cert_to_add.clone());
        self.publish_min_expiration_gauge();

        trace!("{} {:#?}", log_module_context!(), self);

        Ok(cert_to_add.fingerprint)
    }

    /// Delete a certificate from the resolver. May fail if there is no alternative for
    // a domain name
    pub fn remove_certificate(
        &mut self,
        fingerprint: &Fingerprint,
    ) -> Result<(), CertificateResolverError> {
        if let Some(certificate_to_remove) = self.get_certificate(fingerprint) {
            for name in certificate_to_remove.names {
                self.domains.domain_remove(&name.as_bytes().to_vec());

                if let std::collections::hash_map::Entry::Occupied(mut entry) =
                    self.name_fingerprint_idx.entry(name.to_owned())
                {
                    // remove fingerprints from the index for this name
                    entry.get_mut().retain(|t| &t.0 != fingerprint);

                    // reinsert the longest lived certificate in the TrieNode
                    if let Some(longest_lived_cert) = entry.get().last() {
                        self.domains
                            .insert(name.as_bytes().to_vec(), longest_lived_cert.0.to_owned());
                    }

                    // clean up empty index entries to avoid memory leaks
                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
            }

            self.certificates.remove(fingerprint);
            self.publish_min_expiration_gauge();
        }
        trace!("{} {:#?}", log_module_context!(), self);

        Ok(())
    }

    /// Add the new certificate first, then remove the old one.
    /// This ordering ensures that the old certificate remains available
    /// if adding the new one fails.
    pub fn replace_certificate(
        &mut self,
        replace: &ReplaceCertificate,
    ) -> Result<Fingerprint, CertificateResolverError> {
        let add = AddCertificate {
            address: replace.address.to_owned(),
            certificate: replace.new_certificate.to_owned(),
            expired_at: replace.new_expired_at.to_owned(),
        };

        // ── Idempotent-replace short-circuit ──
        //
        // Compute the new fingerprint *before* mutating the resolver so we
        // can compare it with the old one. When `add_certificate` is
        // called with a fingerprint that already exists it early-returns
        // (lib/src/tls.rs add_certificate path) without inserting; if we
        // then unconditionally called `remove_certificate(old)` and old
        // equalled new, we would delete the entry the caller intended to
        // *retain*. An idempotent renewal — typical for retry loops, dead
        // ACME polls, or operator-driven `ReplaceCertificate` requests
        // that resubmit the same PEM — must therefore short-circuit here.
        // Any failure to materialise the wrapper (parse, sign-key check)
        // surfaces as `CertificateResolverError`, identical to the path
        // through `add_certificate`.
        let new_cert = CertifiedKeyWrapper::try_from(&add)?;
        let new_fingerprint = new_cert.fingerprint.to_owned();

        if let Ok(old_fingerprint) = Fingerprint::from_str(&replace.old_fingerprint) {
            if old_fingerprint == new_fingerprint {
                // Re-publish the expiration gauge so dashboards observe the
                // replace request even when the certificate set is unchanged.
                self.publish_min_expiration_gauge();
                return Ok(new_fingerprint);
            }
        }

        let new_fingerprint = self.add_certificate(&add)?;

        match Fingerprint::from_str(&replace.old_fingerprint) {
            Ok(old_fingerprint) => self.remove_certificate(&old_fingerprint)?,
            Err(err) => {
                // The new certificate was already added above. If we can't parse the old
                // fingerprint, the old certificate remains in the resolver (leaked).
                // We return Ok to indicate the new certificate is available, but warn
                // that cleanup of the old one failed.
                warn!(
                    "{} new certificate added but could not remove old one: \
                     failed to parse old fingerprint, {}",
                    log_module_context!(),
                    err
                );
            }
        }

        Ok(new_fingerprint)
    }

    /// return all fingerprints that are available for these domain names,
    /// provided at least one name is given
    #[cfg(test)]
    fn find_certificates_by_names(
        &self,
        names: &HashSet<String>,
    ) -> Result<HashSet<Fingerprint>, CertificateResolverError> {
        let mut fingerprints = HashSet::new();
        for name in names {
            if let Some(fprints) = self.name_fingerprint_idx.get(name) {
                fprints.iter().for_each(|fingerprint| {
                    fingerprints.insert(fingerprint.to_owned().0);
                });
            }
        }

        Ok(fingerprints)
    }

    /// return the hashset of subjects that the certificate is able to handle.
    /// the certificate must be already persisted for this check
    #[cfg(test)]
    fn certificate_names(
        &self,
        fingerprint: &Fingerprint,
    ) -> Result<HashSet<String>, CertificateResolverError> {
        if let Some(cert) = self.certificates.get(fingerprint) {
            return Ok(cert.names.iter().cloned().collect());
        }
        Ok(HashSet::new())
    }

    pub fn domain_lookup(
        &self,
        domain: &[u8],
        accept_wildcard: bool,
    ) -> Option<&KeyValue<Key, Fingerprint>> {
        self.domains.domain_lookup(domain, accept_wildcard)
    }

    /// Resolve the SAN set Sōzu would serve for `domain` (the same trie
    /// lookup rustls uses, wildcard-aware via `domain_lookup`). Returns the
    /// certificate's `names` exactly as stored — wildcards retain their
    /// leading `*.` so the caller can apply RFC 6125 §6.4.3 matching. `None`
    /// when no cert covers `domain` (rustls would fall back to
    /// `DEFAULT_CERTIFICATE`).
    ///
    /// Mirrors `MutexCertificateResolver::resolve` minus the rustls glue, so
    /// the SAN snapshot taken at handshake matches the certificate the peer
    /// actually validated (RFC 7540 §9.1.1 / RFC 9113 §9.1.1 connection
    /// reuse).
    pub fn names_for_sni(&self, domain: &[u8]) -> Option<Vec<String>> {
        let (_, fingerprint) = self.domain_lookup(domain, true)?;
        self.certificates
            .get(fingerprint)
            .map(|cert| cert.names.clone())
    }
}

// -----------------------------------------------------------------------------
// MutexWrappedCertificateResolver struct

#[derive(Default)]
pub struct MutexCertificateResolver(pub Mutex<CertificateResolver>);

impl ResolvesServerCert for MutexCertificateResolver {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        let server_name = client_hello.server_name();
        let sigschemes = client_hello.signature_schemes();

        let Some(name) = server_name else {
            error!(
                "{} cannot look up certificate: no SNI from session",
                log_module_context!()
            );
            return None;
        };
        trace!(
            "{} trying to resolve name: {:?} for signature scheme: {:?}",
            log_module_context!(),
            name,
            sigschemes
        );
        // Every other site uses blocking `lock()`, and silently falling
        // back to `DEFAULT_CERTIFICATE` on lock
        // contention is an attacker-detectable mismatch (different
        // chain → different fingerprint) and a footgun the moment
        // multi-threading enters the worker. Block here. Lock-poisoning
        // (panic-while-holding) is mapped to the same default-cert
        // fallback the previous `try_lock` Err arm produced — preserves
        // the existing observable behaviour for that one corner case
        // without inventing a new failure mode.
        let resolver = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!(
                    "{} cert resolver mutex poisoned, returning default cert: {:?}",
                    log_module_context!(),
                    poisoned
                );
                return DEFAULT_CERTIFICATE.clone();
            }
        };
        if let Some((_, fingerprint)) = resolver.domains.domain_lookup(name.as_bytes(), true) {
            trace!(
                "{} looking for certificate for {:?} with fingerprint {:?}",
                log_module_context!(),
                name,
                fingerprint
            );

            let cert = resolver
                .certificates
                .get(fingerprint)
                .map(|cert| cert.inner.clone());

            trace!(
                "{} found for fingerprint {}: {}",
                log_module_context!(),
                fingerprint,
                cert.is_some()
            );
            return cert;
        }
        drop(resolver);

        // error!("could not look up a certificate for server name '{}'", name);
        // This certificate is used for TLS tunneling with another TLS termination endpoint
        // Note that this is unsafe and you should provide a valid certificate
        debug!(
            "{} default certificate is used for {}",
            log_module_context!(),
            name
        );
        incr!(names::tls::DEFAULT_CERT_USED);
        DEFAULT_CERTIFICATE.clone()
    }
}

impl MutexCertificateResolver {
    /// Snapshot of the SAN set Sōzu would serve for `domain`. Acquires the
    /// resolver lock once. Returns `None` when the underlying mutex is
    /// poisoned — the caller is expected to treat poison the same as
    /// "default cert served" (legacy fallback), mirroring `resolve`'s
    /// own poison handling at the rustls hot path.
    pub fn names_for_sni(&self, domain: &[u8]) -> Option<Vec<String>> {
        match self.0.lock() {
            Ok(guard) => guard.names_for_sni(domain),
            Err(poisoned) => {
                error!(
                    "{} cert resolver mutex poisoned, treating as no SAN match: {:?}",
                    log_module_context!(),
                    poisoned
                );
                None
            }
        }
    }
}

impl fmt::Debug for MutexCertificateResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MutexWrappedCertificateResolver")
    }
}

// -----------------------------------------------------------------------------
// Unit tests

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        error::Error,
        time::{Duration, SystemTime},
    };

    // use rand::{seq::SliceRandom, thread_rng};
    use sozu_command::proto::command::{
        AddCertificate, CertificateAndKey, ReplaceCertificate, SocketAddress,
    };

    use super::CertificateResolver;

    #[test]
    fn lifecycle() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();
        let certificate_and_key = CertificateAndKey {
            certificate: String::from(include_str!("../assets/certificate.pem")),
            key: String::from(include_str!("../assets/key.pem")),
            ..Default::default()
        };

        let fingerprint = resolver
            .add_certificate(&AddCertificate {
                address,
                certificate: certificate_and_key,
                expired_at: None,
            })
            .expect("could not add certificate");

        if resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        // get the names to try and retrieve the certificate AFTER it is supposed to be removed
        let names = resolver.certificate_names(&fingerprint)?;

        if let Err(err) = resolver.remove_certificate(&fingerprint) {
            return Err(format!("the certificate was not removed, {err}").into());
        }

        if resolver.get_certificate(&fingerprint).is_some() {
            return Err("We have retrieved the certificate that should be deleted".into());
        }

        if !resolver.find_certificates_by_names(&names)?.is_empty() {
            return Err(
                "The certificate should be deleted but one of its names is in the index".into(),
            );
        }

        Ok(())
    }

    #[test]
    fn name_override() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();
        let certificate_and_key = CertificateAndKey {
            certificate: String::from(include_str!("../assets/certificate.pem")),
            key: String::from(include_str!("../assets/key.pem")),
            names: vec!["localhost".into(), "lolcatho.st".into()],
            ..Default::default()
        };

        let fingerprint = resolver.add_certificate(&AddCertificate {
            address,
            certificate: certificate_and_key,
            expired_at: None,
        })?;

        if resolver.get_certificate(&fingerprint).is_none() {
            return Err("failed to retrieve certificate".into());
        }

        let mut lolcat = HashSet::new();
        lolcat.insert(String::from("lolcatho.st"));
        if resolver.find_certificates_by_names(&lolcat)?.is_empty()
            || resolver.get_certificate(&fingerprint).is_none()
        {
            return Err("failed to retrieve certificate with custom names".into());
        }

        if let Err(err) = resolver.remove_certificate(&fingerprint) {
            return Err(format!("the certificate could not be removed, {err}").into());
        }

        let names = resolver.certificate_names(&fingerprint)?;
        if !resolver.find_certificates_by_names(&names)?.is_empty()
            && resolver.get_certificate(&fingerprint).is_some()
        {
            return Err("We have retrieved the certificate that should be deleted".into());
        }

        Ok(())
    }

    #[test]
    fn keep_resolving_with_wildcard() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        // ---------------------------------------------------------------------
        // load the wildcard certificate,  expiring in 3 years
        let wildcard_example_org = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-3.pem")),
            key: String::from(include_str!("../assets/tests/key.pem")),
            ..Default::default()
        };

        let wildcard_example_org_fingerprint = resolver.add_certificate(&AddCertificate {
            address,
            certificate: wildcard_example_org,
            expired_at: Some(
                (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?
                    + Duration::from_secs(365 * 24 * 3600))
                .as_secs() as i64,
            ),
        })?;

        if resolver
            .get_certificate(&wildcard_example_org_fingerprint)
            .is_none()
        {
            return Err("could not load the 2-year-valid certificate".into());
        }

        // ---------------------------------------------------------------------
        // try loading the ordinary certificate, expiring in 2 years
        // this one has two names: example.org and www.example.org
        let www_example_org = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-2.pem")),
            key: String::from(include_str!("../assets/tests/key.pem")),
            ..Default::default()
        };

        let www_example_org_fingerprint = resolver.add_certificate(&AddCertificate {
            address,
            certificate: www_example_org,
            expired_at: Some(
                (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?
                    + Duration::from_secs(2 * 365 * 24 * 3600))
                .as_secs() as i64,
            ),
        })?;

        let www_example_org = resolver
            .domain_lookup("www.example.org".as_bytes(), true)
            .expect("there should be a www.example.org cert");
        assert_eq!(www_example_org.1, www_example_org_fingerprint);

        let test_example_org = resolver
            .domain_lookup("test.example.org".as_bytes(), true)
            .expect("there should be a test.example.org cert");
        assert_eq!(test_example_org.1, wildcard_example_org_fingerprint);

        let example_org = resolver
            .domain_lookup("example.org".as_bytes(), true)
            .expect("there should be a example.org cert");
        assert_eq!(example_org.1, www_example_org_fingerprint);

        // check that when removing the www.example.org certificate
        // the resolver falls back on the wildcard
        resolver
            .remove_certificate(&www_example_org_fingerprint)
            .expect("should be able to remove the 2-year certificate");

        let should_be_wildcard_fingerprint = resolver
            .domain_lookup("www.example.org".as_bytes(), true)
            .expect("there should be a www.example.org cert");
        assert_eq!(
            should_be_wildcard_fingerprint.1,
            wildcard_example_org_fingerprint
        );

        assert!(
            resolver
                .domain_lookup("example.org".as_bytes(), true)
                .is_none()
        );

        Ok(())
    }

    #[test]
    fn resolve_the_longer_lived_cert() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        // ---------------------------------------------------------------------
        // load the 2-year valid certificate
        let certificate_and_key_2y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-2y.pem")),
            key: String::from(include_str!("../assets/tests/key-2y.pem")),
            ..Default::default()
        };

        let fingerprint_2y = resolver.add_certificate(&AddCertificate {
            address,
            certificate: certificate_and_key_2y,
            expired_at: None,
        })?;

        if resolver.get_certificate(&fingerprint_2y).is_none() {
            return Err("could not load the 2-year-valid certificate".into());
        }

        // ---------------------------------------------------------------------
        // try loading the 1-year valid certificate
        let certificate_and_key_1y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key-1y.pem")),
            ..Default::default()
        };

        let fingerprint_1y = resolver.add_certificate(&AddCertificate {
            address,
            certificate: certificate_and_key_1y,
            ..Default::default()
        })?;

        let localhost_cert = resolver
            .domain_lookup("localhost".as_bytes(), true)
            .expect("there should be a localhost cert");

        assert_eq!(localhost_cert.1, fingerprint_2y);

        // check that when removing the longer-lived certificate,
        // the resolver falls back on the shorter-lived one

        resolver
            .remove_certificate(&fingerprint_2y)
            .expect("should be able to remove the 2-year certificate");

        let localhost_cert = resolver
            .domain_lookup("localhost".as_bytes(), true)
            .expect("there should be a localhost cert");

        assert_eq!(localhost_cert.1, fingerprint_1y);

        Ok(())
    }

    #[test]
    fn expiration_override() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        // ---------------------------------------------------------------------
        // load first certificate
        let certificate_and_key_1y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key-1y.pem")),
            ..Default::default()
        };

        let fingerprint_1y_overriden = resolver.add_certificate(&AddCertificate {
            address,
            certificate: certificate_and_key_1y,
            expired_at: Some(
                (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?
                    + Duration::from_secs(3 * 365 * 24 * 3600))
                .as_secs() as i64,
            ),
        })?;

        if resolver
            .get_certificate(&fingerprint_1y_overriden)
            .is_none()
        {
            return Err("failed to retrieve certificate".into());
        }

        // ---------------------------------------------------------------------
        // load second certificate
        let certificate_and_key_2y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-2y.pem")),
            key: String::from(include_str!("../assets/tests/key-2y.pem")),
            ..Default::default()
        };

        let fingerprint_2y = resolver.add_certificate(&AddCertificate {
            address,
            certificate: certificate_and_key_2y,
            expired_at: None,
        })?;

        let localhost_cert = resolver
            .domain_lookup("localhost".as_bytes(), true)
            .expect("there should be a localhost cert");

        assert_eq!(localhost_cert.1, fingerprint_1y_overriden);

        // check that when removing the overriden certificate,
        // the resolver falls back on the other one

        resolver
            .remove_certificate(&fingerprint_1y_overriden)
            .expect("should be able to remove the 1-year (3-year-overriden) certificate");

        let localhost_cert = resolver
            .domain_lookup("localhost".as_bytes(), true)
            .expect("there should be a localhost cert");

        assert_eq!(localhost_cert.1, fingerprint_2y);

        Ok(())
    }

    /// Verify that `replace_certificate` adds the new cert before removing
    /// the old one, so lookup always returns a valid certificate.
    #[test]
    fn replace_certificate_add_before_remove() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        // add the initial (1-year) certificate
        let cert_1y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key-1y.pem")),
            ..Default::default()
        };

        let fingerprint_1y = resolver.add_certificate(&AddCertificate {
            address,
            certificate: cert_1y,
            expired_at: None,
        })?;

        // sanity: the 1y cert is resolvable
        assert!(
            resolver
                .domain_lookup("localhost".as_bytes(), true)
                .is_some(),
            "initial certificate should be resolvable"
        );

        // replace with the 2-year certificate
        let cert_2y = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-2y.pem")),
            key: String::from(include_str!("../assets/tests/key-2y.pem")),
            ..Default::default()
        };

        let new_fingerprint = resolver.replace_certificate(&ReplaceCertificate {
            address,
            new_certificate: cert_2y,
            old_fingerprint: fingerprint_1y.to_string(),
            new_expired_at: None,
        })?;

        // the old certificate should be gone
        assert!(
            resolver.get_certificate(&fingerprint_1y).is_none(),
            "old certificate should have been removed"
        );

        // the new certificate should be present and resolvable
        assert!(
            resolver.get_certificate(&new_fingerprint).is_some(),
            "new certificate should be present"
        );
        let resolved = resolver
            .domain_lookup("localhost".as_bytes(), true)
            .expect("a certificate should resolve for localhost");
        assert_eq!(
            resolved.1, new_fingerprint,
            "resolved certificate should be the replacement"
        );

        Ok(())
    }

    /// When `ReplaceCertificate` carries the same certificate / fingerprint
    /// as the existing entry, the previous implementation called
    /// `add_certificate` (which early-returns on duplicate fingerprint
    /// without inserting) and then unconditionally removed the old
    /// fingerprint — i.e. the *current* entry — leaving the resolver
    /// without a certificate for that name. The fix short-circuits the
    /// idempotent case and keeps the existing entry in place. An
    /// idempotent ACME / operator retry must therefore still resolve.
    #[test]
    fn replace_certificate_with_same_fingerprint_is_noop()
    -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        let cert = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key-1y.pem")),
            ..Default::default()
        };

        let initial_fingerprint = resolver.add_certificate(&AddCertificate {
            address,
            certificate: cert.clone(),
            expired_at: None,
        })?;

        // Replace with the SAME PEM/key — identical fingerprint expected.
        let returned_fingerprint = resolver.replace_certificate(&ReplaceCertificate {
            address,
            new_certificate: cert,
            old_fingerprint: initial_fingerprint.to_string(),
            new_expired_at: None,
        })?;

        assert_eq!(
            returned_fingerprint, initial_fingerprint,
            "idempotent replace should return the existing fingerprint"
        );

        assert!(
            resolver.get_certificate(&initial_fingerprint).is_some(),
            "idempotent replace must NOT delete the existing certificate"
        );

        let resolved = resolver
            .domain_lookup("localhost".as_bytes(), true)
            .expect("certificate should still resolve after idempotent replace");
        assert_eq!(
            resolved.1, initial_fingerprint,
            "resolver should still hand back the original fingerprint"
        );

        Ok(())
    }

    /// Verify that removing the last certificate for a domain cleans up
    /// the empty entry in `name_fingerprint_idx` (no memory leak).
    #[test]
    fn removal_cleans_up_empty_index_entries() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        let cert = CertificateAndKey {
            certificate: String::from(include_str!("../assets/tests/certificate-1y.pem")),
            key: String::from(include_str!("../assets/tests/key-1y.pem")),
            ..Default::default()
        };

        let fingerprint = resolver.add_certificate(&AddCertificate {
            address,
            certificate: cert,
            expired_at: None,
        })?;

        // record the names associated with this cert
        let names = resolver.certificate_names(&fingerprint)?;
        assert!(
            !names.is_empty(),
            "certificate should have at least one name"
        );

        // verify index is populated
        for name in &names {
            assert!(
                resolver.name_fingerprint_idx.contains_key(name),
                "name_fingerprint_idx should contain '{name}' before removal"
            );
        }

        resolver.remove_certificate(&fingerprint)?;

        // after removal, all index entries for these names should be gone
        for name in &names {
            assert!(
                !resolver.name_fingerprint_idx.contains_key(name),
                "name_fingerprint_idx should not contain empty entry for '{name}' after removal"
            );
        }

        Ok(())
    }

    /// Many ACME clients (Certbot's `fullchain.pem`, lego, acme.sh) emit
    /// the leaf certificate at the START of the chain file. Without
    /// dedup, the resolver previously stored `[leaf, leaf, ...]` and
    /// the on-wire TLS handshake replayed the leaf twice — accepted by
    /// browsers but rejected by stricter validators (Node.js,
    /// `UNABLE_TO_VERIFY_LEAF_SIGNATURE`). The fix in
    /// `TryFrom<&AddCertificate> for CertifiedKeyWrapper` drops any
    /// chain entry whose DER bytes match the leaf. Closes #1135 / #1148.
    ///
    /// This test passes the SAME leaf PEM as both `certificate` and the
    /// sole `certificate_chain` entry (the `fullchain.pem` shape). The
    /// stored chain length must be `1` (leaf only), not `2`
    /// (`[leaf, leaf]`).
    #[test]
    fn certificate_chain_dedup_drops_duplicate_leaf() -> Result<(), Box<dyn Error + Send + Sync>> {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        let leaf_pem = String::from(include_str!("../assets/certificate.pem"));

        let cert_with_duplicated_leaf = CertificateAndKey {
            certificate: leaf_pem.clone(),
            certificate_chain: vec![leaf_pem],
            key: String::from(include_str!("../assets/key.pem")),
            ..Default::default()
        };

        let fingerprint = resolver.add_certificate(&AddCertificate {
            address,
            certificate: cert_with_duplicated_leaf,
            expired_at: None,
        })?;

        let stored = resolver
            .get_certificate(&fingerprint)
            .ok_or("resolver lost the certificate after add")?;

        assert_eq!(
            stored.inner.cert.len(),
            1,
            "expected dedup to drop the duplicate leaf, got chain of {} cert(s)",
            stored.inner.cert.len()
        );

        Ok(())
    }

    /// When an operator passes the entire `fullchain.pem` content as a
    /// SINGLE chain entry (one string containing multiple
    /// `-----BEGIN CERTIFICATE-----` blocks back-to-back), the previous
    /// code called `parse_pem` once on the multi-PEM string, which only
    /// consumes the first PEM block and silently drops the rest. The
    /// fix splits each chain entry through
    /// `split_certificate_chain` so multi-PEM strings fan out into one
    /// entry per CA before parsing.
    ///
    /// This test concatenates two PEM blocks into a single chain entry
    /// (the leaf duplicated, so dedup also kicks in). The stored chain
    /// must end up with length 1 — the leaf — proving (a) the multi-PEM
    /// entry was split correctly, (b) the duplicate leaf was dropped.
    #[test]
    fn certificate_chain_handles_multi_pem_single_entry() -> Result<(), Box<dyn Error + Send + Sync>>
    {
        let address = SocketAddress::new_v4(127, 0, 0, 1, 8080);
        let mut resolver = CertificateResolver::default();

        let leaf_pem = String::from(include_str!("../assets/certificate.pem"));
        // Two PEM blocks concatenated into one string; both are the
        // leaf so dedup brings the result back to length 1.
        let multi_pem_chain_entry = format!("{leaf_pem}\n{leaf_pem}");

        let cert = CertificateAndKey {
            certificate: leaf_pem,
            certificate_chain: vec![multi_pem_chain_entry],
            key: String::from(include_str!("../assets/key.pem")),
            ..Default::default()
        };

        let fingerprint = resolver.add_certificate(&AddCertificate {
            address,
            certificate: cert,
            expired_at: None,
        })?;

        let stored = resolver
            .get_certificate(&fingerprint)
            .ok_or("resolver lost the certificate after add")?;

        // Without the split, `parse_pem` would only consume the first
        // PEM block and drop the second; with the split + dedup, both
        // get fanned out, both get recognised as the leaf, both get
        // dropped — leaving the original leaf at index 0 only.
        assert_eq!(
            stored.inner.cert.len(),
            1,
            "expected split + dedup to leave only the leaf, got chain of {} cert(s)",
            stored.inner.cert.len()
        );

        Ok(())
    }
}
