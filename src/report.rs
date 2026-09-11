//! The assay report — what the run actually found.
//!
//! Two audiences, one run. A training pipeline wants the admitted rows. The
//! REPO OWNER wants the survivors: every mutation no test noticed is a place the
//! target can be broken silently. On the first real run against nedb that list
//! included `note_shadow_error(e)` -> `pass` in `wrap_sqlite._execute` — the
//! exact observability call added in v3.2.2 to fix a silent-shadow bug, still
//! unguarded by any of the 11 suites.
//!
//! So survivors are printed as a FINDING, not as run noise. A tool that buries
//! them is a tool that wastes the more valuable half of its own output.

use crate::model::{Aborted, Trial};
use std::collections::BTreeMap;

pub struct Assay {
    pub trials: Vec<Trial>,
    pub aborted: Vec<Aborted>,
    pub candidates_seen: usize,
    pub rejected_uncompilable: usize,
    pub files: usize,
    pub wall_ms: u64,
}

impl Assay {
    pub fn killed(&self) -> usize {
        self.trials.iter().filter(|t| t.verdict.killed()).count()
    }
    pub fn survived(&self) -> usize {
        self.trials.len() - self.killed()
    }
    pub fn kill_rate(&self) -> f64 {
        if self.trials.is_empty() {
            0.0
        } else {
            self.killed() as f64 * 100.0 / self.trials.len() as f64
        }
    }

    /// Kill rate per operator. The number that tells you which operators are
    /// earning their place: one that never kills is generating noise, and one
    /// that always kills may be too crude to be interesting.
    pub fn by_operator(&self) -> BTreeMap<String, (usize, usize)> {
        let mut m: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for t in &self.trials {
            let e = m.entry(t.candidate.operator.clone()).or_insert((0, 0));
            e.1 += 1;
            if t.verdict.killed() {
                e.0 += 1;
            }
        }
        m
    }

    /// Survivors whose operator is in the SILENT class, worst first. This is the
    /// coverage report: a silent operator that survives is a defect class the
    /// target cannot currently detect at all.
    pub fn silent_survivors(&self) -> Vec<&Trial> {
        let mut v: Vec<&Trial> = self
            .trials
            .iter()
            .filter(|t| !t.verdict.killed() && t.candidate.severity == "silent")
            .collect();
        v.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.candidate.line.cmp(&b.candidate.line))
        });
        v
    }
}
