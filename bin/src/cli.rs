use std::{collections::BTreeMap, io::IsTerminal, net::SocketAddr, path::PathBuf};

use clap::{ArgAction, CommandFactory, FromArgMatches, Parser, Subcommand};
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
        const GREEN: &str = "\x1b[32m";
        const RED: &str = "\x1b[31m";
        const RESET: &str = "\x1b[0m";

        // ANSI escapes are emitted only when stdout is a real TTY. Redirected
        // output (`sozu --version > file`, package-metadata capture, systemd
        // journal) must stay raw ASCII so scripts and downstream parsers do
        // not ingest escape codes. The logger-colour preference is not
        // consulted because the logger is initialised after argument parsing,
        // so its thread-local state is always `false` at this point.
        let plain_features = env!("SOZU_BUILD_FEATURES");
        let use_color = std::io::stdout().is_terminal();
        let features: String = if use_color {
            plain_features
                .split(' ')
                .map(|flag| {
                    if let Some(name) = flag.strip_prefix('+') {
                        format!("{GREEN}+{name}{RESET}")
                    } else if let Some(name) = flag.strip_prefix('-') {
                        format!("{RED}-{name}{RESET}")
                    } else {
                        flag.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            plain_features.to_owned()
        };

        let long_version = format!(
            "{} ({})\n{}",
            env!("CARGO_PKG_VERSION"),
            env!("SOZU_BUILD_GIT"),
            features,
        );

        // clap requires &'static str for long_version. This intentional leak occurs once
        // during argument parsing (~300 bytes) and lasts for the process lifetime.
        let cmd = Self::command().long_version(long_version.leak() as &'static str);
        let matches = cmd.get_matches();
        Self::from_arg_matches(&matches)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    }
}

// The `Listener` variant balloons through its HttpListenerCmd/HttpsListenerCmd
// `Update` subvariants. Clap-derive's top-level enum can't be Boxed without
// breaking the derive; accept the disparity.
#[allow(clippy::large_enum_variant)]
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
    #[clap(
        name = "events",
        about = "receive sozu events about the status of backends"
    )]
    Events,
    #[clap(
        name = "connection-limit",
        about = "manage the per-(cluster, source-IP) connection limit at runtime"
    )]
    ConnectionLimit {
        #[clap(subcommand)]
        cmd: ConnectionLimitCmd,
    },
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
pub enum ConnectionLimitCmd {
    #[clap(
        name = "set",
        about = "set the global per-(cluster, source-IP) connection limit. `0` disables the feature."
    )]
    Set {
        #[clap(
            help = "maximum simultaneous connections per (cluster, source-IP) pair (0 = unlimited)"
        )]
        limit: u64,
    },
    #[clap(
        name = "remove",
        about = "disable the global per-(cluster, source-IP) limit (equivalent to `set 0`)"
    )]
    Remove,
    #[clap(
        name = "show",
        about = "show the current global per-(cluster, source-IP) connection limit"
    )]
    Show,
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
        #[clap(
            short = 'w',
            long = "workers",
            help = "display metrics of each worker, without merging by metric name or cluster id (takes more space)"
        )]
        workers: bool,
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
        #[clap(
            long = "http2",
            help = "Use HTTP/2 for backend connections to this cluster"
        )]
        http2: bool,
        #[clap(
            long = "https-redirect-port",
            help = "Port to use when building the Location header for an https_redirect (defaults to the listener's effective HTTPS port)"
        )]
        https_redirect_port: Option<u32>,
        #[clap(
            long = "www-authenticate",
            help = "Realm string emitted in the WWW-Authenticate header on a 401 response (e.g. 'Basic realm=\"sozu\"')"
        )]
        www_authenticate: Option<String>,
        #[clap(
            long = "authorized-hash",
            help = "Authorized credential, formatted as 'username:hex(sha256(password))'. Repeatable. Generate with: printf 'user:pass' | sed -n 's/^[^:]*://p' | { read p; printf 'user:%s' \"$(printf %s \"$p\" | sha256sum | cut -d' ' -f1)\"; }"
        )]
        authorized_hash: Vec<String>,
        #[clap(
            long = "answer",
            help = "Per-status HTTP answer template for this cluster. Format: <code>=<body> for an inline literal (the value is taken verbatim, no disk I/O), or <code>=file://<path> to load the body off disk. Repeatable. Examples: --answer 503='HTTP/1.1 503 Service Unavailable\\r\\n\\r\\nbusy' , --answer 503=file:///etc/sozu/503.http ."
        )]
        answer: Vec<String>,
    },
    #[clap(
        name = "h2",
        about = "Enable or disable HTTP/2 for backend connections"
    )]
    H2 {
        #[clap(subcommand)]
        cmd: ClusterH2Cmd,
    },
    #[clap(name = "health-check", about = "Configure backend health checks")]
    HealthCheck {
        #[clap(subcommand)]
        cmd: HealthCheckCmd,
    },
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
pub enum ClusterH2Cmd {
    #[clap(name = "enable", about = "Enable HTTP/2 for backend connections")]
    Enable {
        #[clap(short = 'i', long = "id", help = "cluster id")]
        id: String,
    },
    #[clap(name = "disable", about = "Disable HTTP/2 for backend connections")]
    Disable {
        #[clap(short = 'i', long = "id", help = "cluster id")]
        id: String,
    },
}

