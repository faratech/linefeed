//! History replies retain the request's conversation identity and wire metadata.

use super::*;

fn session() -> (Session, mpsc::UnboundedReceiver<IrcCommand>) {
    let mut session = Session::with_settings(Settings {
        logging_enabled: false,
        logging_load_history: true,
        set_invisible: false,
        ..Settings::default()
    });
    session.set_session_nick("me".into(), true);
    let (tx, rx) = mpsc::unbounded_channel();
    session.test_set_registered(tx);
    feed(
        &mut session,
        ":srv CAP me ACK :draft/chathistory message-tags labeled-response",
    );
    (session, rx)
}

fn feed(session: &mut Session, line: &str) {
    session.handle_incoming_message(IrcMessage::parse(line).unwrap());
}

fn drain(rx: &mut mpsc::UnboundedReceiver<IrcCommand>) -> Vec<IrcCommand> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn history_row(session: &mut Session, batch: &str, target: &str, id: &str) {
    feed(
        session,
        &format!(
            "@batch={batch};time=2026-11-14T22:00:00.000Z;msgid={id} :alice!a@h PRIVMSG {target} :{id}"
        ),
    );
}

fn has_id(session: &Session, target: &str, id: &str) -> bool {
    session.conversations.get(target).is_some_and(|channel| {
        channel
            .messages
            .iter()
            .any(|row| row.msgid.as_deref() == Some(id))
    })
}

#[test]
fn late_failure_stays_visible_without_reopening_a_parted_conversation() {
    let (mut session, _rx) = session();
    feed(&mut session, ":me!u@h JOIN #room");
    feed(&mut session, ":me!u@h PART #room :leaving");
    assert!(session.id_for_target("#room").is_none());
    feed(
        &mut session,
        ":srv FAIL CHATHISTORY INVALID_TARGET LATEST #room :No access to target",
    );
    assert!(session.id_for_target("#room").is_none());
    assert!(session.pending_history_requests.is_empty());
    assert!(session.server_messages.iter().any(|row| {
        row.content
            .contains("[FAIL CHATHISTORY/INVALID_TARGET] LATEST #room")
    }));
}

#[test]
fn old_failure_does_not_change_a_reopened_tab_or_consume_its_request() {
    let (mut session, _rx) = session();
    feed(&mut session, ":me!u@h JOIN #room");
    let retired = session.id_for_target("#room").unwrap();
    feed(&mut session, ":me!u@h PART #room :leaving");
    feed(&mut session, ":me!u@h JOIN #room");
    let reopened = session.id_for_target("#room").unwrap();
    assert_ne!(retired, reopened);
    let before: Vec<_> = session
        .conversations
        .channel(reopened)
        .unwrap()
        .messages
        .iter()
        .map(|row| row.content.clone())
        .collect();
    feed(
        &mut session,
        ":srv FAIL CHATHISTORY INVALID_TARGET LATEST #room :No access to target",
    );
    assert_eq!(
        session
            .conversations
            .channel(reopened)
            .unwrap()
            .messages
            .iter()
            .map(|row| row.content.clone())
            .collect::<Vec<_>>(),
        before
    );
    assert_eq!(session.pending_history_requests["#room"].len(), 1);
    assert_eq!(
        session.pending_history_requests["#room"][0]
            .correlation
            .as_ref()
            .unwrap()
            .owner,
        Some(reopened)
    );
    assert!(
        session
            .server_messages
            .iter()
            .any(|row| row.content.contains("No access to target"))
    );
}

