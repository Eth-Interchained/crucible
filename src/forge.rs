//! The scheduler: break, grade, restore, record — in parallel, in isolation.

use crate::flair;
use crate::locate;
use crate::model::{Aborted, Candidate, LocateReport, Trial};
use crate::report::Assay;
use crate::target::Target;
use crate::verify::{self, Baseline};
use crate::worktree::{self, Worktree};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Content address for a trial. The same forge over the same tree yields the
/// same ids, which is what makes a corpus idempotent: a re-run writes nothing
/// new, exactly like the content-addressed seed loader in salon-platform.
fn trial_id(file_sha: &str, c: &Candidate) -> String {
    let mut h = Sha256::new();
    h.update(file_sha.as_bytes());
    h.update(c.operator.as_bytes());
    h.update(c.start_byte.to_le_bytes());
    h.update(c.end_byte.to_le_bytes());
    h.update(c.after.as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

struct Job {
    rel: PathBuf,
    report_index: usize,
    candidate: Candidate,
}

pub struct Opts {
    pub workers: usize,
    pub limit: Option<usize>,
    pub only_operator: Option<String>,
    pub work_dir: PathBuf,
}

pub fn run(target: &Target, opts: &Opts) -> Result<Assay, String> {
    let repo = PathBuf::from(&target.repo);
    let commit = worktree::head_commit(&repo)?;
    let started = Instant::now();

    let files = locate::discover(&repo, &target.sources, &target.extension);
    if files.is_empty() {
        return Err(format!(
            "no .{} files under {:?} in {} — check the target's `sources`",
            target.extension,
            target.sources,
            repo.display()
        ));
    }

    flair::banner(
        &target.name,
        &commit,
        files.len(),
        target.graders.len(),
        opts.workers,
    );

    // ---- locate ----------------------------------------------------------
    // Locators run against the SOURCE tree (read-only) — no worktree needed to
    // read syntax, and doing it once up front means the expensive part (the
    // graders) is the only thing that touches a worktree.
    flair::phase(
        "LOCATE",
        &format!("{} files · {}", files.len(), target.locator.join(" ")),
    );
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut reports: Vec<LocateReport> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    let mut rejected = 0usize;
    for f in &files {
        match locate::locate(&target.locator, &cwd, f) {
            Ok(r) => {
                if let Some(e) = &r.error {
                    // A file that does not parse is a fact about the target.
                    // Reported, not fatal, and never silent.
                    flair::warn(&format!("{}: {}", f.display(), e));
                }
                rejected += r.rejected_uncompilable;
                let rel = f.strip_prefix(&repo).unwrap_or(f).to_path_buf();
                let idx = reports.len();
                for c in &r.candidates {
                    if let Some(op) = &opts.only_operator {
                        if &c.operator != op {
                            continue;
                        }
                    }
                    jobs.push(Job {
                        rel: rel.clone(),
                        report_index: idx,
                        candidate: c.clone(),
                    });
                }
                reports.push(r);
            }
            Err(e) => {
                flair::abort(&f.display().to_string(), "locate", 0, &e);
            }
        }
    }
    let candidates_seen = jobs.len();
    if let Some(n) = opts.limit {
        jobs.truncate(n);
    }
    flair::note(&format!(
        "{} candidates across {} files · {} refused as uncompilable by the locator{}",
        candidates_seen,
        reports.len(),
        rejected,
        match opts.limit {
            Some(n) if n < candidates_seen => format!(" · capped to {n} for this run"),
            _ => String::new(),
        }
    ));
    if jobs.is_empty() {
        return Err(
            "no candidates to try — every file was empty, unparseable, or filtered out".into(),
        );
    }

    // ---- baseline --------------------------------------------------------
    // Measured in a worktree, not the source tree, so the numbers describe the
    // exact conditions every trial will run under.
    flair::phase(
        "BASELINE",
        "the grader must be green before anything is broken",
    );
    let probe = Worktree::create(&repo, &opts.work_dir, "baseline", &commit)?;
    let all = target.all_graders();
    let base: Baseline = verify::measure_baseline(&probe.root, &all, 120_000, |n, ms, d, ok| {
        flair::baseline(n, ms, d, ok);
    })?;
    let total_baseline: u64 = base.ms.values().sum();
    flair::note(&format!(
        "full grader {} ms · deadlines set at {}x the measured cost (floor {} ms)",
        total_baseline,
        verify::DEADLINE_MULTIPLE,
        verify::DEADLINE_FLOOR_MS
    ));
    drop(probe);

    // ---- forge -----------------------------------------------------------
    flair::phase(
        "FORGE",
        &format!(
            "{} trials · {} workers · isolated worktrees",
            jobs.len(),
            opts.workers
        ),
    );
    let total = jobs.len();
    let queue = Mutex::new(jobs);
    let done = AtomicUsize::new(0);
    let out: Mutex<Vec<Trial>> = Mutex::new(Vec::new());
    let aborts: Mutex<Vec<Aborted>> = Mutex::new(Vec::new());
    let printer = Mutex::new(());

    std::thread::scope(|scope| {
        for w in 0..opts.workers {
            let queue = &queue;
            let done = &done;
            let out = &out;
            let aborts = &aborts;
            let printer = &printer;
            let base = &base;
            let reports = &reports;
            let repo = &repo;
            let commit = commit.as_str();
            scope.spawn(move || {
                let wt = match Worktree::create(repo, &opts.work_dir, &format!("w{w}"), commit) {
                    Ok(w) => w,
                    Err(e) => {
                        let _g = printer.lock().unwrap();
                        flair::abort("(worktree)", "setup", 0, &e);
                        return;
                    }
                };
                loop {
                    let job = { queue.lock().unwrap().pop() };
                    let Some(job) = job else { break };
                    let c = &job.candidate;
                    let rel_s = job.rel.display().to_string();
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;

                    let abs = wt.root.join(&job.rel);
                    let original = match std::fs::read(&abs) {
                        Ok(b) => b,
                        Err(e) => {
                            let _g = printer.lock().unwrap();
                            flair::abort(&rel_s, &c.operator, c.line, &format!("read: {e}"));
                            aborts.lock().unwrap().push(Aborted {
                                file: rel_s.clone(),
                                operator: c.operator.clone(),
                                line: c.line,
                                reason: format!("read: {e}"),
                            });
                            continue;
                        }
                    };

                    // The span must still say what the locator said it said. If
                    // the file moved under us, the splice would corrupt it
                    // quietly — and a corrupted file fails tests for a reason
                    // that has nothing to do with the mutation.
                    if c.end_byte > original.len()
                        || String::from_utf8_lossy(&original[c.start_byte..c.end_byte]) != c.before
                    {
                        let reason = "span no longer matches the locator's `before`".to_string();
                        let _g = printer.lock().unwrap();
                        flair::abort(&rel_s, &c.operator, c.line, &reason);
                        aborts.lock().unwrap().push(Aborted {
                            file: rel_s.clone(),
                            operator: c.operator.clone(),
                            line: c.line,
                            reason,
                        });
                        continue;
                    }

                    let mut mutated = Vec::with_capacity(original.len());
                    mutated.extend_from_slice(&original[..c.start_byte]);
                    mutated.extend_from_slice(c.after.as_bytes());
                    mutated.extend_from_slice(&original[c.end_byte..]);

                    if let Err(e) = wt.splice(&job.rel, &mutated) {
                        let _g = printer.lock().unwrap();
                        flair::abort(&rel_s, &c.operator, c.line, &e);
                        continue;
                    }

                    let graders = target.graders_for(&rel_s);
                    let names: Vec<String> = graders.iter().map(|g| g.name.clone()).collect();
                    let t0 = Instant::now();
                    let verdict = verify::grade(&wt.root, &graders, base);
                    let elapsed_ms = t0.elapsed().as_millis() as u64;

                    // Restore before anything else can go wrong. Not deferred,
                    // not trusted to the next splice: a dirty worktree turns
                    // every later trial on this worker into a mislabelled row.
                    if let Err(e) = wt.restore(&job.rel) {
                        let _g = printer.lock().unwrap();
                        flair::abort(&rel_s, &c.operator, c.line, &format!("restore: {e}"));
                        aborts.lock().unwrap().push(Aborted {
                            file: rel_s.clone(),
                            operator: c.operator.clone(),
                            line: c.line,
                            reason: format!("restore failed: {e}"),
                        });
                        break; // this worker's tree is untrustworthy; stop using it
                    }

                    let verdict = match verdict {
                        Ok(v) => v,
                        Err(e) => {
                            let _g = printer.lock().unwrap();
                            flair::abort(&rel_s, &c.operator, c.line, &e);
                            aborts.lock().unwrap().push(Aborted {
                                file: rel_s.clone(),
                                operator: c.operator.clone(),
                                line: c.line,
                                reason: e,
                            });
                            continue;
                        }
                    };

                    let note = match &verdict {
                        crate::model::Verdict::Failed { suite, .. } => format!("caught by {suite}"),
                        crate::model::Verdict::Timeout { suite, limit_ms } => {
                            format!("{suite} exceeded {limit_ms} ms")
                        }
                        crate::model::Verdict::Survived => {
                            // A count, not ten suite names: the survivor lines
                            // are the ones an operator scans for, and a note
                            // that wraps the terminal hides the signal it
                            // exists to carry. The full grader list is on the
                            // trial in trials.json.
                            format!("invisible to all {} graders", names.len())
                        }
                    };
                    {
                        let _g = printer.lock().unwrap();
                        flair::trial(
                            n,
                            total,
                            verdict.label(),
                            &c.operator,
                            &c.severity,
                            &rel_s,
                            c.line,
                            &c.scope,
                            elapsed_ms,
                            &note,
                        );
                    }
                    let file_sha = reports[job.report_index].sha256.clone();
                    out.lock().unwrap().push(Trial {
                        id: trial_id(&file_sha, c),
                        file: rel_s,
                        file_sha256: file_sha,
                        candidate: c.clone(),
                        verdict,
                        elapsed_ms,
                        graders: names,
                    });
                }
            });
        }
    });

    let mut trials = out.into_inner().unwrap();
    trials.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.candidate.line.cmp(&b.candidate.line))
    });
    Ok(Assay {
        trials,
        aborted: aborts.into_inner().unwrap(),
        candidates_seen,
        rejected_uncompilable: rejected,
        files: reports.len(),
        wall_ms: started.elapsed().as_millis() as u64,
    })
}

/// Read a file from the pristine repo (for the repaired side of a diff).
pub fn read_pristine(repo: &Path, rel: &Path) -> Result<String, String> {
    std::fs::read_to_string(repo.join(rel)).map_err(|e| format!("read pristine {rel:?}: {e}"))
}
