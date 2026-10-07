//! Master/worker hot-upgrade orchestration.
//!
//! Forks a child master, preserves listener and command-channel FDs across
//! exec, restores their userspace state, and tears down the previous master
//! once the new one acks. Keeps the data plane uninterrupted by handing off
//! accepted listeners and existing worker FDs.

use std::{
    fs::File,
    io::{Error as IoError, Read, Seek, Write},
    os::{
        fd::{OwnedFd, RawFd},
        unix::{
            io::{AsRawFd, FromRawFd},
            process::CommandExt,
        },
    },
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use libc::pid_t;
use mio::net::UnixStream;
use nix::{
    errno::Errno,
    unistd::{ForkResult, fork},
};
use serde_json::Error as SerdeError;
use sozu_command_lib::{
    channel::{Channel, ChannelError},
    logging::{LogError, setup_logging_with_config},
    sd_notify,
};
use tempfile::tempfile;

use crate::{
    command::{
        server::{CommandHub, HubError, ServerError},
        upgrade::{LegacyUpgradeData, UpgradeData, UpgradeHandshake, UpgradeStage},
    },
    util::{self, UtilError},
};

#[derive(thiserror::Error, Debug)]
pub enum UpgradeError {
    #[error("could not create temporary state file for the upgrade: {0}")]
    CreateUpgradeFile(IoError),
    #[error("could not disable cloexec on {fd_name}'s file descriptor: {util_err}")]
    DisableCloexec {
        fd_name: String,
        util_err: UtilError,
    },
    #[error("could not create MIO pair of unix stream: {0}")]
    CreateUnixStream(IoError),
    #[error("could not rewind the temporary upgrade file: {0}")]
    Rewind(IoError),
    #[error("could not write upgrade data to temporary file: {0}")]
    SerdeWriteError(SerdeError),
    #[error("could not write upgrade data to temporary file: {0}")]
    WriteFile(IoError),
    #[error("could not read upgrade data from file: {0}")]
    ReadFile(IoError),
    #[error("could not read upgrade data to temporary file: {0}")]
    SerdeReadError(SerdeError),
    #[error("unix fork failed: {0}")]
    Fork(Errno),
    #[error("failed to set metrics on the new main process: {0}")]
    SetupMetrics(UtilError),
    #[error("could not write PID file of new main process: {0}")]
    WritePidFile(UtilError),
    #[error(
        "the channel failed to send confirmation of upgrade {result} to the old main process: {channel_err}"
    )]
    SendConfirmation {
        result: String,
        channel_err: ChannelError,
    },
    #[error(
        "Could not block the fork confirmation channel: {0}. This is not normal, you may need to restart sozu"
    )]
    BlockChannel(ChannelError),
    #[error("could not create a command hub from the upgrade data: {0}")]
    CreateHub(HubError),
    #[error("could not enable cloexec after upgrade: {0}")]
    EnableCloexec(ServerError),
    #[error("could not handle SIGTERM: {0}")]
    HandleSigterm(ServerError),
    #[error("could not setup the logger: {0}")]
    SetupLogging(LogError),
    #[error("could not start the main-upgrade protocol probe: {0}")]
    ProbeSpawn(IoError),
    #[error("main-upgrade candidate rejected protocol v2 with status {0}")]
    ProbeRejected(std::process::ExitStatus),
    #[error("main-upgrade candidate did not answer the protocol probe within {0:?}")]
    ProbeTimeout(Duration),
    #[error("unsupported main-upgrade protocol {0}")]
    UnsupportedProtocol(u16),
    #[error(
        "legacy main upgrade is unavailable while NOTIFY_SOCKET is set; use a controlled restart for this systemd service"
    )]
    LegacyUpgradeUnderSystemd,
    #[cfg(not(target_os = "linux"))]
    #[error("legacy main upgrade requires Linux pidfd support")]
    LegacyUpgradeUnsupportedPlatform,
    #[error("legacy main upgrade cannot follow invalid parent pid {0}")]
    InvalidLegacyParent(pid_t),
    #[error("invalid inherited {name} descriptor {fd}")]
    InvalidInheritedDescriptor { name: &'static str, fd: RawFd },
    #[error(
        "legacy main-upgrade parent changed from pid {expected} to {actual} before acknowledgement"
    )]
    LegacyParentChanged { expected: pid_t, actual: pid_t },
    #[error("could not open pidfd for legacy main-upgrade parent {pid}: {error}")]
    OpenLegacyParentPidfd { pid: pid_t, error: IoError },
    #[error("could not inspect pending SIGTERM before legacy acknowledgement: {0}")]
    InspectLegacySigterm(IoError),
    #[error("legacy main upgrade interrupted by SIGTERM before parent exit")]
    LegacyUpgradeInterrupted,
}

