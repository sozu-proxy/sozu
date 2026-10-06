#!/usr/bin/env python3
"""Generate and validate Sōzu's bounded and exhaustive feature matrices."""

from __future__ import annotations

import argparse
import dataclasses
import fcntl
import hashlib
import itertools
import json
import os
import pathlib
import tempfile
import time
import tomllib
import uuid
from collections.abc import Iterable, Mapping, Sequence


DEFAULT_SEED = 20_261_005
PROVIDERS = ("crypto-ring", "crypto-aws-lc-rs", "crypto-openssl", "fips")
BOOLEAN_FEATURES = (
    "jemallocator",
    "opentelemetry",
    "tolerant-http1-parser",
    "simd",
    "splice",
    "tui",
)
SHARED_FEATURES = tuple(
    feature for feature in BOOLEAN_FEATURES if feature not in {"jemallocator", "tui"}
)
LOG_LEVELS = ("off", "debug", "trace")
LOG_FEATURES = {"off": None, "debug": "logs-debug", "trace": "logs-trace"}
ATTEMPT_ENVIRONMENT = "SOZU_FEATURE_MATRIX_ATTEMPT_ID"


@dataclasses.dataclass(frozen=True)
class Service:
    feature: str
    test: str
    image: str


SERVICES: Mapping[str, Service] = {
    "postgresql": Service(
        "service-postgres",
        "tests::real_services_tcp::postgres::round_trip_and_reconnect_via_sozu",
        "postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873",
    ),
    "mysql": Service(
        "service-mysql",
        "tests::real_services_tcp::mysql::round_trip_and_reconnect_via_sozu",
        "mysql:8.4.11@sha256:6ea90827b1100f8f2ae306a539f86d2c264a26ed435a2a9f75551dd5c3aeb242",
    ),
    "redis": Service(
        "service-redis",
        "tests::real_services_tcp::redis::round_trip_and_reconnect_via_sozu",
        "redis:8.2.10-alpine@sha256:b51665e66f00759be7c3152ad5ac3c66fb2f619c13ef62dea7cc1f9914524635",
    ),
    "mongodb": Service(
        "service-mongodb",
        "tests::real_services_tcp::mongodb::round_trip_and_reconnect_via_sozu",
        "mongo:8.0.32-noble@sha256:0393ab544cbbe92b2dd64719205ecb14a8b3824b17ea75051e2f22482c3e4e66",
    ),
    "kafka": Service(
        "service-kafka",
        "tests::real_services_tcp::kafka::round_trip_and_reconnect_via_sozu",
        "apache/kafka:4.1.2@sha256:5cc2a2fd93fa2687b44015eee04fb2c3edd9e526bd64bf8bec5ff1e268772e0e",
    ),
    "rabbitmq": Service(
        "service-rabbitmq",
        "tests::real_services_tcp::rabbitmq::round_trip_and_reconnect_via_sozu",
        "rabbitmq:4.2.9-alpine@sha256:d5d8797191db5828a2a2a3dabf295d2b558ae81e215e6c6cba26d567ae8fb236",
    ),
    "pulsar": Service(
        "service-pulsar",
        "tests::real_services_tcp::pulsar::round_trip_and_reconnect_via_sozu",
        "apachepulsar/pulsar:4.2.4@sha256:cd5d4a64a32c0770d5d2dbb526169a70081605bbec4b59b73471b68d0a451fb4",
    ),
    "coredns": Service(
        "service-coredns",
        "tests::real_services_udp::authoritative_queries_expiry_and_reactivation_via_sozu_udp",
        "coredns/coredns:1.14.7@sha256:7efd3c635b03efd68c4e8398fc45f0d993d0e9ab016f72c1cefb0fd6d01aa286",
    ),
}


def _stable_order(values: Iterable[str], *, seed: int, namespace: str) -> list[str]:
    return sorted(
        values,
        key=lambda value: hashlib.sha256(
            f"{seed}\0{namespace}\0{value}".encode()
        ).digest(),
    )


