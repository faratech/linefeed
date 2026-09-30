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
    pub fn validate(&self) -> io::Result<()> {
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
        ensure_private_dir(&self.directory)?;
        if hash(&self.helper())? != self.old_hash {
            return Err(io::Error::other("Update helper changed"));
        }
        Ok(())
    }
}

pub fn stage_directory(paths: &Paths) -> io::Result<(String, PathBuf)> {
    let attempt = nonce()?;
    let directory = paths
        .target
        .parent()
        .ok_or_else(|| io::Error::other("Invalid executable location"))?
        .join(format!(".linefeed-update-{attempt}"));
    platform::private_dir(&directory).map_err(|_| {
        io::Error::other("Cannot update this location; download the release manually")
    })?;
    Ok((attempt, directory))
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
    pub paths: Paths,
    pub running: File,
    pub acknowledgment: Option<Transaction>,
    pub snapshot: Option<crate::gui::UpdateSession>,
    pub update_error: Option<String>,
}

pub fn startup(supervised: Option<(PathBuf, String)>) -> io::Result<Startup> {
    let paths = Paths::discover()?;
    let lock = paths.transaction_lock()?;
    if supervised.is_none() {
        timed_lock(&lock, false, Duration::from_secs(90))?;
    }
    let (acknowledgment, snapshot, update_error) =
        match recover_startup(&paths, supervised.as_ref()) {
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
    let running = paths.run_lock()?;
    running.lock_shared()?;
    drop(lock);
    Ok(Startup {
        paths,
        running,
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
        tx.validate()?;
        let own_hash = hash(&paths.target)?;
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
            tx.phase = if tx.phase == Phase::Prepared
                && !tx.backup().exists()
                && hash(&tx.candidate()).ok().as_deref() == Some(&tx.new_hash)
            {
                Phase::Staged
            } else {
                Phase::RolledBack
            };
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
    if hash(&tx.paths.target)? != tx.old_hash || hash(&tx.candidate())? != tx.new_hash {
        return Err(io::Error::other("Update bytes changed"));
    }
    platform::verify_signature(&tx.candidate())?;
    tx.restart = snapshot.is_some();
    if let Some(snapshot) = snapshot {
        write_json(&tx.snapshot(), snapshot)?;
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
    let mut child = command.spawn()?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("No helper input"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !tx.directory.join("ready").exists() {
        if child.try_wait()?.is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            tx.phase = Phase::Staged;
            tx.save()?;
            return Err(io::Error::other("Update helper could not start"));
        }
        std::thread::sleep(Duration::from_millis(50));
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
        // A failure before replacement must reopen the old app when restart
        // was requested. All locks in apply_transaction have now been released.
        let recovery_lock = tx.paths.transaction_lock()?;
        timed_lock(&recovery_lock, false, Duration::from_secs(30))?;
        let latest: Option<Transaction> = read_json(journal).ok();
        if tx.restart
            && latest.as_ref().is_some_and(|latest| {
                latest.attempt == tx.attempt && latest.phase != Phase::Healthy
            })
            && hash(&tx.paths.target).ok().as_deref() == Some(&tx.old_hash)
        {
            tx.phase = Phase::RolledBack;
            tx.save()?;
            launch_fallback(&tx)?;
        }
    }
    result
}

fn apply_transaction(tx: &mut Transaction) -> io::Result<()> {
    let transaction_lock = tx.paths.transaction_lock()?;
    timed_lock(&transaction_lock, false, Duration::from_secs(30))?;
    let running = tx.paths.run_lock()?;
    if timed_lock(&running, false, Duration::from_secs(30)).is_err() {
        tx.phase = Phase::Staged;
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
        tx.phase = Phase::Staged;
        tx.save()?;
        write_json(&tx.paths.state.join("failed.json"), &tx.version)?;
        return Err(io::Error::other("Update validation failed"));
    }
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
    tx.phase = Phase::Healthy;
    tx.save()?;
    let _ = fs::remove_file(tx.snapshot());
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
            let (attempt, directory) = stage_directory(&paths).unwrap();
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
    fn startup_rejects_stale_state_without_deleting_recovery_files() {
        let fixture = Fixture::new();
        fs::write(&fixture.tx.paths.target, b"manual installation").unwrap();
        assert!(recover_startup(&fixture.tx.paths, None).is_err());
        assert_eq!(
            fs::read(&fixture.tx.paths.target).unwrap(),
            b"manual installation"
        );
        assert!(fixture.tx.paths.journal().exists());
        assert!(fixture.tx.candidate().exists());
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
