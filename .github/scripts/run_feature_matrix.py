#!/usr/bin/env python3
"""Run and resume Sōzu feature-matrix campaigns outside GitHub Actions."""

from __future__ import annotations

import argparse
import concurrent.futures
import contextlib
import dataclasses
import gzip
import hashlib
import json
import os
import pathlib
import resource
import select
import shutil
import signal
import stat
import subprocess
import tempfile
import threading
import time
import tomllib
from collections import Counter
from collections.abc import Iterable, Iterator, Mapping, Sequence, Set

import feature_matrix


PROCESS_TESTS = (
    "load_state_rollback_e2e",
    "frontend_validate_before_commit_e2e",
    "listener_validate_before_commit_e2e",
    "sigterm_soft_stop_e2e",
    "upgrade_fd_inheritance_e2e",
    "upgrade_keeps_draining_worker_e2e",
    "main_upgrade_transfer_rejection_e2e",
    "soft_stop_waits_for_drain_e2e",
    "upgrade_pid_file_failure_e2e",
)
SIM_SUITES = (
    "udp_simulation",
    "tcp_preread_sim",
    "h2_simulation",
    "backend_selection_sim",
    "metrics_lease_sim",
)
FUZZ_TARGETS = (
    "fuzz_frame_parser",
    "fuzz_hpack_decoder",
    "fuzz_hpack_roundtrip",
    "fuzz_udp_flow",
    "fuzz_tcp_clienthello",
    "fuzz_command_channel",
)
ALL_SUITES = {
    "bin",
    "command",
    "e2e",
    "fuzz",
    "grpc",
    "lib",
    "process",
    "services",
    "sim",
    "tui-process",
}

MIN_FREE_DISK_BYTES = 100 * 1024**3
MIN_AVAILABLE_MEMORY_BYTES = 12 * 1024**3
MIN_FREE_SWAP_BYTES = 1024**3
MAX_LOAD_PER_CPU = 0.60
DEFAULT_COMMAND_TIMEOUT_SECONDS = 30 * 60
CONTROL_COMMAND_TIMEOUT_SECONDS = 60


@dataclasses.dataclass(frozen=True)
class CommandSpec:
    label: str
    argv: tuple[str, ...]
    environment: tuple[tuple[str, str], ...] = ()
    runtime_arguments: tuple[str, ...] = ()
    timeout_seconds: int = DEFAULT_COMMAND_TIMEOUT_SECONDS
    # A successful exit also requires this text in the command's output.
    required_output: str = ""

    def normalized(self) -> str:
        payload = dataclasses.asdict(self)
        payload["environment"] = [
            (name, "<runtime-owned>")
            if name in {
                "CARGO_TARGET_DIR",
                "SOZU_FEATURE_MATRIX_WORKER",
                "SOZU_PROTOCOL_SERVICE_RUN_ID",
                "TMPDIR",
                "XDG_RUNTIME_DIR",
            }
            else (name, value)
            for name, value in self.environment
        ]
        return json.dumps(_json_ready(payload), sort_keys=True, separators=(",", ":"))


@dataclasses.dataclass(frozen=True)
class SuiteCell:
    id: str
    suite: str
    projection: str
    projection_id: str
    product_ids: tuple[str, ...]
    config: feature_matrix.ProductConfig | None = None
    auxiliary: str | None = None


@dataclasses.dataclass(frozen=True)
class CampaignPlan:
    mode: str
    seed: int
    product_configs: tuple[feature_matrix.ProductConfig, ...]
    cells: tuple[SuiteCell, ...]

    def counts(self) -> dict[str, int]:
        return dict(sorted(Counter(cell.suite for cell in self.cells).items()))


@dataclasses.dataclass(frozen=True)
class WorkerSlot:
    id: int
    root: pathlib.Path

    @property
    def target_dir(self) -> pathlib.Path:
        return self.root / "target"

    @property
    def host_tmp_dir(self) -> pathlib.Path:
        return self.root / "tmp"

    def environment(self) -> tuple[tuple[str, str], ...]:
        return (
            ("CARGO_TARGET_DIR", str(self.target_dir)),
            ("SOZU_FEATURE_MATRIX_WORKER", str(self.id)),
        )


@dataclasses.dataclass(frozen=True)
class PreparedExecutable:
    label: str
    path: pathlib.Path
    sha256: str
    cwd: pathlib.Path
    arguments: tuple[str, ...]
    environment: tuple[tuple[str, str], ...]
    timeout_seconds: int = DEFAULT_COMMAND_TIMEOUT_SECONDS


@dataclasses.dataclass(frozen=True)
class PreparedCell:
    cell: SuiteCell
    slot: WorkerSlot
    specs: tuple[CommandSpec, ...]
    executables: tuple[PreparedExecutable, ...]
    compile_commands: tuple[dict[str, object], ...]
    graph: bytes
    inventory: bytes
    has_doctest_target: bool
    sozu_binary: pathlib.Path | None
    sozu_binary_sha256: str | None
    manifest_sha256: str


def _rust_tool(binary: str, *arguments: str) -> tuple[str, ...]:
    mode = os.environ.get("SOZU_FEATURE_MATRIX_TOOLCHAIN", "mise")
    if mode == "mise":
        return ("mise", "exec", "rust@1.93.1", "--", binary, *arguments)
    if mode == "path":
        return (binary, *arguments)
    raise ValueError("SOZU_FEATURE_MATRIX_TOOLCHAIN must be 'mise' or 'path'")


def _cargo(*arguments: str) -> tuple[str, ...]:
    return _rust_tool("cargo", *arguments)


def _feature_arguments(config: feature_matrix.ProductConfig, projection: str, *extra: str) -> tuple[str, ...]:
    features = (*config.features_for(projection), *extra)
    arguments = ["--no-default-features"]
    if features:
        arguments.extend(("--features", ",".join(features)))
    return tuple(arguments)


def _service_run_id(
    config: feature_matrix.ProductConfig, service: str, attempt_id: str | None = None
) -> str:
    base = os.environ.get(
        "SOZU_PROTOCOL_SERVICE_RUN_ID",
        f"feature-swarm-{config.id}-{service}",
    )
    return f"{base}-{attempt_id}" if attempt_id is not None else base


def _service_attempt_specs(
    cell: SuiteCell, specs: Sequence[CommandSpec], attempt_id: str
) -> tuple[CommandSpec, ...]:
    if cell.suite != "services" or cell.config is None or cell.auxiliary is None:
        return tuple(specs)
    run_id = _service_run_id(cell.config, cell.auxiliary, attempt_id)
    updated = []
    for spec in specs:
        environment = tuple(
            (name, run_id if name == "SOZU_PROTOCOL_SERVICE_RUN_ID" else value)
            for name, value in spec.environment
        )
        updated.append(dataclasses.replace(spec, environment=environment))
    return tuple(updated)


def command_specs(cell: SuiteCell, *, jobs: int, fuzz_seconds: int = 300) -> tuple[CommandSpec, ...]:
    config = cell.config
    release = ("--release", "--locked", f"-j{jobs}")
    if cell.suite == "bin" and config is not None:
        flags = _feature_arguments(config, "bin")
        return (
            CommandSpec("build-production-binary", _cargo("build", "-p", "sozu", *release, *flags)),
            CommandSpec("test-binary", _cargo("test", "-p", "sozu", *release, *flags, "--verbose")),
        )
    if cell.suite == "lib" and config is not None:
        return (
            CommandSpec(
                "test-library",
                _cargo("test", "-p", "sozu-lib", *release, *_feature_arguments(config, "lib"), "--verbose"),
            ),
        )
    if cell.suite == "e2e" and config is not None:
        return (
            CommandSpec(
                "test-e2e",
                _cargo(
                    "test", "-p", "sozu-e2e", *release,
                    *_feature_arguments(config, "e2e"), "--verbose", "--", "--skip", "tests::fuzz_tests::",
                ),
                runtime_arguments=("--skip", "tests::fuzz_tests::"),
            ),
        )
    if cell.suite == "command" and config is not None:
        return (
            CommandSpec(
                "test-command-library",
                _cargo(
                    "test", "-p", "sozu-command-lib", *release,
                    *_feature_arguments(config, "command"), "--verbose",
                ),
            ),
        )
    if cell.suite == "process" and config is not None:
        flags = _feature_arguments(config, "bin")
        return tuple(
            CommandSpec(
                f"process-{target}",
                _cargo("test", "-p", "sozu", *release, *flags, "--test", target, "--", "--ignored"),
                runtime_arguments=("--ignored",),
            )
            for target in PROCESS_TESTS
        )
    if cell.suite == "tui-process" and config is not None:
        flags = _feature_arguments(config, "bin")
        return (
            CommandSpec(
                "test-tui-unit",
                _cargo("test", "-p", "sozu", *release, *flags, "--lib", "--verbose"),
            ),
            CommandSpec(
                "test-tui-process",
                _cargo(
                    "test", "-p", "sozu", *release, *flags,
                    "--test", "sozu_top_e2e", "--", "--include-ignored",
                ),
                runtime_arguments=("--include-ignored",),
            ),
        )
    if cell.suite == "grpc" and config is not None:
        return (
            CommandSpec(
                "test-grpc",
                _cargo(
                    "test", "-p", "sozu-e2e", *release,
                    *_feature_arguments(config, "e2e", "grpc-e2e"),
                    "--verbose", "--", "tests::grpc_tests::",
                ),
                runtime_arguments=("tests::grpc_tests::",),
            ),
        )
    if cell.suite == "services" and config is not None and cell.auxiliary is not None:
        service = feature_matrix.SERVICES[cell.auxiliary]
        run_id = _service_run_id(config, cell.auxiliary)
        environment = (
            ("SOZU_CONTAINER_ENGINE", "docker"),
            ("SOZU_PROTOCOL_SERVICE_RUN_ID", run_id),
        )
        commands = [
            CommandSpec("pull-service-image", ("docker", "pull", service.image)),
            CommandSpec(
                f"test-service-{cell.auxiliary}",
                _cargo(
                    "test", "-p", "sozu-e2e", *release,
                    *_feature_arguments(config, "e2e", service.feature),
                    service.test, "--", "--exact", "--nocapture", "--test-threads=1",
                ),
                environment,
                (service.test, "--exact", "--nocapture", "--test-threads=1"),
            ),
        ]
        if cell.auxiliary == "redis":
            commands.append(
                CommandSpec(
                    "test-service-fixture-cleanup",
                    _cargo(
                        "test", "-p", "sozu-e2e", *release,
                        *_feature_arguments(config, "e2e", service.feature),
                        "tests::real_services_tcp::fixture::tests::", "--",
                        "--nocapture", "--test-threads=1",
                    ),
                    environment,
                    ("tests::real_services_tcp::fixture::tests::", "--nocapture", "--test-threads=1"),
                    required_output="test result: ok. 2 passed; 0 failed;",
                )
            )
        return tuple(commands)
    if cell.suite == "sim" and cell.auxiliary is not None:
        return (
            CommandSpec(
                f"simulation-{cell.auxiliary}",
                _cargo("test", "-p", "sozu-sim", "--locked", f"-j{jobs}", "--test", cell.auxiliary, "--", "--nocapture"),
                (("RUSTFLAGS", "--cfg tokio_unstable"),),
                ("--nocapture",),
            ),
        )
    if cell.suite == "fuzz" and cell.auxiliary is not None:
        return (
            CommandSpec(
                f"fuzz-{cell.auxiliary}",
                (
                    "mise", "exec", "rust@nightly", "--", "cargo", "fuzz", "run",
                    cell.auxiliary, "--", f"-max_total_time={fuzz_seconds}",
                ),
            ),
        )
    raise ValueError(f"unsupported campaign cell: {cell.id}")


def campaign_command_specs(
    cell: SuiteCell, *, jobs: int, timeout_seconds: int
) -> tuple[CommandSpec, ...]:
    if timeout_seconds <= 0:
        raise ValueError("command timeout must be positive")
    return tuple(
        dataclasses.replace(spec, timeout_seconds=timeout_seconds)
        for spec in command_specs(cell, jobs=jobs)
    )


