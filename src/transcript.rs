//! The transcript as plain files (ADR 0010): one folder per thread, an append-only
//! `events.jsonl`, small JSON files written atomically, and a single-instance lock.
//!
//! ```text
//! <root>/roundtable.lock
//! <root>/threads/<ulid>/meta.json
//! <root>/threads/<ulid>/events.jsonl
//! ```
//!
//! Still to land: the writer task and its acks into the event loop; key redaction (with the
//! first resolved key, ADR 0005); `usage.json` (with `/usage`, ADR 0008).

use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("roundtable is already running (pid {pid})")]
    AlreadyRunning { pid: String },
    #[error("{path}: line {line} is not a valid event: {source}")]
    Corrupt {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
    #[error("no thread {0}")]
    NoThread(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Who sent or received an event. `identity` names the exact destination (ADR 0010);
/// `None` for the user and the router.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Msg,
    ToolCall,
    ToolResult,
    Handoff,
    Delivery,
    Settle,
    Approval,
    Error,
    Status,
}

/// An event before it is written; the thread assigns `seq` and `ts`.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    pub turn: Option<u64>,
    pub parent: Option<u64>,
    pub from: Party,
    pub to: Vec<Party>,
    pub kind: Kind,
    pub payload: serde_json::Value,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
}

/// One line of `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventLine {
    pub seq: u64,
    /// Unix milliseconds.
    pub ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    pub from: Party,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<Party>,
    pub kind: Kind,
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    /// Canonical path of the checkout the thread belongs to.
    pub checkout: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_event: Option<u64>,
}

/// The data root, held exclusively by one running roundtable.
pub struct Store {
    root: PathBuf,
    // Monotonic: two threads made in the same millisecond still sort in creation order.
    ids: ulid::Generator,
    // Held, never read: the OS lock lives as long as this handle.
    _lock: File,
}

impl Store {
    /// Takes the single-instance lock, so start-up recovery can never touch a live turn
    /// of another instance.
    pub fn open(root: &Path) -> Result<Self> {
        private_dir(root)?;
        private_dir(&root.join("threads"))?;
        let path = root.join("roundtable.lock");
        let mut lock = OpenOptions::new()
            .mode(0o600)
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut pid = String::new();
                lock.read_to_string(&mut pid)?;
                let pid = pid.trim();
                let pid = if pid.is_empty() { "unknown" } else { pid };
                return Err(Error::AlreadyRunning { pid: pid.into() });
            }
            Err(TryLockError::Error(err)) => return Err(err.into()),
        }
        lock.set_len(0)?;
        write!(lock, "{}", std::process::id())?;
        Ok(Self {
            root: root.to_path_buf(),
            ids: ulid::Generator::new(),
            _lock: lock,
        })
    }

    /// Only a ULID names a thread: an id from a marker or a file is never trusted as a path
    /// (no `..`, no absolute path).
    fn thread_dir(&self, id: &str) -> Result<PathBuf> {
        if id.len() != 26 || ulid::Ulid::from_string(id).is_err() {
            return Err(Error::NoThread(format!("{id:?} is not a thread id")));
        }
        Ok(self.root.join("threads").join(id))
    }

    pub fn create_thread(&mut self, title: &str, checkout: &Path) -> Result<Thread> {
        let id = self
            .ids
            .generate()
            .map_err(|err| io::Error::other(err.to_string()))?
            .to_string();
        let dir = self.thread_dir(&id)?;
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let meta = Meta {
            id: id.clone(),
            title: title.into(),
            created_at: now_ms(),
            checkout: checkout.canonicalize()?,
            parent_id: None,
            fork_event: None,
        };
        write_atomic(&dir.join("meta.json"), &serde_json::to_vec_pretty(&meta)?)?;
        Thread::open(dir, meta)
    }

    pub fn open_thread(&self, id: &str) -> Result<Thread> {
        let dir = self.thread_dir(id)?;
        let meta = match fs::read(dir.join("meta.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(Error::NoThread(id.into()));
            }
            Err(err) => return Err(err.into()),
        };
        Thread::open(dir, meta)
    }

    /// Closes thread `id`'s unfinished writer turn with a durable `settle`, after the user
    /// confirmed the checkout is quiet (ADR 0012). Never recreates a missing transcript: that
    /// would "settle" a history that no longer exists.
    pub fn settle_writer(&self, id: &str) -> Result<()> {
        if !self.thread_dir(id)?.join("events.jsonl").is_file() {
            return Err(Error::NoThread(format!("{id} (events.jsonl is missing)")));
        }
        let mut thread = self.open_thread(id)?;
        thread.append(settle_event(), true)?;
        Ok(())
    }

    /// All threads, newest first. A folder without a readable `meta.json` is skipped.
    pub fn list_threads(&self) -> Result<Vec<Meta>> {
        let mut metas = Vec::new();
        for entry in fs::read_dir(self.root.join("threads"))? {
            let path = entry?.path().join("meta.json");
            match fs::read(&path).map(|b| serde_json::from_slice::<Meta>(&b)) {
                Ok(Ok(meta)) => metas.push(meta),
                other => tracing::warn!(path = %path.display(), ?other, "skipping thread"),
            }
        }
        // ULIDs sort by creation time.
        metas.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(metas)
    }
}