#[test]
fn old_success_cannot_attach_to_a_reopened_conversation() {
    for batch_started_before_part in [false, true] {
        let (mut session, mut rx) = session();
        feed(&mut session, ":me!u@h JOIN #room");
        let retired = session.id_for_target("#room").unwrap();
        if batch_started_before_part {
            feed(&mut session, ":srv BATCH +old chathistory #room");
        }
        feed(&mut session, ":me!u@h PART #room :leaving");
        feed(&mut session, ":me!u@h JOIN #room");
        assert_ne!(session.id_for_target("#room"), Some(retired));
        if !batch_started_before_part {
            feed(&mut session, ":srv BATCH +old chathistory #room");
        }
        drain(&mut rx);
        history_row(&mut session, "old", "#room", "stale-row");
        feed(&mut session, ":srv BATCH -old");
        assert!(!has_id(&session, "#room", "stale-row"));
        assert_eq!(session.pending_history_requests["#room"].len(), 1);
        assert!(drain(&mut rx).is_empty());
    }
}

#[test]
fn manual_around_failure_does_not_consume_later_automatic_latest() {
    let (mut session, mut rx) = session();
    session.process_command("/history AROUND #room msgid=manual-anchor 5");
    assert!(matches!(
        rx.try_recv().unwrap(),
        IrcCommand::ChathistoryAround(..)
    ));
    feed(&mut session, ":me!u@h JOIN #room");
    assert!(matches!(
        rx.try_recv().unwrap(),
        IrcCommand::ChathistoryLatest(..)
    ));
    assert_eq!(session.pending_history_requests["#room"].len(), 2);
    feed(
        &mut session,
        ":srv FAIL CHATHISTORY INVALID_TARGET AROUND #room :No access to target",
    );
    let remaining = &session.pending_history_requests["#room"];
    assert_eq!(remaining.len(), 1);
    assert_eq!(
        remaining[0].correlation.as_ref().unwrap().subcommand,
        HistorySubcommand::Latest
    );
    assert!(
        session
            .conversations
            .get("#room")
            .unwrap()
            .messages
            .iter()
            .any(|row| row.content.contains("INVALID_TARGET] AROUND #room"))
    );
    feed(&mut session, ":srv BATCH +latest chathistory #room");
    history_row(&mut session, "latest", "#room", "current-row");
    feed(&mut session, ":srv BATCH -latest");
    assert!(session.pending_history_requests.is_empty());
    assert!(has_id(&session, "#room", "current-row"));
}

#[test]
fn mismatched_failure_does_not_consume_fifo_head() {
    let (mut session, _rx) = session();
    feed(&mut session, ":me!u@h JOIN #room");
    feed(
        &mut session,
        ":srv FAIL CHATHISTORY INVALID_TARGET AROUND #room :Unmatched failure",
    );
    assert_eq!(session.pending_history_requests["#room"].len(), 1);
    assert!(
        session
            .server_messages
            .iter()
            .any(|row| row.content.contains("Unmatched failure"))
    );
}

#[test]
fn single_page_around_reply_keeps_later_before_pagination_metadata() {
    let (mut session, mut rx) = session();
    session.chathistory_limit = Some(1);
    session.process_command("/history AROUND #room msgid=around 1");
    session.process_command("/history BEFORE #room msgid=before 3");
    drain(&mut rx);
    feed(&mut session, ":srv BATCH +around chathistory #room");
    history_row(&mut session, "around", "#room", "around-row");
    feed(&mut session, ":srv BATCH -around");
    assert!(
        drain(&mut rx).is_empty(),
        "AROUND must not continue the later BEFORE request"
    );
    let waiting = &session.pending_history_requests["#room"];
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].remaining, 3);
    assert_eq!(
        waiting[0].correlation.as_ref().unwrap().subcommand,
        HistorySubcommand::Before
    );
}

