use std::{collections::BTreeMap, net::SocketAddr};

use clap::{Parser, Subcommand};
use sozu_command_lib::proxy::{LoadBalancingAlgorithms, TlsVersion};

#[derive(Parser, PartialEq, Clone, Debug)]
#[clap(author, version, about)]
pub struct Args {
    #[clap(
        short = 'c',
        long = "config",
        global = true,
        help = "Sets a custom config file"
    )]
    pub config: Option<String>,
    #[clap(
        short = 't',
        long = "timeout",
        global = true,
        help = "Sets a custom timeout for commands (in milliseconds). 0 disables the timeout"
    )]
    pub timeout: Option<u64>,
    #[clap(subcommand)]
    pub cmd: SubCmd,
}

impl paw::ParseArgs for Args {
    type Error = std::io::Error;

    fn parse_args() -> Result<Self, Self::Error> {
        Ok(Self::parse())
    }
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum SubCmd {
    #[clap(name = "start", about = "launch the main process")]
    Start,
    #[clap(
        name = "worker",
        about = "start a worker (internal command, should not be used directly)"
    )]
    Worker {
        #[clap(long = "id", help = "worker identifier")]
        id: i32,
        #[clap(long = "fd", help = "IPC file descriptor")]
        fd: i32,
        #[clap(long = "scm", help = "IPC SCM_RIGHTS file descriptor")]
        scm: i32,
        #[clap(
            long = "configuration-state-fd",
            help = "configuration data file descriptor"
        )]
        configuration_state_fd: i32,
        #[clap(
            long = "command-buffer-size",
            help = "Worker's channel buffer size",
            default_value = "1000000"
        )]
        command_buffer_size: usize,
        #[clap(
            long = "max-command-buffer-size",
            help = "Worker's channel max buffer size"
        )]
        max_command_buffer_size: Option<usize>,
    },
    #[clap(
        name = "main",
        about = "start a new main process (internal command, should not be used directly)"
    )]
    Main {
        #[clap(long = "fd", help = "IPC file descriptor")]
        fd: i32,
        #[clap(long = "upgrade-fd", help = "upgrade data file descriptor")]
        upgrade_fd: i32,
        #[clap(
            long = "command-buffer-size",
            help = "Main process channel buffer size",
            default_value = "1000000"
        )]
        command_buffer_size: usize,
        #[clap(
            long = "max-command-buffer-size",
            help = "Main process channel max buffer size"
        )]
        max_command_buffer_size: Option<usize>,
    },

    // sozuctl commands
    #[clap(name = "shutdown", about = "shuts down the proxy")]
    Shutdown {
        #[clap(long = "hard", help = "do not wait for connections to finish")]
        hard: bool,
        #[clap(
            short = 'w',
            long = "worker",
            help = "shuts down the worker with this id"
        )]
        worker: Option<u32>,
    },
    #[clap(name = "upgrade", about = "upgrade the proxy")]
    Upgrade {
        #[clap(short = 'w', long = "worker", help = "Upgrade the worker with this id")]
        worker: Option<u32>,
    },
    #[clap(name = "status", about = "gets information on the running workers")]
    Status {
        #[clap(
            short = 'j',
            long = "json",
            help = "Print the command result in JSON format"
        )]
        json: bool,
    },
    #[clap(
        name = "metrics",
        about = "gets statistics on the main process and its workers"
    )]
    Metrics {
        #[clap(subcommand)]
        cmd: MetricsCmd,
    },
    #[clap(name = "logging", about = "change logging level")]
    Logging {
        #[clap(subcommand)]
        level: LoggingLevel,
    },
    #[clap(name = "state", about = "state management")]
    State {
        #[clap(subcommand)]
        cmd: StateCmd,
    },
    #[clap(
        name = "reload",
        about = "Reloads routing configuration (applications, frontends and backends)"
    )]
    Reload {
        #[clap(
            short = 'f',
            long = "file",
            help = "use a different configuration file from the current one"
        )]
        file: Option<String>,
        #[clap(
            short = 'j',
            long = "json",
            help = "Print the command result in JSON format"
        )]
        json: bool,
    },
    #[clap(name = "application", about = "application management")]
    Application {
        #[clap(subcommand)]
        cmd: ApplicationCmd,
    },
    #[clap(name = "backend", about = "backend management")]
    Backend {
        #[clap(subcommand)]
        cmd: BackendCmd,
    },
    #[clap(name = "frontend", about = "frontend management")]
    Frontend {
        #[clap(subcommand)]
        cmd: FrontendCmd,
    },
    #[clap(name = "listener", about = "listener management")]
    Listener {
        #[clap(subcommand)]
        cmd: ListenerCmd,
    },
    #[clap(name = "certificate", about = "certificate management")]
    Certificate {
        #[clap(subcommand)]
        cmd: CertificateCmd,
    },
    #[clap(name = "query", about = "configuration state verification")]
    Query {
        #[clap(
            short = 'j',
            long = "json",
            help = "Print the command result in JSON format"
        )]
        json: bool,
        #[clap(subcommand)]
        cmd: QueryCmd,
    },
    #[clap(name = "config", about = "configuration file management")]
    Config {
        #[clap(subcommand)]
        cmd: ConfigCmd,
    },
    #[clap(name = "events", about = "receive sozu events")]
    Events,
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum MetricsCmd {
    #[clap(name = "enable", about = "Enables local metrics collection")]
    Enable {
        #[clap(short, long, help = "Enables time metrics collection")]
        time: bool,
    },
    #[clap(name = "disable", about = "Disables local metrics collection")]
    Disable {
        #[clap(short, long, help = "Disables time metrics collection")]
        time: bool,
    },
    #[clap(name = "clear", about = "Deletes local metrics data")]
    Clear,
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum LoggingLevel {
    #[clap(name = "trace", about = "Displays a LOT of logs")]
    Trace,
    #[clap(
        name = "debug",
        about = "Displays more logs about the inner workings of Sōzu"
    )]
    Debug,
    #[clap(name = "error", about = "Displays occurring errors")]
    Error,
    #[clap(name = "warn", about = "Displays warnings about non-critical errors")]
    Warn,
    #[clap(name = "info", about = "Displays logs about normal behaviour of Sōzu")]
    Info,
}
impl std::fmt::Display for LoggingLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum StateCmd {
    #[clap(name = "save", about = "Save state to that file")]
    Save {
        #[clap(short = 'f', long = "file")]
        file: String,
    },
    #[clap(name = "load", about = "Load state from that file")]
    Load {
        #[clap(short = 'f', long = "file")]
        file: String,
    },
    #[clap(name = "dump", about = "Dump current state to STDOUT")]
    Dump {
        #[clap(
            short = 'j',
            long = "json",
            help = "Print the command result in JSON format"
        )]
        json: bool,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum ApplicationCmd {
    #[clap(name = "remove", about = "Remove an application")]
    Remove {
        #[clap(short = 'i', long = "id")]
        id: String,
    },
    #[clap(name = "add", about = "Add an application")]
    Add {
        #[clap(short = 'i', long = "id")]
        id: String,
        #[clap(short = 's', long = "sticky-session")]
        sticky_session: bool,
        #[clap(short = 'r', long = "https-redirect")]
        https_redirect: bool,
        #[clap(
            long = "send-proxy",
            help = "Enforces use of the PROXY protocol version 2 over any connection established to this server."
        )]
        send_proxy: bool,
        #[clap(
            long = "expect-proxy",
            help = "Configures the client-facing connection to receive a PROXY protocol header version 2"
        )]
        expect_proxy: bool,
        #[clap(
            long = "load-balancing-policy",
            help = "Configures the load balancing policy. Possible values are 'roundrobin', 'random' or 'leastconnections'"
        )]
        load_balancing_policy: LoadBalancingAlgorithms,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum BackendCmd {
    #[clap(name = "remove", about = "Remove a backend")]
    Remove {
        #[clap(short = 'i', long = "id")]
        id: String,
        #[clap(long = "backend-id")]
        backend_id: String,
        #[clap(
            short = 'a',
            long = "address",
            help = "server address, format: IP:port"
        )]
        address: SocketAddr,
    },
    #[clap(name = "add", about = "Add a backend")]
    Add {
        #[clap(short = 'i', long = "id")]
        id: String,
        #[clap(long = "backend-id")]
        backend_id: String,
        #[clap(
            short = 'a',
            long = "address",
            help = "server address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(
            short = 's',
            long = "sticky-id",
            help = "value for the sticky session cookie"
        )]
        sticky_id: Option<String>,
        #[clap(short = 'b', long = "backup", help = "set backend as a backup backend")]
        backup: Option<bool>,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum FrontendCmd {
    #[clap(name = "http", about = "HTTP frontend management")]
    Http {
        #[clap(subcommand)]
        cmd: HttpFrontendCmd,
    },
    #[clap(name = "https", about = "HTTPS frontend management")]
    Https {
        #[clap(subcommand)]
        cmd: HttpFrontendCmd,
    },
    #[clap(name = "tcp", about = "TCP frontend management")]
    Tcp {
        #[clap(subcommand)]
        cmd: TcpFrontendCmd,
    },
    #[clap(name = "list", about = "List frontends using filters")]
    List {
        #[clap(long = "http", help = "filter for http frontends")]
        http: bool,
        #[clap(long = "https", help = "filter for https frontends")]
        https: bool,
        #[clap(long = "tcp", help = "filter for tcp frontends")]
        tcp: bool,
        #[clap(
            short = 'd',
            long = "domain",
            help = "filter by domain name (for http & https frontends)"
        )]
        domain: Option<String>,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum Route {
    /// traffic will go to the backend servers with this application id
    Id {
        /// traffic will go to the backend servers with this application id
        id: String,
    },
    /// traffic to this frontend will be rejected with HTTP 401
    Deny,
}

