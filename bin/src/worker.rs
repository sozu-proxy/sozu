use mio_uds::UnixStream;
use mio::Poll;
use libc::{self,c_char,uint32_t,int32_t,pid_t};
use std::io;
use std::ffi::CString;
use std::iter::repeat;
use std::ptr::null_mut;
use std::process::Command;
use std::net::{ToSocketAddrs,UdpSocket};
use std::os::unix::process::CommandExt;
use std::os::unix::io::{AsRawFd,FromRawFd};
use nix;
use nix::unistd::*;
use nix::fcntl::{fcntl,FcntlArg,FdFlag,FD_CLOEXEC};

use sozu::channel::Channel;
use sozu::network::proxy::Server;
use sozu::network::session::Session;
use sozu::messages::{OrderMessage,OrderMessageAnswer};
use sozu::network::metrics::{METRICS,ProxyMetrics};
use sozu::network::{http,tls};
use sozu_command::config::Config;

use logging;
use command::Worker;

pub fn start_workers(config: &Config) -> nix::Result<Vec<Worker>> {
  let mut workers = Vec::new();
  for index in 0..config.worker_count.unwrap_or(1) {
    match start_worker_process(&index.to_string(), config) {
      Ok((pid, command)) => {
        let w =  Worker::new(index as u32, pid, command, config);
        workers.push(w);
      },
      Err(e) => return Err(e)
    };
  }
  Ok(workers)
}

pub fn start_worker(id: u32, config: &Config) -> nix::Result<Worker> {
  match start_worker_process(&id.to_string(), config) {
    Ok((pid, command)) => {
      let w = Worker::new(id, pid, command, config);
      Ok(w)
    },
    Err(e) => Err(e)
  }
}

fn generate_channels() -> io::Result<(Channel<OrderMessage,OrderMessageAnswer>, Channel<OrderMessageAnswer,OrderMessage>)> {
  let (command,proxy) = try!(UnixStream::pair());
  //FIXME: configurable buffer size
  let proxy_channel   = Channel::new(proxy, 10000, 20000);
  let command_channel = Channel::new(command, 10000, 20000);
  Ok((command_channel, proxy_channel))
}

pub fn begin_worker_process(fd: i32, id: &str, channel_buffer_size: usize) {
  let mut command: Channel<OrderMessageAnswer,Config> = Channel::new(
    unsafe { UnixStream::from_raw_fd(fd) },
    channel_buffer_size,
    channel_buffer_size * 2
  );

  command.set_nonblocking(false);

  let proxy_config = command.read_message().expect("worker could not read configuration from socket");
  //println!("got message: {:?}", proxy_config);

  logging::setup(format!("{}-{}", "TAG", id), &proxy_config.log_level, &proxy_config.log_target);

  command.set_nonblocking(true);
  let command: Channel<OrderMessageAnswer,OrderMessage> = command.into();


  metrics_set_up!(&proxy_config.metrics.address[..], proxy_config.metrics.port);
  gauge!("sozu.TEST", 42);

  let mut event_loop  = Poll::new().expect("could not create event loop");

  let http_session = proxy_config.http.and_then(|conf| conf.to_http()).and_then(|http_conf| {
    let max_connections = http_conf.max_connections;
    let max_listeners = 1;
    http::ServerConfiguration::new(http_conf, &mut event_loop, 1 + max_listeners).map(|configuration| {
      Session::new(1, max_connections, 0, configuration, &mut event_loop)
    }).ok()
  });

  let https_session = proxy_config.https.and_then(|conf| conf.to_tls()).and_then(|https_conf| {
    let max_connections = https_conf.max_connections;
    let max_listeners   = 1;
    tls::ServerConfiguration::new(https_conf, 6148914691236517205, &mut event_loop, 1 + max_listeners + 6148914691236517205).map(|configuration| {
      Session::new(max_listeners, max_connections, 6148914691236517205, configuration, &mut event_loop)
    }).ok()
  });
  //TODO: implement for TCP

  let mut server = Server::new(event_loop, command, http_session, https_session, None);
  info!("starting event loop");
  server.run();
  info!("ending event loop");
}