def _projection_cells(
    configs: Sequence[feature_matrix.ProductConfig], projection: str
) -> list[tuple[feature_matrix.ProductConfig, tuple[str, ...]]]:
    grouped: dict[str, list[feature_matrix.ProductConfig]] = {}
    for config in configs:
        grouped.setdefault(config.projection_id(projection), []).append(config)
    return [
        (members[0], tuple(config.id for config in members))
        for _, members in sorted(grouped.items())
    ]


def build_campaign_plan(*, mode: str, seed: int) -> CampaignPlan:
    if mode == "bounded":
        configs = feature_matrix.bounded_product_configs(seed)
    elif mode == "exhaustive":
        configs = feature_matrix.exhaustive_product_configs()
    else:
        raise ValueError(f"unknown campaign mode: {mode}")

    cells: list[SuiteCell] = []
    for projection in ("bin", "lib", "e2e", "command"):
        for config, product_ids in _projection_cells(configs, projection):
            projection_id = config.projection_id(projection)
            cells.append(
                SuiteCell(
                    id=f"{projection}/{projection_id}",
                    suite=projection,
                    projection=projection,
                    projection_id=projection_id,
                    product_ids=product_ids,
                    config=config,
                )
            )

    for config in configs:
        cells.append(
            SuiteCell(
                id=f"process/{config.id}",
                suite="process",
                projection="bin",
                projection_id=config.id,
                product_ids=(config.id,),
                config=config,
            )
        )
        if "tui" in config.enabled:
            cells.append(
                SuiteCell(
                    id=f"tui-process/{config.id}",
                    suite="tui-process",
                    projection="bin",
                    projection_id=config.id,
                    product_ids=(config.id,),
                    config=config,
                )
            )

    for config, product_ids in _projection_cells(configs, "e2e"):
        projection_id = config.projection_id("e2e")
        cells.append(
            SuiteCell(
                id=f"grpc/{projection_id}",
                suite="grpc",
                projection="e2e",
                projection_id=projection_id,
                product_ids=product_ids,
                config=config,
            )
        )
        for service in feature_matrix.SERVICES:
            cells.append(
                SuiteCell(
                    id=f"services/{service}/{projection_id}",
                    suite="services",
                    projection="e2e",
                    projection_id=projection_id,
                    product_ids=product_ids,
                    config=config,
                    auxiliary=service,
                )
            )

    cells.extend(
        SuiteCell(
            id=f"sim/{suite}",
            suite="sim",
            projection="fixed-auxiliary",
            projection_id="fixed-ring",
            product_ids=(),
            auxiliary=suite,
        )
        for suite in SIM_SUITES
    )
    cells.extend(
        SuiteCell(
            id=f"fuzz/{target}",
            suite="fuzz",
            projection="fixed-auxiliary",
            projection_id="fixed-ring",
            product_ids=(),
            auxiliary=target,
        )
        for target in FUZZ_TARGETS
    )

    if len({cell.id for cell in cells}) != len(cells):
        raise ValueError("campaign plan contains duplicate cell identifiers")
    return CampaignPlan(mode, seed, tuple(configs), tuple(cells))


def validate_terminal_summary(plan: CampaignPlan, statuses: Mapping[str, str]) -> None:
    expected = {cell.id for cell in plan.cells}
    actual = set(statuses)
    non_terminal = sorted(
        cell_id for cell_id, status in statuses.items() if status not in {"success", "failed"}
    )
    missing = sorted(expected.difference(actual))
    extra = sorted(actual.difference(expected))
    if non_terminal or missing or extra:
        raise ValueError(
            "campaign is incomplete: "
            f"non_terminal={non_terminal[:8]} missing={missing[:8]} extra={extra[:8]}"
        )


def _sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _sha256_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def source_fingerprint(repo_root: pathlib.Path) -> str:
    head = subprocess.run(
        ("git", "rev-parse", "HEAD"),
        cwd=repo_root,
        check=True,
        capture_output=True,
        timeout=CONTROL_COMMAND_TIMEOUT_SECONDS,
    ).stdout
    diff = subprocess.run(
        ("git", "diff", "--binary", "HEAD", "--"),
        cwd=repo_root,
        check=True,
        capture_output=True,
        timeout=CONTROL_COMMAND_TIMEOUT_SECONDS,
    ).stdout
    untracked = subprocess.run(
        ("git", "ls-files", "--others", "--exclude-standard", "-z"),
        cwd=repo_root,
        check=True,
        capture_output=True,
        timeout=CONTROL_COMMAND_TIMEOUT_SECONDS,
    ).stdout.split(b"\0")
    payload = bytearray(b"HEAD\0" + head + b"DIFF\0" + diff)
    for raw_path in sorted(path for path in untracked if path):
        path = pathlib.Path(os.fsdecode(raw_path))
        if path.parts and path.parts[0] == "target":
            continue
        absolute = repo_root / path
        if not absolute.is_file():
            continue
        payload.extend(b"UNTRACKED\0" + raw_path + b"\0")
        payload.extend(absolute.read_bytes())
    return _sha256_bytes(bytes(payload))


class SourceDriftError(RuntimeError):
    """Raised when a campaign no longer runs against its admitted source."""


class _RetainingWriter:
    """Forward command output to the log while retaining a copy to inspect."""

    def __init__(self, destination: object) -> None:
        self.destination = destination
        self.retained = bytearray()

    def write(self, chunk: bytes) -> int:
        self.retained.extend(chunk)
        return self.destination.write(chunk)


class OwnedProcessGroupError(RuntimeError):
    """Raised when an attempt-owned process group cannot be reconciled."""


def require_source_fingerprint(
    repo_root: pathlib.Path, expected: str, *, phase: str
) -> None:
    """Fail closed before source-dependent work or receipt publication."""
    try:
        observed = source_fingerprint(repo_root)
    except (OSError, subprocess.SubprocessError, ValueError) as error:
        raise SourceDriftError(
            f"cannot revalidate campaign source during {phase}: "
            f"{type(error).__name__}: {error}"
        ) from error
    if observed != expected:
        raise SourceDriftError(
            f"campaign source changed during {phase}: "
            f"expected={expected} observed={observed}"
        )


def toolchain_fingerprint(
    repo_root: pathlib.Path, *, include_fuzz: bool = False
) -> str:
    outputs = [f"mode={os.environ.get('SOZU_FEATURE_MATRIX_TOOLCHAIN', 'mise')}".encode()]
    commands = [
        _rust_tool("cargo", "--version"),
        _rust_tool("rustc", "--version", "--verbose"),
    ]
    if include_fuzz:
        commands.extend(
            (
                ("mise", "exec", "rust@nightly", "--", "cargo", "--version"),
                ("mise", "exec", "rust@nightly", "--", "rustc", "--version", "--verbose"),
                ("mise", "exec", "rust@nightly", "--", "cargo", "fuzz", "--version"),
            )
        )
    for argv in commands:
        result = subprocess.run(
            argv,
            cwd=repo_root,
            check=True,
            capture_output=True,
            timeout=CONTROL_COMMAND_TIMEOUT_SECONDS,
        )
        outputs.extend((b"argv=" + b"\0".join(item.encode() for item in argv), result.stdout, result.stderr))
    return _sha256_bytes(b"\0".join(outputs))


def campaign_toolchains(
    repo_root: pathlib.Path, cells: Sequence[SuiteCell]
) -> dict[str, str]:
    fingerprints = {"stable": toolchain_fingerprint(repo_root)}
    if any(cell.suite == "fuzz" for cell in cells):
        fingerprints["fuzz"] = toolchain_fingerprint(repo_root, include_fuzz=True)
    return fingerprints


def _cell_toolchain(cell: SuiteCell, fingerprints: Mapping[str, str]) -> str:
    return fingerprints["fuzz" if cell.suite == "fuzz" else "stable"]


def _campaign_toolchain_fingerprint(fingerprints: Mapping[str, str]) -> str:
    if set(fingerprints) == {"stable"}:
        return fingerprints["stable"]
    return _sha256_bytes(
        json.dumps(fingerprints, sort_keys=True, separators=(",", ":")).encode()
    )


def generator_fingerprint(repo_root: pathlib.Path) -> str:
    scripts = (
        repo_root / ".github" / "scripts" / "feature_matrix.py",
        repo_root / ".github" / "scripts" / "run_feature_matrix.py",
    )
    return _sha256_bytes(b"\0".join(path.read_bytes() for path in scripts))


def _meminfo_bytes() -> dict[str, int]:
    values: dict[str, int] = {}
    for line in pathlib.Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
        name, raw = line.split(":", 1)
        fields = raw.split()
        if fields:
            values[name] = int(fields[0]) * 1024
    return values


def resource_snapshot(repo_root: pathlib.Path) -> dict[str, object]:
    filesystem = shutil.disk_usage(repo_root)
    memory = _meminfo_bytes()
    load_1, load_5, load_15 = os.getloadavg()
    target = repo_root / "target"
    target_bytes = 0
    if target.exists():
        result = subprocess.run(
            ("du", "-sb", str(target)),
            check=True,
            capture_output=True,
            text=True,
            timeout=CONTROL_COMMAND_TIMEOUT_SECONDS,
        )
        target_bytes = int(result.stdout.split()[0])
    return {
        "disk_free_bytes": filesystem.free,
        "memory_available_bytes": memory.get("MemAvailable", 0),
        "swap_free_bytes": memory.get("SwapFree", 0),
        "load": [load_1, load_5, load_15],
        "cpu_count": os.cpu_count() or 1,
        "target_bytes": target_bytes,
        "timestamp_unix_ns": time.time_ns(),
    }


def validate_capacity(
    snapshot: Mapping[str, object], *, check_load: bool = True
) -> None:
    failures = []
    if int(snapshot["disk_free_bytes"]) < MIN_FREE_DISK_BYTES:
        failures.append("free disk below 100 GiB")
    if int(snapshot["memory_available_bytes"]) < MIN_AVAILABLE_MEMORY_BYTES:
        failures.append("MemAvailable below 12 GiB")
    if int(snapshot["swap_free_bytes"]) < MIN_FREE_SWAP_BYTES:
        failures.append("SwapFree below 1 GiB")
    load_1 = float(list(snapshot["load"])[0])
    cpu_count = int(snapshot["cpu_count"])
    if check_load and load_1 > cpu_count * MAX_LOAD_PER_CPU:
        failures.append(f"one-minute load {load_1:.2f} exceeds 60% of {cpu_count} CPUs")
    if failures:
        raise RuntimeError("capacity gate failed: " + "; ".join(failures))


def _package_for_cell(cell: SuiteCell) -> tuple[str, str] | None:
    if cell.suite in {"bin", "process", "tui-process"}:
        return ("sozu", "bin")
    if cell.suite == "lib":
        return ("sozu-lib", "lib")
    if cell.suite == "command":
        return ("sozu-command-lib", "command")
    if cell.suite in {"e2e", "grpc", "services"}:
        return ("sozu-e2e", "e2e")
    return None


def _package_working_directory(
    cell: SuiteCell, repo_root: pathlib.Path
) -> pathlib.Path:
    package = _package_for_cell(cell)
    if package is None:
        return repo_root
    package_name = package[0]
    relative = {
        "sozu": "bin",
        "sozu-command-lib": "command",
        "sozu-e2e": "e2e",
        "sozu-lib": "lib",
    }[package_name]
    return repo_root / relative


def _extra_features(cell: SuiteCell) -> tuple[str, ...]:
    if cell.suite == "grpc":
        return ("grpc-e2e",)
    if cell.suite == "services" and cell.auxiliary is not None:
        return (feature_matrix.SERVICES[cell.auxiliary].feature,)
    return ()


def effective_feature_graph(
    cell: SuiteCell,
    repo_root: pathlib.Path,
    *,
    timeout_seconds: int = DEFAULT_COMMAND_TIMEOUT_SECONDS,
) -> bytes:
    package = _package_for_cell(cell)
    if package is None or cell.config is None:
        return f"fixed-auxiliary:{cell.id}".encode()
    package_name, projection = package
    argv = _cargo(
        "tree", "-p", package_name, "--locked", "-e", "features",
        "--prefix", "none", "--format", "{p}|{f}",
        *_feature_arguments(cell.config, projection, *_extra_features(cell)),
    )
    output = subprocess.run(
        argv,
        cwd=repo_root,
        check=True,
        capture_output=True,
        timeout=timeout_seconds,
    ).stdout
    text = output.decode("utf-8", errors="strict")
    _validate_effective_feature_graph(cell, text, repo_root)
    return output


