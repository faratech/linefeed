# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

fmIRC is a cross-platform IRC client written in Rust using egui/eframe for the GUI. It supports Windows (ARM64, x86) and Linux with TLS encryption.

## Build Commands

```bash
# Build all platforms (Linux, Windows ARM64, Windows x86)
./build.sh

# Build single platform
cargo build --release                                    # Linux native
cargo build --release --target aarch64-pc-windows-gnullvm  # Windows ARM64
cargo build --release --target i686-pc-windows-gnullvm     # Windows x86

# Run (Linux with display)
./target/release/fmirc
```

Output binaries go to `./dist/`.

## Cross-Compilation Requirements

Windows cross-compilation requires llvm-mingw toolchain at `/root/toolchains/llvm-mingw/`. The `.cargo/config.toml` configures linkers and static CRT linking for standalone executables.

## Architecture

```
src/
├── main.rs      # Application entry, eframe setup, icon loading, connection management
├── gui/
│   └── mod.rs   # IrcApp struct, egui UI (panels, dialogs, message display, user list)
└── irc/
    ├── mod.rs      # Module exports
    ├── client.rs   # IrcClient: TCP/TLS connection, read/write loops, PING handling
    └── message.rs  # IrcCommand enum, IrcMessage parsing, IRC protocol formatting
```

**Key types:**
- `IrcApp` (gui): Main application state, channels, UI rendering
- `IrcClient` (irc/client): Async connection handler using tokio
- `IrcCommand` (irc/message): Enum for all IRC commands with Display impl for wire format
- `IrcMessage` (irc/message): Parsed incoming message with prefix, command, tags
- `Channel` (gui): Channel state with users (nick + mode prefix), messages, topic
- `UserMode` (gui): Op/voice/etc modes with prefix characters (@, +, %, etc.)

**Communication flow:**
1. `FmIrcApp` spawns connection thread with tokio runtime
2. `IrcClient::connect()` establishes TCP/TLS connection
3. Two mpsc channels: `cmd_tx` (GUI→client for outgoing), `msg_rx` (client→GUI for incoming)
4. Client handles PING/PONG automatically; all other messages sent to GUI

## Windows Resource Embedding

`build.rs` manually invokes windres to embed the application icon for Windows Explorer display. Icon source: `assets/fmirc.ico`.
