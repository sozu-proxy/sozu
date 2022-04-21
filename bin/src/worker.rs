#[cfg(target_os = "freebsd")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::iter::repeat;
#[cfg(target_os = "freebsd")]
use std::iter::repeat;
#[cfg(target_os = "freebsd")]
use std::mem::size_of;
#[cfg(target_os = "macos")]
use std::ptr::null_mut;
use std::{
    fs::File,
    io::{Seek, SeekFrom},
    os::unix::io::{AsRawFd, FromRawFd, IntoRawFd},
    os::unix::process::CommandExt,
    process::Command,
};

use anyhow::{bail, Context};
use libc::{self, pid_t};
#[cfg(target_os = "macos")]
use libc::{c_char, PATH_MAX};
#[cfg(target_os = "freebsd")]
use libc::{sysctl, CTL_KERN, KERN_PROC, KERN_PROC_PATHNAME, PATH_MAX};
use mio::net::UnixStream;
use nix::{self, unistd::*};
use serde_json;
use tempfile::tempfile;

use sozu::{metrics, server::Server};
use sozu_command_lib::{
    channel::Channel,
    config::Config,
    logging::target_to_backend,
    proxy::{ProxyRequest, ProxyRequestData, ProxyResponse},
    ready::Ready,
    scm_socket::{Listeners, ScmSocket},
    state::ConfigState,
};

use crate::{command::Worker, logging, util};

pub fn start_workers(executable_path: String, config: &Config) -> anyhow::Result<Vec<Worker>> {
    let state = ConfigState::new();
    let mut workers = Vec::new();
    for index in 0..config.worker_count {
        let listeners = Some(Listeners {
            http: Vec::new(),
            tls: Vec::new(),
            tcp: Vec::new(),
        });

        let (pid, command, scm) = start_worker_process(
            &index.to_string(),
            config,
            executable_path.clone(),
            &state,
            listeners,
        )?;
        let mut w = Worker::new(index as u32, pid, command, scm, config);
        // the new worker expects a status message at startup
        if let Some(channel) = w.channel.as_mut() {
            channel.set_blocking(true);
            channel.write_message(&ProxyRequest {
                id: format!("start-status-{}", index),
                order: ProxyRequestData::Status,
            });
            channel.set_nonblocking(true);
        }
        workers.push(w);
    }
    info!("Created workers");
    Ok(workers)
}

pub fn start_worker(
    id: u32,
    config: &Config,
    executable_path: String,
    state: &ConfigState,
    listeners: Option<Listeners>,
) -> anyhow::Result<Worker> {
    let (pid, command, scm) =
        start_worker_process(&id.to_string(), config, executable_path, state, listeners)?;

    Ok(Worker::new(id, pid, command, scm, config))
}

pub fn begin_worker_process(
    fd: i32,
    scm: i32,
    configuration_state_fd: i32,
    id: i32,
    command_buffer_size: usize,
    max_command_buffer_size: usize,
) -> Result<(), anyhow::Error> {
    let mut command: Channel<ProxyResponse, Config> = Channel::new(
        unsafe { UnixStream::from_raw_fd(fd) },
        command_buffer_size,
        max_command_buffer_size,
    );

    command.set_nonblocking(false);

    let configuration_state_file = unsafe { File::from_raw_fd(configuration_state_fd) };
    let config_state: ConfigState = serde_json::from_reader(configuration_state_file)
        .with_context(|| "could not parse configuration state data")?;

    let worker_config = command
        .read_message()
        .with_context(|| "worker could not read configuration from socket")?;
    //println!("got message: {:?}", worker_config);

    let worker_id = format!("{}-{:02}", "WRK", id);
    logging::setup(
        worker_id.clone(),
        &worker_config.log_level,
        &worker_config.log_target,
        worker_config.log_access_target.as_deref(),
    );
    let backend = target_to_backend(&worker_config.log_target);
    let access_backend = worker_config
        .log_access_target
        .as_deref()
        .map(target_to_backend);
    sozu_command_lib::logging::Logger::init(
        worker_id.clone(),
        &worker_config.log_level,
        backend,
        access_backend,
    );
    info!("worker {} starting...", id);

    command.set_nonblocking(true);
    let mut command: Channel<ProxyResponse, ProxyRequest> = command.into();
    command.readiness.insert(Ready::readable());

    if let Some(metrics) = worker_config.metrics.as_ref() {
        metrics::setup(
            &metrics.address,
            worker_id,
            metrics.tagged_metrics,
            metrics.prefix.clone(),
        );
    }

    let mut server = Server::new_from_config(
        command,
        ScmSocket::new(scm),
        worker_config,
        config_state,
        true,
    );

    info!("starting event loop");
    server.run();
    info!("ending event loop");
    Ok(())
}

