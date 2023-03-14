use std::{fs::File, io::Write, iter, net::SocketAddr, thread, time};

use acme_lib::{create_p384_key, persist::FilePersist, Directory, DirectoryUrl};
use anyhow::{bail, Context};
use mio::net::UnixStream;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use tiny_http::{Response as HttpResponse, Server};

use sozu_command_lib::{
    certificate::{
        calculate_fingerprint, split_certificate_chain, CertificateAndKey, CertificateFingerprint,
        TlsVersion,
    },
    channel::Channel,
    config::Config,
    request::{AddCertificate, RemoveBackend, ReplaceCertificate, Request},
    response::{Backend, HttpFrontend, PathRule, Response, ResponseStatus, Route, RulePosition},
};

use crate::util;

/// sozu-acme is a configuration tool for the
/// [sōzu HTTP reverse proxy](https://github.com/sozu-proxy/sozu)
/// that automates certificate requests from
/// [Let's Encrypt](https://letsencrypt.org/) or other
/// [ACME](https://tools.ietf.org/html/draft-ietf-acme-acme-07) enabled
/// certificate authorities.
///
/// This tool will perform the following actions:
///
/// - contact Let's Encrypt
/// - retrieve the challenge data
/// - launch a web server for the HTTP challenge
/// - configure sōzu to redirect the challenge request to that web server
/// - start the HTTP challenge validation
/// - if the challenge was successful, write the certificate, chain and key to the specified paths
/// - remove the challenge web server from sōzu's configuration
///
/// This tool is in beta right now, don't hesitate to test it and report issues.
pub fn main(
    config_file: String,
    domain: String,
    email: String,
    cluster_id: String,
    old_certificate_path: Option<String>,
    new_certificate_path: String,
    certificate_chain_path: String,
    key_path: String,
    http_frontend_address: String,
    https_frontend_address: String,
) -> anyhow::Result<()> {
    let config = Config::load_from_path(&config_file)
        .with_context(|| "could not parse configuration file")?;

    util::setup_logging(&config, "ACME");
    info!("starting up");

    let http = http_frontend_address
        .parse::<SocketAddr>()
        .with_context(|| "invalid HTTP frontend address format")?;
    let https = https_frontend_address;

    let old_certificate_file = match old_certificate_path {
        Some(path) => {
            let bytes = Config::load_file_bytes(&path)
                .with_context(|| "Could not load old certificate file")?;
            Some(bytes)
        }
        None => None,
    };

    let old_fingerprint = match old_certificate_file {
        Some(bytes) => {
            let f = calculate_fingerprint(&bytes)
                .with_context(|| "Could not calculate old fingerprint")?;
            Some(f)
        }
        None => None,
    };

    let stream = UnixStream::connect(&config.command_socket).with_context(|| {
        format!(
            "could not connect to the command unix socket: {}. Are you sure the proxy is up?",
            config.command_socket
        )
    })?;

    let tls_versions = vec![TlsVersion::TLSv1_2, TlsVersion::TLSv1_3];

    let mut channel: Channel<Request, Response> = Channel::new(stream, 10000, 20000);
    channel
        .blocking()
        .with_context(|| "Could not block channel")?;

    info!("got channel, connecting to Let's Encrypt");

    // Use DirectoryUrl::LetsEncrypStaging for dev/testing
    //let url = DirectoryUrl::LetsEncryptStaging;
    let url = DirectoryUrl::LetsEncrypt;

    let persist = FilePersist::new(".");
    // Create a directory entrypoint.
    let dir = Directory::from_url(persist, url)
        .with_context(|| "Could not create an entrypoint directory")?;

    // Reads the private account key from persistence,
    // or creates a new one before accessing the API to establish that it's there.
    let account = dir
        .account(&email)
        .with_context(|| "Could not create account")?;

    // Order a new TLS certificate for a domain.
    let mut new_cert_order = account
        .new_order(&domain, &[])
        .with_context(|| "Could not create the order for a new certificate")?;

    // If the ownership of the domain(s) has already been
    // authorized in a previous order, you might be able to
    // skip validation. The ACME API provider decides.
    let csr_order = loop {
        // are we done?
        if let Some(csr_order) = new_cert_order.confirm_validations() {
            break csr_order;
        }

        // Get the possible authorizations (for a single domain
        // this will only be one element).
        let auths = new_cert_order.authorizations().unwrap();
        let auth = &auths[0];
        let challenge = auth.http_challenge();
        let challenge_token = challenge.http_token();

        let path = format!("/.well-known/acme-challenge/{challenge_token}");
        let key_authorization = challenge.http_proof();
        debug!(
            "HTTP challenge token: {} key: {}",
            challenge_token, key_authorization
        );

        let server = match Server::http("127.0.0.1:0") {
            Ok(srv) => srv,
            Err(e) => bail!("could not create HTTP server: {}", e.to_string()),
        };

        let address = server
            .server_addr()
            .to_ip()
            .with_context(|| "Could not convert server address to IP")?;

        let acme_app_id = generate_app_id(&cluster_id);

        debug!("setting up proxying");
        set_up_proxying(&mut channel, &http, &acme_app_id, &domain, &path, address)
            .with_context(|| "could not set up proxying to HTTP challenge server")?;

        let path2 = path.clone();
        let _server_thread = thread::spawn(move || {
            info!("HTTP server started");
            loop {
                let request = match server.recv() {
                    Ok(rq) => rq,
                    Err(e) => {
                        error!("server error while receiving request: {}", e);
                        break;
                    }
                };

                info!("got request to URL: {}", request.url());
                if request.url() == path {
                    if let Err(e) = request.respond(
                        HttpResponse::from_data(key_authorization.as_bytes()).with_status_code(200),
                    ) {
                        error!("Error responding with 200 to request: {}", e);
                    }
                    info!("challenge request answered");
                    // the challenge can be called multiple times
                    //return true;
                } else if let Err(e) = request
                    .respond(HttpResponse::from_data(&b"not found"[..]).with_status_code(404))
                {
                    error!("Error responding with 404 to request: {}", e);
                }
            }

            false
        });

        thread::sleep(time::Duration::from_millis(100));

        challenge
            .validate(2000)
            .with_context(|| "The ACME API did not validate the proof of the challenge")?;
        info!("challenge validated");

        new_cert_order
            .refresh()
            .with_context(|| "Could not refresh the order for the new certificate")?;

        //let res = server_thread.join().expect("HTTP server thread failed");
        //if res {
        remove_proxying(&mut channel, &http, &acme_app_id, &domain, &path2, address)
            .with_context(|| "could not deactivate proxying")?;
        //}
    };

    // Ownership is proven. Create a private key for
    // the certificate. These are provided for convenience, you
    // can provide your own keypair instead if you want.
    let pkey_pri = create_p384_key();

    // Submit the CSR. This causes the ACME provider to enter a
    // state of "processing" that must be polled until the
    // certificate is either issued or rejected. Again we poll
    // for the status change.
    let ord_cert = csr_order.finalize_pkey(pkey_pri, 5000).unwrap();

    // Now download the certificate. Also stores the cert in
    // the persistence.
    let cert_chain = ord_cert.download_and_save_cert().unwrap();

    info!("got cert: \n{}", cert_chain.certificate());
    let certificates = split_certificate_chain(cert_chain.certificate().to_string());

    let mut new_cert_file = File::create(&new_certificate_path)
        .with_context(|| "Could not create new certificate file")?;
    new_cert_file
        .write_all(certificates[0].as_bytes())
        .with_context(|| "Could not write certificate chain in the new certificate file")?;

    //FIXME: there may be more than 1 cert in the chain
    let mut certificate_chain_file = File::create(&certificate_chain_path)
        .with_context(|| "Could not create certificate chain file")?;
    certificate_chain_file
        .write_all(certificates[1].as_bytes())
        .with_context(|| "Could not write certificate chain in the certificate chain file")?;

    let mut key_file = File::create(&key_path).with_context(|| "Could not create key file")?;
    key_file
        .write_all(cert_chain.private_key().as_bytes())
        .with_context(|| "Could not write certificate chain in the key file")?;

    info!("saved cert and key");
    add_certificate(
        &mut channel,
        &https,
        &domain,
        &new_certificate_path,
        &certificate_chain_path,
        &key_path,
        old_fingerprint,
        &tls_versions,
    )
    .with_context(|| "could not add new certificate")?;

    info!("added new certificate");

    info!("DONE");
    Ok(())
}

