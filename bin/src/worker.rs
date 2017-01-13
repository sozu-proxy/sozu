use mio_uds::UnixStream;
use libc::{self,c_char,uint32_t,int32_t,pid_t};
use std::io;
use std::ffi::CString;
use std::iter::repeat;
use std::ptr::null_mut;
use std::process::Command;
use std::os::unix::process::CommandExt;
use std::os::unix::io::{AsRawFd,FromRawFd};
use nix::unistd::*;
use nix::fcntl::{fcntl,FcntlArg,FdFlag,FD_CLOEXEC};

use sozu::network::{ProxyOrder,ServerMessage,http,tls};
use sozu::command::CommandChannel;
use command::Proxy;
use command::data::ProxyType;
use config::ProxyConfig;

pub fn start_workers(tag: &str, ls: &ProxyConfig) -> Option<Vec<Proxy>> {
  match ls.proxy_type {
    ProxyType::HTTP => {
      //FIXME: make safer
      if ls.to_http().is_some() {
        let mut http_proxies = Vec::new();
        for index in 1..ls.worker_count.unwrap_or(1) {
          let (pid, command) = start_worker_process(ls, tag, &index.to_string());
          let l =  Proxy::new(tag.to_string(), index as u8, pid, ls.proxy_type, ls.address.clone(), ls.port, command);
          http_proxies.push(l);
        }

        let (pid, command) = start_worker_process(ls, tag, &0.to_string());
        let l =  Proxy::new(tag.to_string(), 0, pid, ls.proxy_type, ls.address.clone(), ls.port, command);
        http_proxies.push(l);

        Some(http_proxies)
      } else {
        None
      }
    },
    ProxyType::HTTPS => {
      if ls.to_tls().is_some() {
        let mut tls_proxies = Vec::new();
        for index in 1..ls.worker_count.unwrap_or(1) {
          let (pid, command) = start_worker_process(ls, tag, &index.to_string());
          let l =  Proxy::new(tag.to_string(), index as u8, pid, ls.proxy_type, ls.address.clone(), ls.port, command);
          tls_proxies.push(l);
        }

        let (pid, command) = start_worker_process(ls, tag, &0.to_string());
        let l =  Proxy::new(tag.to_string(), 0, pid, ls.proxy_type, ls.address.clone(), ls.port, command);
        tls_proxies.push(l);

        Some(tls_proxies)
      } else {
        None
      }
    },
    _ => unimplemented!()
  }
}

fn generate_channels() -> io::Result<(CommandChannel<ProxyOrder,ServerMessage>, CommandChannel<ServerMessage,ProxyOrder>)> {
  let (command,proxy) = try!(UnixStream::pair());
  //FIXME: configurable buffer size
  let proxy_channel   = CommandChannel::new(proxy, 10000, 20000);
  let command_channel = CommandChannel::new(command, 10000, 20000);
  Ok((command_channel, proxy_channel))
}

pub fn begin_worker_process(fd: i32, id: &str, tag: &str) {
  let mut command: CommandChannel<ServerMessage,ProxyConfig> = CommandChannel::new(
    unsafe { UnixStream::from_raw_fd(fd) },
    10000,
    20000
  );

  command.set_nonblocking(false);

  let proxy_config = command.read_message().expect("worker could not read configuration from socket");
  println!("got message: {:?}", proxy_config);

  command.set_nonblocking(true);
  let command: CommandChannel<ServerMessage,ProxyOrder> = command.into();

  let t = format!("{}-{}", tag, id);

  match proxy_config.proxy_type {
    ProxyType::HTTP => {
      if let Some(config) = proxy_config.to_http() {
        http::start(t, config, command);
      }
    },
    ProxyType::HTTPS => {
      if let Some(config) = proxy_config.to_tls() {
        tls::start(t, config, command);
      }
    },
    _ => unimplemented!()
  }

  info!("proxy ended");
}

pub fn start_worker_process(config: &ProxyConfig, tag: &str, id: &str) -> (pid_t, CommandChannel<ProxyOrder,ServerMessage>) {
  println!("parent({})", unsafe { libc::getpid() });

  let (server, client) = UnixStream::pair().unwrap();

  // FD_CLOEXEC is set by default on every fd in Rust standard lib,
  // so we need to remove the flag on the client, otherwise
  // it won't be accessible
  let cl_flags = fcntl(client.as_raw_fd(), FcntlArg::F_GETFD).unwrap();
  let mut new_cl_flags = FdFlag::from_bits(cl_flags).unwrap();
  new_cl_flags.remove(FD_CLOEXEC);
  fcntl(client.as_raw_fd(), FcntlArg::F_SETFD(new_cl_flags));

  let mut command: CommandChannel<ProxyConfig,ServerMessage> = CommandChannel::new(
    server,
    10000,
    20000
  );
  command.set_nonblocking(false);

  let path = unsafe { get_executable_path() };

  println!("launching worker");
  //FIXME: remove the expect, return a result?
  match fork().expect("fork failed") {
    ForkResult::Parent{ child } => {
      println!("worker launched: {}", child);
      command.write_message(config);
      command.set_nonblocking(true);

      let command: CommandChannel<ProxyOrder,ServerMessage> = command.into();
      return (child, command);
    }
    ForkResult::Child => {
      println!("child({}):\twill spawn a child", unsafe { libc::getpid() });
      Command::new(path.to_str().unwrap())
        .arg("worker")
        .arg("--fd")
        .arg(client.as_raw_fd().to_string())
        .arg("--tag")
        .arg(tag)
        .arg("--id")
        .arg(id)
        .exec();

      unreachable!();
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
