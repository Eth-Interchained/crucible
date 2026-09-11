//! Turn a verified trial into a training row.
//!
//! TWO DECISIONS HERE DO MOST OF THE WORK.
//!
//! **The prompt contains the failing test output, verbatim.** Not "there is a
//! bug in this file, fix it" — that is a puzzle, and a model trained on puzzles
//! learns to guess. A real agent arrives holding a stack trace or a failed
//! assertion, so that is what the prompt holds. This is the difference between
//! training a debugger and training an oracle.
//!
//! **The completion is a sentinel block, not prose.** `<<<PATCH>>>` … `<<<END>>>`
//! is Interchained's own extraction contract (npm/PyPI `sentinel-blocks`), and
//! it exists because content inside sentinels is lifted verbatim by regex and
//! never re-parsed as code — so quotes, braces and newlines in a patch cannot
//! corrupt the surrounding structure the way they corrupt JSON. A model whose
//! output is applicable beats one whose output is readable.
//!
//! The patch is a unified diff of exactly one hunk, because the mutation was one
//! localised splice. That is also why `ast.unparse` is banned in the locator: it
//! reformats a whole file, and the diff would be thousands of noise lines with
//! the real change buried in it.

use crate::model::{Candidate, Example, Trial, Verdict};

/// A minimal unified diff for a single-span replacement.
///
/// Hand-rolled rather than shelling out to `diff` so the row is reproducible
/// byte-for-byte on any host, with no dependency on which diffutils version the
/// box happens to ship.
pub fn unified_hunk(rel_path: &str, original: &str, mutated: &str) -> String {
    let o: Vec<&str> = original.split('\n').collect();
    let m: Vec<&str> = mutated.split('\n').collect();
    // Common prefix / suffix, in lines.
    let mut pre = 0usize;
    while pre < o.len() && pre < m.len() && o[pre] == m[pre] {
        pre += 1;
    }
    let mut suf = 0usize;
    while suf < o.len().saturating_sub(pre)
        && suf < m.len().saturating_sub(pre)
        && o[o.len() - 1 - suf] == m[m.len() - 1 - suf]
    {
        suf += 1;
    }
    let ctx = 3usize;
    let start = pre.saturating_sub(ctx);
    let o_end = (o.len() - suf + ctx).min(o.len());
    let m_end = (m.len() - suf + ctx).min(m.len());

    let mut s = String::new();
    s.push_str(&format!("--- a/{rel_path}\n+++ b/{rel_path}\n"));
    s.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        start + 1,
        o_end - start,
        start + 1,
        m_end - start
    ));
    for l in &o[start..pre] {
        s.push_str(&format!(" {l}\n"));
    }
    for l in &o[pre..o.len() - suf] {
        s.push_str(&format!("-{l}\n"));
    }
    for l in &m[pre..m.len() - suf] {
        s.push_str(&format!("+{l}\n"));
    }
    for l in &o[o.len() - suf..o_end] {
        s.push_str(&format!(" {l}\n"));
    }
    s
}

/// Lines of a file around a point, 1-based, for the prompt's excerpt.
pub fn excerpt(source: &str, line: usize, radius: usize) -> String {
    let lines: Vec<&str> = source.split('\n').collect();
    let lo = line.saturating_sub(radius).max(1);
    let hi = (line + radius).min(lines.len());
    let mut s = String::new();
    for (i, l) in lines[lo - 1..hi].iter().enumerate() {
        let n = lo + i;
        let marker = if n == line { ">" } else { " " };
        s.push_str(&format!("{marker}{n:>5} | {l}\n"));
    }
    s
}

/// Markers that mean "this line is the failure", in the order we trust them.
/// Deliberately explicit rather than clever: a regex that guesses at failure
/// syntax would silently mis-focus on suites whose output we have not seen.
const FAILURE_MARKERS: &[&str] = &[
    "Traceback (most recent call last)",
    "AssertionError",
    "SyntaxError",
    "NameError",
    "TypeError",
    "ValueError",
    "KeyError",
    "AttributeError",
    "ImportError",
    "RuntimeError",
    "error(s)",
    "FAILED",
    "FAIL:",
    // Bare "FAIL " with a space: nedb's deploy suite reports
    // `FAIL daemon started`, which the first version of this list missed —
    // the honest fallback note fired instead, which is exactly how a missing
    // marker is supposed to announce itself.
    " FAIL ",
    "FAIL ",
    "  x  ",
    "✗",
];

