//! User-initiated FILEHOST uploads; completion belongs to its original draft.
use super::conversations::ConversationId;
use super::identity::LogicalNetworkKey;
use super::windows::WindowKey;
use super::{App, Session, SessionEvent};
use crate::irc::{IrcCommand, IrcMessage};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CustomAuth {
    #[default]
    None,
    Basic,
    Bearer,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct UploadSettings {
    pub use_custom: bool,
    pub url: String,
    pub auth: CustomAuth,
    pub username: String,
    pub secret: String,
    pub max_bytes: u64,
}
impl Default for UploadSettings {
    fn default() -> Self {
        Self {
            use_custom: false,
            url: String::new(),
            auth: CustomAuth::None,
            username: String::new(),
            secret: String::new(),
            max_bytes: 100 * 1024 * 1024,
        }
    }
}
impl UploadSettings {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.use_custom {
            https_url(self.url.trim())?;
            if self.auth == CustomAuth::Bearer && self.secret.trim().is_empty() {
                return Err("Enter the upload provider's bearer token".into());
            }
            if self.auth == CustomAuth::Basic && self.username.is_empty() {
                return Err("Enter the upload provider's username".into());
            }
            if self.secret.chars().any(char::is_control)
                || self.username.chars().any(char::is_control)
            {
                return Err("Upload credentials must not contain control characters".into());
            }
        }
        if self.max_bytes == 0 || self.max_bytes > 1024 * 1024 * 1024 {
            return Err("Upload size limit must be between 1 byte and 1 GiB".into());
        }
        Ok(())
    }
}
impl std::fmt::Debug for UploadSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadSettings")
            .field("use_custom", &self.use_custom)
            .field("auth", &self.auth)
            .field("credentials", &"<private>")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(crate) enum UploadSource {
    Pick,
    Clipboard,
    Files(Vec<PathBuf>),
    Image(Arc<egui::ColorImage>),
}
#[derive(Default)]
pub(crate) struct UploadDiscovery {
    filehost: Option<String>,
    pending: Option<(String, String, Option<String>, Instant)>,
    paste_seen: bool,
    credentials_stale: bool,
    confirmed_account: Option<String>,
    requests: VecDeque<(String, Option<String>, Option<String>)>,
    unlabeled_token_timed_out: bool,
}
#[derive(Clone)]
enum UploadAuth {
    None,
    Basic(String, String),
    Bearer(String),
    ServiceBearer(String),
    Certificate(PathBuf, Option<PathBuf>),
    TokenService,
}
#[derive(Clone)]
struct Provider {
    url: String,
    auth: UploadAuth,
    max_bytes: u64,
}
#[derive(Clone)]
struct Origin {
    window: WindowKey,
    network: LogicalNetworkKey,
    nonce: String,
    generation: Option<u64>,
}
enum Input {
    File(PathBuf),
    Image(Arc<egui::ColorImage>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Queued,
    Token,
    Uploading,
    Done(String),
    Failed(String),
    Cancelled,
}
struct Job {
    id: String,
    name: String,
    origin: Origin,
    provider: Provider,
    input: Option<Input>,
    state: State,
    cancelled: Arc<AtomicBool>,
    bytes: Arc<AtomicU64>,
    agent: Option<ureq::Agent>,
    created: Instant,
}
impl Job {
    fn finish(&mut self, state: State) {
        debug_assert!(matches!(
            state,
            State::Done(_) | State::Failed(_) | State::Cancelled
        ));
        self.state = state;
        self.input = None;
        if let Some(agent) = self.agent.take() {
            agent.shutdown();
        }
    }
}
struct PickedFiles {
    origin: Origin,
    provider: Provider,
    result: Result<Vec<PathBuf>, String>,
}
enum Event {
    Picked(Box<PickedFiles>),
    Completed(String, Result<String, String>),
}
pub(crate) struct UploadManager {
    pub open: bool,
    jobs: Vec<Job>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    notice: Option<String>,
    pending_pickers: usize,
}
impl Default for UploadManager {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            open: false,
            jobs: Vec::new(),
            tx,
            rx,
            notice: None,
            pending_pickers: 0,
        }
    }
}

fn https_url(value: &str) -> Result<url::Url, String> {
    if value.len() > 8192 || value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("The upload host must be an absolute HTTPS URL".into());
    }
    let parsed = url::Url::parse(value).map_err(|_| "Invalid upload URL".to_string())?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err("Use an HTTPS URL without embedded credentials or fragments".into());
    }
    Ok(parsed)
}
fn upload_location(base: &str, location: &str) -> Result<String, String> {
    if location.len() > 8192
        || location
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("File host returned an invalid download URL".into());
    }
    let url = if let Ok(url) = url::Url::parse(location) {
        url
    } else {
        https_url(base)?
            .join(location)
            .map_err(|_| "File host returned an invalid download URL".to_string())?
    };
    https_url(url.as_str()).map(|u| u.into())
}
fn service_token_current(token: &str) -> bool {
    let mut parts = token.split('.');
    if parts.next().is_none() {
        return false;
    }
    let Some(payload) = parts.next() else {
        return true;
    };
    if parts.next().is_none() {
        return true;
    }
    let Ok(bytes) =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))
    else {
        return false;
    };
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    payload
        .get("exp")
        .and_then(serde_json::Value::as_i64)
        .is_none_or(|expiry| expiry > super::archive::epoch_now())
}
fn io_http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(401 | 403) => {
            "File host rejected authentication; check upload credentials".into()
        }
        ureq::Error::StatusCode(413) => "File exceeds the file host's size limit".into(),
        ureq::Error::StatusCode(415) => "This file type is not supported by the file host".into(),
        ureq::Error::StatusCode(code) => format!("File host returned HTTP {code}"),
        _ => "Upload connection failed or timed out".into(),
    }
}

