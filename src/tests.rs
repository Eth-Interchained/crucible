//! Tests for the parts where being wrong is silent.
//!
//! The bias here is deliberate: every test below targets something that, if
//! broken, would produce a corpus that LOOKS fine. A mangled splice, a diff with
//! the wrong line numbers, a prompt carrying harness paths — none of those throw.
//! They just quietly teach a model the wrong thing.

use crate::render::{excerpt, focus_failure, strip_worktree, unified_hunk};
use crate::target::Target;

/// Count edit lines in a unified diff, EXCLUDING the `---`/`+++` file headers.
///
/// The first version of these tests counted `"\n-"` and `"\n+"` across the whole
/// diff, which also matched the `+++ b/x.py` header and so reported one phantom
/// addition on EVERY diff — including on an unchanged file. Two tests failed and
/// the generator was fine; the assertion was wrong. Kept as a named helper so
/// the next test cannot repeat the mistake.
fn count_edits(diff: &str) -> (usize, usize) {
    let mut minus = 0;
    let mut plus = 0;
    for l in diff.lines() {
        if l.starts_with("---") || l.starts_with("+++") || l.starts_with("@@") {
            continue;
        }
        if l.starts_with('-') {
            minus += 1;
        } else if l.starts_with('+') {
            plus += 1;
        }
    }
    (minus, plus)
}

#[test]
fn a_hunk_is_minimal_and_correctly_numbered() {
    // The whole reason `ast.unparse` is banned in the locator: the diff must
    // contain the one changed line, not a reformatted file. If this ever shows a
    // growing hunk, the corpus has started teaching noise.
    let orig = "a\nb\nc\nd\ne\nf\ng\n";
    let mutd = "a\nb\nc\nD\ne\nf\ng\n";
    let d = unified_hunk("x.py", orig, mutd);
    assert!(d.starts_with("--- a/x.py\n+++ b/x.py\n"), "got:\n{d}");
    assert_eq!(count_edits(&d), (1, 1), "exactly one line each way:\n{d}");
    assert!(d.contains("-d\n") && d.contains("+D\n"), "{d}");
    // 3 lines of context before line 4 => the hunk starts at line 1.
    assert!(d.contains("@@ -1,"), "hunk header should start at 1:\n{d}");
}

#[test]
fn a_hunk_of_an_unchanged_file_has_no_edits() {
    let s = "same\nlines\nhere\n";
    let d = unified_hunk("x.py", s, s);
    assert_eq!(count_edits(&d), (0, 0), "{d}");
}

#[test]
fn focus_lands_on_the_failure_not_the_sixty_ok_lines() {
    // THE REGRESSION THIS EXISTS FOR. nedb's suites narrate every success, so a
    // naive tail is a wall of passing assertions with the defect scrolled off.
    // The first corpus this forge produced had prompts whose opening 1,600
    // characters were `ok  SELECT LIMIT`, `ok  INSERT`, ...
    let mut out = String::new();
    for i in 0..80 {
        out.push_str(&format!("  ok  assertion number {i}\n"));
    }
    out.push_str("Traceback (most recent call last)\n  File \"x.py\", line 3\nValueError: nope\n");
    let f = focus_failure(&out);
    assert!(
        f.contains("ValueError: nope"),
        "must keep the failure:\n{f}"
    );
    let kept = f.matches("  ok  ").count();
    assert!(
        kept < 20,
        "must drop most of the passing log, kept {kept}:\n{f}"
    );
    assert!(f.contains("earlier lines of passing output omitted"));
}

#[test]
fn an_unrecognised_failure_format_says_so_rather_than_guessing() {
    // A silent fallback would put the wrong thing in a prompt and look identical
    // to a correct one. A rising rate of this note is how an unfamiliar grader
    // announces itself.
    let f = focus_failure("something\nwent\nsideways\n");
    assert!(f.contains("no recognised failure marker"), "{f}");
    assert!(
        f.contains("sideways"),
        "raw tail must still be present:\n{f}"
    );
}

#[test]
fn the_deploy_suites_bare_fail_is_recognised() {
    // Found by reading a real row: the marker list had "FAILED" and "FAIL:" but
    // nedb's deploy suite prints `FAIL daemon started`, so the honest fallback
    // note fired instead — which is exactly how a missing marker should behave,
    // and exactly the signal that told me to add this one.
    let f = focus_failure("  ok  one\n  FAIL daemon started\n  daemon did not start\n");
    assert!(!f.contains("no recognised failure marker"), "{f}");
}

#[test]
fn harness_paths_never_reach_a_prompt() {
    // Training-data poison: the grader runs in a throwaway worktree, so
    // tracebacks name a directory that will not exist at inference time.
    //
    // The second argument is the WORK DIR — the parent of the per-worker trees,
    // which is what the call sites have. An earlier version of this test passed
    // the worktree root itself, which is why the worker-segment leak below went
    // unnoticed: the test agreed with the bug.
    let tail = "File \"/tmp/crucible/work/w3/python/nedb/engine.py\", line 44, in put";
    let s = strip_worktree(tail, "/tmp/crucible/work", "nedb");
    assert!(!s.contains("/tmp/crucible"), "{s}");
    assert!(!s.contains("w3/"), "{s}");
    assert!(s.contains("python/nedb/engine.py"), "{s}");
}

