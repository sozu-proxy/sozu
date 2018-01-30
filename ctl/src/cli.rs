#[derive(StructOpt, PartialEq, Debug)]
pub struct App {
  #[structopt(short="c", long = "config", help = "Sets a custom config file")]
  pub config: String,
  #[structopt(short="t", long = "timeout", help = "Sets a custom timeout for commands (in milliseconds). 0 disables the timeout")]
  pub timeout: Option<u64>,
  #[structopt(subcommand)]
  pub cmd: SubCmd,
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum SubCmd {
  #[structopt(name = "shutdown", about = "shuts down the proxy without waiting for connections to finish")]
  Shutdown {
    #[structopt(short = "h", long = "hard")]
    hard: bool
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
  }
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
  #[structopt(name = "dump")]
  Dump {
    #[structopt(short = "j", long = "json", help = "Print the command result in JSON format")]
    json: bool
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum ApplicationCmd {
  #[structopt(name = "remove")]
  Remove {
    #[structopt(short = "i", long = "id")]
    id: String,
  },
  #[structopt(name = "add")]
  Add{
    #[structopt(short = "i", long = "id")]
    id: String,
    #[structopt(short = "s", long = "sticky-session")]
    sticky_session: bool,
    #[structopt(short = "h", long = "https-redirect")]
    https_redirect: bool
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum BackendCmd {
  #[structopt(name = "remove")]
  Remove {
    #[structopt(short = "i", long = "id")]
    id: String,
    #[structopt(long = "instance-id")]
    instance_id: String,
    #[structopt(long = "ip")]
    ip: String,
    #[structopt(short = "p", long = "port")]
    port: u16,
  },
  #[structopt(name = "add")]
  Add {
    #[structopt(short = "i", long = "id")]
    id: String,
    #[structopt(long = "instance-id")]
    instance_id: String,
    #[structopt(long = "ip")]
    ip: String,
    #[structopt(short = "p", long = "port")]
    port: u16,
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum FrontendCmd {
  #[structopt(name = "http", about = "HTTP/HTTPS frontend management")]
  Http {
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
pub enum HttpFrontendCmd {
  #[structopt(name = "add")]
  Add {
    #[structopt(short = "i", long = "id", help = "app id of the frontend")]
    id: String,
    #[structopt(short = "host", long = "hostname")]
    hostname: String,
    #[structopt(short = "p", long = "path", help="URL prefix of the frontend")]
    path_begin: Option<String>,
    #[structopt(long = "certificate", help="path to a certificate file")]
    path_to_certificate: Option<String>,
  },
  #[structopt(name = "remove")]
  Remove {
    #[structopt(short = "i", long = "id", help = "app id of the frontend")]
    id: String,
    #[structopt(short = "host", long = "hostname")]
    hostname: String,
    #[structopt(short = "p", long = "path", help="URL prefix of the frontend")]
    path_begin: Option<String>,
    #[structopt(long = "certificate", help="path to a certificate file")]
    path_to_certificate: Option<String>,
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum TcpFrontendCmd {
  #[structopt(name = "add")]
  Add {
    #[structopt(short = "i", long = "id", help = "app id of the frontend")]
    id: String,
    #[structopt(long = "ip", help = "IP address of the frontend")]
    ip_address: String,
    #[structopt(short = "p", long = "port", help="TCP port of the frontend")]
    port: u16,
  },
  #[structopt(name = "remove")]
  Remove {
    #[structopt(short = "i", long = "id", help = "app id of the frontend")]
    id: String,
    #[structopt(long = "ip", help = "IP address of the frontend")]
    ip_address: String,
    #[structopt(short = "p", long = "port", help="TCP port of the frontend")]
    port: u16,
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum CertificateCmd {
  #[structopt(name = "add")]
  Add {
    #[structopt(long = "certificate", help = "path to the certificate")]
    certificate: String,
    #[structopt(long = "certificate-chain", help = "path to the certificate chain")]
    chain: String,
    #[structopt(long = "key", help = "path to the key")]
    key: Option<String>,
  },
  #[structopt(name = "remove")]
  Remove {
    #[structopt(short = "cert", long = "certificate", help = "path to the certificate")]
    certificate: String,
  },
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum QueryCmd {
  #[structopt(name = "applications")]
  Applications {
    #[structopt(short = "i", long="id", help="application identifier")]
    id: Option<String>,
    #[structopt(short = "d", long="domain", help="application domain name")]
    domain: Option<String>
  }
}
