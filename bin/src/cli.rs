use std::{collections::BTreeMap, net::SocketAddr};

use clap::{Parser, Subcommand};

use sozu_command_lib::{
    proto::command::{LoadBalancingAlgorithms, TlsVersion},
    state::ClusterId as StateClusterId,
};

#[derive(Parser, PartialEq, Eq, Clone, Debug)]
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
    #[clap(
        short = 'j',
        long = "json",
        global = true,
        help = "display responses to queries in a JSON format"
    )]
    pub json: bool,
    #[clap(subcommand)]
    pub cmd: SubCmd,
}

impl paw::ParseArgs for Args {
    type Error = std::io::Error;

    fn parse_args() -> Result<Self, Self::Error> {
        Ok(Self::parse())
    }
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
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
        #[clap(
            long = "fd",
            help = "IPC file descriptor of the worker to main channel"
        )]
        fd: i32,
        #[clap(
            long = "scm",
            help = "IPC SCM_RIGHTS file descriptor of the worker to main scm socket"
        )]
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
        command_buffer_size: u64,
        #[clap(
            long = "max-command-buffer-size",
            help = "Worker's channel max buffer size"
        )]
        max_command_buffer_size: Option<u64>,
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
        command_buffer_size: u64,
        #[clap(
            long = "max-command-buffer-size",
            help = "Main process channel max buffer size"
        )]
        max_command_buffer_size: Option<u64>,
    },

    // sozu command line
    #[clap(name = "shutdown", about = "shuts down the proxy")]
    Shutdown {
        #[clap(long = "hard", help = "do not wait for connections to finish")]
        hard: bool,
    },
    #[clap(
        name = "upgrade",
        about = "upgrade the main process OR a specific worker. Specify a longer timeout."
    )]
    Upgrade {
        #[clap(long = "worker", help = "upgrade a specific worker")]
        worker: Option<u32>,
    },

    #[clap(name = "status", about = "gets information on the running workers")]
    Status,
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
        #[clap(name = "filter")]
        filter: String,
    },
    #[clap(name = "state", about = "state management")]
    State {
        #[clap(subcommand)]
        cmd: StateCmd,
    },
    #[clap(
        name = "reload",
        about = "Reloads routing configuration (clusters, frontends and backends)"
    )]
    Reload {
        #[clap(
            short = 'f',
            long = "file",
            help = "use a different configuration file from the current one"
        )]
        file: Option<String>,
    },
    #[clap(name = "cluster", about = "cluster management")]
    Cluster {
        #[clap(subcommand)]
        cmd: ClusterCmd,
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
    #[clap(name = "certificate", about = "list, add and remove certificates")]
    Certificate {
        #[clap(subcommand)]
        cmd: CertificateCmd,
    },
    #[clap(name = "config", about = "configuration file management")]
    Config {
        #[clap(subcommand)]
        cmd: ConfigCmd,
    },
    #[clap(name = "events", about = "receive sozu events")]
    Events,
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
pub enum MetricsCmd {
    #[clap(name = "enable", about = "Enables local metrics collection")]
    Enable,
    #[clap(name = "disable", about = "Disables local metrics collection")]
    Disable,
    #[clap(name = "clear", about = "Deletes local metrics data")]
    Clear,
    #[clap(
        name = "get",
        about = "get all metrics, filtered, or a list of available metrics"
    )]
    Get {
        #[clap(short, long, help = "list the available metrics on the proxy level")]
        list: bool,
        #[clap(short, long, help = "refresh metrics results (in seconds)")]
        refresh: Option<u32>,
        #[clap(
            short = 'n',
            long = "names",
            help = "Filter by metric names. Coma-separated list.",
            use_value_delimiter = true
        )]
        names: Vec<String>,
        #[clap(
            short = 'k',
            long = "clusters",
            help = "list of cluster ids (= application id)",
            use_value_delimiter = true
        )]
        clusters: Vec<String>,
        #[clap(
            short = 'b',
            long="backends",
            help="coma-separated list of backends, 'one_backend_id,other_backend_id'",
            use_value_delimiter = true
            // parse(try_from_str = split_slash)
        )]
        backends: Vec<String>,
        #[clap(
            long = "no-clusters",
            help = "get only the metrics of main process and workers (no cluster metrics)"
        )]
        no_clusters: bool,
    },
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
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
    #[clap(
        name = "stats",
        about = "show the counts of requests that were received since startup"
    )]
    Stats,
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
pub enum ClusterCmd {
    #[clap(
        name = "list",
        about = "Query clusters, all of them, or filtered by id or domain"
    )]
    List {
        #[clap(short = 'i', long = "id", help = "cluster identifier")]
        id: Option<String>,
        #[clap(short = 'd', long = "domain", help = "cluster domain name")]
        domain: Option<String>,
    },
    #[clap(name = "remove", about = "Remove a cluster")]
    Remove {
        #[clap(short = 'i', long = "id", help = "cluster id")]
        id: String,
    },
    #[clap(name = "add", about = "Add a cluster")]
    Add {
        #[clap(short = 'i', long = "id", help = "cluster id")]
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

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
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

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
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

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
pub enum ClusterId {
    /// traffic will go to the backend servers with this cluster id
    Id {
        /// traffic will go to the backend servers with this cluster id
        id: String,
    },
    /// traffic to this frontend will be rejected with HTTP 401
    Deny,
}

#[allow(clippy::from_over_into)]
impl std::convert::Into<Option<StateClusterId>> for ClusterId {
    fn into(self) -> Option<StateClusterId> {
        match self {
            ClusterId::Deny => None,
            ClusterId::Id { id } => Some(id),
        }
    }
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
pub enum HttpFrontendCmd {
    #[clap(name = "add")]
    Add {
        #[clap(
            short = 'a',
            long = "address",
            help = "frontend address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(subcommand, name = "cluster_id")]
        cluster_id: ClusterId,
        #[clap(long = "hostname", aliases = &["host"])]
        hostname: String,
        #[clap(short = 'p', long = "path-prefix", help = "URL prefix of the frontend")]
        path_prefix: Option<String>,
        #[clap(
            long = "path-regex",
            help = "the frontend URL path should match this regex"
        )]
        path_regex: Option<String>,
        #[clap(
            long = "path-equals",
            help = "the frontend URL path should equal this regex"
        )]
        path_equals: Option<String>,
        #[clap(short = 'm', long = "method", help = "HTTP method")]
        method: Option<String>,
        #[clap(long = "tags", help = "Specify tag (key-value pair) to apply on front-end (example: 'key=value, other-key=other-value')", value_parser = parse_tags)]
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
        #[clap(subcommand, name = "cluster_id")]
        cluster_id: ClusterId,
        #[clap(long = "hostname", aliases = &["host"])]
        hostname: String,
        #[clap(short = 'p', long = "path-prefix", help = "URL prefix of the frontend")]
        path_prefix: Option<String>,
        #[clap(
            long = "path-regex",
            help = "the frontend URL path should match this regex"
        )]
        path_regex: Option<String>,
        #[clap(
            long = "path-equals",
            help = "the frontend URL path should equal this regex"
        )]
        path_equals: Option<String>,
        #[clap(short = 'm', long = "method", help = "HTTP method")]
        method: Option<String>,
    },
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
pub enum TcpFrontendCmd {
    #[clap(name = "add")]
    Add {
        #[clap(
            short = 'i',
            long = "id",
            help = "the id of the cluster to which the frontend belongs"
        )]
        id: String,
        #[clap(
            short = 'a',
            long = "address",
            help = "frontend address, format: IP:port"
        )]
        address: SocketAddr,
        #[clap(
            long = "tags",
            help = "Specify tag (key-value pair) to apply on front-end (example: 'key=value, other-key=other-value')",
            value_parser = parse_tags
        )]
        tags: Option<BTreeMap<String, String>>,
    },
    #[clap(name = "remove")]
    Remove {
        #[clap(
            short = 'i',
            long = "id",
            help = "the id of the cluster to which the frontend belongs"
        )]
        id: String,
        #[clap(
            short = 'a',
            long = "address",
            help = "frontend address, format: IP:port"
        )]
        address: SocketAddr,
    },
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
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
    #[clap(name = "list", about = "List all listeners")]
    List,
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
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
            help = "path to file of the 503 answer sent to the client when a cluster has no backends available"
        )]
        answer_503: Option<String>,
        #[clap(
            long = "expect-proxy",
            help = "Configures the client socket to receive a PROXY protocol header"
        )]
        expect_proxy: bool,
        #[clap(long = "sticky-name", help = "sticky session cookie name")]
        sticky_name: Option<String>,
        #[clap(
            long = "front-timeout",
            help = "maximum time of inactivity for a frontend socket"
        )]
        front_timeout: Option<u32>,
        #[clap(
            long = "back-timeout",
            help = "maximum time of inactivity for a backend socket"
        )]
        back_timeout: Option<u32>,
        #[clap(
            long = "request-timeout",
            help = "maximum time to receive a request since the connection started"
        )]
        request_timeout: Option<u32>,
        #[clap(
            long = "connect-timeout",
            help = "maximum time to connect to a backend server"
        )]
        connect_timeout: Option<u32>,
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

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
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
            help = "path to file of the 503 answer sent to the client when a cluster has no backends available"
        )]
        answer_503: Option<String>,
        #[clap(long = "tls-versions", help = "list of TLS versions to use")]
        tls_versions: Vec<TlsVersion>,
        #[clap(
            long = "tls-cipher-list",
            help = "List of TLS cipher list to use (TLSv1.2 and TLSv1.3)"
        )]
        cipher_list: Option<Vec<String>>,
        #[clap(
            long = "expect-proxy",
            help = "Configures the client socket to receive a PROXY protocol header"
        )]
        expect_proxy: bool,
        #[clap(long = "sticky-name", help = "sticky session cookie name")]
        sticky_name: Option<String>,
        #[clap(
            long = "front-timeout",
            help = "maximum time of inactivity for a frontend socket"
        )]
        front_timeout: Option<u32>,
        #[clap(
            long = "back-timeout",
            help = "maximum time of inactivity for a frontend socket"
        )]
        back_timeout: Option<u32>,
        #[clap(
            long = "request-timeout",
            help = "maximum time to receive a request since the connection started"
        )]
        request_timeout: Option<u32>,
        #[clap(
            long = "connect-timeout",
            help = "maximum time to connect to a backend server"
        )]
        connect_timeout: Option<u32>,
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

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
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

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
pub enum CertificateCmd {
    #[clap(
        name = "list",
        about = "Query all certificates, or filtered by fingerprint or domain name.
This command queries the state of Sōzu by default, but can show results for all workers.
Use the --json option to get a much more verbose result, with certificate contents."
    )]
    List {
        #[clap(
            short = 'f',
            long = "fingerprint",
            help = "get the certificate for a given fingerprint"
        )]
        fingerprint: Option<String>,
        #[clap(
            short = 'd',
            long = "domain",
            help = "list certificates for a domain name"
        )]
        domain: Option<String>,
        #[clap(
            short = 'w',
            long = "workers",
            help = "Show results for each worker (slower)"
        )]
        query_workers: bool,
    },
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
                value_parser = parse_tls_versions)]
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
                value_parser = parse_tls_versions)]
        tls_versions: Vec<TlsVersion>,
    },
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
pub enum ConfigCmd {
    #[clap(name = "check", about = "check configuration file syntax and exit")]
    Check,
}

fn parse_tls_versions(i: &str) -> Result<TlsVersion, String> {
    match i {
        "TLSv1" => Ok(TlsVersion::TlsV10),
        "TLS_V11" => Ok(TlsVersion::TlsV11),
        "TLS_V12" => Ok(TlsVersion::TlsV12),
        "TLS_V13" => Ok(TlsVersion::TlsV12),
        s => Err(format!("unrecognized TLS version: {s}")),
    }
}

fn parse_tags(string_to_parse: &str) -> Result<BTreeMap<String, String>, String> {
    let mut tags: BTreeMap<String, String> = BTreeMap::new();

    for s in string_to_parse.split(',') {
        if let Some((key, value)) = s.trim().split_once('=') {
            tags.insert(key.to_owned(), value.to_owned());
        } else {
            return Err(format!(
                "something went wrong while parsing the tags '{string_to_parse}'"
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
