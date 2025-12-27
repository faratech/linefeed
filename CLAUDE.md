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
python3 build.py --no-upx     # Skip compression
python3 build.py --skip-deps  # Skip dependency check

# Development builds (fast, ~7s incremental)
cargo build                              # Linux debug
cargo build --release                    # Linux release (thin LTO)

# Distribution builds (slow, smallest binaries)
cargo build --profile dist               # Linux
cargo build --profile dist --target aarch64-pc-windows-gnullvm  # Windows ARM64
cargo build --profile dist --target i686-pc-windows-gnullvm     # Windows x86
```

Output binaries: `./dist/` (from build.py) or `./target/<profile>/` (from cargo).

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

Windows cross-compilation requires llvm-mingw toolchain at `/root/toolchains/llvm-mingw/`. The `.cargo/config.toml` configures linkers and static CRT linking. UPX compression is skipped for ARM64 (unsupported).

## Architecture

```
src/
├── main.rs          # Entry point, FmIrcApp wrapper, connection thread, tray (Windows)
├── icon_data.rs     # Generated: embedded icon as raw RGBA bytes
├── gui/
│   └── mod.rs       # IrcApp: UI state, egui rendering, message/command handling (~3700 lines)
└── irc/
    ├── mod.rs       # Re-exports
    ├── client.rs    # IrcClient: async TCP/TLS connection, read/write loops
    └── message.rs   # IrcCommand enum (50+ variants), IrcMessage parsing
```

**Key types:**
- `FmIrcApp` (main.rs): eframe::App wrapper that owns IrcApp and connection thread
- `IrcApp` (gui): UI state (~50 fields), channel data, message/command processing
- `IrcClient` (irc/client): Async connection using tokio, handles TLS via native-tls
- `IrcCommand` (irc/message): Enum for 50+ IRC commands, implements Display for wire format
- `IrcMessage` (irc/message): Parsed message with IRCv3 tags, prefix, command
- `Channel` (gui): Channel state with sorted user list (by mode), messages, topic
- `UserMode` (gui): Op/voice/etc modes (~, &, @, %, +) with ordering for user list
- `Settings` (gui): Persistent config, serialized to `~/.config/fmirc/settings.json`
- `ServerFavorite` (gui): Saved server profiles for quick connect

**Threading model:**
1. Main thread runs egui event loop via `FmIrcApp::update()` (~100ms repaint interval)
2. Connection thread spawns with single-threaded tokio runtime
3. Two mpsc channels bridge threads: `cmd_tx` (GUI→IRC, 100 capacity), `msg_rx` (IRC→GUI, 1000 capacity)
4. IrcClient auto-handles PING/PONG; all messages forwarded to GUI

## Key Implementation Details

**gui/mod.rs structure:**
- `handle_incoming_message()`: Routes IRC messages by type (PRIVMSG, NOTICE, JOIN, PART, numerics)
- `handle_numeric()`: Processes 100+ IRC numeric replies (001 welcome, 353 names, 332 topic, etc.)
- `process_command()`: Implements 100+ slash commands (/join, /msg, /whois, /kick, etc.)
- `parse_irc_colors()`: Parses mIRC color codes (0x03) and formatting (bold, underline, italic)
- `render_irc_text()`: Renders formatted spans with egui RichText

**Features implemented:**
- CTCP support: VERSION, TIME, PING, CLIENTINFO, SOURCE, USERINFO (auto-replies)
- Ignore list with wildcard mask matching (`*!*@*.domain.com`)
- Tab completion for nicknames
- Command history (100 entries, up/down arrows)
- Auto-perform commands (queued after RPL_WELCOME)
- Auto-reconnect with exponential backoff (1s → 60s max)
- Desktop notifications: `notify-send` on Linux, taskbar flash on Windows
- System tray (Windows only): minimize to tray, show/quit menu

**build.rs:** Compiles Windows resources (icon, version info) using target-specific windres from llvm-mingw toolchain.
