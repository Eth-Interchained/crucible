//! What a target repo looks like to the forge.
//!
//! Deliberately a DATA description, not code: adding a repo must never mean
//! writing Rust. A target says where the source lives, which command grades it,
//! and which suites are worth running for which paths. That last mapping is the
//! difference between a 0.9 s verification and a 7.5 s one — measured, not
//! guessed (see `nedb_preset`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grader {
    /// Human name, used in logs and in the corpus row.
    pub name: String,
    /// Argv, run with `cwd` = the worktree root.
    pub argv: Vec<String>,
    /// Environment additions. `{REPO}` expands to the worktree root, so a
    /// PYTHONPATH that must be absolute still works inside a throwaway tree.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Can two copies of this grader run AT THE SAME TIME on one machine?
    ///
    /// A WORKTREE IS NOT ISOLATION IF THE GRADER REACHES OUTSIDE IT. nedb's
    /// `test_deploy` starts a daemon on a HARDCODED port — `PORT = 7172  #
    /// isolated port so it never collides with a running nedbd`. The author
    /// guarded against an external daemon, not against a second copy of the
    /// suite. With two workers both bind 7172: one wins, and the loser either
    /// fails to start its daemon or, far worse, TALKS TO THE WINNER'S DAEMON —
    /// which is serving the winner's MUTATED code. Worker A's mutation then
    /// turns worker B's trial red, and the kill is attributed to B's candidate.
    ///
    /// That produced a partially poisoned corpus and a 45% "kill rate" for
    /// candidates that a single worker proves survive 0/10. It was found by
    /// `crucible eval`: the oracle, which replays each row's own correct repair,
    /// scored 38.9% instead of 100%, and half the rows reported that their own
    /// broken state was GREEN.
    ///
    /// Default TRUE, because most graders are hermetic and a false default
    /// would silently serialise every fleet. A target that is wrong about this
    /// produces mislabelled rows, so it is worth being explicit.
    #[serde(default = "yes")]
    pub hermetic: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    /// Path to the git repo. Never written to — every mutation goes into a
    /// worktree cut from it.
    pub repo: String,
    /// Glob-free, explicit: directories to walk for mutable source.
    pub sources: Vec<String>,
    /// Path substrings to skip inside `sources`. Needed because "the crate" and
    /// "what the grader runs" are not the same set: a `--lib` grader never
    /// executes `src/main.rs` or `src/bin/*`, so mutating them yields survivors
    /// that look like coverage gaps and are really a statement about the grader.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// File extension the locator handles.
    pub extension: String,
    /// Locator argv. `{FILE}` expands to the absolute path of the file.
    pub locator: Vec<String>,
    /// Every grader the target has.
    pub graders: Vec<Grader>,
    /// Which graders to run first for a mutation in a path containing this
    /// substring. Measured finding on nedb: broadening the grader from 3 suites
    /// to 11 killed 0 of 10 survivors, so the targeted subset is not a
    /// heuristic shortcut — it is the same kill power at a quarter of the cost.
    #[serde(default)]
    pub focus: Vec<(String, Vec<String>)>,
    /// Suites excluded from the fast path for being slow rather than
    /// uninformative. Named so the exclusion is a decision, not an accident.
    #[serde(default)]
    pub slow: Vec<String>,
}

impl Target {
    /// Graders that are unsafe to run concurrently.
    pub fn non_hermetic(&self) -> Vec<&Grader> {
        self.graders.iter().filter(|g| !g.hermetic).collect()
    }

    /// Graders for a given file, focused subset first.
    ///
    /// `parallel` drops the non-hermetic ones: with more than one worker they
    /// do not merely fail, they CROSS-ATTRIBUTE — see `Grader::hermetic`.
    pub fn graders_for_parallel(&self, file: &str, parallel: bool) -> Vec<&Grader> {
        let mut g = self.graders_for(file);
        if parallel {
            g.retain(|x| x.hermetic);
        }
        g
    }

    /// Graders for a given file, focused subset first.
    pub fn graders_for(&self, file: &str) -> Vec<&Grader> {
        for (needle, names) in &self.focus {
            if file.contains(needle.as_str()) {
                let picked: Vec<&Grader> = names
                    .iter()
                    .filter_map(|n| self.graders.iter().find(|g| &g.name == n))
                    .collect();
                if !picked.is_empty() {
                    return picked;
                }
            }
        }
        self.graders
            .iter()
            .filter(|g| !self.slow.contains(&g.name))
            .collect()
    }

    /// The full grader, for confirming an admitted example.
    pub fn all_graders(&self) -> Vec<&Grader> {
        self.graders.iter().collect()
    }