def _feature_csv(features: Sequence[str]) -> str:
    return ",".join(features)


def _provider_label(provider: str) -> str:
    return {
        "crypto-ring": "ring",
        "crypto-aws-lc-rs": "aws-lc",
        "crypto-openssl": "openssl",
        "fips": "fips",
    }[provider]


@dataclasses.dataclass(frozen=True)
class ProductConfig:
    provider: str
    log_level: str
    enabled: frozenset[str]

    def __post_init__(self) -> None:
        if self.provider not in PROVIDERS:
            raise ValueError(f"unknown provider: {self.provider}")
        if self.log_level not in LOG_LEVELS:
            raise ValueError(f"unknown log level: {self.log_level}")
        unknown = self.enabled.difference(BOOLEAN_FEATURES)
        if unknown:
            raise ValueError(f"unknown boolean features: {sorted(unknown)}")

    @property
    def bits(self) -> str:
        return "".join("1" if feature in self.enabled else "0" for feature in BOOLEAN_FEATURES)

    @property
    def id(self) -> str:
        return f"{_provider_label(self.provider)}-{self.log_level}-{self.bits}"

    def features_for(self, projection: str) -> tuple[str, ...]:
        if projection == "bin":
            boolean_features = BOOLEAN_FEATURES
        elif projection in {"lib", "e2e"}:
            boolean_features = SHARED_FEATURES
        elif projection == "command":
            boolean_features = ()
        else:
            raise ValueError(f"unknown projection: {projection}")

        features: list[str] = []
        if projection != "command":
            features.append(self.provider)
        features.extend(feature for feature in boolean_features if feature in self.enabled)
        log_feature = LOG_FEATURES[self.log_level]
        if log_feature is not None:
            features.append(log_feature)
        return tuple(features)

    def projection_id(self, projection: str) -> str:
        if projection == "bin":
            return self.id
        if projection in {"lib", "e2e"}:
            bits = "".join("1" if feature in self.enabled else "0" for feature in SHARED_FEATURES)
            return f"{_provider_label(self.provider)}-{self.log_level}-{bits}"
        if projection == "command":
            return self.log_level
        raise ValueError(f"unknown projection: {projection}")


def exhaustive_product_configs() -> list[ProductConfig]:
    configs = []
    for provider, log_level, mask in itertools.product(PROVIDERS, LOG_LEVELS, range(64)):
        enabled = frozenset(
            feature for index, feature in enumerate(BOOLEAN_FEATURES) if mask & (1 << index)
        )
        configs.append(ProductConfig(provider, log_level, enabled))
    return configs


def bounded_product_configs(seed: int = DEFAULT_SEED) -> list[ProductConfig]:
    configs: list[ProductConfig] = []
    all_features = frozenset(BOOLEAN_FEATURES)
    no_features: frozenset[str] = frozenset()

    for provider in PROVIDERS:
        configs.append(ProductConfig(provider, "off", no_features))
        configs.append(ProductConfig(provider, "trace", all_features))

    providers = _stable_order(PROVIDERS, seed=seed, namespace="debug-providers")
    features = _stable_order(BOOLEAN_FEATURES, seed=seed, namespace="debug-features")
    weight_two_columns = ("1100", "1010", "1001", "0110", "0101", "0011")
    columns = dict(zip(features, weight_two_columns, strict=True))
    for row, provider in enumerate(providers):
        enabled = frozenset(feature for feature in BOOLEAN_FEATURES if columns[feature][row] == "1")
        configs.append(ProductConfig(provider, "debug", enabled))

    masks = _stable_order(
        (f"{mask:06b}" for mask in range(1, 63)),
        seed=seed,
        namespace="cross-corners",
    )
    extra_providers = _stable_order(PROVIDERS, seed=seed, namespace="cross-providers")
    completed = None
    for off_mask in masks:
        off_pair = (
            ProductConfig(
                extra_providers[0], "off", _features_from_bits(off_mask)
            ),
            ProductConfig(
                extra_providers[1], "off", _features_from_bits(_complement(off_mask))
            ),
        )
        for trace_mask in masks:
            if trace_mask in {off_mask, _complement(off_mask)}:
                continue
            trace_pair = (
                ProductConfig(
                    extra_providers[2], "trace", _features_from_bits(trace_mask)
                ),
                ProductConfig(
                    extra_providers[3],
                    "trace",
                    _features_from_bits(_complement(trace_mask)),
                ),
            )
            candidate = [*configs, *off_pair, *trace_pair]
            try:
                validate_pairwise_coverage(candidate)
                validate_bounded_projection_ids(candidate)
            except ValueError:
                continue
            completed = candidate
            break
        if completed is not None:
            break
    if completed is None:
        raise ValueError(f"seed {seed} produced no valid bounded cross-corners")

    configs = completed
    configs.sort(key=lambda config: config.id)
    validate_pairwise_coverage(configs)
    validate_bounded_projection_ids(configs)
    return configs


