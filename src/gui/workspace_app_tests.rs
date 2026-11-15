use super::*;

fn app() -> App {
    let mut app = App::with_settings(
        Settings {
            server_host: "workspace.example".into(),
            nickname: "me".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Default::default()
        },
        LayoutFile::default(),
    );
    app.start(None);
    assert!(app.sessions[0].start_session_from_form());
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    app.sessions[0].test_set_registered(sender);
    app
}

#[test]
fn normal_workspace_restores_all_views_and_offline_sessions_without_form_edits() {
    let mut original = app();
    let conn = original.sessions[0].id;
    let room = original.sessions[0].ensure_target("#room");
    original.sessions[0]
        .conversation_channel_mut(room)
        .unwrap()
        .joined = true;
    let query = original.sessions[0].ensure_target("Alice");
    original.sync_windows();
    original
        .windows
        .get_mut(&WindowKey::new(conn, Some(room)))
        .unwrap()
        .view
        .input_text = "channel draft".into();
    original
        .windows
        .get_mut(&WindowKey::new(conn, Some(query)))
        .unwrap()
        .view
        .input_text = "private draft".into();
    original
        .windows
        .get_mut(&WindowKey::new(conn, None))
        .unwrap()
        .view
        .input_text = "status draft".into();
    original.activate(WindowKey::new(conn, Some(query)));
    // A settings scratch endpoint cannot replace the active immutable config.
    original.sessions[0].server_host = "unsaved.example".into();
    let snapshot = original.capture_workspace();
    assert_eq!(
        snapshot.sessions[0]
            .state
            .session
            .as_ref()
            .unwrap()
            .server
            .host,
        "workspace.example"
    );
    let mut restored = App::with_settings(original.settings.clone(), LayoutFile::default());
    restored.workspace.restored = Some(snapshot);
    restored.start(None);
    assert!(restored.sessions[0].is_connecting());
    assert_eq!(restored.sessions[0].current_target_name(), Some("Alice"));
    restored.sync_windows();
    restored.checkpoint_workspace();
    let conn = restored.sessions[0].id;
    let room = restored.sessions[0].id_for_target("#room").unwrap();
    let query = restored.sessions[0].id_for_target("Alice").unwrap();
    assert_eq!(
        restored
            .windows
            .get(&WindowKey::new(conn, Some(room)))
            .unwrap()
            .view
            .input_text,
        "channel draft"
    );
    assert_eq!(
        restored
            .windows
            .get(&WindowKey::new(conn, Some(query)))
            .unwrap()
            .view
            .input_text,
        "private draft"
    );
    assert_eq!(
        restored
            .windows
            .get(&WindowKey::new(conn, None))
            .unwrap()
            .view
            .input_text,
        "status draft"
    );

    original.sessions[0].request_manual_disconnect(None);
    let snapshot = original.capture_workspace();
    assert!(!snapshot.sessions[0].state.reconnect);
    let mut offline = App::with_settings(original.settings.clone(), LayoutFile::default());
    offline.restore_workspace(snapshot);
    assert!(!offline.sessions[0].is_connected());
    assert!(!offline.sessions[0].is_connecting());
    assert!(offline.sessions[0].id_for_target("Alice").is_some());
}

#[test]
fn staged_network_switch_and_closed_status_do_not_restore_old_drafts() {
    let mut original = app();
    original.sync_windows();
    let conn = original.sessions[0].id;
    original
        .windows
        .get_mut(&WindowKey::new(conn, None))
        .unwrap()
        .view
        .input_text = "old network draft".into();
    original.sessions[0].apply_unprofiled_endpoint("new.example", "6697", true);
    assert!(original.sessions[0].switch_session_from_form("Changing network"));
    let snapshot = original.capture_workspace();
    assert!(snapshot.sessions[0].state.reconnect);
    assert!(snapshot.sessions[0].views.is_empty());
    assert_eq!(
        snapshot.sessions[0]
            .state
            .session
            .as_ref()
            .unwrap()
            .server
            .host,
        "new.example"
    );
    original.closing_sessions.insert(conn);
    assert!(original.capture_workspace().sessions.is_empty());
}

#[test]
fn sensitive_drafts_are_not_checkpointed() {
    let mut original = app();
    original.sync_windows();
    let conn = original.sessions[0].id;
    original
        .windows
        .get_mut(&WindowKey::new(conn, None))
        .unwrap()
        .view
        .input_text = "/ns identify unsent-example-secret".into();
    let encoded = serde_json::to_string(&original.capture_workspace()).unwrap();
    assert!(!encoded.contains("unsent-example-secret"));
}

