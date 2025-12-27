# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

fmIRC is a cross-platform IRC client written in Rust using egui/eframe for the GUI. It supports Windows (ARM64, x86) and Linux with TLS encryption via native-tls (SChannel on Windows, OpenSSL on Linux).

## Build Commands

```bash
# Python build script (recommended)
python3 build.py              # ARM64 only (default)
python3 build.py --all        # All platforms (Linux, ARM64, x86) + UPX
python3 build.py --linux      # Linux only
python3 build.py --arm64 --x86  # Multiple targets

# Development builds (fast, ~7s incremental)
cargo build                              # Linux debug
cargo build --release                    # Linux release

# Distribution builds (smallest binaries)
cargo build --profile dist               # Linux
cargo build --profile dist --target aarch64-pc-windows-gnullvm  # Windows ARM64
cargo build --profile dist --target i686-pc-windows-gnullvm     # Windows x86
```

## Build Profiles

- `dev`: Fast incremental builds, no optimization
- `release`: Thin LTO, parallel codegen
- `dist`: Full LTO, single codegen unit - smallest binaries

## Tools

- `tools/update-deps.py`: Updates Cargo.toml dependencies (`--check`, `--pin`, `--dry-run`)
- `tools/gen-icon.py`: Converts `assets/fmirc.png` to `src/icon_data.rs`

## Cross-Compilation

Windows cross-compilation requires llvm-mingw toolchain at `/root/toolchains/llvm-mingw/`. The `.cargo/config.toml` configures linkers and static CRT linking.

## Architecture

```
src/
├── main.rs              # Entry point, FmIrcApp wrapper, connection thread spawning
├── icon_data.rs         # Generated: embedded icon as raw RGBA bytes
├── gui/
│   ├── mod.rs           # IrcApp struct, update loop, message routing (1681 lines)
│   ├── types.rs         # ServerFavorite, Settings, ChatMessage, Channel, UserMode
│   ├── commands.rs      # process_command() - all /slash commands (826 lines)
│   ├── dialogs.rs       # Connect, Settings, Channel List windows
│   ├── formatting.rs    # IRC color parsing, URL detection, text rendering
│   └── helpers.rs       # is_channel(), nick_to_mask(), current_time_hhmm()
└── irc/
    ├── mod.rs           # Re-exports
    ├── client.rs        # IrcClient: async TCP/TLS connection with tokio
    ├── message.rs       # IrcCommand enum, IrcMessage parsing
    └── numerics.rs      # IRC numeric constants (RPL_WELCOME, ERR_NICKNAMEINUSE, etc.)
```

**Key types:**
- `FmIrcApp` (main.rs): eframe::App wrapper that owns IrcApp and spawns connection thread
- `IrcApp` (gui/mod.rs): UI state, channel data, processes incoming messages
- `IrcClient` (irc/client.rs): Async connection, TLS via native-tls, auto PING/PONG
- `IrcCommand` (irc/message.rs): Enum for IRC commands, implements Display for wire format
- `IrcMessage` (irc/message.rs): Parsed message with IRCv3 tags, prefix, command
- `Channel` (gui/types.rs): Channel state with sorted user list, messages, topic
- `Settings` (gui/types.rs): Persistent config at `~/.config/fmirc/settings.json`

**Threading model:**
1. Main thread runs egui event loop via `FmIrcApp::update()`
2. Connection thread spawns with single-threaded tokio runtime
3. Two mpsc channels bridge threads: `cmd_tx` (GUI→IRC), `msg_rx` (IRC→GUI)
4. IrcClient auto-handles PING/PONG; all messages forwarded to GUI

## Module Responsibilities

- **mod.rs**: `handle_incoming_message()` routes by message type, `handle_numeric()` for server replies
- **commands.rs**: All /slash commands (/join, /msg, /kick, /whois, etc.), show_help()
- **dialogs.rs**: Modal windows for connect, settings, channel list
- **formatting.rs**: `parse_irc_colors()` for mIRC codes, `render_irc_text()` with egui
- **types.rs**: All data structures, Settings load/save, Channel user management
- **numerics.rs**: Named constants replacing magic numbers (RPL_TOPIC, ERR_NICKNAMEINUSE)

## Features

- CTCP support: VERSION, TIME, PING, CLIENTINFO (auto-replies)
- Ignore list with wildcard mask matching
- Tab completion for nicknames
- Command history (up/down arrows)
- Auto-perform commands after connect
- Auto-reconnect with exponential backoff
- Desktop notifications (notify-send on Linux)
- System tray on Windows

**build.rs:** Compiles Windows resources (icon, version info) using llvm-mingw windres.
