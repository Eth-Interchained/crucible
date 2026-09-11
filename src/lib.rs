//! crucible — a forge for training data that had to earn its place.
//!
//! The premise, learned the hard way on two prior model projects:
//!
//! 1. `nedb-cast-slm` (3.33M params, trained from scratch in 41 minutes) worked
//!    because **the verifier already existed**. NEDB's own NQL parser was the
//!    corpus generator, the grader, AND the entry gate — no example entered
//!    training unless it round-tripped to a canonically identical plan. We never
//!    wrote a verifier; one had already shipped.
//! 2. `imagine` taught that fine-tuning is usually the wrong tool. Schema design
//!    moved usable tool calls 2/7 -> 6/7 while ~80 minutes of LoRA moved the same
//!    metric by zero and broke two behaviours nobody was measuring.
//!
//! Put together: for a CODING model, the verifier that already exists is the
//! compiler, the type checker, and the test suite. So crucible never asks a model
//! or a human whether an example is correct. It breaks a green tree, watches a
//! real suite go red, and admits the repair only because reverting it goes green
//! again. Execution is the only label.
//!
//! WHAT MEASUREMENT ALREADY CHANGED IN THIS DESIGN (all four found by running it,
//! not by reasoning about it):
//!
//! - **Mutations hang, they do not merely fail.** A dropped guard turned a 117 ms
//!   suite into an infinite loop. Timeouts are therefore ADAPTIVE (a multiple of
//!   the measured baseline) and `Timeout` is its own outcome.
//! - **Isolation is mandatory.** A prototype died before its restore line and
//!   left the target tree dirty. Every verification happens in its own git
//!   worktree; the source tree is never written to.
//! - **Breadth of grader bought nothing.** 0 of 10 surviving mutations died when
//!   the grader went from 3 suites to 11. So we run the targeted subset (0.9 s)
//!   and reserve the full grader (7.5 s) for confirming admitted examples.
//! - **Survivors are a product, not a failure.** A mutation no test notices is a
//!   coverage gap in the target. Those are reported, never silently dropped.

pub mod eval;
pub mod flair;
pub mod forge;
pub mod history;
pub mod locate;
pub mod model;
pub mod outlock;
pub mod pairs;
pub mod render;
pub mod report;
pub mod target;
pub mod verify;
pub mod worktree;

#[cfg(test)]
mod tests;
