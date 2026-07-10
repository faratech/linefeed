#!/usr/bin/env python3
"""Focused regression tests for the repository build helpers."""

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parent.parent


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class BuildScriptTests(unittest.TestCase):
    def test_upx_failure_is_aggregated_and_preserves_diagnostics(self):
        build = load_module("linefeed_build", ROOT / "build.py")
        sizes, failed = build.summarize_upx_results(
            [("linefeed_linux", False, 100, 100, "compression exploded")]
        )
        self.assertTrue(failed)
        self.assertEqual(sizes, {"linefeed_linux": (100, 100)})


class DependencyUpdaterTests(unittest.TestCase):
    def test_failed_lock_update_restores_both_dependency_files(self):
        updater = load_module("linefeed_update_deps", ROOT / "tools" / "update-deps.py")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = root / "Cargo.toml"
            lock = root / "Cargo.lock"
            manifest.write_text('[dependencies]\nserde = "1"\n')
            lock.write_text("original lock\n")
            updater.CARGO_TOML = manifest
            updater.CARGO_LOCK = lock

            failed = subprocess.CompletedProcess(
                ["cargo", "update"], 1, stdout="", stderr="resolver failed"
            )
            with mock.patch.object(updater, "run_cmd", return_value=failed):
                self.assertFalse(updater.update_manifest_and_lock({"serde": "2"}))

            self.assertEqual(manifest.read_text(), '[dependencies]\nserde = "1"\n')
            self.assertEqual(lock.read_text(), "original lock\n")

    def test_successful_lock_update_is_validated_with_locked_metadata(self):
        updater = load_module("linefeed_update_deps_ok", ROOT / "tools" / "update-deps.py")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = root / "Cargo.toml"
            lock = root / "Cargo.lock"
            manifest.write_text('[dependencies]\nserde = "1"\n')
            lock.write_text("original lock\n")
            updater.CARGO_TOML = manifest
            updater.CARGO_LOCK = lock

            def successful_runner(command, capture=True):
                if command[:2] == ["cargo", "update"]:
                    lock.write_text("updated lock\n")
                return subprocess.CompletedProcess(command, 0, stdout="ok", stderr="")

            with mock.patch.object(updater, "run_cmd", side_effect=successful_runner) as run:
                self.assertTrue(updater.update_manifest_and_lock({"serde": "2"}))

            self.assertIn('serde = "2"', manifest.read_text())
            self.assertEqual(lock.read_text(), "updated lock\n")
            self.assertEqual(run.call_args_list[-1].args[0][1:3], ["metadata", "--locked"])


if __name__ == "__main__":
    unittest.main()
