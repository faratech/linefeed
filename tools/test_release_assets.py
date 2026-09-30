"""Offline checks for the publication gate; all GitHub calls are mocked."""
import hashlib
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / ".github/scripts/release-assets.py"
spec = importlib.util.spec_from_file_location("release_assets", SCRIPT)
release_assets = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_assets)


class PublicationTests(unittest.TestCase):
    def test_checksums_require_every_nonempty_asset(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in release_assets.ASSETS:
                (root / name).write_bytes(name.encode())
            sums = release_assets.checksums(root)
            self.assertEqual(len(sums), 6)
            (root / release_assets.ASSETS[0]).write_bytes(b"")
            with self.assertRaises(RuntimeError):
                release_assets.checksums(root)

    def test_tag_must_match_checked_out_package_version(self):
        with patch.object(Path, "read_text", return_value='[package]\nversion = "1.2.3"\n'):
            release_assets.validate_tag("v1.2.3")
            for tag in ("v1.2.4", "v1.2.3-rc.1", "1.2.3", "../main"):
                with self.assertRaises(RuntimeError):
                    release_assets.validate_tag(tag)

    def test_published_assets_cannot_be_changed(self):
        with patch.object(release_assets, "get_release", return_value={"draft": False}):
            with self.assertRaises(RuntimeError):
                release_assets.require_draft("faratech/linefeed", "v1.2.3")

    def test_publication_fails_before_edit_for_missing_digest_or_wrong_checksum(self):
        for failure in ("digest", "checksums", None):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                for name in release_assets.ASSETS:
                    (root / name).write_bytes(name.encode())
                sums = release_assets.checksums(root)
                manifest = "".join(f"{sums[name]}  {name}\n" for name in release_assets.ASSETS)
                (root / "SHA256SUMS").write_text(manifest, encoding="utf-8")
                assets = [
                    {"name": path.name, "state": "uploaded", "digest": f"sha256:{hashlib.sha256(path.read_bytes()).hexdigest()}"}
                    for path in root.iterdir()
                ]
                if failure == "digest":
                    assets[0]["digest"] = None
                if failure == "checksums":
                    (root / "SHA256SUMS").write_text("outdated", encoding="utf-8")
                release = {"draft": True, "assets": assets}
                with patch.dict(os.environ, RELEASE_TAG="v1.2.3", GITHUB_REPOSITORY="faratech/linefeed"), \
                    patch("sys.argv", [str(SCRIPT), "publish", "--directory", str(root)]), \
                    patch.object(release_assets, "validate_tag"), \
                    patch.object(release_assets, "require_draft", return_value=release), \
                    patch.object(release_assets, "verify_windows"), \
                    patch.object(release_assets, "run") as run:
                    if failure:
                        with self.assertRaises(RuntimeError):
                            release_assets.main()
                        run.assert_not_called()
                    else:
                        release_assets.main()
                        run.assert_called_once_with("gh", "release", "edit", "v1.2.3", "--repo",
                                                    "faratech/linefeed", "--draft=false", "--latest")


if __name__ == "__main__":
    unittest.main()