impl Session {
    fn upload_provider(&self) -> Result<Provider, String> {
        let config = self
            .active_session
            .as_ref()
            .map_or(&self.upload_settings, |session| &session.upload_settings);
        if config.use_custom {
            let url = https_url(config.url.trim())?.to_string();
            let auth = match config.auth {
                CustomAuth::None => UploadAuth::None,
                CustomAuth::Basic => {
                    UploadAuth::Basic(config.username.clone(), config.secret.clone())
                }
                CustomAuth::Bearer => UploadAuth::Bearer(config.secret.clone()),
            };
            return Ok(Provider {
                url,
                auth,
                max_bytes: config.max_bytes.clamp(1, 1024 * 1024 * 1024),
            });
        }
        if let Some(url) = &self.upload_discovery.filehost {
            return Ok(Provider {
                url: https_url(url)?.to_string(),
                auth: UploadAuth::TokenService,
                max_bytes: config.max_bytes.clamp(1, 1024 * 1024 * 1024),
            });
        }
        let url=["DRAFT/FILEHOST","SOJU.IM/FILEHOST","FILEHOST"].iter().find_map(|key|self.network_support.isupport.get(*key).and_then(Option::as_ref))
            .ok_or_else(||"This network has no advertised FILEHOST. Configure a custom HTTPS file host in Connection settings.".to_string())?;
        let server = self
            .active_session
            .as_ref()
            .map(|s| &s.server)
            .ok_or_else(|| "Connect before using the network file host".to_string())?;
        if self.upload_discovery.credentials_stale {
            return Err("The IRC account changed; use a generated upload token or configure explicit upload credentials".into());
        }
        let expected_user = server
            .bouncer
            .as_ref()
            .map(|binding| binding.account.as_str())
            .or(server.sasl_username.as_deref());
        let account_matches = match &self.connection_status.authentication {
            crate::irc::client::AuthenticationStatus::LoggedIn { account } => {
                expected_user.is_some_and(|expected| expected.eq_ignore_ascii_case(account))
            }
            crate::irc::client::AuthenticationStatus::Authenticated => true,
            _ => false,
        };
        let auth = match self.connection_status.sasl_mechanism {
            Some(crate::irc::client::SaslMechanism::External) => {
                let cert = server
                    .tls_client_cert_path
                    .as_ref()
                    .ok_or_else(|| "Authenticated client certificate is unavailable".to_string())?;
                UploadAuth::Certificate(
                    cert.into(),
                    server.tls_client_key_path.as_ref().map(PathBuf::from),
                )
            }
            Some(
                crate::irc::client::SaslMechanism::Plain
                | crate::irc::client::SaslMechanism::ScramSha256,
            ) if account_matches => {
                if let (Some(user), Some(password)) =
                    (expected_user, server.sasl_password.as_deref())
                {
                    UploadAuth::Basic(user.into(), password.into())
                } else {
                    UploadAuth::None
                }
            }
            _ => UploadAuth::None,
        };
        Ok(Provider {
            url: https_url(url)?.to_string(),
            auth,
            max_bytes: config.max_bytes.clamp(1, 1024 * 1024 * 1024),
        })
    }
    pub(crate) fn commit_upload_settings(&mut self) {
        let Ok(form) = self.session_from_form() else {
            return;
        };
        if let Some(active) = &mut self.active_session
            && active.logical_network_key() == form.logical_network_key()
            && active.services_identity_key() == form.services_identity_key()
        {
            active.upload_settings = self.upload_settings.clone();
        }
    }
    pub(crate) fn draw_upload_settings(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("File sharing").show(ui,|ui|{
            ui.checkbox(&mut self.upload_settings.use_custom,"Use a custom HTTPS FILEHOST provider");
            if self.upload_settings.use_custom{
                ui.label("Provider URL (standard FILEHOST raw POST / 201 Location)");ui.text_edit_singleline(&mut self.upload_settings.url);
                ui.horizontal_wrapped(|ui|{ui.label("Authentication:");for(value,label)in[(CustomAuth::None,"None"),(CustomAuth::Basic,"Basic"),(CustomAuth::Bearer,"Bearer token")]{ui.selectable_value(&mut self.upload_settings.auth,value,label);}});
                if self.upload_settings.auth==CustomAuth::Basic{ui.horizontal_wrapped(|ui|{ui.label("Upload username:");ui.text_edit_singleline(&mut self.upload_settings.username);});}
                if self.upload_settings.auth!=CustomAuth::None{ui.add(egui::TextEdit::singleline(&mut self.upload_settings.secret).password(true).hint_text("Upload credential (separate from IRC login)"));}
            }else{ui.label("Use the network or bouncer's advertised FILEHOST; generated service tokens are used when available.");}
            ui.horizontal_wrapped(|ui|{ui.label("Maximum file size (MiB):");let mut size=(self.upload_settings.max_bytes/(1024*1024)).max(1);if ui.add(egui::DragValue::new(&mut size).range(1..=1024)).changed(){self.upload_settings.max_bytes=size*1024*1024;}});
        });
    }
    pub(crate) fn draw_upload_controls(&mut self, ui: &mut egui::Ui) {
        if ui
            .add_enabled(
                self.current_conversation.is_some(),
                egui::Button::new("Attach"),
            )
            .on_hover_text("Upload a file and add its URL to this draft")
            .clicked()
        {
            self.request_upload(UploadSource::Pick);
        }
        if ui
            .add_enabled(
                self.current_conversation.is_some(),
                egui::Button::new("Paste file"),
            )
            .clicked()
        {
            self.request_upload(UploadSource::Clipboard);
        }
    }
    fn request_upload(&mut self, source: UploadSource) {
        self.events.push(SessionEvent::UploadRequested {
            conversation: self.current_conversation,
            source,
            composer_nonce: self.composer.nonce.clone(),
        });
    }
    pub(crate) fn capture_upload_input(&mut self, ctx: &egui::Context) {
        if self.current_conversation.is_none() || !self.input_had_focus {
            return;
        }
        let mut images = Vec::new();
        let mut paste_event = false;
        let mut release = false;
        ctx.input_mut(|i| {
            i.events.retain(|event| match event {
                egui::Event::PasteImage(image) => {
                    images.push(image.clone());
                    paste_event = true;
                    false
                }
                egui::Event::Paste(_) => {
                    paste_event = true;
                    true
                }
                egui::Event::Key {
                    key: egui::Key::V,
                    pressed: false,
                    modifiers,
                    ..
                } if modifiers.command => {
                    release = true;
                    true
                }
                _ => true,
            });
        });
        for image in images {
            self.request_upload(UploadSource::Image(image));
        }
        if (paste_event || (release && !self.upload_discovery.paste_seen))
            && let Ok(mut clipboard) = arboard::Clipboard::new()
            && let Ok(paths) = clipboard.get().file_list()
            && !paths.is_empty()
        {
            ctx.input_mut(|i| {
                i.events
                    .retain(|event| !matches!(event, egui::Event::Paste(_)))
            });
            self.request_upload(UploadSource::Files(paths));
        }
        if paste_event {
            self.upload_discovery.paste_seen = true;
        }
        if release {
            self.upload_discovery.paste_seen = false;
        }
    }
    pub(crate) fn capture_upload_drop(&mut self, ui: &egui::Ui) {
        if self.current_conversation.is_none()
            || !ui.ctx().input(|i| {
                i.pointer
                    .latest_pos()
                    .is_some_and(|pos| ui.max_rect().contains(pos))
            })
        {
            return;
        }
        let files = ui
            .ctx()
            .input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
        let paths: Vec<_> = files
            .iter()
            .map(|file| file.path().to_path_buf())
            .filter(|path| !path.as_os_str().is_empty())
            .collect();
        if !paths.is_empty() {
            self.request_upload(UploadSource::Files(paths));
        }
    }
    pub(crate) fn outgoing_upload_token_request(
        command: &IrcCommand,
    ) -> Option<(String, Option<String>)> {
        if !matches!(command, IrcCommand::Raw(_) | IrcCommand::Extension(_, _)) {
            return None;
        }
        let message = IrcMessage::parse(&command.to_string())?;
        let IrcCommand::Extension(name, params) = &message.command else {
            return None;
        };
        if !name.eq_ignore_ascii_case("TOKEN") || !params.first()?.eq_ignore_ascii_case("GENERATE")
        {
            return None;
        }
        Some((params.get(1)?.clone(), message.get_tag("label")))
    }
    pub(crate) fn record_upload_token_request(&mut self, request: (String, Option<String>)) {
        self.upload_discovery
            .requests
            .push_back((request.0, request.1, None));
    }
    pub(crate) fn request_upload_token(&mut self, id: String) -> bool {
        if self.upload_discovery.pending.is_some()
            || self.upload_discovery.unlabeled_token_timed_out
            || !self.is_connected()
        {
            return false;
        }
        let label = self
            .enabled_caps
            .contains("labeled-response")
            .then(|| format!("lf-upload-{id}"));
        let command = IrcCommand::Raw(format!(
            "{}TOKEN GENERATE FILEHOST",
            label
                .as_ref()
                .map(|label| format!("@label={label} "))
                .unwrap_or_default()
        ));
        if !self.send_command(command) {
            return false;
        }
        if let Some(request) = self.upload_discovery.requests.back_mut() {
            request.2 = Some(id.clone());
        }
        self.upload_discovery.pending = Some((id, "FILEHOST".into(), label, Instant::now()));
        true
    }
    pub(crate) fn upload_batch_label(&self, batch_id: &str) -> Option<String> {
        let mut current = batch_id;
        for _ in 0..32 {
            if let Some(label) = self.batch_labels.get(current) {
                return Some(label.clone());
            }
            current = self.batch_parents.get(current)?;
        }
        None
    }
    pub(crate) fn complete_upload_token(
        &mut self,
        service: &str,
        token: String,
        label: Option<String>,
    ) -> bool {
        let index =
            self.upload_discovery
                .requests
                .iter()
                .position(|(expected, expected_label, _)| {
                    service.eq_ignore_ascii_case(expected) && expected_label == &label
                });
        let Some(index) = index else {
            return false;
        };
        let (_, _, job_id) = self.upload_discovery.requests.remove(index).unwrap();
        let Some(id) = job_id else {
            return false;
        };
        if self
            .upload_discovery
            .pending
            .as_ref()
            .is_some_and(|(pending, _, _, _)| pending == &id)
        {
            self.upload_discovery.pending = None;
        }
        let valid = !token.is_empty()
            && token.len() <= 8192
            && !token
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace());
        self.events.push(SessionEvent::UploadTokenReady {
            job_id: id,
            token: valid.then_some(token),
        });
        true
    }
    pub(crate) fn invalidate_upload_credentials(&mut self) {
        self.upload_discovery.credentials_stale = true;
    }
    pub(crate) fn observe_upload_protocol(&mut self, message: &IrcMessage) -> bool {
        let account = match &message.command {
            IrcCommand::Numeric(crate::irc::numerics::RPL_LOGGEDIN, params)
                if params
                    .first()
                    .is_some_and(|nick| self.identifiers_equal(nick, &self.my_nick)) =>
            {
                params.get(2).cloned()
            }
            IrcCommand::Account(account)
                if message
                    .get_sender_nick()
                    .is_some_and(|nick| self.identifiers_equal(&nick, &self.my_nick)) =>
            {
                Some(account.clone())
            }
            IrcCommand::Numeric(crate::irc::numerics::RPL_LOGGEDOUT, params)
                if params
                    .first()
                    .is_some_and(|nick| self.identifiers_equal(nick, &self.my_nick)) =>
            {
                Some("*".into())
            }
            _ => None,
        };
        if let Some(account) = account {
            if account == "*"
                || self
                    .upload_discovery
                    .confirmed_account
                    .as_ref()
                    .is_some_and(|old| !old.eq_ignore_ascii_case(&account))
            {
                self.upload_discovery.credentials_stale = true;
            }
            self.upload_discovery.confirmed_account = Some(account);
        }
        match &message.command {
            IrcCommand::Extension(name, params) if name.eq_ignore_ascii_case("TOKEN") => {
                if params
                    .first()
                    .is_some_and(|p| p.eq_ignore_ascii_case("SERVICE"))
                    && params
                        .get(1)
                        .is_some_and(|p| p.eq_ignore_ascii_case("FILEHOST"))
                {
                    self.upload_discovery.filehost = params
                        .get(2)
                        .filter(|value| https_url(value).is_ok())
                        .cloned();
                }
                // History ancestors are excluded by the caller, and dedicated
                // authtoken chunks are assembled before this observer. A
                // normal labeled-response wrapper still carries a live reply.
                if params
                    .first()
                    .is_some_and(|p| p.eq_ignore_ascii_case("GENERATE"))
                {
                    return self.complete_upload_token(
                        params.get(1).map_or("", String::as_str),
                        params.get(2).cloned().unwrap_or_default(),
                        self.response_label(message),
                    );
                }
            }
            IrcCommand::StandardReply {
                command,
                kind,
                context,
                ..
            } if command.eq_ignore_ascii_case("TOKEN") && kind.eq_ignore_ascii_case("FAIL") => {
                let label = self.response_label(message);
                let service = context.iter().find(|value| {
                    self.upload_discovery
                        .requests
                        .iter()
                        .any(|(service, _, _)| service.eq_ignore_ascii_case(value))
                });
                let index = self
                    .upload_discovery
                    .requests
                    .iter()
                    .position(|(key, expected, _)| {
                        expected == &label
                            && service.is_none_or(|service| key.eq_ignore_ascii_case(service))
                    });
                if let Some(index) = index {
                    let (_, _, job) = self.upload_discovery.requests.remove(index).unwrap();
                    if let Some(id) = job {
                        if self
                            .upload_discovery
                            .pending
                            .as_ref()
                            .is_some_and(|(pending, _, _, _)| pending == &id)
                        {
                            self.upload_discovery.pending = None;
                        }
                        self.events.push(SessionEvent::UploadTokenReady {
                            job_id: id,
                            token: None,
                        });
                        return true;
                    }
                }
            }
            _ => {}
        }
        false
    }
    pub(crate) fn expire_upload_token(&mut self) {
        if self
            .upload_discovery
            .pending
            .as_ref()
            .is_some_and(|(_, _, _, at)| at.elapsed() > Duration::from_secs(20))
        {
            let (id, _, label, _) = self.upload_discovery.pending.take().unwrap();
            // An unlabelled response can only be matched in FIFO order. Keep
            // its tombstone to consume a late reply, and do not issue another
            // automatic request whose fresh token could be mistaken for it.
            self.upload_discovery.unlabeled_token_timed_out |= label.is_none();
            self.events.push(SessionEvent::UploadTokenReady {
                job_id: id,
                token: None,
            });
        }
    }
}

