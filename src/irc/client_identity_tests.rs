use super::*;

#[test]
fn client_batches_are_one_atomic_wire_envelope_and_never_log_their_body() {
    let commands = vec![
        IrcCommand::Raw("BATCH +compose draft/multiline #room".into()),
        IrcCommand::Privmsg("#room".into(), "first line".into()),
        IrcCommand::Privmsg("NickServ".into(), "IDENTIFY example-secret".into()),
        IrcCommand::Raw("BATCH -compose".into()),
    ];
    let batch = IrcCommand::ClientBatch(commands);
    let wire = outbound_line(&batch);
    assert_eq!(
        wire,
        "BATCH +compose draft/multiline #room\r\nPRIVMSG #room :first line\r\nPRIVMSG NickServ :IDENTIFY example-secret\r\nBATCH -compose\r\n"
    );
    assert!(!outbound_log_summary(&batch, &wire).contains("example-secret"));
    let invalid = IrcCommand::ClientBatch(vec![
        IrcCommand::Privmsg("#room".into(), "valid".into()),
        IrcCommand::Raw("PRIVMSG #room :bad\r\nOPER x y".into()),
    ]);
    assert!(
        outbound_line(&invalid).is_empty(),
        "reject the entire batch before writing any child"
    );
}

#[test]
fn accepted_chat_gets_a_receipt_only_after_the_socket_write_and_flush() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for fail in [false, true] {
            let (sender, mut queue) = mpsc::unbounded_channel();
            sender.send(IrcCommand::ClientChat { id: "submission-1".into(), commands: vec![IrcCommand::Privmsg("#room".into(), "accepted payload".into())] }).unwrap();
            let command = queue.recv().await.unwrap();
            let (mut socket, peer) = tokio::io::duplex(1024);
            let _peer = if fail { drop(peer); None } else { Some(peer) };
            let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured = events.clone();
            let callback: EventCallback = std::sync::Arc::new(move |event| captured.lock().unwrap().push(event));
            let result = write_outbound_command(&mut socket, &command, Some(&callback)).await;
            assert_eq!(result.is_ok(), !fail);
            let receipts = events.lock().unwrap();
            assert_eq!(receipts.iter().filter(|event| matches!(event, ClientEvent::ChatWritten { id } if id == "submission-1")).count(), usize::from(!fail));
        }
    });
}

#[test]
fn soju_binds_after_sasl_and_before_cap_end_with_authoritative_status_events() {
    use crate::gui::bouncer::{BouncerBinding, BouncerProvider};
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (read, mut write) = tokio::io::split(client_io);
        let (server_read, mut server_write) = tokio::io::split(server_io);
        let mut reader = BufReader::new(read);
        let mut server_reader = BufReader::new(server_read);
        let server = tokio::spawn(async move {
            server_write.write_all(b":bnc CAP * LS :sasl=PLAIN soju.im/bouncer-networks\r\n").await.unwrap();
            let request = read_wire_line(&mut server_reader).await;
            let caps = request.strip_prefix("CAP REQ :").unwrap();
            assert!(caps.split_whitespace().any(|cap| cap == "soju.im/bouncer-networks"));
            assert!(caps.split_whitespace().any(|cap| cap == "sasl"));
            server_write.write_all(format!(":bnc CAP * ACK :{caps}\r\n").as_bytes()).await.unwrap();
            assert_eq!(read_wire_line(&mut server_reader).await, "AUTHENTICATE PLAIN");
            server_write.write_all(b"AUTHENTICATE +\r\n").await.unwrap();
            let payload = read_wire_line(&mut server_reader).await;
            let payload = BASE64_STANDARD.decode(payload.strip_prefix("AUTHENTICATE ").unwrap()).unwrap();
            assert_eq!(payload, b"\0alice@desktop\0example-password");
            server_write.write_all(b":bnc 900 tester tester!u@h alice :Logged in\r\n:bnc 903 tester :SASL success\r\n").await.unwrap();
            assert_eq!(read_wire_line(&mut server_reader).await, "BOUNCER BIND 42");
            assert_eq!(read_wire_line(&mut server_reader).await, "CAP END");
        });
        let mut client = IrcClient::new(ServerConfig {
            nick: "tester".into(), sasl_username: Some("alice".into()), sasl_password: Some("example-password".into()),
            bouncer: Some(BouncerBinding { provider: BouncerProvider::Soju, account: "alice".into(), network: "42".into(), client_id: "desktop".into() }),
            ..Default::default()
        });
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        client.set_event_callback(std::sync::Arc::new(move |event| captured.lock().unwrap().push(event)));
        let (incoming, _receiver) = mpsc::channel(100);
        let (_nick, mut nick_rx) = mpsc::unbounded_channel();
        timeout(Duration::from_secs(2), client.do_cap_negotiation(&mut write, &mut reader, &incoming, &mut nick_rx, &mut Vec::new())).await.unwrap().unwrap();
        server.await.unwrap();
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(event, ClientEvent::BouncerReady)));
        assert!(events.iter().any(|event| matches!(event, ClientEvent::Authentication(AuthenticationStatus::LoggedIn { account }) if account == "alice")));
        assert!(!events.iter().any(|event| matches!(event, ClientEvent::Tls(TlsStatus::Verified))), "no synthetic TLS success before a handshake");
    });
}

