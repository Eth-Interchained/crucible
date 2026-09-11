//! crucible — CLI.
//!
//!   crucible forge --repo PATH [--workers N] [--limit N] [--operator NAME]
//!                  [--out DIR] [--work DIR]
//!   crucible locate --repo PATH [--file REL]
//!   crucible operators
//!
//! `forge` is the whole product: break a green tree thousands of times, keep
//! only what a real suite proved was detectable, and report the rest as a
//! coverage finding.

use crucible::{eval, flair, forge, history, locate, pairs, render, report, target::Target};
use std::path::PathBuf;
use std::process::ExitCode;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| {
            let p = format!("{name}=");
            args.iter()
                .find_map(|a| a.strip_prefix(&p).map(str::to_string))
        })
}

fn main() -> ExitCode {
    flair::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("forge") => cmd_forge(&args[1..]),
        Some("locate") => cmd_locate(&args[1..]),
        Some("history") => cmd_history(&args[1..]),
        Some("pairs") => cmd_pairs(&args[1..]),
        Some("eval") => cmd_eval(&args[1..]),
        Some("operators") => cmd_operators(),
        _ => {
            eprintln!(
                "crucible — execution-gated corpus forge\n\n\
                 usage:\n\
                 \x20 crucible forge --repo PATH [--target nedb|nedb-rust] [--workers N]\n\
                 \x20                [--limit N] [--operator OP] [--out DIR] [--work DIR]\n\
                 \x20 crucible history --repo PATH [--limit N] [--out DIR] [--work DIR]\n\
                 \x20 crucible pairs --repo PATH --trials FILE [--limit N] [--seed N]\n\
                 \x20 crucible eval --repo PATH --corpus FILE [--model oracle|null|cheat|teacher]\n\
                 \x20                [--teacher-model NAME] [--endpoint URL]\n\
                 \x20                [--teacher-max-tokens N] [--transcript FILE]\n\
                 \x20 crucible locate --repo PATH [--file REL]\n\
                 \x20 crucible operators\n"
            );
            return ExitCode::from(2);
        }
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\n{} {e}", flair::red("crucible failed:"));
            ExitCode::FAILURE
        }
    }
}

