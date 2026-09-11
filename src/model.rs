//! The nouns. Kept in one place because the corpus format IS the contract with
//! whatever trains on it, and a format that drifts silently is a corpus you
//! cannot reproduce.

use serde::{Deserialize, Serialize};

/// One candidate mutation, as reported by a language locator.
///
/// Offsets are ABSOLUTE BYTES into the file. Locators resolve them in the
/// language that owns the file's syntax, because `col_offset` in Python's `ast`
/// is a UTF-8 byte offset within a line — a footgun the moment a non-ASCII
/// character appears above the mutation site. The forge does a pure byte splice
/// and therefore cannot get it wrong.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub operator: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub before: String,
    pub after: String,
    pub line: usize,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub scope: String,
    /// `"silent"` or `"loud"`. Silent operators leave code that still runs,
    /// still returns, and lies — the defect class that cost a full day on
    /// 2026-09-08 (`except Exception: pass` swallowing a TypeError while
    /// `verify()` kept returning True). A model trained to repair these is worth
    /// more than one trained on crashes, because crashes announce themselves.
    #[serde(default)]
    pub severity: String,
}

/// A locator's full report for one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocateReport {
    pub file: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    /// Candidates the locator built and then REFUSED because the spliced file
    /// would not compile. Counted, never hidden: "fix the SyntaxError" teaches
    /// nothing about the defect classes we want, and a rising count here is how
    /// a broken locator announces itself.
    #[serde(default)]
    pub rejected_uncompilable: usize,
    #[serde(default)]
    pub error: Option<String>,
}

/// What the grader said about a mutated tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// A suite failed. The mutation is detectable; this is a usable example.
    Failed { suite: String, tail: String },
    /// A suite ran past its adaptive deadline. STILL a kill — the mutation broke
    /// the code — but recorded separately because the diagnostic an agent would
    /// see is a hang, not an assertion, and because a hang is the one outcome
    /// that can starve a whole run if it is not bounded.
    Timeout { suite: String, limit_ms: u64 },
    /// The suite went red on the mutated tree AND on the pristine tree, checked
    /// back-to-back in the same worktree under the same load. The failure is
    /// real but the mutation did not cause it — a flaky or load-sensitive suite.
    ///
    /// This verdict exists because its absence poisoned a corpus. On a 2-core
    /// box with two workers, `test_proof` credited 11 of 18 kills, including for
    /// a mutation that provably cannot execute. Attributing those to the
    /// candidate produced rows whose "broken" state is green, which `crucible
    /// eval` then caught as the oracle scoring 38.9% instead of 100%.
    Flaky { suite: String },
    /// Every suite passed. The mutation is invisible to the target's tests: a
    /// coverage gap. Useless as a training pair (there is no failing output to
    /// put in the prompt) and valuable as a report.
    Survived,
}

impl Verdict {
    /// A kill is a defect the suite detected BECAUSE of the mutation. A flaky
    /// red is not a kill, and counting it as one is how a corpus gets rows whose
    /// broken state is green.
    pub fn killed(&self) -> bool {
        matches!(self, Verdict::Failed { .. } | Verdict::Timeout { .. })
    }
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Failed { .. } => "FAILED",
            Verdict::Timeout { .. } => "TIMEOUT",
            Verdict::Flaky { .. } => "FLAKY",
            Verdict::Survived => "SURVIVED",
        }
    }
}

/// One verified mutation: what we broke, what the grader said, how long it took.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trial {
    /// Content-addressed: BLAKE-free, plain SHA-256 over (file sha, operator,
    /// span, replacement). Re-running the same forge over the same tree yields
    /// the same ids, so a corpus is idempotent and a re-run writes nothing new.
    pub id: String,
    pub file: String,
    pub file_sha256: String,
    pub candidate: Candidate,
    pub verdict: Verdict,
    pub elapsed_ms: u64,
    /// The suites actually run for this trial, in order.
    pub graders: Vec<String>,
}

/// A training row. Prompt carries what a real agent would have — the failing
/// test output — and the completion is an applicable patch in sentinel-block
/// form, not prose.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Example {
    pub id: String,
    pub prompt: String,
    pub completion: String,
    pub operator: String,
    pub severity: String,
    pub file: String,
    pub line: usize,
    pub scope: String,
    pub verdict: String,
    /// Provenance: the trial this row came from, and the tree it was cut against.
    pub caused_by: Vec<String>,
    pub repo_commit: String,
}

/// Why a trial never reached a verdict. Never silently dropped — an unexplained
/// skip is indistinguishable from a broken dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Aborted {
    pub file: String,
    pub operator: String,
    pub line: usize,
    pub reason: String,
}
