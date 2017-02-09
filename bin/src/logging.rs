use env;
use std::io::stdout;
use std::path::Path;
use rand::{Rng,thread_rng};
use mio_uds::UnixDatagram;
use std::net::{TcpStream,UdpSocket,ToSocketAddrs};
use sozu::logging::{Logger,LoggerBackend};

pub fn setup(level: &Option<String>, target: &Option<String>) {
  let backend: LoggerBackend = target.as_ref().map(|s| {
    if s == "stdout" {
      LoggerBackend::Stdout(stdout())
    } else if s.starts_with("udp://") {
      let mut addr_res = (&s[6..]).to_socket_addrs();
      match addr_res {
        Err(e) => {
          println!("invalid log target configuration ({:?}): {}", e, s);
          LoggerBackend::Stdout(stdout())
        },
        Ok(mut addrs) => {
          let socket = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
          LoggerBackend::Udp(socket, addrs.next().unwrap())
        }
      }
    } else if s.starts_with("tcp://") {
      let mut addr_res = (&s[6..]).to_socket_addrs();
      match addr_res {
        Err(e) => {
          println!("invalid log target configuration ({:?}): {}", e, s);
          LoggerBackend::Stdout(stdout())
        },
        Ok(mut addrs) => {
          LoggerBackend::Tcp(TcpStream::connect(addrs.next().unwrap()).unwrap())
        }
      }
    } else if s.starts_with("unix://") {
      let path = Path::new(&s[7..]);
      if !path.is_file() {
        println!("invalid log target configuration: {} is not a file", &s[7..]);
        LoggerBackend::Stdout(stdout())
      } else {
        let mut dir = env::temp_dir();
        let s: String = thread_rng().gen_ascii_chars().take(12).collect();
        dir.push(s);
        let mut socket = UnixDatagram::bind(dir).unwrap();
        socket.connect(path).unwrap();
        LoggerBackend::Unix(socket)
      }
    } else {
      println!("invalid log target configuration: {}", s);
      LoggerBackend::Stdout(stdout())
    }
  }).unwrap_or(LoggerBackend::Stdout(stdout()));

  if let Ok(log_level) = env::var("RUST_LOG") {
    Logger::init(&log_level, backend);
  } else if let &Some(ref log_level) = level {
    // We set the env variable so every worker can access it
    env::set_var("RUST_LOG", &log_level);
    Logger::init(&log_level, backend);
  }
}