def _complement(bits: str) -> str:
    return "".join("0" if bit == "1" else "1" for bit in bits)


def _features_from_bits(bits: str) -> frozenset[str]:
    if len(bits) != len(BOOLEAN_FEATURES) or set(bits).difference({"0", "1"}):
        raise ValueError(f"invalid feature bitset: {bits}")
    return frozenset(
        feature for feature, enabled in zip(BOOLEAN_FEATURES, bits, strict=True) if enabled == "1"
    )


def _axis_values(config: ProductConfig) -> dict[str, object]:
    values: dict[str, object] = {
        "provider": config.provider,
        "log_level": config.log_level,
    }
    values.update({feature: feature in config.enabled for feature in BOOLEAN_FEATURES})
    return values


def validate_pairwise_coverage(configs: Sequence[ProductConfig]) -> None:
    if len(configs) != 16:
        raise ValueError(f"bounded matrix must contain 16 rows, got {len(configs)}")
    if len({config.id for config in configs}) != len(configs):
        raise ValueError("bounded matrix contains duplicate rows")

    domains: dict[str, tuple[object, ...]] = {
        "provider": PROVIDERS,
        "log_level": LOG_LEVELS,
        **{feature: (False, True) for feature in BOOLEAN_FEATURES},
    }
    rows = [_axis_values(config) for config in configs]
    for left, right in itertools.combinations(domains, 2):
        expected = set(itertools.product(domains[left], domains[right]))
        observed = {(row[left], row[right]) for row in rows}
        missing = expected.difference(observed)
        if missing:
            raise ValueError(f"pairwise coverage missing {left} x {right}: {sorted(missing, key=str)}")


def validate_bounded_projection_ids(configs: Sequence[ProductConfig]) -> None:
    for projection in ("lib", "e2e"):
        ids = [config.projection_id(projection) for config in configs]
        if len(set(ids)) != len(ids):
            duplicates = sorted(
                projection_id
                for projection_id in set(ids)
                if ids.count(projection_id) > 1
            )
            raise ValueError(
                f"bounded {projection} projection contains duplicate cells: {duplicates}"
            )


@dataclasses.dataclass
class ProjectionManifest:
    bin: dict[str, list[str]]
    lib: dict[str, list[str]]
    e2e: dict[str, list[str]]
    command: dict[str, list[str]]

    @property
    def total_mappings(self) -> int:
        return sum(len(projection) for projection in (self.bin, self.lib, self.e2e, self.command))


def build_projection_manifest(configs: Sequence[ProductConfig]) -> ProjectionManifest:
    projections: dict[str, dict[str, list[str]]] = {
        "bin": {},
        "lib": {},
        "e2e": {},
        "command": {},
    }
    for config in configs:
        for name, projection in projections.items():
            projection.setdefault(config.projection_id(name), []).append(config.id)
    return ProjectionManifest(**projections)