/// Check protocol support before any live descriptor is made inheritable or
/// the current Hub is frozen. An older binary rejects this unknown subcommand.
pub fn probe_main_upgrade_candidate(executable_path: &str) -> Result<(), UpgradeError> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

    let mut child = Command::new(executable_path)
        .arg("upgrade-probe")
        .arg("--protocol")
        .arg(crate::command::upgrade::UPGRADE_PROTOCOL_V2.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(UpgradeError::ProbeSpawn)?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().map_err(UpgradeError::ProbeSpawn)? {
            return if status.success() {
                Ok(())
            } else {
                Err(UpgradeError::ProbeRejected(status))
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(UpgradeError::ProbeTimeout(PROBE_TIMEOUT));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// unix-forks the main process
///
/// - Parent: meant to disappear after the child confirms it's alive
/// - Child: calls Sōzu's executable path with `sozu main [...]`
///
/// returns the pid of the new main, and a channel to get confirmation from the new main
pub fn fork_main_into_new_main(
    executable_path: String,
    upgrade_data: UpgradeData,
) -> Result<(pid_t, Channel<UpgradeHandshake, UpgradeHandshake>), UpgradeError> {
    trace!("parent({})", std::process::id());

    let mut upgrade_file = tempfile().map_err(UpgradeError::CreateUpgradeFile)?;

    util::disable_close_on_exec(upgrade_file.as_raw_fd()).map_err(|util_err| {
        UpgradeError::DisableCloexec {
            fd_name: "upgrade-file".to_string(),
            util_err,
        }
    })?;

    // Snapshot the count of worker sessions being handed off BEFORE we move
    // `upgrade_data` into serialization, so we can reconcile it against what
    // the re-execed master will recover (round-trip below). Gated to debug:
    // the snapshots are read only inside the `#[cfg(debug_assertions)]` block.
    #[cfg(debug_assertions)]
    let workers_to_handoff = upgrade_data.snapshot().workers.len();
    #[cfg(debug_assertions)]
    let boot_generation_handoff = upgrade_data.snapshot().boot_generation;

    info!("Writing upgrade data to file");
    let upgrade_data_string =
        serde_json::to_string(&upgrade_data).map_err(UpgradeError::SerdeWriteError)?;

    // The serialized payload must round-trip: deserializing what we are about
    // to write back into `UpgradeData` and re-serializing must reproduce the
    // same DOCUMENT — a mismatch would mean a (de)serialization bug that would
    // silently corrupt the recovered master state on the other side of the
    // re-exec. This is the cheap, in-process half of the write->read
    // round-trip that `begin_new_main_process` completes after exec. (Guarded
    // both let + assert: the re-parse only exists for the check, so it is dead
    // code stripped in release.)
    //
    // Compared as parsed JSON, never byte-for-byte: `UpgradeData` owns a
    // `ConfigState`, whose `tcp_fronts`, `udp_fronts` and `certificates` are
    // `HashMap`s. Every `HashMap` instance gets its own `RandomState` keys, so
    // the re-parsed maps iterate — and serialize — in a different order than
    // the originals, and a byte comparison failed on a perfectly CORRECT
    // upgrade as soon as one of those maps held more than one entry (measured:
    // 18 of 20 rounds with four TCP frontends). `serde_json::Map` is
    // BTreeMap-backed here (no `preserve_order` feature in the lockfile), so
    // comparing `Value`s is key-order-insensitive while still catching every
    // value-level corruption the check exists for.
    #[cfg(debug_assertions)]
    {
        match serde_json::from_str::<UpgradeData>(&upgrade_data_string) {
            Ok(reparsed) => {
                debug_assert_eq!(
                    reparsed.snapshot().workers.len(),
                    workers_to_handoff,
                    "round-tripped upgrade data must preserve the worker-session count"
                );
                debug_assert_eq!(
                    reparsed.snapshot().boot_generation,
                    boot_generation_handoff,
                    "round-tripped upgrade data must preserve the boot generation"
                );
                let reserialized = serde_json::to_string(&reparsed)
                    .expect("re-serializing already-parsed upgrade data cannot fail");
                debug_assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&reserialized)
                        .expect("re-serialized upgrade data is valid JSON"),
                    serde_json::from_str::<serde_json::Value>(&upgrade_data_string)
                        .expect("the upgrade data we just serialized is valid JSON"),
                    "upgrade data must serialize->deserialize->serialize to the same document"
                );
            }
            Err(e) => debug_assert!(
                false,
                "upgrade data we just serialized must deserialize back: {e}"
            ),
        }
    }

    upgrade_file
        .write_all(upgrade_data_string.as_bytes())
        .map_err(UpgradeError::WriteFile)?;
    upgrade_file.rewind().map_err(UpgradeError::Rewind)?;

    let (old_to_new, new_to_old) = UnixStream::pair().map_err(UpgradeError::CreateUnixStream)?;

    // The descriptors the new binary inherits across the re-exec must be
    // distinct kernel objects — none handed off twice. Three are in play at
    // this site: the confirmation channel (`new_to_old`, passed as `--fd`),
    // the upgrade-state file (passed as `--upgrade-fd`), and the unix command
    // listener carried inside `UpgradeData::snapshot` (`command_socket_fd`). A collision
    // would mean a single fd is being used for two roles after exec — an
    // fd-bookkeeping bug in the handoff.
    debug_assert!(
        new_to_old.as_raw_fd() != upgrade_file.as_raw_fd(),
        "confirmation-channel fd must not alias the upgrade-state file fd"
    );
    debug_assert!(
        new_to_old.as_raw_fd() != upgrade_data.snapshot().command_socket_fd,
        "confirmation-channel fd must not alias the inherited command-socket fd"
    );
    debug_assert!(
        upgrade_file.as_raw_fd() != upgrade_data.snapshot().command_socket_fd,
        "upgrade-state file fd must not alias the inherited command-socket fd"
    );

    util::disable_close_on_exec(new_to_old.as_raw_fd()).map_err(|util_err| {
        UpgradeError::DisableCloexec {
            fd_name: "new-main-to-old-main-channel".to_string(),
            util_err,
        }
    })?;

    let mut fork_confirmation_channel: Channel<UpgradeHandshake, UpgradeHandshake> = Channel::new(
        old_to_new,
        upgrade_data.snapshot().config.command_buffer_size,
        upgrade_data.snapshot().config.max_command_buffer_size,
    );

    fork_confirmation_channel
        .blocking()
        .map_err(UpgradeError::BlockChannel)?;

    info!("launching new main");
    // SAFETY: `fork` is unsafe because the child must avoid touching
    // shared mutable state inherited from the parent. The child branch
    // below restricts itself to `exec` (via `Command::exec`), which
    // replaces the process image entirely — no inherited state matters
    // after that point.
    match unsafe { fork().map_err(UpgradeError::Fork)? } {
        ForkResult::Parent { child } => {
            // The parent branch of a successful `fork()` always observes the
            // child's pid, which is strictly positive (0 is the child branch;
            // a failure returns `Err` above). A non-positive value here would
            // mean we mis-classified the fork outcome and would report a bogus
            // new-main pid to the operator.
            debug_assert!(
                child.as_raw() > 0,
                "fork parent branch must observe a strictly positive new-main pid"
            );
            info!("new main launched, with pid {}", child);

            Ok((child.into(), fork_confirmation_channel))
        }
        ForkResult::Child => {
            trace!("child({}):\twill spawn a child", std::process::id());

            // #515 scope clarification: this is the operator-triggered
            // master hot-upgrade path. The whole point is to re-exec the
            // **new** on-disk binary the operator just installed at
            // `executable_path` — a path-based `Command::new(...)` is the
            // intended behaviour. The race-free fd-based exec path
            // (`get_executable_exec_path()`, used in `bin/src/worker.rs`
            // for worker auto-restart) deliberately does NOT apply here:
            // a worker that respawns mid-package-install must match the
            // running master's version (race-free), but the master that
            // hot-upgrades is by design switching to a different version.
            //
            // Operator-driven schema mismatch on `upgrade-main` (the new
            // binary's proto schema being incompatible with the running
            // master's `UpgradeData` payload written via
            // `serde_json::to_string` above) is a separate concern: the
            // newly-execed master will fail to parse the inherited state
            // and the upgrade aborts cleanly. Operators must verify
            // proto compatibility before swapping the on-disk binary.
            let res = Command::new(executable_path)
                .arg("main")
                .arg("--fd")
                .arg(new_to_old.as_raw_fd().to_string())
                .arg("--upgrade-fd")
                .arg(upgrade_file.as_raw_fd().to_string())
                .arg("--upgrade-protocol")
                .arg(crate::command::upgrade::UPGRADE_PROTOCOL_V2.to_string())
                .arg("--command-buffer-size")
                .arg(
                    upgrade_data
                        .snapshot()
                        .config
                        .command_buffer_size
                        .to_string(),
                )
                .arg("--max-command-buffer-size")
                .arg(
                    upgrade_data
                        .snapshot()
                        .config
                        .max_command_buffer_size
                        .to_string(),
                )
                .exec();

            error!("exec call failed: {:?}", res);
            unreachable!();
        }
    }
}

/// Called by the child of a main process fork.
/// Starts new main process with upgrade data, notifies the old main process.
/// Only called from the binary entry point (main.rs), not from the library.
#[allow(dead_code)]
pub fn begin_new_main_process(
    new_to_old_channel_fd: i32,
    upgrade_file_fd: i32,
    upgrade_protocol: Option<u16>,
    command_buffer_size: u64,
    max_command_buffer_size: u64,
) -> Result<(), UpgradeError> {
    match upgrade_protocol {
        Some(crate::command::upgrade::UPGRADE_PROTOCOL_V2) => begin_v2_main_process(
            new_to_old_channel_fd,
            upgrade_file_fd,
            command_buffer_size,
            max_command_buffer_size,
        ),
        None => begin_legacy_main_process(
            new_to_old_channel_fd,
            upgrade_file_fd,
            command_buffer_size,
            max_command_buffer_size,
            std::env::var_os("NOTIFY_SOCKET").is_some(),
        ),
        Some(protocol) => Err(UpgradeError::UnsupportedProtocol(protocol)),
    }
}

fn begin_v2_main_process(
    new_to_old_channel_fd: i32,
    upgrade_file_fd: i32,
    command_buffer_size: u64,
    max_command_buffer_size: u64,
) -> Result<(), UpgradeError> {
    // Both descriptors were handed to us across the re-exec by the old master
    // (`fork_main_into_new_main`), each derived from a live `as_raw_fd()`: they
    // are valid (>= 0) and reference two distinct kernel objects (the
    // confirmation channel vs. the upgrade-state file). Aliasing them would be
    // an fd-bookkeeping bug on the handoff side. (Stays a `debug_assert!`: a
    // genuinely bad descriptor surfaces as a returned channel / read error.)
    debug_assert!(
        new_to_old_channel_fd >= 0,
        "inherited new-main-to-old-main channel fd must be a valid descriptor"
    );
    debug_assert!(
        upgrade_file_fd >= 0,
        "inherited upgrade-state file fd must be a valid descriptor"
    );
    debug_assert!(
        new_to_old_channel_fd != upgrade_file_fd,
        "the confirmation channel and upgrade-state file must be two distinct fds"
    );

    let mut fork_confirmation_channel: Channel<UpgradeHandshake, UpgradeHandshake> = Channel::new(
        // SAFETY: `new_to_old_channel_fd` was just inherited from the
        // pre-exec parent process via the `--fd` CLI argument. It is a valid
        // open descriptor with no other owner inside this freshly-execed
        // process. Ownership transfers to the `UnixStream`, whose `Drop`
        // closes the descriptor.
        unsafe { UnixStream::from_raw_fd(new_to_old_channel_fd) },
        command_buffer_size,
        max_command_buffer_size,
    );

    // DISCUSS: should we propagate the error instead of printing it?
    if let Err(e) = fork_confirmation_channel.blocking() {
        error!("Could not block the fork confirmation channel: {}", e);
    }

    println!("reading upgrade data from file");

    // SAFETY: `upgrade_file_fd` was just inherited from the pre-exec parent
    // process via the `--upgrade-fd` CLI argument. It is a valid open
    // descriptor with no other owner inside this freshly-execed process.
    // Ownership transfers to the `File`, whose `Drop` closes the descriptor.
    let mut upgrade_file = unsafe { File::from_raw_fd(upgrade_file_fd) };
    let mut content = String::new();
    let _ = upgrade_file
        .read_to_string(&mut content)
        .map_err(UpgradeError::ReadFile)?;
    // An unlinked temporary file inherited without `FD_CLOEXEC`: close it once
    // read, or every worker this main forks inherits it.
    drop(upgrade_file);

    // This is the read side of the write->read round-trip started in
    // `fork_main_into_new_main`: the old master wrote a non-empty serialized
    // `UpgradeData` object. The recovered payload must therefore be non-empty
    // and start with the JSON object delimiter. An empty/truncated read means
    // the fd handoff or the file rewind was botched — a malformed payload
    // still surfaces as a returned `SerdeReadError` below, this only flags the
    // logic bug earlier and loudly.
    debug_assert!(
        !content.is_empty(),
        "recovered upgrade state must not be empty (write->read round-trip)"
    );
    debug_assert!(
        content.trim_start().starts_with('{'),
        "recovered upgrade state must be a serialized JSON object"
    );

    let upgrade_data: UpgradeData =
        serde_json::from_str(&content).map_err(UpgradeError::SerdeReadError)?;

    let config = upgrade_data.snapshot().config.clone();
    let upgrade_client_token = mio::Token(upgrade_data.snapshot().upgrade_client_token);
    let upgrade_counts = upgrade_data.counts();

    println!("Setting up logging");

    setup_logging_with_config(&config, "MAIN").map_err(UpgradeError::SetupLogging)?;
    util::setup_metrics(&config).map_err(UpgradeError::SetupMetrics)?;

    let mut paused_hub =
        CommandHub::prepare_from_upgrade_data(upgrade_data).map_err(UpgradeError::CreateHub)?;

    // Every fallible preparation runs here, before PREPARED, where a failure
    // still makes the old main roll back and keep serving. After COMMIT the
    // old main is irreversibly fenced and exits whatever this process does,
    // and its workers return from their event loops once their command
    // channels close: a failure there would stop the whole proxy. None of
    // these steps touches state shared with the old main. `FD_CLOEXEC` is a
    // flag of this process's descriptor table, the SIGTERM self-pipe is
    // private (and `exec` reset the old main's handler), and checking the pid
    // file neither creates nor rewrites it before the publish step below.
    paused_hub
        .enable_cloexec_after_upgrade()
        .map_err(UpgradeError::EnableCloexec)?;
    paused_hub
        .handle_sigterm()
        .map_err(UpgradeError::HandleSigterm)?;
    let pid_file = util::open_pid_file(&config).map_err(UpgradeError::WritePidFile)?;

    fork_confirmation_channel
        .write_message(&UpgradeHandshake::prepared(upgrade_counts))
        .map_err(|channel_err| UpgradeError::SendConfirmation {
            result: "prepared".to_string(),
            channel_err,
        })?;

    let handoff_timeout = Duration::from_secs(config.worker_timeout.max(1) as u64);
    match fork_confirmation_channel.read_message_blocking_timeout(Some(handoff_timeout)) {
        Ok(message)
            if message.stage() == UpgradeStage::Commit
                && message.protocol == u32::from(crate::command::upgrade::UPGRADE_PROTOCOL_V2) => {}
        Ok(message) => {
            error!("unexpected pre-commit main-upgrade message: {:?}", message);
            return Ok(());
        }
        Err(channel_err) => {
            return Err(UpgradeError::SendConfirmation {
                result: "waiting for commit".to_string(),
                channel_err,
            });
        }
    }

    // COMMIT. From here on this process is the only main left: it publishes
    // and runs. Activation is the one step that can still fail, because
    // restoring each worker's SCM socket changes the blocking mode of a file
    // description shared with the old main and so cannot happen earlier.
    // Every other failure below is logged and must not stop the loop.
    let mut command_hub = paused_hub.activate().map_err(UpgradeError::CreateHub)?;

    if let Some((path, file)) = pid_file
        && let Err(error) = util::publish_pid_file(&path, file)
    {
        error!(
            "could not publish the new main pid after the upgrade committed, keeping the proxy running: {}",
            error
        );
    }

    // #228: tell systemd that the new master pid takes over from the
    // pre-exec one (`Type=notify` + `NotifyAccess=main` are required
    // in the unit file for this to be honoured), then signal READY=1
    // for the post-exec master. The old master sent `RELOADING=1`
    // before forking, so systemd is in `reloading` state and will
    // honour MAINPID= even before READY=1 swaps the unit back to
    // active. No-op when `$NOTIFY_SOCKET` is unset.
    let new_pid = std::process::id();
    if let Err(e) = sd_notify::main_pid(new_pid) {
        warn!("could not notify systemd MAINPID={}: {}", new_pid, e);
    }
    match sd_notify::notify(sd_notify::STATE_READY) {
        Ok(true) => debug!(
            "notified systemd post-upgrade: MAINPID={}, READY=1",
            new_pid
        ),
        Ok(false) => {}
        Err(e) => warn!("could not notify systemd READY=1: {}", e),
    }

    if let Err(error) = command_hub.complete_main_upgrade(upgrade_client_token, new_pid) {
        error!(
            "could not answer the client that started the main upgrade, keeping the proxy running: {}",
            error
        );
    }

    // The old main stops whether or not this acknowledgement reaches it.
    if let Err(error) =
        fork_confirmation_channel.write_message(&UpgradeHandshake::new(UpgradeStage::Activated))
    {
        error!(
            "could not acknowledge activation to the old main, keeping the proxy running: {}",
            error
        );
    }
    // The handshake channel was inherited without `FD_CLOEXEC`: close it, or
    // every worker this main forks inherits it.
    drop(fork_confirmation_channel);

    info!("starting new main loop");

    command_hub.run();

    // The new master's `command_hub.run()` exits on graceful shutdown
    // (the upgrade-handoff path is exclusive to the OLD master that
    // forked us); STOPPING=1 here is unambiguous.
    if let Err(e) = sd_notify::notify(sd_notify::STATE_STOPPING) {
        warn!("could not notify systemd STOPPING=1: {}", e);
    }

    info!("main process stopped");
    Ok(())
}

fn begin_legacy_main_process(
    new_to_old_channel_fd: RawFd,
    upgrade_file_fd: RawFd,
    command_buffer_size: u64,
    max_command_buffer_size: u64,
    notify_socket_present: bool,
) -> Result<(), UpgradeError> {
    validate_inherited_fd("legacy confirmation channel", new_to_old_channel_fd)?;
    // SAFETY: the descriptor was inherited from the old main and validated
    // above. Ownership transfers to this channel in the freshly execed child.
    let mut confirmation_channel: Channel<bool, ()> = Channel::new(
        unsafe { UnixStream::from_raw_fd(new_to_old_channel_fd) },
        command_buffer_size,
        max_command_buffer_size,
    );
    confirmation_channel
        .blocking()
        .map_err(UpgradeError::BlockChannel)?;

    if notify_socket_present {
        send_legacy_confirmation(&mut confirmation_channel, false, "rejection")?;
        return Err(UpgradeError::LegacyUpgradeUnderSystemd);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = upgrade_file_fd;
        send_legacy_confirmation(&mut confirmation_channel, false, "rejection")?;
        return Err(UpgradeError::LegacyUpgradeUnsupportedPlatform);
    }

    #[cfg(target_os = "linux")]
    begin_legacy_linux_main_process(confirmation_channel, upgrade_file_fd)
}

fn validate_inherited_fd(name: &'static str, fd: RawFd) -> Result<(), UpgradeError> {
    if fd < 0 {
        return Err(UpgradeError::InvalidInheritedDescriptor { name, fd });
    }
    // SAFETY: F_GETFD only observes the descriptor table entry.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(UpgradeError::InvalidInheritedDescriptor { name, fd });
    }
    Ok(())
}

