//! Provider-specific routing; every upstream remains an independent Session.
use std::collections::BTreeMap;

use super::identity::{BouncerNetworkId, LogicalNetworkKey, TransportEndpointKey};
use super::{ChatMessage, Session, SessionEvent};

pub(crate) use crate::irc::bouncer::{BouncerBinding, BouncerProvider};

impl BouncerBinding {
    pub(crate) fn key(&self, endpoint: TransportEndpointKey) -> LogicalNetworkKey {
        LogicalNetworkKey::Bouncer {
            endpoint,
            principal: self.account.trim().to_lowercase(),
            upstream: match self.provider {
                BouncerProvider::Soju => BouncerNetworkId::Soju(self.network.clone()),
                BouncerProvider::Znc => BouncerNetworkId::Znc(self.network.to_lowercase()),
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct BouncerNetwork {
    pub attributes: BTreeMap<String, String>,
}

fn parse_attributes(input: &str) -> BTreeMap<String, Option<String>> {
    input
        .trim_start_matches(':')
        .split(';')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part
                .split_once('=')
                .map_or((part, None), |(key, value)| (key, Some(value)));
            let value = value.map(|value| {
                let mut out = String::new();
                let mut escaped = false;
                for ch in value.chars() {
                    if escaped {
                        out.push(match ch {
                            's' => ' ',
                            ':' => ';',
                            'r' => '\r',
                            'n' => '\n',
                            other => other,
                        });
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else {
                        out.push(ch);
                    }
                }
                out
            });
            (key.to_owned(), value)
        })
        .collect()
}

/// Configuration for another upstream reached through the same bouncer account.
/// It keeps only what identifies the bouncer account and endpoint; per-network
/// automation (auto-perform, possibly a services password for a different
/// network), auto-join, invisibility, upload credentials and the profile
/// binding belong to the session they were written for.
fn config_for_discovered_network(
    active: &super::SessionConfig,
    network_id: &str,
) -> Option<super::SessionConfig> {
    let mut config = active.clone();
    config.server.bouncer.as_mut()?.network = network_id.to_owned();
    config.auto_perform.clear();
    config.auto_join_channels.clear();
    config.set_invisible = false;
    config.upload_settings = Default::default();
    config.profile_id = None;
    Some(config)
}

impl Session {
    pub(super) fn load_connection_form(&mut self, config: &super::SessionConfig) {
        let server = &config.server;
        self.server_host = server.host.clone();
        self.server_port = server.port.to_string();
        self.use_tls = server.use_tls;
        self.accept_invalid_certs = server.accept_invalid_certs;
        self.nickname = server.nick.clone();
        self.username = server.username.clone();
        self.realname = server.realname.clone();
        self.password = server.password.clone().unwrap_or_default();
        self.sasl_username = server.sasl_username.clone().unwrap_or_default();
        self.sasl_password = server.sasl_password.clone().unwrap_or_default();
        self.sasl_mechanism = server.sasl_mechanism;
        self.sasl_required = server.sasl_required;
        self.tls_client_cert_path = server.tls_client_cert_path.clone().unwrap_or_default();
        self.tls_client_key_path = server.tls_client_key_path.clone().unwrap_or_default();
        self.pre_away_message = server.pre_away_message.clone().unwrap_or_default();
        self.persistence_profile = server.persistence_profile.clone().unwrap_or_default();
        self.auto_join_channels = config.auto_join_channels.clone();
        self.auto_perform = config.auto_perform.clone();
        self.set_invisible = config.set_invisible;
        self.form_profile = config.profile_id.clone();
        self.bouncer = server.bouncer.clone();
        self.upload_settings = config.upload_settings.clone();
    }
    pub(crate) fn membership_managed_by_bouncer(&self) -> bool {
        self.active_session
            .as_ref()
            .is_some_and(|session| session.server.bouncer.is_some())
    }

    pub(crate) fn observe_bouncer_message(&mut self, name: &str, params: &[String]) {
        if !name.eq_ignore_ascii_case("BOUNCER")
            || self
                .active_session
                .as_ref()
                .is_none_or(|session| session.server.bouncer.is_none())
        {
            return;
        }
        if params
            .first()
            .is_some_and(|sub| sub.eq_ignore_ascii_case("NETWORK"))
            && let (Some(id), Some(attributes)) = (params.get(1), params.get(2))
        {
            if attributes == "*" {
                self.bouncer_networks.remove(id);
                if self
                    .active_session
                    .as_ref()
                    .and_then(|session| session.server.bouncer.as_ref())
                    .is_some_and(|binding| {
                        binding.provider == BouncerProvider::Soju && binding.network == *id
                    })
                {
                    self.request_manual_disconnect(None);
                    self.add_server_message(ChatMessage::system("The bouncer removed this upstream network; its conversations remain available offline"));
                }
            } else if self.bouncer_networks.len() < 1000 || self.bouncer_networks.contains_key(id) {
                let network = self.bouncer_networks.entry(id.clone()).or_default();
                for (key, value) in parse_attributes(attributes) {
                    if let Some(value) = value {
                        network.attributes.insert(key, value);
                    } else {
                        network.attributes.remove(&key);
                    }
                }
            }
        }
    }

    pub(crate) fn validate_bouncer_isupport(&mut self, params: &[String]) {
        let expected = self
            .active_session
            .as_ref()
            .and_then(|session| session.server.bouncer.as_ref())
            .filter(|binding| {
                binding.provider == BouncerProvider::Soju && !binding.network.is_empty()
            })
            .map(|binding| binding.network.clone());
        if let Some(expected) = expected
            && params
                .iter()
                .filter_map(|token| token.split_once('='))
                .any(|(name, value)| {
                    name.eq_ignore_ascii_case("BOUNCER_NETID") && value != expected
                })
        {
            self.request_manual_disconnect(None);
            self.add_server_message(ChatMessage::system(
                "The bouncer bound a different upstream network; connection stopped",
            ));
        }
    }

    /// Returns a connect request when discovery was requested explicitly.
    pub(crate) fn draw_bouncer_editor(&mut self, ui: &mut egui::Ui) -> bool {
        let mut enabled = self.bouncer.is_some();
        if ui
            .checkbox(&mut enabled, "Connect through a bouncer")
            .changed()
        {
            self.bouncer = enabled.then(BouncerBinding::default);
        }
        let Some(binding) = &mut self.bouncer else {
            return false;
        };
        ui.horizontal(|ui| {
            ui.label("Bouncer:");
            egui::ComboBox::from_id_salt(("bouncer_provider", self.id))
                .selected_text(match binding.provider {
                    BouncerProvider::Soju => "soju",
                    BouncerProvider::Znc => "ZNC",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut binding.provider, BouncerProvider::Soju, "soju");
                    ui.selectable_value(&mut binding.provider, BouncerProvider::Znc, "ZNC");
                });
        });
        ui.horizontal(|ui| {
            ui.label("Bouncer account:");
            ui.text_edit_singleline(&mut binding.account);
        });
        ui.horizontal(|ui| {
            ui.label(match binding.provider {
                BouncerProvider::Soju => "Network ID:",
                BouncerProvider::Znc => "Network name:",
            });
            ui.text_edit_singleline(&mut binding.network);
        });
        ui.horizontal(|ui| {
            ui.label("Client identifier:");
            ui.text_edit_singleline(&mut binding.client_id);
        });
        ui.label("Use SASL credentials for the bouncer account. Upstream authentication is managed by the bouncer.");
        let discover = binding.provider == BouncerProvider::Soju
            && ui.button("Discover soju networks").clicked();
        if discover {
            binding.network.clear();
        }
        let discovered: Vec<_> = self
            .bouncer_networks
            .iter()
            .map(|(id, network)| (id.clone(), network.attributes.clone()))
            .collect();
        for (id, attributes) in discovered {
            let name = attributes
                .get("name")
                .cloned()
                .unwrap_or_else(|| id.clone());
            let state = attributes
                .get("state")
                .map(String::as_str)
                .unwrap_or("unknown");
            ui.horizontal(|ui| {
                ui.label(format!("{name} — {state}"));
                if ui.button("Open network").clicked()
                    && let Some(config) = self
                        .active_session
                        .as_ref()
                        .and_then(|active| config_for_discovered_network(active, &id))
                {
                    self.events
                        .push(SessionEvent::NewBouncerNetwork(Box::new(config)));
                }
            });
            if let Some(error) = attributes.get("error") {
                ui.label(error);
            }
        }
        discover
    }

    pub(crate) fn begin_bouncer_playback(&mut self) {
        let Some(binding) = self
            .active_session
            .as_ref()
            .and_then(|session| session.server.bouncer.as_ref())
        else {
            return;
        };
        if binding.provider == BouncerProvider::Soju {
            if self.enabled_caps.contains("soju.im/bouncer-networks") {
                self.send_command(crate::irc::IrcCommand::Raw("BOUNCER LISTNETWORKS".into()));
            }
        } else if self.logging_load_history
            && self.enabled_caps.contains("znc.in/playback")
            && !self.chathistory_enabled
        {
            self.send_command(crate::irc::IrcCommand::Privmsg(
                "*playback".into(),
                "PLAY *".into(),
            ));
            self.add_server_message(ChatMessage::system("Loading ZNC playback"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_routes_do_not_change_shared_principal_or_network_identity() {
        let binding = BouncerBinding {
            provider: BouncerProvider::Znc,
            account: "Alice".into(),
            network: "Libera".into(),
            client_id: "desktop".into(),
        };
        assert_eq!(binding.wire_username(), "Alice@desktop/Libera");
        let mut other = binding.clone();
        other.client_id = "laptop".into();
        let endpoint = TransportEndpointKey::new("bnc.example", 6697, true);
        assert_eq!(binding.key(endpoint.clone()), other.key(endpoint));
        assert_eq!(
            parse_attributes("name=My\\sNetwork;removed;state=connected")["name"],
            Some("My Network".into())
        );
    }

    fn session(provider: BouncerProvider, network: &str) -> Session {
        let mut session = Session::with_settings(super::super::Settings {
            nickname: "me".into(),
            server_host: "bnc.example".into(),
            sasl_username: "alice".into(),
            sasl_password: "example-password".into(),
            bouncer: Some(BouncerBinding {
                provider,
                account: "alice".into(),
                network: network.into(),
                client_id: "desktop".into(),
            }),
            logging_enabled: false,
            logging_load_history: false,
            ..Default::default()
        });
        assert!(session.start_session_from_form());
        session
    }

    #[test]
    fn opening_a_discovered_network_does_not_inherit_per_network_automation() {
        let source = session(BouncerProvider::Soju, "1");
        let mut active = source.active_session.clone().unwrap();
        active.auto_perform = "/ns identify secret-for-network-one".into();
        active.auto_join_channels = "#one".into();
        active.set_invisible = true;
        active.upload_settings.use_custom = true;
        active.upload_settings.secret = "upload-secret".into();
        active.profile_id = Some("profile-one".into());

        let opened = config_for_discovered_network(&active, "2").unwrap();
        let binding = opened.server.bouncer.as_ref().unwrap();
        assert_eq!(binding.network, "2");
        assert_eq!(binding.account, "alice");
        assert!(opened.auto_perform.is_empty());
        assert!(opened.auto_join_channels.is_empty());
        assert!(!opened.set_invisible);
        assert!(!opened.upload_settings.use_custom);
        assert!(opened.upload_settings.secret.is_empty());
        assert!(opened.profile_id.is_none());
        // The bouncer account itself is shared by every upstream.
        assert_eq!(opened.server.sasl_username.as_deref(), Some("alice"));
        assert_eq!(
            opened.server.sasl_password.as_deref(),
            Some("example-password")
        );
        assert_ne!(opened.logical_network_key(), active.logical_network_key());
        // The source session keeps its own settings.
        assert_eq!(active.auto_perform, "/ns identify secret-for-network-one");
    }

    #[test]
    fn discovery_deltas_and_upstream_deletion_keep_networks_separate() {
        let mut session = session(BouncerProvider::Soju, "");
        session.observe_bouncer_message(
            "BOUNCER",
            &[
                "NETWORK".into(),
                "42".into(),
                "name=My\\sNetwork;state=connected".into(),
            ],
        );
        session.observe_bouncer_message(
            "BOUNCER",
            &[
                "NETWORK".into(),
                "42".into(),
                "state=disconnected;error=Temporarily\\sunavailable".into(),
            ],
        );
        assert_eq!(
            session.bouncer_networks["42"].attributes["name"],
            "My Network"
        );
        assert_eq!(
            session.bouncer_networks["42"].attributes["state"],
            "disconnected"
        );
        session
            .observe_bouncer_message("BOUNCER", &["NETWORK".into(), "42".into(), "error".into()]);
        assert!(
            !session.bouncer_networks["42"]
                .attributes
                .contains_key("error")
        );
        session.observe_bouncer_message("BOUNCER", &["NETWORK".into(), "42".into(), "*".into()]);
        assert!(session.bouncer_networks.is_empty());
    }

    #[test]
    fn bouncer_state_does_not_blindly_rejoin_saved_memberships() {
        let mut session = session(BouncerProvider::Znc, "libera");
        let (sender, mut commands) = tokio::sync::mpsc::unbounded_channel();
        session.test_set_registered(sender.clone());
        let id = session.ensure_target("#room");
        session.conversation_channel_mut(id).unwrap().joined = true;
        session.mark_connection_lost();
        assert!(session.pending_rejoins.is_none());
        session.test_set_connecting(sender, None);
        session.handle_numeric(
            crate::irc::numerics::RPL_WELCOME,
            &["me".into(), "Welcome".into()],
        );
        assert!(
            !std::iter::from_fn(|| commands.try_recv().ok())
                .any(|command| matches!(command, crate::irc::IrcCommand::Join(..)))
        );
        assert!(!session.conversation_channel(id).unwrap().joined);
    }

    #[test]
    fn znc_playback_is_historical_deduplicated_and_cannot_reopen_closed_queries() {
        let mut session = session(BouncerProvider::Znc, "libera");
        let (sender, _commands) = tokio::sync::mpsc::unbounded_channel();
        session.test_set_registered(sender);
        let feed = |session: &mut Session, line: &str| {
            session.handle_incoming_message(crate::irc::IrcMessage::parse(line).unwrap())
        };
        feed(&mut session, ":bnc BATCH +one znc.in/playback alice");
        feed(
            &mut session,
            "@batch=one;time=2026-11-15T01:00:00.000Z;msgid=old :alice!u@h PRIVMSG me :old message",
        );
        feed(&mut session, ":bnc BATCH -one");
        let id = session.id_for_target("alice").unwrap();
        feed(&mut session, ":bnc BATCH +two znc.in/playback alice");
        feed(
            &mut session,
            "@batch=two;time=2026-11-15T01:00:00.000Z;msgid=old :alice!u@h PRIVMSG me :old message",
        );
        feed(
            &mut session,
            "@batch=two;time=2026-11-15T02:00:00.000Z;msgid=new :alice!u@h PRIVMSG me :new message",
        );
        feed(&mut session, "@batch=two :peer!u@h JOIN #other");
        feed(&mut session, ":bnc BATCH -two");
        let messages = &session.conversation_channel(id).unwrap().messages;
        assert_eq!(
            messages
                .iter()
                .filter(|row| row.content == "old message")
                .count(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .filter(|row| row.content == "new message")
                .count(),
            1
        );
        assert!(session.id_for_target("#other").is_none());
        session.remove_channel("alice");
        feed(&mut session, ":bnc BATCH +late znc.in/playback alice");
        feed(
            &mut session,
            "@batch=late :alice!u@h PRIVMSG me :late playback",
        );
        feed(&mut session, ":bnc BATCH -late");
        assert!(session.id_for_target("alice").is_none());
    }
}
