//! Master/worker hot-upgrade orchestration.
//!
//! Forks a child master, preserves listener and command-channel FDs across
//! exec, restores their userspace state, and tears down the previous master
//! once the new one acks. Keeps the data plane uninterrupted by handing off
//! accepted listeners and existing worker FDs.

use std::{
    fs::File,
    io::{Error as IoError, ErrorKind, Read, Write},
    os::{
        fd::{OwnedFd, RawFd},
        unix::{
            io::{AsRawFd, FromRawFd},
            net::UnixStream as StdUnixStream,
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
    secret::{PROTECTED_TRANSFER_MAX_BYTES, ProtectedTransfer, TransferError, Zeroize, Zeroizing},
};

use crate::{
    command::{
        server::{CommandHub, HubError, ServerError},
        upgrade::{
            LegacyUpgradeData, UPGRADE_PROTOCOL_V2, UPGRADE_PROTOCOL_V3, UpgradeData,
            UpgradeHandshake, UpgradeStage,
        },
    },
    util::{self, UtilError},
};

#[derive(thiserror::Error, Debug)]
pub enum UpgradeError {
    #[error("could not disable cloexec on {fd_name}'s file descriptor: {util_err}")]
    DisableCloexec {
        fd_name: String,
        util_err: UtilError,
    },
    #[error("could not create MIO pair of unix stream: {0}")]
    CreateUnixStream(IoError),
    #[error("could not read upgrade data from file: {0}")]
    ReadFile(IoError),
    #[error("could not read upgrade data to temporary file: {0}")]
    SerdeReadError(SerdeError),
    #[error("could not encode the upgrade snapshot: {0}")]
    EncodeSnapshot(SerdeError),
    #[error("the upgrade snapshot changed while it was being encoded")]
    SnapshotChanged,
    #[error("main-upgrade transfer: unrecognized transfer header ({0})")]
    TransferHeader(&'static str),
    #[error("main-upgrade transfer: the {0} exceeds the maximum")]
    TransferTooLarge(&'static str),
    #[error("main-upgrade transfer: truncated payload")]
    TransferTruncated,
    #[error("main-upgrade transfer: trailing bytes after the payload")]
    TransferTrailing,
    #[error("main-upgrade transfer: handoff deadline exceeded")]
    TransferDeadline,
    #[error("main-upgrade transfer: socket failure: {0}")]
    TransferIo(IoError),
    #[error(
        "main-upgrade transfer: malformed upgrade snapshot ({category:?} at line {line} column {column})"
    )]
    MalformedSnapshot {
        category: serde_json::error::Category,
        line: usize,
        column: usize,
    },
    #[error("main-upgrade transfer: protected state refused: {0}")]
    TransferProtected(TransferError),
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
    #[error("main-upgrade candidate rejected protocol v3 with status {0}")]
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
/// the current Hub is frozen. An older binary rejects this unknown value, or
/// the unknown subcommand.
pub fn probe_main_upgrade_candidate(executable_path: &str) -> Result<(), UpgradeError> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

    let mut child = Command::new(executable_path)
        .arg("upgrade-probe")
        .arg("--protocol")
        .arg(UPGRADE_PROTOCOL_V3.to_string())
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

// ---- Protocol 3 transfer frame -------------------------------------------
//
// One bounded frame on a dedicated Unix socket, all integers big-endian:
//
//   magic            8 bytes  "SOZUUPG3"
//   frame version    u32      1
//   handoff budget   u64      milliseconds left of the old main's deadline
//   snapshot length  u64      JSON `UpgradeData`, at most TRANSFER_MAX_SNAPSHOT_BYTES
//   protected length u64      `ProtectedTransfer`, at most PROTECTED_TRANSFER_MAX_BYTES
//   snapshot bytes, protected bytes, then end-of-file
//
// The sender half-closes after the payload, so the receiver tells the end of
// the frame from trailing bytes. Every read and write is bounded by one
// absolute deadline that nothing renews on progress.

const TRANSFER_MAGIC: &[u8; 8] = b"SOZUUPG3";
const TRANSFER_FRAME_VERSION: u32 = 1;
const TRANSFER_HEADER_LEN: usize = 8 + 4 + 8 + 8 + 8;
/// Hard maximum of the JSON snapshot of a frame.
pub(crate) const TRANSFER_MAX_SNAPSHOT_BYTES: usize = 1 << 30;
/// Longest handoff budget a frame may carry.
const TRANSFER_MAX_BUDGET: Duration = Duration::from_secs(24 * 60 * 60);
/// How long the receiver waits for the fixed-size header, which the sender
/// writes right after the fork.
const TRANSFER_HEADER_WAIT: Duration = Duration::from_secs(5);
/// Bytes handed to the kernel per call, so the deadline is re-read often.
const TRANSFER_CHUNK: usize = 64 * 1024;

/// Counts the bytes a serialization would produce, retaining none.
struct CountingWriter(usize);

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| IoError::from(ErrorKind::OutOfMemory))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Fills a zeroing buffer without ever growing it.
struct ExactWriter<'a>(&'a mut Zeroizing<Vec<u8>>);

impl Write for ExactWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.capacity() - self.0.len() < bytes.len() {
            return Err(IoError::from(ErrorKind::WriteZero));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The two encoded sections of a protocol-3 frame. Both live in zeroing
/// buffers allocated once at their exact size, and are wiped when sent or
/// dropped.
pub struct UpgradePayload {
    snapshot: Zeroizing<Vec<u8>>,
    protected: Zeroizing<Vec<u8>>,
}

impl UpgradePayload {
    /// Count the snapshot's JSON first, allocate exactly that, then serialize
    /// straight into it: nothing is reallocated, so no unwiped copy is left.
    pub fn encode(
        upgrade_data: &UpgradeData,
        protected: &ProtectedTransfer,
    ) -> Result<Self, UpgradeError> {
        let mut counter = CountingWriter(0);
        serde_json::to_writer(&mut counter, upgrade_data).map_err(UpgradeError::EncodeSnapshot)?;
        if counter.0 > TRANSFER_MAX_SNAPSHOT_BYTES {
            return Err(UpgradeError::TransferTooLarge("snapshot"));
        }
        let mut snapshot = Zeroizing::new(Vec::with_capacity(counter.0));
        let (pointer, capacity) = (snapshot.as_ptr(), snapshot.capacity());
        serde_json::to_writer(ExactWriter(&mut snapshot), upgrade_data)
            .map_err(UpgradeError::EncodeSnapshot)?;
        if snapshot.len() != counter.0
            || snapshot.as_ptr() != pointer
            || snapshot.capacity() != capacity
        {
            return Err(UpgradeError::SnapshotChanged);
        }

        let protected_len = protected
            .encoded_len()
            .map_err(UpgradeError::TransferProtected)?;
        let mut encoded = Zeroizing::new(Vec::with_capacity(protected_len));
        protected
            .encode_into(&mut encoded)
            .map_err(UpgradeError::TransferProtected)?;
        Ok(Self {
            snapshot,
            protected: encoded,
        })
    }
}

/// Time left before `deadline`, or the deadline error.
fn time_before(deadline: Instant) -> Result<Duration, UpgradeError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or(UpgradeError::TransferDeadline)
}

fn write_before(
    socket: &StdUnixStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<(), UpgradeError> {
    while !bytes.is_empty() {
        socket
            .set_write_timeout(Some(time_before(deadline)?))
            .map_err(UpgradeError::TransferIo)?;
        let chunk = &bytes[..bytes.len().min(TRANSFER_CHUNK)];
        match (&*socket).write(chunk) {
            Ok(0) => return Err(UpgradeError::TransferIo(ErrorKind::WriteZero.into())),
            Ok(written) => bytes = &bytes[written..],
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(UpgradeError::TransferIo(error)),
        }
    }
    Ok(())
}

fn read_exact_before(
    socket: &StdUnixStream,
    mut buffer: &mut [u8],
    deadline: Instant,
) -> Result<(), UpgradeError> {
    while !buffer.is_empty() {
        socket
            .set_read_timeout(Some(time_before(deadline)?))
            .map_err(UpgradeError::TransferIo)?;
        let chunk_len = buffer.len().min(TRANSFER_CHUNK);
        match (&*socket).read(&mut buffer[..chunk_len]) {
            Ok(0) => return Err(UpgradeError::TransferTruncated),
            Ok(read) => buffer = &mut buffer[read..],
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(UpgradeError::TransferIo(error)),
        }
    }
    Ok(())
}

/// Write the frame to the candidate, which was forked first, and half-close.
/// The payload is wiped whatever the outcome.
pub fn send_upgrade_transfer(
    socket: &mut StdUnixStream,
    payload: &mut UpgradePayload,
    deadline: Instant,
) -> Result<(), UpgradeError> {
    let result = write_frame(socket, payload, deadline);
    // Wipe at once: both buffers hold secret-bearing bytes.
    let UpgradePayload {
        snapshot,
        protected,
    } = payload;
    zeroize_in_place(snapshot);
    zeroize_in_place(protected);
    result
}

fn zeroize_in_place(buffer: &mut Zeroizing<Vec<u8>>) {
    buffer.zeroize();
}

fn write_frame(
    socket: &StdUnixStream,
    payload: &UpgradePayload,
    deadline: Instant,
) -> Result<(), UpgradeError> {
    let budget = time_before(deadline)?;
    let budget_millis = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX).max(1);
    let length =
        |len: usize| u64::try_from(len).map_err(|_| UpgradeError::TransferTooLarge("frame"));
    let mut header = Vec::with_capacity(TRANSFER_HEADER_LEN);
    header.extend_from_slice(TRANSFER_MAGIC);
    header.extend_from_slice(&TRANSFER_FRAME_VERSION.to_be_bytes());
    header.extend_from_slice(&budget_millis.to_be_bytes());
    header.extend_from_slice(&length(payload.snapshot.len())?.to_be_bytes());
    header.extend_from_slice(&length(payload.protected.len())?.to_be_bytes());
    write_before(socket, &header, deadline)?;
    write_before(socket, &payload.snapshot, deadline)?;
    write_before(socket, &payload.protected, deadline)?;
    socket
        .shutdown(std::net::Shutdown::Write)
        .map_err(UpgradeError::TransferIo)
}

/// What a candidate reads from a frame, and the absolute deadline it carries.
pub(crate) struct ReceivedTransfer {
    pub(crate) upgrade_data: UpgradeData,
    pub(crate) protected: ProtectedTransfer,
    pub(crate) deadline: Instant,
}

/// Read one frame. Everything is bounded and validated before PREPARED: the
/// header, both lengths, the exact payload, the end-of-file, then the content.
/// `started` is when this process began receiving.
pub(crate) fn receive_upgrade_transfer(
    socket: &StdUnixStream,
    started: Instant,
) -> Result<ReceivedTransfer, UpgradeError> {
    let mut header = [0u8; TRANSFER_HEADER_LEN];
    read_exact_before(socket, &mut header, started + TRANSFER_HEADER_WAIT)?;
    let (magic, rest) = header.split_at(8);
    if magic != TRANSFER_MAGIC {
        return Err(UpgradeError::TransferHeader("magic"));
    }
    let field = |at: usize| -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&rest[at..at + 8]);
        bytes
    };
    if rest[..4] != TRANSFER_FRAME_VERSION.to_be_bytes() {
        return Err(UpgradeError::TransferHeader("frame version"));
    }
    let budget = Duration::from_millis(u64::from_be_bytes(field(4)));
    if budget.is_zero() || budget > TRANSFER_MAX_BUDGET {
        return Err(UpgradeError::TransferHeader("budget"));
    }
    let bounded = |raw: u64, maximum: usize, what: &'static str| {
        usize::try_from(raw)
            .ok()
            .filter(|len| *len <= maximum)
            .ok_or(UpgradeError::TransferTooLarge(what))
    };
    let snapshot_len = bounded(
        u64::from_be_bytes(field(12)),
        TRANSFER_MAX_SNAPSHOT_BYTES,
        "snapshot",
    )?;
    let protected_len = bounded(
        u64::from_be_bytes(field(20)),
        PROTECTED_TRANSFER_MAX_BYTES,
        "protected state",
    )?;

    let deadline = started + budget;
    let mut snapshot = Zeroizing::new(vec![0u8; snapshot_len]);
    read_exact_before(socket, &mut snapshot, deadline)?;
    let mut protected_bytes = Zeroizing::new(vec![0u8; protected_len]);
    read_exact_before(socket, &mut protected_bytes, deadline)?;

    let mut extra = Zeroizing::new([0u8; 1]);
    loop {
        socket
            .set_read_timeout(Some(time_before(deadline)?))
            .map_err(UpgradeError::TransferIo)?;
        match (&*socket).read(extra.as_mut_slice()) {
            Ok(0) => break,
            Ok(_) => return Err(UpgradeError::TransferTrailing),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(UpgradeError::TransferIo(error)),
        }
    }

    // The snapshot is never echoed in an error: only its category and place.
    let upgrade_data: UpgradeData =
        serde_json::from_slice(&snapshot).map_err(|error| UpgradeError::MalformedSnapshot {
            category: error.classify(),
            line: error.line(),
            column: error.column(),
        })?;
    let protected =
        ProtectedTransfer::decode(&protected_bytes).map_err(UpgradeError::TransferProtected)?;
    Ok(ReceivedTransfer {
        upgrade_data,
        protected,
        deadline,
    })
}