impl UploadManager {
    pub(crate) fn has_pending_work(&self) -> bool {
        self.pending_pickers > 0
            || self
                .jobs
                .iter()
                .any(|job| matches!(job.state, State::Queued | State::Token | State::Uploading))
    }
    fn enqueue(&mut self, origin: Origin, provider: Provider, input: Input) {
        if self
            .jobs
            .iter()
            .filter(|j| matches!(j.state, State::Queued | State::Token | State::Uploading))
            .count()
            >= 18
        {
            self.notice = Some("The upload queue is full; selected file was not queued".into());
            return;
        }
        let name = match &input {
            Input::File(path) => path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            Input::Image(_) => "clipboard.png".into(),
        };
        self.jobs.push(Job {
            id: super::new_profile_id(),
            name,
            origin,
            provider,
            input: Some(input),
            state: State::Queued,
            cancelled: Arc::new(AtomicBool::new(false)),
            bytes: Arc::new(AtomicU64::new(0)),
            agent: None,
            created: Instant::now(),
        });
        if self.jobs.len() > 200
            && let Some(position) = self.jobs.iter().position(|j| {
                matches!(
                    j.state,
                    State::Done(_) | State::Failed(_) | State::Cancelled
                )
            })
        {
            self.jobs.remove(position);
        }
    }
    fn picker(&mut self, origin: Origin, provider: Provider, clipboard: bool) {
        self.pending_pickers += 1;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = if clipboard {
                arboard::Clipboard::new()
                    .map_err(|_| "Clipboard is unavailable".to_string())
                    .and_then(|mut clipboard| {
                        clipboard
                            .get()
                            .file_list()
                            .map_err(|_| "No files were found on the clipboard".to_string())
                    })
            } else {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "File picker could not start".into())
                    .map(|runtime| {
                        runtime
                            .block_on(
                                rfd::AsyncFileDialog::new()
                                    .set_title("Share a file")
                                    .pick_files(),
                            )
                            .unwrap_or_default()
                            .into_iter()
                            .map(|file| file.path().to_path_buf())
                            .collect()
                    })
            };
            let _ = tx.send(Event::Picked(Box::new(PickedFiles {
                origin,
                provider,
                result,
            })));
        });
    }
    fn cancel(&mut self, id: &str) {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
            job.cancelled.store(true, Ordering::SeqCst);
            job.finish(State::Cancelled);
        }
    }
    fn token(&mut self, id: &str, token: Option<String>) {
        if let Some(job) = self
            .jobs
            .iter_mut()
            .find(|j| j.id == id && j.state == State::Token)
        {
            if let Some(token) = token {
                job.provider.auth = UploadAuth::ServiceBearer(token);
                job.state = State::Queued;
            } else {
                job.finish(State::Failed(
                    "File host token could not be obtained; confirm your IRC account login".into(),
                ));
            }
        }
    }
}
impl Drop for UploadManager {
    fn drop(&mut self) {
        for job in &mut self.jobs {
            job.cancelled.store(true, Ordering::SeqCst);
            if let Some(agent) = job.agent.take() {
                agent.shutdown();
            }
        }
    }
}

