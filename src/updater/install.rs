//! Durable transactions for one canonical executable. Backups stay until a
//! healthy GUI launch; journal recovery inspects bytes, not just phase labels.
use super::platform;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    time::{Duration, Instant},
};

pub const MAX_BINARY: u64 = 128 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct Paths {
    pub target: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    pub fn discover() -> io::Result<Self> {
        let target = fs::canonicalize(std::env::current_exe()?)?;
        let key = hex(&Sha256::digest(target.to_string_lossy().as_bytes()));
        let base = dirs::data_local_dir()
            .ok_or_else(|| io::Error::other("No user data directory"))?
            .join("linefeed");
        fs::create_dir_all(&base)?;
        let updates = base.join("updates");
        ensure_private_dir(&updates)?;
        let state = updates.join(&key[..24]);
        ensure_private_dir(&state)?;
        Ok(Self { target, state })
    }
    pub fn journal(&self) -> PathBuf {
        self.state.join("transaction.json")
    }
    pub fn run_lock(&self) -> io::Result<File> {
        lock_file(&self.state.join("running.lock"))
    }
    pub fn transaction_lock(&self) -> io::Result<File> {
        lock_file(&self.state.join("transaction.lock"))
    }
}

pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    match platform::private_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path)?;
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err(io::Error::other(
                    "Update directory must be a real directory",
                ));
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    return Err(io::Error::other(
                        "Update directory cannot be a reparse point",
                    ));
                }
                // Repair directories created with the pre-0.1.3 OWNER RIGHTS
                // ACL. A non-elevated run cannot rewrite an Administrators-
                // owned directory, so say how to recover instead of failing
                // later with a bare "Access is denied".
                if let Err(e) = platform::secure_existing_dir(path) {
                    if e.kind() == io::ErrorKind::PermissionDenied {
                        return Err(io::Error::new(
                            e.kind(),
                            format!(
                                "Linefeed's update data at {} was created by a run as \
                                 administrator. Run Linefeed once as administrator to \
                                 repair it, or delete that folder.",
                                path.display()
                            ),
                        ));
                    }
                    tracing::warn!("Could not re-secure {}: {e}", path.display());
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if meta.uid() != unsafe { libc::geteuid() }
                    || meta.permissions().mode() & 0o077 != 0
                {
                    return Err(io::Error::other(
                        "Update directory is not private to this user",
                    ));
                }
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

pub fn nonce() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(hex(&bytes))
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn hash(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() > MAX_BINARY {
        return Err(io::Error::other("Executable exceeds update size limit"));
    }
    let mut digest = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(hex(&digest.finalize()))
}

/// Re-verify the staged candidate immediately before it is trusted again.
/// The probe can run for up to 30s, so bytes checked before it may have been
/// swapped by the time replacement happens (issue #198).
fn verify_candidate(tx: &Transaction) -> io::Result<()> {
    if hash(&tx.candidate())? != tx.new_hash {
        return Err(io::Error::other("Update bytes changed"));
    }
    platform::verify_signature(&tx.candidate())
}

/// Erase the credential-bearing session snapshot and restart intent when an
/// armed/prepared transaction is abandoned or downgraded (issue #200).
fn scrub_session_snapshot(tx: &mut Transaction) {
    tx.restart = false;
    let _ = fs::remove_file(tx.snapshot());
}

pub fn new_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}
pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(io::Error::other("Update state exceeds limit"));
    }
    serde_json::from_slice(&bytes).map_err(|_| io::Error::other("Invalid update state"))
}
pub fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", nonce()?));
    let result = (|| {
        let mut file = new_file(&tmp)?;
        serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        platform::sync_dir(path.parent().unwrap())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Phase {
    Staged,
    Prepared,
    Replaced,
    Healthy,
    RolledBack,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub paths: Paths,
    pub attempt: String,
    pub directory: PathBuf,
    pub version: String,
    pub old_hash: String,
    pub new_hash: String,
    pub phase: Phase,
    pub restart: bool,
    pub working_directory: PathBuf,
}
impl Transaction {
    pub fn candidate(&self) -> PathBuf {
        self.directory.join(if cfg!(windows) {
            "candidate.exe"
        } else {
            "candidate"
        })
    }
    pub fn backup(&self) -> PathBuf {
        self.directory.join("backup")
    }
    pub fn helper(&self) -> PathBuf {
        self.directory.join(if cfg!(windows) {
            "helper.exe"
        } else {
            "helper"
        })
    }
    pub fn snapshot(&self) -> PathBuf {
        self.paths
            .state
            .join(format!("{}.session.json", self.attempt))
    }
    pub fn save(&self) -> io::Result<()> {
        write_json(&self.paths.journal(), self)
    }
    /// Whether the attempt and staging directory are the ones this installation
    /// would have created. Anything derived from them is only safe to delete
    /// when this holds.
    fn validate_paths(&self) -> io::Result<()> {
        if self.attempt.len() != 32
            || !self.attempt.bytes().all(|b| b.is_ascii_hexdigit())
            || self.directory
                != self
                    .paths
                    .target
                    .parent()
                    .ok_or_else(|| io::Error::other("Invalid target"))?
                    .join(format!(".linefeed-update-{}", self.attempt))
        {
            return Err(io::Error::other("Invalid update transaction paths"));
        }
        Ok(())
    }
    pub fn validate(&self) -> io::Result<()> {
        self.validate_paths()?;
        ensure_private_dir(&self.directory)?;
        if hash(&self.helper())? != self.old_hash {
            return Err(io::Error::other("Update helper changed"));
        }
        Ok(())
    }
}

/// Named inside every staging directory: records which installation owns it
/// and, while locked, that a live download is still using it.
const CLAIM: &str = "owner.lock";

/// Create a staging directory and claim it. The returned file keeps the claim
/// locked until dropped, so a download killed by an exit or crash can later be
/// told apart from one still in progress (#270).
pub fn stage_directory(paths: &Paths) -> io::Result<(String, PathBuf, File)> {
    let attempt = nonce()?;
    let directory = paths
        .target
        .parent()
        .ok_or_else(|| io::Error::other("Invalid executable location"))?
        .join(format!(".linefeed-update-{attempt}"));
    platform::private_dir(&directory).map_err(|_| {
        io::Error::other("Cannot update this location; download the release manually")
    })?;
    let claim = (|| {
        let mut claim = new_file(&directory.join(CLAIM))?;
        claim.write_all(paths.state.to_string_lossy().as_bytes())?;
        claim.lock()?;
        Ok::<_, io::Error>(claim)
    })()
    .inspect_err(|_| {
        let _ = fs::remove_dir_all(&directory);
    })?;
    Ok((attempt, directory, claim))
}

/// Remove staging directories that downloads abandoned when the app exited or
/// crashed mid-download (#270). Only a directory carrying this installation's
/// claim with nothing holding its lock qualifies: the journal's own directory,
/// other installations' directories beside the same executable folder, and
/// directories from releases that predate claims are never touched. A damaged
/// journal skips the sweep because a directory may hold the only recovery copy.
fn sweep_orphans(paths: &Paths) {
    let keep = if paths.journal().exists() {
        match read_json::<Transaction>(&paths.journal()) {
            Ok(tx) => Some(tx.directory),
            Err(_) => return,
        }
    } else {
        None
    };
    let Some(Ok(entries)) = paths.target.parent().map(fs::read_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let is_staging = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix(".linefeed-update-"))
            .is_some_and(|attempt| {
                attempt.len() == 32 && attempt.bytes().all(|b| b.is_ascii_hexdigit())
            });
        // file_type() does not follow links, so a link is never swept.
        let is_directory = entry.file_type().is_ok_and(|t| t.is_dir());
        let path = entry.path();
        if is_staging && is_directory && keep.as_ref() != Some(&path) && abandoned(paths, &path)
        {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// Whether `directory` carries this installation's claim and no live download
/// holds it. The claim is released before the caller removes the directory.
fn abandoned(paths: &Paths, directory: &Path) -> bool {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let Ok(mut claim) = options.open(directory.join(CLAIM)) else {
        return false;
    };
    let mut owner = String::new();
    claim.try_lock().is_ok()
        && claim.read_to_string(&mut owner).is_ok()
        && owner == paths.state.to_string_lossy()
}
pub fn executable_copy(source: &Path, destination: &Path) -> io::Result<()> {
    let mut from = File::open(source)?;
    let mut to = new_file(destination)?;
    io::copy(&mut from, &mut to)?;
    to.set_permissions(from.metadata()?.permissions())?;
    to.sync_all()
}

// Held by main until process exit. Helpers acquire an exclusive lock only
// after all running instances have released their shared locks.
pub struct Startup {
    /// None when the updater could not initialize (see `update_error`).
    pub paths: Option<Paths>,
    pub running: Option<File>,
    pub acknowledgment: Option<Transaction>,
    pub snapshot: Option<crate::gui::UpdateSession>,
    pub update_error: Option<String>,
}

pub fn startup(supervised: Option<(PathBuf, String)>) -> io::Result<Startup> {
    startup_with(Paths::discover(), supervised, Duration::from_secs(90))
}

/// A normal launch must open the app even when the updater cannot start
/// (unusable update directory, lock contention): disable installation and say
/// why in About. A supervised update launch still fails closed so the helper
/// rolls back (#272).
fn startup_with(
    discovered: io::Result<Paths>,
    supervised: Option<(PathBuf, String)>,
    lock_wait: Duration,
) -> io::Result<Startup> {
    match discovered.and_then(|paths| start_updater(paths, supervised.as_ref(), lock_wait)) {
        Err(error) if supervised.is_none() => Ok(Startup {
            paths: None,
            running: None,
            acknowledgment: None,
            snapshot: None,
            update_error: Some(format!(
                "The updater could not start: {error}. Download a release manually."
            )),
        }),
        result => result,
    }
}

fn start_updater(
    paths: Paths,
    supervised: Option<&(PathBuf, String)>,
    lock_wait: Duration,
) -> io::Result<Startup> {
    let lock = paths.transaction_lock()?;
    if supervised.is_none() {
        timed_lock(&lock, false, lock_wait)?;
    }
    let (acknowledgment, snapshot, update_error) =
        match recover_startup(&paths, supervised) {
            Ok((acknowledgment, snapshot)) => (acknowledgment, snapshot, None),
            // A manually replaced executable or damaged journal must not strand a
            // working app. Preserve all recovery files and disable installation.
            Err(error) if supervised.is_none() => (
                None,
                None,
                Some(format!(
                    "Update recovery needs attention: {error}. Download a release manually."
                )),
            ),
            Err(error) => return Err(error),
        };
    if update_error.is_none() && supervised.is_none() {
        sweep_orphans(&paths);
    }
    let running = paths.run_lock()?;
    running.lock_shared()?;
    drop(lock);
    Ok(Startup {
        paths: Some(paths),
        running: Some(running),
        acknowledgment,
        snapshot,
        update_error,
    })
}

fn recover_startup(
    paths: &Paths,
    supervised: Option<&(PathBuf, String)>,
) -> io::Result<(Option<Transaction>, Option<crate::gui::UpdateSession>)> {
    let mut acknowledgment = None;
    let mut snapshot = None;
    if paths.journal().exists() {
        let mut tx: Transaction = read_json(&paths.journal())?;
        if tx.paths.target != paths.target || tx.paths.state != paths.state {
            return Err(io::Error::other(
                "Update state belongs to another installation",
            ));
        }
        let own_hash = hash(&paths.target)?;
        let validation = tx.validate();
        // Staged, Healthy and RolledBack journals hold nothing to recover. One
        // that no longer names the installed executable (a manual or package-
        // manager upgrade) or whose staging is gone is stale, not damaged:
        // discard it instead of failing every launch (#238). Prepared and
        // Replaced journals may hold the only copy of the old executable, so
        // they keep failing closed below.
        if supervised.is_none()
            && !matches!(tx.phase, Phase::Prepared | Phase::Replaced)
            && (validation.is_err() || !names_installed(&tx, &own_hash))
        {
            discard_stale(paths, &tx)?;
            return Ok((None, None));
        }
        validation?;
        if let Some((journal, token)) = supervised {
            if *journal != paths.journal()
                || *token != tx.attempt
                || !matches!(tx.phase, Phase::Replaced | Phase::RolledBack)
                || own_hash
                    != *if tx.phase == Phase::RolledBack {
                        &tx.old_hash
                    } else {
                        &tx.new_hash
                    }
            {
                return Err(io::Error::other("Invalid supervised update launch"));
            }
        } else {
            // Recover an interrupted replacement before opening the GUI.
            reconcile(&mut tx)?;
            if tx.phase == Phase::Staged && (tx.restart || tx.snapshot().exists()) {
                scrub_session_snapshot(&mut tx);
                tx.save()?;
            }
        }
        if tx.restart
            && matches!(tx.phase, Phase::Replaced | Phase::RolledBack)
            && tx.snapshot().exists()
        {
            snapshot = Some(read_json(&tx.snapshot())?);
        }
        if matches!(tx.phase, Phase::Replaced | Phase::RolledBack) {
            acknowledgment = Some(tx);
        }
    } else if supervised.is_some() {
        return Err(io::Error::other("Missing update transaction"));
    }
    Ok((acknowledgment, snapshot))
}

/// Whether a journal in a non-recovery phase still describes the installed
/// executable.
fn names_installed(tx: &Transaction, installed: &str) -> bool {
    match tx.phase {
        Phase::Healthy => installed == tx.old_hash || installed == tx.new_hash,
        _ => installed == tx.old_hash,
    }
}

/// Drop a stale journal with its session snapshot and staging directory. Both
/// are derived from the journal, so they are only touched when the journal
/// names the paths this installation would have created. Directory removal is
/// best effort, as for a Healthy journal: Windows cannot delete a running
/// helper, and a leftover directory is collected on a later launch.
fn discard_stale(paths: &Paths, tx: &Transaction) -> io::Result<()> {
    if tx.validate_paths().is_ok() {
        let _ = fs::remove_file(tx.snapshot());
        let _ = fs::remove_dir_all(&tx.directory);
    }
    fs::remove_file(paths.journal())
}

pub fn timed_lock(file: &File, shared: bool, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let result = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match result {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "another Linefeed instance is busy updating",
                ));
            }
            Err(e) => return Err(io::Error::other(e)),
        }
    }
}