fn send_legacy_confirmation(
    channel: &mut Channel<bool, ()>,
    accepted: bool,
    result: &'static str,
) -> Result<(), UpgradeError> {
    channel
        .write_message(&accepted)
        .map_err(|channel_err| UpgradeError::SendConfirmation {
            result: result.to_owned(),
            channel_err,
        })
}

#[cfg(target_os = "linux")]
fn begin_legacy_linux_main_process(
    mut confirmation_channel: Channel<bool, ()>,
    upgrade_file_fd: RawFd,
) -> Result<(), UpgradeError> {
    if confirmation_channel.fd() == upgrade_file_fd {
        send_legacy_confirmation(&mut confirmation_channel, false, "rejection")?;
        return Err(UpgradeError::InvalidInheritedDescriptor {
            name: "legacy upgrade state",
            fd: upgrade_file_fd,
        });
    }
    if let Err(error) = validate_inherited_fd("legacy upgrade state", upgrade_file_fd) {
        send_legacy_confirmation(&mut confirmation_channel, false, "rejection")?;
        return Err(error);
    }

    // The old 2.2.1 main is blocked waiting for our boolean, so it cannot exit
    // normally during this identity check. A parent change means it died for
    // another reason; do not accidentally wait on the reaper instead.
    let parent_pid = unsafe { libc::getppid() };
    if parent_pid <= 1 {
        send_legacy_confirmation(&mut confirmation_channel, false, "rejection")?;
        return Err(UpgradeError::InvalidLegacyParent(parent_pid));
    }
    let parent_pidfd = open_pidfd(parent_pid)?;
    let observed_parent = unsafe { libc::getppid() };
    if observed_parent != parent_pid {
        send_legacy_confirmation(&mut confirmation_channel, false, "rejection")?;
        return Err(UpgradeError::LegacyParentChanged {
            expected: parent_pid,
            actual: observed_parent,
        });
    }

    println!("reading legacy upgrade data from file");
    // SAFETY: ownership of the validated inherited descriptor transfers to
    // this File in the freshly execed child.
    let mut upgrade_file = unsafe { File::from_raw_fd(upgrade_file_fd) };
    let mut content = String::new();
    upgrade_file
        .read_to_string(&mut content)
        .map_err(UpgradeError::ReadFile)?;
    drop(upgrade_file);
    debug_assert!(
        !content.is_empty(),
        "legacy upgrade state must not be empty"
    );
    debug_assert!(
        content.trim_start().starts_with('{'),
        "legacy upgrade state must be a JSON object"
    );

    let upgrade_data: LegacyUpgradeData =
        serde_json::from_str(&content).map_err(UpgradeError::SerdeReadError)?;
    let config = upgrade_data.config.clone();

    println!("Setting up logging");
    setup_logging_with_config(&config, "MAIN").map_err(UpgradeError::SetupLogging)?;
    util::setup_metrics(&config).map_err(UpgradeError::SetupMetrics)?;

    let mut paused_hub = CommandHub::prepare_from_legacy_upgrade_data(upgrade_data)
        .map_err(UpgradeError::CreateHub)?;
    paused_hub
        .enable_cloexec_after_upgrade()
        .map_err(UpgradeError::EnableCloexec)?;
    paused_hub
        .handle_sigterm()
        .map_err(UpgradeError::HandleSigterm)?;
    let pid_file = util::open_pid_file(&config).map_err(UpgradeError::WritePidFile)?;

    match paused_hub.sigterm_pending() {
        Ok(false) => {}
        Ok(true) => {
            send_legacy_confirmation(&mut confirmation_channel, false, "rejection")?;
            return Err(UpgradeError::LegacyUpgradeInterrupted);
        }
        Err(error) => {
            send_legacy_confirmation(&mut confirmation_channel, false, "rejection")?;
            return Err(UpgradeError::InspectLegacySigterm(error));
        }
    }

    send_legacy_confirmation(&mut confirmation_channel, true, "success")?;
    drop(confirmation_channel);

    let handoff_timeout = Duration::from_secs(config.worker_timeout.max(1) as u64);
    wait_for_parent_exit(parent_pidfd.as_raw_fd(), parent_pid, handoff_timeout);
    drop(parent_pidfd);

    let mut command_hub = paused_hub.activate().map_err(UpgradeError::CreateHub)?;
    if let Some((path, file)) = pid_file
        && let Err(error) = util::publish_pid_file(&path, file)
    {
        error!(
            "could not publish the new main pid after the legacy parent exited, keeping the proxy running: {}",
            error
        );
    }

    info!(
        "legacy main-upgrade parent {} exited; starting replacement main loop",
        parent_pid
    );
    command_hub.run();
    info!("main process stopped");
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: pid_t) -> Result<OwnedFd, UpgradeError> {
    // SAFETY: pidfd_open takes an integer pid and zero flags and returns a new
    // descriptor owned by the caller on success.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(UpgradeError::OpenLegacyParentPidfd {
            pid,
            error: IoError::last_os_error(),
        });
    }
    // SAFETY: the successful syscall returned a fresh owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