def validate_projection_manifest(
    configs: Sequence[ProductConfig], manifest: ProjectionManifest
) -> None:
    expected_ids = {config.id for config in configs}
    if len(expected_ids) != 768:
        raise ValueError(f"product space must contain 768 unique configs, got {len(expected_ids)}")

    expected_shapes = {
        "bin": (768, 1),
        "lib": (192, 4),
        "e2e": (192, 4),
        "command": (3, 256),
    }
    for name, (key_count, preimage_size) in expected_shapes.items():
        projection = getattr(manifest, name)
        if len(projection) != key_count:
            raise ValueError(f"{name} projection has {len(projection)} keys, expected {key_count}")
        seen: dict[str, int] = {config_id: 0 for config_id in expected_ids}
        for values in projection.values():
            for config_id in values:
                if config_id not in seen:
                    raise ValueError(f"{name} projection contains unknown config {config_id}")
                seen[config_id] += 1
        invalid = sorted(config_id for config_id, count in seen.items() if count != 1)
        if invalid:
            raise ValueError(
                f"every product config must map exactly once in {name} projection: {invalid[:8]}"
            )
        if {len(values) for values in projection.values()} != {preimage_size}:
            raise ValueError(
                f"{name} projection preimages must all have size {preimage_size}"
            )

    if manifest.total_mappings != 1_155:
        raise ValueError(
            f"projection manifest must contain 1155 mappings, got {manifest.total_mappings}"
        )


def core_matrix_row(config: ProductConfig, *, seed: int) -> dict[str, str]:
    return {
        "config_id": config.id,
        "seed": str(seed),
        "profile": "release",
        "provider": config.provider,
        "log_level": config.log_level,
        "boolean_features": _feature_csv(
            tuple(feature for feature in BOOLEAN_FEATURES if feature in config.enabled)
        ),
        "bin_features": _feature_csv(config.features_for("bin")),
        "lib_features": _feature_csv(config.features_for("lib")),
        "e2e_features": _feature_csv(config.features_for("e2e")),
        "command_features": _feature_csv(config.features_for("command")),
        "tui": str("tui" in config.enabled).lower(),
        "tolerant": str("tolerant-http1-parser" in config.enabled).lower(),
    }


def grpc_matrix(configs: Sequence[ProductConfig], *, seed: int) -> list[dict[str, str]]:
    rows = []
    for config in configs:
        row = core_matrix_row(config, seed=seed)
        row["cell_id"] = f"grpc/{config.projection_id('e2e')}"
        row["e2e_features"] = _feature_csv((*config.features_for("e2e"), "grpc-e2e"))
        rows.append(row)
    return rows


def service_matrix(configs: Sequence[ProductConfig], *, seed: int) -> list[dict[str, str]]:
    rows = []
    for config in configs:
        for service_name, service in SERVICES.items():
            row = core_matrix_row(config, seed=seed)
            row.update(
                {
                    "cell_id": f"services/{service_name}/{config.projection_id('e2e')}",
                    "service": service_name,
                    "service_feature": service.feature,
                    "test": service.test,
                    "image": service.image,
                    "e2e_features": _feature_csv(
                        (*config.features_for("e2e"), service.feature)
                    ),
                }
            )
            rows.append(row)
    return rows


def github_matrix(projection: str, *, seed: int = DEFAULT_SEED) -> str:
    configs = bounded_product_configs(seed)
    if projection == "core":
        rows = [core_matrix_row(config, seed=seed) for config in configs]
    elif projection == "grpc":
        rows = grpc_matrix(configs, seed=seed)
    elif projection == "services":
        rows = service_matrix(configs, seed=seed)
    else:
        raise ValueError(f"unknown GitHub matrix projection: {projection}")
    return json.dumps({"include": rows}, sort_keys=True, separators=(",", ":"))


