use super::*;

fn identity_session(mechanism: SaslMechanism) -> Session {
    Session::with_settings(Settings {
        server_host: "irc.example".into(),
        server_port: "6697".into(),
        use_tls: true,
        nickname: "oldnick".into(),
        sasl_username: "alice".into(),
        sasl_password: "dummy-password".into(),
        sasl_mechanism: mechanism,
        sasl_required: true,
        tls_client_cert_path: "same-client.pem".into(),
        logging_enabled: false,
        logging_load_history: false,
        ..Settings::default()
    })
}

fn establish_identity_state(session: &mut Session) {
    session.activate_session(session.session_from_form().unwrap());
    session.set_session_nick("oldnick".into(), true);
    session.set_session_nick("oldnick_".into(), true);
    session.away_status = Some("old account away".into());
    session.auto_away_triggered = true;
}

#[test]
fn auto_client_certificate_account_switch_clears_away_and_nick_lineage() {
    let mut session = identity_session(SaslMechanism::Auto);
    establish_identity_state(&mut session);

    session.sasl_username = "bob".into();
    session.nickname = "newnick".into();
    session.activate_session(session.session_from_form().unwrap());

    assert!(session.away_status.is_none());
    assert!(!session.auto_away_triggered);
    assert!(session.past_nicks.is_empty());
    assert!(!session.sender_is_self("oldnick"));
    assert!(!session.sender_is_self("oldnick_"));
    assert!(!session.nick_confirmed);
}

#[test]
fn pending_auto_client_certificate_account_switch_snapshots_no_old_away_state() {
    let mut session = identity_session(SaslMechanism::Auto);
    establish_identity_state(&mut session);
    let (sender, mut commands) = mpsc::unbounded_channel();
    session.test_set_registered(sender);

    session.sasl_username = "bob".into();
    session.nickname = "newnick".into();
    assert!(session.switch_session_from_form("Changing account"));
    assert!(matches!(commands.try_recv(), Ok(IrcCommand::Quit(_))));

    // Until close completes, the old socket still owns its old account state.
    assert_eq!(session.away_status.as_deref(), Some("old account away"));
    let snapshot = session.update_session();
    assert!(snapshot.away.is_none());
    assert_eq!(
        snapshot.session.unwrap().server.sasl_username.as_deref(),
        Some("bob")
    );
}

#[test]
fn explicit_external_ignores_password_account_but_tracks_certificate_changes() {
    let mut session = identity_session(SaslMechanism::External);
    establish_identity_state(&mut session);

    session.sasl_username = "bob".into();
    session.nickname = "newnick".into();
    session.activate_session(session.session_from_form().unwrap());
    assert_eq!(session.away_status.as_deref(), Some("old account away"));
    assert!(session.sender_is_self("oldnick"));

    session.tls_client_cert_path = "other-client.pem".into();
    session.nickname = "othernick".into();
    session.activate_session(session.session_from_form().unwrap());
    assert!(session.away_status.is_none());
    assert!(!session.auto_away_triggered);
    assert!(session.past_nicks.is_empty());
    assert!(!session.sender_is_self("oldnick"));
    assert!(!session.sender_is_self("newnick"));
}