/// `crucible history` — mine VERIFIED fixes out of git history.
///
/// A commit is a bug fix if the suite says so: red at the parent, green at the
/// commit. No commit-message heuristics — "fix:" in a subject is a claim, a
/// suite going red-to-green is a verdict.
fn cmd_history(args: &[String]) -> Result<(), String> {
    let repo = flag(args, "--repo").ok_or("--repo is required")?;
    let repo = PathBuf::from(&repo)
        .canonicalize()
        .map_err(|e| format!("canonicalize --repo {repo}: {e}"))?;
    let limit: usize = flag(args, "--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let out_dir = PathBuf::from(flag(args, "--out").unwrap_or_else(|| "out".into()));
    let work_dir = PathBuf::from(flag(args, "--work").unwrap_or_else(|| "work".into()));
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("mkdir work: {e}"))?;
    let work_dir = work_dir.canonicalize().map_err(|e| e.to_string())?;

    let t = Target::nedb_preset(&repo.display().to_string());
    let head = crucible::worktree::head_commit(&repo)?;
    let cands = history::candidates(&repo, &t.sources, &t.extension, limit)?;

    flair::banner(&t.name, &head, cands.len(), t.graders.len(), 1);
    flair::phase(
        "HISTORY",
        &format!(
            "{} non-merge commits touching {}/*.{} · newest first",
            cands.len(),
            t.sources.join(","),
            t.extension
        ),
    );
    if cands.is_empty() {
        return Err("no candidate commits — check --repo and that history is not shallow".into());
    }

    // The baseline is measured at HEAD, where the suite is known good, and its
    // deadlines are reused for every historical replay. Measuring per-commit
    // would let a commit whose suite is legitimately slow set its own generous
    // deadline and then never be called hung.
    flair::phase(
        "BASELINE",
        "measured at HEAD; deadlines reused for every replay",
    );
    let probe = crucible::worktree::Worktree::create(&repo, &work_dir, "hbase", &head)?;
    let all = t.all_graders();
    let base = crucible::verify::measure_baseline(&probe.root, &all, 120_000, |n, ms, d, ok| {
        flair::baseline(n, ms, d, ok);
    })?;
    drop(probe);

    flair::phase(
        "REPLAY",
        "parent red + commit green = a fix nobody had to label",
    );
    let wt = crucible::worktree::Worktree::create(&repo, &work_dir, "history", &head)?;
    let started = std::time::Instant::now();
    let mut replays: Vec<history::Replay> = Vec::new();
    for (i, c) in cands.iter().enumerate() {
        let (outcome, ms) = history::replay(&wt, &t, &base, c)?;
        let tag = match &outcome {
            history::Outcome::Fixed { suite, .. } => format!("repaired {suite}"),
            history::Outcome::GreenParent => "suite was already green".into(),
            history::Outcome::StillRed { suite } => {
                format!("{suite} red before and after — suite cannot grade this era")
            }
            history::Outcome::NotYetWritten { suites } => {
                format!("{} did not exist yet at this commit", suites.join(","))
            }
            history::Outcome::Unrunnable { why } => why.clone(),
        };
        flair::trial(
            i + 1,
            cands.len(),
            match outcome.label() {
                "FIXED" => "FAILED",
                "UNRUNNABLE" => "ABORT",
                _ => "SURVIVED",
            },
            &c.sha[..9],
            if outcome.usable() { "silent" } else { "loud" },
            &c.touched.first().cloned().unwrap_or_default(),
            c.touched.len(),
            &c.subject.chars().take(38).collect::<String>(),
            ms,
            &tag,
        );
        replays.push(history::Replay {
            commit: c.clone(),
            outcome,
            elapsed_ms: ms,
        });
    }
    // Leave the worktree back at HEAD so a later `git worktree list` is boring.
    let _ = wt.checkout(&head);
    drop(wt);

    let assay = history::HistoryAssay {
        replays,
        wall_ms: started.elapsed().as_millis() as u64,
    };

    // ---- rows -------------------------------------------------------------
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let mut rows = 0usize;
    {
        use std::io::Write;
        let mut fh =
            std::fs::File::create(out_dir.join("history.jsonl")).map_err(|e| e.to_string())?;
        for r in &assay.replays {
            let history::Outcome::Fixed { suite, tail } = &r.outcome else {
                continue;
            };
            for path in &r.commit.touched {
                let broken = match history::file_at(&repo, &r.commit.parent, path) {
                    Ok(s) => s,
                    Err(e) => {
                        // A file ADDED by this commit has no parent version:
                        // there is no "before" to repair, so it is not a row.
                        // Said out loud rather than skipped in silence.
                        flair::note(&format!(
                            "{}: no parent version of {path} ({e})",
                            &r.commit.sha[..9]
                        ));
                        continue;
                    }
                };
                let fixed = match history::file_at(&repo, &r.commit.sha, path) {
                    Ok(s) => s,
                    Err(e) => {
                        flair::note(&format!(
                            "{}: {path} not readable at commit ({e})",
                            &r.commit.sha[..9]
                        ));
                        continue;
                    }
                };
                if broken == fixed {
                    continue;
                }
                let line = history::touched_line(&repo, &r.commit.sha, path);
                let ex = render::history_example(
                    &r.commit.sha,
                    &r.commit.parent,
                    suite,
                    tail,
                    path,
                    &broken,
                    &fixed,
                    line,
                    &t.name,
                    &work_dir.display().to_string(),
                );
                writeln!(
                    fh,
                    "{}",
                    serde_json::to_string(&ex).map_err(|e| e.to_string())?
                )
                .map_err(|e| e.to_string())?;
                rows += 1;
            }
        }
    }
    std::fs::write(
        out_dir.join("replays.json"),
        serde_json::to_string_pretty(&assay.replays).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    flair::phase("ASSAY", "what the history actually contains");
    let n = assay.replays.len();
    for (label, note) in [
        ("FIXED", "parent red, commit green — a verified repair"),
        ("GREEN-BEFORE", "no test could see the change"),
        ("STILL-RED", "suite cannot grade that point in history"),
        (
            "NO-GRADER-YET",
            "the focused suite had not been written yet",
        ),
        ("UNRUNNABLE", "could not be replayed at all"),
    ] {
        let c = assay.count(label);
        println!(
            "  {:<14} {:>4}  {:>5.1}%  {}",
            if label == "FIXED" {
                flair::green(label)
            } else {
                flair::dim(label)
            },
            c,
            c as f64 * 100.0 / n.max(1) as f64,
            flair::dim(note)
        );
    }
    println!(
        "  {:<14} {}",
        "rows written",
        flair::bold(&rows.to_string())
    );
    println!(
        "  {:<14} {}",
        "wall clock",
        flair::dim(&format!("{:.1}s", assay.wall_ms as f64 / 1000.0))
    );
    println!(
        "\n  {} {}",
        flair::ember("▸"),
        flair::dim(&format!(
            "rows: {}/history.jsonl · every replay: {}/replays.json",
            out_dir.display(),
            out_dir.display()
        ))
    );
    Ok(())
}

/// `crucible pairs` — mine EMERGENT defects out of the survivors.
///
/// Two mutations that each survive alone but kill together are a defect neither
/// site is individually blameable for. Single-mutation corpora cannot produce
/// that class, and it is the class real bugs live in. It also turns the ~65% of
/// every forge run that was previously discarded into the largest data source
/// available.
fn cmd_pairs(args: &[String]) -> Result<(), String> {
    let repo = flag(args, "--repo").ok_or("--repo is required")?;
    let repo = PathBuf::from(&repo)
        .canonicalize()
        .map_err(|e| format!("canonicalize --repo {repo}: {e}"))?;
    let trials_path = flag(args, "--trials").ok_or(
        "--trials FILE is required — run `crucible forge` first; its trials.json is the feedstock",
    )?;
    let limit: usize = flag(args, "--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let seed: u64 = flag(args, "--seed")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let cross: usize = flag(args, "--cross-file-pct")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let out_dir = PathBuf::from(flag(args, "--out").unwrap_or_else(|| "out".into()));
    let work_dir = PathBuf::from(flag(args, "--work").unwrap_or_else(|| "work".into()));
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("mkdir work: {e}"))?;
    let work_dir = work_dir.canonicalize().map_err(|e| e.to_string())?;

    let raw =
        std::fs::read_to_string(&trials_path).map_err(|e| format!("read {trials_path}: {e}"))?;
    let trials: Vec<crucible::model::Trial> =
        serde_json::from_str(&raw).map_err(|e| format!("parse {trials_path}: {e}"))?;
    let surv = pairs::survivors(&trials).len();
    let mode = match flag(args, "--mode").as_deref() {
        Some("survivor-pair") => pairs::Mode::SurvivorPair,
        // Default to the mode the measurement favours. Survivor-pair yielded
        // 2.5% on nedb; killer+survivor is red by construction.
        _ => pairs::Mode::KillerPlusSurvivor,
    };
    let picked = pairs::sample(&trials, limit, seed, cross, mode);

    let t = Target::nedb_preset(&repo.display().to_string());
    let head = crucible::worktree::head_commit(&repo)?;
    flair::banner(&t.name, &head, trials.len(), t.graders.len(), 1);
    flair::phase(
        "PAIRS",
        &format!(
            "{} mode · {surv} survivors + {} killers in {} trials · {} pairs (seed {seed})",
            if mode == pairs::Mode::SurvivorPair {
                "survivor-pair"
            } else {
                "killer+survivor"
            },
            pairs::killers(&trials).len(),
            trials.len(),
            picked.len()
        ),
    );
    if picked.is_empty() {
        return Err(format!(
            "no pairs could be sampled from {surv} survivors — run a wider forge first"
        ));
    }
    match mode {
        pairs::Mode::SurvivorPair => flair::note(
            "both sites ALREADY survived alone, proven by execution — a kill below is \
             emergent: neither line is individually blameable.",
        ),
        pairs::Mode::KillerPlusSurvivor => flair::note(
            "one site is a proven killer, the other a proven survivor — the failing \
             output will point at ONE site while TWO need repairing.",
        ),
    }

    flair::phase(
        "BASELINE",
        "the grader must be green before anything is broken",
    );
    let probe = crucible::worktree::Worktree::create(&repo, &work_dir, "pbase", &head)?;
    let all = t.all_graders();
    let base = crucible::verify::measure_baseline(&probe.root, &all, 120_000, |n, ms, d, ok| {
        flair::baseline(n, ms, d, ok);
    })?;
    drop(probe);

    flair::phase("FORGE", "two undetectable changes at once");
    let wt = crucible::worktree::Worktree::create(&repo, &work_dir, "pairs", &head)?;
    let started = std::time::Instant::now();
    let mut killed = 0usize;
    let mut rows = 0usize;
    let mut by_aff: std::collections::BTreeMap<&str, (usize, usize)> =
        std::collections::BTreeMap::new();
    use std::io::Write;
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let mut fh = std::fs::File::create(out_dir.join("pairs.jsonl")).map_err(|e| e.to_string())?;

    for (i, p) in picked.iter().enumerate() {
        let same_file = p.a_file == p.b_file;
        let a_abs = wt.root.join(&p.a_file);
        let b_abs = wt.root.join(&p.b_file);
        let a_orig = std::fs::read(&a_abs).map_err(|e| format!("read {}: {e}", p.a_file))?;
        let b_orig = if same_file {
            a_orig.clone()
        } else {
            std::fs::read(&b_abs).map_err(|e| format!("read {}: {e}", p.b_file))?
        };

        let write_res = if same_file {
            match pairs::apply_same_file(&a_orig, &p.a, &p.b) {
                Ok(m) => std::fs::write(&a_abs, &m).map_err(|e| e.to_string()),
                Err(e) => Err(e),
            }
        } else {
            pairs::apply_one(&a_orig, &p.a)
                .and_then(|m| std::fs::write(&a_abs, &m).map_err(|e| e.to_string()))
                .and_then(|_| pairs::apply_one(&b_orig, &p.b))
                .and_then(|m| std::fs::write(&b_abs, &m).map_err(|e| e.to_string()))
        };
        if let Err(e) = write_res {
            // Overlapping sites and off-the-end sites land here. Named, never
            // a silent skip.
            flair::abort(&p.a_file, &p.a.operator, p.a.line, &e);
            let _ = wt.restore(std::path::Path::new(&p.a_file));
            if !same_file {
                let _ = wt.restore(std::path::Path::new(&p.b_file));
            }
            continue;
        }

        // Grade with the union of both sites' focused graders.
        let mut names: Vec<String> = Vec::new();
        for f in [&p.a_file, &p.b_file] {
            for g in t.graders_for(f) {
                if !names.contains(&g.name) {
                    names.push(g.name.clone());
                }
            }
        }
        let graders: Vec<&crucible::target::Grader> = names
            .iter()
            .filter_map(|n| t.graders.iter().find(|g| &g.name == n))
            .collect();

        let t0 = std::time::Instant::now();
        let verdict = crucible::verify::grade(&wt.root, &graders, &base);
        let ms = t0.elapsed().as_millis() as u64;
        let _ = wt.restore(std::path::Path::new(&p.a_file));
        if !same_file {
            let _ = wt.restore(std::path::Path::new(&p.b_file));
        }
        let verdict = verdict?;

        let e = by_aff.entry(p.affinity.label()).or_insert((0, 0));
        e.1 += 1;
        if verdict.killed() {
            e.0 += 1;
            killed += 1;
        }

        flair::trial(
            i + 1,
            picked.len(),
            verdict.label(),
            &format!("{}+{}", p.a.operator, p.b.operator),
            // This slot is a SEVERITY, and an affinity is not one — passing
            // `same-scope` here made every pair render as "loud", because
            // flair::trial only recognises "silent". A two-site defect is
            // silent if either of its sites is. The affinity is already in the
            // per-affinity table below, where it belongs.
            if p.a.severity == "silent" || p.b.severity == "silent" {
                "silent"
            } else {
                "loud"
            },
            &p.a_file,
            p.a.line,
            &p.a.scope,
            ms,
            &match &verdict {
                crucible::model::Verdict::Failed { suite, .. } => {
                    format!("two-site — caught by {suite}")
                }
                crucible::model::Verdict::Timeout { suite, .. } => format!("{suite} hung"),
                // The pair path does not confirm kills yet — only the single
                // forge does. Stated rather than papered over: a Flaky here
                // would be a bug in this arm, not a real verdict, so it says so
                // instead of pretending to be a category it cannot produce.
                crucible::model::Verdict::Flaky { suite } => {
                    format!("{suite} red on a clean tree too — pair verdict unreliable")
                }
                crucible::model::Verdict::Survived => {
                    format!("both still invisible (L{})", p.b.line)
                }
            },
        );

        if let crucible::model::Verdict::Failed { suite, tail } = &verdict {
            // Row: the broken side has BOTH sites applied; the repair restores
            // both. Only emitted for same-file pairs for now — a cross-file
            // repair needs a multi-file diff, which the row format does not yet
            // carry, and emitting a single-file diff for it would be a LABEL
            // THAT DOES NOT RESTORE GREEN. Said out loud rather than shipped.
            if same_file {
                let pristine = String::from_utf8_lossy(&a_orig).to_string();
                let broken = String::from_utf8_lossy(&pairs::apply_same_file(&a_orig, &p.a, &p.b)?)
                    .to_string();
                let ex = render::pair_example(
                    &p.a_file,
                    suite,
                    tail,
                    &broken,
                    &pristine,
                    p.a.line,
                    p.b.line,
                    &format!("{}+{}", p.a.operator, p.b.operator),
                    p.affinity.label(),
                    &t.name,
                    &head,
                    &work_dir.display().to_string(),
                );
                writeln!(
                    fh,
                    "{}",
                    serde_json::to_string(&ex).map_err(|e| e.to_string())?
                )
                .map_err(|e| e.to_string())?;
                rows += 1;
            } else {
                flair::note(
                    "cross-file kill not written as a row: the repair spans two files and \
                     the row format carries one diff",
                );
            }
        }
    }
    let _ = wt.checkout(&head);
    drop(wt);

    // THE LABEL MUST MATCH THE MODE. An earlier version printed "EMERGENT —
    // neither site detectable alone" for BOTH modes, which is false for
    // killer+survivor: there, one site is detectable alone, and that is the
    // entire point of the mode. An assay that describes the wrong experiment is
    // worse than no assay, because it is quotable. Third time tonight a label
    // contradicted what the code actually did.
    let (headline, meaning) = match mode {
        pairs::Mode::SurvivorPair => (
            "EMERGENT kills",
            "neither site was detectable alone — the interaction IS the defect",
        ),
        pairs::Mode::KillerPlusSurvivor => (
            "two-site kills",
            "red by construction (one site is a proven killer); the value is that the \
             failing output names ONE site while TWO need repairing",
        ),
    };
    flair::phase(
        "ASSAY",
        match mode {
            pairs::Mode::SurvivorPair => "defects that exist only in combination",
            pairs::Mode::KillerPlusSurvivor => "a visible symptom sitting over a latent defect",
        },
    );
    println!(
        "  {:<26} {}  {}",
        "pairs graded",
        flair::bold(&picked.len().to_string()),
        flair::dim(&format!("from {surv} survivors"))
    );
    println!(
        "  {:<26} {}  {}",
        headline,
        flair::green(&killed.to_string()),
        flair::dim(&format!(
            "{:.1}% — {meaning}",
            killed as f64 * 100.0 / picked.len().max(1) as f64
        ))
    );
    println!(
        "  {:<26} {}",
        "rows written",
        flair::bold(&rows.to_string())
    );
    println!(
        "  {:<26} {}",
        "wall clock",
        flair::dim(&format!("{:.1}s", started.elapsed().as_secs_f64()))
    );
    println!("\n  {}", flair::bold("kill rate by affinity"));
    for (aff, (k, n)) in &by_aff {
        println!(
            "  {:<18} {:>3}/{:<3} {:>5.1}%",
            flair::blue(aff),
            k,
            n,
            *k as f64 * 100.0 / *n as f64
        );
    }
    println!(
        "\n  {} {}",
        flair::ember("▸"),
        flair::dim(&format!("rows: {}/pairs.jsonl", out_dir.display()))
    );
    Ok(())
}

/// `crucible eval` — does a model actually repair the defect?
///
/// Execution-gated like everything else: no judge model is asked whether a patch
/// looks right. The patch is applied to the broken tree and the real suite runs.
///
/// RUN `--model oracle` FIRST, ALWAYS. It replays each row's own completion and
/// must score 100%. Anything less means the HARNESS is broken — the
/// reconstruction, the patch application, or the grader — and any number it
/// reports about a real model is meaningless. On a prior project the eval
/// harness was wrong seven times before it was right, and each time it was
/// measuring something other than what it claimed.
fn cmd_eval(args: &[String]) -> Result<(), String> {
    let repo = flag(args, "--repo").ok_or("--repo is required")?;
    let repo = PathBuf::from(&repo)
        .canonicalize()
        .map_err(|e| format!("canonicalize --repo {repo}: {e}"))?;
    let corpus_path = flag(args, "--corpus")
        .ok_or("--corpus FILE is required (out/corpus.jsonl from a forge run)")?;
    let limit: usize = flag(args, "--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let work_dir = PathBuf::from(flag(args, "--work").unwrap_or_else(|| "work".into()));
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("mkdir work: {e}"))?;
    let work_dir = work_dir.canonicalize().map_err(|e| e.to_string())?;

    let mut rows = eval::load_corpus(std::path::Path::new(&corpus_path))?;
    rows.truncate(limit);
    if rows.is_empty() {
        return Err(format!("{corpus_path} has no rows"));
    }

    let responder: Box<dyn eval::Responder> = match flag(args, "--model").as_deref() {
        Some("null") => Box::new(eval::Null),
        Some("cheat") => Box::new(eval::Cheat {
            // The suite most rows are graded by, so the cheat is plausible.
            test_path: "tests/test_nedb.py".into(),
        }),
        // The point of the whole factory: a real teacher model, whose
        // answers are only kept when the suite actually goes green.
        Some("teacher") => {
            let endpoint = flag(args, "--endpoint")
                .or_else(|| std::env::var("CRUCIBLE_TEACHER_ENDPOINT").ok())
                .unwrap_or_else(|| "http://127.0.0.1:11434".into());
            let model = flag(args, "--teacher-model")
                .or_else(|| std::env::var("CRUCIBLE_TEACHER_MODEL").ok())
                .ok_or(
                    "--teacher-model NAME is required with --model teacher (e.g. glm-5.3-flash)",
                )?;
            let mut t = crucible::teacher::Teacher::new(&endpoint, &model);
            if let Some(v) = flag(args, "--teacher-max-tokens").and_then(|v| v.parse().ok()) {
                t.max_tokens = v;
            }
            if let Some(v) = flag(args, "--teacher-timeout").and_then(|v| v.parse().ok()) {
                t.timeout_secs = v;
            }
            // Never read from a flag: a key in argv is a key in every ps
            // listing and shell history on the box.
            t.api_key = std::env::var("CRUCIBLE_TEACHER_KEY").ok();
            // Default the transcript ON, next to the corpus. The reasoning
            // traces are the expensive product of a teacher run, and a
            // scoreboard that reports a percentage while discarding the bytes
            // that earned it means paying for the same tokens twice.
            t.transcript = Some(
                flag(args, "--transcript")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        std::path::Path::new(&corpus_path)
                            .parent()
                            .unwrap_or(std::path::Path::new("."))
                            .join("teacher.jsonl")
                    }),
            );
            eprintln!(
                "{}",
                flair::dim(&format!(
                    "  teacher {model} at {endpoint} · max_tokens {} · transcript {}",
                    t.max_tokens,
                    t.transcript
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ))
            );
            Box::new(t)
        }
        // Default is the oracle, deliberately: the first thing anyone runs
        // should be the thing that validates the harness.
        _ => Box::new(eval::Oracle),
    };

    let t = Target::nedb_preset(&repo.display().to_string());
    let head = crucible::worktree::head_commit(&repo)?;
    flair::banner(&t.name, &head, rows.len(), t.graders.len(), 1);
    flair::phase(
        "EVAL",
        &format!("{} rows · responder `{}`", rows.len(), responder.name()),
    );
    if responder.name() == "oracle" {
        flair::note(
            "the oracle replays each row's OWN completion — it must score 100%, and \
             anything less indicts this harness rather than the corpus",
        );
    }

    flair::phase(
        "BASELINE",
        "the grader must be green before anything is broken",
    );
    let probe = crucible::worktree::Worktree::create(&repo, &work_dir, "ebase", &head)?;
    let all = t.all_graders();
    let base = crucible::verify::measure_baseline(&probe.root, &all, 120_000, |n, ms, d, ok| {
        flair::baseline(n, ms, d, ok);
    })?;
    drop(probe);

    flair::phase("SCORE", "break the tree, ask, apply, re-grade");
    let wt = crucible::worktree::Worktree::create(&repo, &work_dir, "eval", &head)?;
    let started = std::time::Instant::now();
    let mut results = Vec::new();
    for (i, ex) in rows.iter().enumerate() {
        let r = eval::score_one(&wt, &t, &base, ex, responder.as_ref())?;
        let note = match &r.score {
            eval::Score::Solved => "suite went green".to_string(),
            eval::Score::NotFixed { suite } => format!("{suite} still red"),
            eval::Score::Malformed { why } => why.chars().take(64).collect(),
            eval::Score::Cheated { touched } => {
                format!("patched {} — outside the contract", touched.join(","))
            }
            eval::Score::RowDidNotReproduce => "the ROW is bad: broken state was green".into(),
        };
        flair::trial(
            i + 1,
            rows.len(),
            match r.score.label() {
                "SOLVED" => "FAILED", // flair's green tag
                "CHEATED" | "BAD-ROW" | "MALFORMED" => "ABORT",
                _ => "SURVIVED",
            },
            &r.operator,
            "silent",
            &r.file,
            ex.line,
            r.score.label(),
            r.elapsed_ms,
            &note,
        );
        results.push(r);
    }
    let _ = wt.checkout(&head);
    drop(wt);

    let n = results.len();
    let solved = results.iter().filter(|r| r.score.solved()).count();
    flair::phase("SCOREBOARD", &format!("responder `{}`", responder.name()));
    for label in ["SOLVED", "MISSED", "MALFORMED", "CHEATED", "BAD-ROW"] {
        let c = results.iter().filter(|r| r.score.label() == label).count();
        let line = format!(
            "  {:<14} {:>4}  {:>5.1}%",
            label,
            c,
            c as f64 * 100.0 / n as f64
        );
        println!(
            "{}",
            match label {
                "SOLVED" => flair::green(&line),
                "CHEATED" | "BAD-ROW" => {
                    if c > 0 {
                        flair::red(&line)
                    } else {
                        flair::dim(&line)
                    }
                }
                _ => flair::dim(&line),
            }
        );
    }
    println!(
        "\n  {:<14} {}",
        "pass@1",
        flair::bold(&format!("{:.1}%", solved as f64 * 100.0 / n as f64))
    );
    println!(
        "  {:<14} {}",
        "wall clock",
        flair::dim(&format!("{:.1}s", started.elapsed().as_secs_f64()))
    );

    // A harness that cannot solve its own corpus is not a scoreboard, and
    // saying so LOUDLY matters more than exiting zero.
    if responder.name() == "oracle" && solved != n {
        println!();
        flair::warn(&format!(
            "THE ORACLE DID NOT SCORE 100% ({solved}/{n}). This harness is broken, not the \
             corpus — every row's own completion is by construction a correct repair. Do not \
             trust any eval number until this reads {n}/{n}."
        ));
        return Err("oracle below 100% — harness defect".into());
    }
    Ok(())
}

