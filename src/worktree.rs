//! A git worktree per read-only CLI agent (ADR 0016): `<data_root>/worktrees/<checkout-id>/<id>/`,
//! brought to the writer's current state (HEAD plus uncommitted and untracked files) before
//! each of that agent's turns, so what escapes its read-only flags lands in the mirror.

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

/// Creates the mirror of `id` if needed and syncs it with `checkout`; returns its path.
/// Blocking (runs git); any failure is the turn's failure, never a fallback to the checkout.
pub fn sync(data_root: &Path, checkout: &Path, id: &str) -> Result<PathBuf, String> {
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
    if !mirror.join(".git").exists() {
        if mirror.exists() {
            return Err(format!(
                "{} exists but is not a worktree: remove it",
                mirror.display()
            ));
        }
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
        crate::transcript::write_atomic(&dir.join(format!("{id}.json")), &bytes)
            .map_err(|e| e.to_string())?;
    }
    git(&mirror, &["checkout", "--detach", "--force", &head], None)?;
    // Ignored files (build output) are left, so a build there survives between turns.
    git(&mirror, &["clean", "-fd"], None)?;
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
        git(&mirror, &["apply", "--binary"], Some(&diff))?;
    }
    let untracked = git(
        checkout,
        &["ls-files", "-z", "-o", "--exclude-standard"],
        None,
    )?;
    for name in untracked.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = Path::new(std::ffi::OsStr::from_bytes(name));
        copy_entry(&checkout.join(name), &mirror.join(name))
            .map_err(|e| format!("copying {}: {e}", name.display()))?;
    }
    Ok(mirror)
}

/// One untracked file into the mirror; a symlink stays a symlink.
fn copy_entry(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if meta.is_file() {
        fs::copy(from, to).map(drop)
    } else {
        // A nested repository shows as a folder: not mirrored.
        Ok(())
    }
}

/// Runs git without the owner's hooks (a checkout in the mirror must not fire
/// `post-checkout`), returning stdout or the error text.
fn git(dir: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut child = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "-C"])
        .arg(dir)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
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
            // A record pointing outside this folder is not ours to delete.
            (record.mirror == dir.join(&id)).then_some((id, record))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Removes one recorded mirror and its record.
pub fn remove(data_root: &Path, checkout: &Path, id: &str, record: &Record) -> Result<(), String> {
    let path = record.mirror.to_string_lossy();
    if record.source.exists() {
        // Never `worktree prune`: it would touch the owner's own registrations.
        git(
            &record.source,
            &["worktree", "remove", "--force", &path],
            None,
        )?;
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
            sync(&data, &repo, "codex").is_ok(),
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
        let mirror = sync(&data, &repo, "codex").unwrap();
        // Something the agent left in the mirror is gone after the next sync.
        fs::write(mirror.join("stray.txt"), "x").unwrap();
        let mirror = sync(&data, &repo, "codex").unwrap();

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
        assert!(!mirror.join("target").exists(), "ignored files stay behind");
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
    fn a_checkout_without_a_commit_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let data = tmp.path().join("data");
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        run(&repo, &["init", "-q"]);
        let err = sync(&data, &repo, "codex").unwrap_err();
        assert!(err.contains("no commit yet"), "{err}");
    }
}