def expected_manifest_features() -> dict[str, set[str]]:
    log_features = {feature for feature in LOG_FEATURES.values() if feature is not None}
    providers = set(PROVIDERS)
    shared = set(SHARED_FEATURES)
    return {
        "bin": {"default", "unstable", *providers, *BOOLEAN_FEATURES, *log_features},
        "lib": {"default", "e2e-hooks", *providers, *shared, *log_features},
        "command": {"unstable", *log_features},
        "e2e": {
            "default",
            "grpc-e2e",
            *providers,
            *shared,
            *log_features,
            *(service.feature for service in SERVICES.values()),
        },
    }


def read_manifest_features(repo_root: pathlib.Path) -> dict[str, set[str]]:
    manifest_paths = {
        "bin": repo_root / "bin" / "Cargo.toml",
        "lib": repo_root / "lib" / "Cargo.toml",
        "command": repo_root / "command" / "Cargo.toml",
        "e2e": repo_root / "e2e" / "Cargo.toml",
    }
    declared = {}
    for name, path in manifest_paths.items():
        with path.open("rb") as handle:
            manifest = tomllib.load(handle)
        declared[name] = _manifest_feature_names(manifest)
    return declared


def _manifest_feature_names(manifest: Mapping[str, object]) -> set[str]:
    feature_table = manifest.get("features", {})
    if not isinstance(feature_table, Mapping):
        raise ValueError("Cargo manifest [features] must be a table")
    features = set(feature_table)
    explicitly_hidden_dependencies = {
        item.removeprefix("dep:")
        for values in feature_table.values()
        if isinstance(values, list)
        for item in values
        if isinstance(item, str) and item.startswith("dep:")
    }

    dependency_tables: list[Mapping[str, object]] = []
    for dependency_kind in ("dependencies", "build-dependencies"):
        dependencies = manifest.get(dependency_kind, {})
        if isinstance(dependencies, Mapping):
            dependency_tables.append(dependencies)
    targets = manifest.get("target", {})
    if isinstance(targets, Mapping):
        for target in targets.values():
            if not isinstance(target, Mapping):
                continue
            for dependency_kind in ("dependencies", "build-dependencies"):
                target_dependencies = target.get(dependency_kind, {})
                if isinstance(target_dependencies, Mapping):
                    dependency_tables.append(target_dependencies)

    for table in dependency_tables:
        for dependency, specification in table.items():
            if (
                isinstance(specification, Mapping)
                and specification.get("optional") is True
                and dependency not in explicitly_hidden_dependencies
            ):
                features.add(dependency)
    return features


def validate_manifest_features(declared: Mapping[str, set[str]]) -> None:
    expected = expected_manifest_features()
    for package in expected:
        actual_features = declared.get(package)
        if actual_features is None:
            raise ValueError(f"missing feature inventory for {package}")
        unclassified = actual_features.difference(expected[package])
        missing = expected[package].difference(actual_features)
        if unclassified:
            raise ValueError(
                f"unclassified {package} features: {', '.join(sorted(unclassified))}"
            )
        if missing:
            raise ValueError(f"missing {package} features: {', '.join(sorted(missing))}")


@dataclasses.dataclass(frozen=True)
class CellIdentity:
    cell_id: str
    suite: str
    source: str
    cargo_lock: str
    toolchain: str
    generator: str
    command: str
    effective_features: str
    inventory: str

    @classmethod
    def for_test(cls, cell_id: str, *, command: str = "cargo test") -> CellIdentity:
        return cls(
            cell_id=cell_id,
            suite="test",
            source="source",
            cargo_lock="lock",
            toolchain="toolchain",
            generator="generator",
            command=command,
            effective_features="features",
            inventory="inventory",
        )

    @property
    def digest(self) -> str:
        payload = json.dumps(dataclasses.asdict(self), sort_keys=True, separators=(",", ":"))
        return hashlib.sha256(payload.encode()).hexdigest()


def _process_stat(pid: int) -> tuple[int, int]:
    raw = (pathlib.Path("/proc") / str(pid) / "stat").read_text(encoding="utf-8")
    try:
        fields = raw.rsplit(") ", 1)[1].split()
        process_group_id = int(fields[2])
        start_time_ticks = int(fields[19])
    except (IndexError, ValueError) as error:
        raise RuntimeError(f"cannot parse process identity for pid {pid}") from error
    return process_group_id, start_time_ticks


