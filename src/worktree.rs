//! A git worktree per read-only CLI agent (ADR 0016): `<data_root>/worktrees/<checkout-id>/<id>/`,
//! brought to the writer's current state (HEAD plus uncommitted and untracked files) before
//! each of that agent's turns, so what escapes its read-only flags lands in the mirror.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// `<id>.json` next to a mirror: cleanup only ever acts on mirrors listed in one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub source: PathBuf,
    pub mirror: PathBuf,
    /// The mirror folder's device and inode, written once git made it: a path can be
    /// reused, a folder's identity can't, so nothing is reset or deleted without a match.
    #[serde(default)]
    pub folder: Option<(u64, u64)>,
}

/// A folder's (device, inode), links not followed.
fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.is_dir())
        .map(|m| (m.dev(), m.ino()))
}

/// The folder holding one checkout's mirrors and their records.
pub fn checkout_dir(data_root: &Path, checkout: &Path) -> PathBuf {
    data_root
        .join("worktrees")
        .join(crate::registry::fingerprint(&checkout.to_string_lossy()))
}

/// The folder a turn of `id` runs in: `sub`, the launch folder, inside its mirror.
pub fn cwd(data_root: &Path, checkout: &Path, id: &str, sub: &Path) -> PathBuf {
    let mirror = checkout_dir(data_root, checkout).join(id);
    // `join("")` would add a trailing slash, and the path keys sessions.
    if sub.as_os_str().is_empty() {
        mirror
    } else {
        mirror.join(sub)
    }
}