def _manifest_local_feature_closure(
    manifest_path: pathlib.Path, requested: Iterable[str]
) -> set[str]:
    with manifest_path.open("rb") as handle:
        manifest = tomllib.load(handle)
    table = manifest.get("features", {})
    if not isinstance(table, Mapping):
        raise RuntimeError(f"manifest feature table is invalid: {manifest_path}")
    closure = set(requested)
    pending = list(closure)
    while pending:
        feature = pending.pop()
        values = table.get(feature, [])
        if not isinstance(values, list):
            raise RuntimeError(f"manifest feature {feature} is invalid: {manifest_path}")
        for value in values:
            if not isinstance(value, str):
                continue
            if value in table and value not in closure:
                closure.add(value)
                pending.append(value)
    return closure


def _package_features_from_tree(text: str, package_name: str) -> set[str] | None:
    rows = [line for line in text.splitlines() if line.startswith(f"{package_name} v")]
    if not rows:
        return None
    features: set[str] = set()
    for row in rows:
        _, separator, raw_features = row.partition("|")
        if not separator:
            raise RuntimeError(f"effective feature row is malformed: {row}")
        features.update(feature for feature in raw_features.split(",") if feature)
    return features


def _validate_package_features(
    text: str,
    package_name: str,
    manifest_path: pathlib.Path,
    requested: Iterable[str],
) -> None:
    expected = _manifest_local_feature_closure(manifest_path, requested)
    actual = _package_features_from_tree(text, package_name)
    if actual is None:
        raise RuntimeError(f"effective feature graph omitted package {package_name}")
    if actual != expected:
        raise RuntimeError(
            f"effective features for {package_name} differ: "
            f"missing={sorted(expected - actual)} unexpected={sorted(actual - expected)}"
        )


def _validate_effective_feature_graph(
    cell: SuiteCell, text: str, repo_root: pathlib.Path
) -> None:
    package = _package_for_cell(cell)
    if package is None or cell.config is None:
        return
    package_name, projection = package
    manifests = {
        "sozu": repo_root / "bin" / "Cargo.toml",
        "sozu-lib": repo_root / "lib" / "Cargo.toml",
        "sozu-command-lib": repo_root / "command" / "Cargo.toml",
        "sozu-e2e": repo_root / "e2e" / "Cargo.toml",
    }
    root_requested = set(cell.config.features_for(projection)).union(
        _extra_features(cell)
    )
    _validate_package_features(
        text, package_name, manifests[package_name], root_requested
    )
    if package_name != "sozu-command-lib":
        library_requested = set(cell.config.features_for("lib"))
        if cell.suite in {"e2e", "grpc", "services"}:
            library_requested.add("e2e-hooks")
        _validate_package_features(
            text,
            "sozu-lib",
            manifests["sozu-lib"],
            library_requested,
        )
    command_requested = (
        set()
        if package_name == "sozu-lib"
        else {
            feature
            for feature in cell.config.features_for("command")
            if feature in {"logs-debug", "logs-trace"}
        }
    )
    _validate_package_features(
        text,
        "sozu-command-lib",
        manifests["sozu-command-lib"],
        command_requested,
    )


def _inventory_command(spec: CommandSpec) -> tuple[str, ...] | None:
    argv = list(spec.argv)
    if "test" not in argv:
        return None
    separator = argv.index("--") if "--" in argv else len(argv)
    cargo_arguments = argv[:separator]
    test_arguments = argv[separator + 1 :] if separator < len(argv) else []
    filtered = [argument for argument in test_arguments if argument not in {"--nocapture"}]
    return tuple((*cargo_arguments, "--", *filtered, "--list"))


def _validate_e2e_inventory(cell: SuiteCell, payload: bytes) -> None:
    if cell.suite != "e2e" or cell.config is None:
        return
    tolerant = b"test_h1_tolerant_high_byte_method_no_ub"
    strict = b"test_h1_invalid_utf8_method_no_crash"
    tolerant_expected = "tolerant-http1-parser" in cell.config.enabled
    if (tolerant in payload) != tolerant_expected:
        raise RuntimeError(
            f"tolerant parser inventory mismatch for {cell.id}: "
            f"expected={tolerant_expected}"
        )
    strict_expected = not tolerant_expected
    if (strict in payload) != strict_expected:
        raise RuntimeError(
            f"strict parser inventory mismatch for {cell.id}: "
            f"expected={strict_expected}"
        )
    proxy_peer = b"test_h2_proxy_protocol_peer_is_the_advertised_client"
    if proxy_peer not in payload:
        raise RuntimeError(
            f"H2 PROXY peer inventory mismatch for {cell.id}: expected=True"
        )


def test_inventory(
    cell: SuiteCell, specs: Sequence[CommandSpec], repo_root: pathlib.Path
) -> bytes:
    inventories = []
    for spec in specs:
        argv = _inventory_command(spec)
        if argv is None:
            continue
        environment = os.environ.copy()
        environment.update(spec.environment)
        result = subprocess.run(
            argv,
            cwd=repo_root,
            env=environment,
            check=True,
            capture_output=True,
            timeout=spec.timeout_seconds,
        )
        if b": test" not in result.stdout:
            raise RuntimeError(f"test inventory is empty for {cell.id}/{spec.label}")
        inventories.append(spec.label.encode() + b"\0" + result.stdout)
    if not inventories:
        if cell.suite == "fuzz":
            output = subprocess.run(
                ("mise", "exec", "rust@nightly", "--", "cargo", "fuzz", "list"),
                cwd=repo_root / "fuzz",
                check=True,
                capture_output=True,
                timeout=max(spec.timeout_seconds for spec in specs),
            ).stdout
            if cell.auxiliary is None or cell.auxiliary.encode() not in output.splitlines():
                raise RuntimeError(f"fuzz inventory omitted {cell.auxiliary}")
            return output
        raise RuntimeError(f"no test inventory command for {cell.id}")
    payload = b"\0".join(inventories)
    _validate_e2e_inventory(cell, payload)
    return payload


def _environment(
    spec: CommandSpec, slot: WorkerSlot | None = None
) -> dict[str, str]:
    environment = os.environ.copy()
    environment.update(spec.environment)
    if slot is not None:
        environment.update(slot.environment())
    return environment


def _cargo_test_index(spec: CommandSpec) -> int | None:
    try:
        cargo_index = spec.argv.index("cargo")
    except ValueError:
        return None
    if len(spec.argv) <= cargo_index + 1 or spec.argv[cargo_index + 1] != "test":
        return None
    return cargo_index + 1


def _cargo_test_compile_argv(spec: CommandSpec) -> tuple[str, ...]:
    test_index = _cargo_test_index(spec)
    if test_index is None:
        raise ValueError(f"{spec.label} is not a cargo test command")
    try:
        separator = spec.argv.index("--", test_index + 1)
    except ValueError:
        separator = len(spec.argv)
    cargo_arguments = list(spec.argv[:separator])
    cargo_arguments.extend(("--no-run", "--message-format=json-render-diagnostics"))
    return tuple(cargo_arguments)


def _cargo_doctest_argv(spec: CommandSpec) -> tuple[str, ...] | None:
    """Return the doctest phase that a generic `cargo test` would have run."""
    test_index = _cargo_test_index(spec)
    if test_index is None:
        return None
    try:
        separator = spec.argv.index("--", test_index + 1)
    except ValueError:
        separator = len(spec.argv)
    cargo_arguments = spec.argv[test_index + 1 : separator]
    selectors = {
        "--lib",
        "--bin",
        "--bins",
        "--example",
        "--examples",
        "--test",
        "--tests",
        "--bench",
        "--benches",
        "--all-targets",
    }
    if any(argument in selectors for argument in cargo_arguments):
        return None
    argv = list(spec.argv)
    argv.insert(test_index + 1, "--doc")
    return tuple(argv)


def _metadata_package_has_doctests(payload: Mapping[str, object], package: str) -> bool:
    packages = payload.get("packages")
    if not isinstance(packages, list):
        raise RuntimeError("Cargo metadata omitted packages")
    matches = [item for item in packages if isinstance(item, Mapping) and item.get("name") == package]
    if len(matches) != 1:
        raise RuntimeError(f"Cargo metadata did not identify package {package} exactly once")
    targets = matches[0].get("targets")
    if not isinstance(targets, list):
        raise RuntimeError(f"Cargo metadata omitted targets for {package}")
    return any(
        isinstance(target, Mapping) and target.get("doctest") is True
        for target in targets
    )


def _package_has_doctest_target(
    package: str,
    repo_root: pathlib.Path,
    *,
    timeout_seconds: int = DEFAULT_COMMAND_TIMEOUT_SECONDS,
) -> bool:
    result = subprocess.run(
        _cargo("metadata", "--format-version", "1", "--no-deps", "--locked"),
        cwd=repo_root,
        check=True,
        capture_output=True,
        timeout=timeout_seconds,
    )
    payload = json.loads(result.stdout)
    if not isinstance(payload, Mapping):
        raise RuntimeError("Cargo metadata root is not an object")
    return _metadata_package_has_doctests(payload, package)


def _compiler_test_executables(output: bytes) -> tuple[pathlib.Path, ...]:
    paths: set[pathlib.Path] = set()
    for raw_line in output.splitlines():
        try:
            message = json.loads(raw_line)
        except (json.JSONDecodeError, UnicodeDecodeError):
            continue
        if message.get("reason") != "compiler-artifact":
            continue
        executable = message.get("executable")
        profile = message.get("profile")
        if not executable or not isinstance(profile, Mapping) or not profile.get("test"):
            continue
        paths.add(pathlib.Path(executable).resolve())
    if not paths:
        raise RuntimeError("cargo --no-run did not report any test executable")
    return tuple(sorted(paths))


def _reset_worker_tmp(slot: WorkerSlot) -> None:
    resolved_root = slot.root.resolve()
    resolved_tmp = slot.host_tmp_dir.resolve()
    if not resolved_tmp.is_relative_to(resolved_root):
        raise RuntimeError(f"worker tmp escaped its owned root: {resolved_tmp}")
    if slot.host_tmp_dir.exists():
        shutil.rmtree(slot.host_tmp_dir)
    slot.host_tmp_dir.mkdir(parents=True, mode=0o700)


def _prepared_manifest_path(slot: WorkerSlot) -> pathlib.Path:
    return slot.root / "prepared.json"


def _write_prepared_manifest(slot: WorkerSlot, payload: Mapping[str, object]) -> str:
    encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n"
    digest = _sha256_bytes(encoded.encode())
    target = _prepared_manifest_path(slot)
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=slot.root,
        prefix=".prepared.",
        delete=False,
    ) as handle:
        handle.write(encoded)
        temporary = pathlib.Path(handle.name)
    os.replace(temporary, target)
    return digest


