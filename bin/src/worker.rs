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
    io::Seek,
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

use tempfile::tempfile;

use sozu::{metrics, server::Server};
use sozu_command_lib::{
    channel::Channel,
    config::Config,
    logging::target_to_backend,
    worker::{ProxyRequest, ProxyRequestOrder, ProxyResponse},
    ready::Ready,
    scm_socket::{Listeners, ScmSocket},
    state::ConfigState,
};

use crate::{command::Worker, logging, util};

/// Called once at the beginning of the main process, this forks main into as many workers
pub fn start_workers(executable_path: String, config: &Config) -> anyhow::Result<Vec<Worker>> {
    let state = ConfigState::new();
    let mut workers = Vec::new();
    for index in 0..config.worker_count {
        let listeners = Some(Listeners {
            http: Vec::new(),
            tls: Vec::new(),
            tcp: Vec::new(),
        });

        let (pid, command_channel, scm_socket) = fork_main_into_worker(
            &index.to_string(),
            config,
            executable_path.clone(),
            &state,
            listeners,
        )?;
        let mut worker = Worker::new(index as u32, pid, command_channel, scm_socket, config);

        // the new worker expects a status message at startup
        if let Some(worker_channel) = worker.worker_channel.as_mut() {
            if let Err(e) = worker_channel.blocking() {
                error!("Could not block the worker channel: {}", e);
            }

            worker_channel
                .write_message(&ProxyRequest {
                    id: format!("start-status-{index}"),
                    order: ProxyRequestOrder::Status,
                })
                .with_context(|| "Could not send status request to the worker")?;

            if let Err(e) = worker_channel.nonblocking() {
                error!("Could not unblock the worker channel: {}", e);
            }
        }

        workers.push(worker);
    }
    info!("Created workers");
    Ok(workers)
}

/// called by the CommandServer to start an individual worker
/// returns a handle of the worker, with channels to write to it
pub fn start_worker(
    id: u32,
    config: &Config,
    executable_path: String,
    state: &ConfigState,
    listeners: Option<Listeners>,
) -> anyhow::Result<Worker> {
    let (worker_pid, main_to_worker_channel, main_to_worker_scm) =
        fork_main_into_worker(&id.to_string(), config, executable_path, state, listeners)?;

    Ok(Worker::new(
        id,
        worker_pid,
        main_to_worker_channel,
        main_to_worker_scm,
        config,
    ))
}

