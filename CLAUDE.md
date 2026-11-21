# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Linefeed is a cross-platform IRC client written in Rust using egui/eframe for the GUI. It supports Windows (ARM64, x64, x86), Linux (ARM64, x64), and macOS (universal) with TLS encryption via native-tls (SChannel on Windows, OpenSSL on Linux, Security.framework on macOS), IRCv3 CAP negotiation, and SASL authentication. Licensed CC BY 4.0.

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

## Architecture

```
src/
├── main.rs              # Entry point, LinefeedApp (eframe::App): tray/minimized paths, repaint policy, exit
├── systray.rs           # Windows system tray via direct Win32 APIs (show/hide, flash, notify, single-instance)
├── icon_data.rs         # Generated: embedded icon as raw RGBA bytes
├── opener.rs            # Opening hyperlinks in the browser
├── persistence.rs       # Private atomic configuration writes and cross-process locks
├── updater/             # Automatic updates: check, download, install on exit, recovery
├── gui/
│   ├── app.rs           # App: sessions, settings, notifications, top bar, conversation sidebar, windows, detached viewports
│   ├── mod.rs           # Session: protocol state, message routing, numeric handlers, conversation selection
│   ├── connection.rs    # Private connection phases, worker/transport ownership, cancellation and reconnect
│   ├── auth_history.rs  # Defer automatic history until configured services login is confirmed
│   ├── conversations.rs # Stable process-local ConversationIds, target-name indexes and collision merges
│   ├── windows.rs       # WindowManager: placement, tile/cascade, layout.json
│   ├── snap.rs          # Snap layouts: window captions, layout flyout/bar, edge snaps, Snap Assist
│   ├── theme.rs         # Palette (dark/light), Metrics, DisplayPrefs; apply() sets egui visuals, spacing and text sizes
│   ├── fonts.rs         # System font loading: proportional, semibold, monospace, symbol and emoji faces
│   ├── widgets.rs       # Shared pieces: list_row, badge, badge_button, truncated, section_label, dot, strong
│   ├── view.rs          # Drawing one window: header row, grouped people list, scrollback, composer
│   ├── types.rs         # Settings, ChatMessage, Channel, NetworkSupport, UserMode, BanEntry, TabCompletion
│   ├── commands.rs      # process_command() - all /slash commands (~5100 lines)
│   ├── dialogs.rs       # Unified Settings/Connection editor, Channel List and Channel Info windows
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
- `App` (gui/app.rs): owns every `Session` plus everything global: canonical `Settings` (only a validated connect form is ever adopted into them), favorites/network profiles, updater, desktop-notification policy, the `WindowManager`; draws the top bar, conversation sidebar, windows and detached OS windows. Sessions talk to it through `SessionEvent`s
- `Session` (gui/mod.rs): one network's status, channels, queries, connect form and mirrored behaviour settings (`apply_config`/`export_config`); owns a private `ConnectionLifecycle` and `ConversationStore`. `SessionConfig` snapshots the active endpoint/credentials so form edits cannot contaminate it
- `ConnectionLifecycle` (gui/connection.rs): private `Idle`, `Queued`, `Connecting`, `Registered`, `Closing` and `RetryWait` phases. Live phases own their worker and channels; `Closing` owns the retiring worker and any replacement plan. Callers use derived queries such as `is_connected()`, `is_connecting()` and `has_worker()`, never independent writable flags or transport fields
- `RecoveryPolicy` (gui/connection.rs): `ConfiguredAutoJoin` for an initial connection, or `ExactMemberships` for recovery; an exact empty list deliberately restores no channels. Successfully queued leave/join intents determine membership recovery
- `ConversationStore` / `ConversationId` (gui/conversations.rs): channels and queries indexed by IDs that are never reused in a session. Nick/channel names are mutable metadata with exact and negotiated-casemapping lookup indexes; renames preserve identity and collisions merge into an explicit survivor
- `WindowManager` (gui/windows.rs): active/maximized/floating windows, placement and each input line (`TargetView`, swapped into the session while the window draws). Runtime `WindowKey`s contain `(ConnId, Option<ConversationId>)`; `None` identifies the status window
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
3. Each live connection phase privately owns two mpsc channels (GUI→IRC commands and IRC→GUI messages) and an attempt generation. Dispatch checks the generation and whether the current phase accepts messages
4. QUIT retirement keeps the receiver alive for a bounded flush period; a replacement worker starts only after the old worker exits. Final messages retain the old endpoint identity, while update snapshots project the requested replacement
5. IrcClient auto-handles PING/PONG; all messages forwarded to GUI

Conversation IDs key runtime selections, window state, deferred UI work and
native viewports. Resolve an ID to its current target name when sending IRC
commands. Layout and updater snapshots retain compatible name-based formats;
restoration allocates fresh runtime IDs rather than persisting process-local
identity. Closing and reopening the same name must not revive stale UI work.

## Module Responsibilities

- **main.rs**: eframe entry, tray/minimized paths (detached windows keep running while the main window is minimized and hide with it in the tray), repaint policy, exit
- **systray.rs**: Windows-only tray icon, minimize-to-tray, taskbar flash, balloon notifications, single-instance activation (finds the main window by its exact title, which no detached window may use)
- **gui/app.rs**: multi-session frame (pump, timers, lifecycles, `SessionEvent`s), window activation and close semantics, sidebar/shortcuts, floating and detached windows, settings persistence, notifications, update-restart snapshot of every session
- **gui/mod.rs**: `handle_incoming_message()` routes by IrcCommand type, `handle_numeric()` for server replies, lag meter, auto-away
- **gui/auth_history.rs**: Holds automatic history by conversation ID while a queued services login is pending; confirms via account signals or a correlated self-WHOIS after a services reply. Free-form NOTICE text and attempt timeouts never unlock history. Manual WHOIS exchanges remain separate from internal probes.
- **gui/connection.rs**: explicit connection transitions, exclusive worker/transport ownership, attempt generations, bounded cancellation/QUIT retirement, auto-reconnect and derived lifecycle queries
- **gui/conversations.rs**: conversation identity, current wire/display names, casemapped lookup, rename/collision resolution and revision tracking
- **gui/windows.rs**: window set derived from conversation IDs, placement memory, tile/cascade geometry, layout.json name-based compatibility boundary (kept apart from settings.json)
- **gui/snap.rs**: the floating windows' own captions (drag to move, maximize, close) and Windows 11 style snap layouts: flyout on the maximize button, layout bar when dragged to the top, halves/quarters at the edges, Snap Assist for the zones left
- **commands.rs**: All /slash commands including service shortcuts (/ns, /cs), /monitor, /slap
- **dialogs.rs**: One Settings window (Connection, Display, Behavior, About). Startup, Connect and New connection open the complete Connection tab for a pinned session, including favorites, SASL/client certificates and on-connect automation. Save & Close validates and persists without changing a live connection; Connect/Reconnect explicitly stages validated details after checking endpoint reservations. Cancel restores that session's opening form/profile and discards a canceled pristine new connection. Global preferences remain live; explicit favorite updates preserve profile identity and never adopt scratch Quick Connect defaults.
- **formatting.rs**: `parse_irc_colors()` for IRC formatting controls, `render_segments()` with egui (IRC colours are nudged to read on the row's ground), `with_mentions()`
- **theme.rs**, **fonts.rs**, **widgets.rs**: every colour and size comes from `theme::palette(ctx)` / `theme::metrics(ctx)`; control height lives in egui's `spacing.interact_size.y` (`theme::control_height(ui)`). Drawing code never writes a `Color32` literal: only the mIRC colour tables do. This build has no built-in egui fonts, so `fonts::install` loads system faces; the `semibold` family falls back to the proportional face. `gui::tests` paint the main window and Settings in both themes and check every text against the palette's grounds
- **types.rs**: All data structures, Settings load/save, Channel user management with server-aware mode sorting and casemapping-aware keys
- **logging.rs**: irssi-compatible text logs and exact physical spans for archive deduplication; history loading and rotation detection
- **gui/archive.rs**, **search.rs**: authoritative structured JSONL history, rebuildable SQLite FTS index, background import/search/context, scoped permanent redaction
- **gui/identity.rs**, **workspace.rs**: persistent logical network/message identity and private background workspace snapshots; never persist runtime connection/conversation IDs
- **gui/message_ui.rs**, **attention.rs**: validated atomic chat sends, reply/reaction/typing state, read anchors, per-conversation notification rules and activity references
- **gui/automation.rs**, **actions.rs**: bounded parameterized command plans, private alias configuration, configurable actions and window switcher
- **gui/bouncer.rs**, **irc/bouncer.rs**: soju discovery/bind and ZNC upstream routing/playback; logical upstream identity is separate from transport endpoint
- **gui/uploads.rs**: HTTPS FILEHOST discovery/custom providers, scoped token flow, background picker/encoding/streaming/cancellation; completion appends only to the originating draft
- **numerics.rs**: Named constants for IRC numerics including IRCv3 (Monitor, SASL, WHOIS extensions)
- **client.rs**: CAP negotiation (with flood budgets), SASL Auto/PLAIN/EXTERNAL/SCRAM-SHA-256, TLS with optional invalid cert acceptance, overall registration deadline, prompt cancellation of connect/handshake/reads

## Security-Sensitive Areas

The 40-issue audit (all fixed, see closed GitHub issues) concentrated on:
credentials must never reach chat logs or input history (logging.rs and
commands.rs filter them), switching servers must not leak PASS/SASL/
auto-perform secrets to the new endpoint, and CAP negotiation enforces
byte/token/line budgets against hostile servers. Preserve these invariants
when touching connection, logging, or command-history code. With several
sessions: a new session (`/server -m`, "New connection") never inherits
another's credentials, and settings saves never persist an unvalidated or
scratch connect form (only explicit validated Settings save/connect adopts the
form; `SaveConfig` and `SaveAutoPerform` keep their narrower scopes, #316).

## Features

- **Authentication**: SASL Auto/PLAIN/EXTERNAL/SCRAM-SHA-256 with optional required authentication, TLS client certificates, server password, and services login via auto-perform
- **Channel support**: Server-advertised channel prefixes, keys (+k), modes display, topic info, ban list, channel info dialog
- **CTCP**: VERSION, TIME, PING, FINGER, CLIENTINFO (configurable replies)
- **Display**: Dark, light or system theme; comfortable or compact spacing; proportional or monospace message font; text size; timestamps in three formats or hidden; nickname colours; IRC formatting including reverse video, hide join/part/quit, scrollback limit, highlight words; live preview in Settings
- **Logging**: Search all saved history by network/conversation/sender/date, historical context, compatible text logs plus structured archive and rebuildable index
- **Away**: Manual /away, /back, auto-away on idle with configurable timeout
- **Privacy**: CTCP reply toggle, ignore list with wildcard masks
- **UI**: Top bar (connection button with its menu, status line, search field, Activity, Window, Settings), conversation sidebar that becomes a switcher button below 720 px, one-row window header, message rows in time/nick/text columns (stacked below 520 px) with grouped senders and a filled mention row, "New messages" marker, people list grouped by role, multiline composer, quoted replies, grouped reactions, optional typing, account/TLS badges, activity center, per-conversation notification rules, unread navigation, aliases, configurable shortcuts and window switcher
- **Workspace**: Normal restarts restore committed sessions, tabs, drafts, selection and reading anchors; reconnect only sessions previously connected
- **Sharing**: Attach, clipboard images/files and drop via advertised/custom HTTPS FILEHOST; progress/cancel and URL insertion, never automatic sending
- **Connection**: Several servers/upstreams at once, soju/ZNC profiles and playback, safe reconnect/server switching, auto-reconnect with exponential backoff, configurable delay/max attempts, auto-connect network profiles
- **Windows**: mIRC-style maximized or floating (tile/cascade) windows, conversation sidebar grouped by network, detach any window into its own OS window, layout persisted in layout.json
- **Platform**: System tray on Windows (minimize to tray, taskbar flash on notification), desktop notifications on supported platforms

**build.rs:** Compiles Windows resources (icon, version info) using llvm-mingw windres.

New archive, workspace, automation and upload services must stay disabled in `App::with_settings` tests. Enable production persistence only in `App::load`; use in-memory or scratch services in tests. Row measurement must not send actions or advance read state. Background upload results are scoped to logical network, connection generation, window ID and composer nonce.