#[derive(Subcommand, PartialEq, Eq, Clone, Debug)]
pub enum HealthCheckCmd {
    #[clap(name = "set", about = "Set or update the health check for a cluster")]
    Set {
        #[clap(short = 'i', long = "id", help = "cluster id")]
        id: String,
        #[clap(
            short = 'u',
            long = "uri",
            help = "health check URI path (e.g. /health)"
        )]
        uri: String,
        #[clap(
            long = "interval",
            help = "check interval in seconds",
            default_value = "10"
        )]
        interval: u32,
        #[clap(
            long = "timeout",
            help = "check timeout in seconds",
            default_value = "5"
        )]
        timeout: u32,
        #[clap(
            long = "healthy-threshold",
            help = "consecutive successes to mark healthy",
            default_value = "3"
        )]
        healthy_threshold: u32,
        #[clap(
            long = "unhealthy-threshold",
            help = "consecutive failures to mark unhealthy",
            default_value = "3"
        )]
        unhealthy_threshold: u32,
        #[clap(
            long = "expected-status",
            help = "expected HTTP status code (0 = any 2xx)",
            default_value = "0"
        )]
        expected_status: u32,
    },
    #[clap(name = "remove", about = "Remove the health check from a cluster")]
    Remove {
        #[clap(short = 'i', long = "id", help = "cluster id")]
        id: String,
    },
    #[clap(name = "list", about = "List health check configurations")]
    List {
        #[clap(
            short = 'i',
            long = "id",
            help = "filter by cluster id (lists all if omitted)"
        )]
        id: Option<String>,
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
        #[clap(
            long = "redirect",
            help = "Redirect policy. Possible values: 'forward' (default), 'permanent', 'unauthorized'"
        )]
        redirect: Option<String>,
        #[clap(
            long = "redirect-scheme",
            help = "Scheme for permanent-redirect Location URLs. Possible values: 'use-same' (default), 'use-http', 'use-https'"
        )]
        redirect_scheme: Option<String>,
        #[clap(
            long = "redirect-template",
            help = "Optional template applied when emitting a permanent redirect. Supports %REDIRECT_LOCATION."
        )]
        redirect_template: Option<String>,
        #[clap(
            long = "rewrite-host",
            help = "Rewrite request host with this template. Supports $HOST[n] / $PATH[n] capture placeholders."
        )]
        rewrite_host: Option<String>,
        #[clap(
            long = "rewrite-path",
            help = "Rewrite request path with this template. Same grammar as --rewrite-host."
        )]
        rewrite_path: Option<String>,
        #[clap(
            long = "rewrite-port",
            help = "Override the port in the rewritten URL (1..=65535)."
        )]
        rewrite_port: Option<u32>,
        #[clap(
            long = "required-auth",
            help = "Require a valid Authorization: Basic header on this frontend."
        )]
        required_auth: bool,
        #[clap(
            long = "header",
            help = "Header mutation, format: <position>=<name>=<value>. Position is 'request', 'response', or 'both'. Empty <value> deletes the header (HAProxy del-header parity). Repeatable. To replace a header, pass it twice: first with an empty value (deletes the existing one), then with the new value (sets it). The runtime applies all deletes before any sets."
        )]
        header: Vec<String>,
        #[clap(
            long = "hsts-max-age",
            help = "HSTS (RFC 6797) `max-age` directive in seconds. Setting any of the --hsts-* flags enables HSTS on this frontend. Defaults to 31536000 (1 year, HSTS preload list minimum) when --hsts-max-age is omitted but another --hsts-* flag is set. `0` is the RFC 6797 §11.4 kill switch."
        )]
        hsts_max_age: Option<u32>,
        #[clap(
            long = "hsts-include-subdomains",
            help = "Append `; includeSubDomains` to the rendered HSTS header. Implies HSTS enabled."
        )]
        hsts_include_subdomains: bool,
        #[clap(
            long = "hsts-preload",
            help = "Append `; preload` to the rendered HSTS header (Chrome HSTS preload list — see https://hstspreload.org/). Implies HSTS enabled. Opt-in only; once submitted, removal from the preload list is slow and partial (RFC 6797 §14.2)."
        )]
        hsts_preload: bool,
        #[clap(
            long = "hsts-disabled",
            conflicts_with_all = ["hsts_max_age", "hsts_include_subdomains", "hsts_preload", "hsts_force_replace_backend"],
            help = "Explicitly disable HSTS on this frontend, suppressing any inherited listener-default HSTS. Mutually exclusive with --hsts-max-age / --hsts-include-subdomains / --hsts-preload / --hsts-force-replace-backend."
        )]
        hsts_disabled: bool,
        #[clap(
            long = "hsts-force-replace-backend",
            help = "Override any backend-supplied `Strict-Transport-Security` header with sozu's typed policy instead of preserving it (RFC 6797 §6.1 backend-wins is the default). Use when upstream backends emit a stale or weak HSTS policy that the operator wants to harden centrally. Implies HSTS enabled."
        )]
        hsts_force_replace_backend: bool,
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