/*
fn generate_id() -> String {
    let s: String = iter::repeat(())
        .map(|()| thread_rng().sample(Alphanumeric))
        .take(6)
        .map(|c| c as char)
        .collect();
    format!("ID-{s}")
}
*/

fn generate_app_id(app_id: &str) -> String {
    let s: String = iter::repeat(())
        .map(|()| thread_rng().sample(Alphanumeric))
        .take(6)
        .map(|c| c as char)
        .collect();
    format!("{app_id}-ACME-{s}")
}

fn set_up_proxying(
    channel: &mut Channel<Request, Response>,
    frontend: &SocketAddr,
    cluster_id: &str,
    hostname: &str,
    path_begin: &str,
    server_address: SocketAddr,
) -> anyhow::Result<()> {
    let add_http_front = Request::AddHttpFrontend(HttpFrontend {
        route: Route::ClusterId(cluster_id.to_owned()),
        hostname: String::from(hostname),
        address: *frontend,
        path: PathRule::Prefix(path_begin.to_owned()),
        method: None,
        position: RulePosition::Tree,
        tags: None,
    });

    order_request(channel, add_http_front).with_context(|| "Request AddHttpFront failed")?;

    let add_backend = Request::AddBackend(Backend {
        cluster_id: String::from(cluster_id),
        backend_id: format!("{cluster_id}-0"),
        address: server_address,
        load_balancing_parameters: None,
        sticky_id: None,
        backup: None,
    });

    order_request(channel, add_backend).with_context(|| "AddBackend request failed")?;
    Ok(())
}