/// An open thread: its metadata and its transcript, open for appending.
pub struct Thread {
    pub meta: Meta,
    dir: PathBuf,
    events_path: PathBuf,
    file: File,
    next_seq: u64,
    /// Set when a failed append could not be undone: the file may end in a partial line, so
    /// nothing more is appended until the thread is reopened (which repairs the tail).
    broken: bool,
}

impl Thread {
    fn open(dir: PathBuf, meta: Meta) -> Result<Self> {
        let events_path = dir.join("events.jsonl");
        let mut file = OpenOptions::new()
            .mode(0o600)
            .read(true)
            .append(true)
            .create(true)
            .open(&events_path)?;
        let (good_len, lines) = scan(&mut file, &events_path)?;
        if good_len < file.metadata()?.len() {
            // A crash left a torn last line: drop it before the next append.
            tracing::warn!(path = %events_path.display(), "truncating torn last line");
            file.set_len(good_len)?;
            file.sync_data()?;
        }
        Ok(Self {
            meta,
            dir,
            events_path,
            file,
            next_seq: lines,
            broken: false,
        })
    }

    /// Appends one line and returns its `seq`. `durable` syncs it to disk before returning:
    /// use it for lines that gate an action (a handoff before dispatch, a writable turn
    /// before spawn, a `settle`).
    pub fn append(&mut self, event: NewEvent, durable: bool) -> Result<u64> {
        let line = EventLine {
            seq: self.next_seq,
            ts: now_ms(),
            turn: event.turn,
            parent: event.parent,
            from: event.from,
            to: event.to,
            kind: event.kind,
            payload: event.payload,
            tokens_in: event.tokens_in,
            tokens_out: event.tokens_out,
        };
        if self.broken {
            return Err(Error::Io(io::Error::other(
                "an earlier write to this transcript failed; reopen the thread",
            )));
        }
        let mut bytes = serde_json::to_vec(&line)?;
        bytes.push(b'\n');
        let before = self.file.metadata()?.len();
        // One write call per line, so a crash tears at most this line.
        if let Err(err) = self.file.write_all(&bytes) {
            // Undo a partial line, or the next append would bury it mid-file as corruption.
            if self.file.set_len(before).is_err() {
                self.broken = true;
            }
            return Err(err.into());
        }
        // The line is in the file now: its number is used even if the sync below fails.
        self.next_seq += 1;
        if durable {
            self.file.sync_data()?;
        }
        Ok(line.seq)
    }

    /// The native CLI session of each participant in this thread (`sessions.json`), so the
    /// next turn resumes it.
    /// Keys are `id/identity` (ADR 0010), built by the caller.
    pub fn save_sessions(
        &self,
        sessions: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        write_atomic(
            &self.dir.join("sessions.json"),
            &serde_json::to_vec_pretty(sessions)?,
        )?;
        Ok(())
    }

    /// Every event, in order. The thread state is a fold over these (ADR 0010).
    pub fn events(&self) -> Result<Vec<EventLine>> {
        let reader = BufReader::new(File::open(&self.events_path)?);
        let mut events = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            let event = serde_json::from_str(&line).map_err(|source| Error::Corrupt {
                path: self.events_path.clone(),
                line: i + 1,
                source,
            })?;
            events.push(event);
        }
        Ok(events)
    }
}

/// The `settle` that closes a crashed writer turn after `/quiet`.
pub fn settle_event() -> NewEvent {
    NewEvent {
        turn: None,
        parent: None,
        from: Party {
            id: "user".into(),
            identity: None,
        },
        to: vec![],
        kind: Kind::Settle,
        payload: serde_json::json!({ "writable": true, "status": "interrupted" }),
        tokens_in: None,
        tokens_out: None,
    }
}

/// Returns the byte length of the valid prefix and how many lines it holds. Only the last
/// line may be torn; a bad line before it is corruption and an error.
fn scan(file: &mut File, path: &Path) -> Result<(u64, u64)> {
    file.seek(SeekFrom::Start(0))?;
    // Line by line, so reopening a long transcript never loads it whole.
    let mut reader = BufReader::new(&mut *file);
    let mut line = Vec::new();
    let (mut good_len, mut count) = (0, 0);
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 || !line.ends_with(b"\n") {
            break; // end, or a torn last line (no newline)
        }
        match serde_json::from_slice::<EventLine>(&line) {
            Ok(_) => {
                good_len += read as u64;
                count += 1;
            }
            Err(source) => {
                return Err(Error::Corrupt {
                    path: path.into(),
                    line: count as usize + 1,
                    source,
                });
            }
        }
    }
    Ok((good_len, count))
}

/// Creates `path` (and parents) readable by the owner only, and tightens it if it already
/// exists: the transcript holds prompts and replies. Everything under an owner-only folder
/// is out of reach for other users, including files from before this rule.
pub fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    fs::set_permissions(path, Permissions::from_mode(0o700))
}

