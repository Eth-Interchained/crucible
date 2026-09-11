//! Drive a language locator and read its candidates.
//!
//! The forge never parses a target language itself. It shells out to whatever
//! owns the truth about that language's syntax — CPython's `ast` for Python,
//! `syn` for Rust — and consumes JSON with absolute byte offsets. That boundary
//! is the reason this tool can mutate a Python engine and a Rust core with one
//! scheduler, and the reason a mutation is never spliced at an offset some
//! third-party grammar reimplementation guessed at.

use crate::model::LocateReport;
use std::path::Path;
use std::process::Command;

pub fn locate(locator: &[String], cwd: &Path, file: &Path) -> Result<LocateReport, String> {
    if locator.is_empty() {
        return Err("locator argv is empty".into());
    }
    let args: Vec<String> = locator[1..]
        .iter()
        .map(|a| a.replace("{FILE}", &file.display().to_string()))
        .collect();
    let out = Command::new(&locator[0])
        .args(&args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("spawn {}: {e}", locator[0]))?;
    if !out.status.success() {
        // Locator stderr is the diagnosis, so it is carried into the error
        // rather than discarded for a tidy message.
        return Err(format!(
            "locator exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("(no stderr)")
        ));
    }
    serde_json::from_slice::<LocateReport>(&out.stdout).map_err(|e| {
        format!(
            "locator emitted unparseable JSON ({e}); first 160 bytes: {:?}",
            String::from_utf8_lossy(&out.stdout)
                .chars()
                .take(160)
                .collect::<String>()
        )
    })
}

/// Walk a source directory for files the locator handles.
///
/// Skips the obvious non-source: caches, VCS internals, build output, and
/// anything under a `tests/` path — mutating the grader itself would produce a
/// tautology (the test changes, so of course it fails).
pub fn discover(root: &Path, dirs: &[String], ext: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        walk(&root.join(d), ext, &mut out);
    }
    out.sort();
    out
}

fn walk(dir: &Path, ext: &str, out: &mut Vec<std::path::PathBuf>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.')
            || name == "__pycache__"
            || name == "target"
            || name == "node_modules"
            || name == "tests"
            || name == "test"
        {
            continue;
        }
        if p.is_dir() {
            walk(&p, ext, out);
        } else if p.extension().map(|e| e == ext).unwrap_or(false) {
            out.push(p);
        }
    }
}
