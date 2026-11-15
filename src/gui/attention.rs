//! Conversation-scoped alert preferences and the activity inbox.
use super::identity::{ConversationKey, MessageRef};
use super::windows::WindowKey;
use super::{App, Session};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AlertLevel {
    #[default]
    Inherit,
    Muted,
    Mentions,
    All,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AlertSound {
    #[default]
    Inherit,
    Off,
    Default,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConversationRule {
    pub conversation: ConversationKey,
    pub level: AlertLevel,
    pub highlight_terms: Option<Vec<String>>,
    pub sound: AlertSound,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AttentionSettings {
    pub show_typing: bool,
    pub share_typing: bool,
    pub sound: bool,
    pub(crate) rules: Vec<ConversationRule>,
}
impl Default for AttentionSettings {
    fn default() -> Self {
        Self {
            show_typing: true,
            share_typing: false,
            sound: false,
            rules: Vec::new(),
        }
    }
}
impl AttentionSettings {
    pub(crate) fn eligible(&self, event: &AttentionEvent) -> bool {
        match self
            .rule(&event.reference.conversation)
            .map_or(AlertLevel::Inherit, |rule| rule.level)
        {
            AlertLevel::Muted => false,
            AlertLevel::Mentions => event.mention,
            AlertLevel::All => true,
            AlertLevel::Inherit => event.private || event.mention,
        }
    }
    pub(crate) fn sound_for(&self, event: &AttentionEvent) -> bool {
        match self
            .rule(&event.reference.conversation)
            .map_or(AlertSound::Inherit, |rule| rule.sound)
        {
            AlertSound::Off => false,
            AlertSound::Default => true,
            AlertSound::Inherit => self.sound,
        }
    }

    pub(crate) fn rule(&self, key: &ConversationKey) -> Option<&ConversationRule> {
        self.rules.iter().find(|rule| &rule.conversation == key)
    }
    pub(crate) fn rule_mut(&mut self, key: ConversationKey) -> &mut ConversationRule {
        if let Some(i) = self.rules.iter().position(|rule| rule.conversation == key) {
            return &mut self.rules[i];
        }
        self.rules.push(ConversationRule {
            conversation: key,
            level: AlertLevel::Inherit,
            highlight_terms: None,
            sound: AlertSound::Inherit,
        });
        self.rules.last_mut().expect("just inserted")
    }
}
#[derive(Clone, Debug)]
pub(crate) struct AttentionEvent {
    pub reference: MessageRef,
    pub private: bool,
    pub mention: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct ActivityItem {
    pub reference: MessageRef,
    pub read_conversation: ConversationKey,
    pub title: String,
    pub preview: String,
    pub mention: bool,
    pub read: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ActivityRecord {
    pub reference: MessageRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_conversation: Option<ConversationKey>,
    pub mention: bool,
    pub read: bool,
}
#[derive(Default)]
pub(crate) struct ActivityState {
    pub items: VecDeque<ActivityItem>,
    pub open: bool,
    pub owner: Option<WindowKey>,
}
impl Session {
    pub(crate) fn conversation_key(&self, target: &str) -> ConversationKey {
        ConversationKey {
            network: self.logical_network_key().unwrap_or_else(|| {
                super::identity::LogicalNetworkKey::Direct {
                    endpoint: super::identity::TransportEndpointKey::new(
                        &self.server_host,
                        self.server_port.parse().unwrap_or(6667),
                        self.use_tls,
                    ),
                }
            }),
            target: self.network_support.canonicalize(target),
        }
    }
    pub(crate) fn attention_highlight(&self, target: &str, content: &str) -> bool {
        let key = self.conversation_key(target);
        if let Some(terms) = self
            .attention
            .rule(&key)
            .and_then(|rule| rule.highlight_terms.as_ref())
        {
            let content = self.network_support.canonicalize(content);
            return (!self.my_nick_lower.is_empty()
                && super::ident_contains(&content, &self.my_nick_lower))
                || terms.iter().any(|term| {
                    !term.trim().is_empty()
                        && super::ident_contains(
                            &content,
                            &self.network_support.canonicalize(term.trim()),
                        )
                });
        }
        self.check_nick_mention(content)
    }
}
impl App {
    pub(crate) fn activity_records(&self) -> Vec<ActivityRecord> {
        self.activity
            .items
            .iter()
            .map(|item| ActivityRecord {
                reference: item.reference.clone(),
                read_conversation: Some(item.read_conversation.clone()),
                mention: item.mention,
                read: item.read,
            })
            .collect()
    }
    pub(crate) fn restore_activity(&mut self, records: Vec<ActivityRecord>) {
        self.activity.items = records
            .into_iter()
            .take(1000)
            .map(|record| {
                let read_conversation = record
                    .read_conversation
                    .filter(|owner| owner.network == record.reference.conversation.network)
                    .unwrap_or_else(|| record.reference.conversation.clone());
                ActivityItem {
                    title: read_conversation.target.clone(),
                    preview: "Saved activity — open message".into(),
                    reference: record.reference,
                    read_conversation,
                    mention: record.mention,
                    read: record.read,
                }
            })
            .collect();
    }

    pub(crate) fn handle_attention(
        &mut self,
        event: AttentionEvent,
        title: &str,
        preview: &str,
        force: bool,
    ) {
        if (event.private || event.mention)
            && !self.activity.items.iter().any(|item| {
                item.reference.conversation == event.reference.conversation
                    && (item
                        .reference
                        .archive_id
                        .as_ref()
                        .is_some_and(|id| event.reference.archive_id.as_ref() == Some(id))
                        || item
                            .reference
                            .server_msgid
                            .as_ref()
                            .is_some_and(|id| event.reference.server_msgid.as_ref() == Some(id)))
            })
        {
            self.activity.items.push_front(ActivityItem {
                read_conversation: event.reference.conversation.clone(),
                reference: event.reference.clone(),
                title: title.into(),
                preview: super::formatting::strip_irc_formatting(preview),
                mention: event.mention,
                read: false,
            });
            self.activity.items.truncate(1000);
        }
        if !self.settings.attention.eligible(&event) {
            return;
        }
        self.queue_attention_notification(event, title, preview, force);
    }

    pub(crate) fn mark_activity_seen(&mut self, key: WindowKey) {
        let Some(index) = self.session_index(key.conn) else {
            return;
        };
        let Some(id) = key.conversation else {
            return;
        };
        let Some(window) = self.windows.get(&key) else {
            return;
        };
        let Some(last_read) = window.view.timeline.borrow().last_read.clone() else {
            return;
        };
        let Some(channel) = self.sessions[index].conversation_channel(id) else {
            return;
        };
        let position = |reference: &MessageRef| {
            channel.messages.iter().position(|message| {
                reference.archive_id.as_deref() == Some(message.message_key.as_str())
                    || reference
                        .server_msgid
                        .as_ref()
                        .is_some_and(|id| message.msgid.as_ref() == Some(id))
            })
        };
        if let Some(read) = position(&last_read) {
            for item in &mut self.activity.items {
                if item.read_conversation == last_read.conversation
                    && position(&item.reference).is_some_and(|index| index <= read)
                {
                    item.read = true;
                }
            }
        }
    }
    pub(crate) fn rename_activity_conversation(
        &mut self,
        old: &ConversationKey,
        new: &ConversationKey,
    ) {
        for item in &mut self.activity.items {
            if &item.read_conversation == old {
                item.read_conversation = new.clone();
            }
        }
    }
    pub(crate) fn reconcile_activity_conversation(
        &mut self,
        conn: super::windows::ConnId,
        survivor: super::conversations::ConversationId,
    ) {
        let Some(index) = self.session_index(conn) else {
            return;
        };
        let session = &self.sessions[index];
        let Some(target) = session.target_name(survivor) else {
            return;
        };
        let owner = session.conversation_key(target);
        for item in &mut self.activity.items {
            if item.read_conversation.network == owner.network
                && session.id_for_target(&item.read_conversation.target) == Some(survivor)
            {
                item.read_conversation = owner.clone();
            }
        }
    }
    pub(crate) fn cycle_unread(&mut self) {
        let order = self.ordered_windows();
        let current = self
            .windows
            .active
            .and_then(|key| order.iter().position(|item| *item == key))
            .unwrap_or(0);
        for offset in 1..=order.len() {
            let key = order[(current + offset) % order.len()];
            if key
                .conversation
                .and_then(|id| {
                    self.session_index(key.conn)
                        .and_then(|index| self.sessions[index].conversation_channel(id))
                })
                .is_some_and(|channel| channel.unread > 0)
            {
                self.user_activate(key);
                if let Some(ctx) = &self.ui_ctx {
                    self.focus_active_viewport(ctx);
                }
                break;
            }
        }
    }
    pub(crate) fn toggle_activity(&mut self, owner: Option<WindowKey>) {
        self.activity.open = self.activity.owner != owner || !self.activity.open;
        self.activity.owner = owner;
    }
    pub(crate) fn draw_activity(&mut self, ctx: &egui::Context, owner: Option<WindowKey>) {
        if !self.activity.open || self.activity.owner != owner {
            return;
        }
        let mut open = true;
        let mut navigate = None;
        egui::Window::new("Activity")
            .id(egui::Id::unique("linefeed_activity"))
            .open(&mut open)
            .default_size([420.0, 420.0])
            .min_size([120.0, 80.0])
            .max_size(
                (ctx.content_rect().size() - egui::vec2(24.0, 24.0)).max(egui::vec2(120.0, 80.0)),
            )
            .constrain_to(ctx.content_rect())
            .show(ctx, |ui| {
                if ui.button("Clear Activity unread").clicked() {
                    for item in &mut self.activity.items {
                        item.read = true;
                    }
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.activity.items.is_empty() {
                        ui.label("Mentions and private messages will appear here.");
                    }
                    for item in &mut self.activity.items {
                        let label = format!(
                            "{}{}\n{}",
                            if item.mention { "@ " } else { "PM " },
                            item.title,
                            item.preview
                        );
                        if ui.selectable_label(!item.read, label).clicked() {
                            item.read = true;
                            navigate = Some(item.reference.clone());
                        }
                    }
                });
            });
        self.activity.open = open;
        if let Some(reference) = navigate {
            self.claim_global_dialogs(owner);
            self.navigate_message(reference);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::identity::{LogicalNetworkKey, TransportEndpointKey};
    use super::*;
    use crate::gui::{Settings, windows::LayoutFile};
    fn event(target: &str, private: bool, mention: bool) -> AttentionEvent {
        AttentionEvent {
            reference: MessageRef {
                conversation: ConversationKey {
                    network: LogicalNetworkKey::Direct {
                        endpoint: TransportEndpointKey::new("irc.example", 6697, true),
                    },
                    target: target.into(),
                },
                archive_id: Some("row".into()),
                server_msgid: Some("msgid".into()),
                server_time: None,
            },
            private,
            mention,
        }
    }
    fn app() -> App {
        App::with_settings(
            Settings {
                logging_enabled: false,
                logging_load_history: false,
                ..Settings::default()
            },
            LayoutFile::default(),
        )
    }
    #[test]
    fn legacy_notification_defaults_migrate_without_enabling_sound_or_sharing() {
        let settings: Settings = serde_json::from_str("{\"notifications_enabled\":false}").unwrap();
        assert!(!settings.notifications_enabled);
        assert!(settings.attention.show_typing);
        assert!(!settings.attention.share_typing);
        assert!(!settings.attention.sound);
        assert!(settings.attention.rules.is_empty());
    }
    #[test]
    fn conversation_rules_have_explicit_precedence() {
        let mut settings = AttentionSettings::default();
        let pm = event("alice", true, false);
        assert!(settings.eligible(&pm));
        assert!(!settings.sound_for(&pm));
        settings.rule_mut(pm.reference.conversation.clone()).level = AlertLevel::Muted;
        assert!(!settings.eligible(&pm));
        settings.rule_mut(pm.reference.conversation.clone()).level = AlertLevel::Mentions;
        assert!(!settings.eligible(&pm));
        let mention = AttentionEvent {
            mention: true,
            ..pm.clone()
        };
        assert!(settings.eligible(&mention));
        settings.sound = true;
        assert!(settings.sound_for(&pm));
        settings.rule_mut(pm.reference.conversation.clone()).sound = AlertSound::Off;
        assert!(!settings.sound_for(&pm));
        settings.sound = false;
        settings.rule_mut(pm.reference.conversation.clone()).sound = AlertSound::Default;
        assert!(settings.sound_for(&pm));
    }
    #[test]
    fn muting_stops_desktop_alerts_but_keeps_activity() {
        let mut app = app();
        let pm = event("alice", true, false);
        app.settings
            .attention
            .rule_mut(pm.reference.conversation.clone())
            .level = AlertLevel::Muted;
        app.handle_attention(pm, "PM from alice", "hello", true);
        assert!(app.dispatched_notifications.is_empty());
        assert_eq!(app.activity.items.len(), 1);
        assert!(!app.activity.items[0].read);
    }
    #[test]
    fn global_master_stops_desktop_alerts_but_keeps_activity() {
        let mut app = app();
        app.settings.notifications_enabled = false;
        app.handle_attention(event("alice", true, false), "PM from alice", "hello", true);
        assert!(app.dispatched_notifications.is_empty());
        assert_eq!(app.activity.items.len(), 1);
    }
    #[test]
    fn durable_activity_never_contains_preview_or_body() {
        let mut app = app();
        let pm = event("alice", true, false);
        app.handle_attention(
            pm.clone(),
            "private title",
            "private body must stay in memory",
            true,
        );
        app.handle_attention(pm, "private title", "duplicate", true);
        assert_eq!(app.activity.items.len(), 1);
        let serialized = serde_json::to_string(&app.activity_records()).unwrap();
        assert!(!serialized.contains("private body"));
        assert!(!serialized.contains("private title"));
        let records = app.activity_records();
        app.restore_activity(records);
        assert_eq!(app.activity.items.len(), 1);
        assert_eq!(
            app.activity.items[0].reference.server_msgid.as_deref(),
            Some("msgid")
        );
        assert!(!app.activity.items[0].preview.contains("private body"));
    }
    #[test]
    fn redaction_removes_cached_activity_body_and_preserves_navigation_reference() {
        let mut app = app();
        let event = event("alice", true, false);
        app.handle_attention(
            event.clone(),
            "PM from alice",
            "must disappear after redaction",
            true,
        );
        assert!(app.activity.items[0].preview.contains("must disappear"));
        app.redact_activity(&event.reference);
        assert_eq!(app.activity.items[0].preview, "<message redacted>");
        assert_eq!(app.activity.items[0].reference, event.reference);
        assert!(!format!("{:?}", app.activity.items).contains("must disappear"));
    }

    #[test]
    fn reading_a_renamed_query_marks_its_existing_activity_seen() {
        let mut app = app();
        let index = app.add_session();
        app.sessions[index].set_session_nick("me".into(), true);
        assert!(app.sessions[index].open_query("alice"));
        let id = app.sessions[index].id_for_target("alice").unwrap();
        let key = WindowKey::new(app.sessions[index].id, Some(id));
        app.windows.sync(&[super::super::windows::PresentWindow {
            key,
            persist: super::super::windows::PersistKey {
                network: "test".into(),
                target: Some("alice".into()),
            },
        }]);
        let message = super::super::ChatMessage::new_fmt("alice", "hello", "short");
        let before = app.sessions[index].live_message_reference("alice", &message);
        app.sessions[index]
            .conversations
            .get_mut("alice")
            .unwrap()
            .messages
            .push_back(message);
        app.handle_attention(
            AttentionEvent {
                reference: before,
                private: true,
                mention: false,
            },
            "PM from alice",
            "hello",
            false,
        );
        app.sessions[index].handle_incoming_message(
            crate::irc::IrcMessage::parse(":alice!u@host NICK :bob").unwrap(),
        );
        app.process_events();
        assert_eq!(app.sessions[index].target_name(id), Some("bob"));
        let archived_reference = app.activity.items[0].reference.clone();
        assert_eq!(archived_reference.conversation.target, "alice");
        assert_eq!(app.activity.items[0].read_conversation.target, "bob");
        let saved = serde_json::to_vec(&app.activity_records()).unwrap();
        app.restore_activity(serde_json::from_slice(&saved).unwrap());
        assert_eq!(app.activity.items[0].reference, archived_reference);
        let message = app.sessions[index]
            .conversation_channel(id)
            .unwrap()
            .messages
            .back()
            .unwrap();
        let read = app.sessions[index].live_message_reference("bob", message);
        app.windows
            .get_mut(&key)
            .unwrap()
            .view
            .timeline
            .get_mut()
            .last_read = Some(read);
        app.mark_activity_seen(key);
        assert!(
            app.activity.items[0].read,
            "the renamed query's read message stayed unread in Activity"
        );
        app.redact_activity(&archived_reference);
        assert_eq!(app.activity.items[0].preview, "<message redacted>");
    }

    #[test]
    fn legacy_activity_records_default_read_owner_to_the_archival_conversation() {
        let mut app = app();
        app.handle_attention(event("alice", true, false), "PM", "body", false);
        let mut saved = serde_json::to_value(app.activity_records()).unwrap();
        saved[0]
            .as_object_mut()
            .unwrap()
            .remove("read_conversation");
        app.restore_activity(serde_json::from_value(saved).unwrap());
        assert_eq!(
            app.activity.items[0].read_conversation,
            app.activity.items[0].reference.conversation
        );
    }

    #[test]
    fn merged_query_activity_keeps_archival_refs_and_marks_both_sources_seen() {
        let mut app = app();
        let index = app.add_session();
        app.sessions[index].set_session_nick("me".into(), true);
        let mut present = Vec::new();
        for target in ["alice", "bob"] {
            assert!(app.sessions[index].open_query(target));
            let id = app.sessions[index].id_for_target(target).unwrap();
            let message = super::super::ChatMessage::new_fmt(target, "hello", "short");
            let reference = app.sessions[index].live_message_reference(target, &message);
            app.sessions[index]
                .conversations
                .get_mut(target)
                .unwrap()
                .messages
                .push_back(message);
            app.handle_attention(
                AttentionEvent {
                    reference,
                    private: true,
                    mention: false,
                },
                target,
                "hello",
                false,
            );
            present.push(super::super::windows::PresentWindow {
                key: WindowKey::new(app.sessions[index].id, Some(id)),
                persist: super::super::windows::PersistKey {
                    network: "test".into(),
                    target: Some(target.into()),
                },
            });
        }
        app.windows.sync(&present);
        let survivor = app.sessions[index].id_for_target("alice").unwrap();
        app.sessions[index].handle_incoming_message(
            crate::irc::IrcMessage::parse(":alice!u@h NICK :bob").unwrap(),
        );
        app.process_events();
        let key = WindowKey::new(app.sessions[index].id, Some(survivor));
        let latest = app.sessions[index]
            .conversation_channel(survivor)
            .unwrap()
            .messages
            .back()
            .unwrap();
        let read = app.sessions[index].live_message_reference("bob", latest);
        app.windows
            .get_mut(&key)
            .unwrap()
            .view
            .timeline
            .get_mut()
            .last_read = Some(read);
        app.mark_activity_seen(key);
        assert_eq!(app.activity.items.len(), 2);
        assert!(app.activity.items.iter().all(|item| item.read));
        assert!(
            app.activity
                .items
                .iter()
                .any(|item| item.reference.conversation.target == "alice")
        );
        assert!(
            app.activity
                .items
                .iter()
                .any(|item| item.reference.conversation.target == "bob")
        );
    }
}