// `Update` carries ~20 Option<T> fields and is the dominant variant; accept
// the size disparity rather than Box the variant (clap-derive doesn't help
// when you Box a subcommand struct).
#[allow(clippy::large_enum_variant)]
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
    #[clap(name = "update", about = "Patch a running HTTP listener in place")]
    Update {
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
        #[clap(long = "sticky-name", help = "sticky session cookie name")]
        sticky_name: Option<String>,
        #[clap(
            long = "front-timeout",
            help = "maximum time of inactivity for a frontend socket, in seconds"
        )]
        front_timeout: Option<u32>,
        #[clap(
            long = "back-timeout",
            help = "maximum time of inactivity for a backend socket, in seconds"
        )]
        back_timeout: Option<u32>,
        #[clap(
            long = "connect-timeout",
            help = "maximum time to connect to a backend server, in seconds"
        )]
        connect_timeout: Option<u32>,
        #[clap(
            long = "request-timeout",
            help = "maximum time to receive a complete request, in seconds"
        )]
        request_timeout: Option<u32>,

        // Paired boolean flags — fold to Option<bool> in the request builder
        #[clap(long = "expect-proxy", action = ArgAction::SetTrue, overrides_with = "no_expect_proxy",
               help = "Enable PROXY protocol header on the client socket")]
        expect_proxy: bool,
        #[clap(long = "no-expect-proxy", action = ArgAction::SetTrue, overrides_with = "expect_proxy",
               help = "Disable PROXY protocol header on the client socket")]
        no_expect_proxy: bool,

        // H2 flood knobs
        #[clap(
            long,
            help = "Maximum RST_STREAM frames per second window (CVE-2023-44487, CVE-2019-9514); must be >= 1"
        )]
        h2_max_rst_stream_per_window: Option<u32>,
        #[clap(
            long,
            help = "Maximum PING frames per second window (CVE-2019-9512); must be >= 1"
        )]
        h2_max_ping_per_window: Option<u32>,
        #[clap(
            long,
            help = "Maximum SETTINGS frames per second window (CVE-2019-9515); must be >= 1"
        )]
        h2_max_settings_per_window: Option<u32>,
        #[clap(
            long,
            help = "Maximum empty DATA frames per second window (CVE-2019-9518); must be >= 1"
        )]
        h2_max_empty_data_per_window: Option<u32>,
        #[clap(
            long,
            help = "Maximum CONTINUATION frames per header block (CVE-2024-27316); must be >= 1"
        )]
        h2_max_continuation_frames: Option<u32>,
        #[clap(
            long,
            help = "Maximum accumulated protocol anomalies before ENHANCE_YOUR_CALM; must be >= 1"
        )]
        h2_max_glitch_count: Option<u32>,
        #[clap(
            long,
            help = "Connection-level receive window size in bytes (RFC 9113 §6.9.2)"
        )]
        h2_initial_connection_window: Option<u32>,
        #[clap(
            long,
            help = "Maximum concurrent H2 streams (SETTINGS_MAX_CONCURRENT_STREAMS); must be >= 1"
        )]
        h2_max_concurrent_streams: Option<u32>,
        #[clap(
            long,
            help = "Shrink threshold ratio for recycled stream slots; must be >= 1"
        )]
        h2_stream_shrink_ratio: Option<u32>,
        #[clap(
            long,
            help = "Absolute lifetime cap on RST_STREAM frames received (CVE-2023-44487)"
        )]
        h2_max_rst_stream_lifetime: Option<u64>,
        #[clap(
            long,
            help = "Lifetime cap on abusive RST_STREAM frames — Rapid Reset signature"
        )]
        h2_max_rst_stream_abusive_lifetime: Option<u64>,
        #[clap(
            long,
            help = "Absolute lifetime cap on RST_STREAM frames emitted by the server (CVE-2025-8671)"
        )]
        h2_max_rst_stream_emitted_lifetime: Option<u64>,
        #[clap(
            long,
            help = "Maximum HPACK-decoded header list size per request (RFC 9113 §6.5.2)"
        )]
        h2_max_header_list_size: Option<u32>,
        #[clap(long, help = "Maximum HPACK dynamic table size accepted from the peer")]
        h2_max_header_table_size: Option<u32>,
        #[clap(long, help = "Per-stream idle timeout in seconds")]
        h2_stream_idle_timeout_seconds: Option<u32>,
        #[clap(
            long,
            help = "Seconds to wait after GOAWAY(NO_ERROR) before force-closing; 0 = wait forever"
        )]
        h2_graceful_shutdown_deadline_seconds: Option<u32>,
        #[clap(
            long,
            help = "Maximum connection-level (stream 0) WINDOW_UPDATE frames per window (must be >= 1)"
        )]
        h2_max_window_update_stream0_per_window: Option<u32>,
        #[clap(
            long,
            help = "Name of the correlation header injected per request (e.g. \"Sozu-Id\")"
        )]
        sozu_id_header: Option<String>,

        // Listener-default HTTP answer bodies (file paths)
        #[clap(long, help = "path to file for the 301 answer body")]
        answer_301: Option<PathBuf>,
        #[clap(long, help = "path to file for the 401 answer body")]
        answer_401: Option<PathBuf>,
        #[clap(long, help = "path to file for the 404 answer body")]
        answer_404: Option<PathBuf>,
        #[clap(long, help = "path to file for the 408 answer body")]
        answer_408: Option<PathBuf>,
        #[clap(long, help = "path to file for the 413 answer body")]
        answer_413: Option<PathBuf>,
        #[clap(long, help = "path to file for the 421 answer body")]
        answer_421: Option<PathBuf>,
        #[clap(
            long,
            help = "path to file for the 429 answer body (per-(cluster, source-IP) connection limit)"
        )]
        answer_429: Option<PathBuf>,
        #[clap(long, help = "path to file for the 502 answer body")]
        answer_502: Option<PathBuf>,
        #[clap(long, help = "path to file for the 503 answer body")]
        answer_503: Option<PathBuf>,
        #[clap(long, help = "path to file for the 504 answer body")]
        answer_504: Option<PathBuf>,
        #[clap(long, help = "path to file for the 507 answer body")]
        answer_507: Option<PathBuf>,
    },
}