#[test]
fn archived_reading_context_keeps_stable_ids_new_arrivals_draft_and_unread_state() {
    use crate::gui::identity::{ConversationKey, MessageRef, ScrollAnchor};
    use crate::gui::workspace::WorkspaceViewState;
    let mut app = app();
    let network = app.sessions[0].logical_network_key().unwrap();
    let conversation = ConversationKey {
        network,
        target: "alice".into(),
    };
    let id = app.sessions[0].ensure_target("alice");
    let mut old = ChatMessage::new_fmt("alice", "saved row", "short");
    old.timestamp = "01:00".into();
    let old_key = old.message_key.clone();
    app.sessions[0]
        .conversation_channel_mut(id)
        .unwrap()
        .messages
        .push_back(old.clone());
    app.sessions[0].conversation_channel_mut(id).unwrap().unread = 7;
    app.sync_windows();
    let key = WindowKey::new(app.sessions[0].id, Some(id));
    let reference = MessageRef {
        conversation: conversation.clone(),
        archive_id: Some("archived-old".into()),
        server_msgid: Some("old-msgid".into()),
        server_time: Some("2026-11-15T01:00:00.000Z".into()),
    };
    let anchor = ScrollAnchor {
        message: Some(reference.clone()),
        fraction: 0.25,
        at_bottom: false,
    };
    let window = app.windows.get_mut(&key).unwrap();
    window.view.restore_workspace_state(WorkspaceViewState {
        draft: "still typing".into(),
        last_read: Some(reference.clone()),
        viewport: Some(anchor.clone()),
        ..Default::default()
    });
    window.view.set_workspace_hydrating(true);
    app.workspace_restores
        .insert(1, (key, anchor, HashSet::from([old_key])));
    let mut live = ChatMessage::new_fmt("me", "new optimistic row", "short");
    live.pending_echo = true;
    let live_key = live.message_key.clone();
    app.sessions[0]
        .conversation_channel_mut(id)
        .unwrap()
        .messages
        .push_back(live.clone());
    let mut archived = crate::gui::archive::ArchiveRecord::from_message(conversation.clone(), &old);
    archived.id = "archived-old".into();
    archived.msgid = Some("old-msgid".into());
    archived.server_time = reference.server_time.clone();
    let live_record = crate::gui::archive::ArchiveRecord::from_message(conversation, &live);
    app.handle_workspace_archive_reply(1, reference, vec![archived, live_record]);
    let channel = app.sessions[0].conversation_channel(id).unwrap();
    assert_eq!(channel.unread, 7);
    assert_eq!(
        channel
            .messages
            .iter()
            .filter(|row| row.content == "saved row")
            .count(),
        1
    );
    assert!(
        channel
            .messages
            .iter()
            .any(|row| row.message_key == "archived-old"
                && row.msgid.as_deref() == Some("old-msgid"))
    );
    assert!(
        channel
            .messages
            .iter()
            .any(|row| row.message_key == live_key && row.pending_echo)
    );
    assert_eq!(
        channel
            .messages
            .iter()
            .filter(|row| row.content == "new optimistic row")
            .count(),
        1
    );
    let window = app.windows.get(&key).unwrap();
    assert_eq!(window.view.input_text, "still typing");
    assert!(!window.view.timeline.borrow().hydrating);
    assert!(window.view.timeline.borrow().restore_pending);
}

#[test]
fn closed_or_replaced_workspace_owner_discards_late_archive_context() {
    use crate::gui::identity::{ConversationKey, MessageRef, ScrollAnchor};
    let mut app = app();
    let old = app.sessions[0].ensure_target("alice");
    let conversation = ConversationKey {
        network: app.sessions[0].logical_network_key().unwrap(),
        target: "alice".into(),
    };
    let reference = MessageRef {
        conversation,
        archive_id: Some("old-row".into()),
        server_msgid: None,
        server_time: None,
    };
    let key = WindowKey::new(app.sessions[0].id, Some(old));
    app.workspace_restores.insert(
        1,
        (
            key,
            ScrollAnchor {
                message: Some(reference.clone()),
                fraction: 0.0,
                at_bottom: false,
            },
            HashSet::new(),
        ),
    );
    app.sessions[0].remove_channel("alice");
    let replacement = app.sessions[0].ensure_target("alice");
    assert_ne!(old, replacement);
    app.handle_workspace_archive_reply(1, reference, Vec::new());
    assert!(
        app.sessions[0]
            .conversation_channel(replacement)
            .unwrap()
            .messages
            .is_empty()
    );
    assert!(app.workspace_restores.is_empty());
}

