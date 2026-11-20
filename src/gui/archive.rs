//! Durable message metadata and cancellable saved-history search.
//! The journal is authoritative; SQLite is a replaceable search projection.
use super::ChatMessage;
use super::identity::{ConversationKey, LogicalNetworkKey, MessageRef};
use super::logging::{LogSpan, file_generation, line_digest, line_record_id};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) fn new_record_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("operating system randomness unavailable");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ArchiveRecord {
    pub id: String,
    pub conversation: ConversationKey,
    pub received_at: Option<i64>,
    pub display_time: String,
    pub server_time: Option<String>,
    pub msgid: Option<String>,
    pub account: Option<String>,
    pub reply_to: Option<String>,
    pub sender: String,
    pub body: String,
    pub action: bool,
    pub system: bool,
    #[serde(default)]
    pub redacted: bool,
    #[serde(default)]
    pub legacy: bool,
    #[serde(default)]
    pub text_span: Option<LogSpan>,
    #[serde(default)]
    pub reactions: Vec<super::message_ui::ReactionGroup>,
}

impl ArchiveRecord {
    pub(crate) fn from_message(conversation: ConversationKey, message: &ChatMessage) -> Self {
        Self {
            id: message.message_key.clone(),
            conversation,
            received_at: Some(epoch_now()),
            display_time: message.timestamp.clone(),
            server_time: message.server_time.clone(),
            msgid: message.msgid.clone(),
            account: message.account.clone(),
            reply_to: message.reply_to.clone(),
            sender: message.sender.clone(),
            body: message.content.clone(),
            action: message.is_action,
            system: message.is_system,
            redacted: false,
            legacy: false,
            text_span: None,
            reactions: message.reactions.clone(),
        }
    }
    pub(crate) fn reference(&self) -> MessageRef {
        MessageRef {
            conversation: self.conversation.clone(),
            archive_id: Some(self.id.clone()),
            server_msgid: self.msgid.clone(),
            server_time: self.server_time.clone(),
        }
    }
    pub(crate) fn occurred_at(&self) -> Option<i64> {
        self.server_time
            .as_ref()
            .and_then(|time| {
                crate::irc::IrcMessage::parse(&format!("@time={time} :archive NOTICE * :time"))
            })
            .and_then(|message| message.get_server_time())
            .map(|time| time.min(i64::MAX as u64) as i64)
            .or(self.received_at)
    }
}

/// Keyset position of the last row of a result page: the coalesced `occurred`
/// time and the table rowid, in the order results are sorted.
pub(crate) type SearchCursor = (i64, i64);