fn cmd_operators() -> Result<(), String> {
    println!("{}", flair::bold("mutation operators"));
    println!("{}", flair::dim("  severity `silent` = the code still runs, still returns, and lies.\n  That is the defect class worth training on; crashes announce themselves."));
    for (op, sev, what) in [
        (
            "except_swallow",
            "silent",
            "a handled-and-reported error becomes `pass`",
        ),
        (
            "guard_drop",
            "silent",
            "an early-exit guard is deleted; the body runs on input it refused",
        ),
        (
            "return_none",
            "silent",
            "signature kept, value dropped — callers get None far from here",
        ),
        (
            "condition_negate",
            "silent",
            "a branch inverts; both arms run in the wrong circumstances",
        ),
        (
            "compare_flip",
            "silent",
            "< becomes <=, == becomes != — boundary and equality errors",
        ),
        ("boolop_swap", "silent", "and becomes or"),
        ("augassign_flip", "silent", "+= becomes -="),
        ("off_by_one", "silent", "an integer literal moves by one"),
        ("constant_perturb", "silent", "True/False/None flip"),
        (
            "await_drop",
            "silent",
            "an await is removed; the coroutine is created and never run",
        ),
    ] {
        println!(
            "  {:<18} {:<8} {}",
            flair::blue(op),
            flair::dim(sev),
            flair::dim(what)
        );
    }
    Ok(())
}

