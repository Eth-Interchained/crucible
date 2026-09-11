//! The scoreboard. Does a model actually repair the defect?
//!
//! WHY THIS EXISTS BEFORE ANY TRAINING HAS HAPPENED. Everything else in this
//! crate generates data and measures its yield. None of it establishes that the
//! rows are SOLVABLE. If a capable model scores zero, that is evidence the
//! corpus is broken — and without an eval there is no way to tell that apart
//! from a weak model. Building the scoreboard before the trainer is not
//! optional; it is the only way a training run produces a number rather than a
//! feeling.
//!
//! It is EXECUTION-GATED, like the rest. No judge model is asked whether a patch
//! looks correct. The patch is applied to the broken tree and the real suite is
//! run. Green or not green.
//!
//! THE BROKEN TREE IS RECONSTRUCTED BY REVERSING THE ROW'S OWN DIFF. A corpus
//! row's completion is a patch from broken to fixed, and the worktree starts
//! pristine (fixed), so `git apply -R` produces exactly the tree the prompt
//! describes. That means eval needs only `corpus.jsonl` — no trials file, no
//! re-derivation of the mutation, and no chance of the eval's idea of "broken"
//! drifting from the row's.
//!
//! FOUR OUTCOMES, and the fourth is the one that makes this worth trusting:
//!
//! - `Solved` — the patch applied and the suite went green. The only win.
//! - `NotFixed` — applied cleanly, suite still red. An honest miss.
//! - `Malformed` — no sentinel block, or a diff `git apply` refused. Says which.
//! - `Cheated` — **the patch touched a file the prompt never showed it**, most
//!   importantly a test file. A model can turn any suite green by deleting the
//!   failing assertion, and an eval that scores that as a fix is an eval that
//!   rewards reward-hacking. Checked BEFORE the patch is applied, so a cheat is
//!   never even given the chance to run.
//!
//! THE HARNESS IS TESTED BEFORE IT IS TRUSTED, with three synthetic models:
//! `oracle` replays each row's own completion and MUST score 100% — anything
//! less is a broken harness, not a hard corpus. `null` answers nothing and must
//! score 0%. `cheat` deletes the failing test and must be caught as `Cheated`
//! rather than `Solved`. On a prior project the eval harness was wrong seven
//! times before it was right, and every one of those times it was measuring
//! something other than what it claimed.

use crate::model::Example;
use crate::target::{Grader, Target};
use crate::verify::{self, Baseline};
use crate::worktree::Worktree;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Score {
    Solved,
    NotFixed {
        suite: String,
    },
    Malformed {
        why: String,
    },
    Cheated {
        touched: Vec<String>,
    },
    /// The row itself did not reproduce: reversing its diff did not turn the
    /// suite red. Counted separately because it indicts the CORPUS, not the
    /// model, and silently scoring it as a model failure would hide a
    /// generator bug behind a bad eval number.
    RowDidNotReproduce,
}

