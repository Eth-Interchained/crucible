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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    /// Path to the git repo. Never written to — every mutation goes into a
    /// worktree cut from it.
    pub repo: String,
    /// Glob-free, explicit: directories to walk for mutable source.
    pub sources: Vec<String>,
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