#[test]
fn text_log_reading_anchor_survives_workspace_reload_without_server_message_ids() {
    use crate::gui::identity::{MessageRef, ScrollAnchor};
    let root = std::env::temp_dir().join(format!(
        "linefeed-workspace-text-{}",
        super::super::new_profile_id()
    ));
    let manager = crate::gui::logging::LogManager::with_test_dir(root.clone());
    let mut original = app();
    let network = original.sessions[0].logical_network_key().unwrap();
    let target = original.sessions[0].ensure_target("alice");
    manager.log_message(
        &network.storage_key(),
        "alice",
        &ChatMessage::new_fmt("alice", "older saved row", "short"),
    );
    let loaded = manager.load_history_with_legacy(&network.storage_key(), "", "alice", 100, false);
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].msgid.is_none());
    let reference = MessageRef {
        conversation: crate::gui::identity::ConversationKey {
            network: network.clone(),
            target: "alice".into(),
        },
        archive_id: Some(loaded[0].message_key.clone()),
        server_msgid: None,
        server_time: None,
    };
    original.sessions[0]
        .conversation_channel_mut(target)
        .unwrap()
        .messages = loaded.into();
    original.sync_windows();
    let key = WindowKey::new(original.sessions[0].id, Some(target));
    original
        .windows
        .get_mut(&key)
        .unwrap()
        .view
        .timeline
        .get_mut()
        .anchor = Some(ScrollAnchor {
        message: Some(reference),
        fraction: 0.25,
        at_bottom: false,
    });
    original.sessions[0].request_manual_disconnect(None);
    let bytes = serde_json::to_vec(&original.capture_workspace()).unwrap();
    let mut restored = App::with_settings(original.settings.clone(), LayoutFile::default());
    restored.workspace.restored = Some(serde_json::from_slice(&bytes).unwrap());
    restored.start(None);
    restored.sync_windows();
    restored.checkpoint_workspace();
    let target = restored.sessions[0].id_for_target("alice").unwrap();
    let loaded_again =
        manager.load_history_with_legacy(&network.storage_key(), "", "alice", 100, false);
    restored.sessions[0]
        .conversation_channel_mut(target)
        .unwrap()
        .messages = loaded_again.into();
    let key = WindowKey::new(restored.sessions[0].id, Some(target));
    let anchor = restored
        .windows
        .get(&key)
        .unwrap()
        .view
        .timeline
        .borrow()
        .anchor
        .clone()
        .unwrap();
    assert_eq!(anchor.fraction, 0.25);
    let id = anchor.message.unwrap().archive_id.unwrap();
    assert!(
        restored.sessions[0]
            .conversation_channel(target)
            .unwrap()
            .messages
            .iter()
            .any(|row| row.message_key == id)
    );
    drop(manager);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn bouncer_upstreams_share_a_transport_without_being_duplicate_sessions() {
    use crate::gui::bouncer::{BouncerBinding, BouncerProvider};
    let mut app = App::with_settings(
        Settings {
            nickname: "me".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Default::default()
        },
        LayoutFile::default(),
    );
    for network in ["42", "43", "42"] {
        let index = app.add_session();
        let session = &mut app.sessions[index];
        session.server_host = "bnc.example".into();
        session.sasl_username = "alice".into();
        session.sasl_password = "example-password".into();
        session.bouncer = Some(BouncerBinding {
            provider: BouncerProvider::Soju,
            account: "alice".into(),
            network: network.into(),
            client_id: "desktop".into(),
        });
        let conn = session.id;
        app.open_settings(conn);
        assert_eq!(app.connect_settings_form(), network != "42" || index == 0);
        if index == 2 {
            assert!(
                app.sessions[index]
                    .connection_error()
                    .unwrap()
                    .contains("open connection")
            );
        }
    }
    assert_ne!(
        app.sessions[0].logical_network_key(),
        app.sessions[1].logical_network_key()
    );
    assert_eq!(
        app.sessions[0].active_endpoint_key(),
        app.sessions[1].active_endpoint_key()
    );
}
