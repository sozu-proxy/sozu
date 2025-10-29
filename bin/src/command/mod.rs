mod requests;
pub mod server;
pub mod sessions;
pub mod upgrade;

use std::{
    fs, io::Error as IoError, num::ParseIntError, os::unix::fs::PermissionsExt, path::PathBuf,
};

use mio::net::UnixListener;
use sozu_command_lib::{
    config::{Config, ConfigError},
    logging::{LogError, setup_logging_with_config},
};

use self::server::{HubError, ServerError};
use crate::{
    cli::Args,
    command::{requests::load_static_config, server::CommandHub},
    util::{UtilError, get_config_file_path, get_executable_path, setup_metrics, write_pid_file},
};

#[derive(thiserror::Error, Debug)]
pub enum StartError {
    #[error("failed to load config: {0}")]
    LoadConfig(ConfigError),
    #[error("could not get path of config file: {0}")]
    GetConfigPath(UtilError),
    #[error("could not delete previous socket at {0}: {1}")]
    RemoveSocket(PathBuf, IoError),
    #[error("could not bind to listener: {0}")]
    BindToListener(IoError),
    #[error("could not write PID file of main process: {0}")]
    WritePidFile(UtilError),
    #[error("failed to set metrics on the main process: {0}")]
    SetupMetrics(UtilError),
    #[error("failed to get executable path: {0}")]
    GetExecutablePath(UtilError),
    #[error("could not get path to the command socket: {0}")]
    GetSocketPath(ConfigError),
    #[error("could not create command hub: {0}")]
    CreateCommandHub(HubError),
    #[error("could not load file: {0}")]
    LoadProcFile(ConfigError),
    #[error("could parse system max file descriptors: {0}")]
    ParseSystemMaxFd(ParseIntError),
    #[error("Too many allowed connection for a worker")]
    TooManyAllowedConnections,
    #[error("could not set the unix socket permissions: {0}")]
    SetPermissions(IoError),
    #[error("could not launch new worker: {0}")]
    LaunchWorker(ServerError),
    #[error("could not setup the logger: {0}")]
    SetupLogging(LogError),
}

pub fn begin_main_process(args: &Args) -> Result<(), StartError> {
    let config_file_path = get_config_file_path(args).map_err(StartError::GetConfigPath)?;

    let config = Config::load_from_path(config_file_path).map_err(StartError::LoadConfig)?;

    setup_logging_with_config(&config, "MAIN").map_err(StartError::SetupLogging)?;
    info!("Starting up");
    setup_metrics(&config).map_err(StartError::SetupMetrics)?;
    write_pid_file(&config).map_err(StartError::WritePidFile)?;

    update_process_limits(&config)?;

    let executable_path = unsafe { get_executable_path().map_err(StartError::GetExecutablePath)? };

    let command_socket_path = config
        .command_socket_path()
        .map_err(StartError::GetSocketPath)?;

    let path = PathBuf::from(&command_socket_path);

    if fs::metadata(&path).is_ok() {
        info!("A socket is already present. Deleting...");
        fs::remove_file(&path).map_err(|io_err| StartError::RemoveSocket(path.clone(), io_err))?;
    }

    let unix_listener = UnixListener::bind(&path).map_err(StartError::BindToListener)?;

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(StartError::SetPermissions)?;

    // Create a copy of the state path to load state later
    let saved_state_path = config.saved_state.clone();
    let worker_count = config.worker_count;

    info!("Creating command hub");
    let mut command_hub = CommandHub::new(unix_listener, config, executable_path)
        .map_err(StartError::CreateCommandHub)?;

    info!("Launching workers");
    for _ in 0..worker_count {
        command_hub
            .launch_new_worker(None)
            .map_err(StartError::LaunchWorker)?;
    }

    info!("Load static configuration");
    load_static_config(&mut command_hub.server, None, None);

    if let Some(path) = saved_state_path {
        requests::load_state(&mut command_hub.server, None, &path);
    }

    command_hub.run();

    info!("main process stopped");
    Ok(())
}

#[cfg(target_os = "linux")]
/// We check the hard_limit. The soft_limit can be changed at runtime
/// by the process or any user. hard_limit can only be changed by root
fn update_process_limits(config: &Config) -> Result<(), StartError> {
    info!("Updating process limits");
    let wanted_opened_files = (config.max_connections as u64) * 2;

    let system_max_fd = get_system_max_fd("/proc/sys/fs/file-max")?;

    if config.max_connections > system_max_fd {
        error!(
            "Proxies total max_connections can't be higher than system's file-max limit. \
            Current limit: {}, current value: {}",
            system_max_fd, config.max_connections
        );
        return Err(StartError::TooManyAllowedConnections);
    }

    // Get the soft and hard limits for the current process
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) };

    // Ensure we don't exceed the hard limit
    if limits.rlim_max < wanted_opened_files {
        error!(
            "at least one worker can't have that many connections. \
              current max file descriptor hard limit is: {}, \
              configured max_connections is {} (the worker needs two file descriptors \
              per client connection)",
            limits.rlim_max, config.max_connections
        );
        return Err(StartError::TooManyAllowedConnections);
    }

    if limits.rlim_cur < wanted_opened_files && limits.rlim_cur != limits.rlim_max {
        // Try to get twice what we need to be safe, or rlim_max if we exceed that
        limits.rlim_cur = limits.rlim_max.min(wanted_opened_files * 2);
        unsafe {
            libc::setrlimit(libc::RLIMIT_NOFILE, &limits);

            // Refresh the data we have
            libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits);
        }
    }

    // Ensure we don't exceed the new soft limit
    if limits.rlim_cur < wanted_opened_files {
        error!(
            "at least one worker can't have that many connections. \
              current max file descriptor soft limit is: {}, \
              configured max_connections is {} (the worker needs two file descriptors \
              per client connection)",
            limits.rlim_cur, config.max_connections
        );
        return Err(StartError::TooManyAllowedConnections);
    }

    Ok(())
}

/// To ensure we don't exceed the system maximum capacity
fn get_system_max_fd(max_file_path: &str) -> Result<usize, StartError> {
    let max_file = Config::load_file(max_file_path).map_err(StartError::LoadProcFile)?;

    trace!("{}: '{}'", max_file_path, max_file);

    max_file
        .trim()
        .parse::<usize>()
        .map_err(StartError::ParseSystemMaxFd)
}

#[cfg(not(target_os = "linux"))]
fn update_process_limits(_: &Config) -> Result<(), StartError> {
    Ok(())
}
