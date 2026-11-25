//! A git worktree per read-only CLI agent (ADR 0016): `<data_root>/worktrees/<checkout-id>/<id>/`,
//! brought to the writer's current state (HEAD plus uncommitted and untracked files) before
//! each of that agent's turns, so what escapes its read-only flags lands in the mirror.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// `<id>.json` next to a mirror: cleanup only ever acts on mirrors listed in one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub source: PathBuf,
    pub mirror: PathBuf,
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

/// Creates the mirror of `id` if needed and syncs it with `checkout`; returns the folder
/// `sub` (the launch folder, relative to the root) inside it, created if git has nothing
/// there. Blocking (runs git); any failure is the turn's failure, never a fallback.
pub fn sync(data_root: &Path, checkout: &Path, id: &str, sub: &Path) -> Result<PathBuf, String> {
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
    let dir = checkout_dir(data_root, checkout);
    for folder in [&data_root.join("worktrees"), &dir] {
        crate::transcript::owned_private_dir(folder)
            .map_err(|e| format!("{}: {e}", folder.display()))?;
    }
    let mirror = dir.join(id);
    let Some(head) = git(
        checkout,
        &["rev-parse", "--verify", "-q", "HEAD^{commit}"],
        None,
    )
    .ok() else {
        // shortcut: ADR 0016's `worktree add --orphan` would add a branch to the owner's
        // repo; refused until the first commit instead.
        return Err("the checkout has no commit yet: commit once to give agents a mirror".into());
    };
    let head = String::from_utf8_lossy(&head).trim().to_string();
    let record_file = dir.join(format!("{id}.json"));
    if fs::symlink_metadata(&mirror).is_err() {
        let path = mirror.to_string_lossy();
        git(
            checkout,
            &["worktree", "add", "--detach", &path, &head],
            None,
        )?;
        fs::set_permissions(&mirror, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let record = Record {
            source: checkout.to_path_buf(),
            mirror: mirror.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
        crate::transcript::write_atomic(&record_file, &bytes).map_err(|e| e.to_string())?;
    }
    let admin = verify(&mirror, &record_file, checkout)?;
    let in_mirror = |args: &[&str], input| {
        let base = [
            OsStr::new("--git-dir"),
            admin.as_os_str(),
            OsStr::new("--work-tree"),
            mirror.as_os_str(),
            OsStr::new("-C"),
            mirror.as_os_str(),
        ];
        run_git(&base, args, input)
    };
    in_mirror(&["checkout", "-q", "--detach", "--force", &head], None)?;
    // `reset --hard` also drops what the last sync staged (a force-added ignored file),
    // which `checkout` keeps; ignored build output stays, so a build there survives.
    in_mirror(&["reset", "-q", "--hard"], None)?;
    in_mirror(&["clean", "-fdq"], None)?;
    let diff = git(
        checkout,
        &[
            "diff",
            "HEAD",
            "--binary",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ],
        None,
    )?;
    if !diff.is_empty() {
        in_mirror(&["apply", "--binary", "--index"], Some(&diff))?;
    }
    let untracked = git(
        checkout,
        &["ls-files", "-z", "-o", "--exclude-standard"],
        None,
    )?;
    for name in untracked.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = Path::new(OsStr::from_bytes(name));
        copy_entry(&checkout.join(name), &mirror, name)
            .map_err(|e| format!("copying {}: {e}", name.display()))?;
    }
    let cwd = cwd(data_root, checkout, id, sub);
    fs::create_dir_all(&cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;
    Ok(cwd)
}

/// Before forcing a checkout and a clean in it: the path is a real folder, recorded by us
/// for this source, and the checkout's repository registers a worktree there. Returns that
/// registration (its git dir), which every git call in the mirror is given explicitly: the
/// mirror's own `.git` file is the agent's to rewrite, and could point git at a config of
/// its making (`core.fsmonitor` runs a program).
fn verify(mirror: &Path, record_file: &Path, checkout: &Path) -> Result<PathBuf, String> {
    let refuse = |why: &str| Err(format!("{}: {why}; remove it", mirror.display()));
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
    let common = git(
        checkout,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        None,
    )?;
    let common = PathBuf::from(String::from_utf8_lossy(&common).trim());
    let want = mirror.canonicalize().ok();
    let admin = fs::read_dir(common.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .find(|admin| {
            fs::read_to_string(admin.join("gitdir")).is_ok_and(|gitdir| {
                let gitdir = Path::new(gitdir.trim());
                gitdir.file_name() == Some(OsStr::new(".git"))
                    && gitdir.parent().and_then(|p| p.canonicalize().ok()) == want
            })
        });
    match admin {
        Some(admin) => Ok(admin),
        None => refuse("the checkout's repository has no worktree there"),
    }
}

/// One untracked file into the mirror; a symlink stays a symlink. Never writes through a
/// symlink the agent may have left (an ignored one survives `clean`): every folder on the
/// way must be a real folder, and whatever is at the target is removed first.
fn copy_entry(from: &Path, mirror: &Path, name: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    let mut at = mirror.to_path_buf();
    let parents: Vec<_> = name
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .collect();
    for part in parents {
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
    let to = mirror.join(name);
    match fs::symlink_metadata(&to) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(&to)?,
        Ok(_) => fs::remove_file(&to)?,
        Err(_) => {}
    }
    let to = to.as_path();
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if meta.is_file() {
        fs::copy(from, to).map(drop)
    } else {
        // A nested repository shows as a folder: not mirrored.
        Ok(())
    }
}

/// Git in the owner's checkout.
fn git(dir: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    run_git(&[OsStr::new("-C"), dir.as_os_str()], args, input)
}

/// Runs git without hooks (a checkout in the mirror must not fire `post-checkout`) or an
/// fsmonitor program, returning stdout or the error text.
fn run_git(base: &[&OsStr], args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut child = Command::new("git")
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
        .stderr(Stdio::piped())
        // Repository overrides (an index, an object store) would point the mirror's
        // commands at another repository's state: `git rev-parse --local-env-vars`.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_IMPLICIT_WORK_TREE")
        .env_remove("GIT_GRAFT_FILE")
        .env_remove("GIT_SHALLOW_FILE")
        .env_remove("GIT_NO_REPLACE_OBJECTS")
        .env_remove("GIT_REPLACE_REF_BASE")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(bytes).map_err(|e| format!("git: {e}"))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The mirrors recorded for `checkout`, by participant id.
pub fn list(data_root: &Path, checkout: &Path) -> Vec<(String, Record)> {
    let dir = checkout_dir(data_root, checkout);
    let mut found: Vec<(String, Record)> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let id = path.file_stem()?.to_str()?.to_string();
            (path.extension()? == "json").then_some(())?;
            let record: Record = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            // A record naming another folder or another source is not ours to delete.
            (record.mirror == dir.join(&id) && record.source == checkout).then_some((id, record))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Removes one recorded mirror and its record.
pub fn remove(data_root: &Path, checkout: &Path, id: &str, record: &Record) -> Result<(), String> {
    let path = record.mirror.to_string_lossy();
    if checkout.exists() && record.source == checkout {
        // Never `worktree prune`: it would touch the owner's own registrations.
        git(checkout, &["worktree", "remove", "--force", &path], None)?;
    }
    if record.mirror.exists() {
        fs::remove_dir_all(&record.mirror).map_err(|e| e.to_string())?;
    }
    fs::remove_file(checkout_dir(data_root, checkout).join(format!("{id}.json")))
        .map_err(|e| e.to_string())
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
            sync(&data, &repo, "codex", Path::new("")).is_ok(),
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
        let mirror = sync(&data, &repo, "codex", Path::new("")).unwrap();
        assert!(mirror.join("target/forced").exists());
        run(&repo, &["rm", "-q", "--cached", "target/forced"]);
        let mirror = sync(&data, &repo, "codex", Path::new("")).unwrap();
        assert!(
            !mirror.join("target/forced").exists(),
            "unstaged: gone again"
        );
        // Something the agent left in the mirror is gone after the next sync.
        fs::write(mirror.join("stray.txt"), "x").unwrap();
        let mirror = sync(&data, &repo, "codex", Path::new("")).unwrap();

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
        let sub = sync(&data, &repo, "codex", Path::new("empty/dir")).unwrap();
        assert!(
            sub.is_dir() && sub.starts_with(&mirror),
            "the launch folder exists"
        );
        assert!(!mirror.join("stray.txt").exists());
        assert_eq!(
            fs::read_to_string(repo.join("a.txt")).unwrap(),
            "two\n",
            "the checkout is untouched"
        );

        let found = list(&data, &repo);
        assert_eq!(found.len(), 1);
        let (id, record) = &found[0];
        assert!(size(&record.mirror) > 0);
        remove(&data, &repo, id, record).unwrap();
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
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);
        let mirror = sync(&data, &repo, "codex", Path::new("")).unwrap();
        // The agent rewrites the mirror's `.git` pointer: git is never steered by it.
        fs::write(mirror.join(".git"), "gitdir: /nonexistent\n").unwrap();
        sync(&data, &repo, "codex", Path::new("")).unwrap();
        fs::write(repo.join("a.txt"), "writer's work\n").unwrap();
        fs::remove_dir_all(&mirror).unwrap();
        std::os::unix::fs::symlink(&repo, &mirror).unwrap();
        assert!(sync(&data, &repo, "codex", Path::new("")).is_err());
        assert_eq!(
            fs::read_to_string(repo.join("a.txt")).unwrap(),
            "writer's work\n",
            "the checkout was not reset"
        );
        let inside = repo.join("data");
        fs::DirBuilder::new().mode(0o700).create(&inside).unwrap();
        let err = sync(&inside, &repo, "codex", Path::new("")).unwrap_err();
        assert!(err.contains("overlap"), "{err}");
    }

    #[test]
    fn a_checkout_without_a_commit_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let data = tmp.path().join("data");
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        run(&repo, &["init", "-q"]);
        let err = sync(&data, &repo, "codex", Path::new("")).unwrap_err();
        assert!(err.contains("no commit yet"), "{err}");
    }
}
