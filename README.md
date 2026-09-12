# Linefeed IRC Client

A lightweight, cross-platform IRC client written in Rust with a native GUI.

## Features

### Core
- **Cross-platform**: Windows (ARM64, x64, x86), Linux (ARM64, x64), and macOS (universal)
- **TLS encryption**: Native TLS via SChannel (Windows) or OpenSSL (Linux)
- **SASL authentication**: PLAIN mechanism with timeout/error fallback
- **Modern UI**: Clean interface built with egui/eframe

### Connection
- IRCv3 CAP negotiation with timeout fallback
- Auto-reconnect with exponential backoff
- Safe reconnect and server switching without racing the old connection
- Server favorites with quick-connect
- Server password support
- Accept invalid TLS certificates (optional)

### Channels
- Multiple channel support with tabbed interface
- Channel modes display (+nt, +k, etc.)
- Server-aware channel/user prefix handling via ISUPPORT
- Topic viewing and editing
- Ban list management
- Channel info dialog with creation date
- User list with mode indicators (@, +, and server-provided prefixes)

### Messaging
- Private messages (queries)
- CTCP support (VERSION, TIME, PING, FINGER, CLIENTINFO)
- IRC formatting rendering, including mIRC colors and reverse video
- Outbound message splitting at IRC line-length limits
- Nick highlighting with customizable words
- Action messages (/me)
- Notices

### User Experience
- Tab completion for nicks and commands
- Command history (up/down arrows)
- Configurable timestamps
- Hide join/part/quit messages (optional)
- Scrollback limit to manage memory
- Desktop notifications on supported platforms
- Lag meter

### Privacy & Security
- Ignore list with wildcard mask support
- CTCP reply toggle
- Server-side silence support

### Automation
- Auto-join channels on connect
- Auto-perform commands after connect
- Auto-away on idle with configurable timeout
- NickServ auto-identify via auto-perform

### Logging
- Persistent chat history in irssi-compatible format
- IRCv3 server-backed channel history, including public Nefarious `+H` channels
- Configurable history line count
- Automatic log loading on channel join

### Windows-Specific
- System tray with minimize-to-tray
- Taskbar flash on notifications
- Single instance enforcement
- Efficiency Mode (EcoQoS) for reduced power usage

## Installation

### Pre-built Binaries