#[test]
fn soju_rejects_missing_binding_capability_and_a_different_principal() {
    use crate::gui::bouncer::{BouncerBinding, BouncerProvider};
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for other_account in [false, true] {
            let (client_io, server_io) = tokio::io::duplex(8192);
            let (read, mut write) = tokio::io::split(client_io);
            let (server_read, mut server_write) = tokio::io::split(server_io);
            let mut reader = BufReader::new(read);
            let mut server_reader = BufReader::new(server_read);
            let server = tokio::spawn(async move {
                if !other_account {
                    server_write.write_all(b":bnc CAP * LS :sasl=PLAIN\r\n").await.unwrap();
                    return;
                }
                server_write.write_all(b":bnc CAP * LS :sasl=PLAIN soju.im/bouncer-networks\r\n").await.unwrap();
                let request = read_wire_line(&mut server_reader).await;
                let caps = request.strip_prefix("CAP REQ :").unwrap();
                server_write.write_all(format!(":bnc CAP * ACK :{caps}\r\n").as_bytes()).await.unwrap();
                assert_eq!(read_wire_line(&mut server_reader).await, "AUTHENTICATE PLAIN");
                server_write.write_all(b"AUTHENTICATE +\r\n").await.unwrap();
                read_wire_line(&mut server_reader).await;
                server_write.write_all(b":bnc 900 tester tester!u@h bob :Logged in\r\n:bnc 903 tester :Success\r\n").await.unwrap();
            });
            let client = IrcClient::new(ServerConfig {
                nick: "tester".into(), sasl_username: Some("alice".into()), sasl_password: Some("example-password".into()),
                bouncer: Some(BouncerBinding { provider: BouncerProvider::Soju, account: "alice".into(), network: "42".into(), client_id: "desktop".into() }),
                ..Default::default()
            });
            let (incoming, _receiver) = mpsc::channel(100);
            let (_nick, mut nick_rx) = mpsc::unbounded_channel();
            let result = timeout(Duration::from_secs(2), client.do_cap_negotiation(&mut write, &mut reader, &incoming, &mut nick_rx, &mut Vec::new())).await.unwrap();
            assert!(result.is_err());
            server.await.unwrap();
        }
    });
}

async fn read_wire_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> String {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    line.trim_end_matches(['\r', '\n']).to_string()
}

#[test]
fn auto_client_certificate_can_authenticate_as_the_password_account() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for account in ["alice", "bob"] {
                let (client_io, server_io) = tokio::io::duplex(8192);
                let (client_read, mut client_write) = tokio::io::split(client_io);
                let (server_read, mut server_write) = tokio::io::split(server_io);
                let mut reader = BufReader::new(client_read);
                let mut server_reader = BufReader::new(server_read);
                let server = tokio::spawn(async move {
                    // A configured certificate does not imply EXTERNAL is
                    // available: Auto must use this server's password route.
                    server_write
                        .write_all(b":srv CAP * LS :sasl=PLAIN\r\n")
                        .await
                        .unwrap();
                    assert_eq!(read_wire_line(&mut server_reader).await, "CAP REQ :sasl");
                    server_write
                        .write_all(b":srv CAP * ACK :sasl\r\n")
                        .await
                        .unwrap();
                    assert_eq!(
                        read_wire_line(&mut server_reader).await,
                        "AUTHENTICATE PLAIN"
                    );
                    server_write.write_all(b"AUTHENTICATE +\r\n").await.unwrap();
                    let wire = read_wire_line(&mut server_reader).await;
                    let encoded = wire.strip_prefix("AUTHENTICATE ").unwrap();
                    assert_eq!(
                        BASE64_STANDARD.decode(encoded).unwrap(),
                        format!("\0{account}\0dummy-password").as_bytes()
                    );
                    server_write
                        .write_all(b":srv 903 tester :SASL success\r\n")
                        .await
                        .unwrap();
                    assert_eq!(read_wire_line(&mut server_reader).await, "CAP END");
                });
                let client = IrcClient::new(ServerConfig {
                    host: "never-connected.example".into(),
                    use_tls: true,
                    nick: "tester".into(),
                    sasl_username: Some(account.into()),
                    sasl_password: Some("dummy-password".into()),
                    sasl_mechanism: SaslMechanism::Auto,
                    sasl_required: true,
                    tls_client_cert_path: Some("same-client.pem".into()),
                    ..Default::default()
                });
                let (incoming_tx, _incoming_rx) = mpsc::channel(100);
                let (_nick_tx, mut nick_rx) = mpsc::unbounded_channel();
                let mut buffer = Vec::new();
                timeout(
                    Duration::from_secs(2),
                    client.do_cap_negotiation(
                        &mut client_write,
                        &mut reader,
                        &incoming_tx,
                        &mut nick_rx,
                        &mut buffer,
                    ),
                )
                .await
                .expect("in-memory CAP negotiation timed out")
                .unwrap();
                server.await.unwrap();
            }
        });
}
