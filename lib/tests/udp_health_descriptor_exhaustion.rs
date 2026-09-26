//! `UdpProxy::health_poll` must not need a free file descriptor.
//!
//! The server calls `health_poll` once per event-loop turn. It used to clone
//! the mio `Registry` first: `Registry::try_clone` is an
//! `fcntl(F_DUPFD_CLOEXEC)` of the epoll descriptor, and dropping the clone a
//! `close`. That cost two syscalls on every turn, with or without a UDP
//! cluster, and when the clone failed — a worker out of descriptors
//! (`EMFILE`) — the prober was skipped without a word, so an in-flight probe
//! was never timed out and its backend's health never updated.
//!
//! This test pins the second, observable half of that defect: with every
//! descriptor slot taken, a probe that has already timed out must still be
//! recorded as a failure. Progressing an in-flight probe allocates nothing, so
//! only a descriptor the prober allocates for itself can make this fail.
//!
//! It lowers `RLIMIT_NOFILE` for the whole process, which is why it lives in
//! its own integration-test binary with a single test: no other test can run
//! concurrently in this process and hit the exhausted table.
//!
//! To SEE THIS RED: in `lib/src/udp.rs`, make `health_poll` clone the registry
//! again (`if let Ok(registry) = self.registry.try_clone() { self.health.poll(
//! &self.backends, &registry); }`). The final assertion then reports zero
//! consecutive failures.

use std::{
    fs::File,
    net::{SocketAddr, TcpListener},
    thread,
    time::Duration,
};

use sozu_command_lib::proto::command::{
    Cluster, Request, ResponseStatus, UdpClusterConfig, UdpHealthConfig, UdpHealthMode,
    WorkerRequest, request::RequestType,
};
use sozu_lib::{backends::Backend, testing::prebuild_server, udp::UdpProxy};

const CLUSTER: &str = "udp-health-emfile";

/// Highest descriptor currently open in this process.
fn highest_open_fd() -> i32 {
    std::fs::read_dir("/proc/self/fd")
        .expect("/proc/self/fd must be readable")
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .max()
        .expect("a process always has descriptors open")
}

fn set_soft_nofile(limit: libc::rlim_t) {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rlim` is a valid, writable `rlimit`; getrlimit only writes it.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) },
        0,
        "getrlimit(RLIMIT_NOFILE) failed"
    );
    rlim.rlim_cur = limit;
    // SAFETY: `rlim` is a valid `rlimit` read above; only the soft limit changes
    // and it never exceeds the hard limit the kernel just reported.
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) },
        0,
        "setrlimit(RLIMIT_NOFILE) failed"
    );
}

fn soft_nofile() -> libc::rlim_t {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: as in `set_soft_nofile`.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) },
        0,
        "getrlimit(RLIMIT_NOFILE) failed"
    );
    rlim.rlim_cur
}

#[test]
fn a_timed_out_probe_is_recorded_even_when_no_descriptor_is_free() {
    let parts = prebuild_server(16, 16384, false).expect("could not prebuild a test server");

    // A listener that never accepts: the probe's connect completes in the
    // kernel backlog, and since no readiness is ever fed to the prober, the
    // probe stays in flight until its timeout.
    let backend_listener = TcpListener::bind("127.0.0.1:0").expect("bind the probe target");
    let backend_address: SocketAddr = backend_listener.local_addr().expect("probe target address");
    parts.backends.borrow_mut().add_backend(
        CLUSTER,
        Backend::new("backend-1", backend_address, None, None, None),
    );

    let mut proxy = UdpProxy::new(
        parts.registry,
        parts.sessions,
        parts.pool,
        parts.backends.clone(),
        16,
        16384,
    );
    let response = proxy.notify(WorkerRequest::new(
        "add-cluster".to_owned(),
        Request {
            request_type: Some(RequestType::AddCluster(Cluster {
                cluster_id: CLUSTER.to_owned(),
                udp: Some(UdpClusterConfig {
                    health: Some(UdpHealthConfig {
                        mode: Some(UdpHealthMode::TcpProbe as i32),
                        rise: Some(1),
                        fall: Some(1),
                        // One probe for the whole test: the second poll must
                        // not be due to start another one.
                        probe_interval_seconds: Some(3600),
                        probe_timeout_seconds: Some(1),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            })),
        },
    ));
    assert_eq!(
        response.status,
        ResponseStatus::Ok as i32,
        "AddCluster was refused: {response:?}"
    );

    let failures = || {
        parts
            .backends
            .borrow()
            .backends
            .get(CLUSTER)
            .expect("the cluster's backend list")
            .backends[0]
            .borrow()
            .health
            .consecutive_failures
    };

    // First turn, descriptors available: the TCP probe starts and is in flight.
    proxy.health_poll();
    assert_eq!(
        failures(),
        0,
        "precondition: the probe must still be in flight"
    );

    // Let the one-second probe timeout elapse.
    thread::sleep(Duration::from_millis(1200));

    // Take every descriptor slot: cap the soft limit just above the highest
    // open descriptor, then fill the holes below it until the kernel refuses.
    let saved_limit = soft_nofile();
    set_soft_nofile((highest_open_fd() + 1) as libc::rlim_t);
    let mut fillers = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => fillers.push(file),
            Err(err) => {
                assert_eq!(
                    err.raw_os_error(),
                    Some(libc::EMFILE),
                    "descriptor table must be exhausted, not failing for another reason"
                );
                break;
            }
        }
    }

    // Second turn, no descriptor free: the timed-out probe must be recorded.
    proxy.health_poll();
    let recorded = failures();

    drop(fillers);
    set_soft_nofile(saved_limit);
    drop(backend_listener);

    assert_eq!(
        recorded, 1,
        "health_poll skipped the prober when no descriptor was free: the timed-out probe was not recorded"
    );
}