/// Repository overrides (an index, an object store, config) that would point git at another
/// repository's state: `git rev-parse --local-env-vars`, plus config injection. Cleared for
/// every git call here and for the CLI that runs in a mirror.
pub const REPO_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// Creates the mirror of `id` if needed and syncs it with `checkout`; returns the folder
/// `sub` (the launch folder, relative to the root) inside it, created if git has nothing
/// there. Blocking (runs git); `cancel` stops it between files and kills a running git. Any
/// failure is the turn's failure, never a fallback to the checkout.
pub fn sync(
    data_root: &Path,
    checkout: &Path,
    id: &str,
    sub: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let (Ok(data), Ok(source)) = (data_root.canonicalize(), checkout.canonicalize()) else {
        return Err("can't resolve the data folder or the checkout".into());
    };
    if data.starts_with(&source) || source.starts_with(&data) {
        // A mirror inside its own source would copy roundtable's files into the agent's.
        return Err(format!(
            "the data folder {} and the checkout {} overlap: no mirror",
            data.display(),
            source.display()
        ));
    }
    // The writer runs with roundtable's environment: an index or object override there
    // would make its state differ from what a plain read of the checkout shows.
    if let Some(var) = REPO_ENV.iter().find(|v| std::env::var_os(v).is_some()) {
        return Err(format!("{var} is set: unset it to give agents mirrors"));
    }
    let git = |args: &[&str], input: Option<&[u8]>| git(checkout, args, input, cancel);
    let head = match git(&["rev-parse", "--verify", "-q", "HEAD^{commit}"], None) {
        Ok(head) => head,
        Err(_) if cancel.load(Ordering::Relaxed) => return Err("cancelled".into()),
        // shortcut: ADR 0016's `worktree add --orphan` would add a branch to the owner's
        // repo; refused until the first commit instead.
        Err(_) => {
            return Err(
                "the checkout has no commit yet: commit once to give agents a mirror".into(),
            );
        }
    };
    let head = String::from_utf8_lossy(&head).trim().to_string();
    // shortcut: submodules aren't mirrored (`worktree add` leaves them empty); a populated
    // one fails the turn rather than show the reviewer an empty folder. Mirror them if asked.
    let stage = git(&["ls-files", "-z", "--stage"], None)?;
    for entry in stage
        .split(|b| *b == 0)
        .filter(|e| e.starts_with(b"160000 "))
    {
        let path = entry.splitn(2, |b| *b == b'\t').nth(1).unwrap_or_default();
        let path = Path::new(OsStr::from_bytes(path));
        if checkout.join(path).join(".git").exists() {
            return Err(format!(
                "{} is a submodule: mirrors don't carry submodules yet",
                path.display()
            ));
        }
    }
    let dir = checkout_dir(data_root, checkout);
    for folder in [&data_root.join("worktrees"), &dir] {
        crate::transcript::owned_private_dir(folder)
            .map_err(|e| format!("{}: {e}", folder.display()))?;
    }
    let mirror = dir.join(id);
    let record_file = dir.join(format!("{id}.json"));
    if fs::symlink_metadata(&mirror).is_err() {
        // Deleted from outside, its registration left: git would refuse the path. Ours to
        // drop only with our record for it (never `worktree prune`).
        if let Some(stale) = owned_registration(checkout, &mirror, cancel)? {
            let record: Option<Record> = fs::read(&record_file)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            if record.is_none_or(|r| r.mirror != mirror || r.source != checkout) {
                return Err(format!(
                    "a mirror at {} is registered but its record is gone: `git worktree \
                     remove -f -f` it",
                    mirror.display()
                ));
            }
            fs::remove_dir_all(&stale).map_err(|e| format!("{}: {e}", stale.display()))?;
        }
        // The record first: a crash during `worktree add` leaves a mirror `/worktrees clean`
        // can find and remove.
        let mut record = Record {
            source: checkout.to_path_buf(),
            mirror: mirror.clone(),
            folder: None,
        };
        let write = |record: &Record| {
            let bytes = serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?;
            crate::transcript::write_atomic(&record_file, &bytes).map_err(|e| e.to_string())
        };
        write(&record)?;
        let path = mirror.to_string_lossy();
        // Locked with our mark as the reason: git writes it as it registers the worktree, so
        // no crash leaves an unmarked one; a record names only a path, and git could later
        // hold someone else's worktree there. The lock also keeps `worktree prune` off it.
        let reason = mark(&mirror);
        git(
            &[
                "worktree", "add", "--detach", "--lock", "--reason", &reason, &path, &head,
            ],
            None,
        )?;
        fs::set_permissions(&mirror, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        record.folder = identity(&mirror);
        write(&record)?;
    }
    let admin = verify(&mirror, &record_file, checkout, cancel)?;
    // The CLI runs plain git in the mirror: its pointer must name this registration, whatever
    // the last turn wrote there.
    let pointer = mirror.join(".git");
    match fs::symlink_metadata(&pointer) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(&pointer),
        Ok(_) => fs::remove_file(&pointer),
        Err(_) => Ok(()),
    }
    .and_then(|()| fs::write(&pointer, format!("gitdir: {}\n", admin.display())))
    .map_err(|e| format!("{}: {e}", pointer.display()))?;
    // A lock left in the mirror's own git dir by a sync that was stopped hard: only this
    // agent's syncs use it, one at a time.
    let _ = fs::remove_file(admin.join("index.lock"));
    let in_mirror = |args: &[&str], input: Option<&[u8]>| {
        let base = [
            OsStr::new("--git-dir"),
            admin.as_os_str(),
            OsStr::new("--work-tree"),
            mirror.as_os_str(),
            OsStr::new("-C"),
            mirror.as_os_str(),
        ];
        run_git(&base, args, input, &[], cancel)
    };
    // What the last sync copied in as untracked goes first, before the reset (which restores any of them
    // tracked since) (whatever the ignore rules say
    // now), so nothing the writer has since deleted survives; build output the agent made
    // there is not on the list and stays.
    let copied_file = dir.join(format!("{id}.copied"));
    // Missing means a first sync; unreadable must not, or a stale copy would outlive it.
    let listed = match fs::read(&copied_file) {
        Ok(listed) => listed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(format!("{}: {e}", copied_file.display())),
    };
    for name in listed.split(|b| *b == 0) {
        if name.is_empty() {
            continue;
        }
        let name = Path::new(OsStr::from_bytes(name));
        let parent = name.parent().unwrap_or(Path::new(""));
        real_dirs_exist(&mirror, parent)
            .and_then(|real| {
                if real {
                    clear(&mirror.join(name))
                } else {
                    Ok(())
                }
            })
            .map_err(|e| format!("{}: {e}", name.display()))?;
    }
    in_mirror(&["checkout", "-q", "--detach", "--force", &head], None)?;
    in_mirror(&["reset", "-q", "--hard"], None)?;
    // A private copy of the writer's index (git replaces it by rename, so the copy is whole):
    // building the tree may rewrite the index it reads, and a cancel must never leave a lock
    // on the writer's own. The changed files below are listed against the same snapshot.
    let index = git(&["rev-parse", "--git-path", "index"], None)?;
    let index = checkout.join(String::from_utf8_lossy(&index).trim());
    let snapshot = dir.join(format!("{id}.index"));
    // A lock a hard-stopped sync left on the snapshot would fail every `update-index`.
    let _ = fs::remove_file(dir.join(format!("{id}.index.lock")));
    fs::copy(&index, &snapshot).map_err(|e| format!("{}: {e}", index.display()))?;
    let src_in = |args: &[&str], input: Option<&[u8]>| {
        let env = [("GIT_INDEX_FILE", snapshot.as_os_str())];
        run_git(
            &[OsStr::new("-C"), checkout.as_os_str()],
            args,
            input,
            &env,
            cancel,
        )
    };
    let src = |args: &[&str]| src_in(args, None);
    // `assume-unchanged` and `skip-worktree` hide a file's edits from `diff`: cleared in the
    // snapshot, so they are seen. A skip-worktree file absent from the checkout (sparse
    // checkout) keeps its bit: it isn't deleted, just not there.
    let tags = src(&["ls-files", "-v", "-z"])?;
    let mut assumed = vec![];
    let mut skipped = vec![];
    for e in tags.split(|b| *b == 0) {
        let (Some(&tag), Some(name)) = (e.first(), e.get(2..)) else {
            continue;
        };
        if tag.is_ascii_lowercase() {
            assumed.extend(name.iter().chain(&[0]));
        }
        let present = || entry(&checkout.join(OsStr::from_bytes(name))).is_ok_and(|m| m.is_some());
        if tag.eq_ignore_ascii_case(&b'S') && present() {
            skipped.extend(name.iter().chain(&[0]));
        }
    }
    for (flag, names) in [
        ("--no-assume-unchanged", &assumed),
        ("--no-skip-worktree", &skipped),
    ] {
        if !names.is_empty() {
            src_in(&["update-index", flag, "-z", "--stdin"], Some(names))?;
        }
    }
    // The index as a tree (objects only), read into the mirror's index and files whole:
    // renames, file-to-folder swaps and deletions included.
    let tree = match src(&["write-tree"]) {
        Ok(tree) => String::from_utf8_lossy(&tree).trim().to_string(),
        Err(why) => return Err(format!("the writer's index (a merge in progress?): {why}")),
    };
    in_mirror(&["read-tree", "--reset", "-u", &tree], None)?;
    // Then its unstaged work, file by file: changed files copied, deleted ones removed.
    let changed = src(&["diff", "--name-only", "--no-renames", "--no-relative", "-z"])?;
    // `changed`: a tracked path the writer replaced by a folder is a deletion here; its new
    // files come with the untracked ones. Reading through a symlinked folder of the
    // checkout would copy what lies outside it, so such a path counts as deleted.
    let mirror_file = |name: &Path, changed: bool| -> Result<(), String> {
        let from = checkout.join(name);
        let parent = name.parent().unwrap_or(Path::new(""));
        (|| {
            let source = if real_dirs_exist(checkout, parent)? {
                entry(&from)?.filter(|m| !(changed && m.is_dir()))
            } else {
                None
            };
            if source.is_some() {
                copy_entry(checkout, &mirror, name, cancel)
            } else if real_dirs_exist(&mirror, parent)? {
                clear(&mirror.join(name))
            } else {
                Ok(())
            }
        })()
        .map_err(|e| format!("copying {}: {e}", name.display()))
    };
    for name in changed.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        mirror_file(Path::new(OsStr::from_bytes(name)), true)?;
    }
    // Untracked files, and intent-to-add ones (`git add -N`: in the writer's index, not in
    // its tree, so untracked here), copied last. Listed before copying, so a sync cut
    // short still has the next one remove whatever it copied.
    let untracked = src(&["ls-files", "-z", "-o", "--exclude-standard"])?;
    // Only the writer's ignore rules may spare files from `clean`: every untracked
    // `.gitignore` in the mirror goes (ignored ones too: one can ignore itself), then the
    // writer's untracked ones come back and are kept out of `clean`, so their rules hold.
    let stray = in_mirror(
        &["ls-files", "-z", "-o", "--", ":(glob)**/.gitignore"],
        None,
    )?;
    let stray: Vec<&[u8]> = stray.split(|b| *b == 0).filter(|n| !n.is_empty()).collect();
    // Inside a folder the writer ignores (`node_modules/`) they belong to the agent's own
    // tree, which `clean` spares anyway: left alone.
    let spared = ignored_by(checkout, &stray, cancel)?;
    for name in stray.into_iter().filter(|n| !spared.contains(*n)) {
        let name = Path::new(OsStr::from_bytes(name));
        let parent = name.parent().unwrap_or(Path::new(""));
        real_dirs_exist(&mirror, parent)
            .and_then(|real| {
                if real {
                    clear(&mirror.join(name))
                } else {
                    Ok(())
                }
            })
            .map_err(|e| format!("{}: {e}", name.display()))?;
    }
    let mut keep = vec![];
    for name in untracked
        .split(|b| *b == 0)
        .filter(|n| n.ends_with(b".gitignore"))
    {
        let name = Path::new(OsStr::from_bytes(name));
        if name.file_name() == Some(OsStr::new(".gitignore")) {
            mirror_file(name, false)?;
            keep.push("-e".to_string());
            keep.push(format!("/{}", glob_escape(&name.to_string_lossy())));
        }
    }
    // Under the writer's ignore rules (its `.gitignore` edits came above): ignored build
    // output stays, so a build there survives; `-ff` also removes a repository a turn
    // cloned there.
    let mut clean = vec!["clean", "-ffdq"];
    clean.extend(keep.iter().map(String::as_str));
    in_mirror(&clean, None)?;
    let intent = src(&[
        "diff",
        "--name-only",
        "--no-renames",
        "--no-relative",
        "--diff-filter=A",
        "-z",
    ])?;
    let names: Vec<&[u8]> = untracked
        .split(|b| *b == 0)
        .chain(intent.split(|b| *b == 0))
        .filter(|n| !n.is_empty())
        .collect();
    let listed: Vec<u8> = names
        .iter()
        .flat_map(|n| n.iter().chain(&[0]))
        .copied()
        .collect();
    crate::transcript::write_atomic(&copied_file, &listed).map_err(|e| e.to_string())?;
    for name in names {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        mirror_file(Path::new(OsStr::from_bytes(name)), false)?;
    }
    // Through real folders only: an ignored symlink left there could lead the CLI out.
    real_dirs(&mirror, sub).map_err(|e| format!("the launch folder: {e}"))?;
    Ok(cwd(data_root, checkout, id, sub))
}