fn cmd_locate(args: &[String]) -> Result<(), String> {
    let repo = flag(args, "--repo").ok_or("--repo is required")?;
    let t = Target::nedb_preset(&repo);
    let files = locate::discover(&PathBuf::from(&repo), &t.sources, &t.extension, &t.exclude);
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let only = flag(args, "--file");
    let mut total = 0usize;
    let mut refused = 0usize;
    for f in &files {
        let s = f.display().to_string();
        if let Some(o) = &only {
            if !s.contains(o.as_str()) {
                continue;
            }
        }
        let r = locate::locate(&t.locator, &cwd, f)?;
        if let Some(e) = &r.error {
            flair::warn(&format!("{s}: {e}"));
            continue;
        }
        total += r.candidates.len();
        refused += r.rejected_uncompilable;
        let silent = r
            .candidates
            .iter()
            .filter(|c| c.severity == "silent")
            .count();
        println!(
            "  {:>4} candidates ({:>4} silent) {:>4} refused   {}",
            r.candidates.len(),
            silent,
            r.rejected_uncompilable,
            flair::cyan(f.file_name().unwrap().to_str().unwrap())
        );
    }
    println!(
        "\n  {} candidates · {} refused as uncompilable",
        flair::bold(&total.to_string()),
        refused
    );
    Ok(())
}

