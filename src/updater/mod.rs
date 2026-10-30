//! GitHub Releases updater. The UI only sends commands and drains events;
//! downloads, verification and staging run independently of the IRC runtime.
pub mod install;
mod platform;

use install::{Paths, Phase, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const REPOSITORY: &str = "faratech/linefeed";
const CHECK_INTERVAL: u64 = 24 * 60 * 60;
/// How long a worker waits before automatically retrying a failed check.
const CHECK_RETRY: Duration = Duration::from_secs(10 * 60);

pub fn asset_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "aarch64") => Some("linefeed_arm64.exe"),
        ("windows", "x86_64") => Some("linefeed_x64.exe"),
        ("windows", "x86") => Some("linefeed_x86.exe"),
        ("linux", "aarch64") => Some("linefeed_linux_arm64"),
        ("linux", "x86_64") => Some("linefeed_linux_x64"),
        ("macos", "aarch64" | "x86_64") => Some("linefeed_macos"),
        _ => None,
    }
}
pub fn current_asset() -> Option<&'static str> {
    asset_name(std::env::consts::OS, std::env::consts::ARCH)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Offer {
    pub version: String,
    pub tag: String,
    asset: Asset,
    checksums: Asset,
}
impl Offer {
    pub fn release_url(&self) -> String {
        format!("https://github.com/{REPOSITORY}/releases/tag/{}", self.tag)
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
    state: String,
}
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Clone, Default)]
pub enum Status {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading {
        received: u64,
        total: u64,
    },
    Ready,
    Applying,
    Error(String),
}
enum Command {
    Check,
    Download,
    Prepare(Box<crate::gui::UpdateSession>),
    Stop,
}
enum Event {
    Status(Status),
    Offer(Option<Offer>),
    Checked(u64),
    Staged(Transaction),
    Armed(install::Armed),
}

#[derive(Default)]
pub struct Controller {
    pub status: Status,
    pub offer: Option<Offer>,
    pub last_check: Option<u64>,
    pub install_reason: Option<String>,
    pub restart_requested: bool,
    pub closing: bool,
    pub(crate) staged: Option<Transaction>,
    pub(crate) armed: Option<install::Armed>,
    /// Set while a recovery error blocks the updater: the paths whose update
    /// state "Reset update state" would clear (#238).
    recovery: Option<Paths>,
    sender: Option<mpsc::Sender<Command>>,
    events: Option<mpsc::Receiver<Event>>,
    automatic: Arc<AtomicBool>,
    canceled: Arc<AtomicBool>,
}