pub fn start_worker_process(
    id: &str,
    config: &Config,
    executable_path: String,
    state: &ConfigState,
    listeners: Option<Listeners>,
) -> anyhow::Result<(pid_t, Channel<ProxyRequest, ProxyResponse>, ScmSocket)> {
    trace!("parent({})", unsafe { libc::getpid() });

    let mut state_file =
        tempfile().expect("could not create temporary file for configuration state");
    util::disable_close_on_exec(state_file.as_raw_fd())?;

    serde_json::to_writer(&mut state_file, state)
        .with_context(|| "could not write upgrade data to temporary file")?;
    state_file
        .seek(SeekFrom::Start(0))
        .with_context(|| "could not seek to beginning of file")?;

    let (server, client) = UnixStream::pair()?;
    let (scm_server_fd, scm_client) = UnixStream::pair()?;

    let scm_server = ScmSocket::new(scm_server_fd.into_raw_fd());

    util::disable_close_on_exec(client.as_raw_fd())?;
    util::disable_close_on_exec(scm_client.as_raw_fd())?;

    let mut command: Channel<Config, ProxyResponse> = Channel::new(
        server,
        config.command_buffer_size,
        config.max_command_buffer_size,
    );
    command.set_nonblocking(false);

    info!("{} launching worker", id);
    debug!("executable path is {}", executable_path);
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            info!("{} worker launched: {}", id, child);
            command.write_message(config);
            command.set_nonblocking(true);

            if let Some(l) = listeners {
                info!("sending listeners to new worker: {:?}", l);
                let res = scm_server.send_listeners(&l);
                info!("sent listeners from main: {:?}", res);
                l.close();
            };
            util::disable_close_on_exec(scm_server.fd)?;

            let command: Channel<ProxyRequest, ProxyResponse> = command.into();
            Ok((child.into(), command, scm_server))
        }
        Ok(ForkResult::Child) => {
            trace!("child({}):\twill spawn a child", unsafe { libc::getpid() });
            Command::new(executable_path)
                .arg("worker")
                .arg("--id")
                .arg(id)
                .arg("--fd")
                .arg(client.as_raw_fd().to_string())
                .arg("--scm")
                .arg(scm_client.as_raw_fd().to_string())
                .arg("--configuration-state-fd")
                .arg(state_file.as_raw_fd().to_string())
                .arg("--command-buffer-size")
                .arg(config.command_buffer_size.to_string())
                .arg("--max-command-buffer-size")
                .arg(config.max_command_buffer_size.to_string())
                .exec();

            unreachable!();
        }
        Err(e) => {
            error!("Error during fork(): {}", e);
            Err(anyhow::Error::from(e))
        }
    }
}

#[cfg(target_os = "linux")]
pub unsafe fn get_executable_path() -> anyhow::Result<String> {
    use std::fs;

    let path = fs::read_link("/proc/self/exe").with_context(|| "/proc/self/exe doesn't exist")?;

    let mut path_str = match path.into_os_string().into_string() {
        Ok(s) => s,
        Err(_) => bail!("Failed to convert PathBuf to String"),
    };

    if path_str.ends_with(" (deleted)") {
        // The kernel appends " (deleted)" to the symlink when the original executable has been replaced
        let len = path_str.len();
        path_str.truncate(len - 10)
    }

    Ok(path_str)
}

#[cfg(target_os = "macos")]
extern "C" {
    pub fn _NSGetExecutablePath(buf: *mut c_char, size: *mut u32) -> i32;
}

#[cfg(target_os = "macos")]
pub unsafe fn get_executable_path() -> String {
    let capacity = PATH_MAX as usize;
    let mut temp: Vec<u8> = Vec::with_capacity(capacity);
    temp.extend(repeat(0).take(capacity));
    let pathbuf = CString::from_vec_unchecked(temp);
    let ptr = pathbuf.into_raw();

    let mut size = capacity as u32;
    if _NSGetExecutablePath(ptr, &mut size) == 0 {
        let mut temp2: Vec<u8> = Vec::with_capacity(capacity);
        temp2.extend(repeat(0).take(capacity));
        let pathbuf2 = CString::from_vec_unchecked(temp2);
        let ptr2 = pathbuf2.into_raw();

        if libc::realpath(ptr, ptr2) != null_mut() {
            let path = CString::from_raw(ptr2);
            path.to_str()
                .expect("failed to convert CString to String")
                .to_string()
        } else {
            panic!();
        }
    } else {
        panic!("buffer too small");
    }
}

#[cfg(target_os = "freebsd")]
pub unsafe fn get_executable_path() -> String {
    let mut capacity = PATH_MAX as usize;
    let mut path: Vec<u8> = Vec::with_capacity(capacity);
    path.extend(repeat(0).take(capacity));

    let mib: Vec<i32> = vec![CTL_KERN, KERN_PROC, KERN_PROC_PATHNAME, -1];
    let len = mib.len() * size_of::<i32>();
    let element_size = size_of::<i32>();

    let res = sysctl(
        mib.as_ptr(),
        (len / element_size) as u32,
        path.as_mut_ptr() as *mut c_void,
        &mut capacity,
        std::ptr::null() as *const c_void,
        0,
    );
    if res != 0 {
        panic!("Could not retrieve the path of the executable");
    }

    String::from_raw_parts(path.as_mut_ptr(), capacity - 1, path.len())
}