def _process_state(pid: int) -> str:
    raw = (pathlib.Path("/proc") / str(pid) / "stat").read_text(encoding="utf-8")
    try:
        return raw.rsplit(") ", 1)[1].split()[0]
    except IndexError as error:
        raise RuntimeError(f"cannot parse process state for pid {pid}") from error


def _attempt_owned_pids(attempt_id: str) -> tuple[int, ...]:
    marker = f"{ATTEMPT_ENVIRONMENT}={attempt_id}".encode()
    owned = []
    for process in pathlib.Path("/proc").iterdir():
        if not process.name.isdigit() or int(process.name) == os.getpid():
            continue
        try:
            environment = (process / "environ").read_bytes().split(b"\0")
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
        if marker in environment:
            owned.append(int(process.name))
    return tuple(sorted(owned))


def _recorded_live_leaders(payload: Mapping[str, object]) -> tuple[int, ...]:
    process_groups = payload.get("process_groups", [])
    if not isinstance(process_groups, list):
        raise RuntimeError("orphan claim has invalid process group ownership")
    live = []
    for record in process_groups:
        if not isinstance(record, Mapping):
            raise RuntimeError("orphan claim has invalid process group record")
        try:
            leader_pid = int(record["leader_pid"])
            expected = (
                int(record["process_group_id"]),
                int(record["start_time_ticks"]),
            )
            actual = _process_stat(leader_pid)
            state = _process_state(leader_pid)
        except (FileNotFoundError, ProcessLookupError):
            continue
        except (KeyError, TypeError, ValueError) as error:
            raise RuntimeError("orphan claim has incomplete process identity") from error
        if actual == expected and state != "Z":
            live.append(leader_pid)
    return tuple(sorted(live))


def _recorded_live_process_groups(payload: Mapping[str, object]) -> tuple[int, ...]:
    """Return every live member of a recorded group, even after its leader exits."""
    records = payload.get("process_groups", [])
    if not isinstance(records, list):
        raise RuntimeError("orphan claim has invalid process group ownership")
    expected_groups: set[int] = set()
    for record in records:
        if not isinstance(record, Mapping):
            raise RuntimeError("orphan claim has invalid process group record")
        try:
            expected_groups.add(int(record["process_group_id"]))
        except (KeyError, TypeError, ValueError) as error:
            raise RuntimeError("orphan claim has incomplete process group identity") from error
    live: set[int] = set()
    ambiguous = False
    for process in pathlib.Path("/proc").iterdir():
        if not process.name.isdigit() or int(process.name) == os.getpid():
            continue
        pid = int(process.name)
        try:
            process_group_id, _ = _process_stat(pid)
            state = _process_state(pid)
        except (FileNotFoundError, ProcessLookupError):
            continue
        except PermissionError:
            ambiguous = True
            continue
        if process_group_id in expected_groups and state != "Z":
            live.add(pid)
    if ambiguous and expected_groups:
        live.update(expected_groups)
    return tuple(sorted(live))