def prepare_cell_for_worker(
    cell: SuiteCell,
    specs: Sequence[CommandSpec],
    *,
    repo_root: pathlib.Path,
    state_dir: pathlib.Path,
    slot: WorkerSlot,
) -> PreparedCell:
    """Compile one cell without overlapping any other Cargo process."""
    slot.root.mkdir(parents=True, exist_ok=True)
    _prepared_manifest_path(slot).unlink(missing_ok=True)
    slot.target_dir.mkdir(parents=True, exist_ok=True)
    _reset_worker_tmp(slot)
    compile_dir = state_dir / "builder-logs"
    compile_dir.mkdir(parents=True, exist_ok=True)
    compile_log = compile_dir / f"{_safe_log_name(cell.id)}--worker-{slot.id}.log.gz"
    compile_commands: list[dict[str, object]] = []
    prepared: list[PreparedExecutable] = []
    with gzip.open(compile_log, "wt", encoding="utf-8") as log:
        for spec in specs:
            test_index = _cargo_test_index(spec)
            argv = _cargo_test_compile_argv(spec) if test_index is not None else spec.argv
            log.write(f"COMMAND {json.dumps(argv)}\n")
            log.flush()
            started = time.monotonic()
            result = subprocess.run(
                argv,
                cwd=repo_root / "fuzz" if cell.suite == "fuzz" else repo_root,
                env=_environment(spec, slot),
                capture_output=True,
                timeout=spec.timeout_seconds,
            )
            elapsed = time.monotonic() - started
            log.write(result.stdout.decode("utf-8", errors="replace"))
            log.write(result.stderr.decode("utf-8", errors="replace"))
            compile_commands.append(
                {
                    "elapsed_seconds": elapsed,
                    "exit_code": result.returncode,
                    "label": spec.label,
                }
            )
            if result.returncode != 0:
                raise subprocess.CalledProcessError(
                    result.returncode,
                    argv,
                    output=result.stdout,
                    stderr=result.stderr,
                )
            if test_index is None:
                continue
            for executable in _compiler_test_executables(result.stdout):
                prepared.append(
                    PreparedExecutable(
                        label=f"{spec.label}:{executable.name}",
                        path=executable,
                        sha256=_sha256_file(executable),
                        cwd=_package_working_directory(cell, repo_root),
                        arguments=spec.runtime_arguments,
                        environment=tuple((*spec.environment, *slot.environment())),
                        timeout_seconds=spec.timeout_seconds,
                    )
                )

    inventories = []
    for executable in prepared:
        result = subprocess.run(
            (str(executable.path), *executable.arguments, "--list"),
            cwd=repo_root,
            env={**os.environ, **dict(executable.environment)},
            check=True,
            capture_output=True,
            timeout=executable.timeout_seconds,
        )
        if b": test" not in result.stdout:
            raise RuntimeError(f"test inventory is empty for {cell.id}/{executable.label}")
        inventories.append(executable.label.encode() + b"\0" + result.stdout)
    if not inventories:
        raise RuntimeError(f"no directly executable test artifact for {cell.id}")
    inventory = b"\0".join(inventories)
    package = _package_for_cell(cell)
    has_doctest_target = (
        _package_has_doctest_target(
            package[0],
            repo_root,
            timeout_seconds=max(spec.timeout_seconds for spec in specs),
        )
        if package is not None
        else False
    )
    _validate_e2e_inventory(cell, inventory)
    graph = effective_feature_graph(
        cell,
        repo_root,
        timeout_seconds=max(spec.timeout_seconds for spec in specs),
    )
    sozu_binary = slot.target_dir / "release" / "sozu"
    if not sozu_binary.is_file():
        sozu_binary = None
    manifest_payload = {
        "cell_id": cell.id,
        "has_doctest_target": has_doctest_target,
        "executables": [
            {
                "arguments": list(executable.arguments),
                "cwd": str(executable.cwd),
                "path": str(executable.path),
                "sha256": executable.sha256,
                "timeout_seconds": executable.timeout_seconds,
            }
            for executable in prepared
        ],
        "sozu_binary": str(sozu_binary) if sozu_binary is not None else None,
        "sozu_binary_sha256": (
            _sha256_file(sozu_binary) if sozu_binary is not None else None
        ),
        "worker": slot.id,
    }
    manifest_sha256 = _write_prepared_manifest(slot, manifest_payload)
    return PreparedCell(
        cell=cell,
        slot=slot,
        specs=tuple(specs),
        executables=tuple(prepared),
        compile_commands=tuple(compile_commands),
        graph=graph,
        inventory=inventory,
        has_doctest_target=has_doctest_target,
        sozu_binary=sozu_binary,
        sozu_binary_sha256=manifest_payload["sozu_binary_sha256"],
        manifest_sha256=manifest_sha256,
    )


_NAMESPACE_SETUP = r"""
set -euo pipefail
worker_tmp=$1
uid=$2
gid=$3
shift 3
mount --make-rprivate /
chmod 1777 "$worker_tmp"
mount --bind "$worker_tmp" /tmp
mkdir -m 700 /tmp/cargo-tmp /tmp/runtime
ip link set lo up
export TMPDIR=/tmp/cargo-tmp
export XDG_RUNTIME_DIR=/tmp/runtime
exec setpriv --no-new-privs --bounding-set=-all --inh-caps=-all \
  --ambient-caps=-all --reuid "$uid" --regid "$gid" --keep-groups "$@"
"""

_NAMESPACE_PROBE_SETUP = r"""
set -euo pipefail
worker_tmp=$1
uid=$2
gid=$3
worker=$4
mount --make-rprivate /
chmod 1777 "$worker_tmp"
mount --bind "$worker_tmp" /tmp
ip link set lo up
exec setpriv --no-new-privs --bounding-set=-all --inh-caps=-all \
  --ambient-caps=-all --reuid "$uid" --regid "$gid" --keep-groups \
  /usr/bin/bash -c '
    set -euo pipefail
    worker=$1
    probe=/tmp/feature-swarm-parallel-space-probe
    printf "READY %s\n" "$worker"
    IFS= read -r gate
    test "$gate" = allocate
    fallocate -l 1G "$probe"
    printf "%s" "$worker" | dd of="$probe" conv=notrunc status=none
    test "$(stat -c %s "$probe")" = 1073741824
    test "$(head -c "${#worker}" "$probe")" = "$worker"
    printf "ALLOCATED %s 1073741824\n" "$worker"
    IFS= read -r gate
    test "$gate" = release
    rm -- "$probe"
    test ! -e "$probe"
    printf "DONE %s\n" "$worker"
  ' feature-swarm-probe "$worker"
"""


def _parallel_namespace_probe_argv(slot: WorkerSlot) -> tuple[str, ...]:
    return (
        "unshare",
        "--user",
        "--map-current-user",
        "--keep-caps",
        "--net",
        "--mount",
        "--ipc",
        "--fork",
        "--kill-child",
        "/usr/bin/bash",
        "-c",
        _NAMESPACE_PROBE_SETUP,
        "feature-swarm-probe",
        str(slot.host_tmp_dir),
        str(os.getuid()),
        str(os.getgid()),
        str(slot.id),
    )


def _read_probe_lines(
    processes: Sequence[subprocess.Popen[bytes]], prefix: bytes, timeout: float
) -> list[str]:
    selector = select.poll()
    pending: dict[int, subprocess.Popen[bytes]] = {}
    for process in processes:
        assert process.stdout is not None
        selector.register(process.stdout, select.POLLIN)
        pending[process.stdout.fileno()] = process
    deadline = time.monotonic() + timeout
    lines: list[str] = []
    while pending:
        remaining_ms = max(0, int((deadline - time.monotonic()) * 1000))
        if remaining_ms == 0:
            raise TimeoutError(f"parallel namespace probe timed out waiting for {prefix!r}")
        events = selector.poll(remaining_ms)
        if not events:
            raise TimeoutError(f"parallel namespace probe timed out waiting for {prefix!r}")
        for file_descriptor, _ in events:
            process = pending.pop(file_descriptor)
            assert process.stdout is not None
            selector.unregister(process.stdout)
            line = process.stdout.readline()
            if not line.startswith(prefix):
                raise RuntimeError(
                    f"parallel namespace probe expected {prefix!r}, got {line!r}"
                )
            lines.append(line.decode("utf-8").strip())
    return sorted(lines)


def verify_parallel_namespace_isolation(
    slots: Sequence[WorkerSlot], *, timeout: float = 30.0
) -> dict[str, object]:
    """Prove that two private disk-backed /tmp mounts can coexist at 1 GiB."""
    if len(slots) != 2:
        raise ValueError("the initial namespace probe requires exactly two workers")
    processes: list[subprocess.Popen[bytes]] = []
    started_ns = time.time_ns()
    try:
        for slot in slots:
            _reset_worker_tmp(slot)
            process = subprocess.Popen(
                _parallel_namespace_probe_argv(slot),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
            )
            processes.append(process)
        ready = _read_probe_lines(processes, b"READY ", timeout)
        for process in processes:
            assert process.stdin is not None
            process.stdin.write(b"allocate\n")
            process.stdin.flush()
        allocated = _read_probe_lines(processes, b"ALLOCATED ", timeout)
        for process in processes:
            assert process.stdin is not None
            process.stdin.write(b"release\n")
            process.stdin.close()
        done = _read_probe_lines(processes, b"DONE ", timeout)
        for process in processes:
            try:
                exit_code = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired as error:
                raise TimeoutError("parallel namespace probe did not exit") from error
            if exit_code != 0:
                assert process.stderr is not None
                raise RuntimeError(
                    "parallel namespace probe failed: "
                    + process.stderr.read().decode("utf-8", errors="replace")
                )
        for slot in slots:
            if (slot.host_tmp_dir / "feature-swarm-parallel-space-probe").exists():
                raise RuntimeError(f"parallel namespace probe leaked worker {slot.id} data")
        return {
            "allocated": allocated,
            "bytes_per_worker": 1024**3,
            "done": done,
            "ended_unix_ns": time.time_ns(),
            "ready": ready,
            "started_unix_ns": started_ns,
        }
    except BaseException:
        for process in processes:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
        for process in processes:
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
        raise


def _namespaced_test_argv(
    executable: PreparedExecutable, slot: WorkerSlot
) -> tuple[str, ...]:
    return (
        "unshare",
        "--user",
        "--map-current-user",
        "--keep-caps",
        "--net",
        "--mount",
        "--ipc",
        "--fork",
        "--kill-child",
        "/usr/bin/bash",
        "-c",
        _NAMESPACE_SETUP,
        "feature-swarm-worker",
        str(slot.host_tmp_dir),
        str(os.getuid()),
        str(os.getgid()),
        str(executable.path),
        *executable.arguments,
    )


def _maximum_rss_from_time(path: pathlib.Path) -> int | None:
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if "Maximum resident set size (kbytes):" in line:
            return int(line.rsplit(":", 1)[1].strip())
    return None


