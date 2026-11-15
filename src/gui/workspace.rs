//! Private, versioned workspace checkpoints. Message bodies live in the archive.
use std::io;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::identity::{LogicalNetworkKey, MessageRef, ScrollAnchor};
use super::{CaseMapping, UpdateSession};

const VERSION: u32 = 1;
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct WorkspaceViewState {
    pub draft: String,
    pub reply_to: Option<MessageRef>,
    pub last_read: Option<MessageRef>,
    pub viewport: Option<ScrollAnchor>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WorkspaceView {
    pub target: Option<String>,
    pub state: WorkspaceViewState,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WorkspaceSession {
    pub expected_network: LogicalNetworkKey,
    pub state: UpdateSession,
    pub views: Vec<WorkspaceView>,
    pub case_mapping: CaseMapping,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WorkspaceSnapshot {
    pub version: u32,
    pub sessions: Vec<WorkspaceSession>,
    pub active_session: usize,
    #[serde(default)]
    pub activity: Vec<super::attention::ActivityRecord>,
}

impl WorkspaceSnapshot {
    pub(crate) fn new(sessions: Vec<WorkspaceSession>, active_session: usize) -> Self {
        Self {
            version: VERSION,
            sessions,
            active_session,
            activity: Vec::new(),
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != VERSION || self.sessions.len() > 128 {
            return Err(io::Error::other(
                "Unsupported workspace version or session count",
            ));
        }
        for entry in &self.sessions {
            let config =
                entry.state.session.as_ref().ok_or_else(|| {
                    io::Error::other("Workspace session has no committed connection")
                })?;
            if config.logical_network_key() != entry.expected_network
                || config.server.host.trim().is_empty()
                || config.server.port == 0
                || entry.views.len() > 4000
                || entry.state.tabs.len() > 4000
            {
                return Err(io::Error::other("Workspace network identity is invalid"));
            }
            crate::irc::client::IrcClient::validate_registration_fields(
                &config.server.nick,
                &config.server.username,
                &config.server.realname,
                config.server.password.as_deref().unwrap_or_default(),
            )
            .map_err(io::Error::other)?;
            if let Some(binding) = &config.server.bouncer {
                binding.validate().map_err(io::Error::other)?;
            }
        }
        Ok(())
    }

    fn bytes(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        struct Limited(Vec<u8>);
        impl Write for Limited {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes.len() > MAX_BYTES - self.0.len() {
                    return Err(io::Error::other(
                        "Workspace exceeds the 16 MiB limit; save large drafts elsewhere",
                    ));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = Limited(Vec::new());
        serde_json::to_writer(&mut output, self).map_err(io::Error::other)?;
        Ok(output.0)
    }
}

enum WriteRequest {
    Save(WorkspaceSnapshot, Option<mpsc::Sender<Result<(), String>>>),
}

#[derive(Default)]
pub(crate) struct WorkspaceStore {
    pub(crate) restored: Option<WorkspaceSnapshot>,
    sender: Option<mpsc::SyncSender<WriteRequest>>,
    errors: Arc<Mutex<Option<String>>>,
    last_checkpoint: Option<Instant>,
    stopped: bool,
}

impl WorkspaceStore {
    pub(crate) fn is_enabled(&self) -> bool {
        self.sender.is_some() && !self.stopped
    }
    pub(crate) fn open_default() -> Self {
        dirs::config_dir()
            .map(|directory| Self::open(directory.join("linefeed/workspace.json")))
            .unwrap_or_default()
    }

    pub(crate) fn open(path: PathBuf) -> Self {
        let mut store = Self::default();
        match Self::load_from(&path) {
            Ok(snapshot) => store.restored = snapshot,
            Err(error) => {
                *store.errors.lock().unwrap() = Some(format!(
                    "Saved workspace could not be read and was preserved: {error}"
                ));
                return store;
            }
        }
        let (sender, receiver) = mpsc::sync_channel::<WriteRequest>(1);
        let errors = store.errors.clone();
        std::thread::spawn(move || {
            let mut previous: Option<Vec<u8>> = None;
            while let Ok(WriteRequest::Save(snapshot, acknowledge)) = receiver.recv() {
                let result = snapshot
                    .bytes()
                    .and_then(|bytes| {
                        if previous.as_ref() == Some(&bytes) {
                            return Ok(());
                        }
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        crate::persistence::write_private_atomic(&path, &bytes)?;
                        previous = Some(bytes);
                        Ok(())
                    })
                    .map_err(|error| error.to_string());
                if let Err(error) = &result {
                    *errors.lock().unwrap() = Some(format!("Workspace was not saved: {error}"));
                }
                if let Some(acknowledge) = acknowledge {
                    let _ = acknowledge.send(result);
                }
            }
        });
        store.sender = Some(sender);
        store
    }

    fn load_from(path: &Path) -> io::Result<Option<WorkspaceSnapshot>> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > MAX_BYTES {
            return Err(io::Error::other("Saved workspace is too large"));
        }
        let snapshot: WorkspaceSnapshot =
            serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        snapshot.validate()?;
        Ok(Some(snapshot))
    }

    pub(crate) fn checkpoint_due(&mut self) -> bool {
        if self.sender.is_none()
            || self.stopped
            || self
                .last_checkpoint
                .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return false;
        }
        self.last_checkpoint = Some(Instant::now());
        true
    }

    pub(crate) fn save(&self, snapshot: WorkspaceSnapshot) {
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(WriteRequest::Save(snapshot, None));
        }
    }

    pub(crate) fn finish(&mut self, snapshot: WorkspaceSnapshot) -> bool {
        self.stopped = true;
        let Some(sender) = &self.sender else {
            return true;
        };
        let (reply, receiver) = mpsc::channel();
        let mut request = WriteRequest::Save(snapshot, Some(reply));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match sender.try_send(request) {
                Ok(()) => {
                    return receiver
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .is_ok_and(|result| result.is_ok());
                }
                Err(mpsc::TrySendError::Full(returned)) if Instant::now() < deadline => {
                    request = returned;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return false,
            }
        }
    }

    pub(crate) fn take_error(&self) -> Option<String> {
        self.errors.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        std::env::temp_dir().join(format!(
            "linefeed-workspace-{:016x}.json",
            u64::from_ne_bytes(random)
        ))
    }

    #[test]
    fn atomic_workspace_round_trip_and_future_schema_preservation() {
        let path = path();
        let mut store = WorkspaceStore::open(path.clone());
        assert!(store.finish(WorkspaceSnapshot::new(Vec::new(), 0)));
        assert!(WorkspaceStore::load_from(&path).unwrap().is_some());
        let future = br#"{"version":999,"sessions":[],"active_session":0}"#;
        std::fs::write(&path, future).unwrap();
        let mut readonly = WorkspaceStore::open(path.clone());
        assert!(readonly.restored.is_none());
        assert!(readonly.take_error().is_some());
        assert!(readonly.finish(WorkspaceSnapshot::new(Vec::new(), 0)));
        assert_eq!(std::fs::read(&path).unwrap(), future);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn network_mismatch_cannot_cross_bind_saved_state() {
        let mut session = super::super::Session::with_settings(super::super::Settings {
            server_host: "saved.example".into(),
            nickname: "me".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Default::default()
        });
        assert!(session.start_session_from_form());
        let expected_network = LogicalNetworkKey::Direct {
            endpoint: super::super::identity::TransportEndpointKey::new(
                "other.example",
                6697,
                true,
            ),
        };
        let entry = WorkspaceSession {
            expected_network,
            state: session.update_session(),
            views: Vec::new(),
            case_mapping: CaseMapping::default(),
        };
        assert!(WorkspaceSnapshot::new(vec![entry], 0).bytes().is_err());
    }
}
