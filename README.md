# Linefeed IRC Client

A lightweight, cross-platform IRC client written in Rust with a native GUI.

See the [Nefarious IRCv3 compatibility matrix](docs/nefarious-compatibility.md)
for the supported `ircv3.2-upgrade` protocol surface and the
[general IRCd compatibility matrix](docs/ircd-compatibility.md) for common
InspIRCd, UnrealIRCd, Solanum, Ergo, Bahamut, ircu, Hybrid, and Plexus features.

## Features

### Core
- **Cross-platform**: Windows (ARM64, x64, x86), Linux (ARM64, x64), and macOS (universal)
- **TLS encryption**: Native TLS via SChannel (Windows) or OpenSSL (Linux)
- **SASL authentication**: SCRAM-SHA-256, EXTERNAL, and PLAIN with optional required-auth policy
- **Modern UI**: Clean interface built with egui/eframe

### Connection
- IRCv3 CAP negotiation with timeout fallback
- Auto-reconnect with exponential backoff
- Safe reconnect and server switching without racing the old connection
- Server favorites with quick-connect
- Server password support
- Accept invalid TLS certificates (optional)
- IRCv3 STS plaintext upgrade and persistent per-host strict-TLS policy
- TLS client certificates for SASL EXTERNAL

### Channels
- Multiple channel support with tabbed interface
- Channel modes display (+nt, +k, etc.)
- Server-aware channel/user prefix handling via ISUPPORT
- Topic viewing and editing
- Ban, exception, invite-exception, quiet, and server-defined mode-list management
- Channel info dialog with creation date
- User list with mode indicators (@, +, and server-provided prefixes)

### Messaging
- Private messages (queries)
- IRCv3 multiline send/replay, echo reconciliation, replies, reactions, and typing tags
- Message redaction, read markers, and stable message IDs
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
- MONITOR with legacy WATCH fallback

### Automation
- Auto-join channels on connect
- Auto-perform commands after connect
- Auto-away on idle with configurable timeout
- NickServ auto-identify via auto-perform

### Logging
- Persistent chat history in irssi-compatible format
- Paged IRCv3 server-backed history, including reconnect catch-up and public Nefarious `+H` channels
- Historical JOIN/PART/QUIT/KICK/MODE/TOPIC/NICK/REDACT and multiline playback
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
| `/history [#channel] [limit]` | Load paged IRCv3 server history; public `+H` channels do not require joining |
| `/history before\|after\|around <target> <ref> [limit]` | Query history relative to a timestamp or msgid |
| `/history between <target> <ref1> <ref2> [limit]` | Query history between two references |
| `/history targets <ref1> <ref2> [limit]` | List targets with history in a time range |
| `/markread [target] [timestamp=<time>\|*]` | Get or update an IRCv3 read marker |
| `/redact [target] <msgid> [reason]` | Redact a message by IRCv3 message ID |
| `/rename <#new-channel> [reason]` | Rename the current channel |
| `/relocate <#new-channel> [reason]` | Relocate the current channel |

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
| `/reply <msgid> <text>` | Reply to a message using its IRCv3 ID |
| `/react <msgid> <reaction>` | React to a message |
| `/typing active\|paused\|done` | Send an IRCv3 typing state |
| `/label <label> <IRC command>` | Send a command using IRCv3 labeled-response |
| `/cprivmsg <nick> <channel> <text>` | Send a channel-context private message |
| `/cnotice <nick> <channel> <text>` | Send a channel-context notice |
| `/wallchops <channel> <text>` | Message channel operators |
| `/wallvoices <channel> <text>` | Message voiced channel members |

### User Management
| Command | Description |
|---------|-------------|
| `/nick <newnick>` | Change your nickname |
| `/setname <real name>` | Change your IRCv3 real name |
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

If a slash command is not built in, Linefeed forwards a syntactically valid
alphabetic command to the server. This keeps vendor commands usable without a
client release; `/raw` remains available when exact wire syntax is needed.

### Nefarious IRCv3 Extensions
| Command | Description |
|---------|-------------|
| `/bouncer <subcommand> ...` | Manage Nefarious bouncer sessions and settings |
| `/persistence <subcommand> ...` | Manage persistence, replay, and profiles |
| `/metadata <subcommand> ...` | Query, set, clear, and subscribe to metadata |
| `/webpush <subcommand> ...` | Register or remove a web push endpoint |
| `/register <account> <email\|*> <password>` | Register an account |
| `/verify <account> <code>` | Verify an account when supported by server policy |
| `/token servicelist\|generate\|validate ...` | Use Nefarious external-service auth tokens |

The Connect and Automation settings include pre-connect away status and a
Nefarious persistence profile. These are sent during registration, before
`CAP END`, as required by the corresponding drafts.

### Utility
| Command | Description |
|---------|-------------|
| `/clear` | Clear current window |
| `/lastlog <pattern>` | Search messages in current window |
| `/settings` | Open settings dialog |
| `/shelp [topic]` | Request the server's HELP command |
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