fn remove_proxying(
    channel: &mut Channel<Request, Response>,
    frontend: &SocketAddr,
    cluster_id: &str,
    hostname: &str,
    path_begin: &str,
    server_address: SocketAddr,
) -> anyhow::Result<()> {
    let remove_http_front = Request::RemoveHttpFrontend(HttpFrontend {
        route: Route::ClusterId(cluster_id.to_owned()),
        address: *frontend,
        hostname: String::from(hostname),
        path: PathRule::Prefix(path_begin.to_owned()),
        method: None,
        position: RulePosition::Tree,
        tags: None,
    });
    order_request(channel, remove_http_front).with_context(|| "RemoveHttpFront request failed")?;

    let remove_backend = Request::RemoveBackend(RemoveBackend {
        cluster_id: String::from(cluster_id),
        backend_id: format!("{cluster_id}-0"),
        address: server_address,
    });

    order_request(channel, remove_backend).with_context(|| "RemoveBackend request failed")?;
    Ok(())
}

fn add_certificate(
    channel: &mut Channel<Request, Response>,
    frontend: &str,
    hostname: &str,
    certificate_path: &str,
    chain_path: &str,
    key_path: &str,
    old_fingerprint: Option<Vec<u8>>,
    tls_versions: &Vec<TlsVersion>,
) -> anyhow::Result<()> {
    let certificate = Config::load_file(certificate_path)
        .with_context(|| "could not load certificate".to_string())?;

    let key = Config::load_file(key_path).with_context(|| "could not load key".to_string())?;

    let certificate_chain = Config::load_file(chain_path)
        .map(split_certificate_chain)
        .with_context(|| "could not load certificate chain".to_string())?;

    let request = match old_fingerprint {
        None => Request::AddCertificate(AddCertificate {
            address: frontend.to_owned(),
            certificate: CertificateAndKey {
                certificate,
                certificate_chain,
                key,
                versions: tls_versions.clone(),
            },
            names: vec![hostname.to_string()],
            expired_at: None,
        }),

        Some(f) => Request::ReplaceCertificate(ReplaceCertificate {
            address: frontend.to_owned(),
            new_certificate: CertificateAndKey {
                certificate,
                certificate_chain,
                key,
                versions: tls_versions.clone(),
            },
            old_fingerprint: CertificateFingerprint(f),
            new_names: vec![hostname.to_string()],
            new_expired_at: None,
        }),
    };

    info!("Sending the certificate request {:?}", request);
    order_request(channel, request).with_context(|| "Could not send the certificate request")
}

fn order_request(channel: &mut Channel<Request, Response>, request: Request) -> anyhow::Result<()> {
    channel
        .write_message(&request.clone())
        .with_context(|| "Could not write message on the channel")?;

    loop {
        let response = channel
            .read_message()
            .with_context(|| "Could not read response on channel")?;

        match response.status {
            ResponseStatus::Processing => {
                // do nothing here
                // for other messages, we would loop over read_message
                // until an error or ok message was sent
            }
            ResponseStatus::Failure => {
                bail!("could not execute request: {}", response.message);
            }
            ResponseStatus::Ok => {
                // TODO: remove the pattern matching and only display the response message
                match request {
                    Request::AddBackend(_) => {
                        info!("backend added : {}", response.message)
                    }
                    Request::RemoveBackend(_) => {
                        info!("backend removed : {} ", response.message)
                    }
                    Request::AddCertificate(_) => {
                        info!("certificate added: {}", response.message)
                    }
                    Request::RemoveCertificate(_) => {
                        info!("certificate removed: {}", response.message)
                    }
                    Request::AddHttpFrontend(_) => {
                        info!("front added: {}", response.message)
                    }
                    Request::RemoveHttpFrontend(_) => {
                        info!("front removed: {}", response.message)
                    }
                    _ => {
                        // do nothing for now
                    }
                }
                return Ok(());
            }
        }
    }
}