fn reconcile(tx: &mut Transaction) -> io::Result<()> {
    let target = hash(&tx.paths.target).ok();
    if target.as_deref() == Some(&tx.new_hash) {
        if matches!(tx.phase, Phase::Prepared | Phase::Replaced) {
            tx.phase = Phase::Replaced;
            tx.save()?;
        }
        return Ok(());
    }
    if target.as_deref() == Some(&tx.old_hash) {
        if matches!(tx.phase, Phase::Prepared | Phase::Replaced) {
            // A parent crash before the graceful-exit marker preserves a
            // completely staged candidate for the next actual exit.
            let downgraded = tx.phase == Phase::Prepared
                && !tx.backup().exists()
                && hash(&tx.candidate()).ok().as_deref() == Some(&tx.new_hash);
            tx.phase = if downgraded {
                Phase::Staged
            } else {
                Phase::RolledBack
            };
            if downgraded {
                scrub_session_snapshot(tx);
            }
            tx.save()?;
        }
        return Ok(());
    }
    // Never replace a newer installation that does not match this attempt.
    if target.is_some() {
        return Err(io::Error::other("Executable changed outside this update"));
    }
    if hash(&tx.backup()).ok().as_deref() != Some(&tx.old_hash) {
        return Err(io::Error::other("Update recovery backup is unavailable"));
    }
    fs::rename(tx.backup(), &tx.paths.target)?;
    platform::sync_dir(tx.paths.target.parent().unwrap())?;
    tx.phase = Phase::RolledBack;
    tx.save()
}