/// called within a worker process, this starts the actual proxy
pub fn begin_worker_process(
    worker_to_main_channel_fd: i32,
    worker_to_main_scm_fd: i32,
    configuration_state_fd: i32,
    id: i32,
    command_buffer_size: usize,
    max_command_buffer_size: usize,
) -> Result<(), anyhow::Error> {
    let mut worker_to_main_channel: Channel<ProxyResponse, Config> = Channel::new(
        unsafe { UnixStream::from_raw_fd(worker_to_main_channel_fd) },
        command_buffer_size,
        max_command_buffer_size,
    );

    if let Err(e) = worker_to_main_channel.blocking() {
        error!("Could not block the worker-to-main channel: {}", e);
    }

    let configuration_state_file = unsafe { File::from_raw_fd(configuration_state_fd) };
    let config_state: ConfigState = serde_json::from_reader(configuration_state_file)
        .with_context(|| "could not parse configuration state data")?;

    let worker_config = worker_to_main_channel
        .read_message()
        .with_context(|| "worker could not read configuration from socket")?;

    let worker_id = format!("{}-{:02}", "WRK", id);
    logging::setup(
        worker_id.clone(),
        &worker_config.log_level,
        &worker_config.log_target,
        worker_config.log_access_target.as_deref(),
    );

    trace!(
        "Creating worker {} with config: {:#?}",
        worker_id,
        worker_config
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

    if let Err(e) = worker_to_main_channel.nonblocking() {
        error!("Could not unblock the worker-to-main channel: {}", e);
    }

    let mut worker_to_main_channel: Channel<ProxyResponse, ProxyRequest> =
        worker_to_main_channel.into();
    worker_to_main_channel.readiness.insert(Ready::readable());

    if let Some(metrics) = worker_config.metrics.as_ref() {
        metrics::setup(
            &metrics.address,
            worker_id,
            metrics.tagged_metrics,
            metrics.prefix.clone(),
        )
        .with_context(|| "Could not setup metrics")?;
    }
    let worker_to_main_scm_socket = ScmSocket::new(worker_to_main_scm_fd)
        .with_context(|| "could not create worker-to-main scm socket")?;

    let mut server = Server::try_new_from_config(
        worker_to_main_channel,
        worker_to_main_scm_socket,
        worker_config,
        config_state,
        true,
    )
    .with_context(|| "Could not create server from config")?;

    info!("starting event loop");
    server.run();
    info!("ending event loop");
    Ok(())
}

/// unix-forks the main process
///
/// - Parent: sends config and listeners to the new worker
/// - Child: calls the sozu executable path like so: `sozu worker --id <worker_id> [...]`
///
/// returns the child process pid, and channels to talk to it.
pub fn fork_main_into_worker(
    worker_id: &str,
    config: &Config,
    executable_path: String,
    state: &ConfigState,
    listeners: Option<Listeners>,
) -> anyhow::Result<(pid_t, Channel<ProxyRequest, ProxyResponse>, ScmSocket)> {
    trace!("parent({})", unsafe { libc::getpid() });

    let mut state_file =
        tempfile().with_context(|| "could not create temporary file for configuration state")?;
    util::disable_close_on_exec(state_file.as_raw_fd())?;

    serde_json::to_writer(&mut state_file, state)
        .with_context(|| "could not write upgrade data to temporary file")?;

    state_file
        .rewind()
        .with_context(|| "could not seek to beginning of file")?;

    let (main_to_worker, worker_to_main) = UnixStream::pair()?;
    let (main_to_worker_scm, worker_to_main_scm) = UnixStream::pair()?;

    let main_to_worker_scm = ScmSocket::new(main_to_worker_scm.into_raw_fd())
        .with_context(|| "Could not create main-to-worker scm socket")?;

    util::disable_close_on_exec(worker_to_main.as_raw_fd())?;
    util::disable_close_on_exec(worker_to_main_scm.as_raw_fd())?;

    let mut main_to_worker_channel: Channel<Config, ProxyResponse> = Channel::new(
        main_to_worker,
        config.command_buffer_size,
        config.max_command_buffer_size,
    );

    // DISCUSS: should we really block the channel just to write on it?
    if let Err(e) = main_to_worker_channel.blocking() {
        error!("Could not block the main-to-worker channel: {}", e);
    }

    info!("{} launching worker", worker_id);
    debug!("executable path is {}", executable_path);
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child: worker_pid }) => {
            info!("{} worker launched: {}", worker_id, worker_pid);
            main_to_worker_channel
                .write_message(config)
                .with_context(|| "Could not send config to the new worker using the channel")?;

            if let Err(e) = main_to_worker_channel.nonblocking() {
                error!("Could not unblock the main-to-worker channel: {}", e);
            }

            if let Some(listeners) = listeners {
                info!("sending listeners to new worker: {:?}", listeners);
                let result = main_to_worker_scm.send_listeners(&listeners);
                info!("sent listeners from main: {:?}", result);
                listeners.close();
            };
            util::disable_close_on_exec(main_to_worker_scm.fd)?;

            Ok((
                worker_pid.into(),
                main_to_worker_channel.into(),
                main_to_worker_scm,
            ))
        }
        Ok(ForkResult::Child) => {
            trace!("child({}):\twill spawn a child", unsafe { libc::getpid() });
            Command::new(executable_path)
                .arg("worker")
                .arg("--id")
                .arg(worker_id)
                .arg("--fd")
                .arg(worker_to_main.as_raw_fd().to_string())
                .arg("--scm")
                .arg(worker_to_main_scm.as_raw_fd().to_string())
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
pub fn get_executable_path() -> anyhow::Result<String> {
    let path = env::current_exe().with_context(|| "failed to retrieve current executable path")?;
    Ok(path.to_string_lossy().to_string())
}

#[cfg(target_os = "freebsd")]
pub unsafe fn get_executable_path() -> anyhow::Result<String> {
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

    Ok(String::from_raw_parts(path.as_mut_ptr(), capacity - 1, path.len()))
}