/// Writes a small file so readers see either the old or the new content, never a mix.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // A temp name of its own, created fresh: two writers (e.g. two roundtables sharing
    // `models.toml`) never write into each other's temp file.
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let tmp = path.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .mode(0o600)
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_data()?;
    fs::rename(&tmp, path)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user_msg(text: &str) -> NewEvent {
        NewEvent {
            turn: None,
            parent: None,
            from: Party {
                id: "user".into(),
                identity: None,
            },
            to: vec![Party {
                id: "claude".into(),
                identity: Some("9f2c".into()),
            }],
            kind: Kind::Msg,
            payload: json!({ "text": text }),
            tokens_in: None,
            tokens_out: None,
        }
    }

    #[test]
    fn second_instance_is_refused_with_the_pid() {
        let dir = tempfile::tempdir().unwrap();
        let _first = Store::open(dir.path()).unwrap();
        let err = Store::open(dir.path()).err().unwrap();
        let expected = format!("roundtable is already running (pid {})", std::process::id());
        assert_eq!(err.to_string(), expected);
    }

    #[test]
    fn lock_is_released_when_the_store_drops() {
        let dir = tempfile::tempdir().unwrap();
        drop(Store::open(dir.path()).unwrap());
        // A process forked by another test holds a copy of every open file until it execs,
        // the lock included, so allow that brief window instead of failing on it.
        let reopened = (0..50).any(|_| {
            Store::open(dir.path()).is_ok()
                || (
                    std::thread::sleep(std::time::Duration::from_millis(20)),
                    false,
                )
                    .1
        });
        assert!(reopened, "the lock outlived its store");
    }

    #[test]
    fn everything_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, Permissions::from_mode(0o755)).unwrap(); // from an older run
        let mut store = Store::open(&root).unwrap();
        let mut thread = store.create_thread("t", dir.path()).unwrap();
        thread.append(user_msg("secret"), true).unwrap();
        thread.save_sessions(&Default::default()).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let tdir = root.join("threads").join(&thread.meta.id);
        assert_eq!(mode(&root), 0o700, "an existing root is tightened");
        assert_eq!(mode(&root.join("threads")), 0o700);
        assert_eq!(mode(&tdir), 0o700);
        for file in ["meta.json", "events.jsonl", "sessions.json"] {
            assert_eq!(mode(&tdir.join(file)), 0o600, "{file}");
        }
        assert_eq!(mode(&root.join("roundtable.lock")), 0o600);
    }

    #[test]
    fn appends_and_reads_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let mut thread = store.create_thread("t", dir.path()).unwrap();
        assert_eq!(thread.append(user_msg("one"), false).unwrap(), 0);
        assert_eq!(thread.append(user_msg("two"), true).unwrap(), 1);
        let events = thread.events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].seq, 1);
        assert_eq!(events[1].payload["text"], "two");
        assert_eq!(events[0].to[0].identity.as_deref(), Some("9f2c"));
    }

    #[test]
    fn torn_last_line_is_dropped_and_seq_continues() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let mut thread = store.create_thread("t", dir.path()).unwrap();
        thread.append(user_msg("kept"), true).unwrap();
        let id = thread.meta.id.clone();
        drop(thread);
        let path = dir.path().join("threads").join(&id).join("events.jsonl");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"seq":1,"ts":5,"fr"#)
            .unwrap();

        let mut thread = store.open_thread(&id).unwrap();
        assert_eq!(thread.append(user_msg("after"), true).unwrap(), 1);
        let texts: Vec<_> = thread
            .events()
            .unwrap()
            .iter()
            .map(|e| e.payload["text"].clone())
            .collect();
        assert_eq!(texts, [json!("kept"), json!("after")]);
    }

    #[test]
    fn a_bad_line_in_the_middle_is_an_error_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let mut thread = store.create_thread("t", dir.path()).unwrap();
        thread.append(user_msg("a"), true).unwrap();
        let id = thread.meta.id.clone();
        drop(thread);
        let path = dir.path().join("threads").join(&id).join("events.jsonl");
        let good = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("not json\n{good}")).unwrap();
        assert!(matches!(
            store.open_thread(&id),
            Err(Error::Corrupt { line: 1, .. })
        ));
    }

    #[test]
    fn threads_keep_their_checkout_and_list_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let first = store.create_thread("first", dir.path()).unwrap();
        let second = store.create_thread("second", dir.path()).unwrap();
        let listed = store.list_threads().unwrap();
        assert_eq!(listed, [second.meta.clone(), first.meta.clone()]);
        assert_eq!(listed[0].checkout, dir.path().canonicalize().unwrap());
        let thread_dir = dir.path().join("threads").join(&first.meta.id);
        let leftovers = fs::read_dir(&thread_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0, "atomic write leaves no temp file");
        assert!(matches!(store.open_thread("nope"), Err(Error::NoThread(_))));
        for bad in ["../../etc", "/tmp/x", "01M86T4JZ0H3R8743FJPQ596S/"] {
            assert!(
                matches!(store.settle_writer(bad), Err(Error::NoThread(_))),
                "{bad}"
            );
        }
    }
}