#[test]
fn raw_quote_label_and_between_commands_share_request_tracking() {
    let (mut session, mut rx) = session();
    for (command, target, kind, label) in [
        (
            "/raw CHATHISTORY LATEST #raw * 2",
            "#raw",
            HistorySubcommand::Latest,
            None,
        ),
        (
            "/quote CHATHISTORY BEFORE #quote msgid=q 2",
            "#quote",
            HistorySubcommand::Before,
            None,
        ),
        (
            "/label explicit CHATHISTORY AROUND #label msgid=x 2",
            "#label",
            HistorySubcommand::Around,
            Some("explicit"),
        ),
        (
            "/raw @label=raw-label CHATHISTORY BETWEEN #tagged msgid=a msgid=b 2",
            "#tagged",
            HistorySubcommand::Between,
            Some("raw-label"),
        ),
        (
            "/history BETWEEN #between msgid=a msgid=b 2",
            "#between",
            HistorySubcommand::Between,
            None,
        ),
    ] {
        session.process_command(command);
        assert!(rx.try_recv().is_ok(), "{command}");
        let pending = &session.pending_history_requests[target];
        assert_eq!(pending.len(), 1, "{command}");
        let correlation = pending[0].correlation.as_ref().unwrap();
        assert_eq!(correlation.subcommand, kind);
        assert_eq!(correlation.label.as_deref(), label);
        assert_eq!(correlation.owner, session.id_for_target(target));
    }
}

#[test]
fn labeled_failures_match_their_own_request_even_out_of_order() {
    let (mut session, _rx) = session();
    session.process_command("/label first CHATHISTORY LATEST #room * 2");
    session.process_command("/label second CHATHISTORY AFTER #room msgid=a 2");
    feed(
        &mut session,
        "@label=second :srv FAIL CHATHISTORY INVALID_TARGET AFTER #room :Second failed",
    );
    assert_eq!(session.pending_history_requests["#room"].len(), 1);
    assert_eq!(
        session.pending_history_requests["#room"][0]
            .correlation
            .as_ref()
            .unwrap()
            .label
            .as_deref(),
        Some("first")
    );
    feed(
        &mut session,
        "@label=unknown :srv FAIL CHATHISTORY INVALID_TARGET LATEST #room :Unknown failed",
    );
    assert_eq!(session.pending_history_requests["#room"].len(), 1);
    assert!(
        session
            .server_messages
            .iter()
            .any(|row| row.content.contains("Unknown failed"))
    );
}

#[test]
fn labeled_response_parent_identifies_a_nested_history_batch() {
    let (mut session, _rx) = session();
    session.process_command("/label first CHATHISTORY LATEST #room * 2");
    session.process_command("/label second CHATHISTORY AFTER #room msgid=a 2");
    feed(
        &mut session,
        "@label=second :srv BATCH +reply labeled-response",
    );
    feed(
        &mut session,
        "@batch=reply :srv BATCH +history chathistory #room",
    );
    history_row(&mut session, "history", "#room", "labeled-row");
    feed(&mut session, ":srv BATCH -history");
    feed(&mut session, ":srv BATCH -reply");
    assert!(has_id(&session, "#room", "labeled-row"));
    assert_eq!(session.pending_history_requests["#room"].len(), 1);
    assert_eq!(
        session.pending_history_requests["#room"][0]
            .correlation
            .as_ref()
            .unwrap()
            .label
            .as_deref(),
        Some("first")
    );
    assert!(session.batch_labels.is_empty());
}

#[test]
fn pending_and_open_history_follow_renames_and_merges_by_identity() {
    for open_before_rename in [false, true] {
        let (mut session, _rx) = session();
        // The explicit rename preserves #source's ID and merges #destination.
        session.ensure_history_target("#source");
        let survivor = session.id_for_target("#source").unwrap();
        session.process_command("/history #destination 2");
        let retired = session.id_for_target("#destination").unwrap();
        if open_before_rename {
            feed(&mut session, ":srv BATCH +history chathistory #destination");
        }
        feed(&mut session, ":srv RENAME #source #destination :renamed");
        assert_eq!(session.id_for_target("#destination"), Some(survivor));
        assert!(session.target_name(retired).is_none());
        if !open_before_rename {
            feed(&mut session, ":srv BATCH +history chathistory #destination");
        }
        history_row(&mut session, "history", "#destination", "merged-row");
        feed(&mut session, ":srv BATCH -history");
        assert!(has_id(&session, "#destination", "merged-row"));
        assert_eq!(session.id_for_target("#destination"), Some(survivor));
    }
}

