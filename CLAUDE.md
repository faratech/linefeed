# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

fmIRC is a cross-platform IRC client written in Rust using egui/eframe for the GUI. It supports Windows (ARM64, x86) and Linux with TLS encryption via native-tls (SChannel on Windows, OpenSSL on Linux).

## Build Commands

```bash
# Full distribution build (all platforms + UPX compression)
./build.sh

# Development builds (fast, ~7s incremental)
cargo build                              # Linux debug
cargo build --release                    # Linux release (thin LTO)

# Distribution builds (slow, smallest binaries)
cargo build --profile dist               # Linux
cargo build --profile dist --target aarch64-pc-windows-gnullvm  # Windows ARM64
cargo build --profile dist --target i686-pc-windows-gnullvm     # Windows x86
```

Output binaries: `./dist/` (from build.sh) or `./target/<profile>/` (from cargo).

## Build Profiles

- `dev`: Fast incremental builds, no optimization
- `release`: Thin LTO, parallel codegen - balance of speed and size
- `dist`: Full LTO, single codegen unit - smallest binaries, slowest build

## Tools

- `tools/update-deps.py`: Automatically updates Cargo.toml dependencies to latest versions
  - `--check`: Show outdated deps without modifying
  - `--pin`: Update and pin exact versions
  - `--dry-run`: Show changes without writing
- `tools/gen-icon.py`: Converts `assets/fmirc.png` to raw RGBA in `src/icon_data.rs`

## Cross-Compilation

Windows cross-compilation requires llvm-mingw toolchain at `/root/toolchains/llvm-mingw/`. The `.cargo/config.toml` configures linkers and static CRT linking.

## Architecture

```
src/
├── main.rs          # Entry point, FmIrcApp wrapper, connection thread spawning
├── icon_data.rs     # Generated: embedded icon as raw RGBA bytes
├── gui/
│   └── mod.rs       # IrcApp: UI state, egui rendering, message handling
└── irc/
    ├── mod.rs       # Re-exports
    ├── client.rs    # IrcClient: async TCP/TLS connection, read/write loops
    └── message.rs   # IrcCommand enum, IrcMessage parsing, IRC protocol
```

**Key types:**
- `FmIrcApp` (main.rs): eframe::App wrapper that owns IrcApp and connection thread
- `IrcApp` (gui): UI state, channel data, processes incoming messages and user input
- `IrcClient` (irc/client): Async connection using tokio, handles TLS via native-tls
- `IrcCommand` (irc/message): Enum for IRC commands, implements Display for wire format
- `IrcMessage` (irc/message): Parsed incoming message with tags, prefix, command
- `Channel` (gui): Channel state with sorted user list (by mode), messages, topic
- `UserMode` (gui): Op/voice/etc modes (~, &, @, %, +) with ordering for user list

**Threading model:**
1. Main thread runs egui event loop via `FmIrcApp::update()`
2. Connection thread spawns with single-threaded tokio runtime
3. Two mpsc channels bridge threads: `cmd_tx` (GUI→IRC), `msg_rx` (IRC→GUI)
4. IrcClient automatically handles PING/PONG; all messages forwarded to GUI

**build.rs:** Compiles Windows resources (icon, version info) using target-specific windres from llvm-mingw toolchain.
