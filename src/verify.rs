//! The gate. Nothing becomes a training row without passing through here twice.
//!
//! THE DOUBLE GATE, and why both halves are load-bearing:
//!
//! 1. The MUTATED tree must go red. If it stays green the mutation is invisible
//!    to the target's tests — a coverage gap, not an example. Admitting it would
//!    produce a row whose prompt has no failing output to show.
//! 2. The PRISTINE tree must be green, measured in the same worktree, before the
//!    mutation. Skipping this is how a flaky suite or a dirty tree silently
//!    becomes a corpus of mislabelled rows: every mutation "kills" a suite that
//!    was already failing, and the model learns to associate an unrelated patch
//!    with an unrelated error. A mislabelled row is worse than a missing one.
//!
//! TIMEOUTS ARE ADAPTIVE, and that was a measurement, not a preference. A
//! `guard_drop` in `wrap_sqlite._host_scan` turned a 117 ms suite into an
//! infinite loop. With the 180 s default a first prototype used, a single hang
//! cost more wall clock than 1,500 good trials; at 20x the measured baseline the
//! same hang is caught in 3.0 s. Thousands of trials only become affordable
//! because the deadline is derived from what the suite actually costs.
//!
//! The deadline is enforced by coreutils `timeout -k`, not by a hand-rolled
//! poll loop, because a hung suite can leave children behind and `timeout`
//! already handles process-group teardown correctly. Exit code 124 is its
//! signal for "deadline hit".

use crate::model::Verdict;
use crate::target::Grader;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Multiple of the measured baseline allowed before a suite is called hung.
/// 20x is generous enough that ordinary variance (a loaded box, a cold page
/// cache) never trips it, and tight enough that a real infinite loop is caught
/// in seconds.
pub const DEADLINE_MULTIPLE: u32 = 20;
/// Floor, for suites so fast that 20x is still a few hundred milliseconds.
pub const DEADLINE_FLOOR_MS: u64 = 3_000;

#[derive(Debug, Clone)]
pub struct Baseline {
    pub ms: HashMap<String, u64>,
    pub deadline_ms: HashMap<String, u64>,
}

impl Baseline {
    pub fn deadline_for(&self, suite: &str) -> u64 {
        *self.deadline_ms.get(suite).unwrap_or(&DEADLINE_FLOOR_MS)
    }
}

pub struct RunOutcome {
    pub ok: bool,
    pub timed_out: bool,
    /// The grader's command could not be executed at all — `timeout` exits 127
    /// for a missing binary and 126 for one that is not executable.
    ///
    /// THIS IS A DISTINCT OUTCOME BECAUSE CONFLATING IT COST ME A DIAGNOSIS.
    /// A Rust run reported "the grader is not green on an unmutated tree" after
    /// exiting in 1 ms; the suite was in fact perfectly green (71 tests, 0.31 s)
    /// and `cargo` simply was not on PATH for that invocation. "Your tests are
    /// failing" and "your test command does not exist" are different problems
    /// with different fixes, and a tool that says the first when it means the
    /// second sends you to read the wrong code.
    pub not_executable: bool,
    pub tail: String,
    pub elapsed_ms: u64,
}

/// Run one grader in `cwd` with a hard deadline.
pub fn run_grader(cwd: &Path, g: &Grader, deadline_ms: u64) -> Result<RunOutcome, String> {
    let secs = ((deadline_ms as f64) / 1000.0).ceil().max(1.0);
    let mut cmd = Command::new("timeout");
    // -k 2: if the suite ignores TERM, it gets KILL two seconds later. Without
    // this a process that traps TERM would hold the worker forever, which is
    // the exact failure the deadline exists to prevent.
    cmd.arg("-k")
        .arg("2")
        .arg(format!("{secs}"))
        .args(&g.argv)
        .current_dir(cwd)
        .stdin(Stdio::null());
    for (k, v) in &g.env {
        cmd.env(k, v.replace("{REPO}", &cwd.display().to_string()));
    }
    let started = Instant::now();
    let out = cmd
        .output()
        .map_err(|e| format!("spawn `timeout {}`: {e}", g.argv.join(" ")))?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let code = out.status.code();
    let timed_out = code == Some(124) || code == Some(137);
    // 127 = not found, 126 = found but not executable. `timeout` passes these
    // through from the shell convention, and a suite cannot fail this fast.
    let not_executable = (code == Some(127) || code == Some(126)) && elapsed_ms < 2_000;
    // The tail is what a real agent would be staring at, so it is kept verbatim
    // from the END of the output (assertions land last) rather than summarised.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Keep a GENEROUS window. Focusing it on the actual failure is render's
    // job, and it cannot focus on what was already thrown away: nedb's suites
    // print a full passing log (60+ `ok` lines), so a tight tail captured here
    // would be nothing but successful assertions with the defect scrolled off
    // the top. Found by reading the first corpus rows.
    let tail: String = {
        let t = combined.trim_end();
        let start = t
            .char_indices()
            .rev()
            .nth(60_000)
            .map(|(i, _)| i)
            .unwrap_or(0);
        t[start..].to_string()
    };
    Ok(RunOutcome {
        ok: out.status.success(),
        timed_out,
        not_executable,
        tail,
        elapsed_ms,
    })
}