#[test]
fn targets_requests_are_retired_by_their_own_batch() {
    let (mut session, _rx) = session();
    session.process_command(
        "/history TARGETS timestamp=2026-01-01T00:00:00.000Z timestamp=2026-11-14T00:00:00.000Z 5",
    );
    assert_eq!(session.pending_history_requests[""].len(), 1);
    feed(
        &mut session,
        ":srv BATCH +targets draft/chathistory-targets",
    );
    feed(&mut session, ":srv BATCH -targets");
    assert!(session.pending_history_requests.is_empty());
}

#[test]
fn failed_transport_does_not_create_a_history_tab_or_request() {
    let (mut session, _rx) = session();
    session.test_drop_transport();
    session.process_command("/history AROUND #room msgid=x 2");
    assert!(session.id_for_target("#room").is_none());
    assert!(session.pending_history_requests.is_empty());
}

#[test]
fn explicit_after_failure_does_not_leave_a_failed_request_behind_before() {
    let (mut session, _rx) = session();
    session.process_command("/history BEFORE #room msgid=before 3");
    session.process_command("/history AFTER #room msgid=after 3");
    feed(
        &mut session,
        ":srv FAIL CHATHISTORY INVALID_TARGET AFTER #room :After failed first",
    );
    let waiting = &session.pending_history_requests["#room"];
    assert_eq!(waiting.len(), 1);
    assert_eq!(
        waiting[0].correlation.as_ref().unwrap().subcommand,
        HistorySubcommand::Before
    );
}

#[test]
fn renamed_then_closed_request_keeps_aliases_separate_from_reopened_target() {
    let (mut session, _rx) = session();
    feed(&mut session, ":me!u@h JOIN #old");
    let retired = session.id_for_target("#old").unwrap();
    feed(&mut session, ":srv RENAME #old #middle :renamed");
    feed(&mut session, ":srv RENAME #middle #new :renamed again");
    feed(&mut session, ":me!u@h PART #new :leaving");
    feed(&mut session, ":me!u@h JOIN #new");
    assert_ne!(session.id_for_target("#new"), Some(retired));
    feed(&mut session, ":srv BATCH +old chathistory #middle");
    history_row(&mut session, "old", "#middle", "retired-renamed-row");
    feed(&mut session, ":srv BATCH -old");
    assert!(!has_id(&session, "#new", "retired-renamed-row"));
    assert!(session.id_for_target("#middle").is_none());
    assert_eq!(session.pending_history_requests["#new"].len(), 1);
    assert!(!session.pending_history_requests.contains_key("#old"));
}

#[test]
fn cycle_retires_old_history_while_preserving_the_view_identity() {
    let (mut session, mut rx) = session();
    feed(&mut session, ":me!u@h JOIN #room");
    let owner = session.id_for_target("#room").unwrap();
    assert!(session.cycle_conversation(owner));
    feed(&mut session, ":me!u@h PART #room :Cycling");
    feed(&mut session, ":me!u@h JOIN #room");
    assert_eq!(session.id_for_target("#room"), Some(owner));
    drain(&mut rx);
    feed(&mut session, ":srv BATCH +old chathistory #room");
    history_row(&mut session, "old", "#room", "before-cycle");
    feed(&mut session, ":srv BATCH -old");
    assert!(!has_id(&session, "#room", "before-cycle"));
    assert_eq!(session.pending_history_requests["#room"].len(), 1);
    assert_eq!(
        session.pending_history_requests["#room"][0]
            .correlation
            .as_ref()
            .unwrap()
            .owner,
        Some(owner)
    );
    assert!(drain(&mut rx).is_empty());
}
