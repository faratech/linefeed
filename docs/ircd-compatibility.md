# IRCd compatibility

Linefeed derives network rules from IRCv3 CAP and `RPL_ISUPPORT` instead of
assuming one daemon's defaults. This covers ordinary client use on the major
modern and legacy IRCd families, including InspIRCd, UnrealIRCd, Solanum and
ratbox descendants, Ergo, Bahamut, ircu and snircd, Hybrid, Plexus, IRCnet
ircd, and ngIRCd.

| Protocol area | Client behavior | Common implementations |
|---|---|---|
| Casemapping | Supports `ascii`, `rfc1459`, both strict-RFC1459 spellings, and `UTF8MAPPING=rfc8265` PRECIS identifiers. Re-keys channel, query, ignore, and presence state when the server changes mapping. | Ergo, InspIRCd, UnrealIRCd, Solanum and legacy IRCds |
| Channel and status syntax | Reads `CHANTYPES`, arbitrary `PREFIX` mode/symbol pairs, multiple simultaneous prefixes, `CHANMODES`, and `STATUSMSG`. Unknown ranks retain their advertised symbol and sort order. | All listed families |
| User identity | Parses userhost-in-NAMES and extended-join; tracks ACCOUNT, AWAY, CHGHOST, SETNAME, nick changes, bot flags, and WHO/WHOX results across shared channels. It automatically issues token-correlated WHOX after NAMES when advertised. | InspIRCd, UnrealIRCd, Solanum, Ergo |
| Presence | Uses IRCv3 MONITOR and `extended-monitor`; translates `/monitor` to WATCH when only the legacy token is advertised. Presence survives outside shared channels. | Modern IRCv3 servers; Bahamut, Plexus, UnrealIRCd and other WATCH servers |
| Authentication | Negotiates only advertised SASL mechanisms. Auto preference is EXTERNAL with a client certificate, SCRAM-SHA-256 with credentials, then PLAIN. A required-SASL profile fails closed. | InspIRCd, UnrealIRCd, Solanum, Ergo and services-backed networks |
| Transport security | Supports direct native TLS and client PEM identity files. IRCv3 STS performs a plaintext policy probe, reconnects to the advertised TLS port, validates certificates, and persists unexpired per-host policy. | InspIRCd, UnrealIRCd, Ergo and other IRCv3 STS servers |
| Line and tag limits | Applies `LINELEN` to the base IRC message, enforces IRCv3's separate tag allowance, splits UTF-8 messages safely, and honors `CLIENTTAGDENY` for outgoing client-only tags. | InspIRCd, UnrealIRCd, Solanum |
| Channel discovery | Sends ELIST `>N` only when the server advertises the `U` filter and otherwise filters LIST locally. Reads `SAFELIST` and common length limits. | Modern and legacy IRCds |
| Mode lists | Stores ban (`+b`), exception (`+e`), invite-exception (`+I`), quiet (`+q`), and other advertised CHANMODES type-A lists, including setter and timestamp where supplied. | InspIRCd, UnrealIRCd, Solanum, Ergo, Hybrid and descendants |
| Legacy messaging | Sends CPRIVMSG, CNOTICE, WALLCHOPS, and WALLVOICES with their required parameter layout. `/silence` without an argument queries rather than adding a mask. | ircu, snircd, Bahamut, ratbox and derivatives |
| Vendor commands | Unknown alphabetic slash commands are forwarded after rejecting IRC line injection characters. `/shelp` explicitly sends server `HELP`. | Vendor and network modules without a dedicated Linefeed command |
| History and playback | Supports IRCv3 CHATHISTORY paging, batch playback, reconnect catch-up, read markers, redaction, multiline, and public Nefarious `+H` history channels. | Nefarious, InspIRCd, UnrealIRCd, Ergo and compatible bouncers |

The raw ISUPPORT token map is updated on every `005`, including
`draft/extended-isupport` `-TOKEN` removals. Typed routing and limit values are
rebuilt from that map so a removed token cannot leave stale behavior behind.

Operator challenge-response schemes, WEBIRC gateway identity, proxy
transports, identd, browser WebSockets, DCC file transfer, and legacy STARTTLS
are outside this compatibility layer. They do not affect a normal direct IRC
client session and can be added independently if a deployment needs them.

Primary protocol references:

- [Modern IRC client protocol](https://modern.ircdocs.horse/)
- [IRCv3 specifications](https://ircv3.net/irc/)
- [IRCv3 SASL](https://ircv3.net/specs/extensions/sasl-3.2)
- [IRCv3 STS](https://ircv3.net/specs/extensions/sts)
- [IRCv3 extended-monitor](https://ircv3.net/specs/extensions/extended-monitor)
- [InspIRCd modules](https://docs.inspircd.org/4/modules/)
- [UnrealIRCd client protocol](https://www.unrealircd.org/docs/Client_Protocol)
- [Ergo extension specifications](https://ergo.chat/specs/)
