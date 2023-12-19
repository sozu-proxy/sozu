use std::{fs, path::PathBuf};

use std::os::unix::fs::PermissionsExt;

use anyhow::{bail, Context};
use mio::net::UnixListener;
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};

use sozu_command_lib::config::Config;

use crate::{
    command_v2::{requests::load_static_config, server::CommandHub, sessions::ClientSession},
    worker::Worker,
};

mod requests;
mod server;
mod sessions;

pub fn start_server(
    config: Config,
    command_socket_path: String,
    workers: Vec<Worker>,
) -> anyhow::Result<()> {
    let path = PathBuf::from(&command_socket_path);

    if fs::metadata(&path).is_ok() {
        info!("A socket is already present. Deleting...");
        fs::remove_file(&path)
            .with_context(|| format!("could not delete previous socket at {path:?}"))?;
    }

    let unix_listener = match UnixListener::bind(&path) {
        Ok(unix_listener) => unix_listener,
        Err(e) => {
            error!("could not create unix socket: {:?}", e);
            // the workers did not even get the configuration, we can kill them right away
            for worker in workers {
                error!("killing worker n°{} (PID {})", worker.id, worker.pid);
                let _ = kill(Pid::from_raw(worker.pid), Signal::SIGKILL).map_err(|e| {
                    error!("could not kill worker: {:?}", e);
                });
            }
            bail!("couldn't start server");
        }
    };

    if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
        error!("could not set the unix socket permissions: {:?}", e);
        let _ = fs::remove_file(&path).map_err(|e2| {
            error!("could not remove the unix socket: {:?}", e2);
        });
        // the workers did not even get the configuration, we can kill them right away
        for worker in workers {
            error!("killing worker n°{} (PID {})", worker.id, worker.pid);
            let _ = kill(Pid::from_raw(worker.pid), Signal::SIGKILL).map_err(|e| {
                error!("could not kill worker: {:?}", e);
            });
        }
        bail!("couldn't start server");
    }

    let mut command_hub = CommandHub::new(unix_listener, config)?;
    for mut worker in workers {
        command_hub.register_worker(worker.id, worker.pid, worker.worker_channel.take().unwrap())
    }

    load_static_config(&mut command_hub.server);

    command_hub.run();
}
