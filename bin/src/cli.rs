use std::net::SocketAddr;
use sozu_command::config::LoadBalancingAlgorithms;
use sozu_command::proxy::TlsVersion;

#[derive(StructOpt, PartialEq, Debug)]
#[structopt(name = "example", about = "An example of StructOpt usage.")]
pub struct Sozu {
  #[structopt(short="c", long = "config", help = "Sets a custom config file")]
  pub config: Option<String>,
  #[structopt(short="t", long = "timeout", help = "Sets a custom timeout for commands (in milliseconds). 0 disables the timeout")]
  pub timeout: Option<u64>,
  #[structopt(subcommand)]
  pub cmd: SubCmd,
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum SubCmd {
  #[structopt(name = "start", about = "launch the master process")]
  Start,
  #[structopt(name = "worker", about = "start a worker (internal command, should not be used directly)")]
  Worker {
    #[structopt(long = "id", help = "worker identifier")]
    id: i32,
    #[structopt(long = "fd", help = "IPC file descriptor")]
    fd: i32,
    #[structopt(long = "scm", help = "IPC SCM_RIGHTS file descriptor")]
    scm: i32,
    #[structopt(long = "configuration-state-fd", help = "configuration data file descriptor")]
    configuration_state_fd: i32,
    #[structopt(long = "command-buffer-size", help = "Worker's channel buffer size", default_value = "1_000_000")]
    command_buffer_size: usize,
    #[structopt(long = "max-command-buffer-size", help = "Worker's channel max buffer size")]
    max_command_buffer_size: Option<usize>,
  },
  #[structopt(name = "master", about = "start a new master process (internal command, should not be used directly)")]
  Master {
    #[structopt(long = "fd", help = "IPC file descriptor")]
    fd: i32,
    #[structopt(long = "upgrade-fd", help = "upgrade data file descriptor")]
    upgrade_fd: i32,
    #[structopt(long = "command-buffer-size", help = "Master's channel buffer size",  default_value = "1_000_000")]
    command_buffer_size: usize,
    #[structopt(long = "max-command-buffer-size", help = "Master's channel max buffer size")]
    max_command_buffer_size: Option<usize>,
  },

  // sozuctl commands
  #[structopt(name = "shutdown", about = "shuts down the proxy")]
  Shutdown {
    #[structopt(long = "hard", help = "do not wait for connections to finish")]
    hard: bool,
    #[structopt(short = "w", long = "worker", help = "shuts down the worker with this id")]
    worker: Option<u32>,
  },
  #[structopt(name = "upgrade", about = "upgrade the proxy")]
  Upgrade {
    #[structopt(short = "w", long = "worker", help = "Upgrade the worker with this id")]
    worker: Option<u32>,
  },
  #[structopt(name = "status", about = "gets information on the running workers")]
  Status {
    #[structopt(short = "j", long = "json", help = "Print the command result in JSON format")]
    json: bool
  },
  #[structopt(name = "metrics", about = "gets statistics on the master and its workers")]
  Metrics {
    #[structopt(short = "j", long = "json", help = "Print the command result in JSON format")]
    json: bool
  },
  #[structopt(name = "logging", about = "change logging level")]
  Logging {
    #[structopt(short = "l", long = "level", help = "change logging level")]
    level: String
  },
  #[structopt(name = "state", about = "state management")]
  State {
    #[structopt(subcommand)]
    cmd: StateCmd,
  },
  #[structopt(name = "application", about = "application management")]
  Application {
    #[structopt(subcommand)]
    cmd: ApplicationCmd,
  },
  #[structopt(name = "backend", about = "backend management")]
  Backend {
    #[structopt(subcommand)]
    cmd: BackendCmd,
  },
  #[structopt(name = "frontend", about = "frontend management")]
  Frontend {
    #[structopt(subcommand)]
    cmd: FrontendCmd,
  },
  #[structopt(name  = "listener", about = "listener management")]
  Listener {
    #[structopt(subcommand)]
    cmd: ListenerCmd
  },
  #[structopt(name = "certificate", about = "certificate management")]
  Certificate {
    #[structopt(subcommand)]
    cmd: CertificateCmd,
  },
  #[structopt(name = "query", about = "configuration state verification")]
  Query {
    #[structopt(short = "j", long = "json", help = "Print the command result in JSON format")]
    json: bool,
    #[structopt(subcommand)]
    cmd: QueryCmd,
  },
  #[structopt(name = "config", about = "configuration file management")]
  Config {
    #[structopt(subcommand)]
    cmd: ConfigCmd
  },
  #[structopt(name = "events", about = "receive sozu events")]
  Events
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum StateCmd {
  #[structopt(name = "save", about = "Save state to that file")]
  Save {
    #[structopt(short = "f", long = "file")]
    file: String,
  },
  #[structopt(name = "load", about = "Load state from that file")]
  Load {
    #[structopt(short = "f", long = "file")]
    file: String,
  },
  #[structopt(name = "dump", about = "Dump current state to STDOUT")]
  Dump {
    #[structopt(short = "j", long = "json", help = "Print the command result in JSON format")]
    json: bool
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum ApplicationCmd {
  #[structopt(name = "remove", about = "Remove an application")]
  Remove {
    #[structopt(short = "i", long = "id")]
    id: String,
  },
  #[structopt(name = "add", about = "Add an application")]
  Add{
    #[structopt(short = "i", long = "id")]
    id: String,
    #[structopt(short = "s", long = "sticky-session")]
    sticky_session: bool,
    #[structopt(short = "h", long = "https-redirect")]
    https_redirect: bool,
    #[structopt(long = "send-proxy", help = "Enforces use of the PROXY protocol version 2 over any connection established to this server.")]
    send_proxy: bool,
    #[structopt(long = "expect-proxy", help = "Configures the client-facing connection to receive a PROXY protocol header version 2")]
    expect_proxy: bool,
    #[structopt(long = "load-balancing-policy", help = "Configures the load balancing policy. Possible values are 'roundrobin', 'random' or 'leastconnections'")]
    load_balancing_policy: LoadBalancingAlgorithms,
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum BackendCmd {
  #[structopt(name = "remove", about = "Remove a backend")]
  Remove {
    #[structopt(short = "i", long = "id")]
    id: String,
    #[structopt(long = "backend-id")]
    backend_id: String,
    #[structopt(short = "a", long = "address", help = "server address, format: IP:port")]
    address: SocketAddr,
  },
  #[structopt(name = "add", about = "Add a backend")]
  Add {
    #[structopt(short = "i", long = "id")]
    id: String,
    #[structopt(long = "backend-id")]
    backend_id: String,
    #[structopt(short = "a", long = "address", help = "server address, format: IP:port")]
    address: SocketAddr,
    #[structopt(short = "s", long = "sticky-id", help = "value for the sticky session cookie")]
    sticky_id: Option<String>,
    #[structopt(short = "b", long = "backup", help = "set backend as a backup backend")]
    backup: Option<bool>,
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum FrontendCmd {
  #[structopt(name = "http", about = "HTTP frontend management")]
  Http {
    #[structopt(subcommand)]
    cmd: HttpFrontendCmd,
  },
  #[structopt(name = "https", about = "HTTPS frontend management")]
  Https {
    #[structopt(subcommand)]
    cmd: HttpFrontendCmd,
  },
  #[structopt(name = "tcp", about = "TCP frontend management")]
  Tcp {
    #[structopt(subcommand)]
    cmd: TcpFrontendCmd,
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum Route {
    /// traffic will go to the backend servers with this application id
    Id {
        /// traffic will go to the backend servers with this application id
        id: String
    },
    /// traffic to this frontend will be rejected with HTTP 401
    Deny,
}

impl std::convert::Into<sozu_command::proxy::Route> for Route {
    fn into(self) -> sozu_command::proxy::Route {
        match self {
          Route::Deny => sozu_command::proxy::Route::Deny,
          Route::Id { id } => sozu_command::proxy::Route::AppId(id),
        }
    }
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum HttpFrontendCmd {
  #[structopt(name = "add")]
  Add {
    #[structopt(short = "a", long = "address", help = "frontend address, format: IP:port")]
    address: SocketAddr,
    #[structopt(flatten, name = "route")]
    route: Route,
    #[structopt(short = "host", long = "hostname")]
    hostname: String,
    #[structopt(short = "p", long = "path", help="URL prefix of the frontend")]
    path_begin: Option<String>,
  },
  #[structopt(name = "remove")]
  Remove {
    #[structopt(short = "a", long = "address", help = "frontend address, format: IP:port")]
    address: SocketAddr,
    #[structopt(flatten, name = "route")]
    route: Route,
    #[structopt(short = "host", long = "hostname")]
    hostname: String,
    #[structopt(short = "p", long = "path", help="URL prefix of the frontend")]
    path_begin: Option<String>,
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum TcpFrontendCmd {
  #[structopt(name = "add")]
  Add {
    #[structopt(short = "i", long = "id", help = "app id of the frontend")]
    id: String,
    #[structopt(short = "a", long = "address", help = "frontend address, format: IP:port")]
    address: SocketAddr,
  },
  #[structopt(name = "remove")]
  Remove {
    #[structopt(short = "i", long = "id", help = "app id of the frontend")]
    id: String,
    #[structopt(short = "a", long = "address", help = "frontend address, format: IP:port")]
    address: SocketAddr,
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum ListenerCmd {
  #[structopt(name = "http", about = "HTTP listener management")]
  Http {
    #[structopt(subcommand)]
    cmd: HttpListenerCmd,
  },
  #[structopt(name = "https", about = "HTTPS listener management")]
  Https {
    #[structopt(subcommand)]
    cmd: HttpsListenerCmd,
  },
  #[structopt(name = "tcp", about = "TCP listener management")]
  Tcp {
    #[structopt(subcommand)]
    cmd: TcpListenerCmd,
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum HttpListenerCmd {
  #[structopt(name = "add")]
  Add {
    #[structopt(short = "a")]
    address: SocketAddr,
    #[structopt(long = "public-address", help = "a different IP than the one the socket sees, for logs and forwarded headers")]
    public_address: Option<SocketAddr>,
    #[structopt(long = "answer-404", help = "path to file of the 404 answer sent to the client when a frontend is not found")]
    answer_404: Option<String>,
    #[structopt(long = "answer-503", help = "path to file of the 503 answer sent to the client when an application has no backends available")]
    answer_503: Option<String>,
    #[structopt(long = "expect-proxy", help = "Configures the client socket to receive a PROXY protocol header")]
    expect_proxy: bool,
    #[structopt(long = "sticky-name", help = "sticky session cookie name")]
    sticky_name: Option<String>
  },
  #[structopt(name = "remove")]
  Remove {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr,
  },
  #[structopt(name = "activate")]
  Activate {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr
  },
  #[structopt(name = "deactivate")]
  Deactivate {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr
  }
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum HttpsListenerCmd {
  #[structopt(name = "add")]
  Add {
    #[structopt(short = "a")]
    address: SocketAddr,
    #[structopt(long = "public-address", help = "a different IP than the one the socket sees, for logs and forwarded headers")]
    public_address: Option<SocketAddr>,
    #[structopt(long = "answer-404", help = "path to file of the 404 answer sent to the client when a frontend is not found")]
    answer_404: Option<String>,
    #[structopt(long = "answer-503", help = "path to file of the 503 answer sent to the client when an application has no backends available")]
    answer_503: Option<String>,
    #[structopt(long = "tls-versions", help = "list of TLS versions to use")]
    tls_versions: Vec<TlsVersion>,
    #[structopt(long = "tls-ciphers-list", help = "list of OpenSSL TLS ciphers to use")]
    cipher_list: Option<String>,
    #[structopt(long = "rustls-cipher-list", help = "list of RustTLS ciphers to use")]
    rustls_cipher_list: Vec<String>,
    #[structopt(long = "expect-proxy", help = "Configures the client socket to receive a PROXY protocol header")]
    expect_proxy: bool,
    #[structopt(long = "sticky-name", help = "sticky session cookie name")]
    sticky_name: Option<String>
  },
  #[structopt(name = "remove")]
  Remove {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr,
  },
  #[structopt(name = "activate")]
  Activate {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr
  },
  #[structopt(name = "deactivate")]
  Deactivate {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr
  }
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum TcpListenerCmd {
  #[structopt(name = "add")]
  Add {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr,
    #[structopt(long = "public-address", help = "a different IP than the one the socket sees, for logs and forwarded headers")]
    public_address: Option<SocketAddr>,
    #[structopt(long = "expect-proxy", help = "Configures the client socket to receive a PROXY protocol header")]
    expect_proxy: bool
  },
  #[structopt(name = "remove")]
  Remove {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr,
  },
  #[structopt(name = "activate")]
  Activate {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr
  },
  #[structopt(name = "deactivate")]
  Deactivate {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr
  }
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum CertificateCmd {
  #[structopt(name = "add", about = "Add a certificate")]
  Add {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr,
    #[structopt(long = "certificate", help = "path to the certificate")]
    certificate: String,
    #[structopt(long = "certificate-chain", help = "path to the certificate chain")]
    chain: String,
    #[structopt(long = "key", help = "path to the key")]
    key: String,
  },
  #[structopt(name = "remove", about = "Remove a certificate")]
  Remove {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr,
    #[structopt(short = "cert", long = "certificate", help = "path to the certificate")]
    certificate: String,
  },
  #[structopt(name = "replace", about = "Replace an existing certificate")]
  Replace {
    #[structopt(short = "a", long = "address", help = "listener address, format: IP:port")]
    address: SocketAddr,
    #[structopt(long = "new-certificate", help = "path to the new certificate")]
    certificate: String,
    #[structopt(long = "new-certificate-chain", help = "path to the new certificate chain")]
    chain: String,
    #[structopt(long = "new-key", help = "path to the new key")]
    key: String,
    #[structopt(short = "old-cert", long = "old-certificate", help = "path to the old certificate")]
    old_certificate: String,
  }
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum QueryCmd {
  #[structopt(name = "applications", about = "Query applications matching a specific filter")]
  Applications {
    #[structopt(short = "i", long="id", help="application identifier")]
    id: Option<String>,
    #[structopt(short = "d", long="domain", help="application domain name")]
    domain: Option<String>
  },

  #[structopt(name = "certificates", about = "Query certificates matching a specific filter")]
  Certificates {
    #[structopt(short = "f", long="fingerprint", help="certificate fingerprint")]
    fingerprint: Option<String>,
    #[structopt(short = "d", long="domain", help="domain name")]
    domain: Option<String>
  }
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum ConfigCmd {
  #[structopt(name = "check", about = "check configuration file syntax and exit")]
  Check {}
}
