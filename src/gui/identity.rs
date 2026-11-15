//! Persistent identities shared by history, navigation and workspace state.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct TransportEndpointKey {
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
}

impl TransportEndpointKey {
    pub(crate) fn new(host: &str, port: u16, use_tls: bool) -> Self {
        Self {
            host: host
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_lowercase(),
            port,
            use_tls,
        }
    }

    pub(crate) fn label(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "provider", content = "network", rename_all = "snake_case")]
pub(crate) enum BouncerNetworkId {
    Soju(String),
    Znc(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LogicalNetworkKey {
    /// Imported logs with no provable live network mapping. Never connectable.
    Legacy {
        source: String,
    },
    Direct {
        endpoint: TransportEndpointKey,
    },
    Bouncer {
        endpoint: TransportEndpointKey,
        principal: String,
        upstream: BouncerNetworkId,
    },
}

impl LogicalNetworkKey {
    pub(crate) fn storage_key(&self) -> String {
        match self {
            Self::Legacy { source } => format!("legacy:{source}"),
            Self::Direct { endpoint } => format!(
                "{}://{}",
                if endpoint.use_tls { "tls" } else { "plain" },
                endpoint.label()
            ),
            Self::Bouncer { .. } => format!(
                "bouncer:{}",
                serde_json::to_string(self)
                    .expect("network identities contain only strings and integers")
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ConversationKey {
    pub network: LogicalNetworkKey,
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct MessageRef {
    pub conversation: ConversationKey,
    pub archive_id: Option<String>,
    pub server_msgid: Option<String>,
    pub server_time: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ScrollAnchor {
    pub message: Option<MessageRef>,
    /// Position within the first visible row, stable across font/width changes.
    pub fraction: f32,
    pub at_bottom: bool,
}

impl Default for ScrollAnchor {
    fn default() -> Self {
        Self {
            message: None,
            fraction: 0.0,
            at_bottom: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bouncer_accounts_and_upstreams_never_share_storage() {
        let endpoint = TransportEndpointKey::new("BNC.Example", 6697, true);
        let key = |principal: &str, upstream: &str| LogicalNetworkKey::Bouncer {
            endpoint: endpoint.clone(),
            principal: principal.into(),
            upstream: BouncerNetworkId::Soju(upstream.into()),
        };
        assert_ne!(
            key("alice", "1").storage_key(),
            key("bob", "1").storage_key()
        );
        assert_ne!(
            key("alice", "1").storage_key(),
            key("alice", "2").storage_key()
        );
        assert_eq!(
            LogicalNetworkKey::Direct { endpoint }.storage_key(),
            "tls://bnc.example:6697"
        );
    }
}