impl Controller {
    pub fn start(&mut self, paths: Option<Paths>, context: egui::Context, automatic: bool) {
        let Some(paths) = paths else {
            // The updater could not initialize but the app still runs: leave
            // it disabled and say why in About (#272).
            self.status = Status::Error(
                self.install_reason
                    .clone()
                    .unwrap_or_else(|| "The updater is unavailable".into()),
            );
            return;
        };
        self.automatic.store(automatic, Ordering::Relaxed);
        let recovery_error = self.install_reason.is_some();
        if recovery_error {
            self.status = Status::Error(self.install_reason.clone().unwrap());
            self.recovery = Some(paths.clone());
        } else if cfg!(debug_assertions) {
            self.install_reason = Some(
                "Development build: install a release binary to enable automatic installation"
                    .into(),
            );
        } else if current_asset().is_none() {
            self.install_reason =
                Some("Automatic installation is unavailable for this platform".into());
        }
        self.last_check = install::read_json(&paths.state.join("last-check.json")).ok();
        if let Ok(message) = install::read_json::<String>(&paths.state.join("result.json")) {
            self.status = Status::Error(message);
            let _ = fs::remove_file(paths.state.join("result.json"));
        }
        if !recovery_error
            && let Ok(tx) = install::read_json::<Transaction>(&paths.journal())
            && tx.paths.target == paths.target
            && tx.paths.state == paths.state
            && tx.validate().is_ok()
        {
            // A candidate that was quarantined or truncated since staging is not
            // an update to offer: leaving it unstaged lets the same release be
            // downloaded again, which replaces the damaged stage (#296).
            if tx.phase == Phase::Staged
                && install::hash(&paths.target).ok().as_deref() == Some(&tx.old_hash)
                && install::hash(&tx.candidate()).ok().as_deref() == Some(&tx.new_hash)
            {
                // A failure the helper recorded stays visible; the staged
                // update remains installable below it.
                if !matches!(self.status, Status::Error(_)) {
                    self.status = Status::Ready;
                }
                self.staged = Some(tx);
            } else if tx.phase == Phase::Healthy {
                // The snapshot lives in the state dir, outside the
                // transaction directory removed below (issue #200).
                let _ = fs::remove_file(tx.snapshot());
                let _ = fs::remove_dir_all(&tx.directory);
                let _ = fs::remove_file(paths.journal());
            }
        }
        let (sender, commands) = mpsc::channel();
        let (events, receiver) = mpsc::channel();
        self.sender = Some(sender);
        self.events = Some(receiver);
        let enabled = self.automatic.clone();
        let canceled = self.canceled.clone();
        let can_install = self.install_reason.is_none();
        std::thread::spawn(move || {
            worker(
                paths,
                commands,
                events,
                context,
                enabled,
                canceled,
                can_install,
            )
        });
    }
    /// Whether "Reset update state" applies: startup found update recovery
    /// files it will not touch on its own.
    pub fn reset_available(&self) -> bool {
        self.recovery.is_some()
    }
    /// Discard the update recovery files after the user has dealt with the
    /// installation themselves. No live re-plumbing: the updater stays off
    /// until the next launch, which says so.
    pub fn reset_recovery(&mut self) {
        let Some(paths) = self.recovery.take() else {
            return;
        };
        match install::reset_recovery(&paths) {
            Ok(()) => {
                self.status = Status::Idle;
                self.install_reason =
                    Some("Update state was reset. Restart Linefeed to re-enable updates.".into());
            }
            Err(e) => {
                self.status = Status::Error(format!("Could not reset update state: {e}"));
                self.recovery = Some(paths);
            }
        }
    }
    pub fn set_automatic(&self, enabled: bool) {
        self.automatic.store(enabled, Ordering::Relaxed);
    }
    pub fn busy(&self) -> bool {
        matches!(
            self.status,
            Status::Checking | Status::Downloading { .. } | Status::Applying
        )
    }
    /// False when the updater failed to initialize and has no worker.
    pub fn available(&self) -> bool {
        self.sender.is_some()
    }
    pub fn check(&mut self) {
        if !self.busy() && self.available() {
            self.send(Command::Check);
            self.status = Status::Checking;
        }
    }
    /// Whether "Download update" applies: an offer exists and is not staged
    /// yet. A staged older update stays installable but must not block a
    /// newer release (#271).
    pub fn download_available(&self) -> bool {
        let Some(offer) = &self.offer else {
            return false;
        };
        match &self.staged {
            None => true,
            Some(tx) => matches!(
                (
                    semver::Version::parse(&offer.version),
                    semver::Version::parse(&tx.version)
                ),
                (Ok(offered), Ok(staged)) if offered > staged
            ),
        }
    }
    pub fn download(&mut self) {
        if !self.busy() && self.install_reason.is_none() {
            self.send(Command::Download);
            self.status = Status::Downloading {
                received: 0,
                total: self.offer.as_ref().map_or(0, |o| o.asset.size),
            };
        }
    }
    pub fn prepare_restart(&mut self, snapshot: crate::gui::UpdateSession) {
        if self.staged.is_some() && !self.busy() {
            self.status = Status::Applying;
            self.send(Command::Prepare(Box::new(snapshot)));
        }
    }
    fn send(&self, command: Command) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(command);
        }
    }
    pub fn poll(&mut self) {
        if let Some(events) = &self.events {
            while let Ok(event) = events.try_recv() {
                match event {
                    Event::Status(status) => self.status = status,
                    Event::Offer(offer) => self.offer = offer,
                    Event::Checked(time) => self.last_check = Some(time),
                    Event::Staged(tx) => {
                        self.staged = Some(tx);
                        self.status = Status::Ready;
                    }
                    Event::Armed(armed) => {
                        self.armed = Some(armed);
                        self.closing = true;
                    }
                }
            }
        }
    }
    pub fn arm_on_exit(&mut self, automatic: bool) -> Option<install::Armed> {
        self.canceled.store(true, Ordering::Relaxed);
        self.send(Command::Stop);
        self.poll();
        if let Some(armed) = self.armed.take() {
            return Some(armed);
        }
        if automatic
            && self.install_reason.is_none()
            && !matches!(self.status, Status::Applying)
            && let Some(tx) = self.staged.take()
        {
            if install::read_json::<String>(&tx.paths.state.join("failed.json"))
                .ok()
                .as_deref()
                == Some(&tx.version)
            {
                return None;
            }
            match install::arm(tx, None) {
                Ok(armed) => return Some(armed),
                Err(e) => tracing::warn!("Update deferred on exit: {e}"),
            }
        }
        None
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::Relaxed);
        self.send(Command::Stop);
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn emit(sender: &mpsc::Sender<Event>, context: &egui::Context, event: Event) {
    let _ = sender.send(event);
    context.request_repaint();
}
fn worker(
    paths: Paths,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    context: egui::Context,
    automatic: Arc<AtomicBool>,
    canceled: Arc<AtomicBool>,
    can_install: bool,
) {
    let mut offer = None;
    // When a failed check may be retried automatically.
    let mut retry_at = None;
    let initial = Instant::now() + Duration::from_secs(5);
    loop {
        let command = match commands.recv_timeout(Duration::from_secs(5)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() < initial
                    || !automatic.load(Ordering::Relaxed)
                    || canceled.load(Ordering::Relaxed)
                {
                    continue;
                }
                let last =
                    install::read_json::<u64>(&paths.state.join("last-check.json")).unwrap_or(0);
                if !check_due(last, now(), retry_at, Instant::now()) {
                    continue;
                }
                Command::Check
            }
        };
        if matches!(command, Command::Stop) || canceled.load(Ordering::Relaxed) {
            break;
        }
        let result = (|| -> io::Result<()> {
            match command {
                Command::Check => {
                    emit(&events, &context, Event::Status(Status::Checking));
                    let (checked, found) =
                        stamped_check(&paths, check_release).map_err(|error| {
                            if matches!(error, CheckError::Request(_)) {
                                retry_at = Some(Instant::now() + CHECK_RETRY);
                            }
                            error.into_io()
                        })?;
                    emit(&events, &context, Event::Checked(checked));
                    offer = found;
                    emit(&events, &context, Event::Offer(offer.clone()));
                    if let Some(offer) = &offer {
                        emit(&events, &context, Event::Status(Status::Available));
                        let failed =
                            install::read_json::<String>(&paths.state.join("failed.json")).ok();
                        if can_install
                            && automatic.load(Ordering::Relaxed)
                            && failed.as_deref() != Some(&offer.version)
                        {
                            let tx = download(&paths, offer, &events, &context, &canceled)?;
                            emit(&events, &context, Event::Staged(tx));
                        }
                    } else {
                        emit(&events, &context, Event::Status(Status::UpToDate));
                    }
                }
                Command::Download => {
                    let offer = offer
                        .as_ref()
                        .ok_or_else(|| io::Error::other("Check for an update first"))?;
                    let tx = download(&paths, offer, &events, &context, &canceled)?;
                    emit(&events, &context, Event::Staged(tx));
                }
                Command::Prepare(snapshot) => {
                    let tx = install::read_json(&paths.journal())?;
                    let armed = install::arm(tx, Some(&snapshot))?;
                    emit(&events, &context, Event::Armed(armed));
                }
                Command::Stop => {}
            }
            Ok(())
        })();
        if let Err(error) = result {
            emit(
                &events,
                &context,
                Event::Status(Status::Error(error.to_string())),
            );
        }
    }
}

/// Release metadata and the checksum manifest are small, so a slow body is a
/// stuck one and keeps the short limit.
/// Stamp the check cadence for all running instances. The transaction lock is
/// held only for this write, never across the network requests that follow:
/// another launch would otherwise wait on it until its own timeout (#272).
fn record_check(paths: &Paths) -> io::Result<u64> {
    let lock = paths.transaction_lock()?;
    lock.try_lock().map_err(io::Error::other)?;
    let checked = now();
    install::write_json(&paths.state.join("last-check.json"), &checked)?;
    Ok(checked)
}

