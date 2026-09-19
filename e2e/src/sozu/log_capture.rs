//! File-backed capture of a single worker's own log output.
//!
//! The harness otherwise has no way to read what a worker logged, which is
//! what makes a defect whose only observable is a log line untestable
//! end-to-end. Three mechanisms stand in the way and this module plus
//! [`Worker::start_new_worker_with_logging`](crate::sozu::worker::Worker::start_new_worker_with_logging)
//! answer all three:
//!
//! 1. the worker's log target used to be the hardcoded string `"stdout"`, so
//!    the override parameter exists;
//! 2. `LoggerBackend::Stdout` writes through a `std::io::Stdout` handle
//!    (`command/src/logging/logs.rs`), which does not consult libtest's
//!    `OUTPUT_CAPTURE` — only the `print!` path does — so even `--nocapture`
//!    hands the test no `String`. A `file://` target sidesteps the question
//!    entirely;
//! 3. `LOGGER` is a `thread_local!` with a one-shot `initialized` guard, so
//!    the worker's logger is not the test thread's and must be configured at
//!    spawn time. That same fact makes each capture private to one worker, so
//!    two capture tests need no `serial_test` serialisation.
//!
//! `file://` rather than `udp://` is deliberate. `capture_test_logs_at_level`
//! (`lib/src/lib.rs`) drains its receiver only once the run has finished and
//! therefore depends on the kernel socket buffer having held every datagram.
//! That is true of the handful of lines a unit test emits and false under an
//! H2 conversation at `trace`: it drops lines silently and produces a
//! load-sensitive flake. A file has no loss mode.
//!
//! Flushing: the `file://` backend is a `MultiLineWriter`
//! (`command/src/writer.rs`) with a 4096-byte buffer that flushes up to its
//! last newline on overflow and flushes entirely on `Drop`. The worker's `Drop` runs when its thread-local
//! `LOGGER` is destroyed, i.e. when the worker thread exits — so read the
//! capture only after `Worker::wait_for_server_stop`, which joins that thread.

use std::{fs, path::PathBuf};

use tempfile::TempDir;

/// A private temporary directory holding one worker's log file, plus the
/// `file://` target string to hand to
/// [`Worker::start_new_worker_with_logging`](crate::sozu::worker::Worker::start_new_worker_with_logging).
pub struct WorkerLogCapture {
    /// Owns the temporary directory. Held only for its `Drop`, which removes
    /// the directory and the log file with it.
    _directory: TempDir,
    path: PathBuf,
}

impl WorkerLogCapture {
    /// Create an empty capture. `label` only decorates the temporary
    /// directory name, to make a leaked directory attributable to its test.
    pub fn new(label: &str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix(&format!("sozu-e2e-log-{label}-"))
            .tempdir()
            .expect("could not create a temporary directory for the worker log capture");
        let path = directory.path().join("worker.log");
        Self {
            _directory: directory,
            path,
        }
    }

    /// The `log_target` string to pass to
    /// [`Worker::start_new_worker_with_logging`](crate::sozu::worker::Worker::start_new_worker_with_logging).
    pub fn target(&self) -> String {
        format!("file://{}", self.path.display())
    }

    /// Everything the worker has flushed so far. See the module note on when
    /// that is everything it wrote.
    pub fn contents(&self) -> String {
        fs::read_to_string(&self.path).unwrap_or_else(|error| {
            panic!(
                "could not read the captured worker log at {}: {error}. \
                 `target_to_backend` creates this file when the worker thread \
                 initialises its logger, so an absent file means the worker \
                 never got that far — look for `could not setup logging` on \
                 stdout.",
                self.path.display()
            )
        })
    }

    /// The physical lines of [`WorkerLogCapture::contents`] that contain
    /// `needle`.
    ///
    /// A log record that formats a value with `{:#?}` spans several physical
    /// lines; filtering on a protocol tag such as `MUX-H2` therefore selects
    /// the record's first line, which is the one carrying the log-context
    /// envelope and its `peer=` slot.
    pub fn lines_containing(&self, needle: &str) -> Vec<String> {
        self.contents()
            .lines()
            .filter(|line| line.contains(needle))
            .map(str::to_owned)
            .collect()
    }
}
