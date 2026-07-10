# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Linefeed is a cross-platform IRC client written in Rust using egui/eframe for the GUI. It supports Windows (ARM64, x64, x86), Linux (ARM64, x64), and macOS (universal) with TLS encryption via native-tls (SChannel on Windows, OpenSSL on Linux, Security.framework on macOS), IRCv3 CAP negotiation, and SASL PLAIN authentication. Licensed CC BY 4.0.

## Build Commands

```bash
# Python build script (recommended)
python3 build.py              # Windows ARM64 only (default)
python3 build.py --all        # Linux (native) + all Windows targets + UPX compression
python3 build.py --linux      # Native Linux only (binary named for host arch)
python3 build.py --arm64 --x64  # Multiple targets
python3 build.py --update-deps # Explicitly update Cargo.toml dependency pins
python3 build.py --no-upx --no-audit  # Skip UPX / cargo-audit steps

# Development builds (fast, ~7s incremental)
cargo build                              # Native debug
cargo build --release                    # Native release

# Distribution builds (smallest binaries)
cargo build --profile dist               # Native
cargo build --profile dist --target aarch64-pc-windows-gnullvm  # Windows ARM64
cargo build --profile dist --target x86_64-pc-windows-gnullvm   # Windows x64
cargo build --profile dist --target i686-pc-windows-gnullvm     # Windows x86
```

Output: `./dist/` when using `build.py` (Linux binaries are arch-suffixed,
e.g. `linefeed_linux_arm64`), `./target/<profile>/` for native Cargo builds,
or `./target/<target>/<profile>/` for Cargo cross-builds.

The build script runs `tools/update-deps.py --check` and a non-fatal
`cargo audit` before building, uses `cargo build --locked --profile dist`,
fails the build if UPX compression fails, and only mutates dependency pins
when `--update-deps` is supplied.

## Verification Commands

```bash
cargo check --locked
cargo test --locked
cargo clippy --all-targets -- -D warnings
python3 tools/update-deps.py --check
python3 -m py_compile build.py tools/update-deps.py
python3 tools/test_build_tools.py
```

## Testing

Tests live in inline `#[cfg(test)]` modules within each source file (no
separate tests/ directory). Run a single test by its full module path:

```bash
cargo test --locked irc::message::tests::tag_values_are_unescaped
cargo test --locked irc::client::tests::   # all client tests
```

`irc::client` tests exercise real connections against scripted fake servers
on loopback (including TLS handshake, registration, and cancellation paths),
so they are the ones to extend for protocol/connection behavior changes.

## Build Profiles

- `dev`: Fast incremental builds, no optimization
- `release`: Thin LTO, parallel codegen - balance of speed and size
- `dist`: Full LTO, single codegen unit - smallest binaries, slowest build

## Releases

Pushing a `v*` tag triggers `.github/workflows/release.yml`, which builds
the binaries that cannot be cross-compiled locally — Linux x86_64
(ubuntu-22.04 for a low glibc floor) and a macOS universal binary (lipo of
aarch64 + x86_64) — and attaches them to the release. Re-run against an
existing release with `gh workflow run release.yml -f tag=<tag>`.
Windows and native-Linux binaries are built locally via `build.py --all`
and uploaded with `gh release upload`, along with a `SHA256SUMS` file
covering all binaries. The macOS binary is unsigned (no Developer ID).

## Tools

- `build.py`: Async Python build script with parallel compilation
- `tools/update-deps.py`: Updates Cargo.toml dependencies (`--check`, `--pin`, `--dry-run`)
- `tools/test_build_tools.py`: Unit tests for build.py/update-deps.py helpers
- `tools/gen-icon.py`: Converts `assets/linefeed.png` to `src/icon_data.rs`

## Cross-Compilation

Windows cross-compilation requires llvm-mingw. The `.cargo/config.toml`
expects `aarch64-w64-mingw32-*`, `x86_64-w64-mingw32-*`, and
`i686-w64-mingw32-*` tools on `PATH`; `build.py` also auto-detects
`/root/toolchains/llvm-mingw` or honors `LLVM_MINGW_HOME`. The config uses
static CRT linking. macOS cannot be cross-compiled from Linux (no Apple
SDK, and mimalloc compiles C) — macOS builds run only in CI.

## Vendored Dependencies

`vendor/wayland-scanner` is a compatibility-preserving copy of the published
crate with only its XML parser advanced past RUSTSEC-2026-0194/0195, wired
in via `[patch.crates-io]` in Cargo.toml. Remove the patch once crates.io
publishes a wayland-scanner using quick-xml >= 0.41. It retains its
upstream MIT license (the project itself is CC BY 4.0).