/// Makes `rel` a chain of real folders under `root`, creating missing ones; refuses a
/// symlink, a file or a `..` on the way.
fn real_dirs(root: &Path, rel: &Path) -> std::io::Result<()> {
    let mut at = root.to_path_buf();
    for part in rel.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Err(std::io::Error::other(format!(
                "{} leaves the mirror",
                rel.display()
            )));
        }
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => {
                return Err(std::io::Error::other(format!(
                    "{} in the mirror is not a folder",
                    at.display()
                )));
            }
            Err(_) => fs::create_dir(&at)?,
        }
    }
    Ok(())
}

/// Before forcing a checkout and a clean in it: the path is a real folder, recorded by us
/// for this source, and the checkout's repository registers a worktree there. Returns that
/// registration (its git dir), which every git call in the mirror is given explicitly: the
/// mirror's own `.git` file is the agent's to rewrite, and could point git at a config of
/// its making (`core.fsmonitor` runs a program).
fn verify(
    mirror: &Path,
    record_file: &Path,
    checkout: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let refuse = |why: &str| {
        Err(format!(
            "{}: {why}; left alone (`/worktrees clean` removes what roundtable made)",
            mirror.display()
        ))
    };
    if !fs::symlink_metadata(mirror).is_ok_and(|m| m.file_type().is_dir()) {
        return refuse("not a folder");
    }
    let record: Option<Record> = fs::read(record_file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    if record
        .as_ref()
        .is_none_or(|r| r.mirror != mirror || r.source != checkout)
    {
        return refuse("not a mirror roundtable recorded for this checkout");
    }
    if record
        .and_then(|r| r.folder)
        .is_none_or(|f| identity(mirror) != Some(f))
    {
        return refuse("not the folder roundtable made there (moved or replaced?)");
    }
    if fs::symlink_metadata(mirror.join(".git")).is_ok_and(|m| !m.is_file()) {
        return refuse("its `.git` is not a worktree's pointer file");
    }
    match owned_registration(checkout, mirror, cancel)? {
        Some(admin) => Ok(admin),
        None => refuse("the checkout's repository has no worktree there"),
    }
}

/// The lock reason that says roundtable made the worktree at `mirror` (git >= 2.35).
fn mark(mirror: &Path) -> String {
    format!("roundtable mirror {}", mirror.display())
}

/// `registration`, but only if roundtable made it: one git holds at our path without the
/// mark is someone else's worktree, never reset or removed here.
fn owned_registration(
    checkout: &Path,
    mirror: &Path,
    cancel: &AtomicBool,
) -> Result<Option<PathBuf>, String> {
    let Some(admin) = registration(checkout, mirror, cancel)? else {
        return Ok(None);
    };
    match fs::read_to_string(admin.join("locked")) {
        Ok(reason) if reason.trim_end() == mark(mirror) => Ok(Some(admin)),
        _ => Err(format!(
            "git has a worktree at {} that roundtable didn't make (`git worktree remove` it \
             if it is yours to drop)",
            mirror.display()
        )),
    }
}

/// The checkout's registration (`<common git dir>/worktrees/<name>`) of a worktree at
/// `mirror`, if there is one.
fn registration(
    checkout: &Path,
    mirror: &Path,
    cancel: &AtomicBool,
) -> Result<Option<PathBuf>, String> {
    // No `--path-format=absolute` (git 2.31): relative to the checkout on older git.
    let common = git(checkout, &["rev-parse", "--git-common-dir"], None, cancel)?;
    let common = checkout.join(String::from_utf8_lossy(&common).trim());
    let want = resolved(mirror);
    Ok(fs::read_dir(common.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .find(|admin| {
            fs::read_to_string(admin.join("gitdir")).is_ok_and(|gitdir| {
                // Relative with `worktree.useRelativePaths`: to the admin folder.
                let gitdir = admin.join(gitdir.trim());
                gitdir.file_name() == Some(OsStr::new(".git"))
                    && gitdir.parent().and_then(resolved) == want
            })
        }))
}

/// `rel`'s folders under `root` all exist as real folders (no link on the way).
/// Missing or not a folder is `false`; any other error (no permission) is an error, never
/// taken for a deletion.
fn real_dirs_exist(root: &Path, rel: &Path) -> std::io::Result<bool> {
    let mut at = root.to_path_buf();
    for part in rel.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Ok(false);
        }
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// `symlink_metadata`, with a missing path as `None` and other errors kept.
fn entry(path: &Path) -> std::io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// A path as a literal gitignore pattern.
fn glob_escape(path: &str) -> String {
    path.chars()
        .flat_map(|c| {
            let escape = matches!(c, '*' | '?' | '[' | ']' | '\\' | '!' | '#' | ' ');
            escape.then_some('\\').into_iter().chain([c])
        })
        .collect()
}

/// Removes whatever is at `path` (a link itself, never what it points to).
fn clear(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(_) => Ok(()),
    }
}

/// `path` with links resolved; for a path that no longer exists, its folder's.
fn resolved(path: &Path) -> Option<PathBuf> {
    path.canonicalize()
        .ok()
        .or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)))
}