class CampaignState:
    def __init__(self, root: pathlib.Path) -> None:
        self.root = root
        self.receipts = root / "receipts"
        self.outcomes = root / "outcomes"
        self.attempts = root / "attempts"
        self.claims = root / "claims"
        self.receipts.mkdir(parents=True, exist_ok=True)
        self.outcomes.mkdir(parents=True, exist_ok=True)
        self.attempts.mkdir(parents=True, exist_ok=True)
        self.claims.mkdir(parents=True, exist_ok=True)
        self.blocked_claims: dict[str, tuple[int, ...]] = {}

    def receipt_path(self, identity: CellIdentity) -> pathlib.Path:
        return self.receipts / f"{identity.digest}.json"

    def outcome_path(self, identity: CellIdentity) -> pathlib.Path:
        return self.outcomes / f"{identity.digest}.json"

    def attempt_path(self, identity: CellIdentity, attempt_id: str) -> pathlib.Path:
        return self.attempts / identity.digest / f"{attempt_id}.json"

    def claim_path(self, identity: CellIdentity) -> pathlib.Path:
        return self.claims / f"{identity.digest}.json"

    def _write_path(self, target: pathlib.Path, payload: Mapping[str, object]) -> None:
        target.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=target.parent,
            prefix=f".{target.name}.",
            delete=False,
        ) as handle:
            json.dump(payload, handle, sort_keys=True)
            handle.write("\n")
            temporary = pathlib.Path(handle.name)
        os.replace(temporary, target)

    def start(
        self,
        identity: CellIdentity,
        *,
        owner: Mapping[str, object] | None = None,
    ) -> str:
        attempt_id = uuid.uuid4().hex
        claim = self.claim_path(identity)
        claim.parent.mkdir(parents=True, exist_ok=True)
        descriptor = os.open(claim, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "attempt_id": attempt_id,
                    "identity_sha256": identity.digest,
                    "owner": dict(owner or {}),
                    "process_groups": [],
                },
                handle,
                sort_keys=True,
            )
            handle.write("\n")
        self._write_path(
            self.attempt_path(identity, attempt_id),
            {
                "attempt_id": attempt_id,
                "identity": dataclasses.asdict(identity),
                "identity_sha256": identity.digest,
                "state": "running",
            },
        )
        return attempt_id

    def note_process_group(
        self, identity: CellIdentity, attempt_id: str, leader_pid: int
    ) -> None:
        claim = self.claim_path(identity)
        payload = json.loads(claim.read_text(encoding="utf-8"))
        if payload.get("attempt_id") != attempt_id:
            raise RuntimeError(f"cell claim changed while {identity.cell_id} was running")
        stat = _process_stat(leader_pid)
        process_groups = payload.get("process_groups")
        if not isinstance(process_groups, list):
            raise RuntimeError(f"cell claim has invalid process groups: {claim}")
        process_groups.append(
            {
                "leader_pid": leader_pid,
                "process_group_id": stat[0],
                "start_time_ticks": stat[1],
            }
        )
        self._write_path(claim, payload)

    def recover_orphaned_claims(self) -> list[pathlib.Path]:
        recovered = []
        blocked: dict[str, tuple[int, ...]] = {}
        for claim in sorted(self.claims.glob("*.json")):
            digest = claim.stem
            payload = json.loads(claim.read_text(encoding="utf-8"))
            attempt_id = payload.get("attempt_id")
            if not isinstance(attempt_id, str) or not attempt_id:
                raise RuntimeError(f"orphan claim has no valid attempt id: {claim}")
            live = tuple(
                sorted(
                    set(_attempt_owned_pids(attempt_id)).union(
                        _recorded_live_leaders(payload),
                        _recorded_live_process_groups(payload),
                    )
                )
            )
            if live:
                blocked[digest] = live
                continue
            orphan = self.attempts / digest / f"orphan-claim-{time.time_ns()}.json"
            orphan.parent.mkdir(parents=True, exist_ok=True)
            os.replace(claim, orphan)
            recovered.append(orphan)
        self.blocked_claims = blocked
        return recovered

    def finish(
        self,
        identity: CellIdentity,
        *,
        attempt_id: str,
        exit_code: int,
        inventory_hash: str,
        details: Mapping[str, object] | None = None,
    ) -> None:
        claim = self.claim_path(identity)
        claim_payload = json.loads(claim.read_text(encoding="utf-8"))
        if claim_payload.get("attempt_id") != attempt_id:
            raise RuntimeError(f"cell claim changed while {identity.cell_id} was running")
        live = tuple(
            sorted(
                set(_attempt_owned_pids(attempt_id)).union(
                    _recorded_live_leaders(claim_payload),
                    _recorded_live_process_groups(claim_payload),
                )
            )
        )
        if live:
            raise RuntimeError(
                f"cannot finish {identity.cell_id} with live owned processes: {list(live)}"
            )
        state = "success" if exit_code == 0 else "failed"
        payload: dict[str, object] = {
            "identity": dataclasses.asdict(identity),
            "identity_sha256": identity.digest,
            "attempt_id": attempt_id,
            "state": state,
            "exit_code": exit_code,
            "inventory_sha256": inventory_hash,
        }
        if details is not None:
            payload["details"] = details
        self._write_path(self.attempt_path(identity, attempt_id), payload)
        self._write_path(self.outcome_path(identity), payload)
        if exit_code == 0:
            self._write_path(self.receipt_path(identity), payload)
        claim.unlink()

    def terminal_result(self, identity: CellIdentity) -> Mapping[str, object] | None:
        try:
            payload = json.loads(self.outcome_path(identity).read_text(encoding="utf-8"))
        except (OSError, ValueError, TypeError):
            return None
        state = payload.get("state")
        exit_code = payload.get("exit_code")
        if state not in {"success", "failed"} or not isinstance(exit_code, int):
            return None
        if (state == "success") != (exit_code == 0):
            return None
        if (
            payload.get("identity") != dataclasses.asdict(identity)
            or payload.get("identity_sha256") != identity.digest
            or not isinstance(payload.get("inventory_sha256"), str)
            or not payload["inventory_sha256"]
        ):
            return None
        return payload

    def is_success(self, identity: CellIdentity) -> bool:
        try:
            payload = json.loads(self.receipt_path(identity).read_text(encoding="utf-8"))
        except (OSError, ValueError, TypeError):
            return False
        return (
            payload.get("identity") == dataclasses.asdict(identity)
            and payload.get("identity_sha256") == identity.digest
            and payload.get("state") == "success"
            and payload.get("exit_code") == 0
            and isinstance(payload.get("inventory_sha256"), str)
            and bool(payload["inventory_sha256"])
        )


