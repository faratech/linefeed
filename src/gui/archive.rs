//! Durable message metadata and cancellable saved-history search.
//! The journal is authoritative; SQLite is a replaceable search projection.
use super::ChatMessage;
use super::identity::{ConversationKey, LogicalNetworkKey, MessageRef};
use super::logging::{LogSpan, file_generation, line_digest, line_record_id};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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
    pub offset: usize,
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
        more: bool,
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
    pub(crate) fn reaction(
        &self,
        conversation: ConversationKey,
        msgid: String,
        actor: String,
        emoji: String,
        present: bool,
    ) {
        self.submit(Task::Event(JournalEvent::Reaction {
            conversation,
            msgid,
            actor,
            emoji,
            present,
        }));
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
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
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

fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.busy_timeout(Duration::from_secs(2))?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        // This database is a cache. The journal and original text logs remain authoritative.
        db.execute_batch("DROP TABLE IF EXISTS record_search; DROP TABLE IF EXISTS records;
          DROP TABLE IF EXISTS tombstones; DROP TABLE IF EXISTS reactions; DROP TABLE IF EXISTS redirects;
          DROP TABLE IF EXISTS sources; DROP TABLE IF EXISTS legacy_sources; DROP TABLE IF EXISTS text_copies;")?;
    }
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
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

fn project(db: &Connection, event: &JournalEvent) -> Result<(), String> {
    db.execute_batch("SAVEPOINT archive_project")
        .map_err(|e| e.to_string())?;
    match project_inner(db, event) {
        Ok(()) => db
            .execute_batch("RELEASE archive_project")
            .map_err(|e| e.to_string()),
        Err(error) => {
            let _ = db.execute_batch("ROLLBACK TO archive_project; RELEASE archive_project");
            Err(error)
        }
    }
}
fn project_inner(db: &Connection, event: &JournalEvent) -> Result<(), String> {
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
                        return Ok(());
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
    Ok(())
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
) -> Result<(Vec<ArchiveRecord>, bool), String> {
    let progress = QueryProgress::new(db, cancel, Some(generation))?;
    let db = &*progress.0;
    let text = query.text.to_lowercase();
    let use_fts = text.chars().count() >= 3;
    let mut sql = String::from("SELECT r.data FROM records r ");
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
    sql.push_str("AND (?9='' OR instr(lower(r.network),?9)>0) ORDER BY r.occurred DESC,r.rowid DESC LIMIT 101 OFFSET ?8");
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
            query.offset.min(i64::MAX as usize) as i64,
            query.network_pattern.to_lowercase()
        ])
        .map_err(|e| e.to_string())?;
    let mut found = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if cancel.generation.load(Ordering::SeqCst) != generation {
            return Ok((Vec::new(), false));
        }
        let data: String = row.get(0).map_err(|e| e.to_string())?;
        let mut record = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        hydrate_reactions(db, &mut record)?;
        found.push(record);
    }
    let more = found.len() > 100;
    found.truncate(100);
    Ok((found, more))
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
    journal: &mut Option<File>,
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
        Task::Event(event) => {
            if let Some(file) = journal {
                serde_json::to_writer(&mut *file, &event).map_err(|e| e.to_string())?;
                file.write_all(b"\n").map_err(|e| e.to_string())?;
            }
            project(db, &event)?;
            // Exact count is refreshed at barriers, avoiding a table scan per incoming row.
        }
        Task::Flush(response) => {
            client.status.lock().unwrap().records =
                db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
                    .map_err(|e| e.to_string())? as usize;
            let ok = journal
                .as_mut()
                .is_none_or(|file| file.flush().and_then(|_| file.sync_data()).is_ok());
            let _ = response.send(ok);
        }
        Task::Search(generation, query) => {
            if client.generation.load(Ordering::SeqCst) == generation {
                match execute_search(db, &query, client, generation) {
                    Ok((records, more)) => {
                        let _ = reply.send(ArchiveReply::Search {
                            generation,
                            records,
                            more,
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
    journal: &mut Option<File>,
    client: &ArchiveClient,
) -> Result<(), String> {
    let pending = std::mem::take(&mut *client.pending_redactions.lock().unwrap());
    let mut pending = pending.into_iter();
    while let Some((conversation, msgid)) = pending.next() {
        let event = JournalEvent::Redact {
            conversation: conversation.clone(),
            msgid: msgid.clone(),
        };
        let result = (|| {
            if let Some(file) = journal.as_mut() {
                serde_json::to_writer(&mut *file, &event).map_err(|e| e.to_string())?;
                file.write_all(b"\n").map_err(|e| e.to_string())?;
            }
            project(db, &event)
        })();
        if let Err(error) = result {
            // Keep failed controls available without holding the GUI-facing
            // mutex during filesystem or SQLite work.
            let mut outbox = client.pending_redactions.lock().unwrap();
            outbox.insert((conversation, msgid));
            outbox.extend(pending);
            return Err(error);
        }
    }
    Ok(())
}

/// Read one physical line while bounding retained bytes even for corrupt or hostile files.
fn bounded_line(reader: &mut impl BufRead) -> std::io::Result<Option<(Vec<u8>, u64, bool)>> {
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
        if bytes.len().saturating_add(take) <= MAX_ARCHIVE_LINE && !oversized {
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
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
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
        for path in journals {
            let mut reader = BufReader::new(File::open(path).map_err(|e| e.to_string())?);
            while let Some((bytes, _, oversized)) =
                bounded_line(&mut reader).map_err(|e| e.to_string())?
            {
                if oversized {
                    client.status.lock().unwrap().error =
                        Some("An oversized history journal record was skipped".into());
                    continue;
                }
                match serde_json::from_slice::<JournalEvent>(&bytes) {
                    Ok(event) => project(&db, &event)?,
                    Err(_) => {
                        client.status.lock().unwrap().error =
                            Some("An incomplete history journal record was skipped".into())
                    }
                }
            }
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        journal = Some(
            private_append(&dir.join(format!("{stamp:020}-{}.jsonl", new_record_id())))
                .map_err(|e| e.to_string())?,
        );
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
        file.flush()
            .and_then(|_| file.sync_data())
            .map_err(|e| e.to_string())?;
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
    journal: &mut Option<File>,
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
