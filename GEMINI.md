# Linefeed Project Context

## Project Overview

Linefeed is a cross-platform IRC client written in Rust. It utilizes **egui/eframe** for the graphical user interface and supports Windows (ARM64, x86) and Linux.

*   **Language**: Rust (Edition 2024)
*   **GUI Framework**: `eframe` / `egui`
*   **Async Runtime**: `tokio`
*   **Networking**: TLS via `native-tls` (SChannel on Windows, OpenSSL on Linux), SASL PLAIN authentication.
*   **Platform Support**: Linux, Windows (ARM64, x86-64).

## Building and Running

### Python Build Script (Recommended)
The project includes a wrapper script `build.py` for managing builds and cross-compilation.

```bash
python3 build.py              # Build for host (or default target)
python3 build.py --linux      # Build for Linux
python3 build.py --all        # Build for all targets + UPX compression
```

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
Windows cross-compilation from Linux requires the **llvm-mingw** toolchain (configured in `.cargo/config.toml`).

## Codebase Architecture

The application follows a threaded model separating the GUI (main thread) from the network logic (background thread).

### Directory Structure

*   **`src/main.rs`**: Application entry point. Defines `FmIrcApp`, manages the connection thread, and handles the system tray (Windows).
*   **`src/gui/`**: specific GUI logic and state management.
    *   **`mod.rs`**: Contains `IrcApp` (the primary UI state), routes incoming messages, and handles numerics.
    *   **`commands.rs`**: Implementation of all slash commands (e.g., `/join`, `/msg`, `/ns`).
    *   **`dialogs.rs`**: Code for modal windows (Settings, Channel List, Channel Info).
    *   **`formatting.rs`**: IRC color parsing (mIRC codes) and text rendering.
    *   **`types.rs`**: Core data structures (`Settings`, `Channel`, `ChatMessage`).
    *   **`logging.rs`**: `LogManager` for handling persistent chat history (irssi format).
*   **`src/irc/`**: IRC protocol implementation.
    *   **`client.rs`**: `IrcClient` struct. Handles async TCP/TLS connections, SASL, and PING/PONG.
    *   **`message.rs`**: `IrcMessage` parser, `IrcCommand` enum, and IRCv3 tag handling.
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
    *   **Logging**: Chat logs are stored in `~/.config/linefeed/logs/` in irssi-compatible format.
    *   **Settings**: Configuration is stored in `~/.config/linefeed/settings.json`.
    *   **Style**: Rust standard formatting (`cargo fmt`) and strict linting (`clippy`).