class CampaignLease:
    def __init__(self, root: pathlib.Path) -> None:
        self.path = root / "campaign.lock"
        self.handle: object | None = None

    def __enter__(self) -> CampaignLease:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        handle = self.path.open("a+", encoding="utf-8")
        try:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            handle.close()
            raise RuntimeError(f"another campaign orchestrator owns {self.path}") from None
        handle.seek(0)
        handle.truncate()
        handle.write(f"pid={os.getpid()}\n")
        handle.flush()
        self.handle = handle
        return self

    def __exit__(self, _type: object, _value: object, _traceback: object) -> None:
        if self.handle is None:
            return
        handle = self.handle
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        handle.close()
        self.handle = None


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", type=int, default=DEFAULT_SEED)
    subparsers = parser.add_subparsers(dest="command", required=True)

    matrix = subparsers.add_parser("matrix", help="emit a GitHub Actions matrix")
    matrix.add_argument("projection", choices=("core", "grpc", "services"))

    exhaustive = subparsers.add_parser("exhaustive", help="emit exhaustive projections")
    exhaustive.add_argument(
        "projection", choices=("product", "bin", "lib", "e2e", "command")
    )

    validate = subparsers.add_parser("validate", help="validate manifests and coverage")
    validate.add_argument("--repo-root", type=pathlib.Path, default=pathlib.Path.cwd())
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    if args.command == "matrix":
        print(github_matrix(args.projection, seed=args.seed))
        return 0

    product = exhaustive_product_configs()
    projections = build_projection_manifest(product)
    validate_projection_manifest(product, projections)
    if args.command == "exhaustive":
        if args.projection == "product":
            payload = [core_matrix_row(config, seed=args.seed) for config in product]
        else:
            payload = getattr(projections, args.projection)
        print(json.dumps(payload, sort_keys=True, separators=(",", ":")))
        return 0

    validate_manifest_features(read_manifest_features(args.repo_root.resolve()))
    bounded_product_configs(args.seed)
    print(
        json.dumps(
            {
                "bounded": 16,
                "product": len(product),
                "mappings": projections.total_mappings,
                "seed": args.seed,
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
