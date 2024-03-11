#[cfg(target_os = "freebsd")]
use std::{ffi::c_void, iter::repeat, mem::size_of};
use std::{
    fs::File,
    io::Error as IoError,
    io::Seek,
    net::SocketAddr,
    os::unix::io::{AsRawFd, FromRawFd, IntoRawFd},
    os::unix::process::CommandExt,
    process::Command,
};

use libc::pid_t;

use mio::net::UnixStream;
use nix::{
    errno::Errno,
    unistd::{fork, ForkResult},
};
use tempfile::tempfile;

use sozu_command_lib::{
    channel::{Channel, ChannelError},
    config::Config,
    logging::{setup_logging, AccessLogFormat},
    proto::command::{ServerConfig, WorkerRequest, WorkerResponse},
    ready::Ready,
    request::{read_initial_state_from_file, RequestError},
    scm_socket::{Listeners, ScmSocket, ScmSocketError},
    state::{ConfigState, StateError},
};

use sozu_lib::{
    metrics::{self, MetricError},
    server::{Server, ServerError as LibServerError},
};

use crate::util::{self, UtilError};

#[derive(thiserror::Error, Debug)]
pub enum WorkerError {
    #[error("could not read on the channel")]
    ReadChannel(ChannelError),
    #[error("could not parse configuration from temporary file: {0}")]
    ReadRequestsFromFile(RequestError),
    #[error("could not setup metrics on new worker: {0}")]
    SetupMetrics(MetricError),
    #[error("could not create new worker from config: {0}")]
    NewServerFromConfig(LibServerError),
    #[error("could not create {kind} scm socket: {scm_err}")]
    CreateScmSocket {
        kind: String,
        scm_err: ScmSocketError,
    },
    #[error("could not create temporary file to pass the state to the new worker: {0}")]
    CreateStateFile(IoError),
    #[error("could not disable cloexec on {fd_name}'s file descriptor: {util_err}")]
    DisableCloexec {
        fd_name: String,
        util_err: UtilError,
    },
    #[error("could not write state to temporary file: {0}")]
    WriteStateFile(StateError),
    #[error("could not rewind the temporary state file: {0}")]
    Rewind(IoError),
    #[error("could not create MIO pair of unix stream: {0}")]
    CreateUnixStream(IoError),
    #[error("could not send config to the new worker: {0}")]
    SendConfig(ChannelError),
    #[error("unix fork failed: {0}")]
    Fork(Errno),
    #[error("Could not set the worker-to-main channel to {state}: {channel_err}")]
    SetChannel {
        state: String,
        channel_err: ChannelError,
    },
}

/// called within a worker process, this starts the actual proxy
pub fn begin_worker_process(
    worker_to_main_channel_fd: i32,
    worker_to_main_scm_fd: i32,
    configuration_state_fd: i32,
    id: i32,
    command_buffer_size: u64,
    max_command_buffer_size: u64,
) -> Result<(), WorkerError> {
    let mut worker_to_main_channel: Channel<WorkerResponse, ServerConfig> = Channel::new(
        unsafe { UnixStream::from_raw_fd(worker_to_main_channel_fd) },
        command_buffer_size,
        max_command_buffer_size,
    );

    worker_to_main_channel
        .blocking()
        .map_err(|channel_err| WorkerError::SetChannel {
            state: "blocking".to_string(),
            channel_err,
        })?;

    let mut configuration_state_file = unsafe { File::from_raw_fd(configuration_state_fd) };

    let worker_config = worker_to_main_channel
        .read_message()
        .map_err(WorkerError::ReadChannel)?;

    let worker_id = format!("{}-{:02}", "WRK", id);

    let access_log_format = AccessLogFormat::from(&worker_config.access_log_format());

    // do not try to log anything before this, or the logger will panic
    setup_logging(
        &worker_config.log_target,
        worker_config.log_colored,
        worker_config.access_logs_target.as_deref(),
        Some(access_log_format),
        Some(worker_config.log_colored),
        &worker_config.log_level,
        &worker_id,
    );

    trace!(
        "Creating worker {} with config: {:#?}",
        worker_id,
        worker_config
    );
    info!("worker {} starting...", id);

    let initial_state = read_initial_state_from_file(&mut configuration_state_file)
        .map_err(WorkerError::ReadRequestsFromFile)?;

    worker_to_main_channel
        .nonblocking()
        .map_err(|channel_err| WorkerError::SetChannel {
            state: "nonblocking".to_string(),
            channel_err,
        })?;

    let mut worker_to_main_channel: Channel<WorkerResponse, WorkerRequest> =
        worker_to_main_channel.into();
    worker_to_main_channel.readiness.insert(Ready::READABLE);

    if let Some(metrics) = worker_config.metrics.as_ref() {
        let address = metrics
            .address
            .parse::<SocketAddr>()
            .expect("Could not parse metrics address");
        metrics::setup(
            &address,
            worker_id,
            metrics.tagged_metrics,
            metrics.prefix.clone(),
        )
        .map_err(WorkerError::SetupMetrics)?;
    }

    let worker_to_main_scm_socket =
        ScmSocket::new(worker_to_main_scm_fd).map_err(|scm_err| WorkerError::CreateScmSocket {
            kind: "worker-to-main".to_string(),
            scm_err,
        })?;

    let mut server = Server::try_new_from_config(
        worker_to_main_channel,
        worker_to_main_scm_socket,
        worker_config,
        initial_state,
        true,
    )
    .map_err(WorkerError::NewServerFromConfig)?;

    info!("starting event loop");
    server.run();
    info!("ending event loop");
    Ok(())
}