/// One untracked file into the mirror; a symlink stays a symlink. Never writes through a
/// symlink the agent may have left (an ignored one survives `clean`): every folder on the
/// way must be a real folder, and whatever is at the target is removed first.
fn copy_entry(
    checkout: &Path,
    mirror: &Path,
    name: &Path,
    cancel: &AtomicBool,
) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let from = checkout.join(name);
    let meta = fs::symlink_metadata(&from)?;
    real_dirs(mirror, name.parent().unwrap_or(Path::new("")))?;
    let to = mirror.join(name);
    clear(&to)?;
    let to = to.as_path();
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(&from)?, to)
    } else if meta.is_file() {
        // Opened through the checkout's real folders, never a link: the writer may swap
        // one in between the checks above and this read.
        let mut source = open_beneath(checkout, name)?;
        let found = source.metadata()?;
        if !found.is_file() {
            return Err(std::io::Error::other("changed while copying"));
        }
        let mut target = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(found.permissions().mode() & 0o777)
            .custom_flags(libc::O_NOFOLLOW)
            .open(to)?;
        // In chunks, so a cancel stops a large file too.
        let mut buf = vec![0; 1 << 20];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("cancelled"));
            }
            match source.read(&mut buf)? {
                0 => return Ok(()),
                n => target.write_all(&buf[..n])?,
            }
        }
    } else {
        // A nested repository shows as a folder: not mirrored, and not silently missing.
        Err(std::io::Error::other(
            "a nested repository: mirrors don't carry those yet",
        ))
    }
}

/// Opens `rel` under `root` one folder at a time, refusing a symlink anywhere on the way
/// (`O_NOFOLLOW`), so a path swapped meanwhile can't lead outside `root`.
fn open_beneath(root: &Path, rel: &Path) -> std::io::Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let open = |dir: i32, name: &OsStr, flags: i32| -> std::io::Result<OwnedFd> {
        let name = std::ffi::CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
        // SAFETY: a valid C string and flags; the fd returned is owned by the caller.
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            // SAFETY: a fresh descriptor nobody else owns.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    };
    let mut dir = open(
        libc::AT_FDCWD,
        root.as_os_str(),
        libc::O_DIRECTORY | libc::O_RDONLY,
    )?;
    let parts: Vec<_> = rel.components().collect();
    let Some((last, parents)) = parts.split_last() else {
        return Err(std::io::Error::other("an empty path"));
    };
    let nofollow = libc::O_NOFOLLOW | libc::O_RDONLY;
    for part in parents.iter().chain([last]) {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Err(std::io::Error::other("a path leaving the checkout"));
        }
    }
    for part in parents {
        dir = open(
            dir.as_raw_fd(),
            part.as_os_str(),
            libc::O_DIRECTORY | nofollow,
        )?;
    }
    // Non-blocking: a FIFO put there must not hang the sync (it then fails as not a file).
    let file = open(
        dir.as_raw_fd(),
        last.as_os_str(),
        nofollow | libc::O_NONBLOCK,
    )?;
    Ok(fs::File::from(file))
}

