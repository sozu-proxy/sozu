#[derive(StructOpt, PartialEq, Debug)]
pub struct App {
  #[structopt(short="c", long = "config", help = "Sets a custom config file")]
  pub config: String,
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
  Upgrade,
  #[structopt(name = "status", about = "gets information on the running workers")]
  Status,
  #[structopt(name = "metrics", about = "gets statistics on the master and its workers")]
  Metrics,
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
  Dump,
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
    id: Option<String>
  }
}