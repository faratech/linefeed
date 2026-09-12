# Nefarious IRCv3 compatibility

This matrix tracks the client-facing capabilities advertised by Nefarious on
its `ircv3.2-upgrade` branch. Linefeed requests a capability only when the
server advertises it and handles `CAP NEW`/`CAP DEL` changes after registration.

| Capability / feature | Linefeed behavior |
|---|---|
| `multi-prefix` | Reads and preserves simultaneous membership prefixes. |
| `userhost-in-names` | Accepts `nick!user@host` NAMES entries without corrupting membership nicknames. |
| `extended-join` | Reads account and real-name data. |
| `away-notify`, `account-notify`, `chghost` | Updates live user state across shared channels. |
| `sasl` | Selects from advertised SCRAM-SHA-256, EXTERNAL, and PLAIN mechanisms, retries alternatives in Auto mode, and can require successful authentication. |
| `cap-notify` | Requests newly available supported capabilities and removes deleted ones. |
| `server-time`, `message-tags`, `account-tag` | Retains canonical timestamps, msgids, and account identity. |
| `echo-message` | Reconciles the authoritative server echo with the optimistic local row and keeps its msgid/time/account. |
| `invite-notify` | Distinguishes third-party channel invites from invites addressed to the local user. |
| `labeled-response`, `batch` | Handles labeled response batches; `/label` sends an explicitly labeled command. |
| `setname` | Sends and displays real-name changes. |
| `standard-replies` | Parses `FAIL`, `WARN`, and `NOTE` and routes channel-context replies to that channel. |
| `no-implicit-names` | Explicitly requests NAMES after joining. The legacy draft alias remains unnecessary when the final capability is offered. |
| `draft/extended-isupport` | Processes every dynamic 005 line in an ISUPPORT batch. |
| `draft/pre-away` | Sends the configured pre-away state, including `*`, before `CAP END`. |
| `draft/multiline` | Sends and reconstructs nested multiline batches with concat semantics and advertised byte/line limits. |
| `draft/chathistory` | Supports LATEST, BEFORE, AFTER, AROUND, BETWEEN, and TARGETS; pages to the requested limit and honors Nefarious's bare integer page-size value. |
| Nefarious channel `+H` | Opens a public channel history tab and queries it without joining. |
| reconnect catch-up | Requests AFTER the newest retained msgid/time for rejoined channels and retained queries, avoiding the server's suppressed legacy replay path. |
| `draft/event-playback` | Displays historical JOIN, PART, QUIT, KICK, MODE, TOPIC, NICK, SETNAME, REDACT, TAGMSG, gap, and multiline events without mutating live membership. |
| `draft/message-redaction` | Sends REDACT, removes a matching live msgid, and displays live/historical redaction events. |
| `draft/read-marker` | Reads server markers and automatically advances the selected target to its newest server timestamp. |
| `draft/channel-rename` | Rekeys the tab and merges an existing destination history tab without dropping messages. |
| `evilnet/channel-relocate` | Displays the destination and provides `/relocate` for operators. |
| `draft/account-registration` | Provides `/register` and `/verify`; capability policy failures are displayed as structured replies. |
| `draft/metadata-2` | Receives metadata batches and exposes `/metadata`; legacy metadata numerics are identified in the server buffer. |
| `draft/webpush` | Exposes endpoint registration/removal through `/webpush` and reads structured replies. A native desktop client does not create a browser push subscription itself. |
| `draft/authtoken` | Lists services, generates/validates tokens, and concatenates chunked token batches into one usable token without writing it to chat logs. |
| `draft/bouncer` | Exposes all server subcommands through `/bouncer` and identifies bouncer numerics/replies. |
| `draft/persistence` | Exposes status/settings/profile/replay commands; attaches the selected profile after SASL and before `CAP END`; accepts persistence replay batches. |
| `draft/oper-tag` | Shows the operator marker or configured oper name beside tagged senders. |
| status messages | Routes `WALLCHOPS`, `WALLHOPS`, `WALLVOICES`, and STATUSMSG-prefixed targets to the underlying channel. |
| reply/reaction/typing tags | `/reply`, `/react`, and `/typing` emit client-only tags; received reactions remain visible and typing updates stay transient. |
| direct TLS and `sts` | Uses OS-native TLS, validates certificates by default, supports client certificates, and persists valid IRCv3 STS policies. The advertisement-only `sts` token is never sent in `CAP REQ`. |

The administrative extension commands intentionally pass their subcommand
syntax through to the server. Nefarious can add subcommands without requiring a
Linefeed release, while protocol responses remain parsed and visible.