/// Why a release check failed.
#[derive(Debug)]
enum CheckError {
    /// The request did not complete (offline, rate limited, GitHub 5xx), or the
    /// check could not be recorded: it found nothing out, and asking again
    /// soon may well work.
    Request(io::Error),
    /// GitHub answered with a release the updater rejects (incomplete assets,
    /// invalid metadata, an unusable tag). It would answer the same again soon.
    Release(io::Error),
}
impl CheckError {
    fn into_io(self) -> io::Error {
        match self {
            Self::Request(e) | Self::Release(e) => e,
        }
    }
}

/// Stamp the check cadence, then run `check`. The stamp goes first so that
/// instances launched meanwhile skip their own check. A request that fails takes
/// it back, so that it does not suppress automatic checks for a whole interval
/// (#297); a release GitHub answered with and the updater rejected keeps it,
/// since that check completed and repeating it would change nothing.
fn stamped_check(
    paths: &Paths,
    check: impl FnOnce() -> Result<Option<Offer>, CheckError>,
) -> Result<(u64, Option<Offer>), CheckError> {
    let previous = install::read_json::<u64>(&paths.state.join("last-check.json")).ok();
    let checked = record_check(paths).map_err(CheckError::Request)?;
    match check() {
        Ok(offer) => Ok((checked, offer)),
        Err(error) => {
            if matches!(error, CheckError::Request(_)) {
                unrecord_check(paths, checked, previous);
            }
            Err(error)
        }
    }
}
/// Restore the stamp from before a failed check, unless another instance has
/// stamped since: that instance's check stands. Best effort; a stamp that stays
/// only delays the next automatic check.
fn unrecord_check(paths: &Paths, checked: u64, previous: Option<u64>) {
    let Ok(lock) = paths.transaction_lock() else {
        return;
    };
    if install::timed_lock(&lock, false, Duration::from_secs(5)).is_err() {
        return;
    }
    let stamp = paths.state.join("last-check.json");
    if install::read_json::<u64>(&stamp).ok() != Some(checked) {
        return;
    }
    let _ = match previous {
        Some(previous) => install::write_json(&stamp, &previous),
        None => fs::remove_file(&stamp),
    };
}
/// Whether an idle worker starts an automatic check: a full interval has passed
/// since the last stamp, and a failed check is not still backing off. Without
/// the backoff a stamp taken back after a failure would be retried on every
/// idle tick for as long as the network stays down.
fn check_due(last: u64, current: u64, backoff_until: Option<Instant>, tick: Instant) -> bool {
    backoff_until.is_none_or(|until| tick >= until)
        && !(current >= last && current - last < CHECK_INTERVAL)
}