def run_prepared_cell(
    prepared: PreparedCell,
    *,
    repo_root: pathlib.Path,
    state_dir: pathlib.Path,
    state: feature_matrix.CampaignState,
    identity: feature_matrix.CellIdentity,
    attempt_id: str,
) -> tuple[int, pathlib.Path, dict[str, object]]:
    """Execute immutable test artifacts without invoking Cargo or rustc."""
    manifest_path = _prepared_manifest_path(prepared.slot)
    if not manifest_path.is_file() or _sha256_file(manifest_path) != prepared.manifest_sha256:
        raise RuntimeError(f"prepared generation is missing or changed for {prepared.cell.id}")
    log_dir = state_dir / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    log_path = log_dir / f"{_safe_log_name(prepared.cell.id)}--{attempt_id}.log.gz"
    started_ns = time.time_ns()
    started = time.monotonic()
    exit_code = 0
    commands: list[dict[str, object]] = []
    try:
        with gzip.open(log_path, "wb") as log:
            for index, executable in enumerate(prepared.executables):
                _reset_worker_tmp(prepared.slot)
                if _sha256_file(executable.path) != executable.sha256:
                    raise RuntimeError(f"test artifact changed before execution: {executable.path}")
                time_path = prepared.slot.root / f"time-{attempt_id}-{index}.txt"
                argv = (
                    "/usr/bin/time",
                    "--verbose",
                    "--output",
                    str(time_path),
                    "--",
                    *_namespaced_test_argv(executable, prepared.slot),
                )
                log.write(f"COMMAND {json.dumps(argv)}\n".encode())
                log.flush()
                command_started_ns = time.time_ns()
                command_started = time.monotonic()
                environment = os.environ.copy()
                environment.update(executable.environment)
                environment[feature_matrix.ATTEMPT_ENVIRONMENT] = attempt_id
                process = subprocess.Popen(
                    argv,
                    cwd=executable.cwd,
                    env=environment,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                descendant_cleanup = [None]
                with _recorded_process_group(
                    process,
                    state=state,
                    identity=identity,
                    attempt_id=attempt_id,
                    destination=log,
                    cleanup_reason="owned process group survived test handling",
                ) as descendant_cleanup:
                    try:
                        command_exit = _copy_process_output(
                            process,
                            log,
                            timeout_seconds=executable.timeout_seconds,
                        )
                    except subprocess.TimeoutExpired:
                        command_exit = 124
                        log.write(
                            f"COMMAND TIMEOUT after {executable.timeout_seconds}s\n".encode()
                        )
                if descendant_cleanup[0] is not None:
                    command_exit = command_exit or 125
                if _sha256_file(executable.path) != executable.sha256:
                    raise RuntimeError(f"test artifact changed during execution: {executable.path}")
                commands.append(
                    {
                        "artifact": str(executable.path),
                        "artifact_sha256": executable.sha256,
                        "elapsed_seconds": time.monotonic() - command_started,
                        "ended_unix_ns": time.time_ns(),
                        "exit_code": command_exit,
                        "label": executable.label,
                        "maximum_rss_kib": _maximum_rss_from_time(time_path),
                        "owned_descendant_error": descendant_cleanup[0],
                        "started_unix_ns": command_started_ns,
                    }
                )
                if command_exit != 0 and exit_code == 0:
                    exit_code = command_exit
        if prepared.sozu_binary is not None:
            current = _sha256_file(prepared.sozu_binary)
            if current != prepared.sozu_binary_sha256:
                raise RuntimeError(
                    f"Sōzu binary changed during {prepared.cell.id}: {prepared.sozu_binary}"
                )
    finally:
        _reset_worker_tmp(prepared.slot)
    metrics = {
        "commands": commands,
        "compile_commands": list(prepared.compile_commands),
        "elapsed_seconds": time.monotonic() - started,
        "ended_unix_ns": time.time_ns(),
        "log_bytes": log_path.stat().st_size,
        "started_unix_ns": started_ns,
        "worker": prepared.slot.id,
    }
    return exit_code, log_path, metrics


def run_serial_doctests(
    prepared: PreparedCell,
    *,
    repo_root: pathlib.Path,
    state: feature_matrix.CampaignState,
    identity: feature_matrix.CellIdentity,
    attempt_id: str,
    log_path: pathlib.Path,
) -> tuple[int, list[dict[str, object]]]:
    """Run the doctest phase only after every direct worker in the batch exits."""
    exit_code = 0
    commands: list[dict[str, object]] = []
    if not prepared.has_doctest_target:
        return exit_code, commands
    with gzip.open(log_path, "ab") as log:
        for spec in prepared.specs:
            argv = _cargo_doctest_argv(spec)
            if argv is None:
                continue
            log.write(f"DOCTEST COMMAND {json.dumps(argv)}\n".encode())
            log.flush()
            started_ns = time.time_ns()
            started = time.monotonic()
            environment = _environment(spec, prepared.slot)
            environment[feature_matrix.ATTEMPT_ENVIRONMENT] = attempt_id
            process = subprocess.Popen(
                argv,
                cwd=repo_root,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            descendant_error = [None]
            with _recorded_process_group(
                process,
                state=state,
                identity=identity,
                attempt_id=attempt_id,
                destination=log,
                cleanup_reason="owned doctest process group survived command handling",
            ) as descendant_error:
                try:
                    command_exit = _copy_process_output(
                        process, log, timeout_seconds=spec.timeout_seconds
                    )
                except subprocess.TimeoutExpired:
                    command_exit = 124
                    log.write(
                        f"DOCTEST TIMEOUT after {spec.timeout_seconds}s\n".encode()
                    )
            if descendant_error[0] is not None:
                command_exit = command_exit or 125
            commands.append(
                {
                    "elapsed_seconds": time.monotonic() - started,
                    "ended_unix_ns": time.time_ns(),
                    "exit_code": command_exit,
                    "label": spec.label,
                    "owned_descendant_error": descendant_error[0],
                    "started_unix_ns": started_ns,
                }
            )
            if command_exit != 0 and exit_code == 0:
                exit_code = command_exit
    for executable in prepared.executables:
        if _sha256_file(executable.path) != executable.sha256:
            raise RuntimeError(
                f"test artifact changed during doctests: {executable.path}"
            )
    if prepared.sozu_binary is not None:
        if _sha256_file(prepared.sozu_binary) != prepared.sozu_binary_sha256:
            raise RuntimeError(
                f"Sōzu binary changed during doctests: {prepared.sozu_binary}"
            )
    return exit_code, commands


def _cell_identity(
    cell: SuiteCell,
    specs: Sequence[CommandSpec],
    *,
    repo_root: pathlib.Path,
    source: str,
    toolchain: str,
    generator: str,
    graph: bytes,
    inventory: bytes,
) -> feature_matrix.CellIdentity:
    return feature_matrix.CellIdentity(
        cell_id=cell.id,
        suite=cell.suite,
        source=source,
        cargo_lock=_sha256_file(repo_root / "Cargo.lock"),
        toolchain=toolchain,
        generator=generator,
        command=_sha256_bytes("\n".join(spec.normalized() for spec in specs).encode()),
        effective_features=_sha256_bytes(graph),
        inventory=_sha256_bytes(inventory),
    )


def _safe_log_name(cell_id: str) -> str:
    return cell_id.replace("/", "--")


def _write_setup_failure_log(
    cell: SuiteCell,
    *,
    state_dir: pathlib.Path,
    phase: str,
    error: BaseException,
    attempt_id: str,
) -> pathlib.Path:
    log_dir = state_dir / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    log_path = log_dir / f"{_safe_log_name(cell.id)}--{attempt_id}.log.gz"
    with gzip.open(log_path, "wt", encoding="utf-8") as log:
        log.write(f"SETUP PHASE {phase}\n")
        log.write(f"ERROR {type(error).__name__}: {error}\n")
        if isinstance(error, subprocess.CalledProcessError):
            for label, output in (("STDOUT", error.stdout), ("STDERR", error.stderr)):
                if output:
                    if isinstance(output, bytes):
                        rendered = output.decode("utf-8", errors="replace")
                    else:
                        rendered = str(output)
                    log.write(f"{label}\n{rendered}\n")
    return log_path


def _copy_process_output(
    process: subprocess.Popen[bytes],
    destination: object,
    *,
    timeout_seconds: int,
) -> int:
    """Stream a command's output while enforcing one absolute deadline."""
    assert process.stdout is not None
    poller = select.poll()
    poller.register(process.stdout, select.POLLIN | select.POLLHUP | select.POLLERR)
    deadline = time.monotonic() + timeout_seconds
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise subprocess.TimeoutExpired(process.args, timeout_seconds)
        events = poller.poll(max(1, min(int(remaining * 1000), 1000)))
        if not events:
            continue
        chunk = os.read(process.stdout.fileno(), 1024 * 1024)
        if not chunk:
            break
        destination.write(chunk)
    return process.wait(timeout=max(0.001, deadline - time.monotonic()))


def _active_process_group_members(process_group_id: int) -> tuple[int, ...]:
    """Return non-zombie members; a zombie cannot retain files or execute work."""
    members = []
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            raw = (entry / "stat").read_text(encoding="utf-8")
        except (FileNotFoundError, ProcessLookupError):
            # A process that exits between open() and read() yields ESRCH.
            continue
        closing = raw.rfind(")")
        if closing < 0:
            raise OwnedProcessGroupError(f"cannot parse process stat: {entry / 'stat'}")
        fields = raw[closing + 2 :].split()
        if len(fields) < 3:
            raise OwnedProcessGroupError(f"cannot parse process stat: {entry / 'stat'}")
        state = fields[0]
        process_group = int(fields[2])
        if process_group == process_group_id and state != "Z":
            members.append(int(entry.name))
    return tuple(sorted(members))


def _terminate_owned_process_group(
    process: subprocess.Popen[bytes], destination: object, *, reason: str
) -> str | None:
    """Reap only one recorded process group, and fail if it cannot be emptied."""
    active = _active_process_group_members(process.pid)
    if not active:
        process.wait(timeout=2.0)
        return None

    destination.write(
        f"ERROR {reason}; pgid={process.pid}; active={list(active)}\n".encode()
    )
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + 2.0
    while time.monotonic() < deadline:
        if not _active_process_group_members(process.pid):
            break
        time.sleep(0.05)
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 2.0
        while time.monotonic() < deadline:
            if not _active_process_group_members(process.pid):
                break
            time.sleep(0.05)
        else:
            raise OwnedProcessGroupError(
                f"owned process group did not terminate: {process.pid}"
            )
    try:
        process.wait(timeout=2.0)
    except subprocess.TimeoutExpired as error:
        raise OwnedProcessGroupError(
            f"owned process leader did not terminate: {process.pid}"
        ) from error
    return reason


@contextlib.contextmanager
def _recorded_process_group(
    process: subprocess.Popen[bytes],
    *,
    state: feature_matrix.CampaignState,
    identity: feature_matrix.CellIdentity,
    attempt_id: str,
    destination: object,
    cleanup_reason: str,
) -> Iterator[list[str | None]]:
    """Arm exact cleanup before recording or handling one owned process group."""
    cleanup_result: list[str | None] = [None]
    try:
        state.note_process_group(identity, attempt_id, process.pid)
        yield cleanup_result
    finally:
        try:
            cleanup_result[0] = _terminate_owned_process_group(
                process,
                destination,
                reason=cleanup_reason,
            )
        finally:
            if process.stdout is not None:
                process.stdout.close()


def _run_specs(
    cell: SuiteCell,
    specs: Sequence[CommandSpec],
    *,
    repo_root: pathlib.Path,
    state_dir: pathlib.Path,
    state: feature_matrix.CampaignState,
    identity: feature_matrix.CellIdentity,
    attempt_id: str,
) -> tuple[int, pathlib.Path, dict[str, object]]:
    log_dir = state_dir / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    log_path = log_dir / f"{_safe_log_name(cell.id)}--{attempt_id}.log.gz"
    started = time.monotonic()
    maximum_rss_before = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    exit_code = 0
    commands: list[dict[str, object]] = []
    with gzip.open(log_path, "wb") as log:
        for spec in specs:
            log.write(f"COMMAND {spec.normalized()}\n".encode())
            log.flush()
            environment = os.environ.copy()
            environment.update(spec.environment)
            environment[feature_matrix.ATTEMPT_ENVIRONMENT] = attempt_id
            command_started = time.monotonic()
            process: subprocess.Popen[bytes] | None = None
            descendant_cleanup = [None]
            try:
                process = subprocess.Popen(
                    spec.argv,
                    cwd=repo_root / "fuzz" if cell.suite == "fuzz" else repo_root,
                    env=environment,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                with _recorded_process_group(
                    process,
                    state=state,
                    identity=identity,
                    attempt_id=attempt_id,
                    destination=log,
                    cleanup_reason="owned process group survived command handling",
                ) as descendant_cleanup:
                    output = _RetainingWriter(log) if spec.required_output else log
                    try:
                        command_exit_code = _copy_process_output(
                            process, output, timeout_seconds=spec.timeout_seconds
                        )
                        if (
                            command_exit_code == 0
                            and spec.required_output
                            and spec.required_output.encode() not in output.retained
                        ):
                            command_exit_code = 1
                            log.write(
                                f"COMMAND OUTPUT MISSING {spec.required_output!r}\n".encode()
                            )
                    except subprocess.TimeoutExpired:
                        command_exit_code = 124
                        log.write(
                            f"COMMAND TIMEOUT after {spec.timeout_seconds}s\n".encode()
                        )
                    except OSError as error:
                        command_exit_code = 126
                        log.write(
                            f"COMMAND ERROR {type(error).__name__}: {error}\n".encode()
                        )
                if descendant_cleanup[0] is not None:
                    command_exit_code = command_exit_code or 125
            except OSError as error:
                command_exit_code = 126
                log.write(
                    f"COMMAND ERROR {type(error).__name__}: {error}\n".encode()
                )
            commands.append(
                {
                    "elapsed_seconds": time.monotonic() - command_started,
                    "exit_code": command_exit_code,
                    "label": spec.label,
                    "owned_descendant_error": descendant_cleanup[0],
                }
            )
            if command_exit_code != 0 and exit_code == 0:
                exit_code = command_exit_code
    metrics = {
        "commands": commands,
        "elapsed_seconds": time.monotonic() - started,
        "maximum_rss_kib": max(
            0,
            resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss - maximum_rss_before,
        ),
        "log_bytes": log_path.stat().st_size,
    }
    return exit_code, log_path, metrics


def _cleanup_service_containers(
    cell: SuiteCell,
    *,
    run_id: str | None = None,
    timeout_seconds: int = DEFAULT_COMMAND_TIMEOUT_SECONDS,
) -> None:
    if cell.suite != "services" or cell.config is None or cell.auxiliary is None:
        return
    if run_id is None:
        run_id = _service_run_id(cell.config, cell.auxiliary)
    listing = subprocess.run(
        ("docker", "ps", "-aq", "--filter", f"label=org.sozu.e2e.run={run_id}"),
        check=True,
        capture_output=True,
        text=True,
        timeout=timeout_seconds,
    ).stdout.split()
    artifact_dir_raw = os.environ.get("SOZU_PROTOCOL_SERVICE_ARTIFACT_DIR")
    artifact_dir = pathlib.Path(artifact_dir_raw) if artifact_dir_raw else None
    if artifact_dir is not None:
        artifact_dir.mkdir(parents=True, exist_ok=True)
    for container_id in listing:
        if artifact_dir is not None:
            log_result = subprocess.run(
                ("docker", "logs", container_id),
                capture_output=True,
                timeout=timeout_seconds,
            )
            (artifact_dir / f"{container_id}.log").write_bytes(
                log_result.stdout + log_result.stderr
            )
        subprocess.run(
            ("docker", "rm", "--volumes", "--force", container_id),
            check=True,
            timeout=timeout_seconds,
        )
    remaining = subprocess.run(
        ("docker", "ps", "-aq", "--filter", f"label=org.sozu.e2e.run={run_id}"),
        check=True,
        capture_output=True,
        text=True,
        timeout=timeout_seconds,
    ).stdout.strip()
    if remaining:
        raise RuntimeError(f"owned containers remain for {run_id}: {remaining}")


def load_triage_index(path: pathlib.Path | None) -> dict[str, dict[str, str]]:
    if path is None:
        return {}
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, Mapping) or payload.get("version") != 1:
        raise ValueError("triage index must be an object with version 1")
    entries = payload.get("entries")
    if not isinstance(entries, list):
        raise ValueError("triage index entries must be a list")
    indexed: dict[str, dict[str, str]] = {}
    for entry in entries:
        if not isinstance(entry, Mapping):
            raise ValueError("triage entry must be an object")
        identity = entry.get("identity_sha256")
        issue_url = entry.get("issue_url")
        if not isinstance(identity, str) or len(identity) != 64:
            raise ValueError("triage entry identity_sha256 must contain 64 characters")
        if not isinstance(issue_url, str) or not issue_url.startswith("https://"):
            raise ValueError("triage entry issue_url must be an HTTPS URL")
        if identity in indexed:
            raise ValueError(f"duplicate triage identity: {identity}")
        indexed[identity] = {
            "issue_url": issue_url,
            "classification": str(entry.get("classification", "unclassified")),
        }
    return indexed


def triage_failures(
    statuses: Mapping[str, str],
    receipts: Mapping[str, object],
    triage_index: Mapping[str, dict[str, str]],
) -> tuple[dict[str, dict[str, str]], list[str]]:
    triaged: dict[str, dict[str, str]] = {}
    untriaged = []
    for cell_id, status in statuses.items():
        if status != "failed":
            continue
        receipt = receipts.get(cell_id)
        identity = receipt.get("identity_sha256") if isinstance(receipt, Mapping) else None
        entry = triage_index.get(identity) if isinstance(identity, str) else None
        if entry is None:
            untriaged.append(cell_id)
        else:
            triaged[cell_id] = dict(entry)
    return triaged, sorted(untriaged)


def _campaign_report(
    plan: CampaignPlan,
    *,
    selected: Sequence[SuiteCell],
    statuses: Mapping[str, str],
    receipts: Mapping[str, object],
    blocked_reason: str | None,
    source: str,
    toolchain: str,
    generator: str,
    triage_index_path: pathlib.Path | None,
    workers: int,
) -> dict[str, object]:
    status_counts = dict(sorted(Counter(statuses.values()).items()))
    all_executed = status_counts.get("not-run", 0) == 0
    all_passed = all_executed and status_counts.get("failed", 0) == 0
    triaged, untriaged = triage_failures(
        statuses,
        receipts,
        load_triage_index(triage_index_path),
    )
    return {
        "all_executed": all_executed,
        "all_passed": all_passed,
        "blocked_reason": blocked_reason,
        "status": "success" if all_passed else "failed",
        "mode": plan.mode,
        "seed": plan.seed,
        "source_sha256": source,
        "toolchain_sha256": toolchain,
        "generator_sha256": generator,
        "selected": len(selected),
        "status_counts": status_counts,
        "statuses": dict(statuses),
        "triage": triaged,
        "untriaged_failures": untriaged,
        "receipts": dict(receipts),
        "counts": dict(sorted(Counter(cell.suite for cell in selected).items())),
        "workers": workers,
    }


def _reused_terminal_result(
    state: feature_matrix.CampaignState,
    identity: feature_matrix.CellIdentity,
    *,
    replay_failed: bool,
) -> tuple[str, pathlib.Path] | None:
    if state.is_success(identity):
        return ("success", state.receipt_path(identity))
    terminal = state.terminal_result(identity)
    if (
        terminal is not None
        and terminal.get("state") == "failed"
        and not replay_failed
    ):
        return ("failed", state.outcome_path(identity))
    return None


def _write_latest_report(state_dir: pathlib.Path, report: Mapping[str, object]) -> None:
    report_path = state_dir / "latest-report.json"
    temporary = report_path.with_name(f".{report_path.name}.{os.getpid()}.tmp")
    temporary.write_text(
        json.dumps(report, sort_keys=True, separators=(",", ":")) + "\n",
        encoding="utf-8",
    )
    os.replace(temporary, report_path)


def _active_build_processes() -> list[str]:
    active = []
    for process in pathlib.Path("/proc").iterdir():
        if not process.name.isdigit():
            continue
        try:
            command = (process / "comm").read_text(encoding="utf-8").strip()
            if command in {"cargo", "rustc"}:
                active.append(f"{process.name}:{command}")
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
    return sorted(active)


def _record_setup_failure(
    cell: SuiteCell,
    *,
    state: feature_matrix.CampaignState,
    state_dir: pathlib.Path,
    repo_root: pathlib.Path,
    specs: Sequence[CommandSpec],
    source: str,
    toolchain: str,
    generator: str,
    phase: str,
    error: BaseException,
    before: Mapping[str, object],
    replay_failed: bool,
) -> tuple[feature_matrix.CellIdentity, pathlib.Path, bool]:
    require_source_fingerprint(repo_root, source, phase=f"{phase} failure identity")
    marker = f"{phase}:{type(error).__name__}:{error}".encode()
    identity = _cell_identity(
        cell,
        specs,
        repo_root=repo_root,
        source=source,
        toolchain=toolchain,
        generator=generator,
        graph=marker,
        inventory=marker,
    )
    exit_code = error.returncode if isinstance(error, subprocess.CalledProcessError) else 125
    reused = _reused_terminal_result(
        state, identity, replay_failed=replay_failed
    )
    if reused is not None:
        require_source_fingerprint(repo_root, source, phase=f"{phase} reused outcome")
        _, path = reused
        return identity, path, True
    attempt_id = state.start(identity)
    log_path = _write_setup_failure_log(
        cell,
        state_dir=state_dir,
        phase=phase,
        error=error,
        attempt_id=attempt_id,
    )
    details = {
        "after": resource_snapshot(state_dir),
        "before": dict(before),
        "error": str(error),
        "error_type": type(error).__name__,
        "log": str(log_path),
        "log_bytes": log_path.stat().st_size,
        "log_sha256": _sha256_file(log_path),
        "phase": phase,
    }
    require_source_fingerprint(repo_root, source, phase=f"{phase} failure receipt")
    state.finish(
        identity,
        attempt_id=attempt_id,
        exit_code=exit_code,
        inventory_hash=_sha256_bytes(marker),
        details=details,
    )
    return identity, state.attempt_path(identity, attempt_id), False


def _run_campaign_parallel(
    plan: CampaignPlan,
    *,
    repo_root: pathlib.Path,
    state_dir: pathlib.Path,
    suites: Set[str],
    jobs: int,
    command_timeout_seconds: int,
    workers: int,
    cell_id: str | None,
    config_id: str | None,
    triage_index_path: pathlib.Path | None,
    replay_failed: bool,
) -> dict[str, object]:
    if workers != 2:
        raise ValueError("parallel campaigns currently require exactly two workers")
    selected = [cell for cell in plan.cells if cell.suite in suites]
    if cell_id is not None:
        requested = {item for item in cell_id.split(",") if item}
        selected = [cell for cell in selected if cell.id in requested]
        missing = requested.difference(cell.id for cell in selected)
        if missing:
            raise ValueError(f"unknown selected cells: {sorted(missing)}")
    if config_id is not None:
        selected = [cell for cell in selected if config_id in cell.product_ids]
    if not selected:
        raise ValueError("campaign selection contains no cells")

    resolved_repo = repo_root.resolve()
    resolved_state = state_dir.resolve()
    if resolved_state == resolved_repo or resolved_state.is_relative_to(resolved_repo):
        raise ValueError("campaign state must live outside the repository worktree")
    resolved_state.mkdir(parents=True, exist_ok=True)
    state = feature_matrix.CampaignState(resolved_state)
    state.recover_orphaned_claims()
    source = source_fingerprint(resolved_repo)
    toolchains = campaign_toolchains(resolved_repo, selected)
    generator = generator_fingerprint(resolved_repo)
    statuses = {cell.id: "not-run" for cell in selected}
    receipts: dict[str, object] = {}
    blocked_reason: str | None = None
    orphan_blockers: list[str] = []
    slots = tuple(
        WorkerSlot(index, resolved_state / "workers" / f"worker-{index}")
        for index in range(workers)
    )
    if plan.mode == "exhaustive":
        try:
            validate_capacity(resource_snapshot(resolved_state), check_load=True)
        except RuntimeError as error:
            blocked_reason = str(error)
    parallel_probe = (
        verify_parallel_namespace_isolation(slots)
        if blocked_reason is None
        else {"status": "not-run", "reason": blocked_reason}
    )
    exclusive_suites = {"fuzz", "services", "sim"}
    index = 0

    while index < len(selected) and blocked_reason is None:
        cell = selected[index]
        try:
            require_source_fingerprint(
                resolved_repo, source, phase=f"before {cell.id} preparation"
            )
        except SourceDriftError as error:
            blocked_reason = str(error)
            break
        if cell.suite in exclusive_suites:
            single = CampaignPlan(plan.mode, plan.seed, plan.product_configs, (cell,))
            result = _run_campaign_locked(
                single,
                repo_root=resolved_repo,
                state_dir=resolved_state,
                suites={cell.suite},
                dry_run=False,
                jobs=jobs,
                command_timeout_seconds=command_timeout_seconds,
                triage_index_path=triage_index_path,
                replay_failed=replay_failed,
                initial_capacity_check=False,
                expected_source=source,
                expected_toolchains=toolchains,
            )
            statuses[cell.id] = result["statuses"][cell.id]
            receipts[cell.id] = result["receipts"][cell.id]
            if result.get("blocked_reason"):
                blocked_reason = str(result["blocked_reason"])
                break
            index += 1
            continue

        batch: list[SuiteCell] = []
        while index < len(selected) and len(batch) < workers:
            candidate = selected[index]
            if candidate.suite in exclusive_suites:
                break
            batch.append(candidate)
            index += 1

        prepared_batch: list[
            tuple[PreparedCell, feature_matrix.CellIdentity, Mapping[str, object]]
        ] = []
        for slot, batch_cell in zip(slots, batch, strict=True):
            before = resource_snapshot(resolved_state)
            if plan.mode == "exhaustive":
                try:
                    validate_capacity(before, check_load=False)
                except RuntimeError as error:
                    blocked_reason = str(error)
                    break
            specs = campaign_command_specs(
                batch_cell, jobs=jobs, timeout_seconds=command_timeout_seconds
            )
            cell_toolchain = _cell_toolchain(batch_cell, toolchains)
            try:
                require_source_fingerprint(
                    resolved_repo,
                    source,
                    phase=f"before {batch_cell.id} serialized preparation",
                )
                prepared = prepare_cell_for_worker(
                    batch_cell,
                    specs,
                    repo_root=resolved_repo,
                    state_dir=resolved_state,
                    slot=slot,
                )
                require_source_fingerprint(
                    resolved_repo,
                    source,
                    phase=f"after {batch_cell.id} serialized preparation",
                )
                identity = _cell_identity(
                    batch_cell,
                    specs,
                    repo_root=resolved_repo,
                    source=source,
                    toolchain=cell_toolchain,
                    generator=generator,
                    graph=prepared.graph,
                    inventory=prepared.inventory,
                )
            except SourceDriftError as error:
                blocked_reason = str(error)
                break
            except (OSError, subprocess.SubprocessError, RuntimeError, ValueError) as error:
                try:
                    identity, receipt, reused = _record_setup_failure(
                        batch_cell,
                        state=state,
                        state_dir=resolved_state,
                        repo_root=resolved_repo,
                        specs=specs,
                        source=source,
                        toolchain=cell_toolchain,
                        generator=generator,
                        phase="serialized-prepare",
                        error=error,
                        before=before,
                        replay_failed=replay_failed,
                    )
                except SourceDriftError as drift:
                    blocked_reason = str(drift)
                    break
                statuses[batch_cell.id] = "failed"
                receipts[batch_cell.id] = {
                    "identity_sha256": identity.digest,
                    "receipt": str(receipt),
                    "reused": reused,
                }
                continue
            claim = state.claim_path(identity)
            if claim.exists():
                statuses[batch_cell.id] = "not-run"
                receipts[batch_cell.id] = {
                    "claim": str(claim),
                    "identity_sha256": identity.digest,
                    "live_owned_pids": list(state.blocked_claims.get(identity.digest, ())),
                    "reused": False,
                }
                orphan_blockers.append(batch_cell.id)
                continue
            reused = _reused_terminal_result(
                state, identity, replay_failed=replay_failed
            )
            if reused is not None:
                status, receipt = reused
                statuses[batch_cell.id] = status
                receipts[batch_cell.id] = {
                    "identity_sha256": identity.digest,
                    "receipt": str(receipt),
                    "reused": True,
                }
                continue
            prepared_batch.append((prepared, identity, before))
        if blocked_reason is not None:
            break
        if not prepared_batch:
            continue
        active_builds = _active_build_processes()
        if active_builds:
            blocked_reason = f"build processes active before dispatch: {active_builds}"
            break
        if plan.mode == "exhaustive":
            try:
                validate_capacity(
                    resource_snapshot(resolved_state), check_load=False
                )
            except RuntimeError as error:
                blocked_reason = str(error)
                break

        try:
            require_source_fingerprint(
                resolved_repo, source, phase="before direct runtime dispatch"
            )
        except SourceDriftError as error:
            blocked_reason = str(error)
            break

        claimed_batch = [
            (
                prepared,
                identity,
                state.start(
                    identity,
                    owner={"worker": prepared.slot.id, "worker_root": str(prepared.slot.root)},
                ),
                before,
            )
            for prepared, identity, before in prepared_batch
        ]

        with concurrent.futures.ThreadPoolExecutor(
            max_workers=len(claimed_batch), thread_name_prefix="feature-swarm"
        ) as executor:
            futures = {
                executor.submit(
                    run_prepared_cell,
                    prepared,
                    repo_root=resolved_repo,
                    state_dir=resolved_state,
                    state=state,
                    identity=identity,
                    attempt_id=attempt_id,
                ): (prepared, identity, attempt_id, before)
                for prepared, identity, attempt_id, before in claimed_batch
            }
            completed: list[
                tuple[
                    PreparedCell,
                    feature_matrix.CellIdentity,
                    str,
                    Mapping[str, object],
                    int,
                    pathlib.Path,
                    dict[str, object],
                ]
            ] = []
            for future in concurrent.futures.as_completed(futures):
                prepared, identity, attempt_id, before = futures[future]
                try:
                    exit_code, log_path, metrics = future.result()
                except OwnedProcessGroupError as error:
                    blocked_reason = str(error)
                    receipts[prepared.cell.id] = {
                        "claim": str(state.claim_path(identity)),
                        "identity_sha256": identity.digest,
                        "attempt_id": attempt_id,
                        "reused": False,
                    }
                    continue
                except (OSError, subprocess.SubprocessError, RuntimeError, ValueError) as error:
                    exit_code = 125
                    log_path = _write_setup_failure_log(
                        prepared.cell,
                        state_dir=resolved_state,
                        phase="direct-runtime",
                        error=error,
                        attempt_id=attempt_id,
                    )
                    metrics = {
                        "commands": [],
                        "error": str(error),
                        "error_type": type(error).__name__,
                        "phase": "direct-runtime",
                        "worker": prepared.slot.id,
                    }
                completed.append(
                    (
                        prepared,
                        identity,
                        attempt_id,
                        before,
                        exit_code,
                        log_path,
                        metrics,
                    )
                )

        if blocked_reason is not None:
            for prepared, identity, attempt_id, _before in claimed_batch:
                receipts.setdefault(
                    prepared.cell.id,
                    {
                        "claim": str(state.claim_path(identity)),
                        "identity_sha256": identity.digest,
                        "attempt_id": attempt_id,
                        "reused": False,
                    },
                )
            break

        try:
            require_source_fingerprint(
                resolved_repo, source, phase="after direct runtime batch"
            )
        except SourceDriftError as error:
            blocked_reason = str(error)
            for prepared, identity, attempt_id, *_rest in completed:
                receipts[prepared.cell.id] = {
                    "claim": str(state.claim_path(identity)),
                    "identity_sha256": identity.digest,
                    "attempt_id": attempt_id,
                    "reused": False,
                }
            break

        for (
            prepared,
            identity,
            attempt_id,
            before,
            exit_code,
            log_path,
            metrics,
        ) in sorted(completed, key=lambda item: item[0].slot.id):
            try:
                require_source_fingerprint(
                    resolved_repo,
                    source,
                    phase=f"before {prepared.cell.id} doctests",
                )
                doctest_exit, doctest_commands = run_serial_doctests(
                    prepared,
                    repo_root=resolved_repo,
                    state=state,
                    identity=identity,
                    attempt_id=attempt_id,
                    log_path=log_path,
                )
                metrics["doctest_commands"] = doctest_commands
                if exit_code == 0:
                    exit_code = doctest_exit
                require_source_fingerprint(
                    resolved_repo,
                    source,
                    phase=f"after {prepared.cell.id} doctests",
                )
            except SourceDriftError as error:
                blocked_reason = str(error)
                receipts[prepared.cell.id] = {
                    "claim": str(state.claim_path(identity)),
                    "identity_sha256": identity.digest,
                    "attempt_id": attempt_id,
                    "reused": False,
                }
                break
            except OwnedProcessGroupError as error:
                blocked_reason = str(error)
                receipts[prepared.cell.id] = {
                    "claim": str(state.claim_path(identity)),
                    "identity_sha256": identity.digest,
                    "attempt_id": attempt_id,
                    "reused": False,
                }
                break
            except (OSError, subprocess.SubprocessError, RuntimeError, ValueError) as error:
                if exit_code == 0:
                    exit_code = 125
                metrics["doctest_error"] = str(error)
                metrics["doctest_error_type"] = type(error).__name__
                with gzip.open(log_path, "ab") as log:
                    log.write(
                        f"DOCTEST ERROR {type(error).__name__}: {error}\n".encode()
                    )
            metrics.update(
                {
                    "after": resource_snapshot(resolved_state),
                    "before": dict(before),
                    "log": str(log_path),
                    "log_sha256": _sha256_file(log_path),
                }
            )
            try:
                require_source_fingerprint(
                    resolved_repo,
                    source,
                    phase=f"before {prepared.cell.id} terminal receipt",
                )
            except SourceDriftError as error:
                blocked_reason = str(error)
                receipts[prepared.cell.id] = {
                    "claim": str(state.claim_path(identity)),
                    "identity_sha256": identity.digest,
                    "attempt_id": attempt_id,
                    "reused": False,
                }
                break
            state.finish(
                identity,
                attempt_id=attempt_id,
                exit_code=exit_code,
                inventory_hash=_sha256_bytes(prepared.inventory),
                details=metrics,
            )
            statuses[prepared.cell.id] = "success" if exit_code == 0 else "failed"
            receipts[prepared.cell.id] = {
                "identity_sha256": identity.digest,
                "receipt": str(state.attempt_path(identity, attempt_id)),
                "reused": False,
            }

    if blocked_reason is None and orphan_blockers:
        blocked_reason = "live orphaned attempts block cells: " + ", ".join(
            sorted(orphan_blockers)
        )
    report = _campaign_report(
        plan,
        selected=selected,
        statuses=statuses,
        receipts=receipts,
        blocked_reason=blocked_reason,
        source=source,
        toolchain=_campaign_toolchain_fingerprint(toolchains),
        generator=generator,
        triage_index_path=triage_index_path,
        workers=workers,
    )
    report["parallel_namespace_probe"] = parallel_probe
    _write_latest_report(resolved_state, report)
    return report


def _run_campaign_locked(
    plan: CampaignPlan,
    *,
    repo_root: pathlib.Path,
    state_dir: pathlib.Path,
    suites: Set[str],
    dry_run: bool,
    jobs: int,
    command_timeout_seconds: int = DEFAULT_COMMAND_TIMEOUT_SECONDS,
    cell_id: str | None = None,
    config_id: str | None = None,
    triage_index_path: pathlib.Path | None = None,
    replay_failed: bool = False,
    initial_capacity_check: bool = True,
    expected_source: str | None = None,
    expected_toolchains: Mapping[str, str] | None = None,
) -> dict[str, object]:
    if jobs < 1 or jobs > 4:
        raise ValueError("Cargo jobs must be between 1 and 4")
    if command_timeout_seconds <= 0:
        raise ValueError("command timeout must be positive")
    unknown = suites.difference(ALL_SUITES)
    if unknown:
        raise ValueError(f"unknown suites: {sorted(unknown)}")
    selected = [cell for cell in plan.cells if cell.suite in suites]
    if cell_id is not None:
        selected = [cell for cell in selected if cell.id == cell_id]
    if config_id is not None:
        selected = [cell for cell in selected if config_id in cell.product_ids]
    if not selected:
        raise ValueError("campaign selection contains no cells")
    if dry_run:
        return {
            "status": "dry-run",
            "mode": plan.mode,
            "seed": plan.seed,
            "repo_root": str(repo_root.resolve()),
            "state_dir": str(state_dir.resolve()),
            "jobs": jobs,
            "command_timeout_seconds": command_timeout_seconds,
            "workers": 1,
            "cells": [_json_ready(cell) for cell in selected],
            "counts": dict(sorted(Counter(cell.suite for cell in selected).items())),
        }

    resolved_repo = repo_root.resolve()
    resolved_state = state_dir.resolve()
    if resolved_state == resolved_repo or resolved_state.is_relative_to(resolved_repo):
        raise ValueError("campaign state must live outside the repository worktree")
    resolved_state.mkdir(parents=True, exist_ok=True)
    state = feature_matrix.CampaignState(resolved_state)
    state.recover_orphaned_claims()
    source = expected_source or source_fingerprint(resolved_repo)
    toolchains = (
        dict(expected_toolchains)
        if expected_toolchains is not None
        else campaign_toolchains(resolved_repo, selected)
    )
    generator = generator_fingerprint(resolved_repo)
    statuses = {cell.id: "not-run" for cell in selected}
    receipts: dict[str, object] = {}
    blocked_reason: str | None = None
    orphan_blockers: list[str] = []

    if plan.mode == "exhaustive" and initial_capacity_check:
        try:
            validate_capacity(resource_snapshot(resolved_repo), check_load=True)
        except RuntimeError as error:
            blocked_reason = str(error)

    for cell in selected:
        if blocked_reason is not None:
            break
        try:
            require_source_fingerprint(
                resolved_repo, source, phase=f"before {cell.id} setup"
            )
        except SourceDriftError as error:
            blocked_reason = str(error)
            break
        before = resource_snapshot(resolved_repo)
        if plan.mode == "exhaustive":
            try:
                validate_capacity(before, check_load=False)
            except RuntimeError as error:
                blocked_reason = str(error)
                break
        specs = campaign_command_specs(
            cell, jobs=jobs, timeout_seconds=command_timeout_seconds
        )
        cell_toolchain = _cell_toolchain(cell, toolchains)
        setup_phase = "effective-feature-graph"
        graph = b""
        inventory = b""
        setup_error: BaseException | None = None
        try:
            graph = effective_feature_graph(
                cell,
                resolved_repo,
                timeout_seconds=command_timeout_seconds,
            )
            setup_phase = "test-inventory"
            inventory = test_inventory(cell, specs, resolved_repo)
        except (OSError, subprocess.SubprocessError, RuntimeError, ValueError) as error:
            setup_error = error
            marker = f"{setup_phase}:{type(error).__name__}:{error}".encode()
            if not graph:
                graph = marker
            if not inventory:
                inventory = marker
        try:
            require_source_fingerprint(
                resolved_repo, source, phase=f"after {cell.id} setup"
            )
        except SourceDriftError as error:
            blocked_reason = str(error)
            break
        identity = _cell_identity(
            cell,
            specs,
            repo_root=resolved_repo,
            source=source,
            toolchain=cell_toolchain,
            generator=generator,
            graph=graph,
            inventory=inventory,
        )
        if setup_error is not None:
            exit_code = (
                setup_error.returncode
                if isinstance(setup_error, subprocess.CalledProcessError)
                else 125
            )
            reused = _reused_terminal_result(
                state, identity, replay_failed=replay_failed
            )
            if reused is not None:
                try:
                    require_source_fingerprint(
                        resolved_repo,
                        source,
                        phase=f"before {cell.id} reused setup outcome",
                    )
                except SourceDriftError as error:
                    blocked_reason = str(error)
                    break
                status, receipt = reused
                statuses[cell.id] = status
                receipts[cell.id] = {
                    "identity_sha256": identity.digest,
                    "receipt": str(receipt),
                    "reused": True,
                }
                continue
            attempt_id = state.start(identity)
            log_path = _write_setup_failure_log(
                cell,
                state_dir=resolved_state,
                phase=setup_phase,
                error=setup_error,
                attempt_id=attempt_id,
            )
            after = resource_snapshot(resolved_repo)
            metrics = {
                "after": after,
                "before": before,
                "error": str(setup_error),
                "error_type": type(setup_error).__name__,
                "log": str(log_path),
                "log_bytes": log_path.stat().st_size,
                "log_sha256": _sha256_file(log_path),
                "phase": setup_phase,
            }
            try:
                require_source_fingerprint(
                    resolved_repo,
                    source,
                    phase=f"before {cell.id} setup failure receipt",
                )
            except SourceDriftError as error:
                blocked_reason = str(error)
                break
            state.finish(
                identity,
                attempt_id=attempt_id,
                exit_code=exit_code,
                inventory_hash=_sha256_bytes(inventory),
                details=metrics,
            )
            statuses[cell.id] = "failed"
            receipts[cell.id] = {
                "identity_sha256": identity.digest,
                "receipt": str(state.attempt_path(identity, attempt_id)),
                "reused": False,
            }
            continue
        claim = state.claim_path(identity)
        if claim.exists():
            statuses[cell.id] = "not-run"
            receipts[cell.id] = {
                "claim": str(claim),
                "identity_sha256": identity.digest,
                "live_owned_pids": list(state.blocked_claims.get(identity.digest, ())),
                "reused": False,
            }
            orphan_blockers.append(cell.id)
            continue
        reused = _reused_terminal_result(
            state, identity, replay_failed=replay_failed
        )
        if reused is not None:
            try:
                require_source_fingerprint(
                    resolved_repo,
                    source,
                    phase=f"before {cell.id} reused outcome",
                )
            except SourceDriftError as error:
                blocked_reason = str(error)
                break
            status, receipt = reused
            statuses[cell.id] = status
            receipts[cell.id] = {
                "identity_sha256": identity.digest,
                "receipt": str(receipt),
                "reused": True,
            }
            continue

        attempt_id = state.start(identity)
        try:
            require_source_fingerprint(
                resolved_repo, source, phase=f"before {cell.id} execution"
            )
        except SourceDriftError as error:
            blocked_reason = str(error)
            break
        attempt_specs = _service_attempt_specs(cell, specs, attempt_id)
        service_run_id = (
            _service_run_id(cell.config, cell.auxiliary, attempt_id)
            if cell.suite == "services"
            and cell.config is not None
            and cell.auxiliary is not None
            else None
        )
        cleanup_error = None
        try:
            exit_code, log_path, metrics = _run_specs(
                cell,
                attempt_specs,
                repo_root=resolved_repo,
                state_dir=resolved_state,
                state=state,
                identity=identity,
                attempt_id=attempt_id,
            )
        except OwnedProcessGroupError as error:
            blocked_reason = str(error)
            receipts[cell.id] = {
                "claim": str(state.claim_path(identity)),
                "identity_sha256": identity.digest,
                "attempt_id": attempt_id,
                "reused": False,
            }
        finally:
            try:
                _cleanup_service_containers(
                    cell,
                    run_id=service_run_id,
                    timeout_seconds=command_timeout_seconds,
                )
            except (OSError, subprocess.SubprocessError, RuntimeError) as error:
                cleanup_error = str(error)
        if blocked_reason is not None:
            break
        if cleanup_error is not None:
            exit_code = exit_code or 125
            metrics["cleanup_error"] = cleanup_error
        after = resource_snapshot(resolved_repo)
        metrics.update(
            {
                "before": before,
                "after": after,
                "log_sha256": _sha256_file(log_path),
                "log": str(log_path),
            }
        )
        inventory_hash = _sha256_bytes(inventory)
        try:
            require_source_fingerprint(
                resolved_repo, source, phase=f"before {cell.id} terminal receipt"
            )
        except SourceDriftError as error:
            blocked_reason = str(error)
            receipts[cell.id] = {
                "claim": str(state.claim_path(identity)),
                "identity_sha256": identity.digest,
                "attempt_id": attempt_id,
                "reused": False,
            }
            break
        state.finish(
            identity,
            attempt_id=attempt_id,
            exit_code=exit_code,
            inventory_hash=inventory_hash,
            details=metrics,
        )
        statuses[cell.id] = "success" if exit_code == 0 else "failed"
        receipts[cell.id] = {
            "identity_sha256": identity.digest,
            "receipt": str(state.attempt_path(identity, attempt_id)),
            "reused": False,
        }

    if blocked_reason is None and orphan_blockers:
        blocked_reason = "live orphaned attempts block cells: " + ", ".join(
            sorted(orphan_blockers)
        )
    report = _campaign_report(
        plan,
        selected=selected,
        statuses=statuses,
        receipts=receipts,
        blocked_reason=blocked_reason,
        source=source,
        toolchain=_campaign_toolchain_fingerprint(toolchains),
        generator=generator,
        triage_index_path=triage_index_path,
        workers=1,
    )
    _write_latest_report(resolved_state, report)
    return report


def run_campaign(
    plan: CampaignPlan,
    *,
    repo_root: pathlib.Path,
    state_dir: pathlib.Path,
    suites: Set[str],
    dry_run: bool,
    jobs: int,
    command_timeout_seconds: int = DEFAULT_COMMAND_TIMEOUT_SECONDS,
    workers: int = 1,
    cell_id: str | None = None,
    config_id: str | None = None,
    triage_index_path: pathlib.Path | None = None,
    replay_failed: bool = False,
) -> dict[str, object]:
    arguments = {
        "repo_root": repo_root,
        "state_dir": state_dir,
        "suites": suites,
        "dry_run": dry_run,
        "jobs": jobs,
        "command_timeout_seconds": command_timeout_seconds,
        "cell_id": cell_id,
        "config_id": config_id,
        "triage_index_path": triage_index_path,
        "replay_failed": replay_failed,
    }
    if dry_run:
        report = _run_campaign_locked(plan, **arguments)
        report["workers"] = workers
        return report
    checkout_lease_root = checkout_campaign_lease_root(repo_root)
    with feature_matrix.CampaignLease(checkout_lease_root):
        with feature_matrix.CampaignLease(state_dir.resolve()):
            if workers > 1:
                return _run_campaign_parallel(
                    plan,
                    repo_root=repo_root,
                    state_dir=state_dir,
                    suites=suites,
                    jobs=jobs,
                    command_timeout_seconds=command_timeout_seconds,
                    workers=workers,
                    cell_id=cell_id,
                    config_id=config_id,
                    triage_index_path=triage_index_path,
                    replay_failed=replay_failed,
                )
            return _run_campaign_locked(plan, **arguments)


def checkout_campaign_lease_root(
    repo_root: pathlib.Path, *, runtime_root: pathlib.Path | None = None
) -> pathlib.Path:
    """Return one host-local lock root for a canonical checkout."""
    if runtime_root is None:
        # This lock coordinates every campaign for the checkout, including
        # orchestrators whose private worker TMPDIR values intentionally differ.
        runtime_root = pathlib.Path("/tmp")
    lock_parent = runtime_root / f"sozu-feature-swarm-{os.getuid()}"
    lock_parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    metadata = lock_parent.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise RuntimeError(f"campaign lock parent is not a real directory: {lock_parent}")
    if metadata.st_uid != os.getuid() or stat.S_IMODE(metadata.st_mode) & 0o077:
        raise RuntimeError(f"campaign lock parent is not private and owned: {lock_parent}")
    checkout = str(repo_root.resolve()).encode()
    return lock_parent / hashlib.sha256(checkout).hexdigest()


def _json_ready(value: object) -> object:
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return _json_ready(dataclasses.asdict(value))
    if isinstance(value, Mapping):
        return {str(key): _json_ready(item) for key, item in sorted(value.items())}
    if isinstance(value, (set, frozenset)):
        return [_json_ready(item) for item in sorted(value)]
    if isinstance(value, (list, tuple)):
        return [_json_ready(item) for item in value]
    if isinstance(value, pathlib.Path):
        return str(value)
    return value


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("bounded", "exhaustive"), required=True)
    parser.add_argument("--seed", type=int, default=feature_matrix.DEFAULT_SEED)
    parser.add_argument("--repo-root", type=pathlib.Path, default=pathlib.Path.cwd())
    parser.add_argument("--state-dir", type=pathlib.Path, required=True)
    parser.add_argument("--suites", default=",".join(sorted(ALL_SUITES)))
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument(
        "--command-timeout-seconds",
        type=int,
        default=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        help="mandatory per-command supervisor deadline (default: 1800 seconds)",
    )
    parser.add_argument("--workers", type=int, choices=(1, 2), default=1)
    parser.add_argument("--cell")
    parser.add_argument("--config-id")
    parser.add_argument(
        "--replay-failed",
        action="store_true",
        help="explicitly rerun terminal failed cells with the same campaign identity",
    )
    parser.add_argument(
        "--triage-index",
        type=pathlib.Path,
        help="external issue index; it never participates in cell fingerprints",
    )
    parser.add_argument("--dry-run", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    suites = {suite for suite in args.suites.split(",") if suite}
    plan = build_campaign_plan(mode=args.mode, seed=args.seed)
    report = run_campaign(
        plan,
        repo_root=args.repo_root,
        state_dir=args.state_dir,
        suites=suites,
        dry_run=args.dry_run,
        jobs=args.jobs,
        command_timeout_seconds=args.command_timeout_seconds,
        workers=args.workers,
        cell_id=args.cell,
        config_id=args.config_id,
        triage_index_path=args.triage_index,
        replay_failed=args.replay_failed,
    )
    print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 0 if report["status"] in {"success", "dry-run"} else 1


if __name__ == "__main__":
    raise SystemExit(main())
