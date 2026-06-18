# Linefeed Project Context

## Project Overview

Linefeed is a cross-platform IRC client written in Rust. It utilizes **egui/eframe** for the graphical user interface and supports Windows (ARM64, x86) and Linux.

*   **Language**: Rust (Edition 2024)
*   **GUI Framework**: `eframe` / `egui`
*   **Async Runtime**: `tokio`
*   **Networking**: TLS via `native-tls` (SChannel on Windows, OpenSSL on Linux), IRCv3 CAP negotiation, SASL PLAIN authentication with timeout/error fallback.
*   **Platform Support**: Linux, Windows (ARM64, x86).

## Building and Running

### Python Build Script (Recommended)
The project includes a wrapper script `build.py` for managing builds and cross-compilation.

```bash
python3 build.py              # Build Windows ARM64 (default)
python3 build.py --linux      # Build for Linux
python3 build.py --arm64 --x86  # Build Windows ARM64 and x86
python3 build.py --all        # Build all targets + UPX compression where supported
python3 build.py --update-deps # Explicitly update dependency pins
```

The build script checks dependencies with `tools/update-deps.py --check` and
builds with `cargo build --locked --profile dist`.

### Cargo Commands
Standard cargo commands work for development.

```bash
# Development (Fast, incremental)
cargo run
cargo build

# Release (Optimized)
cargo build --release

# Distribution (Smallest binary size, Full LTO)
cargo build --profile dist
```

### Cross-Compilation
Windows cross-compilation from Linux requires the **llvm-mingw** toolchain.
The `.cargo/config.toml` expects `aarch64-w64-mingw32-*` and
`i686-w64-mingw32-*` tools on `PATH`; alternatively set `LLVM_MINGW_HOME` to
the llvm-mingw installation directory.

### Verification

```bash
cargo check --locked
cargo test --locked
cargo clippy --all-targets -- -D warnings
python3 tools/update-deps.py --check
python3 -m py_compile build.py tools/update-deps.py
```

## Codebase Architecture

The application follows a threaded model separating the GUI (main thread) from the network logic (background thread).

### Directory Structure

*   **`src/main.rs`**: Application entry point. Defines `FmIrcApp`, manages the connection thread, and handles the system tray (Windows).
*   **`src/gui/`**: specific GUI logic and state management.
    *   **`mod.rs`**: Contains `IrcApp` (the primary UI state), routes incoming messages, handles numerics, tracks connection intent, and applies server ISUPPORT/MODE state.
    *   **`commands.rs`**: Implementation of all slash commands (e.g., `/join`, `/msg`, `/ns`).
    *   **`dialogs.rs`**: Code for modal windows (Settings, Channel List, Channel Info).
    *   **`formatting.rs`**: IRC formatting parsing, including mIRC colors and reverse video, plus text rendering.
    *   **`types.rs`**: Core data structures (`Settings`, `Channel`, `ChatMessage`).
    *   **`logging.rs`**: `LogManager` for handling persistent chat history (irssi format).
*   **`src/irc/`**: IRC protocol implementation.
    *   **`client.rs`**: `IrcClient` struct. Handles async TCP/TLS connections, CAP/SASL handshakes, and PING/PONG.
    *   **`message.rs`**: `IrcMessage` parser, `IrcCommand` enum, IRCv3 tag handling, and wire-format rendering.
    *   **`numerics.rs`**: Constants for IRC numeric replies.
*   **`tools/`**: Helper scripts (`gen-icon.py` for assets, `update-deps.py` for dependency management).

### Key Architectural Concepts

1.  **Threading Model**:
    *   **Main Thread**: Runs the `egui` event loop (`FmIrcApp::update`).
    *   **Connection Thread**: Runs a single-threaded `tokio` runtime for `IrcClient`.
    *   **Communication**: `mpsc` channels are used: `cmd_tx` (GUI -> IRC) and `msg_rx` (IRC -> GUI).

2.  **Data Structures**:
    *   `FmIrcApp`: Wrapper around `IrcApp`, owns the thread handles.
    *   `IrcApp`: Stores application state, channels, messages, and settings.
    *   `Channel`: Manages user lists, modes, and message history for a specific channel.
    *   `Settings`: Persisted configuration (loaded/saved to JSON).

3.  **Conventions**:
    *   **Logging**: Chat logs are stored in `~/.config/linefeed/logs/` on Linux and `%APPDATA%\linefeed\logs\` on Windows in irssi-compatible format.
    *   **Settings**: Configuration is stored in `~/.config/linefeed/settings.json` on Linux and `%APPDATA%\linefeed\settings.json` on Windows.
    *   **Style**: Rust standard formatting (`cargo fmt`) and strict linting (`clippy`).