#[allow(clippy::large_enum_variant)]
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
    #[clap(name = "update", about = "Patch a running HTTPS listener in place")]
    Update {
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
        #[clap(long = "sticky-name", help = "sticky session cookie name")]
        sticky_name: Option<String>,
        #[clap(
            long = "front-timeout",
            help = "maximum time of inactivity for a frontend socket, in seconds"
        )]
        front_timeout: Option<u32>,
        #[clap(
            long = "back-timeout",
            help = "maximum time of inactivity for a backend socket, in seconds"
        )]
        back_timeout: Option<u32>,
        #[clap(
            long = "connect-timeout",
            help = "maximum time to connect to a backend server, in seconds"
        )]
        connect_timeout: Option<u32>,
        #[clap(
            long = "request-timeout",
            help = "maximum time to receive a complete request, in seconds"
        )]
        request_timeout: Option<u32>,

        // Paired boolean flags — fold to Option<bool> in the request builder
        #[clap(long = "expect-proxy", action = ArgAction::SetTrue, overrides_with = "no_expect_proxy",
               help = "Enable PROXY protocol header on the client socket")]
        expect_proxy: bool,
        #[clap(long = "no-expect-proxy", action = ArgAction::SetTrue, overrides_with = "expect_proxy",
               help = "Disable PROXY protocol header on the client socket")]
        no_expect_proxy: bool,
        #[clap(long = "strict-sni-binding", action = ArgAction::SetTrue, overrides_with = "no_strict_sni_binding",
               help = "Require :authority/Host to match the TLS SNI (CWE-346/CWE-444)")]
        strict_sni_binding: bool,
        #[clap(long = "no-strict-sni-binding", action = ArgAction::SetTrue, overrides_with = "strict_sni_binding",
               help = "Allow :authority/Host to differ from the TLS SNI")]
        no_strict_sni_binding: bool,
        #[clap(long = "disable-http11", action = ArgAction::SetTrue, overrides_with = "enable_http11",
               help = "Only accept H2 connections; HTTP/1.1 is dropped at handshake")]
        disable_http11: bool,
        #[clap(long = "enable-http11", action = ArgAction::SetTrue, overrides_with = "disable_http11",
               help = "Re-enable HTTP/1.1 connections alongside H2")]
        enable_http11: bool,

        // ALPN: either --alpn-protocols h2,http/1.1 (set) or --reset-alpn (empty vec = default)
        #[clap(
            long,
            value_delimiter = ',',
            conflicts_with = "reset_alpn",
            help = "Set ALPN protocols to advertise (comma-separated: h2,http/1.1)"
        )]
        alpn_protocols: Option<Vec<String>>,
        #[clap(long = "reset-alpn", action = ArgAction::SetTrue,
               help = "Reset ALPN to the built-in default ([\"h2\", \"http/1.1\"])")]
        reset_alpn: bool,

        // H2 flood knobs
        #[clap(
            long,
            help = "Maximum RST_STREAM frames per second window (CVE-2023-44487, CVE-2019-9514); must be >= 1"
        )]
        h2_max_rst_stream_per_window: Option<u32>,
        #[clap(
            long,
            help = "Maximum PING frames per second window (CVE-2019-9512); must be >= 1"
        )]
        h2_max_ping_per_window: Option<u32>,
        #[clap(
            long,
            help = "Maximum SETTINGS frames per second window (CVE-2019-9515); must be >= 1"
        )]
        h2_max_settings_per_window: Option<u32>,
        #[clap(
            long,
            help = "Maximum empty DATA frames per second window (CVE-2019-9518); must be >= 1"
        )]
        h2_max_empty_data_per_window: Option<u32>,
        #[clap(
            long,
            help = "Maximum CONTINUATION frames per header block (CVE-2024-27316); must be >= 1"
        )]
        h2_max_continuation_frames: Option<u32>,
        #[clap(
            long,
            help = "Maximum accumulated protocol anomalies before ENHANCE_YOUR_CALM; must be >= 1"
        )]
        h2_max_glitch_count: Option<u32>,
        #[clap(
            long,
            help = "Connection-level receive window size in bytes (RFC 9113 §6.9.2)"
        )]
        h2_initial_connection_window: Option<u32>,
        #[clap(
            long,
            help = "Maximum concurrent H2 streams (SETTINGS_MAX_CONCURRENT_STREAMS); must be >= 1"
        )]
        h2_max_concurrent_streams: Option<u32>,
        #[clap(
            long,
            help = "Shrink threshold ratio for recycled stream slots; must be >= 1"
        )]
        h2_stream_shrink_ratio: Option<u32>,
        #[clap(
            long,
            help = "Absolute lifetime cap on RST_STREAM frames received (CVE-2023-44487)"
        )]
        h2_max_rst_stream_lifetime: Option<u64>,
        #[clap(
            long,
            help = "Lifetime cap on abusive RST_STREAM frames — Rapid Reset signature"
        )]
        h2_max_rst_stream_abusive_lifetime: Option<u64>,
        #[clap(
            long,
            help = "Absolute lifetime cap on RST_STREAM frames emitted by the server (CVE-2025-8671)"
        )]
        h2_max_rst_stream_emitted_lifetime: Option<u64>,
        #[clap(
            long,
            help = "Maximum HPACK-decoded header list size per request (RFC 9113 §6.5.2)"
        )]
        h2_max_header_list_size: Option<u32>,
        #[clap(long, help = "Maximum HPACK dynamic table size accepted from the peer")]
        h2_max_header_table_size: Option<u32>,
        #[clap(long, help = "Per-stream idle timeout in seconds")]
        h2_stream_idle_timeout_seconds: Option<u32>,
        #[clap(
            long,
            help = "Seconds to wait after GOAWAY(NO_ERROR) before force-closing; 0 = wait forever"
        )]
        h2_graceful_shutdown_deadline_seconds: Option<u32>,
        #[clap(
            long,
            help = "Maximum connection-level (stream 0) WINDOW_UPDATE frames per window (must be >= 1)"
        )]
        h2_max_window_update_stream0_per_window: Option<u32>,
        #[clap(
            long,
            help = "Name of the correlation header injected per request (e.g. \"Sozu-Id\")"
        )]
        sozu_id_header: Option<String>,

        // Listener-default HTTP answer bodies (file paths)
        #[clap(long, help = "path to file for the 301 answer body")]
        answer_301: Option<PathBuf>,
        #[clap(long, help = "path to file for the 401 answer body")]
        answer_401: Option<PathBuf>,
        #[clap(long, help = "path to file for the 404 answer body")]
        answer_404: Option<PathBuf>,
        #[clap(long, help = "path to file for the 408 answer body")]
        answer_408: Option<PathBuf>,
        #[clap(long, help = "path to file for the 413 answer body")]
        answer_413: Option<PathBuf>,
        #[clap(long, help = "path to file for the 421 answer body")]
        answer_421: Option<PathBuf>,
        #[clap(
            long,
            help = "path to file for the 429 answer body (per-(cluster, source-IP) connection limit)"
        )]
        answer_429: Option<PathBuf>,
        #[clap(long, help = "path to file for the 502 answer body")]
        answer_502: Option<PathBuf>,
        #[clap(long, help = "path to file for the 503 answer body")]
        answer_503: Option<PathBuf>,
        #[clap(long, help = "path to file for the 504 answer body")]
        answer_504: Option<PathBuf>,
        #[clap(long, help = "path to file for the 507 answer body")]
        answer_507: Option<PathBuf>,

        // ── HSTS (RFC 6797) listener-default knobs ──
        // Same surface as `frontend https add`. The full `HstsConfig`
        // patch follows the documented full-object replacement
        // semantics on `UpdateHttpsListenerConfig.hsts`: when any of
        // these flags is supplied the listener's HSTS policy is
        // replaced wholesale, and `Router::refresh_inheriting_hsts`
        // reflows the new policy onto every frontend that inherits
        // from this listener (no per-frontend override).
        #[clap(
            long = "hsts-max-age",
            help = "HSTS (RFC 6797) `max-age` directive in seconds. Setting any of the --hsts-* flags replaces the listener's HSTS policy and refreshes inheriting frontends. Defaults to 31536000 (1 year, HSTS preload list minimum) when --hsts-max-age is omitted but another --hsts-* flag is set. `0` is the RFC 6797 §11.4 kill switch."
        )]
        hsts_max_age: Option<u32>,
        #[clap(
            long = "hsts-include-subdomains",
            help = "Append `; includeSubDomains` to the rendered HSTS header. Implies HSTS enabled."
        )]
        hsts_include_subdomains: bool,
        #[clap(
            long = "hsts-preload",
            help = "Append `; preload` to the rendered HSTS header (Chrome HSTS preload list — see https://hstspreload.org/). Implies HSTS enabled. Opt-in only; once submitted, removal from the preload list is slow and partial (RFC 6797 §14.2)."
        )]
        hsts_preload: bool,
        #[clap(
            long = "hsts-disabled",
            conflicts_with_all = ["hsts_max_age", "hsts_include_subdomains", "hsts_preload", "hsts_force_replace_backend"],
            help = "Explicitly disable the listener-default HSTS, suppressing it for inheriting frontends. Mutually exclusive with --hsts-max-age / --hsts-include-subdomains / --hsts-preload / --hsts-force-replace-backend."
        )]
        hsts_disabled: bool,
        #[clap(
            long = "hsts-force-replace-backend",
            help = "Override any backend-supplied `Strict-Transport-Security` header with sōzu's typed policy instead of preserving it (RFC 6797 §6.1 backend-wins is the default). Implies HSTS enabled."
        )]
        hsts_force_replace_backend: bool,
    },
}

