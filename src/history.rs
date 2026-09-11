//! Mine verified fixes out of git history.
//!
//! THE IDEA, and why it needs no heuristics: a commit is a bug fix if the test
//! suite says so. Check out its PARENT — if the grader is red there and green at
//! the commit itself, that commit repaired something the suite can see, and the
//! diff between them is a label nobody had to write. Commit messages are never
//! consulted. "fix:" in a subject line is a claim; a suite going from red to
//! green is a verdict.
//!
//! This is the same double gate the mutation forge uses, pointed backwards. The
//! mutation side manufactures defects and is unbounded but synthetic. This side
//! is bounded by how much history exists, and every row is a defect a human
//! actually shipped and a human actually fixed — which is the distribution we
//! ultimately care about.
//!
//! FOUR OUTCOMES, and the three that are not fixes are all interesting:
//!
//! - `Fixed` — parent red, commit green. A verified repair. This is a row.
//! - `GreenParent` — the suite was already green before the commit. A feature, a
//!   refactor, a docs change, or a fix for something no test covers. NOT a row:
//!   there is no failing output to put in a prompt.
//! - `StillRed` — red before AND after. The commit did not fix what this grader
//!   is complaining about. Usually means the suite cannot run at that point in
//!   history (a dependency or an API moved), which is a fact about the repo's
//!   past rather than a defect.
//! - `Unrunnable` — the grader could not be executed at all.
//!
//! `StillRed` is expected to dominate on older history and that is not a
//! failure of this tool. A suite written in August cannot grade a commit from
//! June. The rate is REPORTED so the usable depth of history is a measurement
//! rather than an assumption.