impl std::convert::Into<sozu_command_lib::proxy::Route> for Route {
    fn into(self) -> sozu_command_lib::proxy::Route {
        match self {
            Route::Deny => sozu_command_lib::proxy::Route::Deny,
            Route::Id { id } => sozu_command_lib::proxy::Route::ClusterId(id),
        }
    }
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum HttpFrontendCmd {
    #[clap(name = "add")]
    Add {
        #[clap(
            short = 'a',
            long = "address",
            help = "frontend address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(subcommand, name = "route")]
        route: Route,
        #[clap(long = "hostname", aliases = &["host"])]
        hostname: String,
        #[clap(short = 'p', long = "path", help = "URL prefix of the frontend")]
        path_begin: Option<String>,
        #[clap(short = 'm', long = "method", help = "HTTP method")]
        method: Option<String>,
        #[clap(long = "tag", help = "custom tags", parse(try_from_str = parse_tags))]
        tags: Option<BTreeMap<String, String>>,
    },
    #[clap(name = "remove")]
    Remove {
        #[clap(
            short = 'a',
            long = "address",
            help = "frontend address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(subcommand, name = "route")]
        route: Route,
        #[clap(long = "hostname", aliases = &["host"])]
        hostname: String,
        #[clap(short = 'p', long = "path", help = "URL prefix of the frontend")]
        path_begin: Option<String>,
        #[clap(short = 'm', long = "method", help = "HTTP method")]
        method: Option<String>,
        #[clap(long = "tag", help = "custom tags", parse(try_from_str = parse_tags))]
        tags: Option<BTreeMap<String, String>>,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum TcpFrontendCmd {
    #[clap(name = "add")]
    Add {
        #[clap(short = 'i', long = "id", help = "app id of the frontend")]
        id: String,
        #[clap(
            short = 'a',
            long = "address",
            help = "frontend address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(long = "tag", help = "custom tags",  parse(try_from_str = parse_tags))]
        tags: Option<BTreeMap<String, String>>,
    },
    #[clap(name = "remove")]
    Remove {
        #[clap(short = 'i', long = "id", help = "app id of the frontend")]
        id: String,
        #[clap(
            short = 'a',
            long = "address",
            help = "frontend address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(long = "tag", help = "custom tags", parse(try_from_str = parse_tags))]
        tags: Option<BTreeMap<String, String>>,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum ListenerCmd {
    #[clap(name = "http", about = "HTTP listener management")]
    Http {
        #[clap(subcommand)]
        cmd: HttpListenerCmd,
    },
    #[clap(name = "https", about = "HTTPS listener management")]
    Https {
        #[clap(subcommand)]
        cmd: HttpsListenerCmd,
    },
    #[clap(name = "tcp", about = "TCP listener management")]
    Tcp {
        #[clap(subcommand)]
        cmd: TcpListenerCmd,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum HttpListenerCmd {
    #[clap(name = "add")]
    Add {
        #[clap(short = 'a')]
        address: SocketAddr,
        #[clap(
            long = "public-address",
            help = "a different IP than the one the socket sees, for logs and forwarded headers"
        )]
        public_address: Option<SocketAddr>,
        #[clap(
            long = "answer-404",
            help = "path to file of the 404 answer sent to the client when a frontend is not found"
        )]
        answer_404: Option<String>,
        #[clap(
            long = "answer-503",
            help = "path to file of the 503 answer sent to the client when an application has no backends available"
        )]
        answer_503: Option<String>,
        #[clap(
            long = "expect-proxy",
            help = "Configures the client socket to receive a PROXY protocol header"
        )]
        expect_proxy: bool,
        #[clap(long = "sticky-name", help = "sticky session cookie name")]
        sticky_name: Option<String>,
    },
    #[clap(name = "remove")]
    Remove {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
    },
    #[clap(name = "activate")]
    Activate {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
    },
    #[clap(name = "deactivate")]
    Deactivate {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum HttpsListenerCmd {
    #[clap(name = "add")]
    Add {
        #[clap(short = 'a')]
        address: SocketAddr,
        #[clap(
            long = "public-address",
            help = "a different IP than the one the socket sees, for logs and forwarded headers"
        )]
        public_address: Option<SocketAddr>,
        #[clap(
            long = "answer-404",
            help = "path to file of the 404 answer sent to the client when a frontend is not found"
        )]
        answer_404: Option<String>,
        #[clap(
            long = "answer-503",
            help = "path to file of the 503 answer sent to the client when an application has no backends available"
        )]
        answer_503: Option<String>,
        #[clap(long = "tls-versions", help = "list of TLS versions to use")]
        tls_versions: Vec<TlsVersion>,
        #[clap(long = "tls-ciphers-list", help = "list of OpenSSL TLS ciphers to use")]
        cipher_list: Option<String>,
        #[clap(long = "rustls-cipher-list", help = "list of RustTLS ciphers to use")]
        rustls_cipher_list: Vec<String>,
        #[clap(
            long = "expect-proxy",
            help = "Configures the client socket to receive a PROXY protocol header"
        )]
        expect_proxy: bool,
        #[clap(long = "sticky-name", help = "sticky session cookie name")]
        sticky_name: Option<String>,
    },
    #[clap(name = "remove")]
    Remove {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
    },
    #[clap(name = "activate")]
    Activate {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
    },
    #[clap(name = "deactivate")]
    Deactivate {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum TcpListenerCmd {
    #[clap(name = "add")]
    Add {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(
            long = "public-address",
            help = "a different IP than the one the socket sees, for logs and forwarded headers"
        )]
        public_address: Option<SocketAddr>,
        #[clap(
            long = "expect-proxy",
            help = "Configures the client socket to receive a PROXY protocol header"
        )]
        expect_proxy: bool,
    },
    #[clap(name = "remove")]
    Remove {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
    },
    #[clap(name = "activate")]
    Activate {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
    },
    #[clap(name = "deactivate")]
    Deactivate {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum CertificateCmd {
    #[clap(name = "add", about = "Add a certificate")]
    Add {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(long = "certificate", help = "path to the certificate")]
        certificate: String,
        #[clap(long = "certificate-chain", help = "path to the certificate chain")]
        chain: String,
        #[clap(long = "key", help = "path to the key")]
        key: String,
        #[clap(long = "tls-versions", help = "accepted TLS versions for this certificate",
                parse(try_from_str = parse_tls_versions))]
        tls_versions: Vec<TlsVersion>,
    },
    #[clap(name = "remove", about = "Remove a certificate")]
    Remove {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(aliases = &["cert"], long = "certificate", help = "path to the certificate")]
        certificate: Option<String>,
        #[clap(short = 'f', long = "fingerprint", help = "certificate fingerprint")]
        fingerprint: Option<String>,
    },
    #[clap(name = "replace", about = "Replace an existing certificate")]
    Replace {
        #[clap(
            short = 'a',
            long = "address",
            help = "listener address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(long = "new-certificate", help = "path to the new certificate")]
        certificate: String,
        #[clap(
            long = "new-certificate-chain",
            help = "path to the new certificate chain"
        )]
        chain: String,
        #[clap(long = "new-key", help = "path to the new key")]
        key: String,
        #[clap(
            aliases = &["old-cert"],
            long = "old-certificate",
            help = "path to the old certificate"
        )]
        old_certificate: Option<String>,
        #[clap(
            short = 'f',
            long = "fingerprint",
            help = "old certificate fingerprint"
        )]
        old_fingerprint: Option<String>,
        #[clap(long = "tls-versions", help = "accepted TLS versions for this certificate",
                parse(try_from_str = parse_tls_versions))]
        tls_versions: Vec<TlsVersion>,
    },
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum QueryCmd {
    #[clap(
        name = "applications",
        about = "Query applications matching a specific filter"
    )]
    Applications {
        #[clap(short = 'i', long = "id", help = "application identifier")]
        id: Option<String>,
        #[clap(short = 'd', long = "domain", help = "application domain name")]
        domain: Option<String>,
    },

    #[clap(
        name = "certificates",
        about = "Query certificates matching a specific filter"
    )]
    Certificates {
        #[clap(short = 'f', long = "fingerprint", help = "certificate fingerprint")]
        fingerprint: Option<String>,
        #[clap(short = 'd', long = "domain", help = "domain name")]
        domain: Option<String>,
    },

    #[clap(name = "metrics", about = "Query metrics matching a specific filter")]
    Metrics {
        #[clap(short, long, help = "list available metrics")]
        list: bool,
        #[clap(short, long, help = "refresh metrics results (in seconds)")]
        refresh: Option<u32>,
        #[clap(
            short = 'n',
            long = "names",
            help = "metric names",
            use_delimiter = true
        )]
        names: Vec<String>,
        #[clap(
            short = 'c',
            long = "clusters",
            help = "list of cluster ids",
            use_delimiter = true
        )]
        clusters: Vec<String>,
        #[clap(short = 'b', long="backends", help="list of backend ids", use_delimiter = true, parse(try_from_str = split_slash))]
        backends: Vec<(String, String)>,
    },
}