/// Measure every grader on a pristine tree, and derive each one's deadline.
///
/// A grader that is RED here aborts the run rather than being excluded. The
/// alternative — quietly dropping it — means the forge silently grades against
/// a weaker suite than the operator believes, and the corpus is subtly wrong in
/// a way nothing in the output would reveal.
pub fn measure_baseline(
    cwd: &Path,
    graders: &[&Grader],
    probe_deadline_ms: u64,
    mut on_result: impl FnMut(&str, u64, u64, bool),
) -> Result<Baseline, String> {
    let mut ms = HashMap::new();
    let mut deadline_ms = HashMap::new();
    let mut red = Vec::new();
    for g in graders {
        let r = run_grader(cwd, g, probe_deadline_ms)?;
        let d = std::cmp::max(DEADLINE_FLOOR_MS, r.elapsed_ms * DEADLINE_MULTIPLE as u64);
        ms.insert(g.name.clone(), r.elapsed_ms);
        deadline_ms.insert(g.name.clone(), d);
        on_result(&g.name, r.elapsed_ms, d, r.ok);
        if r.not_executable {
            // Named separately and FIRST, because it is the one failure here
            // that is not about the target's code at all.
            return Err(format!(
                "the grader `{}` could not be executed: `{}` was not found or is not \
                 executable (exit {:?} after {} ms). This is a PATH or toolchain \
                 problem in the environment crucible was launched from, not a \
                 failing test suite — check `command -v {}`.",
                g.name,
                g.argv.first().map(String::as_str).unwrap_or("(empty argv)"),
                127,
                r.elapsed_ms,
                g.argv.first().map(String::as_str).unwrap_or("?")
            ));
        }
        if !r.ok {
            red.push(format!(
                "{} ({})",
                g.name,
                if r.timed_out {
                    "timed out".to_string()
                } else {
                    "tests failed".to_string()
                }
            ));
        }
    }
    if !red.is_empty() {
        return Err(format!(
            "the grader is not green on an unmutated tree: {}. Every verdict this \
             run produced would be attributed to a mutation that did not cause it, \
             so the run is refused rather than producing a mislabelled corpus.",
            red.join(", ")
        ));
    }
    Ok(Baseline { ms, deadline_ms })
}

/// Grade a mutated tree. Stops at the first red suite — the verdict is "some
/// test catches this", and which one caught it first is the diagnostic an agent
/// would have seen.
pub fn grade(cwd: &Path, graders: &[&Grader], base: &Baseline) -> Result<Verdict, String> {
    for g in graders {
        let d = base.deadline_for(&g.name);
        let r = run_grader(cwd, g, d)?;
        if r.not_executable {
            // A missing grader is NOT a kill. Counting it as one would mark
            // every trial in the run as a detected defect and produce a corpus
            // of rows whose "failing output" is a shell error.
            return Err(format!(
                "the grader `{}` stopped being executable mid-run (`{}` not found) — \
                 refusing to score this trial",
                g.name,
                g.argv.first().map(String::as_str).unwrap_or("?")
            ));
        }
        if r.timed_out {
            return Ok(Verdict::Timeout {
                suite: g.name.clone(),
                limit_ms: d,
            });
        }
        if !r.ok {
            return Ok(Verdict::Failed {
                suite: g.name.clone(),
                tail: r.tail,
            });
        }
    }
    Ok(Verdict::Survived)
}