impl App {
    pub(crate) fn handle_upload_request(
        &mut self,
        index: usize,
        conversation: Option<ConversationId>,
        nonce: String,
        source: UploadSource,
    ) {
        self.claim_global_dialogs(Some(WindowKey::new(self.sessions[index].id, conversation)));
        self.uploads.open = true;
        let Some(conversation) = conversation else {
            self.uploads.notice = Some("Open a channel or private conversation first".into());
            return;
        };
        let session = &self.sessions[index];
        let Some(network) = session.logical_network_key() else {
            self.uploads.notice = Some("No network is associated with this conversation".into());
            return;
        };
        if session.target_name(conversation).is_none() {
            return;
        }
        let provider = match session.upload_provider() {
            Ok(provider) => provider,
            Err(error) => {
                self.uploads.notice = Some(error);
                return;
            }
        };
        let origin = Origin {
            window: WindowKey::new(session.id, Some(conversation)),
            network,
            nonce,
            generation: session.connection.generation(),
        };
        match source {
            UploadSource::Pick => self.uploads.picker(origin, provider, false),
            UploadSource::Clipboard => self.uploads.picker(origin, provider, true),
            UploadSource::Files(files) => {
                for file in files {
                    self.uploads
                        .enqueue(origin.clone(), provider.clone(), Input::File(file));
                }
            }
            UploadSource::Image(image) => {
                self.uploads.enqueue(origin, provider, Input::Image(image))
            }
        }
    }
    pub(crate) fn accept_upload_token(&mut self, id: &str, token: Option<String>) {
        self.uploads.token(id, token);
    }
    pub(crate) fn tick_uploads(&mut self) {
        for event in self.uploads.rx.try_iter().collect::<Vec<_>>() {
            match event {
                Event::Picked(picked) => {
                    self.uploads.pending_pickers = self.uploads.pending_pickers.saturating_sub(1);
                    let PickedFiles {
                        origin,
                        provider,
                        result,
                    } = *picked;
                    match result {
                        Ok(files) => {
                            for file in files {
                                self.uploads.enqueue(
                                    origin.clone(),
                                    provider.clone(),
                                    Input::File(file),
                                );
                            }
                        }
                        Err(error) => self.uploads.notice = Some(error),
                    }
                }
                Event::Completed(id, result) => {
                    let Some(job) = self
                        .uploads
                        .jobs
                        .iter_mut()
                        .find(|job| job.id == id && job.state == State::Uploading)
                    else {
                        continue;
                    };
                    job.agent = None;
                    match result {
                        Ok(url) => {
                            job.finish(State::Done(url.clone()));
                            let origin = &job.origin;
                            let valid = self
                                .sessions
                                .iter()
                                .find(|s| s.id == origin.window.conn)
                                .is_some_and(|s| {
                                    s.logical_network_key().as_ref() == Some(&origin.network)
                                        && s.connection.generation() == origin.generation
                                        && origin
                                            .window
                                            .conversation
                                            .is_some_and(|id| s.target_name(id).is_some())
                                });
                            if valid
                                && let Some(window) = self.windows.get_mut(&origin.window)
                                && window.view.composer.nonce == origin.nonce
                            {
                                if !window.view.input_text.is_empty()
                                    && !window.view.input_text.ends_with(char::is_whitespace)
                                {
                                    window.view.input_text.push(' ');
                                }
                                window.view.input_text.push_str(&url);
                            }
                        }
                        Err(error) => job.finish(State::Failed(error)),
                    }
                }
            }
        }
        let mut active = self
            .uploads
            .jobs
            .iter()
            .filter(|j| matches!(j.state, State::Uploading | State::Token))
            .count();
        let token_connections: Vec<_> = self
            .uploads
            .jobs
            .iter()
            .filter(|j| j.state == State::Token)
            .map(|j| j.origin.window.conn)
            .collect();
        for job in &mut self.uploads.jobs {
            if job.state == State::Token && job.created.elapsed() > Duration::from_secs(30) {
                job.finish(State::Failed("File host token request timed out".into()));
                active = active.saturating_sub(1);
            }
            if active >= 2 || job.state != State::Queued {
                continue;
            }
            let Some(index) = self.sessions.iter().position(|s| {
                s.id == job.origin.window.conn
                    && s.logical_network_key().as_ref() == Some(&job.origin.network)
                    && s.connection.generation() == job.origin.generation
            }) else {
                job.finish(State::Failed(
                    "The original connection was closed or changed".into(),
                ));
                continue;
            };
            if matches!(job.provider.auth, UploadAuth::TokenService) {
                if self.sessions[index]
                    .upload_discovery
                    .unlabeled_token_timed_out
                {
                    job.finish(State::Failed(
                        "The previous upload token request timed out without a response label; reconnect before uploading again".into(),
                    ));
                    continue;
                }
                if token_connections.contains(&job.origin.window.conn) {
                    continue;
                }
                if self.sessions[index].request_upload_token(job.id.clone()) {
                    job.state = State::Token;
                    job.created = Instant::now();
                    active += 1;
                }
                continue;
            }
            let Some(input) = job.input.take() else {
                continue;
            };
            let agent = ureq::Agent::config_builder()
                .tls_config(
                    ureq::tls::TlsConfig::builder()
                        .provider(ureq::tls::TlsProvider::NativeTls)
                        .build(),
                )
                .https_only(true)
                .max_redirects(0)
                .timeout_global(Some(Duration::from_secs(300)))
                .timeout_connect(Some(Duration::from_secs(15)))
                .timeout_recv_response(Some(Duration::from_secs(30)))
                .build()
                .new_agent();
            job.agent = Some(agent.clone());
            job.state = State::Uploading;
            active += 1;
            let (id, name, provider, cancelled, bytes, tx) = (
                job.id.clone(),
                job.name.clone(),
                job.provider.clone(),
                job.cancelled.clone(),
                job.bytes.clone(),
                self.uploads.tx.clone(),
            );
            std::thread::spawn(move || {
                let result = perform_upload(agent, provider, input, &name, cancelled, bytes);
                let _ = tx.send(Event::Completed(id, result));
            });
        }
    }
    pub(crate) fn show_uploads(&mut self, ctx: &egui::Context) {
        if !self.uploads.open {
            return;
        }
        let mut open = true;
        let mut cancel = Vec::new();
        egui::Window::new("File transfers").open(&mut open).default_size(egui::vec2(470.0,280.0)).show(ctx,|ui|{
            if let Some(notice)=&self.uploads.notice{ui.colored_label(egui::Color32::YELLOW,notice);}
            egui::ScrollArea::vertical().max_height((ui.available_height()-24.0).max(80.0)).show(ui,|ui|{
                for job in &self.uploads.jobs{ui.group(|ui|{ui.label(&job.name);match &job.state{
                    State::Queued=>{ui.label("Queued");},State::Token=>{ui.label("Obtaining upload token…");},State::Uploading=>{ui.label(format!("Uploading: {} KiB",job.bytes.load(Ordering::Relaxed)/1024));ctx.request_repaint_after(Duration::from_millis(100));},
                    State::Done(url)=>{ui.label("Uploaded; URL added to the original draft when still available.");if ui.button("Copy URL").clicked(){ctx.copy_text(url.clone());}},State::Failed(error)=>{ui.colored_label(egui::Color32::RED,error);},State::Cancelled=>{ui.label("Cancelled");}}
                    if matches!(job.state,State::Queued|State::Token|State::Uploading)&&ui.button("Cancel").clicked(){cancel.push(job.id.clone());}
                });}
            });
        });
        self.uploads.open = open;
        for id in cancel {
            self.uploads.cancel(&id);
        }
    }
}

