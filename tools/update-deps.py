#!/usr/bin/env python3
"""
Automatic Cargo dependency updater.
Checks for outdated dependencies and updates Cargo.toml to latest versions.
"""

import subprocess
import re
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    tomllib = None  # type: ignore[assignment]

# Project root is parent of tools/
PROJECT_ROOT = Path(__file__).resolve().parent.parent
CARGO_TOML = PROJECT_ROOT / "Cargo.toml"
CARGO_LOCK = PROJECT_ROOT / "Cargo.lock"


def run_cmd(cmd: list[str], capture: bool = True) -> subprocess.CompletedProcess:
    """Run a command and return the result."""
    return subprocess.run(cmd, capture_output=capture, text=True, cwd=PROJECT_ROOT)


class SearchError(RuntimeError):
    """Raised when `cargo search` itself fails (distinct from not-found)."""


def _collect_deps(deps: dict[str, dict], table: object, section: str) -> None:
    if not isinstance(table, dict):
        return
    for name, spec in table.items():
        version = None
        if isinstance(spec, str):
            version = spec
        elif isinstance(spec, dict):
            raw = spec.get("version")
            if isinstance(raw, str):
                version = raw
        if version is None:
            print(
                f"Warning: skipping unparsed dependency '{name}' in "
                f"[{section}] (no version spec found)",
                file=sys.stderr,
            )
            continue
        if name not in deps:
            deps[name] = {"current": version, "line": f'{name} = "{version}"'}


def _fallback_parse_deps(content: str) -> dict[str, dict]:
    """Line parser used when tomllib rejects Cargo extensions (e.g.
    multi-line inline tables). Loudly warns on unparsed entries."""
    deps: dict[str, dict] = {}
    section = ""
    subdep: str | None = None
    lines = content.splitlines()
    i = 0
    dep_sections = ("dependencies", "dev-dependencies", "build-dependencies")
    while i < len(lines):
        raw = lines[i]
        i += 1
        stripped = raw.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped.strip("[]").strip()
            subdep = None
            # [dependencies.<name>] or [target.'cfg'.dependencies.<name>]
            m = re.search(
                r"(?:^|\.)(?:dependencies|dev-dependencies|build-dependencies)"
                r"\.(?:\"([^\"]+)\"|'([^']+)'|([A-Za-z0-9_-]+))$",
                section,
            )
            if m:
                subdep = m.group(1) or m.group(2) or m.group(3)
            continue
        is_dep_section = any(
            section == s or section.endswith("." + s) for s in dep_sections
        ) or subdep is not None
        if not is_dep_section or not stripped or stripped.startswith("#"):
            continue
        if subdep is not None:
            m = re.match(r"""\s*version\s*=\s*"([^"]+)\"""", raw)
            if m and subdep not in deps:
                deps[subdep] = {"current": m.group(1), "line": raw}
            continue
        # Accumulate multi-line inline table / array values.
        entry = raw
        while (entry.count("{") > entry.count("}")
               or entry.count("[") > entry.count("]")) and i < len(lines):
            entry += "\n" + lines[i]
            i += 1
        m = re.match(
            r"""\s*(?:"([^"]+)"|'([^']+)'|([A-Za-z0-9_-]+))\s*=\s*(?:"([^"]+)"|\{(.*)\})\s*(?:#.*)?$""",
            entry,
            re.DOTALL,
        )
        if not m:
            print(
                f"Warning: skipping unparsed dependency in [{section}]: "
                f"{stripped!r}",
                file=sys.stderr,
            )
            continue
        name = m.group(1) or m.group(2) or m.group(3)
        version = m.group(4)
        if version is None and m.group(5) is not None:
            vm = re.search(r"""version\s*=\s*"([^"]+)\"""", m.group(5))
            version = vm.group(1) if vm else None
        if version is None:
            print(
                f"Warning: skipping unparsed dependency '{name}' in "
                f"[{section}] (no version spec found)",
                file=sys.stderr,
            )
            continue
        if name not in deps:
            deps[name] = {"current": version, "line": entry.strip().splitlines()[0]}
    return deps


def get_outdated_deps() -> dict[str, dict]:
    """Get list of outdated dependencies using cargo search."""
    if not CARGO_TOML.exists():
        print(f"Error: Cargo.toml not found at {CARGO_TOML}")
        sys.exit(1)

    content = CARGO_TOML.read_text()
    if tomllib is None:
        print(
            "Warning: tomllib is unavailable (Python 3.11+ required); "
            "falling back to line parser for Cargo.toml",
            file=sys.stderr,
        )
        return _fallback_parse_deps(content)

    try:
        data = tomllib.loads(content)
    except Exception as exc:
        print(
            f"Warning: failed to parse Cargo.toml with tomllib ({exc}); "
            "falling back to line parser",
            file=sys.stderr,
        )
        return _fallback_parse_deps(content)

    deps: dict[str, dict] = {}
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        _collect_deps(deps, data.get(section), section)
    workspace = data.get("workspace")
    if isinstance(workspace, dict):
        _collect_deps(deps, workspace.get("dependencies"), "workspace.dependencies")
    target = data.get("target")
    if isinstance(target, dict):
        for cfg, table in target.items():
            if isinstance(table, dict):
                for section in ("dependencies", "dev-dependencies", "build-dependencies"):
                    _collect_deps(deps, table.get(section), f"target.{cfg}.{section}")
    return deps


