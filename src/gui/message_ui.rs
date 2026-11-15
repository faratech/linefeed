//! Per-window message interaction, logical chat sends, and transient typing.
use super::conversations::ConversationId;
use super::identity::{MessageRef, ScrollAnchor};
use super::{ChatMessage, Session, SessionEvent};
use crate::irc::{IrcCommand, IrcMessage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum MessageDelivery {
    #[default]
    None,
    Queued {
        id: String,
        attempt: u64,
    },
    Written {
        attempt: u64,
    },
    Confirmed,
    Uncertain,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionGroup {
    pub emoji: String,
    pub actors: Vec<String>,
}
#[derive(Clone, Debug)]
pub(crate) struct ComposerState {
    pub nonce: String,
    pub reply_to: Option<MessageRef>,
    pub cursor: Option<usize>,
    pub ime_active: bool,
}
impl ComposerState {
    pub(crate) fn insert(&mut self, body: &mut String, text: &str) {
        let cursor = self
            .cursor
            .unwrap_or_else(|| body.chars().count())
            .min(body.chars().count());
        let byte = body
            .char_indices()
            .nth(cursor)
            .map_or(body.len(), |(byte, _)| byte);
        body.insert_str(byte, text);
        self.cursor = Some(cursor + text.chars().count());
    }
}
impl Default for ComposerState {
    fn default() -> Self {
        Self {
            nonce: super::new_profile_id(),
            reply_to: None,
            cursor: None,
            ime_active: false,
        }
    }
}
#[derive(Default)]
pub(crate) struct TimelineView {
    pub anchor: Option<ScrollAnchor>,
    pub last_read: Option<MessageRef>,
    pub entry_read_boundary: Option<MessageRef>,
    pub restore_pending: bool,
    pub hydrating: bool,
    pub restored_hold: bool,
    pub reveal: Option<String>,
    pub pending_offset: Option<f32>,
    pub visible_read: Option<MessageRef>,
    pub actions: Vec<MessageAction>,
}
#[derive(Clone)]
pub(crate) enum MessageAction {
    Reply(String),
    React(String, String),
    Reveal(String),
}
struct OutgoingTyping {
    last_sent: Instant,
    last_edit: Instant,
    paused: bool,
    done: bool,
}
type PendingReactions = HashMap<(String, String), Vec<(String, String, bool)>>;
#[derive(Default)]
pub(crate) struct MessageUiState {
    typing: HashMap<(ConversationId, String), (String, Instant, bool)>,
    outgoing: HashMap<ConversationId, OutgoingTyping>,
    pending_reactions: PendingReactions,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatKind {
    Privmsg,
    Action,
    Notice,
}
pub(crate) struct ChatSendRequest {
    pub target: ConversationId,
    pub body: String,
    pub kind: ChatKind,
    pub reply_to: Option<MessageRef>,
}
pub(crate) struct PreparedChatSend {
    id: String,
    target: ConversationId,
    network: super::identity::LogicalNetworkKey,
    generation: u64,
    wire: Vec<IrcCommand>,
    payloads: Vec<String>,
    kind: ChatKind,
    reply: Option<String>,
    sensitive: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SendError {
    Disconnected,
    ClosedConversation,
    NotJoined,
    InvalidReply,
    InvalidMessage,
    TooLong,
    SensitiveSplit,
}
impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disconnected => "Not connected",
            Self::ClosedConversation => "Conversation was closed",
            Self::NotJoined => "You are not joined to this channel",
            Self::InvalidReply => {
                "Reply unavailable: it needs a server message ID in this conversation"
            }
            Self::InvalidMessage => "Message contains invalid IRC characters",
            Self::TooLong => "Message cannot fit the server's limits",
            Self::SensitiveSplit => "Credential commands must fit in one IRC message",
        })
    }
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SendReceipt {
    Queued,
    Rejected(SendError),
}
fn tag(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(';', "\\:")
        .replace(' ', "\\s")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}
fn split_utf8(text: &str, max: usize) -> Result<Vec<String>, SendError> {
    if max == 0 {
        return Err(SendError::TooLong);
    }
    if text.is_empty() {
        return Ok(vec![String::new()]);
    }
    let mut rest = text;
    let mut parts = Vec::new();
    while !rest.is_empty() {
        let mut end = rest.len().min(max);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            return Err(SendError::TooLong);
        }
        parts.push(rest[..end].into());
        rest = &rest[end..];
    }
    Ok(parts)
}
impl Session {
    pub(crate) fn live_message_reference(&self, target: &str, message: &ChatMessage) -> MessageRef {
        MessageRef {
            conversation: self.conversation_key(target),
            archive_id: Some(message.message_key.clone()),
            server_msgid: message.msgid.clone(),
            server_time: message.server_time.clone(),
        }
    }
    pub(crate) fn plan_chat_send(
        &self,
        request: &ChatSendRequest,
    ) -> Result<PreparedChatSend, SendError> {
        let target = self
            .target_name(request.target)
            .ok_or(SendError::ClosedConversation)?;
        let generation = self
            .connection
            .generation()
            .ok_or(SendError::Disconnected)?;
        if !self.is_connected() || self.connection.sender().is_none_or(|tx| tx.is_closed()) {
            return Err(SendError::Disconnected);
        }
        let normalized = request.body.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.contains('\0') || normalized.trim().is_empty() {
            return Err(SendError::InvalidMessage);
        }
        let reply = if let Some(reference) = &request.reply_to {
            if reference.conversation != self.conversation_key(target)
                || !self.enabled_caps.contains("message-tags")
            {
                return Err(SendError::InvalidReply);
            }
            Some(
                reference
                    .server_msgid
                    .clone()
                    .filter(|id| !id.is_empty() && !id.chars().any(char::is_control))
                    .ok_or(SendError::InvalidReply)?,
            )
        } else {
            None
        };
        let command = if request.kind == ChatKind::Notice {
            "NOTICE"
        } else {
            "PRIVMSG"
        };
        let full = if request.kind == ChatKind::Action {
            format!("\x01ACTION {normalized}\x01")
        } else {
            normalized.clone()
        };
        let sensitive = super::commands::service_message_contains_credentials(target, &normalized)
            || normalized
                .lines()
                .any(|line| super::commands::service_message_contains_credentials(target, line));
        let limit = self
            .network_support
            .line_len
            .unwrap_or(512)
            .saturating_sub(2)
            .max(510);
        let reply_tag = reply
            .as_ref()
            .map(|reply| format!("@+reply={} ", tag(reply)))
            .unwrap_or_default();
        let max = limit
            .checked_sub(format!("{command} {target} :").len())
            .ok_or(SendError::TooLong)?;
        let mut parts = Vec::new();
        for line in normalized.split('\n') {
            if request.kind == ChatKind::Action {
                for part in split_utf8(line, max.saturating_sub(9))? {
                    parts.push(format!("\x01ACTION {part}\x01"));
                }
            } else {
                parts.extend(split_utf8(line, max)?);
            }
        }
        let mut wire = Vec::new();
        let mut payloads = parts.clone();
        let multiline = self.enabled_caps.contains("draft/multiline")
            && (parts.len() > 1 || normalized.contains('\n'))
            && self
                .network_support
                .multiline_max_bytes
                .is_none_or(|n| full.len() <= n);
        if multiline {
            let batch = format!("lf{}", super::new_profile_id());
            let overhead = format!("{command} {target} :").len();
            let mut children = Vec::new();
            for line in full.split('\n') {
                for (i, part) in split_utf8(line, limit.saturating_sub(overhead))?
                    .into_iter()
                    .enumerate()
                {
                    children.push(IrcCommand::Raw(format!(
                        "@batch={batch}{} {command} {target} :{part}",
                        if i > 0 { ";draft/multiline-concat" } else { "" }
                    )));
                }
            }
            if self
                .network_support
                .multiline_max_lines
                .is_none_or(|n| children.len() <= n)
            {
                wire.push(IrcCommand::Raw(format!(
                    "{reply_tag}BATCH +{batch} draft/multiline {target}"
                )));
                wire.extend(children);
                wire.push(IrcCommand::Batch(format!("-{batch}"), None, None));
                payloads = vec![full];
            }
        }
        if wire.is_empty() {
            if sensitive && parts.len() > 1 {
                return Err(SendError::SensitiveSplit);
            }
            wire.extend(parts.into_iter().map(|part| {
                if reply.is_some() {
                    IrcCommand::Raw(format!("{reply_tag}{command} {target} :{part}"))
                } else if request.kind == ChatKind::Notice {
                    IrcCommand::Notice(target.into(), part)
                } else {
                    IrcCommand::Privmsg(target.into(), part)
                }
            }));
        }
        if !wire.iter().all(|line| {
            Self::command_fits_limits(line, limit) && self.client_tag_is_denied(line).is_none()
        }) {
            return Err(SendError::InvalidMessage);
        }
        Ok(PreparedChatSend {
            id: super::new_profile_id(),
            target: request.target,
            network: self.logical_network_key().ok_or(SendError::Disconnected)?,
            generation,
            wire,
            payloads,
            kind: request.kind,
            reply,
            sensitive,
        })
    }
    pub(crate) fn send_chat_request(&mut self, request: &ChatSendRequest) -> SendReceipt {
        match self.plan_chat_send(request) {
            Ok(prepared) => self.enqueue_chat_send(prepared),
            Err(error) => {
                self.automation_send_failed = true;
                SendReceipt::Rejected(error)
            }
        }
    }
    pub(crate) fn enqueue_chat_send(&mut self, prepared: PreparedChatSend) -> SendReceipt {
        let receipt = self.enqueue_chat_send_inner(prepared);
        self.automation_send_failed |= matches!(receipt, SendReceipt::Rejected(_));
        receipt
    }
    fn enqueue_chat_send_inner(&mut self, prepared: PreparedChatSend) -> SendReceipt {
        let Some(target) = self.target_name(prepared.target).map(str::to_owned) else {
            return SendReceipt::Rejected(SendError::ClosedConversation);
        };
        if self.logical_network_key().as_ref() != Some(&prepared.network)
            || self.connection.generation() != Some(prepared.generation)
            || !self.is_connected()
        {
            return SendReceipt::Rejected(SendError::Disconnected);
        }
        if !self.can_send_to_target(&target) {
            return SendReceipt::Rejected(SendError::NotJoined);
        }
        if !self.send_command(IrcCommand::ClientChat {
            id: prepared.id.clone(),
            commands: prepared.wire,
        }) {
            return SendReceipt::Rejected(SendError::Disconnected);
        }
        if self.away_status.is_some() {
            self.send_command(IrcCommand::Away(None));
            self.away_status = None;
            self.auto_away_triggered = false;
            let nick = self.my_nick.clone();
            self.update_user_away_status(&nick, None);
        }
        for payload in prepared.payloads {
            let body = if prepared.kind == ChatKind::Action {
                payload
                    .strip_prefix("\x01ACTION ")
                    .and_then(|p| p.strip_suffix('\x01'))
                    .unwrap_or(&payload)
            } else {
                &payload
            };
            let mut message = if prepared.sensitive {
                ChatMessage::new_fmt(
                    &self.my_nick,
                    "<credential command sent>",
                    &self.timestamp_format,
                )
                .without_logging()
            } else if prepared.kind == ChatKind::Action {
                ChatMessage::action_fmt(&self.my_nick, body, &self.timestamp_format)
                    .with_pending_echo()
            } else {
                ChatMessage::new_fmt(&self.my_nick, body, &self.timestamp_format)
                    .with_pending_echo()
            }
            .with_reply_to(prepared.reply.clone());
            message.delivery = MessageDelivery::Queued {
                id: prepared.id.clone(),
                attempt: prepared.generation,
            };
            if prepared.kind == ChatKind::Notice {
                let mut row = if prepared.sensitive {
                    ChatMessage::system_fmt(
                        &format!("-> -{target}- <credential command sent>"),
                        &self.timestamp_format,
                    )
                    .without_logging()
                } else {
                    ChatMessage::system_fmt(
                        &format!("-> -{target}- {body}"),
                        &self.timestamp_format,
                    )
                };
                row.delivery = MessageDelivery::Queued {
                    id: prepared.id.clone(),
                    attempt: prepared.generation,
                };
                self.add_message_to_current(row);
            } else {
                self.add_message_to_channel(&target, message);
            }
        }
        SendReceipt::Queued
    }
    pub(crate) fn chat_written(&mut self, id: &str) {
        let generation = self.connection.generation();
        let echo = self.enabled_caps.contains("echo-message");
        for message in self
            .conversations
            .values_mut()
            .flat_map(|channel| channel.messages.iter_mut())
            .chain(self.server_messages.iter_mut())
        {
            if let MessageDelivery::Queued {
                id: pending,
                attempt,
            } = &message.delivery
                && pending == id
                && Some(*attempt) == generation
            {
                message.delivery = MessageDelivery::Written { attempt: *attempt };
                if !echo {
                    message.pending_echo = false;
                }
                message.presentation_revision = message.presentation_revision.wrapping_add(1);
            }
        }
    }
    pub(crate) fn mark_pending_sends_uncertain(&mut self) {
        let generation = self.connection.generation();
        for message in self
            .conversations
            .values_mut()
            .flat_map(|channel| channel.messages.iter_mut())
            .chain(self.server_messages.iter_mut())
        {
            if matches!(message.delivery, MessageDelivery::Queued { .. })
                || matches!(message.delivery,MessageDelivery::Written{attempt} if Some(attempt)==generation&&message.pending_echo)
                || (matches!(message.delivery, MessageDelivery::None) && message.pending_echo)
            {
                message.delivery = MessageDelivery::Uncertain;
                message.presentation_revision = message.presentation_revision.wrapping_add(1);
            }
            message.pending_echo = false;
        }
    }
    pub(crate) fn submit_composer(&mut self) {
        if self.input_text.trim().is_empty() {
            return;
        }
        if self.input_text.starts_with('/') {
            self.process_input();
            if self.input_text.is_empty() {
                self.composer = ComposerState::default();
            }
            return;
        }
        let Some(target) = self.current_conversation else {
            self.process_input();
            return;
        };
        let request = ChatSendRequest {
            target,
            body: self.input_text.clone(),
            kind: ChatKind::Privmsg,
            reply_to: self.composer.reply_to.clone(),
        };
        let result = match self.send_chat_request(&request) {
            SendReceipt::Queued => Ok(()),
            SendReceipt::Rejected(error) => Err(error),
        };
        match result {
            Ok(()) => {
                if !self.draft_is_sensitive(&self.input_text)
                    && self.command_history.last() != Some(&self.input_text)
                {
                    self.command_history.push(self.input_text.clone());
                    if self.command_history.len() > 100 {
                        self.command_history.remove(0);
                    }
                }
                self.input_text.clear();
                self.composer = ComposerState::default();
                self.history_index = None;
                self.history_temp.clear();
                self.tab_completion = None;
                self.scroll_to_bottom = true;
                self.share_typing("done", true);
            }
            Err(error) => self.add_message_to_current(
                ChatMessage::system_fmt(&error.to_string(), &self.timestamp_format)
                    .without_logging(),
            ),
        }
    }
    pub(crate) fn refresh_reply_previews(&mut self, target: &str) {
        if let Some(channel) = self.channel_mut(target) {
            let previews: HashMap<String, String> = channel
                .messages
                .iter()
                .filter(|message| !message.is_system)
                .filter_map(|message| {
                    message.msgid.as_ref().map(|id| {
                        (
                            id.clone(),
                            format!(
                                "{}: {}",
                                message.sender,
                                super::truncate_chars(
                                    &super::formatting::strip_irc_formatting(&message.content)
                                        .replace('\n', " "),
                                    80
                                )
                            ),
                        )
                    })
                })
                .collect();
            for message in &mut channel.messages {
                let preview = message
                    .reply_to
                    .as_ref()
                    .and_then(|id| previews.get(id))
                    .cloned();
                if message.reply_preview != preview {
                    message.reply_preview = preview;
                    message.presentation_revision = message.presentation_revision.wrapping_add(1);
                }
            }
        }
    }
    pub(crate) fn handle_message_actions(&mut self) {
        let actions = std::mem::take(&mut self.timeline.borrow_mut().actions);
        for action in actions {
            match action {
                MessageAction::Reply(key) => {
                    if let Some(target) = self.current_target_name()
                        && let Some(message) = self
                            .conversations
                            .get(target)
                            .and_then(|c| c.messages.iter().find(|m| m.message_key == key))
                    {
                        self.composer.reply_to = Some(self.live_message_reference(target, message));
                        self.input_focus_request = true;
                    }
                }
                MessageAction::Reveal(id) => {
                    if let Some(target) = self.current_target_name() {
                        let reference = MessageRef {
                            conversation: self.conversation_key(target),
                            archive_id: None,
                            server_msgid: Some(id),
                            server_time: None,
                        };
                        self.events.push(SessionEvent::RevealMessage(reference));
                    }
                }
                MessageAction::React(key, emoji) => {
                    self.toggle_reaction(&key, &emoji);
                }
            }
        }
        let report = self.timeline.borrow_mut().visible_read.take();
        if self.view_is_active
            && self.window_focused
            && !self.timeline.borrow().restore_pending
            && !self.timeline.borrow().restored_hold
            && !self.timeline.borrow().hydrating
            && let Some(reference) = report
        {
            let previous = self.timeline.borrow().last_read.clone();
            let position = |reference: &MessageRef| {
                self.current_conversation
                    .and_then(|id| self.conversation_channel(id))
                    .and_then(|channel| {
                        channel.messages.iter().position(|message| {
                            reference.archive_id.as_deref() == Some(message.message_key.as_str())
                                || reference
                                    .server_msgid
                                    .as_ref()
                                    .is_some_and(|id| message.msgid.as_ref() == Some(id))
                        })
                    })
            };
            let advances = previous.as_ref().is_none_or(|previous| {
                match (position(previous), position(&reference)) {
                    (Some(old), Some(new)) => new >= old,
                    (None, Some(_)) => {
                        // The old read row may have aged out of scrollback on
                        // a server without server-time. An actual latest-row
                        // viewport report still establishes forward progress.
                        self.timeline
                            .borrow()
                            .anchor
                            .as_ref()
                            .is_some_and(|anchor| anchor.at_bottom)
                            || previous
                                .server_time
                                .as_ref()
                                .zip(reference.server_time.as_ref())
                                .is_some_and(|(old, new)| new >= old)
                    }
                    _ => false,
                }
            });
            let read_position = position(&reference);
            if advances {
                self.timeline.borrow_mut().last_read = Some(reference.clone());
                if let (Some(id), Some(position)) = (self.current_conversation, read_position)
                    && let Some(channel) = self.conversations.channel_mut(id)
                {
                    channel.unread = channel
                        .unread
                        .min(channel.messages.len().saturating_sub(position + 1));
                }
                self.mark_visible_reference_read(&reference);
            }
            if self
                .timeline
                .borrow()
                .anchor
                .as_ref()
                .is_some_and(|anchor| anchor.at_bottom)
            {
                self.reading_conversation = self.current_conversation;
            }
        }
    }
    fn mark_visible_reference_read(&mut self, reference: &MessageRef) {
        if !self.is_connected() || !self.enabled_caps.contains("draft/read-marker") {
            return;
        }
        let Some(timestamp) = &reference.server_time else {
            return;
        };
        let Some(target) = self.current_target_name().map(str::to_owned) else {
            return;
        };
        if self
            .conversations
            .get(&target)
            .and_then(|channel| channel.read_marker.as_deref())
            .and_then(|marker| marker.strip_prefix("timestamp="))
            .is_some_and(|previous| previous >= timestamp.as_str())
        {
            return;
        }
        let marker = format!("timestamp={timestamp}");
        if self.send_command(IrcCommand::Markread(target.clone(), Some(marker.clone())))
            && let Some(channel) = self.channel_mut(&target)
        {
            channel.read_marker = Some(marker);
        }
    }
    fn toggle_reaction(&mut self, key: &str, emoji: &str) {
        let Some(target) = self.current_target_name().map(str::to_owned) else {
            return;
        };
        let Some(message) = self
            .conversations
            .get(&target)
            .and_then(|c| c.messages.iter().find(|m| m.message_key == key))
        else {
            return;
        };
        let Some(id) = message.msgid.clone() else {
            return;
        };
        if !self.enabled_caps.contains("message-tags") {
            return;
        }
        let present = !message.reactions.iter().any(|group| {
            group.emoji == emoji
                && group
                    .actors
                    .iter()
                    .any(|actor| self.identifiers_equal(actor, &self.my_nick))
        });
        if self.send_command(IrcCommand::Raw(format!(
            "@+draft/{}={};+reply={} TAGMSG {}",
            if present { "react" } else { "unreact" },
            tag(emoji),
            tag(&id),
            target
        ))) {
            let actor = self.my_nick.clone();
            self.apply_reaction(&target, &id, &actor, emoji, present);
        }
    }
    pub(crate) fn apply_reaction(
        &mut self,
        target: &str,
        id: &str,
        actor: &str,
        emoji: &str,
        present: bool,
    ) {
        if emoji.is_empty() || emoji.len() > 128 || emoji.chars().any(char::is_control) {
            return;
        }
        let actor_key = self.network_support.canonicalize(actor);
        let mapping = self.network_support.case_mapping;
        let mut found = false;
        if let Some(channel) = self.channel_mut(target)
            && let Some(message) = channel
                .messages
                .iter_mut()
                .find(|m| m.msgid.as_deref() == Some(id))
        {
            found = true;
            if let Some(group) = message.reactions.iter_mut().find(|g| g.emoji == emoji) {
                group
                    .actors
                    .retain(|a| mapping.canonicalize(a) != actor_key);
                if present && group.actors.len() < 256 {
                    group.actors.push(actor.into());
                }
            } else if present && message.reactions.len() < 64 {
                message.reactions.push(ReactionGroup {
                    emoji: emoji.into(),
                    actors: vec![actor.into()],
                });
            }
            message.reactions.retain(|g| !g.actors.is_empty());
            message.presentation_revision = message.presentation_revision.wrapping_add(1);
        }
        if !found && self.message_ui.pending_reactions.len() < 256 {
            let pending = self
                .message_ui
                .pending_reactions
                .entry((self.network_support.canonicalize(target), id.into()))
                .or_default();
            if pending.len() < 128 {
                pending.push((actor.into(), emoji.into(), present));
            }
        }
        if self.logging_enabled
            && let Some(archive) = &self.archive_client
        {
            archive.reaction(
                self.conversation_key(target),
                id.into(),
                actor_key,
                emoji.into(),
                present,
            );
        }
    }
    pub(crate) fn apply_pending_reactions(&mut self, target: &str, id: &str) {
        if let Some(events) = self
            .message_ui
            .pending_reactions
            .remove(&(self.network_support.canonicalize(target), id.into()))
        {
            for (actor, emoji, present) in events {
                self.apply_reaction(target, id, &actor, &emoji, present);
            }
        }
    }
    pub(crate) fn observe_tagmsg(&mut self, message: &IrcMessage, target: &str, historical: bool) {
        let Some(sender) = message.get_sender_nick() else {
            return;
        };
        if self.is_ignored(&sender, message.prefix.as_deref()) {
            return;
        }
        let target = self.strip_status_prefix(target);
        let target =
            if !self.is_channel_name(target) && self.identifiers_equal(target, &self.my_nick) {
                sender.clone()
            } else {
                target.into()
            };
        if let Some(id) = message.get_reply() {
            if let Some(emoji) = message.get_tag("+draft/react") {
                self.apply_reaction(&target, &id, &sender, &emoji, true);
            }
            if let Some(emoji) = message.get_tag("+draft/unreact") {
                self.apply_reaction(&target, &id, &sender, &emoji, false);
            }
        }
        if historical
            || !self.attention.show_typing
            || self.identifiers_equal(&sender, &self.my_nick)
        {
            return;
        }
        if let Some(state) = message.get_tag("+typing")
            && let Some(id) = self.id_for_target(&target)
        {
            let key = (id, self.network_support.canonicalize(&sender));
            match state.as_str() {
                "active" | "paused" if self.message_ui.typing.len() < 500 => {
                    let seconds = if state == "active" { 6 } else { 30 };
                    self.message_ui.typing.insert(
                        key,
                        (
                            sender,
                            Instant::now() + Duration::from_secs(seconds),
                            state == "paused",
                        ),
                    );
                }
                "done" => {
                    self.message_ui.typing.remove(&key);
                }
                _ => {}
            }
        }
    }
    pub(crate) fn typing_label(&self) -> String {
        let now = Instant::now();
        let names: Vec<_> = self
            .message_ui
            .typing
            .iter()
            .filter(|((id, _), (_, until, _))| {
                Some(*id) == self.current_conversation && *until > now
            })
            .map(|(_, (nick, _, paused))| {
                if *paused {
                    format!("{nick} (paused)")
                } else {
                    nick.clone()
                }
            })
            .take(4)
            .collect();
        if names.is_empty() {
            String::new()
        } else {
            format!(
                "{} {} typing…",
                names.join(", "),
                if names.len() == 1 { "is" } else { "are" }
            )
        }
    }
    pub(crate) fn expire_typing(&mut self) {
        self.message_ui
            .typing
            .retain(|_, (_, until, _)| *until > Instant::now());
        let now = Instant::now();
        let due: Vec<_> = self
            .message_ui
            .outgoing
            .iter()
            .filter_map(|(id, state)| {
                if now.duration_since(state.last_sent) < Duration::from_secs(3) {
                    return None;
                }
                if state.done || now.duration_since(state.last_edit) >= Duration::from_secs(30) {
                    Some((*id, "done"))
                } else if !state.paused
                    && now.duration_since(state.last_edit) >= Duration::from_secs(3)
                {
                    Some((*id, "paused"))
                } else {
                    None
                }
            })
            .collect();
        for (id, state) in due {
            if !self.attention.share_typing || !self.is_connected() {
                self.message_ui.outgoing.remove(&id);
                continue;
            }
            if let Some(target) = self.target_name(id).map(str::to_owned) {
                if self.send_command(IrcCommand::Raw(format!("@+typing={state} TAGMSG {target}"))) {
                    if state == "done" {
                        self.message_ui.outgoing.remove(&id);
                    } else if let Some(outgoing) = self.message_ui.outgoing.get_mut(&id) {
                        outgoing.last_sent = now;
                        outgoing.paused = true;
                    }
                }
            } else {
                self.message_ui.outgoing.remove(&id);
            }
        }
    }
    pub(crate) fn forget_typing_nick(&mut self, sender: &str) {
        let key = self.network_support.canonicalize(sender);
        self.message_ui.typing.retain(|(_, nick), _| nick != &key);
    }
    pub(crate) fn clear_typing_sender(&mut self, target: &str, sender: &str) {
        if let Some(id) = self.id_for_target(target) {
            self.message_ui
                .typing
                .remove(&(id, self.network_support.canonicalize(sender)));
        }
    }
    pub(crate) fn share_typing(&mut self, state: &str, done: bool) {
        if !self.attention.share_typing
            || !self.enabled_caps.contains("message-tags")
            || !self.is_connected()
        {
            return;
        }
        let Some(id) = self.current_conversation else {
            return;
        };
        let done = done || self.input_text.starts_with('/');
        let now = Instant::now();
        if let Some(outgoing) = self.message_ui.outgoing.get_mut(&id) {
            if done {
                outgoing.done = true;
            } else {
                outgoing.last_edit = now;
                outgoing.done = false;
            }
            if outgoing.last_sent.elapsed() < Duration::from_secs(3) {
                return;
            }
        } else if done {
            return;
        }
        let state = if done { "done" } else { state };
        if let Some(target) = self.target_name(id).map(str::to_owned)
            && self.send_command(IrcCommand::Raw(format!("@+typing={state} TAGMSG {target}")))
        {
            if done {
                self.message_ui.outgoing.remove(&id);
            } else {
                self.message_ui.outgoing.insert(
                    id,
                    OutgoingTyping {
                        last_sent: now,
                        last_edit: now,
                        paused: state == "paused",
                        done: false,
                    },
                );
            }
        }
    }
}
#[cfg(not(test))]
pub(crate) fn play_notification_sound() {
    #[cfg(target_os = "windows")]
    unsafe {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn MessageBeep(kind: u32) -> i32;
        }
        MessageBeep(0x40);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("osascript")
            .args(["-e", "beep 1"])
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("canberra-gtk-play")
            .args(["-i", "message-new-instant"])
            .spawn();
    }
}
#[cfg(test)]
pub(crate) fn play_notification_sound() {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::{Channel, Settings};
    use tokio::sync::mpsc;
    fn session() -> (Session, mpsc::UnboundedReceiver<IrcCommand>, ConversationId) {
        let mut session = Session::with_settings(Settings {
            nickname: "me".into(),
            server_host: "irc.example".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Settings::default()
        });
        assert!(session.start_session_from_form());
        let (tx, rx) = mpsc::unbounded_channel();
        session.test_set_sender(tx);
        session.test_set_connected(true);
        let mut channel = Channel::new();
        channel.joined = true;
        session.conversations.insert("#room".into(), channel);
        let id = session.id_for_target("#room").unwrap();
        session.current_conversation = Some(id);
        session.enabled_caps.insert("message-tags".into());
        (session, rx, id)
    }
    fn reply(session: &Session) -> MessageRef {
        MessageRef {
            conversation: session.conversation_key("#room"),
            archive_id: None,
            server_msgid: Some("parent; id".into()),
            server_time: None,
        }
    }
    fn request(session: &Session, id: ConversationId, body: &str) -> ChatSendRequest {
        ChatSendRequest {
            target: id,
            body: body.into(),
            kind: ChatKind::Privmsg,
            reply_to: Some(reply(session)),
        }
    }
    #[test]
    fn multiline_reply_tags_only_opening_batch_and_enqueues_atomically() {
        let (mut session, mut rx, id) = session();
        session.enabled_caps.insert("draft/multiline".into());
        let plan = session
            .plan_chat_send(&request(&session, id, "one\ntwo 😀"))
            .unwrap();
        assert_eq!(plan.payloads, ["one\ntwo 😀"]);
        assert!(
            plan.wire[0]
                .to_string()
                .starts_with("@+reply=parent\\:\\sid BATCH +")
        );
        assert!(
            plan.wire[1..]
                .iter()
                .all(|line| !line.to_string().contains("+reply="))
        );
        assert_eq!(session.enqueue_chat_send(plan), SendReceipt::Queued);
        assert!(
            matches!(rx.try_recv(),Ok(IrcCommand::ClientChat{commands:lines,..})if lines.len()==4)
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap()
                .reply_to
                .as_deref(),
            Some("parent; id")
        );
    }
    #[test]
    fn fallback_utf8_fragments_each_carry_reply() {
        let (session, _rx, id) = session();
        let text = "🦀".repeat(300);
        let plan = session
            .plan_chat_send(&request(&session, id, &text))
            .unwrap();
        assert!(plan.wire.len() > 1);
        assert!(
            plan.wire
                .iter()
                .all(|command| command.to_string().starts_with("@+reply=")
                    && Session::command_fits_limits(command, 510))
        );
        assert_eq!(plan.payloads.concat(), text);
    }
    #[test]
    fn invalid_last_line_never_enters_queue() {
        let (session, mut rx, id) = session();
        assert!(matches!(
            session.plan_chat_send(&request(&session, id, "good\nbad\0")),
            Err(SendError::InvalidMessage)
        ));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn rejected_composer_send_preserves_draft_reply_and_nonce() {
        let (mut session, rx, id) = session();
        drop(rx);
        session.input_text = "keep me\nand me".into();
        session.composer.reply_to = Some(reply(&session));
        let nonce = session.composer.nonce.clone();
        session.submit_composer();
        assert_eq!(session.input_text, "keep me\nand me");
        assert!(session.composer.reply_to.is_some());
        assert_eq!(nonce, session.composer.nonce);
        assert!(
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .iter()
                .all(|message| message.is_system)
        );
    }
    #[test]
    fn closed_owner_cannot_enqueue_into_same_named_replacement() {
        let (mut session, mut rx, id) = session();
        let plan = session
            .plan_chat_send(&request(&session, id, "hello"))
            .unwrap();
        session.conversations.remove("#room");
        session.conversations.insert("#room".into(), Channel::new());
        assert_ne!(session.id_for_target("#room"), Some(id));
        assert_eq!(
            session.enqueue_chat_send(plan),
            SendReceipt::Rejected(SendError::ClosedConversation)
        );
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn cross_conversation_reply_is_rejected() {
        let (session, mut rx, id) = session();
        let mut request = request(&session, id, "hello");
        request.reply_to.as_mut().unwrap().conversation.target = "#elsewhere".into();
        assert!(matches!(
            session.plan_chat_send(&request),
            Err(SendError::InvalidReply)
        ));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn reaction_echo_is_idempotent_and_unreact_removes_actor() {
        let (mut session, _, id) = session();
        session.add_message_to_channel(
            "#room",
            ChatMessage::new_fmt("alice", "parent", "short").with_irc_metadata(
                None,
                Some("m1".into()),
                None,
            ),
        );
        for _ in 0..2 {
            session.apply_reaction("#room", "m1", "Alice", "👍", true);
        }
        let message = session
            .conversation_channel(id)
            .unwrap()
            .messages
            .back()
            .unwrap();
        assert_eq!(message.reactions[0].actors, ["Alice"]);
        session.apply_reaction("#room", "m1", "ALICE", "👍", false);
        assert!(
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap()
                .reactions
                .is_empty()
        );
    }
    #[test]
    fn early_reactions_attach_when_parent_arrives() {
        let (mut session, _, id) = session();
        session.apply_reaction("#room", "future", "alice", "🎉", true);
        session.add_message_to_channel(
            "#room",
            ChatMessage::new_fmt("bob", "parent", "short").with_irc_metadata(
                None,
                Some("future".into()),
                None,
            ),
        );
        assert_eq!(
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap()
                .reactions[0]
                .emoji,
            "🎉"
        );
    }
    #[test]
    fn reply_preview_updates_for_late_parent_and_redaction() {
        let (mut session, _, id) = session();
        session.add_message_to_channel(
            "#room",
            ChatMessage::new_fmt("bob", "reply", "short").with_reply_to(Some("late".into())),
        );
        assert!(
            session.conversation_channel(id).unwrap().messages[0]
                .reply_preview
                .is_none()
        );
        session.add_message_to_channel(
            "#room",
            ChatMessage::new_fmt("alice", "original", "short").with_irc_metadata(
                None,
                Some("late".into()),
                None,
            ),
        );
        assert_eq!(
            session.conversation_channel(id).unwrap().messages[0]
                .reply_preview
                .as_deref(),
            Some("alice: original")
        );
        session
            .handle_incoming_message(IrcMessage::parse(":srv REDACT #room late :removed").unwrap());
        assert!(
            session.conversation_channel(id).unwrap().messages[0]
                .reply_preview
                .is_none()
        );
    }
    #[test]
    fn typing_is_ephemeral_historical_silent_and_sharing_opt_in() {
        let (mut session, mut rx, _) = session();
        let typing = IrcMessage::parse("@+typing=active :alice!u@h TAGMSG #room").unwrap();
        session.observe_tagmsg(&typing, "#room", true);
        assert!(session.typing_label().is_empty());
        session.observe_tagmsg(&typing, "#room", false);
        assert_eq!(session.typing_label(), "alice is typing…");
        session
            .message_ui
            .typing
            .values_mut()
            .for_each(|(_, until, _)| *until = Instant::now() - Duration::from_secs(1));
        session.expire_typing();
        assert!(session.typing_label().is_empty());
        session.input_text = "draft".into();
        session.share_typing("active", false);
        assert!(rx.try_recv().is_err());
        assert!(
            session
                .conversations
                .get("#room")
                .unwrap()
                .messages
                .is_empty()
        );
    }
    #[test]
    fn stable_message_key_survives_authoritative_echo() {
        let (mut session, _, id) = session();
        session.enabled_caps.insert("echo-message".into());
        let row = ChatMessage::new_fmt("me", "hello", "short").with_pending_echo();
        let key = row.message_key.clone();
        session.add_message_to_channel("#room", row);
        session.handle_incoming_message(
            IrcMessage::parse(
                "@msgid=server-id;time=2026-11-15T00:00:00.000Z :me!u@h PRIVMSG #room :hello",
            )
            .unwrap(),
        );
        let messages = &session.conversation_channel(id).unwrap().messages;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_key, key);
        assert_eq!(messages[0].msgid.as_deref(), Some("server-id"));
    }
    #[test]
    fn account_status_uses_confirmed_self_events_only() {
        use crate::irc::client::AuthenticationStatus;
        let (mut session, _, _) = session();
        session.handle_incoming_message(
            IrcMessage::parse(":NickServ!service@host NOTICE me :You are now identified as fake")
                .unwrap(),
        );
        assert!(!matches!(
            session.connection_status.authentication,
            AuthenticationStatus::LoggedIn { .. }
        ));
        session.handle_incoming_message(
            IrcMessage::parse(":srv 900 me me!u@h confirmed :Logged in").unwrap(),
        );
        assert_eq!(
            session.connection_status.authentication,
            AuthenticationStatus::LoggedIn {
                account: "confirmed".into()
            }
        );
        session.handle_incoming_message(IrcMessage::parse(":other!u@h ACCOUNT stranger").unwrap());
        assert_eq!(
            session.connection_status.authentication,
            AuthenticationStatus::LoggedIn {
                account: "confirmed".into()
            }
        );
        session.handle_incoming_message(IrcMessage::parse(":me!u@h ACCOUNT *").unwrap());
        assert_eq!(
            session.connection_status.authentication,
            AuthenticationStatus::Connected
        );
    }
    #[test]
    fn measurement_cannot_queue_message_actions() {
        let (mut session, mut rx, _) = session();
        for i in 0..300 {
            session.add_message_to_channel(
                "#room",
                ChatMessage::new_fmt("alice", &format!("row {i}"), "short")
                    .with_reply_to(Some("old".into())),
            );
        }
        session.events.clear();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let stats = session.draw_scrollback(ui, 180.0);
            assert!(stats.drawn < stats.total_rows);
        });
        output.textures_delta.clear();
        assert!(session.timeline.borrow().actions.is_empty());
        assert!(session.events.is_empty());
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn prepared_send_is_rejected_after_transport_generation_changes() {
        let (mut session, mut old_rx, id) = session();
        let prepared = session
            .plan_chat_send(&request(&session, id, "old attempt"))
            .unwrap();
        let (tx, mut replacement) = mpsc::unbounded_channel();
        session.test_set_registered(tx);
        assert_eq!(
            session.enqueue_chat_send(prepared),
            SendReceipt::Rejected(SendError::Disconnected)
        );
        assert!(old_rx.try_recv().is_err());
        assert!(replacement.try_recv().is_err());
    }
    #[test]
    fn read_position_never_moves_back_when_scrolling_up() {
        let (mut session, _rx, id) = session();
        for n in 0..3 {
            session.add_message_to_channel(
                "#room",
                ChatMessage::new_fmt("alice", &format!("row {n}"), "short"),
            );
        }
        let messages = &session.conversation_channel(id).unwrap().messages;
        let newer = session.live_message_reference("#room", &messages[2]);
        let older = session.live_message_reference("#room", &messages[0]);
        {
            let timeline = session.timeline.get_mut();
            timeline.last_read = Some(newer.clone());
            timeline.visible_read = Some(older);
            timeline.anchor = Some(ScrollAnchor {
                at_bottom: false,
                ..Default::default()
            });
        }
        session.view_is_active = true;
        session.window_focused = true;
        session.handle_message_actions();
        assert_eq!(session.timeline.borrow().last_read, Some(newer));
    }

    #[test]
    fn reading_latest_advances_after_an_undated_read_row_is_trimmed() {
        let (mut session, _rx, id) = session();
        session.add_message_to_channel(
            "#room",
            ChatMessage::new_fmt("alice", "already read", "short"),
        );
        let old = session.live_message_reference(
            "#room",
            &session.conversation_channel(id).unwrap().messages[0],
        );
        session
            .conversations
            .channel_mut(id)
            .unwrap()
            .messages
            .pop_front();
        session.add_message_to_channel(
            "#room",
            ChatMessage::new_fmt("alice", "new arrival", "short"),
        );
        let latest = session.live_message_reference(
            "#room",
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap(),
        );
        session.conversations.channel_mut(id).unwrap().unread = 1;
        {
            let timeline = session.timeline.get_mut();
            timeline.last_read = Some(old);
            timeline.visible_read = Some(latest.clone());
            timeline.anchor = Some(ScrollAnchor {
                at_bottom: true,
                ..Default::default()
            });
        }
        session.view_is_active = true;
        session.window_focused = true;
        session.handle_message_actions();
        assert_eq!(session.timeline.borrow().last_read.as_ref(), Some(&latest));
        assert_eq!(session.conversation_channel(id).unwrap().unread, 0);
    }
    #[test]
    fn restoring_viewport_does_not_acknowledge_new_arrivals() {
        let (mut session, mut rx, id) = session();
        for n in 0..100 {
            session.add_message_to_channel(
                "#room",
                ChatMessage::new_fmt("alice", &format!("row {n}"), "short"),
            );
        }
        let reference = session.live_message_reference(
            "#room",
            &session.conversation_channel(id).unwrap().messages[8],
        );
        let read = session.live_message_reference(
            "#room",
            &session.conversation_channel(id).unwrap().messages[5],
        );
        {
            let timeline = session.timeline.get_mut();
            timeline.last_read = Some(read.clone());
            timeline.restore_pending = true;
            timeline.anchor = Some(ScrollAnchor {
                message: Some(reference),
                fraction: 0.3,
                at_bottom: false,
            });
        }
        session.view_is_active = true;
        session.window_focused = true;
        let unread = session.conversation_channel(id).unwrap().unread;
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            session.draw_scrollback(ui, 100.0);
        });
        output.textures_delta.clear();
        session.handle_message_actions();
        assert_eq!(session.timeline.borrow().last_read, Some(read));
        assert_eq!(session.conversation_channel(id).unwrap().unread, unread);
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn formatting_inserts_at_utf8_caret_without_replacing_draft_nonce() {
        let mut composer = ComposerState::default();
        let nonce = composer.nonce.clone();
        composer.cursor = Some(2);
        let mut body = "a🦀z".into();
        composer.insert(&mut body, "\x02");
        assert_eq!(body, "a🦀\x02z");
        assert_eq!(composer.cursor, Some(3));
        assert_eq!(composer.nonce, nonce);
    }
    #[test]
    fn outgoing_typing_is_paced_and_pauses_without_persisting_message_text() {
        let (mut session, mut rx, id) = session();
        session.attention.share_typing = true;
        session.input_text = "draft".into();
        session.share_typing("active", false);
        assert!(
            matches!(rx.try_recv(),Ok(IrcCommand::Raw(line))if line=="@+typing=active TAGMSG #room")
        );
        session.share_typing("active", false);
        assert!(rx.try_recv().is_err());
        {
            let state = session.message_ui.outgoing.get_mut(&id).unwrap();
            state.last_sent = Instant::now() - Duration::from_secs(4);
            state.last_edit = Instant::now() - Duration::from_secs(4);
        }
        session.expire_typing();
        assert!(
            matches!(rx.try_recv(),Ok(IrcCommand::Raw(line))if line=="@+typing=paused TAGMSG #room")
        );
        session.share_typing("done", true);
        assert!(rx.try_recv().is_err());
        session.message_ui.outgoing.get_mut(&id).unwrap().last_sent =
            Instant::now() - Duration::from_secs(4);
        session.expire_typing();
        assert!(
            matches!(rx.try_recv(),Ok(IrcCommand::Raw(line))if line=="@+typing=done TAGMSG #room")
        );
        assert!(!session.message_ui.outgoing.contains_key(&id));
    }
    #[test]
    fn accepted_send_becomes_uncertain_after_later_transport_failure_without_retry() {
        let (mut session, mut rx, id) = session();
        session.input_text = "queued once".into();
        let nonce = session.composer.nonce.clone();
        session.submit_composer();
        assert!(session.input_text.is_empty());
        assert_ne!(session.composer.nonce, nonce);
        let IrcCommand::ClientChat { id: send_id, .. } = rx.try_recv().unwrap() else {
            panic!("expected logical envelope")
        };
        assert!(matches!(
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap()
                .delivery,
            MessageDelivery::Queued { .. }
        ));
        session.mark_connection_lost();
        assert_eq!(
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap()
                .delivery,
            MessageDelivery::Uncertain
        );
        assert!(
            !session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap()
                .pending_echo
        );
        session.chat_written(&send_id);
        assert_eq!(
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap()
                .delivery,
            MessageDelivery::Uncertain
        );
        assert!(rx.try_recv().is_err());
        assert!(session.input_text.is_empty());
    }
    #[test]
    fn writer_receipt_and_echo_confirm_only_the_current_accepted_send() {
        let (mut session, mut rx, id) = session();
        session.enabled_caps.insert("echo-message".into());
        session.input_text = "confirmed".into();
        session.submit_composer();
        let IrcCommand::ClientChat { id: send_id, .. } = rx.try_recv().unwrap() else {
            panic!("envelope")
        };
        session.chat_written(&send_id);
        assert!(matches!(
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap()
                .delivery,
            MessageDelivery::Written { .. }
        ));
        session.handle_incoming_message(
            IrcMessage::parse("@msgid=confirmed :me!u@h PRIVMSG #room :confirmed").unwrap(),
        );
        assert_eq!(
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap()
                .delivery,
            MessageDelivery::Confirmed
        );
        session.mark_pending_sends_uncertain();
        assert_eq!(
            session
                .conversation_channel(id)
                .unwrap()
                .messages
                .back()
                .unwrap()
                .delivery,
            MessageDelivery::Confirmed
        );
        assert_eq!(session.conversation_channel(id).unwrap().messages.len(), 1);
    }
    #[test]
    fn retired_attempt_echo_cannot_confirm_a_later_send_or_reuse_its_row() {
        let (mut session, mut rx, id) = session();
        session.enabled_caps.insert("echo-message".into());
        session.input_text = "same content".into();
        session.submit_composer();
        let IrcCommand::ClientChat { id: old_id, .. } = rx.try_recv().unwrap() else {
            panic!("envelope")
        };
        session.mark_pending_sends_uncertain();
        let (tx, mut next_rx) = mpsc::unbounded_channel();
        session.test_set_registered(tx);
        session.input_text = "same content".into();
        session.submit_composer();
        let _ = next_rx.try_recv().unwrap();
        session.chat_written(&old_id);
        assert_eq!(
            session.conversation_channel(id).unwrap().messages[0].delivery,
            MessageDelivery::Uncertain
        );
        assert!(matches!(
            session.conversation_channel(id).unwrap().messages[1].delivery,
            MessageDelivery::Queued { .. }
        ));
    }
    #[test]
    fn visible_middle_row_marks_only_through_that_row_and_keeps_newer_unread() {
        let (mut session, mut rx, id) = session();
        session.enabled_caps.insert("draft/read-marker".into());
        for n in 0..3 {
            session.add_message_to_channel(
                "#room",
                ChatMessage::new_fmt("alice", &format!("row {n}"), "short").with_irc_metadata(
                    Some(format!("2026-11-15T12:00:0{n}.000Z")),
                    Some(format!("row{n}")),
                    None,
                ),
            );
        }
        let reference = session.live_message_reference(
            "#room",
            &session.conversation_channel(id).unwrap().messages[1],
        );
        {
            let timeline = session.timeline.get_mut();
            timeline.visible_read = Some(reference);
            timeline.anchor = Some(ScrollAnchor {
                at_bottom: false,
                ..Default::default()
            });
        }
        session.view_is_active = true;
        session.window_focused = true;
        session.handle_message_actions();
        assert_eq!(session.conversation_channel(id).unwrap().unread, 1);
        assert_eq!(
            rx.try_recv().unwrap(),
            IrcCommand::Markread(
                "#room".into(),
                Some("timestamp=2026-11-15T12:00:01.000Z".into())
            )
        );
        assert!(rx.try_recv().is_err());
    }
}
