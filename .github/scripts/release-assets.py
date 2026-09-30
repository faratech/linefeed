"""Prepare and validate complete draft releases before exposing them to updaters."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

ASSETS = (
    "linefeed_arm64.exe", "linefeed_x64.exe", "linefeed_x86.exe",
    "linefeed_linux_arm64", "linefeed_linux_x64", "linefeed_macos",
)


def run(*args):
    return subprocess.check_output(list(args), text=True, encoding="utf-8")


def validate_tag(tag):
    version = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag) or tag != f"v{version}":
        raise RuntimeError("Release tag must match Cargo.toml's stable version")


def list_releases(repo):
    # The tag endpoint only resolves published releases. Authenticated listing
    # includes drafts; paginate so older draft tags remain accessible too.
    pages = json.loads(run("gh", "api", "--paginate", "--slurp",
                           f"repos/{repo}/releases?per_page=100"))
    return [release for page in pages for release in page]


def get_release(repo, tag):
    matches = [release for release in list_releases(repo) if release["tag_name"] == tag]
    if len(matches) != 1:
        raise RuntimeError("Expected exactly one release for this tag")
    return matches[0]


def require_draft(repo, tag):
    release = get_release(repo, tag)
    if not release["draft"]:
        raise RuntimeError("Published release assets cannot be replaced; use a new version")
    return release


def checksums(directory):
    result = {}
    for name in ASSETS:
        path = directory / name
        if not path.is_file() or not 0 < path.stat().st_size <= 128 * 1024 * 1024:
            raise RuntimeError(f"Missing or invalid asset: {name}")
        result[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


def verify_windows(directory, tag):
    sdk = Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "Windows Kits/10/bin"
    candidates = list(sdk.glob("*/x64/signtool.exe"))
    if not candidates:
        raise RuntimeError("Windows SDK signtool is required for publication")
    tool = max(candidates, key=lambda p: tuple(map(int, p.parent.parent.name.split("."))))
    for name in ASSETS[:3]:
        output = run(str(tool), "verify", "/pa", "/all", "/v", str(directory / name))
        chains = re.findall(
            r"Signing Certificate Chain:\s*([\s\S]*?)(?:The signature is timestamped|File is not timestamped|Successfully verified)",
            output,
        )
        leaves = []
        for chain in chains:
            names = re.findall(r"Issued to:\s*([^\r\n]+)", chain)
            if names:
                leaves.append(names[-1].strip())
        if leaves != ["Fara Technologies LLC", "Mike Fara"]:
            raise RuntimeError(f"Unexpected signing publishers for {name}: {leaves}")
        # Read resources, without executing release binaries on the runner.
        script = "[Diagnostics.FileVersionInfo]::GetVersionInfo($env:LINEFEED_VERSION_ASSET).ProductVersion"
        version = subprocess.check_output(
            ["pwsh", "-NoProfile", "-Command", script], text=True, encoding="utf-8",
            env={**os.environ, "LINEFEED_VERSION_ASSET": str((directory / name).resolve())},
        ).strip()
        if version != tag[1:]:
            raise RuntimeError(f"Executable version differs from tag: {name}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=["ensure-draft", "guard", "checksums", "publish"])
    parser.add_argument("--directory", type=Path, default=Path("binaries"))
    args = parser.parse_args()
    tag = os.environ["RELEASE_TAG"]
    repo = os.environ["GITHUB_REPOSITORY"]
    validate_tag(tag)
    if args.command == "ensure-draft":
        # Distinguish a missing release from authentication/network failures.
        releases = list_releases(repo)
        if not any(release["tag_name"] == tag for release in releases):
            run("gh", "release", "create", tag, "--repo", repo, "--draft", "--verify-tag",
                "--title", f"Linefeed {tag[1:]}", "--generate-notes")
    release = require_draft(repo, tag)
    if args.command in ("ensure-draft", "guard"):
        return
    values = checksums(args.directory)
    manifest = args.directory / "SHA256SUMS"
    if args.command == "checksums":
        manifest.write_text("".join(f"{values[name]}  {name}\n" for name in ASSETS), encoding="utf-8")
        return
    expected = "".join(f"{values[name]}  {name}\n" for name in ASSETS)
    if manifest.read_text(encoding="utf-8") != expected:
        raise RuntimeError("SHA256SUMS does not cover the exact final assets")
    verify_windows(args.directory, tag)
    values["SHA256SUMS"] = hashlib.sha256(manifest.read_bytes()).hexdigest()
    remote = {asset["name"]: asset for asset in release["assets"]}
    for name, digest in values.items():
        asset = remote.get(name)
        if not asset or asset["state"] != "uploaded" or asset.get("digest") != f"sha256:{digest}":
            raise RuntimeError(f"GitHub asset digest does not match verified bytes: {name}")
    # Re-read under the workflow's tag-specific concurrency group.
    latest = require_draft(repo, tag)
    if latest["assets"] != release["assets"]:
        raise RuntimeError("Release assets changed during validation")
    run("gh", "release", "edit", tag, "--repo", repo, "--draft=false", "--latest")


if __name__ == "__main__":
    main()
