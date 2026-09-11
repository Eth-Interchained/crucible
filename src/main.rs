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

use crucible::{flair, forge, locate, render, report, target::Target};
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
        Some("operators") => cmd_operators(),
        _ => {
            eprintln!(
                "crucible — execution-gated corpus forge\n\n\
                 usage:\n\
                 \x20 crucible forge --repo PATH [--workers N] [--limit N] [--operator OP]\n\
                 \x20                [--out DIR] [--work DIR]\n\
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
    let files = locate::discover(&PathBuf::from(&repo), &t.sources, &t.extension);
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

    let t = Target::nedb_preset(&repo);
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
