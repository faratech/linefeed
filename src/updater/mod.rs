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
    sender: Option<mpsc::Sender<Command>>,
    events: Option<mpsc::Receiver<Event>>,
    automatic: Arc<AtomicBool>,
    canceled: Arc<AtomicBool>,
}

impl Controller {
    pub fn start(&mut self, paths: Paths, context: egui::Context, automatic: bool) {
        self.automatic.store(automatic, Ordering::Relaxed);
        let recovery_error = self.install_reason.is_some();
        if recovery_error {
            self.status = Status::Error(self.install_reason.clone().unwrap());
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
            if tx.phase == Phase::Staged
                && install::hash(&paths.target).ok().as_deref() == Some(&tx.old_hash)
            {
                self.status = Status::Ready;
                self.staged = Some(tx);
            } else if tx.phase == Phase::Healthy {
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
    pub fn set_automatic(&self, enabled: bool) {
        self.automatic.store(enabled, Ordering::Relaxed);
    }
    pub fn busy(&self) -> bool {
        matches!(
            self.status,
            Status::Checking | Status::Downloading { .. } | Status::Applying
        )
    }
    pub fn check(&mut self) {
        if !self.busy() {
            self.send(Command::Check);
            self.status = Status::Checking;
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
                let current = now();
                if current >= last && current - last < CHECK_INTERVAL {
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
                    let lock = paths.transaction_lock()?;
                    // Serialize cadence and staging across running instances.
                    lock.try_lock().map_err(io::Error::other)?;
                    let checked = now();
                    install::write_json(&paths.state.join("last-check.json"), &checked)?;
                    emit(&events, &context, Event::Checked(checked));
                    offer = check_release()?;
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
                    let lock = paths.transaction_lock()?;
                    lock.try_lock().map_err(io::Error::other)?;
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

fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(5)
        .timeout_global(Some(Duration::from_secs(300)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(30)))
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}
fn response(agent: &ureq::Agent, url: &str) -> io::Result<ureq::http::Response<ureq::Body>> {
    agent
        .get(url)
        .header(
            "User-Agent",
            concat!("linefeed/", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2026-03-10")
        .call()
        .map_err(|_| {
            io::Error::other("GitHub request failed; check your connection or try again later")
        })
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
fn check_release() -> io::Result<Option<Offer>> {
    let client = agent();
    let release: Release = serde_json::from_slice(&limited_body(
        response(
            &client,
            &format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"),
        )?,
        1024 * 1024,
    )?)
    .map_err(|_| io::Error::other("GitHub returned invalid release metadata"))?;
    select_release(release, env!("CARGO_PKG_VERSION"), current_asset())
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
    let previous = install::read_json::<Transaction>(&paths.journal()).ok();
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
    let (attempt, directory) = install::stage_directory(paths)?;
    let result = (|| {
        let partial = directory.join("download.part");
        let mut file = install::new_file(&partial)?;
        let mut response = response(&client, &offer.asset.browser_download_url)?;
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
        platform::verify_signature(&tx.candidate())?;
        install::executable_copy(&paths.target, &tx.helper())?;
        platform::sync_dir(&directory)?;
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
        Ok(tx)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(directory);
    }
    result
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
