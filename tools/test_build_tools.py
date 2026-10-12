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


class DependencyVersionAndParsingTests(unittest.TestCase):
    def setUp(self):
        self.updater = load_module(
            "linefeed_update_deps_versions", ROOT / "tools" / "update-deps.py"
        )

    def test_parse_version_prerelease_and_operators(self):
        u = self.updater
        self.assertEqual(u.parse_version("1.2.3-rc.1"), (1, 2, 3))
        self.assertEqual(u.parse_version("2.0.0-beta.3"), (2, 0, 0))
        self.assertEqual(u.parse_version(">=1.2"), (1, 2))
        self.assertEqual(u.parse_version("1.2"), (1, 2))

    def test_is_newer_semver_prerelease_ordering(self):
        u = self.updater
        self.assertFalse(u.is_newer("2.0.0-beta.3", "2.0.1"))
        self.assertFalse(u.is_newer("1.2", "1.2.3"))
        self.assertFalse(u.is_newer("1.2.3-rc.1", "1.2.3"))
        self.assertTrue(u.is_newer("1.2.3", "1.2.3-rc.1"))
        self.assertTrue(u.is_newer("2.0.0", "1.9.9"))

    def test_toml_multiline_indented_and_quoted_deps(self):
        u = self.updater
        with tempfile.TemporaryDirectory() as tmp:
            manifest = Path(tmp) / "Cargo.toml"
            manifest.write_text(
                "[dependencies]\n"
                "plain = \"1\"\n"
                "  indented = \"2\"\n"
                '"quoted-dep" = "3"\n'
                "multi = {\n  version = \"4\",\n  features = [\"x\"]\n}\n"
                "[target.'cfg(unix)'.dependencies]\n"
                'targetdep = "5"\n'
            )
            u.CARGO_TOML = manifest
            deps = u.get_outdated_deps()
            self.assertEqual(deps["plain"]["current"], "1")
            self.assertEqual(deps["indented"]["current"], "2")
            self.assertEqual(deps["quoted-dep"]["current"], "3")
            self.assertEqual(deps["multi"]["current"], "4")
            self.assertEqual(deps["targetdep"]["current"], "5")

    def test_search_failure_is_fatal_not_not_found(self):
        u = self.updater
        failed = subprocess.CompletedProcess(
            ["cargo", "search"], 1, stdout="", stderr="offline"
        )
        with mock.patch.object(u, "run_cmd", return_value=failed):
            with self.assertRaises(Exception):
                u.get_latest_version("serde")

    def test_check_mode_exits_nonzero_on_outdated(self):
        u = self.updater
        with mock.patch("sys.argv", ["update-deps.py", "--check"]), mock.patch.object(
            u, "get_outdated_deps", return_value={"serde": {"current": "1", "line": ""}}
        ), mock.patch.object(u, "get_latest_version", return_value="2"):
            with self.assertRaises(SystemExit) as ctx:
                u.main()
            self.assertNotEqual(ctx.exception.code, 0)

    def test_check_mode_exits_nonzero_on_search_failure(self):
        u = self.updater
        with mock.patch("sys.argv", ["update-deps.py", "--check"]), mock.patch.object(
            u, "get_outdated_deps", return_value={"serde": {"current": "1", "line": ""}}
        ), mock.patch.object(
            u, "get_latest_version", side_effect=RuntimeError("cargo search failed")
        ):
            with self.assertRaises(SystemExit) as ctx:
                u.main()
            self.assertNotEqual(ctx.exception.code, 0)

    def test_scoped_cargo_update_and_pin_precise(self):
        u = self.updater
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest, lock = root / "Cargo.toml", root / "Cargo.lock"
            manifest.write_text('[dependencies]\nserde = "1"\n')
            lock.write_text("orig\n")
            u.CARGO_TOML, u.CARGO_LOCK = manifest, lock
            commands = []

            def runner(command, capture=True):
                commands.append(list(command))
                return subprocess.CompletedProcess(command, 0, stdout="ok", stderr="")

            with mock.patch.object(u, "run_cmd", side_effect=runner):
                self.assertTrue(u.update_manifest_and_lock({"serde": "2"}))
            update_cmds = [c for c in commands if c[:2] == ["cargo", "update"]]
            self.assertTrue(update_cmds)
            self.assertIn("-p", update_cmds[0])
            self.assertIn("serde", update_cmds[0])
            self.assertNotEqual(update_cmds[0], ["cargo", "update"])

            commands.clear()
            with mock.patch.object(u, "run_cmd", side_effect=runner):
                self.assertTrue(
                    u.update_manifest_and_lock({"serde": "2"}, pin=True)
                )
            pin_cmds = [c for c in commands if c[:2] == ["cargo", "update"]]
            self.assertTrue(
                any("--precise" in c and "2" in c and "-p" in c for c in pin_cmds)
            )


class GenIconTests(unittest.TestCase):
    def _png(self, width, height, color_type, bit_depth, rows: bytes):
        import struct as _struct
        import zlib as _zlib

        def chunk(typ, data):
            return (
                _struct.pack(">I", len(data))
                + typ
                + data
                + _struct.pack(">I", _zlib.crc32(typ + data) & 0xFFFFFFFF)
            )

        ihdr = _struct.pack(">IIBBBBB", width, height, bit_depth, color_type, 0, 0, 0)
        return (
            b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", _zlib.compress(rows))
            + chunk(b"IEND", b"")
        )

    def test_rejects_palette_and_grayscale(self):
        gen = load_module("linefeed_gen_icon", ROOT / "tools" / "gen-icon.py")
        # 4x2 palette (color type 3) and grayscale (0), filter byte per row
        palette = self._png(4, 2, 3, 8, b"\x00\x00\x01\x02\x03" * 2)
        gray = self._png(4, 2, 0, 8, b"\x00\x00\x01\x02\x03" * 2)
        for fixture in (palette, gray):
            with self.assertRaises(ValueError):
                gen.decode_png(fixture)

    def test_rejects_unsupported_bit_depth_and_rgb_ok(self):
        gen = load_module("linefeed_gen_icon2", ROOT / "tools" / "gen-icon.py")
        bad = self._png(1, 1, 2, 1, b"\x00\x00")
        with self.assertRaises(ValueError):
            gen.decode_png(bad)
        good = self._png(1, 1, 2, 8, b"\x00\xff\x00\x00")
        w, h, rgba = gen.decode_png(good)
        self.assertEqual((w, h, len(rgba)), (1, 1, 4))


if __name__ == "__main__":
    unittest.main()
