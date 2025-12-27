#!/usr/bin/env python3
"""fmIRC Build Script - Async Python version"""

import argparse
import asyncio
import os
import shutil
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
OUT_DIR = SCRIPT_DIR / "dist"

# All build targets: (flag_name, display_name, cargo_target, src_binary, dst_binary)
ALL_TARGETS = {
    "linux": ("Linux (native)", None, "fmirc", "fmirc_linux"),
    "arm64": ("Windows ARM64", "aarch64-pc-windows-gnullvm", "fmirc.exe", "fmirc_arm64.exe"),
    "x86": ("Windows x86", "i686-pc-windows-gnullvm", "fmirc.exe", "fmirc_x86.exe"),
}

# Default targets when no flags specified
DEFAULT_TARGETS = ["arm64"]

# UPX-compatible targets
UPX_SUPPORTED = {
    "fmirc_linux": True,
    "fmirc_x86.exe": True,
    "fmirc_arm64.exe": False,  # UPX doesn't support win64/arm64
}


async def run_cmd(cmd: list[str], desc: str = "") -> tuple[bool, str]:
    """Run a command asynchronously and return (success, output)"""
    if desc:
        print(f"  {desc}...")

    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
        cwd=SCRIPT_DIR,
    )
    stdout, _ = await proc.communicate()
    output = stdout.decode() if stdout else ""
    return proc.returncode == 0, output


async def check_deps():
    """Check and update dependencies"""
    print("[0/2] Checking dependencies...")
    success, output = await run_cmd(
        ["python3", "tools/update-deps.py", "--pin"],
    )
    print(output)
    if not success:
        print("ERROR: Dependency check failed")
        sys.exit(1)


async def build_target(name: str, target: str | None, src_name: str, dst_name: str) -> bool:
    """Build a single target"""
    print(f"  Building {name}...")

    cmd = ["cargo", "build", "--profile", "dist"]
    if target:
        cmd.extend(["--target", target])

    success, output = await run_cmd(cmd)

    if not success:
        print(f"ERROR building {name}:")
        print(output)
        return False

    # Copy to output directory
    if target:
        src = SCRIPT_DIR / "target" / target / "dist" / src_name
    else:
        src = SCRIPT_DIR / "target" / "dist" / src_name

    dst = OUT_DIR / dst_name
    shutil.copy2(src, dst)
    print(f"    -> {dst.relative_to(SCRIPT_DIR)}")
    return True


async def compress_with_upx(filename: str) -> tuple[str, bool, int, int]:
    """Compress a binary with UPX. Returns (filename, success, before_size, after_size)"""
    filepath = OUT_DIR / filename
    before_size = filepath.stat().st_size

    if not UPX_SUPPORTED.get(filename, False):
        return (filename, True, before_size, before_size)  # No change

    if not shutil.which("upx"):
        return (filename, True, before_size, before_size)  # No change

    success, output = await run_cmd(["upx", "--best", "-q", str(filepath)])
    after_size = filepath.stat().st_size if success else before_size

    return (filename, success, before_size, after_size)


def parse_args():
    parser = argparse.ArgumentParser(
        description="Build fmIRC for various platforms",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  python3 build.py              # Build ARM64 only (default)
  python3 build.py --all        # Build all platforms
  python3 build.py --arm64 --x86  # Build ARM64 and x86
  python3 build.py --linux      # Build Linux only
  python3 build.py --no-upx     # Skip UPX compression
  python3 build.py --jobs 8     # Use 8 parallel jobs
""",
    )

    # Target selection
    parser.add_argument("--all", action="store_true", help="Build all platforms")
    parser.add_argument("--linux", action="store_true", help="Build Linux (native)")
    parser.add_argument("--arm64", action="store_true", help="Build Windows ARM64")
    parser.add_argument("--x86", action="store_true", help="Build Windows x86")

    # Build options
    parser.add_argument("-j", "--jobs", type=int, default=0,
                        help="Number of parallel jobs (default: auto-detect CPU count)")
    parser.add_argument("--no-sccache", action="store_true",
                        help="Disable sccache even if available")
    parser.add_argument("--no-upx", action="store_true",
                        help="Skip UPX compression")
    parser.add_argument("--skip-deps", action="store_true",
                        help="Skip dependency check")

    return parser.parse_args()


async def main():
    args = parse_args()

    print("=== fmIRC Build Script ===")

    # Determine which targets to build
    if args.all:
        targets = list(ALL_TARGETS.keys())
    else:
        targets = []
        if args.linux:
            targets.append("linux")
        if args.arm64:
            targets.append("arm64")
        if args.x86:
            targets.append("x86")

        # Default to ARM64 if no targets specified
        if not targets:
            targets = DEFAULT_TARGETS

    print(f"Targets: {', '.join(targets)}")

    # Set up parallel jobs
    jobs = args.jobs if args.jobs > 0 else (os.cpu_count() or 4)
    os.environ["CARGO_BUILD_JOBS"] = str(jobs)
    print(f"Parallel jobs: {jobs}")

    # Set up sccache if available and not disabled
    if not args.no_sccache and shutil.which("sccache"):
        os.environ["RUSTC_WRAPPER"] = "sccache"
        print("Compiler cache: sccache (enabled)")
    else:
        print("Compiler cache: disabled")

    # Check UPX availability
    upx_available = shutil.which("upx") is not None
    if not args.no_upx and upx_available:
        print("UPX compression: enabled")
    elif args.no_upx:
        print("UPX compression: disabled (--no-upx)")
    else:
        print("UPX compression: unavailable (upx not found)")

    print()

    # Check dependencies
    if not args.skip_deps:
        await check_deps()
        print()

    # Create output directory
    OUT_DIR.mkdir(exist_ok=True)

    # Build all targets (sequential due to Cargo lock)
    print(f"[1/2] Building {len(targets)} target(s)...")
    built_files = []
    for target_key in targets:
        name, cargo_target, src_name, dst_name = ALL_TARGETS[target_key]
        if await build_target(name, cargo_target, src_name, dst_name):
            built_files.append(dst_name)
        else:
            print(f"ERROR: Failed to build {name}")
            sys.exit(1)
    print()

    # Compress with UPX (runs in parallel!)
    file_sizes = {}  # filename -> (before, after)
    if not args.no_upx and upx_available:
        print("[2/2] Compressing with UPX...")
        compress_tasks = [compress_with_upx(f) for f in built_files]
        results = await asyncio.gather(*compress_tasks)
        for filename, success, before, after in results:
            file_sizes[filename] = (before, after)
            if before != after:
                ratio = (after / before) * 100
                print(f"    {filename}: {before/1024/1024:.1f} MB -> {after/1024/1024:.1f} MB ({ratio:.0f}%)")
            elif not UPX_SUPPORTED.get(filename, False):
                print(f"    {filename}: skipped (UPX unsupported)")
        print()
    else:
        print("[2/2] Skipping UPX compression")
        for f in built_files:
            size = (OUT_DIR / f).stat().st_size
            file_sizes[f] = (size, size)
        print()

    # Show results
    print("=== Build Complete ===")
    total_size = 0
    for filename in sorted(built_files):
        before, after = file_sizes.get(filename, (0, 0))
        total_size += after
        print(f"  {filename}: {after/1024/1024:.1f} MB")
    print(f"  Total: {total_size/1024/1024:.1f} MB")


if __name__ == "__main__":
    asyncio.run(main())
