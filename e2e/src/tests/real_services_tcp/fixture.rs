//! Container and TCP-listener fixtures shared by the real-service tests.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{ActivateListener, DeactivateListener, ListenerType, request::RequestType},
};

use crate::{sozu::worker::Worker, tests::tests::create_local_address};

static RUN_COUNTER: AtomicU64 = AtomicU64::new(1);

const OWNER_LABEL: &str = "org.sozu.e2e.owner";
const RUN_LABEL: &str = "org.sozu.e2e.run";
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContainerProtocol {
    Tcp,
    Udp,
}

impl ContainerProtocol {
    fn docker_suffix(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

#[derive(Debug)]
pub(in crate::tests) struct ContainerSpec {
    service: &'static str,
    image: &'static str,
    container_port: u16,
    protocol: ContainerProtocol,
    environment: Vec<(String, String)>,
    read_only_mounts: Vec<(PathBuf, String)>,
    command: Vec<String>,
    health_command: Option<String>,
    startup_timeout: Duration,
}

impl ContainerSpec {
    pub(in crate::tests) fn new(
        service: &'static str,
        image: &'static str,
        container_port: u16,
    ) -> Self {
        Self {
            service,
            image,
            container_port,
            protocol: ContainerProtocol::Tcp,
            environment: Vec::new(),
            read_only_mounts: Vec::new(),
            command: Vec::new(),
            health_command: None,
            startup_timeout: Duration::from_secs(90),
        }
    }

    pub(in crate::tests) fn udp(mut self) -> Self {
        self.protocol = ContainerProtocol::Udp;
        self
    }

    pub(in crate::tests) fn mount_read_only(
        mut self,
        source: impl Into<PathBuf>,
        destination: impl Into<String>,
    ) -> Self {
        let source = source.into();
        assert!(
            source.exists(),
            "container mount source does not exist: {source:?}"
        );
        self.read_only_mounts.push((source, destination.into()));
        self
    }

    pub(in crate::tests) fn environment(mut self, key: &str, value: impl Into<String>) -> Self {
        self.environment.push((key.to_owned(), value.into()));
        self
    }

    pub(in crate::tests) fn command(
        mut self,
        command: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.command = command.into_iter().map(Into::into).collect();
        self
    }

    pub(in crate::tests) fn health_command(mut self, command: impl Into<String>) -> Self {
        self.health_command = Some(command.into());
        self
    }

    pub(in crate::tests) fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }
}

#[derive(Debug)]
struct ContainerEngine {
    executable: OsString,
    identity: String,
}

impl ContainerEngine {
    fn discover() -> Self {
        if let Some(explicit) = env::var_os("SOZU_CONTAINER_ENGINE") {
            return Self::probe(explicit).unwrap_or_else(|error| {
                panic!(
                    "SOZU_CONTAINER_ENGINE is set but unusable; real-service tests never skip: {error}"
                )
            });
        }

        for executable in ["docker", "podman"] {
            if let Ok(engine) = Self::probe(OsString::from(executable)) {
                return engine;
            }
        }

        panic!(
            "neither Docker nor Podman has a reachable server; an enabled real-service feature must run its service"
        );
    }

