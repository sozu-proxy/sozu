use libc::{self,c_char,uint8_t,uint32_t,int32_t};
use std::ffi::CString;
use std::iter::repeat;
use std::ptr::null_mut;
use std::sync::mpsc::{channel};
use std::thread::{self,JoinHandle};
use mio::channel;

use sozu::network::{self,ProxyOrder};
use command::Listener;
use command::data::ListenerType;
use config::ListenerConfig;

pub fn start_workers(tag: &str, ls: &ListenerConfig) -> Option<(JoinHandle<()>, Vec<Listener>)> {
  match ls.listener_type {
    ListenerType::HTTP => {
      //FIXME: make safer
      if let Some(conf) = ls.to_http() {
        let mut http_listeners = Vec::new();

        for index in 1..ls.worker_count.unwrap_or(1) {
          let (sender, receiver) = channel::<network::ServerMessage>();
          let (tx, rx) = channel::channel::<ProxyOrder>();
          let config = conf.clone();
          let t = format!("{}-{}", tag, index);
          thread::spawn(move || {
            network::http::start_listener(t, config, sender, rx);
          });
          let l =  Listener::new(tag.to_string(), index as u8, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
          http_listeners.push(l);
        }

        let (sender, receiver) = channel::<network::ServerMessage>();
        let (tx, rx) = channel::channel::<ProxyOrder>();
        let t = format!("{}-{}", tag, 0);
        //FIXME: keep this to get a join guard
        let jg = thread::spawn(move || {
          network::http::start_listener(t, conf, sender, rx);
        });

        let l =  Listener::new(tag.to_string(), 0, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
        http_listeners.push(l);
        Some((jg, http_listeners))
      } else {
        None
      }
    },
    ListenerType::HTTPS => {
      if let Some(conf) = ls.to_tls() {
        let mut tls_listeners = Vec::new();

        for index in 1..ls.worker_count.unwrap_or(1) {
          let (sender, receiver) = channel::<network::ServerMessage>();
          let (tx, rx) = channel::channel::<ProxyOrder>();
          let config = conf.clone();
          let t = format!("{}-{}", tag, index);
          thread::spawn(move || {
            network::tls::start_listener(t, config, sender, rx);
          });

          let l =  Listener::new(tag.to_string(), index as u8, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
          tls_listeners.push(l);
        }

        let (sender, receiver) = channel::<network::ServerMessage>();
        let (tx, rx) = channel::channel::<ProxyOrder>();
          let t = format!("{}-{}", tag, 0);
        //FIXME: keep this to get a join guard
        let jg = thread::spawn(move || {
          network::tls::start_listener(t, conf, sender, rx);
        });

        let l =  Listener::new(tag.to_string(), 0, ls.listener_type, ls.address.clone(), ls.port, tx, receiver);
        tls_listeners.push(l);
        Some((jg, tls_listeners))
      } else {
        None
      }
    },
    _ => unimplemented!()
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
  let mut pathbuf = CString::from_vec_unchecked(temp);
  let ptr = pathbuf.into_raw();

  let mut size:uint32_t = capacity as u32;
  if _NSGetExecutablePath(ptr, &mut size) == 0 {

    let mut temp2:Vec<u8> = Vec::with_capacity(capacity);
    temp2.extend(repeat(0).take(capacity));
    let mut pathbuf2 = CString::from_vec_unchecked(temp2);
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
