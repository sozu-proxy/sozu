use env;
use std::io::{BufWriter,stdout};
use std::path::Path;
use std::fs::OpenOptions;
use rand::{Rng,thread_rng};
use mio_uds::UnixDatagram;
use std::net::{TcpStream,UdpSocket,ToSocketAddrs};
use sozu::logging::{Logger,LoggerBackend};

pub fn setup(tag: String, level: &str, target: &str, access_target: Option<&str>) {
  let backend = target_to_backend(target);
  let access_backend = access_target.map(target_to_backend);

  if let Ok(log_level) = env::var("RUST_LOG") {
    Logger::init(tag, &log_level, backend, access_backend);
  } else {
    // We set the env variable so every worker can access it
    env::set_var("RUST_LOG", level);
    Logger::init(tag, level, backend, access_backend);
  }
}

pub fn target_to_backend(target: &str) -> LoggerBackend {
  if target == "stdout" {
    LoggerBackend::Stdout(stdout())
  } else if target.starts_with("udp://") {
    let addr_res = (&target[6..]).to_socket_addrs();
    match addr_res {
      Err(e) => {
        println!("invalid log target configuration ({:?}): {}", e, target);
        LoggerBackend::Stdout(stdout())
      },
      Ok(mut addrs) => {
        let socket = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        LoggerBackend::Udp(socket, addrs.next().unwrap())
      }
    }
  } else if target.starts_with("tcp://") {
    let addr_res = (&target[6..]).to_socket_addrs();
    match addr_res {
      Err(e) => {
        println!("invalid log target configuration ({:?}): {}", e, target);
        LoggerBackend::Stdout(stdout())
      },
      Ok(mut addrs) => {
        LoggerBackend::Tcp(TcpStream::connect(addrs.next().unwrap()).unwrap())
      }
    }
  } else if target.starts_with("unix://") {
    let path = Path::new(&target[7..]);
    if !path.is_file() {
      println!("invalid log target configuration: {} is not a file", &target[7..]);
      LoggerBackend::Stdout(stdout())
    } else {
      let mut dir = env::temp_dir();
      let s: String = thread_rng().gen_ascii_chars().take(12).collect();
      dir.push(s);
      let socket = UnixDatagram::bind(dir).unwrap();
      socket.connect(path).unwrap();
      LoggerBackend::Unix(socket)
    }
  } else if target.starts_with("file://") {
    let path = Path::new(&target[7..]);
    match OpenOptions::new().create(true).append(true).open(path) {
      Ok(file) => LoggerBackend::File(BufWriter::new(file)),
      Err(e)   => {
        println!("invalid log target configuration: could not open file at {} (error: {:?})", &target[7..], e);
        LoggerBackend::Stdout(stdout())
      }
    }
  } else {
    println!("invalid log target configuration: {}", target);
    LoggerBackend::Stdout(stdout())
  }
}