/// Which of `names` the checkout's ignore rules (the writer's) exclude, a folder rule
/// covering the files in it.
fn ignored_by(
    checkout: &Path,
    names: &[&[u8]],
    cancel: &AtomicBool,
) -> Result<HashSet<Vec<u8>>, String> {
    if names.is_empty() {
        return Ok(HashSet::new());
    }
    let input: Vec<u8> = names
        .iter()
        .flat_map(|n| n.iter().chain(&[0]))
        .copied()
        .collect();
    // Exit 1 is "none ignored", not a failure.
    let out = run_git_with(
        &[OsStr::new("-C"), checkout.as_os_str()],
        &["check-ignore", "-z", "--stdin", "--no-index"],
        Some(&input),
        &[],
        cancel,
        &[0, 1],
    )?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|n| !n.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

/// Git in the owner's checkout.
fn git(
    dir: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    run_git(
        &[OsStr::new("-C"), dir.as_os_str()],
        args,
        input,
        &[],
        cancel,
    )
}

/// Runs git without hooks (a checkout in the mirror must not fire `post-checkout`) or an
/// fsmonitor program, returning stdout or the error text. Killed when `cancel` is set.
fn run_git(
    base: &[&OsStr],
    args: &[&str],
    input: Option<&[u8]>,
    env: &[(&str, &OsStr)],
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    run_git_with(base, args, input, env, cancel, &[0])
}

/// `run_git`, taking the exit codes in `ok` as success.
fn run_git_with(
    base: &[&OsStr],
    args: &[&str],
    input: Option<&[u8]>,
    env: &[(&str, &OsStr)],
    cancel: &AtomicBool,
    ok: &[i32],
) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    // Its own process group, so a cancel stops git's helpers (checkout workers) too.
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(base)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in REPO_ENV {
        command.env_remove(var);
    }
    command.envs(env.iter().copied());
    let mut child = command.spawn().map_err(|e| format!("git: {e}"))?;
    // Pipes on threads of their own, so this one can watch `cancel` without deadlocking.
    let stdin = child.stdin.take();
    let input = input.map(<[u8]>::to_vec);
    std::thread::spawn(move || {
        if let (Some(mut stdin), Some(bytes)) = (stdin, input) {
            let _ = stdin.write_all(&bytes);
        }
    });
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = vec![];
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = read(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = read(child.stderr.take().map(|p| Box::new(p) as _));
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            // SIGTERM first: git removes its lock files on it. SIGKILL if it lingers.
            let group = i32::try_from(child.id()).ok();
            let signal = |sig| {
                if let Some(group) = group {
                    // SAFETY: kill has no memory preconditions; a negative pid is the group.
                    unsafe { libc::kill(-group, sig) };
                }
            };
            signal(libc::SIGTERM);
            for _ in 0..100 {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            signal(libc::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
            return Err("cancelled".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(e) => return Err(format!("git: {e}")),
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if status.code().is_some_and(|code| ok.contains(&code)) {
        Ok(stdout)
    } else {
        Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&stderr).trim()
        ))
    }
}

/// The mirrors recorded for `checkout`, plus those of checkouts that no longer exist (moved
/// or deleted: nothing else would ever find them), by participant id.
pub fn list(data_root: &Path, checkout: &Path) -> Vec<(String, Record)> {
    let mut found: Vec<(String, Record)> = fs::read_dir(data_root.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|dir| fs::read_dir(dir.path()).into_iter().flatten().flatten())
        .filter_map(|entry| {
            let path = entry.path();
            let id = path.file_stem()?.to_str()?.to_string();
            (path.extension()? == "json").then_some(())?;
            let record: Record = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            // Only a record in its own source's folder, naming the mirror beside it, is
            // ours to act on.
            let dir = checkout_dir(data_root, &record.source);
            let ours = path.parent() == Some(dir.as_path()) && record.mirror == dir.join(&id);
            let here = record.source == checkout || !record.source.exists();
            (ours && here).then_some((id, record))
        })
        .collect();
    found.sort_by(|a, b| (&a.0, &a.1.mirror).cmp(&(&b.0, &b.1.mirror)));
    found
}

/// Removes one mirror `list` returned, and its record. A mirror git never registered (a
/// crash during creation) is just deleted.
pub fn remove(id: &str, record: &Record) -> Result<String, String> {
    let mut note = String::from("removed");
    let never = AtomicBool::new(false);
    if fs::symlink_metadata(&record.mirror).is_ok_and(|m| m.file_type().is_symlink()) {
        // Not ours: a link to who knows which worktree. Only the link goes; its
        // registration is then dropped as for a missing folder.
        fs::remove_file(&record.mirror).map_err(|e| e.to_string())?;
    }
    let ours = record
        .folder
        .is_some_and(|f| identity(&record.mirror) == Some(f));
    if record.source.exists()
        && let Some(admin) = owned_registration(&record.source, &record.mirror, &never)?
    {
        if fs::symlink_metadata(&record.mirror).is_ok() {
            if !ours {
                return Err(format!(
                    "{} is not the folder roundtable made there: left alone",
                    record.mirror.display()
                ));
            }
            // Never `worktree prune`: it would touch the owner's own registrations.
            let path = record.mirror.to_string_lossy();
            // Twice `--force`: it is locked (our mark).
            git(
                &record.source,
                &["worktree", "remove", "--force", "--force", &path],
                None,
                &never,
            )?;
        } else {
            // The folder went missing: drop just its registration.
            fs::remove_dir_all(&admin).map_err(|e| e.to_string())?;
        }
    }
    if ours && !record.source.exists() {
        // The checkout is gone (moved or deleted) and with it git's registration: the
        // folder's identity is the proof it is the one we made.
        fs::remove_dir_all(&record.mirror).map_err(|e| e.to_string())?;
        // A checkout that was moved took its registration along, where we can't find it.
        note = format!(
            "removed; if {} was moved, not deleted, its git still lists this mirror (locked): \
             `git worktree remove -f -f {}` there",
            record.source.display(),
            record.mirror.display()
        );
    }
    if fs::symlink_metadata(&record.mirror).is_ok() {
        // Nothing vouches for it (a crash before git made it, or someone reused the path):
        // not deleted on a guess.
        return Err(format!(
            "{} is not a worktree roundtable can show it made: delete it yourself if it \
             is, then run this again",
            record.mirror.display()
        ));
    }
    let dir = record.mirror.parent().unwrap_or(Path::new(""));
    let _ = fs::remove_file(dir.join(format!("{id}.copied")));
    let _ = fs::remove_file(dir.join(format!("{id}.index")));
    let _ = fs::remove_file(dir.join(format!("{id}.index.lock")));
    fs::remove_file(dir.join(format!("{id}.json"))).map_err(|e| e.to_string())?;
    Ok(note)
}

/// Bytes under `path`, symlinks not followed.
pub fn size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| size(&e.path()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    static NO: AtomicBool = AtomicBool::new(false);

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "{}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }

    #[test]
    fn a_mirror_shows_uncommitted_work_and_is_removed_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        let repo = tmp.path().join("repo").canonicalize().unwrap();
        let data = tmp.path().join("data");
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        run(&repo, &["init", "-q"]);
        fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        fs::write(repo.join("gone.txt"), "x\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);

        assert!(
            sync(&data, &repo, "codex", Path::new(""), &NO).is_ok(),
            "first sync creates the mirror"
        );
        // The writer's uncommitted work: an edit, a deletion, a new file with a space, a
        // symlink, and an ignored build folder that must not be copied.
        fs::write(repo.join("a.txt"), "two\n").unwrap();
        fs::remove_file(repo.join("gone.txt")).unwrap();
        fs::write(repo.join("new file.txt"), "new\n").unwrap();
        std::os::unix::fs::symlink("a.txt", repo.join("link")).unwrap();
        fs::create_dir(repo.join("target")).unwrap();
        fs::write(repo.join("target/out"), "bin").unwrap();
        // A force-added ignored file comes over staged, and a second sync doesn't trip on it.
        fs::write(repo.join("target/forced"), "f").unwrap();
        run(&repo, &["add", "-f", "target/forced"]);
        let mirror = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        assert!(mirror.join("target/forced").exists());
        run(&repo, &["rm", "-q", "--cached", "target/forced"]);
        let mirror = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        assert!(
            !mirror.join("target/forced").exists(),
            "unstaged: gone again"
        );
        // An untracked file the last sync copied is now staged: no conflict.
        fs::write(repo.join("staged.txt"), "s").unwrap();
        sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        run(&repo, &["add", "staged.txt"]);
        // The writer ignores `build/` without committing it: the mirror's build survives.
        fs::write(repo.join(".gitignore"), "target/\nbuild/\n").unwrap();
        let mirror = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        fs::create_dir(mirror.join("build")).unwrap();
        fs::write(mirror.join("build/out"), "o").unwrap();
        let mirror = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        assert!(
            mirror.join("build/out").exists(),
            "the writer's ignore rules apply"
        );
        // Intent-to-add, a staged rename onto a path copied last time, a file replaced by
        // a folder: all come over as in the checkout.
        fs::write(repo.join("intent.txt"), "i").unwrap();
        run(&repo, &["add", "-N", "intent.txt"]);
        fs::write(repo.join("renamed.txt"), "r").unwrap();
        sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        fs::remove_file(repo.join("renamed.txt")).unwrap();
        run(&repo, &["mv", "staged.txt", "renamed.txt"]);
        run(&repo, &["rm", "-q", "--cached", "a.txt"]);
        fs::remove_file(repo.join("a.txt")).unwrap();
        fs::create_dir(repo.join("a.txt")).unwrap();
        fs::write(repo.join("a.txt/inner"), "two\n").unwrap();
        run(&repo, &["add", "a.txt/inner"]);
        // A copied file the writer deletes and then ignores is gone from the mirror too.
        fs::write(repo.join("note.txt"), "n").unwrap();
        sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        fs::remove_file(repo.join("note.txt")).unwrap();
        fs::write(repo.join(".gitignore"), "target/\nbuild/\nnote.txt\n").unwrap();
        let mirror = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        assert_eq!(fs::read_to_string(mirror.join("intent.txt")).unwrap(), "i");
        assert_eq!(fs::read_to_string(mirror.join("renamed.txt")).unwrap(), "s");
        assert!(!mirror.join("staged.txt").exists());
        assert_eq!(
            fs::read_to_string(mirror.join("a.txt/inner")).unwrap(),
            "two\n"
        );
        assert!(!mirror.join("note.txt").exists(), "a stale copy is removed");
        run(&repo, &["rm", "-rq", "--cached", "a.txt"]);
        fs::remove_dir_all(repo.join("a.txt")).unwrap();
        fs::write(repo.join("a.txt"), "two\n").unwrap();
        run(&repo, &["reset", "-q", "--", "a.txt"]);
        run(&repo, &["reset", "-q", "renamed.txt", "staged.txt"]);
        fs::rename(repo.join("renamed.txt"), repo.join("staged.txt")).unwrap();
        run(&repo, &["add", "staged.txt"]);
        // Unstaged: a tracked file replaced by a folder, and a tracked folder replaced by a
        // link to outside the checkout (whose same-named file must not be read).
        fs::create_dir(repo.join("d")).unwrap();
        fs::write(repo.join("d/f"), "tracked").unwrap();
        run(&repo, &["add", "d/f"]);
        run(&repo, &["commit", "-qm", "d"]);
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("f"), "secret").unwrap();
        fs::remove_dir_all(repo.join("d")).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("d")).unwrap();
        fs::remove_file(repo.join("gone.txt")).ok();
        let m = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        assert!(
            fs::read_link(m.join("d")).is_ok() || !m.join("d/f").exists(),
            "never the outside file's contents"
        );
        fs::remove_file(repo.join("d")).unwrap();
        run(&repo, &["rm", "-q", "--cached", "d/f"]);
        run(&repo, &["commit", "-qm", "no d"]);
        // An edit hidden by `assume-unchanged`, and an untracked `.gitignore` whose rule
        // keeps the agent's build output across syncs.
        run(&repo, &["update-index", "--assume-unchanged", "a.txt"]);
        fs::create_dir(repo.join("sub")).unwrap();
        fs::write(repo.join("sub/.gitignore"), "cache/\n").unwrap();
        let m = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        assert_eq!(fs::read_to_string(m.join("a.txt")).unwrap(), "two\n");
        fs::create_dir(m.join("sub/cache")).unwrap();
        fs::write(m.join("sub/cache/x"), "x").unwrap();
        sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        assert!(
            m.join("sub/cache/x").exists(),
            "an untracked ignore file is in force"
        );
        // One the agent wrote, ignoring itself and its output: both go.
        fs::write(m.join("generated.txt"), "g").unwrap();
        fs::create_dir(m.join("own")).unwrap();
        fs::write(m.join("own/.gitignore"), "*\n").unwrap();
        fs::write(m.join("own/kept"), "k").unwrap();
        sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        assert!(
            !m.join("own/kept").exists(),
            "a mirror-only ignore file spares nothing"
        );
        assert!(!m.join("generated.txt").exists());
        run(&repo, &["update-index", "--no-assume-unchanged", "a.txt"]);
        fs::remove_dir_all(repo.join("sub")).unwrap();
        // Something the agent left in the mirror is gone after the next sync.
        fs::write(mirror.join("stray.txt"), "x").unwrap();
        let mirror = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();

        assert_eq!(fs::read_to_string(mirror.join("a.txt")).unwrap(), "two\n");
        assert!(!mirror.join("gone.txt").exists());
        assert_eq!(
            fs::read_to_string(mirror.join("new file.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            fs::read_link(mirror.join("link")).unwrap(),
            Path::new("a.txt")
        );
        assert!(
            !mirror.join("target/out").exists(),
            "ignored files stay behind"
        );
        let sub = sync(&data, &repo, "codex", Path::new("empty/dir"), &NO).unwrap();
        assert!(
            sub.is_dir() && sub.starts_with(&mirror),
            "the launch folder exists"
        );
        assert!(!mirror.join("stray.txt").exists());
        let unstaged = Command::new("git")
            .arg("-C")
            .arg(&mirror)
            .args(["diff", "--name-only"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&unstaged.stdout).contains("a.txt"),
            "an unstaged edit stays unstaged in the mirror"
        );
        assert_eq!(
            fs::read_to_string(repo.join("a.txt")).unwrap(),
            "two\n",
            "the checkout is untouched"
        );

        let found = list(&data, &repo);
        assert_eq!(found.len(), 1);
        let (id, record) = &found[0];
        assert!(size(&record.mirror) > 0);
        remove(id, record).unwrap();
        assert!(!mirror.exists());
        assert!(list(&data, &repo).is_empty());
        let worktrees = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "list"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&worktrees.stdout).lines().count(),
            1,
            "git forgot the mirror too"
        );
    }

    #[test]
    fn a_swapped_in_symlink_or_overlapping_data_folder_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        let repo = tmp.path().join("repo").canonicalize().unwrap();
        let data = tmp.path().join("data");
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        run(&repo, &["init", "-q"]);
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        fs::write(repo.join(".gitignore"), "out\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);
        let mirror = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        // The agent rewrites the mirror's `.git` pointer: git is never steered by it.
        fs::write(mirror.join(".git"), "gitdir: /nonexistent\n").unwrap();
        sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        run(&mirror, &["status", "-s"]);
        // A symlink where the launch folder goes is refused, not followed.
        // (Ignored, so `clean` leaves it, as it would a build folder.)
        std::os::unix::fs::symlink(&repo, mirror.join("out")).unwrap();
        let err = sync(&data, &repo, "codex", Path::new("out/x"), &NO).unwrap_err();
        assert!(err.contains("not a folder"), "{err}");
        assert!(!repo.join("x").exists(), "nothing created through the link");
        fs::remove_file(mirror.join("out")).unwrap();
        fs::write(repo.join("a.txt"), "writer's work\n").unwrap();
        fs::remove_dir_all(&mirror).unwrap();
        std::os::unix::fs::symlink(&repo, &mirror).unwrap();
        assert!(sync(&data, &repo, "codex", Path::new(""), &NO).is_err());
        assert_eq!(
            fs::read_to_string(repo.join("a.txt")).unwrap(),
            "writer's work\n",
            "the checkout was not reset"
        );
        let inside = repo.join("data");
        fs::DirBuilder::new().mode(0o700).create(&inside).unwrap();
        let err = sync(&inside, &repo, "codex", Path::new(""), &NO).unwrap_err();
        assert!(err.contains("overlap"), "{err}");
    }

    #[test]
    fn a_cancelled_sync_stops_and_a_half_made_mirror_is_cleaned() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        let repo = tmp.path().join("repo").canonicalize().unwrap();
        let data = tmp.path().join("data");
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        run(&repo, &["init", "-q"]);
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let stop = AtomicBool::new(true);
        assert_eq!(
            sync(&data, &repo, "codex", Path::new(""), &stop).unwrap_err(),
            "cancelled"
        );
        // A crash between the record and `worktree add`: a folder git never registered.
        let dir = checkout_dir(&data, &repo);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir.join("codex"))
            .unwrap();
        let record = Record {
            source: repo.clone(),
            mirror: dir.join("codex"),
            folder: None,
        };
        fs::write(dir.join("codex.json"), serde_json::to_vec(&record).unwrap()).unwrap();
        let err = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap_err();
        assert!(err.contains("/worktrees clean"), "{err}");
        let found = list(&data, &repo);
        assert_eq!(found, vec![("codex".to_string(), record.clone())]);
        // Nothing vouches for the folder: left to the owner, then its record goes.
        assert!(remove("codex", &record).is_err());
        assert!(dir.join("codex").exists());
        fs::remove_dir(dir.join("codex")).unwrap();
        remove("codex", &record).unwrap();
        assert!(list(&data, &repo).is_empty());
        let mirror = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        // Unstaged: a tracked file replaced by a folder with a new file in it.
        fs::write(repo.join("x"), "x").unwrap();
        run(&repo, &["add", "x"]);
        run(&repo, &["commit", "-qm", "x"]);
        fs::remove_file(repo.join("x")).unwrap();
        fs::create_dir(repo.join("x")).unwrap();
        fs::write(repo.join("x/y"), "y").unwrap();
        sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        assert_eq!(fs::read_to_string(mirror.join("x/y")).unwrap(), "y");
        // Deleted from outside, its registration left: the next sync makes it again.
        fs::remove_dir_all(&mirror).unwrap();
        sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        // Swapped for a link to the owner's own worktree: cleanup drops only the link.
        let owners = tmp.path().join("owners");
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                owners.to_str().unwrap(),
            ],
        );
        fs::write(owners.join("work.txt"), "mine").unwrap();
        fs::remove_dir_all(&mirror).unwrap();
        std::os::unix::fs::symlink(&owners, &mirror).unwrap();
        remove("codex", &record).unwrap();
        assert_eq!(fs::read_to_string(owners.join("work.txt")).unwrap(), "mine");
        assert!(
            owners.join(".git").exists(),
            "the owner's worktree is still registered"
        );
        // Its own registration went with it: a new mirror can be made at the path.
        sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
    }

    #[test]
    fn a_worktree_roundtable_did_not_make_is_never_touched() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        let repo = tmp.path().join("repo").canonicalize().unwrap();
        let data = tmp.path().join("data");
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        run(&repo, &["init", "-q"]);
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let mirror = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        // The owner removes it with git and puts a worktree of their own at the path.
        let path = mirror.to_str().unwrap();
        run(&repo, &["worktree", "remove", "-f", "-f", path]);
        run(&repo, &["worktree", "add", "-q", "--detach", path]);
        fs::write(mirror.join("work.txt"), "mine").unwrap();
        let err = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap_err();
        assert!(err.contains("not the folder"), "{err}");
        let (id, record) = list(&data, &repo).remove(0);
        assert!(remove(&id, &record).is_err());
        assert_eq!(fs::read_to_string(mirror.join("work.txt")).unwrap(), "mine");
    }

    #[test]
    fn open_beneath_never_follows_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("real")).unwrap();
        fs::write(root.join("real/f"), "in").unwrap();
        fs::create_dir(root.join("outside")).unwrap();
        fs::write(root.join("outside/f"), "out").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("dir")).unwrap();
        std::os::unix::fs::symlink(root.join("outside/f"), root.join("real/g")).unwrap();
        let mut text = String::new();
        open_beneath(root, Path::new("real/f"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "in");
        assert!(
            open_beneath(root, Path::new("dir/f")).is_err(),
            "a linked folder"
        );
        assert!(
            open_beneath(root, Path::new("real/g")).is_err(),
            "a linked file"
        );
    }

    #[test]
    fn a_moved_mirror_is_never_reset_and_an_orphan_is_cleaned() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        let repo = tmp.path().join("repo").canonicalize().unwrap();
        let data = tmp.path().join("data");
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        run(&repo, &["init", "-q"]);
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let mirror = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap();
        // Moved aside by hand, another folder in its place: untouched.
        let aside = tmp.path().join("aside");
        fs::rename(&mirror, &aside).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&mirror).unwrap();
        fs::write(mirror.join("keep.txt"), "k").unwrap();
        let err = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap_err();
        assert!(err.contains("not the folder"), "{err}");
        let (id, record) = list(&data, &repo).remove(0);
        assert!(remove(&id, &record).is_err());
        assert!(mirror.join("keep.txt").exists());
        fs::remove_dir_all(&mirror).unwrap();
        fs::rename(&aside, &mirror).unwrap();
        // The checkout is deleted: its mirror is still listed, and removed.
        fs::remove_dir_all(&repo).unwrap();
        let (id, record) = list(&data, Path::new("/elsewhere")).remove(0);
        remove(&id, &record).unwrap();
        assert!(!mirror.exists());
    }

    #[test]
    fn a_checkout_without_a_commit_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let data = tmp.path().join("data");
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        run(&repo, &["init", "-q"]);
        let err = sync(&data, &repo, "codex", Path::new(""), &NO).unwrap_err();
        assert!(err.contains("no commit yet"), "{err}");
    }
}
