//! Bouncer authentication and routing configuration, independent of the GUI.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BouncerProvider {
    #[default]
    Soju,
    Znc,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct BouncerBinding {
    pub provider: BouncerProvider,
    pub account: String,
    /// Opaque soju netid, or the configured ZNC network name. Empty soju
    /// network selects its discovery/control connection.
    pub network: String,
    pub client_id: String,
}

impl Default for BouncerBinding {
    fn default() -> Self {
        let mut random = [0u8; 6];
        let _ = getrandom::fill(&mut random);
        Self {
            provider: BouncerProvider::Soju,
            account: String::new(),
            network: String::new(),
            client_id: format!(
                "linefeed-{}",
                random
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        }
    }
}

impl BouncerBinding {
    pub(crate) fn validate(&self) -> Result<(), String> {
        for (label, value) in [
            ("Bouncer account", &self.account),
            ("Bouncer client identifier", &self.client_id),
        ] {
            if value.is_empty()
                || value.len() > 128
                || value.chars().any(|ch| {
                    ch.is_whitespace() || ch.is_control() || matches!(ch, '/' | '@' | ':' | ',')
                })
            {
                return Err(format!(
                    "{label} must be one nonempty token without '/', '@', ':' or ','"
                ));
            }
        }
        if self.network.len() > 256
            || self.network.chars().any(|ch| {
                ch.is_whitespace() || ch.is_control() || matches!(ch, '/' | '@' | ',' | ':')
            })
        {
            return Err("Bouncer network must be one routing token".into());
        }
        if self.provider == BouncerProvider::Znc && self.network.is_empty() {
            return Err("Choose a ZNC network name".into());
        }
        Ok(())
    }

    pub(crate) fn wire_username(&self) -> String {
        match self.provider {
            BouncerProvider::Soju => format!("{}@{}", self.account, self.client_id),
            BouncerProvider::Znc => format!("{}@{}/{}", self.account, self.client_id, self.network),
        }
    }

    pub(crate) fn is_control(&self) -> bool {
        self.provider == BouncerProvider::Soju && self.network.is_empty()
    }
}