def get_latest_version(crate_name: str) -> str | None:
    """Get latest version of a crate from crates.io.

    Returns None only when the search succeeds but finds no match.
    Raises SearchError when `cargo search` itself fails.
    """
    result = run_cmd(["cargo", "search", crate_name, "--limit", "1"])
    if result.returncode != 0:
        detail = ((result.stderr or "") + (result.stdout or "")).strip()
        raise SearchError(
            f"cargo search failed for '{crate_name}'"
            + (f": {detail}" if detail else "")
        )

    # Parse output: 'crate_name = "version"    # description'
    for line in result.stdout.splitlines():
        match = re.match(rf'^{re.escape(crate_name)}\s*=\s*"([^"]+)"', line)
        if match:
            return match.group(1)
    return None


_VERSION_RE = re.compile(
    r"(\d+(?:\.\d+)*)"
    r"(?:-([0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*))?"
    r"(?:\+[0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*)?"
)


def _parse_semver(v: str):
    """Return (specified_core, padded_core, prerelease_ids) or raise ValueError."""
    if not isinstance(v, str):
        raise ValueError(f"invalid version: {v!r}")
    match = _VERSION_RE.search(v.strip())
    if not match:
        raise ValueError(f"invalid version: {v!r}")
    # Reject strings with no digit at all / pure operators handled by regex.
    core = tuple(int(p) for p in match.group(1).split("."))
    padded = (core + (0, 0, 0))[:3]
    prerelease: tuple[str, ...] = ()
    if match.group(2):
        prerelease = tuple(match.group(2).split("."))
    return core, padded, prerelease


def _prerelease_key(prerelease: tuple[str, ...]):
    key = []
    for ident in prerelease:
        if ident.isdigit():
            key.append((0, int(ident), ""))
        else:
            key.append((1, 0, ident))
    return tuple(key)


def _semver_key(v: str):
    _core, padded, prerelease = _parse_semver(v)
    # Release (no prerelease) sorts after any prerelease of same core.
    is_release = 1 if not prerelease else 0
    return (padded, is_release, _prerelease_key(prerelease))


def _compare_semver(a: str, b: str) -> int:
    ka, kb = _semver_key(a), _semver_key(b)
    # Pad prerelease comparison: shorter prerelease < longer if prefix equal
    # is handled by tuple compare only when is_release equal; embed length
    # via Python tuple semantics on the key's third element.
    if ka < kb:
        return -1
    if ka > kb:
        return 1
    return 0


def parse_version(v: str) -> tuple[int, ...]:
    """Parse version string to numeric core tuple for comparison.

    Operators (>=, ^, ~, =), prerelease and build metadata are not
    folded into the numeric core: ``parse_version("1.2.3-rc.1") == (1, 2, 3)``
    and ``parse_version(">=1.2") == (1, 2)``.
    """
    core, _padded, _pre = _parse_semver(v)
    return core


def is_compatible(latest: str, current: str) -> bool:
    """
    Check if current version spec is compatible with latest.
    Cargo semver: "1" matches 1.x.x, "1.2" matches 1.2.x, etc.
    """
    try:
        latest_core, latest_padded, latest_pre = _parse_semver(latest)
        current_core, _current_padded, current_pre = _parse_semver(current)
        if latest_pre or current_pre:
            # Prerelease involved: only exact semver equality is compatible.
            return _compare_semver(latest, current) == 0
        latest_full = (latest_core + (0, 0, 0))[: max(len(latest_core), len(current_core))]
        for i, cur in enumerate(current_core):
            if i >= len(latest_full):
                return False
            if latest_full[i] != cur:
                return False
        return True
    except ValueError:
        return False


def is_newer(latest: str, current: str) -> bool:
    """Check if latest is strictly newer than current (semver ordering)."""
    try:
        if is_compatible(latest, current):
            return False
        return _compare_semver(latest, current) > 0
    except ValueError:
        return False


def is_prerelease(version: str) -> bool:
    return "-" in version


def update_cargo_toml(updates: dict[str, str], dry_run: bool = False) -> None:
    """Update Cargo.toml with new versions."""
    content = CARGO_TOML.read_text()

    for crate_name, new_version in updates.items():
        # Match both simple and complex dependency formats
        # Use word boundary (^|[\s\[]) to avoid matching 'rustls' in 'tokio-rustls'
        # Simple: crate = "1.0" (also indented / quoted-key forms)
        pattern1 = rf'(^|\n)([ \t]*["\']?{re.escape(crate_name)}["\']?\s*=\s*)"[^"]+"'
        replacement1 = rf'\1\2"{new_version}"'

        # Complex: crate = { version = "1.0", ... } (may span lines)
        pattern2 = rf'(^|\n)([ \t]*["\']?{re.escape(crate_name)}["\']?\s*=\s*\{{[^}}]*?version\s*=\s*)"[^"]+"'
        replacement2 = rf'\1\2"{new_version}"'

        content = re.sub(pattern1, replacement1, content)
        content = re.sub(pattern2, replacement2, content)

    if dry_run:
        print("\n[Dry run] Would update Cargo.toml with:")
        print(content)
    else:
        CARGO_TOML.write_text(content)
        print(f"\nUpdated Cargo.toml with {len(updates)} changes")


