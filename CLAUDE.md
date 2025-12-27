# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

fmIRC is a cross-platform IRC client written in Rust using egui/eframe for the GUI. It supports Windows (ARM64, x86) and Linux with TLS encryption via native-tls (SChannel on Windows, OpenSSL on Linux).

## Build Commands

```bash
# Python build script (recommended)
python3 build.py              # ARM64 only (default)
python3 build.py --all        # All platforms (Linux, ARM64, x86) + UPX compression
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

Output: `./dist/` (build.py) or `./target/<profile>/` (cargo)

## Build Profiles

- `dev`: Fast incremental builds, no optimization
- `release`: Thin LTO, parallel codegen - balance of speed and size
- `dist`: Full LTO, single codegen unit - smallest binaries, slowest build

## Tools

- `build.py`: Async Python build script with parallel compilation
- `tools/update-deps.py`: Updates Cargo.toml dependencies (`--check`, `--pin`, `--dry-run`)
- `tools/gen-icon.py`: Converts `assets/fmirc.png` to `src/icon_data.rs`

## Cross-Compilation

Windows cross-compilation requires llvm-mingw toolchain at `/root/toolchains/llvm-mingw/`. The `.cargo/config.toml` configures linkers and static CRT linking.

## Architecture

```
src/
├── main.rs              # Entry point, FmIrcApp, connection thread, system tray (Windows)
├── icon_data.rs         # Generated: embedded icon as raw RGBA bytes
├── gui/
│   ├── mod.rs           # IrcApp: UI state, message routing, numeric handlers (~2200 lines)
│   ├── types.rs         # Settings, ChatMessage, Channel, UserMode, BanEntry
│   ├── commands.rs      # process_command() - all /slash commands (~1000 lines)
│   ├── dialogs.rs       # Connect, Settings (tabbed), Channel List, Channel Info windows
│   ├── formatting.rs    # IRC color parsing (mIRC codes), nick colors, text rendering
│   ├── helpers.rs       # is_channel(), nick_to_mask(), current_time_formatted(), days_to_ymd()
│   └── logging.rs       # LogManager: persistent chat history in irssi format
└── irc/
    ├── mod.rs           # Re-exports
    ├── client.rs        # IrcClient: async TCP/TLS, SASL PLAIN auth, auto PING/PONG
    ├── message.rs       # IrcCommand enum, IrcMessage parsing, IRCv3 tags
    └── numerics.rs      # IRC numeric constants (RPL_*, ERR_*, IRCv3 Monitor/SASL)
```

**Key types:**
- `FmIrcApp` (main.rs): eframe::App wrapper, owns IrcApp and connection thread
- `IrcApp` (gui/mod.rs): UI state, channel data, settings, message processing
- `IrcClient` (irc/client.rs): Async connection, TLS, SASL negotiation
- `IrcCommand` (irc/message.rs): Enum for all IRC commands, implements Display for wire format
- `IrcMessage` (irc/message.rs): Parsed message with IRCv3 tags, prefix, command
- `Settings` (gui/types.rs): Persistent config at `~/.config/fmirc/settings.json`
- `Channel` (gui/types.rs): Channel state with sorted user list, messages, topic, modes, bans
- `LogManager` (gui/logging.rs): Chat history at `~/.config/fmirc/logs/<network>/<channel>.log`

**Threading model:**
1. Main thread runs egui event loop via `FmIrcApp::update()`
2. Connection thread spawns with single-threaded tokio runtime
3. Two mpsc channels bridge threads: `cmd_tx` (GUI→IRC), `msg_rx` (IRC→GUI)
4. IrcClient auto-handles PING/PONG; all messages forwarded to GUI

## Module Responsibilities

- **main.rs**: App lifecycle, connection management, system tray (Windows via tray-icon/Win32)
- **mod.rs**: `handle_incoming_message()` routes by IrcCommand type, `handle_numeric()` for server replies, lag meter, auto-away
- **commands.rs**: All /slash commands including service shortcuts (/ns, /cs), /monitor, /slap
- **dialogs.rs**: Modal windows with tabbed Settings (Connection, Display, Automation, Behavior, About)
- **formatting.rs**: `parse_irc_colors()` for mIRC codes (\x03), `render_irc_text()` with egui
- **types.rs**: All data structures, Settings load/save, Channel user management with mode sorting
- **logging.rs**: irssi-compatible log format, history loading on join
- **numerics.rs**: Named constants for IRC numerics including IRCv3 (Monitor, SASL, WHOIS extensions)
- **client.rs**: SASL PLAIN authentication via CAP negotiation, TLS with optional invalid cert acceptance

## Features

- **Authentication**: SASL PLAIN, server password, NickServ auto-identify via auto-perform
- **Channel support**: Keys (+k), modes display, topic info, ban list, channel info dialog
- **CTCP**: VERSION, TIME, PING, FINGER, CLIENTINFO (configurable replies)
- **Display**: Configurable timestamps, hide join/part/quit, scrollback limit, highlight words
- **Logging**: Persistent chat history with configurable line count, irssi-compatible format
- **Away**: Manual /away, /back, auto-away on idle with configurable timeout
- **Privacy**: CTCP reply toggle, ignore list with wildcard masks
- **UI**: Tab completion, command history, lag meter, desktop notifications
- **Connection**: Auto-reconnect with exponential backoff, configurable delay/max attempts
- **Platform**: System tray on Windows (minimize to tray, taskbar flash on notification)

**build.rs:** Compiles Windows resources (icon, version info) using llvm-mingw windres.