use crate::target::{Grader, Target};
use crate::verify::{self, Baseline};
use crate::worktree::Worktree;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub parent: String,
    pub subject: String,
    /// Files this commit touched that the target considers mutable source.
    pub touched: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Fixed {
        suite: String,
        tail: String,
    },
    GreenParent,
    StillRed {
        suite: String,
    },
    /// The graders that focus this file had not been written yet at this point
    /// in history. Distinct from `StillRed` on purpose: conflating them made a
    /// missing test file look like an ungradeable era.
    NotYetWritten {
        suites: Vec<String>,
    },
    Unrunnable {
        why: String,
    },
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Fixed { .. } => "FIXED",
            Outcome::GreenParent => "GREEN-BEFORE",
            Outcome::StillRed { .. } => "STILL-RED",
            Outcome::NotYetWritten { .. } => "NO-GRADER-YET",
            Outcome::Unrunnable { .. } => "UNRUNNABLE",
        }
    }
    pub fn usable(&self) -> bool {
        matches!(self, Outcome::Fixed { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Replay {
    pub commit: Commit,
    pub outcome: Outcome,
    pub elapsed_ms: u64,
}

/// Does this grader's script exist in the tree as checked out?
///
/// Looks for the first argv element that resolves to a path inside the
/// worktree. Deliberately conservative: a grader whose command is not a file
/// (a build tool, a shell pipeline) is treated as PRESENT, because absence
/// cannot be established and guessing "missing" would silently drop a valid
/// grader — the opposite of the bug this function exists to fix.
pub fn grader_exists(root: &Path, g: &Grader) -> bool {
    let script = g.argv.iter().skip(1).find(|a| a.contains('/'));
    match script {
        Some(rel) => root.join(rel).exists(),
        None => true,
    }
}

fn git_out(cwd: &Path, args: &[&str]) -> Result<String, String> {
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
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Candidate commits, newest first.
///
/// MERGES ARE SKIPPED. A merge has two parents, so "the state before this
/// change" is ambiguous, and its diff against either parent includes work from
/// the other branch — a label containing changes the commit did not make.
pub fn candidates(
    repo: &Path,
    sources: &[String],
    ext: &str,
    limit: usize,
) -> Result<Vec<Commit>, String> {
    let raw = git_out(
        repo,
        &[
            "log",
            "--no-merges",
            "--format=%H%x00%P%x00%s",
            "-n",
            &(limit * 4).to_string(),
        ],
    )?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let mut parts = line.split('\0');
        let sha = parts.next().unwrap_or("").to_string();
        let parents = parts.next().unwrap_or("").to_string();
        let subject = parts.next().unwrap_or("").to_string();
        if sha.is_empty() || parents.trim().is_empty() {
            continue; // the root commit has no parent to compare against
        }
        let parent = parents.split_whitespace().next().unwrap_or("").to_string();
        let files = git_out(
            repo,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", &sha],
        )
        .unwrap_or_default();
        let touched: Vec<String> = files
            .lines()
            .filter(|f| {
                f.ends_with(&format!(".{ext}")) && sources.iter().any(|s| f.starts_with(s.as_str()))
            })
            .map(str::to_string)
            .collect();
        if touched.is_empty() {
            continue;
        }
        out.push(Commit {
            sha,
            parent,
            subject,
            touched,
        });
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Replay one commit against its parent.
///
/// Order matters: the PARENT is graded first. If the parent is green there is
/// nothing to learn and the commit's own run is skipped entirely — which is most
/// of the history and most of the saved time.
pub fn replay(
    wt: &Worktree,
    target: &Target,
    base: &Baseline,
    c: &Commit,
) -> Result<(Outcome, u64), String> {
    let started = std::time::Instant::now();
    let wanted: Vec<&Grader> = {
        // Grade with the union of what each touched file focuses on, so a commit
        // spanning two subsystems is judged by both.
        let mut names: Vec<String> = Vec::new();
        for f in &c.touched {
            for g in target.graders_for(f) {
                if !names.contains(&g.name) {
                    names.push(g.name.clone());
                }
            }
        }
        names
            .iter()
            .filter_map(|n| target.graders.iter().find(|g| &g.name == n))
            .collect()
    };

    if let Err(e) = wt.checkout(&c.parent) {
        return Ok((
            Outcome::Unrunnable {
                why: format!(
                    "checkout parent {}: {e}",
                    &c.parent[..9.min(c.parent.len())]
                ),
            },
            started.elapsed().as_millis() as u64,
        ));
    }

    // A GRADER THAT DID NOT EXIST YET IS NOT A FAILING GRADER.
    //
    // This was a real defect in the first version, and it was silent in the way
    // this whole project exists to catch. `python3 tests/test_wrap_sqlite_shadow.py`
    // against a commit from before that file was written exits 2 — which scored
    // as "red". Red at the parent and red at the commit, so every such commit
    // came back STILL-RED, and "this test had not been written yet" was
    // reported as "the suite cannot grade this era". 64% of the first run was
    // this one bug wearing a plausible explanation.
    //
    // So applicability is checked against the tree, per commit, and a grader
    // that is absent is dropped with the reason recorded rather than counted as
    // a failure.
    let mut graders: Vec<&Grader> = Vec::new();
    let mut absent: Vec<String> = Vec::new();
    for g in &wanted {
        if grader_exists(&wt.root, g) {
            graders.push(g);
        } else {
            absent.push(g.name.clone());
        }
    }
    if graders.is_empty() {
        return Ok((
            Outcome::NotYetWritten {
                suites: if absent.is_empty() {
                    vec!["(no grader focused this file)".into()]
                } else {
                    absent
                },
            },
            started.elapsed().as_millis() as u64,
        ));
    }

    let mut parent_red: Option<(String, String)> = None;
    for g in &graders {
        let d = base.deadline_for(&g.name);
        match verify::run_grader(&wt.root, g, d) {
            Ok(r) if r.timed_out => {
                parent_red = Some((
                    g.name.clone(),
                    format!("suite did not terminate within {d} ms"),
                ));
                break;
            }
            Ok(r) if !r.ok => {
                parent_red = Some((g.name.clone(), r.tail));
                break;
            }
            Ok(_) => {}
            Err(e) => {
                return Ok((
                    Outcome::Unrunnable { why: e },
                    started.elapsed().as_millis() as u64,
                ))
            }
        }
    }

    let Some((suite, tail)) = parent_red else {
        return Ok((Outcome::GreenParent, started.elapsed().as_millis() as u64));
    };

    // The parent is red. Does THIS commit make that same suite green?
    if let Err(e) = wt.checkout(&c.sha) {
        return Ok((
            Outcome::Unrunnable {
                why: format!("checkout {}: {e}", &c.sha[..9.min(c.sha.len())]),
            },
            started.elapsed().as_millis() as u64,
        ));
    }
    let g = graders
        .iter()
        .find(|g| g.name == suite)
        .ok_or_else(|| format!("grader {suite} vanished between replays"))?;
    let d = base.deadline_for(&g.name);
    let after = verify::run_grader(&wt.root, g, d)?;
    let elapsed = started.elapsed().as_millis() as u64;
    if after.ok {
        Ok((Outcome::Fixed { suite, tail }, elapsed))
    } else {
        // Red before and after. Almost always means the suite cannot grade this
        // point in history — reported rather than discarded, because the RATE is
        // what tells us how far back the history is usable.
        Ok((Outcome::StillRed { suite }, elapsed))
    }
}

/// The diff a commit made to one file, as the repair direction (parent -> commit).
pub fn file_at(repo: &Path, sha: &str, path: &str) -> Result<String, String> {
    git_out(repo, &["show", &format!("{sha}:{path}")])
}

pub fn touched_line(repo: &Path, sha: &str, path: &str) -> usize {
    // First changed line in the commit's own diff for this file, for the excerpt
    // marker. Best effort: a commit touching many hunks has no single line.
    let raw = git_out(
        repo,
        &["diff", "--unified=0", &format!("{sha}^"), sha, "--", path],
    )
    .unwrap_or_default();
    for l in raw.lines() {
        if let Some(rest) = l.strip_prefix("@@ -") {
            if let Some(plus) = rest.split('+').nth(1) {
                if let Some(num) = plus.split(&[',', ' '][..]).next() {
                    if let Ok(n) = num.parse::<usize>() {
                        return n.max(1);
                    }
                }
            }
        }
    }
    1
}

pub struct HistoryAssay {
    pub replays: Vec<Replay>,
    pub wall_ms: u64,
}

impl HistoryAssay {
    pub fn fixed(&self) -> usize {
        self.replays.iter().filter(|r| r.outcome.usable()).count()
    }
    pub fn count(&self, label: &str) -> usize {
        self.replays
            .iter()
            .filter(|r| r.outcome.label() == label)
            .count()
    }
}

/// Where the worktree for history replay lives.
pub fn work_name() -> PathBuf {
    PathBuf::from("history")
}