#[cfg(target_os = "linux")]
fn wait_for_parent_exit(pidfd: RawFd, parent_pid: pid_t, diagnostic_timeout: Duration) {
    let mut descriptor = libc::pollfd {
        fd: pidfd,
        events: libc::POLLIN,
        revents: 0,
    };
    let mut timeout_ms = i32::try_from(diagnostic_timeout.as_millis())
        .unwrap_or(i32::MAX)
        .max(1);
    let mut timeout_reported = false;

    loop {
        descriptor.revents = 0;
        // SAFETY: `descriptor` is one initialized pollfd and remains alive and
        // exclusively borrowed for the syscall.
        let result = unsafe { libc::poll(&raw mut descriptor, 1, timeout_ms) };
        if result < 0 {
            let error = IoError::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            error!(
                "could not poll legacy main-upgrade parent {}: {}; replacement remains paused",
                parent_pid, error
            );
            thread::sleep(Duration::from_secs(1));
            timeout_ms = -1;
            continue;
        }
        if result == 0 {
            debug_assert!(!timeout_reported, "the diagnostic timeout fires only once");
            warn!(
                "legacy main-upgrade parent {} did not exit within {:?}; replacement remains paused and will keep waiting",
                parent_pid, diagnostic_timeout
            );
            timeout_reported = true;
            timeout_ms = -1;
            continue;
        }

        if descriptor.revents & libc::POLLIN != 0 {
            return;
        }
        if descriptor.revents != 0 {
            error!(
                "legacy main-upgrade parent {} pidfd returned events {:#x} without POLLIN; replacement remains paused",
                parent_pid, descriptor.revents
            );
            thread::sleep(Duration::from_secs(1));
            timeout_ms = -1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::fd::{AsRawFd, IntoRawFd},
        os::unix::net::UnixStream as StdUnixStream,
        sync::mpsc,
        thread,
        time::Duration,
    };

    #[cfg(target_os = "linux")]
    use nix::sys::wait::waitpid;
    #[cfg(target_os = "linux")]
    use nix::unistd::ForkResult;

    use sozu_command_lib::channel::Channel;

    use super::{UpgradeError, begin_legacy_main_process, begin_new_main_process};
    #[cfg(target_os = "linux")]
    use super::{open_pidfd, wait_for_parent_exit};

    #[test]
    fn explicit_legacy_or_unknown_protocol_is_rejected_before_fd_use() {
        for protocol in [1, 3, u16::MAX] {
            let error = begin_new_main_process(-1, -1, Some(protocol), 64, 512)
                .expect_err("an explicit non-v2 protocol must be rejected");
            assert!(
                matches!(error, UpgradeError::UnsupportedProtocol(actual) if actual == protocol)
            );
        }
    }

    #[test]
    fn legacy_upgrade_under_systemd_sends_false_before_reading_state_fd() {
        let (candidate_channel, mut old_main_channel): (Channel<bool, ()>, Channel<(), bool>) =
            Channel::generate_nonblocking(64, 512).expect("could not create legacy handshake");
        let candidate_fd = candidate_channel.sock.into_raw_fd();
        old_main_channel
            .blocking()
            .expect("could not block old-main side of handshake");

        let error = begin_legacy_main_process(candidate_fd, -1, 64, 512, true)
            .expect_err("legacy upgrades under systemd must fail closed");
        assert!(
            matches!(error, UpgradeError::LegacyUpgradeUnderSystemd),
            "unexpected refusal error: {error:?}"
        );
        assert!(
            !old_main_channel
                .read_message()
                .expect("the old main must receive its negative acknowledgement")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_barrier_survives_diagnostic_timeout_until_process_exit() {
        let (release_child, child_gate) =
            StdUnixStream::pair().expect("could not create child gate");
        // SAFETY: the child performs only async-signal-safe descriptor I/O and
        // `_exit`; the parent retains all Rust test-harness state.
        match unsafe { nix::unistd::fork().expect("could not fork barrier child") } {
            ForkResult::Child => {
                drop(release_child);
                let mut byte = 0_u8;
                // SAFETY: `child_gate` is live and `byte` is a valid one-byte
                // output buffer. The child blocks until the parent releases it.
                let read = unsafe { libc::read(child_gate.as_raw_fd(), (&raw mut byte).cast(), 1) };
                unsafe { libc::_exit(i32::from(read != 1)) }
            }
            ForkResult::Parent { child } => {
                drop(child_gate);
                let pidfd = open_pidfd(child.as_raw()).expect("could not open child pidfd");
                let (finished_tx, finished_rx) = mpsc::channel();
                thread::spawn(move || {
                    wait_for_parent_exit(
                        pidfd.as_raw_fd(),
                        child.as_raw(),
                        Duration::from_millis(5),
                    );
                    finished_tx
                        .send(())
                        .expect("could not report barrier completion");
                });

                let before_exit = finished_rx.recv_timeout(Duration::from_millis(50));
                let byte = 1_u8;
                // SAFETY: the parent owns the live gate endpoint and `byte`
                // remains valid for the one-byte write.
                let written =
                    unsafe { libc::write(release_child.as_raw_fd(), (&raw const byte).cast(), 1) };
                assert_eq!(written, 1, "could not release barrier child");
                let after_exit = finished_rx.recv_timeout(Duration::from_secs(1));
                waitpid(child, None).expect("could not reap barrier child");

                assert!(
                    before_exit.is_err(),
                    "the diagnostic timeout must not open the ownership barrier"
                );
                assert!(
                    after_exit.is_ok(),
                    "the pidfd becoming readable must open the ownership barrier"
                );
            }
        }
    }
}
