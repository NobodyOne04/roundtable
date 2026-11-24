//! The writer lock lives in the checkout (ADR 0012): `<git dir>/roundtable/` holds
//! `writer.lock` (an exclusive lock held for one writable turn) and `writer.json` (the crash
//! marker). Both are shared by every roundtable on the checkout, whatever its data folder,
//! and move with the checkout when it is renamed.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where roundtable runs, as far as editing is concerned.
#[derive(Debug)]
pub enum Checkout {
    /// A git checkout: its root (for thread metadata) and its lock.
    Git { root: PathBuf, lock: CheckoutLock },
    /// Not a git repository: nobody edits here (ADR 0012).
    Outside,
}

/// Finds the checkout around `cwd`. `Err` means it can't be named reliably, and edits must
/// stay locked: no git, a broken worktree, a root that doesn't contain `cwd`, or a
/// `GIT_DIR`/`GIT_WORK_TREE` override that could give one checkout two lock folders.
pub fn resolve(cwd: &Path) -> Result<Checkout, String> {
    for var in ["GIT_DIR", "GIT_WORK_TREE"] {
        if std::env::var_os(var).is_some() {
            return Err(format!("{var} is set; unset it to let the writer edit"));
        }
    }
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel", "--absolute-git-dir"])
        .current_dir(cwd)
        // English messages, so "not a git repository" is recognised whatever the locale.
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Exactly git's "no repository here or above" (also when it stops at a mount
        // point); a broken worktree says "not a git repository: <gitdir>" and must not count.
        if stderr.contains("not a git repository (or any of the parent directories)")
            || stderr.contains("not a git repository (or any parent up to mount point")
        {
            return Ok(Checkout::Outside);
        }
        return Err(stderr.trim().to_string());
    }
    let stdout = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    // One path per line; only the line terminator is stripped, a name may end in spaces.
    let mut lines = stdout.split('\n');
    let (Some(root), Some(git_dir)) = (lines.next(), lines.next()) else {
        return Err(format!("unexpected git output: {stdout:?}"));
    };
    let root = PathBuf::from(root);
    match (root.canonicalize(), cwd.canonicalize()) {
        (Ok(r), Ok(c)) if c.starts_with(&r) => Ok(Checkout::Git {
            root,
            lock: CheckoutLock {
                dir: PathBuf::from(git_dir).join("roundtable"),
            },
        }),
        _ => Err(format!(
            "git names {} as the checkout, which doesn't contain {}",
            root.display(),
            cwd.display()
        )),
    }
}

/// What a crash marker records, to show the user who left it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub pid: u32,
    pub started_ms: u64,
    pub thread: String,
    pub data_root: PathBuf,
}

/// The state of the crash marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerState {
    Absent,
    /// `text` is for showing; `key` fingerprints the raw bytes, so `/quiet` removes exactly
    /// the marker the user saw, even if two markers would display alike.
    Present {
        text: String,
        key: String,
    },
    /// The file exists but can't be read: it can't be identified, so `/quiet` can't confirm it.
    Unreadable(String),
}

/// Why a writable turn may not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Another roundtable holds the lock: it is editing this checkout right now.
    Busy,
    /// An earlier writer never finished; edits stay locked until `/quiet`.
    Unfinished(MarkerState),
    Failed(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Busy => write!(f, "another roundtable is editing this checkout"),
            Refusal::Unfinished(_) => write!(f, "an earlier writer never finished; see /quiet"),
            Refusal::Failed(why) => write!(f, "the writer lock failed: {why}"),
        }
    }
}

/// The outcome of `/quiet`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cleared {
    /// Removed; the marker it removed, parsed when readable.
    Removed(Option<Marker>),
    /// The marker is not the one the user confirmed: confirm this one instead.
    Changed(MarkerState),
    /// A writer holds the lock right now: nothing to clear.
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutLock {
    dir: PathBuf,
}

impl CheckoutLock {
    #[cfg(test)]
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn marker_path(&self) -> PathBuf {
        self.dir.join("writer.json")
    }

