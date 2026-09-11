//! An exclusive claim on a forge output directory.
//!
//! This exists because of a real incident, not a hypothetical. Two `forge`
//! runs were pointed at the same `--out` dir. Both wrote `corpus.jsonl`
//! happily, so the file changed row count and content *underneath a running
//! eval* — which scored 84.2%, printed a warning blaming the harness, and sent
//! the investigation in the wrong direction for twenty minutes. The eval was
//! fine. The corpus had simply stopped being one thing.
//!
//! The corpus is the single artifact this project exists to make trustworthy.
//! A corpus that mutates while it is being read is worse than no corpus, and
//! worse than that because nothing said so. Two overlapping writers must be
//! refused, loudly, before a byte moves.
//!
//! Mechanism: `create_new` on a lock file, which is atomic — the OS resolves
//! the race between two processes reaching this line together, so exactly one
//! wins. The file records the winner's PID and start time so a stale lock can
//! be understood rather than merely cursed at. Removed on drop, including on
//! the error paths, because a lock that outlives its run turns one crash into
//! a permanently unusable directory.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Held for the lifetime of a forge run. Releases on drop.
#[derive(Debug)]
pub struct OutLock {
    path: PathBuf,
}

impl OutLock {
    /// Claim `out_dir` for this process.
    ///
    /// Fails when another run holds it. `force` breaks a lock deliberately —
    /// the escape hatch for a stale lock left by a killed run, which is a real
    /// situation and should not require the operator to know the file's name.
    pub fn claim(out_dir: &Path, force: bool) -> Result<OutLock, String> {
        std::fs::create_dir_all(out_dir).map_err(|e| format!("mkdir out dir: {e}"))?;
        let path = out_dir.join(".crucible-forge.lock");

        if force {
            // Deliberate override. Report it rather than doing it silently: an
            // operator who forces a lock should see that a lock was there.
            if path.exists() {
                eprintln!(
                    "crucible: --force breaking the existing lock on {} ({})",
                    out_dir.display(),
                    std::fs::read_to_string(&path)
                        .unwrap_or_default()
                        .trim()
                        .replace('\n', " ")
                );
            }
            let _ = std::fs::remove_file(&path);
        }

        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut fh) => {
                // Best-effort provenance. A failure to write the body does not
                // invalidate the lock — the file's EXISTENCE is the lock — so
                // it must not fail the run, but it is not swallowed either.
                let body = format!(
                    "pid {}\nstarted {}\n",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                );
                if let Err(e) = fh.write_all(body.as_bytes()) {
                    eprintln!(
                        "crucible: lock on {} is held but its provenance could not be \
                         written ({e}) — a later --force will report less detail",
                        out_dir.display()
                    );
                }
                Ok(OutLock { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let who = std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .trim()
                    .replace('\n', ", ");
                let who = if who.is_empty() {
                    "no provenance recorded".to_string()
                } else {
                    who
                };
                Err(format!(
                    "{} is already claimed by another forge run ({who}).\n  \
                     Two runs sharing one --out dir overwrite the same corpus.jsonl, and a \
                     corpus that changes while it is read is not evidence.\n  \
                     Either point this run at a fresh --out DIR, or pass --force if that \
                     lock is stale from a run that was killed.",
                    out_dir.display()
                ))
            }
            Err(e) => Err(format!("could not claim {}: {e}", path.display())),
        }
    }
}

impl Drop for OutLock {
    fn drop(&mut self) {
        // A lock that survives its run turns one killed process into a
        // directory nobody can use. Drop runs on the error paths too, which is
        // the point of tying release to ownership rather than to a happy path.
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "crucible: could not release the lock at {} ({e}) — a later run will \
                     need --force",
                    self.path.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-PROCESS fixture dir. Fixed names collide when two test binaries
    /// run at once, and one suite then deletes another's fixture — which
    /// produces a failure that looks like a code bug and is not. Exactly the
    /// class of flakiness this project exists to refuse to record as a
    /// verdict, so it does not get to live in this project's own tests.
    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("crucible-outlock-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        d
    }

    #[test]
    fn a_second_claim_on_the_same_dir_is_refused() {
        let d = tmp("second");
        let first = OutLock::claim(&d, false).expect("first claim");
        let second = OutLock::claim(&d, false);
        let msg = second.expect_err("a second run must NOT be allowed to share the dir");
        // The message has to say what to DO, not just that something is wrong.
        // The incident this guards against cost twenty minutes precisely
        // because nothing named the cause.
        assert!(
            msg.contains("--force"),
            "must offer the escape hatch: {msg}"
        );
        assert!(
            msg.contains("--out"),
            "must suggest a fresh output dir: {msg}"
        );
        assert!(
            msg.contains("corpus"),
            "must say WHY it matters — the corpus is the artifact at risk: {msg}"
        );
        drop(first);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_lock_is_released_on_drop_so_a_later_run_succeeds() {
        let d = tmp("release");
        {
            let _l = OutLock::claim(&d, false).expect("first claim");
        }
        // Dropped: the next run must not need --force.
        let _again = OutLock::claim(&d, false).expect("lock must release on drop");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn force_breaks_a_stale_lock() {
        let d = tmp("force");
        // Leak the guard to simulate a killed run: the file stays behind with
        // no live owner, which is exactly the stale case --force exists for.
        let held = OutLock::claim(&d, false).expect("first claim");
        std::mem::forget(held);
        assert!(
            OutLock::claim(&d, false).is_err(),
            "a stale lock must still block by default"
        );
        let _forced = OutLock::claim(&d, true).expect("--force must break a stale lock");
        std::fs::remove_dir_all(&d).ok();
    }
}