def update_manifest_and_lock(updates: dict[str, str], pin: bool = False) -> bool:
    """Update Cargo.toml and Cargo.lock as one rollback-safe operation."""
    original_toml = CARGO_TOML.read_bytes()
    original_lock = CARGO_LOCK.read_bytes() if CARGO_LOCK.exists() else None

    try:
        update_cargo_toml(updates)
        if pin:
            # --precise applies to a single -p package, so update per crate.
            for crate_name, new_version in updates.items():
                result = run_cmd(
                    ["cargo", "update", "-p", crate_name, "--precise", new_version]
                )
                if result.returncode != 0:
                    detail = (result.stdout or "") + (result.stderr or "")
                    raise RuntimeError(f"cargo update failed:\n{detail.rstrip()}")
        elif updates:
            cmd = ["cargo", "update"]
            for crate_name in updates:
                cmd += ["-p", crate_name]
            result = run_cmd(cmd)
            if result.returncode != 0:
                detail = (result.stdout or "") + (result.stderr or "")
                raise RuntimeError(f"cargo update failed:\n{detail.rstrip()}")
        else:
            result = run_cmd(["cargo", "update"])
            if result.returncode != 0:
                detail = (result.stdout or "") + (result.stderr or "")
                raise RuntimeError(f"cargo update failed:\n{detail.rstrip()}")

        result = run_cmd(
            ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"]
        )
        if result.returncode != 0:
            detail = (result.stdout or "") + (result.stderr or "")
            raise RuntimeError(f"locked dependency validation failed:\n{detail.rstrip()}")
    except Exception as exc:
        CARGO_TOML.write_bytes(original_toml)
        if original_lock is None:
            CARGO_LOCK.unlink(missing_ok=True)
        else:
            CARGO_LOCK.write_bytes(original_lock)
        print(f"\nDependency update rolled back: {exc}", file=sys.stderr)
        return False

    print("Cargo.toml and Cargo.lock updated and validated together")
    return True


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Update Cargo dependencies to latest versions")
    parser.add_argument("--dry-run", "-n", action="store_true", help="Show what would be updated without making changes")
    parser.add_argument("--check", "-c", action="store_true", help="Only check for outdated deps, don't update")
    parser.add_argument("--pin", "-p", action="store_true", help="Pin all deps to exact latest versions (e.g., 0.33 -> 0.33.3)")
    parser.add_argument("--allow-prerelease", action="store_true", help="Allow prerelease versions from cargo search")
    args = parser.parse_args()

    print("Checking for outdated dependencies...\n")

    deps = get_outdated_deps()
    if not deps:
        print("No dependencies found in Cargo.toml")
        return

    updates = {}
    outdated = []

    search_failed = False
    for name, info in deps.items():
        current = info["current"]
        print(f"Checking {name}...", end=" ", flush=True)

        try:
            latest = get_latest_version(name)
        except Exception as exc:
            print(f"error: {exc}", file=sys.stderr)
            print("search failed (see stderr)")
            search_failed = True
            continue
        if latest is None:
            print("not found on crates.io")
            continue
        if is_prerelease(latest) and not args.allow_prerelease:
            print(f"{current} (latest {latest} is prerelease; skipped)")
            continue

        if is_newer(latest, current):
            print(f"{current} -> {latest} (outdated)")
            outdated.append((name, current, latest))
            updates[name] = latest
        elif args.pin and current != latest and not is_newer(current, latest):
            print(f"{current} -> {latest} (pinning)")
            outdated.append((name, current, latest))
            updates[name] = latest
        elif is_compatible(latest, current):
            print(f"{current} (ok, resolves to {latest})")
        else:
            print(f"{current} (up to date)")

    print(f"\n{'='*50}")

    if search_failed:
        print("Error: cargo search failed; dependency check incomplete.", file=sys.stderr)
        sys.exit(1)

    if not outdated:
        print("All dependencies are up to date!")
        return

    action = "to pin" if args.pin else "outdated"
    print(f"Found {len(outdated)} dependencies {action}:\n")
    for name, current, latest in outdated:
        print(f"  {name}: {current} -> {latest}")

    if args.check:
        print("\n[Check mode] No changes made.")
        sys.exit(1)

    if args.dry_run:
        update_cargo_toml(updates, dry_run=True)
    else:
        if not update_manifest_and_lock(updates, pin=args.pin):
            sys.exit(1)
        print("\nDependency files are ready for a --locked build.")
        print("Note: Major version updates may require code changes!")


if __name__ == "__main__":
    main()