/// Cut a grader's output down to the part a human would actually read.
///
/// WHY THIS EXISTS: nedb's suites narrate their successes — one `ok` line per
/// assertion, sixty or more of them — and then report the failure at the end or
/// in a traceback. A naive tail is therefore a wall of passing assertions with
/// the defect missing, and the first corpus this forge produced had prompts
/// whose first 1,600 characters were `ok  SELECT LIMIT`, `ok  INSERT`, … A model
/// trained on that spends its context learning to skip noise.
///
/// Takes a window ending shortly after the LAST failure marker (the last one
/// because a suite can report several and the final one is usually the fatal
/// one) and beginning some lines before it, so the failure has context.
///
/// If NO marker is found the raw tail is returned WITH a note saying so. Never
/// a silent fallback: a prompt that quietly contains the wrong thing is worse
/// than one that admits its own uncertainty, and a rising rate of this note is
/// how an unfamiliar grader announces itself.
pub fn focus_failure(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let hit = lines
        .iter()
        .rposition(|l| FAILURE_MARKERS.iter().any(|m| l.contains(m)));
    match hit {
        Some(i) => {
            let lo = i.saturating_sub(12);
            let hi = (i + 40).min(lines.len());
            let mut s = String::new();
            if lo > 0 {
                s.push_str(&format!(
                    "… {lo} earlier lines of passing output omitted …
"
                ));
            }
            s.push_str(&lines[lo..hi].join(
                "
",
            ));
            s
        }
        None => {
            let lo = lines.len().saturating_sub(40);
            format!(
                "(no recognised failure marker in this suite's output; showing the last                  {} lines verbatim)
{}",
                lines.len() - lo,
                lines[lo..].join("
")
            )
        }
    }
}

/// Rewrite absolute worktree paths to repo-relative.
///
/// TRAINING-DATA POISON OTHERWISE. The grader runs inside a throwaway worktree,
/// so tracebacks name `/…/crucible/work/w0/python/nedb/engine.py` — a directory
/// that will not exist at inference time and has nothing to do with the repo.
/// A model trained on those learns the harness instead of the codebase. Found
/// by reading the first real corpus row rather than by reasoning about it.
pub fn strip_worktree(tail: &str, work_dir: &str, repo_name: &str) -> String {
    // `work_dir` is the PARENT of the per-worker trees, so stripping it alone
    // leaves the worker's own directory name behind as a path prefix:
    // `/…/work/w0/python/nedb/engine.py` became `w0/python/nedb/engine.py`.
    //
    // I SHIPPED THAT AND CALLED IT CLEAN. The check I ran grepped for `/work/`
    // and for `crucible`, and a bare leading `w0/` matches neither — so an
    // incomplete verification reported success on a corpus that still carried
    // harness paths in every row. The fix consumes the work dir AND the segment
    // after it; the test below is the one that would have caught me.
    let mut out = String::with_capacity(tail.len());
    let mut rest = tail;
    let needle = format!("{}/", work_dir.trim_end_matches('/'));
    while let Some(i) = rest.find(needle.as_str()) {
        out.push_str(&rest[..i]);
        let after = &rest[i + needle.len()..];
        // Drop the worker segment too (w0, w1, history, baseline, …).
        rest = match after.find('/') {
            Some(j) => &after[j + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    // Any bare mention of the work dir with no trailing path becomes the repo.
    out.replace(work_dir.trim_end_matches('/'), repo_name)
}

fn describe(v: &Verdict) -> (String, String) {
    match v {
        Verdict::Failed { suite, tail } => (suite.clone(), tail.clone()),
        Verdict::Timeout { suite, limit_ms } => (
            suite.clone(),
            format!(
                "The suite did not terminate. It was killed after {limit_ms} ms \
                 against a healthy baseline far below that, so the defect is a \
                 hang — most likely a loop whose exit condition is now \
                 unreachable — rather than a failed assertion."
            ),
        ),
        Verdict::Survived => (String::new(), String::new()),
    }
}

/// Build the row. `mutated_source` is the broken tree's file; `original_source`
/// is the repaired (pristine) one — the completion restores the second from the
/// first, which is the direction an agent works in.
#[allow(clippy::too_many_arguments)]
pub fn example(
    trial: &Trial,
    rel_path: &str,
    original_source: &str,
    mutated_source: &str,
    repo_commit: &str,
    repo_name: &str,
    worktree_root: &str,
) -> Option<Example> {
    if !trial.verdict.killed() {
        return None;
    }
    let (suite, tail) = describe(&trial.verdict);
    let tail = focus_failure(&strip_worktree(&tail, worktree_root, repo_name));
    let c: &Candidate = &trial.candidate;

    let prompt = format!(
        "Repository: {repo}\nCommit: {commit}\n\nThe test suite `{suite}` is failing.\n\n\
         ── what the suite reported ──\n{tail}\n\n\
         ── the file it points into: {path} ──\n{excerpt}\n\
         Find the defect and repair it. Reply with ONLY a unified diff inside a \
         sentinel block:\n\n<<<PATCH>>>\n--- a/<path>\n+++ b/<path>\n@@ ... @@\n...\n<<<END>>>\n",
        repo = repo_name,
        commit = &repo_commit[..repo_commit.len().min(9)],
        suite = suite,
        tail = tail.trim(),
        path = rel_path,
        excerpt = excerpt(mutated_source, c.line, 14),
    );

    let diff = unified_hunk(rel_path, mutated_source, original_source);
    let completion = format!("<<<PATCH>>>\n{diff}<<<END>>>\n");

    Some(Example {
        id: trial.id.clone(),
        prompt,
        completion,
        operator: c.operator.clone(),
        severity: c.severity.clone(),
        file: rel_path.to_string(),
        line: c.line,
        scope: c.scope.clone(),
        verdict: trial.verdict.label().to_string(),
        // Provenance, the way nedb-cast-slm chained its training lineage:
        // every row points at the trial that produced it, and the trial points
        // at the exact tree it was cut against. `TRACE caused_by` over these
        // gives the full ancestry of any example in the corpus.
        caused_by: vec![format!("trial:{}", trial.id), format!("tree:{repo_commit}")],
        repo_commit: repo_commit.to_string(),
    })
}

/// A training row from a REAL fix in git history.
///
/// Same shape as the mutation rows on purpose — a trainer should not be able to
/// tell them apart from the format, only from `source`. The difference that
/// matters is upstream: this defect was shipped by a human and repaired by a
/// human, so the diff can span several lines and several hunks, where a
/// mutation repair is always one splice.
#[allow(clippy::too_many_arguments)]
pub fn history_example(
    sha: &str,
    parent: &str,
    suite: &str,
    tail: &str,
    rel_path: &str,
    broken: &str,
    fixed: &str,
    line: usize,
    repo_name: &str,
    worktree_root: &str,
) -> Example {
    let tail = focus_failure(&strip_worktree(tail, worktree_root, repo_name));
    let prompt = format!(
        "Repository: {repo_name}\nCommit: {parent_short}\n\nThe test suite `{suite}` is failing.\n\n\
         ── what the suite reported ──\n{tail}\n\n\
         ── the file it points into: {rel_path} ──\n{excerpt}\n\
         Find the defect and repair it. Reply with ONLY a unified diff inside a \
         sentinel block:\n\n<<<PATCH>>>\n--- a/<path>\n+++ b/<path>\n@@ ... @@\n...\n<<<END>>>\n",
        parent_short = &parent[..parent.len().min(9)],
        tail = tail.trim(),
        excerpt = excerpt(broken, line, 14),
    );
    let diff = unified_hunk(rel_path, broken, fixed);
    Example {
        id: format!("hist:{}:{}", &sha[..sha.len().min(12)], rel_path),
        prompt,
        completion: format!("<<<PATCH>>>\n{diff}<<<END>>>\n"),
        operator: "history".into(),
        severity: "real".into(),
        file: rel_path.to_string(),
        line,
        scope: String::new(),
        verdict: "FIXED".into(),
        // The ancestry of a mined row is the commit pair itself.
        caused_by: vec![format!("commit:{sha}"), format!("parent:{parent}")],
        repo_commit: sha.to_string(),
    }
}

/// A row from an EMERGENT defect — two sites, each proven undetectable alone.
///
/// The prompt says there are two, and says it plainly. Hiding that would make
/// this indistinguishable from a single-site row while the label reverts two
/// places, and a model that finds one site and stops would look wrong when it
/// was reasoning correctly about the information it was given.
#[allow(clippy::too_many_arguments)]
pub fn pair_example(
    rel_path: &str,
    suite: &str,
    tail: &str,
    broken: &str,
    fixed: &str,
    line_a: usize,
    line_b: usize,
    operator: &str,
    affinity: &str,
    repo_name: &str,
    repo_commit: &str,
    work_dir: &str,
) -> Example {
    let tail = focus_failure(&strip_worktree(tail, work_dir, repo_name));
    let (lo, hi) = if line_a <= line_b {
        (line_a, line_b)
    } else {
        (line_b, line_a)
    };
    // One excerpt when the sites are close, two when they are far apart —
    // otherwise a 400-line span would be pasted to show two lines.
    let body = if hi - lo <= 24 {
        excerpt(broken, (lo + hi) / 2, ((hi - lo) / 2) + 8)
    } else {
        format!(
            "{}\n        ⋮\n{}",
            excerpt(broken, lo, 8),
            excerpt(broken, hi, 8)
        )
    };
    let prompt = format!(
        "Repository: {repo_name}\nCommit: {commit}\n\nThe test suite `{suite}` is failing.\n\n\
         ── what the suite reported ──\n{tail}\n\n\
         ── the file it points into: {rel_path} ──\n{body}\n\
         There are TWO defects in this file. The failing output above may only \
         point at one of them. Repair both. Reply with ONLY a unified diff inside \
         a sentinel block:\n\n\
         <<<PATCH>>>\n--- a/<path>\n+++ b/<path>\n@@ ... @@\n...\n<<<END>>>\n",
        commit = &repo_commit[..repo_commit.len().min(9)],
        tail = tail.trim(),
    );
    let diff = unified_hunk(rel_path, broken, fixed);
    Example {
        id: format!("pair:{rel_path}:{lo}:{hi}"),
        prompt,
        completion: format!("<<<PATCH>>>\n{diff}<<<END>>>\n"),
        operator: operator.to_string(),
        severity: format!("emergent/{affinity}"),
        file: rel_path.to_string(),
        line: lo,
        scope: String::new(),
        verdict: "EMERGENT".into(),
        caused_by: vec![
            format!("site:{rel_path}:{lo}"),
            format!("site:{rel_path}:{hi}"),
            format!("tree:{repo_commit}"),
        ],
        repo_commit: repo_commit.to_string(),
    }
}