/// The replacement main as the old main sees it right after the fork.
pub struct NewMain {
    pub pid: pid_t,
    /// PREPARED / COMMIT / ACTIVATED travel here, never the snapshot.
    pub confirmation: Channel<UpgradeHandshake, UpgradeHandshake>,
    /// The old main's end of the transfer socket.
    pub transfer: StdUnixStream,
}

/// unix-forks the main process
///
/// - Parent: meant to disappear after the child confirms it's alive
/// - Child: calls Sōzu's executable path with `sozu main [...]`
///
/// The candidate is forked and exec'd first; the snapshot is sent afterwards
/// with [`send_upgrade_transfer`] over `transfer`, a socket pair of its own
/// whose other end is the candidate's `--upgrade-fd`. Every endpoint this
/// function created and does not return is closed on every path.
pub fn fork_main_into_new_main(
    executable_path: String,
    upgrade_data: &UpgradeData,
) -> Result<NewMain, UpgradeError> {
    trace!("parent({})", std::process::id());

    let (transfer, candidate_transfer) =
        StdUnixStream::pair().map_err(UpgradeError::CreateUnixStream)?;
    let (old_to_new, new_to_old) = UnixStream::pair().map_err(UpgradeError::CreateUnixStream)?;

    // The descriptors the new binary inherits across the re-exec must be
    // distinct kernel objects — none handed off twice. Three are in play at
    // this site: the confirmation channel (`new_to_old`, passed as `--fd`),
    // the transfer socket (passed as `--upgrade-fd`), and the unix command
    // listener carried inside `UpgradeData::snapshot` (`command_socket_fd`). A collision
    // would mean a single fd is being used for two roles after exec — an
    // fd-bookkeeping bug in the handoff.
    debug_assert!(
        new_to_old.as_raw_fd() != candidate_transfer.as_raw_fd(),
        "confirmation-channel fd must not alias the transfer socket fd"
    );
    debug_assert!(
        new_to_old.as_raw_fd() != upgrade_data.snapshot().command_socket_fd,
        "confirmation-channel fd must not alias the inherited command-socket fd"
    );
    debug_assert!(
        candidate_transfer.as_raw_fd() != upgrade_data.snapshot().command_socket_fd,
        "transfer socket fd must not alias the inherited command-socket fd"
    );

    util::disable_close_on_exec(candidate_transfer.as_raw_fd()).map_err(|util_err| {
        UpgradeError::DisableCloexec {
            fd_name: "upgrade-transfer-socket".to_string(),
            util_err,
        }
    })?;
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

            // `candidate_transfer` and `new_to_old` close here: the candidate
            // owns its copies, and neither may leak into a later fork.
            Ok(NewMain {
                pid: child.into(),
                confirmation: fork_confirmation_channel,
                transfer,
            })
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
            // master's `UpgradeData` payload) is a separate concern: the
            // newly-execed master will fail to parse the received state
            // and the upgrade aborts cleanly. Operators must verify
            // proto compatibility before swapping the on-disk binary.
            let res = Command::new(executable_path)
                .arg("main")
                .arg("--fd")
                .arg(new_to_old.as_raw_fd().to_string())
                .arg("--upgrade-fd")
                .arg(candidate_transfer.as_raw_fd().to_string())
                .arg("--upgrade-protocol")
                .arg(UPGRADE_PROTOCOL_V3.to_string())
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
        Some(UPGRADE_PROTOCOL_V3) => begin_v3_main_process(
            new_to_old_channel_fd,
            upgrade_file_fd,
            command_buffer_size,
            max_command_buffer_size,
        ),
        Some(UPGRADE_PROTOCOL_V2) => begin_v2_main_process(
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

    prepare_and_activate_main(
        fork_confirmation_channel,
        upgrade_data,
        None,
        UPGRADE_PROTOCOL_V2,
        None,
    )
}

/// Receive a protocol-3 hand-over: one bounded frame from a Unix socket, never
/// a file, carrying the snapshot and the confirmed protected state.
fn begin_v3_main_process(
    new_to_old_channel_fd: i32,
    transfer_fd: i32,
    command_buffer_size: u64,
    max_command_buffer_size: u64,
) -> Result<(), UpgradeError> {
    let started = Instant::now();
    validate_inherited_fd("upgrade confirmation channel", new_to_old_channel_fd)?;
    validate_inherited_fd("upgrade transfer socket", transfer_fd)?;
    if new_to_old_channel_fd == transfer_fd {
        return Err(UpgradeError::InvalidInheritedDescriptor {
            name: "upgrade transfer socket",
            fd: transfer_fd,
        });
    }
    // SAFETY: both descriptors were inherited from the old main, validated
    // above and are distinct; each has no other owner in this fresh process.
    let mut fork_confirmation_channel: Channel<UpgradeHandshake, UpgradeHandshake> = Channel::new(
        unsafe { UnixStream::from_raw_fd(new_to_old_channel_fd) },
        command_buffer_size,
        max_command_buffer_size,
    );
    // SAFETY: as above; ownership moves to the stream, which closes the socket
    // once the frame is read, so no later fork inherits it.
    let transfer = unsafe { StdUnixStream::from_raw_fd(transfer_fd) };
    if let Err(e) = fork_confirmation_channel.blocking() {
        error!("Could not block the fork confirmation channel: {}", e);
    }

    println!("reading upgrade data from the transfer socket");
    let received = receive_upgrade_transfer(&transfer, started)?;
    drop(transfer);

    prepare_and_activate_main(
        fork_confirmation_channel,
        received.upgrade_data,
        Some(received.protected),
        UPGRADE_PROTOCOL_V3,
        Some(received.deadline),
    )
}

/// Everything after the snapshot is read: prepare the replacement Hub, answer
/// PREPARED, wait for COMMIT, activate. `deadline` is the absolute handoff
/// deadline of protocol 3; protocol 2 waits one `worker_timeout` for COMMIT.
fn prepare_and_activate_main(
    mut fork_confirmation_channel: Channel<UpgradeHandshake, UpgradeHandshake>,
    upgrade_data: UpgradeData,
    protected: Option<ProtectedTransfer>,
    protocol: u16,
    deadline: Option<Instant>,
) -> Result<(), UpgradeError> {
    let config = upgrade_data.snapshot().config.clone();
    let upgrade_client_token = mio::Token(upgrade_data.snapshot().upgrade_client_token);
    let upgrade_counts = upgrade_data.counts();

    println!("Setting up logging");

    setup_logging_with_config(&config, "MAIN").map_err(UpgradeError::SetupLogging)?;
    util::setup_metrics(&config).map_err(UpgradeError::SetupMetrics)?;

    let mut paused_hub = CommandHub::prepare_from_upgrade_data(upgrade_data, protected)
        .map_err(UpgradeError::CreateHub)?;

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
        .write_message(&UpgradeHandshake::prepared(upgrade_counts, protocol))
        .map_err(|channel_err| UpgradeError::SendConfirmation {
            result: "prepared".to_string(),
            channel_err,
        })?;

    let handoff_timeout = match deadline {
        None => Duration::from_secs(config.worker_timeout.max(1) as u64),
        Some(deadline) => deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(UpgradeError::TransferDeadline)?,
    };
    match fork_confirmation_channel.read_message_blocking_timeout(Some(handoff_timeout)) {
        Ok(message)
            if message.stage() == UpgradeStage::Commit
                && message.protocol == u32::from(protocol) => {}
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
    if let Err(error) = fork_confirmation_channel
        .write_message(&UpgradeHandshake::new(UpgradeStage::Activated, protocol))
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
        collections::HashMap,
        io::{Read, Write},
        os::fd::{AsRawFd, IntoRawFd},
        os::unix::net::UnixStream as StdUnixStream,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    #[cfg(target_os = "linux")]
    use nix::sys::wait::waitpid;
    #[cfg(target_os = "linux")]
    use nix::unistd::ForkResult;

    use sozu_command_lib::channel::Channel;

    use super::{
        TRANSFER_FRAME_VERSION, TRANSFER_MAGIC, UpgradeError, UpgradePayload,
        begin_legacy_main_process, begin_new_main_process, receive_upgrade_transfer,
        send_upgrade_transfer,
    };
    #[cfg(target_os = "linux")]
    use super::{open_pidfd, wait_for_parent_exit};
    use crate::command::upgrade::{UpgradeHandshake, tests::sample_upgrade_data};
    use sozu_command_lib::{
        proto::command::{HealthCheckHeader, HealthCheckHeaderValue},
        secret::{
            FingerprintKey, PROTECTED_TRANSFER_MAX_BYTES, ProtectedHeader, ProtectedHeaderChange,
            ProtectedHeaderJournal, ProtectedHeaderSet, ProtectedTransfer, SecretValue,
            TransferError, Zeroizing,
        },
    };

    #[test]
    fn explicit_legacy_or_unknown_protocol_is_rejected_before_fd_use() {
        for protocol in [1, 4, u16::MAX] {
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

    /// A candidate that answers `upgrade-probe --protocol <accepted>` with
    /// success and everything else with failure, recording what it was asked.
    fn probe_candidate(directory: &std::path::Path, accepted: u16) -> (String, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let log = directory.join(format!("asked-{accepted}.log"));
        let script = directory.join(format!("candidate-{accepted}.sh"));
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$1 $2 $3\" >> {}\n[ \"$1\" = upgrade-probe ] && [ \"$3\" = {accepted} ]\n",
                log.display()
            ),
        )
        .expect("write the probe candidate");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make the probe candidate executable");
        (script.to_string_lossy().into_owned(), log)
    }

    #[test]
    fn the_probe_requests_protocol_three_and_accepts_a_candidate_that_supports_it() {
        let directory = tempfile::tempdir().expect("probe directory");
        let (candidate, log) = probe_candidate(directory.path(), 3);

        super::probe_main_upgrade_candidate(&candidate)
            .expect("a candidate that supports protocol 3 must be accepted");
        assert_eq!(
            std::fs::read_to_string(log).expect("candidate log"),
            "upgrade-probe --protocol 3\n"
        );
    }

    #[test]
    fn the_probe_rejects_a_candidate_that_only_supports_protocol_two() {
        let directory = tempfile::tempdir().expect("probe directory");
        let (candidate, log) = probe_candidate(directory.path(), 2);

        let error = super::probe_main_upgrade_candidate(&candidate)
            .expect_err("a protocol-2-only candidate must not receive a V3 handoff");
        assert!(matches!(error, UpgradeError::ProbeRejected(_)), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(log).expect("candidate log"),
            "upgrade-probe --protocol 3\n",
            "the parent must ask for protocol 3 and never fall back to 2"
        );
    }

    #[test]
    fn protocol_three_reads_the_socket_and_stops_before_prepared_on_a_bad_frame() {
        let (candidate_channel, mut old_main_channel): (
            Channel<UpgradeHandshake, UpgradeHandshake>,
            Channel<UpgradeHandshake, UpgradeHandshake>,
        ) = Channel::generate_nonblocking(64, 512).expect("could not create handshake");
        let candidate_fd = candidate_channel.sock.into_raw_fd();
        old_main_channel
            .blocking()
            .expect("could not block old-main side of handshake");
        let (mut sender, receiver) = StdUnixStream::pair().expect("transfer pair");
        std::io::Write::write_all(&mut sender, b"not a frame").expect("write garbage");
        drop(sender);

        let error = begin_new_main_process(candidate_fd, receiver.into_raw_fd(), Some(3), 64, 512)
            .expect_err("a malformed transfer must fail");
        assert!(
            !matches!(error, UpgradeError::UnsupportedProtocol(_)),
            "protocol 3 must have a receiver: {error:?}"
        );
        assert!(
            old_main_channel.read_message().is_err(),
            "no PREPARED may precede a refused transfer"
        );
    }

    // ---- protocol-3 frame: the real sender and receiver functions ----------
    //
    // These drive `send_upgrade_transfer` and `receive_upgrade_transfer` over a
    // socket pair. The real-binary receiver tests in
    // `bin/tests/main_upgrade_v3_memory_e2e.rs` cover the refusals that need a
    // process; a successful handoff of a whole process needs the worker
    // bootstrap, which is not part of this slice, so success stops at these
    // functions and the PREPARED boundary is the one asserted by
    // `protocol_three_reads_the_socket_and_stops_before_prepared_on_a_bad_frame`.

    /// Synthetic secret: it must never appear in an error or a wiped buffer.
    const HANDOFF_SENTINEL: &str = "Bearer SYNTHETIC-V3-HANDOFF-SECRET-5d2e";

    type Metadata = HashMap<String, Vec<HealthCheckHeader>>;

    struct Handoff {
        transfer: ProtectedTransfer,
        metadata: Metadata,
        key: FingerprintKey,
    }

    fn fingerprint_key() -> FingerprintKey {
        FingerprintKey::from_parts(Zeroizing::new([7u8; 32]), [9u8; 16])
    }

    /// Confirm `headers` for `cluster` and record the metadata naming them.
    fn confirm(
        key: &FingerprintKey,
        journal: &mut ProtectedHeaderJournal,
        metadata: &mut Metadata,
        cluster: &str,
        headers: Vec<(String, Vec<u8>)>,
    ) {
        let set = ProtectedHeaderSet::new(
            headers
                .into_iter()
                .map(|(name, value)| {
                    let mut carrier = HealthCheckHeaderValue {
                        name: name.to_owned(),
                        value,
                    };
                    let value = SecretValue::take_from_ingress(&mut carrier)
                        .expect("a valid synthetic value");
                    ProtectedHeader::new(key, name, value)
                })
                .collect(),
        );
        metadata.insert(
            cluster.to_owned(),
            set.headers()
                .iter()
                .map(|header| HealthCheckHeader {
                    name: header.name().to_owned(),
                    fingerprint: header.fingerprint().to_string(),
                })
                .collect(),
        );
        let version = journal
            .propose(cluster, ProtectedHeaderChange::Replace(set))
            .expect("a free version");
        journal.accept(cluster, version);
    }

    fn lookup<'m>(metadata: &'m Metadata) -> impl Fn(&str) -> Option<&'m [HealthCheckHeader]> {
        |cluster| metadata.get(cluster).map(Vec::as_slice)
    }

    /// Cluster `a` holds the ordered A/B pair, `empty` is an explicit empty
    /// confirmed set, and `filler` further clusters of `headers` headers of
    /// `value_len` bytes each make the protected block as large as asked.
    fn handoff(filler: usize, headers: usize, value_len: usize) -> Handoff {
        let key = fingerprint_key();
        let mut journal = ProtectedHeaderJournal::default();
        let mut metadata = Metadata::new();
        confirm(
            &key,
            &mut journal,
            &mut metadata,
            "a",
            vec![
                ("X-B".to_owned(), b"second-by-name-first-by-order".to_vec()),
                (
                    "Authorization".to_owned(),
                    HANDOFF_SENTINEL.as_bytes().to_vec(),
                ),
            ],
        );
        confirm(&key, &mut journal, &mut metadata, "empty", Vec::new());
        for cluster in 0..filler {
            let values = (0..headers)
                .map(|index| (format!("X-Fill-{index}"), vec![b'x'; value_len]))
                .collect();
            confirm(
                &key,
                &mut journal,
                &mut metadata,
                &format!("f{cluster:02}"),
                values,
            );
        }
        let transfer = ProtectedTransfer::export(&journal, Some(&key), lookup(&metadata))
            .expect("every confirmed set is named by its metadata");
        Handoff {
            transfer,
            metadata,
            key,
        }
    }

    fn send_buffer_size(socket: &StdUnixStream) -> usize {
        let mut size: libc::c_int = 0;
        let mut length = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `size` and `length` are live, correctly sized outputs for
        // `SO_SNDBUF` on an open descriptor.
        let result = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw mut size).cast(),
                &raw mut length,
            )
        };
        assert_eq!(result, 0, "could not read SO_SNDBUF");
        usize::try_from(size).expect("a positive buffer size")
    }

    /// The bytes of a zeroing buffer's whole allocation, including the part a
    /// wipe moved out of `len`.
    ///
    /// SAFETY contract: `pointer`/`capacity` were taken from a live
    /// `Zeroizing<Vec<u8>>` that still owns the allocation, and the wipe
    /// writes every byte of it, so all `capacity` bytes are initialised.
    fn allocation<'a>(pointer: *const u8, capacity: usize) -> &'a [u8] {
        // SAFETY: see above; the callers keep the owning payload alive.
        unsafe { std::slice::from_raw_parts(pointer, capacity) }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    /// Where a payload's two buffers live, to prove they were wiped in place.
    struct Allocations {
        snapshot: (*const u8, usize),
        protected: (*const u8, usize),
    }

    fn allocations(payload: &UpgradePayload) -> Allocations {
        Allocations {
            snapshot: (payload.snapshot.as_ptr(), payload.snapshot.capacity()),
            protected: (payload.protected.as_ptr(), payload.protected.capacity()),
        }
    }

    fn assert_wiped_in_place(payload: &UpgradePayload, before: &Allocations) {
        assert!(payload.snapshot.is_empty() && payload.protected.is_empty());
        assert_eq!(
            (payload.snapshot.as_ptr(), payload.snapshot.capacity()),
            before.snapshot,
            "the snapshot buffer was reallocated"
        );
        assert_eq!(
            (payload.protected.as_ptr(), payload.protected.capacity()),
            before.protected,
            "the protected buffer was reallocated"
        );
        for (what, (pointer, capacity)) in [
            ("snapshot", before.snapshot),
            ("protected", before.protected),
        ] {
            assert!(
                allocation(pointer, capacity).iter().all(|&byte| byte == 0),
                "the {what} buffer was not wiped"
            );
        }
    }

    #[test]
    fn a_frame_larger_than_the_socket_buffer_round_trips_and_the_sender_is_wiped() {
        let Handoff {
            transfer,
            metadata,
            key,
        } = handoff(8, 32, 8000);
        let upgrade_data = sample_upgrade_data();
        let expected_snapshot = serde_json::to_string(&upgrade_data).expect("snapshot json");
        let mut payload = UpgradePayload::encode(&upgrade_data, &transfer).expect("encode");

        let (mut sender, receiver) = StdUnixStream::pair().expect("transfer pair");
        assert!(
            payload.protected.len() > 2 * send_buffer_size(&sender),
            "the protected block must not fit the socket buffer"
        );
        assert!(
            contains(&payload.protected, HANDOFF_SENTINEL.as_bytes()),
            "the sender holds the value before sending"
        );
        let before = allocations(&payload);

        let deadline = Instant::now() + Duration::from_secs(20);
        let writer = thread::spawn(move || {
            let result = send_upgrade_transfer(&mut sender, &mut payload, deadline);
            (result, payload)
        });
        let received = receive_upgrade_transfer(&receiver, Instant::now())
            .expect("the frame must be received whole");
        let (sent, payload) = writer.join().expect("the sender thread");
        sent.expect("the frame must be sent whole");

        assert_wiped_in_place(&payload, &before);
        assert_eq!(
            serde_json::to_string(&received.upgrade_data).expect("snapshot json"),
            expected_snapshot
        );
        assert!(received.deadline > Instant::now(), "one absolute deadline");

        let (restored_key, mut journal) = received
            .protected
            .restore(lookup(&metadata))
            .expect("the receiver restores exactly what the sender exported");
        let restored_key = restored_key.expect("the key travels");
        let probe = SecretValue::take_from_ingress(&mut HealthCheckHeaderValue {
            name: "x".to_owned(),
            value: b"probe".to_vec(),
        })
        .expect("a probe value");
        assert_eq!(
            restored_key.fingerprint("x", &probe).to_string(),
            key.fingerprint("x", &probe).to_string(),
            "same key bytes and epoch"
        );
        let held = journal.effective("a").expect("cluster a is held");
        let names: Vec<_> = held.headers().iter().map(ProtectedHeader::name).collect();
        assert_eq!(names, ["X-B", "Authorization"], "A/B order is kept");
        assert_eq!(
            held.headers()[1].value().expose_secret_bytes(),
            HANDOFF_SENTINEL.as_bytes()
        );
        assert!(journal.effective("empty").expect("kept").is_empty());
        assert_eq!(journal.effective("f07").expect("last filler").len(), 32);
        assert_eq!(
            journal
                .propose("a", ProtectedHeaderChange::Clear)
                .expect("a next version")
                .get(),
            2 + 8 + 1,
            "the counter continues after the versions the sender issued"
        );
    }

    #[test]
    fn a_forced_write_error_still_wipes_both_sender_buffers() {
        let Handoff { transfer, .. } = handoff(8, 32, 8000);
        let mut payload =
            UpgradePayload::encode(&sample_upgrade_data(), &transfer).expect("encode");
        let before = allocations(&payload);
        let (mut sender, receiver) = StdUnixStream::pair().expect("transfer pair");

        // The peer reads a little of the frame and goes away mid-payload.
        let peer = thread::spawn(move || {
            let mut some = vec![0u8; 100 * 1024];
            (&receiver).read_exact(&mut some).expect("read a prefix");
        });
        let result = send_upgrade_transfer(
            &mut sender,
            &mut payload,
            Instant::now() + Duration::from_secs(20),
        );
        peer.join().expect("the peer thread");

        let error = result.expect_err("a vanished peer must fail the send");
        assert!(matches!(error, UpgradeError::TransferIo(_)), "{error:?}");
        assert!(
            !format!("{error} {error:?}").contains("SYNTHETIC"),
            "a diagnostic carries no value"
        );
        assert_wiped_in_place(&payload, &before);
    }

    /// The two encoded sections of a small payload.
    fn frame_parts() -> (Vec<u8>, Vec<u8>) {
        let Handoff { transfer, .. } = handoff(0, 0, 0);
        let payload = UpgradePayload::encode(&sample_upgrade_data(), &transfer).expect("encode");
        (payload.snapshot.to_vec(), payload.protected.to_vec())
    }

    fn frame(snapshot_len: u64, protected_len: u64, body: &[&[u8]]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(TRANSFER_MAGIC);
        frame.extend_from_slice(&TRANSFER_FRAME_VERSION.to_be_bytes());
        frame.extend_from_slice(&5_000u64.to_be_bytes());
        frame.extend_from_slice(&snapshot_len.to_be_bytes());
        frame.extend_from_slice(&protected_len.to_be_bytes());
        for part in body {
            frame.extend_from_slice(part);
        }
        frame
    }

    /// Receive `bytes` that the sender then half-closed.
    fn receive_bytes(bytes: &[u8]) -> Result<(), UpgradeError> {
        let (mut sender, receiver) = StdUnixStream::pair().expect("transfer pair");
        sender
            .write_all(bytes)
            .expect("a small frame fits the buffer");
        sender
            .shutdown(std::net::Shutdown::Write)
            .expect("half-close");
        receive_upgrade_transfer(&receiver, Instant::now()).map(|_| ())
    }

    /// A named frame and the refusal it must provoke.
    type RefusedFrame<'a> = (&'a str, Vec<u8>, fn(&UpgradeError) -> bool);

    #[test]
    fn malformed_truncated_trailing_and_oversized_frames_fail_without_a_value_in_the_error() {
        let (snapshot, protected) = frame_parts();
        let (snapshot_len, protected_len) = (snapshot.len() as u64, protected.len() as u64);
        let whole = frame(snapshot_len, protected_len, &[&snapshot, &protected]);
        assert!(receive_bytes(&whole).is_ok(), "the control frame is valid");

        let mut flag = protected.clone();
        flag[1] = 2;
        let mut layout = protected.clone();
        layout[0] = 9;
        let cut = &protected[..protected.len() - 1];

        let cases: Vec<RefusedFrame> = vec![
            (
                "truncated protected block",
                frame(snapshot_len, protected_len, &[&snapshot, cut]),
                |e| matches!(e, UpgradeError::TransferTruncated),
            ),
            (
                "trailing byte",
                frame(snapshot_len, protected_len, &[&snapshot, &protected, b"x"]),
                |e| matches!(e, UpgradeError::TransferTrailing),
            ),
            (
                "oversized protected length",
                frame(
                    snapshot_len,
                    PROTECTED_TRANSFER_MAX_BYTES as u64 + 1,
                    &[&snapshot, &protected],
                ),
                |e| matches!(e, UpgradeError::TransferTooLarge("protected state")),
            ),
            (
                "oversized snapshot length",
                frame(u64::MAX, protected_len, &[&snapshot, &protected]),
                |e| matches!(e, UpgradeError::TransferTooLarge("snapshot")),
            ),
            (
                "protected key flag",
                frame(snapshot_len, protected_len, &[&snapshot, &flag]),
                |e| matches!(e, UpgradeError::TransferProtected(TransferError::Malformed)),
            ),
            (
                "protected layout version",
                frame(snapshot_len, protected_len, &[&snapshot, &layout]),
                |e| matches!(e, UpgradeError::TransferProtected(TransferError::Malformed)),
            ),
            (
                "snapshot that is not json",
                frame(
                    snapshot_len,
                    protected_len,
                    &[&vec![b'!'; snapshot.len()], &protected],
                ),
                |e| matches!(e, UpgradeError::MalformedSnapshot { .. }),
            ),
        ];
        for (what, bytes, expected) in &cases {
            let error = receive_bytes(bytes).expect_err(what);
            assert!(expected(&error), "{what}: {error:?}");
            let shown = format!("{error} {error:?}");
            assert!(!shown.contains("SYNTHETIC"), "{what}: a value in {shown}");
        }
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