#[derive(Clone, Debug, Default)]
pub(crate) struct SearchQuery {
    pub network_pattern: String,
    pub text: String,
    pub network: Option<LogicalNetworkKey>,
    pub target: Option<String>,
    pub sender: String,
    pub from: Option<i64>,
    pub until: Option<i64>,
    pub include_undated: bool,
    /// Continue strictly after this position. Unlike an OFFSET, new, redacted
    /// or removed rows cannot shift where the next page starts.
    pub after: Option<SearchCursor>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ArchiveStatus {
    pub indexing: bool,
    pub records: usize,
    pub error: Option<String>,
}

pub(crate) enum ArchiveReply {
    Restored {
        token: u64,
        reference: MessageRef,
        records: Vec<ArchiveRecord>,
    },
    RestoredError {
        token: u64,
        message: String,
    },
    Search {
        generation: u64,
        records: Vec<ArchiveRecord>,
        /// Cursor for the next page, present only when more rows match.
        next: Option<SearchCursor>,
    },
    Context {
        generation: u64,
        reference: MessageRef,
        records: Vec<ArchiveRecord>,
    },
    Error {
        generation: u64,
        message: String,
    },
    ContextError {
        generation: u64,
        message: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum JournalEvent {
    Upsert {
        record: Box<ArchiveRecord>,
    },
    Redact {
        conversation: ConversationKey,
        msgid: String,
    },
    Reaction {
        conversation: ConversationKey,
        msgid: String,
        actor: String,
        emoji: String,
        present: bool,
    },
}

enum Task {
    ApplyRedactions,
    #[cfg(test)]
    Pause(mpsc::SyncSender<()>, mpsc::Receiver<()>),
    Restore(u64, MessageRef, usize),
    Event(JournalEvent),
    Search(u64, SearchQuery),
    Context(u64, MessageRef, usize),
    Flush(mpsc::SyncSender<bool>),
}

#[cfg(test)]
pub(crate) struct TestSink(mpsc::Receiver<Task>);
#[cfg(test)]
impl TestSink {
    pub(crate) fn queued(&self) -> usize {
        self.0.try_iter().count()
    }
}

#[derive(Clone, Default)]
pub(crate) struct ArchiveClient {
    tx: Option<mpsc::SyncSender<Task>>,
    generation: Arc<AtomicU64>,
    context_generation: Arc<AtomicU64>,
    status: Arc<Mutex<ArchiveStatus>>,
    stop: Arc<AtomicBool>,
    redactions: Arc<Mutex<HashSet<(ConversationKey, String)>>>,
    pending_redactions: Arc<Mutex<HashSet<(ConversationKey, String)>>>,
    redacted_ids: Arc<Mutex<HashSet<(ConversationKey, String)>>>,
    /// Text-log lines (keyed like scrollback rebuilt from them) whose archive
    /// record is redacted, so reloaded history can omit them.
    redacted_lines: Arc<Mutex<HashSet<String>>>,
    redacted_lines_version: Arc<AtomicU64>,
    available: Arc<AtomicBool>,
    query_failure: Arc<Mutex<Option<ArchiveReply>>>,
    context_failure: Arc<Mutex<Option<ArchiveReply>>>,
}

impl ArchiveClient {
    pub(crate) fn is_enabled(&self) -> bool {
        self.tx.is_some() && self.available.load(Ordering::Acquire)
    }
    pub(crate) fn restore_context(&self, token: u64, reference: MessageRef, radius: usize) -> bool {
        self.submit(Task::Restore(token, reference, radius.min(250)))
    }
    fn submit(&self, task: Task) -> bool {
        let Some(tx) = &self.tx else {
            return false;
        };
        if !self.available.load(Ordering::Acquire) {
            return false;
        }
        match tx.try_send(task) {
            Ok(()) => true,
            Err(error) => {
                if matches!(error, mpsc::TrySendError::Disconnected(_)) {
                    self.available.store(false, Ordering::Release);
                }
                self.status.lock().unwrap().error=Some("History archive is unavailable or its queue is full; some new history may not be saved".into());
                false
            }
        }
    }
    pub(crate) fn upsert(&self, record: ArchiveRecord) -> bool {
        if super::commands::service_message_contains_credentials(
            &record.conversation.target,
            &record.body,
        ) {
            return false;
        }
        self.submit(Task::Event(JournalEvent::Upsert {
            record: Box::new(record),
        }))
    }
    pub(crate) fn is_redacted(&self, reference: &MessageRef) -> bool {
        reference.server_msgid.as_ref().is_some_and(|msgid| {
            self.redactions
                .lock()
                .unwrap()
                .contains(&(reference.conversation.clone(), msgid.clone()))
        }) || reference.archive_id.as_ref().is_some_and(|id| {
            self.redacted_ids
                .lock()
                .unwrap()
                .contains(&(reference.conversation.clone(), id.clone()))
        })
    }
    pub(crate) fn redact_reference(&self, reference: &MessageRef) {
        if let Some(id) = &reference.archive_id {
            self.redacted_ids
                .lock()
                .unwrap()
                .insert((reference.conversation.clone(), id.clone()));
        }
        if let Some(msgid) = &reference.server_msgid {
            self.redact(reference.conversation.clone(), msgid.clone());
        }
    }
    pub(crate) fn is_line_redacted(&self, key: &str) -> bool {
        self.redacted_lines.lock().unwrap().contains(key)
    }
    /// Changes whenever a line is added to the redacted set.
    pub(crate) fn redacted_lines_version(&self) -> u64 {
        self.redacted_lines_version.load(Ordering::Acquire)
    }
    fn note_redacted_lines(&self, spans: Vec<(String, u64, u64, String)>) {
        if spans.is_empty() {
            return;
        }
        let mut lines = self.redacted_lines.lock().unwrap();
        let mut added = false;
        for (generation, offset, len, digest) in spans {
            added |= lines.insert(super::logging::span_record_id(&LogSpan {
                path: String::new(),
                generation,
                offset,
                len,
                digest,
            }));
        }
        if added {
            self.redacted_lines_version.fetch_add(1, Ordering::Release);
        }
    }
    pub(crate) fn apply_redactions(&self, record: &mut ArchiveRecord) {
        if record.redacted || self.is_redacted(&record.reference()) {
            record.redacted = true;
            record.body.clear();
        }
    }
    pub(crate) fn redact(&self, conversation: ConversationKey, msgid: String) {
        // Privacy takes effect before the worker queue, including already returned UI rows.
        let key = (conversation, msgid);
        if !self.redactions.lock().unwrap().insert(key.clone()) {
            return;
        }
        // Unlike ordinary history, privacy controls must survive a full queue.
        // The deduplicated outbox is drained before queued work and flush barriers.
        self.pending_redactions.lock().unwrap().insert(key);
        if let Some(tx) = &self.tx
            && let Err(mpsc::TrySendError::Disconnected(_)) = tx.try_send(Task::ApplyRedactions)
        {
            self.available.store(false, Ordering::Release);
            self.status.lock().unwrap().error = Some(
                "History archive is unavailable; a message redaction could not be saved".into(),
            );
        }
    }
    /// A client whose queue holds `capacity` tasks and nothing consumes them;
    /// beyond that every submit fails like a full archive queue.
    #[cfg(test)]
    pub(crate) fn test_sink(capacity: usize) -> (Self, TestSink) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let client = Self {
            tx: Some(tx),
            available: Arc::new(AtomicBool::new(true)),
            ..Default::default()
        };
        (client, TestSink(rx))
    }
    pub(crate) fn reaction(
        &self,
        conversation: ConversationKey,
        msgid: String,
        actor: String,
        emoji: String,
        present: bool,
    ) -> bool {
        self.submit(Task::Event(JournalEvent::Reaction {
            conversation,
            msgid,
            actor,
            emoji,
            present,
        }))
    }
    pub(crate) fn search(&self, query: SearchQuery) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if !self.submit(Task::Search(generation, query)) {
            self.fail_query(generation);
        }
        generation
    }
    pub(crate) fn context(&self, reference: MessageRef, radius: usize) -> u64 {
        let generation = self.context_generation.fetch_add(1, Ordering::SeqCst) + 1;
        if !self.submit(Task::Context(generation, reference, radius.min(250))) {
            let message = self
                .status
                .lock()
                .unwrap()
                .error
                .clone()
                .unwrap_or_else(|| "History archive is unavailable".into());
            *self.context_failure.lock().unwrap() = Some(ArchiveReply::ContextError {
                generation,
                message,
            });
        }
        generation
    }
    fn fail_query(&self, generation: u64) {
        let message = self
            .status
            .lock()
            .unwrap()
            .error
            .clone()
            .unwrap_or_else(|| "History archive is unavailable".into());
        *self.query_failure.lock().unwrap() = Some(ArchiveReply::Error {
            generation,
            message,
        });
    }
    pub(crate) fn cancel_query(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
    pub(crate) fn cancel_context(&self) {
        self.context_generation.fetch_add(1, Ordering::SeqCst);
    }
    pub(crate) fn status(&self) -> ArchiveStatus {
        self.status.lock().unwrap().clone()
    }
    pub(crate) fn flush_barrier(&self, timeout: Duration) -> bool {
        if self.tx.is_none() {
            return true;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.submit(Task::Flush(tx)) && rx.recv_timeout(timeout).unwrap_or(false)
    }
}

#[derive(Default)]
pub(crate) struct ArchiveService {
    pub client: ArchiveClient,
    replies: Option<mpsc::Receiver<ArchiveReply>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ArchiveService {
    pub(crate) fn open(config_root: &Path) -> Self {
        Self::start(Some(config_root.to_path_buf()))
    }
    #[cfg(test)]
    fn memory() -> Self {
        Self::start(None)
    }
    fn start(root: Option<PathBuf>) -> Self {
        let (tx, rx) = mpsc::sync_channel(4096);
        let (reply_tx, reply_rx) = mpsc::channel();
        let client = ArchiveClient {
            tx: Some(tx),
            available: Arc::new(AtomicBool::new(true)),
            ..Default::default()
        };
        client.status.lock().unwrap().indexing = true;
        let worker_client = client.clone();
        let worker = std::thread::spawn(move || {
            if let Err(error) = run_archive(root, &rx, &reply_tx, &worker_client) {
                worker_client.available.store(false, Ordering::Release);
                let mut status = worker_client.status.lock().unwrap();
                status.error = Some(format!("History archive: {error}"));
                status.indexing = false;
                drop(status);
                for task in rx.try_iter() {
                    match task {
                        Task::Restore(token, ..) => {
                            let _ = reply_tx.send(ArchiveReply::RestoredError {
                                token,
                                message: "History archive is unavailable".into(),
                            });
                        }
                        Task::Flush(response) => {
                            let _ = response.send(false);
                        }
                        Task::Search(generation, _) => {
                            let _ = reply_tx.send(ArchiveReply::Error {
                                generation,
                                message: "History archive is unavailable".into(),
                            });
                        }
                        Task::Context(generation, ..) => {
                            let _ = reply_tx.send(ArchiveReply::ContextError {
                                generation,
                                message: "History archive is unavailable".into(),
                            });
                        }
                        _ => {}
                    }
                }
            }
        });
        Self {
            client,
            replies: Some(reply_rx),
            worker: Some(worker),
        }
    }
    pub(crate) fn poll(&self) -> Vec<ArchiveReply> {
        let mut replies = Vec::new();
        if let Some(error) = self.client.query_failure.lock().unwrap().take() {
            replies.push(error);
        }
        if let Some(error) = self.client.context_failure.lock().unwrap().take() {
            replies.push(error);
        }
        if let Some(rx) = &self.replies {
            replies.extend(rx.try_iter().take(32 - replies.len()));
        }
        for reply in &mut replies {
            match reply {
                ArchiveReply::Search { records, .. }
                | ArchiveReply::Context { records, .. }
                | ArchiveReply::Restored { records, .. } => {
                    for record in records {
                        self.client.apply_redactions(record);
                    }
                }
                _ => {}
            }
        }
        replies
    }
}

impl Drop for ArchiveService {
    fn drop(&mut self) {
        self.client.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let deadline = std::time::Instant::now() + Duration::from_millis(250);
            while !worker.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    super::logging::tighten_private_dir(path)?;
    Ok(())
}

fn private_append(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

const SCHEMA_VERSION: i64 = 2;
const MAX_ARCHIVE_LINE: usize = 1024 * 1024;
const MAX_JOURNAL_RECORD: usize = 64 * 1024 * 1024;
const JOURNAL_CHUNK_BYTES: usize = 256 * 1024;
const JOURNAL_RECORD_TOO_LARGE: &str =
    "A history entry exceeds the 64 MiB archive limit and was not saved";

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum JournalFrame {
    Chunk {
        version: u8,
        id: String,
        index: usize,
        parts: usize,
        total: usize,
        sha256: String,
        data: String,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JournalLine {
    Event(JournalEvent),
    Frame(JournalFrame),
}

enum JournalWriteError {
    TooLarge,
    Failed(String),
}
impl JournalWriteError {
    fn message(self) -> String {
        match self {
            Self::TooLarge => JOURNAL_RECORD_TOO_LARGE.into(),
            Self::Failed(message) => message,
        }
    }
}

struct ArchiveJournal {
    file: File,
    dir: PathBuf,
    name: String,
    digest: Sha256,
    previous: Vec<(String, String)>,
}
impl Write for ArchiveJournal {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(bytes)?;
        self.digest.update(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
impl ArchiveJournal {
    fn append(&mut self, event: &JournalEvent) -> Result<(), JournalWriteError> {
        struct Limited {
            bytes: Vec<u8>,
            exceeded: bool,
        }
        impl Write for Limited {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > MAX_JOURNAL_RECORD.saturating_sub(self.bytes.len()) {
                    self.exceeded = true;
                    return Err(std::io::Error::other(JOURNAL_RECORD_TOO_LARGE));
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut encoded = Limited {
            bytes: Vec::new(),
            exceeded: false,
        };
        if let Err(error) = serde_json::to_writer(&mut encoded, event) {
            return Err(if encoded.exceeded {
                JournalWriteError::TooLarge
            } else {
                JournalWriteError::Failed(error.to_string())
            });
        }
        let result = (|| -> std::io::Result<()> {
            if encoded.bytes.len() <= MAX_ARCHIVE_LINE {
                self.write_all(&encoded.bytes)?;
                return self.write_all(b"\n");
            }
            let id = new_record_id();
            let sha256 = line_digest(&encoded.bytes);
            let parts = encoded.bytes.len().div_ceil(JOURNAL_CHUNK_BYTES);
            for (index, bytes) in encoded.bytes.chunks(JOURNAL_CHUNK_BYTES).enumerate() {
                let frame = JournalFrame::Chunk {
                    version: 1,
                    id: id.clone(),
                    index,
                    parts,
                    total: encoded.bytes.len(),
                    sha256: sha256.clone(),
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                };
                serde_json::to_writer(&mut *self, &frame).map_err(std::io::Error::other)?;
                self.write_all(b"\n")?;
            }
            Ok(())
        })();
        result.map_err(|error| JournalWriteError::Failed(error.to_string()))
    }

    /// Rewrite every journal file without the bodies of the targeted records.
    /// A file that cannot be rewritten is left exactly as it was; the first
    /// such failure is returned after the remaining files were attempted. A
    /// failure to continue appending to the rewritten active journal is fatal.
    fn scrub(&mut self, db: &Connection, targets: &ScrubTargets) -> Result<(), ScrubError> {
        let mut failure = None;
        let mut rewritten = false;
        let names: Vec<String> = self
            .previous
            .iter()
            .map(|(name, _)| name.clone())
            .chain(std::iter::once(self.name.clone()))
            .collect();
        for name in names {
            let path = self.dir.join(&name);
            match scrub_journal_file(&path, targets) {
                Ok(None) => {}
                Ok(Some(digest)) => {
                    rewritten = true;
                    if name == self.name {
                        // The old descriptor now names the replaced inode.
                        self.file =
                            private_append(&path).map_err(|e| ScrubError::Fatal(e.to_string()))?;
                        self.digest = digest;
                    } else if let Some(entry) = self.previous.iter_mut().find(|e| e.0 == name) {
                        entry.1 = hex_digest(&digest);
                    }
                }
                Err(error) => {
                    tracing::warn!("Could not remove redacted text from {name}: {error}");
                    failure.get_or_insert(error);
                }
            }
        }
        if rewritten {
            self.sync(db).map_err(ScrubError::Fatal)?;
        }
        failure.map_or(Ok(()), |error| Err(ScrubError::Failed(error)))
    }

    fn sync(&mut self, db: &Connection) -> Result<(), String> {
        self.file
            .flush()
            .and_then(|_| self.file.sync_data())
            .map_err(|e| e.to_string())?;
        let mut sources = self.previous.clone();
        sources.push((
            self.name.clone(),
            self.digest
                .clone()
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        ));
        save_journal_manifest(db, &sources)
    }
}

enum ScrubError {
    /// The archive can no longer be trusted to append correctly.
    Fatal(String),
    /// Some journal could not be rewritten; it was left untouched.
    Failed(String),
}

fn hex_digest(digest: &Sha256) -> String {
    digest
        .clone()
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The records whose bodies a journal rewrite removes.
#[derive(Default)]
struct ScrubTargets {
    ids: HashSet<String>,
    msgids: HashMap<String, Vec<ConversationKey>>,
}
impl ScrubTargets {
    fn matches(&self, record: &ArchiveRecord) -> bool {
        self.ids.contains(&record.id)
            || record
                .msgid
                .as_ref()
                .and_then(|msgid| self.msgids.get(msgid))
                .is_some_and(|conversations| conversations.contains(&record.conversation))
    }
    fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.msgids.is_empty()
    }
    /// Also reach journal events written under ids that were merged into a
    /// targeted record before it acquired its server identity.
    fn follow_redirects(&mut self, db: &Connection) -> Result<(), String> {
        let mut frontier: Vec<String> = self.ids.iter().cloned().collect();
        for _ in 0..32 {
            let mut next = Vec::new();
            for survivor in &frontier {
                let mut stmt = db
                    .prepare("SELECT id FROM redirects WHERE survivor=?1")
                    .map_err(|e| e.to_string())?;
                let merged = stmt
                    .query_map([survivor], |r| r.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                for id in merged {
                    let id = id.map_err(|e| e.to_string())?;
                    if self.ids.insert(id.clone()) {
                        next.push(id);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(())
    }
    /// Targets for server redactions just applied. A redaction naming a
    /// message the archive never stored has nothing in any journal to remove.
    fn for_redactions(
        db: &Connection,
        redactions: &[(ConversationKey, String)],
    ) -> Result<Self, String> {
        let mut targets = Self::default();
        for (conversation, msgid) in redactions {
            let encoded = serde_json::to_string(conversation).map_err(|e| e.to_string())?;
            let mut stmt = db
                .prepare("SELECT id FROM records WHERE conversation=?1 AND msgid=?2")
                .map_err(|e| e.to_string())?;
            let ids = stmt
                .query_map(params![encoded, msgid], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            if !ids.is_empty() {
                targets.ids.extend(ids);
                targets
                    .msgids
                    .entry(msgid.clone())
                    .or_default()
                    .push(conversation.clone());
            }
        }
        targets.follow_redirects(db)?;
        Ok(targets)
    }
    /// Targets for every redaction the projection knows about.
    fn for_all_redactions(db: &Connection) -> Result<Self, String> {
        let mut targets = Self::default();
        let mut stmt = db
            .prepare("SELECT id FROM records WHERE deleted=1")
            .map_err(|e| e.to_string())?;
        for id in stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
        {
            targets.ids.insert(id.map_err(|e| e.to_string())?);
        }
        let mut stmt = db
            .prepare("SELECT conversation,msgid FROM tombstones")
            .map_err(|e| e.to_string())?;
        let tombstones = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        for (conversation, msgid) in tombstones {
            if let Ok(conversation) = serde_json::from_str::<ConversationKey>(&conversation) {
                targets.msgids.entry(msgid).or_default().push(conversation);
            }
        }
        targets.follow_redirects(db)?;
        Ok(targets)
    }
}

/// Byte patterns a journal line must contain to concern a scrub: every target
/// id and msgid in the form serde_json writes it. A line holding none of them
/// cannot be a targeted record, so it is never parsed. Matching uses a rolling
/// hash per pattern length, with a byte comparison to confirm each hit.
/// Patterns of one length, by rolling hash.
type NeedleTable = HashMap<u64, Vec<Vec<u8>>>;
struct NeedleSet {
    groups: Vec<(usize, u64, NeedleTable)>,
    /// An empty pattern would match everything; parse every line instead.
    match_all: bool,
}
impl NeedleSet {
    const BASE: u64 = 0x0000_0100_0000_01b3;
    fn hash(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0u64, |hash, byte| {
            hash.wrapping_mul(Self::BASE)
                .wrapping_add(u64::from(*byte) + 1)
        })
    }
    fn new(targets: &ScrubTargets) -> Self {
        let escaped = |text: &str| {
            let quoted = serde_json::to_string(text).unwrap_or_default();
            quoted
                .get(1..quoted.len().saturating_sub(1))
                .unwrap_or_default()
                .as_bytes()
                .to_vec()
        };
        let mut by_len: HashMap<usize, NeedleTable> = HashMap::new();
        let mut match_all = false;
        for text in targets.ids.iter().chain(targets.msgids.keys()) {
            let needle = escaped(text);
            if needle.is_empty() {
                match_all = true;
                continue;
            }
            let bucket = by_len.entry(needle.len()).or_default();
            let list = bucket.entry(Self::hash(&needle)).or_default();
            if !list.contains(&needle) {
                list.push(needle);
            }
        }
        let groups = by_len
            .into_iter()
            .map(|(len, table)| {
                let pow = (1..len).fold(1u64, |pow, _| pow.wrapping_mul(Self::BASE));
                (len, pow, table)
            })
            .collect();
        Self { groups, match_all }
    }
    fn found_in(&self, haystack: &[u8]) -> bool {
        if self.match_all {
            return true;
        }
        for (len, pow, table) in &self.groups {
            let len = *len;
            if haystack.len() < len {
                continue;
            }
            let mut hash = Self::hash(&haystack[..len]);
            let mut at = 0;
            loop {
                if table
                    .get(&hash)
                    .is_some_and(|list| list.iter().any(|n| n[..] == haystack[at..at + len]))
                {
                    return true;
                }
                if at + len == haystack.len() {
                    break;
                }
                hash = hash
                    .wrapping_sub((u64::from(haystack[at]) + 1).wrapping_mul(*pow))
                    .wrapping_mul(Self::BASE)
                    .wrapping_add(u64::from(haystack[at + len]) + 1);
                at += 1;
            }
        }
        false
    }
}

#[cfg(test)]
thread_local! {
    /// Journal lines (or reassembled records) the scrubber parsed as JSON.
    static SCRUB_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
fn note_scrub_parse() {
    #[cfg(test)]
    SCRUB_PARSES.with(|count| count.set(count.get() + 1));
}

struct HashingWriter<W: Write> {
    inner: W,
    digest: Sha256,
}
impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.digest.update(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct ScrubLine {
    bytes: Vec<u8>,
    /// Longer than the record limit: `bytes` is only its beginning and the
    /// rest, up to the newline, is still unread.
    oversized: bool,
}
fn read_scrub_line(reader: &mut impl BufRead, limit: usize) -> std::io::Result<Option<ScrubLine>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((!bytes.is_empty()).then_some(ScrubLine {
                bytes,
                oversized: false,
            }));
        }
        let take = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if bytes.len().saturating_add(take) > limit {
            return Ok(Some(ScrubLine {
                bytes,
                oversized: true,
            }));
        }
        let ends = available[take - 1] == b'\n';
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if ends {
            return Ok(Some(ScrubLine {
                bytes,
                oversized: false,
            }));
        }
    }
}
fn copy_rest_of_line(reader: &mut impl BufRead, out: &mut dyn Write) -> std::io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        let take = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        let ends = available[take - 1] == b'\n';
        out.write_all(&available[..take])?;
        reader.consume(take);
        if ends {
            return Ok(());
        }
    }
}

/// Copy one journal to `out`, replacing each targeted Upsert (single-line or
/// reassembled from checked chunk frames) with the same record carrying no
/// body. Every other byte, including lines this build cannot parse, is copied
/// unchanged. Returns whether anything was replaced; with `stop_at_first` it
/// returns as soon as the first replacement is found, without finishing `out`.
fn rewrite_journal(
    path: &Path,
    targets: &ScrubTargets,
    out: &mut dyn Write,
    stop_at_first: bool,
) -> Result<bool, String> {
    const CHUNK_PREFIX: &[u8] = b"{\"event\":\"chunk\"";
    let needles = NeedleSet::new(targets);
    struct Run {
        id: String,
        parts: usize,
        total: usize,
        sha256: String,
        next: usize,
        bytes: Vec<u8>,
        raw: Vec<Vec<u8>>,
    }
    fn flush_run(run: &mut Option<Run>, out: &mut dyn Write) -> Result<(), String> {
        if let Some(run) = run.take() {
            for line in run.raw {
                out.write_all(&line).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
    fn needs_scrub(targets: &ScrubTargets, record: &ArchiveRecord) -> bool {
        targets.matches(record) && (!record.body.is_empty() || !record.redacted)
    }
    fn scrubbed(record: &ArchiveRecord) -> Result<Vec<u8>, String> {
        let mut record = record.clone();
        record.body.clear();
        record.redacted = true;
        let mut line = serde_json::to_vec(&JournalEvent::Upsert {
            record: Box::new(record),
        })
        .map_err(|e| e.to_string())?;
        line.push(b'\n');
        Ok(line)
    }
    let io = |e: std::io::Error| e.to_string();
    let mut reader = BufReader::new(File::open(path).map_err(io)?);
    let mut run: Option<Run> = None;
    let mut changed = false;
    while let Some(line) = read_scrub_line(&mut reader, MAX_JOURNAL_RECORD).map_err(io)? {
        let bytes = line.bytes;
        if line.oversized {
            flush_run(&mut run, out)?;
            out.write_all(&bytes).map_err(io)?;
            copy_rest_of_line(&mut reader, out).map_err(io)?;
            continue;
        }
        // Only chunk frames can extend a run, and only a line holding a target
        // can be one to scrub; every other line is carried over unparsed.
        if !bytes.starts_with(CHUNK_PREFIX) && !needles.found_in(&bytes) {
            flush_run(&mut run, out)?;
            out.write_all(&bytes).map_err(io)?;
            continue;
        }
        note_scrub_parse();
        match serde_json::from_slice::<JournalLine>(&bytes) {
            Ok(JournalLine::Event(event)) => {
                flush_run(&mut run, out)?;
                match &event {
                    JournalEvent::Upsert { record } if needs_scrub(targets, record) => {
                        changed = true;
                        if stop_at_first {
                            return Ok(true);
                        }
                        out.write_all(&scrubbed(record)?).map_err(io)?;
                    }
                    _ => out.write_all(&bytes).map_err(io)?,
                }
            }
            Ok(JournalLine::Frame(JournalFrame::Chunk {
                version,
                id,
                index,
                parts,
                total,
                sha256,
                data,
            })) => {
                // The same frame checks replay applies; anything that fails
                // them is not a record and is carried over untouched.
                let valid = version == 1
                    && total > MAX_ARCHIVE_LINE
                    && total <= MAX_JOURNAL_RECORD
                    && parts == total.div_ceil(JOURNAL_CHUNK_BYTES)
                    && index < parts
                    && id.len() == 32
                    && id.bytes().all(|b| b.is_ascii_hexdigit())
                    && sha256.len() == 64
                    && sha256.bytes().all(|b| b.is_ascii_hexdigit())
                    && data.len() <= JOURNAL_CHUNK_BYTES.div_ceil(3) * 4;
                let decoded = valid
                    .then(|| base64::engine::general_purpose::STANDARD.decode(&data).ok())
                    .flatten()
                    .filter(|decoded| {
                        decoded.len()
                            == (total - index * JOURNAL_CHUNK_BYTES).min(JOURNAL_CHUNK_BYTES)
                    });
                let Some(decoded) = decoded else {
                    flush_run(&mut run, out)?;
                    out.write_all(&bytes).map_err(io)?;
                    continue;
                };
                if index == 0 {
                    flush_run(&mut run, out)?;
                    run = Some(Run {
                        id: id.clone(),
                        parts,
                        total,
                        sha256: sha256.clone(),
                        next: 0,
                        bytes: Vec::new(),
                        raw: Vec::new(),
                    });
                }
                let continues = run.as_ref().is_some_and(|run| {
                    run.id == id
                        && run.parts == parts
                        && run.total == total
                        && run.sha256 == sha256
                        && run.next == index
                });
                if !continues {
                    flush_run(&mut run, out)?;
                    out.write_all(&bytes).map_err(io)?;
                    continue;
                }
                let current = run.as_mut().expect("run continues");
                current.bytes.extend_from_slice(&decoded);
                current.raw.push(bytes);
                current.next += 1;
                if current.next == parts {
                    let complete = run.take().expect("run completes");
                    if complete.bytes.len() == total
                        && line_digest(&complete.bytes) == sha256
                        && needles.found_in(&complete.bytes)
                        && {
                            note_scrub_parse();
                            true
                        }
                        && let Ok(JournalEvent::Upsert { record }) =
                            serde_json::from_slice::<JournalEvent>(&complete.bytes)
                        && needs_scrub(targets, &record)
                    {
                        changed = true;
                        if stop_at_first {
                            return Ok(true);
                        }
                        out.write_all(&scrubbed(&record)?).map_err(io)?;
                    } else {
                        for line in complete.raw {
                            out.write_all(&line).map_err(io)?;
                        }
                    }
                }
            }
            Err(_) => {
                flush_run(&mut run, out)?;
                out.write_all(&bytes).map_err(io)?;
            }
        }
    }
    flush_run(&mut run, out)?;
    Ok(changed)
}

/// Atomically replace one journal with its scrubbed copy. Returns the digest
/// state of the new content, or `None` when nothing in the file was targeted.
/// The original is only replaced by a complete, synced temporary file; on any
/// error it is untouched and the temporary file is removed.
fn scrub_journal_file(path: &Path, targets: &ScrubTargets) -> Result<Option<Sha256>, String> {
    if targets.is_empty() || !rewrite_journal(path, targets, &mut std::io::sink(), true)? {
        return Ok(None);
    }
    let temporary = path.with_extension("jsonl.scrub");
    let result = (|| -> Result<Sha256, String> {
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&temporary).map_err(|e| e.to_string())?;
        let mut out = HashingWriter {
            inner: BufWriter::new(file),
            digest: Sha256::new(),
        };
        rewrite_journal(path, targets, &mut out, false)?;
        let HashingWriter { inner, digest } = out;
        let file = inner.into_inner().map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        fs::rename(&temporary, path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        if let Some(dir) = path.parent() {
            File::open(dir)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| e.to_string())?;
        }
        Ok(digest)
    })();
    match result {
        Ok(digest) => Ok(Some(digest)),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// Interrupted rewrites leave a temporary copy beside the intact original.
/// Only `.jsonl` files are journals, so it is never replayed; remove it.
fn remove_stale_scrub_files(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            if path.extension().is_some_and(|ext| ext == "scrub") {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn journal_manifest(sources: &[(String, String)]) -> Result<String, String> {
    serde_json::to_vec(sources)
        .map(|bytes| line_digest(&bytes))
        .map_err(|e| e.to_string())
}
fn save_journal_manifest(db: &Connection, sources: &[(String, String)]) -> Result<(), String> {
    db.execute(
        "INSERT OR REPLACE INTO journal_manifest VALUES(1,?1)",
        [journal_manifest(sources)?],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

fn clear_projection(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
        DELETE FROM record_search; DELETE FROM records; DELETE FROM tombstones;
        DELETE FROM reactions; DELETE FROM redirects; DELETE FROM sources;
        DELETE FROM legacy_sources; DELETE FROM text_copies; DELETE FROM journal_manifest;
        COMMIT;",
    )
    .map_err(|e| e.to_string())
}

struct DigestReader {
    file: File,
    digest: Sha256,
}
impl Read for DigestReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let read = self.file.read(bytes)?;
        self.digest.update(&bytes[..read]);
        Ok(read)
    }
}

fn replay_journal(path: &Path, db: &Connection, client: &ArchiveClient) -> Result<String, String> {
    struct Assembly {
        id: String,
        parts: usize,
        total: usize,
        sha256: String,
        next: usize,
        bytes: Vec<u8>,
    }
    let mut assembly: Option<Assembly> = None;
    let mut reader = BufReader::new(DigestReader {
        file: File::open(path).map_err(|e| e.to_string())?,
        digest: Sha256::new(),
    });
    let warn = || {
        client.status.lock().unwrap().error =
            Some("An incomplete or invalid history journal record was skipped".into());
    };
    // The larger legacy-line bound reads pre-framing journals produced by
    // earlier builds, while all new large records use small checked frames.
    while let Some((bytes, _, oversized)) =
        bounded_line_limit(&mut reader, MAX_JOURNAL_RECORD).map_err(|e| e.to_string())?
    {
        if oversized {
            assembly = None;
            warn();
            continue;
        }
        match serde_json::from_slice::<JournalLine>(&bytes) {
            Ok(JournalLine::Event(event)) => {
                if assembly.take().is_some() {
                    warn();
                }
                project(db, &event)?;
            }
            Ok(JournalLine::Frame(JournalFrame::Chunk {
                version,
                id,
                index,
                parts,
                total,
                sha256,
                data,
            })) => {
                let valid = version == 1
                    && total > MAX_ARCHIVE_LINE
                    && total <= MAX_JOURNAL_RECORD
                    && parts == total.div_ceil(JOURNAL_CHUNK_BYTES)
                    && index < parts
                    && id.len() == 32
                    && id.bytes().all(|b| b.is_ascii_hexdigit())
                    && sha256.len() == 64
                    && sha256.bytes().all(|b| b.is_ascii_hexdigit())
                    && data.len() <= JOURNAL_CHUNK_BYTES.div_ceil(3) * 4;
                if !valid {
                    assembly = None;
                    warn();
                    continue;
                }
                let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(data) else {
                    assembly = None;
                    warn();
                    continue;
                };
                if decoded.len() != (total - index * JOURNAL_CHUNK_BYTES).min(JOURNAL_CHUNK_BYTES) {
                    assembly = None;
                    warn();
                    continue;
                }
                if index == 0 {
                    if assembly.take().is_some() {
                        warn();
                    }
                    assembly = Some(Assembly {
                        id: id.clone(),
                        parts,
                        total,
                        sha256: sha256.clone(),
                        next: 0,
                        bytes: Vec::new(),
                    });
                }
                let Some(current) = assembly.as_mut() else {
                    warn();
                    continue;
                };
                if current.id != id
                    || current.parts != parts
                    || current.total != total
                    || current.sha256 != sha256
                    || current.next != index
                {
                    assembly = None;
                    warn();
                    continue;
                }
                current.bytes.extend_from_slice(&decoded);
                current.next += 1;
                if current.next == parts {
                    let complete = assembly.take().unwrap();
                    if complete.bytes.len() == total && line_digest(&complete.bytes) == sha256 {
                        match serde_json::from_slice::<JournalEvent>(&complete.bytes) {
                            Ok(event) => project(db, &event)?,
                            Err(_) => warn(),
                        }
                    } else {
                        warn();
                    }
                }
            }
            Err(_) => {
                assembly = None;
                warn();
            }
        }
    }
    if assembly.is_some() {
        warn();
    }
    Ok(reader
        .into_inner()
        .digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.busy_timeout(Duration::from_secs(2))?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        // This database is a cache. The journal and original text logs remain authoritative.
        db.execute_batch("DROP TABLE IF EXISTS record_search; DROP TABLE IF EXISTS records;
          DROP TABLE IF EXISTS tombstones; DROP TABLE IF EXISTS reactions; DROP TABLE IF EXISTS redirects;
          DROP TABLE IF EXISTS sources; DROP TABLE IF EXISTS legacy_sources; DROP TABLE IF EXISTS text_copies; DROP TABLE IF EXISTS journal_manifest;")?;
    }
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA secure_delete=ON;
        CREATE TABLE IF NOT EXISTS records(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,
          conversation TEXT NOT NULL, network TEXT NOT NULL, target TEXT NOT NULL, sender_fold TEXT NOT NULL,
          body_fold TEXT NOT NULL, occurred INTEGER, msgid TEXT, data TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0);
        CREATE INDEX IF NOT EXISTS conversation_order ON records(conversation,occurred);
        CREATE UNIQUE INDEX IF NOT EXISTS server_identity ON records(conversation,msgid) WHERE msgid IS NOT NULL;
        CREATE VIRTUAL TABLE IF NOT EXISTS record_search USING fts5(body_fold,sender_fold,tokenize='trigram');
        CREATE TABLE IF NOT EXISTS tombstones(conversation TEXT NOT NULL,msgid TEXT NOT NULL,PRIMARY KEY(conversation,msgid));
        CREATE TABLE IF NOT EXISTS reactions(conversation TEXT NOT NULL,msgid TEXT NOT NULL,actor TEXT NOT NULL,emoji TEXT NOT NULL,present INTEGER NOT NULL DEFAULT 1,
          PRIMARY KEY(conversation,msgid,actor,emoji));
        CREATE TABLE IF NOT EXISTS redirects(id TEXT PRIMARY KEY,survivor TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS sources(path TEXT PRIMARY KEY,fingerprint TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS legacy_sources(id TEXT PRIMARY KEY,path TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS text_copies(generation TEXT NOT NULL,offset INTEGER NOT NULL,len INTEGER NOT NULL,
          digest TEXT NOT NULL,id TEXT NOT NULL,PRIMARY KEY(generation,offset,len,digest,id));
        CREATE TABLE IF NOT EXISTS journal_manifest(id INTEGER PRIMARY KEY,digest TEXT NOT NULL);
        PRAGMA user_version=2;")
}

fn delete_record(db: &Connection, id: &str) -> Result<(), String> {
    db.execute(
        "DELETE FROM record_search WHERE rowid IN (SELECT rowid FROM records WHERE id=?1)",
        [id],
    )
    .map_err(|e| e.to_string())?;
    db.execute("DELETE FROM records WHERE id=?1", [id])
        .map_err(|e| e.to_string())?;
    db.execute("DELETE FROM legacy_sources WHERE id=?1", [id])
        .map_err(|e| e.to_string())?;
    db.execute("DELETE FROM text_copies WHERE id=?1", [id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn load_record(db: &Connection, id: &str) -> Result<Option<ArchiveRecord>, String> {
    let data: Option<String> = db
        .query_row("SELECT data FROM records WHERE id=?1", [id], |r| r.get(0))
        .optional()
        .map_err(|e| e.to_string())?;
    data.map(|data| serde_json::from_str(&data).map_err(|e| e.to_string()))
        .transpose()
}

/// Physical text-log position (generation, offset, len, digest) of a line.
type SpanKey = (String, u64, u64, String);

fn project(db: &Connection, event: &JournalEvent) -> Result<(), String> {
    project_collecting(db, event).map(|_| ())
}
/// Project an event and publish the text-log lines of any record it leaves
/// redacted, so scrollback rebuilt from those logs can omit them.
fn project_noting(
    db: &Connection,
    event: &JournalEvent,
    client: &ArchiveClient,
) -> Result<(), String> {
    client.note_redacted_lines(project_collecting(db, event)?);
    Ok(())
}
fn project_collecting(db: &Connection, event: &JournalEvent) -> Result<Vec<SpanKey>, String> {
    db.execute_batch("SAVEPOINT archive_project")
        .map_err(|e| e.to_string())?;
    match project_inner(db, event) {
        Ok(spans) => db
            .execute_batch("RELEASE archive_project")
            .map(|_| spans)
            .map_err(|e| e.to_string()),
        Err(error) => {
            let _ = db.execute_batch("ROLLBACK TO archive_project; RELEASE archive_project");
            Err(error)
        }
    }
}
/// Every physical text-log copy recorded for one archive record.
fn physical_spans(
    db: &Connection,
    id: &str,
    own: Option<&LogSpan>,
) -> Result<Vec<SpanKey>, String> {
    let mut spans: Vec<SpanKey> = own
        .map(|span| {
            (
                span.generation.clone(),
                span.offset,
                span.len,
                span.digest.clone(),
            )
        })
        .into_iter()
        .collect();
    let mut stmt = db
        .prepare("SELECT generation,offset,len,digest FROM text_copies WHERE id=?1")
        .map_err(|e| e.to_string())?;
    let copies = stmt
        .query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for copy in copies {
        spans.push(copy.map_err(|e| e.to_string())?);
    }
    Ok(spans)
}
/// Text-log lines of every redacted record, for a freshly replayed projection.
fn note_all_redacted_lines(db: &Connection, client: &ArchiveClient) -> Result<(), String> {
    let mut stmt = db
        .prepare(
            "SELECT c.generation,c.offset,c.len,c.digest FROM text_copies c
             JOIN records r ON r.id=c.id WHERE r.deleted=1",
        )
        .map_err(|e| e.to_string())?;
    let spans = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    client.note_redacted_lines(spans);
    Ok(())
}
fn project_inner(db: &Connection, event: &JournalEvent) -> Result<Vec<SpanKey>, String> {
    let mut spans = Vec::new();
    match event {
        JournalEvent::Upsert { record } => {
            let mut record = record.as_ref().clone();
            let conversation =
                serde_json::to_string(&record.conversation).map_err(|e| e.to_string())?;
            let existing: Option<String> = if let Some(msgid) = &record.msgid {
                db.query_row(
                    "SELECT id FROM records WHERE conversation=?1 AND msgid=?2",
                    params![conversation, msgid],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?
            } else {
                None
            };
            if let Some(existing) = existing
                && existing != record.id
            {
                if let Some(previous) = load_record(db, &record.id)? {
                    record.received_at = previous.received_at.or(record.received_at);
                    record.text_span = record.text_span.or(previous.text_span);
                }
                // A local row may already exist before its echo acquires a server msgid.
                // Redirect all references and transfer physical-copy provenance before deletion.
                db.execute(
                    "UPDATE redirects SET survivor=?1 WHERE survivor=?2",
                    params![existing, record.id],
                )
                .map_err(|e| e.to_string())?;
                db.execute("INSERT OR IGNORE INTO text_copies SELECT generation,offset,len,digest,?1 FROM text_copies WHERE id=?2",params![existing,record.id]).map_err(|e|e.to_string())?;
                delete_record(db, &record.id)?;
                db.execute(
                    "INSERT OR REPLACE INTO redirects VALUES(?1,?2)",
                    params![record.id, existing],
                )
                .map_err(|e| e.to_string())?;
                record.id = existing;
            }
            if let Some(previous) = load_record(db, &record.id)? {
                record.received_at = previous.received_at.or(record.received_at);
                record.text_span = record.text_span.or(previous.text_span);
                record.redacted |= previous.redacted;
            }
            if let Some(span) = &record.text_span {
                let mut stmt = db.prepare("SELECT c.id,r.data FROM text_copies c JOIN records r ON r.id=c.id WHERE c.generation=?1 AND c.offset=?2 AND c.len=?3 AND c.digest=?4").map_err(|e|e.to_string())?;
                let copies: Vec<(String, String)> = stmt
                    .query_map(
                        params![
                            span.generation,
                            span.offset as i64,
                            span.len as i64,
                            span.digest
                        ],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .map_err(|e| e.to_string())?
                    .collect::<Result<_, _>>()
                    .map_err(|e| e.to_string())?;
                for (id, data) in copies {
                    let copy: ArchiveRecord =
                        serde_json::from_str(&data).map_err(|e| e.to_string())?;
                    if record.legacy && !copy.legacy {
                        return Ok(Vec::new());
                    }
                    if !record.legacy && copy.legacy {
                        delete_record(db, &id)?;
                    }
                }
            }
            let tombstone: bool = if let Some(msgid) = &record.msgid {
                db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tombstones WHERE conversation=?1 AND msgid=?2)",
                    params![conversation, msgid],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?
            } else {
                false
            };
            if tombstone || record.redacted {
                record.redacted = true;
                record.body.clear();
            }
            if let Some(msgid) = &record.msgid {
                for group in &record.reactions {
                    for actor in &group.actors {
                        db.execute(
                            "INSERT OR IGNORE INTO reactions VALUES(?1,?2,?3,?4,1)",
                            params![conversation, msgid, actor, group.emoji],
                        )
                        .map_err(|e| e.to_string())?;
                    }
                }
            }
            let encoded = serde_json::to_string(&record).map_err(|e| e.to_string())?;
            db.execute("INSERT INTO records(id,conversation,network,target,sender_fold,body_fold,occurred,msgid,data,deleted)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                ON CONFLICT(id) DO UPDATE SET conversation=excluded.conversation,network=excluded.network,target=excluded.target,sender_fold=excluded.sender_fold,body_fold=excluded.body_fold,
                occurred=excluded.occurred,msgid=excluded.msgid,data=excluded.data,deleted=excluded.deleted",
                params![record.id,conversation,record.conversation.network.storage_key(),record.conversation.target,
                    record.sender.to_lowercase(),record.body.to_lowercase(),record.occurred_at(),record.msgid,encoded,record.redacted]).map_err(|e|e.to_string())?;
            if let Some(span) = &record.text_span {
                db.execute(
                    "INSERT OR IGNORE INTO text_copies VALUES(?1,?2,?3,?4,?5)",
                    params![
                        span.generation,
                        span.offset as i64,
                        span.len as i64,
                        span.digest,
                        record.id
                    ],
                )
                .map_err(|e| e.to_string())?;
            }
            let rowid: i64 = db
                .query_row(
                    "SELECT rowid FROM records WHERE id=?1",
                    [&record.id],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            db.execute("DELETE FROM record_search WHERE rowid=?1", [rowid])
                .map_err(|e| e.to_string())?;
            if !record.redacted {
                db.execute(
                    "INSERT INTO record_search(rowid,body_fold,sender_fold) VALUES(?1,?2,?3)",
                    params![
                        rowid,
                        record.body.to_lowercase(),
                        record.sender.to_lowercase()
                    ],
                )
                .map_err(|e| e.to_string())?;
            }
            if record.redacted {
                spans = physical_spans(db, &record.id, record.text_span.as_ref())?;
            }
        }
        JournalEvent::Redact {
            conversation,
            msgid,
        } => {
            let conversation = serde_json::to_string(conversation).map_err(|e| e.to_string())?;
            db.execute(
                "INSERT OR IGNORE INTO tombstones VALUES(?1,?2)",
                params![conversation, msgid],
            )
            .map_err(|e| e.to_string())?;
            let found: Option<(i64, String)> = db
                .query_row(
                    "SELECT rowid,data FROM records WHERE conversation=?1 AND msgid=?2",
                    params![conversation, msgid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            if let Some((rowid, data)) = found {
                let mut record: ArchiveRecord =
                    serde_json::from_str(&data).map_err(|e| e.to_string())?;
                record.redacted = true;
                record.body.clear();
                db.execute(
                    "UPDATE records SET body_fold='',data=?1,deleted=1 WHERE rowid=?2",
                    params![
                        serde_json::to_string(&record).map_err(|e| e.to_string())?,
                        rowid
                    ],
                )
                .map_err(|e| e.to_string())?;
                db.execute("DELETE FROM record_search WHERE rowid=?1", [rowid])
                    .map_err(|e| e.to_string())?;
                spans = physical_spans(db, &record.id, record.text_span.as_ref())?;
            }
        }
        JournalEvent::Reaction {
            conversation,
            msgid,
            actor,
            emoji,
            present,
        } => {
            let conversation = serde_json::to_string(conversation).map_err(|e| e.to_string())?;
            db.execute(
                "INSERT OR REPLACE INTO reactions VALUES(?1,?2,?3,?4,?5)",
                params![conversation, msgid, actor, emoji, present],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(spans)
}

fn hydrate_reactions(db: &Connection, record: &mut ArchiveRecord) -> Result<(), String> {
    let Some(msgid) = &record.msgid else {
        return Ok(());
    };
    let conversation = serde_json::to_string(&record.conversation).map_err(|e| e.to_string())?;
    let mut stmt = db.prepare("SELECT actor,emoji FROM reactions WHERE conversation=?1 AND msgid=?2 AND present=1 ORDER BY emoji,actor").map_err(|e|e.to_string())?;
    let entries = stmt
        .query_map(params![conversation, msgid], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    record.reactions.clear();
    for entry in entries {
        let (actor, emoji) = entry.map_err(|e| e.to_string())?;
        if let Some(group) = record.reactions.iter_mut().find(|g| g.emoji == emoji) {
            group.actors.push(actor);
        } else {
            record.reactions.push(super::message_ui::ReactionGroup {
                emoji,
                actors: vec![actor],
            });
        }
    }
    Ok(())
}

struct QueryProgress<'a>(&'a mut Connection);
impl<'a> QueryProgress<'a> {
    fn new(
        db: &'a mut Connection,
        client: &ArchiveClient,
        generation: Option<u64>,
    ) -> Result<Self, String> {
        Self::with_counter(db, client, generation, client.generation.clone())
    }
    fn context(
        db: &'a mut Connection,
        client: &ArchiveClient,
        generation: u64,
    ) -> Result<Self, String> {
        Self::with_counter(
            db,
            client,
            Some(generation),
            client.context_generation.clone(),
        )
    }
    fn with_counter(
        db: &'a mut Connection,
        client: &ArchiveClient,
        generation: Option<u64>,
        counter: Arc<AtomicU64>,
    ) -> Result<Self, String> {
        let stop = client.stop.clone();
        db.progress_handler(
            1000,
            Some(move || {
                stop.load(Ordering::Relaxed)
                    || generation.is_some_and(|g| counter.load(Ordering::Relaxed) != g)
            }),
        )
        .map_err(|e| e.to_string())?;
        Ok(Self(db))
    }
}
impl Drop for QueryProgress<'_> {
    fn drop(&mut self) {
        let _ = self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

fn execute_search(
    db: &mut Connection,
    query: &SearchQuery,
    cancel: &ArchiveClient,
    generation: u64,
) -> Result<(Vec<ArchiveRecord>, Option<SearchCursor>), String> {
    let progress = QueryProgress::new(db, cancel, Some(generation))?;
    let db = &*progress.0;
    let text = query.text.to_lowercase();
    let use_fts = text.chars().count() >= 3;
    let mut sql = String::from(
        "SELECT r.data,coalesce(r.occurred,-9223372036854775808),r.rowid FROM records r ",
    );
    if use_fts {
        sql.push_str("JOIN record_search ON record_search.rowid=r.rowid ");
    }
    sql.push_str("WHERE r.deleted=0 AND (?1='' OR r.network=?1) AND (?2='' OR r.target=?2)
       AND (?3='' OR r.sender_fold=?3) AND ((?4 IS NULL AND ?5 IS NULL) OR
       (r.occurred IS NOT NULL AND (?4 IS NULL OR r.occurred>=?4) AND (?5 IS NULL OR r.occurred<?5)) OR (?6 AND r.occurred IS NULL)) ");
    if use_fts {
        sql.push_str("AND record_search MATCH ?7 ");
    } else {
        sql.push_str("AND (instr(r.body_fold,?7)>0 OR instr(r.sender_fold,?7)>0) ");
    }
    sql.push_str("AND (?9='' OR instr(lower(r.network),?9)>0) AND (?8 IS NULL OR (coalesce(r.occurred,-9223372036854775808),r.rowid)<(?8,?10))
       ORDER BY coalesce(r.occurred,-9223372036854775808) DESC,r.rowid DESC LIMIT 101");
    let needle = if use_fts {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    };
    let mut stmt = db.prepare(&sql).map_err(|e| e.to_string())?;
    let network = query
        .network
        .as_ref()
        .map(LogicalNetworkKey::storage_key)
        .unwrap_or_default();
    let mut rows = stmt
        .query(params![
            network,
            query.target.as_deref().unwrap_or(""),
            query.sender.to_lowercase(),
            query.from,
            query.until,
            query.include_undated,
            needle,
            query.after.map(|(occurred, _)| occurred),
            query.network_pattern.to_lowercase(),
            query.after.map_or(0, |(_, rowid)| rowid)
        ])
        .map_err(|e| e.to_string())?;
    let mut found = Vec::new();
    let mut cursors = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if cancel.generation.load(Ordering::SeqCst) != generation {
            return Ok((Vec::new(), None));
        }
        let data: String = row.get(0).map_err(|e| e.to_string())?;
        let mut record = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        hydrate_reactions(db, &mut record)?;
        found.push(record);
        cursors.push((
            row.get(1).map_err(|e| e.to_string())?,
            row.get(2).map_err(|e| e.to_string())?,
        ));
    }
    let more = found.len() > 100;
    found.truncate(100);
    let next = if more { cursors.get(99).copied() } else { None };
    Ok((found, next))
}

fn execute_context(
    db: &Connection,
    reference: &MessageRef,
    radius: usize,
) -> Result<Vec<ArchiveRecord>, String> {
    let conversation = serde_json::to_string(&reference.conversation).map_err(|e| e.to_string())?;
    let id = reference.archive_id.as_deref().unwrap_or("");
    let survivor: Option<String> = db
        .query_row("WITH RECURSIVE chain(id,depth) AS (SELECT ?1,0 UNION ALL SELECT survivor,depth+1 FROM redirects JOIN chain ON redirects.id=chain.id WHERE depth<32) SELECT id FROM chain ORDER BY depth DESC LIMIT 1", [id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?;
    let center: Option<(i64,Option<i64>)> = db
        .query_row(
            "SELECT rowid,occurred FROM records WHERE conversation=?1 AND (id=?2 OR (?3!='' AND msgid=?3))",
            params![
                conversation,
                survivor.as_deref().unwrap_or(id),
                reference.server_msgid.as_deref().unwrap_or("")
            ],
            |r| Ok((r.get(0)?,r.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((center, occurred)) = center else {
        return Ok(Vec::new());
    };
    let mut stmt=db.prepare("SELECT rowid,data FROM (SELECT rowid,data FROM records WHERE conversation=?1
       AND (coalesce(occurred,-9223372036854775808),rowid)<=(?5,?2) ORDER BY coalesce(occurred,-9223372036854775808) DESC,rowid DESC LIMIT ?3)
       UNION ALL SELECT rowid,data FROM (SELECT rowid,data FROM records WHERE conversation=?1
       AND (coalesce(occurred,-9223372036854775808),rowid)>(?5,?2) ORDER BY coalesce(occurred,-9223372036854775808),rowid LIMIT ?4)").map_err(|e|e.to_string())?;
    let rows = stmt
        .query_map(
            params![
                conversation,
                center,
                (radius + 1) as i64,
                radius as i64,
                occurred.unwrap_or(i64::MIN)
            ],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .map_err(|e| e.to_string())?;
    let mut found = Vec::new();
    for row in rows {
        let (sequence, data) = row.map_err(|e| e.to_string())?;
        let mut record = serde_json::from_str::<ArchiveRecord>(&data).map_err(|e| e.to_string())?;
        hydrate_reactions(db, &mut record)?;
        found.push((sequence, record));
    }
    found.sort_by(|a, b| {
        a.1.occurred_at()
            .cmp(&b.1.occurred_at())
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(found.into_iter().map(|(_, record)| record).collect())
}

fn process_task(
    task: Task,
    db: &mut Connection,
    journal: &mut Option<ArchiveJournal>,
    reply: &mpsc::Sender<ArchiveReply>,
    client: &ArchiveClient,
) -> Result<(), String> {
    drain_redactions(db, journal, client)?;
    match task {
        Task::ApplyRedactions => {}
        #[cfg(test)]
        Task::Pause(entered, release) => {
            let _ = entered.send(());
            let _ = release.recv();
        }
        Task::Restore(token, reference, radius) => {
            let result = (|| {
                let progress = QueryProgress::new(db, client, None)?;
                execute_context(progress.0, &reference, radius)
            })();
            let response = match result {
                Ok(records) => ArchiveReply::Restored {
                    token,
                    reference,
                    records,
                },
                Err(message) => ArchiveReply::RestoredError { token, message },
            };
            let _ = reply.send(response);
        }
        Task::Event(mut event) => {
            withhold_redacted_body(db, &mut event)?;
            if let Some(file) = journal {
                match file.append(&event) {
                    Ok(()) => {}
                    Err(JournalWriteError::TooLarge) => {
                        client.status.lock().unwrap().error = Some(JOURNAL_RECORD_TOO_LARGE.into());
                        return Ok(());
                    }
                    Err(error) => return Err(error.message()),
                }
            }
            project_noting(db, &event, client)?;
            // Exact count is refreshed at barriers, avoiding a table scan per incoming row.
        }
        Task::Flush(response) => {
            client.status.lock().unwrap().records =
                db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
                    .map_err(|e| e.to_string())? as usize;
            let ok = journal.as_mut().is_none_or(|file| file.sync(db).is_ok());
            let _ = response.send(ok);
        }
        Task::Search(generation, query) => {
            if client.generation.load(Ordering::SeqCst) == generation {
                match execute_search(db, &query, client, generation) {
                    Ok((records, next)) => {
                        let _ = reply.send(ArchiveReply::Search {
                            generation,
                            records,
                            next,
                        });
                    }
                    Err(message) if client.generation.load(Ordering::SeqCst) == generation => {
                        let _ = reply.send(ArchiveReply::Error {
                            generation,
                            message,
                        });
                    }
                    Err(_) => {}
                }
            }
        }
        Task::Context(generation, reference, radius) => {
            if client.context_generation.load(Ordering::SeqCst) == generation {
                let progress = QueryProgress::context(db, client, generation)?;
                match execute_context(progress.0, &reference, radius) {
                    Ok(records) => {
                        let _ = reply.send(ArchiveReply::Context {
                            generation,
                            reference,
                            records,
                        });
                    }
                    Err(message)
                        if client.context_generation.load(Ordering::SeqCst) == generation =>
                    {
                        let _ = reply.send(ArchiveReply::ContextError {
                            generation,
                            message,
                        });
                    }
                    Err(_) => {}
                }
            }
        }
    }
    Ok(())
}

fn drain_redactions(
    db: &Connection,
    journal: &mut Option<ArchiveJournal>,
    client: &ArchiveClient,
) -> Result<(), String> {
    let pending = std::mem::take(&mut *client.pending_redactions.lock().unwrap());
    let mut pending = pending.into_iter();
    let mut applied = Vec::new();
    let mut failure = None;
    while let Some((conversation, msgid)) = pending.next() {
        let event = JournalEvent::Redact {
            conversation: conversation.clone(),
            msgid: msgid.clone(),
        };
        let result = (|| {
            if let Some(file) = journal.as_mut() {
                file.append(&event).map_err(JournalWriteError::message)?;
            }
            project_noting(db, &event, client)
        })();
        match result {
            Ok(()) => applied.push((conversation, msgid)),
            Err(error) => {
                // Keep failed controls available without holding the GUI-facing
                // mutex during filesystem or SQLite work.
                let mut outbox = client.pending_redactions.lock().unwrap();
                outbox.insert((conversation, msgid));
                outbox.extend(pending.by_ref());
                failure = Some(error);
                break;
            }
        }
    }
    // The tombstones are journaled and projected, so the earlier bodies can go.
    if !applied.is_empty() {
        if let Some(file) = journal.as_mut() {
            let targets = ScrubTargets::for_redactions(db, &applied)?;
            scrub_journals(db, file, client, &targets)?;
        }
        // Forget pages and write-ahead frames that still hold the old bodies.
        let _ = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    }
    failure.map_or(Ok(()), Err)
}

const SCRUBBED_MARKER: i64 = 2;

/// Remove targeted bodies from every journal. Only a failure that leaves the
/// journal unusable is an error; otherwise the unscrubbed files are intact,
/// the user is told, and the next start retries every known redaction.
fn scrub_journals(
    db: &Connection,
    file: &mut ArchiveJournal,
    client: &ArchiveClient,
    targets: &ScrubTargets,
) -> Result<(), String> {
    match file.scrub(db, targets) {
        Ok(()) => Ok(()),
        Err(ScrubError::Failed(error)) => {
            client.status.lock().unwrap().error = Some(format!(
                "A redacted message could not be erased from the history journal ({error}); it will be retried at the next start"
            ));
            db.execute(
                "DELETE FROM journal_manifest WHERE id=?1",
                [SCRUBBED_MARKER],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        }
        Err(ScrubError::Fatal(error)) => Err(error),
    }
}

/// Journals written before redactions were erased, or by a run that failed to
/// erase one, still hold bodies. Once per projection, scrub all known redactions.
fn scrub_known_redactions(
    db: &Connection,
    journal: &mut Option<ArchiveJournal>,
    client: &ArchiveClient,
) -> Result<(), String> {
    let Some(file) = journal.as_mut() else {
        return Ok(());
    };
    let done: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM journal_manifest WHERE id=?1)",
            [SCRUBBED_MARKER],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if done {
        return Ok(());
    }
    let targets = ScrubTargets::for_all_redactions(db)?;
    let before = client.status.lock().unwrap().error.clone();
    scrub_journals(db, file, client, &targets)?;
    if client.status.lock().unwrap().error == before {
        db.execute(
            "INSERT OR REPLACE INTO journal_manifest VALUES(?1,'scrubbed')",
            [SCRUBBED_MARKER],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// A message arriving after its redaction must never be journaled with its body.
fn withhold_redacted_body(db: &Connection, event: &mut JournalEvent) -> Result<(), String> {
    let JournalEvent::Upsert { record } = event else {
        return Ok(());
    };
    if record.body.is_empty() {
        return Ok(());
    }
    let tombstoned: bool = if let Some(msgid) = &record.msgid {
        let conversation =
            serde_json::to_string(&record.conversation).map_err(|e| e.to_string())?;
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM tombstones WHERE conversation=?1 AND msgid=?2)",
            params![conversation, msgid],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?
    } else {
        false
    };
    if tombstoned || load_record(db, &record.id)?.is_some_and(|previous| previous.redacted) {
        record.body.clear();
        record.redacted = true;
    }
    Ok(())
}

/// Read one physical line while bounding retained bytes even for corrupt or hostile files.
fn bounded_line(reader: &mut impl BufRead) -> std::io::Result<Option<(Vec<u8>, u64, bool)>> {
    bounded_line_limit(reader, MAX_ARCHIVE_LINE)
}

fn bounded_line_limit(
    reader: &mut impl BufRead,
    limit: usize,
) -> std::io::Result<Option<(Vec<u8>, u64, bool)>> {
    let mut bytes = Vec::new();
    let mut consumed = 0u64;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((consumed > 0).then_some((bytes, consumed, oversized)));
        }
        let take = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        let end = available[take - 1] == b'\n';
        if bytes.len().saturating_add(take) <= limit && !oversized {
            bytes.extend_from_slice(&available[..take]);
        } else {
            oversized = true;
            bytes.clear();
        }
        consumed = consumed.saturating_add(take as u64);
        reader.consume(take);
        if end {
            return Ok(Some((bytes, consumed, oversized)));
        }
    }
}

fn open_projection(root: Option<&Path>) -> Result<Connection, String> {
    let Some(root) = root else {
        let db = Connection::open_in_memory().map_err(|e| e.to_string())?;
        initialize(&db).map_err(|e| e.to_string())?;
        return Ok(db);
    };
    let dir = root.join("archive");
    private_dir(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("index.sqlite3");
    for retry in 0..2 {
        // Create privately before SQLite opens the database (and its private WAL).
        let file = private_append(&path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        super::logging::tighten_private_file(&path, &file).map_err(|e| e.to_string())?;
        drop(file);
        let db = Connection::open(&path).map_err(|e| e.to_string())?;
        let result = initialize(&db).and_then(|_| {
            db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
                .and_then(|value| {
                    if value == "ok" {
                        Ok(())
                    } else {
                        Err(rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                            Some(value),
                        ))
                    }
                })
        });
        match result {
            Ok(()) => return Ok(db),
            Err(error)
                if retry == 0
                    && matches!(
                        error.sqlite_error_code(),
                        Some(
                            rusqlite::ErrorCode::DatabaseCorrupt
                                | rusqlite::ErrorCode::NotADatabase
                        )
                    ) =>
            {
                drop(db);
                for candidate in [
                    path.clone(),
                    dir.join("index.sqlite3-wal"),
                    dir.join("index.sqlite3-shm"),
                ] {
                    match fs::remove_file(candidate) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.to_string()),
                    }
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    unreachable!()
}

fn run_archive(
    root: Option<PathBuf>,
    rx: &mpsc::Receiver<Task>,
    replies: &mpsc::Sender<ArchiveReply>,
    client: &ArchiveClient,
) -> Result<(), String> {
    let mut db = open_projection(root.as_deref())?;
    let mut journal = None;
    let mut deferred = VecDeque::new();
    if let Some(root) = &root {
        let dir = root.join("archive");
        remove_stale_scrub_files(&dir);
        let mut journals: Vec<_> = fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        // v1 journal names were random. Creation time preserves session order;
        // all new names carry time, with metadata as compatibility fallback.
        journals.sort_by_key(|path| {
            let metadata = fs::metadata(path).ok();
            let stamp = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.split_once('-'))
                .and_then(|(s, _)| s.parse::<u128>().ok())
                .or_else(|| {
                    metadata
                        .and_then(|m| m.created().or_else(|_| m.modified()).ok())
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                })
                .unwrap_or_default();
            (stamp, path.clone())
        });
        let previous_manifest: Option<String> = db
            .query_row(
                "SELECT digest FROM journal_manifest WHERE id=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let mut sources = Vec::new();
        for path in &journals {
            let digest = replay_journal(path, &db, client)?;
            sources.push((
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                digest,
            ));
        }
        if previous_manifest.as_deref() != Some(journal_manifest(&sources)?.as_str()) {
            // Replaying changes alone cannot remove rows absent from a deleted,
            // truncated or restored journal. Rebuild only when the actual source
            // content/order differs, retaining the normal legacy-import cache.
            clear_projection(&db)?;
            for path in &journals {
                replay_journal(path, &db, client)?;
            }
        }
        save_journal_manifest(&db, &sources)?;
        note_all_redacted_lines(&db, client)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = format!("{stamp:020}-{}.jsonl", new_record_id());
        let path = dir.join(&name);
        let mut options = OpenOptions::new();
        options.create_new(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        journal = Some(ArchiveJournal {
            file: options.open(path).map_err(|e| e.to_string())?,
            dir: dir.clone(),
            name,
            digest: Sha256::new(),
            previous: sources,
        });
        // A successful flush barrier must also survive loss of the new journal's directory entry.
        #[cfg(unix)]
        {
            File::open(&dir)
                .and_then(|file| file.sync_all())
                .map_err(|e| e.to_string())?;
            File::open(root)
                .and_then(|file| file.sync_all())
                .map_err(|e| e.to_string())?;
        }
        drain_redactions(&db, &mut journal, client)?;
        scrub_known_redactions(&db, &mut journal, client)?;
        import_legacy(
            &root.join("logs"),
            &mut db,
            rx,
            replies,
            client,
            &mut journal,
            &mut deferred,
        )?;
    }
    {
        let mut status = client.status.lock().unwrap();
        status.indexing = false;
        status.records = db
            .query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())? as usize;
    }
    loop {
        drain_redactions(&db, &mut journal, client)?;
        let task = if let Some(task) = deferred.pop_front() {
            Some(task)
        } else if client.stop.load(Ordering::SeqCst) {
            rx.try_recv().ok()
        } else {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(task) => Some(task),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            }
        };
        let Some(task) = task else {
            break;
        };
        process_task(task, &mut db, &mut journal, replies, client)?;
    }
    drain_redactions(&db, &mut journal, client)?;
    if let Some(file) = journal.as_mut() {
        file.sync(&db)?;
    }
    Ok(())
}

fn purge_legacy_source(db: &Connection, source: &str) -> Result<(), String> {
    let ids: Vec<String> = {
        let mut stmt = db
            .prepare("SELECT id FROM legacy_sources WHERE path=?1")
            .map_err(|e| e.to_string())?;
        stmt.query_map([source], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?
    };
    for id in ids {
        delete_record(db, &id)?;
    }
    db.execute("DELETE FROM sources WHERE path=?1", [source])
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn import_legacy(
    log_dir: &Path,
    db: &mut Connection,
    rx: &mpsc::Receiver<Task>,
    reply: &mpsc::Sender<ArchiveReply>,
    client: &ArchiveClient,
    journal: &mut Option<ArchiveJournal>,
    deferred: &mut VecDeque<Task>,
) -> Result<(), String> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    if let Ok(networks) = fs::read_dir(log_dir) {
        for network in networks
            .filter_map(Result::ok)
            .filter(|n| n.file_type().is_ok_and(|k| k.is_dir()))
        {
            if let Ok(files) = fs::read_dir(network.path()) {
                for entry in files.filter_map(Result::ok).filter(|f| {
                    f.file_type().is_ok_and(|k| k.is_file())
                        && f.path().extension().is_some_and(|e| e == "log")
                }) {
                    paths.push((
                        entry.path(),
                        network.file_name().to_string_lossy().into_owned(),
                    ));
                }
            }
        }
    }
    paths.sort();
    for (path, folder) in paths {
        if client.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let source = path.to_string_lossy().into_owned();
        seen.insert(source.clone());
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        let generation = file_generation(&file).map_err(|e| e.to_string())?;
        let fingerprint = format!(
            "{generation}:{}:{:?}",
            metadata.len(),
            metadata.modified().ok()
        );
        let prior: Option<String> = db
            .query_row(
                "SELECT fingerprint FROM sources WHERE path=?1",
                [&source],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if prior.as_deref() == Some(&fingerprint) {
            continue;
        }
        purge_legacy_source(db, &source)?;
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let target = stem
            .rsplit_once('~')
            .map_or(stem.as_ref(), |(prefix, _)| prefix)
            .to_string();
        let conversation = ConversationKey {
            network: LogicalNetworkKey::Legacy { source: folder },
            target: target.clone(),
        };
        let mut reader = BufReader::new(file);
        let mut offset = 0u64;
        let mut count = 0;
        while let Some((bytes, len, oversized)) =
            bounded_line(&mut reader).map_err(|e| e.to_string())?
        {
            if client.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if oversized {
                client.status.lock().unwrap().error =
                    Some("An oversized legacy history line was skipped".into());
            } else {
                let line = String::from_utf8_lossy(&bytes);
                if let Some(message) =
                    super::logging::parse_log_line(line.trim_end_matches(['\r', '\n']))
                    && !super::commands::service_message_contains_credentials(
                        &target,
                        &message.content,
                    )
                {
                    let digest = line_digest(&bytes);
                    let id = line_record_id(&generation, offset, &bytes);
                    let mut record = ArchiveRecord::from_message(conversation.clone(), &message);
                    record.id = id.clone();
                    record.legacy = true;
                    record.received_at = None;
                    record.text_span = Some(LogSpan {
                        path: source.clone(),
                        generation: generation.clone(),
                        offset,
                        len,
                        digest,
                    });
                    project(
                        db,
                        &JournalEvent::Upsert {
                            record: Box::new(record),
                        },
                    )?;
                    // A structured copy may suppress this exact physical line.
                    db.execute("INSERT OR REPLACE INTO legacy_sources SELECT id,?1 FROM records WHERE id=?2",params![source,id]).map_err(|e|e.to_string())?;
                }
            }
            offset = offset.saturating_add(len);
            count += 1;
            if count % 128 == 0 && deferred.len() < 4096 {
                for _ in 0..32 {
                    match rx.try_recv() {
                        Ok(task @ Task::Restore(..)) => deferred.push_back(task),
                        Ok(task) => process_task(task, db, journal, reply, client)?,
                        Err(_) => break,
                    }
                }
            }
        }
        db.execute(
            "INSERT OR REPLACE INTO sources VALUES(?1,?2)",
            params![source, fingerprint],
        )
        .map_err(|e| e.to_string())?;
    }
    let prior: Vec<String> = {
        let mut stmt = db
            .prepare("SELECT path FROM sources")
            .map_err(|e| e.to_string())?;
        stmt.query_map([], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?
    };
    for source in prior {
        if !seen.contains(&source) {
            purge_legacy_source(db, &source)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::identity::TransportEndpointKey;
    use super::*;
    fn conversation(target: &str) -> ConversationKey {
        ConversationKey {
            network: LogicalNetworkKey::Direct {
                endpoint: TransportEndpointKey::new("irc.example", 6697, true),
            },
            target: target.into(),
        }
    }
    fn record(target: &str, body: &str, msgid: &str) -> ArchiveRecord {
        let mut record = ArchiveRecord::from_message(
            conversation(target),
            &ChatMessage::new_fmt("alice", body, "short"),
        );
        record.msgid = Some(msgid.into());
        record
    }
    fn receive(service: &ArchiveService) -> ArchiveReply {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(reply) = service.poll().into_iter().next() {
                return reply;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn archive_search_is_scoped_and_literal_and_keeps_short_queries() {
        let service = ArchiveService::memory();
        service.client.upsert(record("#a", "hello % literal", "a"));
        service.client.upsert(record("#b", "other", "b"));
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        for text in ["hello", "%", "lo"] {
            let generation = service.client.search(SearchQuery {
                text: text.into(),
                target: Some("#a".into()),
                ..Default::default()
            });
            match receive(&service) {
                ArchiveReply::Search {
                    generation: g,
                    records,
                    ..
                } => {
                    assert_eq!(g, generation);
                    assert_eq!(records.len(), 1);
                    assert_eq!(records[0].body, "hello % literal");
                }
                _ => panic!("search failed"),
            }
        }
    }
    fn dated(target: &str, body: &str, msgid: &str, at: i64) -> ArchiveRecord {
        let mut row = record(target, body, msgid);
        row.received_at = Some(at);
        row
    }
    fn page(
        service: &ArchiveService,
        query: SearchQuery,
    ) -> (Vec<ArchiveRecord>, Option<SearchCursor>) {
        service.client.search(query);
        match receive(service) {
            ArchiveReply::Search { records, next, .. } => (records, next),
            _ => panic!("search failed"),
        }
    }
    #[test]
    fn search_paging_is_stable_when_newer_matches_arrive_between_pages() {
        let service = ArchiveService::memory();
        for index in 0..150 {
            service.client.upsert(dated(
                "#a",
                "needle",
                &format!("old{index}"),
                1_000 + index as i64,
            ));
        }
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        let query = SearchQuery {
            text: "needle".into(),
            ..Default::default()
        };
        let (first, next) = page(&service, query.clone());
        assert_eq!(first.len(), 100);
        assert!(next.is_some());
        for index in 0..5 {
            service.client.upsert(dated(
                "#a",
                "needle",
                &format!("new{index}"),
                5_000 + index as i64,
            ));
        }
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        let (second, next) = page(
            &service,
            SearchQuery {
                after: next,
                ..query
            },
        );
        assert!(next.is_none());
        let mut ids: Vec<_> = first.iter().chain(&second).map(|r| r.id.clone()).collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total, "a page repeated rows already shown");
        assert_eq!(second.len(), 50, "rows were skipped between pages");
    }
    #[test]
    fn search_paging_does_not_skip_rows_when_shown_rows_are_redacted() {
        let service = ArchiveService::memory();
        for index in 0..150 {
            service.client.upsert(dated(
                "#a",
                "needle",
                &format!("m{index}"),
                1_000 + index as i64,
            ));
        }
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        let query = SearchQuery {
            text: "needle".into(),
            ..Default::default()
        };
        let (first, next) = page(&service, query.clone());
        for row in first.iter().take(3) {
            service
                .client
                .redact(row.conversation.clone(), row.msgid.clone().unwrap());
        }
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        let (second, next) = page(
            &service,
            SearchQuery {
                after: next,
                ..query
            },
        );
        assert!(next.is_none());
        assert_eq!(second.len(), 50, "redaction shifted the next page");
    }
    #[test]
    fn redaction_before_creation_survives_replay_and_context() {
        let service = ArchiveService::memory();
        let record = record("#a", "secret body", "a");
        let reference = record.reference();
        service
            .client
            .redact(record.conversation.clone(), "a".into());
        service.client.upsert(record.clone());
        service.client.upsert(record);
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        service.client.search(SearchQuery {
            text: "secret".into(),
            ..Default::default()
        });
        assert!(matches!(receive(&service),ArchiveReply::Search{records,..}if records.is_empty()));
        service.client.context(reference, 50);
        assert!(
            matches!(receive(&service),ArchiveReply::Context{records,..}if records.len()==1&&records[0].redacted&&records[0].body.is_empty())
        );
    }

    #[test]
    fn redaction_during_queue_saturation_survives_a_restart() {
        let root = Scratch::new();
        let service = ArchiveService::open(&root.0);
        let original = record("#a", "body that must stay redacted", "one");
        assert!(service.client.upsert(original.clone()));
        assert!(service.client.flush_barrier(Duration::from_secs(5)));

        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::channel();
        assert!(service.client.submit(Task::Pause(entered_tx, release_rx)));
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        for _ in 0..4096 {
            assert!(
                service
                    .client
                    .submit(Task::Search(u64::MAX, SearchQuery::default()))
            );
        }
        service
            .client
            .redact(original.conversation.clone(), "one".into());
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !service.client.flush_barrier(Duration::from_secs(1)) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        drop(service);

        let reopened = ArchiveService::open(&root.0);
        assert!(reopened.client.flush_barrier(Duration::from_secs(5)));
        reopened.client.context(original.reference(), 0);
        assert!(
            matches!(receive(&reopened), ArchiveReply::Context { records, .. }
            if records.len() == 1 && records[0].redacted && records[0].body.is_empty())
        );
    }

    fn journal_files(root: &Scratch) -> Vec<PathBuf> {
        let mut files: Vec<_> = fs::read_dir(root.0.join("archive"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        files.sort();
        files
    }
    fn journal_bytes(root: &Scratch) -> Vec<u8> {
        journal_files(root)
            .iter()
            .flat_map(|path| fs::read(path).unwrap())
            .collect()
    }
    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    }
    fn synced(service: &ArchiveService) {
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
    }
    fn context_of(service: &ArchiveService, row: &ArchiveRecord) -> Vec<ArchiveRecord> {
        service.client.context(row.reference(), 0);
        match receive(service) {
            ArchiveReply::Context { records, .. } => records,
            _ => panic!("context failed"),
        }
    }

    #[test]
    fn redaction_removes_the_body_from_every_journal_file_and_keeps_the_tombstone() {
        let root = Scratch::new();
        let old_secret = record("#a", "hunter2-old-marker", "one");
        let survivor = record("#a", "survivor-marker", "two");
        let first = ArchiveService::open(&root.0);
        assert!(first.client.upsert(old_secret.clone()));
        assert!(first.client.upsert(survivor.clone()));
        synced(&first);
        drop(first);

        let second = ArchiveService::open(&root.0);
        let new_secret = record("#a", "hunter2-new-marker", "three");
        assert!(second.client.upsert(new_secret.clone()));
        synced(&second);
        assert!(contains(&journal_bytes(&root), "hunter2-old-marker"));
        assert!(contains(&journal_bytes(&root), "hunter2-new-marker"));
        assert_eq!(journal_files(&root).len(), 2);

        second
            .client
            .redact(old_secret.conversation.clone(), "one".into());
        second
            .client
            .redact(new_secret.conversation.clone(), "three".into());
        synced(&second);
        let bytes = journal_bytes(&root);
        assert!(!contains(&bytes, "hunter2-old-marker"));
        assert!(!contains(&bytes, "hunter2-new-marker"));
        assert!(contains(&bytes, "survivor-marker"));
        assert_eq!(
            bytes
                .windows(b"\"event\":\"redact\"".len())
                .filter(|window| *window == b"\"event\":\"redact\"")
                .count(),
            2,
            "the tombstones must stay in the journal"
        );

        // Appending after the active journal was rewritten must still work.
        assert!(
            second
                .client
                .upsert(record("#a", "after-scrub-marker", "four"))
        );
        synced(&second);
        drop(second);

        let reopened = ArchiveService::open(&root.0);
        synced(&reopened);
        for row in [&old_secret, &new_secret] {
            let found = context_of(&reopened, row);
            assert_eq!(found.len(), 1);
            assert!(found[0].redacted && found[0].body.is_empty());
        }
        reopened.client.search(SearchQuery::default());
        match receive(&reopened) {
            ArchiveReply::Search { records, .. } => {
                let mut bodies: Vec<_> = records.iter().map(|r| r.body.as_str()).collect();
                bodies.sort_unstable();
                assert_eq!(bodies, ["after-scrub-marker", "survivor-marker"]);
            }
            _ => panic!("search failed"),
        }
        let bytes = journal_bytes(&root);
        assert!(contains(&bytes, "after-scrub-marker"));
        assert!(!contains(&bytes, "hunter2"));
    }

    #[test]
    fn redaction_removes_a_chunked_large_body_and_leaves_other_large_records_intact() {
        let root = Scratch::new();
        let service = ArchiveService::open(&root.0);
        let big = |marker: &str| format!("{marker}{}", "x".repeat(MAX_ARCHIVE_LINE + 4096));
        let secret = record("#a", &big("hunter2-big-marker"), "one");
        let other = record("#a", &big("keep-big-marker"), "two");
        assert!(service.client.upsert(secret.clone()));
        assert!(service.client.upsert(other.clone()));
        synced(&service);
        let before = journal_bytes(&root);
        assert!(contains(&before, "\"event\":\"chunk\""));
        service
            .client
            .redact(secret.conversation.clone(), "one".into());
        synced(&service);
        let after = journal_bytes(&root);
        assert!(
            after.len() < before.len() / 2 + 4096,
            "the secret's chunks must be replaced by a small redacted record"
        );
        drop(service);

        let reopened = ArchiveService::open(&root.0);
        synced(&reopened);
        let redacted = context_of(&reopened, &secret);
        assert!(redacted.len() == 1 && redacted[0].redacted && redacted[0].body.is_empty());
        let kept = context_of(&reopened, &other);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].body, other.body);
    }

    #[test]
    fn a_message_arriving_after_its_redaction_is_never_journaled_with_its_body() {
        let root = Scratch::new();
        let service = ArchiveService::open(&root.0);
        let late = record("#a", "hunter2-late-marker", "one");
        service
            .client
            .redact(late.conversation.clone(), "one".into());
        synced(&service);
        assert!(service.client.upsert(late.clone()));
        synced(&service);
        assert!(!contains(&journal_bytes(&root), "hunter2-late-marker"));
        drop(service);
        let reopened = ArchiveService::open(&root.0);
        synced(&reopened);
        let found = context_of(&reopened, &late);
        assert!(found.len() == 1 && found[0].redacted && found[0].body.is_empty());
    }

    fn write_journal(path: &Path, events: &[JournalEvent]) {
        let mut journal = ArchiveJournal {
            file: private_append(path).unwrap(),
            dir: path.parent().unwrap().to_path_buf(),
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            digest: Sha256::new(),
            previous: Vec::new(),
        };
        for event in events {
            journal.append(event).ok().unwrap();
        }
    }
    fn upsert(row: &ArchiveRecord) -> JournalEvent {
        JournalEvent::Upsert {
            record: Box::new(row.clone()),
        }
    }
    fn lines(bytes: &[u8]) -> Vec<Vec<u8>> {
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .map(<[u8]>::to_vec)
            .collect()
    }
    fn is_chunk(line: &[u8]) -> bool {
        contains(line, "\"event\":\"chunk\"")
    }

    #[test]
    fn scrubbing_changes_only_the_targeted_records_and_reports_the_new_digest() {
        let root = Scratch::new();
        let path = root.0.join("one.jsonl");
        let secret = record("#a", "hunter2-unit-marker", "one");
        let other = record("#a", "other body", "two");
        let big = record("#a", &"y".repeat(MAX_ARCHIVE_LINE + 100), "three");
        let big_secret = record("#a", &"z".repeat(MAX_ARCHIVE_LINE + 100), "four");
        write_journal(
            &path,
            &[
                upsert(&other),
                upsert(&secret),
                JournalEvent::Reaction {
                    conversation: other.conversation.clone(),
                    msgid: "two".into(),
                    actor: "bob".into(),
                    emoji: "+".into(),
                    present: true,
                },
                upsert(&big),
                upsert(&big_secret),
                JournalEvent::Redact {
                    conversation: secret.conversation.clone(),
                    msgid: "one".into(),
                },
            ],
        );
        // Lines this build cannot read are not its to alter or drop.
        let mut original = fs::read(&path).unwrap();
        original.extend_from_slice(b"not json\n\n{\"event\":\"future\"}\nno trailing newline");
        fs::write(&path, &original).unwrap();
        let before = lines(&original);

        let mut targets = ScrubTargets::default();
        targets.ids.insert(secret.id.clone());
        targets.ids.insert(big_secret.id.clone());
        let digest = scrub_journal_file(&path, &targets).unwrap().unwrap();
        let after_bytes = fs::read(&path).unwrap();
        assert_eq!(hex_digest(&digest), line_digest(&after_bytes));
        assert!(!contains(&after_bytes, "hunter2-unit-marker"));
        let after = lines(&after_bytes);

        // Both secrets collapse to one bodyless line each; everything else is
        // identical, byte for byte and in order.
        let chunks_before: Vec<_> = before.iter().filter(|l| is_chunk(l)).collect();
        let chunks_after: Vec<_> = after.iter().filter(|l| is_chunk(l)).collect();
        assert!(chunks_after.len() < chunks_before.len());
        assert!(
            chunks_before.starts_with(&chunks_after),
            "unrelated chunks changed"
        );
        let untouched = |lines: &[Vec<u8>]| -> Vec<Vec<u8>> {
            lines
                .iter()
                .filter(|line| {
                    !is_chunk(line)
                        && !contains(line, "hunter2-unit-marker")
                        && !contains(line, "\"redacted\":true")
                })
                .cloned()
                .collect()
        };
        assert_eq!(untouched(&before), untouched(&after));
        let rewritten: Vec<ArchiveRecord> = after
            .iter()
            .filter_map(|line| match serde_json::from_slice(line) {
                Ok(JournalLine::Event(JournalEvent::Upsert { record })) if record.redacted => {
                    Some(*record)
                }
                _ => None,
            })
            .collect();
        assert_eq!(rewritten.len(), 2);
        assert!(rewritten.iter().all(|row| row.body.is_empty()));
        assert!(
            rewritten
                .iter()
                .any(|row| row.msgid.as_deref() == Some("one"))
        );
        assert!(
            rewritten
                .iter()
                .any(|row| row.msgid.as_deref() == Some("four"))
        );

        // Nothing left to remove: a second pass leaves the file alone.
        assert!(scrub_journal_file(&path, &targets).unwrap().is_none());
        assert_eq!(fs::read(&path).unwrap(), after_bytes);
        assert!(!path.with_extension("jsonl.scrub").exists());
    }

    #[test]
    fn unrelated_journal_lines_are_never_parsed_or_rewritten() {
        let root = Scratch::new();
        let path = root.0.join("big.jsonl");
        let unrelated: Vec<_> = (0..500)
            .map(|index| {
                upsert(&record(
                    "#a",
                    &format!("body {index}"),
                    &format!("m{index}"),
                ))
            })
            .collect();
        write_journal(&path, &unrelated);
        let before = fs::read(&path).unwrap();

        let mut targets = ScrubTargets::default();
        targets.ids.insert(new_record_id());
        targets
            .msgids
            .insert("absent-msgid".into(), vec![conversation("#a")]);
        SCRUB_PARSES.with(|count| count.set(0));
        assert!(scrub_journal_file(&path, &targets).unwrap().is_none());
        assert_eq!(SCRUB_PARSES.with(|count| count.get()), 0);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!path.with_extension("jsonl.scrub").exists());

        // With one real target only the lines that mention it are parsed, and a
        // msgid needing JSON escapes is still found.
        let odd = record("#a", "hunter2-odd-marker", "q\"uote\\and\u{e9}\n");
        let mut events = unrelated;
        events.insert(250, upsert(&odd));
        write_journal(&root.0.join("two.jsonl"), &events);
        let mut targets = ScrubTargets::default();
        targets.ids.insert(odd.id.clone());
        targets
            .msgids
            .insert(odd.msgid.clone().unwrap(), vec![odd.conversation.clone()]);
        SCRUB_PARSES.with(|count| count.set(0));
        let digest = scrub_journal_file(&root.0.join("two.jsonl"), &targets)
            .unwrap()
            .unwrap();
        let parsed = SCRUB_PARSES.with(|count| count.get());
        assert!(parsed <= 2, "parsed {parsed} lines for one targeted record");
        let after = fs::read(root.0.join("two.jsonl")).unwrap();
        assert_eq!(hex_digest(&digest), line_digest(&after));
        assert!(!contains(&after, "hunter2-odd-marker"));
        assert_eq!(lines(&after).len(), 501);
    }

    #[test]
    fn a_failed_rewrite_leaves_the_journal_untouched_and_the_archive_running() {
        let root = Scratch::new();
        let service = ArchiveService::open(&root.0);
        let secret = record("#a", "hunter2-failure-marker", "one");
        assert!(service.client.upsert(secret.clone()));
        synced(&service);
        let journal = journal_files(&root).remove(0);
        let before = fs::read(&journal).unwrap();
        // A directory where the temporary copy belongs makes the rewrite fail.
        fs::create_dir(journal.with_extension("jsonl.scrub")).unwrap();
        service
            .client
            .redact(secret.conversation.clone(), "one".into());
        synced(&service);
        let after = fs::read(&journal).unwrap();
        assert!(after.starts_with(&before));
        assert!(contains(&after, "hunter2-failure-marker"));
        assert!(contains(&after, "\"event\":\"redact\""));
        assert!(
            service
                .client
                .status()
                .error
                .is_some_and(|error| error.contains("could not be erased"))
        );
        // The archive still works, and still hides the message.
        assert!(service.client.upsert(record("#a", "later body", "two")));
        synced(&service);
        service.client.search(SearchQuery::default());
        assert!(matches!(
            receive(&service),
            ArchiveReply::Search { records, .. }
                if records.len() == 1 && records[0].body == "later body"
        ));
        drop(service);

        // The next start retries and, with the obstruction gone, erases it.
        fs::remove_dir(journal.with_extension("jsonl.scrub")).unwrap();
        let reopened = ArchiveService::open(&root.0);
        synced(&reopened);
        assert!(!contains(&journal_bytes(&root), "hunter2-failure-marker"));
        let found = context_of(&reopened, &secret);
        assert!(found.len() == 1 && found[0].redacted && found[0].body.is_empty());
    }

    #[test]
    fn an_interrupted_rewrite_is_ignored_and_cleaned_up_on_the_next_open() {
        let root = Scratch::new();
        let first = ArchiveService::open(&root.0);
        let kept = record("#a", "kept-marker", "one");
        assert!(first.client.upsert(kept.clone()));
        synced(&first);
        drop(first);
        let journal = journal_files(&root).remove(0);
        let original = fs::read(&journal).unwrap();
        // A crash after writing part of the replacement and before the rename.
        let stale = journal.with_extension("jsonl.scrub");
        fs::write(&stale, b"{\"event\":\"upsert\",\"half written").unwrap();

        let reopened = ArchiveService::open(&root.0);
        synced(&reopened);
        assert!(!stale.exists());
        assert_eq!(fs::read(&journal).unwrap(), original);
        let found = context_of(&reopened, &kept);
        assert!(found.len() == 1 && found[0].body == "kept-marker");
    }

    #[test]
    fn redactions_already_in_old_journals_are_erased_when_the_archive_opens() {
        let root = Scratch::new();
        let secret = record("#a", "hunter2-legacy-marker", "one");
        let dir = root.0.join("archive");
        fs::create_dir_all(&dir).unwrap();
        // A journal from before bodies were erased: the redaction sits beside it,
        // and a later copy of the message arrived after the redaction.
        write_journal(
            &dir.join("00000000000000000001-a.jsonl"),
            &[
                upsert(&secret),
                JournalEvent::Redact {
                    conversation: secret.conversation.clone(),
                    msgid: "one".into(),
                },
            ],
        );
        write_journal(
            &dir.join("00000000000000000002-b.jsonl"),
            &[upsert(&secret)],
        );
        let service = ArchiveService::open(&root.0);
        synced(&service);
        assert!(!contains(&journal_bytes(&root), "hunter2-legacy-marker"));
        let found = context_of(&service, &secret);
        assert!(found.len() == 1 && found[0].redacted && found[0].body.is_empty());
    }

    #[test]
    fn redacted_text_does_not_linger_in_the_search_database() {
        let root = Scratch::new();
        let service = ArchiveService::open(&root.0);
        let secret = record("#a", "hunter2-sqlite-marker", "one");
        assert!(service.client.upsert(secret.clone()));
        synced(&service);
        service
            .client
            .redact(secret.conversation.clone(), "one".into());
        synced(&service);
        for name in ["index.sqlite3", "index.sqlite3-wal"] {
            if let Ok(bytes) = fs::read(root.0.join("archive").join(name)) {
                assert!(!contains(&bytes, "hunter2-sqlite-marker"), "{name}");
            }
        }
    }

    #[test]
    fn searches_and_contexts_have_independent_work_and_error_lanes() {
        let service = ArchiveService::memory();
        let row = record("#a", "matching row", "one");
        service.client.upsert(row.clone());
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::channel();
        service.client.submit(Task::Pause(entered_tx, release_rx));
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let search_generation = service.client.search(SearchQuery::default());
        let context_generation = service.client.context(row.reference(), 0);
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut search = false;
        let mut context = false;
        while !(search && context) {
            for reply in service.poll() {
                match reply {
                    ArchiveReply::Search {
                        generation,
                        records,
                        ..
                    } if generation == search_generation => {
                        assert_eq!(records.len(), 1);
                        search = true;
                    }
                    ArchiveReply::Context {
                        generation,
                        records,
                        ..
                    } if generation == context_generation => {
                        assert_eq!(records.len(), 1);
                        context = true;
                    }
                    _ => panic!("unexpected reply"),
                }
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }

        let generation = service.client.context(row.reference(), 0);
        service.client.cancel_query();
        assert!(
            matches!(receive(&service), ArchiveReply::Context {generation:found,..} if generation == found)
        );
        let generation = service.client.search(SearchQuery::default());
        service.client.cancel_context();
        assert!(
            matches!(receive(&service), ArchiveReply::Search {generation:found,..} if generation == found)
        );

        let (tx, _rx) = mpsc::sync_channel(0);
        let full = ArchiveService {
            client: ArchiveClient {
                tx: Some(tx),
                available: Arc::new(AtomicBool::new(true)),
                ..Default::default()
            },
            replies: None,
            worker: None,
        };
        let search = full.client.search(SearchQuery::default());
        let context = full.client.context(row.reference(), 0);
        let replies = full.poll();
        assert_eq!(replies.len(), 2);
        assert!(
            replies.iter().any(
                |reply| matches!(reply,ArchiveReply::Error{generation,..}if *generation==search)
            )
        );
        assert!(replies.iter().any(
            |reply| matches!(reply,ArchiveReply::ContextError{generation,..}if *generation==context)
        ));
    }
    #[test]
    fn duplicate_msgid_and_echo_updates_keep_one_record() {
        let service = ArchiveService::memory();
        let first = record("#a", "hello", "a");
        let reference = first.reference();
        service.client.upsert(first.clone());
        let mut echo = first;
        echo.account = Some("alice-account".into());
        service.client.upsert(echo.clone());
        echo.id = new_record_id();
        service.client.upsert(echo.clone());
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        service.client.context(reference, 50);
        assert!(
            matches!(receive(&service),ArchiveReply::Context{records,..}if records.len()==1&&records[0].account.as_deref()==Some("alice-account"))
        );
        service.client.context(echo.reference(), 50);
        assert!(matches!(receive(&service),ArchiveReply::Context{records,..}if records.len()==1));
    }
    #[test]
    fn credential_service_content_never_enters_archive() {
        let service = ArchiveService::memory();
        assert!(
            !service
                .client
                .upsert(record("NickServ", "IDENTIFY dummy-password", "a"))
        );
        service.client.search(SearchQuery {
            ..Default::default()
        });
        assert!(matches!(receive(&service),ArchiveReply::Search{records,..}if records.is_empty()));
    }
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("linefeed-archive-{}", new_record_id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn log(&self, body: &[u8]) -> PathBuf {
            let dir = self.0.join("logs/network");
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("#a.log");
            fs::write(&path, body).unwrap();
            path
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn import(db: &mut Connection, root: &Path) {
        let (_tx, rx) = mpsc::sync_channel(4);
        let (reply, _) = mpsc::channel();
        import_legacy(
            &root.join("logs"),
            db,
            &rx,
            &reply,
            &ArchiveClient::default(),
            &mut None,
            &mut VecDeque::new(),
        )
        .unwrap();
    }
    fn test_db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        db
    }
    fn all(db: &mut Connection) -> Vec<ArchiveRecord> {
        execute_search(db, &SearchQuery::default(), &ArchiveClient::default(), 0)
            .unwrap()
            .0
    }

    #[test]
    fn exact_text_span_skips_only_physical_dual_copy_and_metadata_keeps_span() {
        let root = Scratch::new();
        let line = b"[12:00] <alice> repeat\n";
        let path = root.log(&[line.as_slice(), line.as_slice()].concat());
        let file = File::open(&path).unwrap();
        let mut db = test_db();
        let mut saved = record("#a", "repeat", "one");
        saved.received_at = Some(123);
        saved.text_span = Some(LogSpan {
            path: path.to_string_lossy().into_owned(),
            generation: file_generation(&file).unwrap(),
            offset: 0,
            len: line.len() as u64,
            digest: line_digest(line),
        });
        project(
            &db,
            &JournalEvent::Upsert {
                record: Box::new(saved.clone()),
            },
        )
        .unwrap();
        let mut echo = saved.clone();
        echo.received_at = Some(999);
        echo.text_span = None;
        echo.account = Some("alice".into());
        project(
            &db,
            &JournalEvent::Upsert {
                record: Box::new(echo),
            },
        )
        .unwrap();
        import(&mut db, &root.0);
        let found = all(&mut db);
        assert_eq!(found.len(), 2, "a genuine repeated line must remain");
        let structured = found.iter().find(|r| !r.legacy).unwrap();
        assert_eq!(structured.received_at, Some(123));
        assert!(structured.text_span.is_some());
        // The reverse order is possible when a live event arrives during import.
        let mut db2 = test_db();
        import(&mut db2, &root.0);
        assert_eq!(all(&mut db2).len(), 2);
        project(
            &db2,
            &JournalEvent::Upsert {
                record: Box::new(saved),
            },
        )
        .unwrap();
        assert_eq!(all(&mut db2).len(), 2);
    }
    #[test]
    fn legacy_rewrites_truncation_and_removal_invalidate_cached_rows() {
        let root = Scratch::new();
        let path = root.log(b"[12:00] <alice> old\n");
        let mut db = test_db();
        import(&mut db, &root.0);
        assert_eq!(all(&mut db)[0].body, "old");
        fs::write(&path, b"[12:00] <alice> newer\n").unwrap();
        import(&mut db, &root.0);
        assert_eq!(all(&mut db).len(), 1);
        assert_eq!(all(&mut db)[0].body, "newer");
        fs::write(&path, b"").unwrap();
        import(&mut db, &root.0);
        assert!(all(&mut db).is_empty());
        fs::write(&path, b"[12:00] <alice> return\n").unwrap();
        import(&mut db, &root.0);
        assert_eq!(all(&mut db).len(), 1);
        fs::remove_file(path).unwrap();
        import(&mut db, &root.0);
        assert!(all(&mut db).is_empty());
    }
    #[test]
    fn oversized_legacy_line_is_bounded_and_does_not_hide_following_line() {
        let root = Scratch::new();
        let mut bytes = vec![b'x'; MAX_ARCHIVE_LINE + 99];
        bytes.extend_from_slice(b"\n[12:00] <alice> retained  \n");
        root.log(&bytes);
        let mut db = test_db();
        import(&mut db, &root.0);
        let found = all(&mut db);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].body, "retained  ");
        let mut reader = BufReader::new(bytes.as_slice());
        let (kept, len, oversized) = bounded_line(&mut reader).unwrap().unwrap();
        assert!(oversized);
        assert!(kept.is_empty());
        assert_eq!(len as usize, MAX_ARCHIVE_LINE + 100);
    }
    #[test]
    fn local_row_merging_existing_server_id_redirects_all_old_references() {
        let mut db = test_db();
        let mut server = record("#a", "hello", "server");
        server.id = "server-row".into();
        let mut local = server.clone();
        local.id = "local-row".into();
        local.msgid = None;
        let old = local.reference();
        project(
            &db,
            &JournalEvent::Upsert {
                record: Box::new(server.clone()),
            },
        )
        .unwrap();
        project(
            &db,
            &JournalEvent::Upsert {
                record: Box::new(local.clone()),
            },
        )
        .unwrap();
        local.msgid = server.msgid;
        project(
            &db,
            &JournalEvent::Upsert {
                record: Box::new(local),
            },
        )
        .unwrap();
        assert_eq!(all(&mut db).len(), 1);
        assert_eq!(execute_context(&db, &old, 50).unwrap()[0].id, "server-row");
    }
    #[test]
    fn context_uses_monotonic_sequence_for_equal_times_and_restores_ignore_search_cancel() {
        let mut db = test_db();
        let mut first = record("#a", "first", "first");
        first.id = "z".into();
        first.received_at = Some(100);
        let mut second = record("#a", "second", "second");
        second.id = "a".into();
        second.received_at = Some(100);
        project(
            &db,
            &JournalEvent::Upsert {
                record: Box::new(first.clone()),
            },
        )
        .unwrap();
        project(
            &db,
            &JournalEvent::Upsert {
                record: Box::new(second),
            },
        )
        .unwrap();
        assert_eq!(
            execute_context(&db, &first.reference(), 50)
                .unwrap()
                .iter()
                .map(|r| r.body.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        let client = ArchiveClient::default();
        client.cancel_query();
        let (tx, rx) = mpsc::channel();
        process_task(
            Task::Restore(42, first.reference(), 50),
            &mut db,
            &mut None,
            &tx,
            &client,
        )
        .unwrap();
        assert!(
            matches!(rx.recv().unwrap(),ArchiveReply::Restored{token:42,records,..}if records.len()==2)
        );
    }
    #[test]
    fn corrupt_private_projection_rebuilds_from_journal_and_replays_in_creation_order() {
        let root = Scratch::new();
        let dir = root.0.join("archive");
        fs::create_dir(&dir).unwrap();
        let mut first = record("#a", "old", "one");
        first.id = "row".into();
        let mut bytes = serde_json::to_vec(&JournalEvent::Upsert {
            record: Box::new(first.clone()),
        })
        .unwrap();
        bytes.push(b'\n');
        fs::write(dir.join("zz-random.jsonl"), bytes).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        first.body = "new".into();
        let mut bytes = serde_json::to_vec(&JournalEvent::Upsert {
            record: Box::new(first),
        })
        .unwrap();
        bytes.push(b'\n');
        fs::write(dir.join("aa-random.jsonl"), bytes).unwrap();
        fs::write(dir.join("index.sqlite3"), b"corrupt cache").unwrap();
        let service = ArchiveService::open(&root.0);
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        service.client.search(SearchQuery::default());
        assert!(
            matches!(receive(&service),ArchiveReply::Search{records,..} if records.len()==1&&records[0].body=="new")
        );
        drop(service);
        // A second process/session rebuild path must retain the same projection.
        let reopened = ArchiveService::open(&root.0);
        assert!(reopened.client.flush_barrier(Duration::from_secs(5)));
        reopened.client.search(SearchQuery::default());
        assert!(
            matches!(receive(&reopened),ArchiveReply::Search{records,..} if records.len()==1&&records[0].body=="new")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join("index.sqlite3"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn deleted_or_truncated_journal_does_not_leave_ghost_history_in_the_cache() {
        for removed in [true, false] {
            let root = Scratch::new();
            let service = ArchiveService::open(&root.0);
            assert!(
                service
                    .client
                    .upsert(record("#a", "removed history", "one"))
            );
            assert!(service.client.flush_barrier(Duration::from_secs(5)));
            drop(service);
            let journals: Vec<_> = fs::read_dir(root.0.join("archive"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .collect();
            assert_eq!(journals.len(), 1);
            if removed {
                fs::remove_file(&journals[0]).unwrap();
            } else {
                fs::write(&journals[0], b"").unwrap();
            }
            let reopened = ArchiveService::open(&root.0);
            assert!(reopened.client.flush_barrier(Duration::from_secs(5)));
            reopened.client.search(SearchQuery::default());
            assert!(
                matches!(receive(&reopened), ArchiveReply::Search { records, .. } if records.is_empty()),
                "projection retained a row after its only journal was removed={removed}"
            );
        }
    }

    #[test]
    fn accepted_escaped_message_survives_a_projection_rebuild() {
        let root = Scratch::new();
        let service = ArchiveService::open(&root.0);
        // Negotiated multiline payloads may be this large, and JSON escaping
        // makes the journal line much larger than the original message body.
        let body = "\\".repeat(MAX_ARCHIVE_LINE / 2 + 1024);
        assert!(service.client.upsert(record("#a", &body, "large")));
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        drop(service);
        for filename in ["index.sqlite3", "index.sqlite3-wal", "index.sqlite3-shm"] {
            match fs::remove_file(root.0.join("archive").join(filename)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove projection: {error}"),
            }
        }
        let reopened = ArchiveService::open(&root.0);
        assert!(reopened.client.flush_barrier(Duration::from_secs(5)));
        reopened.client.search(SearchQuery::default());
        assert!(
            matches!(receive(&reopened), ArchiveReply::Search { records, .. }
            if records.len() == 1 && records[0].body == body)
        );
    }
    #[test]
    fn same_size_and_mtime_journal_replacement_invalidates_the_projection() {
        let root = Scratch::new();
        let mut old = record("#a", "old", "one");
        old.id = "a".repeat(32);
        let service = ArchiveService::open(&root.0);
        assert!(service.client.upsert(old.clone()));
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        drop(service);
        let path = fs::read_dir(root.0.join("archive"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        old.id = "b".repeat(32);
        old.msgid = Some("two".into());
        old.body = "new".into();
        let mut bytes = serde_json::to_vec(&JournalEvent::Upsert {
            record: Box::new(old),
        })
        .unwrap();
        bytes.push(b'\n');
        assert_eq!(bytes.len() as u64, metadata.len());
        let mut file = File::create(&path).unwrap();
        file.write_all(&bytes).unwrap();
        file.set_modified(metadata.modified().unwrap()).unwrap();
        drop(file);
        let reopened = ArchiveService::open(&root.0);
        assert!(reopened.client.flush_barrier(Duration::from_secs(5)));
        reopened.client.search(SearchQuery::default());
        assert!(
            matches!(receive(&reopened), ArchiveReply::Search { records, .. }
            if records.len() == 1 && records[0].body == "new")
        );
    }

    #[test]
    fn broken_large_frames_never_project_partial_messages() {
        let root = Scratch::new();
        let path = root.0.join("framed.jsonl");
        let mut journal = ArchiveJournal {
            file: private_append(&path).unwrap(),
            dir: root.0.clone(),
            name: "framed.jsonl".into(),
            digest: Sha256::new(),
            previous: Vec::new(),
        };
        let event = JournalEvent::Upsert {
            record: Box::new(record(
                "#a",
                &"\\".repeat(MAX_ARCHIVE_LINE / 2 + 1024),
                "one",
            )),
        };
        assert!(journal.append(&event).is_ok());
        drop(journal);
        let original = fs::read(&path).unwrap();
        let lines: Vec<Vec<u8>> = original
            .split_inclusive(|byte| *byte == b'\n')
            .map(<[u8]>::to_vec)
            .collect();
        assert!(lines.len() > 2);
        for mutation in 0..4 {
            let mut changed = lines.clone();
            match mutation {
                0 => {
                    changed.pop();
                }
                1 => {
                    changed.swap(0, 1);
                }
                2 => {
                    for line in &mut changed {
                        let mut frame: serde_json::Value = serde_json::from_slice(line).unwrap();
                        frame["sha256"] = serde_json::Value::String("0".repeat(64));
                        *line = serde_json::to_vec(&frame).unwrap();
                        line.push(b'\n');
                    }
                }
                _ => {
                    let mut frame: serde_json::Value = serde_json::from_slice(&changed[0]).unwrap();
                    frame["total"] = serde_json::json!(MAX_JOURNAL_RECORD + 1);
                    changed[0] = serde_json::to_vec(&frame).unwrap();
                    changed[0].push(b'\n');
                }
            }
            fs::write(&path, changed.concat()).unwrap();
            let mut db = test_db();
            let client = ArchiveClient::default();
            replay_journal(&path, &db, &client).unwrap();
            assert!(
                all(&mut db).is_empty(),
                "mutation {mutation} produced a partial record"
            );
            assert!(client.status().error.is_some());
        }
    }

    #[test]
    fn an_entry_beyond_the_logical_limit_reports_failure_without_stopping_history() {
        let root = Scratch::new();
        let service = ArchiveService::open(&root.0);
        let body = "x".repeat(MAX_JOURNAL_RECORD);
        assert!(service.client.upsert(record("#a", &body, "too-large")));
        assert!(service.client.upsert(record("#a", "retained", "normal")));
        assert!(service.client.flush_barrier(Duration::from_secs(5)));
        assert!(
            service
                .client
                .status()
                .error
                .as_deref()
                .is_some_and(|error| error.contains("64 MiB"))
        );
        service.client.search(SearchQuery::default());
        assert!(
            matches!(receive(&service), ArchiveReply::Search { records, .. }
            if records.len() == 1 && records[0].body == "retained")
        );
    }

    #[test]
    fn network_pattern_is_bound_literal_and_reaction_removal_survives_late_upsert() {
        let mut db = test_db();
        let mut row = record("#a", "hi", "one");
        row.reactions = vec![super::super::message_ui::ReactionGroup {
            emoji: "ok".into(),
            actors: vec!["alice".into()],
        }];
        project(
            &db,
            &JournalEvent::Reaction {
                conversation: row.conversation.clone(),
                msgid: "one".into(),
                actor: "alice".into(),
                emoji: "ok".into(),
                present: false,
            },
        )
        .unwrap();
        project(
            &db,
            &JournalEvent::Upsert {
                record: Box::new(row),
            },
        )
        .unwrap();
        let client = ArchiveClient::default();
        let query = SearchQuery {
            network_pattern: "IRC.EXAMPLE".into(),
            ..Default::default()
        };
        let found = execute_search(&mut db, &query, &client, 0).unwrap().0;
        assert_eq!(found.len(), 1);
        assert!(found[0].reactions.is_empty());
        let query = SearchQuery {
            network_pattern: "' OR 1=1 --".into(),
            ..Default::default()
        };
        assert!(
            execute_search(&mut db, &query, &client, 0)
                .unwrap()
                .0
                .is_empty()
        );
    }
    #[test]
    fn live_redaction_masks_already_returned_and_inflight_results_before_worker_runs() {
        let client = ArchiveClient::default();
        let mut row = record("#a", "secret", "one");
        let reference = row.reference();
        client.redact(row.conversation.clone(), "one".into());
        assert!(client.is_redacted(&reference));
        client.apply_redactions(&mut row);
        assert!(row.redacted);
        assert!(row.body.is_empty());
        let other = record("#b", "untouched", "one");
        assert!(!client.is_redacted(&other.reference()));
    }
    #[test]
    fn context_neighbors_follow_time_when_history_arrives_after_live_rows() {
        let db = test_db();
        let mut reference = None;
        for (time, body) in [(300, "live"), (100, "old"), (200, "center")] {
            let mut row = record("#a", body, body);
            row.received_at = Some(time);
            if time == 200 {
                reference = Some(row.reference());
            }
            project(
                &db,
                &JournalEvent::Upsert {
                    record: Box::new(row),
                },
            )
            .unwrap();
        }
        let found = execute_context(&db, &reference.unwrap(), 1).unwrap();
        assert_eq!(
            found.iter().map(|r| r.body.as_str()).collect::<Vec<_>>(),
            ["old", "center", "live"]
        );
    }
    #[test]
    fn sqlite_scan_cancels_before_any_matching_row_is_produced() {
        let mut db = test_db();
        let client = ArchiveClient::default();
        client.cancel_query();
        let progress = QueryProgress::new(&mut db, &client, Some(0)).unwrap();
        let error=progress.0.query_row("WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM numbers WHERE n<10000000) SELECT sum(n) FROM numbers",[],|r|r.get::<_,i64>(0)).unwrap_err();
        assert_eq!(
            error.sqlite_error().unwrap().extended_code,
            rusqlite::ffi::SQLITE_INTERRUPT
        );
    }
    #[test]
    fn initial_import_defers_restore_until_late_anchor_has_been_indexed() {
        let root = Scratch::new();
        let mut bytes = Vec::new();
        let mut offset = 0;
        for index in 0..300 {
            if index == 299 {
                offset = bytes.len();
            }
            bytes.extend_from_slice(format!("[12:00] <alice> row{index}\n").as_bytes());
        }
        let path = root.log(&bytes);
        let last = &bytes[offset..];
        let generation = file_generation(&File::open(path).unwrap()).unwrap();
        let id = line_digest(
            format!("{generation}:{offset}:{}:{}", last.len(), line_digest(last)).as_bytes(),
        );
        let reference = MessageRef {
            conversation: ConversationKey {
                network: LogicalNetworkKey::Legacy {
                    source: "network".into(),
                },
                target: "#a".into(),
            },
            archive_id: Some(id),
            server_msgid: None,
            server_time: None,
        };
        let mut db = test_db();
        let (tx, rx) = mpsc::sync_channel(4);
        let (reply, responses) = mpsc::channel();
        let client = ArchiveClient::default();
        tx.send(Task::Restore(7, reference, 1)).unwrap();
        let mut deferred = VecDeque::new();
        import_legacy(
            &root.0.join("logs"),
            &mut db,
            &rx,
            &reply,
            &client,
            &mut None,
            &mut deferred,
        )
        .unwrap();
        assert_eq!(deferred.len(), 1);
        assert!(responses.try_recv().is_err());
        client.cancel_query();
        process_task(
            deferred.pop_front().unwrap(),
            &mut db,
            &mut None,
            &reply,
            &client,
        )
        .unwrap();
        assert!(
            matches!(responses.recv().unwrap(),ArchiveReply::Restored {token:7,records,..} if records.iter().any(|r|r.body=="row299"))
        );
    }
    #[test]
    fn redaction_by_live_message_key_masks_cached_pre_echo_row_without_retaining_all_rows() {
        let client = ArchiveClient::default();
        let mut row = record("#a", "pending text", "echo-id");
        let confirmed = row.reference();
        row.msgid = None;
        client.redact_reference(&confirmed);
        client.apply_redactions(&mut row);
        assert!(row.redacted);
        assert!(row.body.is_empty());
    }
    #[test]
    fn failed_startup_and_full_queue_complete_search_and_context_with_errors() {
        let root = Scratch::new();
        let path = root.0.join("not-a-directory");
        fs::write(&path, b"file").unwrap();
        let service = ArchiveService::open(&path);
        let generation = service.client.search(SearchQuery::default());
        assert!(matches!(receive(&service),ArchiveReply::Error {generation:g,..}if g==generation));
        assert!(!service.client.is_enabled());
        let generation = service
            .client
            .context(record("#a", "x", "x").reference(), 5);
        assert!(
            matches!(receive(&service),ArchiveReply::ContextError {generation:g,..}if g==generation)
        );
        let (tx, _rx) = mpsc::sync_channel(0);
        let full = ArchiveService {
            client: ArchiveClient {
                tx: Some(tx),
                available: Arc::new(AtomicBool::new(true)),
                ..Default::default()
            },
            replies: None,
            worker: None,
        };
        let generation = full.client.search(SearchQuery::default());
        assert!(matches!(receive(&full),ArchiveReply::Error{generation:g,..}if g==generation));
        let generation = full.client.context(record("#a", "x", "x").reference(), 5);
        assert!(
            matches!(receive(&full),ArchiveReply::ContextError{generation:g,..}if g==generation)
        );
    }
}