pub fn acknowledge(tx: &Transaction) -> io::Result<()> {
    let expected = if tx.phase == Phase::RolledBack {
        &tx.old_hash
    } else {
        &tx.new_hash
    };
    if hash(&std::env::current_exe()?)? != *expected {
        return Err(io::Error::other("Startup executable does not match update"));
    }
    write_json(
        &tx.directory.join("healthy.json"),
        &(tx.attempt.clone(), expected.clone()),
    )
}

pub struct Armed {
    pub child: Child,
    pub input: ChildStdin,
}
impl Armed {
    pub fn commit(mut self) {
        // This line is sent only after run_native returned successfully. EOF
        // without it (a crash) leaves the old installation untouched.
        let _ = self.input.write_all(b"apply\n");
        drop(self.input);
        // Reap the helper without blocking normal application shutdown.
        std::thread::spawn(move || {
            let _ = self.child.wait();
        });
    }
}

pub fn arm(mut tx: Transaction, snapshot: Option<&crate::gui::UpdateSession>) -> io::Result<Armed> {
    let lock = tx.paths.transaction_lock()?;
    lock.try_lock().map_err(io::Error::other)?;
    let persisted: Transaction = read_json(&tx.paths.journal())?;
    if persisted.attempt != tx.attempt || persisted.phase != Phase::Staged {
        return Err(io::Error::other("Update is no longer ready"));
    }
    tx.validate()?;
    if hash(&tx.paths.target)? != tx.old_hash {
        return Err(io::Error::other("Update bytes changed"));
    }
    verify_candidate(&tx)?;
    tx.restart = snapshot.is_some();
    if let Some(snapshot) = snapshot {
        write_json(&tx.snapshot(), snapshot)?;
    } else {
        // No restart was requested: drop any snapshot an abandoned arm left.
        let _ = fs::remove_file(tx.snapshot());
    }
    // Persist restart intent while still Staged. If spawning the helper fails,
    // the user can retry without leaving an orphaned Prepared transaction.
    tx.save()?;
    let _ = fs::remove_file(tx.directory.join("ready"));
    let mut command = Command::new(tx.helper());
    command
        .arg("--internal-update-helper")
        .arg(tx.paths.journal())
        .arg(std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    quiet(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            scrub_session_snapshot(&mut tx);
            tx.phase = Phase::Staged;
            let _ = tx.save();
            return Err(error);
        }
    };
    let Some(input) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        scrub_session_snapshot(&mut tx);
        tx.phase = Phase::Staged;
        let _ = tx.save();
        return Err(io::Error::other("No helper input"));
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !tx.directory.join("ready").exists() {
        if child.try_wait()?.is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            scrub_session_snapshot(&mut tx);
            tx.phase = Phase::Staged;
            tx.save()?;
            return Err(io::Error::other("Update helper could not start"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Re-hash after the helper-ready wait: bytes verified above could have
    // been swapped while the helper was starting (issue #198).
    if let Err(error) = verify_candidate(&tx) {
        let _ = child.kill();
        let _ = child.wait();
        scrub_session_snapshot(&mut tx);
        tx.phase = Phase::Staged;
        let _ = tx.save();
        return Err(error);
    }
    tx.phase = Phase::Prepared;
    tx.save()?;
    drop(lock);
    Ok(Armed { child, input })
}

pub fn quiet(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn wait_child(child: &mut Child, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn early_mode() -> Option<io::Result<()>> {
    let args: Vec<_> = std::env::args_os().collect();
    match args.get(1)?.to_str()? {
        "--internal-update-probe" => Some((|| {
            if args.len() != 3 || args[2] != env!("CARGO_PKG_VERSION") {
                return Err(io::Error::other("Unexpected update version"));
            }
            Ok(())
        })()),
        "--internal-update-helper" => Some((|| {
            if args.len() != 4 {
                return Err(io::Error::other("Invalid helper arguments"));
            }
            let parent = args[3]
                .to_str()
                .and_then(|v| v.parse::<u32>().ok())
                .ok_or_else(|| io::Error::other("Invalid parent process"))?;
            helper(Path::new(&args[2]), parent)
        })()),
        _ => None,
    }
}

pub fn supervised_args() -> io::Result<Option<(PathBuf, String)>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|v| v == "--internal-update-launch") {
        if args.len() != 4 {
            return Err(io::Error::other("Invalid update launch arguments"));
        }
        return Ok(Some((
            PathBuf::from(&args[2]),
            args[3]
                .to_str()
                .ok_or_else(|| io::Error::other("Invalid update token"))?
                .to_owned(),
        )));
    }
    Ok(None)
}

fn helper(journal: &Path, parent: u32) -> io::Result<()> {
    let mut tx: Transaction = read_json(journal)?;
    tx.validate()?;
    if journal != tx.paths.journal() || hash(&std::env::current_exe()?)? != tx.old_hash {
        return Err(io::Error::other("Invalid helper identity"));
    }
    #[cfg(windows)]
    let parent_handle = unsafe {
        windows::Win32::System::Threading::OpenProcess(
            windows::Win32::System::Threading::PROCESS_SYNCHRONIZE,
            false,
            parent,
        )
        .map_err(io::Error::other)?
    };
    #[cfg(not(windows))]
    let _ = parent;
    new_file(&tx.directory.join("ready"))?.sync_all()?;
    let mut input = Vec::new();
    io::stdin().take(16).read_to_end(&mut input)?;
    if input != b"apply\n" {
        return Ok(());
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{
            Foundation::{CloseHandle, WAIT_OBJECT_0},
            System::Threading::WaitForSingleObject,
        };
        let result = WaitForSingleObject(parent_handle, 30_000);
        let _ = CloseHandle(parent_handle);
        if result != WAIT_OBJECT_0 {
            return Err(io::Error::other("Parent did not exit"));
        }
    }
    let result = apply_transaction(&mut tx);
    if let Err(error) = &result {
        let _ = write_json(
            &tx.paths.state.join("result.json"),
            &format!("Update could not complete: {error}"),
        );
        relaunch_after_failure(&mut tx, journal)?;
    }
    result
}

/// A failure before replacement must reopen the old app when restart was
/// requested. All locks in apply_transaction have been released by now.
fn relaunch_after_failure(tx: &mut Transaction, journal: &Path) -> io::Result<()> {
    let recovery_lock = tx.paths.transaction_lock()?;
    timed_lock(&recovery_lock, false, Duration::from_secs(30))?;
    let latest: Option<Transaction> = read_json(journal).ok();
    if tx.restart
        && latest
            .as_ref()
            .is_some_and(|latest| latest.attempt == tx.attempt && latest.phase != Phase::Healthy)
        && hash(&tx.paths.target).ok().as_deref() == Some(&tx.old_hash)
    {
        tx.phase = Phase::RolledBack;
        tx.save()?;
        launch_fallback(tx)?;
    }
    Ok(())
}

/// Return an unreplaced transaction to Staged after a failure. When a restart
/// was requested, keep that intent and its session snapshot: `helper` reopens
/// the old app with them (#239). Otherwise nothing will consume the snapshot,
/// so erase it (#200).
fn abandon_before_replace(tx: &mut Transaction) {
    if !tx.restart {
        scrub_session_snapshot(tx);
    }
    tx.phase = Phase::Staged;
}

fn apply_transaction(tx: &mut Transaction) -> io::Result<()> {
    let transaction_lock = tx.paths.transaction_lock()?;
    timed_lock(&transaction_lock, false, Duration::from_secs(30))?;
    let running = tx.paths.run_lock()?;
    if timed_lock(&running, false, Duration::from_secs(30)).is_err() {
        abandon_before_replace(tx);
        tx.save()?;
        return Err(io::Error::other(
            "Another instance is still running; update deferred",
        ));
    }
    let latest: Transaction = read_json(&tx.paths.journal())?;
    if latest.attempt != tx.attempt
        || latest.phase != Phase::Prepared
        || hash(&tx.paths.target)? != tx.old_hash
        || hash(&tx.candidate())? != tx.new_hash
    {
        return Err(io::Error::other("Update transaction changed"));
    }
    tx.phase = Phase::Prepared;
    platform::verify_signature(&tx.candidate())?;
    // Check the embedded version and runtime dependencies before touching target.
    let mut probe = Command::new(tx.candidate());
    probe
        .arg("--internal-update-probe")
        .arg(&tx.version)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    quiet(&mut probe);
    if !wait_child(&mut probe.spawn()?, Duration::from_secs(30))? {
        abandon_before_replace(tx);
        tx.save()?;
        write_json(&tx.paths.state.join("failed.json"), &tx.version)?;
        return Err(io::Error::other("Update validation failed"));
    }
    // The probe above may have run for 30s; re-hash (and re-verify the
    // Windows signature) immediately before replacement (issue #198).
    verify_candidate(tx)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match platform::replace(&tx.paths.target, &tx.candidate(), Some(&tx.backup())) {
            Ok(()) => break,
            Err(e) => {
                // ReplaceFileW can move the original to backup on failure.
                let target_hash = hash(&tx.paths.target).ok();
                if target_hash.as_deref() == Some(&tx.new_hash) {
                    break;
                }
                if target_hash.as_deref() != Some(&tx.old_hash) {
                    reconcile(tx)?;
                }
                if tx.phase == Phase::RolledBack || Instant::now() >= deadline {
                    if tx.phase != Phase::RolledBack {
                        tx.phase = Phase::Staged;
                        tx.save()?;
                    }
                    write_json(
                        &tx.paths.state.join("result.json"),
                        &format!("Update installation failed: {e}"),
                    )?;
                    if tx.restart {
                        tx.phase = Phase::RolledBack;
                        tx.save()?;
                    }
                    return Err(e);
                }
                // A backup copy from an unsuccessful Unix attempt may exist.
                let _ = fs::remove_file(tx.backup());
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
    tx.phase = Phase::Replaced;
    tx.save()?;
    running.unlock()?;
    if tx.restart {
        let mut child = launch(tx).ok();
        let good = if let Some(child) = &mut child {
            wait_healthy(tx, child).unwrap_or(false)
        } else {
            false
        };
        if !good {
            if let Some(child) = &mut child {
                let _ = child.kill();
                let _ = child.wait();
            }
            timed_lock(&running, false, Duration::from_secs(30))?;
            rollback(tx)?;
            // The helper wrapper releases apply locks and relaunches the old
            // GUI once. Keeping fallback in one place prevents duplicate
            // launches when the old GUI also fails its startup acknowledgment.
            return Err(io::Error::other(
                "Update failed to start; the previous version was restored",
            ));
        } else {
            finish(tx)?;
        }
    } else {
        let mut probe = Command::new(&tx.paths.target);
        probe
            .arg("--internal-update-probe")
            .arg(&tx.version)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        quiet(&mut probe);
        let good = probe
            .spawn()
            .and_then(|mut c| wait_child(&mut c, Duration::from_secs(30)))
            .unwrap_or(false);
        if !good {
            timed_lock(&running, false, Duration::from_secs(30))?;
            rollback(tx)?;
        }
    }
    Ok(())
}

fn launch(tx: &Transaction) -> io::Result<Child> {
    let mut command = Command::new(&tx.paths.target);
    command
        .arg("--internal-update-launch")
        .arg(tx.paths.journal())
        .arg(&tx.attempt)
        .current_dir(if tx.working_directory.is_dir() {
            &tx.working_directory
        } else {
            tx.paths.target.parent().unwrap()
        })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    quiet(&mut command);
    command.spawn()
}
fn wait_healthy(tx: &Transaction, child: &mut Child) -> io::Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let expected = if tx.phase == Phase::RolledBack {
        &tx.old_hash
    } else {
        &tx.new_hash
    };
    loop {
        if let Ok((attempt, digest)) =
            read_json::<(String, String)>(&tx.directory.join("healthy.json"))
            && attempt == tx.attempt
            && digest == *expected
        {
            return Ok(true);
        }
        if child.try_wait()?.is_some() || Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn rollback(tx: &mut Transaction) -> io::Result<()> {
    if hash(&tx.paths.target)? != tx.new_hash || hash(&tx.backup())? != tx.old_hash {
        return Err(io::Error::other("Rollback files changed"));
    }
    platform::replace(&tx.paths.target, &tx.backup(), None)?;
    tx.phase = Phase::RolledBack;
    tx.save()?;
    write_json(&tx.paths.state.join("failed.json"), &tx.version)?;
    write_json(
        &tx.paths.state.join("result.json"),
        &"Update failed to start; the previous version was restored",
    )
}
fn launch_fallback(tx: &Transaction) -> io::Result<()> {
    let mut child = launch(tx)?;
    if wait_healthy(tx, &mut child)? {
        let mut tx = tx.clone();
        finish(&mut tx)?;
    }
    Ok(())
}
pub fn finish(tx: &mut Transaction) -> io::Result<()> {
    if tx.phase == Phase::Replaced
        && read_json::<String>(&tx.paths.state.join("failed.json"))
            .ok()
            .as_deref()
            == Some(&tx.version)
    {
        let _ = fs::remove_file(tx.paths.state.join("failed.json"));
    }
    let _ = fs::remove_file(tx.snapshot());
    tx.phase = Phase::Healthy;
    tx.save()?;
    let _ = fs::remove_file(tx.backup());
    // Windows cannot remove the helper while it is executing. The next normal
    // launch cleans the remaining directory after observing Healthy.
    Ok(())
}

pub fn confirm_startup(tx: &Transaction) -> io::Result<()> {
    acknowledge(tx)?;
    let lock = tx.paths.transaction_lock()?;
    if lock.try_lock().is_ok() {
        let latest: Transaction = read_json(&tx.paths.journal())?;
        if latest.attempt == tx.attempt
            && matches!(latest.phase, Phase::Replaced | Phase::RolledBack)
        {
            let mut latest = latest;
            finish(&mut latest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        tx: Transaction,
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("linefeed-update-test-{}", nonce().unwrap()));
            platform::private_dir(&root).unwrap();
            let paths = Paths {
                target: root.join("renamed app"),
                state: root.join("state"),
            };
            platform::private_dir(&paths.state).unwrap();
            fs::write(&paths.target, b"original executable").unwrap();
            let (attempt, directory, _claim) = stage_directory(&paths).unwrap();
            let tx = Transaction {
                paths,
                attempt,
                directory,
                version: "0.0.10".into(),
                old_hash: hex(&Sha256::digest(b"original executable")),
                new_hash: hex(&Sha256::digest(b"new executable")),
                phase: Phase::Staged,
                restart: false,
                working_directory: root.clone(),
            };
            fs::write(tx.candidate(), b"new executable").unwrap();
            fs::write(tx.helper(), b"original executable").unwrap();
            tx.save().unwrap();
            Self { tx, root }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn replacement_keeps_complete_old_backup_and_complete_new_target() {
        let fixture = Fixture::new();
        let tx = &fixture.tx;
        platform::replace(&tx.paths.target, &tx.candidate(), Some(&tx.backup())).unwrap();
        assert_eq!(hash(&tx.paths.target).unwrap(), tx.new_hash);
        assert_eq!(hash(&tx.backup()).unwrap(), tx.old_hash);
        assert!(!tx.candidate().exists());
    }
    #[test]
    fn interruption_before_apply_leaves_staged_candidate() {
        let mut fixture = Fixture::new();
        let tx = &mut fixture.tx;
        tx.phase = Phase::Prepared;
        tx.save().unwrap();
        reconcile(tx).unwrap();
        assert!(tx.phase == Phase::Staged);
        assert_eq!(hash(&tx.paths.target).unwrap(), tx.old_hash);
        assert_eq!(hash(&tx.candidate()).unwrap(), tx.new_hash);
    }
    #[test]
    fn downgrade_to_staged_scrubs_session_snapshot() {
        let mut fixture = Fixture::new();
        let tx = &mut fixture.tx;
        tx.phase = Phase::Prepared;
        tx.restart = true;
        tx.save().unwrap();
        write_json(&tx.snapshot(), &"unsent draft").unwrap();
        reconcile(tx).unwrap();
        assert!(tx.phase == Phase::Staged);
        assert!(!tx.restart);
        assert!(!tx.snapshot().exists());
    }
    #[test]
    fn tampered_candidate_fails_reverification() {
        let fixture = Fixture::new();
        verify_candidate(&fixture.tx).unwrap();
        fs::write(fixture.tx.candidate(), b"swapped executable").unwrap();
        assert!(verify_candidate(&fixture.tx).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn failed_helper_start_scrubs_session_snapshot() {
        let fixture = Fixture::new();
        write_json(&fixture.tx.snapshot(), &"unsent draft").unwrap();
        // The fixture helper is not executable, so spawning it fails.
        assert!(arm(fixture.tx.clone(), None).is_err());
        assert!(!fixture.tx.snapshot().exists());
    }
    #[test]
    fn interruption_after_replace_recovers_from_bytes_even_with_old_journal_phase() {
        let mut fixture = Fixture::new();
        let tx = &mut fixture.tx;
        tx.phase = Phase::Prepared;
        tx.save().unwrap();
        platform::replace(&tx.paths.target, &tx.candidate(), Some(&tx.backup())).unwrap();
        let mut persisted: Transaction = read_json(&tx.paths.journal()).unwrap();
        reconcile(&mut persisted).unwrap();
        assert!(persisted.phase == Phase::Replaced);
        assert_eq!(hash(&persisted.backup()).unwrap(), persisted.old_hash);
    }
    #[test]
    fn partial_replacement_restores_missing_target_from_backup() {
        let mut fixture = Fixture::new();
        let tx = &mut fixture.tx;
        tx.phase = Phase::Prepared;
        fs::rename(&tx.paths.target, tx.backup()).unwrap();
        reconcile(tx).unwrap();
        assert!(tx.phase == Phase::RolledBack);
        assert_eq!(hash(&tx.paths.target).unwrap(), tx.old_hash);
    }
    #[test]
    fn stale_generation_never_overwrites_external_newer_installation() {
        let mut fixture = Fixture::new();
        let tx = &mut fixture.tx;
        tx.phase = Phase::Replaced;
        fs::write(tx.backup(), b"original executable").unwrap();
        fs::write(&tx.paths.target, b"a newer installation").unwrap();
        assert!(reconcile(tx).is_err());
        assert!(rollback(tx).is_err());
        assert_eq!(fs::read(&tx.paths.target).unwrap(), b"a newer installation");
    }
    #[test]
    fn startup_rejects_unrelated_executable_without_deleting_recovery_files() {
        // Prepared and Replaced journals can hold the only copy of the old
        // executable, so they keep failing closed.
        for phase in [Phase::Prepared, Phase::Replaced] {
            let mut fixture = Fixture::new();
            fixture.tx.phase = phase;
            fixture.tx.save().unwrap();
            fs::write(&fixture.tx.paths.target, b"manual installation").unwrap();
            assert!(recover_startup(&fixture.tx.paths, None).is_err());
            assert_eq!(
                fs::read(&fixture.tx.paths.target).unwrap(),
                b"manual installation"
            );
            assert!(fixture.tx.paths.journal().exists());
            assert!(fixture.tx.candidate().exists());
        }
    }
    #[test]
    fn spent_journals_for_a_replaced_executable_are_discarded_not_fatal() {
        // A manual or package-manager upgrade leaves a journal that matches
        // neither hash; with nothing to recover it must not disable the
        // updater on every launch (#238).
        for phase in [Phase::Staged, Phase::Healthy, Phase::RolledBack] {
            let mut fixture = Fixture::new();
            fixture.tx.phase = phase;
            fixture.tx.save().unwrap();
            write_json(&fixture.tx.snapshot(), &"unsent draft").unwrap();
            fs::write(&fixture.tx.paths.target, b"a newer installation").unwrap();
            let (acknowledgment, snapshot) = recover_startup(&fixture.tx.paths, None).unwrap();
            assert!(acknowledgment.is_none() && snapshot.is_none());
            assert_eq!(
                fs::read(&fixture.tx.paths.target).unwrap(),
                b"a newer installation"
            );
            assert!(!fixture.tx.paths.journal().exists());
            assert!(!fixture.tx.directory.exists());
            assert!(!fixture.tx.snapshot().exists());
        }
    }
    #[test]
    fn journal_matching_the_installed_executable_is_kept() {
        for (phase, installed) in [
            (Phase::Staged, "old"),
            (Phase::RolledBack, "old"),
            (Phase::Healthy, "old"),
            (Phase::Healthy, "new"),
        ] {
            let mut fixture = Fixture::new();
            fixture.tx.phase = phase;
            fixture.tx.save().unwrap();
            if installed == "new" {
                fs::write(&fixture.tx.paths.target, b"new executable").unwrap();
            }
            recover_startup(&fixture.tx.paths, None).unwrap();
            assert!(fixture.tx.paths.journal().exists());
            assert!(fixture.tx.directory.exists());
        }
    }
    #[test]
    fn staged_journal_for_an_already_installed_candidate_is_discarded() {
        let fixture = Fixture::new();
        fs::write(&fixture.tx.paths.target, b"new executable").unwrap();
        recover_startup(&fixture.tx.paths, None).unwrap();
        assert!(!fixture.tx.paths.journal().exists());
    }
    #[test]
    fn staged_journal_whose_staging_was_deleted_is_discarded() {
        // The hidden staging directory was removed, or the helper quarantined,
        // so validate() can never pass again (#238).
        for remove_directory in [true, false] {
            let fixture = Fixture::new();
            if remove_directory {
                fs::remove_dir_all(&fixture.tx.directory).unwrap();
            } else {
                fs::remove_file(fixture.tx.helper()).unwrap();
            }
            recover_startup(&fixture.tx.paths, None).unwrap();
            assert!(!fixture.tx.paths.journal().exists());
            assert_eq!(hash(&fixture.tx.paths.target).unwrap(), fixture.tx.old_hash);
        }
        // A Replaced journal keeps failing closed: its backup lives there.
        let mut fixture = Fixture::new();
        fixture.tx.phase = Phase::Replaced;
        fixture.tx.save().unwrap();
        fs::remove_file(fixture.tx.helper()).unwrap();
        assert!(recover_startup(&fixture.tx.paths, None).is_err());
        assert!(fixture.tx.paths.journal().exists());
    }
    #[test]
    fn damaged_journal_paths_are_never_discarded() {
        let mut fixture = Fixture::new();
        let outside = fixture.root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious"), b"data").unwrap();
        fixture.tx.directory = outside.clone();
        fixture.tx.save().unwrap();
        fs::write(&fixture.tx.paths.target, b"a newer installation").unwrap();
        let _ = recover_startup(&fixture.tx.paths, None);
        assert!(outside.join("precious").exists());
    }
    #[test]
    fn corrupt_journal_is_preserved_for_manual_recovery() {
        let fixture = Fixture::new();
        fs::write(fixture.tx.paths.journal(), b"invalid journal").unwrap();
        assert!(recover_startup(&fixture.tx.paths, None).is_err());
        assert_eq!(hash(&fixture.tx.paths.target).unwrap(), fixture.tx.old_hash);
        assert_eq!(
            fs::read(fixture.tx.paths.journal()).unwrap(),
            b"invalid journal"
        );
    }
    /// Sweep until `directory` disappears: a handle another test's fork keeps
    /// alive for an instant can make a just-released claim look busy.
    fn swept_eventually(paths: &Paths, directory: &Path) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while directory.exists() && Instant::now() < deadline {
            sweep_orphans(paths);
            std::thread::sleep(Duration::from_millis(20));
        }
        !directory.exists()
    }
    #[test]
    fn staging_abandoned_by_an_interrupted_download_is_swept() {
        // A download killed mid-way (exit or crash) leaves its directory and a
        // partial file beside the executable (#270).
        let fixture = Fixture::new();
        let (_, orphan, claim) = stage_directory(&fixture.tx.paths).unwrap();
        fs::write(orphan.join("download.part"), vec![0u8; 4096]).unwrap();
        // While the download is alive its claim protects the directory.
        sweep_orphans(&fixture.tx.paths);
        assert!(orphan.join("download.part").exists());
        drop(claim);
        assert!(swept_eventually(&fixture.tx.paths, &orphan));
        // The staged update the journal points to is never swept.
        assert!(fixture.tx.directory.exists());
        fixture.tx.validate().unwrap();
    }
    #[test]
    fn startup_sweeps_abandoned_staging_but_not_after_a_damaged_journal() {
        let fixture = Fixture::new();
        let (_, orphan, claim) = stage_directory(&fixture.tx.paths).unwrap();
        drop(claim);
        let startup = startup_with(Ok(fixture.tx.paths.clone()), None, Duration::from_secs(1));
        assert!(startup.unwrap().update_error.is_none());
        assert!(swept_eventually(&fixture.tx.paths, &orphan));

        // A damaged journal means a staging directory may hold the only copy
        // of the old executable: nothing is deleted.
        let (_, kept, claim) = stage_directory(&fixture.tx.paths).unwrap();
        drop(claim);
        fs::write(fixture.tx.paths.journal(), b"invalid journal").unwrap();
        let startup = startup_with(Ok(fixture.tx.paths.clone()), None, Duration::from_secs(1));
        assert!(startup.unwrap().update_error.is_some());
        sweep_orphans(&fixture.tx.paths);
        assert!(kept.exists());
    }
    #[test]
    fn sweep_leaves_foreign_unclaimed_and_linked_directories_alone() {
        let fixture = Fixture::new();
        let paths = &fixture.tx.paths;
        let parent = paths.target.parent().unwrap();
        // Another installation beside the same folder (a renamed copy).
        let other = Paths {
            target: paths.target.clone(),
            state: fixture.root.join("other-state"),
        };
        let (_, foreign, claim) = stage_directory(&other).unwrap();
        drop(claim);
        // Not created by a claim-aware release: nothing says it is ours.
        let unclaimed = parent.join(format!(".linefeed-update-{}", nonce().unwrap()));
        platform::private_dir(&unclaimed).unwrap();
        // Not a staging name at all.
        let unrelated = parent.join(".linefeed-update-notes");
        platform::private_dir(&unrelated).unwrap();
        // A link to a claimed directory is never followed.
        let (_, target_dir, claim) = stage_directory(paths).unwrap();
        drop(claim);
        #[cfg(unix)]
        let link = {
            let link = parent.join(format!(".linefeed-update-{}", nonce().unwrap()));
            std::os::unix::fs::symlink(&target_dir, &link).unwrap();
            link
        };
        sweep_orphans(paths);
        for kept in [&foreign, &unclaimed, &unrelated] {
            assert!(kept.exists());
        }
        assert!(swept_eventually(paths, &target_dir));
        #[cfg(unix)]
        assert!(fs::symlink_metadata(&link).is_ok());
    }
    #[test]
    fn updater_infrastructure_failure_does_not_stop_a_normal_launch() {
        // e.g. a 0755 or foreign-owned update directory (#272).
        let failure = || Err(io::Error::other("Update directory is not private to this user"));
        let startup = startup_with(failure(), None, Duration::from_secs(1)).unwrap();
        assert!(startup.paths.is_none() && startup.running.is_none());
        assert!(startup.update_error.unwrap().contains("not private"));
        // A supervised update launch still fails closed so the helper rolls back.
        let supervised = Some((PathBuf::from("transaction.json"), "token".to_owned()));
        assert!(startup_with(failure(), supervised, Duration::from_secs(1)).is_err());
    }
    #[test]
    fn busy_transaction_lock_does_not_stop_a_normal_launch() {
        let fixture = Fixture::new();
        let held = fixture.tx.paths.transaction_lock().unwrap();
        held.lock().unwrap();
        let paths = fixture.tx.paths.clone();
        let startup = startup_with(Ok(paths), None, Duration::from_millis(200)).unwrap();
        assert!(startup.paths.is_none());
        assert!(startup.update_error.is_some());
    }
    #[test]
    fn healthy_startup_keeps_the_updater_and_holds_no_transaction_lock() {
        let fixture = Fixture::new();
        let paths = fixture.tx.paths.clone();
        let startup = startup_with(Ok(paths.clone()), None, Duration::from_secs(1)).unwrap();
        assert!(startup.paths.is_some() && startup.running.is_some());
        assert!(startup.update_error.is_none());
        // A concurrently forked child (other tests spawn probes) can briefly
        // keep the closed handle's lock alive, so wait rather than try once.
        timed_lock(&paths.transaction_lock().unwrap(), false, Duration::from_secs(5)).unwrap();
    }
    #[test]
    fn rollback_preserves_snapshot_and_suppresses_failed_version() {
        let mut fixture = Fixture::new();
        let tx = &mut fixture.tx;
        write_json(&tx.snapshot(), &"unsent text").unwrap();
        platform::replace(&tx.paths.target, &tx.candidate(), Some(&tx.backup())).unwrap();
        tx.phase = Phase::Replaced;
        tx.save().unwrap();
        rollback(tx).unwrap();
        assert_eq!(hash(&tx.paths.target).unwrap(), tx.old_hash);
        assert_eq!(read_json::<String>(&tx.snapshot()).unwrap(), "unsent text");
        assert_eq!(
            read_json::<String>(&tx.paths.state.join("failed.json")).unwrap(),
            tx.version
        );
        finish(tx).unwrap();
        assert!(!tx.snapshot().exists());
        assert!(tx.paths.state.join("failed.json").exists());
    }
    #[test]
    fn running_instance_locks_block_apply_until_every_instance_exits() {
        let fixture = Fixture::new();
        let first = fixture.tx.paths.run_lock().unwrap();
        let second = fixture.tx.paths.run_lock().unwrap();
        let helper = fixture.tx.paths.run_lock().unwrap();
        first.lock_shared().unwrap();
        second.lock_shared().unwrap();
        assert!(matches!(
            helper.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        first.unlock().unwrap();
        assert!(matches!(
            helper.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        second.unlock().unwrap();
        helper.try_lock().unwrap();
    }
    #[test]
    fn invalid_helper_and_escaped_staging_paths_are_rejected() {
        let mut fixture = Fixture::new();
        fixture.tx.validate().unwrap();
        fs::write(fixture.tx.helper(), b"changed helper").unwrap();
        assert!(fixture.tx.validate().is_err());
        fixture.tx.directory = fixture.root.clone();
        assert!(fixture.tx.validate().is_err());
    }
    #[test]
    fn journal_rewrites_remain_valid_and_do_not_leave_temporary_files() {
        let fixture = Fixture::new();
        let path = fixture.tx.paths.state.join("record.json");
        for value in 0..10 {
            write_json(&path, &value).unwrap();
            assert_eq!(read_json::<u32>(&path).unwrap(), value);
        }
        assert!(
            !fs::read_dir(&fixture.tx.paths.state).unwrap().any(|e| e
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "tmp"))
        );
    }
    #[cfg(unix)]
    fn probe_fixture(success: bool) -> Fixture {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = Fixture::new();
        let code = if success {
            "#!/bin/sh\nif [ \"$1\" = --internal-update-probe ] && [ \"$2\" = 0.0.10 ]; then exit 0; fi\nexit 1\n"
        } else {
            "#!/bin/sh\nexit 1\n"
        };
        fs::write(fixture.tx.candidate(), code).unwrap();
        fs::set_permissions(fixture.tx.candidate(), fs::Permissions::from_mode(0o700)).unwrap();
        fixture.tx.new_hash = hash(&fixture.tx.candidate()).unwrap();
        fixture.tx.phase = Phase::Prepared;
        fixture.tx.save().unwrap();
        fixture
    }
    #[cfg(unix)]
    #[test]
    fn ordinary_exit_applies_runnable_candidate_and_retains_backup_for_gui_startup() {
        let mut fixture = probe_fixture(true);
        apply_transaction(&mut fixture.tx).unwrap();
        assert!(fixture.tx.phase == Phase::Replaced);
        assert_eq!(hash(&fixture.tx.paths.target).unwrap(), fixture.tx.new_hash);
        assert_eq!(hash(&fixture.tx.backup()).unwrap(), fixture.tx.old_hash);
    }
    #[cfg(unix)]
    #[test]
    fn candidate_swapped_during_probe_is_never_installed() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = probe_fixture(true);
        let script =
            "#!/bin/sh\nprintf 'tampered executable' > \"$0.tmp\"\nmv \"$0.tmp\" \"$0\"\nexit 0\n";
        fs::write(fixture.tx.candidate(), script).unwrap();
        fs::set_permissions(fixture.tx.candidate(), fs::Permissions::from_mode(0o700)).unwrap();
        fixture.tx.new_hash = hash(&fixture.tx.candidate()).unwrap();
        fixture.tx.save().unwrap();
        assert!(apply_transaction(&mut fixture.tx).is_err());
        assert_eq!(hash(&fixture.tx.paths.target).unwrap(), fixture.tx.old_hash);
        assert!(!fixture.tx.backup().exists());
    }
    #[cfg(unix)]
    #[test]
    fn failing_candidate_probe_never_replaces_original() {
        let mut fixture = probe_fixture(false);
        assert!(apply_transaction(&mut fixture.tx).is_err());
        assert_eq!(hash(&fixture.tx.paths.target).unwrap(), fixture.tx.old_hash);
        assert!(!fixture.tx.backup().exists());
        assert_eq!(
            read_json::<String>(&fixture.tx.paths.state.join("failed.json")).unwrap(),
            fixture.tx.version
        );
    }
    #[cfg(unix)]
    #[test]
    fn failed_probe_after_restart_request_reopens_old_app_with_its_session() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = probe_fixture(false);
        let tx = &mut fixture.tx;
        // Stand-in for the old app: records how it was launched.
        let marker = fixture.root.join("old-app-launched");
        let old_app = format!(
            "#!/bin/sh\nprintf '%s ' \"$@\" > '{}'\n",
            marker.display()
        );
        for path in [tx.paths.target.clone(), tx.helper()] {
            fs::write(&path, &old_app).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        tx.old_hash = hash(&tx.paths.target).unwrap();
        tx.restart = true;
        write_json(&tx.snapshot(), &"unsent draft").unwrap();
        tx.save().unwrap();
        // The old app's startup confirmation, which the relaunch waits for.
        let healthy = (tx.attempt.clone(), tx.old_hash.clone());
        write_json(&tx.directory.join("healthy.json"), &healthy).unwrap();

        assert!(apply_transaction(tx).is_err());
        // The candidate never replaced anything, but the user asked to restart:
        // the snapshot and intent survive so the old app can be reopened (#239).
        assert!(tx.restart);
        assert!(tx.snapshot().exists());
        assert_eq!(hash(&tx.paths.target).unwrap(), tx.old_hash);

        relaunch_after_failure(tx, &tx.paths.journal()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            fs::read_to_string(&marker)
                .unwrap()
                .starts_with("--internal-update-launch ")
        );
        let journal: Transaction = read_json(&tx.paths.journal()).unwrap();
        assert!(journal.phase == Phase::Healthy);
        assert!(!tx.snapshot().exists());
    }
    #[cfg(unix)]
    #[test]
    fn failed_probe_without_restart_request_erases_the_snapshot() {
        let mut fixture = probe_fixture(false);
        write_json(&fixture.tx.snapshot(), &"unsent draft").unwrap();
        assert!(apply_transaction(&mut fixture.tx).is_err());
        assert!(!fixture.tx.restart);
        assert!(!fixture.tx.snapshot().exists());
        assert!(fixture.tx.phase == Phase::Staged);
    }
    #[cfg(unix)]
    #[test]
    fn candidate_gui_failure_rolls_back_and_keeps_unsent_snapshot() {
        let mut fixture = probe_fixture(true);
        fixture.tx.restart = true;
        write_json(&fixture.tx.snapshot(), &"unsent draft").unwrap();
        fixture.tx.save().unwrap();
        // Apply reports the failure before the helper wrapper relaunches the
        // old GUI, allowing inspection of the preserved recovery files.
        assert!(apply_transaction(&mut fixture.tx).is_err());
        assert!(fixture.tx.phase == Phase::RolledBack);
        assert_eq!(hash(&fixture.tx.paths.target).unwrap(), fixture.tx.old_hash);
        assert_eq!(
            read_json::<String>(&fixture.tx.snapshot()).unwrap(),
            "unsent draft"
        );
    }
    #[cfg(unix)]
    #[test]
    fn staging_and_snapshot_are_private_and_symlink_directories_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = Fixture::new();
        write_json(&fixture.tx.snapshot(), &"private").unwrap();
        assert_eq!(
            fs::metadata(&fixture.tx.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(fixture.tx.snapshot())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let link = fixture.root.join("link");
        symlink(&fixture.tx.directory, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn unsigned_windows_candidate_is_rejected() {
        let fixture = Fixture::new();
        assert!(platform::verify_signature(&fixture.tx.candidate()).is_err());
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "Requires LINEFEED_SIGNED_FIXTURE pointing to a dual-signed release"]
    fn real_windows_release_has_both_expected_publishers() {
        let path = std::env::var_os("LINEFEED_SIGNED_FIXTURE").expect("signed fixture");
        platform::verify_signature(Path::new(&path)).unwrap();
    }
}