fn cmd_forge(args: &[String]) -> Result<(), String> {
    let repo = flag(args, "--repo").ok_or("--repo is required")?;
    let workers: usize = flag(args, "--workers")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2)
        });
    let limit = flag(args, "--limit").and_then(|v| v.parse().ok());
    let only_operator = flag(args, "--operator");
    let out_dir = PathBuf::from(flag(args, "--out").unwrap_or_else(|| "out".into()));
    let work_dir = PathBuf::from(flag(args, "--work").unwrap_or_else(|| "work".into()));
    // Absolutised here as well as in Worktree::create, so the path printed in
    // the banner is the path git will actually use.
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("mkdir work dir: {e}"))?;
    let work_dir = work_dir
        .canonicalize()
        .map_err(|e| format!("canonicalize work dir: {e}"))?;
    let repo = PathBuf::from(&repo)
        .canonicalize()
        .map_err(|e| format!("canonicalize --repo {repo}: {e}"))?
        .display()
        .to_string();

    let t = match flag(args, "--target").as_deref() {
        Some("nedb-rust") => Target::nedb_rust_preset(&repo),
        // Default stays the Python engine: it is the fast grader and the one
        // with all ten operators.
        _ => Target::nedb_preset(&repo),
    };
    let opts = forge::Opts {
        workers,
        limit,
        only_operator,
        work_dir: work_dir.clone(),
    };
    let assay = forge::run(&t, &opts)?;

    // ---- corpus ----------------------------------------------------------
    let repo_p = PathBuf::from(&repo);
    let commit = crucible::worktree::head_commit(&repo_p)?;
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let mut rows = 0usize;
    let mut render_failed = 0usize;
    {
        use std::io::Write;
        let path = out_dir.join("corpus.jsonl");
        let mut fh = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        for tr in &assay.trials {
            if !tr.verdict.killed() {
                continue;
            }
            let rel = PathBuf::from(&tr.file);
            let pristine = match forge::read_pristine(&repo_p, &rel) {
                Ok(s) => s,
                Err(e) => {
                    // Never a silent skip: a row missing from the corpus with no
                    // explanation is indistinguishable from a forge that never
                    // tried.
                    flair::abort(&tr.file, &tr.candidate.operator, tr.candidate.line, &e);
                    render_failed += 1;
                    continue;
                }
            };
            let c = &tr.candidate;
            // Rebuild the broken side by BYTE splice, never by treating bytes as
            // chars: `b as char` mangles every non-ASCII byte into a different
            // codepoint, which would corrupt the prompt excerpt for any file
            // containing so much as an em dash — and nedb's sources are full of
            // them.
            let pb = pristine.as_bytes();
            if c.end_byte > pb.len() {
                flair::abort(
                    &tr.file,
                    &c.operator,
                    c.line,
                    "candidate span is past the end of the pristine file",
                );
                render_failed += 1;
                continue;
            }
            let mut mb: Vec<u8> = Vec::with_capacity(pb.len());
            mb.extend_from_slice(&pb[..c.start_byte]);
            mb.extend_from_slice(c.after.as_bytes());
            mb.extend_from_slice(&pb[c.end_byte..]);
            let mutated = match String::from_utf8(mb) {
                Ok(s) => s,
                Err(e) => {
                    flair::abort(
                        &tr.file,
                        &c.operator,
                        c.line,
                        &format!("splice is not utf-8: {e}"),
                    );
                    render_failed += 1;
                    continue;
                }
            };
            if let Some(ex) = render::example(
                tr,
                &tr.file,
                &pristine,
                &mutated,
                &commit,
                &t.name,
                &work_dir.display().to_string(),
            ) {
                writeln!(
                    fh,
                    "{}",
                    serde_json::to_string(&ex).map_err(|e| e.to_string())?
                )
                .map_err(|e| e.to_string())?;
                rows += 1;
            }
        }
    }
    let trials_path = out_dir.join("trials.json");
    std::fs::write(
        &trials_path,
        serde_json::to_string_pretty(&assay.trials).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    print_assay(&assay, rows, render_failed, &out_dir);
    Ok(())
}

