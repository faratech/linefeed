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

`.github/workflows/audit.yml` runs `cargo audit --deny yanked` on pushes to
main, pull requests, and weekly; unlike the build script, it fails on any
advisory or yanked crate.

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

Pushing a `v*` tag triggers `.github/workflows/release.yml`, which creates a draft and builds
the binaries that cannot be cross-compiled locally — Linux x86_64 and ARM64
(ubuntu-22.04 runners, for a low glibc and OpenSSL 3 floor) and a macOS
universal binary (lipo of aarch64 + x86_64) — and attaches them to the release. Re-run against an
existing release with `gh workflow run release.yml -f tag=<tag>`.
Windows binaries are built locally via `build.py --arm64 --x64 --x86`
and uploaded to the draft with `gh release upload`. After all six assets exist,
run `sign-release.yml` to sign Windows files and generate final `SHA256SUMS`,
then `publish-release.yml` to verify and publish. Published assets cannot be
replaced by these workflows. See `docs/auto-updates.md`.
The macOS binary is unsigned (no Developer ID).

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
├── main.rs              # Entry point, LinefeedApp (eframe::App): tray/minimized paths, repaint policy, exit
├── systray.rs           # Windows system tray via direct Win32 APIs (show/hide, flash, notify, single-instance)
├── icon_data.rs         # Generated: embedded icon as raw RGBA bytes
├── opener.rs            # Opening hyperlinks in the browser
├── updater/             # Automatic updates: check, download, install on exit, recovery
├── gui/
│   ├── app.rs           # App: sessions, settings, notifications, toolbar/switchbar, windows, detached viewports (~2600 lines)
│   ├── mod.rs           # Session: one connection's state, message routing, numeric handlers (~12700 lines)
│   ├── connection.rs    # Per-session connection worker lifecycle (thread, intent, reconnect)
│   ├── windows.rs       # WindowManager: placement, tile/cascade, layout.json
│   ├── view.rs          # Drawing one window: topic, user list, scrollback, input line
│   ├── types.rs         # Settings, ChatMessage, Channel, NetworkSupport, UserMode, BanEntry, TabCompletion
│   ├── commands.rs      # process_command() - all /slash commands (~5100 lines)
│   ├── dialogs.rs       # Connect (per session), Settings (App), Channel List, Channel Info windows
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
- `LinefeedApp` (main.rs): eframe::App wrapper around `App`, tray/minimized handling
- `App` (gui/app.rs): owns every `Session` plus everything global: canonical `Settings` (only a validated connect form is ever adopted into them), favorites/network profiles, updater, desktop-notification policy, the `WindowManager`; draws the toolbar, switchbar, windows and detached OS windows. Sessions talk to it through `SessionEvent`s
- `Session` (gui/mod.rs): one connection (its status window, channels, queries), its own connect form, mirrored behaviour settings (`apply_config`/`export_config`), its worker thread; `SessionConfig` snapshots the live connection's endpoint/credentials so form edits cannot contaminate it
- `WindowManager` (gui/windows.rs): which window is active, maximized vs floating, per-window placement and input line (`TargetView`, swapped into the session while the window draws)
- `IrcClient` (irc/client.rs): Async connection, TLS, SASL negotiation
- `IrcCommand` (irc/message.rs): Enum for all IRC commands, implements Display for wire format
- `IrcMessage` (irc/message.rs): Parsed message with IRCv3 tags, prefix, command
- `Settings` (gui/types.rs): Persistent config at `~/.config/linefeed/settings.json` (Linux) / `%APPDATA%\linefeed` (Windows)
- `Channel` (gui/types.rs): Channel state with sorted user list, messages, topic, modes, bans
- `NetworkSupport` (gui/types.rs): Server-advertised channel prefixes, user mode prefixes, and CASEMAPPING from ISUPPORT
- `LogManager` (gui/logging.rs): Chat history at `~/.config/linefeed/logs/<network>/<target>.log`, endpoint-scoped paths, credential filtering at write time

**Threading model:**
1. Main thread runs egui event loop via `LinefeedApp::ui()`; detached windows are immediate viewports on the same thread
2. Each `Session` spawns its own connection thread with a single-threaded tokio runtime
3. Two mpsc channels per session bridge threads: `cmd_tx` (GUI→IRC), `msg_rx` (IRC→GUI)
4. IrcClient auto-handles PING/PONG; all messages forwarded to GUI

## Module Responsibilities

- **main.rs**: eframe entry, tray/minimized paths (detached windows keep running while the main window is minimized and hide with it in the tray), repaint policy, exit
- **systray.rs**: Windows-only tray icon, minimize-to-tray, taskbar flash, balloon notifications, single-instance activation (finds the main window by its exact title, which no detached window may use)
- **gui/app.rs**: multi-session frame (pump, timers, lifecycles, `SessionEvent`s), window activation and close semantics, switchbar/shortcuts, floating and detached windows, settings persistence, notifications, update-restart snapshot of every session
- **gui/mod.rs**: `handle_incoming_message()` routes by IrcCommand type, `handle_numeric()` for server replies, lag meter, auto-away
- **gui/connection.rs**: per-session worker thread, `ConnectionIntent` handling, auto-reconnect
- **gui/windows.rs**: window set derived from the sessions, placement memory, tile/cascade geometry, layout.json (kept apart from settings.json)
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
when touching connection, logging, or command-history code. With several
sessions: a new session (`/server -m`, "New connection") never inherits
another's credentials, and settings saves never persist an unvalidated or
scratch connect form (`SaveSettings` vs `SaveConfig`/`SaveAutoPerform`,
#316).

## Features

- **Authentication**: SASL PLAIN with graceful fallback, server password, NickServ auto-identify via auto-perform
- **Channel support**: Server-advertised channel prefixes, keys (+k), modes display, topic info, ban list, channel info dialog
- **CTCP**: VERSION, TIME, PING, FINGER, CLIENTINFO (configurable replies)
- **Display**: Configurable timestamps, IRC formatting including reverse video, hide join/part/quit, scrollback limit, highlight words
- **Logging**: Persistent chat history with configurable line count, irssi-compatible format
- **Away**: Manual /away, /back, auto-away on idle with configurable timeout
- **Privacy**: CTCP reply toggle, ignore list with wildcard masks
- **UI**: Tab completion, command history, lag meter, desktop notifications
- **Connection**: Several servers at once, safe reconnect/server switching, auto-reconnect with exponential backoff, configurable delay/max attempts, auto-connect network profiles
- **Windows**: mIRC-style maximized or floating (tile/cascade) windows, switchbar grouped by network, detach any window into its own OS window, layout persisted in layout.json
- **Platform**: System tray on Windows (minimize to tray, taskbar flash on notification), desktop notifications on supported platforms

**build.rs:** Compiles Windows resources (icon, version info) using llvm-mingw windres.