pub fn start_worker_process(id: &str, config: &Config) -> nix::Result<(pid_t, Channel<OrderMessage,OrderMessageAnswer>)> {
  trace!("parent({})", unsafe { libc::getpid() });

  let (server, client) = UnixStream::pair().unwrap();

  // FD_CLOEXEC is set by default on every fd in Rust standard lib,
  // so we need to remove the flag on the client, otherwise
  // it won't be accessible
  let cl_flags = fcntl(client.as_raw_fd(), FcntlArg::F_GETFD).unwrap();
  let mut new_cl_flags = FdFlag::from_bits(cl_flags).unwrap();
  new_cl_flags.remove(FD_CLOEXEC);
  fcntl(client.as_raw_fd(), FcntlArg::F_SETFD(new_cl_flags));

  let channel_buffer_size = config.channel_buffer_size.unwrap_or(10000);
  let channel_max_buffer_size = channel_buffer_size * 2;

  let mut command: Channel<Config,OrderMessageAnswer> = Channel::new(
    server,
    channel_buffer_size,
    channel_max_buffer_size
  );
  command.set_nonblocking(false);

  let path = unsafe { get_executable_path() };

  info!("launching worker");
  match fork() {
    Ok(ForkResult::Parent{ child }) => {
      info!("worker launched: {}", child);
      command.write_message(config);
      command.set_nonblocking(true);

      let command: Channel<OrderMessage,OrderMessageAnswer> = command.into();
      Ok((child, command))
    },
    Ok(ForkResult::Child) => {
      trace!("child({}):\twill spawn a child", unsafe { libc::getpid() });
      Command::new(path.to_str().unwrap())
        .arg("worker")
        .arg("--fd")
        .arg(client.as_raw_fd().to_string())
        .arg("--id")
        .arg(id)
        .arg("--channel-buffer-size")
        .arg(channel_buffer_size.to_string())
        .exec();

      unreachable!();
    },
    Err(e) => {
      error!("Error during fork(): {}", e);
      Err(e)
    }
  }
}

#[cfg(target_os = "linux")]
pub unsafe fn get_executable_path() -> CString {
  let capacity = 2000;
  let mut temp:Vec<u8> = Vec::with_capacity(capacity);
  temp.extend(repeat(0).take(capacity));
  let mut pathbuf = CString::from_vec_unchecked(temp);
  let ptr = pathbuf.into_raw();

  let proc_path = CString::new("/proc/self/exe").unwrap();
  let sz = libc::readlink( proc_path.as_ptr(), ptr, 1999);
  let path = CString::from_raw(ptr);
  path
}

#[cfg(target_os = "macos")]
extern {
  pub fn _NSGetExecutablePath(buf: *mut c_char, size: *mut uint32_t) -> int32_t;
}

#[cfg(target_os = "macos")]
pub unsafe fn get_executable_path() -> CString {
  let capacity = 2000;
  let mut temp:Vec<u8> = Vec::with_capacity(capacity);
  temp.extend(repeat(0).take(capacity));
  let pathbuf = CString::from_vec_unchecked(temp);
  let ptr = pathbuf.into_raw();

  let mut size:uint32_t = capacity as u32;
  if _NSGetExecutablePath(ptr, &mut size) == 0 {

    let mut temp2:Vec<u8> = Vec::with_capacity(capacity);
    temp2.extend(repeat(0).take(capacity));
    let pathbuf2 = CString::from_vec_unchecked(temp2);
    let ptr2 = pathbuf2.into_raw();

    if libc::realpath(ptr, ptr2) != null_mut() {
      let path = CString::from_raw(ptr2);
      path
    } else {
      panic!();
    }
  } else {
    panic!("buffer too small");
  }
}
