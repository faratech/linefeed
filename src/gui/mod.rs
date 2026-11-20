mod actions;
mod app;
mod archive;
mod attention;
mod auth_history;
mod automation;
pub(crate) mod bouncer;
mod commands;
mod connection;
mod conversations;
mod dialogs;
pub(crate) mod fonts;
mod formatting;
mod helpers;
#[cfg(test)]
mod history_regressions;
pub(crate) mod identity;
mod logging;
mod message_ui;
mod search;
mod snap;
pub(crate) mod theme;
mod types;
mod uploads;
mod view;
mod widgets;
mod windows;
pub(crate) mod workspace;

use connection::{ConnectionLifecycle, RecoveryPolicy};

use egui::{Color32, RichText, ScrollArea};
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use tokio::sync::mpsc;

use crate::irc::client::{DESIRED_CAPS, SaslMechanism, ServerConfig};
use crate::irc::numerics::*;
use crate::irc::{IrcCommand, IrcMessage};
pub use app::App;
pub(crate) use app::MAIN_WINDOW_TITLE;
pub(crate) use conversations::ConversationId;
use conversations::ConversationStore;
use formatting::{nick_color, render_segments};
use helpers::{
    ctcp_action_text, epoch_time_formatted, format_ctcp_time, format_timestamp,
    local_utc_offset_secs, mask_matches, parse_ctcp, truncate_chars,
};
pub use types::{
    CaseMapping, Channel, ChannelListEntry, ChannelListSort, ChatMessage, KnownUser,
    NetworkSupport, RowHeightEntry, ServerFavorite, Settings, SortDirection, TabCompletion,
    UserMode,
};
use types::{CtcpReplyDecision, CtcpReplyLimiter};

/// `/window` subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowCommand {
    Tile,
    Cascade,
    Maximize,
    Restore,
    Detach,
    Attach,
    Close,
    New,
}

impl WindowCommand {
    pub(crate) fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "tile" => Self::Tile,
            "cascade" => Self::Cascade,
            "max" | "maximize" => Self::Maximize,
            "restore" | "float" => Self::Restore,
            "detach" => Self::Detach,
            "attach" | "reattach" => Self::Attach,
            "close" => Self::Close,
            "new" => Self::New,
            _ => return None,
        })
    }
}

/// Requests a session makes of the App, drained every frame. A session never
/// reaches the App directly: notifications, settings and new connections are
/// global, so they go through here.
pub(crate) enum SessionEvent {
    UploadRequested {
        conversation: Option<ConversationId>,
        source: uploads::UploadSource,
        composer_nonce: String,
    },
    UploadTokenReady {
        job_id: String,
        token: Option<String>,
    },
    SearchRequested {
        query: String,
        all_networks: bool,
        conversation: Option<ConversationId>,
    },
    RevealMessage(identity::MessageRef),
    MessageRedacted(identity::MessageRef),
    /// A desktop notification; the App applies focus, the setting and rate
    /// limits (`App::notify`).
    Notify {
        title: String,
        body: String,
        force: bool,
        attention: Option<attention::AttentionEvent>,
    },
    /// `/settings`
    OpenSettings,
    /// Persist this session's behaviour settings and the favorites, not
    /// its connect form (#316).
    SaveConfig,
    /// Persist this session's auto-perform list (`/perform`), not the rest
    /// of its connect form (#316).
    SaveAutoPerform,
    /// `/window ...`, typed in the window showing `target`.
    Window {
        command: WindowCommand,
        conversation: Option<ConversationId>,
    },
    /// Mutable target metadata; runtime identity is unchanged.
    ConversationRenamed {
        id: ConversationId,
        old: String,
        new: String,
    },
    /// Redirect references once, then permanently invalidate the retired ID.
    ConversationMerged {
        survivor: ConversationId,
        retired: ConversationId,
    },
    /// A new endpoint retires old target and status editing state.
    EndpointChanged,
    /// A worker was not started because another session is live on the
    /// same endpoint.
    DuplicateEndpoint,
    NewBouncerNetwork(Box<SessionConfig>),
    /// `/server -m`: connect to an endpoint in a new status window.
    NewServer {
        host: String,
        port: String,
        use_tls: bool,
    },
}

pub(crate) use identity::TransportEndpointKey as EndpointKey;

/// Immutable configuration for one connection/reconnection lifecycle. The
/// editable Settings/Connect fields may change while a socket is active; those
/// changes must not relabel the session or alter what an automatic reconnect
/// authenticates with and executes.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionConfig {
    #[serde(default)]
    upload_settings: uploads::UploadSettings,
    /// Required: see `ServerConfig`. Without it credentials would have no
    /// endpoint to go to.
    server: ServerConfig,
    #[serde(default)]
    auto_join_channels: String,
    #[serde(default)]
    auto_perform: String,
    #[serde(default)]
    set_invisible: bool,
    /// The network profile (server favorite) this connection was started
    /// from, if any: it names the session's windows and keys its layout.
    #[serde(default)]
    profile_id: Option<String>,
}

/// Private, short-lived state used only across an explicit update restart.
/// Deliberately has no Debug implementation: credentials and drafts are secret.
/// The previous binary writes it and the updated one reads it, so fields added
/// later must default rather than fail the launch (`serde(default)`). Nothing
/// here names an endpoint or credentials on its own: the session that does is
/// an `Option`, and without it nothing is restored.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UpdateSession {
    session: Option<SessionConfig>,
    reconnect: bool,
    /// Initial connections and switches to another endpoint start their own
    /// configured joins rather than restoring an established membership set.
    fresh_connect: bool,
    tabs: Vec<(String, Option<String>, bool)>,
    selected: Option<String>,
    input: String,
    /// Per-window drafts. Absent in older snapshots, which only carried input.
    drafts: Option<Vec<(Option<String>, String)>>,
    away: Option<String>,
    /// Every other session, each a snapshot of the same shape (with no
    /// `others` of its own). Older binaries ignore this and restore only
    /// the top-level (active) session.
    others: Vec<UpdateSession>,
    /// Where the top-level session stood among `others`, so a restart keeps
    /// the networks in their order (switchbar, Alt+N).
    active_position: usize,
}

impl UpdateSession {
    pub(crate) fn input(&self) -> &str {
        &self.input
    }

    pub(crate) fn set_input(&mut self, input: String) {
        self.input = input;
    }

    pub(crate) fn set_drafts(&mut self, drafts: Vec<(Option<String>, String)>) {
        self.drafts = Some(drafts);
    }

    pub(crate) fn take_drafts(&mut self) -> Option<Vec<(Option<String>, String)>> {
        self.drafts.take()
    }

    pub(crate) fn set_others(&mut self, others: Vec<UpdateSession>) {
        self.others = others;
    }

    pub(crate) fn take_others(&mut self) -> Vec<UpdateSession> {
        std::mem::take(&mut self.others)
    }

    pub(crate) fn active_position(&self) -> usize {
        self.active_position
    }

    pub(crate) fn set_active_position(&mut self, position: usize) {
        self.active_position = position;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryPlacement {
    Prepend,
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryDirection {
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistorySubcommand {
    Latest,
    Before,
    After,
    Around,
    Between,
    Targets,
}

impl HistorySubcommand {
    fn parse(word: &str) -> Option<Self> {
        Some(match word.to_ascii_uppercase().as_str() {
            "LATEST" => Self::Latest,
            "BEFORE" => Self::Before,
            "AFTER" => Self::After,
            "AROUND" => Self::Around,
            "BETWEEN" => Self::Between,
            "TARGETS" => Self::Targets,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
struct HistoryRequestCorrelation {
    // None after the owning view is retired. Keep the record until its reply
    // arrives, so a later same-name view cannot inherit that exchange.
    owner: Option<ConversationId>,
    target: String,
    // A server can name either the requested target or an intervening rename.
    aliases: Vec<String>,
    subcommand: HistorySubcommand,
    label: Option<String>,
    sequence: u64,
}

struct OutgoingHistoryRequest {
    target: String,
    subcommand: HistorySubcommand,
    label: Option<String>,
    limit: usize,
}

#[derive(Debug, Clone)]
struct PendingHistoryRequest {
    correlation: Option<HistoryRequestCorrelation>,
    placement: HistoryPlacement,
    direction: HistoryDirection,
    remaining: usize,
    loaded: usize,
    shown: usize,
    page_limit: usize,
    /// An automatic reconnect catch-up anchored at a message this client
    /// already has (CHATHISTORY AFTER): its rows are genuinely new, so they
    /// raise unread and notifications. Latest-page fallbacks and user-requested
    /// history are never announced (#306).
    announce_arrivals: bool,
}

struct ChatHistoryBatch {
    legacy_playback: bool,
    target: String,
    messages: Vec<ChatMessage>,
    request: PendingHistoryRequest,
    complete: bool,
    partial: bool,
}

struct MultilineBatch {
    target: String,
    sender: String,
    sender_prefix: Option<String>,
    content: String,
    has_lines: bool,
    line_count: usize,
    oversized: bool,
    is_notice: bool,
    parent_history: Option<String>,
    server_time: Option<String>,
    msgid: Option<String>,
    account: Option<String>,
    oper: Option<String>,
    reply_to: Option<String>,
}

/// Longest draft/authtoken token kept: a token is a short credential, and a
/// batch that streams chunks without end must not grow memory for as long as
/// the connection lives (the multiline batches are capped alike, #191).
const MAX_AUTHTOKEN_BYTES: usize = 8192;

struct AuthtokenBatch {
    service: String,
    token: String,
    /// The token outgrew `MAX_AUTHTOKEN_BYTES`; it was dropped and the rest
    /// of the batch is ignored.
    oversized: bool,
}

impl SessionConfig {
    fn endpoint_key(&self) -> EndpointKey {
        EndpointKey {
            host: self.server.host.to_lowercase(),
            port: self.server.port,
            use_tls: self.server.use_tls,
        }
    }

    fn endpoint_label(&self) -> String {
        let host = if self.server.host.contains(':') {
            format!("[{}]", self.server.host)
        } else {
            self.server.host.clone()
        };
        format!("{}:{}", host, self.server.port)
    }

    fn log_identity(&self) -> String {
        self.logical_network_key().storage_key()
    }

    fn logical_network_key(&self) -> identity::LogicalNetworkKey {
        self.server.bouncer.as_ref().map_or_else(
            || identity::LogicalNetworkKey::Direct {
                endpoint: self.endpoint_key(),
            },
            |binding| binding.key(self.endpoint_key()),
        )
    }

    /// The configured services identities this session can authenticate as.
    /// Auto can use either the certificate or a password-account fallback,
    /// depending on what the server offers. Track both so changing either
    /// starts a new lineage. Explicit EXTERNAL ignores the unused account.
    /// Account case and surrounding whitespace do not fork the lineage.
    fn services_identity_key(&self) -> (Option<String>, Option<String>) {
        let cert = self.server.tls_client_cert_path.as_deref().map(str::trim);
        let account = self
            .server
            .sasl_username
            .as_deref()
            .map(|account| account.trim().to_ascii_lowercase());
        match self.server.sasl_mechanism {
            SaslMechanism::External => (None, cert.map(str::to_string)),
            SaslMechanism::Auto => (account, cert.map(str::to_string)),
            _ => (account, None),
        }
    }

    fn allows_legacy_log_migration(&self) -> bool {
        self.server.bouncer.is_none()
            && ((self.server.use_tls && self.server.port == 6697)
                || (!self.server.use_tls && self.server.port == 6667))
    }
}

/// Logical payloads that were queued successfully; deliberately not Debug:
/// service messages may contain credentials.
#[derive(Default)]
struct TextSendOutcome {
    complete: bool,
    payloads: Vec<String>,
}

const IRC_MAX_LINE_BYTES: usize = 510;
pub(crate) const PROJECT_SOURCE_URL: &str = "https://github.com/faratech/linefeed";
const COMMAND_COMPLETIONS: &[&str] = &[
    "/accept",
    "/admin",
    "/ame",
    "/amsg",
    "/away",
    "/back",
    "/ban",
    "/bouncer",
    "/botserv",
    "/chanserv",
    "/clear",
    "/close",
    "/cnotice",
    "/cprivmsg",
    "/ctcp",
    "/cycle",
    "/dehalfop",
    "/deop",
    "/devoice",
    "/describe",
    "/disconnect",
    "/echo",
    "/grep",
    "/halfop",
    "/history",
    "/hop",
    "/hostserv",
    "/ignore",
    "/info",
    "/invite",
    "/ison",
    "/join",
    "/kick",
    "/kickban",
    "/label",
    "/knock",
    "/lastlog",
    "/links",
    "/list",
    "/lusers",
    "/me",
    "/markread",
    "/metadata",
    "/memoserv",
    "/mode",
    "/monitor",
    "/motd",
    "/msg",
    "/names",
    "/nick",
    "/nickserv",
    "/notice",
    "/onotice",
    "/op",
    "/operserv",
    "/part",
    "/persistence",
    "/perform",
    "/ping",
    "/query",
    "/quote",
    "/quit",
    "/raw",
    "/react",
    "/redact",
    "/register",
    "/relocate",
    "/rename",
    "/reconnect",
    "/rejoin",
    "/reply",
    "/say",
    "/search",
    "/server",
    "/setname",
    "/settings",
    "/shelp",
    "/silence",
    "/slap",
    "/stats",
    "/sversion",
    "/time",
    "/topic",
    "/token",
    "/trace",
    "/typing",
    "/umode",
    "/unban",
    "/unignore",
    "/userhost",
    "/version",
    "/verify",
    "/webpush",
    "/window",
    "/voice",
    "/wallchops",
    "/wallops",
    "/wallvoices",
    "/who",
    "/whois",
    "/whowas",
];

pub struct Session {
    pub(crate) upload_settings: uploads::UploadSettings,
    pub(crate) upload_discovery: uploads::UploadDiscovery,
    pub(crate) archive_client: Option<archive::ArchiveClient>,
    redacted_history_version: u64,
    automation: std::sync::Arc<automation::AutomationConfig>,
    macro_jobs: VecDeque<automation::MacroJob>,
    next_macro_at: std::time::Instant,
    automation_send_failed: bool,
    account_history: auth_history::AccountHistory,
    /// Outstanding memberships to recover on this endpoint. Some(empty)
    /// deliberately suppresses configured auto-join after a registered loss.
    pending_rejoins: RecoveryPolicy,
    /// Successfully queued PARTs that the current server has not confirmed.
    /// They must not become recovery JOINs if that transport disappears.
    pending_parts: Vec<String>,
    /// Deliberate PART/JOIN cycles retain their conversation identity.
    pending_cycles: std::collections::HashSet<ConversationId>,
    update_selected: Option<String>,
    update_join_targets: Vec<String>,
    // The phase owns the worker, transport and any replacement connection.
    connection: ConnectionLifecycle,
    pub(crate) connection_status: crate::irc::client::ConnectionStatus,
    pub my_nick: String,

    // Server config
    pub server_host: String,
    pub server_port: String,
    pub use_tls: bool,
    pub accept_invalid_certs: bool,
    pub nickname: String,
    pub username: String,
    pub realname: String,
    pub password: String,

    // Immutable identity of the currently displayed endpoint. A replacement
    // belongs to the Closing phase until the old worker has exited.
    active_session: Option<SessionConfig>,
    connection_error: Option<String>,

    // Channels and messages
    conversations: ConversationStore,
    pub(crate) current_conversation: Option<ConversationId>,
    pub server_messages: VecDeque<ChatMessage>,
    pub server_unread: usize,

    // Input
    pub input_text: String,
    pub(crate) composer: message_ui::ComposerState,
    pub(crate) timeline: std::cell::RefCell<message_ui::TimelineView>,
    pub(crate) message_ui: message_ui::MessageUiState,
    pub(crate) attention: attention::AttentionSettings,
    pub(crate) view_is_active: bool,
    pub(crate) reading_conversation: Option<ConversationId>,
    pub join_channel: String,

    /// Identifies this session to the window manager; never reused.
    pub(crate) id: windows::ConnId,
    /// The network profile (server favorite) this session was started from.
    pub(crate) profile_id: Option<String>,
    /// The favorite the connect form was last loaded from, cleared once the
    /// form stops describing it (its credentials are cleared).
    pub(crate) form_profile: Option<String>,
    /// Requests for the App, drained every frame (see `SessionEvent`).
    pub(crate) events: Vec<SessionEvent>,
    /// Endpoints other sessions are live on (connected, connecting, or
    /// waiting to reconnect), set by the App before each lifecycle pass. A
    /// worker is never started for one of them: two sessions on one
    /// endpoint would share log files and nick state.
    pub(crate) blocked_endpoints: Vec<EndpointKey>,
    pub(crate) blocked_networks: Vec<identity::LogicalNetworkKey>,

    // UI state
    pub scroll_to_bottom: bool,
    pub show_connect_dialog: bool,

    // Command history. While a window draws, its own history is swapped
    // in from its TargetView, so this holds the window's lines then; the
    // resting value serves input submitted outside any window.
    pub command_history: Vec<String>,
    pub history_index: Option<usize>,
    pub history_temp: String,

    // Channel list popup
    pub channel_list: Vec<ChannelListEntry>,
    pub channel_list_loading: bool,
    /// Minimum user count applied to each incoming 322 row, frozen when the
    /// LIST was requested so the Min control cannot change the rule midway
    /// through one response. 0 means no client-side filtering (#317).
    pub(crate) channel_list_min_users: u32,
    pub show_channel_list: bool,
    pub channel_list_filter: String,
    pub channel_list_selected: Option<String>,
    pub channel_list_sort: ChannelListSort,
    pub channel_list_sort_dir: SortDirection,

    // Channel list view cache: filtered+sorted indices into channel_list,
    // recomputed only when the list, filter, or sort changes (re-sorting up to
    // 50k rows every frame would otherwise burn a core while the dialog is open).
    pub(crate) channel_list_cache: Vec<usize>,
    pub(crate) channel_list_dirty: bool,
    pub(crate) channel_list_cache_filter: String,
    pub(crate) channel_list_cache_sort: (ChannelListSort, SortDirection),
    pub(crate) channel_list_last_refilter: Option<std::time::Instant>,

    // Tab completion
    pub tab_completion: Option<TabCompletion>,
    /// Give the input line keyboard focus on its next draw (swapped in per
    /// window, like the input text: see `app::with_window_view`).
    pub(crate) input_focus_request: bool,
    /// Whether the input line had keyboard focus when last drawn.
    pub(crate) input_had_focus: bool,

    // User list selection
    pub selected_user: Option<String>,

    // Connection options
    pub auto_join_channels: String,
    pub set_invisible: bool,
    pub auto_reconnect: bool,

    // Window focus tracking (for notifications)
    pub window_focused: bool,

    // Ignore list
    pub ignore_list: Vec<String>,

    // Auto-perform commands (newline-separated)
    pub auto_perform: String,
    pub pre_away_message: String,
    pub persistence_profile: String,

    // Pending auto-perform (to execute after connect)
    pub pending_auto_perform: Option<Vec<String>>,

    // Pending channel keys (for storing key when joining +k channels)
    pub pending_channel_keys: HashMap<String, String>,
    /// Channels the user asked to join (/join, the join box, the channel
    /// list). The matching own-JOIN echo moves focus to the new tab; any other
    /// own-JOIN (auto-join, bouncer replay, reconnect) leaves the tab the user
    /// is reading alone (#290).
    join_focus_requests: Vec<String>,

    // Pending invites (from_nick, channel)
    pub pending_invites: Vec<(String, String)>,

    // Away status tracking
    pub away_status: Option<String>,
    pub last_activity: std::time::Instant,
    pub auto_away_triggered: bool,

    // Auto-away settings
    pub auto_away_enabled: bool,
    pub auto_away_minutes: u32,
    pub auto_away_message: String,

    // SASL authentication
    pub sasl_username: String,
    pub sasl_password: String,
    pub sasl_mechanism: SaslMechanism,
    pub sasl_required: bool,
    pub(crate) bouncer: Option<bouncer::BouncerBinding>,
    pub(crate) bouncer_networks: std::collections::BTreeMap<String, bouncer::BouncerNetwork>,
    bouncer_ready: bool,
    bouncer_retired_targets: std::collections::HashSet<String>,
    pub tls_client_cert_path: String,
    pub tls_client_key_path: String,

    // Logging
    pub log_manager: logging::LogManager,
    pub logging_enabled: bool,
    pub logging_load_history: bool,
    pub logging_history_lines: usize,

    // Display settings
    pub timestamp_format: String,
    pub hide_join_part: bool,
    pub max_scrollback: usize,

    // Privacy
    pub ctcp_replies_enabled: bool,

    // Highlights
    pub highlight_words: String,

    // Server-advertised protocol support
    pub network_support: NetworkSupport,
    enabled_caps: std::collections::HashSet<String>,
    chathistory_enabled: bool,
    chathistory_limit: Option<usize>,
    chathistory_batches: HashMap<String, ChatHistoryBatch>,
    batch_parents: HashMap<String, String>,
    batch_labels: HashMap<String, String>,
    multiline_batches: HashMap<String, MultilineBatch>,
    authtoken_batches: HashMap<String, AuthtokenBatch>,
    pending_history_requests: HashMap<String, VecDeque<PendingHistoryRequest>>,
    next_history_request: u64,
    known_users: HashMap<String, KnownUser>,
    monitored_users: std::collections::HashSet<String>,
    /// Outstanding WHOX queries as (token, channel), oldest first: a 315 names
    /// no token, so it completes the oldest query still open for its channel.
    pending_whox: VecDeque<(String, String)>,
    next_whox_token: u16,
    /// Budget for automatic CTCP replies (see `CTCP_REPLY_BURST`).
    ctcp_reply_limiter: CtcpReplyLimiter,

    // Cached lowercase versions for efficient comparison (updated when source changes)
    my_nick_lower: String,
    ignore_list_lower: Vec<String>,
    highlight_words_lower: Vec<String>,

    // Reconnect settings
    pub reconnect_delay_secs: u32,
    pub max_reconnect_attempts: u32,

    // Custom messages
    pub quit_message: String,
    pub part_message: String,

    // Channel list
    pub list_min_users: u32,

    // Channel info dialog
    pub show_channel_info: bool,
    pub(crate) channel_info_target: Option<ConversationId>,

    // Lag meter
    pub lag_ms: Option<u32>,
    pub ping_sent_time: Option<std::time::Instant>,
    pub last_lag_check: std::time::Instant,

    // Color picker state
    pub show_color_picker: bool,
    pub color_picker_fg: bool, // true = selecting foreground, false = selecting background

    // CPU optimization state
    pub had_messages_this_frame: bool,

    nick_retry_attempts: u8,
    /// Nicks this client has held this session (newest first, capped). Own
    /// rows replayed under a former nick — e.g. a credential command sent
    /// before a GHOST/REGAIN — must still be recognized as ours (#163).
    past_nicks: Vec<String>,
    /// Whether `my_nick` was confirmed in use (001 or an accepted NICK) on
    /// the current connection. Unconfirmed nicks — the configured nick a 433
    /// retry walked away from, a pre-registration speculative suffix — must
    /// not be recorded as held when replaced (#167).
    nick_confirmed: bool,
    /// Canonicalized keys of my_nick + past_nicks, maintained on nick change
    /// and CASEMAPPING refresh so sender_is_self costs one canonicalize of
    /// the incoming nick (the held-nick side is precomputed).
    self_nick_keys: Vec<String>,
    /// Memoized switchbar tab order with the fingerprint it was sorted
    /// under: the order is rebuilt every frame, but the tab set (and the
    /// casemapping that orders it) rarely changes.
    tab_order_cache: std::cell::RefCell<Option<(u64, Vec<Option<ConversationId>>)>>,
}

impl Default for Session {
    fn default() -> Self {
        Self::with_settings(Settings::load())
    }
}

impl Session {
    pub(crate) fn with_settings(settings: Settings) -> Self {
        ChatMessage::configure_default_timestamp_format(&settings.timestamp_format);
        let nickname = if settings.nickname.is_empty() {
            format!("Linefeed_{}", rand_suffix())
        } else {
            settings.nickname.clone()
        };
        let network_support = NetworkSupport::default();
        let my_nick_lower = network_support.canonicalize(&nickname);
        let self_nick_keys = vec![my_nick_lower.clone()];

        Self {
            automation: std::sync::Arc::new(automation::AutomationConfig::default()),
            macro_jobs: VecDeque::new(),
            next_macro_at: std::time::Instant::now(),
            automation_send_failed: false,
            account_history: auth_history::AccountHistory::default(),
            archive_client: None,
            redacted_history_version: 0,
            upload_settings: settings.upload_settings,
            upload_discovery: Default::default(),
            pending_rejoins: RecoveryPolicy::ConfiguredAutoJoin,
            pending_parts: Vec::new(),
            pending_cycles: std::collections::HashSet::new(),
            update_selected: None,
            update_join_targets: Vec::new(),
            connection: ConnectionLifecycle::default(),
            connection_status: crate::irc::client::ConnectionStatus::default(),
            my_nick: nickname.clone(),

            server_host: settings.server_host,
            server_port: settings.server_port,
            use_tls: settings.use_tls,
            accept_invalid_certs: settings.accept_invalid_certs,
            nickname,
            username: settings.username,
            realname: settings.realname,
            password: settings.password,
            active_session: None,
            connection_error: None,

            conversations: ConversationStore::default(),
            current_conversation: None,
            server_messages: VecDeque::new(),
            server_unread: 0,

            input_text: String::new(),
            composer: message_ui::ComposerState::default(),
            timeline: Default::default(),
            message_ui: Default::default(),
            attention: settings.attention.clone(),
            view_is_active: false,
            reading_conversation: None,
            join_channel: String::new(),

            id: 0,
            profile_id: None,
            form_profile: None,
            blocked_endpoints: Vec::new(),
            blocked_networks: Vec::new(),
            input_focus_request: false,
            input_had_focus: false,
            events: Vec::new(),

            scroll_to_bottom: true,
            show_connect_dialog: true,

            command_history: Vec::new(),
            history_index: None,
            history_temp: String::new(),

            channel_list: Vec::new(),
            channel_list_loading: false,
            channel_list_min_users: 0,
            show_channel_list: false,
            channel_list_filter: String::new(),
            channel_list_selected: None,
            channel_list_sort: ChannelListSort::Users,
            channel_list_sort_dir: SortDirection::Descending,

            channel_list_cache: Vec::new(),
            channel_list_dirty: false,
            channel_list_cache_filter: String::new(),
            channel_list_cache_sort: (ChannelListSort::Users, SortDirection::Descending),
            channel_list_last_refilter: None,

            tab_completion: None,

            selected_user: None,

            auto_join_channels: settings.auto_join_channels,
            set_invisible: settings.set_invisible,
            auto_reconnect: settings.auto_reconnect,

            window_focused: true,

            // Cached lowercase versions (computed before moving owned values)
            my_nick_lower,
            ignore_list_lower: settings
                .ignore_list
                .iter()
                .map(|s| network_support.canonicalize(s))
                .collect(),
            highlight_words_lower: settings
                .highlight_words
                .split(',')
                .map(|s| network_support.canonicalize(s.trim()))
                .filter(|s| !s.is_empty())
                .collect(),

            ignore_list: settings.ignore_list,
            auto_perform: settings.auto_perform,
            pre_away_message: settings.pre_away_message,
            persistence_profile: settings.persistence_profile,
            pending_auto_perform: None,

            pending_channel_keys: HashMap::new(),
            join_focus_requests: Vec::new(),
            pending_invites: Vec::new(),

            // Away status
            away_status: None,
            last_activity: std::time::Instant::now(),
            auto_away_triggered: false,
            auto_away_enabled: settings.auto_away_enabled,
            // Values from settings.json are clamped to the same floors the
            // settings dialog enforces: an out-of-range persisted value (hand
            // edit / older version) must not disable a safety floor (see #144:
            // 0 would empty the server buffer, storm reconnects, or flap away).
            auto_away_minutes: settings.auto_away_minutes.max(1),
            auto_away_message: settings.auto_away_message,
            sasl_username: settings.sasl_username,
            sasl_password: settings.sasl_password,
            sasl_mechanism: settings.sasl_mechanism,
            sasl_required: settings.sasl_required,
            bouncer: settings.bouncer,
            bouncer_networks: std::collections::BTreeMap::new(),
            bouncer_ready: false,
            bouncer_retired_targets: Default::default(),
            tls_client_cert_path: settings.tls_client_cert_path,
            tls_client_key_path: settings.tls_client_key_path,
            log_manager: logging::LogManager::new(settings.logging_enabled),
            logging_enabled: settings.logging_enabled,
            logging_load_history: settings.logging_load_history,
            logging_history_lines: settings.logging_history_lines.max(10),

            // Display settings
            timestamp_format: settings.timestamp_format,
            hide_join_part: settings.hide_join_part,
            max_scrollback: settings.max_scrollback.max(100),

            // Privacy
            ctcp_replies_enabled: settings.ctcp_replies_enabled,

            // Highlights
            highlight_words: settings.highlight_words.clone(),
            network_support,
            enabled_caps: std::collections::HashSet::new(),
            chathistory_enabled: false,
            chathistory_limit: None,
            chathistory_batches: HashMap::new(),
            batch_parents: HashMap::new(),
            batch_labels: HashMap::new(),
            multiline_batches: HashMap::new(),
            authtoken_batches: HashMap::new(),
            pending_history_requests: HashMap::new(),
            next_history_request: 1,
            known_users: HashMap::new(),
            monitored_users: std::collections::HashSet::new(),
            pending_whox: VecDeque::new(),
            next_whox_token: 1,
            ctcp_reply_limiter: CtcpReplyLimiter::default(),

            // Reconnect settings
            reconnect_delay_secs: settings.reconnect_delay_secs.max(1),
            max_reconnect_attempts: settings.max_reconnect_attempts.max(1),

            // Custom messages
            quit_message: settings.quit_message,
            part_message: settings.part_message,

            // Channel list
            list_min_users: settings.list_min_users,

            show_channel_info: false,
            channel_info_target: None,

            // Lag meter
            lag_ms: None,
            ping_sent_time: None,
            last_lag_check: std::time::Instant::now(),

            // Color picker
            show_color_picker: false,
            color_picker_fg: true,

            // CPU optimization
            had_messages_this_frame: false,
            nick_retry_attempts: 0,
            past_nicks: Vec::new(),
            nick_confirmed: false,
            self_nick_keys,
            tab_order_cache: std::cell::RefCell::new(None),
        }
    }

    #[cfg(test)]
    /// This session's view of the settings: its connect form and its copy of
    /// the behaviour settings. App-only settings (favorites, tray, updates,
    /// font) keep their defaults; the App owns those.
    fn get_settings(&self) -> Settings {
        let mut settings = Settings::default();
        self.export_form(&mut settings);
        self.export_config(&mut settings);
        settings
    }

    /// Write this session's connect form into `settings`.
    pub(crate) fn export_form(&self, settings: &mut Settings) {
        settings.upload_settings = self.upload_settings.clone();
        settings.server_host = self.server_host.clone();
        settings.server_port = self.server_port.clone();
        settings.use_tls = self.use_tls;
        settings.accept_invalid_certs = self.accept_invalid_certs;
        settings.nickname = self.nickname.clone();
        settings.username = self.username.clone();
        settings.realname = self.realname.clone();
        settings.password = self.password.clone();
        settings.auto_join_channels = self.auto_join_channels.clone();
        settings.set_invisible = self.set_invisible;
        settings.auto_perform = self.auto_perform.clone();
        settings.pre_away_message = self.pre_away_message.clone();
        settings.persistence_profile = self.persistence_profile.clone();
        settings.sasl_username = self.sasl_username.clone();
        settings.sasl_password = self.sasl_password.clone();
        settings.sasl_mechanism = self.sasl_mechanism;
        settings.sasl_required = self.sasl_required;
        settings.bouncer = self.bouncer.clone();
        settings.tls_client_cert_path = self.tls_client_cert_path.clone();
        settings.tls_client_key_path = self.tls_client_key_path.clone();
    }

    /// Write this session's copy of the behaviour settings into `settings`.
    pub(crate) fn export_config(&self, settings: &mut Settings) {
        settings.attention = self.attention.clone();
        settings.auto_reconnect = self.auto_reconnect;
        settings.ignore_list = self.ignore_list.clone();
        settings.auto_away_enabled = self.auto_away_enabled;
        settings.auto_away_minutes = self.auto_away_minutes;
        settings.auto_away_message = self.auto_away_message.clone();
        settings.logging_enabled = self.logging_enabled;
        settings.logging_load_history = self.logging_load_history;
        settings.logging_history_lines = self.logging_history_lines;
        settings.timestamp_format = self.timestamp_format.clone();
        settings.hide_join_part = self.hide_join_part;
        settings.max_scrollback = self.max_scrollback;
        settings.ctcp_replies_enabled = self.ctcp_replies_enabled;
        settings.highlight_words = self.highlight_words.clone();
        settings.reconnect_delay_secs = self.reconnect_delay_secs;
        settings.max_reconnect_attempts = self.max_reconnect_attempts;
        settings.quit_message = self.quit_message.clone();
        settings.part_message = self.part_message.clone();
        settings.list_min_users = self.list_min_users;
    }

    /// Mirror the App's behaviour settings into this session, with the same
    /// floors as loading them (#144).
    pub(crate) fn apply_config(&mut self, settings: &Settings) {
        self.attention = settings.attention.clone();
        self.auto_reconnect = settings.auto_reconnect;
        self.ignore_list = settings.ignore_list.clone();
        self.auto_away_enabled = settings.auto_away_enabled;
        self.auto_away_minutes = settings.auto_away_minutes.max(1);
        self.auto_away_message = settings.auto_away_message.clone();
        self.logging_enabled = settings.logging_enabled;
        self.logging_load_history = settings.logging_load_history;
        self.logging_history_lines = settings.logging_history_lines.max(10);
        self.timestamp_format = settings.timestamp_format.clone();
        self.hide_join_part = settings.hide_join_part;
        self.max_scrollback = settings.max_scrollback.max(100);
        self.ctcp_replies_enabled = settings.ctcp_replies_enabled;
        self.highlight_words = settings.highlight_words.clone();
        self.reconnect_delay_secs = settings.reconnect_delay_secs.max(1);
        self.max_reconnect_attempts = settings.max_reconnect_attempts.max(1);
        self.quit_message = settings.quit_message.clone();
        self.part_message = settings.part_message.clone();
        self.list_min_users = settings.list_min_users;
        self.update_cached_lowercase();
    }

    /// Check if a sender is ignored (by nick or mask)
    /// Uses cached ignore_list_lower to avoid allocation per message
    fn is_ignored(&self, sender: &str, prefix: Option<&str>) -> bool {
        let sender_key = self.network_support.canonicalize(sender);
        for (pattern, pattern_lower) in self.ignore_list.iter().zip(self.ignore_list_lower.iter()) {
            if pattern_lower == &sender_key {
                return true;
            }

            let has_wildcard = pattern.contains('*') || pattern.contains('?');
            if has_wildcard && !pattern.contains('!') && !pattern.contains('@') {
                if mask_matches(pattern_lower, &sender_key) {
                    return true;
                }
            } else if (has_wildcard || pattern.contains('!') || pattern.contains('@'))
                && let Some(full_prefix) = prefix
                // RFC casemapping applies to nicknames, while user/host masks
                // are conventionally ASCII case-insensitive. Canonicalizing the
                // whole mask still provides both properties without allocation
                // inside the matcher.
                && mask_matches(
                    pattern_lower,
                    &self.network_support.canonicalize(full_prefix),
                )
            {
                return true;
            }
        }
        false
    }

    /// Exponential reconnect delay, preserving the configured base and 120s cap.
    pub fn get_reconnect_delay(&self) -> std::time::Duration {
        let delay = u64::from(self.reconnect_delay_secs)
            .saturating_mul(2u64.saturating_pow(self.connection.attempts().min(6)));
        std::time::Duration::from_secs(delay.min(120))
    }

    pub fn should_reconnect(&self) -> bool {
        self.awaiting_reconnect()
            && self
                .connection
                .failed_at()
                .is_none_or(|failed| failed.elapsed() >= self.get_reconnect_delay())
    }

    pub fn awaiting_reconnect(&self) -> bool {
        self.auto_reconnect
            && self.connection.connection_lost()
            && self.connection.attempts() < self.max_reconnect_attempts
    }

    pub fn mark_connection_lost(&mut self) {
        if self.connection.is_closing() {
            return;
        }
        self.connection_status = Default::default();
        self.message_ui = Default::default();
        self.pump(None);
        self.mark_pending_sends_uncertain();
        self.capture_rejoin_memberships();
        self.connection
            .enter_retry_wait(Some(std::time::Instant::now()));
        self.lag_ms = None;
        self.ping_sent_time = None;
        self.mark_channel_tabs_unjoined();
    }

    pub fn start_reconnect(&mut self) {
        let attempts = self.connection.attempts().saturating_add(1);
        self.nick_retry_attempts = 0;
        self.queue_current_connection(attempts);
        let delay = self.get_reconnect_delay();
        self.add_server_message(ChatMessage::system(&format!(
            "Reconnecting... (attempt {attempts}, next retry in {delay:?})"
        )));
    }

    #[cfg(test)]
    /// Minimal update when window is minimized - drains messages without rendering UI
    /// This prevents the message buffer from filling up while keeping CPU usage near zero
    pub fn update_minimal(&mut self) {
        // Only the minimized/tray-hidden path runs this, and no frame refreshes
        // the focus flag meanwhile: a stale `true` would suppress highlight,
        // kick and invite notifications and mark unseen messages read (#237).
        self.set_window_focused(false);
        self.pump(None);
        self.run_timed_tasks();
    }

    /// Drain this session's incoming messages: at most `limit` (plus one)
    /// per call so a flood cannot stall a frame, or all of them. Returns
    /// whether more were left queued. Logged lines are flushed once per call
    /// instead of once per message.
    pub(crate) fn pump(&mut self, limit: Option<usize>) -> bool {
        self.drain_connection_events();
        let (generation, messages, has_more) = self.connection.take_messages(limit);
        self.had_messages_this_frame = !messages.is_empty();
        for msg in messages {
            if !generation.is_some_and(|generation| self.connection.can_dispatch(generation)) {
                break;
            }
            self.handle_incoming_message(msg);
        }
        self.log_manager.flush_all();
        has_more
    }

    /// The user did something in one of our windows: restart the idle
    /// clock and undo an automatic away.
    pub(crate) fn note_user_activity(&mut self, now: std::time::Instant) {
        self.last_activity = now;
        if self.auto_away_triggered && self.away_status.is_some() {
            self.send_command(IrcCommand::Away(None));
            self.away_status = None;
            self.auto_away_triggered = false;
            self.add_server_message(ChatMessage::system(
                "You are no longer away (auto-detected activity)",
            ));
        }
    }

    /// Record whether the window has focus. Regaining it is the moment the
    /// selected tab is actually seen, so whatever it accrued while the window
    /// was minimised, hidden or in the background is acknowledged then (#305).
    pub(crate) fn set_window_focused(&mut self, focused: bool) {
        let regained = focused && !self.window_focused;
        self.window_focused = focused;
        if regained {
            self.mark_selected_tab_seen();
        }
    }

    /// The user is looking at the selected tab (or the server buffer): clear
    /// its unread badge and report it read.
    fn mark_selected_tab_seen(&mut self) {
        if self.reading_conversation != self.current_conversation
            || self.timeline.borrow().restore_pending
            || self
                .timeline
                .borrow()
                .anchor
                .as_ref()
                .is_some_and(|anchor| !anchor.at_bottom)
        {
            return;
        }
        match self.current_target_name().map(str::to_owned) {
            Some(target) => {
                if let Some(channel) = self.channel_mut(&target) {
                    channel.unread = 0;
                }
                self.mark_target_read(&target);
            }
            None => self.server_unread = 0,
        }
    }

    /// Time-based work that must keep running whether or not a frame is
    /// drawn: queued auto-perform commands, the auto-away idle check and the
    /// lag ping. Called from `ui()` and from `update_minimal()`, which is all
    /// that runs while minimized or hidden to the tray (#237).
    pub(crate) fn run_timed_tasks(&mut self) {
        self.expire_upload_token();
        self.expire_typing();
        self.run_macro_jobs(std::time::Instant::now());
        // Serialize login commands until the preceding attempt is answered.
        // Only a registered connection runs them: an unfinished queue belongs
        // to the connection that produced it and is cleared with its state.
        if self.is_connected()
            && let Some(mut commands) = self.pending_auto_perform.take()
        {
            if let Some(command) = commands.first().cloned()
                && self.auto_perform_can_run(&command)
            {
                commands.remove(0);
                self.auto_perform_dequeued(&command);
                let command = command.trim();
                let command = if command.starts_with('/') {
                    command.to_owned()
                } else {
                    format!("/{command}")
                };
                self.process_builtin_command(&command);
            }
            if !commands.is_empty() {
                self.pending_auto_perform = Some(commands);
            }
        }
        self.flush_authenticated_history();

        // Check for idle timeout and set auto-away
        if self.is_connected() && self.away_status.is_none() && self.auto_away_enabled {
            let idle_duration = self.last_activity.elapsed();
            let timeout = std::time::Duration::from_secs(self.auto_away_minutes as u64 * 60);
            if idle_duration >= timeout {
                let msg = self.auto_away_message.clone();
                self.send_command(IrcCommand::Away(Some(msg.clone())));
                self.away_status = Some(msg.clone());
                self.auto_away_triggered = true;
                self.add_server_message(ChatMessage::system(&format!("Auto-away: {}", msg)));
            }
        }

        // Lag meter - send PING every 30 seconds when connected
        if self.is_connected() && self.last_lag_check.elapsed().as_secs() >= 30 {
            self.last_lag_check = std::time::Instant::now();
            self.ping_sent_time = Some(std::time::Instant::now());
            self.send_command(IrcCommand::Ping("LAG".to_string()));
        }
    }

    /// Desktop notification for an incoming chat message: PMs always notify
    /// (force=true), highlights only while the window is unfocused. Shared by
    /// live PRIVMSGs and completed multiline batches so both deliver alike
    /// (#289). `preview` is the already-truncated message text.
    fn notify_incoming_message(
        &mut self,
        sender: &str,
        target_name: &str,
        preview: &str,
        is_pm: bool,
        is_highlight: bool,
        message: &ChatMessage,
    ) {
        if message.msgid.as_ref().is_some_and(|id| {
            self.conversations.get(target_name).is_some_and(|channel| {
                channel
                    .messages
                    .iter()
                    .any(|message| message.msgid.as_ref() == Some(id))
            })
        }) {
            return;
        }
        if !is_pm
            && !is_highlight
            && !self
                .attention
                .rule(&self.conversation_key(target_name))
                .is_some_and(|rule| rule.level == attention::AlertLevel::All)
        {
            return;
        }
        // Skip if we sent it ourselves. Current nick only, not held nicks: a
        // peer squatting a nick we vacated must not suppress the user's
        // notifications (#168) - a racing echo under the old nick costs at
        // worst a duplicate row.
        if self.identifiers_equal(sender, &self.my_nick) {
            return;
        }
        let title = if is_pm {
            format!("PM from {sender}")
        } else if is_highlight {
            format!("{sender} mentioned you in {target_name}")
        } else {
            format!("{sender} in {target_name}")
        };
        self.events.push(SessionEvent::Notify {
            title,
            body: preview.into(),
            force: is_pm,
            attention: Some(attention::AttentionEvent {
                reference: self.live_message_reference(target_name, message),
                private: is_pm,
                mention: is_highlight,
            }),
        });
    }

    /// Ask the App for a desktop notification; it decides from focus, the
    /// setting and its rate limit (`App::notify`).
    pub fn send_notification(&mut self, title: &str, body: &str, force: bool) {
        self.events.push(SessionEvent::Notify {
            title: title.to_string(),
            body: body.to_string(),
            force,
            attention: None,
        });
    }

    /// Ask the App to persist the auto-perform list on its own, not the
    /// rest of the connect form (#316).
    fn save_auto_perform(&mut self) {
        self.events.push(SessionEvent::SaveAutoPerform);
    }

    /// Ask the App to persist this session's behaviour settings (ignore
    /// list, channel-list minimum) and the favorites, but never the connect
    /// form: its fields may be unsaved scratch or invalid (#316).
    pub(crate) fn save_config(&mut self) {
        self.update_cached_lowercase();
        self.events.push(SessionEvent::SaveConfig);
    }

    pub(crate) fn update_changes_endpoint(&self) -> bool {
        self.connection.staged_config().is_some_and(|pending| {
            self.active_session
                .as_ref()
                .is_none_or(|active| active.logical_network_key() != pending.logical_network_key())
        })
    }

    pub fn update_session(&self) -> UpdateSession {
        let staged = self.connection.staged_config();
        let changes_endpoint = self.update_changes_endpoint();
        let changes_identity = staged.is_some_and(|pending| {
            self.active_session.as_ref().is_some_and(|active| {
                active.services_identity_key() != pending.services_identity_key()
            })
        });
        let mut session = staged.cloned().or_else(|| self.active_session.clone());
        if staged.is_none()
            && let Some(session) = &mut session
        {
            session.server.nick = self.my_nick.clone();
        }
        let mut tabs: Vec<_> = self
            .conversations
            .iter()
            .map(|(name, channel)| {
                let pending = self.pending_rejoins.as_ref().and_then(|rejoins| {
                    rejoins
                        .iter()
                        .find(|(target, _)| self.identifiers_equal(target, name))
                });
                let key = if channel.joined {
                    channel.key.clone()
                } else {
                    pending.map_or_else(|| channel.key.clone(), |(_, key)| key.clone())
                };
                (
                    name.clone(),
                    key,
                    !self.is_parting(name) && (channel.joined || pending.is_some()),
                )
            })
            .collect();
        if let Some(rejoins) = self.pending_rejoins.as_ref() {
            for (name, key) in rejoins.iter().filter(|(name, _)| !self.is_parting(name)) {
                if !tabs
                    .iter()
                    .any(|(target, _, _)| self.identifiers_equal(target, name))
                {
                    tabs.push((name.clone(), key.clone(), true));
                }
            }
        }
        if changes_endpoint {
            tabs.clear();
        }
        UpdateSession {
            session,
            reconnect: self.is_connected()
                || self.has_pending_connect()
                || self.awaiting_reconnect(),
            fresh_connect: changes_endpoint
                || (!self.is_connected()
                    && self.pending_rejoins.is_none()
                    && self.conversations.values().all(|channel| !channel.joined)),
            tabs,
            selected: (!changes_endpoint)
                .then(|| self.current_target_name().map(str::to_owned))
                .flatten(),
            input: if changes_endpoint || self.draft_is_sensitive(&self.input_text) {
                String::new()
            } else {
                self.input_text.clone()
            },
            drafts: None,
            away: if changes_endpoint || changes_identity {
                None
            } else {
                self.away_status.clone()
            },
            others: Vec::new(),
            active_position: 0,
        }
    }

    pub fn restore_update_session(&mut self, snapshot: UpdateSession) {
        if let Some(session) = snapshot.session {
            self.activate_session(session);
        }
        let mut rejoins = Vec::new();
        for (name, key, joined) in snapshot.tabs {
            let mut channel = Channel::new();
            channel.key = key.clone();
            channel.joined = false;
            // Membership is authoritative even before the server advertises
            // its channel prefixes again on the restored connection.
            if joined {
                rejoins.push((name.clone(), key));
            }
            if self.logging_load_history
                && let Some((network, legacy, allow_legacy)) = self.active_log_context()
            {
                channel.messages.extend(self.load_text_history(
                    &network,
                    &legacy,
                    &self.network_support.canonicalize(&name),
                    &name,
                    self.logging_history_lines,
                    allow_legacy,
                ));
                channel.history_loaded = !channel.messages.is_empty();
            }
            self.conversations.insert(name, channel);
        }
        self.update_join_targets = rejoins.iter().map(|(name, _)| name.clone()).collect();
        self.pending_rejoins = if snapshot.fresh_connect || self.membership_managed_by_bouncer() {
            RecoveryPolicy::ConfiguredAutoJoin
        } else {
            RecoveryPolicy::ExactMemberships(rejoins)
        };
        self.current_conversation = snapshot
            .selected
            .as_deref()
            .and_then(|target| self.id_for_target(target));
        self.update_selected = snapshot.selected;
        self.input_text = snapshot.input;
        self.away_status = snapshot.away;
        if snapshot.reconnect && self.active_session.is_some() {
            self.queue_current_connection(0);
        }
        self.show_connect_dialog = false;
    }

    pub fn prepare_update_shutdown(&mut self) {
        self.log_manager.flush_all();
        self.close_connection(Some(self.quit_message.clone()), false);
    }

    /// Update cached lowercase versions of strings for efficient comparison
    fn update_cached_lowercase(&mut self) {
        self.highlight_words_lower = self
            .highlight_words
            .split(',')
            .map(|s| self.network_support.canonicalize(s.trim()))
            .filter(|s| !s.is_empty())
            .collect();
        self.ignore_list_lower = self
            .ignore_list
            .iter()
            .map(|s| self.network_support.canonicalize(s))
            .collect();
    }

    /// The single nick-change choke point. A nick the server confirmed on
    /// this connection lineage is remembered when replaced, so replay rows
    /// sent under it stay attributed to us; recording derives from
    /// nick_confirmed, so call sites pass only whether the INCOMING nick is
    /// server-accepted on this path (true for 001 and accepted NICK echoes;
    /// false for activation and 433 walkaways, where confirmation waits for
    /// this connection's own 001 — #169, recording derivation #171).
    /// Drop held-nick attribution for the current lineage. Order-coupled:
    /// nick_confirmed clears with the list, or set_session_nick's record
    /// guard re-inserts the outgoing nick (#172).
    fn clear_nick_lineage(&mut self) {
        self.past_nicks.clear();
        self.nick_confirmed = false;
        self.refresh_self_nick_keys();
    }

    fn set_session_nick(&mut self, nick: String, accepted: bool) {
        // Re-applying the server-confirmed nick itself preserves its
        // confirmation: a manual same-endpoint reconnect (which passes
        // accepted=false — this connection's own 001 has not happened yet)
        // must not lose held-nick attribution (#169/#170).
        let preserved = self.nick_confirmed && self.identifiers_equal(&self.my_nick, &nick);
        if self.nick_confirmed
            && !preserved
            && !self.my_nick.is_empty()
            && !self.identifiers_equal(&self.my_nick, &nick)
        {
            let already_held = self
                .past_nicks
                .iter()
                .any(|held| self.identifiers_equal(held, &self.my_nick));
            if !already_held {
                self.past_nicks.insert(0, self.my_nick.clone());
            }
            self.past_nicks.truncate(32);
        }
        self.my_nick = nick;
        self.my_nick_lower = self.network_support.canonicalize(&self.my_nick);
        self.nick_confirmed = accepted || preserved;
        self.refresh_self_nick_keys();
    }

    /// Canonicalized keys for every nick our own rows may carry (current
    /// plus held), so sender_is_self per row costs one canonicalize of the
    /// incoming nick instead of one per held nick (#167).
    fn refresh_self_nick_keys(&mut self) {
        let mut keys = Vec::with_capacity(self.past_nicks.len() + 1);
        keys.push(self.network_support.canonicalize(&self.my_nick));
        for held in &self.past_nicks {
            let key = self.network_support.canonicalize(held);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        self.self_nick_keys = keys;
    }

    /// Whether a row's sender is us — the current nick or one we held
    /// earlier this session (#163). Over-matching held nicks is the safe
    /// direction here: this gate feeds credential redaction and replay
    /// drops, where a false positive only over-redacts. Lossy or
    /// suppressive gates (echo merge, notification and echo suppression)
    /// deliberately compare the current nick exactly.
    fn sender_is_self(&self, nick: &str) -> bool {
        let key = self.network_support.canonicalize(nick);
        self.self_nick_keys.contains(&key)
    }

    pub fn is_channel_name(&self, name: &str) -> bool {
        self.network_support.is_channel(name)
    }

    fn strip_status_prefix<'a>(&self, target: &'a str) -> &'a str {
        let mut candidates = Vec::new();
        for (index, character) in target.char_indices() {
            if !self.network_support.status_prefixes.contains(character) {
                break;
            }
            candidates.push(index + character.len_utf8());
        }
        for end in candidates.into_iter().rev() {
            let rest = &target[end..];
            if self.is_channel_name(rest) {
                return rest;
            }
        }
        target
    }

    pub fn normalize_channel_name(&self, name: &str) -> String {
        if self.is_channel_name(name) {
            name.to_string()
        } else {
            format!("#{}", name)
        }
    }

    fn identifiers_equal(&self, left: &str, right: &str) -> bool {
        self.network_support.identifiers_equal(left, right)
    }

    /// Return the display-preserving key already used for an IRC target.
    /// `HashMap<String, _>` itself is byte-sensitive, so every lookup must pass
    /// through negotiated CASEMAPPING to avoid duplicate channel/query tabs.
    pub(crate) fn id_for_target(&self, name: &str) -> Option<ConversationId> {
        self.conversations
            .lookup(name)
            .filter(|id| {
                self.target_name(*id)
                    .is_some_and(|target| self.identifiers_equal(target, name))
            })
            .or_else(|| {
                self.conversations.ids().find(|id| {
                    self.target_name(*id)
                        .is_some_and(|target| self.identifiers_equal(target, name))
                })
            })
    }

    pub(crate) fn target_name(&self, id: ConversationId) -> Option<&str> {
        self.conversations.target(id)
    }

    pub(crate) fn conversation_channel(&self, id: ConversationId) -> Option<&Channel> {
        self.conversations.channel(id)
    }

    pub(crate) fn conversation_channel_mut(&mut self, id: ConversationId) -> Option<&mut Channel> {
        self.conversations.channel_mut(id)
    }

    pub(crate) fn conversation_ids(&self) -> Vec<ConversationId> {
        self.conversations.ids().collect()
    }

    pub(crate) fn current_target_name(&self) -> Option<&str> {
        self.current_conversation
            .and_then(|id| self.target_name(id))
    }

    pub(crate) fn current_target(&self) -> Option<&str> {
        self.current_target_name()
    }

    #[cfg(test)]
    pub(crate) fn ensure_target(&mut self, name: &str) -> ConversationId {
        if let Some(id) = self.id_for_target(name) {
            return id;
        }
        let mut channel = Channel::new();
        channel.set_case_mapping(self.network_support.case_mapping);
        channel.set_prefix_schema(
            &self.network_support.prefix_modes,
            &self.network_support.user_prefixes,
        );
        self.conversations.insert(name.to_owned(), channel);
        self.id_for_target(name).unwrap()
    }

    pub(crate) fn select_target(&mut self, name: &str) -> bool {
        let Some(id) = self.id_for_target(name) else {
            return false;
        };
        self.current_conversation = Some(id);
        true
    }

    pub(crate) fn select_conversation(&mut self, id: Option<ConversationId>) -> bool {
        if id.is_some_and(|id| self.target_name(id).is_none()) {
            return false;
        }
        self.current_conversation = id;
        true
    }

    fn channel_key(&self, name: &str) -> Option<String> {
        self.id_for_target(name)
            .and_then(|id| self.target_name(id))
            .map(str::to_owned)
    }

    fn channel_mut(&mut self, name: &str) -> Option<&mut Channel> {
        let key = self.channel_key(name)?;
        self.conversations.get_mut(&key)
    }

    fn remove_channel(&mut self, name: &str) -> Option<Channel> {
        self.forget_rejoin_membership(name);
        let key = self.channel_key(name)?;
        let id = self.id_for_target(&key)?;
        self.retire_history_owner(id);
        let removed = self.conversations.remove(&key);
        self.pending_cycles.remove(&id);
        self.forget_automatic_history(id);
        if self.current_conversation == Some(id) {
            self.current_conversation = self.conversations.ids().next();
        }
        if self.channel_info_target == Some(id) {
            self.channel_info_target = None;
            self.show_channel_info = false;
        }
        self.prune_known_users();
        removed
    }

    /// Drop identities that nothing references any more: users who share no
    /// channel with us, have no query tab, and are not monitored. Run when we
    /// stop sharing a channel (own PART, kick, tab close) so a long session
    /// does not keep every nick from every channel it ever joined (#257).
    fn prune_known_users(&mut self) {
        let mut referenced = self.monitored_users.clone();
        referenced.insert(self.network_support.canonicalize(&self.my_nick));
        for (name, channel) in &self.conversations {
            // A query tab is named after its partner.
            referenced.insert(self.network_support.canonicalize(name));
            referenced.extend(
                channel
                    .users
                    .iter()
                    .map(|user| self.network_support.canonicalize(&user.nick)),
            );
        }
        self.known_users.retain(|key, _| referenced.contains(key));
    }

    /// Open a query through the same initialization path used by incoming
    /// private messages: casemapped reuse, query-window cap, history loading,
    /// and session markers all stay consistent across UI entry points.
    pub(crate) fn open_query(&mut self, target: &str) -> bool {
        if target.is_empty() || self.is_channel_name(target) {
            return false;
        }
        if let Some(existing) = self.channel_key(target) {
            self.select_target(&existing);
            return true;
        }

        self.add_message_to_channel(
            target,
            ChatMessage::system_fmt(
                &format!("Conversation with {}", target),
                &self.timestamp_format,
            ),
        );
        if let Some(created) = self.channel_key(target) {
            self.select_target(&created);
            true
        } else {
            false
        }
    }

    fn current_target_is(&self, target: &str) -> bool {
        self.current_target_name()
            .is_some_and(|current| self.identifiers_equal(current, target))
    }

    fn mark_target_read(&mut self, target: &str) {
        if !self.is_connected() || !self.enabled_caps.contains("draft/read-marker") {
            return;
        }
        let Some(key) = self.channel_key(target) else {
            return;
        };
        let timestamp = self.conversations.get(&key).and_then(|channel| {
            channel
                .messages
                .iter()
                .filter_map(|message| message.server_time.as_ref())
                .max()
                .cloned()
        });
        let Some(timestamp) = timestamp else {
            return;
        };
        // A server marker can be newer than the rows retained locally (for
        // example after loading an older page or redacting the newest row).
        // Automatic marking must never move that known read position back.
        if self.conversations[&key]
            .read_marker
            .as_deref()
            .and_then(|marker| marker.strip_prefix("timestamp="))
            .is_some_and(|previous| previous >= timestamp.as_str())
        {
            return;
        }
        let marker = format!("timestamp={timestamp}");
        if self.send_command(IrcCommand::Markread(key.clone(), Some(marker.clone())))
            && let Some(channel) = self.conversations.get_mut(&key)
        {
            channel.read_marker = Some(marker);
        }
    }

    /// Remember actual memberships before a registered transport is retired.
    /// Preserve unconfirmed recovery JOINs when another retry fails early.
    fn capture_rejoin_memberships(&mut self) {
        if self.membership_managed_by_bouncer() {
            self.pending_rejoins = RecoveryPolicy::ConfiguredAutoJoin;
            return;
        }
        if !self.is_connected()
            && self.pending_rejoins.is_none()
            && !self.conversations.values().any(|channel| channel.joined)
        {
            return;
        }
        let mut rejoins = self.pending_rejoins.take().unwrap_or_default();
        // Automatic JOINs still deferred behind a services login were never
        // sent, so no membership intent exists for them yet. They are the
        // lineage's intent: an exact empty list here would suppress them on
        // every later reconnect.
        for (name, key) in self.deferred_automatic_join_targets() {
            if self.is_parting(&name) {
                continue;
            }
            if let Some(existing) = rejoins
                .iter_mut()
                .find(|(target, _)| self.network_support.identifiers_equal(target, &name))
            {
                *existing = (name, key);
            } else {
                rejoins.push((name, key));
            }
        }
        for (name, channel) in &self.conversations {
            if !channel.joined || self.is_parting(name) {
                continue;
            }
            if let Some(existing) = rejoins
                .iter_mut()
                .find(|(target, _)| self.network_support.identifiers_equal(target, name))
            {
                existing.clone_from(&(name.clone(), channel.key.clone()));
            } else {
                rejoins.push((name.clone(), channel.key.clone()));
            }
        }
        self.pending_rejoins = RecoveryPolicy::ExactMemberships(rejoins);
    }

    fn forget_rejoin_membership(&mut self, name: &str) {
        if let Some(rejoins) = self.pending_rejoins.as_mut() {
            let support = &self.network_support;
            rejoins.retain(|(target, _)| !support.identifiers_equal(target, name));
        }
    }

    fn is_parting(&self, name: &str) -> bool {
        self.pending_parts
            .iter()
            .any(|target| self.identifiers_equal(target, name))
    }

    fn forget_part_intent(&mut self, name: &str) {
        let support = &self.network_support;
        self.pending_parts
            .retain(|target| !support.identifiers_equal(target, name));
    }

    fn mark_channel_tabs_unjoined(&mut self) {
        for channel in self.conversations.values_mut() {
            channel.joined = false;
            channel.abandon_user_burst();
            channel.abandon_mode_list_bursts();
        }
        self.channel_list_loading = false;
    }

    fn can_send_to_target(&self, target: &str) -> bool {
        if !self.is_channel_name(target) {
            return true;
        }
        self.channel_key(target)
            .and_then(|key| self.conversations.get(&key))
            // An explicit target without an open tab may be a channel that
            // permits external messages. Only a retained, known-stale tab is
            // blocked locally.
            .is_none_or(|channel| channel.joined)
    }

    fn refresh_case_mapping_state(&mut self) {
        let mapping = self.network_support.case_mapping;
        self.conversations
            .provisional_support(&self.network_support);
        for channel in self.conversations.values_mut() {
            channel.set_case_mapping(mapping);
        }
        for (survivor, retired, channel) in self.conversations.set_mapping(mapping) {
            self.merge_conversation_payload(survivor, channel);
            self.redirect_conversation(survivor, retired);
        }
        self.my_nick_lower = self.network_support.canonicalize(&self.my_nick);
        self.refresh_self_nick_keys();
        self.update_cached_lowercase();

        // Pending JOIN keys are internal lookup state, so normalize them rather
        // than keeping the spelling used before CASEMAPPING arrived.
        self.pending_channel_keys = self
            .pending_channel_keys
            .drain()
            .map(|(channel, key)| (self.network_support.canonicalize(&channel), key))
            .collect();
    }

    fn reset_connection_support(&mut self) {
        self.network_support = NetworkSupport::default();
        self.conversations
            .provisional_support(&self.network_support);
        // The ignore/highlight keys were folded under the old network's
        // CASEMAPPING; a new network that advertises the default never
        // triggers refresh_case_mapping_state, so rebuild them here (#249).
        self.update_cached_lowercase();
        self.reset_per_connection_state();
        self.known_users.clear();
        self.nick_retry_attempts = 0;
    }

    /// State that only has meaning on the socket that produced it: negotiated
    /// capabilities, unanswered history requests, half-received batches, the
    /// server-side MONITOR list and in-flight WHOX tokens. A reconnect to the
    /// same endpoint keeps the rest of the session (tabs, known nicks), but
    /// must not carry these over: an unanswered request would be paired with
    /// the next batch for that target, and leaked batches eventually stop new
    /// history from being collected (#248).
    fn reset_per_connection_state(&mut self) {
        self.bouncer_retired_targets.clear();
        self.upload_discovery = Default::default();
        self.mark_pending_sends_uncertain();
        self.message_ui = Default::default();
        self.macro_jobs.clear();
        self.reset_history_authentication();
        // Auto-perform lines still waiting behind a login belong to the old
        // socket; the next 001 rebuilds the queue from the configuration.
        self.pending_auto_perform = None;
        self.pending_cycles.clear();
        // Recovery membership was captured before retiring the old socket,
        // excluding its unconfirmed PARTs. Its acknowledgments cannot arrive
        // on this socket, and no retained tab is still marked joined.
        self.pending_parts.clear();
        self.enabled_caps.clear();
        self.chathistory_enabled = false;
        self.chathistory_limit = None;
        self.chathistory_batches.clear();
        self.batch_parents.clear();
        self.batch_labels.clear();
        self.multiline_batches.clear();
        self.authtoken_batches.clear();
        self.pending_history_requests.clear();
        self.next_history_request = 1;
        self.monitored_users.clear();
        self.pending_whox.clear();
        self.next_whox_token = 1;
        self.ctcp_reply_limiter = CtcpReplyLimiter::default();
        // A NAMES burst or mode-list reply the old socket never finished.
        for channel in self.conversations.values_mut() {
            channel.abandon_user_burst();
            channel.abandon_mode_list_bursts();
            // Only the transport that queued a local row can confirm it.
            // A new socket's identical send must acquire its own identity.
            for message in &mut channel.messages {
                message.pending_echo = false;
            }
        }
        // Keys of JOINs the old socket never confirmed: the new connection's
        // JOINs carry their own.
        self.pending_channel_keys.clear();
        self.join_focus_requests.clear();
    }

    /// Called when a connection worker is about to start, after the previous
    /// worker (if any) has fully exited.
    pub(crate) fn begin_connection_attempt(&mut self) {
        // The previous worker has exited, so any intent still recorded is
        // stale (e.g. a disconnect issued while no worker was left to consume
        // it) and must not be applied to this worker's exit (#247).
        self.reset_per_connection_state();
    }

    fn request_capabilities(&mut self, caps: Vec<String>) {
        const CAP_REQ_OVERHEAD: usize = "CAP REQ :".len();
        let mut chunk: Vec<String> = Vec::new();
        let mut chunk_len = CAP_REQ_OVERHEAD;
        for cap in caps {
            let added = usize::from(!chunk.is_empty()) + cap.len();
            if chunk_len + added > IRC_MAX_LINE_BYTES && !chunk.is_empty() {
                self.send_command(IrcCommand::Cap(
                    String::new(),
                    "REQ".to_string(),
                    vec![chunk.join(" ")],
                ));
                chunk.clear();
                chunk_len = CAP_REQ_OVERHEAD;
            }
            chunk_len += usize::from(!chunk.is_empty()) + cap.len();
            chunk.push(cap);
        }
        if !chunk.is_empty() {
            self.send_command(IrcCommand::Cap(
                String::new(),
                "REQ".to_string(),
                vec![chunk.join(" ")],
            ));
        }
    }

    fn handle_cap_message(&mut self, subcommand: &str, params: &[String]) {
        let Some(caps) = params.last() else {
            return;
        };
        let mut newly_available = Vec::new();
        for raw_token in caps.split_whitespace() {
            let removing = raw_token.starts_with('-');
            let token = raw_token.trim_start_matches(['-', '~', '=']);
            let (name, value) = token
                .split_once('=')
                .map_or((token, None), |(name, value)| (name, Some(value)));
            let canonical_name = name.to_ascii_lowercase();

            if subcommand.eq_ignore_ascii_case("NEW")
                && DESIRED_CAPS.contains(&canonical_name.as_str())
                && !self.enabled_caps.contains(&canonical_name)
            {
                newly_available.push(canonical_name.clone());
            }

            if subcommand.eq_ignore_ascii_case("ACK") {
                if removing {
                    self.enabled_caps.remove(&canonical_name);
                } else {
                    self.enabled_caps.insert(canonical_name.clone());
                }
            } else if subcommand.eq_ignore_ascii_case("DEL")
                || subcommand.eq_ignore_ascii_case("NAK")
            {
                self.enabled_caps.remove(&canonical_name);
            }

            if name.eq_ignore_ascii_case("draft/chathistory") {
                if let Some(value) = value {
                    self.chathistory_limit = value
                        .parse::<usize>()
                        .ok()
                        .filter(|limit| *limit > 0)
                        .or_else(|| {
                            value.split(',').find_map(|item| {
                                item.strip_prefix("limit=")
                                    .and_then(|limit| limit.parse::<usize>().ok())
                                    .filter(|limit| *limit > 0)
                            })
                        });
                }

                if subcommand.eq_ignore_ascii_case("ACK") {
                    self.chathistory_enabled = !removing;
                } else if subcommand.eq_ignore_ascii_case("DEL")
                    || subcommand.eq_ignore_ascii_case("NAK")
                {
                    self.chathistory_enabled = false;
                }
            } else if name.eq_ignore_ascii_case("draft/multiline")
                && let Some(value) = value
            {
                for item in value.split(',') {
                    if let Some(value) = item.strip_prefix("max-bytes=") {
                        self.network_support.multiline_max_bytes = value.parse().ok();
                    } else if let Some(value) = item.strip_prefix("max-lines=") {
                        self.network_support.multiline_max_lines = value.parse().ok();
                    }
                }
            }
        }
        if !newly_available.is_empty() {
            self.request_capabilities(newly_available);
        }
    }

    fn total_history_limit(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.logging_history_lines)
            .min(self.max_scrollback)
            .max(1)
    }

    fn history_page_limit(&self, remaining: usize) -> usize {
        remaining.min(self.chathistory_limit.unwrap_or(100)).max(1)
    }

    fn message_reference(message: &ChatMessage) -> Option<String> {
        message
            .msgid
            .as_ref()
            .map(|msgid| format!("msgid={msgid}"))
            .or_else(|| {
                message
                    .server_time
                    .as_ref()
                    .map(|time| format!("timestamp={time}"))
            })
    }

    fn same_history_row(left: &ChatMessage, right: &ChatMessage) -> bool {
        left.timestamp == right.timestamp
            && left.sender == right.sender
            && left.content == right.content
            && left.is_action == right.is_action
            && left.is_system == right.is_system
    }

    /// Drop catch-up rows that repeat a row the tab already shows without any
    /// message identity - rows restored from the local log (no msgid, no
    /// server-time, so catch-up falls back to the latest page) or sent here
    /// without echo-message. msgid dedup cannot see these; compare content
    /// the way the Prepend fallback does. Each such row absorbs at most one
    /// server row, so genuinely repeated lines survive (#320).
    fn drop_rows_shown_without_identity(
        rows: &mut Vec<ChatMessage>,
        shown: &VecDeque<ChatMessage>,
    ) {
        let mut candidates: Vec<&ChatMessage> = shown
            .iter()
            .filter(|row| row.msgid.is_none() && row.server_time.is_none())
            .collect();
        if candidates.is_empty() {
            return;
        }
        rows.retain(|row| {
            match candidates
                .iter()
                .position(|local| Self::same_history_row(local, row))
            {
                Some(index) => {
                    candidates.swap_remove(index);
                    false
                }
                None => true,
            }
        });
    }

    fn queue_history_request(
        &mut self,
        _target: &str,
        command: IrcCommand,
        request: PendingHistoryRequest,
    ) -> bool {
        self.send_command_with_history(command, Some(request))
    }

    fn request_latest_history(
        &mut self,
        target: &str,
        requested: Option<usize>,
        placement: HistoryPlacement,
    ) -> bool {
        if !self.chathistory_enabled {
            return false;
        }
        let total = self.total_history_limit(requested);
        let limit = self.history_page_limit(total);
        let reference = if placement == HistoryPlacement::Append {
            self.channel_key(target)
                .and_then(|key| self.conversations.get(&key))
                .and_then(|channel| {
                    channel
                        .messages
                        .iter()
                        .rev()
                        .find_map(Self::message_reference)
                })
                .unwrap_or_else(|| "*".to_string())
        } else {
            "*".to_string()
        };
        let (direction, command) = if placement == HistoryPlacement::Append && reference != "*" {
            (
                HistoryDirection::After,
                IrcCommand::ChathistoryAfter(target.to_string(), reference, limit),
            )
        } else {
            (
                HistoryDirection::Before,
                IrcCommand::ChathistoryLatest(target.to_string(), reference, limit),
            )
        };
        self.queue_history_request(
            target,
            command,
            PendingHistoryRequest {
                correlation: None,
                placement,
                direction,
                remaining: total,
                loaded: 0,
                shown: 0,
                page_limit: limit,
                announce_arrivals: direction == HistoryDirection::After,
            },
        )
    }

    fn prepare_history_target(&mut self, target: &str) {
        let id = self.ensure_history_target(target);
        self.select_conversation(Some(id));
    }

    fn ensure_history_target(&mut self, target: &str) -> ConversationId {
        let key = if let Some(key) = self.channel_key(target) {
            key
        } else {
            let mut channel = Channel::new();
            channel.set_case_mapping(self.network_support.case_mapping);
            channel.set_prefix_schema(
                &self.network_support.prefix_modes,
                &self.network_support.user_prefixes,
            );
            self.conversations.insert(target.to_string(), channel);
            target.to_string()
        };
        self.id_for_target(&key)
            .expect("history target was inserted")
    }

    fn outgoing_history_request(command: &IrcCommand) -> Option<OutgoingHistoryRequest> {
        if !matches!(
            command,
            IrcCommand::ChathistoryLatest(..)
                | IrcCommand::ChathistoryBefore(..)
                | IrcCommand::ChathistoryAfter(..)
                | IrcCommand::ChathistoryAround(..)
                | IrcCommand::ChathistoryBetween(..)
                | IrcCommand::ChathistoryTargets(..)
                | IrcCommand::Raw(..)
                | IrcCommand::Extension(..)
        ) {
            return None;
        }
        // Parse the actual wire form, including /raw, /quote and /label. The
        // incoming CHATHISTORY parser intentionally recognizes only TARGETS
        // replies, so its typed command is not an outgoing request descriptor.
        let line = command.to_string();
        let message = IrcMessage::parse(&line)?;
        let mut body = line.trim_start();
        for prefix in ['@', ':'] {
            if body.starts_with(prefix) {
                body = body.split_once(' ')?.1.trim_start();
            }
        }
        let mut words = body.split_whitespace();
        if !words.next()?.eq_ignore_ascii_case("CHATHISTORY") {
            return None;
        }
        let subcommand = HistorySubcommand::parse(words.next()?)?;
        let target = if subcommand == HistorySubcommand::Targets {
            String::new()
        } else {
            words.next()?.to_string()
        };
        let limit = words.last().and_then(|word| word.parse().ok()).unwrap_or(1);
        Some(OutgoingHistoryRequest {
            target,
            subcommand,
            label: message.get_tag("label").filter(|label| !label.is_empty()),
            limit,
        })
    }

    fn record_history_request(
        &mut self,
        outgoing: OutgoingHistoryRequest,
        request: Option<PendingHistoryRequest>,
    ) {
        let mut request = request.unwrap_or(PendingHistoryRequest {
            correlation: None,
            placement: if outgoing.subcommand == HistorySubcommand::After {
                HistoryPlacement::Append
            } else {
                HistoryPlacement::Prepend
            },
            direction: if outgoing.subcommand == HistorySubcommand::After {
                HistoryDirection::After
            } else {
                HistoryDirection::Before
            },
            remaining: 0,
            loaded: 0,
            shown: 0,
            page_limit: outgoing.limit.max(1),
            announce_arrivals: false,
        });
        let owner = if let Some(correlation) = &request.correlation {
            correlation.owner
        } else if outgoing.subcommand == HistorySubcommand::Targets {
            None
        } else {
            Some(self.ensure_history_target(&outgoing.target))
        };
        request.correlation = Some(HistoryRequestCorrelation {
            owner,
            target: outgoing.target.clone(),
            aliases: Vec::new(),
            subcommand: outgoing.subcommand,
            label: outgoing.label,
            sequence: self.next_history_request,
        });
        self.next_history_request = self
            .next_history_request
            .checked_add(1)
            .expect("history request sequence exhausted");
        self.pending_history_requests
            .entry(self.network_support.canonicalize(&outgoing.target))
            .or_default()
            .push_back(request);
    }

    fn history_request_matches_target(
        &self,
        key: &str,
        request: &PendingHistoryRequest,
        target: &str,
    ) -> bool {
        request.correlation.as_ref().map_or_else(
            || self.identifiers_equal(key, target),
            |correlation| {
                self.identifiers_equal(&correlation.target, target)
                    || correlation
                        .aliases
                        .iter()
                        .any(|alias| self.identifiers_equal(alias, target))
                    || correlation
                        .owner
                        .and_then(|id| self.target_name(id))
                        .is_some_and(|name| self.identifiers_equal(name, target))
            },
        )
    }

    fn has_pending_history_target(&self, target: &str) -> bool {
        self.pending_history_requests.iter().any(|(key, requests)| {
            requests
                .iter()
                .any(|request| self.history_request_matches_target(key, request, target))
        })
    }

    fn take_history_request(
        &mut self,
        target: &str,
        subcommand: Option<HistorySubcommand>,
        label: Option<&str>,
    ) -> Option<PendingHistoryRequest> {
        let mut candidates = Vec::new();
        for (key, requests) in &self.pending_history_requests {
            for (index, request) in requests.iter().enumerate() {
                let correlation = request.correlation.as_ref();
                if subcommand
                    .is_some_and(|kind| correlation.is_some_and(|value| value.subcommand != kind))
                {
                    continue;
                }
                let matches = if let Some(label) = label {
                    correlation.and_then(|value| value.label.as_deref()) == Some(label)
                        && (target.is_empty()
                            || self.history_request_matches_target(key, request, target))
                } else {
                    self.history_request_matches_target(key, request, target)
                };
                if matches {
                    candidates.push((
                        key.clone(),
                        index,
                        correlation.map_or(0, |value| value.sequence),
                    ));
                }
            }
        }
        // Labels identify a single exchange. Otherwise preserve wire order
        // among requests compatible with the target and explicit subcommand,
        // including cancelled requests whose owner was closed.
        if label.is_some() && candidates.len() != 1 {
            return None;
        }
        let (key, index, _) = candidates.into_iter().min_by_key(|value| value.2)?;
        let request = &self.pending_history_requests[&key][index];
        if let Some(correlation) = &request.correlation
            && ((label.is_none() && correlation.label.is_some())
                || subcommand.is_some_and(|kind| correlation.subcommand != kind))
        {
            return None;
        }
        let mut request = self.pending_history_requests.get_mut(&key)?.remove(index)?;
        if self
            .pending_history_requests
            .get(&key)
            .is_some_and(VecDeque::is_empty)
        {
            self.pending_history_requests.remove(&key);
        }
        // Test fixtures and unsolicited batches have no outgoing descriptor.
        // Capture only an already-open conversation, never create one here.
        if request.correlation.is_none() {
            request.correlation = Some(HistoryRequestCorrelation {
                owner: self.id_for_target(target),
                target: target.to_string(),
                aliases: Vec::new(),
                subcommand: subcommand.unwrap_or(HistorySubcommand::Latest),
                label: label.map(str::to_owned),
                sequence: 0,
            });
        }
        Some(request)
    }

    fn response_label(&self, message: &IrcMessage) -> Option<String> {
        if let Some(label) = message.get_tag("label") {
            return Some(label);
        }
        let mut current = message.get_batch()?;
        for _ in 0..32 {
            if let Some(label) = self.batch_labels.get(&current) {
                return Some(label.clone());
            }
            current = self.batch_parents.get(&current)?.clone();
        }
        None
    }

    fn retire_history_owner(&mut self, owner: ConversationId) {
        if self.membership_managed_by_bouncer()
            && let Some(target) = self.target_name(owner)
        {
            self.bouncer_retired_targets
                .insert(self.network_support.canonicalize(target));
        }
        for request in self.pending_history_requests.values_mut().flatten().chain(
            self.chathistory_batches
                .values_mut()
                .map(|batch| &mut batch.request),
        ) {
            if let Some(correlation) = &mut request.correlation
                && correlation.owner == Some(owner)
            {
                correlation.owner = None;
            }
        }
    }

    fn load_local_history_fallback(&mut self, target: &str) -> Option<VecDeque<ChatMessage>> {
        if !self.logging_load_history
            || self
                .channel_key(target)
                .and_then(|key| self.conversations.get(&key))
                .is_some_and(|channel| channel.history_loaded)
        {
            return None;
        }
        let (network, legacy_host, allow_legacy) = self.active_log_context()?;
        let mut local = self.load_text_history(
            &network,
            &legacy_host,
            &self.network_support.canonicalize(target),
            target,
            self.logging_history_lines.saturating_add(1),
            allow_legacy,
        );
        // The current JOIN has already written this marker to the log and
        // placed it in the tab. Keep its live copy in the right position.
        // The marker may carry a CASEMAPPING-equivalent spelling of the
        // target (one shared canonical log file), so match by casemapping,
        // not exact text.
        if local.last().is_some_and(|message| {
            message.is_system
                && message
                    .content
                    .strip_prefix("Now talking in ")
                    .is_some_and(|rest| self.identifiers_equal(rest, target))
        }) {
            local.pop();
        }
        if local.len() > self.logging_history_lines {
            local.drain(..local.len() - self.logging_history_lines);
        }
        self.log_manager
            .log_session_start(&network, &self.network_support.canonicalize(target));
        if local.is_empty() {
            return None;
        }
        let count = local.len();
        let mut history = VecDeque::with_capacity(count + 1);
        history.push_back(
            ChatMessage::system_fmt(
                &format!(
                    "--- {count} line{} of history loaded ---",
                    if count == 1 { "" } else { "s" }
                ),
                &self.timestamp_format,
            )
            .without_logging(),
        );
        history.extend(local);
        if let Some(key) = self.channel_key(target)
            && let Some(channel) = self.conversations.get_mut(&key)
        {
            channel.history_loaded = true;
        }
        Some(history)
    }

    fn queue_chathistory_message(&mut self, batch_id: &str, message: ChatMessage) {
        if let Some(batch) = self.chathistory_batches.get_mut(batch_id)
            && batch.messages.len() < self.max_scrollback
        {
            batch.messages.push(message);
        }
    }

    fn containing_history_batch(&self, batch_id: &str) -> Option<String> {
        let mut current = batch_id;
        for _ in 0..32 {
            if self.chathistory_batches.contains_key(current) {
                return Some(current.to_string());
            }
            current = self.batch_parents.get(current)?;
        }
        None
    }

    fn collect_multiline_line(&mut self, msg: &IrcMessage) -> bool {
        let Some(batch_id) = msg.get_batch() else {
            return false;
        };
        let Some(batch) = self.multiline_batches.get_mut(&batch_id) else {
            return false;
        };
        let (content, is_notice) = match &msg.command {
            IrcCommand::Privmsg(_, content) => (content, false),
            IrcCommand::Notice(_, content) => (content, true),
            _ => return false,
        };
        let concat = msg.get_tag("draft/multiline-concat").is_some();
        // Enforce negotiated multiline limits inbound (#191); fall back to
        // the draft defaults so an unadvertised limit cannot grow unbounded.
        let max_bytes = self.network_support.multiline_max_bytes.unwrap_or(4096);
        let max_lines = self.network_support.multiline_max_lines.unwrap_or(24);
        let new_line = batch.has_lines && !concat;
        let projected_lines = batch.line_count + usize::from(new_line || !batch.has_lines);
        let projected_bytes = batch.content.len() + usize::from(new_line) + content.len();
        if projected_bytes > max_bytes || projected_lines > max_lines {
            batch.oversized = true;
            batch.content.clear();
            return true;
        }
        if batch.oversized {
            return true;
        }
        if new_line {
            batch.content.push('\n');
        }
        batch.content.push_str(content);
        batch.line_count = projected_lines;
        batch.has_lines = true;
        batch.is_notice = is_notice;
        true
    }

    fn collect_authtoken_line(&mut self, msg: &IrcMessage) -> bool {
        let Some(batch_id) = msg.get_batch() else {
            return false;
        };
        let Some(batch) = self.authtoken_batches.get_mut(&batch_id) else {
            return false;
        };
        let IrcCommand::Extension(name, params) = &msg.command else {
            return false;
        };
        if !name.eq_ignore_ascii_case("TOKEN")
            || !params
                .first()
                .is_some_and(|subcommand| subcommand.eq_ignore_ascii_case("GENERATE"))
        {
            return false;
        }
        if let Some(chunk) = params.get(2)
            && !batch.oversized
        {
            if batch.token.len() + chunk.len() > MAX_AUTHTOKEN_BYTES {
                batch.oversized = true;
                batch.token = String::new();
            } else {
                batch.token.push_str(chunk);
            }
        }
        true
    }

    fn finish_authtoken_batch(&mut self, batch_id: &str) {
        let upload_label = self.upload_batch_label(batch_id);
        let Some(batch) = self.authtoken_batches.remove(batch_id) else {
            return;
        };
        if self.complete_upload_token(
            &batch.service,
            if batch.oversized {
                String::new()
            } else {
                batch.token.clone()
            },
            upload_label,
        ) {
            return;
        }
        if batch.oversized {
            self.add_server_message(
                ChatMessage::system_fmt(
                    &format!(
                        "[TOKEN GENERATE {}] token longer than {MAX_AUTHTOKEN_BYTES} bytes discarded",
                        batch.service
                    ),
                    &self.timestamp_format,
                )
                .without_logging(),
            );
            return;
        }
        if batch.token.is_empty() {
            return;
        }
        self.add_server_message(
            ChatMessage::system_fmt(
                &format!("[TOKEN GENERATE {}] {}", batch.service, batch.token),
                &self.timestamp_format,
            )
            .without_logging(),
        );
    }

    fn finish_multiline_batch(&mut self, batch_id: &str) {
        let Some(batch) = self.multiline_batches.remove(batch_id) else {
            return;
        };
        if batch.oversized {
            return;
        }
        // Multiline messages must honour the ignore list like live PRIVMSG (#192).
        if self.is_ignored(&batch.sender, batch.sender_prefix.as_deref()) {
            return;
        }
        // Notices have explicit local confirmation rows, just like the
        // non-multiline NOTICE handler. Historical notices still replay.
        if batch.is_notice
            && batch.parent_history.is_none()
            && self.enabled_caps.contains("echo-message")
            && self.identifiers_equal(&batch.sender, &self.my_nick)
        {
            return;
        }
        let action_text = ctcp_action_text(&batch.content);
        let is_action = action_text.is_some();
        let content = action_text.unwrap_or(&batch.content);
        // Route like a live PRIVMSG: STATUSMSG prefixes are stripped and a DM
        // addressed to our nick belongs to the sender's query.
        let routed_target = self.strip_status_prefix(&batch.target).to_string();
        let is_pm = !self.is_channel_name(&routed_target)
            && self.identifiers_equal(&routed_target, &self.my_nick);
        let target = if is_pm {
            batch.sender.clone()
        } else {
            routed_target
        };
        // Highlight like a live PRIVMSG (#289): without it a nick mention in a
        // multiline message rendered as a plain row. Per-conversation rules are
        // keyed by the routed conversation, not the raw wire target.
        let is_highlight = !batch.is_notice && self.attention_highlight(&target, content);
        let fmt = &self.timestamp_format;
        let message = if batch.is_notice {
            ChatMessage::system_fmt(&format!("-{}- {}", batch.sender, content), fmt)
        } else if is_action && is_highlight {
            ChatMessage::action_highlighted_fmt(&batch.sender, content, fmt)
        } else if is_action {
            ChatMessage::action_fmt(&batch.sender, content, fmt)
        } else if is_highlight {
            ChatMessage::highlighted_fmt(&batch.sender, content, fmt)
        } else {
            ChatMessage::new_fmt(&batch.sender, content, fmt)
        }
        .with_irc_metadata(batch.server_time, batch.msgid, batch.account)
        .with_oper(batch.oper)
        .with_reply_to(batch.reply_to);

        if let Some(history_batch) = batch.parent_history {
            self.queue_chathistory_message(&history_batch, message.without_logging());
            return;
        }

        if !batch.is_notice {
            let notified_content = truncate_chars(content, 50);
            self.notify_incoming_message(
                &batch.sender,
                &target,
                &notified_content,
                is_pm,
                is_highlight,
                &message,
            );
        }
        if !self.merge_server_echo(&target, &message) {
            self.add_message_to_channel(&target, message);
        }
    }

    /// Reconcile an `echo-message` response with the optimistic local row.
    /// The server supplies the authoritative timestamp, msgid, and account;
    /// retaining one row avoids duplicate messages while making that identity
    /// available to REDACT, reconnect history cursors, and MARKREAD.
    fn merge_server_echo(&mut self, target: &str, echoed: &ChatMessage) -> bool {
        // Current nick only: merging is lossy (stamps the local row with the
        // server's msgid/server-time), so a peer squatting a nick we vacated
        // must not qualify — a racing echo under a vacated nick costs at
        // worst a duplicate row (#166/#169). sender_is_self stays on the
        // redaction and replay-drop gates, where over-matching is the safe
        // direction.
        if !self.enabled_caps.contains("echo-message")
            || !self.identifiers_equal(&echoed.sender, &self.my_nick)
            || echoed.is_system
        {
            return false;
        }
        let Some(channel) = self.channel_mut(target) else {
            return false;
        };
        // Echoes arrive in send order, so pair with the oldest unconfirmed
        // matching retained row; searching newest-first would
        // swap msgid/time between identical consecutive sends. Only rows this
        // session sent are candidates: rows loaded from the local log also
        // lack a msgid and server-time, and stamping one of those would
        // leave the new row unconfirmed.
        let Some(local) = channel.messages.iter_mut().find(|candidate| {
            candidate.pending_echo
                && !candidate.is_system
                && candidate.msgid.is_none()
                && candidate.server_time.is_none()
                && candidate.sender.eq_ignore_ascii_case(&echoed.sender)
                && candidate.content == echoed.content
                && candidate.is_action == echoed.is_action
        }) else {
            return false;
        };
        local.pending_echo = false;
        local.delivery = message_ui::MessageDelivery::Confirmed;
        local.timestamp.clone_from(&echoed.timestamp);
        local.server_time.clone_from(&echoed.server_time);
        local.msgid.clone_from(&echoed.msgid);
        local.account.clone_from(&echoed.account);
        local.oper.clone_from(&echoed.oper);
        local.reply_to.clone_from(&echoed.reply_to);
        local.presentation_revision = local.presentation_revision.wrapping_add(1);
        let archived = local.clone();
        self.record_archive_message(target, &archived, false);
        true
    }

    fn history_event_message(&self, msg: &IrcMessage) -> Option<ChatMessage> {
        let sender = msg
            .get_sender_nick()
            .unwrap_or_else(|| "Server".to_string());
        let content = match &msg.command {
            IrcCommand::Join(channel, _, account, _) => account.as_ref().map_or_else(
                || format!("{sender} joined {channel}"),
                |account| format!("{sender} [{account}] joined {channel}"),
            ),
            IrcCommand::Part(channel, reason) => format!(
                "{sender} left {channel}{}",
                reason
                    .as_ref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            IrcCommand::Quit(reason) => format!(
                "{sender} quit{}",
                reason
                    .as_ref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            IrcCommand::Kick(channel, nick, reason) => format!(
                "{nick} was kicked from {channel} by {sender}{}",
                reason
                    .as_ref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            IrcCommand::Mode(target, modes, params) => format!(
                "{sender} set mode {} {} {}",
                target,
                modes.as_deref().unwrap_or_default(),
                params.join(" ")
            ),
            IrcCommand::Topic(channel, topic) => format!(
                "{sender} changed the topic in {channel} to: {}",
                topic.as_deref().unwrap_or_default()
            ),
            IrcCommand::Nick(new_nick) => format!("{sender} is now known as {new_nick}"),
            // A replayed channel move is history, not a command: the live tab
            // must stay put so the batch still finds the target it was
            // requested for.
            IrcCommand::Rename(old, new, reason) => format!(
                "Channel renamed from {old} to {new}{}",
                reason
                    .as_deref()
                    .filter(|reason| !reason.is_empty())
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            IrcCommand::Relocate(old, new, reason) => format!(
                "Channel {old} moved to {new}{}",
                reason
                    .as_deref()
                    .filter(|reason| !reason.is_empty())
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            IrcCommand::Setname(realname) => {
                format!("{sender} changed their realname to {realname}")
            }
            IrcCommand::Redact(target, _, reason) => format!(
                "A message in {target} was redacted{}",
                reason
                    .as_deref()
                    .filter(|reason| !reason.is_empty())
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            _ => return None,
        };
        Some(
            ChatMessage::system_fmt(&content, &self.timestamp_format)
                .with_irc_metadata(
                    msg.get_server_time_raw(),
                    msg.get_msgid(),
                    msg.get_account(),
                )
                .without_logging(),
        )
    }

    /// Sort only timestamped slots. Comparing an untagged row as Equal to
    /// every tagged row is not a total order, and also fails to keep those
    /// untagged rows at stable positions within a history page.
    fn order_history_page(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
        let mut slots = Vec::with_capacity(messages.len());
        let mut timed = Vec::new();
        for message in messages {
            if message.server_time.is_some() {
                timed.push(message);
                slots.push(None);
            } else {
                slots.push(Some(message));
            }
        }
        // All entries have a canonical, validated server-time. Stable sort
        // retains wire order for messages sharing the same millisecond.
        timed.sort_by(|left, right| left.server_time.cmp(&right.server_time));
        let mut timed = timed.into_iter();
        slots
            .into_iter()
            .map(|slot| slot.unwrap_or_else(|| timed.next().expect("one row per timestamped slot")))
            .collect()
    }

    /// Catch-up pages and live traffic may interleave on the wire. Merge
    /// their timestamped rows instead of placing an older page after newer
    /// live rows. Untagged rows keep their stream order and stay with the
    /// preceding timestamp; leading history banners travel with its first row.
    fn merge_catch_up_history(
        existing: &mut VecDeque<ChatMessage>,
        history: VecDeque<ChatMessage>,
    ) {
        // Echo reconciliation can stamp an earlier optimistic row with a
        // later time than its following peer row. Repair such inversions
        // before the linear merge while leaving untagged slots anchored.
        let mut previous = None;
        let out_of_order = existing
            .iter()
            .filter_map(|row| row.server_time.as_ref())
            .any(|time| {
                let inverted = previous.is_some_and(|old: &String| old > time);
                previous = Some(time);
                inverted
            });
        if out_of_order {
            *existing = Self::order_history_page(existing.drain(..).collect()).into();
        }
        let mut merged = VecDeque::with_capacity(existing.len().saturating_add(history.len()));
        let mut leading = VecDeque::new();
        let mut has_timestamp = false;
        for message in history {
            if let Some(timestamp) = message.server_time.as_deref() {
                while existing.front().is_some_and(|row| {
                    row.server_time
                        .as_deref()
                        .is_none_or(|existing_time| existing_time <= timestamp)
                }) {
                    merged.push_back(existing.pop_front().unwrap());
                }
                merged.append(&mut leading);
                merged.push_back(message);
                has_timestamp = true;
            } else if has_timestamp {
                merged.push_back(message);
            } else {
                leading.push_back(message);
            }
        }
        if !has_timestamp {
            // An empty result's status banner, or an entirely untagged page,
            // has no chronological reference; retain its existing placement.
            existing.append(&mut leading);
        } else {
            merged.append(existing);
            *existing = merged;
        }
    }

    fn finish_chathistory_batch(&mut self, batch_id: &str) {
        let Some(mut batch) = self.chathistory_batches.remove(batch_id) else {
            return;
        };
        let Some(key) = batch
            .request
            .correlation
            .as_ref()
            .and_then(|correlation| correlation.owner)
            .and_then(|owner| self.target_name(owner))
            .map(str::to_owned)
        else {
            // A reply belongs to the conversation that requested it, even if
            // a new tab now happens to have the same IRC target name.
            return;
        };
        batch.target.clone_from(&key);
        let mut messages = Self::order_history_page(batch.messages);
        if batch.legacy_playback {
            let mut existing: Vec<_> = self.conversations[&key].messages.iter().collect();
            messages.retain(|message| {
                let repeated = existing.iter().position(|row| {
                    row.server_time == message.server_time && Self::same_history_row(row, message)
                });
                if let Some(index) = repeated {
                    existing.remove(index);
                    false
                } else {
                    true
                }
            });
        }
        let page_count = messages.len();
        let remaining = batch.request.remaining.saturating_sub(page_count);
        let loaded = batch.request.loaded.saturating_add(page_count);
        let complete = batch.complete || page_count < batch.request.page_limit;
        let next_reference = match batch.request.direction {
            HistoryDirection::Before => messages.first().and_then(Self::message_reference),
            HistoryDirection::After => messages.last().and_then(Self::message_reference),
        };
        let existing_ids: std::collections::HashSet<String> = self.conversations[&key]
            .messages
            .iter()
            .filter_map(|message| message.msgid.clone())
            .collect();
        messages.retain(|message| {
            message
                .msgid
                .as_ref()
                .is_none_or(|msgid| !existing_ids.contains(msgid))
        });
        if batch.request.placement == HistoryPlacement::Append {
            Self::drop_rows_shown_without_identity(
                &mut messages,
                &self.conversations[&key].messages,
            );
        }
        // Own-notice replays whose local confirmation row ("-> -target- text")
        // is already in the tab are display duplicates, and own-credential
        // chat rows replay the server's verbatim copy of a redacted local
        // row: both are dropped here — after page_count/remaining were
        // computed, so the pagination heuristic stays exact (#153/#158).
        let has_candidates = messages.iter().any(|row| {
            (row.is_system && row.content.starts_with('-'))
                || (!row.is_system && self.sender_is_self(&row.sender))
        });
        if has_candidates {
            let routed_target = self.strip_status_prefix(&batch.target).to_string();
            // Only rows the client itself rendered as notice confirmations
            // count — a peer typing the literal "-> -#chan- hello" must not
            // suppress a replay (#157). The stored target is the spelling the
            // user typed, so it is compared after stripping status prefixes.
            let confirmations: Vec<(String, String)> = self
                .conversations
                .get(&key)
                .map(|channel| {
                    channel
                        .messages
                        .iter()
                        .rev()
                        .take(64)
                        .filter(|row| row.is_system)
                        .filter_map(|row| {
                            row.content.strip_prefix("-> -").and_then(|rest| {
                                rest.split_once("- ")
                                    .map(|(target, text)| (target.to_string(), text.to_string()))
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let keep_row = |row: &ChatMessage| {
                if !row.is_system {
                    // Own-credential chat rows: the local redacted copy has
                    // no msgid; replaying the server's verbatim copy would
                    // re-render the secret. Only for a service-shaped batch
                    // target — the keywords (drop, group, login...) are
                    // ordinary words in a channel, where an own line must
                    // survive a reconnect replay. The keyword check itself
                    // is not tied to the current nick, which may predate a
                    // GHOST/REGAIN (#163). Non-own rows are never touched.
                    if !self.sender_is_self(&row.sender) {
                        return true;
                    }
                    return !commands::service_message_contains_credentials(
                        &batch.target,
                        &row.content,
                    );
                }
                let Some((nick, text)) = row
                    .content
                    .strip_prefix('-')
                    .and_then(|rest| rest.split_once("- "))
                else {
                    return true;
                };
                if !self.sender_is_self(nick) {
                    return true;
                }
                // A credential command's confirmation was redacted locally,
                // so replaying the verbatim copy would re-display the secret.
                // Gated on a service-shaped batch target like the PRIVMSG
                // branch: ordinary channel notices survive the replay.
                if commands::service_message_contains_credentials(&batch.target, text) {
                    return false;
                }
                !confirmations.iter().any(|(target, confirmed_text)| {
                    confirmed_text == text
                        && self.identifiers_equal(self.strip_status_prefix(target), &routed_target)
                })
            };
            messages.retain(keep_row);
        }
        // Archive only rows accepted by the same privacy and display filters.
        for message in &messages {
            self.record_archive_message(&key, message, true);
        }
        let shown = batch.request.shown.saturating_add(messages.len());
        let next = if !batch.partial && !complete && remaining > 0 {
            next_reference.map(|reference| {
                let limit = self.history_page_limit(remaining);
                let command = match batch.request.direction {
                    HistoryDirection::Before => {
                        IrcCommand::ChathistoryBefore(batch.target.clone(), reference, limit)
                    }
                    HistoryDirection::After => {
                        IrcCommand::ChathistoryAfter(batch.target.clone(), reference, limit)
                    }
                };
                (
                    command,
                    PendingHistoryRequest {
                        remaining,
                        loaded,
                        shown,
                        page_limit: limit,
                        ..batch.request.clone()
                    },
                )
            })
        } else {
            None
        };
        let mut history = VecDeque::with_capacity(page_count.saturating_add(1));
        if next.is_none() {
            if loaded == 0 {
                if let Some(local) = (batch.request.placement == HistoryPlacement::Prepend)
                    .then(|| self.load_local_history_fallback(&batch.target))
                    .flatten()
                {
                    history = local;
                } else {
                    let status = if batch.partial {
                        "--- No server history available (server reports a partial result) ---"
                    } else {
                        "--- No server history available ---"
                    };
                    history.push_back(
                        ChatMessage::system_fmt(status, &self.timestamp_format).without_logging(),
                    );
                }
            } else {
                let mut local_count = 0;
                let mut local_history_loaded = false;
                if batch.request.placement == HistoryPlacement::Prepend
                    && loaded < batch.request.loaded.saturating_add(batch.request.remaining)
                    && let Some(mut local) = self.load_local_history_fallback(&batch.target)
                {
                    local_history_loaded = true;
                    local.pop_front(); // Replace the local-only status below.
                    if let Some(channel) = self.conversations.get(&key) {
                        local.retain(|row| {
                            !messages
                                .iter()
                                .any(|server| Self::same_history_row(row, server))
                                && !channel
                                    .messages
                                    .iter()
                                    .any(|live| Self::same_history_row(row, live))
                        });
                        let room = self.max_scrollback.saturating_sub(
                            messages
                                .len()
                                .saturating_add(channel.messages.len())
                                .saturating_add(1),
                        );
                        if local.len() > room {
                            local.drain(..local.len() - room);
                        }
                    }
                    local_count = local.len();
                    history.extend(local);
                }
                let mut status = if local_count > 0 {
                    format!(
                        "--- History loaded: {local_count} local line{}, {shown} server line{} ---",
                        if local_count == 1 { "" } else { "s" },
                        if shown == 1 { "" } else { "s" }
                    )
                } else if shown == 0 {
                    "--- No new server history available ---".to_string()
                } else if shown == 1 {
                    "--- 1 line of server history loaded ---".to_string()
                } else {
                    format!("--- {shown} lines of server history loaded ---")
                };
                if batch.partial {
                    status.push_str(" (server reports a partial result)");
                } else if !complete && remaining > 0 {
                    status.push_str(" (more history is available)");
                }
                history.push_front(
                    ChatMessage::system_fmt(&status, &self.timestamp_format).without_logging(),
                );
                if let Some(channel) = self.conversations.get_mut(&key) {
                    channel.history_loaded = true;
                }
                if !local_history_loaded && let Some((network, _, _)) = self.active_log_context() {
                    self.log_manager.log_session_start(
                        &network,
                        &self.network_support.canonicalize(&batch.target),
                    );
                }
            }
        }
        // Catch-up (Append) rows are genuinely new to this client, so they
        // count as unread and notify like the identical live message (#306).
        // Only others' chat rows qualify; backlog pages (Prepend) are old by
        // design and stay silent.
        let arrivals: Vec<(String, String, bool)> = if batch.request.placement
            == HistoryPlacement::Append
            && batch.request.announce_arrivals
        {
            messages
                .iter()
                .filter(|row| !row.is_system && !self.sender_is_self(&row.sender))
                .map(|row| (row.sender.clone(), row.content.clone(), row.is_highlight))
                .collect()
        } else {
            Vec::new()
        };
        history.extend(messages.drain(..));

        if let Some(channel) = self.conversations.get_mut(&key) {
            if batch.request.placement == HistoryPlacement::Prepend {
                history.append(&mut channel.messages);
                channel.messages = history;
            } else {
                Self::merge_catch_up_history(&mut channel.messages, history);
            }
            while channel.messages.len() > self.max_scrollback {
                channel.messages.pop_front();
            }
        }
        let pending_ids: Vec<_> = self
            .conversations
            .get(&key)
            .map(|channel| {
                channel
                    .messages
                    .iter()
                    .filter_map(|message| message.msgid.clone())
                    .collect()
            })
            .unwrap_or_default();
        for id in pending_ids {
            self.apply_pending_reactions(&key, &id);
        }
        self.refresh_reply_previews(&key);
        if !arrivals.is_empty() {
            if !(self.window_focused
                && self.current_target_is(&batch.target)
                && self.reading_conversation == self.current_conversation)
                && let Some(channel) = self.conversations.get_mut(&key)
            {
                channel.unread = channel.unread.saturating_add(arrivals.len());
            }
            self.notify_history_arrivals(&batch.target, &arrivals);
        }
        if let Some((command, request)) = next {
            self.queue_history_request(&batch.target, command, request);
        } else if self.current_target_is(&batch.target)
            && self.window_focused
            && self.reading_conversation == self.current_conversation
        {
            self.mark_target_read(&batch.target);
        }
    }

    /// One desktop notification for a catch-up page, whatever its size: a
    /// reconnect can deliver dozens of missed messages at once. A query tab
    /// notifies (forced, like a live PM) for any arrival; a channel only for
    /// mentions. `arrivals` holds (sender, content, is_highlight) per new
    /// chat row, oldest first.
    fn notify_history_arrivals(&mut self, target: &str, arrivals: &[(String, String, bool)]) {
        let private = !self.is_channel_name(target);
        let relevant: Vec<_> = arrivals
            .iter()
            .filter(|(_, _, mention)| private || *mention)
            .collect();
        let Some((sender, content, mention)) = relevant.last() else {
            return;
        };
        let title = if private {
            if relevant.len() == 1 {
                format!("PM from {sender}")
            } else {
                format!("{} new messages from {target}", relevant.len())
            }
        } else if relevant.len() == 1 {
            format!("{sender} mentioned you in {target}")
        } else {
            format!("{} mentions in {target}", relevant.len())
        };
        let reference = self
            .conversations
            .get(target)
            .and_then(|channel| {
                channel
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.sender == *sender && message.content == *content)
            })
            .map(|message| self.live_message_reference(target, message));
        self.events.push(SessionEvent::Notify {
            title,
            body: truncate_chars(content, 50),
            force: private,
            attention: reference.map(|reference| attention::AttentionEvent {
                reference,
                private,
                mention: *mention,
            }),
        });
    }

    /// Clamp a QUIT reason so the resulting line always fits in one IRC line.
    /// An overlong reason must never cause the QUIT itself to be dropped by the
    /// send_command length check: that would orphan the connection thread.
    fn clamp_quit_reason(reason: Option<String>) -> Option<String> {
        const MAX_REASON_BYTES: usize = IRC_MAX_LINE_BYTES - "QUIT :".len();
        reason.map(|r| {
            if r.len() <= MAX_REASON_BYTES {
                r
            } else {
                let mut end = MAX_REASON_BYTES;
                while end > 0 && !r.is_char_boundary(end) {
                    end -= 1;
                }
                r[..end].to_string()
            }
        })
    }

    pub fn request_manual_disconnect(&mut self, reason: Option<String>) {
        self.connection_status = Default::default();
        self.message_ui = Default::default();
        self.reset_history_authentication();
        self.clear_update_restore();
        self.close_connection(reason, false);
    }

    #[cfg(test)]
    pub(crate) fn release_closing_connection(&mut self, worker_exited: bool) {
        self.connection.release_closing_receiver(worker_exited);
    }

    pub fn request_reconnect_after_close(&mut self, reason: &str) {
        // Capture before the reset below discards still-deferred automatic
        // JOINs, which are part of the membership to recover.
        self.capture_rejoin_memberships();
        self.reset_history_authentication();
        self.close_connection(Some(reason.to_string()), true);
    }

    fn fail_registration(&mut self, message: String) {
        self.add_server_message(ChatMessage::system_fmt(&message, &self.timestamp_format));
        // A connection already being closed on purpose (disconnect, /server,
        // /reconnect) can still deliver a late registration error. It must
        // not turn a staged switch into a manual disconnect (#246).
        if self.connection.is_closing() {
            return;
        }
        self.request_manual_disconnect(Some("Registration failed".to_string()));
    }

    /// Drop what an update-restart snapshot still has queued for the first
    /// registration: the memberships (with their channel keys) to rejoin and
    /// the tab to select. They describe the snapshot's endpoint, so another
    /// endpoint (or a disconnect that cancels the restore) must never see
    /// them: B's 001 would otherwise JOIN A's channels with A's keys.
    fn clear_update_restore(&mut self) {
        self.pending_rejoins = RecoveryPolicy::ConfiguredAutoJoin;
        self.pending_parts.clear();
        self.update_join_targets.clear();
        self.update_selected = None;
    }

    fn reset_session_state(&mut self) {
        self.macro_jobs.clear();
        self.reset_history_authentication();
        self.clear_update_restore();
        self.events.push(SessionEvent::EndpointChanged);
        self.pending_cycles.clear();
        self.conversations.clear();
        self.current_conversation = None;
        self.input_text.clear();
        self.composer = Default::default();
        self.timeline = Default::default();
        self.message_ui = Default::default();
        self.reading_conversation = None;
        self.command_history.clear();
        self.history_index = None;
        self.history_temp.clear();
        self.tab_completion = None;
        self.channel_info_target = None;
        self.show_channel_info = false;
        self.server_messages.clear();
        self.server_unread = 0;
        self.selected_user = None;
        self.pending_invites.clear();
        self.pending_channel_keys.clear();
        self.join_focus_requests.clear();
        self.pending_auto_perform = None;
        self.channel_list.clear();
        self.channel_list_dirty = true;
        self.channel_list_loading = false;
        self.away_status = None;
        self.auto_away_triggered = false;
        // A new session never shares a previous connection's nick history
        // (#166): replay attribution must not treat the old network's nicks
        // (or nicks someone else held) as ours. reset_connection_support
        // runs first so the keys are rebuilt under the incoming mapping.
        self.reset_connection_support();
        self.clear_nick_lineage();
    }

    /// Load a server favorite into the connection form
    pub(crate) fn load_favorite(&mut self, fav: &ServerFavorite) {
        self.bouncer = fav.bouncer.clone();
        self.form_profile = (!fav.id.is_empty()).then(|| fav.id.clone());
        self.server_host = fav.host.clone();
        self.server_port = fav.port.clone();
        self.use_tls = fav.use_tls;
        self.password = fav.password.clone();
        if !fav.nickname.is_empty() {
            self.nickname = fav.nickname.clone();
        }
        self.auto_join_channels = fav.auto_join.clone();
        self.auto_perform = fav.auto_perform.clone();
        self.upload_settings = fav.upload_settings.clone();
        self.pre_away_message = fav.pre_away_message.clone();
        self.persistence_profile = fav.persistence_profile.clone();
        self.sasl_username = fav.sasl_username.clone();
        self.sasl_password = fav.sasl_password.clone();
        self.sasl_mechanism = fav.sasl_mechanism;
        self.sasl_required = fav.sasl_required;
        self.tls_client_cert_path = fav.tls_client_cert_path.clone();
        self.tls_client_key_path = fav.tls_client_key_path.clone();
        // Restore the previously-dropped connection details. username/realname are
        // only applied when non-empty so loading an older favorite (serde default)
        // doesn't wipe the current values.
        if !fav.username.is_empty() {
            self.username = fav.username.clone();
        }
        if !fav.realname.is_empty() {
            self.realname = fav.realname.clone();
        }
        self.accept_invalid_certs = fav.accept_invalid_certs;
    }
}

#[cfg(test)]
mod update_session_tests {
    use super::*;

    #[test]
    fn update_restart_restores_active_endpoint_memberships_queries_and_draft() {
        let settings = Settings {
            server_host: "active.example".into(),
            nickname: "me".into(),
            logging_enabled: false,
            logging_load_history: false,
            auto_join_channels: "#configured".into(),
            ..Settings::default()
        };
        let mut before = Session::with_settings(settings.clone());
        assert!(before.start_session_from_form());
        before.test_set_connected(true);
        before.test_set_connecting_flag(false);
        before.server_host = "edited.example".into();
        let mut live = Channel::new();
        live.joined = true;
        live.key = Some("private-key".into());
        let mut kicked = Channel::new();
        kicked.joined = false;
        before.conversations.insert("#live".into(), live);
        let mut custom = Channel::new();
        custom.joined = true;
        before.conversations.insert("!custom".into(), custom);
        before.conversations.insert("#kicked".into(), kicked);
        before.conversations.insert("alice".into(), Channel::new());
        before.select_target("alice");
        before.input_text = "unsent draft".into();
        let snapshot = before.update_session();
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        let snapshot = serde_json::from_slice(&encoded).unwrap();
        let mut after = Session::with_settings(settings);
        after.restore_update_session(snapshot);
        assert_eq!(after.active_server_config().unwrap().host, "active.example");
        assert!(after.is_connecting());
        assert_eq!(after.input_text, "unsent draft");
        assert_eq!(after.current_target_name(), Some("alice"));
        assert!(after.conversations.contains_key("#kicked"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        after.test_set_sender(tx);
        after.handle_numeric(RPL_WELCOME, &["me".into(), "Welcome".into()]);
        let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(sent.iter().any(|cmd| matches!(cmd, IrcCommand::Join(name, Some(key), _, _) if name == "#live" && key == "private-key")));
        assert!(
            sent.iter()
                .any(|cmd| matches!(cmd, IrcCommand::Join(name, _, _, _) if name == "!custom"))
        );
        assert!(!sent.iter().any(|cmd| matches!(cmd, IrcCommand::Join(name, _, _, _) if name == "#kicked" || name == "#configured")));
        after.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #live").unwrap());
        assert_eq!(after.current_target_name(), Some("alice"));
        after.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN !custom").unwrap());
        assert!(after.update_selected.is_none());
    }

    /// An app restored from an update snapshot of network A: a keyed channel
    /// to rejoin, a selected tab, and A's session active and connecting.
    fn restored_from_network_a() -> Session {
        let settings = Settings {
            server_host: "a.example".into(),
            nickname: "me".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        };
        let mut before = Session::with_settings(settings.clone());
        assert!(before.start_session_from_form());
        let mut secret = Channel::new();
        secret.joined = true;
        secret.key = Some("a-channel-key".into());
        before.conversations.insert("#secret".into(), secret);
        before.select_target("#secret");
        let mut after = Session::with_settings(settings);
        after.restore_update_session(before.update_session());
        assert!(after.is_connecting());
        assert!(after.pending_rejoins.is_some());
        assert_eq!(after.update_join_targets, ["#secret"]);
        assert_eq!(after.update_selected.as_deref(), Some("#secret"));
        after
    }

    #[test]
    fn switching_networks_after_an_update_restore_does_not_replay_the_old_memberships() {
        let mut after = restored_from_network_a();

        // Network A is unreachable, so the user connects to B instead.
        after.server_host = "b.example".into();
        after.auto_join_channels = "#b-chan".into();
        assert!(after.switch_session_from_form("Changing servers"));
        assert!(after.pending_rejoins.is_none());
        assert!(after.update_join_targets.is_empty());
        assert!(after.update_selected.is_none());

        let (tx, mut rx) = mpsc::unbounded_channel();
        after.test_set_sender(tx);
        after.handle_numeric(RPL_WELCOME, &["me".into(), "Welcome".into()]);
        let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        // A's channels and keys never reach B, and B's own auto-join runs.
        let wire = format!("{sent:?}");
        assert!(
            !wire.contains("a-channel-key") && !wire.contains("#secret"),
            "{wire}"
        );
        assert!(
            sent.iter()
                .any(|cmd| matches!(cmd, IrcCommand::Join(name, _, _, _) if name == "#b-chan")),
            "{wire}"
        );
    }

    #[test]
    fn manual_disconnect_cancels_a_pending_update_restore() {
        let mut after = restored_from_network_a();

        after.request_manual_disconnect(None);

        assert!(after.pending_rejoins.is_none());
        assert!(after.update_join_targets.is_empty());
        assert!(after.update_selected.is_none());
    }

    #[test]
    fn snapshot_written_by_an_older_schema_still_restores() {
        // The previous binary writes the snapshot and the updated one reads it,
        // so fields added since must default instead of failing the launch (#274).
        let older = r##"{"session":{"server":{"host":"old.example","port":6697,"use_tls":true,
            "accept_invalid_certs":false,"nick":"me","username":"u","realname":"r","password":null},
            "auto_join_channels":"#a","auto_perform":""},
            "reconnect":true,"tabs":[["#a",null,true]],"selected":"#a"}"##;
        let snapshot: UpdateSession = serde_json::from_str(older).unwrap();
        let mut app = Session::with_settings(Settings::default());
        app.restore_update_session(snapshot);
        let server = app.active_server_config().unwrap();
        assert_eq!(server.host, "old.example");
        assert_eq!(server.sasl_mechanism, SaslMechanism::Auto);
        assert!(server.sasl_password.is_none());
        assert!(app.is_connecting());
        assert!(app.conversations.contains_key("#a"));
        assert!(app.input_text.is_empty());
        assert!(app.away_status.is_none());
        // An empty object (a snapshot with nothing to restore) also parses.
        assert!(serde_json::from_str::<UpdateSession>("{}").is_ok());
    }

    #[test]
    fn snapshot_missing_its_endpoint_fails_instead_of_restoring_a_default_server() {
        // Credentials that lost the server they belong to must not be sent to
        // the built-in default one: the launch fails and the update rolls back.
        let snapshot = |server: &serde_json::Value| {
            format!(
                r##"{{"session":{{"server":{server},"auto_join_channels":"",
                "auto_perform":"/msg NickServ IDENTIFY secret"}},"reconnect":true}}"##
            )
        };
        let server: serde_json::Value = serde_json::from_str(
            r##"{"host":"h.example","port":6697,"use_tls":true,"nick":"me",
            "username":"u","password":"secret","sasl_password":"s3"}"##,
        )
        .unwrap();
        assert!(serde_json::from_str::<UpdateSession>(&snapshot(&server)).is_ok());
        for field in ["host", "port", "use_tls", "nick", "username"] {
            let mut damaged = server.clone();
            damaged.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_str::<UpdateSession>(&snapshot(&damaged)).is_err(),
                "a snapshot without server.{field} must not parse"
            );
        }
        // Nor may a session without any server at all.
        assert!(
            serde_json::from_str::<UpdateSession>(
                r##"{"session":{"auto_perform":"x"},"reconnect":true}"##
            )
            .is_err()
        );
    }

    #[test]
    fn update_restart_preserves_disconnected_state_and_old_settings_default_to_auto_updates() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(settings.automatic_updates);
        let before = Session::with_settings(settings.clone());
        let mut after = Session::with_settings(settings);
        after.restore_update_session(before.update_session());
        assert!(!after.is_connecting());
        assert!(!after.is_connected());
    }
}

#[cfg(test)]
mod connection_lifecycle_tests {
    use super::*;

    fn lifecycle_app() -> Session {
        Session::with_settings(Settings {
            // Keep unit tests from writing chat logs to the real config dir.
            logging_enabled: false,
            ..Settings::default()
        })
    }

    #[test]
    fn switching_networks_refreshes_casemapped_ignore_and_highlight_caches() {
        let mut app = lifecycle_app();
        app.ignore_list = vec!["Foo[away]".to_string()];
        app.highlight_words = "Foo[away]".to_string();
        // The old network folds ASCII only, so `[` and `{` are distinct there.
        app.network_support.case_mapping = CaseMapping::Ascii;
        app.update_cached_lowercase();
        assert_eq!(app.ignore_list_lower, ["foo[away]"]);

        // A new network that never advertises CASEMAPPING is rfc1459, so
        // the cached keys must follow the reset default.
        app.reset_session_state();

        assert_eq!(app.network_support.case_mapping, CaseMapping::Rfc1459);
        assert_eq!(app.ignore_list_lower, ["foo{away}"]);
        assert_eq!(app.highlight_words_lower, ["foo{away}"]);
        assert!(app.is_ignored("Foo{away}", None));
        assert!(app.is_ignored("FOO[AWAY]", None));
    }

    #[test]
    fn disconnect_right_after_reconnect_or_server_switch_keeps_the_quit_flushable() {
        for switch_server in [false, true] {
            let mut app = lifecycle_app();
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
            let (msg_tx, msg_rx) = mpsc::channel(4);
            app.test_set_sender(cmd_tx);
            app.test_set_incoming(msg_rx);
            app.test_set_connected(true);

            // /reconnect and /server queue the old connection's QUIT and keep
            // its receiver open until the worker exits...
            if switch_server {
                app.server_host = "other.example".into();
                assert!(app.switch_session_from_form("Changing servers"));
            } else {
                app.request_reconnect_after_close("Reconnecting");
            }
            assert!(matches!(cmd_rx.try_recv(), Ok(IrcCommand::Quit(_))));
            assert!(!msg_tx.is_closed());

            // ...so a /disconnect right behind them, which finds no command
            // sender left to queue a QUIT of its own, must not drop that
            // receiver: that would cancel the writer before the first QUIT is
            // flushed to the server.
            app.request_manual_disconnect(None);
            assert!(
                !msg_tx.is_closed(),
                "switch_server={switch_server}: the receiver was dropped with a QUIT in flight"
            );
            assert!(!app.test_has_incoming());
            assert!(app.test_has_closing_receiver());
            assert!(app.connection.staged_config().is_none());

            app.release_closing_connection(true);
            assert!(msg_tx.is_closed());
        }
    }

    #[test]
    fn manual_disconnect_retires_the_receiver_until_the_worker_has_flushed_quit() {
        let mut app = lifecycle_app();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (msg_tx, msg_rx) = mpsc::channel(4);
        app.test_set_sender(cmd_tx);
        app.test_set_incoming(msg_rx);
        app.test_set_connected(true);

        app.request_manual_disconnect(Some("bye".to_string()));

        assert!(matches!(cmd_rx.try_recv(), Ok(IrcCommand::Quit(Some(reason))) if reason == "bye"));
        // The worker still sees an open channel, so it flushes QUIT instead of
        // being cancelled; whatever it reads meanwhile is never processed.
        assert!(!msg_tx.is_closed());
        assert!(!app.test_has_incoming());
        msg_tx
            .try_send(IrcMessage::parse(":srv NOTICE * :late").unwrap())
            .unwrap();
        app.update_minimal();
        assert!(
            app.server_messages
                .iter()
                .all(|m| !m.content.contains("late"))
        );

        // Still held while the worker runs and within the grace period...
        app.release_closing_connection(false);
        assert!(!msg_tx.is_closed());
        // ...released as soon as the worker has exited...
        app.release_closing_connection(true);
        assert!(msg_tx.is_closed());

        // ...or, for a worker that cannot flush QUIT, after the grace period.
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let (msg_tx, msg_rx) = mpsc::channel(4);
        app.test_set_sender(cmd_tx);
        app.test_set_incoming(msg_rx);
        app.request_manual_disconnect(None);
        app.test_age_closing_grace(std::time::Duration::from_secs(5));
        app.release_closing_connection(false);
        assert!(msg_tx.is_closed());
    }

    #[test]
    fn manual_disconnect_without_a_queued_quit_cancels_the_worker_at_once() {
        let mut app = lifecycle_app();
        let (msg_tx, msg_rx) = mpsc::channel::<IrcMessage>(4);
        app.test_set_incoming(msg_rx);

        app.request_manual_disconnect(None);

        assert!(msg_tx.is_closed());
        assert!(!app.test_has_closing_receiver());
    }

    #[test]
    fn late_registration_error_from_the_closing_connection_keeps_a_staged_switch() {
        let mut app = lifecycle_app();
        assert!(app.start_session_from_form());
        let (tx, _rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        app.apply_unprofiled_endpoint("other.example", "7000", false);
        assert!(app.switch_session_from_form("Changing servers"));
        assert!(app.is_closing() && app.has_pending_connect());

        app.handle_numeric(
            ERR_PASSWDMISMATCH,
            &["nick".to_string(), "Password incorrect".to_string()],
        );

        // The error is shown, but the switch the user asked for still happens.
        assert!(app.is_closing() && app.has_pending_connect());
        assert!(app.connection.staged_config().is_some());
    }

    #[test]
    fn explicit_connects_start_with_a_fresh_auto_reconnect_budget() {
        let mut app = lifecycle_app();
        app.auto_reconnect = true;
        app.max_reconnect_attempts = 3;
        // Auto-reconnect gave up: the budget is spent and a retry is stale.
        app.test_set_attempts(3);
        app.test_set_lost(true);
        app.test_set_failed_at(Some(std::time::Instant::now()));
        assert!(!app.awaiting_reconnect());

        assert!(app.start_session_from_form());
        assert_eq!(app.reconnect_attempts(), 0);
        assert!(app.connection.failed_at().is_none());

        // The same holds for /reconnect and /server.
        app.test_set_attempts(3);
        app.test_set_connecting_flag(false);
        app.request_reconnect_after_close("Reconnecting");
        assert_eq!(app.reconnect_attempts(), 0);
        app.test_set_attempts(3);
        app.test_set_connecting_flag(false);
        app.apply_unprofiled_endpoint("other.example", "7000", false);
        assert!(app.switch_session_from_form("Changing servers"));
        assert_eq!(app.reconnect_attempts(), 0);
    }

    #[test]
    fn background_tick_runs_the_time_based_work_and_marks_the_window_unfocused() {
        let mut app = lifecycle_app();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        app.test_set_connected(true);
        // Focus as last seen by a drawn frame, before the window went away.
        app.window_focused = true;
        app.auto_away_enabled = true;
        app.auto_away_minutes = 5;
        app.last_activity = std::time::Instant::now() - std::time::Duration::from_secs(600);
        app.last_lag_check = std::time::Instant::now() - std::time::Duration::from_secs(31);
        app.pending_auto_perform = Some(vec!["/echo identified".to_string()]);

        app.update_minimal();

        assert!(!app.window_focused);
        assert!(app.auto_away_triggered);
        assert!(app.away_status.is_some());
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Away(Some(_)))));
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Ping(token)) if token == "LAG"));
        assert!(app.pending_auto_perform.is_none());
        assert!(
            app.server_messages
                .iter()
                .any(|message| message.content.contains("identified"))
        );
    }

    #[test]
    fn background_tick_does_not_flap_away_when_the_user_is_active() {
        let mut app = lifecycle_app();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        app.test_set_connected(true);
        app.auto_away_enabled = true;
        app.auto_away_minutes = 5;

        app.update_minimal();

        assert!(app.away_status.is_none());
        assert!(rx.try_recv().is_err());
    }

    fn history_request(placement: HistoryPlacement) -> PendingHistoryRequest {
        PendingHistoryRequest {
            correlation: None,
            placement,
            direction: HistoryDirection::Before,
            remaining: 10,
            loaded: 0,
            shown: 0,
            page_limit: 10,
            announce_arrivals: false,
        }
    }

    #[test]
    fn new_connection_drops_state_that_belonged_to_the_previous_socket() {
        let mut app = lifecycle_app();
        let (tx, _rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        app.test_set_connected(true);
        app.handle_cap_message("ACK", &["draft/chathistory batch".to_string()]);
        assert!(app.chathistory_enabled);
        // Dropped mid-CHATHISTORY: one request unanswered, one batch (and a
        // multiline batch nested in the stream) never closed.
        app.pending_history_requests
            .entry("#chan".to_string())
            .or_default()
            .push_back(history_request(HistoryPlacement::Append));
        app.pending_history_requests
            .entry("#other".to_string())
            .or_default()
            .push_back(history_request(HistoryPlacement::Prepend));
        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +h1 chathistory #other").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(":a!u@h BATCH +m1 draft/multiline #chan").unwrap(),
        );
        app.monitored_users.insert("alice".to_string());
        app.pending_whox
            .push_back(("7".to_string(), "#chan".to_string()));
        app.next_whox_token = 8;
        app.known_users
            .insert("alice".to_string(), KnownUser::default());
        assert!(!app.chathistory_batches.is_empty());
        assert!(!app.multiline_batches.is_empty());
        assert!(!app.pending_history_requests.is_empty());

        app.begin_connection_attempt();

        assert!(app.enabled_caps.is_empty());
        assert!(!app.chathistory_enabled);
        assert!(app.chathistory_batches.is_empty());
        assert!(app.batch_parents.is_empty());
        assert!(app.multiline_batches.is_empty());
        assert!(app.authtoken_batches.is_empty());
        assert!(app.pending_history_requests.is_empty());
        assert!(app.monitored_users.is_empty());
        assert!(app.pending_whox.is_empty());
        assert_eq!(app.next_whox_token, 1);
        // Who we have seen is genuinely cross-connection knowledge.
        assert!(app.known_users.contains_key("alice"));
    }

    #[test]
    fn names_burst_cut_off_by_a_disconnect_is_not_resumed_by_the_next_connection() {
        type EndOfConnection = fn(&mut Session);
        let ways: [(&str, EndOfConnection); 3] = [
            ("lost", |app| app.mark_connection_lost()),
            ("manual disconnect", |app| {
                app.request_manual_disconnect(None)
            }),
            // Nothing ended the old connection explicitly; the new attempt
            // is what discards its leftovers.
            ("new attempt", |app| app.begin_connection_attempt()),
        ];
        for (how, end_connection) in ways {
            let mut app = lifecycle_app();
            app.set_session_nick("me".into(), true);
            let mut channel = Channel::new();
            channel.joined = true;
            app.conversations.insert("#chan".to_string(), channel);

            // The connection dies after a 353 but before its 366.
            app.handle_numeric(
                RPL_NAMREPLY,
                &[
                    "me".into(),
                    "=".into(),
                    "#chan".into(),
                    "stale1 stale2".into(),
                ],
            );
            end_connection(&mut app);

            // The next connection's NAMES starts within the burst gap. It
            // must start from scratch, not append to the dead one's names.
            app.handle_numeric(
                RPL_NAMREPLY,
                &["me".into(), "=".into(), "#chan".into(), "carol".into()],
            );
            app.handle_numeric(RPL_ENDOFNAMES, &["me".into(), "#chan".into()]);

            let nicks: Vec<&str> = app.conversations["#chan"]
                .users
                .iter()
                .map(|user| user.nick.as_str())
                .collect();
            assert_eq!(nicks, ["carol"], "{how}: ghosts from the dead connection");
        }
    }

    #[test]
    fn history_requested_after_a_reconnect_is_not_paired_with_a_dead_requests_placement() {
        let mut app = lifecycle_app();
        let (tx, _rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        app.test_set_connected(true);
        app.handle_cap_message("ACK", &["draft/chathistory".to_string()]);
        // The dead connection's unanswered live-catch-up request.
        app.pending_history_requests
            .entry("#chan".to_string())
            .or_default()
            .push_back(history_request(HistoryPlacement::Append));

        app.begin_connection_attempt();
        app.handle_cap_message("ACK", &["draft/chathistory".to_string()]);
        assert!(app.request_latest_history("#chan", Some(5), HistoryPlacement::Prepend));
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +h2 chathistory #chan").unwrap());

        assert_eq!(
            app.chathistory_batches["h2"].request.placement,
            HistoryPlacement::Prepend
        );
    }
}

/// Show a desktop notification now: notify-send on Linux/BSD, osascript on
/// macOS, a taskbar flash or tray balloon on Windows. Policy (focus, the
/// setting, rate limiting) is the caller's (`App::notify`).
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn show_desktop_notification(title: &str, body: &str) {
    // Use notify-send on Linux/BSD (usually pre-installed on Linux desktops)
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let title = title.to_string();
        let body = body.to_string();
        std::thread::spawn(move || {
            let mut command = std::process::Command::new("notify-send");
            command.args(notify_send_args(&title, &body));
            match wait_for_notification_helper(&mut command) {
                Ok(status) if !status.success() => {
                    tracing::warn!("notify-send exited with status {}", status)
                }
                Err(error) => tracing::warn!("Failed to run notify-send: {}", error),
                Ok(_) => {}
            }
        });
    }

    #[cfg(target_os = "macos")]
    {
        let script = applescript_notification(title, body);
        std::thread::spawn(move || {
            let mut command = std::process::Command::new("osascript");
            command.args(["-e", &script]);
            match wait_for_notification_helper(&mut command) {
                Ok(status) if !status.success() => {
                    tracing::warn!("osascript exited with status {}", status)
                }
                Err(error) => tracing::warn!("Failed to run osascript: {}", error),
                Ok(_) => {}
            }
        });
    }

    // On Windows, flash the taskbar - or, when hidden to the tray (no
    // taskbar button to flash), show a tray balloon instead.
    #[cfg(windows)]
    {
        crate::systray::notify(title, body);
    }
}

/// A random 16-hex-digit id for a network profile.
/// The profile id for a favorite that has none: one saved before ids
/// existed, or by 0.1.4, which also drops the id whenever it rewrites
/// settings.json. Derived from what identifies the favorite, so after a
/// downgrade and upgrade it gets the same id back, and with it its window
/// placements in layout.json.
pub(crate) fn derived_profile_id(name: &str, host: &str, port: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("linefeed-profile\0{name}\0{host}\0{port}"));
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn new_profile_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    format!("{:016x}", hasher.finish())
}

fn rand_suffix() -> u32 {
    suffix_from(std::time::SystemTime::now())
}

/// Four-digit nickname suffix from a clock reading. A clock set before 1970
/// (a dead RTC battery) reads as the epoch instead of aborting startup.
fn suffix_from(time: std::time::SystemTime) -> u32 {
    (time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        % 10000) as u32
}

fn is_irc_nick_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '_' | '[' | ']' | '\\' | '`' | '^' | '{' | '}' | '|' | '~'
        )
}

/// True when `needle_key` occurs in `haystack_key` at IRC-identifier
/// boundaries: the characters immediately before and after each occurrence
/// must not themselves be nick characters. Both arguments must already be
/// casemapped consistently (see `NetworkSupport::canonicalize`).
fn ident_contains(haystack_key: &str, needle_key: &str) -> bool {
    if needle_key.is_empty() {
        return false;
    }
    haystack_key.match_indices(needle_key).any(|(start, _)| {
        let end = start + needle_key.len();
        let before = haystack_key[..start].chars().next_back();
        let after = haystack_key[end..].chars().next();
        !before.is_some_and(is_irc_nick_char) && !after.is_some_and(is_irc_nick_char)
    })
}

/// Hash of every input that shapes a scrollback row's height: the content
/// width and the two font sizes used by message rows. A change in any of them
/// invalidates all cached row heights.
fn row_layout_key(ui: &egui::Ui, content_width: f32) -> u64 {
    let body = ui.style().text_styles[&egui::TextStyle::Body]
        .size
        .to_bits();
    let mono = ui.style().text_styles[&egui::TextStyle::Monospace]
        .size
        .to_bits();
    (content_width.to_bits() as u64) ^ (body as u64).rotate_left(32) ^ (mono as u64).rotate_left(17)
}

/// How far below the scroll area's clip rect scratch measurement rows are
/// parked: far enough that nothing they paint can ever be visible.
const SCRATCH_MEASURE_OFFSET: f32 = 100_000.0;

/// The row's cached advance, measuring it first if the cache is cold or was
/// invalidated by a layout change (width/font size).
///
/// Measurement lays the row out for real inside a scratch child Ui far below
/// the clip rect: nothing paints, but egui runs the full widget layout, so
/// the cached height is exactly what drawing will occupy - including egui's
/// trailing item_spacing inside the row's horizontal layout. Metric-based
/// estimation cannot stay in lockstep with widget rendering; this can.
///
/// The scratch Ui is created with [`Ui::new_child`] rather than
/// `scope_builder` on purpose: scope folds the child's min_rect back into
/// the parent cursor (`advance_cursor_after_rect`), which would balloon the
/// scroll content to ~100k px for every frame that measured a new row -
/// visibly flashing the whole message area blank.
fn row_height(
    ui: &mut egui::Ui,
    msg: &ChatMessage,
    my_nick: &str,
    content_width: f32,
    layout_key: u64,
    stats: &mut ScrollbackStats,
) -> f32 {
    let layout_key = layout_key ^ msg.presentation_revision.wrapping_mul(0x9e3779b97f4a7c15);
    let cached = msg.row_height.get();
    if cached.key == layout_key && cached.height > 0.0 {
        return cached.height;
    }
    stats.measured += 1;
    let scratch_origin = egui::pos2(
        ui.max_rect().left(),
        ui.max_rect().bottom() + SCRATCH_MEASURE_OFFSET,
    );
    let scratch_rect = egui::Rect::from_min_size(scratch_origin, egui::vec2(content_width, 0.0));
    let mut scratch = ui.new_child(egui::UiBuilder::new().max_rect(scratch_rect));
    scratch.disable();
    draw_chat_row(&mut scratch, msg, my_nick);
    let h = scratch.min_rect().height();
    msg.row_height.set(RowHeightEntry {
        key: layout_key,
        height: h,
    });
    h
}

/// Whether a saved message reference names this scrollback row.
fn message_ref_names(reference: &identity::MessageRef, message: &ChatMessage) -> bool {
    reference.archive_id.as_deref() == Some(message.message_key.as_str())
        || reference
            .server_msgid
            .as_ref()
            .is_some_and(|id| message.msgid.as_ref() == Some(id))
}

/// Draw one scrollback row: timestamp, optional indicator, nick, body.
fn draw_chat_row(
    ui: &mut egui::Ui,
    msg: &ChatMessage,
    my_nick: &str,
) -> Option<message_ui::MessageAction> {
    let mut action = None;
    // Current nick only, matching the other display gates (#170): held-nick
    // styling would misattribute a stranger who picked up a nick we
    // vacated, and misattribution is not the safe direction for display.
    let is_own_msg = !msg.is_system && msg.sender.eq_ignore_ascii_case(my_nick);

    let mut render_row = |ui: &mut egui::Ui| {
        if let Some(reply_to) = &msg.reply_to {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                if ui
                    .link(
                        RichText::new(format!(
                            "↪ {}",
                            msg.reply_preview
                                .as_deref()
                                .unwrap_or("Message unavailable · view context")
                        ))
                        .small(),
                    )
                    .clicked()
                {
                    action = Some(message_ui::MessageAction::Reveal(reply_to.clone()));
                }
            });
        }
        ui.horizontal(|ui| {
            if msg.is_highlight {
                ui.label(RichText::new("*").color(Color32::YELLOW).strong());
            }

            ui.label(
                RichText::new(&msg.timestamp)
                    .color(Color32::GRAY)
                    .monospace(),
            );

            if msg.is_system {
                render_segments(ui, msg.render_segments(), Color32::GRAY);
            } else if msg.is_action {
                let action_color = if msg.is_highlight {
                    Color32::YELLOW
                } else if is_own_msg {
                    Color32::from_rgb(100, 180, 220)
                } else {
                    Color32::from_rgb(150, 100, 200)
                };
                let sender = msg.oper.as_ref().map_or_else(
                    || msg.sender.clone(),
                    |oper| format!("{} [{}]", msg.sender, oper),
                );
                ui.label(RichText::new(format!("* {sender} ")).color(action_color));
                render_segments(ui, msg.render_segments(), action_color);
            } else {
                let sender = msg.oper.as_ref().map_or_else(
                    || msg.sender.clone(),
                    |oper| format!("{} [{}]", msg.sender, oper),
                );
                let nick_style = if is_own_msg {
                    RichText::new(format!("<{sender}>")).color(Color32::from_rgb(100, 200, 255))
                } else {
                    RichText::new(format!("<{sender}>")).color(nick_color(&msg.sender))
                };
                ui.label(nick_style);


                let text_color = if msg.is_highlight {
                    Color32::YELLOW
                } else if is_own_msg {
                    Color32::from_rgb(200, 210, 220)
                } else {
                    Color32::WHITE
                };
                render_segments(ui, msg.render_segments(), text_color);
            }
            match &msg.delivery {
                message_ui::MessageDelivery::Queued{..}=>{ui.label(RichText::new("Queued").small().color(Color32::GRAY));},
                message_ui::MessageDelivery::Uncertain=>{ui.label(RichText::new("Delivery uncertain").small().color(Color32::YELLOW)).on_hover_text("The connection ended before this send was confirmed. It will not be sent again automatically.");},
                _=>{}
            }
        });
    };

    // Only highlighted/own rows pay for a Frame (a nested Ui with its own
    // layout pass); plain rows - the overwhelming majority - are laid out
    // directly. Frame::NONE adds no margin, so heights match measurement.
    if msg.is_highlight {
        egui::Frame::NONE
            .fill(Color32::from_rgb(60, 40, 20))
            .show(ui, render_row);
    } else if is_own_msg {
        egui::Frame::NONE
            .fill(Color32::from_rgb(25, 35, 45))
            .show(ui, render_row);
    } else {
        render_row(ui);
    }
    if !msg.reactions.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for group in &msg.reactions {
                let own = group
                    .actors
                    .iter()
                    .any(|actor| actor.eq_ignore_ascii_case(my_nick));
                if ui
                    .selectable_label(
                        own,
                        RichText::new(format!("{} {}", group.emoji, group.actors.len())).small(),
                    )
                    .on_hover_text(group.actors.join(", "))
                    .clicked()
                {
                    action = Some(message_ui::MessageAction::React(
                        msg.message_key.clone(),
                        group.emoji.clone(),
                    ));
                }
            }
        });
    }
    action
}

/// Statistics from one virtualized scrollback pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbackStats {
    /// Rows actually laid out this frame (visible band plus straddlers).
    pub drawn: usize,
    /// Heights that had to be measured fresh (cold cache or layout change).
    pub measured: usize,
    pub total_rows: usize,
}

/// Escape `&`, `<` and `>` for a desktop notification. Many notification
/// daemons render `<a href>` and `<img>` markup in the body, so message text
/// must not be able to inject links or images. `&` goes first so the entities
/// added here are not themselves escaped.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn notification_markup_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// notify-send arguments. `--` ends option parsing so a title or body that
/// starts with `-` (a PM beginning `--hint=...`) is shown as text, not parsed
/// as an option (#269). The text is markup-escaped (see
/// `notification_markup_escape`).
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn notify_send_args(title: &str, body: &str) -> [String; 7] {
    [
        "-a".to_string(),
        "Linefeed".to_string(),
        "-t".to_string(),
        "5000".to_string(),
        "--".to_string(),
        notification_markup_escape(title),
        notification_markup_escape(body),
    ]
}

/// Escape text for an AppleScript string literal. Backslashes go first so the
/// backslash added before a quote is not itself doubled; a nick ending in a
/// backslash would otherwise escape the closing quote of the literal (#269).
#[cfg(any(target_os = "macos", test))]
fn applescript_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(any(target_os = "macos", test))]
fn applescript_notification(title: &str, body: &str) -> String {
    format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(body),
        applescript_escape(title)
    )
}

#[cfg(unix)]
fn wait_for_notification_helper(
    command: &mut std::process::Command,
) -> std::io::Result<std::process::ExitStatus> {
    // `status` waits for and reaps the child. Dropping a handle returned by
    // `spawn` would leave an exited helper as a zombie on Unix.
    command.status()
}

impl Session {
    /// Never configured or connected: a status window opened with "New
    /// connection" whose Connect dialog was dismissed.
    pub(crate) fn is_pristine(&self) -> bool {
        self.active_session.is_none()
            && self.connection.staged_config().is_none()
            && self.is_fully_stopped()
            && self.conversations.is_empty()
    }

    fn session_from_form(&self) -> Result<SessionConfig, String> {
        self.upload_settings.validate()?;
        // Mirror connect()'s wire-value rules via the shared validator so the
        // dialog rejects up front what the connection would fail on later
        // (#180).
        let nickname = self.nickname.trim();
        crate::irc::client::IrcClient::validate_registration_fields(
            nickname,
            &self.username,
            &self.realname,
            &self.password,
        )
        .map_err(|error| format!("Nickname/identity: {error}"))?;
        let mut host = self.server_host.trim();
        if let Some(inner) = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
        {
            host = inner;
        }
        if host.is_empty() {
            return Err("Server host cannot be empty".to_string());
        }
        let port = commands::parse_server_port(&self.server_port)?;
        let has_sasl_username = !self.sasl_username.is_empty();
        let has_sasl_password = !self.sasl_password.is_empty();
        if has_sasl_username != has_sasl_password {
            return Err("SASL requires both a username and password".to_string());
        }
        let has_sasl_credentials = has_sasl_username && has_sasl_password;
        let has_client_cert = !self.tls_client_cert_path.trim().is_empty();
        if let Some(binding) = &self.bouncer {
            binding.validate()?;
            if binding.provider == bouncer::BouncerProvider::Soju
                && !has_sasl_credentials
                && !has_client_cert
            {
                return Err("soju discovery and network binding require SASL credentials or a client certificate".into());
            }
            if binding.provider == bouncer::BouncerProvider::Znc
                && !has_sasl_credentials
                && !has_client_cert
                && self.password.is_empty()
            {
                return Err("A ZNC profile requires bouncer credentials".into());
            }
            if has_sasl_credentials && !binding.account.eq_ignore_ascii_case(&self.sasl_username) {
                return Err("SASL username must match the bouncer base account".into());
            }
        }
        if has_client_cert && !self.use_tls {
            return Err("A TLS client certificate requires TLS to be enabled".to_string());
        }
        match self.sasl_mechanism {
            SaslMechanism::Plain | SaslMechanism::ScramSha256 if !has_sasl_credentials => {
                return Err(
                    "The selected SASL mechanism requires a username and password".to_string(),
                );
            }
            SaslMechanism::External if !has_client_cert => {
                return Err("SASL EXTERNAL requires a TLS client certificate".to_string());
            }
            _ => {}
        }
        if self.pre_away_message.contains(['\r', '\n']) || self.pre_away_message.len() > 300 {
            return Err("Pre-away message must be one IRC line of at most 300 bytes".to_string());
        }
        let persistence_profile = self.persistence_profile.trim();
        if !persistence_profile.is_empty()
            && (persistence_profile.len() > 32
                || !persistence_profile
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
        {
            return Err("Persistence profile must be 1-32 letters, digits, '_' or '-'".to_string());
        }

        Ok(SessionConfig {
            upload_settings: self.upload_settings.clone(),
            server: ServerConfig {
                host: host.to_string(),
                port,
                use_tls: self.use_tls,
                accept_invalid_certs: self.accept_invalid_certs,
                nick: self.nickname.trim().to_string(),
                username: self.username.clone(),
                realname: self.realname.clone(),
                password: if self.password.is_empty() {
                    None
                } else {
                    Some(self.password.clone())
                },
                sasl_username: if self.sasl_username.is_empty() {
                    None
                } else {
                    Some(self.sasl_username.clone())
                },
                sasl_password: if self.sasl_password.is_empty() {
                    None
                } else {
                    Some(self.sasl_password.clone())
                },
                sasl_mechanism: self.sasl_mechanism,
                sasl_required: self.sasl_required,
                bouncer: self.bouncer.clone(),
                tls_client_cert_path: if self.tls_client_cert_path.trim().is_empty() {
                    None
                } else {
                    Some(self.tls_client_cert_path.trim().to_string())
                },
                tls_client_key_path: if self.tls_client_key_path.trim().is_empty() {
                    None
                } else {
                    Some(self.tls_client_key_path.trim().to_string())
                },
                pre_away_message: if self.pre_away_message.is_empty() {
                    None
                } else {
                    Some(self.pre_away_message.clone())
                },
                persistence_profile: if persistence_profile.is_empty() {
                    None
                } else {
                    Some(persistence_profile.to_string())
                },
            },
            auto_join_channels: self.auto_join_channels.clone(),
            auto_perform: self.auto_perform.clone(),
            set_invisible: self.set_invisible,
            profile_id: self.form_profile.clone(),
        })
    }

    fn activate_session(&mut self, session: SessionConfig) {
        self.profile_id = session.profile_id.clone();
        let endpoint_changed = self
            .active_session
            .as_ref()
            .is_some_and(|active| active.logical_network_key() != session.logical_network_key());
        if endpoint_changed {
            // Do not let buffered messages from the old socket recreate tabs or
            // logs after the new endpoint identity becomes active.
            self.connection.clear();
            self.reset_session_state();
        }
        // Session start. On an endpoint change the previous connection's
        // nick history is dropped (reset_session_state) and the configured
        // nick is adopted unrecorded (#166). On a same-endpoint reconnect
        // the outgoing nick WAS ours on this connection lineage — record it
        // so replay attribution survives (#168) — but only when the services
        // identity (SASL account, or the client certificate for EXTERNAL) is
        // unchanged; the nickname is precisely what legitimately changes, so
        // it never breaks the lineage, while a changed identity is a new
        // lineage whose nicks mean someone else (#171). An unconfirmed nick
        // (fresh or speculative) is adopted unrecorded; a re-applied
        // confirmed nick stays confirmed (#170).
        let identity_changed = self.active_session.as_ref().is_some_and(|active| {
            active.services_identity_key() != session.services_identity_key()
        });
        if !endpoint_changed && identity_changed {
            // The flag goes too: the outgoing nick was the other account's
            // holder, and leaving it confirmed would let the record below
            // re-insert it (#171). The outgoing session's away state belongs
            // to its account, not the new one (#172).
            self.clear_nick_lineage();
            self.away_status = None;
            self.auto_away_triggered = false;
        }
        // A fresh connection gets a fresh 433 walkaway budget: one exhausted
        // ladder must not brick every later reconnect to this endpoint
        // (#172).
        self.nick_retry_attempts = 0;
        let accepted =
            self.nick_confirmed && self.identifiers_equal(&session.server.nick, &self.my_nick);
        self.set_session_nick(session.server.nick.clone(), accepted);
        self.active_session = Some(session);
        self.connection_error = None;
    }

    pub(crate) fn start_session_from_form(&mut self) -> bool {
        match self.session_from_form() {
            Ok(session) => {
                self.connect_validated_session(session, "Connecting");
                true
            }
            Err(error) => {
                self.connection_error = Some(error.clone());
                self.show_connect_dialog = true;
                self.add_server_message(ChatMessage::system_fmt(&error, &self.timestamp_format));
                false
            }
        }
    }

    pub(crate) fn validate_connection_form(&mut self) -> bool {
        match self.session_from_form() {
            Ok(_) => {
                self.connection_error = None;
                true
            }
            Err(error) => {
                self.connection_error = Some(error);
                false
            }
        }
    }

    pub(crate) fn switch_session_from_form(&mut self, reason: &str) -> bool {
        let session = match self.session_from_form() {
            Ok(session) => session,
            Err(error) => {
                self.connection_error = Some(error.clone());
                self.add_server_message(ChatMessage::system_fmt(&error, &self.timestamp_format));
                return false;
            }
        };

        self.connect_validated_session(session, reason);
        true
    }

    #[cfg(test)]
    pub(crate) fn active_server_config(&self) -> Option<ServerConfig> {
        self.active_session
            .as_ref()
            .map(|session| session.server.clone())
    }

    fn active_log_context(&self) -> Option<(String, String, bool)> {
        self.active_session.as_ref().map(|session| {
            (
                session.log_identity(),
                session.server.host.clone(),
                session.allows_legacy_log_migration(),
            )
        })
    }

    fn active_endpoint_label(&self) -> String {
        self.active_session
            .as_ref()
            .map(SessionConfig::endpoint_label)
            .unwrap_or_else(|| format!("{}:{}", self.server_host, self.server_port))
    }

    pub(crate) fn note_unprofiled_endpoint_edit(&mut self) {
        self.clear_endpoint_credentials();
        self.connection_error = None;
    }

    pub(crate) fn apply_unprofiled_endpoint(&mut self, host: &str, port: &str, use_tls: bool) {
        // Selecting a preset is an unprofiled action even when it happens to
        // name the same endpoint. Only loading an explicit favorite/profile is
        // allowed to carry endpoint-scoped credentials and automation.
        self.clear_endpoint_credentials();
        self.server_host = host.to_string();
        self.server_port = port.to_string();
        self.use_tls = use_tls;
        self.connection_error = None;
    }

    pub(crate) fn connection_error(&self) -> Option<&str> {
        self.connection_error.as_deref()
    }

    fn current_session_automation(&self) -> (bool, String, String) {
        if let Some(binding) = self
            .active_session
            .as_ref()
            .and_then(|session| session.server.bouncer.as_ref())
        {
            if binding.is_control() {
                return (false, String::new(), String::new());
            }
            if let Some(session) = &self.active_session {
                return (
                    session.set_invisible,
                    String::new(),
                    session.auto_perform.clone(),
                );
            }
        }
        self.active_session
            .as_ref()
            .map(|session| {
                (
                    session.set_invisible,
                    session.auto_join_channels.clone(),
                    session.auto_perform.clone(),
                )
            })
            .unwrap_or_else(|| {
                (
                    self.set_invisible,
                    self.auto_join_channels.clone(),
                    self.auto_perform.clone(),
                )
            })
    }

    pub(crate) fn active_endpoint_key(&self) -> Option<EndpointKey> {
        self.active_session
            .as_ref()
            .map(SessionConfig::endpoint_key)
    }

    pub(crate) fn logical_network_key(&self) -> Option<identity::LogicalNetworkKey> {
        self.active_session
            .as_ref()
            .map(SessionConfig::logical_network_key)
    }

    #[cfg(test)]
    fn form_server_config_for_tests(&self) -> Result<ServerConfig, String> {
        self.session_from_form().map(|session| session.server)
    }

    /// Rename bookkeeping for every NICK (self or peer): per-channel user
    /// renames with a notice, known-user/monitor re-keying, and query-tab
    /// re-keying (#173).
    fn apply_nick_change(&mut self, old_nick: &str, new_nick: &str) {
        let sys_msg = ChatMessage::system(&format!("{} is now known as {}", old_nick, new_nick));
        let max = self.max_scrollback;
        for channel in self.conversations.values_mut() {
            if channel.has_user(old_nick) {
                channel.push_trimmed(sys_msg.clone(), max);
                channel.rename_user(old_nick, new_nick);
            }
        }
        let old_identity_key = self.network_support.canonicalize(old_nick);
        if let Some(mut identity) = self.known_users.remove(&old_identity_key) {
            identity.nick = new_nick.to_string();
            let new_key = self.network_support.canonicalize(new_nick);
            if let Some(existing) = self.known_users.get(&new_key) {
                // Do not silently overwrite the destination identity (#194):
                // keep its populated fields, fill gaps from the renamed user.
                let mut merged = existing.clone();
                if merged.username.is_none() {
                    merged.username = identity.username;
                }
                if merged.hostname.is_none() {
                    merged.hostname = identity.hostname;
                }
                if merged.account.is_none() {
                    merged.account = identity.account;
                }
                if merged.realname.is_none() {
                    merged.realname = identity.realname;
                }
                if merged.away.is_none() {
                    merged.away = identity.away;
                }
                merged.online = merged.online || identity.online;
                self.known_users.insert(new_key, merged);
            } else {
                self.known_users.insert(new_key, identity);
            }
        }
        if self.monitored_users.remove(&old_identity_key) {
            self.monitored_users
                .insert(self.network_support.canonicalize(new_nick));
        }
        if let Some(id) = self.id_for_target(old_nick)
            && self
                .target_name(id)
                .is_some_and(|name| !self.is_channel_name(name))
        {
            self.rename_conversation(id, new_nick);
            if let Some(channel) = self.conversations.channel_mut(id) {
                channel.push_trimmed(sys_msg, max);
            }
        }
    }

    /// Merge data without moving the surviving identity or its editing state.
    fn merge_conversation_payload(&mut self, survivor: ConversationId, mut retired: Channel) {
        let Some(channel) = self.conversations.channel_mut(survivor) else {
            return;
        };
        while let Some(message) = retired.messages.pop_back() {
            let duplicate = message.msgid.as_ref().is_some_and(|msgid| {
                channel
                    .messages
                    .iter()
                    .any(|row| row.msgid.as_ref() == Some(msgid))
            });
            if !duplicate {
                channel.messages.push_front(message);
            }
        }
        channel.unread = channel.unread.saturating_add(retired.unread);
        channel.joined |= retired.joined;
        channel.history_loaded |= retired.history_loaded;
        channel.merge_metadata_from(&retired);
        channel.merge_users(retired.users);
        if self.max_scrollback > 0 {
            while channel.messages.len() > self.max_scrollback {
                channel.messages.pop_front();
            }
        }
    }

    fn redirect_conversation(&mut self, survivor: ConversationId, retired: ConversationId) {
        if self.current_conversation == Some(retired) {
            self.current_conversation = Some(survivor);
        }
        if self.channel_info_target == Some(retired) {
            self.channel_info_target = Some(survivor);
        }
        if self.pending_cycles.remove(&retired) {
            self.pending_cycles.insert(survivor);
        }
        for request in self.pending_history_requests.values_mut().flatten().chain(
            self.chathistory_batches
                .values_mut()
                .map(|batch| &mut batch.request),
        ) {
            if let Some(correlation) = &mut request.correlation
                && correlation.owner == Some(retired)
            {
                correlation.owner = Some(survivor);
            }
        }
        self.redirect_automatic_history(survivor, retired);
        self.events
            .push(SessionEvent::ConversationMerged { survivor, retired });
    }

    fn rename_conversation(&mut self, id: ConversationId, new: &str) {
        let Some(old) = self.target_name(id).map(str::to_owned) else {
            return;
        };
        if let Some((retired, channel)) = self.conversations.rename(id, new) {
            self.merge_conversation_payload(id, channel);
            self.redirect_conversation(id, retired);
        }
        self.notify_target_rename(id, &old, new);
    }

    fn notify_target_rename(&mut self, id: ConversationId, old: &str, new: &str) {
        for request in self.pending_history_requests.values_mut().flatten().chain(
            self.chathistory_batches
                .values_mut()
                .map(|batch| &mut batch.request),
        ) {
            if let Some(correlation) = &mut request.correlation
                && correlation.owner == Some(id)
            {
                for alias in [old, new] {
                    if correlation.target != alias
                        && !correlation.aliases.iter().any(|value| value == alias)
                    {
                        correlation.aliases.push(alias.to_string());
                    }
                }
            }
        }
        let support = &self.network_support;
        if self
            .update_selected
            .as_deref()
            .is_some_and(|target| support.identifiers_equal(target, old))
        {
            self.update_selected = Some(new.to_string());
        }
        for target in &mut self.update_join_targets {
            if support.identifiers_equal(target, old) {
                *target = new.to_string();
            }
        }
        if let Some(rejoins) = self.pending_rejoins.as_mut() {
            for (target, _) in rejoins {
                if support.identifiers_equal(target, old) {
                    *target = new.to_string();
                }
            }
        }
        for target in &mut self.pending_parts {
            if support.identifiers_equal(target, old) {
                *target = new.to_string();
            }
        }
        self.events.push(SessionEvent::ConversationRenamed {
            id,
            old: old.to_string(),
            new: new.to_string(),
        });
    }

    pub fn handle_incoming_message(&mut self, msg: IrcMessage) {
        if self.collect_multiline_line(&msg) {
            self.scroll_to_bottom = true;
            return;
        }
        if self.collect_authtoken_line(&msg) {
            self.scroll_to_bottom = true;
            return;
        }
        let history_batch = msg
            .get_batch()
            .and_then(|batch_id| self.containing_history_batch(&batch_id));
        if let Some(batch_id) = history_batch.as_deref()
            && let Some(event) = self.history_event_message(&msg)
        {
            self.queue_chathistory_message(batch_id, event);
            self.scroll_to_bottom = true;
            return;
        }
        if history_batch.is_none() && self.observe_upload_protocol(&msg) {
            return;
        }
        if history_batch.is_none() && self.observe_history_authentication(&msg) {
            return;
        }
        match &msg.command {
            IrcCommand::Privmsg(target, content) => {
                let sender = msg.get_sender_nick().unwrap_or_else(|| "???".to_string());
                let routed_target = self.strip_status_prefix(target).to_string();

                // Check if sender is ignored
                if self.is_ignored(&sender, msg.prefix.as_deref()) {
                    tracing::debug!("Ignoring message from {}", sender);
                    return;
                }

                // Handle CTCP requests (except ACTION which is displayed as a message)
                if history_batch.is_none() && self.handle_ctcp_request(&sender, content) {
                    return; // CTCP was handled, don't display as regular message
                }

                let action_text = ctcp_action_text(content);

                // Check if the message mentions our nick (case-insensitive word boundary check)
                let highlight_target = if self.identifiers_equal(&routed_target, &self.my_nick) {
                    &sender
                } else {
                    &routed_target
                };
                let is_highlight = self.attention_highlight(highlight_target, content);

                // Get server-time from IRCv3 tags if available, converted to
                // local time and rendered in the user's timestamp format.
                let server_time = msg
                    .get_server_time()
                    .map(|epoch| epoch_time_formatted(epoch, &self.timestamp_format));
                let server_time_raw = msg.get_server_time_raw();
                let msgid = msg.get_msgid();
                let account = msg.get_account();

                let fmt = &self.timestamp_format;
                // The gap marker is a client-only tag any user can attach, so
                // only a server-sourced line inside a history batch counts.
                let is_gap_marker = history_batch.is_some()
                    && msg
                        .prefix
                        .as_deref()
                        .is_some_and(|prefix| !prefix.contains(['!', '@']))
                    && msg.get_tag("+evilnet.github.io/chathistory-gap").is_some();
                let chat_msg = if is_gap_marker {
                    ChatMessage::system_fmt(content, fmt).with_server_time(server_time)
                } else if let Some(action_text) = action_text {
                    if is_highlight {
                        ChatMessage::action_highlighted_fmt(&sender, action_text, fmt)
                            .with_server_time(server_time)
                    } else {
                        ChatMessage::action_fmt(&sender, action_text, fmt)
                            .with_server_time(server_time)
                    }
                } else if is_highlight {
                    ChatMessage::highlighted_fmt(&sender, content, fmt)
                        .with_server_time(server_time)
                } else {
                    ChatMessage::new_fmt(&sender, content, fmt).with_server_time(server_time)
                }
                .with_irc_metadata(server_time_raw, msgid, account)
                .with_oper(msg.get_oper())
                .with_reply_to(msg.get_reply());

                if let Some(batch_id) = &history_batch {
                    self.queue_chathistory_message(batch_id, chat_msg.without_logging());
                    self.scroll_to_bottom = true;
                    return;
                }

                // Determine target channel/query
                let is_pm = !self.is_channel_name(&routed_target)
                    && self.identifiers_equal(&routed_target, &self.my_nick);
                let target_name = if self.is_channel_name(&routed_target) {
                    routed_target
                } else if is_pm {
                    // Private message to us - use sender as channel
                    sender.clone()
                } else {
                    target.clone()
                };
                tracing::debug!(
                    "PRIVMSG: target={:?} sender={:?} is_pm={} target_name={:?}",
                    target,
                    sender,
                    is_pm,
                    target_name
                );

                // Send desktop notification for highlights and PMs
                self.notify_incoming_message(
                    &sender,
                    &target_name,
                    &truncate_chars(content, 50),
                    is_pm,
                    is_highlight,
                    &chat_msg,
                );
                let is_from_self = self.identifiers_equal(&sender, &self.my_nick);

                if !is_from_self || !self.merge_server_echo(&target_name, &chat_msg) {
                    self.add_message_to_channel(&target_name, chat_msg);
                }
            }

            IrcCommand::Notice(target, content) => {
                let sender = msg
                    .get_sender_nick()
                    .unwrap_or_else(|| "Server".to_string());
                let routed_target = self.strip_status_prefix(target).to_string();

                // NOTICE uses a decorated local confirmation row rather than a
                // normal chat row, so its server echo cannot be merged by
                // identity. Suppress only our own negotiated echo — but not
                // inside a CHATHISTORY replay batch, where our own rows must
                // reach the queue branch just like the PRIVMSG arm's do.
                // Current nick only: echoes always arrive under the nick we
                // hold now, while a squatted former nick belongs to a peer
                // whose notices must still render (#168).
                if history_batch.is_none()
                    && self.enabled_caps.contains("echo-message")
                    && self.identifiers_equal(&sender, &self.my_nick)
                {
                    return;
                }

                // Check if sender is ignored (but not server notices)
                if msg.prefix.is_some() && self.is_ignored(&sender, msg.prefix.as_deref()) {
                    tracing::debug!("Ignoring notice from {}", sender);
                    return;
                }

                if let Some(batch_id) = &history_batch {
                    let server_time = msg
                        .get_server_time()
                        .map(|epoch| epoch_time_formatted(epoch, &self.timestamp_format));
                    let replayed = ChatMessage::system_fmt(
                        &format!("-{sender}- {content}"),
                        &self.timestamp_format,
                    )
                    .with_server_time(server_time)
                    .with_irc_metadata(
                        msg.get_server_time_raw(),
                        msg.get_msgid(),
                        msg.get_account(),
                    )
                    .without_logging();
                    self.queue_chathistory_message(batch_id, replayed);
                    self.scroll_to_bottom = true;
                    return;
                }

                // Check for a CTCP reply (the closing \x01 is optional).
                if let Some((ctcp_cmd, ctcp_reply)) = parse_ctcp(content) {
                    // Handle CTCP PING reply to show latency
                    if ctcp_cmd.eq_ignore_ascii_case("PING") {
                        if let Ok(sent_time) = ctcp_reply.parse::<u128>() {
                            use std::time::{SystemTime, UNIX_EPOCH};
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis();
                            let latency = now.saturating_sub(sent_time);
                            self.add_message_to_current(ChatMessage::system(&format!(
                                "[CTCP PING reply] {} - {}ms",
                                sender, latency
                            )));
                        } else {
                            self.add_message_to_current(ChatMessage::system(&format!(
                                "[CTCP PING reply] {} - {}",
                                sender, ctcp_reply
                            )));
                        }
                    } else {
                        // Other CTCP replies (VERSION, TIME, etc.)
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "[CTCP {} reply] {} - {}",
                            ctcp_cmd, sender, ctcp_reply
                        )));
                    }
                    return;
                }

                let chat_msg = ChatMessage::system(&format!("-{}- {}", sender, content))
                    .with_irc_metadata(
                        msg.get_server_time_raw(),
                        msg.get_msgid(),
                        msg.get_account(),
                    );

                // A prefix without `!`/`@` (or none) names a server, not a user.
                let from_server = msg
                    .prefix
                    .as_deref()
                    .is_none_or(|prefix| !prefix.contains('!') && !prefix.contains('@'));
                let is_private = !self.is_channel_name(&routed_target)
                    && self.identifiers_equal(&routed_target, &self.my_nick);
                if routed_target == "*" || !self.is_connected() || (is_private && from_server) {
                    self.add_server_message(chat_msg);
                } else {
                    // For private notices (target is our nick), route to sender's window
                    // This handles NickServ, X3, and other service responses
                    let target_name = if is_private {
                        sender.clone() // Route to sender's query window
                    } else {
                        routed_target
                    };
                    self.add_message_to_channel(&target_name, chat_msg);
                }
            }

            IrcCommand::StatusMessage(status, target, content) => {
                let sender = msg
                    .get_sender_nick()
                    .unwrap_or_else(|| "Server".to_string());
                if msg.prefix.is_some() && self.is_ignored(&sender, msg.prefix.as_deref()) {
                    tracing::debug!("Ignoring status message from {}", sender);
                    return;
                }
                let target = self.strip_status_prefix(target).to_string();
                let label = match status {
                    '@' => "ops",
                    '%' => "halfops",
                    '+' => "voices",
                    _ => "members",
                };
                let chat_msg = ChatMessage::system_fmt(
                    &format!("-{sender}/{label}- {content}"),
                    &self.timestamp_format,
                )
                .with_irc_metadata(
                    msg.get_server_time_raw(),
                    msg.get_msgid(),
                    msg.get_account(),
                );
                self.add_message_to_channel(&target, chat_msg);
            }

            IrcCommand::Tagmsg(target) => {
                self.observe_tagmsg(&msg, target, history_batch.is_some());
            }

            IrcCommand::StandardReply {
                kind,
                command,
                code,
                context,
                description,
            } => {
                let context_text = context.join(" ");
                let text = if context_text.is_empty() {
                    format!("[{kind} {command}/{code}] {description}")
                } else {
                    format!("[{kind} {command}/{code}] {context_text}: {description}")
                };
                let target = context
                    .iter()
                    .flat_map(|value| value.split_whitespace())
                    .find(|value| self.is_channel_name(value) || self.channel_key(value).is_some())
                    .map(str::to_string);
                let message = ChatMessage::system_fmt(&text, &self.timestamp_format)
                    .with_irc_metadata(msg.get_server_time_raw(), None, None);
                if command.eq_ignore_ascii_case("CHATHISTORY") && kind.eq_ignore_ascii_case("FAIL")
                {
                    let words: Vec<_> = context
                        .iter()
                        .flat_map(|value| value.split_whitespace())
                        .collect();
                    let subcommand = words
                        .first()
                        .and_then(|word| HistorySubcommand::parse(word));
                    let reply_target = if subcommand == Some(HistorySubcommand::Targets) {
                        ""
                    } else {
                        words.get(1).copied().unwrap_or("")
                    };
                    let label = self.response_label(&msg);
                    let failed_request = if subcommand.is_some() || label.is_some() {
                        self.take_history_request(reply_target, subcommand, label.as_deref())
                    } else {
                        None
                    };
                    let owner_target = failed_request
                        .as_ref()
                        .and_then(|request| request.correlation.as_ref())
                        .and_then(|correlation| correlation.owner)
                        .and_then(|owner| self.target_name(owner))
                        .map(str::to_owned);
                    if let Some(owner_target) = owner_target {
                        if failed_request
                            .is_some_and(|request| request.placement == HistoryPlacement::Prepend)
                            && let Some(mut history) =
                                self.load_local_history_fallback(&owner_target)
                            && let Some(channel) = self.conversations.get_mut(&owner_target)
                        {
                            history.append(&mut channel.messages);
                            channel.messages = history;
                            while channel.messages.len() > self.max_scrollback {
                                channel.messages.pop_front();
                            }
                        }
                        self.add_message_to_channel(&owner_target, message);
                    } else {
                        // Keep genuine server errors visible without reopening
                        // a closed tab or writing into its same-named successor.
                        self.add_server_message(message);
                    }
                } else if let Some(target) = &target {
                    self.add_message_to_channel(target, message);
                } else {
                    self.add_server_message(message);
                }
            }

            IrcCommand::Markread(target, timestamp) => {
                if let Some(key) = self.channel_key(target)
                    && let Some(channel) = self.conversations.get_mut(&key)
                {
                    channel.read_marker = timestamp.clone();
                }
            }

            IrcCommand::Redact(target, msgid, reason) => {
                // A peer's DM redaction is relayed with our own nick as the
                // target, exactly like the PRIVMSG it retracts, while the query
                // tab is named after the peer. Resolve through the negotiated
                // casemapping and never create a tab here.
                let routed = self.strip_status_prefix(target);
                let name = match msg.get_sender_nick() {
                    Some(sender)
                        if !self.is_channel_name(routed)
                            && self.identifiers_equal(routed, &self.my_nick) =>
                    {
                        sender
                    }
                    _ => routed.to_string(),
                };
                let key = self.channel_key(&name);
                let target = key.clone().unwrap_or(name);
                let target = target.as_str();
                let reference = key
                    .as_deref()
                    .and_then(|key| self.conversations.get(key))
                    .and_then(|channel| {
                        channel
                            .messages
                            .iter()
                            .find(|message| message.msgid.as_ref() == Some(msgid))
                    })
                    .map(|message| self.live_message_reference(target, message))
                    .unwrap_or_else(|| identity::MessageRef {
                        conversation: self.conversation_key(target),
                        archive_id: None,
                        server_msgid: Some(msgid.clone()),
                        server_time: None,
                    });
                if let Some(archive) = &self.archive_client {
                    archive.redact_reference(&reference);
                }
                self.events.push(SessionEvent::MessageRedacted(reference));
                if let Some(key) = &key
                    && let Some(channel) = self.conversations.get_mut(key)
                {
                    let removed = channel.redact_message(msgid);
                    let reason = reason
                        .as_deref()
                        .filter(|reason| !reason.is_empty())
                        .map(|reason| format!(": {reason}"))
                        .unwrap_or_default();
                    channel.push_trimmed(
                        ChatMessage::system_fmt(
                            &if removed {
                                format!("A message was redacted{reason}")
                            } else {
                                format!("Message {msgid} was redacted{reason}")
                            },
                            &self.timestamp_format,
                        )
                        .without_logging(),
                        self.max_scrollback,
                    );
                }
                self.refresh_reply_previews(target);
            }

            IrcCommand::Rename(old, new, reason) => {
                if let Some(id) = self.id_for_target(old) {
                    self.rename_conversation(id, new);
                    if let Some(channel) = self.conversations.channel_mut(id) {
                        channel.push_trimmed(
                            ChatMessage::system_fmt(
                                &format!(
                                    "Channel renamed from {old} to {new}{}",
                                    reason
                                        .as_deref()
                                        .filter(|reason| !reason.is_empty())
                                        .map(|reason| format!(": {reason}"))
                                        .unwrap_or_default()
                                ),
                                &self.timestamp_format,
                            ),
                            self.max_scrollback,
                        );
                    }
                }
            }

            IrcCommand::Relocate(old, new, reason) => {
                let reason = reason
                    .as_deref()
                    .filter(|reason| !reason.is_empty())
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default();
                self.add_message_to_channel(
                    old,
                    ChatMessage::system_fmt(
                        &format!("Channel has moved to {new}{reason}; use /join {new} to follow"),
                        &self.timestamp_format,
                    ),
                );
            }

            IrcCommand::Setname(realname) => {
                let sender = msg.get_sender_nick().unwrap_or_else(|| "???".to_string());
                self.update_user_identity(
                    &sender,
                    None,
                    None,
                    None,
                    Some(Some(realname.clone())),
                    None,
                    None,
                    None,
                );
                let notice = ChatMessage::system_fmt(
                    &format!("{sender} changed their realname to {realname}"),
                    &self.timestamp_format,
                );
                let max = self.max_scrollback;
                for channel in self.conversations.values_mut() {
                    if channel.has_user(&sender) {
                        channel.set_user_realname(&sender, Some(realname.clone()));
                        channel.push_trimmed(notice.clone(), max);
                    }
                }
            }

            IrcCommand::Extension(name, params) => {
                self.observe_bouncer_message(name, params);
                let text = if name.eq_ignore_ascii_case("TOKEN")
                    && params
                        .first()
                        .is_some_and(|subcommand| subcommand.eq_ignore_ascii_case("SERVICE"))
                {
                    let key = params.get(1).map(String::as_str).unwrap_or("?");
                    let url = params.get(2).map(String::as_str).unwrap_or("");
                    let description = params.get(3).map(String::as_str).unwrap_or("");
                    format!("Token service {key} ({url}): {description}")
                } else if name.eq_ignore_ascii_case("TOKEN")
                    && params
                        .first()
                        .is_some_and(|subcommand| subcommand.eq_ignore_ascii_case("GENERATE"))
                {
                    let service = params.get(1).map(String::as_str).unwrap_or("?");
                    let token = params.get(2).map(String::as_str).unwrap_or("");
                    format!("[TOKEN GENERATE {service}] {token}")
                } else {
                    format!("[{name}] {}", params.join(" "))
                };
                self.add_server_message(
                    ChatMessage::system_fmt(&text, &self.timestamp_format).without_logging(),
                );
            }

            IrcCommand::ChathistoryTarget(target, timestamp) => {
                self.add_server_message(
                    ChatMessage::system_fmt(
                        &format!("History target {target}: last activity {timestamp}"),
                        &self.timestamp_format,
                    )
                    .without_logging(),
                );
            }

            IrcCommand::Join(channel, _key, account, realname) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                let (username, hostname) = Self::prefix_user_host(msg.prefix.as_deref());
                if self.identifiers_equal(&sender, &self.my_nick) {
                    if account
                        .as_deref()
                        .is_some_and(|account| !account.is_empty() && account != "*")
                    {
                        self.confirm_history_authentication();
                    }
                    self.forget_rejoin_membership(channel);
                    // We joined a channel
                    let existing_key = self.channel_key(channel);
                    let cycled = self
                        .id_for_target(channel)
                        .is_some_and(|id| self.pending_cycles.remove(&id));
                    let is_new_channel = existing_key.is_none() || cycled;
                    // A key sent with our JOIN belongs to the tab whether it is
                    // new or retained (kicked/disconnected), so consume it
                    // here rather than only for new tabs (#250).
                    let pending_key = self
                        .pending_channel_keys
                        .remove(&self.network_support.canonicalize(channel));
                    if is_new_channel {
                        let mut new_channel = Channel::new();
                        new_channel.set_case_mapping(self.network_support.case_mapping);
                        new_channel.set_prefix_schema(
                            &self.network_support.prefix_modes,
                            &self.network_support.user_prefixes,
                        );
                        // Prefer server-backed history when negotiated. It has
                        // authoritative server-time and, on Nefarious +H
                        // channels, is also available without membership.
                        if !self.chathistory_enabled
                            && self.logging_load_history
                            && let Some((network, legacy_host, allow_legacy)) =
                                self.active_log_context()
                        {
                            let log_key = self.network_support.canonicalize(channel);
                            let history = self.load_text_history(
                                &network,
                                &legacy_host,
                                &log_key,
                                channel,
                                self.logging_history_lines,
                                allow_legacy,
                            );
                            if !history.is_empty() {
                                new_channel.messages.push_back(ChatMessage::system(&format!(
                                    "--- {} lines of history loaded ---",
                                    history.len()
                                )));
                                new_channel.messages.extend(history);
                                new_channel.history_loaded = true;
                            }
                            // Log session start
                            self.log_manager.log_session_start(&network, &log_key);
                        } else if (!self.chathistory_enabled || !self.logging_load_history)
                            && let Some((network, _, _)) = self.active_log_context()
                        {
                            self.log_manager.log_session_start(
                                &network,
                                &self.network_support.canonicalize(channel),
                            );
                        }
                        self.conversations.insert(channel.clone(), new_channel);
                    }
                    if let Some(joined_channel) = self.channel_mut(channel) {
                        joined_channel.joined = true;
                        // Membership starts over with this join's NAMES.
                        joined_channel.abandon_user_burst();
                        if pending_key.is_some() {
                            joined_channel.key = pending_key;
                        }
                    }
                    if self.enabled_caps.contains("no-implicit-names") {
                        self.send_command(IrcCommand::Names(Some(channel.clone())));
                    }
                    if let Some(index) = self
                        .update_join_targets
                        .iter()
                        .position(|target| self.identifiers_equal(target, channel))
                    {
                        self.update_join_targets.remove(index);
                        self.current_conversation = self
                            .update_selected
                            .as_deref()
                            .and_then(|target| self.id_for_target(target));
                        if self.update_join_targets.is_empty() {
                            self.update_selected = None;
                        }
                    } else if self.take_join_focus_request(channel)
                        || self.current_conversation.is_none()
                    {
                        // The user asked for this channel - or has no tab
                        // selected at all, so landing on the first joined
                        // channel takes nothing away. Anything else (an
                        // auto-join or replayed JOIN) must not steal the tab
                        // being read (#290).
                        self.select_target(&existing_key.unwrap_or_else(|| channel.clone()));
                    }
                    // The banner is not news: it must not raise an unread
                    // badge on a tab joined in the background.
                    let sys_msg = ChatMessage::system(&format!("Now talking in {}", channel));
                    let unread_before = self.channel_mut(channel).map(|tab| tab.unread);
                    self.add_message_to_channel(channel, sys_msg);
                    if let Some(unread) = unread_before
                        && let Some(tab) = self.channel_mut(channel)
                    {
                        tab.unread = unread;
                    }
                    if self.chathistory_enabled && self.logging_load_history {
                        self.request_automatic_history(
                            channel,
                            if is_new_channel {
                                HistoryPlacement::Prepend
                            } else {
                                HistoryPlacement::Append
                            },
                        );
                    }
                } else {
                    if !self.hide_join_part {
                        // Include account info if available (IRCv3 extended-join)
                        let sys_msg = match (account, realname) {
                            (Some(acct), Some(real)) => ChatMessage::system(&format!(
                                "{} ({}) [{}] has joined {}",
                                sender, real, acct, channel
                            )),
                            (Some(acct), None) => ChatMessage::system(&format!(
                                "{} [{}] has joined {}",
                                sender, acct, channel
                            )),
                            (None, Some(real)) => ChatMessage::system(&format!(
                                "{} ({}) has joined {}",
                                sender, real, channel
                            )),
                            (None, None) => {
                                ChatMessage::system(&format!("{} has joined {}", sender, channel))
                            }
                        };
                        self.add_message_to_channel(channel, sys_msg);
                    }
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.add_user(&sender, UserMode::Normal);
                        ch.set_user_account(&sender, account.clone());
                        ch.set_user_realname(&sender, realname.clone());
                    }
                }
                // A prefix without user@host must not erase what is known.
                self.update_user_identity(
                    &sender,
                    username.map(Some),
                    hostname.map(Some),
                    account.clone().map(Some),
                    realname.clone().map(Some),
                    None,
                    None,
                    Some(true),
                );
            }

            IrcCommand::Part(channel, reason) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                self.clear_typing_sender(channel, &sender);
                let reason_str = reason.as_deref().unwrap_or("");
                if self.identifiers_equal(&sender, &self.my_nick) {
                    // /cycle may already have queued a newer JOIN after this
                    // PART. Its recovery intent belongs to that newer command.
                    let rejoin = self.pending_rejoins.as_ref().and_then(|rejoins| {
                        rejoins
                            .iter()
                            .find(|(target, _)| self.identifiers_equal(target, channel))
                            .cloned()
                    });
                    if let Some(id) = self
                        .id_for_target(channel)
                        .filter(|id| self.pending_cycles.contains(id))
                    {
                        self.retire_history_owner(id);
                        self.forget_automatic_history(id);
                        self.forget_rejoin_membership(channel);
                        // Reset protocol/history data just as a fresh JOIN did,
                        // while retaining the view and its immutable identity.
                        let mut fresh = Channel::new();
                        fresh.set_case_mapping(self.network_support.case_mapping);
                        fresh.set_prefix_schema(
                            &self.network_support.prefix_modes,
                            &self.network_support.user_prefixes,
                        );
                        if let Some(tab) = self.conversations.channel_mut(id) {
                            *tab = fresh;
                        }
                        self.prune_known_users();
                    } else {
                        self.remove_channel(channel);
                    }
                    self.forget_part_intent(channel);
                    if let Some(rejoin) = rejoin {
                        self.pending_rejoins
                            .get_or_insert_with(Vec::new)
                            .push(rejoin);
                    }
                } else {
                    if !self.hide_join_part {
                        let sys_msg = ChatMessage::system(&format!(
                            "{} has left {} ({})",
                            sender, channel, reason_str
                        ));
                        self.add_message_to_channel(channel, sys_msg);
                    }
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.remove_user(&sender);
                    }
                    self.evict_known_user_if_orphaned(&sender);
                }
            }

            IrcCommand::Kick(channel, kicked, reason) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                let reason_str = reason.as_deref().unwrap_or("");
                let max = self.max_scrollback;
                if self.identifiers_equal(kicked, &self.my_nick) {
                    self.forget_rejoin_membership(channel);
                    // We were kicked: keep the tab visible with a notice, but clear
                    // membership state since the server has removed us.
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.joined = false;
                        ch.clear_users();
                        ch.push_trimmed(
                            ChatMessage::system(&format!(
                                "You were kicked from {} by {} ({})",
                                channel, sender, reason_str
                            )),
                            max,
                        );
                    }
                    self.prune_known_users();
                    self.send_notification(
                        &format!("Kicked from {}", channel),
                        &format!("by {} ({})", sender, reason_str),
                        false,
                    );
                } else {
                    // Kicks are moderation events, shown even when join/part is hidden.
                    let sys_msg = ChatMessage::system(&format!(
                        "{} was kicked from {} by {} ({})",
                        kicked, channel, sender, reason_str
                    ));
                    self.add_message_to_channel(channel, sys_msg);
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.remove_user(kicked);
                    }
                    self.evict_known_user_if_orphaned(kicked);
                }
            }

            IrcCommand::Quit(reason) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                self.forget_typing_nick(&sender);
                let reason_str = reason.as_deref().unwrap_or("Quit");

                // Remove user from all channels, optionally show quit message
                let max = self.max_scrollback;
                for channel in self.conversations.values_mut() {
                    if channel.has_user(&sender) {
                        if !self.hide_join_part {
                            let sys_msg = ChatMessage::system(&format!(
                                "{} has quit ({})",
                                sender, reason_str
                            ));
                            channel.push_trimmed(sys_msg, max);
                        }
                        channel.remove_user(&sender);
                    }
                }
                // Evict orphaned identities instead of retaining them
                // offline forever; monitored / still-present users are
                // marked offline (#195).
                let sender_key = self.network_support.canonicalize(&sender);
                self.evict_known_user_if_orphaned(&sender);
                if self.known_users.contains_key(&sender_key) {
                    self.update_user_identity(
                        &sender,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(false),
                    );
                }
            }

            IrcCommand::Nick(new_nick) => {
                let old_nick = msg.get_sender_nick().unwrap_or_default();
                if self.identifiers_equal(&old_nick, &self.my_nick) {
                    // Accepted NICK change: the outgoing nick was confirmed
                    // in use, so record it as held (#168).
                    self.set_session_nick(new_nick.clone(), true);
                }
                // Rename bookkeeping applies to every NICK (channel user
                // lists, identity/monitor re-keying, query-tab re-keying);
                // only the self-attribution above is gated (#173).
                self.apply_nick_change(&old_nick, new_nick);
            }

            IrcCommand::Topic(channel, topic) => {
                let max = self.max_scrollback;
                let sender = msg.get_sender_nick();
                // Prefer the server's timestamp so a bouncer replay or lagged
                // delivery is attributed to when the topic actually changed.
                let set_time = msg.get_server_time().unwrap_or_else(|| {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                });
                if let Some(ch) = self.channel_mut(channel) {
                    ch.set_topic(topic.clone(), sender.clone(), Some(set_time));
                    let sys_msg = match (sender, ch.topic.as_deref()) {
                        (Some(s), Some(topic)) => {
                            ChatMessage::system(&format!("{} changed the topic to: {}", s, topic))
                        }
                        (Some(s), None) => ChatMessage::system(&format!("{} cleared the topic", s)),
                        (None, Some(topic)) => ChatMessage::system(&format!("Topic: {}", topic)),
                        (None, None) => ChatMessage::system("Topic cleared"),
                    };
                    ch.push_trimmed(sys_msg, max);
                }
            }

            IrcCommand::Invite(target, channel) => {
                let sender = msg
                    .get_sender_nick()
                    .unwrap_or_else(|| "Someone".to_string());
                if msg.prefix.is_some() && self.is_ignored(&sender, msg.prefix.as_deref()) {
                    tracing::debug!("Ignoring invite from {}", sender);
                    return;
                }
                if !self.identifiers_equal(target, &self.my_nick) {
                    self.add_message_to_channel(
                        channel,
                        ChatMessage::system_fmt(
                            &format!("{sender} invited {target} to {channel}"),
                            &self.timestamp_format,
                        ),
                    );
                    return;
                }
                // Store the invite, bounding the queue so unsolicited INVITE spam
                // cannot grow it without limit (drop the oldest when full).
                const MAX_PENDING_INVITES: usize = 64;
                if self.pending_invites.len() >= MAX_PENDING_INVITES {
                    self.pending_invites.remove(0);
                }
                self.pending_invites.push((sender.clone(), channel.clone()));
                // Show prominent message in server buffer
                self.add_server_message(ChatMessage::system(&format!(
                    "*** {} has invited you to {} - type /join {} to accept",
                    sender, channel, channel
                )));
                // Send notification
                self.send_notification(
                    "Channel Invite",
                    &format!("{} invited you to {}", sender, channel),
                    false,
                );
            }

            IrcCommand::Mode(target, mode, params) => {
                let sender = msg
                    .get_sender_nick()
                    .unwrap_or_else(|| "Server".to_string());
                self.handle_mode_message(&sender, target, mode.as_deref(), params);
            }

            IrcCommand::Numeric(num, params) => {
                self.handle_numeric(*num, params);
            }

            IrcCommand::Ping(_) => {
                // Handled automatically in client
            }

            IrcCommand::Pong(_) => {
                // Calculate lag from our ping
                if let Some(sent_time) = self.ping_sent_time.take() {
                    let elapsed = sent_time.elapsed();
                    self.lag_ms = Some(elapsed.as_millis() as u32);
                }
            }

            // IRCv3 away-notify: user changed away status
            IrcCommand::Away(away_msg) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                // Update away status for user in ALL channels they're in
                self.update_user_away_status(&sender, away_msg.clone());

                // Show message once (in first shared channel)
                if !self.hide_join_part {
                    let sys_msg = if let Some(away_text) = away_msg {
                        ChatMessage::system(&format!("{} is now away: {}", sender, away_text))
                    } else {
                        ChatMessage::system(&format!("{} is back", sender))
                    };
                    // Find first channel where user is present and show message there
                    let max = self.max_scrollback;
                    for channel in self.conversations.values_mut() {
                        if channel.has_user(&sender) {
                            channel.push_trimmed(sys_msg, max);
                            break;
                        }
                    }
                }
            }

            // IRCv3 account-notify: user logged in/out of account
            IrcCommand::Account(account) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                self.update_user_identity(
                    &sender,
                    None,
                    None,
                    Some(if account == "*" {
                        None
                    } else {
                        Some(account.clone())
                    }),
                    None,
                    None,
                    None,
                    None,
                );
                // Optionally show message (only if not hiding join/part)
                if !self.hide_join_part {
                    let sys_msg = if account == "*" {
                        ChatMessage::system(&format!("{} has logged out", sender))
                    } else {
                        ChatMessage::system(&format!("{} has logged in as {}", sender, account))
                    };
                    // Show in channels where user is present
                    let max = self.max_scrollback;
                    for channel in self.conversations.values_mut() {
                        if channel.has_user(&sender) {
                            channel.push_trimmed(sys_msg.clone(), max);
                            break; // Only show once
                        }
                    }
                }
            }

            // IRCv3 chghost: user changed their username/hostname
            IrcCommand::Chghost(new_user, new_host) => {
                let sender = msg.get_sender_nick().unwrap_or_default();
                self.update_user_identity(
                    &sender,
                    Some(Some(new_user.clone())),
                    Some(Some(new_host.clone())),
                    None,
                    None,
                    None,
                    None,
                    None,
                );
                if !self.hide_join_part {
                    let sys_msg = ChatMessage::system(&format!(
                        "{} changed host to {}@{}",
                        sender, new_user, new_host
                    ));
                    let max = self.max_scrollback;
                    for channel in self.conversations.values_mut() {
                        if channel.has_user(&sender) {
                            channel.push_trimmed(sys_msg.clone(), max);
                            break; // Only show once
                        }
                    }
                }
            }

            IrcCommand::Cap(_target, subcommand, params) => {
                self.handle_cap_message(subcommand, params);
            }

            // Collect chathistory rows until the batch closes so they can be
            // inserted before live traffic as one chronological block.
            IrcCommand::Batch(reference, batch_type, params) => {
                if reference.starts_with('+')
                    && batch_type.as_deref() == Some("soju.im/bouncer-networks")
                {
                    self.bouncer_networks.clear();
                }
                if let Some(batch_id) = reference.strip_prefix('+') {
                    tracing::debug!("Batch started: {} type={:?}", reference, batch_type);
                    if let Some(parent) = msg.get_batch() {
                        // Server-controlled map: a stream of unclosed tagged
                        // opens must not grow it for the whole session. Evict
                        // rather than refuse — a full map pinned shut would
                        // permanently lose legitimate nesting links (issue
                        // #151); a displaced entry only costs one link.
                        if self.batch_parents.len() >= 4096
                            && let Some(evicted) = self.batch_parents.keys().next().cloned()
                        {
                            self.batch_parents.remove(&evicted);
                        }
                        self.batch_parents.insert(batch_id.to_string(), parent);
                    }
                    if let Some(label) = msg.get_tag("label") {
                        if self.batch_labels.len() >= 4096
                            && let Some(evicted) = self.batch_labels.keys().next().cloned()
                        {
                            self.batch_labels.remove(&evicted);
                        }
                        self.batch_labels.insert(batch_id.to_string(), label);
                    }
                    if batch_type
                        .as_deref()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("draft/multiline"))
                        && let Some(target) = params
                        && self.multiline_batches.len() < 32
                    {
                        let parent_history = msg
                            .get_batch()
                            .and_then(|parent| self.containing_history_batch(&parent));
                        self.multiline_batches.insert(
                            batch_id.to_string(),
                            MultilineBatch {
                                target: target.clone(),
                                sender: msg.get_sender_nick().unwrap_or_else(|| "???".to_string()),
                                sender_prefix: msg.prefix.clone(),
                                content: String::new(),
                                has_lines: false,
                                line_count: 0,
                                oversized: false,
                                is_notice: false,
                                parent_history,
                                server_time: msg.get_server_time_raw(),
                                msgid: msg.get_msgid(),
                                account: msg.get_account(),
                                oper: msg.get_oper(),
                                reply_to: msg.get_reply(),
                            },
                        );
                    }
                    if batch_type
                        .as_deref()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("draft/authtoken"))
                        && self.authtoken_batches.len() < 32
                    {
                        self.authtoken_batches.insert(
                            batch_id.to_string(),
                            AuthtokenBatch {
                                service: params.clone().unwrap_or_else(|| "*".to_string()),
                                token: String::new(),
                                oversized: false,
                            },
                        );
                    }
                    if batch_type.as_deref().is_some_and(|kind| {
                        kind.eq_ignore_ascii_case("chathistory")
                            || (kind.eq_ignore_ascii_case("znc.in/playback")
                                && self.membership_managed_by_bouncer())
                    }) && let Some(target) = params
                        && self.chathistory_batches.len() < 32
                    {
                        let legacy_playback = batch_type
                            .as_deref()
                            .is_some_and(|kind| kind.eq_ignore_ascii_case("znc.in/playback"));
                        let playback_owner = if legacy_playback {
                            self.id_for_target(target).or_else(|| {
                                (!self
                                    .bouncer_retired_targets
                                    .contains(&self.network_support.canonicalize(target)))
                                .then(|| self.ensure_history_target(target))
                            })
                        } else {
                            None
                        };
                        let placement = if legacy_playback
                            && playback_owner
                                .and_then(|id| self.conversation_channel(id))
                                .is_some_and(|channel| {
                                    channel
                                        .messages
                                        .iter()
                                        .any(|message| Self::message_reference(message).is_some())
                                }) {
                            HistoryPlacement::Append
                        } else {
                            HistoryPlacement::Prepend
                        };
                        let label = self.response_label(&msg);
                        let has_pending = self.has_pending_history_target(target);
                        let request = if legacy_playback {
                            None
                        } else {
                            self.take_history_request(target, None, label.as_deref())
                        }
                        .unwrap_or(PendingHistoryRequest {
                            correlation: Some(HistoryRequestCorrelation {
                                owner: if legacy_playback {
                                    playback_owner
                                } else if !has_pending && label.is_none() {
                                    self.id_for_target(target)
                                } else {
                                    None
                                },
                                target: target.clone(),
                                aliases: Vec::new(),
                                subcommand: HistorySubcommand::Latest,
                                label,
                                sequence: 0,
                            }),
                            placement,
                            direction: HistoryDirection::Before,
                            remaining: 0,
                            loaded: 0,
                            shown: 0,
                            page_limit: self.max_scrollback.max(1),
                            announce_arrivals: false,
                        });
                        self.chathistory_batches.insert(
                            batch_id.to_string(),
                            ChatHistoryBatch {
                                legacy_playback,
                                target: target.clone(),
                                messages: Vec::new(),
                                request,
                                complete: msg.get_tag("draft/chathistory-end").is_some(),
                                partial: msg
                                    .get_tag("evilnet.github.io/chathistory-partial")
                                    .is_some(),
                            },
                        );
                    }
                    if batch_type
                        .as_deref()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("draft/chathistory-targets"))
                    {
                        let label = self.response_label(&msg);
                        self.take_history_request(
                            "",
                            Some(HistorySubcommand::Targets),
                            label.as_deref(),
                        );
                    }
                } else if let Some(batch_id) = reference.strip_prefix('-') {
                    tracing::debug!("Batch ended: {}", reference);
                    self.finish_multiline_batch(batch_id);
                    self.finish_authtoken_batch(batch_id);
                    self.finish_chathistory_batch(batch_id);
                    self.batch_parents.remove(batch_id);
                    self.batch_labels.remove(batch_id);
                }
            }

            _ => {
                // Log unknown messages
                let sys_msg = ChatMessage::system(&msg.raw);
                self.add_server_message(sys_msg);
            }
        }

        self.scroll_to_bottom = true;
    }

    fn apply_isupport_tokens(&mut self, params: &[String]) {
        let previous_case_mapping = self.network_support.case_mapping;
        let previous_prefix_modes = self.network_support.prefix_modes.clone();
        let previous_prefixes = self.network_support.user_prefixes.clone();
        let previous_history_limit = self.network_support.history_limit;

        for token in params.iter().skip(1) {
            // The last parameter is the trailing explanation ("are supported
            // by this server"): the parser strips its ':', leaving one
            // parameter containing spaces. Real tokens never contain spaces,
            // so stop there instead of recording the text as a token.
            if token.starts_with(':') || token.contains(' ') {
                break;
            }
            if let Some(name) = token.strip_prefix('-') {
                if !name.is_empty() {
                    self.network_support
                        .isupport
                        .remove(&name.to_ascii_uppercase());
                }
                continue;
            }
            let (name, value) = token
                .split_once('=')
                .map(|(name, value)| (name, Some(value.to_string())))
                .unwrap_or((token.as_str(), None));
            if !name.is_empty() {
                self.network_support
                    .isupport
                    .insert(name.to_ascii_uppercase(), value);
            }
        }

        // Rebuild every typed value from the raw token map. This makes a
        // draft/extended-isupport `-TOKEN` removal restore the protocol
        // default instead of leaving stale routing or limit state behind.
        let tokens = std::mem::take(&mut self.network_support.isupport);
        let multiline_max_bytes = self.network_support.multiline_max_bytes;
        let multiline_max_lines = self.network_support.multiline_max_lines;
        let mut support = NetworkSupport {
            isupport: tokens,
            multiline_max_bytes,
            multiline_max_lines,
            ..NetworkSupport::default()
        };

        let raw_tokens = support.isupport.clone();
        let value = |name: &str| {
            raw_tokens
                .get(&name.to_ascii_uppercase())
                .and_then(|value| value.clone())
        };
        if let Some(v) = value("CHANTYPES").filter(|v| !v.is_empty()) {
            support.channel_types = v;
        }
        if let Some(v) = value("CASEMAPPING") {
            support.case_mapping = if v.eq_ignore_ascii_case("ascii") {
                CaseMapping::Ascii
            } else if v.eq_ignore_ascii_case("rfc1459-strict")
                || v.eq_ignore_ascii_case("strict-rfc1459")
            {
                CaseMapping::StrictRfc1459
            } else {
                CaseMapping::Rfc1459
            };
        }
        if value("UTF8MAPPING").is_some_and(|v| v.eq_ignore_ascii_case("rfc8265")) {
            support.case_mapping = CaseMapping::Utf8Rfc8265;
        }
        if let Some(v) = value("PREFIX")
            && let Some(rest) = v.strip_prefix('(')
            && let Some((letters, symbols)) = rest.split_once(')')
            && !symbols.is_empty()
            && letters.chars().count() == symbols.chars().count()
        {
            support.prefix_modes = letters.to_string();
            support.user_prefixes = symbols.to_string();
        }
        if let Some(v) = value("CHANMODES") {
            let mut groups = v.split(',');
            if let (Some(a), Some(b), Some(c), Some(_d)) =
                (groups.next(), groups.next(), groups.next(), groups.next())
            {
                support.chanmodes_a = a.to_string();
                support.chanmodes_b = b.to_string();
                support.chanmodes_c = c.to_string();
            }
        }
        if let Some(v) = value("STATUSMSG") {
            support.status_prefixes = v;
        }
        // CHATHISTORY=0 means the server sets no limit.
        support.history_limit = value("CHATHISTORY")
            .and_then(|v| v.parse().ok())
            .filter(|limit| *limit > 0);
        support.history_reference_types = value("MSGREFTYPES").unwrap_or_default();
        support.history_retention_secs =
            value("EVILNET/CHATHISTORYRETENTION").and_then(|v| v.parse().ok());
        support.nick_len = value("NICKLEN").and_then(|v| v.parse().ok());
        support.channel_len = value("CHANNELLEN").and_then(|v| v.parse().ok());
        support.topic_len = value("TOPICLEN").and_then(|v| v.parse().ok());
        support.away_len = value("AWAYLEN").and_then(|v| v.parse().ok());
        support.kick_len = value("KICKLEN").and_then(|v| v.parse().ok());
        support.monitor_limit = value("MONITOR").and_then(|v| v.parse().ok());
        support.watch_limit = value("WATCH").and_then(|v| v.parse().ok());
        support.silence_limit = value("SILENCE").and_then(|v| v.parse().ok());
        support.whox = support.has_isupport("WHOX");
        support.elist = value("ELIST").unwrap_or_default();
        support.safelist = support.has_isupport("SAFELIST");
        support.line_len = value("LINELEN").and_then(|v| v.parse().ok());
        support.client_tag_deny = value("CLIENTTAGDENY");
        support.bot_mode = value("BOT").and_then(|v| v.chars().next());
        support.except_mode = if support.has_isupport("EXCEPTS") {
            value("EXCEPTS")
                .and_then(|v| v.chars().next())
                .or(Some('e'))
        } else {
            None
        };
        support.invex_mode = if support.has_isupport("INVEX") {
            value("INVEX").and_then(|v| v.chars().next()).or(Some('I'))
        } else {
            None
        };
        support.extban = value("EXTBAN");
        support.network_name = value("NETWORK");
        support.utf8_only = support.has_isupport("UTF8ONLY");
        support.account_required = support.has_isupport("DRAFT/ACCOUNTREQUIRED");

        // The page limit may also come from the draft/chathistory capability
        // value, and most servers never send a CHATHISTORY token; only a token
        // that is present (or one that was withdrawn) changes what CAP set.
        if support.history_limit.is_some() || previous_history_limit.is_some() {
            self.chathistory_limit = support.history_limit;
        }
        self.network_support = support;
        if self.network_support.case_mapping != previous_case_mapping
            || self.network_support.has_isupport("CASEMAPPING")
            || self.network_support.has_isupport("UTF8MAPPING")
        {
            self.refresh_case_mapping_state();
        } else {
            self.conversations
                .provisional_support(&self.network_support);
        }
        if self.network_support.prefix_modes != previous_prefix_modes
            || self.network_support.user_prefixes != previous_prefixes
        {
            let modes = self.network_support.prefix_modes.clone();
            let prefixes = self.network_support.user_prefixes.clone();
            for channel in self.conversations.values_mut() {
                channel.set_prefix_schema(&modes, &prefixes);
            }
        }
    }

    fn handle_mode_message(
        &mut self,
        sender: &str,
        target: &str,
        mode: Option<&str>,
        params: &[String],
    ) {
        let Some(mode) = mode else {
            return;
        };
        let is_channel = self.is_channel_name(target);
        let mut param_idx = 0usize;
        let mut sign = '+';
        let max = self.max_scrollback;
        let timestamp_format = self.timestamp_format.clone();
        // Which modes consume a parameter comes from ISUPPORT (PREFIX mode
        // letters + CHANMODES groups); hardcoded sets misalign parameters on
        // networks with extra parameterized modes (e.g. `MODE #c +fo [4:5] nick`
        // would read "[4:5]" as the nick to op).
        let ns = self.network_support.clone();

        if is_channel {
            if let Some(channel) = self.channel_mut(target) {
                for c in mode.chars() {
                    match c {
                        '+' | '-' => sign = c,
                        // Prefix (user) modes: always take a nick parameter.
                        c if ns.prefix_modes.contains(c) => {
                            if let Some(nick) = params.get(param_idx) {
                                param_idx += 1;
                                if sign == '+' {
                                    channel.add_user_status_mode(nick, c);
                                } else {
                                    channel.remove_user_status_mode(nick, c);
                                }
                            }
                        }
                        // Type A list modes (bans etc.): parameter in both
                        // directions. Numeric list replies fill setter/time data;
                        // live MODE messages keep the cached lists current.
                        c if ns.chanmodes_a.contains(c) => {
                            if let Some(mask) = params.get(param_idx) {
                                param_idx += 1;
                                channel.apply_mode_list_change(sign, c, mask, sender);
                            }
                        }
                        // Type B: parameter in both directions.
                        c if ns.chanmodes_b.contains(c) => {
                            let parameter = params.get(param_idx).map(String::as_str);
                            if parameter.is_some() {
                                param_idx += 1;
                            }
                            channel.apply_channel_mode(sign, c, parameter);
                        }
                        // Type C: parameter only when set.
                        c if ns.chanmodes_c.contains(c) => {
                            let parameter = if sign == '+' {
                                let value = params.get(param_idx).map(String::as_str);
                                if value.is_some() {
                                    param_idx += 1;
                                }
                                value
                            } else {
                                None
                            };
                            channel.apply_channel_mode(sign, c, parameter);
                        }
                        // Type D (and unknown): never takes a parameter.
                        other => {
                            channel.apply_channel_mode_flag(sign, other);
                        }
                    }
                }
                let rendered = if params.is_empty() {
                    mode.to_string()
                } else {
                    format!("{} {}", mode, params.join(" "))
                };
                channel.push_trimmed(
                    ChatMessage::system_fmt(
                        &format!("{} set mode {}", sender, rendered),
                        &timestamp_format,
                    ),
                    max,
                );
            }
        } else if self.identifiers_equal(target, &self.my_nick) {
            let rendered = if params.is_empty() {
                mode.to_string()
            } else {
                format!("{} {}", mode, params.join(" "))
            };
            self.add_server_message(ChatMessage::system_fmt(
                &format!("{} set user mode {}", sender, rendered),
                &self.timestamp_format,
            ));
        }
    }

    fn handle_numeric(&mut self, num: u16, params: &[String]) {
        // 470 ERR_LINKCHANNEL <client> <#old> <#new>: the JOIN was forwarded.
        if num == ERR_LINKCHANNEL
            && let (Some(from), Some(to)) = (params.get(1), params.get(2))
        {
            self.redirect_join_focus_request(from, to);
            let recovering = self.pending_rejoins.as_ref().is_some_and(|rejoins| {
                rejoins
                    .iter()
                    .any(|(name, _)| self.identifiers_equal(name, from))
            });
            self.abandon_refused_join(from);
            if recovering {
                self.pending_rejoins
                    .as_mut()
                    .unwrap()
                    .push((to.clone(), None));
            }
        }
        // A refused JOIN ends the attempt the key was sent for. Left in the
        // map, the key would attach to the tab of whatever later JOIN of that
        // channel (keyless, from an invite or a bouncer) the server confirms.
        // The same goes for the user's request to focus the channel (#290).
        // 437 also means "nick unavailable" during registration, where
        // params[1] is a nick, so the later numerics need a channel name.
        if matches!(
            num,
            ERR_NOSUCHCHANNEL
                | ERR_UNAVAILRESOURCE
                | ERR_TOOMANYCHANNELS
                | ERR_CHANNELISFULL
                | ERR_INVITEONLYCHAN
                | ERR_BANNEDFROMCHAN
                | ERR_BADCHANNELKEY
                | ERR_BADCHANMASK
                | ERR_NEEDREGGEDNICK
                | ERR_BADCHANNAME
                | ERR_SECUREONLYCHAN
                | ERR_OPERONLY
        ) && let Some(channel) = params.get(1)
            && (!matches!(num, ERR_UNAVAILRESOURCE | ERR_OPERONLY | ERR_SECUREONLYCHAN)
                || self.is_channel_name(channel))
        {
            self.abandon_refused_join(channel);
        }
        match num {
            RPL_SAVENICK => {
                // ircu/IRCnet/Undernet force a rename on nick collision; the
                // granted nick is server-accepted and the outgoing one was
                // ours, so both route through the choke point (#172). Unlike
                // an accepted NICK echo, the rename is announced explicitly:
                // pre-registration renames have no other visible trace.
                if let Some(new_nick) = params.get(1).filter(|nick| !nick.is_empty()) {
                    let old_nick = self.my_nick.clone();
                    self.set_session_nick(new_nick.clone(), true);
                    self.apply_nick_change(&old_nick, new_nick);
                    self.add_server_message(ChatMessage::system(&format!(
                        "Server forced nick change to {}",
                        new_nick
                    )));
                }
            }

            RPL_WELCOME => {
                self.drain_connection_events();
                // A 001 that was already in flight when QUIT was queued
                // (disconnect, /server, /reconnect) belongs to the connection
                // being closed. It must neither mark the session connected nor
                // wipe the intent the lifecycle needs to finish the switch
                // once that worker exits (#246).
                if self.connection.is_closing() {
                    return;
                }
                if self
                    .active_session
                    .as_ref()
                    .and_then(|session| session.server.bouncer.as_ref())
                    .is_some_and(|binding| binding.provider == bouncer::BouncerProvider::Soju)
                    && !self.bouncer_ready
                {
                    self.fail_registration(
                        "The bouncer registered before the selected network was bound".into(),
                    );
                    return;
                }
                if !self.connection.accept_registration() {
                    return;
                }
                self.nick_retry_attempts = 0;
                // Clear any lag reading left over from a previous connection.
                self.lag_ms = None;
                if let Some(nick) = params.first() {
                    // 001 accepts whatever nick it grants, confirmed or not;
                    // a previously confirmed nick it replaces is recorded as
                    // held so bouncer replay attribution survives (#168),
                    // while a rewrite of a speculative pre-registration nick
                    // records nothing (#167/#169).
                    self.set_session_nick(nick.clone(), true);
                }
                let msg = params
                    .get(1)
                    .cloned()
                    .unwrap_or_else(|| "Welcome!".to_string());
                self.add_server_message(ChatMessage::system(&msg));

                let (set_invisible, auto_join, auto_perform) = self.current_session_automation();
                let auto_perform = self.expand_auto_perform(&auto_perform);
                self.prepare_history_authentication(&auto_perform);

                // Set invisible mode if requested for this session snapshot.
                if set_invisible {
                    self.send_command(IrcCommand::Mode(
                        self.my_nick.clone(),
                        Some("+i".to_string()),
                        Vec::new(),
                    ));
                    self.add_server_message(ChatMessage::system(
                        "Setting user mode +i (invisible)",
                    ));
                }

                // Restore actual memberships, including keys, through JOIN.
                if let Some(rejoins) = self.pending_rejoins.as_ref().cloned() {
                    for (name, key) in rejoins {
                        self.send_automatic_join(name, key);
                    }
                } else if !auto_join.is_empty() {
                    for chan in auto_join.split(',') {
                        let chan = chan.trim();
                        if !chan.is_empty() {
                            // An entry may carry a key: "#chan key". Keep it out
                            // of the channel name and the server window.
                            let mut words = chan.split_whitespace();
                            let name = words.next().unwrap_or(chan);
                            let key = words.next().map(str::to_string);
                            let channel = self.normalize_channel_name(name);
                            self.send_automatic_join(channel.clone(), key);
                            self.add_server_message(ChatMessage::system(&format!(
                                "Auto-joining {}",
                                channel
                            )));
                        }
                    }
                }

                // Re-send AWAY on every successful registration, not only
                // the app-update restore path, so ordinary auto-reconnects
                // do not silently clear the server-side away state (#190).
                if let Some(away) = self.away_status.clone() {
                    self.send_command(IrcCommand::Away(Some(away)));
                }

                // Nefarious suppresses its legacy bouncer replay for clients
                // that negotiated CHATHISTORY. Channel tabs catch up after
                // their JOIN acknowledgement; retained query tabs have no
                // JOIN event, so request their missed messages here.
                if self.chathistory_enabled && self.logging_load_history {
                    let query_targets: Vec<String> = self
                        .conversations
                        .iter()
                        .filter(|(target, channel)| {
                            !self.is_channel_name(target)
                                && channel
                                    .messages
                                    .iter()
                                    .rev()
                                    .any(|message| Self::message_reference(message).is_some())
                        })
                        .map(|(target, _)| target.clone())
                        .collect();
                    for target in query_targets {
                        self.request_automatic_history(&target, HistoryPlacement::Append);
                    }
                }

                // Queue auto-perform commands for execution
                if !auto_perform.is_empty() {
                    let commands: Vec<String> = auto_perform
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| line.to_string())
                        .collect();
                    if !commands.is_empty() {
                        self.pending_auto_perform = Some(commands);
                        self.add_server_message(ChatMessage::system(
                            "Running auto-perform commands...",
                        ));
                    }
                }

                // Initial lag check
                self.begin_bouncer_playback();
                self.ping_sent_time = Some(std::time::Instant::now());
                self.send_command(IrcCommand::Ping("LAG".to_string()));
            }

            RPL_ISUPPORT => {
                self.validate_bouncer_isupport(params);
                self.apply_isupport_tokens(params);
            }

            RPL_NOTOPIC => {
                // Format: 331 <nick> <channel> :No topic is set
                // Clears a topic left over from before a rejoin or one removed
                // while we were disconnected.
                let max = self.max_scrollback;
                if let Some(channel) = params.get(1) {
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.set_topic(None, None, None);
                        ch.push_trimmed(ChatMessage::system("No topic is set"), max);
                    } else {
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "No topic is set for {}",
                            channel
                        )));
                    }
                }
            }

            RPL_TOPIC => {
                let max = self.max_scrollback;
                if let (Some(channel), Some(topic)) = (params.get(1), params.get(2))
                    && let Some(ch) = self.channel_mut(channel)
                {
                    // The setter and time arrive in a following 333; drop the
                    // previous topic's attribution until then.
                    ch.set_topic(Some(topic.clone()), None, None);
                    if ch.topic.is_some() {
                        ch.push_trimmed(ChatMessage::system(&format!("Topic: {}", topic)), max);
                    }
                }
            }

            RPL_CHANNELMODEIS => {
                // Format: 324 <nick> <channel> <modes> [<mode params>...]
                let support = self.network_support.clone();
                if let (Some(channel), Some(modes)) = (params.get(1), params.get(2))
                    && let Some(ch) = self.channel_mut(channel)
                {
                    ch.set_channel_modes_from_reply(modes, &params[3..], &support);
                }
            }

            RPL_CREATIONTIME => {
                // Format: 329 <nick> <channel> <timestamp>
                if let (Some(channel), Some(ts_str)) = (params.get(1), params.get(2)) {
                    let ts: u64 = ts_str.parse().unwrap_or(0);
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.created = Some(ts);
                    }
                }
            }

            RPL_TOPICWHOTIME => {
                // Format: 333 <nick> <channel> <setter> <timestamp>
                if let (Some(channel), Some(setter), Some(ts_str)) =
                    (params.get(1), params.get(2), params.get(3))
                {
                    let ts: u64 = ts_str.parse().unwrap_or(0);

                    // Store topic setter info
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.topic_set_by = Some(setter.clone());
                        ch.topic_set_time = Some(ts);
                    }

                    let time_str = format_timestamp(ts);
                    let max = self.max_scrollback;
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.push_trimmed(
                            ChatMessage::system(&format!(
                                "Topic set by {} on {}",
                                setter, time_str
                            )),
                            max,
                        );
                    }
                }
            }

            RPL_NAMREPLY => {
                if let Some(channel) = params.get(2)
                    && let Some(names) = params.get(3)
                {
                    let support = self.network_support.clone();
                    let mut identities = Vec::new();
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.begin_user_burst();
                        for name in names.split_whitespace() {
                            // PREFIX symbols precede the nick and may themselves
                            // include `!`, so remove them before interpreting the
                            // userhost-in-names `nick!user@host` separator.
                            let (nick_token, modes) = support.parse_prefixed_nick(name);
                            let (nick, username, hostname) =
                                if let Some((nick, user_host)) = nick_token.split_once('!') {
                                    let (username, hostname) = user_host
                                        .split_once('@')
                                        .map(|(user, host)| {
                                            (Some(user.to_string()), Some(host.to_string()))
                                        })
                                        .unwrap_or((Some(user_host.to_string()), None));
                                    (nick, username, hostname)
                                } else {
                                    (nick_token, None, None)
                                };
                            ch.add_user_burst_status_modes(nick, &modes);
                            identities.push((nick.to_string(), username, hostname));
                        }
                    }
                    for (nick, username, hostname) in identities {
                        // Without userhost-in-names the entry carries no
                        // user/host: leave what JOIN prefixes or WHO taught us
                        // unchanged instead of overwriting it with None.
                        self.update_user_identity(
                            &nick,
                            username.map(Some),
                            hostname.map(Some),
                            None,
                            None,
                            None,
                            None,
                            Some(true),
                        );
                    }
                }
            }

            RPL_ENDOFNAMES => {
                if let Some(channel) = params.get(1) {
                    // NAMES burst complete: dedup and sort the user list once.
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.finish_user_burst();
                    }
                    // After getting the user list, request durable identity and
                    // presence fields. The query token lets concurrent WHOX
                    // responses be correlated with their channel.
                    if self.is_channel_name(channel) {
                        if self.network_support.whox {
                            let token = self.next_whox_token.max(1).to_string();
                            self.next_whox_token = if self.next_whox_token >= 999 {
                                1
                            } else {
                                self.next_whox_token + 1
                            };
                            // Tokens wrap at 999; a reused one replaces the
                            // query it outlived, which keeps the queue bounded.
                            self.pending_whox.retain(|(pending, _)| *pending != token);
                            self.pending_whox
                                .push_back((token.clone(), channel.clone()));
                            self.send_command(IrcCommand::Raw(format!(
                                "WHO {channel} %tcuhnafr,{token}"
                            )));
                        } else {
                            self.send_command(IrcCommand::Who(channel.clone()));
                        }
                    }
                }
            }

            RPL_BANLIST => {
                // Format: 367 <nick> <channel> <banmask> [<setter> <timestamp>]
                // RFC 1459/2812 define only the channel and mask; an entry
                // dropped here would make 368 present an authoritative empty
                // list.
                if let (Some(channel), Some(mask)) = (params.get(1), params.get(2)) {
                    let setter = params.get(3).cloned().unwrap_or_default();
                    let ts: u64 = params.get(4).and_then(|v| v.parse().ok()).unwrap_or(0);
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.record_mode_list_reply('b', mask.clone(), setter, ts);
                    }
                }
            }

            RPL_ENDOFBANLIST => {
                // Format: 368 <nick> <channel> :End of channel ban list
                if let Some(channel) = params.get(1)
                    && let Some(ch) = self.channel_mut(channel)
                {
                    ch.ban_list_complete = true;
                    ch.complete_mode_list('b');
                }
            }

            RPL_INVITELIST | RPL_EXCEPTLIST => {
                // Common layout: <me> <channel> <mask> <setter> <timestamp>.
                if let (Some(channel), Some(mask)) = (params.get(1), params.get(2)) {
                    let mode = if num == RPL_INVITELIST {
                        self.network_support.invex_mode.unwrap_or('I')
                    } else {
                        self.network_support.except_mode.unwrap_or('e')
                    };
                    let setter = params.get(3).cloned().unwrap_or_default();
                    let set_time = params
                        .get(4)
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0);
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.record_mode_list_reply(mode, mask.clone(), setter, set_time);
                    }
                }
            }

            RPL_ENDOFINVITELIST | RPL_ENDOFEXCEPTLIST => {
                if let Some(channel) = params.get(1) {
                    let mode = if num == RPL_ENDOFINVITELIST {
                        self.network_support.invex_mode.unwrap_or('I')
                    } else {
                        self.network_support.except_mode.unwrap_or('e')
                    };
                    if let Some(ch) = self.channel_mut(channel) {
                        ch.complete_mode_list(mode);
                    }
                }
            }

            RPL_MOTD | RPL_MOTDSTART | RPL_ENDOFMOTD => {
                if let Some(text) = params.last() {
                    self.add_server_message(ChatMessage::system(text));
                }
            }

            ERR_NICKNAMEINUSE => {
                // After registration this is a failed /nick attempt: report it and
                // keep our current nick. Auto-retrying here would silently rename
                // the user to "<current>_" and desync my_nick from the server.
                if self.is_connected() {
                    let rejected = params.get(1).cloned().unwrap_or_default();
                    let text = params
                        .last()
                        .cloned()
                        .unwrap_or_else(|| "Nickname is already in use".to_string());
                    self.add_server_message(ChatMessage::system_fmt(
                        &format!("{}: {}", rejected, text),
                        &self.timestamp_format,
                    ));
                    return;
                }
                if self.nick_retry_attempts >= 3 {
                    let text = params
                        .last()
                        .cloned()
                        .unwrap_or_else(|| "Nickname is already in use".to_string());
                    self.fail_registration(format!("Registration failed: {}", text));
                    return;
                }
                self.nick_retry_attempts = self.nick_retry_attempts.saturating_add(1);
                // Keep the suffixed retry within the server's NICKLEN when
                // advertised: an over-limit retry would fail with
                // ERR_ERRONEUSNICKNAME instead of walking away (#172).
                let mut base = self.my_nick.clone();
                if let Some(max) = self.network_support.nick_len {
                    while base.chars().count() + 1 > max && !base.is_empty() {
                        base.pop();
                    }
                }
                let mut new_nick = format!("{base}_");
                // Progress guard: a clamp that regenerates the identical
                // just-rejected nick (e.g. my_nick already ends with '_' at
                // the limit) would burn the budget on guaranteed retries
                // (#173).
                while new_nick == self.my_nick && !base.is_empty() {
                    base.pop();
                    new_nick = format!("{base}_");
                }
                // The walkaway records the rejected nick only when it was
                // confirmed in use on this lineage (an auto-reconnect that
                // registered as a held nick before 433 hit): its replayed
                // rows must stay attributed (#169). A pre-registration
                // walkaway from the configured nick records nothing (#166).
                self.set_session_nick(new_nick.clone(), false);
                if let Some(tx) = self.connection.sender() {
                    let _ = tx.send(IrcCommand::Nick(new_nick));
                }
                self.add_server_message(ChatMessage::system(
                    "Nickname in use, trying alternative...",
                ));
            }

            ERR_NONICKNAMEGIVEN | ERR_ERRONEUSNICKNAME | ERR_NICKCOLLISION | ERR_PASSWDMISMATCH
            | ERR_YOUREBANNEDCREEP => {
                let text = params
                    .last()
                    .cloned()
                    .unwrap_or_else(|| format!("registration error {}", num));
                if !self.is_connected() {
                    self.fail_registration(format!("Registration failed: {}", text));
                } else {
                    self.add_server_message(ChatMessage::system_fmt(
                        &format!("[{}] {}", num, text),
                        &self.timestamp_format,
                    ));
                }
            }

            RPL_LISTSTART => {
                // Clear old list, start collecting
                self.channel_list.clear();
                self.channel_list_dirty = true;
                self.channel_list_loading = true;
                self.show_channel_list = true;
            }

            RPL_LIST => {
                // Bound the in-memory channel list so an enormous (or malicious)
                // LIST reply cannot grow it without limit.
                const MAX_LIST_ENTRIES: usize = 50_000;
                if self.channel_list.len() >= MAX_LIST_ENTRIES {
                    return;
                }

                // Use the advertised CHANTYPES ('!' and '+' channels exist on
                // IRCnet); params[0] is our own nick.
                let channel_idx = params
                    .iter()
                    .skip(1)
                    .position(|p| self.network_support.is_channel(p))
                    .map(|index| index + 1);

                let parsed = channel_idx.and_then(|ci| {
                    if ci + 1 >= params.len() {
                        return None;
                    }
                    fn parse_count(p: &str) -> Option<usize> {
                        let clean = p.replace(',', "");
                        if !clean.is_empty()
                            && clean.chars().all(|c| c.is_ascii_digit() || c == '+')
                        {
                            clean.trim_start_matches('+').parse().ok()
                        } else {
                            None
                        }
                    }

                    let last = params.last()?;
                    let (user_count, topic) = if ci + 1 == params.len() - 1 {
                        // One parameter after the channel: per the standard
                        // format it is the count ("322 me #chan 42"); only a
                        // non-numeric value there means the server skipped the
                        // count and sent the topic directly.
                        match parse_count(last) {
                            Some(count) => (count, String::new()),
                            None => (0, last.clone()),
                        }
                    } else {
                        // Standard form: counts sit between the channel name
                        // and the trailing topic. The trailing parameter is the
                        // topic even when purely numeric ("322 me #a 12 :2024").
                        let count = params[ci + 1..params.len() - 1]
                            .iter()
                            .find_map(|p| parse_count(p))
                            .unwrap_or(0);
                        (count, last.clone())
                    };
                    Some((params[ci].clone(), user_count, topic))
                });

                match parsed {
                    Some((name, user_count, topic)) => {
                        if user_count < self.channel_list_min_users as usize {
                            return;
                        }
                        self.channel_list
                            .push(ChannelListEntry::new(name, user_count, topic));
                        self.channel_list_dirty = true;
                    }
                    None => {
                        // Fallback: if we have at least 3 params, assume standard format
                        // [client, channel, count, topic]
                        if params.len() >= 3 {
                            let user_count = params[2].replace(',', "").parse().unwrap_or(0);
                            if user_count < self.channel_list_min_users as usize {
                                return;
                            }
                            self.channel_list.push(ChannelListEntry::new(
                                params[1].clone(),
                                user_count,
                                params.get(3).cloned().unwrap_or_default(),
                            ));
                            self.channel_list_dirty = true;
                        }
                    }
                }
            }

            RPL_LISTEND => {
                self.channel_list_loading = false;
                // Force a final view refresh in case the streaming throttle
                // skipped the last appends.
                self.channel_list_dirty = true;
            }

            RPL_AWAY => {
                // <nick> :<away message> - shown during WHOIS or when messaging away user
                if let (Some(nick), Some(message)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is away: {}",
                        nick, message
                    )));
                }
            }

            RPL_UNAWAY => {
                if let Some(text) = params.get(1) {
                    self.add_server_message(ChatMessage::system(text));
                }
            }

            RPL_NOWAWAY => {
                if let Some(text) = params.get(1) {
                    self.add_server_message(ChatMessage::system(text));
                }
            }

            // WHOIS responses
            RPL_WHOISUSER => {
                // <nick> <user> <host> * :<realname>
                if let (Some(nick), Some(user), Some(host)) =
                    (params.get(1), params.get(2), params.get(3))
                {
                    let realname = params.get(5).cloned().unwrap_or_default();
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} ({}@{}) - {}",
                        nick, user, host, realname
                    )));
                }
            }

            RPL_WHOISSERVER => {
                // <nick> <server> :<serverinfo>
                if let (Some(nick), Some(server)) = (params.get(1), params.get(2)) {
                    let info = params.get(3).cloned().unwrap_or_default();
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is on server {} ({})",
                        nick, server, info
                    )));
                }
            }

            RPL_WHOISOPERATOR => {
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is an IRC operator",
                        nick
                    )));
                }
            }

            RPL_WHOISIDLE => {
                // <nick> <seconds> <signon> :seconds idle, signon time
                if let (Some(nick), Some(idle_secs)) = (params.get(1), params.get(2)) {
                    let idle: u64 = idle_secs.parse().unwrap_or(0);
                    let idle_str = if idle >= 3600 {
                        format!("{}h {}m", idle / 3600, (idle % 3600) / 60)
                    } else if idle >= 60 {
                        format!("{}m {}s", idle / 60, idle % 60)
                    } else {
                        format!("{}s", idle)
                    };
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} has been idle for {}",
                        nick, idle_str
                    )));
                }
            }

            RPL_ENDOFWHOIS => {
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] End of WHOIS for {}",
                        nick
                    )));
                }
            }

            RPL_WHOISCHANNELS => {
                if let (Some(nick), Some(channels)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is on: {}",
                        nick, channels
                    )));
                }
            }

            RPL_WHOISACCOUNT => {
                if let (Some(nick), Some(account)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is logged in as {}",
                        nick, account
                    )));
                }
            }

            RPL_WHOISACTUALLY => {
                if let (Some(nick), Some(host)) = (params.get(1), params.get(2)) {
                    let ip = params.get(3).cloned().unwrap_or_default();
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is actually using host {} ({})",
                        nick, host, ip
                    )));
                }
            }

            RPL_WHOISMARKS => {
                // Format: 339 <me> <nick> :is marked: <mark>
                if let (Some(nick), Some(text)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} {}",
                        nick, text
                    )));
                }
            }

            RPL_WHOISSECURE => {
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is using a secure connection",
                        nick
                    )));
                }
            }

            RPL_WHOISSPECIAL => {
                // <nick> :<special info> - used by some networks for custom titles/info
                if let (Some(nick), Some(info)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} - {}",
                        nick, info
                    )));
                }
            }

            RPL_WHOISCERTFP => {
                // <nick> :has client certificate fingerprint <fingerprint>
                if let (Some(nick), Some(fp)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} has TLS fingerprint: {}",
                        nick, fp
                    )));
                }
            }

            RPL_WHOISREGNICK => {
                // <nick> :is a registered nick
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is a registered nick",
                        nick
                    )));
                }
            }

            RPL_WHOISBOT => {
                // <nick> :is a Bot
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is a bot",
                        nick
                    )));
                }
            }

            RPL_WHOISHOST => {
                // <nick> :is connecting from <host> <ip>
                if let (Some(nick), Some(info)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} - {}",
                        nick, info
                    )));
                }
            }

            RPL_WHOISMODES => {
                // <nick> :is using modes <modes>
                if let (Some(nick), Some(modes)) = (params.get(1), params.get(2)) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WHOIS] {} is using modes {}",
                        nick, modes
                    )));
                }
            }

            // WHO responses
            RPL_WHOREPLY => {
                // <channel> <user> <host> <server> <nick> <H|G>[*][@|+] :<hopcount> <realname>
                if let (
                    Some(channel),
                    Some(user),
                    Some(host),
                    Some(_server),
                    Some(nick),
                    Some(flags),
                ) = (
                    params.get(1),
                    params.get(2),
                    params.get(3),
                    params.get(4),
                    params.get(5),
                    params.get(6),
                ) {
                    // Update away status based on H (Here) or G (Gone/away) flag
                    let is_away = flags.starts_with('G');
                    let realname = params
                        .get(7)
                        .and_then(|trailing| trailing.split_once(' ').map(|(_, name)| name))
                        .map(str::to_string);
                    self.update_user_identity(
                        nick,
                        Some(Some(user.clone())),
                        Some(Some(host.clone())),
                        None,
                        realname.map(Some),
                        Some(if is_away {
                            Some("Away".to_string())
                        } else {
                            None
                        }),
                        Some(flags.contains('B')),
                        Some(true),
                    );
                    let _ = channel;
                }
            }

            RPL_WHOSPCRPL => {
                // Our request is `%tcuhnafr,<token>`, but WHOX replies in the
                // fixed order `t c u i h s n f d l a o r` whatever the request
                // order was, so after the target:
                // token, channel, user, host, nick, flags, account, realname.
                if let (
                    Some(token),
                    Some(_channel),
                    Some(user),
                    Some(host),
                    Some(nick),
                    Some(flags),
                    Some(account),
                ) = (
                    params.get(1),
                    params.get(2),
                    params.get(3),
                    params.get(4),
                    params.get(5),
                    params.get(6),
                    params.get(7),
                ) && self
                    .pending_whox
                    .iter()
                    .any(|(pending, _)| pending == token)
                {
                    let account = if account == "0" || account == "*" {
                        None
                    } else {
                        Some(account.clone())
                    };
                    self.update_user_identity(
                        nick,
                        Some(Some(user.clone())),
                        Some(Some(host.clone())),
                        Some(account),
                        Some(params.get(8).cloned()),
                        Some(if flags.starts_with('G') {
                            Some("Away".to_string())
                        } else {
                            None
                        }),
                        Some(flags.contains('B')),
                        Some(true),
                    );
                }
            }

            RPL_ENDOFWHO => {
                // WHOX replies include their token but 315 does not. Queries
                // are answered in the order they were sent, so each 315
                // completes only the oldest request for this channel; the
                // tokens of overlapping requests stay valid for their replies.
                if let Some(mask) = params.get(1)
                    && let Some(index) = self.pending_whox.iter().position(|(_, channel)| {
                        self.network_support.identifiers_equal(channel, mask)
                    })
                {
                    self.pending_whox.remove(index);
                }
            }

            // Error numerics
            ERR_NOSUCHNICK => {
                if let Some(nick) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "No such nick/channel: {}",
                        nick
                    )));
                }
            }

            ERR_NOSUCHCHANNEL => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "No such channel: {}",
                        channel
                    )));
                }
            }

            ERR_CANNOTSENDTOCHAN => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Cannot send to channel: {}",
                        channel
                    )));
                }
            }

            ERR_NOTONCHANNEL => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "You're not on that channel: {}",
                        channel
                    )));
                }
            }

            ERR_INVITEONLYCHAN => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Cannot join {} (invite only)",
                        channel
                    )));
                }
            }

            ERR_BANNEDFROMCHAN => {
                if let Some(channel) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Cannot join {} (banned)",
                        channel
                    )));
                }
            }

            ERR_BADCHANNELKEY => {
                if let Some(channel) = params.get(1) {
                    // Remove any pending key since it was wrong
                    self.pending_channel_keys
                        .remove(&self.network_support.canonicalize(channel));
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "Cannot join {} (bad or missing channel key). Use: /join {} <key>",
                        channel, channel
                    )));
                }
            }

            // Quiet list (mode +q)
            RPL_QUIETLIST => {
                // Servers use either <me> <channel> <mask>... or
                // <me> <channel> <mode> <mask>... for 728.
                if let Some(channel) = params.get(1) {
                    let has_mode_field = params.get(2).is_some_and(|value| {
                        value.chars().count() == 1
                            && self.network_support.chanmodes_a.contains(value)
                    });
                    let mask_idx = if has_mode_field { 3 } else { 2 };
                    if let Some(mask) = params.get(mask_idx) {
                        let mode = params
                            .get(2)
                            .filter(|_| has_mode_field)
                            .and_then(|value| value.chars().next())
                            .unwrap_or('q');
                        let setter = params.get(mask_idx + 1).cloned().unwrap_or_default();
                        let set_time = params
                            .get(mask_idx + 2)
                            .and_then(|value| value.parse().ok())
                            .unwrap_or(0);
                        if let Some(ch) = self.channel_mut(channel) {
                            ch.record_mode_list_reply(mode, mask.clone(), setter, set_time);
                        }
                    }
                }
            }

            RPL_ENDOFQUIETLIST => {
                // The end marker names its list like the entries do: either
                // <me> <channel> :text or <me> <channel> <mode> :text.
                let mode = params
                    .get(2)
                    .filter(|value| {
                        value.chars().count() == 1
                            && self.network_support.chanmodes_a.contains(*value)
                    })
                    .and_then(|value| value.chars().next())
                    .unwrap_or('q');
                if let Some(channel) = params.get(1)
                    && let Some(ch) = self.channel_mut(channel)
                {
                    ch.complete_mode_list(mode);
                }
            }

            // IRCv3 Monitor
            RPL_MONONLINE => {
                // :server 730 <nick> :target1,target2,...
                if let Some(targets) = params.get(1) {
                    self.update_presence_targets(targets, true);
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[MONITOR] Online: {}",
                        targets
                    )));
                }
            }

            RPL_MONOFFLINE => {
                // :server 731 <nick> :target1,target2,...
                if let Some(targets) = params.get(1) {
                    self.update_presence_targets(targets, false);
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[MONITOR] Offline: {}",
                        targets
                    )));
                }
            }

            RPL_MONLIST => {
                // :server 732 <nick> :target1,target2,...
                if let Some(targets) = params.get(1) {
                    for nick in targets.trim_start_matches(':').split(',') {
                        if !nick.is_empty() {
                            self.monitored_users
                                .insert(self.network_support.canonicalize(nick));
                        }
                    }
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[MONITOR] List: {}",
                        targets
                    )));
                }
            }

            RPL_ENDOFMONLIST => {
                self.add_message_to_current(ChatMessage::system("[MONITOR] End of monitor list"));
            }

            ERR_MONLISTFULL => {
                if let Some(limit) = params.get(1) {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[MONITOR] List is full (limit: {})",
                        limit
                    )));
                }
            }

            RPL_LOGON | RPL_NOWON | RPL_LOGOFF | RPL_NOWOFF => {
                let online = matches!(num, RPL_LOGON | RPL_NOWON);
                if let Some(nick) = params.get(1) {
                    let username = params.get(2).cloned();
                    let hostname = params.get(3).cloned();
                    self.monitored_users
                        .insert(self.network_support.canonicalize(nick));
                    self.update_user_identity(
                        nick,
                        username.map(Some),
                        hostname.map(Some),
                        None,
                        None,
                        None,
                        None,
                        Some(online),
                    );
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WATCH] {nick} is {}",
                        if online { "online" } else { "offline" }
                    )));
                }
            }

            RPL_WATCHOFF => {
                if let Some(nick) = params.get(1) {
                    self.monitored_users
                        .remove(&self.network_support.canonicalize(nick));
                }
            }

            RPL_WATCHLIST => {
                if let Some(list) = params.last() {
                    for nick in list.split_whitespace() {
                        self.monitored_users
                            .insert(self.network_support.canonicalize(nick));
                    }
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[WATCH] List: {list}"
                    )));
                }
            }

            RPL_WATCHSTAT | RPL_ENDOFWATCHLIST | RPL_CLEARWATCH | ERR_TOOMANYWATCH => {
                let text = params.last().cloned().unwrap_or_default();
                self.add_message_to_current(ChatMessage::system(&format!("[WATCH] {text}")));
            }

            RPL_WHOISKEYVALUE
            | RPL_KEYVALUE
            | RPL_METADATAEND
            | RPL_KEYNOTSET
            | RPL_METADATASUBOK
            | RPL_METADATAUNSUBOK
            | RPL_METADATASUBS
            | RPL_METADATASYNCLATER => {
                let text = params.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
                self.add_server_message(ChatMessage::system_fmt(
                    &format!("[METADATA {num}] {text}"),
                    &self.timestamp_format,
                ));
            }

            RPL_BOUNCERSESSION | RPL_BOUNCETOKEN | RPL_BOUNCERSETTINGS => {
                let text = params.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
                self.add_server_message(ChatMessage::system_fmt(
                    &format!("[BOUNCER {num}] {text}"),
                    &self.timestamp_format,
                ));
            }

            _ => {
                // Show other numerics in current window for visibility
                let text = params.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
                if !text.is_empty() {
                    self.add_message_to_current(ChatMessage::system(&format!(
                        "[{}] {}",
                        num, text
                    )));
                }
            }
        }
    }

    fn add_message_to_channel(&mut self, channel: &str, mut msg: ChatMessage) {
        let existing_key = self.channel_key(channel);
        let key = existing_key.clone().unwrap_or_else(|| channel.to_string());

        // Redact at the shared display/log boundary as defense in depth. This
        // covers ordinary query input, /say, service shortcuts, server echo-
        // message, and future command paths that might otherwise forget to use
        // outgoing_local_echo().
        // sender_is_self (not just the current nick): a bouncer replaying our
        // own credential row under a former nick must still be redacted and
        // excluded from the disk log (#166).
        if self.sender_is_self(&msg.sender)
            && commands::service_message_contains_credentials(&key, &msg.content)
        {
            msg.content = "<credential command sent>".to_string();
            msg.no_log = true;
            msg.render_cache = Default::default();
        }

        let is_new = existing_key.is_none();
        if is_new {
            // Cap auto-created query (non-channel) windows so a flood of messages
            // from many distinct senders cannot grow `channels` without bound.
            // Real channels you joined are exempt; over the cap we fall back to the
            // server buffer instead of opening a new window.
            if !self.is_channel_name(channel) {
                const MAX_QUERY_WINDOWS: usize = 200;
                let query_count = self
                    .conversations
                    .keys()
                    .filter(|k| !self.is_channel_name(k))
                    .count();
                if query_count >= MAX_QUERY_WINDOWS {
                    self.add_server_message(msg);
                    return;
                }
            } else {
                // A new channel tab here is auto-created (not joined by us):
                // cap unjoined channel tabs the same way (#193).
                const MAX_UNJOINED_CHANNEL_WINDOWS: usize = 200;
                let unjoined_count = self
                    .conversations
                    .iter()
                    .filter(|(k, ch)| self.is_channel_name(k) && !ch.joined)
                    .count();
                if unjoined_count >= MAX_UNJOINED_CHANNEL_WINDOWS {
                    self.add_server_message(msg);
                    return;
                }
            }
            let mut new_channel = Channel::new();
            new_channel.set_case_mapping(self.network_support.case_mapping);
            new_channel.set_prefix_schema(
                &self.network_support.prefix_modes,
                &self.network_support.user_prefixes,
            );
            // Load chat history for new query windows (non-channels)
            if self.logging_load_history
                && !self.is_channel_name(&key)
                && let Some((network, legacy_host, allow_legacy)) = self.active_log_context()
            {
                let log_key = self.network_support.canonicalize(&key);
                let history = self.load_text_history(
                    &network,
                    &legacy_host,
                    &log_key,
                    &key,
                    self.logging_history_lines,
                    allow_legacy,
                );
                if !history.is_empty() {
                    new_channel.messages.push_back(ChatMessage::system(&format!(
                        "--- {} lines of history loaded ---",
                        history.len()
                    )));
                    new_channel.messages.extend(history);
                    new_channel.history_loaded = true;
                }
                self.log_manager.log_session_start(&network, &log_key);
            }
            self.conversations.insert(key.clone(), new_channel);
        }
        if msg.msgid.as_ref().is_some_and(|msgid| {
            self.conversations.get(&key).is_some_and(|channel| {
                channel
                    .messages
                    .iter()
                    .any(|existing| existing.msgid.as_ref() == Some(msgid))
            })
        }) {
            return;
        }
        // Log message to disk (display-only messages like /lastlog output are
        // excluded so search results don't pollute the persistent history).
        // Always use the casemapped key: CASEMAPPING-equivalent spellings must
        // never fork one tab into multiple log files (the readable prefix of
        // the path keeps the session's display spelling).
        let text_span = if !msg.no_log
            && let Some((network, _, _)) = self.active_log_context()
        {
            let log_key = self.network_support.canonicalize(&key);
            self.log_manager.log_message(&network, &log_key, &msg)
        } else {
            None
        };

        self.record_archive_message_with_span(&key, &msg, false, text_span);
        // Unread unless it lands in the current tab of a focused window: a
        // selected tab nobody is looking at (minimised, tray-hidden, another
        // window in front) accrues its badge and read marker is held back
        // until focus returns (#305).
        let pending_id = msg.msgid.clone();
        self.clear_typing_sender(&key, &msg.sender);
        let seen = self.window_focused
            && self.current_target_is(&key)
            && self.reading_conversation == self.current_conversation
            && !self.timeline.borrow().restore_pending;
        if let Some(ch) = self.conversations.get_mut(&key) {
            ch.push_trimmed(msg, self.max_scrollback);
            if !seen {
                ch.unread += 1;
            }
        }
        if let Some(id) = pending_id {
            self.apply_pending_reactions(&key, &id);
        }
        self.refresh_reply_previews(&key);
        if seen {
            self.mark_target_read(&key);
        }
    }

    pub fn add_server_message(&mut self, msg: ChatMessage) {
        self.server_messages.push_back(msg);
        // Limit scrollback
        while self.server_messages.len() > self.max_scrollback {
            self.server_messages.pop_front();
        }
        // Unread unless the server buffer is shown in a focused window
        if self.current_conversation.is_some() || !self.window_focused {
            self.server_unread += 1;
        }
    }

    /// Evict a departed user who shares no channel and is not monitored (#195).
    fn evict_known_user_if_orphaned(&mut self, nick: &str) {
        let key = self.network_support.canonicalize(nick);
        if self.monitored_users.contains(&key) {
            return;
        }
        if self
            .conversations
            .values()
            .any(|channel| channel.has_user(nick))
        {
            return;
        }
        self.known_users.remove(&key);
    }

    /// Update a user's away status in all channels they're in
    pub fn update_user_away_status(&mut self, nick: &str, away_msg: Option<String>) {
        self.update_user_identity(nick, None, None, None, None, Some(away_msg), None, None);
    }

    #[allow(clippy::too_many_arguments)]
    fn update_user_identity(
        &mut self,
        nick: &str,
        username: Option<Option<String>>,
        hostname: Option<Option<String>>,
        account: Option<Option<String>>,
        realname: Option<Option<String>>,
        away: Option<Option<String>>,
        bot: Option<bool>,
        online: Option<bool>,
    ) {
        if nick.is_empty() {
            return;
        }
        let key = self.network_support.canonicalize(nick);
        let identity = self.known_users.entry(key).or_insert_with(|| KnownUser {
            nick: nick.to_string(),
            ..KnownUser::default()
        });
        identity.nick = nick.to_string();
        if let Some(value) = username {
            identity.username = value;
        }
        if let Some(value) = hostname {
            identity.hostname = value;
        }
        if let Some(value) = account {
            identity.account = value;
        }
        if let Some(value) = realname {
            identity.realname = value;
        }
        if let Some(value) = away {
            identity.away = value;
        }
        if let Some(value) = bot {
            identity.bot = value;
        }
        if let Some(value) = online {
            identity.online = value;
        }
        let identity = identity.clone();
        for channel in self.conversations.values_mut() {
            channel.apply_user_identity(&identity);
        }
    }

    fn prefix_user_host(prefix: Option<&str>) -> (Option<String>, Option<String>) {
        let Some(prefix) = prefix else {
            return (None, None);
        };
        let Some((_, user_host)) = prefix.split_once('!') else {
            return (None, None);
        };
        let Some((username, hostname)) = user_host.split_once('@') else {
            return (Some(user_host.to_string()), None);
        };
        (Some(username.to_string()), Some(hostname.to_string()))
    }

    fn update_presence_targets(&mut self, targets: &str, online: bool) {
        for target in targets.trim_start_matches(':').split(',') {
            let target = target.trim();
            if target.is_empty() {
                continue;
            }
            let nick = target.split('!').next().unwrap_or(target);
            let (username, hostname) = Self::prefix_user_host(Some(target));
            let key = self.network_support.canonicalize(nick);
            self.monitored_users.insert(key);
            // A bare nick says nothing about user@host; keep what JOIN, NAMES
            // or WHO already taught us rather than clearing it.
            self.update_user_identity(
                nick,
                username.map(Some),
                hostname.map(Some),
                None,
                None,
                None,
                None,
                Some(online),
            );
        }
    }

    /// Add a message to the current window (channel or server buffer)
    fn add_message_to_current(&mut self, msg: ChatMessage) {
        if let Some(channel_name) = &self.current_target_name().map(str::to_owned) {
            self.add_message_to_channel(channel_name, msg);
        } else {
            self.add_server_message(msg);
        }
    }

    #[cfg(test)]
    fn command_fits_irc_line(cmd: &IrcCommand) -> bool {
        Self::command_fits_limits(cmd, IRC_MAX_LINE_BYTES)
    }

    fn command_fits_limits(cmd: &IrcCommand, base_limit: usize) -> bool {
        if let IrcCommand::ClientBatch(commands) | IrcCommand::ClientChat { commands, .. } = cmd {
            return !commands.is_empty()
                && commands
                    .iter()
                    .all(|command| Self::command_fits_limits(command, base_limit));
        }
        let line = format!("{}", cmd);
        if line.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0)) {
            return false;
        }
        if let Some(tagged) = line.strip_prefix('@')
            && let Some((tags, rest)) = tagged.split_once(' ')
        {
            return tags.len() <= 4094 && rest.len() <= base_limit;
        }
        line.len() <= base_limit
    }

    /// Verb and wire size of a command, for logs. The parameters can carry
    /// credentials (PASS, NickServ IDENTIFY, AUTHENTICATE), so they are never
    /// logged (#227), and neither is the size of a credential command: it
    /// would give away the length of the secret.
    fn command_log_summary(cmd: &IrcCommand) -> String {
        if let IrcCommand::ClientBatch(commands) | IrcCommand::ClientChat { commands, .. } = cmd {
            return format!("CLIENTBATCH ({} commands)", commands.len());
        }
        let line = cmd.to_string();
        let untagged = line
            .strip_prefix('@')
            .and_then(|tagged| tagged.split_once(' '))
            .map_or(line.as_str(), |(_, rest)| rest);
        let verb: String = untagged
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .take(16)
            .collect();
        if commands::command_line_contains_credentials(untagged) {
            verb
        } else {
            format!("{verb} ({} bytes)", line.len())
        }
    }

    fn client_tag_is_denied(&self, command: &IrcCommand) -> Option<String> {
        if let IrcCommand::ClientBatch(commands) | IrcCommand::ClientChat { commands, .. } = command
        {
            return commands
                .iter()
                .find_map(|command| self.client_tag_is_denied(command));
        }
        let deny = self.network_support.client_tag_deny.as_deref()?;
        let line = command.to_string();
        let tags = line.strip_prefix('@')?.split_once(' ')?.0;
        let entries: Vec<&str> = deny.split(',').filter(|entry| !entry.is_empty()).collect();
        let block_all = entries.first().is_some_and(|entry| *entry == "*");
        for tag in tags.split(';') {
            let name = tag.split('=').next().unwrap_or(tag);
            let Some(name) = name.strip_prefix('+') else {
                continue;
            };
            let exempt = entries
                .iter()
                .any(|entry| entry.strip_prefix('-') == Some(name));
            let specifically_blocked = entries.contains(&name);
            if specifically_blocked || (block_all && !exempt) {
                return Some(name.to_string());
            }
        }
        None
    }

    #[cfg(test)]
    fn split_irc_text_payload(
        command: &str,
        target: &str,
        text: &str,
        wrapper_prefix: &str,
        wrapper_suffix: &str,
    ) -> Option<Vec<String>> {
        Self::split_irc_text_payload_with_limit(
            command,
            target,
            text,
            wrapper_prefix,
            wrapper_suffix,
            IRC_MAX_LINE_BYTES,
        )
    }

    fn split_irc_text_payload_with_limit(
        command: &str,
        target: &str,
        text: &str,
        wrapper_prefix: &str,
        wrapper_suffix: &str,
        line_limit: usize,
    ) -> Option<Vec<String>> {
        let overhead =
            format!("{} {} :", command, target).len() + wrapper_prefix.len() + wrapper_suffix.len();
        if overhead >= line_limit {
            return None;
        }
        let max_payload_bytes = line_limit - overhead;
        if text.len() <= max_payload_bytes {
            return Some(vec![format!(
                "{}{}{}",
                wrapper_prefix, text, wrapper_suffix
            )]);
        }

        let mut chunks = Vec::new();
        let mut remaining = text;
        while !remaining.is_empty() {
            let mut end = remaining.len().min(max_payload_bytes);
            while end > 0 && !remaining.is_char_boundary(end) {
                end -= 1;
            }
            if end == 0 {
                return None;
            }
            chunks.push(format!(
                "{}{}{}",
                wrapper_prefix,
                &remaining[..end],
                wrapper_suffix
            ));
            remaining = &remaining[end..];
        }
        Some(chunks)
    }

    fn send_text_parts(
        &mut self,
        command: &str,
        target: &str,
        chunks: Vec<String>,
        sensitive: bool,
    ) -> TextSendOutcome {
        // A fragmented service command is not a valid authentication command,
        // and its continuation would evade keyword-based credential filtering.
        if sensitive && chunks.len() > 1 {
            self.add_message_to_current(
                ChatMessage::system_fmt(
                    "Credential command not sent - it must fit in one IRC message",
                    &self.timestamp_format,
                )
                .without_logging(),
            );
            return TextSendOutcome::default();
        }
        let make_command = |chunk: String| match command {
            "NOTICE" => IrcCommand::Notice(target.to_string(), chunk),
            _ => IrcCommand::Privmsg(target.to_string(), chunk),
        };
        // Validate every part before queueing any of them (for example, a NUL
        // in the last pasted line must not send the preceding lines).
        if !chunks
            .iter()
            .all(|chunk| self.validate_command(&make_command(chunk.clone())))
        {
            return TextSendOutcome::default();
        }
        let mut outcome = TextSendOutcome {
            complete: true,
            payloads: Vec::new(),
        };
        if self.send_command(if chunks.len() == 1 {
            make_command(chunks[0].clone())
        } else {
            IrcCommand::ClientBatch(chunks.iter().cloned().map(make_command).collect())
        }) {
            outcome.payloads = chunks;
        } else {
            outcome.complete = false;
        }
        outcome
    }

    fn send_text_command(
        &mut self,
        command: &str,
        target: &str,
        text: &str,
        wrapper_prefix: &str,
        wrapper_suffix: &str,
    ) -> TextSendOutcome {
        let mut normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        // As in the composer (#380): blank edge lines are dropped, and blank
        // interior lines are never sent as empty messages one per line.
        if normalized.contains('\n') {
            normalized = message_ui::trim_blank_edges(&normalized);
        }
        let multi_line = normalized.contains('\n');
        let sensitive = commands::service_message_contains_credentials(target, &normalized)
            || normalized
                .split('\n')
                .any(|line| commands::service_message_contains_credentials(target, line));
        let line_limit = self
            .network_support
            .line_len
            .unwrap_or(IRC_MAX_LINE_BYTES + 2)
            .saturating_sub(2)
            .max(IRC_MAX_LINE_BYTES);
        let line_limit = line_limit.saturating_sub(self.relay_prefix_reserve(line_limit));
        let mut chunks = Vec::new();
        let mut sent_lines = 0;
        for logical_line in normalized.split('\n') {
            if multi_line && logical_line.trim().is_empty() {
                continue;
            }
            sent_lines += 1;
            let Some(mut line_chunks) = Self::split_irc_text_payload_with_limit(
                command,
                target,
                logical_line,
                wrapper_prefix,
                wrapper_suffix,
                line_limit,
            ) else {
                self.add_message_to_current(
                    ChatMessage::system_fmt(
                        "Message not sent - target name leaves no room for text",
                        &self.timestamp_format,
                    )
                    .without_logging(),
                );
                return TextSendOutcome::default();
            };
            chunks.append(&mut line_chunks);
        }
        let full_text = format!("{wrapper_prefix}{normalized}{wrapper_suffix}");
        let use_multiline = self.enabled_caps.contains("draft/multiline")
            && (normalized.contains('\n') || chunks.len() > 1)
            && self
                .network_support
                .multiline_max_bytes
                .is_none_or(|max| full_text.len() <= max);
        if !use_multiline {
            // Unbatched lines go out in one burst; refuse a flooding paste.
            // One long line still goes out in as many chunks as it needs.
            // The commands then add "Message not sent", as the composer does.
            if sent_lines > message_ui::MAX_FALLBACK_LINES {
                self.add_message_to_current(
                    ChatMessage::system_fmt(
                        &message_ui::SendError::TooManyLines.to_string(),
                        &self.timestamp_format,
                    )
                    .without_logging(),
                );
                return TextSendOutcome::default();
            }
            return self.send_text_parts(command, target, chunks, sensitive);
        }

        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_BATCH: AtomicU64 = AtomicU64::new(1);
        let batch_id = format!("lf{}", NEXT_BATCH.fetch_add(1, Ordering::Relaxed));
        let concat_overhead =
            format!("@batch={batch_id};draft/multiline-concat {command} {target} :").len();
        if concat_overhead >= IRC_MAX_LINE_BYTES {
            return TextSendOutcome::default();
        }
        let max_payload = line_limit.saturating_sub(concat_overhead);
        if max_payload == 0 {
            return TextSendOutcome::default();
        }
        let mut lines: Vec<(String, bool)> = Vec::new();
        for logical_line in full_text.split('\n') {
            if logical_line.is_empty() {
                lines.push((String::new(), false));
                continue;
            }
            let mut rest = logical_line;
            let mut first = true;
            while !rest.is_empty() {
                let mut end = rest.len().min(max_payload);
                while end > 0 && !rest.is_char_boundary(end) {
                    end -= 1;
                }
                if end == 0 {
                    return TextSendOutcome::default();
                }
                lines.push((rest[..end].to_string(), !first));
                first = false;
                rest = &rest[end..];
            }
        }
        if self
            .network_support
            .multiline_max_lines
            .is_some_and(|max| lines.len() > max)
        {
            return self.send_text_parts(command, target, chunks, sensitive);
        }
        let mut wire = vec![IrcCommand::Batch(
            format!("+{batch_id}"),
            Some("draft/multiline".to_string()),
            Some(target.to_string()),
        )];
        for (line, concat) in lines {
            let concat_tag = if concat {
                ";draft/multiline-concat"
            } else {
                ""
            };
            wire.push(IrcCommand::Raw(format!(
                "@batch={batch_id}{concat_tag} {command} {target} :{line}"
            )));
        }
        wire.push(IrcCommand::Batch(format!("-{batch_id}"), None, None));
        if !wire.iter().all(|command| self.validate_command(command)) {
            return TextSendOutcome::default();
        }
        if !self.send_command(if wire.len() == 1 {
            wire.remove(0)
        } else {
            IrcCommand::ClientBatch(wire)
        }) {
            return TextSendOutcome::default();
        }
        TextSendOutcome {
            complete: true,
            payloads: vec![full_text],
        }
    }

    fn send_privmsg_text(&mut self, target: &str, text: &str) -> TextSendOutcome {
        if !self.can_send_to_target(target) {
            return TextSendOutcome::default();
        }
        self.send_text_command("PRIVMSG", target, text, "", "")
    }

    fn send_notice_text(&mut self, target: &str, text: &str) -> TextSendOutcome {
        if !self.can_send_to_target(target) {
            return TextSendOutcome::default();
        }
        self.send_text_command("NOTICE", target, text, "", "")
    }

    fn send_action_text(&mut self, target: &str, text: &str) -> TextSendOutcome {
        if !self.can_send_to_target(target) {
            return TextSendOutcome::default();
        }
        self.send_text_command("PRIVMSG", target, text, "\x01ACTION ", "\x01")
    }

    /// Local rows describe exactly what was queued, so each echo can acquire
    /// the server's identity without duplicating an unsplit optimistic row.
    fn send_chat_text(&mut self, target: &str, text: &str, action: bool) -> bool {
        if self.is_connected()
            && let Some(id) = self.id_for_target(target)
        {
            let request = message_ui::ChatSendRequest {
                target: id,
                body: text.into(),
                kind: if action {
                    message_ui::ChatKind::Action
                } else {
                    message_ui::ChatKind::Privmsg
                },
                reply_to: None,
            };
            return match self.send_chat_request(&request) {
                message_ui::SendReceipt::Queued => true,
                message_ui::SendReceipt::Rejected(error) => {
                    self.add_message_to_current(
                        ChatMessage::system_fmt(&error.to_string(), &self.timestamp_format)
                            .without_logging(),
                    );
                    false
                }
            };
        }
        let outcome = if action {
            self.send_action_text(target, text)
        } else {
            self.send_privmsg_text(target, text)
        };
        let sensitive = commands::service_message_contains_credentials(target, text);
        for payload in outcome.payloads {
            let content = if action {
                payload
                    .strip_prefix("\x01ACTION ")
                    .and_then(|text| text.strip_suffix('\x01'))
                    .unwrap_or(&payload)
            } else {
                &payload
            };
            let row =
                if sensitive || commands::service_message_contains_credentials(target, content) {
                    ChatMessage::new_fmt(
                        &self.my_nick,
                        "<credential command sent>",
                        &self.timestamp_format,
                    )
                    .without_logging()
                } else if action {
                    ChatMessage::action_fmt(&self.my_nick, content, &self.timestamp_format)
                        .with_pending_echo()
                } else {
                    commands::outgoing_local_echo(
                        target,
                        &self.my_nick,
                        content,
                        &self.timestamp_format,
                    )
                };
            self.add_message_to_channel(target, row);
        }
        self.automation_send_failed |= !outcome.complete;
        outcome.complete
    }

    fn send_notice_with_confirmation(&mut self, target: &str, text: &str) -> bool {
        if self.is_connected()
            && let Some(id) = self.id_for_target(target)
        {
            let request = message_ui::ChatSendRequest {
                target: id,
                body: text.into(),
                kind: message_ui::ChatKind::Notice,
                reply_to: None,
            };
            return match self.send_chat_request(&request) {
                message_ui::SendReceipt::Queued => true,
                message_ui::SendReceipt::Rejected(error) => {
                    self.add_message_to_current(
                        ChatMessage::system_fmt(&error.to_string(), &self.timestamp_format)
                            .without_logging(),
                    );
                    false
                }
            };
        }
        let outcome = self.send_notice_text(target, text);
        let sensitive = commands::service_message_contains_credentials(target, text);
        for payload in outcome.payloads {
            let row = if sensitive
                || commands::service_message_contains_credentials(target, &payload)
            {
                ChatMessage::system_fmt(
                    &format!("-> -{target}- <credential command sent>"),
                    &self.timestamp_format,
                )
                .without_logging()
            } else {
                ChatMessage::system_fmt(&format!("-> -{target}- {payload}"), &self.timestamp_format)
            };
            self.add_message_to_current(row);
        }
        self.automation_send_failed |= !outcome.complete;
        outcome.complete
    }

    /// Feedback for a failed send. While the connection channel is gone the
    /// command was silently dropped, so say so. While it is live — including
    /// pre-WELCOME CAP/SASL negotiation — send_command already printed its
    /// own accurate failure row, and a "Not connected" row here would
    /// contradict it (also wrong during negotiation, where connected is
    /// still false; #161).
    fn report_send_failure(&mut self, sent: bool, message: &str) {
        self.automation_send_failed |= !sent;
        if !sent && !self.has_transport() {
            // without_logging: connection-scope client state does not belong
            // in a channel's on-disk history, which add_message_to_channel
            // would otherwise write (#166).
            self.add_message_to_current(ChatMessage::system(message).without_logging());
        }
    }

    fn validate_command(&mut self, cmd: &IrcCommand) -> bool {
        let base_limit = self
            .network_support
            .line_len
            .unwrap_or(IRC_MAX_LINE_BYTES.saturating_add(2))
            .saturating_sub(2)
            .max(IRC_MAX_LINE_BYTES);
        if !Self::command_fits_limits(cmd, base_limit) {
            tracing::warn!(
                "Outgoing command not sent (line too long): {}",
                Self::command_log_summary(cmd)
            );
            self.add_message_to_current(
                ChatMessage::system_fmt(
                    "Command not sent - IRC line is too long",
                    &self.timestamp_format,
                )
                .without_logging(),
            );
            self.automation_send_failed = true;
            return false;
        }
        if let Some(tag) = self.client_tag_is_denied(cmd) {
            self.add_message_to_current(
                ChatMessage::system_fmt(
                    &format!("Command not sent - server blocks client tag +{tag}"),
                    &self.timestamp_format,
                )
                .without_logging(),
            );
            self.automation_send_failed = true;
            return false;
        }
        true
    }

    pub fn send_command(&mut self, cmd: IrcCommand) -> bool {
        self.send_command_with_history(cmd, None)
    }

    fn send_command_with_history(
        &mut self,
        cmd: IrcCommand,
        history: Option<PendingHistoryRequest>,
    ) -> bool {
        if !self.validate_command(&cmd) {
            self.automation_send_failed = true;
            return false;
        }
        if history
            .as_ref()
            .and_then(|request| request.correlation.as_ref())
            .is_some_and(|correlation| {
                correlation
                    .owner
                    .is_none_or(|owner| self.target_name(owner).is_none())
            })
        {
            return false;
        }
        let outgoing_history = Self::outgoing_history_request(&cmd);
        let account_login = match &cmd {
            IrcCommand::ClientChat { commands, .. } if commands.len() == 1 => {
                Self::account_login_service(&commands[0])
            }
            _ => Self::account_login_service(&cmd),
        };
        let upload_token_request = Self::outgoing_upload_token_request(&cmd);
        let account_whois = self.outgoing_account_whois(&cmd);
        // Track intent only after the command really entered the transport.
        // Raw/tagged forms share the same leave/rejoin semantics.
        let parsed = if let IrcCommand::Raw(line) = &cmd {
            IrcMessage::parse(line)
        } else {
            None
        };
        let membership = match parsed.as_ref().map_or(&cmd, |message| &message.command) {
            IrcCommand::Part(target, _) => Some((target.clone(), None, false)),
            IrcCommand::Join(target, key, _, _) => Some((target.clone(), key.clone(), true)),
            _ => None,
        };
        if let Some(tx) = self.connection.sender() {
            if let Err(e) = tx.send(cmd) {
                self.automation_send_failed = true;
                tracing::warn!(
                    "Outgoing command not sent (disconnected): {}",
                    Self::command_log_summary(&e.0)
                );
                return false;
            }
            if let Some((targets, keys, joining)) = membership {
                self.note_membership_intent(&targets, keys.as_deref(), joining);
            }
            if let Some(outgoing) = outgoing_history {
                self.record_history_request(outgoing, history);
            }
            if let Some(request) = upload_token_request {
                self.record_upload_token_request(request);
            }
            if let Some(service) = account_login {
                self.note_account_login_sent(service);
            }
            if let Some(request) = account_whois {
                self.note_account_whois_sent(request);
            }
            true
        } else {
            self.automation_send_failed = true;
            false
        }
    }

    fn note_membership_intent(&mut self, targets: &str, keys: Option<&str>, joining: bool) {
        if joining && targets == "0" {
            self.cancel_all_automatic_joins();
            // JOIN 0 is the protocol spelling for leaving every channel.
            let mut names: Vec<_> = self
                .conversations
                .keys()
                .filter(|name| self.is_channel_name(name))
                .cloned()
                .collect();
            if let Some(rejoins) = self.pending_rejoins.as_ref() {
                names.extend(rejoins.iter().map(|(name, _)| name.clone()));
            }
            for name in names {
                self.note_membership_intent(&name, None, false);
            }
            return;
        }
        let mut keys = keys.unwrap_or("").split(',');
        for name in targets
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            self.cancel_automatic_join(name);
            let key = keys
                .next()
                .filter(|key| !key.is_empty())
                .map(str::to_string);
            if !joining {
                let known = self.channel_key(name).is_some()
                    || self.pending_rejoins.as_ref().is_some_and(|rejoins| {
                        rejoins
                            .iter()
                            .any(|(target, _)| self.identifiers_equal(target, name))
                    });
                self.forget_rejoin_membership(name);
                self.pending_channel_keys
                    .remove(&self.network_support.canonicalize(name));
                self.take_join_focus_request(name);
                if known && !self.is_parting(name) {
                    self.pending_parts.push(name.to_string());
                }
                continue;
            }
            let was_parting = self.is_parting(name);
            self.forget_part_intent(name);
            let already_joined = self
                .channel_key(name)
                .and_then(|key| self.conversations.get(&key))
                .is_some_and(|channel| channel.joined);
            if (!self.is_connected() && self.pending_rejoins.is_none())
                || (already_joined && !was_parting)
            {
                continue;
            }
            let support = &self.network_support;
            let rejoins = self.pending_rejoins.get_or_insert_with(Vec::new);
            if let Some(existing) = rejoins
                .iter_mut()
                .find(|(target, _)| support.identifiers_equal(target, name))
            {
                *existing = (name.to_string(), key);
            } else {
                rejoins.push((name.to_string(), key));
            }
        }
    }

    /// Send a JOIN and remember its key until the server's own-JOIN echo
    /// attaches it to the tab, whether that tab is new or retained. Every
    /// user-initiated JOIN goes through here so /cycle, keyed auto-join
    /// entries and rejoins on a kicked tab keep the key (#250).
    pub(crate) fn send_join(&mut self, channel: String, key: Option<String>) -> bool {
        let sent = self.send_join_in_background(channel.clone(), key);
        if sent {
            self.request_join_focus(&channel, false);
        }
        sent
    }

    /// The JOIN that follows a PART in /cycle. The tab still reads as joined
    /// until the PART echo arrives, but the JOIN echo will recreate it, so the
    /// focus request must be recorded even then.
    pub(crate) fn send_rejoin(&mut self, channel: String, key: Option<String>) -> bool {
        let sent = self.send_join_in_background(channel.clone(), key);
        if sent {
            self.request_join_focus(&channel, true);
        }
        sent
    }

    pub(crate) fn cycle_conversation(&mut self, id: ConversationId) -> bool {
        if self.pending_cycles.contains(&id) {
            return false;
        }
        let Some(target) = self.target_name(id).map(str::to_owned) else {
            return false;
        };
        let key = self
            .conversations
            .channel(id)
            .and_then(|channel| channel.key.clone());
        if !self.send_command(IrcCommand::Part(target.clone(), Some("Cycling".into()))) {
            return false;
        }
        // The accepted PART must retain the view even if the following JOIN
        // cannot be queued. Another cycle waits for its outcome instead of
        // sharing one pending bit across several PART/JOIN pairs.
        self.pending_cycles.insert(id);
        if !self.send_rejoin(target, key) {
            return false;
        }
        true
    }

    /// The user asked to join `channels` (a comma list is several requests):
    /// each channel's own-JOIN echo may take focus. A channel the user is
    /// already in produces no echo to wait for, so it is focused right away
    /// instead of leaving a request that a later unsolicited JOIN would
    /// consume (#290).
    fn request_join_focus(&mut self, channels: &str, rejoining: bool) {
        const MAX_JOIN_FOCUS_REQUESTS: usize = 64;
        for name in channels
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let joined_tab = self.channel_key(name).filter(|key| {
                self.conversations
                    .get(key)
                    .is_some_and(|channel| channel.joined)
            });
            if let Some(key) = joined_tab
                && !rejoining
            {
                self.select_tab(self.id_for_target(&key));
                continue;
            }
            if self
                .join_focus_requests
                .iter()
                .any(|requested| self.identifiers_equal(requested, name))
            {
                continue;
            }
            if self.join_focus_requests.len() >= MAX_JOIN_FOCUS_REQUESTS {
                self.join_focus_requests.remove(0);
            }
            self.join_focus_requests.push(name.to_string());
        }
    }

    /// 470: the server forwarded the JOIN of `from` to `to`, so the request
    /// follows the redirect.
    fn redirect_join_focus_request(&mut self, from: &str, to: &str) {
        if self.take_join_focus_request(from) {
            self.request_join_focus(to, false);
        }
    }

    /// JOIN on the client's own initiative (the auto-join list): same key
    /// bookkeeping as `send_join`, but the confirmed tab never takes focus.
    fn send_join_in_background(&mut self, channel: String, key: Option<String>) -> bool {
        let sent = self.send_command(IrcCommand::Join(channel.clone(), key.clone(), None, None));
        // Store the key only for a channel actually being joined. A comma
        // list joins several channels with one command and the server
        // confirms each with its own JOIN echo, so the key list is paired
        // with the channel list positionally (an absent or empty entry
        // means that channel got no key).
        if sent && let Some(key) = key {
            if channel.contains(',') {
                let mut keys = key.split(',');
                for name in channel.split(',').map(str::trim) {
                    if let Some(entry) = keys.next().filter(|entry| !entry.is_empty())
                        && !name.is_empty()
                    {
                        self.pending_channel_keys
                            .insert(self.network_support.canonicalize(name), entry.to_string());
                    }
                }
            } else {
                self.pending_channel_keys
                    .insert(self.network_support.canonicalize(&channel), key);
            }
        }
        sent
    }

    /// Consume the user's request to focus `channel`, if there is one.
    /// Forget every piece of state the attempt to join `channel` left behind
    /// once the server refuses or forwards it, so nothing attaches to a later
    /// JOIN of that channel or is replayed on reconnect (#290).
    fn abandon_refused_join(&mut self, channel: &str) {
        if let Some(id) = self.id_for_target(channel) {
            self.pending_cycles.remove(&id);
        }
        self.pending_channel_keys
            .remove(&self.network_support.canonicalize(channel));
        self.take_join_focus_request(channel);
        self.forget_rejoin_membership(channel);
    }

    fn take_join_focus_request(&mut self, channel: &str) -> bool {
        let Some(index) = self
            .join_focus_requests
            .iter()
            .position(|requested| self.identifiers_equal(requested, channel))
        else {
            return false;
        };
        self.join_focus_requests.remove(index);
        true
    }

    pub fn process_input(&mut self) {
        let input = self.input_text.trim().to_string();
        if input.is_empty() {
            return;
        }

        // Credentials must never enter the Up/Down command history. Besides
        // explicit /msg and /query forms, account for ordinary input and /say
        // while the active query is NickServ/AuthServ.
        let contains_credentials = self.draft_is_sensitive(&input);

        // Save non-sensitive input to history (avoid duplicates of last entry).
        if !contains_credentials && self.command_history.last() != Some(&input) {
            self.command_history.push(input.clone());
            if self.command_history.len() > 100 {
                self.command_history.remove(0);
            }
        }

        // Reset history browsing state
        self.history_index = None;
        self.history_temp.clear();

        self.input_text.clear();

        if input.starts_with('/') {
            self.process_command(&input);
        } else if let Some(channel) = &self.current_target_name().map(str::to_owned) {
            // Don't echo a message we can't actually send: without a live
            // connection the command channel is gone and the line would be lost.
            if !self.is_connected() || !self.has_transport() {
                self.add_message_to_current(
                    ChatMessage::system("Not connected - message not sent").without_logging(),
                );
                return;
            }

            if !self.can_send_to_target(channel) {
                self.add_message_to_current(
                    ChatMessage::system_fmt(
                        "You are not joined to this channel - message not sent",
                        &self.timestamp_format,
                    )
                    .without_logging(),
                );
                return;
            }

            // Auto-back: clear away status when user sends a message
            if self.away_status.is_some() {
                let _ = self.send_command(IrcCommand::Away(None));
                self.away_status = None;
                self.auto_away_triggered = false;
                // Update our own status in all channels
                let my_nick = self.my_nick.clone();
                self.update_user_away_status(&my_nick, None);
                self.add_server_message(ChatMessage::system(
                    "You are no longer marked as away (auto-back)",
                ));
            }

            // Send message to current channel
            if !self.send_chat_text(channel, &input, false) {
                self.add_message_to_current(
                    ChatMessage::system_fmt("Message not sent", &self.timestamp_format)
                        .without_logging(),
                );
            }
        }
    }

    fn history_up(&mut self) {
        if self.command_history.is_empty() {
            return;
        }

        match self.history_index {
            None => {
                // Start browsing - save current input
                self.history_temp = self.input_text.clone();
                self.history_index = Some(self.command_history.len() - 1);
                self.input_text = self.command_history.last().unwrap().clone();
            }
            Some(i) if i > 0 => {
                self.history_index = Some(i - 1);
                self.input_text = self.command_history[i - 1].clone();
            }
            _ => {} // At beginning, do nothing
        }
    }

    fn history_down(&mut self) {
        if let Some(i) = self.history_index {
            if i + 1 < self.command_history.len() {
                self.history_index = Some(i + 1);
                self.input_text = self.command_history[i + 1].clone();
            } else {
                // Past end of history - restore temp
                self.history_index = None;
                self.input_text = self.history_temp.clone();
                self.history_temp.clear();
            }
        }
    }

    fn find_nick_completions(&self, prefix: &str) -> Vec<String> {
        if let Some(channel_name) = self.current_target_name()
            && let Some(channel) = self.conversations.get(channel_name)
        {
            let prefix_key = self.network_support.canonicalize(prefix);
            let mut matches: Vec<String> = channel
                .users
                .iter()
                .filter(|user| {
                    self.network_support
                        .canonicalize(&user.nick)
                        .starts_with(&prefix_key)
                })
                .map(|user| user.nick.clone())
                .collect();
            matches.sort_by_key(|a| self.network_support.canonicalize(a));
            return matches;
        }
        Vec::new()
    }

    fn find_command_completions(&self, prefix: &str) -> Vec<String> {
        let prefix = prefix.to_ascii_lowercase();
        let mut matches: Vec<_> = COMMAND_COMPLETIONS
            .iter()
            .filter(|command| command.starts_with(&prefix))
            .map(|command| (*command).to_string())
            .collect();
        matches.extend(self.alias_completions(&prefix));
        matches.sort();
        matches.dedup();
        matches
    }

    fn handle_tab_completion(&mut self) {
        // Once completion starts, cycle using the original replacement span;
        // the inserted trailing space must not become the next prefix.
        if let Some(completion) = &mut self.tab_completion {
            if completion.matches.is_empty() {
                return;
            }
            completion.index = (completion.index + 1) % completion.matches.len();
            self.input_text = format!(
                "{}{}{}",
                completion.base, completion.matches[completion.index], completion.suffix
            );
            return;
        }

        let input = self.input_text.clone();
        let (prefix, matches, base, suffix) =
            if input.starts_with('/') && !input.chars().any(char::is_whitespace) {
                (
                    input.clone(),
                    self.find_command_completions(&input),
                    String::new(),
                    " ".to_string(),
                )
            } else {
                // Advance past the whole whitespace character: multi-byte
                // whitespace (U+00A0, U+3000, ...) would otherwise leave `idx + 1`
                // mid-character and the slicing below would panic.
                let start = input.rfind(char::is_whitespace).map_or(0, |idx| {
                    idx + input[idx..].chars().next().map_or(1, char::len_utf8)
                });
                let prefix = input[start..].to_string();
                let suffix = if start == 0 { ": " } else { " " };
                (
                    prefix.clone(),
                    self.find_nick_completions(&prefix),
                    input[..start].to_string(),
                    suffix.to_string(),
                )
            };

        if prefix.is_empty() || matches.is_empty() {
            return;
        }

        self.input_text = format!("{}{}{}", base, matches[0], suffix);
        self.tab_completion = Some(TabCompletion {
            matches,
            index: 0,
            base,
            suffix,
        });
    }

    fn reset_tab_completion(&mut self) {
        self.tab_completion = None;
    }

    /// Keep the "Unread messages" marker above messages that were unread
    /// when this window came to show them, and retire it afterwards. It marks
    /// the first unread message when there is no marker yet, and it goes once
    /// everything under it was read and the conversation has moved on (a
    /// newer message arrived while the newest ones are on screen). Messages
    /// read as they arrive therefore never get one, and a new absence marks
    /// its own messages rather than keeping the old position.
    fn settle_read_boundary(&self, msgs: &VecDeque<ChatMessage>, unread: usize) {
        let Some(newest) = msgs.back() else {
            return;
        };
        {
            let mut timeline = self.timeline.borrow_mut();
            let at_bottom = timeline
                .anchor
                .as_ref()
                .is_none_or(|anchor| anchor.at_bottom);
            if timeline.entry_read_boundary.is_some() {
                let caught_up = timeline
                    .last_read
                    .as_ref()
                    .is_some_and(|read| message_ref_names(read, newest));
                if caught_up {
                    if timeline.caught_up_tail.as_deref() != Some(newest.message_key.as_str()) {
                        timeline.caught_up_tail = Some(newest.message_key.clone());
                    }
                } else if at_bottom
                    && timeline
                        .caught_up_tail
                        .as_ref()
                        .is_some_and(|seen| *seen != newest.message_key)
                {
                    timeline.entry_read_boundary = None;
                    timeline.caught_up_tail = None;
                }
            }
        }
        if self.timeline.borrow().entry_read_boundary.is_none()
            && unread > 0
            && unread < msgs.len()
            && let Some(target) = self.current_target_name()
        {
            self.timeline.borrow_mut().entry_read_boundary =
                Some(self.live_message_reference(target, &msgs[msgs.len() - unread - 1]));
        }
    }

    /// Draw the active view's message scrollback, virtualized: only the rows
    /// intersecting the viewport (plus one straddler below) are laid out
    /// visibly each frame. Row heights are cached per message and invalidated
    /// as a whole whenever the content width or font size changes.
    ///
    /// Heights are not estimated from font metrics: a never-measured row is
    /// laid out once inside a scratch Ui parked far below the clip rect (so it
    /// paints nothing) and its real laid-out advance is cached. Measurement
    /// and drawing therefore cannot disagree, which keeps cumulative offsets,
    /// the scrollbar range, and stick-to-bottom exact.
    pub fn draw_scrollback(&self, ui: &mut egui::Ui, available_height: f32) -> ScrollbackStats {
        let messages = if let Some(channel_name) = self.current_target_name() {
            self.conversations.get(channel_name).map(|c| &c.messages)
        } else {
            Some(&self.server_messages)
        };
        let Some(msgs) = messages else {
            return ScrollbackStats {
                drawn: 0,
                measured: 0,
                total_rows: 0,
            };
        };
        let unread = self
            .current_target_name()
            .and_then(|target| self.conversations.get(target))
            .map_or(0, |channel| channel.unread);
        self.settle_read_boundary(msgs, unread);
        let unread_from_start = unread > 0
            && unread >= msgs.len()
            && self.timeline.borrow().entry_read_boundary.is_none();
        let my_nick = self.my_nick.clone();
        let stick = self
            .timeline
            .borrow()
            .anchor
            .as_ref()
            .is_none_or(|anchor| anchor.at_bottom);
        let reveal = self.timeline.borrow_mut().reveal.take();
        let restore = self.timeline.borrow().restore_pending;
        if restore {
            self.timeline.borrow_mut().restored_hold = true;
        }
        let explicit_read = self.view_is_active
            && ui.input(|input| {
                input.pointer.any_pressed()
                    || input.events.iter().any(|event| {
                        matches!(
                            event,
                            egui::Event::Key { pressed: true, .. } | egui::Event::Text(_)
                        )
                    })
            });
        if explicit_read {
            self.timeline.borrow_mut().restored_hold = false;
        }
        let scroll_interaction = ui.rect_contains_pointer(ui.max_rect())
            && ui.input(|input| input.smooth_scroll_delta.y != 0.0 || input.pointer.any_pressed());
        if scroll_interaction {
            let mut timeline = self.timeline.borrow_mut();
            timeline.restore_pending = false;
            timeline.restored_hold = false;
            timeline.hydrating = false;
        }
        let restore_anchor = self.timeline.borrow().anchor.clone();
        let mut stats = ScrollbackStats {
            drawn: 0,
            measured: 0,
            total_rows: msgs.len(),
        };

        let mut painted_offset = 0.0;
        let mut first_visible = None;
        let mut last_visible = None;
        let mut reveal_offset = None;
        let mut first_fraction = 0.0;
        let pending_offset = self.timeline.borrow_mut().pending_offset.take();
        let mut scroll_area = ScrollArea::vertical();
        if let Some(offset) = pending_offset {
            scroll_area = scroll_area.vertical_scroll_offset(offset);
        }
        let output = scroll_area
            .id_salt(("chat_scrollback", self.id, self.current_conversation))
            .auto_shrink([false; 2])
            .max_height(available_height)
            .stick_to_bottom(stick)
            .show_viewport(ui, |ui, viewport| {
                painted_offset = viewport.min.y;
                if msgs.is_empty() {
                    return;
                }
                let content_width = ui.available_width();
                let spacing = ui.spacing().item_spacing;
                let layout_key = row_layout_key(ui, content_width);

                // Pass 1 (arithmetic on warm cache): cumulative offsets to
                // find the first row intersecting the viewport top and the
                // total content height. Rows above the viewport are never
                // drawn. Offsets are in content coordinates (0 = top of the
                // scrolled content), matching `viewport`.
                let mut y = 0.0f32;
                let mut first_idx = msgs.len();
                let mut first_y = 0.0f32;
                for (i, msg) in msgs.iter().enumerate() {
                    let height =
                        row_height(ui, msg, &my_nick, content_width, layout_key, &mut stats);
                    if reveal.as_ref().is_some_and(|key| key == &msg.message_key)
                        || (restore
                            && restore_anchor
                                .as_ref()
                                .filter(|anchor| !anchor.at_bottom)
                                .and_then(|anchor| anchor.message.as_ref())
                                .is_some_and(|reference| {
                                    reference.archive_id.as_deref()
                                        == Some(msg.message_key.as_str())
                                        || reference
                                            .server_msgid
                                            .as_ref()
                                            .is_some_and(|id| msg.msgid.as_ref() == Some(id))
                                }))
                    {
                        reveal_offset = Some(
                            y + if restore {
                                height
                                    * restore_anchor
                                        .as_ref()
                                        .map_or(0.0, |a| a.fraction.clamp(0.0, 1.0))
                            } else {
                                0.0
                            },
                        );
                    }
                    let bottom = y + height;
                    if first_idx == msgs.len() && bottom >= viewport.min.y {
                        first_idx = i;
                        first_y = y;
                        first_fraction = ((viewport.min.y - y) / height.max(1.0)).clamp(0.0, 1.0);
                        first_visible = Some(msg);
                    }
                    if bottom <= viewport.max.y {
                        last_visible = Some(msg);
                    }
                    y = bottom + spacing.y;
                }
                ui.set_height((y - spacing.y).max(0.0));
                if first_idx == msgs.len() {
                    // The viewport sits past the end of the content (a
                    // transient state mid-scroll, e.g. right after scrollback
                    // trimming shrank the history). Pin the buffer's tail to
                    // the viewport bottom for this frame rather than flashing
                    // an empty area; egui re-clamps the offset next frame.
                    let mut yy = viewport.max.y;
                    for msg in msgs.iter().rev() {
                        let h =
                            row_height(ui, msg, &my_nick, content_width, layout_key, &mut stats);
                        let top = yy - h;
                        first_idx -= 1;
                        first_y = top;
                        if top <= viewport.min.y {
                            break;
                        }
                        yy = top - spacing.y;
                    }
                }

                // Pass 2: lay out only the visible band, flowing naturally
                // from the first visible row - the same trick egui's own
                // show_rows uses for uniform rows, adapted to variable ones.
                // The row straddling the viewport's bottom edge is included,
                // so partial rows never flicker in from below.
                //
                // `viewport` is content space while UiBuilder rects are in
                // the parent's screen space (shifted up by the scroll
                // offset), so the band origin must be translated by
                // ui.max_rect().top() - exactly what egui's show_rows does.
                let top = ui.max_rect().top();
                let x_range = ui.max_rect().x_range();
                let band_top = top + first_y;
                if band_top > ui.max_rect().bottom() {
                    return; // degenerate mid-scroll frame; nothing to draw
                }
                let band = egui::Rect::from_x_y_ranges(x_range, band_top..=ui.max_rect().bottom());
                let mut drawn = 0usize;
                // Track the band cursor in content space ourselves: the child
                // Ui's cursor is in screen space and must not be compared to
                // the content-space viewport.
                let mut cy = first_y;
                ui.scope_builder(egui::UiBuilder::new().max_rect(band), |rows_ui| {
                    for msg in msgs.iter().skip(first_idx) {
                        if cy > viewport.max.y {
                            break;
                        }
                        // Warm cache at this point: pass 1 measured everything.
                        cy += msg.row_height.get().height + spacing.y;
                        let row = rows_ui.scope_builder(
                            egui::UiBuilder::new()
                                .id_salt((&msg.message_key, "row"))
                                .sense(egui::Sense::click()),
                            |ui| draw_chat_row(ui, msg, &my_nick),
                        );
                        // Under the newest row the marker would introduce nothing.
                        let divider = !msgs.back().is_some_and(|newest| std::ptr::eq(newest, msg))
                            && self
                                .timeline
                                .borrow()
                                .entry_read_boundary
                                .as_ref()
                                .is_some_and(|reference| message_ref_names(reference, msg));
                        if divider || (unread_from_start && msg.message_key == msgs[0].message_key)
                        {
                            let y = if divider {
                                row.response.rect.bottom()
                            } else {
                                row.response.rect.top()
                            };
                            rows_ui.painter().hline(
                                rows_ui.max_rect().x_range(),
                                y,
                                egui::Stroke::new(1.0, Color32::YELLOW),
                            );
                            rows_ui.painter().text(
                                egui::pos2(rows_ui.max_rect().right() - 4.0, y),
                                egui::Align2::RIGHT_BOTTOM,
                                "Unread messages",
                                egui::FontId::proportional(10.0),
                                Color32::YELLOW,
                            );
                        }
                        if let Some(action) = row.inner {
                            self.timeline.borrow_mut().actions.push(action);
                        }
                        let response = row.response;
                        if !msg.is_system {
                            response.context_menu(|ui| {
                                if ui.button("Copy message").clicked() {
                                    ui.ctx().copy_text(msg.content.clone());
                                    ui.close();
                                }
                                if ui
                                    .add_enabled(
                                        msg.msgid.is_some()
                                            && self.enabled_caps.contains("message-tags"),
                                        egui::Button::new("Reply"),
                                    )
                                    .clicked()
                                {
                                    self.timeline.borrow_mut().actions.push(
                                        message_ui::MessageAction::Reply(msg.message_key.clone()),
                                    );
                                    ui.close();
                                }
                                if let Some(id) = &msg.reply_to
                                    && ui.button("Show replied message").clicked()
                                {
                                    self.timeline
                                        .borrow_mut()
                                        .actions
                                        .push(message_ui::MessageAction::Reveal(id.clone()));
                                    ui.close();
                                }
                                ui.add_enabled_ui(
                                    msg.msgid.is_some()
                                        && self.enabled_caps.contains("message-tags"),
                                    |ui| {
                                        ui.horizontal(|ui| {
                                            for emoji in ["👍", "❤️", "😂", "🎉", "👀", "👎"]
                                            {
                                                if ui.button(emoji).clicked() {
                                                    self.timeline.borrow_mut().actions.push(
                                                        message_ui::MessageAction::React(
                                                            msg.message_key.clone(),
                                                            emoji.into(),
                                                        ),
                                                    );
                                                    ui.close();
                                                }
                                            }
                                        });
                                    },
                                );
                            });
                        }
                        drawn += 1;
                    }
                });
                stats.drawn = drawn;
            });
        let at_bottom = output.state.clamped_offset().y + output.inner_rect.height()
            >= output.content_size.y - 2.0;
        let target = self.current_target_name();
        {
            let mut timeline = self.timeline.borrow_mut();
            if let Some(target) = target {
                if !restore || !timeline.restore_pending || scroll_interaction {
                    timeline.anchor = Some(identity::ScrollAnchor {
                        message: first_visible
                            .map(|message| self.live_message_reference(target, message)),
                        fraction: first_fraction,
                        at_bottom,
                    });
                }
                if self.view_is_active
                    && self.window_focused
                    && !restore
                    && reveal.is_none()
                    && reveal_offset.is_none()
                {
                    timeline.visible_read =
                        last_visible.map(|message| self.live_message_reference(target, message));
                }
            }
            if restore
                && !timeline.hydrating
                && (reveal_offset.is_some()
                    || restore_anchor
                        .as_ref()
                        .is_none_or(|anchor| anchor.at_bottom || anchor.message.is_none())
                    || scroll_interaction)
            {
                timeline.restore_pending = false;
            }
        }
        if let Some(offset) = reveal_offset {
            let mut timeline = self.timeline.borrow_mut();
            timeline.pending_offset = Some(offset);
            if let Some(anchor) = &mut timeline.anchor {
                anchor.at_bottom = false;
            }
            ui.ctx().request_discard("reveal saved message position");
        }
        // egui adjusts a sticky/clamped offset after the content is drawn.
        // A history prepend therefore paints one frame at the old offset,
        // then jumps back on the next frame. Redo that layout before painting
        // it on screen, using the corrected state already stored by egui.
        // Only redo content-size changes: ordinary wheel/drag scrolling must
        // not replay input or trigger extra layout passes.
        let size_id = output.id.with("last_content_size");
        let previous_size = ui.ctx().data_mut(|data| {
            let previous = data.get_temp::<egui::Vec2>(size_id);
            data.insert_temp(size_id, output.content_size);
            previous
        });
        if previous_size != Some(output.content_size)
            && (output.state.clamped_offset().y - painted_offset).abs() > 0.5
        {
            ui.ctx()
                .request_discard("history changed the scrollback extent");
        }
        stats
    }

    pub(crate) fn ordered_tabs(&self) -> Vec<Option<ConversationId>> {
        let fingerprint = self.tab_order_fingerprint();
        if let Some((cached, tabs)) = &*self.tab_order_cache.borrow()
            && *cached == fingerprint
        {
            return tabs.clone();
        }
        let tabs: Vec<Option<ConversationId>> = self
            .ordered_tab_names()
            .into_iter()
            .map(|name| name.and_then(|name| self.id_for_target(name)))
            .collect();
        *self.tab_order_cache.borrow_mut() = Some((fingerprint, tabs.clone()));
        tabs
    }

    /// Allocation-free fingerprint of everything the tab order depends
    /// on: the tab names and the casemapping that orders them (including
    /// the channel types its UTF-8 form preserves). Order-independent,
    /// since channel iteration order is not stable.
    #[cfg(test)]
    fn ordered_target_names(&self) -> Vec<Option<String>> {
        self.ordered_tabs()
            .into_iter()
            .map(|id| id.and_then(|id| self.target_name(id).map(str::to_owned)))
            .collect()
    }

    fn tab_order_fingerprint(&self) -> u64 {
        fn fnv(hash: u64, bytes: &[u8]) -> u64 {
            bytes.iter().fold(hash, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        }
        let mut session_hash = fnv(
            0xcbf2_9ce4_8422_2325,
            &self.conversations.revision().to_le_bytes(),
        );
        session_hash = fnv(session_hash, &self.conversations.len().to_le_bytes());
        session_hash = fnv(session_hash, self.network_support.channel_types.as_bytes());
        let mapping = match self.network_support.case_mapping {
            CaseMapping::Ascii => 0u8,
            CaseMapping::StrictRfc1459 => 1,
            CaseMapping::Rfc1459 => 2,
            CaseMapping::Utf8Rfc8265 => 3,
        };
        session_hash = fnv(session_hash, &[mapping]);
        let mut total = session_hash;
        for name in self.conversations.keys() {
            total = total.wrapping_add(fnv(session_hash, name.as_bytes()).rotate_left(17));
        }
        total
    }

    /// The switchbar's tab order, borrowing the channel names: this runs
    /// every frame, so it must neither clone every name nor build a
    /// canonical key per name just to compare them.
    fn ordered_tab_names(&self) -> Vec<Option<&str>> {
        let mut tabs = vec![None];
        let mut names: Vec<&str> = self.conversations.keys().map(String::as_str).collect();
        names.sort_by(|a, b| self.network_support.cmp_canonical(a, b));
        tabs.extend(names.into_iter().map(Some));
        tabs
    }

    fn select_tab(&mut self, conversation: Option<ConversationId>) {
        if !self.select_conversation(conversation) {
            return;
        }
        self.selected_user = None;
        // Completion matches belong to the previous tab's users, and the
        // input text (which normally invalidates them) may be unchanged (#266).
        self.reset_tab_completion();
        if let Some(id) = self.current_conversation {
            if let Some(channel) = self.conversations.channel_mut(id) {
                channel.unread = 0;
            }
        } else {
            self.server_unread = 0;
        }
        if let Some(target) = self.current_target_name().map(str::to_owned) {
            self.mark_target_read(&target);
        }
    }

    #[cfg(test)]
    fn cycle_tab(&mut self, reverse: bool) {
        let tabs = self.ordered_tabs();
        if tabs.is_empty() {
            return;
        }
        let current = tabs
            .iter()
            .position(|tab| *tab == self.current_conversation)
            .unwrap_or(0);
        let next = if reverse {
            current.checked_sub(1).unwrap_or(tabs.len() - 1)
        } else {
            (current + 1) % tabs.len()
        };
        self.select_tab(tabs[next]);
    }

    /// Whether closing this tab has to PART first. Only a joined channel does;
    /// queries and kicked/disconnected channel tabs are closed locally (#256).
    fn tab_close_needs_part(&self, name: &str) -> bool {
        self.is_channel_name(name)
            && self
                .channel_key(name)
                .and_then(|key| self.conversations.get(&key))
                .is_some_and(|channel| channel.joined)
    }

    pub(crate) fn close_current_tab(&mut self) {
        if let Some(channel_name) = self.current_target_name().map(str::to_owned) {
            self.close_tab(&channel_name);
        }
    }

    /// Close a tab by name: PART a joined channel, otherwise drop the tab.
    fn close_tab(&mut self, name: &str) {
        if self.tab_close_needs_part(name)
            && self.send_command(IrcCommand::Part(name.to_string(), None))
        {
            // Keep the tab until the server confirms our PART.
            return;
        }
        let was_current = self.current_target_is(name);
        self.remove_channel(name);
        if was_current {
            self.current_conversation = self.conversations.ids().next();
        }
    }

    /// Handle CTCP request and send reply if applicable
    /// Returns true if the message was a CTCP request that was handled
    fn handle_ctcp_request(&mut self, sender: &str, content: &str) -> bool {
        // The closing \x01 is optional; a lone "\x01" has no command.
        let Some((ctcp_cmd, ctcp_args)) = parse_ctcp(content) else {
            return false;
        };
        let ctcp_cmd = ctcp_cmd.to_uppercase();

        // ACTION is not a request, it's a message type - don't reply
        if ctcp_cmd == "ACTION" {
            return false;
        }

        // Our own request relayed back (echo-message, bouncer): consume it
        // without replying to ourselves or logging it as an incoming request.
        // Current nick only, as for the PRIVMSG self check (#168).
        if self.identifiers_equal(sender, &self.my_nick) {
            return true;
        }

        // Build the reply based on the CTCP command
        let reply = match ctcp_cmd.as_str() {
            "VERSION" => Some(
                concat!(
                    "\x01VERSION Linefeed v",
                    env!("CARGO_PKG_VERSION"),
                    " - Rust/egui cross-platform IRC client\x01"
                )
                .to_string(),
            ),
            "TIME" => {
                // Get current local time
                use std::time::{SystemTime, UNIX_EPOCH};
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                let time_str = format_ctcp_time(secs, local_utc_offset_secs(secs));

                Some(format!("\x01TIME {}\x01", time_str))
            }
            "PING" => {
                // Echo back the ping argument
                Some(format!("\x01PING {}\x01", ctcp_args))
            }
            "CLIENTINFO" => Some(
                "\x01CLIENTINFO ACTION PING VERSION TIME CLIENTINFO SOURCE USERINFO\x01"
                    .to_string(),
            ),
            "SOURCE" => Some(format!("\x01SOURCE {}\x01", PROJECT_SOURCE_URL)),
            "USERINFO" => Some(format!("\x01USERINFO {}\x01", self.realname)),
            _ => None,
        };

        // Send the reply if we have one and CTCP replies are enabled
        if let Some(reply_msg) = reply {
            if self.ctcp_replies_enabled {
                match self.ctcp_reply_limiter.check(std::time::Instant::now()) {
                    CtcpReplyDecision::Reply => {
                        self.send_command(IrcCommand::Notice(sender.to_string(), reply_msg));
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "[CTCP] {} from {} - replied",
                            ctcp_cmd, sender
                        )));
                    }
                    CtcpReplyDecision::Suppress { first: true } => {
                        self.add_message_to_current(ChatMessage::system(&format!(
                            "[CTCP] {} from {} - not replied (rate limited); further requests are ignored silently",
                            ctcp_cmd, sender
                        )));
                    }
                    CtcpReplyDecision::Suppress { first: false } => {}
                }
            } else {
                self.add_message_to_current(ChatMessage::system(&format!(
                    "[CTCP] {} from {} - ignored (replies disabled)",
                    ctcp_cmd, sender
                )));
            }
            true
        } else {
            // Unknown CTCP, log it but don't reply
            self.add_message_to_current(ChatMessage::system(&format!(
                "[CTCP] Unknown {} from {}",
                ctcp_cmd, sender
            )));
            true
        }
    }

    /// Check if a message contains a mention of our nick or highlight words
    fn check_nick_mention(&self, content: &str) -> bool {
        // IRC nicknames and configured highlight words admit punctuation that
        // generic "word" tokenizers split (`foo-bar`, `[alice]`, `node.js`,
        // ...). Search for each complete casemapped needle and validate
        // IRC-identifier boundaries instead of token equality.
        let content_key = self.network_support.canonicalize(content);

        if !self.my_nick_lower.is_empty() && ident_contains(&content_key, &self.my_nick_lower) {
            return true;
        }

        // Highlight words are cached casemapped (see update_cached_lowercase)
        self.highlight_words_lower
            .iter()
            .any(|highlight| ident_contains(&content_key, highlight))
    }
}

#[cfg(test)]
mod identity_tests;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn temporary_log_dir(test_name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "linefeed-gui-{test_name}-{}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_app() -> Session {
        Session::with_settings(Settings {
            // Keep unit tests from writing chat logs to the real config dir.
            logging_enabled: false,
            ..Settings::default()
        })
    }

    /// A connected app whose outgoing commands can be inspected.
    fn test_app_connected() -> (Session, mpsc::UnboundedReceiver<IrcCommand>) {
        let mut app = test_app();
        let (tx, rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        app.test_set_connected(true);
        (app, rx)
    }

    fn commit_test_endpoint(app: &mut Session) {
        let form_nick = std::mem::replace(
            &mut app.nickname,
            if app.my_nick.is_empty() {
                "test".into()
            } else {
                app.my_nick.clone()
            },
        );
        app.active_session = Some(
            app.session_from_form()
                .expect("typed send fixture endpoint"),
        );
        app.nickname = form_nick;
    }

    fn test_registration_transport(app: &mut Session) -> mpsc::UnboundedReceiver<IrcCommand> {
        let (tx, rx) = mpsc::unbounded_channel();
        app.test_set_connecting(tx, None);
        rx
    }

    /// Run `f` under a TRACE-level subscriber and return everything it logged.
    pub(crate) fn capture_logs(f: impl FnOnce()) -> String {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct SharedBuffer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buffer = SharedBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn rejected_commands_never_log_their_contents() {
        // Too long: a credential command must be summarized, not dumped (#227).
        let (mut app, _rx) = test_app_connected();
        let secret = "hunter2-".repeat(100);
        let logs = capture_logs(|| {
            assert!(!app.send_command(IrcCommand::Privmsg(
                "NickServ".into(),
                format!("IDENTIFY {secret}")
            )));
        });
        assert!(!logs.contains("hunter2"), "{logs}");
        assert!(!logs.contains("IDENTIFY"), "{logs}");
        assert!(logs.contains("PRIVMSG"), "verb is still logged: {logs}");

        // Disconnected channel: the SendError wraps the whole command.
        let (mut app, rx) = test_app_connected();
        drop(rx);
        let logs = capture_logs(|| {
            assert!(!app.send_command(IrcCommand::Pass("hunter2".into())));
        });
        assert!(!logs.contains("hunter2"), "{logs}");
    }

    #[test]
    fn notifications_never_log_message_text() {
        // Skipped notifications (disabled or focused) must not leak the body
        // to the default-level log either (#228).
        let mut app = test_app();
        let logs = capture_logs(|| {
            app.send_notification("PM from alice", "meet me at the secret place", true);
        });
        assert!(!logs.contains("secret place"), "{logs}");

        app.window_focused = true;
        let logs = capture_logs(|| {
            app.send_notification(
                "bob mentioned you in #room",
                "the password is swordfish",
                false,
            );
        });
        assert!(!logs.contains("swordfish"), "{logs}");
    }

    #[test]
    fn notify_send_treats_dash_prefixed_text_as_operands() {
        let args = notify_send_args("--urgency=critical", "--hint=string:x:y");
        let separator = args.iter().position(|arg| *arg == "--").unwrap();
        assert_eq!(
            args[separator + 1..],
            ["--urgency=critical", "--hint=string:x:y"]
        );
        assert!(args[..separator].iter().all(|arg| !arg.contains("hint")));
    }

    #[test]
    fn notify_send_text_has_markup_escaped() {
        // Many notification daemons render <a href> / <img> in the body.
        assert_eq!(
            notification_markup_escape(r#"<a href="http://evil.test">x</a> & <img src=y>"#),
            r#"&lt;a href="http://evil.test"&gt;x&lt;/a&gt; &amp; &lt;img src=y&gt;"#
        );
        // The ampersand is escaped first, so an entity is not double-decoded.
        assert_eq!(notification_markup_escape("&lt;"), "&amp;lt;");
        assert_eq!(notification_markup_escape("plain text"), "plain text");

        let args = notify_send_args("<b>nick</b>", "a < b && c > d");
        let separator = args.iter().position(|arg| *arg == "--").unwrap();
        assert_eq!(
            args[separator + 1..],
            ["&lt;b&gt;nick&lt;/b&gt;", "a &lt; b &amp;&amp; c &gt; d"]
        );
    }

    #[test]
    fn applescript_escape_handles_backslashes_before_quotes() {
        assert_eq!(applescript_escape(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(applescript_escape(r"evil\"), r"evil\\");
        // A pre-escaped quote must not terminate the literal: the backslash is
        // doubled first, then the quote gets its own escape.
        assert_eq!(applescript_escape(r#"a\"b"#), r#"a\\\"b"#);

        // Every quote in the escaped text is preceded by an odd number of
        // backslashes, so none ends the literal early.
        for hostile in [
            r#"\"#,
            r#"\""#,
            r#"\\""#,
            r#"x\" & (do shell script "id") & ""#,
        ] {
            let escaped = applescript_escape(hostile);
            let chars: Vec<char> = escaped.chars().collect();
            let mut index = 0;
            while index < chars.len() {
                match chars[index] {
                    '\\' => index += 2,
                    '"' => panic!("unescaped quote in {escaped:?}"),
                    _ => index += 1,
                }
            }
            assert_eq!(
                index,
                chars.len(),
                "trailing backslash escapes the closing quote"
            );
        }

        assert_eq!(
            applescript_notification(r"PM from evil\", r#"say "x""#),
            r#"display notification "say \"x\"" with title "PM from evil\\""#
        );
    }

    #[cfg(unix)]
    #[test]
    fn notification_helper_waits_for_exit_status() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "exit 7"]);
        let status = wait_for_notification_helper(&mut command).unwrap();
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn minimal_update_drains_and_processes_background_messages() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel(2);
        app.test_set_incoming(rx);
        tx.try_send(IrcMessage::parse(":server NOTICE * :background").unwrap())
            .unwrap();

        app.update_minimal();

        assert!(
            app.server_messages
                .iter()
                .any(|message| { message.content.contains("background") })
        );
    }

    #[test]
    fn self_kick_marks_channel_unjoined_and_blocks_send_and_echo() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        let mut channel = Channel::new();
        channel.joined = true;
        channel.add_user("me", UserMode::Normal);
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        app.handle_incoming_message(IrcMessage::parse(":oper!u@h KICK #chan me :bye").unwrap());
        assert!(!app.conversations["#chan"].joined);

        app.input_text = "hello".to_string();
        app.process_input();
        app.process_command("/amsg broadcast");
        assert!(rx.try_recv().is_err());
        assert!(
            !app.conversations["#chan"]
                .messages
                .iter()
                .any(|message| message.sender == "me" && message.content == "hello")
        );
        assert!(
            app.conversations["#chan"]
                .messages
                .iter()
                .any(|message| { message.content.contains("not joined") })
        );
    }

    #[test]
    fn tab_completion_cycles_nicks_and_completes_commands() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("Alina", UserMode::Normal);
        channel.add_user("Alice", UserMode::Normal);
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        app.input_text = "Al".to_string();
        app.handle_tab_completion();
        assert_eq!(app.input_text, "Alice: ");
        app.handle_tab_completion();
        assert_eq!(app.input_text, "Alina: ");
        app.handle_tab_completion();
        assert_eq!(app.input_text, "Alice: ");

        app.reset_tab_completion();
        app.input_text = "/jo".to_string();
        app.handle_tab_completion();
        assert_eq!(app.input_text, "/join ");
    }

    #[test]
    fn switching_tabs_drops_in_progress_tab_completion() {
        let mut app = test_app();
        let mut first = Channel::new();
        first.add_user("Alina", UserMode::Normal);
        first.add_user("Alice", UserMode::Normal);
        app.conversations.insert("#one".to_string(), first);
        app.conversations.insert("#two".to_string(), Channel::new());
        app.select_target("#one");

        app.input_text = "Al".to_string();
        app.handle_tab_completion();
        assert_eq!(app.input_text, "Alice: ");

        // The input is untouched by the switch, so only the tab change can
        // invalidate the first channel's matches (#266).
        app.select_tab(app.id_for_target("#two"));
        assert!(app.tab_completion.is_none());
        app.handle_tab_completion();
        assert_eq!(app.input_text, "Alice: ", "no stale nick cycling");

        // Selecting the server buffer is a tab change too.
        app.select_tab(app.id_for_target("#one"));
        app.input_text = "Al".to_string();
        app.handle_tab_completion();
        assert!(app.tab_completion.is_some());
        app.select_tab(None);
        assert!(app.tab_completion.is_none());
    }

    #[test]
    fn tab_completion_survives_multi_byte_whitespace() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("Alice", UserMode::Normal);
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        // A multi-byte whitespace char before the word must not panic on
        // byte-offset slicing (rfind returns the char's start offset).
        for ws in ['\u{a0}', '\u{2003}', '\u{3000}'] {
            app.reset_tab_completion();
            app.input_text = format!("hi{ws}Al");
            app.handle_tab_completion();
            let expected_base = format!("hi{ws}");
            assert_eq!(app.input_text, format!("{expected_base}Alice "));
        }
    }

    #[test]
    fn history_pages_keep_visible_tail_stationary() {
        let mut app = test_app();
        let mut channel = Channel::new();
        for i in 0..100 {
            channel.messages.push_back(ChatMessage::new_fmt(
                "alice",
                &format!("history {i}: {}", "wrapped words ".repeat(12)),
                "short",
            ));
        }
        app.conversations.insert("#history".into(), channel);
        app.select_target("#history");
        let ctx = egui::Context::default();
        let mut positions = Vec::new();
        for frame in 0..30 {
            if frame >= 20 && frame % 2 == 0 {
                app.conversations
                    .get_mut("#history")
                    .unwrap()
                    .messages
                    .push_front(ChatMessage::new_fmt(
                        "bob",
                        "another page of older history",
                        "short",
                    ));
            }
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(600.0, 400.0),
                    )),
                    time: Some(frame as f64 / 60.0),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        app.draw_scrollback(ui, 350.0);
                    });
                },
            );
            let visible: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|clipped| {
                    if let egui::Shape::Text(text) = &clipped.shape {
                        let bounds = egui::Rect::from_min_size(text.pos, text.galley.rect.size());
                        if clipped.clip_rect.intersects(bounds) {
                            return Some((text.galley.text().to_string(), text.pos.y));
                        }
                    }
                    None
                })
                .collect();
            output.textures_delta.clear();
            if frame >= 20 {
                assert!(!visible.is_empty(), "history must remain visible");
                positions.push(visible);
            }
        }
        for frames in positions.windows(2) {
            assert_eq!(frames[0], frames[1], "idle history viewport keeps moving");
        }
    }

    #[test]
    fn cached_tab_order_tracks_tabs_and_casemapping() {
        let mut app = test_app();
        for name in ["#delta", "#Alpha", "#charlie"] {
            app.conversations.insert(name.to_string(), Channel::new());
        }
        let expected: Vec<Option<String>> = vec![
            None,
            Some("#Alpha".into()),
            Some("#charlie".into()),
            Some("#delta".into()),
        ];
        assert_eq!(app.ordered_target_names(), expected);
        // Second call is the memoized path and must agree.
        assert_eq!(app.ordered_target_names(), expected);

        app.conversations
            .insert("#bravo".to_string(), Channel::new());
        app.conversations.remove("#charlie");
        assert_eq!(
            app.ordered_target_names(),
            vec![
                None,
                Some("#Alpha".into()),
                Some("#bravo".into()),
                Some("#delta".into())
            ]
        );

        // Casemapping changes the order: under RFC1459 "#Z" folds to "#z"
        // and sorts before "#[a" (folded "#{a"); ASCII keeps '[' first.
        app.conversations.clear();
        app.conversations.insert("#[a".to_string(), Channel::new());
        app.conversations.insert("#Z".to_string(), Channel::new());
        assert_eq!(
            app.ordered_target_names(),
            vec![None, Some("#Z".into()), Some("#[a".into())]
        );
        app.network_support.case_mapping = CaseMapping::Ascii;
        assert_eq!(
            app.ordered_target_names(),
            vec![None, Some("#[a".into()), Some("#Z".into())]
        );
    }

    #[test]
    fn scrollback_virtualizes_large_history() {
        let mut app = test_app();
        let mut channel = Channel::new();
        for i in 0..2000 {
            channel.messages.push_back(ChatMessage::system(&format!(
                "message {i}: some channel chatter"
            )));
        }
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        // Headless egui pass: no renderer needed, real layout runs.
        let ctx = egui::Context::default();
        let mut first_stats = None;
        let mut first_frame_measured = 0;
        ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let stats = app.draw_scrollback(ui, 400.0);
                first_frame_measured += stats.measured;
                first_stats = Some(stats);
            });
        })
        .textures_delta
        .clear();
        let stats = first_stats.expect("panel ran");
        assert_eq!(stats.total_rows, 2000);
        assert!(
            stats.drawn < 300,
            "virtualization failed: drew {} of 2000 rows",
            stats.drawn
        );
        // Cold cache: the first pass measures every height once so it can
        // compute cumulative offsets - that is the one-time cost, paid
        // instead of re-laying out every widget on every future frame.
        // Include the corrective layout pass used to settle the initial
        // bottom offset before painting; it must reuse those measurements.
        assert_eq!(first_frame_measured, 2000);

        // Second identical frame: everything cached, nothing re-measured,
        // and still only the visible band drawn.
        let mut second_stats = None;
        ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                second_stats = Some(app.draw_scrollback(ui, 400.0));
            });
        })
        .textures_delta
        .clear();
        let stats2 = second_stats.expect("second panel ran");
        assert_eq!(stats2.measured, 0, "heights must stay cached across frames");
        assert!(
            stats2.drawn < 300,
            "virtualization failed on warm cache: drew {} of 2000",
            stats2.drawn
        );

        // Scrolling up into older history keeps the work bounded.
        let scrolled_input = egui::RawInput {
            events: vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                // Positive Y moves content down, revealing older rows above.
                delta: egui::vec2(0.0, 80.0),
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Wheel,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        };
        let mut scrolled_stats = None;
        ctx.run_ui(scrolled_input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                scrolled_stats = Some(app.draw_scrollback(ui, 400.0));
            });
        })
        .textures_delta
        .clear();
        let stats3 = scrolled_stats.expect("scrolled panel ran");
        assert!(
            stats3.drawn < 300,
            "post-scroll draw count exploded: {}",
            stats3.drawn
        );
    }

    #[test]
    fn row_measurement_tracks_wrapping_heights() {
        let ctx = egui::Context::default();
        let short_msg = ChatMessage::system("hi");
        let long_msg = ChatMessage::system(
            "a very long system message that will certainly need to wrap across \
             several lines when the available width is small enough for wrapping",
        );
        let mut heights = None;
        ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let width = 120.0;
                let key = row_layout_key(ui, width);
                let mut stats = ScrollbackStats {
                    drawn: 0,
                    measured: 0,
                    total_rows: 2,
                };
                // Narrow width forces the long message to wrap over lines;
                // the measured advance must reflect that.
                let h_short = row_height(ui, &short_msg, "me", width, key, &mut stats);
                let h_long = row_height(ui, &long_msg, "me", width, key, &mut stats);
                heights = Some((h_short, h_long));
            });
        })
        .textures_delta
        .clear();
        let (h_short, h_long) = heights.expect("panel ran");
        assert!(h_long > h_short * 1.5, "{h_long} vs {h_short}");
    }

    #[test]
    fn virtualized_rows_paint_inside_the_scroll_clip() {
        // Count-based assertions cannot see coordinate bugs: rows could be
        // laid out invisibly outside the clip rect. Inspect the painted
        // shapes instead - message text must be visible on EVERY frame,
        // including the frame right after a new message arrives (whose row
        // needs measuring). A frame with no visible text is what users see
        // as the whole message area flashing blank.
        let mut app = test_app();
        let mut channel = Channel::new();
        for i in 0..500 {
            channel
                .messages
                .push_back(ChatMessage::system(&format!("row {i} of history")));
        }
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        let ctx = egui::Context::default();
        for frame in 0..6 {
            if frame > 0 {
                // Simulate a newly arrived message: its row has no cached
                // height yet, forcing a measurement this frame.
                let chan = app.conversations.get_mut("#chan").unwrap();
                chan.messages
                    .push_back(ChatMessage::system(&format!("fresh {frame}")));
            }
            let mut output = ctx.run_ui(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    app.draw_scrollback(ui, ui.max_rect().height() - 30.0);
                });
            });
            let mut visible_text = 0usize;
            for clipped in &output.shapes {
                if let egui::Shape::Text(text) = &clipped.shape {
                    let bounds = egui::Rect::from_min_size(text.pos, text.galley.rect.size());
                    if clipped.clip_rect.intersects(bounds) && !text.galley.is_empty() {
                        visible_text += 1;
                    }
                }
            }
            assert!(
                visible_text >= 5,
                "frame {frame}: only {visible_text} visible text shapes - \
                 the message area would flash blank"
            );
            // Headless: no renderer consumes the font atlas uploads, so drop
            // them explicitly (egui debug-asserts on unapplied deltas).
            output.textures_delta.clear();
        }
    }

    #[test]
    fn ctrl_tab_navigation_wraps_in_both_directions() {
        let mut app = test_app();
        app.conversations.insert("#b".to_string(), Channel::new());
        let mut a = Channel::new();
        a.unread = 2;
        app.conversations.insert("#a".to_string(), a);

        app.cycle_tab(false);
        assert_eq!(app.current_target_name(), Some("#a"));
        assert_eq!(app.conversations["#a"].unread, 0);
        app.cycle_tab(false);
        assert_eq!(app.current_target_name(), Some("#b"));
        app.cycle_tab(false);
        assert!(app.current_conversation.is_none());
        app.cycle_tab(true);
        assert_eq!(app.current_target_name(), Some("#b"));
    }

    fn drain_commands(rx: &mut mpsc::UnboundedReceiver<IrcCommand>) -> Vec<IrcCommand> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn comma_list_join_stores_each_channels_own_key() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.process_command("/join #a,#b keya,keyb");
        assert_eq!(
            drain_commands(&mut rx),
            vec![IrcCommand::Join(
                "#a,#b".into(),
                Some("keya,keyb".into()),
                None,
                None
            )]
        );
        // The server confirms each channel with its own JOIN echo.
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #a").unwrap());
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #b").unwrap());
        assert_eq!(app.conversations["#a"].key.as_deref(), Some("keya"));
        assert_eq!(app.conversations["#b"].key.as_deref(), Some("keyb"));
        assert!(
            app.pending_channel_keys.is_empty(),
            "no stale pending key for the list as a whole is left behind"
        );
    }

    #[test]
    fn cycle_repeatedly_keeps_the_channel_key() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.process_command("/join #sec hunter2");
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #sec").unwrap());
        assert_eq!(app.conversations["#sec"].key.as_deref(), Some("hunter2"));
        drain_commands(&mut rx);

        // The retained conversation resets its protocol data on PART; the
        // pending JOIN key must survive each round (#250).
        let original = app.id_for_target("#sec").unwrap();
        for round in 0..2 {
            app.select_target("#sec");
            app.process_command("/cycle");
            assert_eq!(
                drain_commands(&mut rx),
                vec![
                    IrcCommand::Part("#sec".into(), Some("Cycling".into())),
                    IrcCommand::Join("#sec".into(), Some("hunter2".into()), None, None),
                ],
                "round {round}"
            );
            app.handle_incoming_message(IrcMessage::parse(":me!u@h PART #sec :Cycling").unwrap());
            assert_eq!(app.id_for_target("#sec"), Some(original));
            assert!(!app.conversations["#sec"].joined);
            app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #sec").unwrap());
            assert_eq!(
                app.conversations["#sec"].key.as_deref(),
                Some("hunter2"),
                "round {round}"
            );
        }
        assert!(app.pending_channel_keys.is_empty());
    }

    #[test]
    fn a_refused_join_does_not_leave_its_key_for_a_later_keyless_join() {
        for refusal in [
            ERR_NOSUCHCHANNEL,
            ERR_TOOMANYCHANNELS,
            ERR_CHANNELISFULL,
            ERR_INVITEONLYCHAN,
            ERR_BANNEDFROMCHAN,
            ERR_BADCHANNELKEY,
            ERR_BADCHANMASK,
            ERR_NEEDREGGEDNICK,
        ] {
            let (mut app, mut rx) = test_app_connected();
            app.set_session_nick("me".to_string(), true);
            app.process_command("/join #sec hunter2");
            assert_eq!(drain_commands(&mut rx).len(), 1);
            assert!(!app.pending_channel_keys.is_empty());

            app.handle_numeric(refusal, &["me".into(), "#sec".into(), "Cannot join".into()]);
            assert!(
                app.pending_channel_keys.is_empty(),
                "{refusal}: the refused join's key was kept"
            );

            // The channel is later joined without a key (an invite, a
            // bouncer, a keyless auto-join): it must not inherit the old one.
            app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #sec").unwrap());
            assert_eq!(app.conversations["#sec"].key, None, "{refusal}");
        }
    }

    #[test]
    fn every_join_refusal_leaves_no_ghost_join_state() {
        for refusal in [
            ERR_UNAVAILRESOURCE,
            ERR_BADCHANNAME,
            ERR_SECUREONLYCHAN,
            ERR_OPERONLY,
            ERR_NOSUCHCHANNEL,
        ] {
            let (mut app, mut rx) = test_app_connected();
            app.set_session_nick("me".to_string(), true);
            app.process_command("/join #sec hunter2");
            assert_eq!(drain_commands(&mut rx).len(), 1);
            assert!(!app.join_focus_requests.is_empty());

            app.handle_numeric(refusal, &["me".into(), "#sec".into(), "Cannot join".into()]);

            assert!(app.pending_channel_keys.is_empty(), "{refusal}: key kept");
            assert!(app.join_focus_requests.is_empty(), "{refusal}: focus kept");
            assert!(
                app.pending_rejoins
                    .as_ref()
                    .is_none_or(|rows| rows.is_empty()),
                "{refusal}: rejoin membership kept"
            );
            assert!(app.update_session().tabs.is_empty(), "{refusal}: snapshot");
            app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #sec").unwrap());
            assert_eq!(app.conversations["#sec"].key, None, "{refusal}");
        }
    }

    #[test]
    fn a_nick_unavailable_437_does_not_touch_pending_joins() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.process_command("/join #sec hunter2");
        assert_eq!(drain_commands(&mut rx).len(), 1);

        app.handle_numeric(
            ERR_UNAVAILRESOURCE,
            &[
                "*".into(),
                "me".into(),
                "Nick/channel is temporarily unavailable".into(),
            ],
        );

        assert!(!app.pending_channel_keys.is_empty());
        assert!(!app.join_focus_requests.is_empty());
        assert_eq!(
            app.pending_rejoins.as_ref().map(Vec::len),
            Some(1),
            "the pending JOIN is still outstanding"
        );
    }

    #[test]
    fn a_forwarded_join_does_not_leave_the_original_channels_key() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.process_command("/join #old hunter2");
        assert_eq!(drain_commands(&mut rx).len(), 1);

        app.handle_numeric(
            ERR_LINKCHANNEL,
            &[
                "me".into(),
                "#old".into(),
                "#new".into(),
                "Forwarding".into(),
            ],
        );

        assert!(app.pending_channel_keys.is_empty(), "#old key kept");
        assert_eq!(
            app.pending_rejoins.as_ref().unwrap().as_slice(),
            [("#new".to_string(), None)]
        );
        assert_eq!(app.join_focus_requests.len(), 1);
        assert!(app.identifiers_equal(&app.join_focus_requests[0], "#new"));
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #old").unwrap());
        assert_eq!(app.conversations["#old"].key, None);
    }

    #[test]
    fn a_new_connection_does_not_inherit_keys_of_joins_the_old_one_never_finished() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.process_command("/join #sec hunter2");
        assert_eq!(drain_commands(&mut rx).len(), 1);
        assert!(!app.pending_channel_keys.is_empty());

        // The connection drops before the server confirms the JOIN.
        app.begin_connection_attempt();

        assert!(app.pending_channel_keys.is_empty());
    }

    #[test]
    fn join_with_key_on_retained_tab_updates_the_stored_key() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        let mut kicked = Channel::new();
        kicked.key = Some("old-key".into());
        app.conversations.insert("#sec".to_string(), kicked);

        app.process_command("/join #sec new-key");
        assert_eq!(
            drain_commands(&mut rx),
            vec![IrcCommand::Join(
                "#sec".into(),
                Some("new-key".into()),
                None,
                None
            )]
        );
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #sec").unwrap());

        assert!(app.conversations["#sec"].joined);
        assert_eq!(app.conversations["#sec"].key.as_deref(), Some("new-key"));
        assert!(
            app.pending_channel_keys.is_empty(),
            "no stale pending key is left behind"
        );
        assert_eq!(
            app.update_session().tabs,
            vec![("#sec".to_string(), Some("new-key".to_string()), true)]
        );
    }

    #[test]
    fn keyed_auto_join_entries_send_a_key_and_store_it() {
        let (mut app, mut rx) = test_app_connected();
        app.auto_join_channels = "#plain, #sec hunter2".into();
        app.handle_numeric(RPL_WELCOME, &["me".into(), "welcome".into()]);

        let joins: Vec<IrcCommand> = drain_commands(&mut rx)
            .into_iter()
            .filter(|cmd| matches!(cmd, IrcCommand::Join(..)))
            .collect();
        assert_eq!(
            joins,
            vec![
                IrcCommand::Join("#plain".into(), None, None, None),
                IrcCommand::Join("#sec".into(), Some("hunter2".into()), None, None),
            ]
        );
        assert!(
            !app.server_messages
                .iter()
                .any(|message| message.content.contains("hunter2")),
            "the key must not be echoed into the server window"
        );

        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #sec").unwrap());
        assert_eq!(app.conversations["#sec"].key.as_deref(), Some("hunter2"));
        // #plain is still awaiting its JOIN echo, and must survive an
        // update/reconnect during the initial auto-join burst as well.
        let mut tabs = app.update_session().tabs;
        tabs.sort();
        assert_eq!(
            tabs,
            vec![
                ("#plain".to_string(), None, true),
                ("#sec".to_string(), Some("hunter2".to_string()), true),
            ]
        );
    }

    #[test]
    fn live_key_changes_update_the_stored_channel_key_used_for_rejoin() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.process_command("/join #sec oldkey");
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #sec").unwrap());
        assert_eq!(app.conversations["#sec"].key.as_deref(), Some("oldkey"));

        app.handle_incoming_message(IrcMessage::parse(":op!u@h MODE #sec +k newkey").unwrap());
        assert_eq!(app.conversations["#sec"].key.as_deref(), Some("newkey"));
        drain_commands(&mut rx);
        app.select_target("#sec");
        app.process_command("/cycle");
        assert_eq!(
            drain_commands(&mut rx),
            vec![
                IrcCommand::Part("#sec".into(), Some("Cycling".into())),
                IrcCommand::Join("#sec".into(), Some("newkey".into()), None, None),
            ]
        );
        assert_eq!(
            app.update_session().tabs,
            vec![("#sec".to_string(), Some("newkey".to_string()), true)]
        );

        // Removal names the old key on most servers; the stored key goes too.
        app.handle_incoming_message(IrcMessage::parse(":op!u@h MODE #sec -k newkey").unwrap());
        assert_eq!(app.conversations["#sec"].key, None);
        app.handle_incoming_message(IrcMessage::parse(":op!u@h MODE #sec +k again").unwrap());
        app.handle_incoming_message(IrcMessage::parse(":op!u@h MODE #sec -k *").unwrap());
        assert_eq!(app.conversations["#sec"].key, None);
    }

    #[test]
    fn the_channel_modes_reply_updates_the_stored_channel_key() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.process_command("/join #sec oldkey");
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #sec").unwrap());

        app.handle_incoming_message(IrcMessage::parse(":srv 324 me #sec +ntk rotated").unwrap());
        assert_eq!(app.conversations["#sec"].key.as_deref(), Some("rotated"));

        // Servers that hide the key from the reply show a placeholder; it is
        // not a key and must not replace the real one.
        for placeholder in ["*", "<key>"] {
            app.handle_incoming_message(
                IrcMessage::parse(&format!(":srv 324 me #sec +ntk {placeholder}")).unwrap(),
            );
            assert_eq!(
                app.conversations["#sec"].key.as_deref(),
                Some("rotated"),
                "{placeholder}"
            );
        }
    }

    #[test]
    fn a_channel_modes_reply_without_a_key_clears_the_stale_stored_key() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.process_command("/join #sec oldkey");
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #sec").unwrap());
        assert_eq!(app.conversations["#sec"].key.as_deref(), Some("oldkey"));

        // The key was removed while we were away; we come back without one.
        app.handle_incoming_message(IrcMessage::parse(":op!u@h KICK #sec me :bye").unwrap());
        assert!(!app.conversations["#sec"].joined);
        app.process_command("/join #sec");
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #sec").unwrap());
        assert!(app.conversations["#sec"].joined);
        assert_eq!(app.conversations["#sec"].key.as_deref(), Some("oldkey"));

        app.handle_incoming_message(IrcMessage::parse(":srv 324 me #sec +nt").unwrap());
        assert_eq!(app.conversations["#sec"].key, None);
        drain_commands(&mut rx);
        app.select_target("#sec");
        app.process_command("/cycle");
        assert_eq!(
            drain_commands(&mut rx),
            vec![
                IrcCommand::Part("#sec".into(), Some("Cycling".into())),
                IrcCommand::Join("#sec".into(), None, None, None),
            ]
        );
        assert_eq!(
            app.update_session().tabs,
            vec![("#sec".to_string(), None, true)]
        );
    }

    #[test]
    fn close_current_tab_parts_live_channels_and_removes_stale_ones() {
        let (mut connected, mut rx) = test_app_connected();
        let mut live = Channel::new();
        live.joined = true;
        connected.conversations.insert("#live".to_string(), live);
        connected.select_target("#live");
        connected.close_current_tab();
        assert!(connected.conversations.contains_key("#live"));
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Part(channel, None)) if channel == "#live"));

        let mut disconnected = test_app();
        let mut stale = Channel::new();
        stale.joined = true;
        disconnected
            .conversations
            .insert("#stale".to_string(), stale);
        disconnected.select_target("#stale");
        disconnected.close_current_tab();
        assert!(!disconnected.conversations.contains_key("#stale"));
    }

    #[test]
    fn context_menu_close_removes_unjoined_tabs_without_sending_part() {
        let (mut app, mut rx) = test_app_connected();
        let mut live = Channel::new();
        live.joined = true;
        app.conversations.insert("#live".to_string(), live);
        app.conversations
            .insert("#kicked".to_string(), Channel::new());
        app.conversations
            .insert("alice".to_string(), Channel::new());
        app.select_target("#live");

        // The tab menu offers Close (not Part Channel) exactly when no PART
        // is needed: kicked/disconnected channels and queries (#256).
        assert!(app.tab_close_needs_part("#live"));
        assert!(!app.tab_close_needs_part("#kicked"));
        assert!(!app.tab_close_needs_part("alice"));

        app.close_tab("#kicked");
        app.close_tab("alice");
        assert!(rx.try_recv().is_err(), "unjoined tabs close locally");
        assert!(!app.conversations.contains_key("#kicked"));
        assert!(!app.conversations.contains_key("alice"));
        assert_eq!(app.current_target_name(), Some("#live"));

        // A live channel is parted and kept until the server confirms.
        app.close_tab("#live");
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Part(channel, None)) if channel == "#live"));
        assert!(app.conversations.contains_key("#live"));

        // Closing the selected stale tab moves the selection off it.
        app.conversations
            .insert("#kicked".to_string(), Channel::new());
        app.select_target("#kicked");
        app.close_tab("#kicked");
        assert!(!app.conversations.contains_key("#kicked"));
        assert_eq!(app.current_target_name(), Some("#live"));
    }

    #[test]
    fn user_list_query_opening_uses_normal_initialization_and_casemapped_reuse() {
        let mut app = test_app();
        app.logging_load_history = false;
        assert!(app.open_query("Alice"));
        assert_eq!(app.current_target_name(), Some("Alice"));
        assert!(
            app.conversations["Alice"]
                .messages
                .iter()
                .any(|message| message.content == "Conversation with Alice")
        );

        assert!(app.open_query("alice"));
        assert_eq!(app.conversations.len(), 1);
        assert_eq!(app.current_target_name(), Some("Alice"));
    }

    #[test]
    fn active_session_is_immutable_while_connection_form_is_edited() {
        let mut app = test_app();
        app.server_host = "old.example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        app.password = "old-server-pass".into();
        app.sasl_username = "old-account".into();
        app.sasl_password = "old-sasl-pass".into();
        app.accept_invalid_certs = true;
        app.auto_join_channels = "#old".into();
        app.auto_perform = "/ns identify old-nickserv-pass".into();
        app.set_invisible = true;
        assert!(app.start_session_from_form());

        // Manual/unprofiled endpoint edits clear the form's scoped secrets but
        // cannot mutate the active socket's reconnect/automation snapshot.
        app.apply_unprofiled_endpoint("new.example", "7000", false);
        app.auto_join_channels = "#new".into();
        app.auto_perform = "/ns identify new-nickserv-pass".into();
        assert!(app.password.is_empty());
        assert!(app.sasl_password.is_empty());
        assert_eq!(app.active_endpoint_label(), "old.example:6697");
        assert_eq!(
            app.active_log_context().map(|context| context.0),
            Some("tls://old.example:6697".into())
        );
        assert_eq!(
            app.active_endpoint_key(),
            Some(EndpointKey {
                host: "old.example".into(),
                port: 6697,
                use_tls: true,
            })
        );
        let active = app.active_server_config().unwrap();
        assert_eq!(active.password.as_deref(), Some("old-server-pass"));
        assert_eq!(active.sasl_password.as_deref(), Some("old-sasl-pass"));

        let (tx, mut rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        app.handle_numeric(RPL_WELCOME, &["me".into(), "welcome".into()]);
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Mode(_, Some(mode), _)) if mode == "+i"));
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Ping(token)) if token == "LAG"));
        assert!(
            rx.try_recv().is_err(),
            "automatic joins wait for service authentication"
        );
        assert_eq!(
            app.pending_auto_perform.as_deref(),
            Some(["/ns identify old-nickserv-pass".to_string()].as_slice())
        );
        app.run_timed_tasks();
        assert_eq!(
            drain_commands(&mut rx),
            vec![IrcCommand::Privmsg(
                "NickServ".into(),
                "identify old-nickserv-pass".into(),
            )]
        );
        app.handle_incoming_message(
            IrcMessage::parse(":srv 900 me me!u@h old-account :Logged in").unwrap(),
        );
        app.run_timed_tasks();
        assert_eq!(
            drain_commands(&mut rx),
            vec![IrcCommand::Join("#old".into(), None, None, None)]
        );
    }

    #[test]
    fn explicit_endpoint_switch_promotes_fresh_config_after_old_socket_closes() {
        let mut app = test_app();
        app.server_host = "old.example".into();
        app.server_port = "6697".into();
        app.password = "old-server-pass".into();
        app.sasl_username = "old-account".into();
        app.sasl_password = "old-sasl-pass".into();
        app.accept_invalid_certs = true;
        app.auto_join_channels = "#old".into();
        app.auto_perform = "/ns identify old-nickserv-pass".into();
        assert!(app.start_session_from_form());
        app.conversations.insert("#old".into(), Channel::new());

        let (tx, mut rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        app.test_set_connected(true);
        app.test_set_connecting_flag(false);
        app.apply_unprofiled_endpoint("new.example", "7000", false);
        assert!(app.switch_session_from_form("Changing servers"));

        // Until the old worker exits, display/log/reconnect identity remains A.
        assert_eq!(app.active_endpoint_label(), "old.example:6697");
        assert!(matches!(rx.try_recv(), Ok(IrcCommand::Quit(_))));
        assert!(app.connection.staged_config().is_some());

        app.test_finish_close();
        assert_eq!(app.active_endpoint_label(), "new.example:7000");
        assert_eq!(
            app.active_log_context().map(|context| context.0),
            Some("plain://new.example:7000".into())
        );
        assert!(app.conversations.is_empty());
        assert!(app.pending_auto_perform.is_none());
        let session = app.active_session.as_ref().unwrap();
        assert!(session.server.password.is_none());
        assert!(session.server.sasl_password.is_none());
        assert!(session.auto_join_channels.is_empty());
        assert!(session.auto_perform.is_empty());
        assert!(!session.server.accept_invalid_certs);
        let config = app.active_server_config().unwrap();
        assert_eq!(config.host, "new.example");
        assert_eq!(config.port, 7000);
        assert!(!config.use_tls);
        assert!(config.sasl_username.is_none());
    }

    #[test]
    fn manual_disconnect_cancels_a_staged_endpoint_switch() {
        let mut app = test_app();
        assert!(app.start_session_from_form());
        app.test_set_connected(true);
        let (tx, _rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        app.apply_unprofiled_endpoint("other.example", "6667", false);
        assert!(app.switch_session_from_form("Changing servers"));
        assert!(app.connection.staged_config().is_some());

        app.request_manual_disconnect(None);
        assert!(app.connection.staged_config().is_none());
    }

    #[test]
    fn connection_form_rejects_every_invalid_port_without_a_fallback() {
        let mut app = test_app();
        for invalid in ["", "0", "abc", "65536", "70000", "-1"] {
            app.server_port = invalid.into();
            assert!(
                app.form_server_config_for_tests().is_err(),
                "port {invalid:?} must be rejected"
            );
        }
        for valid in ["1", "65535"] {
            app.server_port = valid.into();
            assert_eq!(
                app.form_server_config_for_tests().unwrap().port.to_string(),
                valid
            );
        }

        app.server_host = "   ".into();
        app.server_port = "6697".into();
        app.show_connect_dialog = false;
        assert!(!app.start_session_from_form());
        assert!(app.show_connect_dialog);
        assert!(!app.is_connecting());
        assert!(app.connection_error().is_some());
        assert!(
            app.server_messages
                .back()
                .is_some_and(|message| message.content.contains("cannot be empty"))
        );
    }

    #[test]
    fn favorite_load_preserves_its_endpoint_scoped_profile() {
        let mut app = test_app();
        let favorite = ServerFavorite {
            upload_settings: Default::default(),
            name: "private".into(),
            host: "favorite.example".into(),
            port: "7443".into(),
            use_tls: true,
            password: "server-pass".into(),
            nickname: "favnick".into(),
            auto_join: "#private key".into(),
            auto_perform: "/ns identify nickserv-pass".into(),
            sasl_username: "account".into(),
            sasl_password: "sasl-pass".into(),
            sasl_mechanism: SaslMechanism::Auto,
            sasl_required: false,
            bouncer: None,
            tls_client_cert_path: String::new(),
            tls_client_key_path: String::new(),
            username: "ident".into(),
            realname: "Favorite User".into(),
            accept_invalid_certs: true,
            pre_away_message: "syncing".into(),
            persistence_profile: "mobile".into(),
            id: "0123456789abcdef".into(),
            auto_connect: false,
        };
        app.load_favorite(&favorite);
        assert!(app.start_session_from_form());

        let config = app.active_server_config().unwrap();
        assert_eq!(config.host, "favorite.example");
        assert_eq!(config.port, 7443);
        assert_eq!(config.password.as_deref(), Some("server-pass"));
        assert_eq!(config.sasl_username.as_deref(), Some("account"));
        assert_eq!(config.sasl_password.as_deref(), Some("sasl-pass"));
        assert_eq!(config.pre_away_message.as_deref(), Some("syncing"));
        assert_eq!(config.persistence_profile.as_deref(), Some("mobile"));
        assert!(config.accept_invalid_certs);
        assert_eq!(
            app.active_session.as_ref().unwrap().auto_join_channels,
            "#private key"
        );
    }

    #[test]
    fn chathistory_replay_keeps_own_notice_rows() {
        let mut app = test_app();
        app.prepare_history_target("#chan");
        app.set_session_nick("me".into(), true);
        app.handle_cap_message("ACK", &["echo-message".into()]);

        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +replay chathistory #chan").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=replay :me!u@h NOTICE #chan :my own notice").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -replay").unwrap());

        let chan = app
            .conversations
            .get("#chan")
            .expect("history target remains open");
        assert!(
            chan.messages
                .iter()
                .any(|message| message.content.contains("my own notice")),
            "replayed own notices must not be dropped as echoes"
        );

        // A live (non-batch) own notice is still suppressed as an echo.
        app.handle_incoming_message(IrcMessage::parse(":me!u@h NOTICE #chan :live echo").unwrap());
        assert!(
            !app.conversations["#chan"]
                .messages
                .iter()
                .any(|message| message.content.contains("live echo"))
        );
    }

    #[test]
    fn credential_commands_never_enter_history_search_or_local_scrollback() {
        let (mut app, mut rx) = test_app_connected();
        commit_test_endpoint(&mut app);
        app.set_session_nick("me".into(), true);
        assert!(app.open_query("NickServ"));

        for input in [
            "IDENTIFY hunter2",
            "/say IDENTIFY hunter3",
            "/msg NickServ IDENTIFY hunter4",
            "/query NickServ IDENTIFY hunter5",
            "/ns IDENTIFY hunter6",
            "/notice NickServ IDENTIFY hunter7",
        ] {
            app.input_text = input.into();
            app.process_input();
        }
        while rx.try_recv().is_ok() {}

        assert!(app.command_history.is_empty());
        let query = &app.conversations["NickServ"];
        for secret in ["hunter2", "hunter3", "hunter4", "hunter5", "hunter6"] {
            assert!(
                query
                    .messages
                    .iter()
                    .all(|message| !message.content.contains(secret))
            );
        }
        assert!(
            query.messages.iter().any(|message| {
                message.no_log && message.content == "<credential command sent>"
            })
        );
        assert!(
            query
                .messages
                .iter()
                .all(|message| { !message.content.contains("hunter7") })
        );

        app.input_text = "/lastlog IDENTIFY".into();
        app.process_input();
        assert!(
            app.conversations["NickServ"]
                .messages
                .iter()
                .all(|message| !message.content.contains("hunter"))
        );
    }

    #[test]
    fn casemapped_channel_messages_share_one_endpoint_scoped_log() {
        let dir = temporary_log_dir("casemapped-log");
        let mut app = test_app();
        app.log_manager = logging::LogManager::with_test_dir(dir.clone());
        app.logging_enabled = true;
        app.server_host = "IRC.Example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        assert!(app.start_session_from_form());

        app.add_message_to_channel("#Te[st", ChatMessage::new_fmt("alice", "first", "short"));
        app.add_message_to_channel("#te{st", ChatMessage::new_fmt("bob", "second", "short"));
        app.log_manager.flush_all();

        assert_eq!(app.conversations.len(), 1);
        assert!(app.conversations.contains_key("#Te[st"));
        // Log keys are casemapped (#142): both spellings hash to one file.
        let canonical = app.network_support.canonicalize("#Te[st");
        let path = app
            .log_manager
            .test_log_path("tls://irc.example:6697", &canonical);
        let alternate = app
            .log_manager
            .test_log_path("tls://irc.example:6697", "#te{st");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("first"));
        assert!(contents.contains("second"));
        assert_eq!(alternate, std::path::PathBuf::from(&path));

        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn casemapping_variant_spelling_still_finds_history() {
        let dir = temporary_log_dir("casemapped-history");
        let mut app = test_app();
        app.log_manager = logging::LogManager::with_test_dir(dir.clone());
        app.logging_enabled = true;
        app.server_host = "IRC.Example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        assert!(app.start_session_from_form());

        // A pre-#142 session wrote under the raw spelling.
        app.log_manager.log_message(
            "tls://irc.example:6697",
            "#Te[st",
            &ChatMessage::new_fmt("alice", "legacy line", "short"),
        );
        app.log_manager.flush_all();

        // The new session joins under the canonical spelling and must still
        // find the old file through the raw-spelling fallback.
        let raw = "#Te[st";
        let canonical = app.network_support.canonicalize(raw);
        assert_ne!(raw, canonical.as_str());
        let history = app.log_manager.load_history_casemapped(
            "tls://irc.example:6697",
            "irc.example",
            &canonical,
            raw,
            50,
            false,
        );
        assert!(
            history
                .iter()
                .any(|message| message.content == "legacy line")
        );

        // The fallback must not seal once the canonical file has a row: the
        // first post-upgrade write lands in the canonical file, and later
        // sessions must still see the pre-#142 lines (#146).
        app.log_manager.log_message(
            "tls://irc.example:6697",
            &canonical,
            &ChatMessage::new_fmt("bob", "canonical line", "short"),
        );
        app.log_manager.flush_all();
        let history = app.log_manager.load_history_casemapped(
            "tls://irc.example:6697",
            "irc.example",
            &canonical,
            raw,
            50,
            false,
        );
        assert!(
            history
                .iter()
                .any(|message| message.content == "legacy line"),
            "raw-spelling history must survive the first canonical write"
        );
        assert!(
            history
                .iter()
                .any(|message| message.content == "canonical line")
        );
        // Older file first.
        let contents: Vec<&str> = history
            .iter()
            .filter_map(|message| match message.content.as_str() {
                "legacy line" => Some("legacy"),
                "canonical line" => Some("canonical"),
                _ => None,
            })
            .collect();
        assert_eq!(contents, vec!["legacy", "canonical"]);

        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn whox_fields_follow_the_spec_column_order() {
        let mut app = test_app();
        app.set_session_nick("me".into(), true);
        let mut channel = Channel::new();
        channel.add_user("alice", UserMode::Normal);
        channel.add_user("bob", UserMode::Normal);
        app.conversations.insert("#chan".to_string(), channel);
        app.pending_whox
            .push_back(("7".to_string(), "#chan".to_string()));

        // Request is %tcuhnafr, but WHOX always replies in the fixed order
        // `t c u i h s n f d l a o r` regardless of request order:
        // token, channel, user, host, nick, flags, account, realname.
        app.handle_numeric(
            RPL_WHOSPCRPL,
            &[
                "me".into(),
                "7".into(),
                "#chan".into(),
                "u".into(),
                "h".into(),
                "alice".into(),
                "G".into(),
                "acc".into(),
                "Alice Real".into(),
            ],
        );
        let alice = app.conversations["#chan"].get_user("alice").unwrap();
        assert_eq!(alice.account.as_deref(), Some("acc"));
        assert!(alice.is_away(), "flags column G must mark away");
        assert_eq!(alice.realname.as_deref(), Some("Alice Real"));

        // Logged-out user: account "0" maps to None, here-flags to present.
        app.handle_numeric(
            RPL_WHOSPCRPL,
            &[
                "me".into(),
                "7".into(),
                "#chan".into(),
                "u".into(),
                "h".into(),
                "bob".into(),
                "H@".into(),
                "0".into(),
                "Bob".into(),
            ],
        );
        let bob = app.conversations["#chan"].get_user("bob").unwrap();
        assert_eq!(bob.account.as_deref(), None);
        assert!(!bob.is_away());
    }

    #[test]
    fn whox_account_starting_with_g_is_not_mistaken_for_away() {
        let mut app = test_app();
        app.set_session_nick("me".into(), true);
        let mut channel = Channel::new();
        channel.add_user("carol", UserMode::Normal);
        app.conversations.insert("#chan".to_string(), channel);
        app.pending_whox
            .push_back(("9".to_string(), "#chan".to_string()));

        // Present user (flags "H") whose services account is "Guest42".
        app.handle_numeric(
            RPL_WHOSPCRPL,
            &[
                "me".into(),
                "9".into(),
                "#chan".into(),
                "u".into(),
                "h".into(),
                "carol".into(),
                "H".into(),
                "Guest42".into(),
                "Carol".into(),
            ],
        );
        let carol = app.conversations["#chan"].get_user("carol").unwrap();
        assert_eq!(carol.account.as_deref(), Some("Guest42"));
        assert!(!carol.is_away(), "an account starting with G is not away");
    }

    #[test]
    fn end_of_who_completes_only_the_oldest_whox_request_for_the_channel() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.network_support.whox = true;
        let mut channel = Channel::new();
        channel.joined = true;
        channel.add_user("alice", UserMode::Normal);
        app.conversations.insert("#chan".to_string(), channel);
        let whox_reply = |token: &str, account: &str| {
            [
                "me".to_string(),
                token.to_string(),
                "#chan".to_string(),
                "u".to_string(),
                "h".to_string(),
                "alice".to_string(),
                "H".to_string(),
                account.to_string(),
                "Alice".to_string(),
            ]
        };
        let account = |app: &Session| {
            app.conversations["#chan"]
                .get_user("alice")
                .unwrap()
                .account
                .clone()
        };

        // Two overlapping NAMES bursts (a manual /names during the automatic
        // WHOX window) each queue a WHOX query; tokens count up from 1.
        app.handle_numeric(RPL_ENDOFNAMES, &["me".into(), "#chan".into()]);
        app.handle_numeric(RPL_ENDOFNAMES, &["me".into(), "#chan".into()]);

        // 315 carries no token: it ends the first query only.
        app.handle_numeric(
            RPL_ENDOFWHO,
            &["me".into(), "#chan".into(), "End of /WHO list".into()],
        );
        app.handle_numeric(RPL_WHOSPCRPL, &whox_reply("1", "stale"));
        assert_eq!(account(&app), None, "the finished query's token is spent");
        app.handle_numeric(RPL_WHOSPCRPL, &whox_reply("2", "acc"));
        assert_eq!(
            account(&app).as_deref(),
            Some("acc"),
            "the second query is still being answered"
        );

        // A 315 for another channel completes nothing here.
        app.handle_numeric(
            RPL_ENDOFWHO,
            &["me".into(), "#other".into(), "End of /WHO list".into()],
        );
        app.handle_numeric(RPL_WHOSPCRPL, &whox_reply("2", "acc2"));
        assert_eq!(account(&app).as_deref(), Some("acc2"));

        app.handle_numeric(
            RPL_ENDOFWHO,
            &["me".into(), "#chan".into(), "End of /WHO list".into()],
        );
        app.handle_numeric(RPL_WHOSPCRPL, &whox_reply("2", "late"));
        assert_eq!(account(&app).as_deref(), Some("acc2"));
    }

    #[test]
    fn names_without_userhost_keeps_the_known_user_and_host() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        for name in ["#a", "#b"] {
            let mut channel = Channel::new();
            channel.joined = true;
            app.conversations.insert(name.to_string(), channel);
        }
        app.handle_numeric(
            RPL_NAMREPLY,
            &["me".into(), "=".into(), "#a".into(), "alice bob".into()],
        );
        app.handle_numeric(RPL_ENDOFNAMES, &["me".into(), "#a".into()]);
        // WHO teaches us alice's user and host.
        app.handle_numeric(
            RPL_WHOREPLY,
            &[
                "me".into(),
                "#a".into(),
                "au".into(),
                "ahost".into(),
                "srv".into(),
                "alice".into(),
                "H".into(),
                "0 Alice".into(),
            ],
        );

        // Joining a second channel on a server without userhost-in-names.
        app.handle_numeric(
            RPL_NAMREPLY,
            &["me".into(), "=".into(), "#b".into(), "@alice carol".into()],
        );
        app.handle_numeric(RPL_ENDOFNAMES, &["me".into(), "#b".into()]);

        for channel in ["#a", "#b"] {
            let alice = app.conversations[channel].get_user("alice").unwrap();
            assert_eq!(alice.username.as_deref(), Some("au"), "{channel}");
            assert_eq!(alice.hostname.as_deref(), Some("ahost"), "{channel}");
        }

        // When the server does send userhost-in-names, it is still applied.
        app.handle_numeric(
            RPL_NAMREPLY,
            &[
                "me".into(),
                "=".into(),
                "#b".into(),
                "@alice!newu@newhost carol".into(),
            ],
        );
        app.handle_numeric(RPL_ENDOFNAMES, &["me".into(), "#b".into()]);
        let alice = app.conversations["#b"].get_user("alice").unwrap();
        assert_eq!(alice.username.as_deref(), Some("newu"));
        assert_eq!(alice.hostname.as_deref(), Some("newhost"));
    }

    #[test]
    fn large_names_and_whox_propagation_across_channels_is_not_quadratic() {
        const USERS: usize = 10_000;
        let started = std::time::Instant::now();
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.network_support.whox = true;
        for name in ["#a", "#b"] {
            let mut channel = Channel::new();
            channel.joined = true;
            app.conversations.insert(name.to_string(), channel);
        }
        let nicks: Vec<String> = (0..USERS).map(|i| format!("user{i}")).collect();

        // The same 10k members are listed in two channels, 100 per 353 line.
        for name in ["#a", "#b"] {
            for chunk in nicks.chunks(100) {
                let line = chunk
                    .iter()
                    .enumerate()
                    .map(|(i, nick)| {
                        if i % 25 == 0 {
                            format!("@{nick}")
                        } else {
                            nick.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                app.handle_numeric(RPL_NAMREPLY, &["me".into(), "=".into(), name.into(), line]);
            }
            app.handle_numeric(RPL_ENDOFNAMES, &["me".into(), name.into()]);
        }
        assert_eq!(app.conversations["#a"].users.len(), USERS);
        assert_eq!(app.conversations["#b"].users.len(), USERS);

        // 366 queued a WHOX for each channel; answer #a's for every member.
        let token = app
            .pending_whox
            .iter()
            .find(|(_, channel)| channel.as_str() == "#a")
            .map(|(token, _)| token.clone())
            .expect("WHOX requested after NAMES");
        for (i, nick) in nicks.iter().enumerate() {
            app.handle_numeric(
                RPL_WHOSPCRPL,
                &[
                    "me".into(),
                    token.clone(),
                    "#a".into(),
                    format!("u{i}"),
                    "host".into(),
                    nick.clone(),
                    "H".into(),
                    format!("acct{i}"),
                    "Real".into(),
                ],
            );
        }

        // The reply for each member reached both channels they share.
        for channel in ["#a", "#b"] {
            let user = app.conversations[channel].get_user("USER7777").unwrap();
            assert_eq!(user.account.as_deref(), Some("acct7777"), "{channel}");
            assert_eq!(user.username.as_deref(), Some("u7777"), "{channel}");
        }
        // Quadratic allocation-heavy scans take minutes here; the bound is
        // generous so slow CI machines do not flake.
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "10k-user NAMES+WHOX took {elapsed:?}"
        );
    }

    #[test]
    fn replayed_own_notice_does_not_duplicate_the_local_confirmation() {
        // Must be connected: the confirmation row only exists when the
        // /notice actually went out (#152 — the disconnected branch never
        // creates the row and the dedup under test would not run).
        let (mut app, _rx) = test_app_connected();
        commit_test_endpoint(&mut app);
        app.set_session_nick("me".into(), true);
        app.handle_cap_message("ACK", &["echo-message".into(), "draft/chathistory".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        // The local confirmation row "-> -#chan- unique-notice-text" exists.
        app.process_command("/notice #chan unique-notice-text");
        assert!(
            app.conversations["#chan"]
                .messages
                .iter()
                .any(|message| message.content == "-> -#chan- unique-notice-text"),
            "precondition: the confirmation row must exist"
        );

        // The server replays the same notice with a msgid inside a batch.
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +r chathistory #chan").unwrap());
        app.handle_incoming_message(
            IrcMessage::parse("@batch=r;msgid=n1 :me!u@h NOTICE #chan :unique-notice-text")
                .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -r").unwrap());

        let hits = app.conversations["#chan"]
            .messages
            .iter()
            .filter(|message| message.content.contains("unique-notice-text"))
            .count();
        assert_eq!(hits, 1, "the notice must appear exactly once");
    }

    #[test]
    fn replayed_own_credential_privmsg_is_not_displayed() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.handle_cap_message("ACK", &["draft/chathistory".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("NickServ".to_string(), channel);
        app.select_target("NickServ");

        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +r chathistory NickServ").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=r;msgid=n2 :me!u@h PRIVMSG NickServ :IDENTIFY hunter12")
                .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -r").unwrap());

        assert!(
            app.conversations["NickServ"]
                .messages
                .iter()
                .all(|message| !message.content.contains("hunter12")),
            "the server's verbatim copy of a redacted local row must not render"
        );
    }

    #[test]
    fn status_prefixed_confirmation_still_suppresses_replay() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.handle_cap_message("ACK", &["echo-message".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        // Confirmation stores the target as typed: "-> -@#chan- op hello".
        app.process_command("/notice @#chan op hello");

        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +r chathistory #chan").unwrap());
        app.handle_incoming_message(
            IrcMessage::parse("@batch=r;msgid=n3 :me!u@h NOTICE #chan :op hello").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -r").unwrap());

        let hits = app.conversations["#chan"]
            .messages
            .iter()
            .filter(|message| message.content.contains("op hello"))
            .count();
        assert_eq!(hits, 1, "status-prefixed confirmation must match");
    }

    #[test]
    fn identical_consecutive_sends_pair_with_echoes_in_send_order() {
        let (mut app, _rx) = test_app_connected();
        commit_test_endpoint(&mut app);
        app.set_session_nick("me".into(), true);
        app.handle_cap_message("ACK", &["echo-message".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        app.process_command("/say lol");
        app.process_command("/say lol");
        assert_eq!(app.conversations["#chan"].messages.len(), 2);

        // Echoes come back in send order; the first must confirm the first row.
        app.handle_incoming_message(
            IrcMessage::parse(
                "@msgid=one;time=2026-10-25T12:00:00.000Z :me!u@h PRIVMSG #chan :lol",
            )
            .unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@msgid=two;time=2026-10-25T12:00:01.000Z :me!u@h PRIVMSG #chan :lol",
            )
            .unwrap(),
        );

        let msgids: Vec<_> = app.conversations["#chan"]
            .messages
            .iter()
            .map(|message| message.msgid.as_deref())
            .collect();
        assert_eq!(msgids, vec![Some("one"), Some("two")]);
    }

    #[test]
    fn echo_never_stamps_a_row_loaded_from_the_local_log() {
        let (mut app, _rx) = test_app_connected();
        commit_test_endpoint(&mut app);
        app.set_session_nick("me".into(), true);
        app.handle_cap_message("ACK", &["echo-message".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        // Rows loaded from the irssi log carry neither a msgid nor a
        // server-time, exactly like a freshly sent, unconfirmed local row.
        channel
            .messages
            .push_back(ChatMessage::new_fmt("me", "ok", "short"));
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        app.process_command("/say ok");
        assert_eq!(app.conversations["#chan"].messages.len(), 2);
        app.handle_incoming_message(
            IrcMessage::parse(
                "@msgid=fresh;time=2026-10-25T12:00:00.000Z :me!u@h PRIVMSG #chan :ok",
            )
            .unwrap(),
        );

        let messages = &app.conversations["#chan"].messages;
        assert_eq!(messages.len(), 2, "the echo must not add a third row");
        assert_eq!(
            messages[0].msgid, None,
            "the old history row must stay unstamped"
        );
        assert_eq!(
            messages[1].msgid.as_deref(),
            Some("fresh"),
            "the echo must confirm the row this session just sent"
        );
        assert!(messages[1].server_time.is_some());

        // The row is confirmed once: a later echo is not paired with it again.
        app.handle_incoming_message(
            IrcMessage::parse(
                "@msgid=again;time=2026-10-25T12:00:01.000Z :me!u@h PRIVMSG #chan :ok",
            )
            .unwrap(),
        );
        let messages = &app.conversations["#chan"].messages;
        assert_eq!(messages[0].msgid, None);
        assert_eq!(messages[1].msgid.as_deref(), Some("fresh"));
    }

    #[test]
    fn peer_chat_text_does_not_create_a_confirmation() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.handle_cap_message("ACK", &["echo-message".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        // A peer types the literal confirmation shape: it must not suppress
        // a genuine replay of our own notice sent from another device.
        app.handle_incoming_message(
            IrcMessage::parse(":pal!u@h PRIVMSG #chan :-> -#chan- hello").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +r chathistory #chan").unwrap());
        app.handle_incoming_message(
            IrcMessage::parse("@batch=r;msgid=n4 :me!u@h NOTICE #chan :hello").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -r").unwrap());

        let hits = app.conversations["#chan"]
            .messages
            .iter()
            .filter(|message| message.content.contains("hello"))
            .count();
        // Both rows must survive: the peer's lookalike AND our own replay.
        // (hits >= 1 would pass even with the confirmation collector broken,
        // because the peer row alone satisfies it — #163.)
        assert_eq!(hits, 2, "replay must survive third-party lookalikes");
    }

    #[test]
    fn own_notice_starting_with_an_auth_keyword_is_dropped_from_service_replay() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.handle_cap_message("ACK", &["echo-message".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("NickServ".to_string(), channel);
        app.select_target("NickServ");

        // An own notice with auth-keyword text replayed from a service query
        // is dropped (over-redaction is the safe direction there); the
        // channel counterpart is covered by
        // replayed_own_channel_chat_with_credential_words_survives.
        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +r chathistory NickServ").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=r;msgid=n5 :me!u@h NOTICE NickServ :IDENTIFY hunter15")
                .unwrap(),
        );
        // Non-auth own notices survive.
        app.handle_incoming_message(
            IrcMessage::parse("@batch=r;msgid=n6 :me!u@h NOTICE NickServ :see you at the vote")
                .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -r").unwrap());

        let rows: Vec<&str> = app.conversations["NickServ"]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert!(
            rows.iter().all(|content| !content.contains("hunter15")),
            "{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|content| content.contains("see you at the vote")),
            "{rows:?}"
        );
    }

    #[test]
    fn nick_in_use_retry_does_not_record_the_configured_nick() {
        let mut app = test_app();
        // Pre-registration: alice was configured but never confirmed here.
        app.set_session_nick("alice".into(), false);
        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &["me".into(), "alice".into(), "in use".into()],
        );
        assert_eq!(app.my_nick, "alice_");
        assert!(
            !app.past_nicks.iter().any(|held| held == "alice"),
            "the in-use nick is held by someone else and must not count as ours"
        );
    }

    #[test]
    fn session_reset_clears_past_nicks() {
        let mut app = test_app();
        app.set_session_nick("alice".into(), true);
        app.set_session_nick("bob".into(), true);
        assert!(!app.past_nicks.is_empty());
        app.reset_session_state();
        assert!(app.past_nicks.is_empty());
    }

    #[test]
    fn pass_and_authenticate_are_credential_commands() {
        assert!(commands::service_message_contains_credentials(
            "NickServ",
            "PASS hunter2"
        ));
        assert!(commands::service_message_contains_credentials(
            "NickServ",
            "AUTHENTICATE <b64>"
        ));
        assert!(commands::command_line_contains_credentials(
            "/ns PASS hunter2"
        ));
    }

    #[test]
    fn log_summaries_of_credential_commands_omit_the_size() {
        // A logged byte count would give away the length of the secret.
        for secret in [
            IrcCommand::Pass("hunter2".into()),
            IrcCommand::Authenticate("aHVudGVyMg==".into()),
            IrcCommand::Privmsg("NickServ".into(), "IDENTIFY hunter2".into()),
            IrcCommand::Notice("ChanServ".into(), "IDENTIFY #room hunter2".into()),
            IrcCommand::Raw("OPER admin hunter2".into()),
            IrcCommand::Raw("SQUERY NickServ :IDENTIFY hunter2".into()),
            IrcCommand::Raw("@label=1 PRIVMSG NickServ :IDENTIFY hunter2".into()),
        ] {
            let summary = Session::command_log_summary(&secret);
            assert!(!summary.contains("bytes"), "{secret:?}: {summary}");
            assert!(!summary.contains("hunter2"), "{secret:?}: {summary}");
            assert!(!summary.is_empty());
        }
        assert_eq!(
            Session::command_log_summary(&IrcCommand::Pass("x".into())),
            "PASS"
        );

        // Everything else keeps its size, which is what makes the log useful.
        assert_eq!(
            Session::command_log_summary(&IrcCommand::Privmsg("#room".into(), "hello".into())),
            "PRIVMSG (20 bytes)"
        );
        assert_eq!(
            Session::command_log_summary(&IrcCommand::Privmsg(
                "NickServ".into(),
                "INFO alice".into()
            )),
            "PRIVMSG (28 bytes)"
        );
    }

    #[test]
    fn send_failure_rows_are_not_persisted_to_logs() {
        let mut app = Session::with_settings(crate::gui::Settings::default());
        app.select_target("#chan");
        app.conversations.insert("#chan".into(), Channel::new());
        app.report_send_failure(false, "Not connected - away status not changed");
        assert!(
            app.conversations["#chan"]
                .messages
                .iter()
                .all(|message| message.no_log)
        );
    }

    #[test]
    fn live_credential_redaction_recovers_former_nicks() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("Guest123".into(), true);
        app.set_session_nick("alice".into(), true);
        app.add_message_to_channel(
            "NickServ",
            ChatMessage::new_fmt("Guest123", "IDENTIFY hunter16", "short"),
        );
        let query = &app.conversations["NickServ"];
        assert!(
            query
                .messages
                .iter()
                .all(|message| !message.content.contains("hunter16")),
            "former-nick credential rows must be redacted on the live path"
        );
        assert!(query.messages.iter().any(|message| message.no_log));
    }

    #[test]
    fn replayed_own_non_credential_privmsg_survives() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.handle_cap_message("ACK", &["draft/chathistory".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("NickServ".to_string(), channel);
        app.select_target("NickServ");

        // Own row to an auth service but NOT a credential command: replay
        // must keep it (#158 drops only the credential shape).
        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +r chathistory NickServ").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=r;msgid=n6 :me!u@h PRIVMSG NickServ :INFO alice").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -r").unwrap());

        assert!(
            app.conversations["NickServ"]
                .messages
                .iter()
                .any(|message| message.content == "INFO alice")
        );
    }

    #[test]
    fn replayed_own_credential_row_is_dropped_for_service_batches_and_former_nick() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("Guest123".into(), true);
        app.set_session_nick("alice".into(), true); // GHOST/REGAIN: Guest123 is a past nick
        app.handle_cap_message("ACK", &["draft/chathistory".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("NickServ".to_string(), channel);
        app.select_target("NickServ");

        // The sending nick predates the GHOST/REGAIN: the keyword check still
        // runs for any own row replayed from a service query.
        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +r chathistory NickServ").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=r;msgid=n7 :Guest123!u@h PRIVMSG NickServ :IDENTIFY hunter13",
            )
            .unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=r;msgid=n9 :Guest123!u@h NOTICE NickServ :IDENTIFY hunter14")
                .unwrap(),
        );
        // A non-credential own row under the former nick must survive.
        app.handle_incoming_message(
            IrcMessage::parse("@batch=r;msgid=n8 :Guest123!u@h PRIVMSG NickServ :INFO alice")
                .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -r").unwrap());

        let rows: Vec<&str> = app.conversations["NickServ"]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert!(
            rows.iter()
                .all(|content| !content.contains("hunter13") && !content.contains("hunter14")),
            "{rows:?}"
        );
        assert!(rows.contains(&"INFO alice"), "{rows:?}");
    }

    #[test]
    fn replayed_own_channel_chat_with_credential_words_survives() {
        // "drop me a line", "group chat", "login works" are ordinary words in
        // a channel: the keyword drop applies only to service-shaped targets.
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.handle_cap_message("ACK", &["draft/chathistory".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +r chathistory #chan").unwrap());
        for (id, line) in [
            ("c1", "PRIVMSG #chan :drop me a line"),
            ("c2", "PRIVMSG #chan :group chat tonight?"),
            ("c3", "NOTICE #chan :login works now"),
        ] {
            app.handle_incoming_message(
                IrcMessage::parse(&format!("@batch=r;msgid={id} :me!u@h {line}")).unwrap(),
            );
        }
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -r").unwrap());

        let rows: Vec<&str> = app.conversations["#chan"]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        for expected in ["drop me a line", "group chat tonight?"] {
            assert!(rows.contains(&expected), "{expected}: {rows:?}");
        }
        assert!(
            rows.iter()
                .any(|content| content.contains("login works now")),
            "{rows:?}"
        );
    }

    #[test]
    fn same_endpoint_reconnect_keeps_held_nicks() {
        let mut app = test_app();
        app.nickname = "alice".into();
        app.server_host = "irc.example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        let session = app.session_from_form().unwrap();
        app.activate_session(session.clone());
        let _registration = test_registration_transport(&mut app);
        // This connection's own 001 confirms the configured nick.
        app.handle_numeric(RPL_WELCOME, &["alice".into(), "welcome".into()]);
        assert!(app.nick_confirmed);
        // "/nick bob" accepted mid-session: alice recorded as held.
        app.set_session_nick("bob".into(), true);
        assert!(app.sender_is_self("alice"));

        // Reconnect to the SAME endpoint: bob was genuinely ours, and the
        // configured alice stays held (#168).
        app.test_set_disconnected();
        app.activate_session(session);
        assert!(
            app.sender_is_self("bob"),
            "held nick must survive reconnect"
        );
        assert!(app.sender_is_self("alice"));
    }

    #[test]
    fn endpoint_change_and_speculative_nicks_are_not_recorded() {
        let mut app = test_app();
        app.server_host = "irc.example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        let session = app.session_from_form().unwrap();
        app.activate_session(session.clone());
        let _registration = test_registration_transport(&mut app);
        app.handle_numeric(RPL_WELCOME, &["bob".into(), "welcome".into()]);
        // The connection drops; the auto-reconnect re-registers as "bob"
        // and the server 433s — bob was confirmed on this lineage.
        app.mark_connection_lost();
        let _registration = test_registration_transport(&mut app);

        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &["me".into(), "bob".into(), "in use".into()],
        );
        assert_eq!(app.my_nick, "bob_");
        assert!(
            app.sender_is_self("bob"),
            "confirmed walkaway must be recorded"
        );

        // Endpoint change drops the old lineage entirely.
        app.nickname = "other".into();
        app.server_host = "other.example".into();
        let other = app.session_from_form().unwrap();
        app.test_set_disconnected();
        app.activate_session(other);
        assert!(app.past_nicks.is_empty());
        assert!(!app.sender_is_self("bob_"));
        assert!(app.sender_is_self("other"));
    }

    #[test]
    fn welcome_records_a_confirmed_replacement_but_adopts_speculative() {
        let mut app = test_app();
        let _registration = test_registration_transport(&mut app);
        // Confirmed nick replaced by the server's 001 (bouncer rewrite):
        // the confirmed nick is recorded as held (#168).
        app.set_session_nick("bob".into(), true);
        // Real 001 shape: the granted nick is the first parameter.
        app.handle_numeric(RPL_WELCOME, &["alice".into(), "welcome".into()]);
        assert_eq!(app.my_nick, "alice");
        assert!(app.sender_is_self("bob"), "confirmed nick must be recorded");

        // Speculative pre-registration nick replaced at 001: never held.
        let mut app2 = test_app();
        let _registration2 = test_registration_transport(&mut app2);
        app2.handle_numeric(ERR_NICKNAMEINUSE, &["x".into(), "in use".into()]);
        app2.handle_numeric(RPL_WELCOME, &["x__".into(), "welcome".into()]);
        assert_eq!(app2.my_nick, "x__");
        assert!(!app2.sender_is_self("x"), "never-held nick must not count");
    }

    #[test]
    fn squatted_former_nick_notices_and_notifications_still_render() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("alice".into(), true);
        app.set_session_nick("bob".into(), true);
        app.handle_cap_message("ACK", &["echo-message".into()]);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        // A peer took 'alice' and sends a channel NOTICE: the live
        // echo-suppression gate is current-nick-only (#168).
        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h NOTICE #chan :maintenance at noon").unwrap(),
        );
        assert!(
            app.conversations["#chan"]
                .messages
                .iter()
                .any(|message| message.content.contains("maintenance at noon")),
            "a squatter's notice must not be dropped as our echo"
        );
    }

    #[test]
    fn reconnection_chains_keep_attribution_exact_across_identities() {
        let mut app = test_app();
        app.nickname = "alice".into();
        app.server_host = "irc.example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        app.sasl_username = "acct".into();
        app.sasl_password = "pw1".into();
        let session = app.session_from_form().unwrap();
        app.activate_session(session.clone());
        let _registration = test_registration_transport(&mut app);

        // Pre-registration walkaway: nothing recorded, then 001 grants the
        // suffixed nick (confirmed, past_nicks empty).
        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &["me".into(), "alice".into(), "in use".into()],
        );
        app.handle_numeric(RPL_WELCOME, &["alice_".into(), "welcome".into()]);
        assert_eq!(app.my_nick, "alice_");
        assert!(app.past_nicks.is_empty());
        assert!(app.nick_confirmed);

        // Manual same-endpoint reconnect with the same account: the config
        // nick differs from the confirmed one, so it is adopted unconfirmed
        // and a subsequent pre-registration 433 must NOT attribute the
        // stranger-held "alice" (#171).
        app.test_set_disconnected();
        app.activate_session(session.clone());
        let _registration = test_registration_transport(&mut app);
        assert!(!app.nick_confirmed);
        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &["me".into(), "alice".into(), "in use".into()],
        );
        assert!(
            !app.past_nicks.iter().any(|held| held == "alice"),
            "a never-granted nick must not be recorded from the config match (#171)"
        );

        // Re-applying the confirmed nick itself keeps confirmation (#170),
        // and the equal-case 001 records nothing (#169).
        app.test_set_disconnected();
        app.activate_session(session.clone());
        let _registration = test_registration_transport(&mut app);
        app.handle_numeric(RPL_WELCOME, &["alice_".into(), "welcome".into()]);
        assert_eq!(app.my_nick, "alice_");
        assert!(app.nick_confirmed);
        // The same-lineage reconnect kept the genuinely-held nick (#168).
        assert_eq!(app.past_nicks, vec!["alice_".to_string()]);

        // SASL account change: the lineage is dropped even on the same
        // endpoint (#171) — a different account's holder must not inherit
        // our attribution.
        app.sasl_username = "other".into();
        app.sasl_password = "pw2".into();
        let other = app.session_from_form().unwrap();
        app.test_set_disconnected();
        app.activate_session(other);
        assert!(app.past_nicks.is_empty());
        assert!(!app.sender_is_self("alice_"));
    }

    #[test]
    fn same_endpoint_reactivation_resets_the_433_walkaway_budget() {
        let mut app = test_app();
        app.nickname = "alice".into();
        app.server_host = "irc.example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        let session = app.session_from_form().unwrap();
        app.activate_session(session.clone());

        // Exhaust the walkaway ladder.
        for _ in 0..3 {
            app.handle_numeric(
                ERR_NICKNAMEINUSE,
                &["me".into(), "alice".into(), "in use".into()],
            );
        }
        app.fail_registration("Registration failed".into());

        // Manual reconnect to the same endpoint: the fresh connection gets
        // a fresh budget, so the FIRST 433 walks away again (#172).
        app.activate_session(session);
        assert_eq!(app.nick_retry_attempts, 0);
        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &["me".into(), "alice".into(), "in use".into()],
        );
        assert_eq!(app.my_nick, "alice_");
    }

    #[test]
    fn identity_change_clears_the_outgoing_sessions_away_state() {
        let mut app = test_app();
        app.nickname = "alice".into();
        app.sasl_username = "acct".into();
        app.sasl_password = "pw1".into();
        app.server_host = "irc.example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        let session = app.session_from_form().unwrap();
        app.activate_session(session.clone());
        app.away_status = Some("brb".into());
        app.auto_away_triggered = true;

        app.sasl_username = "other".into();
        app.sasl_password = "pw2".into();
        let other = app.session_from_form().unwrap();
        app.activate_session(other);
        assert!(
            app.away_status.is_none(),
            "#172: away state is account-scoped"
        );
        assert!(!app.auto_away_triggered);
    }

    #[test]
    fn forced_nick_rename_routes_through_the_choke_point() {
        let mut app = test_app();
        app.set_session_nick("mine".into(), true);
        // ircu/IRCnet 043: the server force-renamed us on a collision.
        app.handle_numeric(RPL_SAVENICK, &["me".into(), "forced2".into()]);
        assert_eq!(app.my_nick, "forced2");
        assert!(app.nick_confirmed);
        assert!(
            app.sender_is_self("mine"),
            "the forced-away nick stays ours"
        );
    }

    #[test]
    fn walkaway_suffix_respects_the_advertised_nick_len() {
        let mut app = test_app();
        app.set_session_nick("michael".into(), false);
        app.network_support.nick_len = Some(5);
        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &["me".into(), "michael".into(), "in use".into()],
        );
        assert_eq!(app.my_nick, "mich_");
        assert!(app.my_nick.chars().count() <= 5);
    }

    #[test]
    fn clamp_progresses_past_an_identical_rejected_nick() {
        let mut app = test_app();
        app.network_support.nick_len = Some(6);
        app.set_session_nick("alice_".into(), false);
        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &["me".into(), "alice_".into(), "in use".into()],
        );
        assert_eq!(
            app.my_nick, "alic_",
            "the clamp must not regenerate the just-rejected nick (#173)"
        );
    }

    #[test]
    fn auto_reconnect_resets_the_walkaway_budget() {
        let mut app = test_app();
        app.nick_retry_attempts = 3;
        app.test_set_lost(true);
        app.auto_reconnect = true;
        app.test_set_failed_at(Some(std::time::Instant::now()));
        app.start_reconnect();
        assert_eq!(
            app.nick_retry_attempts, 0,
            "#173: fresh registration, fresh budget"
        );
    }

    #[test]
    fn forced_rename_applies_bookkeeping_and_is_announced() {
        let mut app = test_app();
        app.set_session_nick("mine".into(), true);
        let mut channel = Channel::new();
        channel.add_user("mine", crate::gui::UserMode::Normal);
        app.conversations.insert("#chan".into(), channel);
        app.handle_numeric(RPL_SAVENICK, &["me".into(), "forced2".into()]);
        assert_eq!(app.my_nick, "forced2");
        assert!(app.sender_is_self("mine"), "forced-away nick stays ours");
        let chan = &app.conversations["#chan"];
        assert!(chan.get_user("forced2").is_some(), "user list renamed");
        assert!(
            chan.messages
                .iter()
                .any(|message| message.content.contains("is now known as")),
            "the rename bookkeeping ran"
        );
        assert!(
            app.server_messages
                .iter()
                .any(|message| message.content.contains("Server forced nick change")),
            "the forced rename is announced in the server window"
        );
    }

    #[test]
    fn batch_parents_is_capped_against_hostile_opens() {
        let mut app = test_app();
        for i in 0..5000 {
            app.handle_incoming_message(
                IrcMessage::parse(&format!("@batch=p BATCH +x{i} chathistory #c")).unwrap(),
            );
        }
        assert_eq!(app.batch_parents.len(), 4096);
    }

    #[test]
    fn out_of_range_persisted_settings_self_heal_on_load() {
        let app = Session::with_settings(Settings {
            max_scrollback: 0,
            reconnect_delay_secs: 0,
            max_reconnect_attempts: 0,
            auto_away_minutes: 0,
            logging_history_lines: 0,
            ..Settings::default()
        });
        assert_eq!(app.max_scrollback, 100);
        assert_eq!(app.reconnect_delay_secs, 1);
        assert_eq!(app.max_reconnect_attempts, 1);
        assert_eq!(app.auto_away_minutes, 1);
        assert_eq!(app.logging_history_lines, 10);
    }

    #[test]
    fn ctcp_source_and_clientinfo_advertise_supported_metadata() {
        let (mut app, mut rx) = test_app_connected();
        assert!(app.handle_ctcp_request("peer", "\x01SOURCE\x01"));
        assert!(matches!(
            rx.try_recv(),
            Ok(IrcCommand::Notice(target, reply))
                if target == "peer"
                    && reply == format!("\x01SOURCE {}\x01", PROJECT_SOURCE_URL)
        ));

        assert!(app.handle_ctcp_request("peer", "\x01CLIENTINFO\x01"));
        assert!(matches!(
            rx.try_recv(),
            Ok(IrcCommand::Notice(target, reply))
                if target == "peer" && reply.contains("SOURCE")
        ));
    }

    #[test]
    fn default_nickname_suffix_survives_a_pre_1970_clock() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(suffix_from(UNIX_EPOCH + Duration::from_millis(12345)), 2345);
        assert_eq!(suffix_from(UNIX_EPOCH - Duration::from_secs(1)), 0);
        assert_eq!(
            suffix_from(UNIX_EPOCH - Duration::from_secs(1_000_000_000)),
            0
        );
        assert!(rand_suffix() < 10000);
    }

    #[test]
    fn ctcp_time_reply_is_a_calendar_date() {
        let (mut app, mut rx) = test_app_connected();
        assert!(app.handle_ctcp_request("peer", "\x01TIME\x01"));
        let Ok(IrcCommand::Notice(target, reply)) = rx.try_recv() else {
            panic!("TIME must be answered with a NOTICE");
        };
        assert_eq!(target, "peer");
        let body = reply
            .strip_prefix("\x01TIME ")
            .and_then(|body| body.strip_suffix('\x01'))
            .expect("framed reply");
        let bytes = body.as_bytes();
        assert_eq!(bytes.len(), 19, "{body}");
        for (i, byte) in bytes.iter().enumerate() {
            match i {
                4 | 7 => assert_eq!(*byte, b'-', "{body}"),
                10 => assert_eq!(*byte, b' ', "{body}"),
                13 | 16 => assert_eq!(*byte, b':', "{body}"),
                _ => assert!(byte.is_ascii_digit(), "{body}"),
            }
        }
    }

    #[test]
    fn echoed_own_ctcp_request_is_not_answered() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".into(), true);

        // echo-message (or a bouncer) relays our own `/ctcp bob VERSION`.
        app.handle_incoming_message(
            IrcMessage::parse(":me!u@h PRIVMSG bob :\x01VERSION\x01").unwrap(),
        );

        assert!(rx.try_recv().is_err(), "must not reply to ourselves");
        assert!(app.server_messages.is_empty(), "no CTCP request notice");
        assert!(app.conversations.is_empty(), "no query tab for the echo");

        // A peer's request is still answered.
        app.handle_incoming_message(
            IrcMessage::parse(":peer!u@h PRIVMSG me :\x01VERSION\x01").unwrap(),
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(IrcCommand::Notice(target, _)) if target == "peer"
        ));
    }

    #[test]
    fn ctcp_request_floods_are_answered_within_a_bounded_budget() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        // Fifty channel-wide requests, from one flooder and from fifty nicks.
        for i in 0..50 {
            app.handle_incoming_message(
                IrcMessage::parse(":flood!u@h PRIVMSG #chan :\x01PING 12345\x01").unwrap(),
            );
            app.handle_incoming_message(
                IrcMessage::parse(&format!(":n{i}!u@h PRIVMSG #chan :\x01VERSION\x01")).unwrap(),
            );
        }

        let replies = drain_commands(&mut rx)
            .into_iter()
            .filter(|command| matches!(command, IrcCommand::Notice(..)))
            .count();
        assert!(
            (1..=types::CTCP_REPLY_BURST).contains(&replies),
            "{replies} CTCP replies were sent"
        );
        // The focused tab is told once that replies are being held back, not
        // once per request.
        let rows = app.conversations["#chan"]
            .messages
            .iter()
            .filter(|message| message.content.starts_with("[CTCP]"))
            .count();
        assert_eq!(rows, replies + 1);
        assert!(
            app.conversations["#chan"]
                .messages
                .iter()
                .any(|message| message.content.contains("rate limit"))
        );
    }

    #[test]
    fn a_new_connection_gets_a_fresh_ctcp_reply_budget() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        for _ in 0..=types::CTCP_REPLY_BURST {
            app.handle_incoming_message(
                IrcMessage::parse(":peer!u@h PRIVMSG me :\x01VERSION\x01").unwrap(),
            );
        }
        assert_eq!(drain_commands(&mut rx).len(), types::CTCP_REPLY_BURST);

        app.begin_connection_attempt();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        app.handle_incoming_message(
            IrcMessage::parse(":peer!u@h PRIVMSG me :\x01VERSION\x01").unwrap(),
        );
        assert_eq!(drain_commands(&mut rx).len(), 1);
    }

    fn topic_test_channel(app: &mut Session) {
        let mut channel = Channel::new();
        channel.joined = true;
        channel.topic = Some("old topic".to_string());
        channel.topic_set_by = Some("olduser".to_string());
        channel.topic_set_time = Some(1);
        app.conversations.insert("#chan".to_string(), channel);
    }

    #[test]
    fn live_topic_change_updates_setter_and_time() {
        let mut app = test_app();
        topic_test_channel(&mut app);

        app.handle_incoming_message(
            IrcMessage::parse("@time=2026-10-25T12:00:00.000Z :alice!u@h TOPIC #chan :new topic")
                .unwrap(),
        );
        let ch = &app.conversations["#chan"];
        assert_eq!(ch.topic.as_deref(), Some("new topic"));
        assert_eq!(ch.topic_set_by.as_deref(), Some("alice"));
        assert_eq!(ch.topic_set_time, Some(1792929600));

        // Without server-time the change is stamped with the current time.
        app.handle_incoming_message(
            IrcMessage::parse(":bob!u@h TOPIC #chan :newer topic").unwrap(),
        );
        let ch = &app.conversations["#chan"];
        assert_eq!(ch.topic_set_by.as_deref(), Some("bob"));
        assert!(ch.topic_set_time.unwrap() > 1792929600);
    }

    #[test]
    fn empty_live_topic_clears_the_topic() {
        let mut app = test_app();
        topic_test_channel(&mut app);

        app.handle_incoming_message(IrcMessage::parse(":alice!u@h TOPIC #chan :").unwrap());

        let ch = &app.conversations["#chan"];
        assert_eq!(ch.topic, None);
        assert_eq!(ch.topic_set_by, None);
        assert_eq!(ch.topic_set_time, None);
        let last = ch.messages.back().unwrap();
        assert!(
            last.content.contains("cleared the topic"),
            "{}",
            last.content
        );
    }

    #[test]
    fn rpl_notopic_clears_a_stale_topic() {
        let mut app = test_app();
        topic_test_channel(&mut app);

        app.handle_numeric(
            RPL_NOTOPIC,
            &["me".into(), "#chan".into(), "No topic is set".into()],
        );

        let ch = &app.conversations["#chan"];
        assert_eq!(ch.topic, None);
        assert_eq!(ch.topic_set_by, None);
        assert_eq!(ch.topic_set_time, None);
    }

    #[test]
    fn rpl_topic_replaces_the_setter_and_ignores_empty_topics() {
        let mut app = test_app();
        topic_test_channel(&mut app);

        // 332 starts a fresh topic: the old setter must not linger if the
        // server omits 333.
        app.handle_numeric(RPL_TOPIC, &["me".into(), "#chan".into(), "fresh".into()]);
        let ch = &app.conversations["#chan"];
        assert_eq!(ch.topic.as_deref(), Some("fresh"));
        assert_eq!(ch.topic_set_by, None);
        assert_eq!(ch.topic_set_time, None);

        app.handle_numeric(
            RPL_TOPICWHOTIME,
            &[
                "me".into(),
                "#chan".into(),
                "carol".into(),
                "1700000000".into(),
            ],
        );
        let ch = &app.conversations["#chan"];
        assert_eq!(ch.topic_set_by.as_deref(), Some("carol"));
        assert_eq!(ch.topic_set_time, Some(1700000000));

        // An empty 332 is no topic, not an empty "Topic:" row.
        app.handle_numeric(RPL_TOPIC, &["me".into(), "#chan".into(), "".into()]);
        assert_eq!(app.conversations["#chan"].topic, None);
    }

    #[test]
    fn isupport_updates_channel_types() {
        let mut app = test_app();
        app.handle_numeric(
            RPL_ISUPPORT,
            &[
                "nick".to_string(),
                "CHANTYPES=#&+!".to_string(),
                "PREFIX=(qaohv)~&@%+".to_string(),
                "CASEMAPPING=strict-rfc1459".to_string(),
                "are supported".to_string(),
            ],
        );
        assert!(app.is_channel_name("+ops"));
        assert!(app.is_channel_name("!safe"));
        assert!(!app.is_channel_name("nick"));
        assert_eq!(app.network_support.case_mapping, CaseMapping::StrictRfc1459);
        assert!(app.identifiers_equal("[Nick]", "{nick}"));
        assert!(!app.identifiers_equal("Nick^", "nick~"));
    }

    #[test]
    fn isupport_trailing_text_is_not_a_token() {
        let mut app = test_app();
        // The final 005 parameter is the trailing explanation text; on the
        // wire it is one parameter containing spaces, never a token.
        app.handle_numeric(
            RPL_ISUPPORT,
            &[
                "nick".to_string(),
                "CHANTYPES=#".to_string(),
                "are supported by this server".to_string(),
            ],
        );
        assert_eq!(app.network_support.isupport.len(), 1);
        assert!(app.network_support.has_isupport("CHANTYPES"));

        // A 005 carrying no real token leaves ISUPPORT unknown, so early
        // /perform commands are still sent rather than reported unsupported.
        let mut app = test_app();
        app.handle_numeric(
            RPL_ISUPPORT,
            &[
                "nick".to_string(),
                "are supported by this server".to_string(),
            ],
        );
        assert!(app.network_support.isupport.is_empty());
    }

    #[test]
    fn extended_isupport_tracks_common_ircd_features_and_removals() {
        let mut app = test_app();
        app.handle_numeric(
            RPL_ISUPPORT,
            &[
                "nick".into(),
                "MONITOR=100".into(),
                "WATCH=128".into(),
                "WHOX".into(),
                "ELIST=CMNTU".into(),
                "SAFELIST".into(),
                "LINELEN=2048".into(),
                "CLIENTTAGDENY=*,-draft/reply".into(),
                "EXCEPTS=e".into(),
                "INVEX=I".into(),
                "EXTBAN=$,acjors".into(),
                "NETWORK=ExampleNet".into(),
                "are supported".into(),
            ],
        );
        assert_eq!(app.network_support.monitor_limit, Some(100));
        assert_eq!(app.network_support.watch_limit, Some(128));
        assert!(app.network_support.whox);
        assert!(app.network_support.safelist);
        assert_eq!(app.network_support.line_len, Some(2048));
        assert_eq!(app.network_support.except_mode, Some('e'));
        assert_eq!(app.network_support.invex_mode, Some('I'));
        assert_eq!(
            app.network_support.network_name.as_deref(),
            Some("ExampleNet")
        );

        app.handle_numeric(
            RPL_ISUPPORT,
            &["nick".into(), "-WATCH".into(), "are supported".into()],
        );
        assert!(!app.network_support.has_isupport("WATCH"));
        assert_eq!(app.network_support.watch_limit, None);
        assert_eq!(app.network_support.monitor_limit, Some(100));
    }

    #[test]
    fn utf8mapping_applies_precis_after_a_channel_sigil() {
        let support = NetworkSupport {
            case_mapping: CaseMapping::Utf8Rfc8265,
            ..NetworkSupport::default()
        };
        assert_eq!(support.canonicalize("#CAFÉ"), support.canonicalize("#café"));
        assert!(support.identifiers_equal("Élodie", "élodie"));
    }

    #[test]
    fn dynamic_prefix_and_statusmsg_routing_follow_isupport() {
        let mut app = test_app();
        app.handle_numeric(
            RPL_ISUPPORT,
            &[
                "nick".into(),
                "CHANTYPES=#&+".into(),
                "PREFIX=(Yov)!@+".into(),
                "STATUSMSG=@+".into(),
                "are supported".into(),
            ],
        );
        let mut channel = Channel::new();
        channel.set_prefix_schema(
            &app.network_support.prefix_modes,
            &app.network_support.user_prefixes,
        );
        app.conversations.insert("#room".into(), channel);
        app.handle_numeric(
            RPL_NAMREPLY,
            &["nick".into(), "=".into(), "#room".into(), "!@Alice".into()],
        );
        app.handle_numeric(RPL_ENDOFNAMES, &["nick".into(), "#room".into()]);

        assert_eq!(
            app.conversations["#room"]
                .get_user("alice")
                .unwrap()
                .prefix(),
            "!"
        );
        app.handle_mode_message("oper", "#room", Some("-Y"), &["alice".into()]);
        assert_eq!(
            app.conversations["#room"]
                .get_user("alice")
                .unwrap()
                .prefix(),
            "@"
        );
        assert_eq!(app.strip_status_prefix("@+#room"), "#room");
        assert_eq!(app.strip_status_prefix("+local"), "+local");
    }

    #[test]
    fn common_channel_mode_lists_are_structured_and_updated_live() {
        let mut app = test_app();
        app.conversations.insert("#room".into(), Channel::new());
        app.handle_numeric(
            RPL_EXCEPTLIST,
            &[
                "me".into(),
                "#room".into(),
                "*!*@trusted.example".into(),
                "oper".into(),
                "123".into(),
            ],
        );
        app.handle_numeric(RPL_ENDOFEXCEPTLIST, &["me".into(), "#room".into()]);
        let exceptions = &app.conversations["#room"].mode_lists[&'e'];
        assert_eq!(exceptions[0].mask, "*!*@trusted.example");
        assert!(
            app.conversations["#room"]
                .mode_lists_complete
                .contains(&'e')
        );

        app.handle_mode_message(
            "oper",
            "#room",
            Some("+I-b"),
            &["account:*".into(), "*!*@bad.example".into()],
        );
        assert_eq!(
            app.conversations["#room"].mode_lists[&'I'][0].mask,
            "account:*"
        );
    }

    #[test]
    fn ban_list_entries_without_setter_or_time_are_kept() {
        let mut app = test_app();
        app.conversations.insert("#room".into(), Channel::new());
        app.handle_numeric(
            RPL_BANLIST,
            &["me".into(), "#room".into(), "*!*@bad.host".into()],
        );
        app.handle_numeric(
            RPL_BANLIST,
            &[
                "me".into(),
                "#room".into(),
                "*!*@worse.host".into(),
                "op".into(),
            ],
        );
        app.handle_numeric(RPL_ENDOFBANLIST, &["me".into(), "#room".into()]);

        let channel = &app.conversations["#room"];
        let bans: Vec<_> = channel
            .bans
            .iter()
            .map(|ban| (ban.mask.as_str(), ban.set_by.as_str(), ban.set_time))
            .collect();
        assert_eq!(bans, [("*!*@bad.host", "", 0), ("*!*@worse.host", "op", 0)]);
        assert_eq!(channel.mode_lists[&'b'].len(), 2);
        assert!(channel.ban_list_complete);
        assert!(channel.mode_lists_complete.contains(&'b'));
    }

    #[test]
    fn relisting_a_mode_list_drops_entries_the_server_no_longer_has() {
        // (list reply, end-of-list reply, list mode)
        let lists = [
            (RPL_BANLIST, RPL_ENDOFBANLIST, 'b'),
            (RPL_INVITELIST, RPL_ENDOFINVITELIST, 'I'),
            (RPL_EXCEPTLIST, RPL_ENDOFEXCEPTLIST, 'e'),
            (RPL_QUIETLIST, RPL_ENDOFQUIETLIST, 'q'),
        ];
        for (entry, end, mode) in lists {
            let mut app = test_app();
            app.conversations.insert("#room".into(), Channel::new());
            let list = |app: &Session| -> Vec<String> {
                let channel = &app.conversations["#room"];
                let masks: Vec<String> = channel
                    .mode_lists
                    .get(&mode)
                    .map(|entries| entries.iter().map(|e| e.mask.clone()).collect())
                    .unwrap_or_default();
                if mode == 'b' {
                    let bans: Vec<String> = channel.bans.iter().map(|b| b.mask.clone()).collect();
                    assert_eq!(bans, masks, "+{mode} mirror");
                }
                masks
            };
            let burst = |app: &mut Session, masks: &[&str]| {
                for mask in masks {
                    app.handle_numeric(
                        entry,
                        &[
                            "me".into(),
                            "#room".into(),
                            (*mask).into(),
                            "op".into(),
                            "100".into(),
                        ],
                    );
                }
                app.handle_numeric(end, &["me".into(), "#room".into()]);
            };

            burst(&mut app, &["a!*@*", "b!*@*"]);
            assert_eq!(list(&app), ["a!*@*", "b!*@*"], "+{mode}");

            // Another op removed b while we were away; the fresh listing is
            // the whole truth, not an addition to what we remembered.
            burst(&mut app, &["a!*@*"]);
            assert_eq!(list(&app), ["a!*@*"], "+{mode}");

            // An emptied list sends only the end marker.
            burst(&mut app, &[]);
            assert!(list(&app).is_empty(), "+{mode}");
            assert!(
                app.conversations["#room"]
                    .mode_lists_complete
                    .contains(&mode)
            );

            // A listing cut off by a disconnect is not continued by the next.
            burst(&mut app, &["a!*@*"]);
            app.handle_numeric(
                entry,
                &[
                    "me".into(),
                    "#room".into(),
                    "stale!*@*".into(),
                    "op".into(),
                    "100".into(),
                ],
            );
            app.mark_connection_lost();
            burst(&mut app, &["c!*@*"]);
            assert_eq!(list(&app), ["c!*@*"], "+{mode}");
        }
    }

    #[test]
    fn quiet_lists_with_a_mode_letter_field_are_replaced_and_completed_per_mode() {
        // Solanum (Libera) names the list in both the entries and the end
        // marker: `728 me #chan q mask setter ts` / `729 me #chan q :End ...`.
        let mut app = test_app();
        app.handle_incoming_message(
            IrcMessage::parse(":srv 005 me CHANMODES=eIbqZ,k,fl,imnt :are supported").unwrap(),
        );
        app.conversations.insert("#chan".into(), Channel::new());
        let masks = |app: &Session, mode: char| -> Vec<String> {
            app.conversations["#chan"]
                .mode_lists
                .get(&mode)
                .map(|entries| entries.iter().map(|entry| entry.mask.clone()).collect())
                .unwrap_or_default()
        };
        let burst = |app: &mut Session, mode: &str, entries: &[&str]| {
            for mask in entries {
                app.handle_incoming_message(
                    IrcMessage::parse(&format!(":srv 728 me #chan {mode} {mask} op 100")).unwrap(),
                );
            }
            app.handle_incoming_message(
                IrcMessage::parse(&format!(":srv 729 me #chan {mode} :End of Channel List"))
                    .unwrap(),
            );
        };

        burst(&mut app, "q", &["a!*@*", "b!*@*"]);
        burst(&mut app, "Z", &["z!*@*"]);
        assert_eq!(masks(&app, 'q'), ["a!*@*", "b!*@*"]);
        assert_eq!(masks(&app, 'Z'), ["z!*@*"]);
        assert!(
            app.conversations["#chan"]
                .mode_lists_complete
                .contains(&'q')
        );
        assert!(
            app.conversations["#chan"]
                .mode_lists_complete
                .contains(&'Z')
        );

        // The end marker's mode letter decides which list it closes: a Z
        // listing in progress is not completed by (or merged into) q's.
        app.handle_incoming_message(IrcMessage::parse(":srv 728 me #chan Z y!*@* op 100").unwrap());
        burst(&mut app, "q", &["a!*@*"]);
        assert_eq!(masks(&app, 'q'), ["a!*@*"]);
        assert_eq!(masks(&app, 'Z'), ["y!*@*"]);
        app.handle_incoming_message(
            IrcMessage::parse(":srv 729 me #chan Z :End of Channel List").unwrap(),
        );
        burst(&mut app, "Z", &["w!*@*"]);
        assert_eq!(masks(&app, 'Z'), ["w!*@*"]);
        assert_eq!(masks(&app, 'q'), ["a!*@*"]);

        // Either list can be emptied on its own.
        burst(&mut app, "q", &[]);
        assert!(masks(&app, 'q').is_empty());
        assert_eq!(masks(&app, 'Z'), ["w!*@*"]);
    }

    #[test]
    fn custom_except_and_invite_letters_are_replaced_on_relisting() {
        let mut app = test_app();
        app.handle_incoming_message(
            IrcMessage::parse(":srv 005 me EXCEPTS=x INVEX=y :are supported").unwrap(),
        );
        app.conversations.insert("#chan".into(), Channel::new());
        let masks = |app: &Session, mode: char| -> Vec<String> {
            app.conversations["#chan"]
                .mode_lists
                .get(&mode)
                .map(|entries| entries.iter().map(|entry| entry.mask.clone()).collect())
                .unwrap_or_default()
        };
        for (entry, end, mode) in [
            (RPL_EXCEPTLIST, RPL_ENDOFEXCEPTLIST, 'x'),
            (RPL_INVITELIST, RPL_ENDOFINVITELIST, 'y'),
        ] {
            let mut burst = |entries: &[&str]| {
                for mask in entries {
                    app.handle_numeric(
                        entry,
                        &[
                            "me".into(),
                            "#chan".into(),
                            (*mask).into(),
                            "op".into(),
                            "100".into(),
                        ],
                    );
                }
                app.handle_numeric(end, &["me".into(), "#chan".into()]);
            };
            burst(&["a!*@*", "b!*@*"]);
            burst(&["a!*@*"]);
            assert_eq!(masks(&app, mode), ["a!*@*"], "+{mode}");
            assert!(
                app.conversations["#chan"]
                    .mode_lists_complete
                    .contains(&mode),
                "+{mode}"
            );
        }
        // Nothing leaked into the default letters.
        assert!(masks(&app, 'e').is_empty());
        assert!(masks(&app, 'I').is_empty());
    }

    #[test]
    fn casemapped_incoming_query_reuses_existing_tab() {
        let mut app = test_app();
        app.set_session_nick("me".to_string(), true);
        app.add_message_to_channel("Alice", ChatMessage::system("opened"));

        let msg = IrcMessage::parse(":alice!u@h PRIVMSG me :hello").unwrap();
        app.handle_incoming_message(msg);

        assert_eq!(app.conversations.len(), 1);
        assert!(app.conversations.contains_key("Alice"));
        assert!(
            app.conversations["Alice"]
                .messages
                .iter()
                .any(|message| message.content == "hello")
        );
    }

    #[test]
    fn question_mark_only_ignore_pattern_matches_nick() {
        let mut app = test_app();
        app.ignore_list = vec!["a?ice".to_string()];
        app.update_cached_lowercase();

        assert!(app.is_ignored("alice", Some("alice!u@host")));
        assert!(app.is_ignored("axice", Some("axice!u@host")));
        assert!(!app.is_ignored("allice", Some("allice!u@host")));
        assert!(!app.is_ignored("bob", Some("bob!u@host")));
    }

    #[test]
    fn special_character_nicks_highlight_with_identifier_boundaries() {
        let mut app = test_app();
        app.set_session_nick("foo-bar".to_string(), true);
        assert!(app.check_nick_mention("hello foo-bar!"));
        assert!(app.check_nick_mention("(FOO-BAR), ping"));
        assert!(!app.check_nick_mention("xfoo-bar"));
        assert!(!app.check_nick_mention("foo-barry"));

        app.set_session_nick("[alice]".to_string(), true);
        assert!(app.check_nick_mention("hi {ALICE},"));
        assert!(!app.check_nick_mention("x[alice]"));
    }

    #[test]
    fn highlight_words_match_with_punctuation_at_identifier_boundaries() {
        let mut app = test_app();
        app.highlight_words = "foo-bar, node.js".to_string();
        app.update_cached_lowercase();

        // Punctuation-bearing words (the dialog hint suggests alt nicks like
        // "your-other-nick") must fire; token-equality matching silently
        // dropped them.
        assert!(app.check_nick_mention("foo-bar: are you there"));
        assert!(app.check_nick_mention("pinging FOO-BAR again"));
        assert!(app.check_nick_mention("the node.js bot said hi"));
        assert!(!app.check_nick_mention("xfoo-bar"));
        assert!(!app.check_nick_mention("node.jss"));
        assert!(!app.check_nick_mention("no highlights here"));
    }

    #[test]
    fn rpl_list_min_users_filter_applies_to_every_channel_prefix() {
        let rows = [
            ("#small", "1"),
            ("!ABCDEsafe", "1"),
            ("+modeless", "2"),
            ("#big", "50"),
            ("!ABCDEbig", "50"),
            ("+bigmodeless", "12"),
        ];
        for (min_users, expected) in [
            (10, vec!["#big", "!ABCDEbig", "+bigmodeless"]),
            (
                0,
                vec![
                    "#small",
                    "!ABCDEsafe",
                    "+modeless",
                    "#big",
                    "!ABCDEbig",
                    "+bigmodeless",
                ],
            ),
        ] {
            let mut app = test_app();
            app.channel_list_min_users = min_users;
            for (name, users) in rows {
                app.handle_numeric(
                    RPL_LIST,
                    &["me".into(), name.into(), users.into(), "topic".into()],
                );
            }
            let names: Vec<_> = app.channel_list.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, expected, "min users {min_users}");
        }
    }

    #[test]
    fn rpl_list_keeps_numeric_topics_from_becoming_user_counts() {
        let mut app = test_app();

        // Standard: 322 <me> <#chan> <count> :<topic>
        app.handle_numeric(
            RPL_LIST,
            &[
                "me".to_string(),
                "#general".to_string(),
                "12".to_string(),
                "2024".to_string(),
            ],
        );
        let entry = &app.channel_list[0];
        assert_eq!(entry.name, "#general");
        assert_eq!(entry.user_count, 12, "topic must not overwrite the count");
        assert_eq!(entry.topic_clean, "2024", "numeric topic must be kept");

        // Ordinary topic still lands.
        app.handle_numeric(
            RPL_LIST,
            &[
                "me".to_string(),
                "#other".to_string(),
                "5".to_string(),
                "hello world".to_string(),
            ],
        );
        assert_eq!(app.channel_list[1].user_count, 5);
        assert_eq!(app.channel_list[1].topic_clean, "hello world");
    }

    #[test]
    fn names_numeric_atomically_replaces_stale_users_and_reads_multi_prefix() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("alice", UserMode::Normal);
        channel.add_user("stale", UserMode::Op);
        app.conversations.insert("#chan".to_string(), channel);

        app.handle_numeric(
            RPL_NAMREPLY,
            &[
                "me".to_string(),
                "=".to_string(),
                "#CHAN".to_string(),
                "@+Alice bob".to_string(),
            ],
        );
        // The visible list is unchanged until the authoritative burst ends.
        assert!(app.conversations["#chan"].has_user("stale"));
        app.handle_numeric(
            RPL_ENDOFNAMES,
            &["me".to_string(), "#CHAN".to_string(), "end".to_string()],
        );

        let channel = &app.conversations["#chan"];
        assert!(!channel.has_user("stale"));
        let alice = channel.get_user("alice").unwrap();
        assert!(alice.has_mode(UserMode::Op));
        assert!(alice.has_mode(UserMode::Voice));
        assert_eq!(alice.mode, UserMode::Op);
    }

    #[test]
    fn userhost_in_names_keeps_only_the_membership_nickname() {
        let mut app = test_app();
        app.conversations.insert("#chan".into(), Channel::new());
        app.handle_numeric(
            RPL_NAMREPLY,
            &[
                "me".into(),
                "=".into(),
                "#chan".into(),
                "@+Alice!user@example.test bob!ident@host".into(),
            ],
        );
        app.handle_numeric(RPL_ENDOFNAMES, &["me".into(), "#chan".into()]);

        let channel = &app.conversations["#chan"];
        assert!(channel.has_user("Alice"));
        assert!(channel.has_user("bob"));
        assert!(!channel.has_user("Alice!user@example.test"));
    }

    #[test]
    fn extended_join_and_setname_retain_member_identity() {
        let mut app = test_app();
        app.set_session_nick("me".into(), true);
        app.conversations.insert("#room".into(), Channel::new());

        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h JOIN #room alice-account :Alice Example").unwrap(),
        );
        let alice = app.conversations["#room"].get_user("alice").unwrap();
        assert_eq!(alice.account.as_deref(), Some("alice-account"));
        assert_eq!(alice.realname.as_deref(), Some("Alice Example"));

        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h SETNAME :Alice Updated").unwrap(),
        );
        assert_eq!(
            app.conversations["#room"]
                .get_user("alice")
                .unwrap()
                .realname
                .as_deref(),
            Some("Alice Updated")
        );
    }

    #[test]
    fn private_reaction_routes_to_the_senders_query() {
        let mut app = test_app();
        app.set_session_nick("me".into(), true);
        app.add_message_to_channel(
            "alice",
            ChatMessage::new_fmt("alice", "parent", "short").with_irc_metadata(
                None,
                Some("abc".into()),
                None,
            ),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@+draft/react=👍;+reply=abc :alice!u@h TAGMSG me").unwrap(),
        );

        assert!(app.conversations.contains_key("alice"));
        assert!(!app.conversations.contains_key("me"));
        assert_eq!(app.conversations["alice"].messages.len(), 1);
        assert_eq!(
            app.conversations["alice"].messages[0].reactions[0].emoji,
            "👍"
        );
        assert_eq!(
            app.conversations["alice"].messages[0].reactions[0].actors,
            ["alice"]
        );
    }

    #[test]
    fn no_implicit_names_requests_membership_after_join() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :no-implicit-names").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Names(Some("#room".into()))
        );
    }

    #[test]
    fn echo_message_enriches_the_local_row_without_duplication() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.handle_incoming_message(IrcMessage::parse(":srv CAP me ACK :echo-message").unwrap());
        app.add_message_to_channel(
            "#room",
            ChatMessage::new_fmt("me", "hello", "short").with_pending_echo(),
        );

        app.handle_incoming_message(
            IrcMessage::parse(
                "@time=2026-09-12T12:00:00.000Z;msgid=abc;account=acct;+reply=prior :me!u@h PRIVMSG #room :hello",
            )
            .unwrap(),
        );

        let channel = &app.conversations["#room"];
        assert_eq!(channel.messages.len(), 1);
        assert_eq!(channel.messages[0].msgid.as_deref(), Some("abc"));
        assert_eq!(channel.messages[0].account.as_deref(), Some("acct"));
        assert_eq!(channel.messages[0].reply_to.as_deref(), Some("prior"));
    }

    #[test]
    fn selected_live_message_advances_the_server_read_marker() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.conversations.insert("#room".into(), Channel::new());
        app.select_target("#room");
        // The active viewport has reported that it is reading the buffer tail.
        app.reading_conversation = app.current_conversation;
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/read-marker").unwrap(),
        );

        app.handle_incoming_message(
            IrcMessage::parse(
                "@time=2026-09-12T12:00:00.000Z;msgid=abc :alice!u@h PRIVMSG #room :hello",
            )
            .unwrap(),
        );

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Markread(
                "#room".into(),
                Some("timestamp=2026-09-12T12:00:00.000Z".into())
            )
        );
    }

    #[test]
    fn standard_reply_uses_its_channel_context() {
        let mut app = test_app();
        app.conversations.insert("#room".into(), Channel::new());
        app.handle_incoming_message(
            IrcMessage::parse(":srv FAIL JOIN NO_SUCH_CHANNEL #room :No such channel").unwrap(),
        );
        assert!(
            app.conversations["#room"]
                .messages
                .iter()
                .any(|message| message.content.contains("NO_SUCH_CHANNEL"))
        );
        assert!(app.server_messages.is_empty());
    }

    #[test]
    fn live_redaction_removes_the_identified_message() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.messages.push_back(
            ChatMessage::new_fmt("alice", "remove me", "short").with_irc_metadata(
                None,
                Some("gone".into()),
                None,
            ),
        );
        app.conversations.insert("#room".into(), channel);

        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h REDACT #room gone :cleanup").unwrap(),
        );

        assert!(
            app.conversations["#room"]
                .messages
                .iter()
                .all(|message| message.msgid.as_deref() != Some("gone"))
        );
        assert!(
            app.conversations["#room"]
                .messages
                .iter()
                .any(|message| message.content == "A message was redacted: cleanup")
        );
    }

    fn redacted_reference(app: &Session) -> Option<identity::MessageRef> {
        app.events.iter().find_map(|event| match event {
            SessionEvent::MessageRedacted(reference) => Some(reference.clone()),
            _ => None,
        })
    }

    fn session_with_redactable_message(tab: &str) -> Session {
        let mut app = test_app();
        app.set_session_nick("me".to_string(), true);
        let mut channel = Channel::new();
        channel.messages.push_back(
            ChatMessage::new_fmt("alice", "remove me", "short").with_irc_metadata(
                None,
                Some("gone".into()),
                None,
            ),
        );
        app.conversations.insert(tab.into(), channel);
        app
    }

    #[test]
    fn peer_redacting_a_dm_removes_it_from_the_query_and_archive_key() {
        for line in [
            // The server relays a peer's DM redaction with our nick as target.
            ":alice!u@h REDACT me gone :oops",
            // Casemapped nick and target spelling.
            ":Alice!u@h REDACT ME gone :oops",
            // Our own echo already names the peer.
            ":me!u@h REDACT alice gone :oops",
        ] {
            let mut app = session_with_redactable_message("alice");
            app.handle_incoming_message(IrcMessage::parse(line).unwrap());

            let messages = &app.conversations["alice"].messages;
            assert!(
                messages.iter().all(|m| m.msgid.as_deref() != Some("gone")),
                "{line}"
            );
            assert!(
                messages
                    .iter()
                    .any(|m| m.content == "A message was redacted: oops"),
                "{line}"
            );
            assert!(!app.conversations.contains_key("me"), "{line}");
            let reference = redacted_reference(&app).expect(line);
            assert_eq!(
                reference.conversation,
                app.conversation_key("alice"),
                "{line}"
            );
            assert!(reference.archive_id.is_some(), "{line}");
        }

        // A closed tab still records the redaction under the peer.
        let mut app = test_app();
        app.set_session_nick("me".to_string(), true);
        app.handle_incoming_message(IrcMessage::parse(":alice!u@h REDACT me gone").unwrap());
        let reference = redacted_reference(&app).unwrap();
        assert_eq!(reference.conversation, app.conversation_key("alice"));
        assert!(!app.conversations.contains_key("me"));
    }

    #[test]
    fn channel_redaction_resolves_the_tab_through_casemapping() {
        let mut app = session_with_redactable_message("#room");
        app.handle_incoming_message(IrcMessage::parse(":alice!u@h REDACT #ROOM gone").unwrap());
        assert!(redacted_reference(&app).unwrap().archive_id.is_some());
        assert!(
            app.conversations["#room"]
                .messages
                .iter()
                .all(|m| m.msgid.as_deref() != Some("gone"))
        );
    }

    #[test]
    fn cap_new_requests_newly_available_nefarious_features() {
        let (mut app, mut rx) = test_app_connected();
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me NEW :draft/chathistory=20 draft/webpush=vapid-key")
                .unwrap(),
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Cap(
                String::new(),
                "REQ".into(),
                vec!["draft/chathistory draft/webpush".into()]
            )
        );
        assert_eq!(app.chathistory_limit, Some(20));
    }

    #[test]
    fn cap_new_splits_the_full_supported_set_into_valid_lines() {
        let (mut app, mut rx) = test_app_connected();
        app.handle_cap_message("NEW", &[DESIRED_CAPS.join(" ")]);

        let mut requested = Vec::new();
        while let Ok(command) = rx.try_recv() {
            assert!(Session::command_fits_irc_line(&command));
            let IrcCommand::Cap(_, subcommand, params) = command else {
                panic!("expected CAP REQ");
            };
            assert_eq!(subcommand, "REQ");
            requested.extend(params.join(" ").split_whitespace().map(str::to_string));
        }
        assert_eq!(requested, DESIRED_CAPS);
    }

    #[test]
    fn fatal_registration_error_clears_connecting() {
        let mut app = test_app();
        app.test_set_connecting_flag(true);
        app.handle_numeric(
            ERR_PASSWDMISMATCH,
            &["nick".to_string(), "Password incorrect".to_string()],
        );
        assert!(!app.is_connecting());
        assert!(!app.is_connected());
        assert!(!app.has_pending_connect());
        assert!(
            app.server_messages
                .back()
                .is_some_and(|m| m.content.contains("Registration failed"))
        );
    }

    #[test]
    fn incoming_mode_updates_user_mode() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("alice", UserMode::Normal);
        app.conversations.insert("#chan".to_string(), channel);

        app.handle_mode_message("oper", "#chan", Some("+o"), &["alice".to_string()]);

        let alice = app.conversations["#chan"].get_user("alice").unwrap();
        assert_eq!(alice.mode, UserMode::Op);
    }

    #[test]
    fn incoming_membership_modes_preserve_other_prefixes() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("alice", UserMode::Op);
        app.conversations.insert("#chan".to_string(), channel);

        app.handle_mode_message("oper", "#CHAN", Some("+v"), &["Alice".to_string()]);
        let alice = app.conversations["#chan"].get_user("alice").unwrap();
        assert_eq!(alice.mode, UserMode::Op);
        assert!(alice.has_mode(UserMode::Voice));

        app.handle_mode_message("oper", "#CHAN", Some("-v"), &["Alice".to_string()]);
        let alice = app.conversations["#chan"].get_user("alice").unwrap();
        assert_eq!(alice.mode, UserMode::Op);
        assert!(!alice.has_mode(UserMode::Voice));
    }

    #[test]
    fn live_channel_modes_update_parameter_values() {
        let mut app = test_app();
        app.conversations
            .insert("#chan".to_string(), Channel::new());
        app.handle_numeric(
            RPL_CHANNELMODEIS,
            &[
                "me".to_string(),
                "#CHAN".to_string(),
                "+ntk".to_string(),
                "oldkey".to_string(),
            ],
        );

        app.handle_mode_message(
            "oper",
            "#CHAN",
            Some("-k+l"),
            &["oldkey".into(), "50".into()],
        );
        let channel = &app.conversations["#chan"];
        assert_eq!(channel.modes, "+ntl");
        assert_eq!(channel.mode_params, ["50"]);

        app.handle_mode_message("oper", "#chan", Some("-l"), &[]);
        let channel = &app.conversations["#chan"];
        assert_eq!(channel.modes, "+nt");
        assert!(channel.mode_params.is_empty());
    }

    #[test]
    fn mode_params_align_with_isupport_chanmodes() {
        let mut app = test_app();
        // InspIRCd-style network: +f (flood limit) is a type-C mode that takes
        // a parameter when set; PREFIX only has o/v.
        app.handle_numeric(
            RPL_ISUPPORT,
            &[
                "nick".to_string(),
                "CHANMODES=eIbq,k,flj,imnpst".to_string(),
                "PREFIX=(ov)@+".to_string(),
                "are supported".to_string(),
            ],
        );
        let mut channel = Channel::new();
        channel.add_user("alice", UserMode::Normal);
        app.conversations.insert("#chan".to_string(), channel);

        // "+fo [4:5] alice": 'f' must consume "[4:5]" so 'o' reads "alice".
        app.handle_mode_message(
            "oper",
            "#chan",
            Some("+fo"),
            &["[4:5]".to_string(), "alice".to_string()],
        );
        let alice = app.conversations["#chan"].get_user("alice").unwrap();
        assert_eq!(alice.mode, UserMode::Op);

        // Libera-style +q is a quiet LIST mode here, not owner: consuming the
        // mask must not create or promote any user.
        app.handle_mode_message("oper", "#chan", Some("+q"), &["*!*@spam".to_string()]);
        assert!(!app.conversations["#chan"].has_user("*!*@spam"));
        assert_eq!(
            app.conversations["#chan"].get_user("alice").unwrap().mode,
            UserMode::Op
        );
    }

    #[test]
    fn incoming_kick_removes_user_and_notes_channel() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("bob", UserMode::Normal);
        app.conversations.insert("#chan".to_string(), channel);

        let msg = IrcMessage::parse(":alice!a@h KICK #chan bob :spamming").unwrap();
        app.handle_incoming_message(msg);

        let ch = &app.conversations["#chan"];
        assert!(!ch.has_user("bob"));
        assert!(ch.messages.iter().any(|m| {
            m.content
                .contains("bob was kicked from #chan by alice (spamming)")
        }));
    }

    #[test]
    fn incoming_self_kick_clears_users_but_keeps_tab() {
        let mut app = test_app();
        app.set_session_nick("me".to_string(), true);
        let mut channel = Channel::new();
        channel.add_user("me", UserMode::Normal);
        channel.add_user("bob", UserMode::Normal);
        app.conversations.insert("#chan".to_string(), channel);
        app.select_target("#chan");

        let msg = IrcMessage::parse(":alice!a@h KICK #chan me :bye").unwrap();
        app.handle_incoming_message(msg);

        let ch = &app.conversations["#chan"];
        assert!(ch.users.is_empty());
        assert!(ch.messages.iter().any(|m| {
            m.content
                .contains("You were kicked from #chan by alice (bye)")
        }));
    }

    #[test]
    fn nick_in_use_while_connected_reports_without_renaming() {
        let mut app = test_app();
        app.set_session_nick("mike".to_string(), true);
        app.test_set_connected(true);
        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &[
                "mike".to_string(),
                "john".to_string(),
                "Nickname is already in use".to_string(),
            ],
        );
        assert_eq!(app.my_nick, "mike");
        assert!(
            app.server_messages
                .back()
                .is_some_and(|m| m.content.contains("john"))
        );
    }

    #[test]
    fn nick_in_use_during_registration_retries_with_suffix() {
        let mut app = test_app();
        // Pre-registration: mike was configured but never confirmed here
        // (accepted=false — a state the connected test above cannot model).
        app.set_session_nick("mike".to_string(), false);
        app.test_set_connected(false);
        app.handle_numeric(
            ERR_NICKNAMEINUSE,
            &[
                "*".to_string(),
                "mike".to_string(),
                "Nickname is already in use".to_string(),
            ],
        );
        assert_eq!(app.my_nick, "mike_");
        assert!(
            !app.sender_is_self("mike"),
            "the walkaway must not record the configured nick (#166)"
        );
    }

    #[test]
    fn overlong_quit_reason_is_clamped_to_fit_one_line() {
        let clamped = Session::clamp_quit_reason(Some("é".repeat(600))).unwrap();
        let line = format!("{}", IrcCommand::Quit(Some(clamped.clone())));
        assert!(line.len() <= IRC_MAX_LINE_BYTES);
        assert!(clamped.is_char_boundary(clamped.len()));
        // Short reasons pass through untouched.
        assert_eq!(
            Session::clamp_quit_reason(Some("bye".to_string())).as_deref(),
            Some("bye")
        );
    }

    #[test]
    fn disconnect_command_stops_reconnect_loop_without_touching_setting() {
        let mut app = test_app();
        app.test_set_lost(true);
        app.auto_reconnect = true;
        app.test_set_failed_at(None);
        app.channel_list_loading = true;

        app.process_command("/disconnect");

        assert!(!app.connection_lost());
        assert!(!app.should_reconnect());
        assert!(!app.channel_list_loading);
        assert!(app.auto_reconnect, "persisted setting must not be flipped");
    }

    #[test]
    fn unexpected_disconnect_stops_channel_list_loading() {
        let mut app = test_app();
        app.test_set_connected(true);
        app.channel_list_loading = true;

        app.mark_connection_lost();

        assert!(!app.channel_list_loading);
        assert!(app.connection_lost());
    }

    #[test]
    fn awaiting_reconnect_covers_the_whole_backoff_window() {
        let mut app = test_app();
        app.auto_reconnect = true;
        assert!(!app.awaiting_reconnect(), "nothing pending before a loss");

        app.mark_connection_lost();
        // The backoff deadline has not passed yet (should_reconnect is still
        // false), but the event loop must already keep polling or the frame
        // would never re-run to observe it on an idle window.
        assert!(app.awaiting_reconnect());
        assert!(!app.should_reconnect());

        app.test_set_attempts(app.max_reconnect_attempts);
        assert!(!app.awaiting_reconnect(), "attempts exhausted");

        app.test_set_attempts(0);
        app.test_set_connecting_flag(true);
        assert!(!app.awaiting_reconnect(), "a retry is already in flight");

        app.test_set_connecting_flag(false);
        app.auto_reconnect = false;
        assert!(!app.awaiting_reconnect(), "setting disabled");
    }

    fn sent_privmsg_texts(rx: &mut mpsc::UnboundedReceiver<IrcCommand>) -> Vec<String> {
        let mut texts = Vec::new();
        while let Ok(command) = rx.try_recv() {
            let commands = match command {
                IrcCommand::ClientBatch(commands) => commands,
                other => vec![other],
            };
            texts.extend(commands.into_iter().filter_map(|command| match command {
                IrcCommand::Privmsg(_, text) => Some(text),
                _ => None,
            }));
        }
        texts
    }

    #[test]
    fn multi_line_msg_skips_blank_lines_and_refuses_a_flooding_paste() {
        let (mut app, mut rx) = test_app_connected();
        app.process_command("/msg alice first\n\nsecond\n");
        assert_eq!(sent_privmsg_texts(&mut rx), ["first", "second"]);

        // Each case uses a target with no open query, so it takes the
        // command path rather than the composer path (which #380 covers).
        let paste: Vec<String> = (0..=20).map(|i| format!("line {i}")).collect();
        app.process_command(&format!("/msg bob {}", paste.join("\n")));
        assert!(
            sent_privmsg_texts(&mut rx).is_empty(),
            "an unbatched paste over the line cap is refused"
        );
        assert!(
            app.server_messages
                .iter()
                .any(|message| message.content.contains("Too many lines")),
            "the refusal explains itself"
        );

        let few: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        app.process_command(&format!("/msg carol {}", few.join("\n")));
        assert_eq!(sent_privmsg_texts(&mut rx).len(), 20);
    }

    #[test]
    fn msg_with_doubled_space_still_reaches_target() {
        let (mut app, mut rx) = test_app_connected();
        app.process_command("/msg  alice hi");
        match rx.try_recv() {
            Ok(IrcCommand::Privmsg(target, content)) => {
                assert_eq!(target, "alice");
                assert_eq!(content, "hi");
            }
            other => panic!("expected Privmsg, got {:?}", other),
        }
        assert!(
            !app.conversations.contains_key(""),
            "no phantom empty-name tab"
        );
    }

    #[test]
    fn kickban_with_hostmask_bans_mask_and_skips_kick() {
        let (mut app, mut rx) = test_app_connected();
        app.conversations
            .insert("#chan".to_string(), Channel::new());
        app.select_target("#chan");

        app.process_command("/kb *!*@spam.example.com flooding");
        match rx.try_recv() {
            Ok(IrcCommand::Mode(chan, mode, params)) => {
                assert_eq!(chan, "#chan");
                assert_eq!(mode.as_deref(), Some("+b"));
                assert_eq!(params, vec!["*!*@spam.example.com".to_string()]);
            }
            other => panic!("expected Mode, got {:?}", other),
        }
        assert!(rx.try_recv().is_err(), "no KICK for a hostmask");

        // A bare nick still bans nick!*@* and kicks.
        app.process_command("/kb spammer flooding");
        match rx.try_recv() {
            Ok(IrcCommand::Mode(_, _, params)) => {
                assert_eq!(params, vec!["spammer!*@*".to_string()]);
            }
            other => panic!("expected Mode, got {:?}", other),
        }
        match rx.try_recv() {
            Ok(IrcCommand::Kick(chan, nick, reason)) => {
                assert_eq!(chan, "#chan");
                assert_eq!(nick, "spammer");
                assert_eq!(reason.as_deref(), Some("flooding"));
            }
            other => panic!("expected Kick, got {:?}", other),
        }
    }

    #[test]
    fn monitor_attached_sign_is_parsed() {
        let (mut app, mut rx) = test_app_connected();
        app.process_command("/monitor +friend");
        match rx.try_recv() {
            Ok(IrcCommand::Monitor(sub, targets)) => {
                assert_eq!(sub, "+");
                assert_eq!(targets.as_deref(), Some("friend"));
            }
            other => panic!("expected Monitor, got {:?}", other),
        }
    }

    #[test]
    fn bare_nick_presence_replies_keep_the_known_user_and_host() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.conversations
            .insert("#chan".to_string(), Channel::new());
        app.handle_incoming_message(IrcMessage::parse(":alice!user@host JOIN #chan").unwrap());
        let identity = |app: &Session| {
            let known = &app.known_users["alice"];
            let in_channel = app.conversations["#chan"].get_user("alice").unwrap();
            assert_eq!(known.username, in_channel.username);
            assert_eq!(known.hostname, in_channel.hostname);
            (known.username.clone(), known.hostname.clone(), known.online)
        };
        assert_eq!(
            identity(&app),
            (Some("user".to_string()), Some("host".to_string()), true)
        );

        // MONITOR may report a target as a bare nick; that teaches nothing
        // new about the user and host already learned.
        app.handle_incoming_message(IrcMessage::parse(":srv 731 me :alice").unwrap());
        assert_eq!(
            identity(&app),
            (Some("user".to_string()), Some("host".to_string()), false)
        );
        app.handle_incoming_message(IrcMessage::parse(":srv 730 me :alice").unwrap());
        assert_eq!(
            identity(&app),
            (Some("user".to_string()), Some("host".to_string()), true)
        );

        // A full mask still replaces what was known.
        app.handle_incoming_message(IrcMessage::parse(":srv 730 me :alice!new@place").unwrap());
        assert_eq!(
            identity(&app),
            (Some("new".to_string()), Some("place".to_string()), true)
        );

        // A JOIN whose prefix is a bare nick (some bouncers and servers omit
        // user@host) and a WATCH reply without them keep it as well.
        app.handle_incoming_message(IrcMessage::parse(":alice JOIN #chan").unwrap());
        app.handle_numeric(RPL_NOWON, &["me".into(), "alice".into()]);
        assert_eq!(
            identity(&app),
            (Some("new".to_string()), Some("place".to_string()), true)
        );
    }

    #[test]
    fn part_while_disconnected_closes_window_locally() {
        let mut app = test_app();
        app.conversations
            .insert("#stale".to_string(), Channel::new());
        app.select_target("#stale");

        app.process_command("/part");

        assert!(!app.conversations.contains_key("#stale"));
        assert_ne!(app.current_target_name(), Some("#stale"));
    }

    #[test]
    fn nick_change_rekeys_query_window() {
        let mut app = test_app();
        let mut query = Channel::new();
        query.push_trimmed(ChatMessage::system("old talk"), 100);
        app.conversations.insert("alice".to_string(), query);
        app.select_target("alice");

        let msg = IrcMessage::parse(":alice!u@h NICK :alice2").unwrap();
        app.handle_incoming_message(msg);

        assert!(!app.conversations.contains_key("alice"));
        let renamed = app.conversations.get("alice2").expect("window re-keyed");
        assert!(
            renamed
                .messages
                .iter()
                .any(|m| m.content.contains("old talk"))
        );
        assert!(
            renamed
                .messages
                .iter()
                .any(|m| m.content.contains("alice is now known as alice2"))
        );
        assert_eq!(app.current_target_name(), Some("alice2"));
    }

    #[test]
    fn welcome_resends_away_on_ordinary_reconnect() {
        let (mut app, mut rx) = test_app_connected();
        app.away_status = Some("lunch".into());
        app.handle_numeric(RPL_WELCOME, &["me".into(), "welcome".into()]);
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok())
                .any(|cmd| matches!(cmd, IrcCommand::Away(Some(ref m)) if m == "lunch")),
            "#190: AWAY re-sent on every 001"
        );
    }

    #[test]
    fn multiline_batch_over_limits_is_dropped() {
        let mut app = test_app();
        app.network_support.multiline_max_bytes = Some(10);
        app.network_support.multiline_max_lines = Some(10);
        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h BATCH +ml draft/multiline #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=ml :alice!u@h PRIVMSG #room :this is too long").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":alice BATCH -ml").unwrap());
        assert!(
            !app.conversations.contains_key("#room"),
            "#191: oversized batch dropped"
        );
        // Line cap.
        app.network_support.multiline_max_bytes = Some(4096);
        app.network_support.multiline_max_lines = Some(1);
        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h BATCH +m2 draft/multiline #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=m2 :alice!u@h PRIVMSG #room :one").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=m2 :alice!u@h PRIVMSG #room :two").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":alice BATCH -m2").unwrap());
        assert!(
            !app.conversations.contains_key("#room"),
            "#191: over-line batch dropped"
        );
    }

    #[test]
    fn multiline_batch_from_ignored_sender_is_dropped() {
        let mut app = test_app();
        app.ignore_list = vec!["Troll".to_string()];
        app.update_cached_lowercase();
        app.handle_incoming_message(
            IrcMessage::parse(":Troll!u@h BATCH +ml draft/multiline #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=ml :Troll!u@h PRIVMSG #room :hello").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":Troll BATCH -ml").unwrap());
        assert!(
            !app.conversations.contains_key("#room"),
            "#192: ignored multiline dropped"
        );
    }

    #[test]
    fn ignored_sender_invite_reaction_and_status_message_are_dropped() {
        let mut app = test_app();
        app.set_session_nick("me".into(), true);
        app.ignore_list = vec!["Troll".to_string()];
        app.update_cached_lowercase();
        let mut channel = Channel::new();
        channel.joined = true;
        app.conversations.insert("#room".to_string(), channel);

        // INVITE to us: no pending invite, server-buffer line or notification.
        app.handle_incoming_message(IrcMessage::parse(":Troll!u@h INVITE me #elsewhere").unwrap());
        // invite-notify line about someone else.
        app.handle_incoming_message(IrcMessage::parse(":Troll!u@h INVITE bob #room").unwrap());
        // Reaction and ops-only message (WALLCHOPS).
        app.handle_incoming_message(
            IrcMessage::parse("@+draft/react=:+1: :Troll!u@h TAGMSG #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(":Troll!u@h WALLCHOPS #room :ops only").unwrap(),
        );

        assert!(app.pending_invites.is_empty());
        assert!(app.server_messages.is_empty());
        assert!(app.conversations["#room"].messages.is_empty());
        assert!(!app.conversations.contains_key("#elsewhere"));

        // A valid peer reaction attaches to its parent rather than adding a noise row.
        app.add_message_to_channel(
            "#room",
            ChatMessage::new_fmt("bob", "parent", "short").with_irc_metadata(
                None,
                Some("parent".into()),
                None,
            ),
        );
        // The same events from a peer are still shown.
        app.handle_incoming_message(IrcMessage::parse(":alice!u@h INVITE me #elsewhere").unwrap());
        app.handle_incoming_message(
            IrcMessage::parse("@+draft/react=:+1:;+reply=parent :alice!u@h TAGMSG #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h WALLCHOPS #room :ops only").unwrap(),
        );
        assert_eq!(app.pending_invites.len(), 1);
        assert_eq!(app.conversations["#room"].messages.len(), 2);
        assert_eq!(
            app.conversations["#room"].messages[0].reactions[0].actors,
            ["alice"]
        );
    }

    #[test]
    fn server_prefixed_notice_to_our_nick_goes_to_the_server_buffer() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);

        // Post-registration server notice: no `!`/`@` in the prefix.
        app.handle_incoming_message(
            IrcMessage::parse(":irc.example.net NOTICE me :*** Server restart soon").unwrap(),
        );
        // And one with no prefix at all.
        app.handle_incoming_message(IrcMessage::parse("NOTICE me :bare server notice").unwrap());

        assert!(
            app.conversations.is_empty(),
            "no query tab named after a server"
        );
        assert_eq!(app.server_messages.len(), 2);
        assert!(
            app.server_messages[0]
                .content
                .contains("-irc.example.net- *** Server restart soon")
        );

        // Services and other users still get their own query window.
        app.handle_incoming_message(
            IrcMessage::parse(":NickServ!NickServ@services.example.net NOTICE me :Welcome")
                .unwrap(),
        );
        assert!(app.conversations.contains_key("NickServ"));
        assert_eq!(app.server_messages.len(), 2);
    }

    #[test]
    fn unjoined_channel_tabs_are_capped() {
        let mut app = test_app();
        for i in 0..200 {
            app.conversations
                .insert(format!("#spam{i}"), Channel::new());
        }
        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h PRIVMSG #spam200 :hello").unwrap(),
        );
        assert!(
            !app.conversations.contains_key("#spam200"),
            "#193: unjoined cap"
        );
        assert_eq!(
            app.conversations
                .keys()
                .filter(|k| app.is_channel_name(k))
                .count(),
            200
        );
    }

    #[test]
    fn nick_change_merges_query_on_collision() {
        let mut app = test_app();
        let mut q1 = Channel::new();
        q1.push_trimmed(ChatMessage::system("old talk"), 100);
        app.conversations.insert("alice".to_string(), q1);
        let mut q2 = Channel::new();
        q2.push_trimmed(ChatMessage::system("dest talk"), 100);
        app.conversations.insert("alice2".to_string(), q2);
        app.select_target("alice");
        app.handle_incoming_message(IrcMessage::parse(":alice!u@h NICK :alice2").unwrap());
        assert!(
            !app.conversations.contains_key("alice"),
            "#194: old tab merged away"
        );
        let merged = &app.conversations["alice2"];
        assert!(
            merged
                .messages
                .iter()
                .any(|m| m.content.contains("old talk"))
        );
        assert!(
            merged
                .messages
                .iter()
                .any(|m| m.content.contains("dest talk"))
        );
        assert!(
            merged
                .messages
                .iter()
                .any(|m| m.content.contains("alice is now known as alice2"))
        );
        assert_eq!(app.current_target_name(), Some("alice2"));
    }

    #[test]
    fn known_users_evicted_on_part_and_quit_when_orphaned() {
        let mut app = test_app();
        app.handle_incoming_message(IrcMessage::parse(":bob!u@h JOIN #room").unwrap());
        assert!(
            app.known_users
                .contains_key(&app.network_support.canonicalize("bob"))
        );
        app.handle_incoming_message(IrcMessage::parse(":bob!u@h PART #room :bye").unwrap());
        assert!(
            !app.known_users
                .contains_key(&app.network_support.canonicalize("bob")),
            "#195: PART evicts orphaned user"
        );
        app.handle_incoming_message(IrcMessage::parse(":carol!u@h JOIN #room").unwrap());
        app.handle_incoming_message(IrcMessage::parse(":carol!u@h QUIT :gone").unwrap());
        assert!(
            !app.known_users
                .contains_key(&app.network_support.canonicalize("carol")),
            "#195: QUIT evicts orphaned user"
        );
        // Monitored users are retained.
        app.monitored_users
            .insert(app.network_support.canonicalize("eve"));
        app.handle_incoming_message(IrcMessage::parse(":eve!u@h JOIN #room").unwrap());
        app.handle_incoming_message(IrcMessage::parse(":eve!u@h PART #room :bye").unwrap());
        assert!(
            app.known_users
                .contains_key(&app.network_support.canonicalize("eve"))
        );
    }

    #[test]
    fn known_users_are_pruned_when_leaving_channels() {
        let mut app = test_app();
        app.set_session_nick("me".to_string(), true);
        let know = |app: &mut Session, nick: &str| {
            app.update_user_identity(nick, None, None, None, None, None, None, Some(true));
        };
        let is_known = |app: &Session, nick: &str| {
            app.known_users
                .contains_key(&app.network_support.canonicalize(nick))
        };
        for (name, users) in [
            ("#a", vec!["me", "bob", "carol", "frank"]),
            ("#b", vec!["me", "carol", "dave"]),
            ("#c", vec!["me", "gina"]),
        ] {
            let mut channel = Channel::new();
            channel.joined = true;
            for user in users {
                channel.add_user(user, UserMode::Normal);
                know(&mut app, user);
            }
            app.conversations.insert(name.to_string(), channel);
        }
        // A query partner, a monitored friend, and a nick only seen in a
        // WHO reply: only the first two are referenced by anything.
        app.conversations.insert("erin".to_string(), Channel::new());
        know(&mut app, "erin");
        app.monitored_users
            .insert(app.network_support.canonicalize("frank"));
        know(&mut app, "stranger");

        // Leaving #b orphans dave (and the stray WHO entry), not carol.
        app.handle_incoming_message(IrcMessage::parse(":me!u@h PART #b :bye").unwrap());
        assert!(
            !is_known(&app, "dave"),
            "#257: PART evicts the channel's users"
        );
        assert!(!is_known(&app, "stranger"), "#257: unreferenced entry");
        for kept in ["me", "bob", "carol", "frank", "erin", "gina"] {
            assert!(is_known(&app, kept), "{kept} is still referenced");
        }

        // Being kicked from #c clears its users from the retained tab.
        app.handle_incoming_message(IrcMessage::parse(":op!u@h KICK #c me :bye").unwrap());
        assert!(app.conversations.contains_key("#c"));
        assert!(!is_known(&app, "gina"), "#257: self-kick evicts users");

        // Leaving #a keeps the monitored friend, drops everyone else there.
        app.handle_incoming_message(IrcMessage::parse(":me!u@h PART #a :bye").unwrap());
        assert!(!is_known(&app, "bob"));
        assert!(!is_known(&app, "carol"));
        assert!(is_known(&app, "frank"), "monitored users are retained");
        assert!(is_known(&app, "erin"), "open query tabs are retained");

        // Closing the query tab releases its partner.
        app.close_tab("erin");
        assert!(!is_known(&app, "erin"));
        assert!(is_known(&app, "frank"));
    }

    #[test]
    fn nick_completion_uses_negotiated_casemapping() {
        let mut app = test_app();
        let mut channel = Channel::new();
        channel.add_user("^Alice", UserMode::Normal);
        channel.add_user("[Bob]", UserMode::Normal);
        app.conversations.insert("#room".to_string(), channel);
        app.select_target("#room");
        assert_eq!(
            app.find_nick_completions("~a"),
            vec!["^Alice".to_string()],
            "#197"
        );
        assert_eq!(
            app.find_nick_completions("{b"),
            vec!["[Bob]".to_string()],
            "#197"
        );
    }

    #[test]
    fn split_irc_text_payload_preserves_utf8_boundaries() {
        let text = "é".repeat(400);
        let chunks = Session::split_irc_text_payload("PRIVMSG", "#chan", &text, "", "").unwrap();
        assert!(chunks.len() > 1);
        for chunk in chunks {
            assert!(format!("PRIVMSG #chan :{}", chunk).len() <= IRC_MAX_LINE_BYTES);
            assert!(chunk.is_char_boundary(chunk.len()));
        }
    }

    #[test]
    fn joining_with_chathistory_requests_server_backlog() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );

        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryLatest("#room".into(), "*".into(), 100)
        );
        assert!(app.conversations["#room"].joined);
    }

    #[test]
    fn reconnect_requests_history_after_last_seen_msgid() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        let mut channel = Channel::new();
        channel.messages.push_back(
            ChatMessage::new_fmt("alice", "last seen", "short").with_irc_metadata(
                Some("2026-09-12T12:00:00.000Z".into()),
                Some("42-last".into()),
                Some("alice".into()),
            ),
        );
        app.conversations.insert("#room".into(), channel);

        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryAfter("#room".into(), "msgid=42-last".into(), 100)
        );
    }

    #[test]
    fn reconnect_requests_history_for_retained_queries_after_welcome() {
        let (mut app, mut rx) = test_app_connected();
        app.logging_load_history = true;
        app.set_invisible = false;
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        let mut query = Channel::new();
        query.messages.push_back(
            ChatMessage::new_fmt("alice", "last message", "short").with_irc_metadata(
                Some("2026-09-12T12:00:00.000Z".into()),
                Some("query-last".into()),
                None,
            ),
        );
        app.conversations.insert("alice".into(), query);

        app.handle_numeric(RPL_WELCOME, &["me".into(), "welcome".into()]);

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryAfter("alice".into(), "msgid=query-last".into(), 100)
        );
        assert_eq!(rx.try_recv().unwrap(), IrcCommand::Ping("LAG".into()));
    }

    #[test]
    fn nefarious_bare_chathistory_cap_value_sets_page_limit() {
        let (mut app, _rx) = test_app_connected();
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me LS :draft/chathistory=37").unwrap(),
        );
        assert_eq!(app.chathistory_limit, Some(37));
    }

    #[test]
    fn cap_negotiated_history_limit_survives_isupport_without_a_chathistory_token() {
        let (mut app, _rx) = test_app_connected();
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me LS :draft/chathistory=37").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(":srv 005 me CHANTYPES=# NETWORK=Net :are supported").unwrap(),
        );
        assert_eq!(app.chathistory_limit, Some(37));
        assert_eq!(app.history_page_limit(100), 37);

        // A limit the server states through ISUPPORT is the later and more
        // specific word; withdrawing it falls back to the protocol default.
        app.handle_incoming_message(
            IrcMessage::parse(":srv 005 me CHATHISTORY=50 :are supported").unwrap(),
        );
        assert_eq!(app.chathistory_limit, Some(50));
        app.handle_incoming_message(
            IrcMessage::parse(":srv 005 me -CHATHISTORY :are supported").unwrap(),
        );
        assert_eq!(app.chathistory_limit, None);
    }

    #[test]
    fn isupport_chathistory_zero_means_no_limit_and_keeps_the_cap_limit() {
        // CHATHISTORY=0 is "no limit", never a page size of zero.
        let (mut app, _rx) = test_app_connected();
        app.handle_incoming_message(
            IrcMessage::parse(":srv 005 me CHATHISTORY=0 :are supported").unwrap(),
        );
        assert_eq!(app.chathistory_limit, None);
        assert_eq!(app.history_page_limit(100), 100);

        let (mut app, _rx) = test_app_connected();
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me LS :draft/chathistory=37").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(":srv 005 me CHATHISTORY=0 :are supported").unwrap(),
        );
        assert_eq!(app.chathistory_limit, Some(37));
        assert_eq!(app.history_page_limit(100), 37);
    }

    #[test]
    fn chathistory_batch_is_prepended_without_unread_or_logging() {
        let mut app = test_app();
        app.max_scrollback = 20;
        app.prepare_history_target("#public");
        app.conversations
            .get_mut("#public")
            .unwrap()
            .messages
            .push_back(ChatMessage::system("live row"));

        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #public")
                .unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=hist;time=2026-09-12T12:00:00.000Z :alice!u@h PRIVMSG #public :old row",
            )
            .unwrap(),
        );
        assert_eq!(app.conversations["#public"].messages.len(), 1);

        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());

        let channel = &app.conversations["#public"];
        let contents: Vec<&str> = channel
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(
            contents,
            [
                "--- 1 line of server history loaded ---",
                "old row",
                "live row"
            ]
        );
        assert!(channel.messages[1].no_log);
        assert_eq!(channel.unread, 0);
    }

    #[test]
    fn chathistory_pages_backward_until_requested_limit() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.max_scrollback = 5;
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me LS :draft/chathistory=2").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryLatest("#room".into(), "*".into(), 2)
        );

        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +page chathistory #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=page;time=2026-09-12T12:00:00.000Z;msgid=old :a PRIVMSG #room :old",
            )
            .unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=page;time=2026-09-12T12:01:00.000Z;msgid=new :b PRIVMSG #room :new",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -page").unwrap());

        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryBefore("#room".into(), "msgid=old".into(), 2)
        );
    }

    #[test]
    fn duplicate_server_history_is_not_reported_as_new_history() {
        let mut app = test_app();
        app.prepare_history_target("#room");
        app.conversations
            .get_mut("#room")
            .unwrap()
            .messages
            .push_back(
                ChatMessage::new_fmt("alice", "already visible", "short").with_irc_metadata(
                    None,
                    Some("same-id".into()),
                    None,
                ),
            );
        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=hist;msgid=same-id :alice PRIVMSG #room :already visible")
                .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());

        let contents: Vec<&str> = app.conversations["#room"]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(
            contents,
            ["--- No new server history available ---", "already visible"]
        );
    }

    #[test]
    fn multiline_batches_preserve_newline_and_concat_semantics() {
        let mut app = test_app();
        app.handle_incoming_message(
            IrcMessage::parse(
                "@time=2026-09-12T12:00:00.000Z;msgid=ml1 :alice!u@h BATCH +ml draft/multiline #room",
            )
            .unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=ml :alice!u@h PRIVMSG #room :one").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=ml;draft/multiline-concat :alice!u@h PRIVMSG #room :two")
                .unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=ml :alice!u@h PRIVMSG #room :three").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":alice BATCH -ml").unwrap());

        let message = app.conversations["#room"].messages.back().unwrap();
        assert_eq!(message.content, "onetwo\nthree");
        assert_eq!(message.msgid.as_deref(), Some("ml1"));
        assert_eq!(
            message.server_time.as_deref(),
            Some("2026-09-12T12:00:00.000Z")
        );
    }

    #[test]
    fn authtoken_chunks_are_joined_into_one_usable_token() {
        let mut app = test_app();
        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +tok draft/authtoken web-api").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=tok :srv TOKEN GENERATE * :abc").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=tok :srv TOKEN GENERATE * :def").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -tok").unwrap());

        assert_eq!(app.server_messages.len(), 1);
        assert_eq!(
            app.server_messages[0].content,
            "[TOKEN GENERATE web-api] abcdef"
        );
        assert!(app.server_messages[0].no_log);
    }

    #[test]
    fn authtoken_batches_stop_accumulating_at_a_size_cap() {
        let mut app = test_app();
        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +tok draft/authtoken web-api").unwrap(),
        );
        let chunk = "x".repeat(400);
        let mut longest = 0;
        for _ in 0..200 {
            app.handle_incoming_message(
                IrcMessage::parse(&format!("@batch=tok :srv TOKEN GENERATE * :{chunk}")).unwrap(),
            );
            longest = longest.max(app.authtoken_batches["tok"].token.len());
        }
        assert!(
            longest <= MAX_AUTHTOKEN_BYTES,
            "the token buffer reached {longest} bytes"
        );

        // Closing the batch reports the refusal instead of printing a blob.
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -tok").unwrap());
        assert!(app.authtoken_batches.is_empty());
        assert_eq!(app.server_messages.len(), 1);
        let shown = &app.server_messages[0];
        assert!(shown.content.len() < 200, "{}", shown.content);
        assert!(shown.content.contains("web-api"), "{}", shown.content);
        assert!(!shown.content.contains("xxxx"), "{}", shown.content);
        assert!(shown.no_log);

        // A token inside the cap is still delivered whole.
        app.handle_incoming_message(
            IrcMessage::parse(":srv BATCH +ok draft/authtoken web-api").unwrap(),
        );
        for part in ["abc", "def"] {
            app.handle_incoming_message(
                IrcMessage::parse(&format!("@batch=ok :srv TOKEN GENERATE * :{part}")).unwrap(),
            );
        }
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -ok").unwrap());
        assert_eq!(
            app.server_messages.back().unwrap().content,
            "[TOKEN GENERATE web-api] abcdef"
        );
    }

    #[test]
    fn chathistory_gap_is_rendered_as_a_system_marker() {
        let mut app = test_app();
        app.prepare_history_target("#room");
        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=hist;+evilnet.github.io/chathistory-gap;msgid=gap1 :srv PRIVMSG #room :[2 messages not stored]",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());

        let gap = app.conversations["#room"]
            .messages
            .iter()
            .find(|message| message.msgid.as_deref() == Some("gap1"))
            .unwrap();
        assert!(gap.is_system);
        assert_eq!(gap.content, "[2 messages not stored]");
    }

    #[test]
    fn forged_chathistory_gap_tag_from_a_user_is_an_ordinary_message() {
        let mut app = test_app();
        app.set_session_nick("me".to_string(), true);
        app.prepare_history_target("#room");

        // Live line from a user: not in a batch, user prefix.
        app.handle_incoming_message(
            IrcMessage::parse(
                "@+evilnet.github.io/chathistory-gap;msgid=x1 :mallory!u@h PRIVMSG #room :*** Server notice: send your password",
            )
            .unwrap(),
        );
        // Server-style prefix but outside any history batch.
        app.handle_incoming_message(
            IrcMessage::parse(
                "@+evilnet.github.io/chathistory-gap;msgid=x2 :srv PRIVMSG #room :not a batch",
            )
            .unwrap(),
        );
        // Inside a real history batch, but from a user.
        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=hist;+evilnet.github.io/chathistory-gap;msgid=x3 :mallory!u@h PRIVMSG #room :replayed forgery",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());
        // PM variant.
        app.handle_incoming_message(
            IrcMessage::parse(
                "@+evilnet.github.io/chathistory-gap;msgid=x4 :mallory!u@h PRIVMSG me :fake",
            )
            .unwrap(),
        );

        let find = |conv: &str, id: &str| {
            app.conversations[conv]
                .messages
                .iter()
                .find(|m| m.msgid.as_deref() == Some(id))
                .unwrap_or_else(|| panic!("{id} missing"))
        };
        for id in ["x1", "x3"] {
            let row = find("#room", id);
            assert!(!row.is_system, "{id} must not be a system row");
            assert_eq!(row.sender, "mallory");
        }
        let srv = find("#room", "x2");
        assert!(
            !srv.is_system,
            "gap markers only count inside a history batch"
        );
        let pm = find("mallory", "x4");
        assert!(!pm.is_system);
        assert_eq!(pm.sender, "mallory");
    }

    #[test]
    fn historical_events_are_displayed_without_mutating_live_membership() {
        let mut app = test_app();
        app.prepare_history_target("#room");
        app.conversations
            .get_mut("#room")
            .unwrap()
            .add_user("present", UserMode::Normal);
        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=hist;time=2026-09-12T12:00:00.000Z;msgid=j1 :gone!u@h JOIN #room",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());

        let channel = &app.conversations["#room"];
        assert!(channel.has_user("present"));
        assert!(!channel.has_user("gone"));
        assert!(
            channel
                .messages
                .iter()
                .any(|message| message.content == "gone joined #room")
        );
    }

    #[test]
    fn historical_channel_rename_and_relocate_do_not_mutate_the_live_tab() {
        for (event, shown) in [
            (
                ":op!u@h RENAME #old #new :new name",
                "Channel renamed from #old to #new: new name",
            ),
            (
                ":op!u@h RELOCATE #old #new :moved",
                "Channel #old moved to #new: moved",
            ),
        ] {
            let mut app = test_app();
            app.prepare_history_target("#old");
            app.select_target("#old");
            app.handle_incoming_message(
                IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #old")
                    .unwrap(),
            );
            app.handle_incoming_message(
                IrcMessage::parse(&format!(
                    "@batch=hist;time=2026-09-12T12:00:00.000Z;msgid=r1 {event}"
                ))
                .unwrap(),
            );
            // The replay is still open: nothing may have moved yet.
            assert!(app.conversations.contains_key("#old"), "{event}");
            assert!(!app.conversations.contains_key("#new"), "{event}");
            assert_eq!(app.current_target_name(), Some("#old"), "{event}");
            app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());

            let mut tabs: Vec<&String> = app.conversations.keys().collect();
            tabs.sort();
            assert_eq!(tabs, ["#old"], "{event}");
            assert_eq!(app.current_target_name(), Some("#old"), "{event}");
            assert!(
                app.conversations["#old"]
                    .messages
                    .iter()
                    .any(|message| message.content == shown
                        && message.msgid.as_deref() == Some("r1")),
                "{event}"
            );
        }
    }

    #[test]
    fn outbound_multiline_uses_a_tagged_batch() {
        let (mut app, mut rx) = test_app_connected();
        app.handle_incoming_message(IrcMessage::parse(":srv CAP me ACK :draft/multiline").unwrap());
        assert!(app.send_privmsg_text("#room", "one\ntwo").complete);

        let IrcCommand::ClientBatch(commands) = rx.try_recv().unwrap() else {
            panic!("expected atomic multiline envelope")
        };
        assert!(rx.try_recv().is_err());
        let mut wire = commands.into_iter();
        let IrcCommand::Batch(open, Some(kind), Some(target)) = wire.next().unwrap() else {
            panic!("expected multiline batch opener");
        };
        assert!(open.starts_with("+lf"));
        assert_eq!(kind, "draft/multiline");
        assert_eq!(target, "#room");
        let batch_id = open.trim_start_matches('+');
        assert_eq!(
            wire.next().unwrap(),
            IrcCommand::Raw(format!("@batch={batch_id} PRIVMSG #room :one"))
        );
        assert_eq!(
            wire.next().unwrap(),
            IrcCommand::Raw(format!("@batch={batch_id} PRIVMSG #room :two"))
        );
        assert_eq!(
            wire.next().unwrap(),
            IrcCommand::Batch(format!("-{batch_id}"), None, None)
        );
    }

    #[test]
    fn chathistory_empty_batch_falls_back_to_local_log_history() {
        let dir = temporary_log_dir("chathistory-empty-fallback");
        let mut app = test_app();
        app.log_manager = logging::LogManager::with_test_dir(dir.clone());
        app.logging_enabled = true;
        app.logging_load_history = true;
        app.server_host = "irc.example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        assert!(app.start_session_from_form());
        let (tx, _rx) = mpsc::unbounded_channel();
        app.test_set_registered(tx);

        let (network, _, _) = app.active_log_context().unwrap();
        app.log_manager.log_message(
            &network,
            "#room",
            &ChatMessage::new_fmt("alice", "local log message", "short"),
        );
        app.log_manager.flush_all();

        app.set_session_nick("me".into(), true);
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());

        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #room").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());

        let channel = &app.conversations["#room"];
        assert!(channel.history_loaded);
        let contents: Vec<&str> = channel
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(
            contents,
            [
                "--- 1 line of history loaded ---",
                "local log message",
                "Now talking in #room"
            ]
        );

        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn short_server_history_includes_saved_local_history() {
        let dir = temporary_log_dir("chathistory-short-server");
        let mut app = test_app();
        app.log_manager = logging::LogManager::with_test_dir(dir.clone());
        app.logging_enabled = true;
        app.logging_load_history = true;
        app.server_host = "irc.example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        assert!(app.start_session_from_form());
        let (tx, _rx) = mpsc::unbounded_channel();
        app.test_set_registered(tx);

        let (network, _, _) = app.active_log_context().unwrap();
        app.log_manager.log_message(
            &network,
            "#room",
            &ChatMessage::new_fmt("alice", "saved discussion", "short"),
        );
        app.log_manager.flush_all();

        app.set_session_nick("me".into(), true);
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());
        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=hist;time=2026-09-12T12:00:00.000Z;msgid=one :bob PRIVMSG #room :server row")
                .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());

        let contents: Vec<&str> = app.conversations["#room"]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(
            contents,
            [
                "--- History loaded: 1 local line, 1 server line ---",
                "saved discussion",
                "server row",
                "Now talking in #room"
            ]
        );

        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn chathistory_fail_reply_falls_back_to_local_log_history() {
        let dir = temporary_log_dir("chathistory-fail-fallback");
        let mut app = test_app();
        app.log_manager = logging::LogManager::with_test_dir(dir.clone());
        app.logging_enabled = true;
        app.logging_load_history = true;
        app.server_host = "irc.example".into();
        app.server_port = "6697".into();
        app.use_tls = true;
        assert!(app.start_session_from_form());
        let (tx, _rx) = mpsc::unbounded_channel();
        app.test_set_registered(tx);

        let (network, _, _) = app.active_log_context().unwrap();
        app.log_manager.log_message(
            &network,
            "#room",
            &ChatMessage::new_fmt("alice", "saved discussion", "short"),
        );
        app.log_manager.flush_all();

        app.set_session_nick("me".into(), true);
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());

        app.handle_incoming_message(
            IrcMessage::parse(
                ":srv FAIL CHATHISTORY INVALID_TARGET LATEST #room :No access to target",
            )
            .unwrap(),
        );

        let channel = &app.conversations["#room"];
        assert!(channel.history_loaded);
        assert!(
            channel
                .messages
                .iter()
                .any(|m| m.content == "saved discussion")
        );
        assert!(
            channel
                .messages
                .iter()
                .any(|m| m.content == "--- 1 line of history loaded ---")
        );

        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Deliver a complete two-line draft/multiline message through the same
    /// entry point the connection thread uses.
    /// The desktop notifications the session asked the App for, in order.
    /// Whether one is shown (focus, the setting, rate limits) is the App's
    /// decision, tested in `app.rs`.
    fn notified(app: &Session) -> Vec<(String, String)> {
        app.events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Notify { title, body, .. } => Some((title.clone(), body.clone())),
                _ => None,
            })
            .collect()
    }

    fn deliver_multiline(app: &mut Session, id: &str, prefix: &str, target: &str, lines: &[&str]) {
        app.handle_incoming_message(
            IrcMessage::parse(&format!(":{prefix} BATCH +{id} draft/multiline {target}")).unwrap(),
        );
        for line in lines {
            app.handle_incoming_message(
                IrcMessage::parse(&format!("@batch={id} :{prefix} PRIVMSG {target} :{line}"))
                    .unwrap(),
            );
        }
        app.handle_incoming_message(IrcMessage::parse(&format!(":{prefix} BATCH -{id}")).unwrap());
    }

    #[test]
    fn multiline_messages_highlight_and_notify_like_single_line_messages() {
        let mut app = test_app();
        app.set_session_nick("me".to_string(), true);
        app.window_focused = false;

        // A nick mention in a channel: highlighted row and a highlight
        // notification, exactly as the single-line message gets.
        deliver_multiline(
            &mut app,
            "m1",
            "alice!u@h",
            "#room",
            &["hello me", "second line"],
        );
        let row = app.conversations["#room"].messages.back().unwrap();
        assert_eq!(row.content, "hello me\nsecond line");
        assert!(row.is_highlight, "#289: multiline mention not highlighted");
        assert_eq!(notified(&app).len(), 1);
        assert_eq!(notified(&app)[0].0, "alice mentioned you in #room");

        // A multiline PM notifies even while the window is focused.
        app.events.clear();
        app.window_focused = true;
        deliver_multiline(&mut app, "m2", "bob!u@h", "me", &["psst", "are you there"]);
        let row = app.conversations["bob"].messages.back().unwrap();
        assert_eq!(row.content, "psst\nare you there");
        assert!(!row.is_highlight);
        assert_eq!(notified(&app).len(), 1, "#289: PM not notified");
        assert_eq!(notified(&app)[0].0, "PM from bob");

        // A plain channel message neither highlights nor notifies.
        app.events.clear();
        app.window_focused = false;
        deliver_multiline(&mut app, "m3", "alice!u@h", "#room", &["nothing", "here"]);
        assert!(
            !app.conversations["#room"]
                .messages
                .back()
                .unwrap()
                .is_highlight
        );
        assert!(notified(&app).is_empty());
    }

    #[test]
    fn unterminated_ctcp_is_recognised_like_the_terminated_form() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        let last = |app: &mut Session, content: &str| {
            let line = format!(":alice!u@h PRIVMSG #a :{content}");
            app.handle_incoming_message(IrcMessage::parse(&line).unwrap());
            app.conversations["#a"].messages.back().cloned()
        };
        for (content, text) in [
            ("\x01ACTION waves", "waves"),
            ("\x01ACTION waves\x01", "waves"),
            ("\x01ACTION\x01", ""),
            ("\x01ACTION", ""),
        ] {
            let row = last(&mut app, content).unwrap();
            assert!(row.is_action, "{content:?}");
            assert_eq!(row.content, text, "{content:?}");
        }
        // Not actions; nothing may panic and no action row appears.
        for content in ["\x01", "\x01\x01", "\x01ACTIONS waves\x01"] {
            let row = last(&mut app, content);
            assert!(row.is_none_or(|row| !row.is_action), "{content:?}");
        }

        // An unterminated request is answered like a terminated one, to the
        // sender and not the channel.
        while rx.try_recv().is_ok() {}
        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h PRIVMSG me :\x01SOURCE").unwrap(),
        );
        let reply = rx.try_recv().expect("unterminated SOURCE answered");
        assert!(
            matches!(&reply, IrcCommand::Notice(to, body)
                if to == "alice" && body.starts_with("\x01SOURCE ") && body.ends_with('\x01')),
            "{reply:?}"
        );

        // An unterminated reply is a CTCP reply, not a raw NOTICE.
        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h NOTICE me :\x01VERSION SomeClient 1.0").unwrap(),
        );
        assert!(
            app.server_messages
                .iter()
                .chain(app.conversations.values().flat_map(|c| c.messages.iter()))
                .any(|m| m.content == "[CTCP VERSION reply] alice - SomeClient 1.0"),
        );

        // draft/multiline batches use the same parser.
        deliver_multiline(
            &mut app,
            "ml",
            "alice!u@h",
            "#a",
            &["\x01ACTION waves", "and smiles"],
        );
        let row = app.conversations["#a"].messages.back().unwrap();
        assert!(row.is_action);
        assert_eq!(row.content, "waves\nand smiles");
    }

    #[test]
    fn multiline_highlight_rules_use_the_routed_conversation() {
        fn highlighted(app: &Session, conversation: &str, rows_before: usize) -> bool {
            let rows = &app.conversations[conversation].messages;
            assert_eq!(rows.len(), rows_before + 1);
            rows.back().unwrap().is_highlight
        }
        let mut app = test_app();
        app.set_session_nick("me".to_string(), true);
        app.window_focused = false;
        app.attention
            .rule_mut(app.conversation_key("bob"))
            .highlight_terms = Some(vec!["deploy".into()]);
        app.attention
            .rule_mut(app.conversation_key("#room"))
            .highlight_terms = Some(vec!["deploy".into()]);

        // A DM batch is addressed to our own nick; the rule is keyed by bob.
        deliver_multiline(&mut app, "p1", "bob!u@h", "me", &["deploy now", "please"]);
        assert!(highlighted(&app, "bob", 0), "DM rule ignored");
        // A STATUSMSG batch is addressed to `@#room`.
        deliver_multiline(&mut app, "p2", "alice!u@h", "@#room", &["deploy now", "x"]);
        assert!(highlighted(&app, "#room", 0), "STATUSMSG rule ignored");

        // Parity with the single-line path, including an override that
        // replaces the global highlight words.
        app.highlight_words = "standup".into();
        app.highlight_words_lower = vec!["standup".into()];
        deliver_multiline(&mut app, "p3", "bob!u@h", "me", &["standup soon", "ok"]);
        assert!(!highlighted(&app, "bob", 1), "global word bypassed rule");
        app.handle_incoming_message(
            IrcMessage::parse(":bob!u@h PRIVMSG me :standup soon").unwrap(),
        );
        assert!(!highlighted(&app, "bob", 2));
    }

    #[test]
    fn multiline_action_mentions_highlight_and_own_or_replayed_messages_stay_silent() {
        let mut app = test_app();
        app.set_session_nick("me".to_string(), true);
        app.window_focused = false;

        // CTCP ACTION multiline mention: highlighted action row.
        deliver_multiline(
            &mut app,
            "a1",
            "alice!u@h",
            "#room",
            &["\x01ACTION waves at me", "and smiles\x01"],
        );
        let row = app.conversations["#room"].messages.back().unwrap();
        assert!(row.is_action && row.is_highlight, "#289: {row:?}");

        // Our own multiline message (echo-message) never notifies.
        app.events.clear();
        deliver_multiline(
            &mut app,
            "s1",
            "me!u@h",
            "me",
            &["note to self", "me me me"],
        );
        assert!(notified(&app).is_empty());

        // A multiline message replayed inside a chathistory batch keeps its
        // highlight but never raises a notification.
        app.prepare_history_target("#room");
        app.handle_incoming_message(
            IrcMessage::parse("@draft/chathistory-end :srv BATCH +hist chathistory #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=hist :bob!u@h BATCH +hm draft/multiline #room").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=hm :bob!u@h PRIVMSG #room :old news for me").unwrap(),
        );
        app.handle_incoming_message(
            IrcMessage::parse("@batch=hm :bob!u@h PRIVMSG #room :second").unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":bob!u@h BATCH -hm").unwrap());
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -hist").unwrap());
        let replayed = app.conversations["#room"]
            .messages
            .iter()
            .find(|row| row.content == "old news for me\nsecond")
            .expect("replayed multiline row");
        assert!(replayed.is_highlight);
        assert!(notified(&app).is_empty());
    }

    #[test]
    fn unsolicited_self_join_keeps_the_tab_the_user_is_reading() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.window_focused = true;
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/read-marker").unwrap(),
        );
        app.add_message_to_channel("alice", ChatMessage::new_fmt("alice", "hi", "short"));
        app.select_target("alice");

        // A bouncer replay / reconnect echoes our JOIN for channels the user
        // did not just ask for (#290): the view must not jump to them.
        for channel in ["#a", "#b", "#c"] {
            app.handle_incoming_message(
                IrcMessage::parse(&format!(":me!u@h JOIN {channel}")).unwrap(),
            );
            assert_eq!(app.current_target_name(), Some("alice"), "{channel}");
            assert!(app.conversations[channel].joined);
        }

        // Messages for the never-selected tab count as unread and are not
        // reported read to the server.
        app.handle_incoming_message(
            IrcMessage::parse(
                "@time=2026-09-12T12:00:00.000Z;msgid=abc :bob!u@h PRIVMSG #c :anyone here?",
            )
            .unwrap(),
        );
        assert_eq!(app.conversations["#c"].unread, 1);
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok())
                .any(|cmd| matches!(cmd, IrcCommand::Markread(..))),
            "#290: MARKREAD sent for a tab the user never selected"
        );
    }

    #[test]
    fn join_the_user_asked_for_still_takes_focus() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".to_string(), true);
        app.add_message_to_channel("alice", ChatMessage::new_fmt("alice", "hi", "short"));
        app.select_target("alice");

        app.process_command("/join #wanted");
        // The server confirms a different channel first (bouncer replay): no
        // focus change for it, and the request stays pending for #wanted.
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #replayed").unwrap());
        assert_eq!(app.current_target_name(), Some("alice"));
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #WANTED").unwrap());
        assert_eq!(app.current_target_name(), Some("#WANTED"));

        // The request is consumed: a later rejoin of it does not refocus.
        app.select_target("alice");
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #WANTED").unwrap());
        assert_eq!(app.current_target_name(), Some("alice"));
    }

    #[test]
    fn auto_join_from_the_server_buffer_lands_on_the_first_channel_only() {
        let mut app = test_app();
        app.set_session_nick("me".to_string(), true);
        assert!(app.current_conversation.is_none());

        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #first").unwrap());
        app.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #second").unwrap());
        assert_eq!(app.current_target_name(), Some("#first"));
    }

    #[test]
    fn selected_tab_keeps_unread_while_the_window_is_unfocused_until_focus_returns() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.conversations.insert("#room".into(), Channel::new());
        app.select_target("#room");
        // The active viewport has reported that it is reading the buffer tail.
        app.reading_conversation = app.current_conversation;
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/read-marker").unwrap(),
        );

        // Minimised to the tray: nothing is being read.
        app.update_minimal();
        for n in 1..=5 {
            app.handle_incoming_message(
                IrcMessage::parse(&format!(
                    "@time=2026-09-12T12:00:0{n}.000Z;msgid=m{n} :alice!u@h PRIVMSG #room :msg {n}"
                ))
                .unwrap(),
            );
        }
        assert_eq!(
            app.conversations["#room"].unread, 5,
            "#305: unfocused current tab silently treated as read"
        );
        assert!(
            rx.try_recv().is_err(),
            "unseen messages must not be reported read to the server"
        );

        // Restoring the window is when the user sees them: badge cleared,
        // read marker sent for the newest message.
        app.set_window_focused(true);
        assert_eq!(app.conversations["#room"].unread, 0);
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Markread(
                "#room".into(),
                Some("timestamp=2026-09-12T12:00:05.000Z".into())
            )
        );

        // Focused and selected: still read on arrival.
        app.handle_incoming_message(
            IrcMessage::parse("@time=2026-09-12T12:00:09.000Z :alice!u@h PRIVMSG #room :seen")
                .unwrap(),
        );
        assert_eq!(app.conversations["#room"].unread, 0);
    }

    #[test]
    fn server_buffer_counts_unread_while_unfocused_and_clears_on_focus() {
        let mut app = test_app();
        assert!(app.current_conversation.is_none());
        app.add_server_message(ChatMessage::system("seen live"));
        assert_eq!(app.server_unread, 0);

        app.set_window_focused(false);
        app.add_server_message(ChatMessage::system("missed"));
        assert_eq!(app.server_unread, 1);

        app.set_window_focused(true);
        assert_eq!(app.server_unread, 0);
    }

    /// A connected app with chathistory negotiated and a retained query tab
    /// ("alice") plus a channel the user is reading ("#room"), after a
    /// reconnect whose registration just requested catch-up for the query.
    fn reconnected_app_reading_a_channel() -> (Session, mpsc::UnboundedReceiver<IrcCommand>) {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("mynick".into(), true);
        app.logging_load_history = true;
        app.set_invisible = false;
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        let mut query = Channel::new();
        query.messages.push_back(
            ChatMessage::new_fmt("alice", "last message", "short").with_irc_metadata(
                Some("2026-09-12T12:00:00.000Z".into()),
                Some("query-last".into()),
                None,
            ),
        );
        app.conversations.insert("alice".into(), query);
        let mut room = Channel::new();
        room.joined = true;
        app.conversations.insert("#room".into(), room);
        app.select_target("#room");
        app.handle_numeric(RPL_WELCOME, &["mynick".into(), "welcome".into()]);
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryAfter("alice".into(), "msgid=query-last".into(), 100)
        );
        (app, rx)
    }

    #[test]
    fn reconnect_catch_up_raises_unread_and_notifies_for_new_private_messages() {
        let (mut app, _rx) = reconnected_app_reading_a_channel();
        app.window_focused = true;

        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +h chathistory alice").unwrap());
        for (n, line) in [(1, "are you there?"), (2, "hello?")] {
            app.handle_incoming_message(
                IrcMessage::parse(&format!(
                    "@batch=h;time=2026-09-12T12:0{n}:00.000Z;msgid=n{n} :alice!u@h PRIVMSG mynick :{line}"
                ))
                .unwrap(),
            );
        }
        // Our own line sent from another client is catch-up too, but never
        // counts as unread.
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=h;time=2026-09-12T12:03:00.000Z;msgid=n3 :mynick!u@h PRIVMSG alice :on my way",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -h").unwrap());

        let alice = &app.conversations["alice"];
        assert_eq!(alice.messages.back().unwrap().content, "on my way");
        assert_eq!(alice.unread, 2, "#306: catch-up PMs left no unread badge");
        assert_eq!(notified(&app).len(), 1, "#306: no notification");
        assert_eq!(notified(&app)[0].0, "2 new messages from alice");
        assert_eq!(notified(&app)[0].1, "hello?");
    }

    #[test]
    fn catch_up_in_the_selected_focused_tab_is_already_seen() {
        let (mut app, _rx) = reconnected_app_reading_a_channel();
        app.select_target("alice");
        // The active viewport has reported that it is reading the buffer tail.
        app.reading_conversation = app.current_conversation;
        app.window_focused = true;

        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +h chathistory alice").unwrap());
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=h;time=2026-09-12T12:01:00.000Z;msgid=n1 :alice!u@h PRIVMSG mynick :hi",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -h").unwrap());

        assert_eq!(app.conversations["alice"].unread, 0);
    }

    #[test]
    fn channel_catch_up_notifies_only_for_mentions_and_old_backlog_stays_silent() {
        let (mut app, mut rx) = reconnected_app_reading_a_channel();
        app.window_focused = false;
        app.select_target("alice");

        // Retained channel rejoin: Append catch-up after the last known msgid.
        app.conversations
            .get_mut("#room")
            .unwrap()
            .messages
            .push_back(
                ChatMessage::new_fmt("bob", "earlier", "short").with_irc_metadata(
                    Some("2026-09-12T11:00:00.000Z".into()),
                    Some("room-last".into()),
                    None,
                ),
            );
        while rx.try_recv().is_ok() {}
        assert!(app.request_latest_history("#room", None, HistoryPlacement::Append));
        assert!(matches!(
            rx.try_recv(),
            Ok(IrcCommand::ChathistoryAfter(target, _, _)) if target == "#room"
        ));
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +c chathistory #room").unwrap());
        for (n, line) in [(1, "chatter"), (2, "nothing to see here")] {
            app.handle_incoming_message(
                IrcMessage::parse(&format!(
                    "@batch=c;time=2026-09-12T12:0{n}:00.000Z;msgid=c{n} :bob!u@h PRIVMSG #room :{line}"
                ))
                .unwrap(),
            );
        }
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -c").unwrap());
        assert_eq!(app.conversations["#room"].unread, 2);
        assert!(
            notified(&app).is_empty(),
            "no mention, no notification: {:?}",
            notified(&app)
        );

        app.handle_incoming_message(IrcMessage::parse(":mynick!u@h JOIN #fresh").unwrap());
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +p chathistory #fresh").unwrap());
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=p;time=2026-09-12T10:00:00.000Z;msgid=p1 :bob!u@h PRIVMSG #fresh :mynick mynick",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -p").unwrap());
        assert!(
            notified(&app).is_empty(),
            "old backlog (Prepend) never notifies"
        );

        // A mention inside the Append page does.
        assert!(app.request_latest_history("#room", None, HistoryPlacement::Append));
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +d chathistory #room").unwrap());
        app.handle_incoming_message(
            IrcMessage::parse(
                "@batch=d;time=2026-09-12T12:05:00.000Z;msgid=d1 :bob!u@h PRIVMSG #room :hey mynick, ping",
            )
            .unwrap(),
        );
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -d").unwrap());
        assert_eq!(notified(&app).len(), 1);
        assert_eq!(notified(&app)[0].0, "bob mentioned you in #room");
    }

    #[test]
    fn catch_up_does_not_duplicate_rows_restored_from_the_local_log() {
        let (mut app, mut rx) = test_app_connected();
        app.set_session_nick("mynick".into(), true);
        app.logging_load_history = true;
        app.handle_incoming_message(
            IrcMessage::parse(":srv CAP me ACK :draft/chathistory").unwrap(),
        );
        // A retained tab preloaded from the local log: its rows carry no
        // msgid and no server-time, so catch-up has no message to start after.
        let stamp = epoch_time_formatted(1_789_511_400, &app.timestamp_format);
        let mut room = Channel::new();
        for (sender, content) in [("alice", "first"), ("bob", "first")] {
            let mut row = ChatMessage::new_fmt(sender, content, &app.timestamp_format);
            row.timestamp.clone_from(&stamp);
            room.messages.push_back(row);
        }
        app.conversations.insert("#room".into(), room);
        app.select_target("#room");

        app.handle_incoming_message(IrcMessage::parse(":mynick!u@h JOIN #room").unwrap());
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryLatest("#room".into(), "*".into(), 100)
        );

        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +h chathistory #room").unwrap());
        for (id, time, sender, content) in [
            ("s1", "22:30:00", "alice", "first"),
            ("s2", "22:30:00", "bob", "first"),
            ("s3", "22:31:00", "alice", "brand new"),
        ] {
            app.handle_incoming_message(
                IrcMessage::parse(&format!(
                    "@batch=h;time=2026-09-15T{time}.000Z;msgid={id} :{sender}!u@h PRIVMSG #room :{content}"
                ))
                .unwrap(),
            );
        }
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -h").unwrap());

        let rows: Vec<(&str, &str)> = app.conversations["#room"]
            .messages
            .iter()
            .filter(|row| !row.is_system)
            .map(|row| (row.sender.as_str(), row.content.as_str()))
            .collect();
        assert_eq!(
            rows,
            [("alice", "first"), ("bob", "first"), ("alice", "brand new")],
            "#320: server rows duplicated the restored local rows"
        );
    }

    #[test]
    fn catch_up_keeps_repeated_identical_lines_it_has_no_local_copy_of() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("mynick".into(), true);
        let stamp = epoch_time_formatted(1_789_511_400, &app.timestamp_format);
        let mut room = Channel::new();
        room.joined = true;
        let mut local = ChatMessage::new_fmt("alice", "ok", &app.timestamp_format);
        local.timestamp.clone_from(&stamp);
        room.messages.push_back(local);
        app.conversations.insert("#room".into(), room);
        app.select_target("#room");
        app.pending_history_requests
            .entry("#room".into())
            .or_default()
            .push_back(PendingHistoryRequest {
                correlation: None,
                placement: HistoryPlacement::Append,
                direction: HistoryDirection::Before,
                remaining: 100,
                loaded: 0,
                shown: 0,
                page_limit: 100,
                announce_arrivals: false,
            });

        // The server holds two identical "ok" lines in that minute; only one
        // is the row already shown, so exactly one new row remains.
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH +h chathistory #room").unwrap());
        for id in ["s1", "s2"] {
            app.handle_incoming_message(
                IrcMessage::parse(&format!(
                    "@batch=h;time=2026-09-15T22:30:00.000Z;msgid={id} :alice!u@h PRIVMSG #room :ok"
                ))
                .unwrap(),
            );
        }
        app.handle_incoming_message(IrcMessage::parse(":srv BATCH -h").unwrap());

        let ok_rows = app.conversations["#room"]
            .messages
            .iter()
            .filter(|row| row.content == "ok")
            .count();
        assert_eq!(ok_rows, 2);
    }

    #[test]
    fn typed_credentials_never_enter_command_history() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        let mut room = Channel::new();
        room.joined = true;
        app.conversations.insert("#room".into(), room);
        app.conversations.insert("NickServ".into(), Channel::new());
        let secret_lines = [
            "IDENTIFY hunter2",
            "identify hunter2",
            "ID hunter2",
            "/msg NickServ IDENTIFY hunter2",
            "/msg ns identify hunter2",
            "/msg NickServ@services. IDENTIFY hunter2",
            "/notice NickServ IDENTIFY hunter2",
            "/query NickServ IDENTIFY hunter2",
            "/say IDENTIFY hunter2",
            "/me IDENTIFY hunter2",
            "//ns identify hunter2",
            "/ns IDENTIFY hunter2",
            "/cs IDENTIFY #room hunter2",
            "/msg ChanServ SET #room PASSWORD hunter2",
            "/msg NickServ SET PASSWORD hunter2",
            "/msg NickServ DROP me hunter2",
            "/raw PRIVMSG NickServ :IDENTIFY hunter2",
            "/quote PRIVMSG NickServ :IDENTIFY hunter2",
            "/msg\tNickServ\tIDENTIFY hunter2",
            "/msg NickServ :IDENTIFY hunter2",
            "/cprivmsg NickServ #room IDENTIFY hunter2",
            "/pass hunter2",
            "/oper admin hunter2",
            "/server irc.example.test 6667 hunter2",
            "/server irc.example.test hunter2",
            "/identify hunter2",
            "/ghost me hunter2",
        ];
        for current in ["#room", "NickServ"] {
            app.select_target(current);
            for line in secret_lines {
                app.input_text = line.to_string();
                app.process_input();
            }
            // Ordinary typed text in a service query is a credential risk
            // only with an auth keyword; plain chat in a channel is history.
            app.input_text = "hello world".to_string();
            app.process_input();
        }
        assert!(
            !app.command_history
                .iter()
                .any(|entry| entry.contains("hunter2")),
            "a credential reached command history: {:?}",
            app.command_history
        );
    }

    #[test]
    fn incoming_routing_never_panics_on_hostile_lines() {
        let (mut app, _rx) = test_app_connected();
        app.set_session_nick("me".into(), true);
        app.conversations.insert("#a".into(), Channel::new());
        app.select_target("#a");
        // Wire-line soup: commands, numerics and tags assembled from a small
        // alphabet so state (channels, batches, lists) actually builds up.
        let tokens = [
            ":alice!u@h",
            ":bob!u@h",
            ":srv",
            "PRIVMSG",
            "NOTICE",
            "JOIN",
            "PART",
            "QUIT",
            "NICK",
            "MODE",
            "KICK",
            "TOPIC",
            "BATCH",
            "+b1",
            "-b1",
            "chathistory",
            "draft/multiline",
            "#a",
            "#b",
            "me",
            "alice",
            "bob",
            "+o",
            "-o",
            "+b",
            "*!*@h",
            "001",
            "005",
            "324",
            "329",
            "332",
            "333",
            "353",
            "366",
            "367",
            "368",
            "352",
            "354",
            "315",
            "322",
            "323",
            "728",
            "729",
            "730",
            "731",
            "CAP",
            "ACK",
            "LS",
            "multi-prefix",
            "CHANTYPES=#",
            "@batch=b1",
            "@time=2025-01-01T00:00:00.000Z",
            ":",
            ":hi there",
            "hello",
            "\u{1}ACTION waves\u{1}",
            "%",
            "@",
            "=",
            "42",
        ];
        let mut state: u64 = 0x5eed_5eed_5eed_5eed;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _case in 0..8000 {
            let count = 1 + (next() % 7) as usize;
            let line = (0..count)
                .map(|_| tokens[(next() as usize) % tokens.len()])
                .collect::<Vec<_>>()
                .join(" ");
            if let Some(msg) = IrcMessage::parse(&line) {
                app.handle_incoming_message(msg);
            }
        }
        // The session must still be usable afterwards.
        app.handle_incoming_message(
            IrcMessage::parse(":alice!u@h PRIVMSG #a :still alive").unwrap(),
        );
        assert!(
            app.conversations["#a"]
                .messages
                .iter()
                .any(|m| m.content == "still alive")
        );
    }
}

#[cfg(test)]
fn flatten_test_commands(command: IrcCommand) -> Vec<IrcCommand> {
    match command {
        IrcCommand::ClientBatch(commands) | IrcCommand::ClientChat { commands, .. } => commands
            .into_iter()
            .flat_map(flatten_test_commands)
            .collect(),
        command => vec![command],
    }
}

#[cfg(test)]
mod recovery_and_echo_regressions {
    use super::*;

    fn session() -> (Session, mpsc::UnboundedReceiver<IrcCommand>) {
        let mut app = Session::with_settings(Settings {
            nickname: "me".into(),
            auto_join_channels: "#configured".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        assert!(app.start_session_from_form());
        app.test_set_connected(true);
        app.test_set_connecting_flag(false);
        let rx = transport(&mut app);
        (app, rx)
    }

    fn transport(app: &mut Session) -> mpsc::UnboundedReceiver<IrcCommand> {
        let (tx, rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        rx
    }

    fn feed(app: &mut Session, line: &str) {
        app.handle_incoming_message(IrcMessage::parse(line).unwrap());
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<IrcCommand>) -> Vec<IrcCommand> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .flat_map(flatten_test_commands)
            .collect()
    }

    fn joins(rx: &mut mpsc::UnboundedReceiver<IrcCommand>) -> Vec<(String, Option<String>)> {
        drain(rx)
            .into_iter()
            .filter_map(|cmd| match cmd {
                IrcCommand::Join(name, key, _, _) => Some((name, key)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn automatic_and_explicit_reconnect_restore_actual_keyed_memberships() {
        for explicit in [false, true] {
            let (mut app, mut rx) = session();
            app.process_command("/join #manual secret-key");
            feed(&mut app, ":me!u@h JOIN #manual");
            feed(&mut app, ":me!u@h JOIN #kicked");
            feed(&mut app, ":op!u@h KICK #kicked me :bye");
            feed(&mut app, ":me!u@h JOIN #parted");
            feed(&mut app, ":me!u@h PART #parted");
            app.open_query("alice");
            drain(&mut rx);
            if explicit {
                app.request_reconnect_after_close("Reconnecting");
                // The lifecycle clears this after the old worker closes.
                app.test_finish_close();
            } else {
                app.mark_connection_lost();
            }
            // Repeated pre-registration failures must retain the original key.
            for _ in 0..2 {
                app.begin_connection_attempt();
                app.mark_connection_lost();
            }
            let mut rx = transport(&mut app);
            feed(&mut app, ":srv 001 me :Welcome back");
            assert_eq!(
                joins(&mut rx),
                [("#manual".into(), Some("secret-key".into()))]
            );
            feed(&mut app, ":me!u@h JOIN #manual");
            assert!(app.conversations["#manual"].joined);
            assert!(app.send_privmsg_text("#manual", "hello").complete);
        }
    }

    #[test]
    fn recovery_survives_unconfirmed_joins_and_update_restart() {
        let (mut app, mut rx) = session();
        app.process_command("/join #first first-key");
        feed(&mut app, ":me!u@h JOIN #first");
        app.process_command("/join #pending pending-key");
        app.mark_connection_lost();
        app.begin_connection_attempt();
        let mut recovered = transport(&mut app);
        feed(&mut app, ":srv 001 me :Welcome back");
        assert_eq!(joins(&mut recovered).len(), 2);
        feed(&mut app, ":me!u@h JOIN #first");
        // A second loss happens before #pending's JOIN is confirmed.
        app.mark_connection_lost();
        app.test_set_connecting_flag(true);
        let snapshot = app.update_session();
        let mut restored = Session::with_settings(Settings {
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        restored.restore_update_session(snapshot);
        restored.begin_connection_attempt();
        let mut wire = transport(&mut restored);
        feed(&mut restored, ":srv 001 me :Welcome after update");
        let mut sent = joins(&mut wire);
        sent.sort();
        assert_eq!(
            sent,
            [
                ("#first".into(), Some("first-key".into())),
                ("#pending".into(), Some("pending-key".into())),
            ]
        );
        drain(&mut rx);
    }

    #[test]
    fn update_snapshot_prefers_confirmed_key_after_an_ignored_redundant_join() {
        let (mut app, _rx) = session();
        app.process_command("/join #room real-key");
        feed(&mut app, ":me!u@h JOIN #room");
        // Servers ignore JOIN for a channel we already belong to.
        app.process_command("/join #room wrong-key");
        let snapshot = app.update_session();
        let (_, key, joined) = snapshot
            .tabs
            .iter()
            .find(|(name, _, _)| name == "#room")
            .unwrap();
        assert_eq!(key.as_deref(), Some("real-key"));
        assert!(*joined);
    }

    #[test]
    fn recovery_cancellation_and_initial_registration_keep_autojoin_semantics() {
        let (mut app, _rx) = session();
        feed(&mut app, ":me!u@h JOIN #configured");
        feed(&mut app, ":me!u@h PART #configured");
        app.mark_connection_lost();
        let mut wire = transport(&mut app);
        feed(&mut app, ":srv 001 me :Welcome back");
        assert!(
            joins(&mut wire).is_empty(),
            "do not resurrect a deliberately parted channel"
        );
        app.request_manual_disconnect(None);
        assert!(app.pending_rejoins.is_none());
        app.test_set_connecting_flag(true);
        app.test_finish_close();
        app.mark_connection_lost();
        assert!(
            app.pending_rejoins.is_none(),
            "initial failure must not suppress auto-join"
        );
        let mut wire = transport(&mut app);
        feed(&mut app, ":srv 001 me :Welcome");
        assert_eq!(joins(&mut wire), [("#configured".into(), None)]);
        // The initial auto-join itself must survive a loss before its echo.
        app.mark_connection_lost();
        app.begin_connection_attempt();
        let mut wire = transport(&mut app);
        feed(&mut app, ":srv 001 me :Welcome again");
        assert_eq!(joins(&mut wire), [("#configured".into(), None)]);
    }

    #[test]
    fn closed_refused_and_foreign_endpoint_memberships_do_not_reappear() {
        let (mut app, _rx) = session();
        for name in ["#kept", "#closed", "#denied"] {
            feed(&mut app, &format!(":me!u@h JOIN {name}"));
        }
        app.mark_connection_lost();
        app.close_tab("#closed");
        let mut wire = transport(&mut app);
        feed(&mut app, ":srv 001 me :Welcome back");
        joins(&mut wire);
        feed(&mut app, ":srv 475 me #denied :Bad key");
        app.mark_connection_lost();
        let mut wire = transport(&mut app);
        feed(&mut app, ":srv 001 me :Welcome again");
        assert_eq!(joins(&mut wire), [("#kept".into(), None)]);
        app.server_host = "different.example".into();
        app.auto_join_channels.clear();
        assert!(app.switch_session_from_form("Switching"));
        app.test_finish_close();
        app.begin_connection_attempt();
        let mut wire = transport(&mut app);
        feed(&mut app, ":srv 001 me :Different network");
        assert!(joins(&mut wire).is_empty());
    }

    fn echo(app: &mut Session, wire: &[IrcCommand]) {
        for (index, command) in wire.iter().enumerate() {
            let tags = format!("time=2026-11-14T12:00:00.000Z;msgid=echo{index}");
            let line = if let IrcCommand::Raw(raw) = command {
                if let Some((existing, rest)) =
                    raw.strip_prefix('@').and_then(|line| line.split_once(' '))
                {
                    format!("@{tags};{existing} :me!u@h {rest}")
                } else {
                    format!("@{tags} :me!u@h {raw}")
                }
            } else {
                format!("@{tags} :me!u@h {command}")
            };
            feed(app, &line);
        }
    }

    #[test]
    fn all_chat_entrypoints_echo_actual_fallback_chunks_once() {
        for command in [
            "",
            "/msg alice ",
            "/query alice ",
            "/say ",
            "/me ",
            "/describe alice ",
            "/slap alice ",
            "/amsg ",
            "/ame ",
        ] {
            let (mut app, mut rx) = session();
            feed(&mut app, ":me!u@h JOIN #room");
            feed(&mut app, ":srv CAP me ACK :echo-message");
            app.input_text = format!("{command}{}", "🙂word".repeat(150));
            app.process_input();
            let wire = drain(&mut rx);
            assert!(wire.len() > 1, "{command}");
            assert!(
                wire.iter()
                    .all(|cmd| matches!(cmd, IrcCommand::Privmsg(_, _)))
            );
            echo(&mut app, &wire);
            let rows: Vec<_> = app
                .conversations
                .values()
                .flat_map(|channel| &channel.messages)
                .filter(|row| !row.is_system)
                .collect();
            assert_eq!(rows.len(), wire.len(), "{command}");
            assert!(
                rows.iter()
                    .all(|row| !row.pending_echo && row.msgid.is_some()),
                "{command}"
            );
        }
    }

    #[test]
    fn more_than_32_chunks_reconcile_and_acquire_server_identity() {
        let (mut app, mut rx) = session();
        feed(&mut app, ":srv CAP me ACK :echo-message");
        app.process_command(&format!("/msg alice {}", "x".repeat(20_000)));
        let wire = drain(&mut rx);
        assert!(wire.len() > 32);
        echo(&mut app, &wire);
        assert_eq!(app.conversations["alice"].messages.len(), wire.len());
        assert!(
            app.conversations["alice"]
                .messages
                .iter()
                .all(|row| !row.pending_echo && row.msgid.is_some())
        );
    }

    #[test]
    fn a_new_connection_does_not_confirm_an_old_identical_pending_row() {
        let (mut app, mut rx) = session();
        feed(&mut app, ":srv CAP me ACK :echo-message");
        app.process_command("/msg alice same text");
        drain(&mut rx);
        app.mark_connection_lost();
        app.begin_connection_attempt();
        let mut rx = transport(&mut app);
        feed(&mut app, ":srv 001 me :Welcome back");
        drain(&mut rx);
        feed(&mut app, ":srv CAP me ACK :echo-message");
        app.process_command("/msg alice same text");
        echo(&mut app, &drain(&mut rx));
        let rows = &app.conversations["alice"].messages;
        assert_eq!(rows.len(), 2);
        assert!(rows[0].msgid.is_none());
        assert!(rows[1].msgid.is_some());
        assert!(rows.iter().all(|row| !row.pending_echo));
    }

    #[test]
    fn multiline_and_limit_fallback_echo_the_transmitted_shape() {
        for fallback in [false, true] {
            for action in [false, true] {
                let (mut app, mut rx) = session();
                feed(&mut app, ":srv CAP me ACK :echo-message draft/multiline");
                if fallback {
                    app.network_support.multiline_max_lines = Some(1);
                }
                let text = format!("{}\nsecond", "x".repeat(700));
                assert!(app.send_chat_text("alice", &text, action));
                let wire = drain(&mut rx);
                let expected = if fallback { wire.len() } else { 1 };
                echo(&mut app, &wire);
                let rows = &app.conversations["alice"].messages;
                assert_eq!(rows.len(), expected);
                assert!(rows.iter().all(|row| row.is_action == action
                    && !row.pending_echo
                    && row.msgid.is_some()));
            }
        }
    }

    #[test]
    fn notices_confirm_each_chunk_and_suppress_live_echoes_including_multiline() {
        for multiline in [false, true] {
            let (mut app, mut rx) = session();
            feed(&mut app, ":me!u@h JOIN #room");
            feed(&mut app, ":srv CAP me ACK :echo-message");
            if multiline {
                feed(&mut app, ":srv CAP me ACK :draft/multiline");
            }
            app.process_command(&format!("/notice alice {}", "n".repeat(700)));
            let wire = drain(&mut rx);
            let confirmations = app.conversations["#room"]
                .messages
                .iter()
                .filter(|row| row.content.starts_with("-> -alice- "))
                .count();
            assert_eq!(confirmations, if multiline { 1 } else { wire.len() });
            let before = app
                .conversations
                .values()
                .map(|channel| channel.messages.len())
                .sum::<usize>();
            echo(&mut app, &wire);
            let after = app
                .conversations
                .values()
                .map(|channel| channel.messages.len())
                .sum::<usize>();
            assert_eq!(before, after);
        }
    }

    #[test]
    fn invalid_pastes_and_fragmented_credentials_never_queue_partial_input() {
        for (target, text) in [
            ("alice", "valid\ninvalid\0".into()),
            ("NickServ", format!("IDENTIFY {}", "s".repeat(700))),
        ] {
            let (mut app, mut rx) = session();
            app.process_command(&format!("/msg {target} {text}"));
            assert!(drain(&mut rx).is_empty());
            assert!(
                app.conversations
                    .values()
                    .flat_map(|channel| &channel.messages)
                    .all(|row| !row.content.contains(&"s".repeat(32)))
            );
        }
        let (mut app, rx) = session();
        drop(rx);
        assert!(!app.send_chat_text("alice", "not queued", false));
        assert!(!app.conversations.contains_key("alice"));
    }

    #[test]
    fn multiline_service_credentials_are_redacted_on_every_line_and_echo_path() {
        for action in [false, true] {
            let (mut app, mut rx) = session();
            feed(&mut app, ":srv CAP me ACK :echo-message draft/multiline");
            app.open_query("NickServ");
            app.input_text = if action {
                "/me hello\nIDENTIFY secret-on-second-line".into()
            } else {
                "/msg NickServ hello\nIDENTIFY secret-on-second-line".into()
            };
            app.process_input();
            assert!(app.command_history.is_empty());
            let wire = drain(&mut rx);
            assert!(wire.len() > 2);
            echo(&mut app, &wire);
            assert!(app.conversations["NickServ"].messages.iter().all(|row| {
                !row.content.contains("secret-on-second-line")
                    && (!row.content.contains("credential command") || row.no_log)
            }));
        }
    }
}

#[cfg(test)]
mod history_interleaving_tests {
    use super::*;

    fn feed(app: &mut Session, line: &str) {
        app.handle_incoming_message(IrcMessage::parse(line).unwrap());
    }

    fn session(page_limit: usize) -> (Session, mpsc::UnboundedReceiver<IrcCommand>) {
        let mut app = Session::with_settings(Settings {
            nickname: "me".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        assert!(app.start_session_from_form());
        app.test_set_connected(true);
        app.test_set_connecting_flag(false);
        let (tx, rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        feed(
            &mut app,
            &format!(":srv CAP me ACK :draft/chathistory={page_limit} draft/read-marker"),
        );
        (app, rx)
    }

    fn message(app: &mut Session, batch: Option<&str>, second: usize, id: &str) {
        let batch = batch.map(|id| format!(";batch={id}")).unwrap_or_default();
        feed(
            app,
            &format!(
                "@time=2026-11-14T12:00:{second:02}.000Z;msgid={id}{batch} \
                 :alice!u@h PRIVMSG #room :{id}"
            ),
        );
    }

    fn ids(app: &Session) -> Vec<String> {
        app.conversations["#room"]
            .messages
            .iter()
            .filter_map(|message| message.msgid.clone())
            .collect()
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<IrcCommand>) -> Vec<IrcCommand> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn catch_up_rows_precede_interleaved_live_traffic_without_rewinding_read_marker() {
        let (mut app, mut rx) = session(100);
        message(&mut app, None, 0, "m0");
        app.process_command("/history after #room msgid=m0 10");
        app.select_target("#room");
        app.reading_conversation = app.current_conversation;
        feed(&mut app, ":srv BATCH +h chathistory #room");
        message(&mut app, Some("h"), 1, "m1");
        message(&mut app, None, 3, "m3");
        message(&mut app, Some("h"), 2, "m2");
        feed(&mut app, ":srv BATCH -h");

        assert_eq!(ids(&app), ["m0", "m1", "m2", "m3"]);
        let markers: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|command| match command {
                IrcCommand::Markread(_, marker) => marker,
                _ => None,
            })
            .collect();
        assert_eq!(markers, ["timestamp=2026-11-14T12:00:03.000Z"]);
        assert_eq!(
            app.conversations["#room"].read_marker.as_deref(),
            Some("timestamp=2026-11-14T12:00:03.000Z")
        );
    }

    #[test]
    fn subsequent_catch_up_pages_merge_before_live_rows_and_keep_page_cursors() {
        let (mut app, mut rx) = session(2);
        app.window_focused = false;
        message(&mut app, None, 0, "m0");
        app.conversations.get_mut("#room").unwrap().unread = 0;
        assert!(app.request_latest_history("#room", Some(4), HistoryPlacement::Append));
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryAfter("#room".into(), "msgid=m0".into(), 2)
        );

        feed(&mut app, ":srv BATCH +first chathistory #room");
        message(&mut app, Some("first"), 2, "m2");
        message(&mut app, None, 5, "m5");
        message(&mut app, Some("first"), 1, "m1");
        feed(&mut app, ":srv BATCH -first");
        assert_eq!(ids(&app), ["m0", "m1", "m2", "m5"]);
        assert_eq!(app.conversations["#room"].unread, 3);
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::ChathistoryAfter("#room".into(), "msgid=m2".into(), 2)
        );

        feed(&mut app, ":srv BATCH +second chathistory #room");
        message(&mut app, Some("second"), 4, "m4");
        message(&mut app, Some("second"), 3, "m3");
        feed(&mut app, ":srv BATCH -second");
        assert_eq!(ids(&app), ["m0", "m1", "m2", "m3", "m4", "m5"]);
        assert_eq!(app.conversations["#room"].unread, 5);
        assert!(app.pending_history_requests.is_empty());
        assert!(drain(&mut rx).is_empty());
        assert!(app.conversations["#room"].messages.iter().any(|row| {
            row.is_system && row.content.contains("4 lines of server history loaded")
        }));
    }

    #[test]
    fn live_overlap_is_deduplicated_and_scrollback_keeps_the_newest_live_row() {
        let (mut app, mut rx) = session(100);
        app.window_focused = false;
        app.max_scrollback = 4;
        message(&mut app, None, 0, "m0");
        app.conversations.get_mut("#room").unwrap().unread = 0;
        assert!(app.request_latest_history("#room", Some(3), HistoryPlacement::Append));
        feed(&mut app, ":srv BATCH +h chathistory #room");
        message(&mut app, Some("h"), 1, "m1");
        message(&mut app, None, 3, "m3");
        message(&mut app, Some("h"), 3, "m3");
        message(&mut app, Some("h"), 2, "m2");
        feed(&mut app, ":srv BATCH -h");

        assert_eq!(ids(&app), ["m1", "m2", "m3"]);
        assert_eq!(app.conversations["#room"].messages.len(), 4);
        assert_eq!(
            app.conversations["#room"]
                .messages
                .back()
                .unwrap()
                .msgid
                .as_deref(),
            Some("m3")
        );
        assert_eq!(app.conversations["#room"].unread, 3);
        assert_eq!(
            drain(&mut rx).len(),
            1,
            "overlap still counts toward the page limit"
        );
    }

    #[test]
    fn automatic_read_markers_use_the_newest_time_and_preserve_a_newer_server_marker() {
        let (mut app, mut rx) = session(100);
        app.window_focused = false;
        message(&mut app, None, 12, "newest");
        message(&mut app, None, 11, "last-arrived");
        app.conversations.get_mut("#room").unwrap().read_marker =
            Some("timestamp=2026-11-14T12:00:10.000Z".into());
        app.mark_target_read("#room");
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Markread(
                "#room".into(),
                Some("timestamp=2026-11-14T12:00:12.000Z".into())
            )
        );
        let channel = app.conversations.get_mut("#room").unwrap();
        channel.messages.clear();
        channel.messages.push_back(
            ChatMessage::new_fmt("alice", "older retained row", "short").with_irc_metadata(
                Some("2026-11-14T12:00:04.000Z".into()),
                Some("older".into()),
                None,
            ),
        );
        app.mark_target_read("#room");
        assert!(drain(&mut rx).is_empty());
        assert_eq!(
            app.conversations["#room"].read_marker.as_deref(),
            Some("timestamp=2026-11-14T12:00:12.000Z")
        );
    }

    #[test]
    fn page_ordering_keeps_untagged_rows_in_their_arrival_slots() {
        let (mut app, _rx) = session(100);
        app.process_command("/history #room 10");
        feed(&mut app, ":srv BATCH +h chathistory #room");
        message(&mut app, Some("h"), 5, "m5");
        feed(
            &mut app,
            "@batch=h;msgid=untagged-a :alice!u@h PRIVMSG #room :untagged A",
        );
        message(&mut app, Some("h"), 1, "m1");
        feed(
            &mut app,
            "@batch=h;msgid=untagged-b :alice!u@h PRIVMSG #room :untagged B",
        );
        message(&mut app, Some("h"), 3, "m3");
        feed(&mut app, ":srv BATCH -h");
        assert_eq!(ids(&app), ["m1", "untagged-a", "m3", "untagged-b", "m5"]);
    }

    #[test]
    fn catch_up_preserves_untagged_rows_with_their_existing_stream_anchors() {
        let (mut app, _rx) = session(100);
        message(&mut app, None, 0, "m0");
        app.conversations
            .get_mut("#room")
            .unwrap()
            .messages
            .push_back(ChatMessage::system_fmt("local before live", "short"));
        message(&mut app, None, 3, "m3");
        app.conversations
            .get_mut("#room")
            .unwrap()
            .messages
            .push_back(ChatMessage::system_fmt("local after live", "short"));
        app.process_command("/history after #room msgid=m0 10");
        feed(&mut app, ":srv BATCH +h chathistory #room");
        message(&mut app, Some("h"), 2, "m2");
        message(&mut app, Some("h"), 1, "m1");
        feed(&mut app, ":srv BATCH -h");
        let contents: Vec<_> = app.conversations["#room"]
            .messages
            .iter()
            .filter_map(|row| {
                (row.msgid.is_some() || row.content.starts_with("local "))
                    .then_some(row.content.as_str())
            })
            .collect();
        assert_eq!(
            contents,
            [
                "m0",
                "local before live",
                "m1",
                "m2",
                "m3",
                "local after live"
            ]
        );
    }
}

#[cfg(test)]
mod leave_and_handoff_regressions {
    use super::*;

    fn session() -> (Session, mpsc::UnboundedReceiver<IrcCommand>) {
        let mut app = Session::with_settings(Settings {
            server_host: "a.example".into(),
            nickname: "me".into(),
            auto_join_channels: String::new(),
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        assert!(app.start_session_from_form());
        app.test_set_connected(true);
        app.test_set_connecting_flag(false);
        let rx = transport(&mut app);
        app.process_command("/join #room room-key");
        feed(&mut app, ":me!u@h JOIN #room");
        (app, rx)
    }

    fn transport(app: &mut Session) -> mpsc::UnboundedReceiver<IrcCommand> {
        let (tx, rx) = mpsc::unbounded_channel();
        app.test_set_sender(tx);
        rx
    }

    fn feed(app: &mut Session, line: &str) {
        app.handle_incoming_message(IrcMessage::parse(line).unwrap());
    }

    fn joins(rx: &mut mpsc::UnboundedReceiver<IrcCommand>) -> Vec<(String, Option<String>)> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|cmd| match cmd {
                IrcCommand::Join(target, key, _, _) => Some((target, key)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn part_and_window_close_exclude_unacknowledged_membership_from_recovery() {
        for command in ["/part #room", "/raw PART #room", "/raw JOIN 0", "/close"] {
            let (mut app, _rx) = session();
            app.process_command(command);
            assert!(app.is_parting("#ROOM"));
            let snapshot = app.update_session();
            assert!(
                !snapshot
                    .tabs
                    .iter()
                    .find(|(name, _, _)| name == "#room")
                    .unwrap()
                    .2
            );
            app.mark_connection_lost();
            app.begin_connection_attempt();
            let mut rx = transport(&mut app);
            feed(&mut app, ":srv 001 me :Welcome back");
            assert!(joins(&mut rx).is_empty(), "{command}");
        }
    }

    #[test]
    fn part_during_registration_cancels_recovery_join() {
        let (mut app, _rx) = session();
        app.mark_connection_lost();
        app.begin_connection_attempt();
        let mut rx = transport(&mut app);
        app.test_set_connecting_flag(true);
        app.process_command("/part #room");
        feed(&mut app, ":srv 001 me :Welcome back");
        assert!(joins(&mut rx).is_empty());
    }

    #[test]
    fn failed_part_does_not_discard_confirmed_membership() {
        let (mut app, _rx) = session();
        app.process_command(&format!("/part #room {}", "x".repeat(600)));
        assert!(!app.is_parting("#room"));
        app.mark_connection_lost();
        app.begin_connection_attempt();
        let mut rx = transport(&mut app);
        feed(&mut app, ":srv 001 me :Welcome back");
        assert_eq!(joins(&mut rx), [("#room".into(), Some("room-key".into()))]);
    }

    #[test]
    fn cycle_and_explicit_rejoin_supersede_leave_intent_even_before_join_echo() {
        for command in ["/cycle", "/part #room"] {
            let (mut app, _rx) = session();
            app.process_command(command);
            if command.starts_with("/part") {
                app.process_command("/join #room room-key");
            }
            assert!(!app.is_parting("#room"));
            feed(&mut app, ":me!u@h PART #room");
            // The JOIN following PART has not been confirmed yet.
            app.mark_connection_lost();
            app.begin_connection_attempt();
            let mut rx = transport(&mut app);
            feed(&mut app, ":srv 001 me :Welcome back");
            assert_eq!(joins(&mut rx), [("#room".into(), Some("room-key".into()))]);
        }
    }

    #[test]
    fn update_during_explicit_reconnect_keeps_nick_intent_and_memberships() {
        let (mut app, _rx) = session();
        app.nickname = "next-nick".into();
        assert!(app.switch_session_from_form("Reconnecting"));
        assert!(app.is_closing() && app.has_pending_connect());
        let snapshot = app.update_session();
        assert!(snapshot.reconnect);
        assert_eq!(snapshot.session.as_ref().unwrap().server.nick, "next-nick");
        let mut after = Session::with_settings(Settings {
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        after.restore_update_session(snapshot);
        assert!(after.is_connecting());
        after.begin_connection_attempt();
        let mut rx = transport(&mut after);
        feed(&mut after, ":srv 001 next-nick :Welcome back");
        assert_eq!(joins(&mut rx), [("#room".into(), Some("room-key".into()))]);
    }

    #[test]
    fn update_during_server_switch_preserves_pending_endpoint_without_old_secrets() {
        let (mut app, _rx) = session();
        app.input_text = "old-network draft".into();
        app.away_status = Some("old-network away".into());
        app.server_host = "b.example".into();
        app.nickname = "new-nick".into();
        app.auto_join_channels = "#new new-key".into();
        assert!(app.switch_session_from_form("Changing servers"));
        assert!(app.update_changes_endpoint());
        let snapshot = app.update_session();
        assert!(snapshot.reconnect);
        assert_eq!(snapshot.session.as_ref().unwrap().server.host, "b.example");
        assert_eq!(snapshot.session.as_ref().unwrap().server.nick, "new-nick");
        assert!(snapshot.tabs.is_empty() && snapshot.selected.is_none());
        assert!(snapshot.input.is_empty() && snapshot.away.is_none());
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("room-key") && !json.contains("old-network"));
        let mut after = Session::with_settings(Settings {
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        after.restore_update_session(snapshot);
        after.begin_connection_attempt();
        let mut rx = transport(&mut after);
        feed(&mut after, ":srv 001 new-nick :Welcome to B");
        assert_eq!(joins(&mut rx), [("#new".into(), Some("new-key".into()))]);
    }

    #[test]
    fn update_during_initial_registration_preserves_configured_autojoin() {
        let mut app = Session::with_settings(Settings {
            nickname: "me".into(),
            auto_join_channels: "#initial initial-key".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        assert!(app.start_session_from_form());
        assert!(app.is_connecting() && !app.is_connected());
        let snapshot = app.update_session();
        assert!(snapshot.fresh_connect);
        let mut after = Session::with_settings(Settings {
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        after.restore_update_session(snapshot);
        after.begin_connection_attempt();
        let mut rx = transport(&mut after);
        feed(&mut after, ":srv 001 me :Welcome");
        assert_eq!(
            joins(&mut rx),
            [("#initial".into(), Some("initial-key".into()))]
        );
    }

    #[test]
    fn catch_up_repairs_optimistic_echo_timestamp_inversion() {
        let row = |id: &str, second| {
            ChatMessage::new_fmt("peer", id, "short").with_irc_metadata(
                Some(format!("2026-11-14T12:00:0{second}.000Z")),
                Some(id.into()),
                None,
            )
        };
        let mut shown = VecDeque::from([row("own-echo", 3), row("peer", 2)]);
        Session::merge_catch_up_history(&mut shown, VecDeque::from([row("history", 1)]));
        assert_eq!(
            shown
                .iter()
                .filter_map(|row| row.msgid.as_deref())
                .collect::<Vec<_>>(),
            ["history", "peer", "own-echo"]
        );
    }
}

#[cfg(test)]
mod conversation_identity_tests {
    use super::*;

    fn session() -> Session {
        let mut session = Session::with_settings(Settings {
            server_host: "one.example".into(),
            nickname: "me".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        assert!(session.start_session_from_form());
        session
    }

    #[test]
    fn peer_nick_and_case_changes_keep_conversation_and_selection() {
        let mut session = session();
        assert!(session.open_query("alice"));
        let original = session.current_conversation.unwrap();
        session.handle_incoming_message(IrcMessage::parse(":alice!a@h NICK bob").unwrap());
        assert_eq!(session.id_for_target("bob"), Some(original));
        assert_eq!(session.current_conversation, Some(original));
        session.handle_incoming_message(IrcMessage::parse(":bob!a@h NICK BOB").unwrap());
        assert_eq!(session.target_name(original), Some("BOB"));
        assert_eq!(session.current_conversation, Some(original));
        assert!(
            !session
                .events
                .iter()
                .any(|event| matches!(event, SessionEvent::ConversationMerged { .. }))
        );
    }

    #[test]
    fn explicit_rename_keeps_source_and_redirects_destination_selection() {
        let mut session = session();
        let destination = session.ensure_target("#destination");
        let source = session.ensure_target("#source");
        session
            .conversation_channel_mut(destination)
            .unwrap()
            .messages
            .push_back(ChatMessage::system("destination history"));
        session
            .conversation_channel_mut(source)
            .unwrap()
            .messages
            .push_back(ChatMessage::system("source history"));
        session.select_conversation(Some(destination));
        session.handle_incoming_message(
            IrcMessage::parse(":server RENAME #source #destination :moving").unwrap(),
        );
        assert_eq!(session.current_conversation, Some(source));
        assert_eq!(session.id_for_target("#destination"), Some(source));
        assert!(session.target_name(destination).is_none());
        let rows = &session.conversation_channel(source).unwrap().messages;
        assert!(rows.iter().any(|row| row.content == "source history"));
        assert!(rows.iter().any(|row| row.content == "destination history"));
        assert_eq!(session.events.iter().filter(|event| matches!(event, SessionEvent::ConversationMerged { survivor, retired } if *survivor == source && *retired == destination)).count(), 1);
    }

    #[test]
    fn stale_selection_cannot_resolve_a_reopened_name_or_another_endpoint() {
        let mut session = session();
        let first = session.ensure_target("alice");
        session.close_tab("alice");
        let second = session.ensure_target("alice");
        assert_ne!(first, second);
        assert!(!session.select_conversation(Some(first)));
        assert!(session.select_conversation(Some(second)));
        session.apply_unprofiled_endpoint("two.example", "6697", true);
        assert!(session.switch_session_from_form("Changing servers"));
        let third = session.ensure_target("alice");
        assert_ne!(second, third);
        assert!(!session.select_conversation(Some(second)));
        assert!(
            session
                .events
                .iter()
                .any(|event| matches!(event, SessionEvent::EndpointChanged))
        );
    }

    #[test]
    fn legacy_snapshot_keeps_exact_names_until_explicit_casemapping() {
        let mut before = session();
        before.network_support.case_mapping = CaseMapping::Ascii;
        before
            .conversations
            .provisional_support(&before.network_support);
        before.ensure_target("#[room");
        before.ensure_target("#{room");
        before.select_target("#{room");
        let json = serde_json::to_string(&before.update_session()).unwrap();
        assert!(!json.contains("ConversationId"));
        let mut after = Session::with_settings(Settings {
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        after.restore_update_session(serde_json::from_str(&json).unwrap());
        assert_eq!(after.conversations.len(), 2);
        assert_eq!(after.current_target_name(), Some("#{room"));
        after.handle_incoming_message(
            IrcMessage::parse(":srv 005 me CASEMAPPING=ascii :supported").unwrap(),
        );
        assert_eq!(after.conversations.len(), 2);
        let oldest = after.conversations.ids().next().unwrap();
        after.handle_incoming_message(
            IrcMessage::parse(":srv 005 me CASEMAPPING=rfc1459 :supported").unwrap(),
        );
        assert_eq!(after.conversations.len(), 1);
        assert_eq!(after.current_conversation, Some(oldest));
    }

    #[test]
    fn casemapping_merge_preserves_live_metadata_when_oldest_tab_was_unjoined() {
        let mut session = session();
        session.network_support.case_mapping = CaseMapping::Ascii;
        session
            .conversations
            .provisional_support(&session.network_support);
        let oldest = session.ensure_target("#[room");
        let live = session.ensure_target("#{room");
        let channel = session.conversation_channel_mut(live).unwrap();
        channel.joined = true;
        channel.apply_channel_mode('+', 'k', Some("channel-key"));
        channel.apply_channel_mode('+', 'n', None);
        channel.add_user("alice", UserMode::Op);
        channel.set_user_realname("alice", Some("Alice".into()));
        session.select_conversation(Some(live));
        session.handle_incoming_message(
            IrcMessage::parse(":srv 005 me CASEMAPPING=rfc1459 :supported").unwrap(),
        );
        assert_eq!(session.current_conversation, Some(oldest));
        let merged = session.conversation_channel(oldest).unwrap();
        assert!(merged.joined);
        assert_eq!(merged.key.as_deref(), Some("channel-key"));
        assert!(merged.modes.contains('k') && merged.modes.contains('n'));
        assert_eq!(
            merged.get_user("alice").unwrap().realname.as_deref(),
            Some("Alice")
        );
        assert_eq!(merged.get_user("alice").unwrap().mode, UserMode::Op);
    }

    #[test]
    fn retained_kick_and_reconnect_keep_conversation_ids() {
        let mut session = session();
        let (tx, _rx) = mpsc::unbounded_channel();
        session.test_set_registered(tx);
        session.handle_incoming_message(IrcMessage::parse(":me!u@h JOIN #room").unwrap());
        let original = session.id_for_target("#room").unwrap();
        session.handle_incoming_message(IrcMessage::parse(":op!u@h KICK #room me :bye").unwrap());
        assert_eq!(session.id_for_target("#room"), Some(original));
        assert!(!session.conversation_channel(original).unwrap().joined);
        session.request_reconnect_after_close("Reconnecting");
        assert_eq!(session.id_for_target("#room"), Some(original));
    }
}