/// unix-forks the main process
///
/// - Parent: sends config, state and listeners to the new worker
/// - Child: calls the sozu executable path like so: `sozu worker --id <worker_id> [...]`
///
/// returns the child process pid, and channels to talk to it.
pub fn fork_main_into_worker(
    worker_id: &str,
    config: &Config,
    executable_path: String,
    state: &ConfigState,
    listeners: Option<Listeners>,
) -> Result<(pid_t, Channel<WorkerRequest, WorkerResponse>, ScmSocket), WorkerError> {
    trace!("parent({})", unsafe { libc::getpid() });

    let mut state_file = tempfile().map_err(WorkerError::CreateStateFile)?;
    util::disable_close_on_exec(state_file.as_raw_fd()).map_err(|util_err| {
        WorkerError::DisableCloexec {
            fd_name: "state_file".to_string(),
            util_err,
        }
    })?;

    state
        .write_initial_state_to_file(&mut state_file)
        .map_err(WorkerError::WriteStateFile)?;

    state_file.rewind().map_err(WorkerError::Rewind)?;

    let (main_to_worker, worker_to_main) =
        UnixStream::pair().map_err(WorkerError::CreateUnixStream)?;
    let (main_to_worker_scm, worker_to_main_scm) =
        UnixStream::pair().map_err(WorkerError::CreateUnixStream)?;

    let main_to_worker_scm =
        ScmSocket::new(main_to_worker_scm.into_raw_fd()).map_err(|scm_err| {
            WorkerError::CreateScmSocket {
                kind: "main-to-worker".to_string(),
                scm_err,
            }
        })?;

    util::disable_close_on_exec(worker_to_main.as_raw_fd()).map_err(|util_err| {
        WorkerError::DisableCloexec {
            fd_name: "worker-to-main".to_string(),
            util_err,
        }
    })?;
    util::disable_close_on_exec(worker_to_main_scm.as_raw_fd()).map_err(|util_err| {
        WorkerError::DisableCloexec {
            fd_name: "worker-to-main-scm".to_string(),
            util_err,
        }
    })?;

    let worker_config = ServerConfig::from(config);

    let mut main_to_worker_channel: Channel<ServerConfig, WorkerResponse> = Channel::new(
        main_to_worker,
        worker_config.command_buffer_size,
        worker_config.max_command_buffer_size,
    );

    // DISCUSS: should we really block the channel just to write on it?
    if let Err(e) = main_to_worker_channel.blocking() {
        error!("Could not block the main-to-worker channel: {}", e);
    }

    info!("launching worker {}", worker_id);
    debug!("executable path is {}", executable_path);

    match unsafe { fork().map_err(WorkerError::Fork)? } {
        ForkResult::Parent { child: worker_pid } => {
            info!("launching worker {} with pid {}", worker_id, worker_pid);
            main_to_worker_channel
                .write_message(&worker_config)
                .map_err(WorkerError::SendConfig)?;

            main_to_worker_channel
                .nonblocking()
                .map_err(|channel_err| WorkerError::SetChannel {
                    state: "nonblocking".to_string(),
                    channel_err,
                })?;

            if let Some(listeners) = listeners {
                info!("sending listeners to new worker: {:?}", listeners);
                let result = main_to_worker_scm.send_listeners(&listeners);
                info!("sent listeners from main: {:?}", result);
                listeners.close();
            };

            util::disable_close_on_exec(main_to_worker_scm.fd).map_err(|util_err| {
                WorkerError::DisableCloexec {
                    fd_name: "main-to-worker-main-scm".to_string(),
                    util_err,
                }
            })?;

            Ok((
                worker_pid.into(),
                main_to_worker_channel.into(),
                main_to_worker_scm,
            ))
        }
        ForkResult::Child => {
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
    }
}
