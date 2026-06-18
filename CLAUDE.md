# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Linefeed is a cross-platform IRC client written in Rust using egui/eframe for the GUI. It supports Windows (ARM64, x86) and Linux with TLS encryption via native-tls (SChannel on Windows, OpenSSL on Linux), IRCv3 CAP negotiation, and SASL PLAIN authentication.

## Build Commands

```bash
# Python build script (recommended)
python3 build.py              # ARM64 only (default)
python3 build.py --all        # All platforms (Linux, ARM64, x86) + UPX compression
python3 build.py --linux      # Linux only
python3 build.py --arm64 --x86  # Multiple targets
python3 build.py --update-deps # Explicitly update Cargo.toml dependency pins

# Development builds (fast, ~7s incremental)
cargo build                              # Linux debug
cargo build --release                    # Linux release

# Distribution builds (smallest binaries)
cargo build --profile dist               # Linux
cargo build --profile dist --target aarch64-pc-windows-gnullvm  # Windows ARM64
cargo build --profile dist --target i686-pc-windows-gnullvm     # Windows x86
```

Output: `./dist/` when using `build.py`, `./target/<profile>/` for native
Cargo builds, or `./target/<target>/<profile>/` for Cargo cross-builds.

The build script runs `tools/update-deps.py --check` before building, uses
`cargo build --locked --profile dist`, and only mutates dependency pins when
`--update-deps` is supplied.

## Verification Commands

```bash
cargo check --locked
cargo test --locked
cargo clippy --all-targets -- -D warnings
python3 tools/update-deps.py --check
python3 -m py_compile build.py tools/update-deps.py
```

## Build Profiles

- `dev`: Fast incremental builds, no optimization
- `release`: Thin LTO, parallel codegen - balance of speed and size
- `dist`: Full LTO, single codegen unit - smallest binaries, slowest build

## Tools

- `build.py`: Async Python build script with parallel compilation
- `tools/update-deps.py`: Updates Cargo.toml dependencies (`--check`, `--pin`, `--dry-run`)
- `tools/gen-icon.py`: Converts `assets/linefeed.png` to `src/icon_data.rs`

## Cross-Compilation

Windows cross-compilation requires llvm-mingw. The `.cargo/config.toml`
expects `aarch64-w64-mingw32-*` and `i686-w64-mingw32-*` tools on `PATH`; set
`LLVM_MINGW_HOME` if the tools are not already discoverable. The config uses
static CRT linking.

## Architecture

```
src/
├── main.rs              # Entry point, FmIrcApp, connection thread, system tray (Windows)
├── icon_data.rs         # Generated: embedded icon as raw RGBA bytes
├── gui/
│   ├── mod.rs           # IrcApp: UI state, message routing, numeric handlers, lifecycle
│   ├── types.rs         # Settings, ChatMessage, Channel, UserMode, BanEntry
│   ├── commands.rs      # process_command() - all /slash commands (~1000 lines)
│   ├── dialogs.rs       # Connect, Settings (tabbed), Channel List, Channel Info windows
│   ├── formatting.rs    # IRC formatting parsing, nick colors, text rendering
│   ├── helpers.rs       # is_channel(), nick_to_mask(), current_time_formatted(), days_to_ymd()
│   └── logging.rs       # LogManager: persistent chat history in irssi format
└── irc/
    ├── mod.rs           # Re-exports
    ├── client.rs        # IrcClient: async TCP/TLS, CAP/SASL auth, auto PING/PONG
    ├── message.rs       # IrcCommand enum, IrcMessage parsing, IRCv3 tags, wire format
    └── numerics.rs      # IRC numeric constants (RPL_*, ERR_*, IRCv3 Monitor/SASL)
```

**Key types:**
- `FmIrcApp` (main.rs): eframe::App wrapper, owns IrcApp and connection thread
- `IrcApp` (gui/mod.rs): UI state, channel data, settings, message processing
- `IrcClient` (irc/client.rs): Async connection, TLS, SASL negotiation
- `IrcCommand` (irc/message.rs): Enum for all IRC commands, implements Display for wire format
- `IrcMessage` (irc/message.rs): Parsed message with IRCv3 tags, prefix, command
- `Settings` (gui/types.rs): Persistent config at `~/.config/linefeed/settings.json`
- `Channel` (gui/types.rs): Channel state with sorted user list, messages, topic, modes, bans
- `NetworkSupport` (gui/types.rs): Server-advertised channel prefixes and user mode prefixes from ISUPPORT
- `LogManager` (gui/logging.rs): Chat history at `~/.config/linefeed/logs/<network>/<channel>.log` on Linux and `%APPDATA%\linefeed\logs\<network>\<channel>.log` on Windows

**Threading model:**
1. Main thread runs egui event loop via `FmIrcApp::update()`
2. Connection thread spawns with single-threaded tokio runtime
3. Two mpsc channels bridge threads: `cmd_tx` (GUI→IRC), `msg_rx` (IRC→GUI)
4. IrcClient auto-handles PING/PONG; all messages forwarded to GUI

## Module Responsibilities

- **main.rs**: App lifecycle, connection management, system tray (Windows via tray-icon/Win32)
- **mod.rs**: `handle_incoming_message()` routes by IrcCommand type, `handle_numeric()` for server replies, connection lifecycle, lag meter, auto-away
- **commands.rs**: All /slash commands including service shortcuts (/ns, /cs), /monitor, /slap
- **dialogs.rs**: Modal windows with tabbed Settings (Connection, Display, Automation, Behavior, About)
- **formatting.rs**: `parse_irc_colors()` for IRC formatting controls, `render_irc_text()` with egui
- **types.rs**: All data structures, Settings load/save, Channel user management with server-aware mode sorting
- **logging.rs**: irssi-compatible log format, history loading on join
- **numerics.rs**: Named constants for IRC numerics including IRCv3 (Monitor, SASL, WHOIS extensions)
- **client.rs**: CAP negotiation, SASL PLAIN authentication, TLS with optional invalid cert acceptance, handshake timeout fallback

## Features

- **Authentication**: SASL PLAIN with graceful fallback, server password, NickServ auto-identify via auto-perform
- **Channel support**: Server-advertised channel prefixes, keys (+k), modes display, topic info, ban list, channel info dialog
- **CTCP**: VERSION, TIME, PING, FINGER, CLIENTINFO (configurable replies)
- **Display**: Configurable timestamps, IRC formatting including reverse video, hide join/part/quit, scrollback limit, highlight words
- **Logging**: Persistent chat history with configurable line count, irssi-compatible format
- **Away**: Manual /away, /back, auto-away on idle with configurable timeout
- **Privacy**: CTCP reply toggle, ignore list with wildcard masks
- **UI**: Tab completion, command history, lag meter, desktop notifications
- **Connection**: Safe reconnect/server switching, auto-reconnect with exponential backoff, configurable delay/max attempts
- **Platform**: System tray on Windows (minimize to tray, taskbar flash on notification), desktop notifications on supported platforms

**build.rs:** Compiles Windows resources (icon, version info) using llvm-mingw windres.