fn agent() -> ureq::Agent {
    build_agent(Some(Duration::from_secs(30)), true)
}
/// The binary download. ureq's body limit is a total for the whole body and is
/// not restarted per read, so the metadata limit would abort every download on
/// a link slower than size/30 s; the 300 s global deadline bounds it instead.
fn download_agent() -> ureq::Agent {
    build_agent(None, true)
}
fn build_agent(recv_body: Option<Duration>, https_only: bool) -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    ureq::Agent::config_builder()
        .https_only(https_only)
        // Redirects are followed in `response`, not by ureq; see there.
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(300)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(recv_body)
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}
/// GET `url`, following up to five redirects here rather than in ureq. While
/// following redirects ureq accepts a 3xx whose headers arrived only in part,
/// then parses the remaining header lines as the body and fails; every release
/// download is a redirect to GitHub's asset host, so a header block split
/// across reads would break every update.
fn response(agent: &ureq::Agent, url: &str) -> io::Result<ureq::http::Response<ureq::Body>> {
    let failed =
        || io::Error::other("GitHub request failed; check your connection or try again later");
    let mut url: ureq::http::Uri = url.parse().map_err(|_| failed())?;
    for _ in 0..=5 {
        let response = agent
            .get(&url)
            .header(
                "User-Agent",
                concat!("linefeed/", env!("CARGO_PKG_VERSION")),
            )
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2026-03-10")
            .call()
            .map_err(|_| failed())?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        url = redirect_target(&url, response.headers().get("location")).ok_or_else(failed)?;
    }
    Err(failed())
}
/// The absolute URI a redirect points to; a path-only Location keeps the
/// current scheme and host. The agent's https_only setting still applies.
fn redirect_target(
    current: &ureq::http::Uri,
    location: Option<&ureq::http::HeaderValue>,
) -> Option<ureq::http::Uri> {
    let location: ureq::http::Uri = location?.to_str().ok()?.parse().ok()?;
    if location.scheme().is_some() && location.authority().is_some() {
        return Some(location);
    }
    let mut parts = location.into_parts();
    if !parts.path_and_query.as_ref()?.as_str().starts_with('/') {
        return None;
    }
    parts.scheme = current.scheme().cloned();
    parts.authority = current.authority().cloned();
    ureq::http::Uri::from_parts(parts).ok()
}
fn limited_body(mut response: ureq::http::Response<ureq::Body>, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("GitHub response exceeds size limit"));
    }
    Ok(bytes)
}
fn check_release() -> Result<Option<Offer>, CheckError> {
    check_release_from(
        &agent(),
        &format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"),
        env!("CARGO_PKG_VERSION"),
        current_asset(),
    )
}
fn check_release_from(
    client: &ureq::Agent,
    url: &str,
    current: &str,
    asset: Option<&str>,
) -> Result<Option<Offer>, CheckError> {
    let body = response(client, url)
        .and_then(|response| limited_body(response, 1024 * 1024))
        .map_err(CheckError::Request)?;
    let release: Release = serde_json::from_slice(&body).map_err(|_| {
        CheckError::Release(io::Error::other("GitHub returned invalid release metadata"))
    })?;
    select_release(release, current, asset).map_err(CheckError::Release)
}
fn select_release(
    release: Release,
    current: &str,
    name: Option<&str>,
) -> io::Result<Option<Offer>> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = semver::Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name),
    )
    .map_err(|_| io::Error::other("Release tag is not a semantic version"))?;
    if !version.pre.is_empty()
        || version <= semver::Version::parse(current).map_err(io::Error::other)?
    {
        return Ok(None);
    }
    let name = name.ok_or_else(|| io::Error::other("No release asset for this platform"))?;
    let find = |name: &str| -> io::Result<Asset> {
        let matches: Vec<_> = release.assets.iter().filter(|a| a.name == name).collect();
        if matches.len() != 1 {
            return Err(io::Error::other("Release assets are incomplete"));
        }
        let asset = matches[0];
        let expected = format!(
            "https://github.com/{REPOSITORY}/releases/download/{}/{name}",
            release.tag_name
        );
        if asset.browser_download_url != expected
            || asset.state != "uploaded"
            || asset.size == 0
            || asset.size > install::MAX_BINARY
            || digest(asset).is_err()
        {
            return Err(io::Error::other("Release asset metadata is invalid"));
        }
        Ok(asset.clone())
    };
    Ok(Some(Offer {
        version: version.to_string(),
        tag: release.tag_name.clone(),
        asset: find(name)?,
        checksums: find("SHA256SUMS")?,
    }))
}
fn digest(asset: &Asset) -> io::Result<&str> {
    let digest = asset
        .digest
        .as_deref()
        .and_then(|v| v.strip_prefix("sha256:"))
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| io::Error::other("Release asset has no SHA-256 digest"))?;
    Ok(digest)
}
fn checksum(text: &[u8], name: &str) -> io::Result<String> {
    let text =
        std::str::from_utf8(text).map_err(|_| io::Error::other("Invalid checksum manifest"))?;
    let mut found = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(hash), Some(file), None) = (fields.next(), fields.next(), fields.next())
            && file.trim_start_matches('*') == name
        {
            if found.is_some() || hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(io::Error::other("Ambiguous or invalid release checksum"));
            }
            found = Some(hash.to_ascii_lowercase());
        }
    }
    found.ok_or_else(|| io::Error::other("Release checksum is missing"))
}
fn download(
    paths: &Paths,
    offer: &Offer,
    events: &mpsc::Sender<Event>,
    context: &egui::Context,
    canceled: &AtomicBool,
) -> io::Result<Transaction> {
    let previous = existing_journal(paths)?;
    if let Some(tx) = &previous {
        if matches!(tx.phase, Phase::Prepared | Phase::Replaced) {
            return Err(io::Error::other(
                "An update is awaiting installation or startup confirmation",
            ));
        }
        if tx.phase == Phase::Staged
            && tx.version == offer.version
            && tx.new_hash == digest(&offer.asset)?.to_ascii_lowercase()
            && tx.validate().is_ok()
            && install::hash(&paths.target).ok().as_deref() == Some(&tx.old_hash)
            && install::hash(&tx.candidate()).ok().as_deref() == Some(&tx.new_hash)
        {
            return Ok(tx.clone());
        }
    }
    let client = agent();
    let sums = limited_body(
        response(&client, &offer.checksums.browser_download_url)?,
        65536,
    )?;
    if install::hex(&Sha256::digest(&sums)) != digest(&offer.checksums)?.to_ascii_lowercase() {
        return Err(io::Error::other("Checksum manifest failed verification"));
    }
    let expected = checksum(&sums, &offer.asset.name)?;
    if expected != digest(&offer.asset)?.to_ascii_lowercase() {
        return Err(io::Error::other("GitHub and release checksums disagree"));
    }
    let (attempt, directory, claim) = install::stage_directory(paths)?;
    let result = (|| {
        let partial = directory.join("download.part");
        let mut file = install::new_file(&partial)?;
        let mut response = response(&download_agent(), &offer.asset.browser_download_url)?;
        let mut reader = response.body_mut().as_reader();
        copy_download(
            &mut reader,
            &mut file,
            offer.asset.size,
            &expected,
            canceled,
            |received| {
                emit(
                    events,
                    context,
                    Event::Status(Status::Downloading {
                        received,
                        total: offer.asset.size,
                    }),
                );
            },
        )?;
        file.set_permissions(fs::metadata(&paths.target)?.permissions())?;
        file.sync_all()?;
        drop(file);
        let tx = Transaction {
            paths: paths.clone(),
            attempt,
            directory: directory.clone(),
            version: offer.version.clone(),
            old_hash: install::hash(&paths.target)?,
            new_hash: expected,
            phase: Phase::Staged,
            restart: false,
            working_directory: std::env::current_dir()
                .unwrap_or_else(|_| paths.target.parent().unwrap().to_owned()),
        };
        fs::rename(partial, tx.candidate())?;
        // The machine is online here by construction, so require revocation data.
        platform::verify_signature(&tx.candidate(), platform::Revocation::Strict)?;
        install::executable_copy(&paths.target, &tx.helper())?;
        platform::sync_dir(&directory)?;
        publish_staged(paths, &tx)?;
        Ok(tx)
    })();
    // Release the claim first: Windows cannot delete a directory holding an
    // open handle. A kill before this point leaves the directory for the
    // startup sweep (#270).
    drop(claim);
    if result.is_err() {
        let _ = fs::remove_dir_all(directory);
    }
    result
}

