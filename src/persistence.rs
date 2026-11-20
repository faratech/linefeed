//! Private, atomic configuration writes and bounded cross-process locking.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn temporary_path(path: &Path) -> io::Result<PathBuf> {
    let filename = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "missing destination filename")
    })?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let nonce: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut name = std::ffi::OsString::from(".");
    name.push(filename);
    name.push(format!(".{nonce}.tmp"));
    Ok(path.with_file_name(name))
}

/// Each writer owns a different sibling file. In particular, a concurrent
/// writer cannot retain a writable handle to the inode we just published.
pub(crate) fn write_private_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = parent_dir(path);
    fs::create_dir_all(parent)?;
    let temporary = temporary_path(path)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // If exclusive creation fails, the file is not ours to clean up.
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        // Windows requires our write handle to close before replacement.
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// `bytes` without one leading UTF-8 byte order mark. Windows tools (Notepad,
/// PowerShell 5 `Out-File -Encoding utf8`) write it, and JSON parsers reject it.
pub(crate) fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)
}

/// Move an unreadable file aside to `<name>.bak`, or the first free
/// `<name>.bak.N`, so an earlier backup is never overwritten. A rename keeps
/// the file's private mode (these files can hold plaintext credentials).
pub(crate) fn quarantine(path: &Path) -> io::Result<PathBuf> {
    let filename = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing source filename"))?;
    let mut name = filename.to_owned();
    name.push(".bak");
    let mut candidate = path.with_file_name(&name);
    let mut n = 1u32;
    while candidate.symlink_metadata().is_ok() {
        let mut numbered = filename.to_owned();
        numbered.push(format!(".bak.{n}"));
        candidate = path.with_file_name(numbered);
        n += 1;
    }
    fs::rename(path, &candidate)?;
    Ok(candidate)
}

/// The caller retains this file for the entire read-modify-write transaction.
/// The lock file must remain at its stable path, even after its handle closes.
pub(crate) fn exclusive_lock(path: &Path, wait: Duration) -> io::Result<File> {
    fs::create_dir_all(parent_dir(path))?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "another Linefeed instance is writing configuration",
                    ));
                }
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Err(error) => return Err(io::Error::other(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = temporary_path(&std::env::temp_dir().join("linefeed-persistence")).unwrap();
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn concurrent_writers_publish_only_complete_payloads() {
        let fixture = Fixture::new();
        let path = fixture.0.join("settings.json");
        let payloads: Arc<Vec<Vec<u8>>> = Arc::new(
            (0..8)
                .map(|writer| {
                    serde_json::to_vec(&serde_json::json!({
                        "writer": writer,
                        "message": "x".repeat(writer * 127 + 1),
                    }))
                    .unwrap()
                })
                .collect(),
        );
        write_private_atomic(&path, &payloads[0]).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let observer = {
            let path = path.clone();
            let payloads = payloads.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    let data = match fs::read(&path) {
                        Ok(data) => data,
                        // Windows briefly denies opening a file while a rename
                        // replaces it; that is not a partial payload.
                        Err(error)
                            if cfg!(windows)
                                && error.kind() == std::io::ErrorKind::PermissionDenied =>
                        {
                            std::thread::yield_now();
                            continue;
                        }
                        Err(error) => panic!("read during replacement: {error}"),
                    };
                    assert!(
                        payloads.contains(&data),
                        "a partial/mixed payload was published"
                    );
                    std::thread::yield_now();
                }
            })
        };
        let start = Arc::new(Barrier::new(payloads.len()));
        let writers: Vec<_> = (0..payloads.len())
            .map(|writer| {
                let path = path.clone();
                let payloads = payloads.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..8 {
                        write_private_atomic(&path, &payloads[writer]).unwrap();
                    }
                })
            })
            .collect();
        let results: Vec<_> = writers.into_iter().map(|writer| writer.join()).collect();
        done.store(true, Ordering::Release);
        observer.join().unwrap();
        for result in results {
            result.unwrap();
        }
        assert!(payloads.contains(&fs::read(&path).unwrap()));
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn failed_replacement_cleans_up_only_its_temporary_file() {
        let fixture = Fixture::new();
        let destination = fixture.0.join("settings.json");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("original"), b"preserved").unwrap();
        assert!(write_private_atomic(&destination, b"replacement").is_err());
        assert_eq!(
            fs::read(destination.join("original")).unwrap(),
            b"preserved"
        );
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn replacement_files_remain_private() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let path = fixture.0.join("settings.json");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private_atomic(&path, b"secret").unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn exclusive_lock_is_bounded_and_released_on_drop() {
        let fixture = Fixture::new();
        let path = fixture.0.join("sts_policies.json.lock");
        let first = exclusive_lock(&path, Duration::ZERO).unwrap();
        let error = exclusive_lock(&path, Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(first);
        exclusive_lock(&path, Duration::ZERO).unwrap();
        assert!(
            path.exists(),
            "removing the lock file would allow different lock inodes"
        );
    }

    #[test]
    fn lock_child_process() {
        let Some(path) = std::env::var_os("LINEFEED_TEST_LOCK_PATH") else {
            return;
        };
        let error = exclusive_lock(Path::new(&path), Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn exclusive_lock_excludes_another_process() {
        let fixture = Fixture::new();
        let path = fixture.0.join("sts_policies.json.lock");
        let _lock = exclusive_lock(&path, Duration::ZERO).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "persistence::tests::lock_child_process",
                "--nocapture",
            ])
            .env("LINEFEED_TEST_LOCK_PATH", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