    fn probe(executable: OsString) -> Result<Self, String> {
        let executable_name = Path::new(&executable)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        let format = if executable_name.contains("podman") {
            "{{.Host.ID}}|{{.Host.Hostname}}|{{.Version.Version}}"
        } else {
            "{{.ID}}|{{.Name}}|{{.ServerVersion}}"
        };
        let output = Command::new(&executable)
            .args(["info", "--format", format])
            .output()
            .map_err(|error| format!("could not execute {executable:?}: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "{executable:?} info failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let identity = String::from_utf8(output.stdout)
            .map_err(|error| format!("engine identity was not UTF-8: {error}"))?
            .trim()
            .to_owned();
        if identity.is_empty() {
            return Err(format!("{executable:?} returned an empty server identity"));
        }
        Ok(Self {
            executable,
            identity,
        })
    }

    fn output<I, S>(&self, args: I) -> io::Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(&self.executable).args(args).output()
    }

    fn verify_same_server(&self) -> Result<(), String> {
        let current = Self::probe(self.executable.clone())
            .map_err(|error| format!("container engine disappeared before cleanup: {error}"))?;
        if current.identity != self.identity {
            return Err("container engine server identity changed during the test".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(in crate::tests) struct OwnedContainer {
    engine: ContainerEngine,
    id: String,
    name: String,
    fingerprint: String,
    run_id: String,
    backend: Option<SocketAddr>,
    log_path: PathBuf,
    stopped: bool,
}

impl OwnedContainer {
    pub(in crate::tests) fn start(spec: ContainerSpec) -> Self {
        let engine = ContainerEngine::discover();
        let serial = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let fingerprint = format!("{}-{}-{serial}", spec.service, std::process::id());
        let name = format!("sozu-e2e-{fingerprint}");
        let run_id = owned_run_id();
        let log_path = artifact_root().join(format!("{name}.log"));

        let mut args = vec![
            OsString::from("run"),
            OsString::from("--detach"),
            OsString::from("--name"),
            OsString::from(&name),
            OsString::from("--label"),
            OsString::from(format!("{OWNER_LABEL}={fingerprint}")),
            OsString::from("--label"),
            OsString::from(format!("{RUN_LABEL}={run_id}")),
            OsString::from("--publish"),
            OsString::from(format!(
                "127.0.0.1::{}/{}",
                spec.container_port,
                spec.protocol.docker_suffix()
            )),
        ];
        for (key, value) in &spec.environment {
            args.push(OsString::from("--env"));
            args.push(OsString::from(format!("{key}={value}")));
        }
        for (source, destination) in &spec.read_only_mounts {
            args.push(OsString::from("--mount"));
            args.push(OsString::from(format!(
                "type=bind,source={},destination={destination},readonly",
                source.display()
            )));
        }
        if let Some(health_command) = &spec.health_command {
            args.extend([
                OsString::from("--health-cmd"),
                OsString::from(health_command),
                OsString::from("--health-interval"),
                OsString::from("1s"),
                OsString::from("--health-timeout"),
                OsString::from("5s"),
                OsString::from("--health-retries"),
                OsString::from("120"),
            ]);
        }
        args.push(OsString::from(spec.image));
        args.extend(spec.command.iter().map(OsString::from));

        let output = engine
            .output(args)
            .unwrap_or_else(|error| panic!("could not start {}: {error}", spec.service));
        if !output.status.success() {
            let cleanup = Self::resolve_owned_id(&engine, &name, &fingerprint, &run_id).map(|id| {
                id.map(|id| {
                    let mut container = Self {
                        engine,
                        id,
                        name,
                        fingerprint,
                        run_id,
                        backend: None,
                        log_path,
                        stopped: false,
                    };
                    container.capture_diagnostics();
                    container.cleanup_owned()
                })
            });
            panic!(
                "could not start {} from {}: {}; failed-start cleanup: {:?}",
                spec.service,
                spec.image,
                String::from_utf8_lossy(&output.stderr).trim(),
                cleanup
            );
        }
        let id = String::from_utf8(output.stdout)
            .expect("container ID must be UTF-8")
            .trim()
            .to_owned();
        assert!(!id.is_empty(), "container engine returned an empty ID");

        let mut container = Self {
            engine,
            id,
            name,
            fingerprint,
            run_id,
            backend: None,
            log_path,
            stopped: false,
        };
        let backend = wait_for_published_port(
            &container.engine,
            &container.id,
            spec.container_port,
            spec.protocol,
            spec.startup_timeout,
            &container.log_path,
        );
        if spec.health_command.is_some() {
            wait_for_healthy(
                &container.engine,
                &container.id,
                spec.startup_timeout,
                spec.service,
                &container.log_path,
            );
        }
        if spec.protocol == ContainerProtocol::Tcp {
            wait_for_tcp(
                backend,
                spec.startup_timeout,
                spec.service,
                &container.engine,
                &container.id,
                &container.log_path,
            );
        }
        container.backend = Some(backend);
        container
    }

    fn resolve_owned_id(
        engine: &ContainerEngine,
        name: &str,
        fingerprint: &str,
        run_id: &str,
    ) -> Result<Option<String>, String> {
        engine.verify_same_server()?;
        let format = format!(
            "{{{{.Id}}}}|{{{{index .Config.Labels \"{OWNER_LABEL}\"}}}}|{{{{index .Config.Labels \"{RUN_LABEL}\"}}}}"
        );
        let output = engine
            .output(["inspect", "--format", &format, name])
            .map_err(|error| format!("could not inspect failed container start: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let value = String::from_utf8(output.stdout)
            .map_err(|error| format!("failed container identity was not UTF-8: {error}"))?;
        let mut fields = value.trim().split('|');
        let id = fields.next().unwrap_or_default();
        let actual_owner = fields.next().unwrap_or_default();
        let actual_run = fields.next().unwrap_or_default();
        if id.is_empty()
            || actual_owner != fingerprint
            || actual_run != run_id
            || fields.next().is_some()
        {
            return Err(format!(
                "container named {name} did not match the expected owned identity"
            ));
        }
        Ok(Some(id.to_owned()))
    }

    fn artifact_path(&self, extension: &str) -> PathBuf {
        self.log_path.with_extension(extension)
    }

    fn capture_diagnostics(&self) {
        if let Some(parent) = self.log_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if let Ok(output) = self.engine.output(["inspect", &self.id]) {
            let mut bytes = output.stdout;
            bytes.extend_from_slice(&output.stderr);
            let _ = fs::write(self.artifact_path("inspect.json"), bytes);
        }
        let state_format = "{{.Id}}|{{.State.Status}}|{{.State.Running}}|{{.State.ExitCode}}|{{.State.OOMKilled}}|{{.State.StartedAt}}|{{.State.FinishedAt}}";
        if let Ok(output) = self
            .engine
            .output(["inspect", "--format", state_format, &self.id])
        {
            let mut bytes = output.stdout;
            bytes.extend_from_slice(&output.stderr);
            let _ = fs::write(self.artifact_path("state"), bytes);
        }
        let volumes_format =
            "{{range .Mounts}}{{if eq .Type \"volume\"}}{{println .Name}}{{end}}{{end}}";
        if let Ok(output) = self
            .engine
            .output(["inspect", "--format", volumes_format, &self.id])
        {
            let mut bytes = output.stdout;
            bytes.extend_from_slice(&output.stderr);
            let _ = fs::write(self.artifact_path("volumes"), bytes);
        }

        let output = match self.engine.output(["logs", &self.id]) {
            Ok(output) => output,
            Err(error) => {
                let _ = fs::write(&self.log_path, format!("could not collect logs: {error}\n"));
                return;
            }
        };
        let mut bytes = output.stdout;
        bytes.extend_from_slice(&output.stderr);
        let _ = fs::write(&self.log_path, bytes);
    }

    fn verify_ownership(&self) -> Result<(), String> {
        let format = format!(
            "{{{{index .Config.Labels \"{OWNER_LABEL}\"}}}}|{{{{index .Config.Labels \"{RUN_LABEL}\"}}}}"
        );
        let output = self
            .engine
            .output(["inspect", "--format", &format, &self.id])
            .map_err(|error| format!("could not inspect owned container {}: {error}", self.id))?;
        if !output.status.success() {
            return Err(format!(
                "could not inspect owned container {}: {}",
                self.id,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let expected = format!("{}|{}", self.fingerprint, self.run_id);
        let actual = String::from_utf8(output.stdout)
            .map_err(|error| format!("owned container labels were not UTF-8: {error}"))?;
        if actual.trim() != expected {
            return Err(format!(
                "container {} ownership changed: expected {expected:?}, got {:?}",
                self.id,
                actual.trim()
            ));
        }
        Ok(())
    }

    pub(in crate::tests) fn backend(&self) -> SocketAddr {
        self.backend
            .expect("container backend is initialized before start returns")
    }

    fn assert_backend_alive(&self) {
        let backend = self.backend();
        TcpStream::connect_timeout(&backend, Duration::from_secs(1)).unwrap_or_else(|error| {
            panic!(
                "{} backend {} stopped while the Sōzu listener was disabled: {error}",
                self.name, backend
            )
        });
    }

    fn cleanup_owned(&mut self) -> Result<(), String> {
        if self.stopped {
            return Ok(());
        }
        self.engine.verify_same_server()?;
        self.verify_ownership()?;
        let remove = self
            .engine
            .output(["rm", "--volumes", "--force", &self.id])
            .map_err(|error| format!("could not execute exact container cleanup: {error}"))?;
        if !remove.status.success() {
            return Err(format!(
                "could not remove owned container {}: {}",
                self.id,
                String::from_utf8_lossy(&remove.stderr).trim()
            ));
        }
        self.stopped = true;
        Ok(())
    }

    pub(in crate::tests) fn finish(mut self) {
        self.cleanup_owned()
            .unwrap_or_else(|error| panic!("{error}"));
        let inspect = self
            .engine
            .output(["inspect", &self.id])
            .expect("could not verify exact container cleanup");
        assert!(
            !inspect.status.success(),
            "owned container {} still exists after cleanup",
            self.id
        );
    }
}

impl Drop for OwnedContainer {
    fn drop(&mut self) {
        if !self.stopped {
            self.capture_diagnostics();
            if let Err(error) = self.cleanup_owned() {
                let _ = fs::write(self.artifact_path("cleanup-error"), format!("{error}\n"));
            }
        }
    }
}

pub(super) struct ServiceHarness {
    container: Option<OwnedContainer>,
    worker: Option<Worker>,
    frontend: SocketAddr,
}

impl ServiceHarness {
    pub(super) fn start(spec: impl FnOnce(SocketAddr) -> ContainerSpec) -> Self {
        let frontend = create_local_address();
        let container = OwnedContainer::start(spec(frontend));
        let (config, listeners, state) = Worker::empty_tcp_config(frontend);
        let mut worker = Worker::start_new_worker_owned(
            format!("real-service-{}", container.name),
            config,
            listeners,
            state,
        );
        worker.send_proxy_request_type(RequestType::AddTcpListener(
            ListenerBuilder::new_tcp(frontend.into())
                .to_tcp(None)
                .expect("TCP listener config"),
        ));
        worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
            interface: None,
            address: frontend.into(),
            proxy: ListenerType::Tcp.into(),
            from_scm: false,
        }));
        worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
            "real_service",
        )));
        worker.send_proxy_request_type(RequestType::AddTcpFrontend(Worker::default_tcp_frontend(
            "real_service",
            frontend,
        )));
        worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
            "real_service",
            "real_service_backend",
            container.backend(),
            None,
        )));
        worker.read_to_last();

        Self {
            container: Some(container),
            worker: Some(worker),
            frontend,
        }
    }

    pub(super) fn frontend(&self) -> SocketAddr {
        self.frontend
    }

    pub(super) fn deactivate_and_assert_no_bypass(&mut self, application_address: SocketAddr) {
        let worker = self.worker.as_mut().expect("worker must be running");
        worker.send_proxy_request_type(RequestType::DeactivateListener(DeactivateListener {
            interface: None,
            address: self.frontend.into(),
            proxy: ListenerType::Tcp.into(),
            to_scm: false,
        }));
        worker.read_to_last();

        self.container
            .as_ref()
            .expect("container must be running")
            .assert_backend_alive();
        assert!(
            TcpStream::connect_timeout(&application_address, Duration::from_millis(500)).is_err(),
            "a fresh application connection succeeded while the Sōzu TCP listener was disabled"
        );
    }

    pub(super) fn reactivate(&mut self) {
        let worker = self.worker.as_mut().expect("worker must be running");
        worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
            interface: None,
            address: self.frontend.into(),
            proxy: ListenerType::Tcp.into(),
            from_scm: false,
        }));
        worker.read_to_last();
    }

    pub(super) fn finish(mut self) {
        let mut worker = self.worker.take().expect("worker must be running");
        worker.soft_stop();
        assert!(worker.wait_for_server_stop(), "Sōzu worker did not stop");
        self.container
            .take()
            .expect("container must be running")
            .finish();
    }
}