## Architecture

```
src/
├── main.rs              # Entry point, LinefeedApp (eframe::App), connection thread spawn
├── systray.rs           # Windows system tray via direct Win32 APIs (show/hide, flash, notify, single-instance)
├── icon_data.rs         # Generated: embedded icon as raw RGBA bytes
├── gui/
│   ├── mod.rs           # IrcApp: UI state, message routing, numeric handlers, lifecycle (~5000 lines)
│   ├── types.rs         # Settings, ChatMessage, Channel, NetworkSupport, UserMode, BanEntry, TabCompletion
│   ├── commands.rs      # process_command() - all /slash commands (~2000 lines)
│   ├── dialogs.rs       # Connect, Settings (tabbed), Channel List, Channel Info windows
│   ├── formatting.rs    # IRC formatting parsing, nick colors, text rendering
│   ├── helpers.rs       # is_channel(), nick_to_mask(), current_time_formatted(), days_to_ymd()
│   └── logging.rs       # LogManager: persistent chat history in irssi format
└── irc/
    ├── mod.rs           # Re-exports
    ├── client.rs        # IrcClient: async TCP/TLS, CAP/SASL auth, auto PING/PONG, cancellation
    ├── message.rs       # IrcCommand enum, IrcMessage parsing, IRCv3 tags, wire format
    └── numerics.rs      # IRC numeric constants (RPL_*, ERR_*, IRCv3 Monitor/SASL)
```

**Key types:**
- `LinefeedApp` (main.rs): eframe::App wrapper, owns IrcApp and connection thread
- `IrcApp` (gui/mod.rs): UI state, channel data, settings, message processing; `SessionConfig` snapshots the active connection's endpoint/credentials so dialog edits cannot contaminate a live session
- `IrcClient` (irc/client.rs): Async connection, TLS, SASL negotiation
- `IrcCommand` (irc/message.rs): Enum for all IRC commands, implements Display for wire format
- `IrcMessage` (irc/message.rs): Parsed message with IRCv3 tags, prefix, command
- `Settings` (gui/types.rs): Persistent config at `~/.config/linefeed/settings.json` (Linux) / `%APPDATA%\linefeed` (Windows)
- `Channel` (gui/types.rs): Channel state with sorted user list, messages, topic, modes, bans
- `NetworkSupport` (gui/types.rs): Server-advertised channel prefixes, user mode prefixes, and CASEMAPPING from ISUPPORT
- `LogManager` (gui/logging.rs): Chat history at `~/.config/linefeed/logs/<network>/<target>.log`, endpoint-scoped paths, credential filtering at write time

**Threading model:**
1. Main thread runs egui event loop via `LinefeedApp::update()`
2. Connection thread spawns with single-threaded tokio runtime
3. Two mpsc channels bridge threads: `cmd_tx` (GUI→IRC), `msg_rx` (IRC→GUI)
4. IrcClient auto-handles PING/PONG; all messages forwarded to GUI

## Module Responsibilities

- **main.rs**: App lifecycle, connection management, background tick while tray-hidden/minimized
- **systray.rs**: Windows-only tray icon, minimize-to-tray, taskbar flash, balloon notifications, single-instance activation
- **gui/mod.rs**: `handle_incoming_message()` routes by IrcCommand type, `handle_numeric()` for server replies, connection lifecycle, lag meter, auto-away
- **commands.rs**: All /slash commands including service shortcuts (/ns, /cs), /monitor, /slap
- **dialogs.rs**: Modal windows with tabbed Settings (Connection, Display, Automation, Behavior, About)
- **formatting.rs**: `parse_irc_colors()` for IRC formatting controls, `render_irc_text()` with egui
- **types.rs**: All data structures, Settings load/save, Channel user management with server-aware mode sorting and casemapping-aware keys
- **logging.rs**: irssi-compatible log format, history loading on join, log rotation detection (same-file)
- **numerics.rs**: Named constants for IRC numerics including IRCv3 (Monitor, SASL, WHOIS extensions)
- **client.rs**: CAP negotiation (with flood budgets), SASL PLAIN, TLS with optional invalid cert acceptance, overall registration deadline, prompt cancellation of connect/handshake/reads

## Security-Sensitive Areas

The 40-issue audit (all fixed, see closed GitHub issues) concentrated on:
credentials must never reach chat logs or input history (logging.rs and
commands.rs filter them), switching servers must not leak PASS/SASL/
auto-perform secrets to the new endpoint, and CAP negotiation enforces
byte/token/line budgets against hostile servers. Preserve these invariants
when touching connection, logging, or command-history code.

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
