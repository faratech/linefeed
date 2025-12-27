#!/usr/bin/env python3
"""
Automatic Cargo dependency updater.
Checks for outdated dependencies and updates Cargo.toml to latest versions.
"""

import subprocess
import re
import sys
from pathlib import Path

# Project root is parent of tools/
PROJECT_ROOT = Path(__file__).resolve().parent.parent
CARGO_TOML = PROJECT_ROOT / "Cargo.toml"


def run_cmd(cmd: list[str], capture: bool = True) -> subprocess.CompletedProcess:
    """Run a command and return the result."""
    return subprocess.run(cmd, capture_output=capture, text=True, cwd=PROJECT_ROOT)


def get_outdated_deps() -> dict[str, dict]:
    """Get list of outdated dependencies using cargo search."""
    if not CARGO_TOML.exists():
        print(f"Error: Cargo.toml not found at {CARGO_TOML}")
        sys.exit(1)

    content = CARGO_TOML.read_text()

    # Parse dependencies from Cargo.toml
    # Includes [dependencies] and [target.'cfg(...)'.dependencies] sections
    deps = {}
    in_deps = False

    for line in content.splitlines():
        # Match [dependencies] or [target.'cfg(...)'.dependencies]
        if line.strip() == "[dependencies]" or "dependencies]" in line:
            in_deps = True
            continue
        elif line.startswith("[") and in_deps:
            # Check if this is another dependencies section or a different section
            if "dependencies]" not in line:
                in_deps = False
            continue
        elif in_deps and "=" in line and not line.strip().startswith("#"):
            # Parse dependency line
            match = re.match(r'^(\S+)\s*=\s*(?:"([^"]+)"|{[^}]*version\s*=\s*"([^"]+)")', line)
            if match:
                name = match.group(1)
                version = match.group(2) or match.group(3)
                deps[name] = {"current": version, "line": line}

    return deps


def get_latest_version(crate_name: str) -> str | None:
    """Get latest version of a crate from crates.io."""
    result = run_cmd(["cargo", "search", crate_name, "--limit", "1"])
    if result.returncode != 0:
        return None

    # Parse output: 'crate_name = "version"    # description'
    for line in result.stdout.splitlines():
        match = re.match(rf'^{re.escape(crate_name)}\s*=\s*"([^"]+)"', line)
        if match:
            return match.group(1)
    return None


def parse_version(v: str) -> tuple[int, ...]:
    """Parse version string to tuple for comparison."""
    # Handle versions like "1", "1.0", "1.0.0"
    parts = v.split(".")
    return tuple(int(p) for p in parts if p.isdigit())


def is_compatible(latest: str, current: str) -> bool:
    """
    Check if current version spec is compatible with latest.
    Cargo semver: "1" matches 1.x.x, "1.2" matches 1.2.x, etc.
    """
    try:
        latest_parts = parse_version(latest)
        current_parts = parse_version(current)

        # Compare only the parts specified in current
        # e.g., "1" matches "1.48.0", "0.33" matches "0.33.3"
        for i, cur in enumerate(current_parts):
            if i >= len(latest_parts):
                return False
            if latest_parts[i] != cur:
                return False
        return True
    except ValueError:
        return False


def is_newer(latest: str, current: str) -> bool:
    """Check if latest version has a newer major/minor than current specifies."""
    try:
        latest_parts = parse_version(latest)
        current_parts = parse_version(current)

        # If current is compatible with latest, not outdated
        if is_compatible(latest, current):
            return False

        # Check if any specified part in current is older than latest
        for i, cur in enumerate(current_parts):
            if i >= len(latest_parts):
                return True  # current specifies more parts than latest has
            if latest_parts[i] > cur:
                return True
            if latest_parts[i] < cur:
                return False
        return False
    except ValueError:
        return False


def update_cargo_toml(updates: dict[str, str], dry_run: bool = False) -> None:
    """Update Cargo.toml with new versions."""
    content = CARGO_TOML.read_text()

    for crate_name, new_version in updates.items():
        # Match both simple and complex dependency formats
        # Use word boundary (^|[\s\[]) to avoid matching 'rustls' in 'tokio-rustls'
        # Simple: crate = "1.0"
        pattern1 = rf'(^|\n)({re.escape(crate_name)}\s*=\s*)"[^"]+"'
        replacement1 = rf'\1\2"{new_version}"'

        # Complex: crate = { version = "1.0", ... }
        pattern2 = rf'(^|\n)({re.escape(crate_name)}\s*=\s*\{{[^}}]*version\s*=\s*)"[^"]+"'
        replacement2 = rf'\1\2"{new_version}"'

        content = re.sub(pattern1, replacement1, content)
        content = re.sub(pattern2, replacement2, content)

    if dry_run:
        print("\n[Dry run] Would update Cargo.toml with:")
        print(content)
    else:
        CARGO_TOML.write_text(content)
        print(f"\nUpdated Cargo.toml with {len(updates)} changes")


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Update Cargo dependencies to latest versions")
    parser.add_argument("--dry-run", "-n", action="store_true", help="Show what would be updated without making changes")
    parser.add_argument("--check", "-c", action="store_true", help="Only check for outdated deps, don't update")
    parser.add_argument("--pin", "-p", action="store_true", help="Pin all deps to exact latest versions (e.g., 0.33 -> 0.33.3)")
    args = parser.parse_args()

    print("Checking for outdated dependencies...\n")

    deps = get_outdated_deps()
    if not deps:
        print("No dependencies found in Cargo.toml")
        return

    updates = {}
    outdated = []

    for name, info in deps.items():
        current = info["current"]
        print(f"Checking {name}...", end=" ", flush=True)

        latest = get_latest_version(name)
        if latest is None:
            print("not found on crates.io")
            continue

        if is_newer(latest, current):
            print(f"{current} -> {latest} (outdated)")
            outdated.append((name, current, latest))
            updates[name] = latest
        elif args.pin and current != latest:
            print(f"{current} -> {latest} (pinning)")
            outdated.append((name, current, latest))
            updates[name] = latest
        elif is_compatible(latest, current):
            print(f"{current} (ok, resolves to {latest})")
        else:
            print(f"{current} (up to date)")

    print(f"\n{'='*50}")

    if not outdated:
        print("All dependencies are up to date!")
        return

    action = "to pin" if args.pin else "outdated"
    print(f"Found {len(outdated)} dependencies {action}:\n")
    for name, current, latest in outdated:
        print(f"  {name}: {current} -> {latest}")

    if args.check:
        print("\n[Check mode] No changes made.")
        return

    if args.dry_run:
        update_cargo_toml(updates, dry_run=True)
    else:
        update_cargo_toml(updates)
        print("\nRun 'cargo build' to fetch new versions.")
        print("Note: Major version updates may require code changes!")


if __name__ == "__main__":
    main()