Download from the [Releases](https://github.com/faratech/linefeed/releases) page:
- `linefeed_arm64.exe` - Windows ARM64
- `linefeed_x64.exe` - Windows x64
- `linefeed_x86.exe` - Windows x86 (32-bit)
- `linefeed_linux_arm64` - Linux ARM64
- `linefeed_linux_x64` - Linux x86_64
- `linefeed_macos` - macOS universal (Apple Silicon + Intel)

macOS binaries are unsigned; on first run either right-click > Open, or run
`xattr -d com.apple.quarantine linefeed_macos`.

### Building from Source

**Requirements:**
- Rust 1.97+
- Python 3.10+ (for build script)
- llvm-mingw on `PATH` or `LLVM_MINGW_HOME` set when building Windows targets from Linux

**Quick build:**
```bash
# Clone the repository
git clone https://github.com/faratech/linefeed.git
cd linefeed

# Build for your platform
cargo build --release

# Or use the build script for optimized binaries
python3 build.py            # Windows ARM64 default
python3 build.py --linux    # Linux
python3 build.py --arm64    # Windows ARM64
python3 build.py --x64      # Windows x64
python3 build.py --x86      # Windows x86
python3 build.py --arm64 --x64
python3 build.py --all      # All platforms
```

Windows cross-builds look for `aarch64-w64-mingw32-*` and
`i686-w64-mingw32-*` tools on `PATH`. If they are not already on `PATH`, set
`LLVM_MINGW_HOME` to the llvm-mingw installation directory.

The build script runs `tools/update-deps.py --check` before building, uses
`cargo build --locked --profile dist`, and optionally compresses supported
outputs with UPX. Use `python3 build.py --update-deps` only when intentionally
updating dependency pins.

Output binaries are in `./dist/` when using `build.py`. Direct Cargo outputs
are in `./target/<profile>/` for native builds and
`./target/<target>/<profile>/` for cross-builds.

**Verification:**
```bash
cargo check --locked
cargo test --locked
cargo clippy --all-targets -- -D warnings
python3 tools/update-deps.py --check
python3 -m py_compile build.py tools/update-deps.py
```

## Commands

### Connection
| Command | Description |
|---------|-------------|
| `/server <host[:port]>` | Connect to server |
| `/disconnect` | Disconnect from server |
| `/reconnect` | Reconnect to server |
| `/quit [message]` | Disconnect with optional message |

### Channels
| Command | Description |
|---------|-------------|
| `/join <channel> [key]` | Join a channel (alias: `/j`) |
| `/part [#channel] [message]` | Leave channel (alias: `/leave`) |
| `/cycle` | Part and rejoin current channel (alias: `/hop`, `/rejoin`) |
| `/topic [text]` | View or set channel topic |
| `/names [#channel]` | List users in channel |
| `/list [pattern]` | List channels matching pattern |
| `/invite <nick>` | Invite user to current channel |
| `/knock <channel>` | Request invite to channel |
| `/history [#channel] [limit]` | Load IRCv3 server history; public `+H` channels do not require joining |

### Messaging
| Command | Description |
|---------|-------------|
| `/msg <nick> <text>` | Send private message (alias: `/query`) |
| `/notice <target> <text>` | Send notice (alias: `/n`) |
| `/me <action>` | Send action message |
| `/say <text>` | Send text as message (even if starts with /) |
| `/amsg <text>` | Message all joined channels |
| `/ame <action>` | Action to all joined channels |
| `/onotice <text>` | Notice to channel operators |

### User Management
| Command | Description |
|---------|-------------|
| `/nick <newnick>` | Change your nickname |
| `/whois <nick>` | Get user information |
| `/whowas <nick>` | Get info on offline user |
| `/who <mask>` | Query matching users |
| `/userhost <nick>` | Get user@host |
| `/ison <nicks>` | Check if users are online |

### Channel Moderation
| Command | Description |
|---------|-------------|
| `/kick <nick> [reason]` | Kick user (alias: `/k`) |
| `/ban <mask>` | Ban user (+b) |
| `/unban <mask>` | Remove ban (-b) |
| `/kickban <nick> [reason]` | Ban and kick (alias: `/kb`) |
| `/op <nick>` | Give operator status (+o) |
| `/deop <nick>` | Remove operator status (-o) |
| `/voice <nick>` | Give voice (+v) |
| `/devoice <nick>` | Remove voice (-v) |
| `/mode <+/-modes>` | Set channel/user modes |

### Services
| Command | Description |
|---------|-------------|
| `/ns <command>` | Send to NickServ |
| `/cs <command>` | Send to ChanServ |
| `/ms <command>` | Send to MemoServ |
| `/hs <command>` | Send to HostServ |
| `/os <command>` | Send to OperServ |
| `/bs <command>` | Send to BotServ |

### CTCP
| Command | Description |
|---------|-------------|
| `/ctcp <nick> <command>` | Send CTCP request |
| `/version <nick>` | Query client version |
| `/ping <nick>` | Measure round-trip latency |

### Status
| Command | Description |
|---------|-------------|
| `/away [message]` | Set away status |
| `/back` | Clear away status |
| `/umode [modes]` | Set/view user modes |

### Privacy
| Command | Description |
|---------|-------------|
| `/ignore [mask]` | List or add to ignore list |
| `/unignore <mask>` | Remove from ignore list |
| `/silence [+/-mask]` | Server-side ignore |
| `/accept [nick]` | Manage callerid accept list |

### Monitoring
| Command | Description |
|---------|-------------|
| `/monitor + <nicks>` | Add nicks to monitor list |
| `/monitor - <nicks>` | Remove from monitor list |
| `/monitor l` | List monitored nicks |
| `/monitor c` | Clear monitor list |
| `/monitor s` | Show online monitored nicks |

### Utility
| Command | Description |
|---------|-------------|
| `/clear` | Clear current window |
| `/lastlog <pattern>` | Search messages in current window |
| `/settings` | Open settings dialog |
| `/perform [command]` | View/add auto-perform commands |
| `/raw <command>` | Send raw IRC command |
| `/echo <text>` | Echo text to current window |
| `/slap <nick>` | Slap someone with a trout |

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Tab` | Complete nick/command |
| `Up/Down` | Command history |
| `Ctrl+W` | Close current tab |
| `Ctrl+Tab` | Next tab |
| `Ctrl+Shift+Tab` | Previous tab |
| `Page Up/Down` | Scroll chat |

## Configuration

Settings are stored in:
- **Linux**: `~/.config/linefeed/settings.json`
- **Windows**: `%APPDATA%\linefeed\settings.json`

Chat logs are stored in:
- **Linux**: `~/.config/linefeed/logs/<network>/<channel>.log`
- **Windows**: `%APPDATA%\linefeed\logs\<network>\<channel>.log`

## Architecture

```
src/
├── main.rs              # Entry point, connection management, system tray
├── icon_data.rs         # Embedded application icon
├── systray.rs           # Windows system tray (Win32 API)
├── gui/
│   ├── mod.rs           # Main UI logic, message handling
│   ├── types.rs         # Data structures, settings
│   ├── commands.rs      # Slash command processing
│   ├── dialogs.rs       # Settings, connect, channel list dialogs
│   ├── formatting.rs    # IRC color parsing, text rendering
│   ├── helpers.rs       # Utility functions
│   └── logging.rs       # Chat history management
└── irc/
    ├── mod.rs           # Module exports
    ├── client.rs        # Async IRC connection, TLS, SASL
    ├── message.rs       # IRC protocol parsing
    └── numerics.rs      # IRC numeric constants
```

## Performance

Linefeed is designed for low resource usage:

- **Adaptive refresh rate**: Reduces from 100ms to 500ms when idle
- **Minimized optimization**: 1 second refresh when minimized
- **System tray mode**: Near-zero CPU when hidden to tray (Windows)
- **Batch message processing**: Handles high-traffic events smoothly
- **Efficiency Mode**: Uses Windows EcoQoS for power efficiency

## License

Creative Commons Attribution 4.0 International (CC BY 4.0) - See [LICENSE](LICENSE) for details.

You are free to share and adapt this work for any purpose, including
commercially, provided you give appropriate credit to the Linefeed project
(https://github.com/faratech/linefeed), link to the license, and indicate
if changes were made.

The vendored `vendor/wayland-scanner` crate retains its upstream MIT
license (see `vendor/wayland-scanner/LICENSE.txt`).

## Contributing

Contributions are welcome! Please feel free to submit issues and pull requests.

## Acknowledgments

- Built with [egui](https://github.com/emilk/egui) - Immediate mode GUI library
- IRC protocol implementation inspired by various open-source clients