/// The journal on disk, or None when there is none. One that exists but cannot
/// be read is an error, never "none": it may be a Prepared or Replaced journal
/// holding the only copy of the old executable, and a new stage would
/// overwrite it.
fn existing_journal(paths: &Paths) -> io::Result<Option<Transaction>> {
    match install::read_json(&paths.journal()) {
        Ok(tx) => Ok(Some(tx)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(io::Error::other(
            "Update state is unreadable; restart Linefeed to repair it",
        )),
    }
}

/// Point the journal at a fully verified candidate. This is the only step of a
/// download that holds the transaction lock, so other launches and exits never
/// wait on network I/O (#272).
fn publish_staged(paths: &Paths, tx: &Transaction) -> io::Result<()> {
    let lock = paths.transaction_lock()?;
    install::timed_lock(&lock, false, Duration::from_secs(30))?;
    // Re-read under the lock: an install may have started during the download.
    let previous = existing_journal(paths)?;
    if let Some(old) = &previous
        && matches!(old.phase, Phase::Prepared | Phase::Replaced)
    {
        return Err(io::Error::other(
            "An update is awaiting installation or startup confirmation",
        ));
    }
    tx.save()?;
    // Preserve the old stage until the new candidate is verified and the
    // journal points to it. Then remove old helper bytes and credentials.
    if let Some(old) = &previous
        && old.paths.target == paths.target
        && old.paths.state == paths.state
        && old.directory != tx.directory
        && old.validate().is_ok()
    {
        let _ = fs::remove_file(old.snapshot());
        let _ = fs::remove_dir_all(&old.directory);
    }
    Ok(())
}

fn copy_download(
    reader: &mut impl Read,
    file: &mut impl Write,
    size: u64,
    expected: &str,
    canceled: &AtomicBool,
    mut report: impl FnMut(u64),
) -> io::Result<()> {
    let mut digest = Sha256::new();
    let mut received = 0u64;
    let mut buf = [0u8; 65536];
    let mut progress = Instant::now();
    loop {
        if canceled.load(Ordering::Relaxed) {
            return Err(io::Error::other("Update download canceled"));
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        received += n as u64;
        if received > size || received > install::MAX_BINARY {
            return Err(io::Error::other("Update download exceeds expected size"));
        }
        file.write_all(&buf[..n])?;
        digest.update(&buf[..n]);
        if progress.elapsed() > Duration::from_millis(100) {
            report(received);
            progress = Instant::now();
        }
    }
    if received != size || install::hex(&digest.finalize()) != expected {
        return Err(io::Error::other(
            "Update download failed SHA-256 verification",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn release(tag: &str) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: [
                "linefeed_arm64.exe",
                "linefeed_x64.exe",
                "linefeed_x86.exe",
                "linefeed_linux_arm64",
                "linefeed_linux_x64",
                "linefeed_macos",
                "SHA256SUMS",
            ]
            .iter()
            .map(|name| Asset {
                name: name.to_string(),
                size: 10,
                state: "uploaded".into(),
                digest: Some(format!("sha256:{}", "a".repeat(64))),
                browser_download_url: format!(
                    "https://github.com/{REPOSITORY}/releases/download/{tag}/{name}"
                ),
            })
            .collect(),
        }
    }
    #[test]
    fn every_supported_architecture_selects_its_own_asset() {
        for (os, arch, expected) in [
            ("windows", "aarch64", "linefeed_arm64.exe"),
            ("windows", "x86_64", "linefeed_x64.exe"),
            ("windows", "x86", "linefeed_x86.exe"),
            ("linux", "aarch64", "linefeed_linux_arm64"),
            ("linux", "x86_64", "linefeed_linux_x64"),
            ("macos", "aarch64", "linefeed_macos"),
            ("macos", "x86_64", "linefeed_macos"),
        ] {
            let offer = select_release(release("v0.0.10"), "0.0.9", asset_name(os, arch))
                .unwrap()
                .unwrap();
            assert_eq!(offer.asset.name, expected);
        }
        assert!(asset_name("linux", "x86").is_none());
    }
    #[test]
    fn compares_versions_numerically_and_never_downgrades() {
        assert!(
            select_release(release("v0.0.10"), "0.0.9", Some("linefeed_x64.exe"))
                .unwrap()
                .is_some()
        );
        assert!(
            select_release(release("v0.0.9"), "0.0.10", Some("linefeed_x64.exe"))
                .unwrap()
                .is_none()
        );
        assert!(
            select_release(release("v0.0.9"), "0.0.9", Some("linefeed_x64.exe"))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn rejects_drafts_prereleases_and_incomplete_releases() {
        let mut fixture = release("v0.0.10");
        fixture.draft = true;
        assert!(
            select_release(fixture, "0.0.9", current_asset())
                .unwrap()
                .is_none()
        );
        let mut fixture = release("v0.0.10");
        fixture.prerelease = true;
        assert!(
            select_release(fixture, "0.0.9", current_asset())
                .unwrap()
                .is_none()
        );
        assert!(
            select_release(release("v0.0.10-rc.1"), "0.0.9", current_asset())
                .unwrap()
                .is_none()
        );
        let mut fixture = release("v0.0.10");
        fixture.assets.pop();
        assert!(select_release(fixture, "0.0.9", current_asset()).is_err());
    }
    #[test]
    fn rejects_missing_digest_duplicate_assets_and_foreign_download_urls() {
        for mutation in 0..3 {
            let mut fixture = release("v0.0.10");
            match mutation {
                0 => fixture.assets[0].digest = None,
                1 => fixture.assets[0].browser_download_url = "https://example.com/app.exe".into(),
                _ => fixture.assets.push(fixture.assets[0].clone()),
            }
            assert!(select_release(fixture, "0.0.9", Some("linefeed_arm64.exe")).is_err());
        }
    }
    #[test]
    fn checksum_requires_exact_filename_and_single_digest() {
        let hash = "ab".repeat(32);
        assert_eq!(
            checksum(
                format!("{hash}  *linefeed_x64.exe\n").as_bytes(),
                "linefeed_x64.exe"
            )
            .unwrap(),
            hash
        );
        assert!(
            checksum(
                format!("{hash}  linefeed_x64.exe.old\n").as_bytes(),
                "linefeed_x64.exe"
            )
            .is_err()
        );
        assert!(
            checksum(
                format!("{hash}  linefeed_x64.exe\n{hash}  linefeed_x64.exe\n").as_bytes(),
                "linefeed_x64.exe"
            )
            .is_err()
        );
    }
    #[test]
    fn truncated_corrupt_oversized_and_canceled_downloads_never_verify() {
        let bytes = b"verified executable";
        let expected = install::hex(&Sha256::digest(bytes));
        for (body, size, digest, canceled) in [
            (&bytes[..], bytes.len() as u64 + 1, expected.as_str(), false),
            (&bytes[..], bytes.len() as u64, "wrong digest", false),
            (&bytes[..], 1, expected.as_str(), false),
            (&bytes[..], bytes.len() as u64, expected.as_str(), true),
        ] {
            assert!(
                copy_download(
                    &mut &body[..],
                    &mut Vec::new(),
                    size,
                    digest,
                    &AtomicBool::new(canceled),
                    |_| {}
                )
                .is_err()
            );
        }
        let mut output = Vec::new();
        copy_download(
            &mut &bytes[..],
            &mut output,
            bytes.len() as u64,
            &expected,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(output, bytes);
    }
    /// A valid Staged transaction on disk beside a stand-in executable.
    fn staged_fixture(version: &str) -> (std::path::PathBuf, Paths, Transaction) {
        let root = std::env::temp_dir().join(format!("linefeed-updater-test-{}", install::nonce().unwrap()));
        platform::private_dir(&root).unwrap();
        let paths = Paths {
            target: root.join("app"),
            state: root.join("state"),
        };
        platform::private_dir(&paths.state).unwrap();
        fs::write(&paths.target, b"original executable").unwrap();
        let (attempt, directory, _claim) = install::stage_directory(&paths).unwrap();
        let tx = Transaction {
            paths: paths.clone(),
            attempt,
            directory,
            version: version.into(),
            old_hash: install::hash(&paths.target).unwrap(),
            new_hash: install::hex(&Sha256::digest(b"new executable")),
            phase: Phase::Staged,
            restart: false,
            working_directory: root.clone(),
        };
        fs::write(tx.candidate(), b"new executable").unwrap();
        install::executable_copy(&paths.target, &tx.helper()).unwrap();
        tx.save().unwrap();
        (root, paths, tx)
    }
    #[test]
    fn a_staged_older_update_does_not_block_downloading_a_newer_offer() {
        // With automatic updates off, a staged 0.0.10 persists across launches;
        // when 0.0.11 appears the user must be able to fetch it (#271).
        let (root, _paths, staged) = staged_fixture("0.0.10");
        let offer = |tag: &str| {
            select_release(release(tag), "0.0.9", Some("linefeed_x64.exe"))
                .unwrap()
                .unwrap()
        };
        let mut controller = Controller::default();
        assert!(!controller.download_available());
        controller.offer = Some(offer("v0.0.11"));
        assert!(controller.download_available());
        controller.staged = Some(staged.clone());
        assert!(controller.download_available());
        // The same release is already staged; an older one is not an upgrade.
        controller.offer = Some(offer("v0.0.10"));
        assert!(!controller.download_available());
        controller.staged = Some(Transaction {
            version: "0.0.12".into(),
            ..staged
        });
        controller.offer = Some(offer("v0.0.11"));
        assert!(!controller.download_available());
        drop(controller);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn recorded_update_failure_stays_visible_over_a_ready_staged_update() {
        // The helper records why an install failed; the still-valid staged
        // journal must not replace that message with "downloaded and verified".
        let (root, paths, _tx) = staged_fixture("0.0.10");
        let failure = "Update could not complete: Update validation failed";
        install::write_json(&paths.state.join("result.json"), &failure).unwrap();
        let mut controller = Controller::default();
        controller.start(Some(paths), egui::Context::default(), false);
        assert!(matches!(&controller.status, Status::Error(message) if message == failure));
        assert!(controller.staged.is_some());
        drop(controller);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn a_staged_update_whose_candidate_was_damaged_is_not_ready_and_can_be_downloaded_again() {
        // Antivirus truncated only the candidate after staging. Startup must not
        // call that update ready/verified, or the same release can never be
        // fetched again from the UI (#296).
        let (root, paths, tx) = staged_fixture("0.0.10");
        fs::write(tx.candidate(), b"new exec").unwrap();
        let mut controller = Controller::default();
        controller.start(Some(paths.clone()), egui::Context::default(), false);
        assert!(controller.staged.is_none());
        assert!(!matches!(controller.status, Status::Ready));
        controller.offer = Some(
            select_release(release("v0.0.10"), "0.0.9", Some("linefeed_x64.exe"))
                .unwrap()
                .unwrap(),
        );
        assert!(controller.download_available());
        // The journal and staging stay for the re-download to replace.
        assert!(paths.journal().exists() && tx.directory.exists());
        drop(controller);
        // A missing candidate is the same damage.
        fs::remove_file(tx.candidate()).unwrap();
        let mut controller = Controller::default();
        controller.start(Some(paths), egui::Context::default(), false);
        assert!(controller.staged.is_none());
        assert!(!matches!(controller.status, Status::Ready));
        drop(controller);
        let _ = fs::remove_dir_all(root);
    }
    /// A second verified candidate for the fixture's installation.
    fn newer_candidate(paths: &Paths) -> Transaction {
        let (attempt, directory, _claim) = install::stage_directory(paths).unwrap();
        let tx = Transaction {
            paths: paths.clone(),
            attempt,
            directory,
            version: "0.0.11".into(),
            old_hash: install::hash(&paths.target).unwrap(),
            new_hash: install::hex(&Sha256::digest(b"newer executable")),
            phase: Phase::Staged,
            restart: false,
            working_directory: paths.state.clone(),
        };
        fs::write(tx.candidate(), b"newer executable").unwrap();
        install::executable_copy(&paths.target, &tx.helper()).unwrap();
        tx
    }
    #[test]
    fn publishing_replaces_the_previous_stage_and_releases_the_lock() {
        let (root, paths, old) = staged_fixture("0.0.10");
        let newer = newer_candidate(&paths);
        publish_staged(&paths, &newer).unwrap();
        let journal: Transaction = install::read_json(&paths.journal()).unwrap();
        assert_eq!(journal.attempt, newer.attempt);
        assert!(!old.directory.exists());
        // Nothing keeps holding the lock once the commit is done (waiting
        // briefly: a concurrently forked child can keep a closed handle's lock
        // alive until it execs).
        let lock = paths.transaction_lock().unwrap();
        install::timed_lock(&lock, false, Duration::from_secs(5)).unwrap();
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn publishing_never_overwrites_a_journal_that_began_installing() {
        let (root, paths, mut old) = staged_fixture("0.0.10");
        // An arm moved the journal on while the newer candidate downloaded.
        old.phase = Phase::Prepared;
        old.save().unwrap();
        let newer = newer_candidate(&paths);
        assert!(publish_staged(&paths, &newer).is_err());
        let journal: Transaction = install::read_json(&paths.journal()).unwrap();
        assert_eq!(journal.attempt, old.attempt);
        assert!(old.directory.exists());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn publishing_never_overwrites_a_journal_it_cannot_read() {
        // An unreadable journal may be a Prepared/Replaced one holding the only
        // copy of the old executable: not "no journal" (#240).
        let (root, paths, old) = staged_fixture("0.0.10");
        fs::write(paths.journal(), b"invalid journal").unwrap();
        let newer = newer_candidate(&paths);
        assert!(publish_staged(&paths, &newer).is_err());
        assert_eq!(fs::read(paths.journal()).unwrap(), b"invalid journal");
        assert!(old.directory.exists());
        // A missing journal is the ordinary first download.
        fs::remove_file(paths.journal()).unwrap();
        publish_staged(&paths, &newer).unwrap();
        assert!(paths.journal().exists());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn checking_for_updates_does_not_hold_the_lock_during_the_request() {
        // check_release() runs after record_check returns; a launch or another
        // instance's commit must be able to take the lock meanwhile (#272).
        let (root, paths, _tx) = staged_fixture("0.0.10");
        let checked = record_check(&paths).unwrap();
        assert_eq!(
            install::read_json::<u64>(&paths.state.join("last-check.json")).unwrap(),
            checked
        );
        let lock = paths.transaction_lock().unwrap();
        install::timed_lock(&lock, false, Duration::from_secs(5)).unwrap();
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn a_failed_update_check_gives_back_its_stamp_so_the_next_attempt_is_not_a_day_away() {
        // Launching offline or hitting a GitHub 5xx must not suppress automatic
        // checks for 24 hours (#297). The stamp is still written before the
        // request, for other instances' sake.
        let (root, paths, _tx) = staged_fixture("0.0.10");
        let stamp = paths.state.join("last-check.json");

        // The very first check fails: no stamp may be left behind.
        let failed = stamped_check(&paths, || {
            assert!(install::read_json::<u64>(&stamp).is_ok());
            Err(CheckError::Request(io::Error::other("offline")))
        });
        assert!(failed.is_err());
        assert!(!stamp.exists());

        // A failure restores an earlier stamp instead of erasing it.
        install::write_json(&stamp, &1234u64).unwrap();
        assert!(stamped_check(&paths, || Err(CheckError::Request(io::Error::other("503")))).is_err());
        assert_eq!(install::read_json::<u64>(&stamp).unwrap(), 1234);

        // A check recorded by another instance in the meantime stands.
        let newer = now() + 100;
        assert!(
            stamped_check(&paths, || {
                install::write_json(&stamp, &newer).unwrap();
                Err(CheckError::Request(io::Error::other("503")))
            })
            .is_err()
        );
        assert_eq!(install::read_json::<u64>(&stamp).unwrap(), newer);

        // A check that succeeds keeps its stamp, whether or not it finds an update.
        let (checked, offer) = stamped_check(&paths, || Ok(None)).unwrap();
        assert!(offer.is_none());
        assert_eq!(install::read_json::<u64>(&stamp).unwrap(), checked);
        let _ = fs::remove_dir_all(root);
    }
    /// A loopback "GitHub" answering one request with `status` and `body`.
    fn release_server(status: &str, body: &str) -> String {
        let base = scripted_server(vec![vec![format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )]]);
        format!("{base}/releases/latest")
    }
    #[test]
    fn only_a_failed_request_gives_back_the_stamp_not_a_release_github_answered_with() {
        // GitHub answering with data the updater rejects (incomplete assets,
        // invalid metadata) would answer the same ten minutes later: that costs
        // one request per day, not one per backoff (#297).
        let check = |status: &str, body: &str| {
            let url = release_server(status, body);
            move || {
                let client = build_agent(None, false);
                check_release_from(&client, &url, "0.0.9", Some("linefeed_x64.exe"))
            }
        };
        let incomplete = r#"{"tag_name":"v9.9.9","draft":false,"prerelease":false,"assets":[]}"#;
        let (root, paths, _tx) = staged_fixture("0.0.10");
        let stamp = paths.state.join("last-check.json");

        // Rejected data: the check completed, so its stamp stands.
        for (status, body) in [("200 OK", incomplete), ("200 OK", "not json")] {
            assert!(stamped_check(&paths, check(status, body)).is_err());
            assert!(install::read_json::<u64>(&stamp).is_ok(), "{body}");
            fs::remove_file(&stamp).unwrap();
        }
        // The request itself failed (here a 503): nothing was learned.
        assert!(stamped_check(&paths, check("503 Service Unavailable", "")).is_err());
        assert!(!stamp.exists());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn automatic_checks_wait_a_day_but_retry_a_failed_check_only_after_a_backoff() {
        let tick = Instant::now();
        let stamped = 1_000_000;
        // No stamp (first launch, or a failed check took its stamp back): due.
        assert!(check_due(0, stamped, None, tick));
        assert!(!check_due(stamped, stamped + CHECK_INTERVAL - 1, None, tick));
        assert!(check_due(stamped, stamped + CHECK_INTERVAL, None, tick));
        // A clock set back is not a reason to stop checking.
        assert!(check_due(stamped, stamped - 1, None, tick));
        // A missing stamp must not turn an outage into a check every idle tick.
        assert!(!check_due(0, stamped, Some(tick + CHECK_RETRY), tick));
        assert!(check_due(0, stamped, Some(tick), tick));
    }
    #[test]
    fn controller_without_updater_state_stays_disabled_and_explains_why() {
        let mut controller = Controller::default();
        controller.install_reason = Some("The updater could not start".into());
        controller.start(None, egui::Context::default(), true);
        assert!(!controller.available());
        assert!(matches!(&controller.status, Status::Error(m) if m.contains("could not start")));
        controller.check();
        controller.download();
        assert!(matches!(controller.status, Status::Error(_)));
    }
    #[test]
    fn recovery_error_offers_a_reset_that_clears_the_journal_and_asks_for_a_restart() {
        // A Prepared journal beside a manually installed newer release fails
        // closed; the only way out is the user's explicit reset (#238).
        let (root, paths, mut tx) = staged_fixture("0.0.10");
        tx.phase = Phase::Prepared;
        tx.save().unwrap();
        fs::write(&paths.target, b"manual installation").unwrap();
        let mut controller = Controller::default();
        controller.install_reason = Some("Update recovery needs attention".into());
        controller.start(Some(paths.clone()), egui::Context::default(), false);
        assert!(controller.reset_available());

        controller.reset_recovery();
        assert!(!paths.journal().exists() && !tx.directory.exists());
        assert!(!controller.reset_available());
        assert!(!matches!(controller.status, Status::Error(_)));
        // The updater stays off until a restart re-reads the (now clean) state.
        assert!(controller.install_reason.as_deref().unwrap().contains("Restart"));
        drop(controller);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn reset_is_offered_only_for_a_recovery_error() {
        let (root, paths, _tx) = staged_fixture("0.0.10");
        let mut controller = Controller::default();
        controller.start(Some(paths), egui::Context::default(), false);
        assert!(!controller.reset_available());
        // An updater that could not start has no state to reset either.
        let mut controller = Controller::default();
        controller.install_reason = Some("The updater could not start".into());
        controller.start(None, egui::Context::default(), false);
        assert!(!controller.reset_available());
        let _ = fs::remove_dir_all(root);
    }
    /// Loopback HTTP server: headers at once, then one body byte per `pause`.
    fn slow_body_server(parts: usize, pause: Duration) -> String {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/asset", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                line.clear();
            }
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {parts}\r\nConnection: close\r\n\r\n");
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            for _ in 0..parts {
                std::thread::sleep(pause);
                if stream.write_all(b"x").and_then(|()| stream.flush()).is_err() {
                    return;
                }
            }
        });
        url
    }
    /// Loopback HTTP server answering successive connections with scripted
    /// responses, each written in the given pieces with a pause in between.
    fn scripted_server(responses: Vec<Vec<String>>) -> String {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let host = base.clone();
        std::thread::spawn(move || {
            for pieces in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                    line.clear();
                }
                for piece in pieces {
                    let piece = piece.replace("{base}", &host);
                    if stream.write_all(piece.as_bytes()).and_then(|()| stream.flush()).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        base
    }
    #[test]
    fn redirect_with_headers_split_across_reads_still_downloads() {
        // GitHub answers every asset URL with a chunked 302 to its asset host.
        // ureq's own redirect handling accepted the first half of such a
        // header block and failed parsing the rest as the chunked body.
        let redirect = |location: &str| {
            vec![
                format!(
                    "HTTP/1.1 302 Found\r\nConnection: close\r\nTransfer-Encoding: chunked\r\nLocation: {location}\r\n"
                ),
                "Content-Type: text/html\r\nSet-Cookie: a=b\r\n\r\n0\r\n\r\n".to_owned(),
            ]
        };
        let asset = vec!["HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nasset".to_owned()];
        let base = scripted_server(vec![
            redirect("{base}/hop"),
            redirect("/asset"),
            asset,
        ]);
        let agent = build_agent(None, false);
        let body = limited_body(response(&agent, &format!("{base}/download")).unwrap(), 1024);
        assert_eq!(body.unwrap(), b"asset");
    }
    #[test]
    fn redirects_are_bounded_and_require_a_location() {
        let hop = || vec!["HTTP/1.1 302 Found\r\nContent-Length: 0\r\nLocation: /again\r\n\r\n".to_owned()];
        let base = scripted_server((0..7).map(|_| hop()).collect());
        let agent = build_agent(None, false);
        assert!(response(&agent, &format!("{base}/start")).is_err());
        let base = scripted_server(vec![vec![
            "HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n".to_owned(),
        ]]);
        assert!(response(&agent, &format!("{base}/start")).is_err());
    }
    #[test]
    fn redirect_targets_resolve_against_the_current_url() {
        let current: ureq::http::Uri = "https://github.com/o/r/releases/download/v1/a".parse().unwrap();
        let header = |v: &str| ureq::http::HeaderValue::from_str(v).unwrap();
        assert_eq!(
            redirect_target(&current, Some(&header("https://release-assets.githubusercontent.com/x?y=1")))
                .unwrap()
                .to_string(),
            "https://release-assets.githubusercontent.com/x?y=1"
        );
        assert_eq!(
            redirect_target(&current, Some(&header("/elsewhere?q"))).unwrap().to_string(),
            "https://github.com/elsewhere?q"
        );
        assert!(redirect_target(&current, Some(&header("relative/path"))).is_none());
        assert!(redirect_target(&current, None).is_none());
        // An http:// target parses here; the https_only agent refuses to fetch it.
        let insecure = redirect_target(&current, Some(&header("http://example.com/a"))).unwrap();
        assert!(build_agent(None, true).get(&insecure).call().is_err());
    }
    #[test]
    fn slow_asset_download_is_not_cut_off_by_the_metadata_body_limit() {
        // ureq's recv_body limit covers the whole body and is not restarted per
        // read, so the short metadata limit would abort any slow download (#240).
        let url = slow_body_server(6, Duration::from_millis(150));
        let strict = build_agent(Some(Duration::from_millis(400)), false);
        assert!(limited_body(response(&strict, &url).unwrap(), 1024).is_err());
        let url = slow_body_server(6, Duration::from_millis(150));
        let unlimited = build_agent(None, false);
        assert_eq!(
            limited_body(response(&unlimited, &url).unwrap(), 1024).unwrap(),
            b"xxxxxx"
        );
    }
    #[test]
    fn only_the_asset_download_drops_the_short_body_limit() {
        let metadata = agent();
        let download = download_agent();
        let (metadata, download) = (metadata.config().timeouts(), download.config().timeouts());
        assert_eq!(metadata.recv_body, Some(Duration::from_secs(30)));
        assert_eq!(download.recv_body, None);
        // Both stay bounded by the same overall deadline.
        assert_eq!(metadata.global, Some(Duration::from_secs(300)));
        assert_eq!(download.global, metadata.global);
    }
    #[test]
    fn failed_network_read_and_disk_write_abort_download() {
        struct Failed;
        impl Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("offline"))
            }
        }
        impl Write for Failed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(
            copy_download(
                &mut Failed,
                &mut Vec::new(),
                10,
                "unused",
                &AtomicBool::new(false),
                |_| {}
            )
            .is_err()
        );
        assert!(
            copy_download(
                &mut &b"file"[..],
                &mut Failed,
                4,
                "unused",
                &AtomicBool::new(false),
                |_| {}
            )
            .is_err()
        );
    }
}