    /// nedb's RUST core — the v2/v3 DAG engine.
    ///
    /// MEASURED 2026-09-10: 764 candidates across 31 files, 0 parse errors,
    /// 91,776 bytes of inline test code excluded by the locator (db.rs alone
    /// carries 31,336 of them). Grader cost is 1.58 s for a real one-line edit
    /// plus a full recompile and 71 tests — 1.8x the focused Python grader, not
    /// the 10-20x first guessed at.
    ///
    /// SOURCES ARE THE LIBRARY ONLY, and that is a correctness decision rather
    /// than a scoping preference. The grader is `cargo test -p nedb-engine
    /// --lib`, which never executes `main.rs`, `bin/nedb-cli.rs`, the benches,
    /// or `build.rs`. Mutating those produces ~80 GUARANTEED survivors: trials
    /// that cost 1.6 s each to prove nothing, and a survivor list that reads
    /// like a coverage finding when it is really a statement about what `--lib`
    /// runs. If those binaries are ever worth mutating they need their own
    /// grader, not this one.
    ///
    /// A COLD BUILD IS 42 s IN A FRESH WORKTREE. That is the real cost driver,
    /// not the per-trial recompile — so a production run should give each worker
    /// a persistent `CARGO_TARGET_DIR` and pay it once. Without that, every
    /// worker pays 42 s before its first verdict.
    pub fn nedb_rust_preset(repo: &str) -> Target {
        Target {
            name: "nedb-rust".into(),
            repo: repo.into(),
            // Only the crate the grader actually runs.
            sources: vec!["rust/nedb-v2/src".into()],
            // …and only the LIBRARY within it. `cargo test --lib` does not
            // execute `src/main.rs` or anything under `src/bin/`, so mutations
            // there are guaranteed survivors. The first Rust run spent all five
            // of its trials in `src/bin/nedb-cli.rs` proving exactly that.
            exclude: vec!["/bin/".into(), "/main.rs".into()],
            extension: "rs".into(),
            locator: vec![
                "locators/rust-locate/target/release/rust-locate".into(),
                "{FILE}".into(),
            ],
            graders: vec![Grader {
                name: "cargo-test-lib".into(),
                argv: vec![
                    "cargo".into(),
                    "test".into(),
                    "-p".into(),
                    "nedb-engine".into(),
                    "--lib".into(),
                    "--manifest-path".into(),
                    "rust/Cargo.toml".into(),
                ],
                // A shared target dir per worktree would serialise every worker
                // behind cargo's lock, so it is deliberately NOT set here; the
                // operator sets CARGO_TARGET_DIR per worker if they want to skip
                // the cold build.
                env: vec![],
                // cargo takes a lock on the target dir, and each worktree has
                // its own, so concurrent copies are safe.
                hermetic: true,
            }],
            focus: vec![],
            slow: vec![],
        }
    }

    /// nedb, with the numbers measured on 2026-09-10 baked in as comments.
    ///
    /// 11 suites, 7.5 s total, 11/11 green at 5a83bfe. Each is a standalone
    /// script invoked as `python3 tests/<name>.py` with `PYTHONPATH=python` —
    /// NOT pytest, which matters because a pytest invocation silently collects
    /// nothing here and a suite that collects nothing passes.
    pub fn nedb_preset(repo: &str) -> Target {
        let suites = [
            "test_nedb",
            "test_bitemporal",
            "test_causal",
            "test_crypto",
            "test_deploy",
            "test_proof",
            "test_v050",
            "test_wrap_redis",
            "test_adapters",
            "test_wrap_sqlite_shadow",
            "test_concurrent",
        ];
        Target {
            name: "nedb".into(),
            repo: repo.into(),
            sources: vec!["python/nedb".into()],
            exclude: vec![],
            extension: "py".into(),
            locator: vec![
                "python3".into(),
                "locators/python_locate.py".into(),
                "{FILE}".into(),
            ],
            graders: suites
                .iter()
                .map(|s| Grader {
                    name: (*s).into(),
                    argv: vec!["python3".into(), format!("tests/{s}.py")],
                    env: vec![("PYTHONPATH".into(), "{REPO}/python".into())],
                    // test_deploy binds a hardcoded port (7172) and starts a
                    // daemon, so two copies cannot run at once. See
                    // Grader::hermetic — this one field is the difference
                    // between a clean corpus and a poisoned one.
                    hermetic: *s != "test_deploy",
                })
                .collect(),
            focus: vec![
                (
                    "wrap_sqlite".into(),
                    vec![
                        "test_wrap_sqlite_shadow".into(),
                        "test_nedb".into(),
                        "test_adapters".into(),
                    ],
                ),
                (
                    "wrap_".into(),
                    vec![
                        "test_wrap_redis".into(),
                        "test_adapters".into(),
                        "test_nedb".into(),
                    ],
                ),
                (
                    "crypto".into(),
                    vec!["test_crypto".into(), "test_nedb".into()],
                ),
                (
                    "query".into(),
                    vec![
                        "test_nedb".into(),
                        "test_bitemporal".into(),
                        "test_causal".into(),
                    ],
                ),
                (
                    "merkle".into(),
                    vec!["test_proof".into(), "test_nedb".into()],
                ),
                (
                    "proof".into(),
                    vec!["test_proof".into(), "test_nedb".into()],
                ),
                (
                    "concurrent".into(),
                    vec!["test_concurrent".into(), "test_nedb".into()],
                ),
            ],
            // 4.4 s against a 120 ms median. Kept in the full grader, out of the
            // fast path.
            slow: vec!["test_concurrent".into()],
        }
    }
}