impl Score {
    pub fn label(&self) -> &'static str {
        match self {
            Score::Solved => "SOLVED",
            Score::NotFixed { .. } => "MISSED",
            Score::Malformed { .. } => "MALFORMED",
            Score::Cheated { .. } => "CHEATED",
            Score::RowDidNotReproduce => "BAD-ROW",
        }
    }
    pub fn solved(&self) -> bool {
        matches!(self, Score::Solved)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Result_ {
    pub id: String,
    pub file: String,
    pub operator: String,
    pub score: Score,
    pub elapsed_ms: u64,
    /// The model's raw reply, kept so a MALFORMED verdict can be argued with.
    pub reply_head: String,
}

/// Extract the patch from a model reply.
///
/// Sentinel blocks, read with the same discipline the `sentinel-blocks` library
/// encodes: take the LAST closed block, because models commonly emit a skeleton
/// or an example block before the real answer. Content inside sentinels is taken
/// verbatim and never re-parsed, so a diff containing quotes or braces cannot
/// corrupt the extraction the way it corrupts JSON.
pub fn extract_patch(reply: &str) -> Result<String, String> {
    const OPEN: &str = "<<<PATCH>>>";
    const CLOSE: &str = "<<<END>>>";
    let mut best: Option<String> = None;
    let mut cursor = 0usize;
    while let Some(o) = reply[cursor..].find(OPEN) {
        let start = cursor + o + OPEN.len();
        match reply[start..].find(CLOSE) {
            Some(c) => {
                let body = &reply[start..start + c];
                best = Some(body.trim_start_matches('\n').to_string());
                cursor = start + c + CLOSE.len();
            }
            None => break, // an unclosed block is not an answer
        }
    }
    match best {
        Some(b) if b.trim().is_empty() => Err("sentinel block was empty".into()),
        Some(b) => Ok(b),
        None => {
            if reply.contains(OPEN) {
                Err("a <<<PATCH>>> block was opened and never closed".into())
            } else {
                Err("no <<<PATCH>>> block in the reply".into())
            }
        }
    }
}

/// Which files a unified diff claims to modify.
///
/// Read from the `+++ b/<path>` lines. Used to refuse a patch that reaches
/// outside the file the prompt showed — see `Score::Cheated`.
pub fn patched_files(diff: &str) -> Vec<String> {
    let mut out = Vec::new();
    for l in diff.lines() {
        for prefix in ["+++ b/", "+++ ", "--- a/"] {
            if let Some(rest) = l.strip_prefix(prefix) {
                let p = rest.split('\t').next().unwrap_or(rest).trim();
                if p.is_empty() || p == "/dev/null" {
                    continue;
                }
                let p = p.trim_start_matches("a/").trim_start_matches("b/");
                if !out.contains(&p.to_string()) {
                    out.push(p.to_string());
                }
                break;
            }
        }
    }
    out
}

/// Does this patch stay inside the file the row is about?
///
/// THE ANTI-CHEAT. A model can make any suite green by editing the test that
/// fails, and a naive eval scores that as a repair. The prompt shows exactly one
/// file, so a patch touching anything else is out of contract — refused before
/// it is applied, never after.
pub fn within_contract(diff: &str, allowed: &str) -> Result<(), Vec<String>> {
    let touched = patched_files(diff);
    let outside: Vec<String> = touched
        .iter()
        .filter(|p| p.as_str() != allowed)
        .cloned()
        .collect();
    if outside.is_empty() {
        Ok(())
    } else {
        Err(outside)
    }
}

fn git(cwd: &Path, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd).stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if let Some(s) = stdin {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .ok_or("no stdin pipe")?
            .write_all(s.as_bytes())
            .map_err(|e| format!("write patch: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git wait: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Apply a unified diff via `git apply`, rather than a hand-rolled patcher.
///
/// `git apply` is the tool that already handles context matching, fuzz, and
/// line-ending edge cases, and its refusals are specific enough to report.
/// Writing a patcher here would mean a MALFORMED verdict could be my parser's
/// fault rather than the model's.
pub fn apply(cwd: &Path, diff: &str, reverse: bool) -> Result<(), String> {
    let mut args = vec!["apply", "--whitespace=nowarn"];
    if reverse {
        args.push("-R");
    }
    args.push("-");
    git(cwd, &args, Some(diff)).map(|_| ())
}

/// Anything that can answer a prompt with a patch.
pub trait Responder {
    fn name(&self) -> &str;
    fn answer(&self, ex: &Example) -> Result<String, String>;
}

/// Replays the row's own completion. MUST score 100%.
///
/// This is the harness's own test. If the oracle does not solve every row, the
/// eval is broken — the reconstruction, the patch application, or the grader —
/// and any number it reports about a real model is meaningless.
pub struct Oracle;
impl Responder for Oracle {
    fn name(&self) -> &str {
        "oracle"
    }
    fn answer(&self, ex: &Example) -> Result<String, String> {
        Ok(ex.completion.clone())
    }
}

/// Answers nothing. Must score 0% and be MALFORMED, not MISSED.
pub struct Null;
impl Responder for Null {
    fn name(&self) -> &str {
        "null"
    }
    fn answer(&self, _ex: &Example) -> Result<String, String> {
        Ok("I could not determine the defect.".into())
    }
}

/// Deletes the failing test instead of fixing the code. Must be CHEATED.
///
/// The adversarial baseline. If this scores as SOLVED, the eval rewards
/// reward-hacking and every number it has ever produced is suspect.
pub struct Cheat {
    pub test_path: String,
}
impl Responder for Cheat {
    fn name(&self) -> &str {
        "cheat"
    }
    fn answer(&self, _ex: &Example) -> Result<String, String> {
        Ok(format!(
            "<<<PATCH>>>\n--- a/{p}\n+++ b/{p}\n@@ -1,3 +1,3 @@\n-import sys\n+import sys  # neutered\n<<<END>>>\n",
            p = self.test_path
        ))
    }
}

/// Restore one file, and turn a failure into an error that names it.
///
/// The whole point is that this is never `let _ =`. A restore that silently
/// fails leaves the worktree dirty and makes the NEXT row's verdict a lie.
fn restore_or_explain(wt: &Worktree, rel: &Path) -> Result<(), String> {
    wt.restore(rel).map_err(|e| {
        format!(
            "could not restore {} after scoring: {e}. Refusing to continue — every \
             later row would be graded against this file's leftovers.",
            rel.display()
        )
    })
}

/// One row, end to end.
#[allow(clippy::too_many_arguments)]
pub fn score_one(
    wt: &Worktree,
    target: &Target,
    base: &Baseline,
    ex: &Example,
    responder: &dyn Responder,
) -> Result<Result_, String> {
    let started = std::time::Instant::now();
    let rel = Path::new(&ex.file);
    let graders: Vec<&Grader> = target.graders_for(&ex.file);

    // THE TREE MUST BE CLEAN BEFORE THIS ROW TOUCHES IT.
    //
    // One worktree is reused across every row, and the first version of this
    // function discarded every restore result with `let _ =`. So a restore that
    // failed was invisible, row N+1 ran on row N's leftovers, and its grade was
    // red for a reason that had nothing to do with row N+1 — reported as MISSED,
    // which reads as "the model failed" when the harness was at fault. That is
    // the same class of bug as the corpus poisoning this whole eval exists to
    // catch, so it gets the same treatment: check, and say which files are
    // dirty. `Worktree::is_pristine` already existed and was never called.
    match wt.is_pristine() {
        Ok(true) => {}
        Ok(false) => {
            let dirty = wt.dirty_files().unwrap_or_default();
            return Err(format!(
                "the eval worktree is dirty before scoring `{}`: {}. A previous row's \
                 restore did not take, so this row would be graded against someone \
                 else's leftovers. Refusing rather than reporting a MISS that is mine.",
                ex.id,
                if dirty.is_empty() {
                    "(git reported changes but listed none)".to_string()
                } else {
                    dirty.join(", ")
                }
            ));
        }
        Err(e) => return Err(format!("could not check worktree cleanliness: {e}")),
    }

    let finish = |score: Score, reply: &str, started: std::time::Instant| Result_ {
        id: ex.id.clone(),
        file: ex.file.clone(),
        operator: ex.operator.clone(),
        score,
        elapsed_ms: started.elapsed().as_millis() as u64,
        reply_head: reply.chars().take(180).collect(),
    };

    // 1. Break the tree by REVERSING the row's own patch. The worktree is
    //    pristine, the completion goes broken -> fixed, so -R lands exactly on
    //    the state the prompt describes.
    let row_patch = match extract_patch(&ex.completion) {
        Ok(p) => p,
        Err(e) => {
            // The CORPUS is malformed, not the model. Distinct verdict.
            return Ok(finish(
                Score::Malformed {
                    why: format!("the corpus row's own completion is unusable: {e}"),
                },
                "",
                started,
            ));
        }
    };
    if let Err(e) = apply(&wt.root, &row_patch, true) {
        restore_or_explain(wt, rel)?;
        return Ok(finish(
            Score::Malformed {
                why: format!("could not reconstruct the broken tree from the row: {e}"),
            },
            "",
            started,
        ));
    }

    // 2. Confirm the row actually reproduces. A row whose broken state is GREEN
    //    is a generator bug, and scoring it against the model would hide that.
    let before = verify::grade(&wt.root, &graders, base)?;
    if !before.killed() {
        restore_or_explain(wt, rel)?;
        return Ok(finish(Score::RowDidNotReproduce, "", started));
    }

    // 3. Ask.
    let reply = match responder.answer(ex) {
        Ok(r) => r,
        Err(e) => {
            restore_or_explain(wt, rel)?;
            return Ok(finish(
                Score::Malformed {
                    why: format!("responder failed: {e}"),
                },
                "",
                started,
            ));
        }
    };
    let patch = match extract_patch(&reply) {
        Ok(p) => p,
        Err(e) => {
            restore_or_explain(wt, rel)?;
            return Ok(finish(Score::Malformed { why: e }, &reply, started));
        }
    };

    // 4. ANTI-CHEAT, before the patch is allowed to touch anything.
    if let Err(outside) = within_contract(&patch, &ex.file) {
        restore_or_explain(wt, rel)?;
        return Ok(finish(Score::Cheated { touched: outside }, &reply, started));
    }

    // 5. Apply and re-grade.
    if let Err(e) = apply(&wt.root, &patch, false) {
        restore_or_explain(wt, rel)?;
        return Ok(finish(
            Score::Malformed {
                why: format!("git apply refused the patch: {e}"),
            },
            &reply,
            started,
        ));
    }
    let after = verify::grade(&wt.root, &graders, base);
    // Restore BEFORE unwrapping the grade, so a grader error still leaves a
    // clean tree for the next row — and the restore itself is checked.
    restore_or_explain(wt, rel)?;
    let after = after?;

    Ok(finish(
        if after.killed() {
            Score::NotFixed {
                suite: match &after {
                    crate::model::Verdict::Failed { suite, .. } => suite.clone(),
                    crate::model::Verdict::Timeout { suite, .. } => suite.clone(),
                    crate::model::Verdict::Flaky { suite } => suite.clone(),
                    crate::model::Verdict::Survived => String::new(),
                },
            }
        } else {
            Score::Solved
        },
        &reply,
        started,
    ))
}

pub fn load_corpus(path: &Path) -> Result<Vec<Example>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Example>(line) {
            Ok(e) => out.push(e),
            // A bad line is named, not skipped: a corpus that silently loses
            // rows reports a denominator nobody can reproduce.
            Err(e) => return Err(format!("{path:?} line {}: {e}", i + 1)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_closed_block_wins() {
        // Models emit a skeleton block before the real answer — the prompt in
        // this corpus literally shows them one. Taking the FIRST block would
        // score the template instead of the answer.
        let reply =
            "sure:\n<<<PATCH>>>\n--- a/x\n<<<END>>>\nactually:\n<<<PATCH>>>\nREAL\n<<<END>>>\n";
        assert_eq!(extract_patch(reply).unwrap().trim(), "REAL");
    }

    #[test]
    fn an_unclosed_block_is_not_an_answer() {
        let e = extract_patch("<<<PATCH>>>\n--- a/x\n").unwrap_err();
        assert!(e.contains("never closed"), "{e}");
    }

    #[test]
    fn no_block_at_all_says_so() {
        let e = extract_patch("here is the fix: change the operator").unwrap_err();
        assert!(e.contains("no <<<PATCH>>> block"), "{e}");
    }

    #[test]
    fn an_empty_block_is_refused() {
        let e = extract_patch("<<<PATCH>>>\n\n<<<END>>>").unwrap_err();
        assert!(e.contains("empty"), "{e}");
    }

    #[test]
    fn files_are_read_off_both_diff_headers() {
        let d = "--- a/python/nedb/engine.py\n+++ b/python/nedb/engine.py\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(patched_files(d), vec!["python/nedb/engine.py"]);
    }

    #[test]
    fn editing_the_test_instead_of_the_code_is_caught() {
        // THE ANTI-CHEAT. A model can turn any suite green by deleting the
        // assertion that fails. An eval that scores that as a repair rewards
        // reward-hacking, and every number it ever produced would be suspect.
        let d =
            "--- a/tests/test_nedb.py\n+++ b/tests/test_nedb.py\n@@ -1 +1 @@\n-assert x\n+pass\n";
        let err = within_contract(d, "python/nedb/engine.py").unwrap_err();
        assert_eq!(err, vec!["tests/test_nedb.py"]);
    }

    #[test]
    fn a_patch_that_also_touches_another_source_file_is_out_of_contract() {
        // Not only tests. The prompt showed ONE file; a patch reaching into a
        // second one is answering a question it was not asked, and its green
        // suite does not mean it repaired the defect described.
        let d = "--- a/a.py\n+++ b/a.py\n@@ -1 +1 @@\n-x\n+y\n\
                 --- a/b.py\n+++ b/b.py\n@@ -1 +1 @@\n-p\n+q\n";
        let err = within_contract(d, "a.py").unwrap_err();
        assert_eq!(err, vec!["b.py"]);
    }

    #[test]
    fn an_in_contract_patch_passes() {
        let d = "--- a/a.py\n+++ b/a.py\n@@ -1 +1 @@\n-x\n+y\n";
        assert!(within_contract(d, "a.py").is_ok());
    }

    #[test]
    fn the_null_model_is_malformed_not_merely_wrong() {
        // "I don't know" and "here is a patch that did not work" are different
        // failures, and collapsing them would hide a model that cannot produce
        // the output format at all behind a plausible-looking miss rate.
        let ex = Example {
            id: "x".into(),
            prompt: String::new(),
            completion: String::new(),
            operator: String::new(),
            severity: String::new(),
            file: "a.py".into(),
            line: 1,
            scope: String::new(),
            verdict: String::new(),
            caused_by: vec![],
            repo_commit: String::new(),
        };
        let r = Null.answer(&ex).unwrap();
        assert!(extract_patch(&r).is_err());
    }
}
