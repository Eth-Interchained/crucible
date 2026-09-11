//! Throwaway git worktrees, one per concurrent worker.
//!
//! WHY THIS IS NOT OPTIONAL: a prototype of this tool mutated the target tree in
//! place, hit an exception before its restore line, and left the repo dirty with
//! a deliberate bug in it. On a repo you also develop in, that is a defect you
//! ship. Worktrees make the source tree structurally unreachable — the forge
//! never opens it for writing at all.
//!
//! `git worktree` rather than `cp -r` because it shares the object store: a
//! worktree of a large repo costs one checkout, not one copy, and `git checkout
//! -- <file>` inside it is an exact, fast restore of a mutated file.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Worktree {
    pub root: PathBuf,
    repo: PathBuf,
    name: String,
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn head_commit(repo: &Path) -> Result<String, String> {
    git(repo, &["rev-parse", "HEAD"])
}

impl Worktree {
    /// Cut a detached worktree at `commit`. Detached on purpose: a worktree on a
    /// branch would move that branch, and the forge must leave no trace in the
    /// target's refs.
    pub fn create(repo: &Path, base: &Path, name: &str, commit: &str) -> Result<Worktree, String> {
        let root = base.join(name);
        if root.exists() {
            // A leftover from a killed run. Reclaim it rather than failing —
            // but say so, because a forge that silently inherits a stale tree
            // is a forge that can verify against the wrong code.
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force", &root.display().to_string()])
                .current_dir(repo)
                .output();
            let _ = std::fs::remove_dir_all(&root);
        }
        std::fs::create_dir_all(base).map_err(|e| format!("mkdir {}: {e}", base.display()))?;
        // ABSOLUTE, always. `git worktree add` runs with cwd = the repo, so a
        // relative path is resolved against the TARGET's tree, not ours: a
        // first run created `nedb/work/baseline`, reported success, and then
        // every grader failed with ENOENT on a cwd that never existed here.
        // git even left the stray tree inside the repo we promised never to
        // write to. Canonicalising at the boundary makes that unrepresentable.
        let root = root
            .canonicalize()
            .or_else(|_| std::fs::create_dir_all(&root).and_then(|_| root.canonicalize()))
            .map_err(|e| format!("canonicalize {}: {e}", root.display()))?;
        // git refuses to populate a directory that already exists and is not
        // empty; we only created it to resolve the absolute path.
        let _ = std::fs::remove_dir(&root);
        git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                "--quiet",
                &root.display().to_string(),
                commit,
            ],
        )?;
        Ok(Worktree {
            root,
            repo: repo.to_path_buf(),
            name: name.to_string(),
        })
    }

    /// Write mutated bytes over one file.
    pub fn splice(&self, rel: &Path, bytes: &[u8]) -> Result<(), String> {
        let p = self.root.join(rel);
        std::fs::write(&p, bytes).map_err(|e| format!("write {}: {e}", p.display()))
    }

    /// Restore one file to the worktree's commit. Exact, and cheap enough to do
    /// after every trial rather than trusting the next splice to overwrite it.
    pub fn restore(&self, rel: &Path) -> Result<(), String> {
        git(&self.root, &["checkout", "--", &rel.display().to_string()]).map(|_| ())
    }

    /// Move this worktree to another commit. Detached, forced: the tree is ours
    /// and a local modification left by a previous step must never block a
    /// replay — a checkout that silently refused would grade the WRONG commit
    /// and report it as a verdict about this one.
    pub fn checkout(&self, commit: &str) -> Result<(), String> {
        git(
            &self.root,
            &["checkout", "--detach", "--force", "--quiet", commit],
        )
        .map(|_| ())?;
        // `checkout -f` does not remove untracked files, and a stray .pyc or a
        // leftover data directory from a previous suite run can change what the
        // next suite sees. Clean is the only way the replay is comparable.
        git(&self.root, &["clean", "-qfdx"]).map(|_| ())
    }

    /// Is the worktree exactly its commit? Called before admitting an example,
    /// because a trial verified against an accidentally-dirty tree is a
    /// mislabelled row, and a mislabelled row is worse than a missing one.
    pub fn is_pristine(&self) -> Result<bool, String> {
        Ok(git(&self.root, &["status", "--porcelain"])?.is_empty())
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // Never leak a worktree past the run that made it. Failure here is
        // reported rather than swallowed: a leaked worktree is a confusing
        // `git worktree list` next time, and silence would make it a mystery.
        let out = Command::new("git")
            .args([
                "worktree",
                "remove",
                "--force",
                &self.root.display().to_string(),
            ])
            .current_dir(&self.repo)
            .output();
        let failed = match out {
            Ok(o) => !o.status.success(),
            Err(_) => true,
        };
        if failed {
            let _ = std::fs::remove_dir_all(&self.root);
            eprintln!(
                "crucible: worktree {} could not be removed by git; directory deleted directly. \
                 Run `git worktree prune` in {} if `git worktree list` still shows it.",
                self.name,
                self.repo.display()
            );
        }
    }
}