fn split_slash(input: &str) -> Result<(String, String), String> {
    let mut it = input.split('/').map(|s| s.trim().to_string());

    if let (Some(cluster), Some(backend)) = (it.next(), it.next()) {
        Ok((cluster, backend))
    } else {
        Err(format!(
            "could not split cluster id and backend id in {}",
            input
        ))
    }
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum ConfigCmd {
    #[clap(name = "check", about = "check configuration file syntax and exit")]
    Check {},
}

fn parse_tls_versions(i: &str) -> Result<TlsVersion, String> {
    match i {
        "TLSv1" => Ok(TlsVersion::TLSv1_0),
        "TLSv1.1" => Ok(TlsVersion::TLSv1_1),
        "TLSv1.2" => Ok(TlsVersion::TLSv1_2),
        "TLSv1.3" => Ok(TlsVersion::TLSv1_2),
        s => return Err(format!("unrecognized TLS version: {}", s)),
    }
}

fn parse_tags(string_to_parse: &str) -> Result<BTreeMap<String, String>, String> {
    let mut tags: BTreeMap<String, String> = BTreeMap::new();

    for s in string_to_parse.split(",") {
        if let Some((key, value)) = s.trim().split_once("=") {
            tags.insert(key.to_owned(), value.to_owned());
        } else {
            return Err(format!(
                "something went wrong while parsing the tags '{}'",
                string_to_parse
            ));
        }
    }

    Ok(tags)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_tags_from_string() {
        use super::*;

        let tags_to_parse =
            "owner=John ,uuid=0dd8d7b1-a50a-461a-b1f9-5211a5f45a83=, hexkey=#846e84";

        assert_eq!(
            Ok(BTreeMap::from([
                ("owner".to_owned(), "John".to_owned()),
                (
                    "uuid".to_owned(),
                    "0dd8d7b1-a50a-461a-b1f9-5211a5f45a83=".to_owned(),
                ),
                ("hexkey".to_owned(), "#846e84".to_owned())
            ])),
            parse_tags(tags_to_parse)
        );
    }
}
