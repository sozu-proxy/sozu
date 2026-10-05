#!/usr/bin/env python3

import dataclasses
import gzip
import json
import hashlib
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock


SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))

import feature_matrix  # noqa: E402
import run_feature_matrix  # noqa: E402


class FeatureMatrixTests(unittest.TestCase):
    def test_exhaustive_space_and_projections_are_total(self) -> None:
        product = feature_matrix.exhaustive_product_configs()
        self.assertEqual(len(product), 768)
        self.assertEqual(len({config.id for config in product}), 768)

        projections = feature_matrix.build_projection_manifest(product)
        self.assertEqual(projections.total_mappings, 1_155)
        self.assertEqual(len(projections.bin), 768)
        self.assertEqual(len(projections.lib), 192)
        self.assertEqual(len(projections.e2e), 192)
        self.assertEqual(len(projections.command), 3)
        self.assertEqual({len(value) for value in projections.bin.values()}, {1})
        self.assertEqual({len(value) for value in projections.lib.values()}, {4})
        self.assertEqual({len(value) for value in projections.e2e.values()}, {4})
        self.assertEqual({len(value) for value in projections.command.values()}, {256})
        feature_matrix.validate_projection_manifest(product, projections)

    def test_projection_validation_rejects_a_missing_product_config(self) -> None:
        product = feature_matrix.exhaustive_product_configs()
        projections = feature_matrix.build_projection_manifest(product)
        first_key = next(iter(projections.bin))
        projections.bin[first_key].pop()

        with self.assertRaisesRegex(ValueError, "must map exactly once"):
            feature_matrix.validate_projection_manifest(product, projections)

    def test_projection_validation_rejects_a_collision(self) -> None:
        product = feature_matrix.exhaustive_product_configs()
        projections = feature_matrix.build_projection_manifest(product)
        keys = list(projections.bin)
        projections.bin[keys[0]].extend(projections.bin[keys[1]])

        with self.assertRaisesRegex(ValueError, "bin projection"):
            feature_matrix.validate_projection_manifest(product, projections)

    def test_bounded_matrix_has_full_pairwise_coverage(self) -> None:
        configs = feature_matrix.bounded_product_configs(seed=20_261_005)
        self.assertEqual(len(configs), 16)
        self.assertEqual(len({config.id for config in configs}), 16)
        self.assertEqual(
            {provider: sum(config.provider == provider for config in configs) for provider in feature_matrix.PROVIDERS},
            {provider: 4 for provider in feature_matrix.PROVIDERS},
        )
        feature_matrix.validate_pairwise_coverage(configs)

    def test_bounded_matrix_is_replayable_and_seeded(self) -> None:
        first = feature_matrix.bounded_product_configs(seed=20_261_005)
        replay = feature_matrix.bounded_product_configs(seed=20_261_005)
        other = feature_matrix.bounded_product_configs(seed=20_261_006)

        self.assertEqual(first, replay)
        self.assertNotEqual(first, other)

    def test_bounded_matrix_keeps_the_contractual_row_shapes(self) -> None:
        configs = feature_matrix.bounded_product_configs(seed=20_261_005)
        all_features = frozenset(feature_matrix.BOOLEAN_FEATURES)

        for provider in feature_matrix.PROVIDERS:
            self.assertIn(feature_matrix.ProductConfig(provider, "off", frozenset()), configs)
            self.assertIn(feature_matrix.ProductConfig(provider, "trace", all_features), configs)

        debug = [config for config in configs if config.log_level == "debug"]
        self.assertEqual(len(debug), 4)
        self.assertEqual({len(config.enabled) for config in debug}, {3})
        memberships = {
            feature: tuple(index for index, config in enumerate(debug) if feature in config.enabled)
            for feature in feature_matrix.BOOLEAN_FEATURES
        }
        self.assertEqual(len(set(memberships.values())), 6)
        self.assertEqual({len(indices) for indices in memberships.values()}, {2})

        for log_level in ("off", "trace"):
            extra = [
                config
                for config in configs
                if config.log_level == log_level
                and config.enabled not in {frozenset(), all_features}
            ]
            self.assertEqual(len(extra), 2)
            self.assertEqual(extra[0].enabled ^ extra[1].enabled, all_features)

        snapshot = "\n".join(config.id for config in configs).encode()
        self.assertEqual(
            hashlib.sha256(snapshot).hexdigest(),
            "76bcf6f0157148e4fac7d74e51e0110b05b46994ad24ad8bcf86b5083a578160",
        )

    def test_bounded_matrix_keeps_projected_ci_cell_ids_unique_across_seeds(self) -> None:
        for seed in (*range(10_000), 20_261_005):
            configs = feature_matrix.bounded_product_configs(seed=seed)
            for projection in ("lib", "e2e"):
                ids = [config.projection_id(projection) for config in configs]
                self.assertEqual(len(ids), len(set(ids)), (seed, projection, ids))

    def test_bounded_projection_validation_rejects_ignored_bit_collisions(self) -> None:
        configs = feature_matrix.bounded_product_configs(seed=20_261_005)
        first = configs[0]
        toggled = set(first.enabled)
        ignored = "jemallocator"
        if ignored in toggled:
            toggled.remove(ignored)
        else:
            toggled.add(ignored)
        colliding = dataclasses.replace(first, enabled=frozenset(toggled))

        with self.assertRaisesRegex(ValueError, "duplicate cells"):
            feature_matrix.validate_bounded_projection_ids([first, colliding])

    def test_release_rows_forward_logging_and_harness_features(self) -> None:
        for config in feature_matrix.bounded_product_configs(seed=20_261_005):
            row = feature_matrix.core_matrix_row(config, seed=20_261_005)
            self.assertEqual(row["profile"], "release")
            self.assertEqual(row["log_level"], config.log_level)
            self.assertNotIn("e2e-hooks", row["bin_features"].split(","))
            self.assertIn(config.provider, row["bin_features"].split(","))
            self.assertIn(config.provider, row["e2e_features"].split(","))
            if config.log_level != "off":
                log_feature = f"logs-{config.log_level}"
                self.assertIn(log_feature, row["bin_features"].split(","))
                self.assertIn(log_feature, row["e2e_features"].split(","))

    def test_ci_toolchain_mode_uses_the_toolchain_installed_on_path(self) -> None:
        with mock.patch.dict(
            os.environ, {"SOZU_FEATURE_MATRIX_TOOLCHAIN": "path"}, clear=False
        ):
            self.assertEqual(
                run_feature_matrix._cargo("test", "-p", "sozu"),
                ("cargo", "test", "-p", "sozu"),
            )

    def test_every_matrix_workflow_job_installs_supported_python(self) -> None:
        workflows = (
            "feature-swarm.yml",
            "grpc-e2e.yml",
            "protocol-services.yml",
        )
        for name in workflows:
            with self.subTest(workflow=name):
                text = (
                    SCRIPT_DIR.parent / "workflows" / name
                ).read_text(encoding="utf-8")
                self.assertEqual(text.count("actions/setup-python@"), 2)
                self.assertEqual(text.count('python-version: "3.12"'), 2)

    def test_runtime_service_identity_does_not_change_the_command_fingerprint(self) -> None:
        first = run_feature_matrix.CommandSpec(
            "service",
            ("cargo", "test"),
            (("SOZU_PROTOCOL_SERVICE_RUN_ID", "run-one"),),
        )
        second = dataclasses.replace(
            first,
            environment=(("SOZU_PROTOCOL_SERVICE_RUN_ID", "run-two"),),
        )
        self.assertEqual(first.normalized(), second.normalized())

    def test_service_and_grpc_matrices_cover_every_bounded_projection(self) -> None:
        configs = feature_matrix.bounded_product_configs(seed=20_261_005)
        grpc = feature_matrix.grpc_matrix(configs, seed=20_261_005)
        services = feature_matrix.service_matrix(configs, seed=20_261_005)

        self.assertEqual(len(grpc), 16)
        self.assertEqual(len(services), 16 * len(feature_matrix.SERVICES))
        self.assertEqual(len({row["cell_id"] for row in grpc}), 16)
        self.assertEqual(len({row["cell_id"] for row in services}), 16 * len(feature_matrix.SERVICES))
        self.assertEqual({row["config_id"] for row in grpc}, {config.id for config in configs})
        for service in feature_matrix.SERVICES:
            rows = [row for row in services if row["service"] == service]
            self.assertEqual(len(rows), 16)
            self.assertEqual({row["config_id"] for row in rows}, {config.id for config in configs})

    def test_unclassified_manifest_feature_fails_closed(self) -> None:
        declared = feature_matrix.expected_manifest_features()
        declared["bin"].add("new-production-toggle")

        with self.assertRaisesRegex(ValueError, "unclassified.*new-production-toggle"):
            feature_matrix.validate_manifest_features(declared)

    def test_manifest_inventory_includes_implicit_optional_dependency_features(self) -> None:
        declared = feature_matrix.read_manifest_features(SCRIPT_DIR.parent.parent)
        self.assertIn("jemallocator", declared["bin"])

    def test_manifest_inventory_includes_optional_build_dependency_features(self) -> None:
        manifest = {
            "features": {},
            "build-dependencies": {"codegen": {"optional": True}},
            "target": {
                "cfg(unix)": {
                    "build-dependencies": {"unix-codegen": {"optional": True}}
                }
            },
        }
        self.assertEqual(
            feature_matrix._manifest_feature_names(manifest),
            {"codegen", "unix-codegen"},
        )

    def test_dry_run_is_byte_identical_across_python_hash_seeds(self) -> None:
        command = [
            sys.executable,
            str(SCRIPT_DIR / "run_feature_matrix.py"),
            "--mode",
            "exhaustive",
            "--seed",
            "20261005",
            "--repo-root",
            str(SCRIPT_DIR.parent.parent),
            "--state-dir",
            "/tmp/sozu-feature-matrix-determinism-test",
            "--dry-run",
        ]
        outputs = []
        for hash_seed in ("1", "2"):
            environment = dict(os.environ)
            environment["PYTHONHASHSEED"] = hash_seed
            outputs.append(subprocess.run(command, env=environment, check=True, capture_output=True).stdout)
        self.assertEqual(outputs[0], outputs[1])

    def test_cli_emits_github_matrix_json(self) -> None:
        payload = feature_matrix.github_matrix("core", seed=20_261_005)
        decoded = json.loads(payload)
        self.assertEqual(len(decoded["include"]), 16)

    def test_atomic_receipt_never_promotes_nonzero_or_partial_runs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state = feature_matrix.CampaignState(pathlib.Path(directory))
            identity = feature_matrix.CellIdentity.for_test("cell-a")
            attempt_id = state.start(identity)
            self.assertFalse(state.is_success(identity))
            state.finish(
                identity,
                attempt_id=attempt_id,
                exit_code=1,
                inventory_hash="inventory",
            )
            self.assertFalse(state.is_success(identity))
            attempt_id = state.start(identity)
            state.finish(
                identity,
                attempt_id=attempt_id,
                exit_code=0,
                inventory_hash="inventory",
            )
            self.assertTrue(state.is_success(identity))

    def test_failed_outcome_is_terminal_until_explicit_replay(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state = feature_matrix.CampaignState(pathlib.Path(directory))
            identity = feature_matrix.CellIdentity.for_test("cell-failed")
            first_attempt = state.start(identity)
            state.finish(
                identity,
                attempt_id=first_attempt,
                exit_code=7,
                inventory_hash="inventory",
            )

            terminal = state.terminal_result(identity)
            self.assertIsNotNone(terminal)
            assert terminal is not None
            self.assertEqual(terminal["state"], "failed")
            self.assertEqual(
                run_feature_matrix._reused_terminal_result(
                    state, identity, replay_failed=False
                ),
                ("failed", state.outcome_path(identity)),
            )
            self.assertIsNone(
                run_feature_matrix._reused_terminal_result(
                    state, identity, replay_failed=True
                )
            )

            second_attempt = state.start(identity)
            self.assertNotEqual(first_attempt, second_attempt)
            state.finish(
                identity,
                attempt_id=second_attempt,
                exit_code=0,
                inventory_hash="inventory",
            )
            self.assertEqual(
                run_feature_matrix._reused_terminal_result(
                    state, identity, replay_failed=False
                ),
                ("success", state.receipt_path(identity)),
            )

    def test_capacity_load_is_checked_only_when_requested(self) -> None:
        snapshot = {
            "disk_free_bytes": run_feature_matrix.MIN_FREE_DISK_BYTES,
            "memory_available_bytes": run_feature_matrix.MIN_AVAILABLE_MEMORY_BYTES,
            "swap_free_bytes": run_feature_matrix.MIN_FREE_SWAP_BYTES,
            "load": [13.0, 0.0, 0.0],
            "cpu_count": 20,
            "target_bytes": 0,
            "timestamp_unix_ns": 0,
        }
        with self.assertRaisesRegex(RuntimeError, "one-minute load"):
            run_feature_matrix.validate_capacity(snapshot, check_load=True)
        run_feature_matrix.validate_capacity(snapshot, check_load=False)

        low_memory = dict(snapshot)
        low_memory["memory_available_bytes"] = (
            run_feature_matrix.MIN_AVAILABLE_MEMORY_BYTES - 1
        )
        with self.assertRaisesRegex(RuntimeError, "MemAvailable"):
            run_feature_matrix.validate_capacity(low_memory, check_load=False)

    def test_exhaustive_runner_checks_load_only_at_admission(self) -> None:
        config = feature_matrix.ProductConfig("crypto-ring", "off", frozenset())
        cell = run_feature_matrix.SuiteCell(
            id="command/off",
            suite="command",
            projection="command",
            projection_id="off",
            product_ids=(config.id,),
            config=config,
        )
        plan = run_feature_matrix.CampaignPlan(
            "exhaustive", 20_261_005, (config,), (cell,)
        )
        snapshot = {
            "disk_free_bytes": run_feature_matrix.MIN_FREE_DISK_BYTES,
            "memory_available_bytes": run_feature_matrix.MIN_AVAILABLE_MEMORY_BYTES,
            "swap_free_bytes": run_feature_matrix.MIN_FREE_SWAP_BYTES,
            "load": [0.0, 0.0, 0.0],
            "cpu_count": 20,
            "target_bytes": 0,
            "timestamp_unix_ns": 0,
        }

        def succeeds(
            _cell: run_feature_matrix.SuiteCell,
            _specs: object,
            *,
            repo_root: pathlib.Path,
            state_dir: pathlib.Path,
            state: feature_matrix.CampaignState,
            identity: feature_matrix.CellIdentity,
            attempt_id: str,
        ) -> tuple[int, pathlib.Path, dict[str, object]]:
            del repo_root, state, identity, attempt_id
            log_path = state_dir / "cell.log"
            log_path.write_text("ok", encoding="utf-8")
            return 0, log_path, {"commands": []}

        with tempfile.TemporaryDirectory() as directory, mock.patch.multiple(
            run_feature_matrix,
            source_fingerprint=mock.DEFAULT,
            toolchain_fingerprint=mock.DEFAULT,
            generator_fingerprint=mock.DEFAULT,
            resource_snapshot=mock.DEFAULT,
            effective_feature_graph=mock.DEFAULT,
            test_inventory=mock.DEFAULT,
            _run_specs=mock.DEFAULT,
            _cleanup_service_containers=mock.DEFAULT,
            validate_capacity=mock.DEFAULT,
        ) as mocks:
            mocks["source_fingerprint"].return_value = "source"
            mocks["toolchain_fingerprint"].return_value = "toolchain"
            mocks["generator_fingerprint"].return_value = "generator"
            mocks["resource_snapshot"].return_value = snapshot
            mocks["effective_feature_graph"].return_value = b"graph"
            mocks["test_inventory"].return_value = b"one: test"
            mocks["_run_specs"].side_effect = succeeds

            report = run_feature_matrix.run_campaign(
                plan,
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=pathlib.Path(directory),
                suites={"command"},
                dry_run=False,
                jobs=4,
            )

        self.assertTrue(report["all_passed"])
        self.assertEqual(
            [call.kwargs for call in mocks["validate_capacity"].call_args_list],
            [{"check_load": True}, {"check_load": False}],
        )

    def test_truncated_receipt_and_changed_identity_are_not_reused(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state = feature_matrix.CampaignState(pathlib.Path(directory))
            identity = feature_matrix.CellIdentity.for_test("cell-a")
            attempt_id = state.start(identity)
            state.finish(
                identity,
                attempt_id=attempt_id,
                exit_code=0,
                inventory_hash="inventory",
            )
            receipt = state.receipt_path(identity)
            receipt.write_text('{"state":', encoding="utf-8")
            self.assertFalse(state.is_success(identity))

            changed = feature_matrix.CellIdentity.for_test("cell-a", command="different")
            self.assertFalse(state.is_success(changed))

    def test_campaign_lease_refuses_a_second_orchestrator(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            with feature_matrix.CampaignLease(root):
                with self.assertRaisesRegex(RuntimeError, "another campaign orchestrator"):
                    with feature_matrix.CampaignLease(root):
                        self.fail("the second orchestrator unexpectedly acquired the lease")

    def test_checkout_lease_refuses_two_state_directories_for_one_repo(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            repo = root / "repo"
            repo.mkdir()
            first = run_feature_matrix.checkout_campaign_lease_root(
                repo, runtime_root=root / "runtime"
            )
            second = run_feature_matrix.checkout_campaign_lease_root(
                repo, runtime_root=root / "runtime"
            )
            self.assertEqual(first, second)
            with feature_matrix.CampaignLease(first):
                with self.assertRaisesRegex(RuntimeError, "another campaign orchestrator"):
                    with feature_matrix.CampaignLease(second):
                        self.fail("a second state directory bypassed the checkout lease")

    def test_checkout_lease_is_shared_across_distinct_tmpdirs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            repo = root / "repo"
            repo.mkdir()
            first_tmp = root / "tmp-first"
            second_tmp = root / "tmp-second"
            first_tmp.mkdir()
            second_tmp.mkdir()
            program = "\n".join(
                (
                    "import pathlib, sys",
                    f"sys.path.insert(0, {str(SCRIPT_DIR)!r})",
                    "from feature_matrix import CampaignLease",
                    "from run_feature_matrix import checkout_campaign_lease_root",
                    "lock_root = checkout_campaign_lease_root(pathlib.Path(sys.argv[1]))",
                    "with CampaignLease(lock_root):",
                    "    print(lock_root, flush=True)",
                    "    sys.stdin.readline()",
                )
            )

            first_environment = os.environ.copy()
            first_environment["TMPDIR"] = str(first_tmp)
            second_environment = os.environ.copy()
            second_environment["TMPDIR"] = str(second_tmp)
            first = subprocess.Popen(
                (sys.executable, "-c", program, str(repo)),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=first_environment,
            )
            try:
                self.assertIsNotNone(first.stdout)
                first_line = first.stdout.readline().strip()
                self.assertTrue(first_line, "first orchestrator did not publish its lock path")
                first_lock = pathlib.Path(first_line)

                second = subprocess.run(
                    (sys.executable, "-c", program, str(repo)),
                    input="\n",
                    capture_output=True,
                    text=True,
                    env=second_environment,
                    check=False,
                )
                self.assertNotEqual(second.returncode, 0)
                self.assertIn("another campaign orchestrator", second.stderr)
                self.assertTrue(first_lock.is_relative_to("/tmp"))
                self.assertFalse(first_lock.is_relative_to(first_tmp))
                self.assertFalse(first_lock.is_relative_to(second_tmp))
            finally:
                _remaining_stdout, first_stderr = first.communicate(input="\n", timeout=5)
                self.assertEqual(first.returncode, 0, first_stderr)

    def test_orphaned_claim_keeps_attempt_evidence_and_can_resume(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state = feature_matrix.CampaignState(pathlib.Path(directory))
            identity = feature_matrix.CellIdentity.for_test("cell-a")
            first_attempt = state.start(identity)
            with self.assertRaises(FileExistsError):
                state.start(identity)
            recovered = state.recover_orphaned_claims()
            second_attempt = state.start(identity)

            self.assertNotEqual(first_attempt, second_attempt)
            self.assertEqual(
                json.loads(state.attempt_path(identity, first_attempt).read_text())["state"],
                "running",
            )
            self.assertEqual(
                len(list((state.attempts / identity.digest).glob("orphan-claim-*.json"))),
                1,
            )
            self.assertEqual(len(recovered), 1)
            state.finish(
                identity,
                attempt_id=second_attempt,
                exit_code=0,
                inventory_hash="inventory",
            )
            self.assertTrue(state.is_success(identity))
            self.assertFalse(state.claim_path(identity).exists())

    def test_live_owned_process_blocks_orphan_claim_recovery(self) -> None:
        identity = feature_matrix.CellIdentity.for_test("live-orphan")
        with tempfile.TemporaryDirectory() as directory:
            state = feature_matrix.CampaignState(pathlib.Path(directory))
            attempt_id = state.start(identity, owner={"worker": "worker-0"})
            environment = os.environ.copy()
            environment[feature_matrix.ATTEMPT_ENVIRONMENT] = attempt_id
            child = subprocess.Popen(
                (
                    sys.executable,
                    "-c",
                    "import subprocess,sys,time; "
                    "child=subprocess.Popen((sys.executable,'-c','import time; time.sleep(60)')); "
                    "print(child.pid, flush=True); time.sleep(60)",
                ),
                env=environment,
                stdout=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            try:
                assert child.stdout is not None
                descendant_pid = int(child.stdout.readline())
                state.note_process_group(identity, attempt_id, child.pid)

                with self.assertRaisesRegex(RuntimeError, "live owned processes"):
                    state.finish(
                        identity,
                        attempt_id=attempt_id,
                        exit_code=0,
                        inventory_hash="inventory",
                    )
                self.assertEqual(state.recover_orphaned_claims(), [])
                self.assertEqual(
                    state.blocked_claims,
                    {identity.digest: tuple(sorted((child.pid, descendant_pid)))},
                )
                self.assertTrue(state.claim_path(identity).exists())
            finally:
                os.killpg(child.pid, signal.SIGTERM)
                child.wait(timeout=5)
                assert child.stdout is not None
                child.stdout.close()

            recovered = state.recover_orphaned_claims()
            self.assertEqual(len(recovered), 1)
            self.assertFalse(state.claim_path(identity).exists())

    def test_recorded_process_start_time_blocks_when_token_is_unavailable(self) -> None:
        identity = feature_matrix.CellIdentity.for_test("uncertain-orphan")
        with tempfile.TemporaryDirectory() as directory:
            state = feature_matrix.CampaignState(pathlib.Path(directory))
            attempt_id = state.start(identity, owner={"worker": "worker-0"})
            child = subprocess.Popen(
                (sys.executable, "-c", "import time; time.sleep(60)"),
                start_new_session=True,
            )
            try:
                state.note_process_group(identity, attempt_id, child.pid)

                self.assertEqual(state.recover_orphaned_claims(), [])
                self.assertEqual(
                    state.blocked_claims,
                    {identity.digest: (child.pid,)},
                )
                self.assertTrue(state.claim_path(identity).exists())
            finally:
                child.terminate()
                child.wait(timeout=5)

    def test_non_dumpable_descendant_blocks_after_recorded_leader_exits(self) -> None:
        identity = feature_matrix.CellIdentity.for_test("non-dumpable-descendant")
        with tempfile.TemporaryDirectory() as directory:
            state = feature_matrix.CampaignState(pathlib.Path(directory))
            attempt_id = state.start(identity, owner={"worker": "worker-0"})
            environment = os.environ.copy()
            environment[feature_matrix.ATTEMPT_ENVIRONMENT] = attempt_id
            leader = subprocess.Popen(
                (
                    sys.executable,
                    "-c",
                    "import subprocess,sys; "
                    "code='import ctypes,time; ctypes.CDLL(None).prctl(4,0,0,0,0); print(\"READY\",flush=True); time.sleep(60)'; "
                    "child=subprocess.Popen((sys.executable,'-c',code),stdout=subprocess.PIPE,text=True); "
                    "assert child.stdout.readline().strip() == 'READY'; "
                    "print(child.pid, flush=True); sys.stdin.readline()",
                ),
                env=environment,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            assert leader.stdin is not None
            assert leader.stdout is not None
            descendant_pid = int(leader.stdout.readline())
            state.note_process_group(identity, attempt_id, leader.pid)
            leader.stdin.write("exit\n")
            leader.stdin.flush()
            leader.wait(timeout=5)
            try:
                with self.assertRaises(PermissionError):
                    (pathlib.Path("/proc") / str(descendant_pid) / "environ").read_bytes()
                self.assertEqual(state.recover_orphaned_claims(), [])
                self.assertEqual(
                    state.blocked_claims,
                    {identity.digest: (descendant_pid,)},
                )
                self.assertTrue(state.claim_path(identity).exists())
            finally:
                os.killpg(leader.pid, signal.SIGTERM)
                deadline = time.monotonic() + 5
                while (pathlib.Path("/proc") / str(descendant_pid)).exists():
                    if time.monotonic() >= deadline:
                        os.killpg(leader.pid, signal.SIGKILL)
                        break
                    time.sleep(0.01)
                leader.stdout.close()
                leader.stdin.close()

    def test_failed_prepare_invalidates_an_older_dispatch_manifest(self) -> None:
        config = feature_matrix.ProductConfig("crypto-ring", "off", frozenset())
        cell = run_feature_matrix.SuiteCell(
            id="command/off",
            suite="command",
            projection="command",
            projection_id="off",
            product_ids=(config.id,),
            config=config,
        )
        spec = run_feature_matrix.CommandSpec(
            "test-command", ("cargo", "test", "-p", "sozu-command-lib")
        )
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            slot = run_feature_matrix.WorkerSlot(0, root / "worker")
            slot.root.mkdir(parents=True)
            manifest = run_feature_matrix._prepared_manifest_path(slot)
            manifest.write_text('{"stale":true}\n', encoding="utf-8")
            failed = subprocess.CompletedProcess(
                spec.argv,
                101,
                stdout=b'{"reason":"compiler-artifact","executable":"/stale"}\n',
                stderr=b"compile failed",
            )
            with mock.patch.object(run_feature_matrix.subprocess, "run", return_value=failed):
                with self.assertRaises(subprocess.CalledProcessError):
                    run_feature_matrix.prepare_cell_for_worker(
                        cell,
                        (spec,),
                        repo_root=SCRIPT_DIR.parent.parent,
                        state_dir=root,
                        slot=slot,
                    )
            self.assertFalse(manifest.exists())

    def test_prepared_manifest_is_published_atomically(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            slot = run_feature_matrix.WorkerSlot(
                0, pathlib.Path(directory) / "worker"
            )
            slot.root.mkdir(parents=True)
            payload = {"cell_id": "command/off", "executables": []}

            digest = run_feature_matrix._write_prepared_manifest(slot, payload)

            manifest = run_feature_matrix._prepared_manifest_path(slot)
            self.assertEqual(
                json.loads(manifest.read_text(encoding="utf-8")), payload
            )
            self.assertEqual(run_feature_matrix._sha256_file(manifest), digest)
            self.assertEqual(list(slot.root.glob(".prepared.*")), [])

    def test_namespace_launcher_preserves_uid_and_drops_all_capabilities(self) -> None:
        executable = run_feature_matrix.PreparedExecutable(
            "test",
            pathlib.Path("/owned/test-binary"),
            "a" * 64,
            pathlib.Path("/owned/package"),
            ("--exact",),
            (),
        )
        slot = run_feature_matrix.WorkerSlot(1, pathlib.Path("/owned/worker"))
        argv = run_feature_matrix._namespaced_test_argv(executable, slot)
        rendered = " ".join(argv)
        self.assertIn("--map-current-user", argv)
        self.assertIn("--keep-caps", argv)
        self.assertIn("--net", argv)
        self.assertIn("--mount", argv)
        self.assertIn("--ipc", argv)
        self.assertIn("mount --bind", rendered)
        self.assertNotIn("tmpfs", rendered)
        self.assertIn("--bounding-set=-all", rendered)
        self.assertIn("--ambient-caps=-all", rendered)

    def test_builder_environment_keeps_only_target_and_worker_identity(self) -> None:
        slot = run_feature_matrix.WorkerSlot(1, pathlib.Path("/owned/worker"))
        environment = dict(slot.environment())
        self.assertEqual(environment["CARGO_TARGET_DIR"], "/owned/worker/target")
        self.assertEqual(environment["SOZU_FEATURE_MATRIX_WORKER"], "1")
        self.assertNotIn("TMPDIR", environment)

    def test_parallel_namespace_probe_uses_private_disk_backed_gibibyte_files(self) -> None:
        slots = (
            run_feature_matrix.WorkerSlot(0, pathlib.Path("/owned/worker-0")),
            run_feature_matrix.WorkerSlot(1, pathlib.Path("/owned/worker-1")),
        )
        rendered = [" ".join(run_feature_matrix._parallel_namespace_probe_argv(slot)) for slot in slots]
        self.assertTrue(all("mount --bind" in command for command in rendered))
        self.assertTrue(all("tmpfs" not in command for command in rendered))
        self.assertTrue(all("fallocate -l 1G" in command for command in rendered))
        self.assertTrue(
            all("/tmp/feature-swarm-parallel-space-probe" in command for command in rendered)
        )
        self.assertNotEqual(str(slots[0].host_tmp_dir), str(slots[1].host_tmp_dir))

    def test_test_artifacts_run_from_their_package_directory(self) -> None:
        repo_root = pathlib.Path("/owned/sozu")
        expected = {
            "bin": repo_root / "bin",
            "process": repo_root / "bin",
            "tui-process": repo_root / "bin",
            "lib": repo_root / "lib",
            "command": repo_root / "command",
            "e2e": repo_root / "e2e",
            "grpc": repo_root / "e2e",
            "services": repo_root / "e2e",
        }
        config = feature_matrix.exhaustive_product_configs()[0]
        for suite, directory in expected.items():
            with self.subTest(suite=suite):
                cell = run_feature_matrix.SuiteCell(
                    id=f"{suite}/test",
                    suite=suite,
                    projection=suite,
                    projection_id="test",
                    config=config,
                    auxiliary="postgres" if suite == "services" else None,
                    product_ids=(config.id,),
                )
                self.assertEqual(
                    run_feature_matrix._package_working_directory(cell, repo_root),
                    directory,
                )

    def test_cargo_no_run_reports_only_test_artifacts(self) -> None:
        output = b"\n".join(
            json.dumps(payload).encode()
            for payload in (
                {
                    "reason": "compiler-artifact",
                    "executable": "/tmp/unit-test",
                    "profile": {"test": True},
                },
                {
                    "reason": "compiler-artifact",
                    "executable": "/tmp/example",
                    "profile": {"test": False},
                },
            )
        )
        self.assertEqual(
            run_feature_matrix._compiler_test_executables(output),
            (pathlib.Path("/tmp/unit-test"),),
        )

    def test_parallel_runner_preserves_generic_cargo_doctests(self) -> None:
        generic = run_feature_matrix.CommandSpec(
            "library",
            (
                "cargo",
                "test",
                "-p",
                "sozu-lib",
                "--release",
                "--no-default-features",
                "--",
                "--nocapture",
            ),
        )
        self.assertEqual(
            run_feature_matrix._cargo_doctest_argv(generic),
            (
                "cargo",
                "test",
                "--doc",
                "-p",
                "sozu-lib",
                "--release",
                "--no-default-features",
                "--",
                "--nocapture",
            ),
        )

    def test_parallel_runner_does_not_invent_doctests_for_selected_targets(self) -> None:
        for selector in ("--lib", "--test", "--all-targets"):
            with self.subTest(selector=selector):
                spec = run_feature_matrix.CommandSpec(
                    "selected",
                    ("cargo", "test", "-p", "sozu", selector, "selected"),
                )
                self.assertIsNone(run_feature_matrix._cargo_doctest_argv(spec))

    def test_doctest_lane_uses_cargo_target_metadata(self) -> None:
        payload = {
            "packages": [
                {
                    "name": "with-docs",
                    "targets": [
                        {"name": "library", "doctest": True},
                        {"name": "tool", "doctest": False},
                    ],
                },
                {
                    "name": "without-docs",
                    "targets": [{"name": "tool", "doctest": False}],
                },
            ]
        }
        self.assertTrue(
            run_feature_matrix._metadata_package_has_doctests(payload, "with-docs")
        )
        self.assertFalse(
            run_feature_matrix._metadata_package_has_doctests(payload, "without-docs")
        )

    def test_effective_graph_requires_exact_root_and_forwarded_features(self) -> None:
        config = feature_matrix.ProductConfig(
            "crypto-ring",
            "debug",
            frozenset(
                {"opentelemetry", "tolerant-http1-parser", "simd", "splice"}
            ),
        )
        cell = run_feature_matrix.SuiteCell(
            id="e2e/test",
            suite="e2e",
            projection="e2e",
            projection_id=config.projection_id("e2e"),
            product_ids=(config.id,),
            config=config,
        )
        valid = "\n".join(
            (
                "sozu-e2e v2.2.1|crypto-ring,logs-debug,opentelemetry,simd,splice,tolerant-http1-parser",
                "sozu-lib v2.2.1|crypto-ring,e2e-hooks,logs-debug,opentelemetry,simd,splice,tolerant-http1-parser",
                "sozu-command-lib v2.2.1|logs-debug",
            )
        )
        run_feature_matrix._validate_effective_feature_graph(
            cell, valid, SCRIPT_DIR.parent.parent
        )

        extra_provider = valid.replace(
            "sozu-e2e v2.2.1|crypto-ring,",
            "sozu-e2e v2.2.1|crypto-aws-lc-rs,crypto-ring,",
        )
        with self.assertRaisesRegex(RuntimeError, "unexpected=.*crypto-aws-lc-rs"):
            run_feature_matrix._validate_effective_feature_graph(
                cell, extra_provider, SCRIPT_DIR.parent.parent
            )

        missing_forward = valid.replace(
            "sozu-lib v2.2.1|crypto-ring,e2e-hooks,logs-debug,",
            "sozu-lib v2.2.1|crypto-ring,e2e-hooks,",
        )
        with self.assertRaisesRegex(RuntimeError, "missing=.*logs-debug"):
            run_feature_matrix._validate_effective_feature_graph(
                cell, missing_forward, SCRIPT_DIR.parent.parent
            )

    def test_library_graph_does_not_invent_command_logging_forwarding(self) -> None:
        config = feature_matrix.ProductConfig(
            "crypto-ring", "debug", frozenset({"simd"})
        )
        cell = run_feature_matrix.SuiteCell(
            id="lib/debug",
            suite="lib",
            projection="lib",
            projection_id=config.projection_id("lib"),
            product_ids=(config.id,),
            config=config,
        )
        valid = "\n".join(
            (
                "sozu-lib v2.2.1|crypto-ring,logs-debug,simd",
                "sozu-command-lib v2.2.1|",
            )
        )
        run_feature_matrix._validate_effective_feature_graph(
            cell, valid, SCRIPT_DIR.parent.parent
        )

        unexpected_forward = valid.replace(
            "sozu-command-lib v2.2.1|",
            "sozu-command-lib v2.2.1|logs-debug",
        )
        with self.assertRaisesRegex(RuntimeError, "unexpected=.*logs-debug"):
            run_feature_matrix._validate_effective_feature_graph(
                cell, unexpected_forward, SCRIPT_DIR.parent.parent
            )

    def test_release_e2e_inventory_keeps_the_proxy_peer_oracle(self) -> None:
        spec = run_feature_matrix.CommandSpec(
            "generic-e2e", ("cargo", "test", "-p", "sozu-e2e", "--release")
        )
        proxy_test = b"tests::h2_log_context_tests::test_h2_proxy_protocol_peer_is_the_advertised_client: test\n"

        for log_level in ("off", "debug", "trace"):
            with self.subTest(log_level=log_level):
                config = feature_matrix.ProductConfig(
                    "crypto-ring", log_level, frozenset()
                )
                cell = run_feature_matrix.SuiteCell(
                    id=f"e2e/{log_level}",
                    suite="e2e",
                    projection="e2e",
                    projection_id=config.projection_id("e2e"),
                    product_ids=(config.id,),
                    config=config,
                )
                result = subprocess.CompletedProcess(
                    spec.argv, 0, stdout=b"other: test\n", stderr=b""
                )
                with mock.patch.object(
                    run_feature_matrix.subprocess, "run", return_value=result
                ), self.assertRaisesRegex(RuntimeError, "H2 PROXY peer inventory mismatch"):
                    run_feature_matrix.test_inventory(
                        cell, (spec,), SCRIPT_DIR.parent.parent
                    )
                result = subprocess.CompletedProcess(
                    spec.argv, 0, stdout=proxy_test, stderr=b""
                )
                with mock.patch.object(
                    run_feature_matrix.subprocess, "run", return_value=result
                ):
                    run_feature_matrix.test_inventory(
                        cell, (spec,), SCRIPT_DIR.parent.parent
                    )

    def test_exhaustive_campaign_plans_every_applicable_suite(self) -> None:
        plan = run_feature_matrix.build_campaign_plan(mode="exhaustive", seed=20_261_005)
        self.assertEqual(
            plan.counts(),
            {
                "bin": 768,
                "command": 3,
                "e2e": 192,
                "fuzz": 6,
                "grpc": 192,
                "lib": 192,
                "process": 768,
                "services": 1_536,
                "sim": 5,
                "tui-process": 384,
            },
        )
        self.assertEqual(len(plan.cells), 4_046)
        self.assertEqual(len({cell.id for cell in plan.cells}), len(plan.cells))
        fixed = [cell for cell in plan.cells if cell.suite in {"sim", "fuzz"}]
        self.assertEqual(len(fixed), 11)
        self.assertTrue(all(cell.projection == "fixed-auxiliary" for cell in fixed))
        self.assertTrue(all(cell.product_ids == () for cell in fixed))

    def test_bounded_campaign_uses_the_same_suite_mapping(self) -> None:
        plan = run_feature_matrix.build_campaign_plan(mode="bounded", seed=20_261_005)
        self.assertEqual(
            plan.counts(),
            {
                "bin": 16,
                "command": 3,
                "e2e": 16,
                "fuzz": 6,
                "grpc": 16,
                "lib": 16,
                "process": 16,
                "services": 128,
                "sim": 5,
                "tui-process": 8,
            },
        )

    def test_redis_service_cells_retain_container_failure_cleanup_gate(self) -> None:
        plan = run_feature_matrix.build_campaign_plan(mode="bounded", seed=20_261_005)
        redis_cells = [
            cell
            for cell in plan.cells
            if cell.suite == "services" and cell.auxiliary == "redis"
        ]
        self.assertEqual(len(redis_cells), 16)
        for cell in redis_cells:
            specs = run_feature_matrix.command_specs(cell, jobs=4)
            cleanup = [
                spec for spec in specs if spec.label == "test-service-fixture-cleanup"
            ]
            self.assertEqual(len(cleanup), 1)
            self.assertIn(
                "tests::real_services_tcp::fixture::tests::",
                cleanup[0].argv,
            )
            self.assertEqual(
                cleanup[0].runtime_arguments[0],
                "tests::real_services_tcp::fixture::tests::",
            )

    def test_dry_run_writes_no_success_receipts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            plan = run_feature_matrix.build_campaign_plan(mode="bounded", seed=20_261_005)
            report = run_feature_matrix.run_campaign(
                plan,
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=pathlib.Path(directory),
                suites={"command"},
                dry_run=True,
                jobs=4,
            )
            self.assertEqual(report["status"], "dry-run")
            self.assertEqual(list(pathlib.Path(directory).rglob("*.json")), [])

    def test_summary_rejects_missing_failed_and_running_cells(self) -> None:
        plan = run_feature_matrix.build_campaign_plan(mode="bounded", seed=20_261_005)
        statuses = {cell.id: "success" for cell in plan.cells}
        run_feature_matrix.validate_terminal_summary(plan, statuses)
        statuses[plan.cells[0].id] = "failed"
        run_feature_matrix.validate_terminal_summary(plan, statuses)
        for bad_status in (None, "running", "not-run"):
            mutated = dict(statuses)
            target = plan.cells[0].id
            if bad_status is None:
                mutated.pop(target)
            else:
                mutated[target] = bad_status
            with self.assertRaisesRegex(ValueError, "campaign is incomplete"):
                run_feature_matrix.validate_terminal_summary(plan, mutated)

    def test_command_failure_does_not_skip_later_commands_in_the_cell(self) -> None:
        cell = run_feature_matrix.SuiteCell(
            id="sim/keep-going",
            suite="sim",
            projection="fixed-auxiliary",
            projection_id="fixed-ring",
            product_ids=(),
            auxiliary="keep-going",
        )
        with tempfile.TemporaryDirectory() as directory:
            state_dir = pathlib.Path(directory)
            marker = state_dir / "second-command-ran"
            state = feature_matrix.CampaignState(state_dir)
            identity = feature_matrix.CellIdentity.for_test("keep-going")
            attempt_id = state.start(identity)
            specs = (
                run_feature_matrix.CommandSpec(
                    "fails-first",
                    (
                        sys.executable,
                        "-c",
                        "print('first-child-output'); raise SystemExit(7)",
                    ),
                ),
                run_feature_matrix.CommandSpec(
                    "still-runs",
                    (
                        sys.executable,
                        "-c",
                        f"from pathlib import Path; Path({str(marker)!r}).touch(); print('second-child-output')",
                    ),
                ),
            )
            exit_code, log_path, metrics = run_feature_matrix._run_specs(
                cell,
                specs,
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=state_dir,
                state=state,
                identity=identity,
                attempt_id=attempt_id,
            )
            marker_exists = marker.exists()
            with gzip.open(log_path, "rt", encoding="utf-8") as log:
                log_text = log.read()

        self.assertEqual(exit_code, 7)
        self.assertTrue(marker_exists)
        self.assertIn("fails-first", log_text)
        self.assertIn("still-runs", log_text)
        self.assertIn("first-child-output", log_text)
        self.assertIn("second-child-output", log_text)
        self.assertEqual(
            [(command["label"], command["exit_code"]) for command in metrics["commands"]],
            [("fails-first", 7), ("still-runs", 0)],
        )

    def test_command_deadline_kills_its_group_and_keeps_going(self) -> None:
        cell = run_feature_matrix.SuiteCell(
            id="sim/timeout",
            suite="sim",
            projection="fixed-auxiliary",
            projection_id="fixed-ring",
            product_ids=(),
            auxiliary="timeout",
        )
        with tempfile.TemporaryDirectory() as directory:
            state_dir = pathlib.Path(directory)
            marker = state_dir / "later-command-ran"
            state = feature_matrix.CampaignState(state_dir)
            identity = feature_matrix.CellIdentity.for_test("timeout")
            attempt_id = state.start(identity)
            specs = (
                run_feature_matrix.CommandSpec(
                    "times-out",
                    (sys.executable, "-c", "import time; time.sleep(60)"),
                    timeout_seconds=1,
                ),
                run_feature_matrix.CommandSpec(
                    "still-runs",
                    (
                        sys.executable,
                        "-c",
                        f"from pathlib import Path; Path({str(marker)!r}).touch()",
                    ),
                    timeout_seconds=5,
                ),
            )

            exit_code, log_path, metrics = run_feature_matrix._run_specs(
                cell,
                specs,
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=state_dir,
                state=state,
                identity=identity,
                attempt_id=attempt_id,
            )
            claim = json.loads(state.claim_path(identity).read_text(encoding="utf-8"))
            marker_exists = marker.exists()
            with gzip.open(log_path, "rt", encoding="utf-8") as log:
                log_text = log.read()

        self.assertEqual(exit_code, 124)
        self.assertTrue(marker_exists)
        self.assertIn("COMMAND TIMEOUT after 1s", log_text)
        self.assertEqual(
            [command["exit_code"] for command in metrics["commands"]], [124, 0]
        )
        for group in claim["process_groups"]:
            self.assertEqual(
                run_feature_matrix._active_process_group_members(
                    group["process_group_id"]
                ),
                (),
            )

    def test_successful_leader_with_a_live_descendant_is_failed_and_reaped(self) -> None:
        cell = run_feature_matrix.SuiteCell(
            id="sim/descendant",
            suite="sim",
            projection="fixed-auxiliary",
            projection_id="fixed-ring",
            product_ids=(),
            auxiliary="descendant",
        )
        with tempfile.TemporaryDirectory() as directory:
            state_dir = pathlib.Path(directory)
            child_pid_path = state_dir / "child.pid"
            code = (
                "import pathlib,subprocess,sys; "
                "child=subprocess.Popen((sys.executable,'-c','import time; time.sleep(60)'),"
                "stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); "
                f"pathlib.Path({str(child_pid_path)!r}).write_text(str(child.pid))"
            )
            state = feature_matrix.CampaignState(state_dir)
            identity = feature_matrix.CellIdentity.for_test("descendant")
            attempt_id = state.start(identity)
            exit_code, log_path, metrics = run_feature_matrix._run_specs(
                cell,
                (run_feature_matrix.CommandSpec("leaks-child", (sys.executable, "-c", code)),),
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=state_dir,
                state=state,
                identity=identity,
                attempt_id=attempt_id,
            )
            child_pid = int(child_pid_path.read_text(encoding="utf-8"))
            with gzip.open(log_path, "rt", encoding="utf-8") as log:
                log_text = log.read()

        self.assertEqual(exit_code, 125)
        if (pathlib.Path("/proc") / str(child_pid)).exists():
            stat_text = (pathlib.Path("/proc") / str(child_pid) / "stat").read_text()
            self.assertEqual(stat_text[stat_text.rfind(")") + 2 :].split()[0], "Z")
        self.assertIn("owned process group survived command handling", log_text)
        self.assertEqual(metrics["commands"][0]["exit_code"], 125)

    def test_process_group_recording_failure_reaps_the_started_command(self) -> None:
        cell = run_feature_matrix.SuiteCell(
            id="sim/recording-failure",
            suite="sim",
            projection="fixed-auxiliary",
            projection_id="fixed-ring",
            product_ids=(),
            auxiliary="recording-failure",
        )
        recorded_pid: int | None = None

        def reject_recording(
            _identity: feature_matrix.CellIdentity,
            _attempt_id: str,
            process_group_id: int,
        ) -> None:
            nonlocal recorded_pid
            recorded_pid = process_group_id
            deadline = time.monotonic() + 2
            while not run_feature_matrix._active_process_group_members(process_group_id):
                if time.monotonic() >= deadline:
                    self.fail("started command never became an active process group")
                time.sleep(0.01)
            raise RuntimeError("injected process-group recording failure")

        state = mock.Mock()
        state.note_process_group.side_effect = reject_recording
        identity = feature_matrix.CellIdentity.for_test("recording-failure")
        try:
            with tempfile.TemporaryDirectory() as directory:
                with self.assertRaisesRegex(
                    RuntimeError, "injected process-group recording failure"
                ):
                    run_feature_matrix._run_specs(
                        cell,
                        (
                            run_feature_matrix.CommandSpec(
                                "sleep",
                                (sys.executable, "-c", "import time; time.sleep(60)"),
                            ),
                        ),
                        repo_root=SCRIPT_DIR.parent.parent,
                        state_dir=pathlib.Path(directory),
                        state=state,
                        identity=identity,
                        attempt_id="recording-failure-attempt",
                    )
            self.assertIsNotNone(recorded_pid)
            assert recorded_pid is not None
            self.assertEqual(
                run_feature_matrix._active_process_group_members(recorded_pid), ()
            )
        finally:
            if recorded_pid is not None and run_feature_matrix._active_process_group_members(
                recorded_pid
            ):
                os.killpg(recorded_pid, signal.SIGKILL)

    def test_prepared_runtime_recording_failure_reaps_the_started_command(self) -> None:
        recorded_pid: int | None = None

        def reject_recording(
            _identity: feature_matrix.CellIdentity,
            _attempt_id: str,
            process_group_id: int,
        ) -> None:
            nonlocal recorded_pid
            recorded_pid = process_group_id
            deadline = time.monotonic() + 2
            while not run_feature_matrix._active_process_group_members(process_group_id):
                if time.monotonic() >= deadline:
                    self.fail("started command never became an active process group")
                time.sleep(0.01)
            raise RuntimeError("injected process-group recording failure")

        try:
            with tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                slot = run_feature_matrix.WorkerSlot(0, root / "worker")
                slot.root.mkdir(parents=True)
                manifest = run_feature_matrix._prepared_manifest_path(slot)
                manifest.write_text("{}\n", encoding="utf-8")
                executable = run_feature_matrix.PreparedExecutable(
                    "sleep",
                    pathlib.Path(sys.executable),
                    run_feature_matrix._sha256_file(pathlib.Path(sys.executable)),
                    SCRIPT_DIR.parent.parent,
                    (),
                    (),
                    60,
                )
                config = feature_matrix.ProductConfig("crypto-ring", "off", frozenset())
                prepared = run_feature_matrix.PreparedCell(
                    cell=run_feature_matrix.SuiteCell(
                        id="command/recording-failure",
                        suite="command",
                        projection="command",
                        projection_id="off",
                        product_ids=(config.id,),
                        config=config,
                    ),
                    slot=slot,
                    specs=(),
                    executables=(executable,),
                    compile_commands=(),
                    graph=b"graph",
                    inventory=b"inventory",
                    has_doctest_target=False,
                    sozu_binary=None,
                    sozu_binary_sha256=None,
                    manifest_sha256=run_feature_matrix._sha256_file(manifest),
                )
                state = mock.Mock()
                state.note_process_group.side_effect = reject_recording
                with mock.patch.object(
                    run_feature_matrix,
                    "_namespaced_test_argv",
                    return_value=(
                        sys.executable,
                        "-c",
                        "import time; time.sleep(60)",
                    ),
                ), self.assertRaisesRegex(
                    RuntimeError, "injected process-group recording failure"
                ):
                    run_feature_matrix.run_prepared_cell(
                        prepared,
                        repo_root=SCRIPT_DIR.parent.parent,
                        state_dir=root,
                        state=state,
                        identity=feature_matrix.CellIdentity.for_test(
                            "prepared-recording-failure"
                        ),
                        attempt_id="prepared-recording-failure-attempt",
                    )
            self.assertIsNotNone(recorded_pid)
            assert recorded_pid is not None
            self.assertEqual(
                run_feature_matrix._active_process_group_members(recorded_pid), ()
            )
        finally:
            if recorded_pid is not None and run_feature_matrix._active_process_group_members(
                recorded_pid
            ):
                os.killpg(recorded_pid, signal.SIGKILL)

    def test_doctest_recording_failure_reaps_the_started_command(self) -> None:
        recorded_pid: int | None = None

        def reject_recording(
            _identity: feature_matrix.CellIdentity,
            _attempt_id: str,
            process_group_id: int,
        ) -> None:
            nonlocal recorded_pid
            recorded_pid = process_group_id
            deadline = time.monotonic() + 2
            while not run_feature_matrix._active_process_group_members(process_group_id):
                if time.monotonic() >= deadline:
                    self.fail("started command never became an active process group")
                time.sleep(0.01)
            raise RuntimeError("injected process-group recording failure")

        try:
            with tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                slot = run_feature_matrix.WorkerSlot(0, root / "worker")
                slot.root.mkdir(parents=True)
                spec = run_feature_matrix.CommandSpec(
                    "doctest", ("cargo", "test", "-p", "sozu-lib")
                )
                config = feature_matrix.ProductConfig("crypto-ring", "off", frozenset())
                prepared = run_feature_matrix.PreparedCell(
                    cell=run_feature_matrix.SuiteCell(
                        id="lib/recording-failure",
                        suite="lib",
                        projection="lib",
                        projection_id=config.projection_id("lib"),
                        product_ids=(config.id,),
                        config=config,
                    ),
                    slot=slot,
                    specs=(spec,),
                    executables=(),
                    compile_commands=(),
                    graph=b"graph",
                    inventory=b"inventory",
                    has_doctest_target=True,
                    sozu_binary=None,
                    sozu_binary_sha256=None,
                    manifest_sha256="unused",
                )
                state = mock.Mock()
                state.note_process_group.side_effect = reject_recording
                with mock.patch.object(
                    run_feature_matrix,
                    "_cargo_doctest_argv",
                    return_value=(
                        sys.executable,
                        "-c",
                        "import time; time.sleep(60)",
                    ),
                ), self.assertRaisesRegex(
                    RuntimeError, "injected process-group recording failure"
                ):
                    run_feature_matrix.run_serial_doctests(
                        prepared,
                        repo_root=SCRIPT_DIR.parent.parent,
                        state=state,
                        identity=feature_matrix.CellIdentity.for_test(
                            "doctest-recording-failure"
                        ),
                        attempt_id="doctest-recording-failure-attempt",
                        log_path=root / "doctest.log.gz",
                    )
            self.assertIsNotNone(recorded_pid)
            assert recorded_pid is not None
            self.assertEqual(
                run_feature_matrix._active_process_group_members(recorded_pid), ()
            )
        finally:
            if recorded_pid is not None and run_feature_matrix._active_process_group_members(
                recorded_pid
            ):
                os.killpg(recorded_pid, signal.SIGKILL)

    def test_service_run_identity_is_unique_per_attempt_and_cleanup_is_exact(self) -> None:
        config = feature_matrix.ProductConfig("crypto-ring", "off", frozenset())
        cell = run_feature_matrix.SuiteCell(
            id="services/redis/test",
            suite="services",
            projection="e2e",
            projection_id=config.projection_id("e2e"),
            product_ids=(config.id,),
            config=config,
            auxiliary="redis",
        )
        base_spec = run_feature_matrix.CommandSpec(
            "redis",
            ("cargo", "test"),
            environment=(("SOZU_PROTOCOL_SERVICE_RUN_ID", "non-owned-placeholder"),),
        )
        first = run_feature_matrix._service_run_id(config, "redis", "attempt-one")
        second = run_feature_matrix._service_run_id(config, "redis", "attempt-two")
        self.assertNotEqual(first, second)
        self.assertEqual(
            dict(run_feature_matrix._service_attempt_specs(cell, (base_spec,), "attempt-one")[0].environment)[
                "SOZU_PROTOCOL_SERVICE_RUN_ID"
            ],
            first,
        )

        calls: list[tuple[str, ...]] = []

        def docker(argv: tuple[str, ...], **_kwargs: object) -> subprocess.CompletedProcess[str]:
            calls.append(tuple(argv))
            if argv[:3] == ("docker", "ps", "-aq"):
                output = "owned-first\n" if calls.count(tuple(argv)) == 1 else ""
                return subprocess.CompletedProcess(argv, 0, stdout=output)
            return subprocess.CompletedProcess(argv, 0, stdout="")

        with mock.patch.object(run_feature_matrix.subprocess, "run", side_effect=docker):
            run_feature_matrix._cleanup_service_containers(
                cell, run_id=first, timeout_seconds=5
            )

        self.assertIn(("docker", "rm", "--volumes", "--force", "owned-first"), calls)
        self.assertTrue(
            all(second not in argument for call in calls for argument in call),
            "cleanup for one attempt referenced another attempt identity",
        )

    def test_fuzz_toolchain_identity_tracks_nightly_and_cargo_fuzz(self) -> None:
        fuzz_version = [b"cargo-fuzz 0.13.1\n"]

        def version(argv: tuple[str, ...], **_kwargs: object) -> subprocess.CompletedProcess[bytes]:
            output = (
                fuzz_version[0]
                if tuple(argv[-3:]) == ("cargo", "fuzz", "--version")
                else (" ".join(argv) + "\n").encode()
            )
            return subprocess.CompletedProcess(argv, 0, stdout=output, stderr=b"")

        with mock.patch.object(run_feature_matrix.subprocess, "run", side_effect=version):
            stable_before = run_feature_matrix.toolchain_fingerprint(SCRIPT_DIR.parent.parent)
            fuzz_before = run_feature_matrix.toolchain_fingerprint(
                SCRIPT_DIR.parent.parent, include_fuzz=True
            )
            fuzz_version[0] = b"cargo-fuzz 0.14.0\n"
            stable_after = run_feature_matrix.toolchain_fingerprint(SCRIPT_DIR.parent.parent)
            fuzz_after = run_feature_matrix.toolchain_fingerprint(
                SCRIPT_DIR.parent.parent, include_fuzz=True
            )

        self.assertEqual(stable_before, stable_after)
        self.assertNotEqual(fuzz_before, fuzz_after)

    def test_campaign_failure_does_not_skip_later_cells(self) -> None:
        config = feature_matrix.ProductConfig("crypto-ring", "off", frozenset())
        cells = tuple(
            run_feature_matrix.SuiteCell(
                id=f"command/{name}",
                suite="command",
                projection="command",
                projection_id=name,
                product_ids=(config.id,),
                config=config,
            )
            for name in ("first", "second")
        )
        plan = run_feature_matrix.CampaignPlan("bounded", 20_261_005, (config,), cells)

        def run_specs(
            cell: run_feature_matrix.SuiteCell,
            _specs: object,
            *,
            repo_root: pathlib.Path,
            state_dir: pathlib.Path,
            state: feature_matrix.CampaignState,
            identity: feature_matrix.CellIdentity,
            attempt_id: str,
        ) -> tuple[int, pathlib.Path, dict[str, object]]:
            del repo_root, state, identity, attempt_id
            log_path = state_dir / f"{cell.projection_id}.log"
            log_path.write_text(cell.id, encoding="utf-8")
            return (
                1 if cell.projection_id == "first" else 0,
                log_path,
                {"commands": []},
            )

        snapshot = {
            "disk_free_bytes": run_feature_matrix.MIN_FREE_DISK_BYTES,
            "memory_available_bytes": run_feature_matrix.MIN_AVAILABLE_MEMORY_BYTES,
            "swap_free_bytes": run_feature_matrix.MIN_FREE_SWAP_BYTES,
            "load": [0.0, 0.0, 0.0],
            "cpu_count": 1,
            "target_bytes": 0,
            "timestamp_unix_ns": 0,
        }
        with tempfile.TemporaryDirectory() as directory, mock.patch.multiple(
            run_feature_matrix,
            source_fingerprint=mock.DEFAULT,
            toolchain_fingerprint=mock.DEFAULT,
            generator_fingerprint=mock.DEFAULT,
            resource_snapshot=mock.DEFAULT,
            effective_feature_graph=mock.DEFAULT,
            test_inventory=mock.DEFAULT,
            _run_specs=mock.DEFAULT,
            _cleanup_service_containers=mock.DEFAULT,
        ) as mocks:
            mocks["source_fingerprint"].return_value = "source"
            mocks["toolchain_fingerprint"].return_value = "toolchain"
            mocks["generator_fingerprint"].return_value = "generator"
            mocks["resource_snapshot"].return_value = snapshot
            mocks["effective_feature_graph"].return_value = b"graph"
            mocks["test_inventory"].return_value = b"one: test"
            mocks["_run_specs"].side_effect = run_specs
            report = run_feature_matrix.run_campaign(
                plan,
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=pathlib.Path(directory),
                suites={"command"},
                dry_run=False,
                jobs=4,
            )
            first_call_count = mocks["_run_specs"].call_count
            resumed = run_feature_matrix.run_campaign(
                plan,
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=pathlib.Path(directory),
                suites={"command"},
                dry_run=False,
                jobs=4,
            )
            resumed_call_count = mocks["_run_specs"].call_count
            replayed = run_feature_matrix.run_campaign(
                plan,
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=pathlib.Path(directory),
                suites={"command"},
                dry_run=False,
                jobs=4,
                replay_failed=True,
            )
            replayed_call_count = mocks["_run_specs"].call_count

        self.assertEqual(
            report["statuses"],
            {"command/first": "failed", "command/second": "success"},
        )
        self.assertTrue(report["all_executed"])
        self.assertFalse(report["all_passed"])
        self.assertEqual(report["status_counts"], {"failed": 1, "success": 1})
        self.assertEqual(first_call_count, 2)
        self.assertEqual(resumed_call_count, 2)
        self.assertEqual(replayed_call_count, 3)
        self.assertEqual(resumed["statuses"], report["statuses"])
        self.assertTrue(resumed["all_executed"])
        self.assertFalse(resumed["all_passed"])
        self.assertTrue(resumed["receipts"]["command/first"]["reused"])
        self.assertTrue(resumed["receipts"]["command/second"]["reused"])
        self.assertFalse(replayed["receipts"]["command/first"]["reused"])
        self.assertTrue(replayed["receipts"]["command/second"]["reused"])

    def test_serial_campaign_refuses_receipts_after_source_drift(self) -> None:
        config = feature_matrix.ProductConfig("crypto-ring", "off", frozenset())
        cells = tuple(
            run_feature_matrix.SuiteCell(
                id=f"command/{name}",
                suite="command",
                projection="command",
                projection_id=name,
                product_ids=(config.id,),
                config=config,
            )
            for name in ("changes-source", "must-not-run")
        )
        plan = run_feature_matrix.CampaignPlan("bounded", 20_261_005, (config,), cells)
        source = ["admitted-source"]
        snapshot = {
            "disk_free_bytes": run_feature_matrix.MIN_FREE_DISK_BYTES,
            "memory_available_bytes": run_feature_matrix.MIN_AVAILABLE_MEMORY_BYTES,
            "swap_free_bytes": run_feature_matrix.MIN_FREE_SWAP_BYTES,
            "load": [0.0, 0.0, 0.0],
            "cpu_count": 1,
            "target_bytes": 0,
            "timestamp_unix_ns": 0,
        }

        def changes_source(
            cell: run_feature_matrix.SuiteCell,
            _specs: object,
            *,
            repo_root: pathlib.Path,
            state_dir: pathlib.Path,
            state: feature_matrix.CampaignState,
            identity: feature_matrix.CellIdentity,
            attempt_id: str,
        ) -> tuple[int, pathlib.Path, dict[str, object]]:
            del repo_root, state, identity, attempt_id
            source[0] = "changed-source"
            log_path = state_dir / f"{cell.projection_id}.log"
            log_path.write_text("changed", encoding="utf-8")
            return 0, log_path, {"commands": []}

        with tempfile.TemporaryDirectory() as directory, mock.patch.multiple(
            run_feature_matrix,
            source_fingerprint=mock.DEFAULT,
            generator_fingerprint=mock.DEFAULT,
            resource_snapshot=mock.DEFAULT,
            effective_feature_graph=mock.DEFAULT,
            test_inventory=mock.DEFAULT,
            _run_specs=mock.DEFAULT,
            _cleanup_service_containers=mock.DEFAULT,
        ) as mocks:
            mocks["source_fingerprint"].side_effect = lambda _repo: source[0]
            mocks["generator_fingerprint"].return_value = "generator"
            mocks["resource_snapshot"].return_value = snapshot
            mocks["effective_feature_graph"].return_value = b"graph"
            mocks["test_inventory"].return_value = b"one: test"
            mocks["_run_specs"].side_effect = changes_source
            state_dir = pathlib.Path(directory)
            report = run_feature_matrix._run_campaign_locked(
                plan,
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=state_dir,
                suites={"command"},
                dry_run=False,
                jobs=4,
                expected_source="admitted-source",
                expected_toolchains={"stable": "toolchain"},
            )
            outcomes = list((state_dir / "outcomes").glob("*.json"))
            claims = list((state_dir / "claims").glob("*.json"))

        self.assertIn("campaign source changed", report["blocked_reason"])
        self.assertEqual(report["statuses"], {cell.id: "not-run" for cell in cells})
        self.assertEqual(mocks["_run_specs"].call_count, 1)
        self.assertEqual(outcomes, [])
        self.assertEqual(len(claims), 1)

    def test_parallel_campaign_refuses_receipts_after_source_drift(self) -> None:
        config = feature_matrix.ProductConfig("crypto-ring", "off", frozenset())
        cells = tuple(
            run_feature_matrix.SuiteCell(
                id=f"command/{name}",
                suite="command",
                projection="command",
                projection_id=name,
                product_ids=(config.id,),
                config=config,
            )
            for name in ("first", "second")
        )
        plan = run_feature_matrix.CampaignPlan("bounded", 20_261_005, (config,), cells)
        source = ["admitted-source"]
        snapshot = {
            "disk_free_bytes": run_feature_matrix.MIN_FREE_DISK_BYTES,
            "memory_available_bytes": run_feature_matrix.MIN_AVAILABLE_MEMORY_BYTES,
            "swap_free_bytes": run_feature_matrix.MIN_FREE_SWAP_BYTES,
            "load": [0.0, 0.0, 0.0],
            "cpu_count": 1,
            "target_bytes": 0,
            "timestamp_unix_ns": 0,
        }

        def prepare(
            cell: run_feature_matrix.SuiteCell,
            specs: object,
            *,
            repo_root: pathlib.Path,
            state_dir: pathlib.Path,
            slot: run_feature_matrix.WorkerSlot,
        ) -> run_feature_matrix.PreparedCell:
            del repo_root, state_dir
            return run_feature_matrix.PreparedCell(
                cell=cell,
                slot=slot,
                specs=tuple(specs),
                executables=(),
                compile_commands=(),
                graph=b"graph",
                inventory=b"one: test",
                has_doctest_target=False,
                sozu_binary=None,
                sozu_binary_sha256=None,
                manifest_sha256="manifest",
            )

        def changes_source(
            prepared: run_feature_matrix.PreparedCell,
            *,
            repo_root: pathlib.Path,
            state_dir: pathlib.Path,
            state: feature_matrix.CampaignState,
            identity: feature_matrix.CellIdentity,
            attempt_id: str,
        ) -> tuple[int, pathlib.Path, dict[str, object]]:
            del repo_root, state, identity, attempt_id
            source[0] = "changed-source"
            log_path = state_dir / f"{prepared.slot.id}.log"
            log_path.write_text("changed", encoding="utf-8")
            return 0, log_path, {"commands": [], "worker": prepared.slot.id}

        with tempfile.TemporaryDirectory() as directory, mock.patch.multiple(
            run_feature_matrix,
            source_fingerprint=mock.DEFAULT,
            campaign_toolchains=mock.DEFAULT,
            generator_fingerprint=mock.DEFAULT,
            resource_snapshot=mock.DEFAULT,
            verify_parallel_namespace_isolation=mock.DEFAULT,
            prepare_cell_for_worker=mock.DEFAULT,
            run_prepared_cell=mock.DEFAULT,
            _active_build_processes=mock.DEFAULT,
        ) as mocks:
            mocks["source_fingerprint"].side_effect = lambda _repo: source[0]
            mocks["campaign_toolchains"].return_value = {"stable": "toolchain"}
            mocks["generator_fingerprint"].return_value = "generator"
            mocks["resource_snapshot"].return_value = snapshot
            mocks["verify_parallel_namespace_isolation"].return_value = {"status": "ok"}
            mocks["prepare_cell_for_worker"].side_effect = prepare
            mocks["run_prepared_cell"].side_effect = changes_source
            mocks["_active_build_processes"].return_value = []
            state_dir = pathlib.Path(directory)
            report = run_feature_matrix._run_campaign_parallel(
                plan,
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=state_dir,
                suites={"command"},
                jobs=4,
                command_timeout_seconds=30,
                workers=2,
                cell_id=None,
                config_id=None,
                triage_index_path=None,
                replay_failed=False,
            )
            outcomes = list((state_dir / "outcomes").glob("*.json"))
            claims = list((state_dir / "claims").glob("*.json"))

        self.assertIn("campaign source changed", report["blocked_reason"])
        self.assertEqual(report["statuses"], {cell.id: "not-run" for cell in cells})
        self.assertEqual(outcomes, [])
        self.assertEqual(len(claims), 2)

    def test_compile_graph_failure_is_recorded_without_skipping_later_cells(self) -> None:
        config = feature_matrix.ProductConfig("crypto-ring", "off", frozenset())
        cells = tuple(
            run_feature_matrix.SuiteCell(
                id=f"command/{name}",
                suite="command",
                projection="command",
                projection_id=name,
                product_ids=(config.id,),
                config=config,
            )
            for name in ("compile-fails", "still-runs")
        )
        plan = run_feature_matrix.CampaignPlan("bounded", 20_261_005, (config,), cells)
        snapshot = {
            "disk_free_bytes": run_feature_matrix.MIN_FREE_DISK_BYTES,
            "memory_available_bytes": run_feature_matrix.MIN_AVAILABLE_MEMORY_BYTES,
            "swap_free_bytes": run_feature_matrix.MIN_FREE_SWAP_BYTES,
            "load": [0.0, 0.0, 0.0],
            "cpu_count": 1,
            "target_bytes": 0,
            "timestamp_unix_ns": 0,
        }

        with tempfile.TemporaryDirectory() as directory, mock.patch.multiple(
            run_feature_matrix,
            source_fingerprint=mock.DEFAULT,
            toolchain_fingerprint=mock.DEFAULT,
            generator_fingerprint=mock.DEFAULT,
            resource_snapshot=mock.DEFAULT,
            effective_feature_graph=mock.DEFAULT,
            test_inventory=mock.DEFAULT,
            _run_specs=mock.DEFAULT,
            _cleanup_service_containers=mock.DEFAULT,
        ) as mocks:
            mocks["source_fingerprint"].return_value = "source"
            mocks["toolchain_fingerprint"].return_value = "toolchain"
            mocks["generator_fingerprint"].return_value = "generator"
            mocks["resource_snapshot"].return_value = snapshot
            mocks["effective_feature_graph"].side_effect = (
                subprocess.CalledProcessError(
                    101,
                    ("cargo", "tree"),
                    output=b"partial stdout",
                    stderr=b"compile failed",
                ),
                b"graph",
            )
            mocks["test_inventory"].return_value = b"one: test"

            def succeeds(
                cell: run_feature_matrix.SuiteCell,
                _specs: object,
                *,
                repo_root: pathlib.Path,
                state_dir: pathlib.Path,
                state: feature_matrix.CampaignState,
                identity: feature_matrix.CellIdentity,
                attempt_id: str,
            ) -> tuple[int, pathlib.Path, dict[str, object]]:
                del repo_root, state, identity, attempt_id
                log_path = state_dir / f"{cell.projection_id}.log"
                log_path.write_text("ok", encoding="utf-8")
                return 0, log_path, {"commands": []}

            mocks["_run_specs"].side_effect = succeeds
            report = run_feature_matrix.run_campaign(
                plan,
                repo_root=SCRIPT_DIR.parent.parent,
                state_dir=pathlib.Path(directory),
                suites={"command"},
                dry_run=False,
                jobs=4,
            )
            failed_receipt_path = pathlib.Path(
                report["receipts"]["command/compile-fails"]["receipt"]
            )
            failed_receipt = json.loads(failed_receipt_path.read_text(encoding="utf-8"))

        self.assertEqual(
            report["statuses"],
            {"command/compile-fails": "failed", "command/still-runs": "success"},
        )
        self.assertEqual(failed_receipt["details"]["phase"], "effective-feature-graph")
        self.assertEqual(failed_receipt["exit_code"], 101)

    def test_external_triage_index_links_exact_receipts_without_changing_them(self) -> None:
        failed_identity = "a" * 64
        statuses = {"cell-failed": "failed", "cell-passed": "success"}
        receipts = {
            "cell-failed": {"identity_sha256": failed_identity},
            "cell-passed": {"identity_sha256": "b" * 64},
        }
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "triage.json"
            path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "entries": [
                            {
                                "identity_sha256": failed_identity,
                                "issue_url": "https://github.com/sozu-proxy/sozu/issues/1860",
                                "classification": "same verified cause",
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            index = run_feature_matrix.load_triage_index(path)
            triaged, untriaged = run_feature_matrix.triage_failures(
                statuses, receipts, index
            )

        self.assertEqual(
            triaged,
            {
                "cell-failed": {
                    "issue_url": "https://github.com/sozu-proxy/sozu/issues/1860",
                    "classification": "same verified cause",
                }
            },
        )
        self.assertEqual(untriaged, [])
        self.assertEqual(receipts["cell-failed"]["identity_sha256"], failed_identity)


if __name__ == "__main__":
    unittest.main()