fn print_assay(a: &report::Assay, rows: usize, render_failed: usize, out: &std::path::Path) {
    flair::phase("ASSAY", "what the run found");
    println!(
        "  {:<26} {}",
        "trials graded",
        flair::bold(&a.trials.len().to_string())
    );
    println!(
        "  {:<26} {}  {}",
        "killed (usable)",
        flair::green(&a.killed().to_string()),
        flair::dim(&format!("{:.1}% kill rate", a.kill_rate()))
    );
    println!(
        "  {:<26} {}  {}",
        "survived (coverage gap)",
        flair::yellow(&a.survived().to_string()),
        flair::dim("no test noticed these")
    );
    println!(
        "  {:<26} {}",
        "corpus rows written",
        flair::bold(&rows.to_string())
    );
    if render_failed > 0 {
        println!(
            "  {:<26} {}",
            "rows lost to render errors",
            flair::red(&render_failed.to_string())
        );
    }
    if !a.aborted.is_empty() {
        println!(
            "  {:<26} {}  {}",
            "aborted",
            flair::red(&a.aborted.len().to_string()),
            flair::dim("each reason is printed above, never swallowed")
        );
    }
    println!(
        "  {:<26} {}",
        "wall clock",
        flair::dim(&format!("{:.1}s", a.wall_ms as f64 / 1000.0))
    );

    println!("\n  {}", flair::bold("kill rate by operator"));
    for (op, (k, n)) in a.by_operator() {
        let pct = k as f64 * 100.0 / n as f64;
        let bar_len = (pct / 5.0).round() as usize;
        let bar = "█".repeat(bar_len);
        println!(
            "  {:<18} {:>3}/{:<3} {:>5.1}%  {}",
            flair::blue(&op),
            k,
            n,
            pct,
            if pct >= 50.0 {
                flair::green(&bar)
            } else {
                flair::yellow(&bar)
            }
        );
    }

    let silent = a.silent_survivors();
    if !silent.is_empty() {
        println!(
            "\n  {} {}",
            flair::bold("COVERAGE FINDING"),
            flair::dim(&format!(
                "{} silent mutations no suite detected — each is a place this repo \
                 can be broken without any test objecting",
                silent.len()
            ))
        );
        for t in silent.iter().take(25) {
            println!(
                "    {} {:<18} {}:{:<5} {:<24} {}",
                flair::yellow("blind"),
                flair::blue(&t.candidate.operator),
                t.file.rsplit('/').next().unwrap_or(&t.file),
                t.candidate.line,
                flair::dim(&t.candidate.scope),
                flair::dim(&t.candidate.context.chars().take(46).collect::<String>())
            );
        }
        if silent.len() > 25 {
            println!(
                "    {}",
                flair::dim(&format!("… and {} more in trials.json", silent.len() - 25))
            );
        }
    }

    println!(
        "\n  {} {}",
        flair::ember("▸"),
        flair::dim(&format!(
            "corpus: {}/corpus.jsonl · every trial: {}/trials.json",
            out.display(),
            out.display()
        ))
    );
}