#[test]
fn the_worker_directory_name_is_stripped_too() {
    // THE TEST THAT WOULD HAVE CAUGHT ME. `strip_worktree` originally removed
    // only the work dir, leaving the per-worker segment as a path prefix, and
    // the verification I ran grepped for "/work/" and "crucible" — neither of
    // which matches a bare leading "w0/". I reported the corpus clean while
    // every row still carried a harness path.
    for worker in ["w0", "w11", "history", "baseline"] {
        let tail = format!(
            "File \"/agent/work/{worker}/python/nedb/engine.py\", line 44, in put\n  \
             File \"/agent/work/{worker}/tests/test_nedb.py\", line 9, in <module>"
        );
        let s = strip_worktree(&tail, "/agent/work", "nedb");
        assert!(
            s.contains("python/nedb/engine.py") && s.contains("tests/test_nedb.py"),
            "paths must survive, got: {s}"
        );
        assert!(
            !s.contains(&format!("{worker}/")),
            "worker segment `{worker}/` survived: {s}"
        );
        assert!(!s.contains("/agent/work"), "work dir survived: {s}");
    }
}

#[test]
fn a_trailing_slash_on_the_work_dir_changes_nothing() {
    let tail = "at /w/w0/python/nedb/log.py:12";
    assert_eq!(
        strip_worktree(tail, "/w", "nedb"),
        strip_worktree(tail, "/w/", "nedb")
    );
}

#[test]
fn the_excerpt_marks_the_defect_line() {
    let src = (1..=30)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let e = excerpt(&src, 10, 3);
    assert!(e.contains(">   10 | line 10"), "marker missing:\n{e}");
    assert!(e.contains("     7 | line 7"), "{e}");
    assert!(!e.contains("line 14"), "radius should exclude 14:\n{e}");
}

#[test]
fn excerpt_near_the_top_of_a_file_does_not_underflow() {
    let e = excerpt("a\nb\nc\n", 1, 14);
    assert!(e.contains(">    1 | a"), "{e}");
}

#[test]
fn focused_graders_are_a_subset_and_the_slow_one_is_out_of_the_fast_path() {
    // Measured on nedb 2026-09-10: broadening the grader from 3 suites to 11
    // killed 0 of 10 survivors, so the focused subset is the same kill power at
    // a quarter of the cost — not a heuristic shortcut.
    let t = Target::nedb_preset("/nowhere");
    let f = t.graders_for("python/nedb/wrap_sqlite.py");
    let names: Vec<&str> = f.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["test_wrap_sqlite_shadow", "test_nedb", "test_adapters"]
    );
    // A file with no focus entry falls back to everything EXCEPT the slow one.
    let fb = t.graders_for("python/nedb/autoindex.py");
    assert!(
        !fb.iter().any(|g| g.name == "test_concurrent"),
        "test_concurrent is 4.4s against a 120ms median; it belongs to the full \
         grader, not the fast path"
    );
    // But it is still in the full grader — excluded for cost, never dropped.
    assert!(t.all_graders().iter().any(|g| g.name == "test_concurrent"));
}

#[test]
fn every_focus_entry_names_a_grader_that_exists() {
    // A typo here would silently fall through to the full grader: correct
    // results, four times the cost, and nothing in the output saying why.
    let t = Target::nedb_preset("/nowhere");
    for (needle, names) in &t.focus {
        for n in names {
            assert!(
                t.graders.iter().any(|g| &g.name == n),
                "focus `{needle}` names `{n}`, which is not a grader on this target"
            );
        }
    }
    for n in &t.slow {
        assert!(
            t.graders.iter().any(|g| &g.name == n),
            "slow list names `{n}`, which is not a grader on this target"
        );
    }
}

#[test]
fn a_grader_env_var_resolves_the_repo_placeholder() {
    // PYTHONPATH must be absolute inside a throwaway worktree, so `{REPO}` is
    // expanded at spawn time. If this ever stops expanding, every Python suite
    // imports the SYSTEM nedb instead of the mutated one — and then every
    // mutation "survives", which reads as a well-tested repo rather than as a
    // broken harness. That is the most dangerous silent failure in this tool.
    let t = Target::nedb_preset("/nowhere");
    let g = &t.graders[0];
    let (k, v) = &g.env[0];
    assert_eq!(k, "PYTHONPATH");
    assert!(v.contains("{REPO}"), "got {v}");
    assert_eq!(v.replace("{REPO}", "/wt"), "/wt/python");
}

#[test]
fn a_missing_grader_is_not_a_failing_grader() {
    // THE MISDIAGNOSIS THIS PREVENTS. A Rust run reported "the grader is not
    // green on an unmutated tree" after exiting in 1 ms. The suite was green —
    // 71 tests, 0.31 s — and `cargo` simply was not on PATH for that
    // invocation. "Your tests are failing" and "your test command does not
    // exist" are different problems with different fixes, and the first message
    // sends you to read the wrong code.
    let g = crate::target::Grader {
        name: "ghost".into(),
        argv: vec!["definitely-not-a-real-binary-xyzzy".into()],
        env: vec![],
    };
    let r = crate::verify::run_grader(std::path::Path::new("."), &g, 3_000)
        .expect("timeout itself must run");
    assert!(!r.ok);
    assert!(
        r.not_executable,
        "exit {:?} after {} ms should be classified as not-executable",
        127, r.elapsed_ms
    );
    assert!(!r.timed_out, "a missing binary is not a timeout");

    // And the baseline must refuse with a message that names the real cause.
    let err =
        crate::verify::measure_baseline(std::path::Path::new("."), &[&g], 3_000, |_, _, _, _| {})
            .expect_err("must refuse");
    assert!(err.contains("not found or is not executable"), "{err}");
    assert!(
        !err.contains("is not green on an unmutated tree"),
        "must NOT blame the target's tests: {err}"
    );
}