#[allow(clippy::large_enum_variant)]
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
    #[clap(name = "update", about = "Patch a running TCP listener in place")]
    Update {
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
            long = "front-timeout",
            help = "maximum time of inactivity for a frontend socket, in seconds"
        )]
        front_timeout: Option<u32>,
        #[clap(
            long = "back-timeout",
            help = "maximum time of inactivity for a backend socket, in seconds"
        )]
        back_timeout: Option<u32>,
        #[clap(
            long = "connect-timeout",
            help = "maximum time to connect to a backend server, in seconds"
        )]
        connect_timeout: Option<u32>,

        // Paired boolean flags — fold to Option<bool> in the request builder
        #[clap(long = "expect-proxy", action = ArgAction::SetTrue, overrides_with = "no_expect_proxy",
               help = "Enable PROXY protocol header on the client socket")]
        expect_proxy: bool,
        #[clap(long = "no-expect-proxy", action = ArgAction::SetTrue, overrides_with = "expect_proxy",
               help = "Disable PROXY protocol header on the client socket")]
        no_expect_proxy: bool,
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
        "TLSv1" => {
            eprintln!("warning: TLS 1.0 is deprecated and insecure (RFC 8996)");
            Ok(TlsVersion::TlsV10)
        }
        "TLS_V11" => {
            eprintln!("warning: TLS 1.1 is deprecated and insecure (RFC 8996)");
            Ok(TlsVersion::TlsV11)
        }
        "TLS_V12" => Ok(TlsVersion::TlsV12),
        "TLS_V13" => Ok(TlsVersion::TlsV13),
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

    // ── HSTS flags on `sozu listener https update` ──
    // Validates the clap surface exposed for the hot listener-default
    // HSTS patch path (UpdateHttpsListenerConfig.hsts). The destructure
    // has to stay in lock-step with `request_builder.rs::https_listener_command`
    // — these tests catch a missed field rename or a forgotten dispatch
    // arg the next time clap-derive grows another `Update` knob.

    fn extract_https_update(args: super::Args) -> super::HttpsListenerCmd {
        match args.cmd {
            super::SubCmd::Listener {
                cmd: super::ListenerCmd::Https { cmd },
            } => cmd,
            other => panic!("expected listener https subcommand, got {other:?}"),
        }
    }

    #[test]
    fn listener_https_update_parses_hsts_enabling_flags() {
        use super::*;

        let args = Args::try_parse_from([
            "sozu",
            "listener",
            "https",
            "update",
            "-a",
            "127.0.0.1:443",
            "--hsts-max-age",
            "31536000",
            "--hsts-include-subdomains",
            "--hsts-force-replace-backend",
        ])
        .expect("clap should accept --hsts-* on listener https update");

        let HttpsListenerCmd::Update {
            hsts_max_age,
            hsts_include_subdomains,
            hsts_preload,
            hsts_disabled,
            hsts_force_replace_backend,
            ..
        } = extract_https_update(args)
        else {
            panic!("expected HttpsListenerCmd::Update");
        };
        assert_eq!(hsts_max_age, Some(31_536_000));
        assert!(hsts_include_subdomains);
        assert!(!hsts_preload);
        assert!(!hsts_disabled);
        assert!(hsts_force_replace_backend);
    }

    #[test]
    fn listener_https_update_parses_hsts_disabled_alone() {
        use super::*;

        let args = Args::try_parse_from([
            "sozu",
            "listener",
            "https",
            "update",
            "-a",
            "127.0.0.1:443",
            "--hsts-disabled",
        ])
        .expect("clap should accept --hsts-disabled on listener https update");

        let HttpsListenerCmd::Update {
            hsts_disabled,
            hsts_max_age,
            hsts_include_subdomains,
            hsts_preload,
            hsts_force_replace_backend,
            ..
        } = extract_https_update(args)
        else {
            panic!("expected HttpsListenerCmd::Update");
        };
        assert!(hsts_disabled);
        assert_eq!(hsts_max_age, None);
        assert!(!hsts_include_subdomains);
        assert!(!hsts_preload);
        assert!(!hsts_force_replace_backend);
    }

    #[test]
    fn listener_https_update_no_hsts_flags_inherits_listener_default() {
        use super::*;

        // Sanity: the new flags are all optional and the existing
        // surface still parses with no `--hsts-*` argument at all.
        let args = Args::try_parse_from([
            "sozu",
            "listener",
            "https",
            "update",
            "-a",
            "127.0.0.1:443",
            "--front-timeout",
            "120",
        ])
        .expect("clap should accept the existing listener-update surface unchanged");

        let HttpsListenerCmd::Update {
            hsts_max_age,
            hsts_include_subdomains,
            hsts_preload,
            hsts_disabled,
            hsts_force_replace_backend,
            front_timeout,
            ..
        } = extract_https_update(args)
        else {
            panic!("expected HttpsListenerCmd::Update");
        };
        assert_eq!(hsts_max_age, None);
        assert!(!hsts_include_subdomains);
        assert!(!hsts_preload);
        assert!(!hsts_disabled);
        assert!(!hsts_force_replace_backend);
        assert_eq!(front_timeout, Some(120));
    }

    #[test]
    fn listener_https_update_hsts_disabled_conflicts_with_max_age() {
        use super::*;

        let err = Args::try_parse_from([
            "sozu",
            "listener",
            "https",
            "update",
            "-a",
            "127.0.0.1:443",
            "--hsts-disabled",
            "--hsts-max-age",
            "31536000",
        ])
        .expect_err("clap should reject --hsts-disabled with --hsts-max-age");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn listener_https_update_hsts_disabled_conflicts_with_force_replace_backend() {
        use super::*;

        let err = Args::try_parse_from([
            "sozu",
            "listener",
            "https",
            "update",
            "-a",
            "127.0.0.1:443",
            "--hsts-disabled",
            "--hsts-force-replace-backend",
        ])
        .expect_err("clap should reject --hsts-disabled with --hsts-force-replace-backend");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