struct ProgressReader<R> {
    inner: R,
    bytes: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
    limit: u64,
}
impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled",
            ));
        }
        let read = self.inner.read(buffer)?;
        let total = self
            .bytes
            .fetch_add(read as u64, Ordering::Relaxed)
            .saturating_add(read as u64);
        if total > self.limit {
            return Err(std::io::Error::other("file exceeded upload size limit"));
        }
        Ok(read)
    }
}
fn perform_upload(
    agent: ureq::Agent,
    provider: Provider,
    input: Input,
    name: &str,
    cancelled: Arc<AtomicBool>,
    bytes: Arc<AtomicU64>,
) -> Result<String, String> {
    https_url(&provider.url)?;
    let (reader, size, mime): (Box<dyn Read>, u64, String) = match input {
        Input::File(path) => {
            let file =
                File::open(&path).map_err(|_| "Selected file could not be opened".to_string())?;
            let metadata = file
                .metadata()
                .map_err(|_| "File size could not be read".to_string())?;
            if !metadata.is_file() {
                return Err("Choose a regular file".into());
            }
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            (Box::new(file), metadata.len(), mime)
        }
        Input::Image(image) => {
            let count = image.size[0]
                .checked_mul(image.size[1])
                .ok_or("Image dimensions are too large")?;
            if count > 16 * 1024 * 1024 || count != image.pixels.len() {
                return Err("Clipboard image exceeds the 16 megapixel limit".into());
            }
            let mut encoded = Vec::new();
            {
                let mut encoder =
                    png::Encoder::new(&mut encoded, image.size[0] as u32, image.size[1] as u32);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder
                    .write_header()
                    .map_err(|_| "Image encoding failed")?;
                let pixels: Vec<u8> = image
                    .pixels
                    .iter()
                    .flat_map(|pixel| pixel.to_srgba_unmultiplied())
                    .collect();
                writer
                    .write_image_data(&pixels)
                    .map_err(|_| "Image encoding failed")?;
            }
            let size = encoded.len() as u64;
            (Box::new(Cursor::new(encoded)), size, "image/png".into())
        }
    };
    if size > provider.max_bytes {
        return Err("File exceeds the configured upload size limit".into());
    }
    if cancelled.load(Ordering::SeqCst) {
        return Err("Cancelled".into());
    }
    let mut client_tls = None;
    if let UploadAuth::Certificate(cert, key) = &provider.auth {
        let cert =
            std::fs::read(cert).map_err(|_| "Client certificate could not be read".to_string())?;
        let key = if let Some(path) = key {
            std::fs::read(path).map_err(|_| "Client key could not be read".to_string())?
        } else {
            cert.clone()
        };
        let mut chain = Vec::new();
        for item in ureq::tls::parse_pem(&cert) {
            if let Ok(ureq::tls::PemItem::Certificate(cert)) = item {
                chain.push(cert);
            }
        }
        let key = ureq::tls::PrivateKey::from_pem(&key)
            .map_err(|_| "Client key could not be loaded".to_string())?;
        client_tls = Some(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .client_cert(Some(ureq::tls::ClientCert::new_with_certs(&chain, key)))
                .build(),
        );
    }
    perform_filehost_request(
        agent,
        provider,
        name,
        UploadBody { reader, size, mime },
        client_tls,
        cancelled,
        bytes,
    )
}
fn disposition(name: &str) -> String {
    let name: String = name
        .chars()
        .filter(|ch| !ch.is_control() && !matches!(ch, '"' | '\\'))
        .take(180)
        .collect();
    let ascii: String = name
        .chars()
        .map(|ch| if ch.is_ascii() { ch } else { '_' })
        .collect();
    let encoded: String = name
        .as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
                (*byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}
struct UploadBody {
    reader: Box<dyn Read>,
    size: u64,
    mime: String,
}
fn perform_filehost_request(
    agent: ureq::Agent,
    provider: Provider,
    name: &str,
    payload: UploadBody,
    client_tls: Option<ureq::tls::TlsConfig>,
    cancelled: Arc<AtomicBool>,
    bytes: Arc<AtomicU64>,
) -> Result<String, String> {
    let UploadBody { reader, size, mime } = payload;
    let mut options = agent.options(&provider.url);
    if let Some(tls) = &client_tls {
        options = options.config().tls_config(tls.clone()).build();
    }
    options = match &provider.auth {
        UploadAuth::Basic(user, password) => options.header(
            "Authorization",
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
            ),
        ),
        UploadAuth::Bearer(token) => options.header("Authorization", format!("Bearer {token}")),
        _ => options,
    };
    let options = options.call().map_err(io_http_error)?;
    if let Some(accepted) = options
        .headers()
        .get("accept-post")
        .and_then(|v| v.to_str().ok())
    {
        let accepted = accepted.split(',').map(str::trim).any(|value| {
            value == "*/*"
                || value.eq_ignore_ascii_case(&mime)
                || value
                    .strip_suffix("/*")
                    .is_some_and(|prefix| mime.starts_with(&format!("{prefix}/")))
        });
        if !accepted {
            return Err("The file host does not accept this file type".into());
        }
    }
    if let UploadAuth::ServiceBearer(token) = &provider.auth
        && !service_token_current(token)
    {
        return Err(
            "The generated upload token expired before transfer; attach the file again".into(),
        );
    }
    let mut request = agent
        .post(&provider.url)
        .header("Content-Type", &mime)
        .header("Content-Length", size.to_string());
    if let Some(tls) = client_tls {
        request = request.config().tls_config(tls).build();
    }
    request = request.header("Content-Disposition", disposition(name));
    request = match provider.auth {
        UploadAuth::Basic(user, password) => request.header(
            "Authorization",
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
            ),
        ),
        UploadAuth::Bearer(token) | UploadAuth::ServiceBearer(token) => {
            request.header("Authorization", format!("Bearer {token}"))
        }
        _ => request,
    };
    let mut reader = ProgressReader {
        inner: reader,
        bytes,
        cancelled: cancelled.clone(),
        limit: provider.max_bytes,
    };
    let response = request
        .send(ureq::SendBody::from_reader(&mut reader))
        .map_err(io_http_error)?;
    if cancelled.load(Ordering::SeqCst) {
        return Err("Cancelled".into());
    }
    if response.status().as_u16() != 201 {
        return Err("File host must return 201 Created and a download Location".into());
    }
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or("File host did not return a download URL")?;
    upload_location(&provider.url, location)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filehost_fixture(responses: Vec<String>) -> (String, mpsc::Receiver<(String, Vec<u8>)>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/upload", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(5);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.is_empty() {
                        return;
                    }
                    headers.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let size = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; size];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                if tx.send((headers, body)).is_err() {
                    return;
                }
                if reader.get_mut().write_all(response.as_bytes()).is_err() {
                    return;
                }
            }
        });
        (url, rx)
    }

    #[test]
    fn filehost_posts_exact_bytes_and_uses_generated_token_only_once() {
        let (url, requests) = filehost_fixture(vec![
            "HTTP/1.1 204 No Content\r\nAccept-Post: text/*\r\nConnection: close\r\n\r\n".into(),
            "HTTP/1.1 201 Created\r\nLocation: https://cdn.example/file.txt\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let provider = Provider {
            url,
            auth: UploadAuth::ServiceBearer("test-token".into()),
            max_bytes: 100,
        };
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .new_agent();
        let bytes = Arc::new(AtomicU64::new(0));
        let result = perform_filehost_request(
            agent,
            provider,
            "hello.txt",
            UploadBody {
                reader: Box::new(Cursor::new(b"hello")),
                size: 5,
                mime: "text/plain".into(),
            },
            None,
            Arc::new(AtomicBool::new(false)),
            bytes.clone(),
        )
        .unwrap();
        assert_eq!(result, "https://cdn.example/file.txt");
        let (options, _) = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(options.starts_with("OPTIONS /upload "));
        assert!(!options.to_lowercase().contains("authorization:"));
        let (post, body) = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(post.starts_with("POST /upload "));
        assert!(
            post.to_lowercase()
                .contains("authorization: bearer test-token")
        );
        assert_eq!(body, b"hello");
        assert_eq!(bytes.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn unsupported_mime_stops_before_posting_file_bytes() {
        let (url, requests) = filehost_fixture(vec![
            "HTTP/1.1 204 No Content\r\nAccept-Post: image/*\r\nConnection: close\r\n\r\n".into(),
        ]);
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .new_agent();
        let bytes = Arc::new(AtomicU64::new(0));
        let result = perform_filehost_request(
            agent,
            Provider {
                url,
                auth: UploadAuth::None,
                max_bytes: 100,
            },
            "a.zip",
            UploadBody {
                reader: Box::new(Cursor::new(b"secret file")),
                size: 11,
                mime: "application/zip".into(),
            },
            None,
            Arc::new(AtomicBool::new(false)),
            bytes.clone(),
        );
        assert!(result.unwrap_err().contains("does not accept"));
        assert_eq!(bytes.load(Ordering::Relaxed), 0);
        assert!(
            requests
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .0
                .starts_with("OPTIONS")
        );
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn unicode_filename_is_encoded_without_header_injection() {
        let value = disposition("你好\r\nInjected: yes.png");
        assert!(!value.contains(['\r', '\n']));
        assert!(value.contains("filename*=UTF-8''%E4%BD%A0%E5%A5%BD"));
    }

    #[test]
    fn manual_token_reply_cannot_be_consumed_by_automatic_upload() {
        let mut session = Session::with_settings(super::super::Settings {
            logging_enabled: false,
            ..Default::default()
        });
        session.record_upload_token_request(("FILEHOST".into(), None));
        session.record_upload_token_request(("FILEHOST".into(), None));
        session.upload_discovery.requests.back_mut().unwrap().2 = Some("job".into());
        session.upload_discovery.pending =
            Some(("job".into(), "FILEHOST".into(), None, Instant::now()));
        assert!(!session.complete_upload_token("FILEHOST", "manual-token".into(), None));
        assert!(session.upload_discovery.pending.is_some());
        assert!(session.complete_upload_token("FILEHOST", "private-token".into(), None));
        assert!(
            matches!(session.events.last(), Some(SessionEvent::UploadTokenReady { job_id, token: Some(token) }) if job_id=="job" && token=="private-token")
        );
        assert!(session.server_messages.is_empty());
    }

    fn pending_labeled_upload() -> Session {
        let mut session = Session::with_settings(super::super::Settings {
            nickname: "me".into(),
            logging_enabled: false,
            logging_load_history: false,
            ..Default::default()
        });
        let (sender, mut outgoing) = tokio::sync::mpsc::unbounded_channel();
        session.test_set_registered(sender);
        session.enabled_caps.insert("labeled-response".into());
        assert!(session.request_upload_token("job".into()));
        assert_eq!(
            outgoing.try_recv().unwrap().to_string(),
            "@label=lf-upload-job TOKEN GENERATE FILEHOST"
        );
        session
    }

    fn assert_private_upload_token(session: &Session, expected: &str) {
        let ready: Vec<_> = session
            .events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::UploadTokenReady { job_id, token } => Some((job_id, token)),
                _ => None,
            })
            .collect();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, "job");
        assert_eq!(ready[0].1.as_deref(), Some(expected));
        assert!(session.upload_discovery.pending.is_none());
        assert!(
            session
                .server_messages
                .iter()
                .chain(
                    session
                        .conversations
                        .values()
                        .flat_map(|channel| &channel.messages)
                )
                .all(|message| !message.content.contains(expected))
        );
    }

    #[test]
    fn generic_labeled_wrapper_completes_upload_without_displaying_the_token() {
        let mut session = pending_labeled_upload();
        for line in [
            "@label=lf-upload-job :server BATCH +response labeled-response",
            "@batch=response :server TOKEN GENERATE FILEHOST private-wrapper-token",
            ":server BATCH -response",
        ] {
            session.handle_incoming_message(IrcMessage::parse(line).unwrap());
        }
        assert_private_upload_token(&session, "private-wrapper-token");
    }

    #[test]
    fn nested_authtoken_wrapper_assembles_upload_token_privately() {
        let mut session = pending_labeled_upload();
        for line in [
            "@label=lf-upload-job :server BATCH +response labeled-response",
            "@batch=response :server BATCH +token draft/authtoken FILEHOST",
            "@batch=token :server TOKEN GENERATE FILEHOST private-chunk-",
            "@batch=token :server TOKEN GENERATE FILEHOST assembled",
            "@batch=response :server BATCH -token",
            ":server BATCH -response",
        ] {
            session.handle_incoming_message(IrcMessage::parse(line).unwrap());
        }
        assert_private_upload_token(&session, "private-chunk-assembled");
        assert!(session.server_messages.iter().all(|message| {
            !message.content.contains("private-chunk-") && !message.content.contains("assembled")
        }));
    }

    #[test]
    fn unsaved_connection_form_cannot_change_active_upload_credentials() {
        let mut session = Session::with_settings(super::super::Settings {
            server_host: "a.example".into(),
            nickname: "me".into(),
            logging_enabled: false,
            upload_settings: UploadSettings {
                use_custom: true,
                url: "https://a.example/upload".into(),
                ..Default::default()
            },
            ..Default::default()
        });
        assert!(session.start_session_from_form());
        session.upload_settings.url = "https://b.example/upload".into();
        assert_eq!(
            session.upload_provider().unwrap().url,
            "https://a.example/upload"
        );
        session.commit_upload_settings();
        assert_eq!(
            session.upload_provider().unwrap().url,
            "https://b.example/upload"
        );
        session.server_host = "different.example".into();
        session.upload_settings.url = "https://other.example/upload".into();
        session.commit_upload_settings();
        assert_eq!(
            session.upload_provider().unwrap().url,
            "https://b.example/upload"
        );
    }

    #[test]
    fn auto_certificate_password_fallback_uses_confirmed_upload_authentication() {
        use crate::irc::client::{AuthenticationStatus, SaslMechanism};
        let mut session = Session::with_settings(super::super::Settings {
            server_host: "irc.example".into(),
            nickname: "me".into(),
            logging_enabled: false,
            sasl_username: "alice".into(),
            sasl_password: "dummy-password".into(),
            tls_client_cert_path: "test-cert.pem".into(),
            ..Default::default()
        });
        assert!(session.start_session_from_form());
        session.network_support.isupport.insert(
            "SOJU.IM/FILEHOST".into(),
            Some("https://files.example/upload".into()),
        );
        session.connection_status.sasl_mechanism = Some(SaslMechanism::Plain);
        session.connection_status.authentication = AuthenticationStatus::LoggedIn {
            account: "alice".into(),
        };
        assert!(
            matches!(session.upload_provider().unwrap().auth, UploadAuth::Basic(user, _) if user=="alice")
        );
        session.connection_status.authentication = AuthenticationStatus::LoggedIn {
            account: "other-account".into(),
        };
        assert!(matches!(
            session.upload_provider().unwrap().auth,
            UploadAuth::None
        ));
        session.connection_status.sasl_mechanism = Some(SaslMechanism::External);
        assert!(matches!(
            session.upload_provider().unwrap().auth,
            UploadAuth::Certificate(_, _)
        ));
    }

    fn upload_app() -> (
        App,
        WindowKey,
        tokio::sync::mpsc::UnboundedReceiver<IrcCommand>,
    ) {
        let mut app = App::with_settings(
            super::super::Settings {
                server_host: "irc.example".into(),
                nickname: "me".into(),
                logging_enabled: false,
                upload_settings: UploadSettings {
                    use_custom: true,
                    url: "https://files.example/upload".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            Default::default(),
        );
        let index = app.add_session();
        assert!(app.sessions[index].start_session_from_form());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        app.sessions[index].test_set_registered(tx);
        app.sessions[index]
            .conversations
            .insert("alice".into(), super::super::Channel::new());
        let key = WindowKey::new(
            app.sessions[index].id,
            app.sessions[index].id_for_target("alice"),
        );
        app.windows.sync(&[super::super::windows::PresentWindow {
            key,
            persist: super::super::windows::PersistKey {
                network: "test".into(),
                target: Some("alice".into()),
            },
        }]);
        (app, key, rx)
    }

    #[test]
    fn rejected_upload_token_releases_the_queued_image_payload() {
        let (mut app, key, _rx) = upload_app();
        let image = Arc::new(egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]));
        let weak = Arc::downgrade(&image);
        let nonce = app.windows.get(&key).unwrap().view.composer.nonce.clone();
        app.handle_upload_request(0, key.conversation, nonce, UploadSource::Image(image));
        let job = &mut app.uploads.jobs[0];
        job.state = State::Token;
        let id = job.id.clone();
        assert!(weak.upgrade().is_some());
        app.uploads.token(&id, None);
        assert!(matches!(app.uploads.jobs[0].state, State::Failed(_)));
        assert!(
            weak.upgrade().is_none(),
            "failed transfer history retained the full image payload"
        );
    }

    #[test]
    fn obsolete_upload_origin_releases_the_queued_image_payload() {
        let (mut app, key, _rx) = upload_app();
        let image = Arc::new(egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]));
        let weak = Arc::downgrade(&image);
        let nonce = app.windows.get(&key).unwrap().view.composer.nonce.clone();
        app.handle_upload_request(0, key.conversation, nonce, UploadSource::Image(image));
        app.uploads.jobs[0].origin.generation = Some(u64::MAX);
        app.tick_uploads();
        assert!(matches!(app.uploads.jobs[0].state, State::Failed(_)));
        assert!(
            weak.upgrade().is_none(),
            "obsolete transfer retained its image payload"
        );
    }

    #[test]
    fn completed_upload_preserves_origin_and_does_not_send_or_steal_a_new_draft() {
        for new_draft in [false, true] {
            let (mut app, key, mut outgoing) = upload_app();
            let window = app.windows.get_mut(&key).unwrap();
            window.view.input_text = "original draft".into();
            let nonce = window.view.composer.nonce.clone();
            app.handle_upload_request(
                0,
                key.conversation,
                nonce,
                UploadSource::Files(vec![PathBuf::from("not-opened.txt")]),
            );
            let job = app.uploads.jobs.first_mut().unwrap();
            job.state = State::Uploading;
            job.input = None;
            let id = job.id.clone();
            let window = app.windows.get_mut(&key).unwrap();
            if new_draft {
                window.view.composer = Default::default();
                window.view.input_text = "replacement draft".into();
            } else {
                window.view.input_text.push_str(" with more typing");
            }
            app.uploads
                .tx
                .send(Event::Completed(id, Ok("https://cdn.example/file".into())))
                .unwrap();
            app.tick_uploads();
            assert_eq!(
                app.windows.get(&key).unwrap().view.input_text,
                if new_draft {
                    "replacement draft"
                } else {
                    "original draft with more typing https://cdn.example/file"
                }
            );
            assert!(outgoing.try_recv().is_err());
        }
    }

    #[test]
    fn cancelled_upload_ignores_a_late_success() {
        let (mut app, key, _outgoing) = upload_app();
        let window = app.windows.get_mut(&key).unwrap();
        window.view.input_text = "keep this".into();
        let nonce = window.view.composer.nonce.clone();
        app.handle_upload_request(
            0,
            key.conversation,
            nonce,
            UploadSource::Files(vec![PathBuf::from("not-opened.txt")]),
        );
        let id = app.uploads.jobs[0].id.clone();
        app.uploads.cancel(&id);
        app.uploads
            .tx
            .send(Event::Completed(id, Ok("https://cdn.example/file".into())))
            .unwrap();
        app.tick_uploads();
        assert_eq!(app.windows.get(&key).unwrap().view.input_text, "keep this");
        assert_eq!(app.uploads.jobs[0].state, State::Cancelled);
    }

    #[test]
    fn unlabeled_token_timeout_requires_reconnect_before_later_uploads() {
        let (mut app, key, mut outgoing) = upload_app();
        let nonce = app.windows.get(&key).unwrap().view.composer.nonce.clone();
        app.handle_upload_request(
            0,
            key.conversation,
            nonce.clone(),
            UploadSource::Files(vec![PathBuf::from("not-opened-first.txt")]),
        );
        app.uploads.jobs[0].provider.auth = UploadAuth::TokenService;
        app.tick_uploads();
        assert_eq!(app.uploads.jobs[0].state, State::Token);
        assert_eq!(
            outgoing.try_recv().unwrap().to_string(),
            "TOKEN GENERATE FILEHOST"
        );
        app.sessions[0].upload_discovery.pending.as_mut().unwrap().3 =
            Instant::now() - Duration::from_secs(21);
        app.sessions[0].expire_upload_token();
        app.process_events();
        assert!(matches!(app.uploads.jobs[0].state, State::Failed(_)));

        app.handle_upload_request(
            0,
            key.conversation,
            nonce,
            UploadSource::Files(vec![PathBuf::from("not-opened-second.txt")]),
        );
        app.uploads.jobs[1].provider.auth = UploadAuth::TokenService;
        app.tick_uploads();
        assert!(
            matches!(&app.uploads.jobs[1].state, State::Failed(error) if error.to_lowercase().contains("reconnect")),
            "A timed-out unlabeled response cannot safely be correlated to a later upload: {:?}",
            app.uploads.jobs[1].state
        );
        assert!(
            outgoing.try_recv().is_err(),
            "Do not send an ambiguous second request"
        );
        assert!(app.sessions[0].complete_upload_token(
            "FILEHOST",
            "late-private-token".into(),
            None
        ));
        app.process_events();
        assert!(matches!(app.uploads.jobs[1].state, State::Failed(_)));
        assert!(app.windows.get(&key).unwrap().view.input_text.is_empty());
    }

    #[test]
    fn labeled_token_timeout_allows_a_distinct_request_and_ignores_the_late_token() {
        let (mut app, _key, mut outgoing) = upload_app();
        let session = &mut app.sessions[0];
        session.enabled_caps.insert("labeled-response".into());
        assert!(session.request_upload_token("old".into()));
        session.upload_discovery.pending.as_mut().unwrap().3 =
            Instant::now() - Duration::from_secs(21);
        session.expire_upload_token();
        session.events.clear();
        assert!(session.request_upload_token("new".into()));
        assert!(session.complete_upload_token(
            "FILEHOST",
            "old-private-token".into(),
            Some("lf-upload-old".into())
        ));
        assert_eq!(session.upload_discovery.pending.as_ref().unwrap().0, "new");
        assert!(session.complete_upload_token(
            "FILEHOST",
            "new-private-token".into(),
            Some("lf-upload-new".into())
        ));
        let ready: Vec<_> = session
            .events
            .iter()
            .filter_map(|event| {
                if let SessionEvent::UploadTokenReady { job_id, token } = event {
                    Some((job_id.as_str(), token.as_deref()))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            ready,
            [
                ("old", Some("old-private-token")),
                ("new", Some("new-private-token"))
            ]
        );
        assert!(outgoing.try_recv().is_ok());
        assert!(outgoing.try_recv().is_ok());
        assert!(outgoing.try_recv().is_err());
    }

    #[test]
    fn a_picker_completion_keeps_polling_until_the_gui_drains_it() {
        let (mut app, key, _outgoing) = upload_app();
        let origin = Origin {
            window: key,
            network: app.sessions[0].logical_network_key().unwrap(),
            nonce: app.windows.get(&key).unwrap().view.composer.nonce.clone(),
            generation: app.sessions[0].connection.generation(),
        };
        let provider = app.sessions[0].upload_provider().unwrap();
        app.uploads.open = false;
        app.uploads.pending_pickers = 1;
        app.sessions[0].test_set_disconnected();
        app.uploads
            .tx
            .send(Event::Picked(Box::new(PickedFiles {
                origin,
                provider,
                result: Ok(Vec::new()),
            })))
            .unwrap();
        assert!(app.uploads.has_pending_work());
        assert!(app.wants_polling());
        app.tick_uploads();
        assert_eq!(app.uploads.pending_pickers, 0);
        assert!(!app.uploads.has_pending_work());
        assert!(!app.wants_polling());
    }

    #[test]
    fn an_upload_keeps_polling_after_the_transfer_panel_is_closed() {
        let (mut app, key, _outgoing) = upload_app();
        let nonce = app.windows.get(&key).unwrap().view.composer.nonce.clone();
        app.handle_upload_request(
            0,
            key.conversation,
            nonce,
            UploadSource::Files(vec![PathBuf::from("not-opened.txt")]),
        );
        app.uploads.open = false;
        app.sessions[0].test_set_disconnected();
        assert!(app.uploads.has_pending_work());
        assert!(app.wants_polling());
        let id = app.uploads.jobs[0].id.clone();
        app.uploads.cancel(&id);
        assert!(!app.uploads.has_pending_work());
        assert!(!app.wants_polling());
    }
    #[test]
    fn download_locations_resolve_safely_without_forwarding_credentials() {
        assert_eq!(
            upload_location("https://files.example/upload", "../image.png").unwrap(),
            "https://files.example/image.png"
        );
        assert!(upload_location("https://files.example/upload", "https://cdn.example/a").is_ok());
        for location in [
            "http://example/a",
            "javascript:alert(1)",
            "https://user:secret@example/a",
            "/bad\nheader",
        ] {
            assert!(upload_location("https://files.example/upload", location).is_err());
        }
    }
    #[test]
    fn upload_settings_debug_does_not_disclose_secret() {
        let settings = UploadSettings {
            secret: "PRIVATE-CREDENTIAL".into(),
            ..Default::default()
        };
        assert!(!format!("{settings:?}").contains("PRIVATE-CREDENTIAL"));
    }
    #[test]
    fn size_limit_is_checked_during_streaming_and_cancellation_is_terminal() {
        let bytes = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let mut reader = ProgressReader {
            inner: Cursor::new(b"123456"),
            bytes,
            cancelled: cancel.clone(),
            limit: 4,
        };
        assert!(reader.read(&mut [0u8; 8]).is_err());
        cancel.store(true, Ordering::SeqCst);
        assert_eq!(
            reader.read(&mut [0u8; 8]).unwrap_err().kind(),
            std::io::ErrorKind::Interrupted
        );
    }
}