    /// Creates the folder (owner-only) and makes its entry durable the first time.
    fn ensure_dir(&self) -> io::Result<()> {
        if self.dir.is_dir() {
            return Ok(());
        }
        crate::transcript::private_dir(&self.dir)?;
        sync_dir(self.dir.parent().unwrap_or(&self.dir))
    }

    fn try_lock(&self) -> io::Result<Option<File>> {
        self.ensure_dir()?;
        let file = OpenOptions::new()
            .mode(0o600)
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join("writer.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    pub fn marker(&self) -> MarkerState {
        match fs::read(self.marker_path()) {
            Ok(bytes) => MarkerState::Present {
                text: String::from_utf8_lossy(&bytes).into_owned(),
                key: fingerprint(&bytes),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => MarkerState::Absent,
            Err(e) => MarkerState::Unreadable(e.to_string()),
        }
    }

    /// At startup: a marker while nobody holds the lock means a writer crashed.
    pub fn crashed(&self) -> Option<MarkerState> {
        match self.marker() {
            MarkerState::Absent => None,
            state => match self.try_lock() {
                // Someone holds it: a live writer, not a crash; its turn will clear it.
                Ok(None) => None,
                Ok(Some(_released_on_drop)) => Some(state),
                Err(e) => Some(MarkerState::Unreadable(format!("writer.lock: {e}"))),
            },
        }
    }

    /// Takes the lock and writes a durable marker, in that order (ADR 0012). Call before
    /// the turn's delivery is recorded as sent.
    pub fn begin(&self, marker: &Marker) -> Result<WriterGuard, Refusal> {
        let fail = |e: io::Error| Refusal::Failed(e.to_string());
        let lock = self.try_lock().map_err(fail)?.ok_or(Refusal::Busy)?;
        match self.marker() {
            MarkerState::Absent => {}
            state => return Err(Refusal::Unfinished(state)),
        }
        let bytes =
            serde_json::to_vec_pretty(marker).map_err(|e| Refusal::Failed(e.to_string()))?;
        crate::transcript::write_atomic(&self.marker_path(), &bytes).map_err(fail)?;
        sync_dir(&self.dir).map_err(fail)?;
        Ok(WriterGuard {
            lock: self.clone(),
            _file: lock,
        })
    }

    /// `/quiet`: removes the marker the user saw, only if it is still exactly that one and
    /// no writer holds the lock, and syncs before reporting success.
    /// `confirmed` is the `key` of the marker the user saw.
    pub fn clear(&self, confirmed: &str) -> io::Result<Cleared> {
        let Some(_held) = self.try_lock()? else {
            return Ok(Cleared::Busy);
        };
        let text = match self.marker() {
            MarkerState::Absent => return Ok(Cleared::Removed(None)),
            MarkerState::Present { text, key } if key == confirmed => text,
            other => return Ok(Cleared::Changed(other)),
        };
        match fs::remove_file(self.marker_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        sync_dir(&self.dir)?;
        Ok(Cleared::Removed(serde_json::from_str(&text).ok()))
    }
}

/// Held for one writable turn: the lock and the marker. `finish` clears both; dropping it
/// without `finish` (a crash, a timeout at quit) leaves the marker, which is the point.
#[derive(Debug)]
pub struct WriterGuard {
    lock: CheckoutLock,
    _file: File,
}

impl WriterGuard {
    /// After the CLI is reaped and its reply recorded: remove the marker durably, then let
    /// the lock go.
    pub fn finish(self) -> io::Result<()> {
        fs::remove_file(self.lock.marker_path())?;
        sync_dir(&self.lock.dir)
    }
}

fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Lets tests and callers build a marker for this process.
pub fn marker_for(thread: &str, data_root: &Path) -> Marker {
    Marker {
        pid: std::process::id(),
        started_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        thread: thread.into(),
        data_root: data_root.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> (tempfile::TempDir, CheckoutLock) {
        let dir = tempfile::tempdir().unwrap();
        let lock = CheckoutLock::at(dir.path().join("roundtable"));
        (dir, lock)
    }

    #[test]
    fn one_writer_at_a_time_across_instances() {
        let (_d, lock) = lock();
        let guard = lock.begin(&marker_for("T1", Path::new("/data"))).unwrap();
        // A second instance (another handle, maybe another data root) is refused.
        let other = CheckoutLock::at(lock.dir.clone());
        assert_eq!(
            other
                .begin(&marker_for("T2", Path::new("/other")))
                .unwrap_err(),
            Refusal::Busy
        );
        assert_eq!(other.crashed(), None, "a live writer is not a crash");
        guard.finish().unwrap();
        assert_eq!(lock.marker(), MarkerState::Absent);
        other.begin(&marker_for("T2", Path::new("/other"))).unwrap();
    }

    #[test]
    fn a_dropped_guard_is_a_crash_until_quiet() {
        let (_d, lock) = lock();
        drop(lock.begin(&marker_for("T1", Path::new("/data"))).unwrap());
        let Some(MarkerState::Present { key: shown, .. }) = lock.crashed() else {
            panic!("crash not seen")
        };
        assert!(matches!(
            lock.begin(&marker_for("T2", Path::new("/data"))),
            Err(Refusal::Unfinished(_))
        ));
        assert!(
            matches!(lock.clear("something else").unwrap(), Cleared::Changed(MarkerState::Present { key, .. }) if key == shown),
            "only the marker the user saw is removed"
        );
        assert!(
            matches!(lock.clear(&shown).unwrap(), Cleared::Removed(Some(m)) if m.thread == "T1")
        );
        assert_eq!(lock.crashed(), None);
        lock.begin(&marker_for("T3", Path::new("/data"))).unwrap();
    }

    #[test]
    fn markers_that_display_alike_are_still_different() {
        let (_d, lock) = lock();
        fs::create_dir_all(&lock.dir).unwrap();
        // Two invalid-UTF-8 markers both display as the replacement character.
        fs::write(lock.marker_path(), [0xff]).unwrap();
        let MarkerState::Present { key: first, .. } = lock.marker() else {
            panic!()
        };
        fs::write(lock.marker_path(), [0xfe]).unwrap();
        assert!(matches!(lock.clear(&first).unwrap(), Cleared::Changed(_)));
        assert!(matches!(lock.marker(), MarkerState::Present { .. }));
    }

    #[test]
    fn quiet_does_nothing_while_a_writer_runs() {
        let (_d, lock) = lock();
        let _guard = lock.begin(&marker_for("T1", Path::new("/data"))).unwrap();
        let shown = match lock.marker() {
            MarkerState::Present { key, .. } => key,
            other => panic!("{other:?}"),
        };
        let other = CheckoutLock::at(lock.dir.clone());
        assert_eq!(other.clear(&shown).unwrap(), Cleared::Busy);
        assert!(matches!(lock.marker(), MarkerState::Present { .. }));
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .status()
            .unwrap();
        assert!(ok.success());
    }

    #[test]
    fn resolves_the_git_checkout_and_its_lock_folder() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo ");
        fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "-q"]);
        match resolve(&repo.join("src")).unwrap() {
            Checkout::Git { root, lock } => {
                assert_eq!(root.canonicalize().unwrap(), repo.canonicalize().unwrap());
                assert!(lock.dir.ends_with(".git/roundtable"), "{:?}", lock.dir);
            }
            other => panic!("{other:?}"),
        }
        let outside = tempfile::tempdir().unwrap();
        assert!(matches!(
            resolve(outside.path()).unwrap(),
            Checkout::Outside
        ));

        let broken = dir.path().join("broken");
        fs::create_dir(&broken).unwrap();
        fs::write(broken.join(".git"), "gitdir: /nonexistent/worktrees/x\n").unwrap();
        assert!(
            resolve(&broken).is_err(),
            "a broken worktree is not outside"
        );
    }

    #[test]
    fn the_lock_moves_with_a_renamed_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("a");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let Checkout::Git { lock, .. } = resolve(&repo).unwrap() else {
            panic!()
        };
        drop(lock.begin(&marker_for("T1", Path::new("/data"))).unwrap());
        let renamed = dir.path().join("b");
        fs::rename(&repo, &renamed).unwrap();
        let Checkout::Git { lock, .. } = resolve(&renamed).unwrap() else {
            panic!()
        };
        assert!(
            matches!(lock.crashed(), Some(MarkerState::Present { .. })),
            "the crash is still found after the rename"
        );
    }
}