impl Drop for ServiceHarness {
    fn drop(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            worker.hard_stop();
            let _ = worker.wait_for_server_stop();
        }
    }
}

pub(super) async fn eventually<T, E, F, Fut>(
    context: &str,
    timeout: Duration,
    mut operation: F,
) -> T
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_error = None;
    while tokio::time::Instant::now() < deadline {
        match operation().await {
            Ok(value) => return value,
            Err(error) => last_error = Some(error.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "{context} did not become ready before {timeout:?}: {}",
        last_error.as_deref().unwrap_or("no attempt completed")
    );
}

fn artifact_root() -> PathBuf {
    env::var_os("SOZU_PROTOCOL_SERVICE_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("sozu-protocol-services"))
}

fn owned_run_id() -> String {
    let run_id = env::var("SOZU_PROTOCOL_SERVICE_RUN_ID")
        .unwrap_or_else(|_| format!("local-{}", std::process::id()));
    assert!(
        !run_id.is_empty()
            && run_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "SOZU_PROTOCOL_SERVICE_RUN_ID must contain only ASCII letters, digits, '-', '_' or '.'"
    );
    run_id
}

fn wait_for_published_port(
    engine: &ContainerEngine,
    id: &str,
    container_port: u16,
    protocol: ContainerProtocol,
    timeout: Duration,
    log_path: &Path,
) -> SocketAddr {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(output) = engine.output([
            "port",
            id,
            &format!("{container_port}/{}", protocol.docker_suffix()),
        ]) && output.status.success()
        {
            let line = String::from_utf8_lossy(&output.stdout);
            if let Some(port) = line.lines().find_map(|line| {
                line.rsplit_once(':')
                    .and_then(|(_, port)| port.parse().ok())
            }) {
                return SocketAddr::new(LOOPBACK, port);
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = fs::write(log_path, b"container never published its requested port\n");
    panic!(
        "container {id} never published {}/{}",
        container_port,
        protocol.docker_suffix()
    );
}

fn wait_for_tcp(
    address: SocketAddr,
    timeout: Duration,
    service: &str,
    engine: &ContainerEngine,
    id: &str,
    log_path: &Path,
) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let output = engine.output(["logs", id]).ok();
    if let Some(output) = output {
        let mut bytes = output.stdout;
        bytes.extend_from_slice(&output.stderr);
        if let Some(parent) = log_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(log_path, bytes);
    }
    panic!("{service} did not accept TCP connections at {address} before {timeout:?}");
}

fn wait_for_healthy(
    engine: &ContainerEngine,
    id: &str,
    timeout: Duration,
    service: &str,
    log_path: &Path,
) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(output) = engine.output([
            "inspect",
            "--format",
            "{{.State.Health.Status}}|{{.State.Status}}",
            id,
        ]) && output.status.success()
        {
            let state = String::from_utf8_lossy(&output.stdout);
            if state.trim() == "healthy|running" {
                return;
            }
            if state.trim().ends_with("|exited") || state.trim().ends_with("|dead") {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    let output = engine.output(["logs", id]).ok();
    if let Some(output) = output {
        let mut bytes = output.stdout;
        bytes.extend_from_slice(&output.stderr);
        if let Some(parent) = log_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(log_path, bytes);
    }
    panic!("{service} container did not report healthy before {timeout:?}");
}

#[cfg(all(test, feature = "service-redis"))]
mod tests {
    use std::{
        fs,
        panic::{AssertUnwindSafe, catch_unwind},
        path::PathBuf,
        time::Duration,
    };

    use super::{
        ContainerEngine, ContainerSpec, OWNER_LABEL, RUN_LABEL, artifact_root, owned_run_id,
    };

    struct TestRunCleanup {
        engine: ContainerEngine,
        owner_prefix: String,
        run_id: String,
    }

    impl TestRunCleanup {
        fn new(service: &str) -> Self {
            Self {
                engine: ContainerEngine::discover(),
                owner_prefix: format!("{service}-{}-", std::process::id()),
                run_id: owned_run_id(),
            }
        }

        fn try_owned_ids(&self) -> Result<Vec<String>, String> {
            let output = self
                .engine
                .output([
                    "ps",
                    "--all",
                    "--quiet",
                    "--filter",
                    &format!("label={RUN_LABEL}={}", self.run_id),
                ])
                .map_err(|error| format!("list test-owned containers: {error}"))?;
            if !output.status.success() {
                return Err(format!(
                    "could not list test-owned containers: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Ok(String::from_utf8(output.stdout)
                .map_err(|error| format!("container IDs were not UTF-8: {error}"))?
                .lines()
                .filter(|id| {
                    let Ok(inspect) = self.engine.output([
                        "inspect",
                        "--format",
                        &format!("{{{{index .Config.Labels \"{OWNER_LABEL}\"}}}}"),
                        id,
                    ]) else {
                        return false;
                    };
                    inspect.status.success()
                        && String::from_utf8_lossy(&inspect.stdout)
                            .trim()
                            .starts_with(&self.owner_prefix)
                })
                .map(str::to_owned)
                .collect())
        }

        fn owned_ids(&self) -> Vec<String> {
            self.try_owned_ids().expect("list test-owned containers")
        }
    }

    impl Drop for TestRunCleanup {
        fn drop(&mut self) {
            let Ok(current) = ContainerEngine::probe(self.engine.executable.clone()) else {
                return;
            };
            if current.identity != self.engine.identity {
                return;
            }
            if let Ok(ids) = self.try_owned_ids() {
                for id in ids {
                    let _ = self.engine.output(["rm", "--volumes", "--force", &id]);
                }
            }
        }
    }

    fn artifact(service: &str, suffix: &str) -> PathBuf {
        let prefix = format!("sozu-e2e-{service}-{}-", std::process::id());
        let matches = fs::read_dir(artifact_root())
            .expect("read fixture artifact directory")
            .map(|entry| entry.expect("read fixture artifact entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(suffix))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            matches.len(),
            1,
            "expected one {suffix} artifact for {service}, found {matches:?}"
        );
        matches.into_iter().next().expect("one artifact path")
    }

    fn assert_artifact_volumes_removed(engine: &ContainerEngine, service: &str) {
        let volumes = fs::read_to_string(artifact(service, ".volumes"))
            .expect("read owned container volume list");
        assert!(
            !volumes.trim().is_empty(),
            "fixture image declared no volume"
        );
        for volume in volumes.lines().filter(|name| !name.is_empty()) {
            let inspect = engine
                .output(["volume", "inspect", volume])
                .expect("inspect owned container volume");
            assert!(
                !inspect.status.success(),
                "owned anonymous volume {volume} remained after startup failure"
            );
        }
    }

    #[test]
    fn failed_owned_container_preserves_exit_diagnostics_and_is_removed() {
        let service = "fixture-crash";
        let cleanup = TestRunCleanup::new(service);
        let result = catch_unwind(AssertUnwindSafe(|| {
            super::OwnedContainer::start(
                ContainerSpec::new(service, super::super::redis::IMAGE, 6379)
                    .command(["sh", "-c", "printf fixture-exit-marker; exit 23"])
                    .startup_timeout(Duration::from_secs(3)),
            );
        }));
        assert!(result.is_err(), "exited fixture unexpectedly became ready");

        let log = fs::read_to_string(artifact(service, ".log"))
            .expect("read exited fixture container log");
        assert!(
            log.contains("fixture-exit-marker"),
            "exited fixture log did not preserve its marker: {log:?}"
        );
        let state =
            fs::read_to_string(artifact(service, ".state")).expect("read exited fixture state");
        assert!(
            state.contains("|exited|false|23|false|"),
            "exited fixture state did not preserve exit 23: {state:?}"
        );
        assert!(
            cleanup.owned_ids().is_empty(),
            "exited fixture container remained after startup failure"
        );
        assert_artifact_volumes_removed(&cleanup.engine, service);
    }

    #[test]
    fn unhealthy_owned_container_is_removed_after_startup_timeout() {
        let service = "fixture-unhealthy";
        let cleanup = TestRunCleanup::new(service);
        let result = catch_unwind(AssertUnwindSafe(|| {
            super::OwnedContainer::start(
                ContainerSpec::new(service, super::super::redis::IMAGE, 6379)
                    .command(["redis-server", "--save", "", "--appendonly", "no"])
                    .health_command("exit 1")
                    .startup_timeout(Duration::from_secs(2)),
            );
        }));
        assert!(
            result.is_err(),
            "unhealthy fixture unexpectedly became ready"
        );

        let state =
            fs::read_to_string(artifact(service, ".state")).expect("read unhealthy fixture state");
        assert!(
            state.contains("|running|true|0|false|"),
            "unhealthy fixture was not alive at cleanup: {state:?}"
        );
        assert!(
            cleanup.owned_ids().is_empty(),
            "unhealthy fixture container remained after startup timeout"
        );
        assert_artifact_volumes_removed(&cleanup.engine, service);
    }
}
