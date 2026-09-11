//! Loud logs, on purpose.
//!
//! A forge that runs for hours on a box you are not sitting at has exactly one
//! job in its output: make the state of the run obvious at a glance, and make a
//! problem impossible to scroll past. Every line here is keyed by VERDICT, and
//! the verdict is the first coloured thing on the line — so a screen of green
//! `KILLED` with one yellow `SURVIVED` reads correctly at arm's length.
//!
//! Colour is suppressed when stdout is not a terminal (or `NO_COLOR` is set),
//! because a log file full of escape codes is a log file nobody greps.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static PLAIN: AtomicBool = AtomicBool::new(false);

pub fn init() {
    let plain = std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal();
    PLAIN.store(plain, Ordering::Relaxed);
}

fn plain() -> bool {
    PLAIN.load(Ordering::Relaxed)
}

fn wrap(code: &str, s: &str) -> String {
    if plain() {
        s.to_string()
    } else {
        format!("\x1b[{code}m{s}\x1b[0m")
    }
}

pub fn red(s: &str) -> String {
    wrap("31;1", s)
}
pub fn green(s: &str) -> String {
    wrap("32;1", s)
}
pub fn yellow(s: &str) -> String {
    wrap("33;1", s)
}
pub fn blue(s: &str) -> String {
    wrap("34;1", s)
}
pub fn magenta(s: &str) -> String {
    wrap("35;1", s)
}
pub fn cyan(s: &str) -> String {
    wrap("36;1", s)
}
pub fn dim(s: &str) -> String {
    wrap("2", s)
}
pub fn bold(s: &str) -> String {
    wrap("1", s)
}
pub fn ember(s: &str) -> String {
    // 208 — the colour of the thing this tool is named after.
    wrap("38;5;208", s)
}

/// The opening banner. Prints the shape of the run BEFORE any work, so a
/// misconfigured forge is caught by reading, not by waiting for a bad corpus.
pub fn banner(repo: &str, commit: &str, files: usize, graders: usize, workers: usize) {
    let bar = "─".repeat(74);
    println!("{}", ember(&format!("┌{bar}┐")));
    println!(
        "{} {}  {}",
        ember("│"),
        bold("CRUCIBLE"),
        dim("execution-gated corpus forge · no label without a verdict")
    );
    println!("{}", ember(&format!("├{bar}┤")));
    println!(
        "{} target   {}  {}",
        ember("│"),
        cyan(repo),
        dim(&format!("@ {}", &commit[..commit.len().min(9)]))
    );
    println!(
        "{} surface  {} files · {} graders · {} workers",
        ember("│"),
        bold(&files.to_string()),
        bold(&graders.to_string()),
        bold(&workers.to_string())
    );
    println!("{}", ember(&format!("└{bar}┘")));
}

pub fn phase(title: &str, detail: &str) {
    println!();
    println!("{} {} {}", ember("▌"), bold(title), dim(detail));
}

/// The baseline line. If this is not green the whole run is meaningless, so it
/// gets its own loud statement rather than being folded into a summary.
pub fn baseline(suite: &str, ms: u64, limit_ms: u64, ok: bool) {
    let verdict = if ok { green("GREEN") } else { red("RED") };
    println!(
        "  {verdict:<18} {:<28} {:>7} {}",
        cyan(suite),
        format!("{ms} ms"),
        dim(&format!("deadline {limit_ms} ms"))
    );
}

/// One trial. The verdict leads, because that is what the eye scans for.
#[allow(clippy::too_many_arguments)]
pub fn trial(
    n: usize,
    total: usize,
    verdict: &str,
    operator: &str,
    severity: &str,
    file: &str,
    line: usize,
    scope: &str,
    ms: u64,
    note: &str,
) {
    let tag = match verdict {
        "FAILED" => green(" KILLED "),
        "TIMEOUT" => magenta(" HUNG   "),
        "SURVIVED" => yellow(" BLIND  "),
        // A flaky red is louder than a survivor: it means the TARGET's suite is
        // unreliable under load, which is a finding about the repo.
        "FLAKY" => red(" FLAKY  "),
        _ => dim(" ?      "),
    };
    // A silent operator that nothing caught is the single most interesting line
    // this tool can print: an invisible defect class in an untested place.
    let sev = if severity == "silent" {
        dim("silent")
    } else {
        dim("loud")
    };
    let short = file.rsplit('/').next().unwrap_or(file);
    println!(
        "  {tag} {} {:<18} {:<6} {:>22}:{:<5} {:<22} {:>7} {}",
        dim(&format!("{n:>5}/{total}")),
        blue(operator),
        sev,
        short,
        line,
        dim(scope),
        format!("{ms} ms"),
        dim(note)
    );
}

/// Something went wrong that is neither a kill nor a survival. Never a silent
/// skip: an unexplained abort is indistinguishable from a broken dependency.
pub fn abort(file: &str, operator: &str, line: usize, reason: &str) {
    println!(
        "  {} {:<18} {:>22}:{:<5} {}",
        red(" ABORT  "),
        blue(operator),
        file.rsplit('/').next().unwrap_or(file),
        line,
        red(reason)
    );
}

pub fn note(s: &str) {
    println!("  {} {}", dim("·"), dim(s));
}

pub fn warn(s: &str) {
    println!("  {} {}", yellow("!"), yellow(s));
}
