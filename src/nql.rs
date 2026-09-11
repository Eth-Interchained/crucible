//! `crucible nql` — the NQL translator factory.
//!
//! Same rule as the mutation forge, different language: NO LABEL WITHOUT A
//! VERDICT. There the verdict is a test suite going red; here it is a parser
//! recovering the same query plan from the teacher's own English.
//!
//! WHAT THIS FIXES
//! ---------------
//! `nedb-cast-slm` trains a 3.33M model to turn English into NQL. Every prompt
//! in its corpus comes from hand-written templates, and `cast/paraphrase.py`
//! names the problem in its own first paragraph: "this module is the real
//! quality ceiling of the project". Train only on "show me orders where total
//! is greater than 99" and the model folds when a user types "which orders
//! cleared a hundred bucks".
//!
//! Its generator also already verifies one direction — that the NQL it emits
//! parses back to the sampled plan. Nothing verified the direction that
//! matters: WHETHER THE ENGLISH IS ANSWERABLE. A template can produce an
//! ambiguous prompt and the corpus swallows it, teaching the student to map an
//! unanswerable question to one specific plan. That is noise wearing a label.
//!
//! THE ROUND TRIP
//! --------------
//! 1. the bridge samples a plan          -> gold canonical form
//! 2. the teacher writes English for it  -> candidate prompts
//! 3. the teacher reads ONLY that English — no plan, no gold NQL — and answers
//!    in NQL
//! 4. the bridge parses that NQL         -> recovered canonical form
//! 5. keep the pair iff recovered == gold
//!
//! Step 5 is PLAN equality, not string equality. Two different NQL strings that
//! compile to the same plan are both correct, and comparing text would discard
//! valid answers — the same reason `cast.evaluate` scores on canonical form.
//!
//! A prompt that fails the round trip is usually an AMBIGUOUS PROMPT rather
//! than a bad teacher, and discarding it is the point: if a 271 GiB model
//! cannot recover the query from its own description, a 3.33M student never
//! will.
//!
//! THE LABEL IS NEVER THE TEACHER'S. It is always the generator's canonical
//! rendering, which the bridge has already parser-checked. The teacher's only
//! jobs are to invent English and to act as its own judge — it does not get to
//! write syntax into the corpus.
//!
//! WHY A PYTHON BRIDGE
//! -------------------
//! NQL's parser, grammar and plan canonicaliser live in `nedb-cast-slm`, in
//! Python, next to the model that has to learn them. crucible already shells
//! out to Python for its mutation locator; this is that pattern for a different
//! language. crucible orchestrates and owns the verdict discipline; the bridge
//! owns language semantics and holds no opinions about training.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::teacher::Teacher;

/// One plan the bridge handed over, ready to be paraphrased.
#[derive(Debug, Clone, Deserialize)]
pub struct Plan {
    pub nql: String,
    pub canonical: String,
    pub schema: String,
    pub domain: String,
    pub coll: String,
    pub clauses: Vec<String>,
}

/// A verified (prompt, NQL) pair. Shaped to match what `cast.dataset` emits so
/// the existing tokenizer and trainer consume it unchanged.
#[derive(Debug, Clone, Serialize)]
pub struct Pair {
    pub prompt: String,
    /// ALWAYS the generator's canonical rendering, never the teacher's answer.
    pub nql: String,
    pub plan: String,
    pub domain: String,
    pub coll: String,
    pub clauses: Vec<String>,
    pub source: &'static str,
    pub teacher_model: String,
    /// The teacher's reasoning trace, when the gateway exposed one. Kept
    /// because it is the expensive part of the run.
    pub reasoning: Option<String>,
}

/// Why a candidate prompt did not become a row. Counted separately on purpose:
/// "ambiguous English" and "the teacher cannot write NQL" are different
/// problems with different fixes, and a single `rejected` total hides which one
/// is actually costing rows.
#[derive(Debug, Default)]
pub struct Tally {
    pub plans: usize,
    pub offered: usize,
    pub verified: usize,
    /// Parsed cleanly, described a DIFFERENT plan — the English was ambiguous.
    pub mismatch: usize,
    /// Did not parse at all — a statement about the teacher's syntax.
    pub unparseable: usize,
    pub teacher_errors: usize,
}

impl Tally {
    pub fn rate(&self) -> f64 {
        if self.offered == 0 {
            0.0
        } else {
            self.verified as f64 / self.offered as f64
        }
    }
}

/// A live `nql_bridge.py` process, spoken to over stdin/stdout.
///
/// One long-lived process rather than one per request: starting Python and
/// importing the grammar costs more than every question this asks it.
pub struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Bridge {
    /// Spawn the bridge inside `cast_repo`.
    ///
    /// `PYTHONPATH` must reach both `cast` and `nedb`, which is why the nedb
    /// checkout is a parameter rather than a guess: the bridge imports
    /// `nedb.query`, and a missing import here surfaces as an unhelpful
    /// ModuleNotFoundError on the first request instead of at startup.
    pub fn spawn(cast_repo: &Path, nedb_python: Option<&Path>) -> Result<Bridge, String> {
        let script = cast_repo.join("scripts/nql_bridge.py");
        if !script.exists() {
            return Err(format!(
                "no bridge at {} — pass --cast PATH pointing at a nedb-cast-slm checkout",
                script.display()
            ));
        }
        let mut pypath = cast_repo.display().to_string();
        if let Some(n) = nedb_python {
            pypath = format!("{pypath}:{}", n.display());
        }
        if let Ok(existing) = std::env::var("PYTHONPATH") {
            if !existing.is_empty() {
                pypath = format!("{pypath}:{existing}");
            }
        }

        let mut child = Command::new("python3")
            .arg(&script)
            .env("PYTHONPATH", &pypath)
            .current_dir(cast_repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr inherited: a Python traceback belongs on the operator's
            // terminal, not swallowed into a pipe nobody reads.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("could not start python3 for the bridge: {e}"))?;

        let stdin = child.stdin.take().ok_or("bridge stdin was not piped")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("bridge stdout was not piped")?);
        Ok(Bridge {
            child,
            stdin,
            stdout,
        })
    }

    /// One request, one reply.
    fn ask(&mut self, req: &serde_json::Value) -> Result<serde_json::Value, String> {
        writeln!(self.stdin, "{req}").map_err(|e| {
            format!("the bridge stopped reading (it probably died — see its output above): {e}")
        })?;
        self.stdin
            .flush()
            .map_err(|e| format!("could not flush to the bridge: {e}"))?;
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .map_err(|e| format!("could not read from the bridge: {e}"))?;
        if n == 0 {
            return Err(
                "the bridge closed its output without answering — check the traceback above"
                    .to_string(),
            );
        }
        serde_json::from_str(&line).map_err(|e| {
            format!("the bridge answered with something that is not JSON ({e}): {line}")
        })
    }

    /// Ask for `n` distinct plans.
    pub fn plans(&mut self, n: usize, seed: u64) -> Result<Vec<Plan>, String> {
        let v = self.ask(&serde_json::json!({"op":"plans","n":n,"seed":seed}))?;
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            return Err(format!("bridge refused to sample plans: {e}"));
        }
        // A short supply is the bridge telling the truth about the grammar, not
        // an error — but it must reach the operator, or the run reports a rate
        // against a denominator that is not the one they asked for.
        if v.get("short").and_then(|s| s.as_bool()).unwrap_or(false) {
            eprintln!(
                "crucible: {}",
                v.get("note")
                    .and_then(|n| n.as_str())
                    .unwrap_or("the bridge returned fewer plans than requested")
            );
        }
        let plans = v
            .get("plans")
            .ok_or("the bridge answered without a `plans` field")?;
        serde_json::from_value(plans.clone())
            .map_err(|e| format!("could not read the bridge's plans: {e}"))
    }

    /// Gate one candidate answer against a gold canonical form.
    pub fn check(&mut self, nql: &str, gold: &str) -> Result<Check, String> {
        let v = self.ask(&serde_json::json!({"op":"check","nql":nql,"gold":gold}))?;
        let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
        if ok {
            return Ok(Check::Verified);
        }
        // The bridge distinguishes these two and so must this: one indicts the
        // prompt, the other the teacher.
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            return Ok(Check::Unparseable(err.to_string()));
        }
        Ok(Check::Mismatch)
    }
}

/// The verdict on one candidate answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Verified,
    /// Parsed, but describes another query — the English was ambiguous.
    Mismatch,
    Unparseable(String),
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Closing stdin is how the bridge learns the run is over; its `for line
        // in sys.stdin` loop ends and it exits on its own. Kill only if it does
        // not, because a killed process cannot flush its own diagnostics.
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Everything one `crucible nql` run needs to know.
pub struct NqlConfig {
    pub cast_repo: PathBuf,
    pub nedb_python: Option<PathBuf>,
    pub plans: usize,
    /// Prompts requested per plan. Two teacher calls cover all of them, so
    /// raising this is nearly free per prompt.
    pub variants: usize,
    pub seed: u64,
    pub out: PathBuf,
}

/// Ask the teacher for English, then make it prove the English is answerable.
///
/// Two teacher calls per plan regardless of `variants`: one to write all the
/// English, one to translate all of it back. Per-prompt calls would cost
/// `variants` times as much for exactly the same information.
pub fn forge_nql(
    cfg: &NqlConfig,
    teacher: &Teacher,
    tally: &Mutex<Tally>,
) -> Result<Vec<Pair>, String> {
    let mut bridge = Bridge::spawn(&cfg.cast_repo, cfg.nedb_python.as_deref())?;
    let plans = bridge.plans(cfg.plans, cfg.seed)?;
    if plans.is_empty() {
        return Err("the bridge produced no plans — nothing to paraphrase".into());
    }
    eprintln!(
        "crucible: {} distinct plan(s) x up to {} prompt(s) · teacher {}",
        plans.len(),
        cfg.variants,
        teacher.model
    );

    let done = AtomicUsize::new(0);
    let mut rows: Vec<Pair> = Vec::new();

    for plan in &plans {
        let prompts = match ask_for_prompts(teacher, plan, cfg.variants) {
            Ok(p) if !p.is_empty() => p,
            Ok(_) => {
                // Reported rather than counted as zero prompts: a run that
                // quietly yields nothing is indistinguishable from a broken
                // endpoint.
                eprintln!(
                    "crucible: teacher returned no usable prompts for {} — skipping",
                    plan.nql
                );
                tally.lock().unwrap().teacher_errors += 1;
                continue;
            }
            Err(e) => {
                eprintln!("crucible: teacher failed writing prompts: {e}");
                tally.lock().unwrap().teacher_errors += 1;
                continue;
            }
        };

        let answers = match ask_for_nql(teacher, plan, &prompts) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("crucible: teacher failed translating back: {e}");
                tally.lock().unwrap().teacher_errors += 1;
                continue;
            }
        };

        let mut kept = 0usize;
        for (i, prompt) in prompts.iter().enumerate() {
            let answer = answers.get(i).cloned().unwrap_or_default();
            {
                tally.lock().unwrap().offered += 1;
            }
            if answer.trim().is_empty() {
                tally.lock().unwrap().unparseable += 1;
                continue;
            }
            match bridge.check(&answer, &plan.canonical)? {
                Check::Verified => {
                    tally.lock().unwrap().verified += 1;
                    kept += 1;
                    rows.push(Pair {
                        prompt: prompt.clone(),
                        nql: plan.nql.clone(),
                        plan: plan.canonical.clone(),
                        domain: plan.domain.clone(),
                        coll: plan.coll.clone(),
                        clauses: plan.clauses.clone(),
                        source: "teacher",
                        teacher_model: teacher.model.clone(),
                        reasoning: None,
                    });
                }
                Check::Mismatch => tally.lock().unwrap().mismatch += 1,
                Check::Unparseable(_) => tally.lock().unwrap().unparseable += 1,
            }
        }

        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        let t = tally.lock().unwrap();
        eprintln!(
            "  [{n}/{}] +{kept} verified · {}/{} kept ({:.1}%)",
            plans.len(),
            t.verified,
            t.offered,
            t.rate() * 100.0
        );
    }

    tally.lock().unwrap().plans = plans.len();
    Ok(rows)
}

const PARAPHRASE_SYSTEM: &str = "You write the kinds of questions real people type into a database search box.\n\
     \n\
     You will be shown a collection's schema and one NQL query. Write DIFFERENT ways a person might ask for exactly that data.\n\
     \n\
     Rules:\n\
     - Output ONE question per line. Nothing else — no numbering, no quotes, no commentary.\n\
     - Every question must be answerable by that exact query. Do not add or drop a filter, a sort, or a limit.\n\
     - Vary hard: verb (\"show me\" / \"list\" / \"pull up\" / none), operator wording (\"over\" / \"more than\" / \"above\"), field naming, clause order, register, and casing.\n\
     - Some should be sloppy the way real input is: lowercase, no punctuation, contractions.\n\
     - Never mention NQL, SQL, syntax, fields you were not given, or the word \"query\".";

const TRANSLATE_SYSTEM: &str = "You convert a natural-language request into a single NQL query.\n\
     \n\
     NQL grammar (keywords case-insensitive):\n\
     \x20   FROM <collection>\n\
     \x20     [ AS OF <seq> ]\n\
     \x20     [ VALID AS OF <date> ]\n\
     \x20     [ WHERE <field> <op> <value> (AND <field> <op> <value>)* ]\n\
     \x20     [ SEARCH \"<text>\" ]\n\
     \x20     [ ORDER BY <field> [ASC|DESC] ]\n\
     \x20     [ TRAVERSE <relation> ]\n\
     \x20     [ LIMIT <n> ]\n\
     \x20   op    := = | != | < | <= | > | >=\n\
     \x20   value := number | \"string\" | 'string' | true | false | null\n\
     \n\
     Output ONLY the query on one line. No explanation, no code fences, no trailing punctuation.";

fn ask_for_prompts(teacher: &Teacher, plan: &Plan, variants: usize) -> Result<Vec<String>, String> {
    let user = format!(
        "{}\n\nNQL query:\n{}\n\nWrite {variants} different questions a person might type to get exactly this.",
        plan.schema, plan.nql
    );
    // Diversity IS the product on this call, so this is the one place in the
    // pipeline where sampling away from the mode is the goal.
    let text = teacher.complete(PARAPHRASE_SYSTEM, &user, 0.9)?;
    Ok(clean_prompts(&text, variants))
}

fn ask_for_nql(teacher: &Teacher, plan: &Plan, prompts: &[String]) -> Result<Vec<String>, String> {
    let numbered: Vec<String> = prompts
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{}. {p}", i + 1))
        .collect();
    let user = format!(
        "{}\n\nConvert each request to one NQL query. Output exactly {} lines, in order, each `N. <query>`.\n\n{}",
        plan.schema,
        prompts.len(),
        numbered.join("\n")
    );
    // 0.0 going back: there is one right plan and creativity is a defect.
    let text = teacher.complete(TRANSLATE_SYSTEM, &user, 0.0)?;
    Ok(match_answers(&text, prompts.len()))
}

/// Strip the decorations a chat model adds even when told not to.
pub fn clean_nql(text: &str) -> String {
    let t = text.trim().trim_matches('`').trim();
    for raw in t.lines() {
        let line = raw.trim().trim_matches('`').trim_end_matches(';').trim();
        if line.len() >= 5 && line[..5].eq_ignore_ascii_case("from ") {
            return line.to_string();
        }
    }
    t.lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('`')
        .trim_end_matches(';')
        .trim()
        .to_string()
}

/// One prompt per line, with numbering, bullets, quotes and narration removed.
pub fn clean_prompts(text: &str, want: usize) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut s = raw.trim().to_string();
        if s.is_empty() {
            continue;
        }
        // "1. ", "2) ", "- ", "* ", "• "
        if let Some(rest) = strip_list_marker(&s) {
            s = rest;
        }
        let s = s
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim()
            .to_string();
        let lower = s.to_lowercase();
        // A model narrating ("Here are 8 ways:") is not a prompt.
        if s.len() < 4
            || s.ends_with(':')
            || lower.starts_with("here are")
            || lower.starts_with("sure")
            || lower.starts_with("certainly")
        {
            continue;
        }
        out.push(s);
        if out.len() >= want {
            break;
        }
    }
    out
}

fn strip_list_marker(s: &str) -> Option<String> {
    let t = s.trim_start();
    if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
        return Some(rest.to_string());
    }
    if let Some(rest) = t.strip_prefix("• ") {
        return Some(rest.to_string());
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = &t[digits.len()..];
    for sep in [". ", ") "] {
        if let Some(rest) = after.strip_prefix(sep) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Map the teacher's `N. <query>` lines back to prompt positions.
///
/// Numbered lines win. Unnumbered ones are used positionally ONLY when the
/// count matches exactly — guessing across indices would attach a prompt to
/// someone else's answer, which the gate would then score as a mismatch and
/// blame on the English.
pub fn match_answers(text: &str, want: usize) -> Vec<String> {
    let mut numbered: Vec<Option<String>> = vec![None; want];
    let mut positional: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = strip_list_marker(line) {
            let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(idx) = digits.parse::<usize>() {
                if idx >= 1 && idx <= want {
                    numbered[idx - 1] = Some(clean_nql(&rest));
                    continue;
                }
            }
            positional.push(clean_nql(&rest));
            continue;
        }
        if line.len() >= 5 && line[..5].eq_ignore_ascii_case("from ") {
            positional.push(clean_nql(line));
        }
    }
    // Positional rescue is allowed ONLY when the count matches exactly. Any
    // other length means the mapping is unknowable, and filling slots from a
    // short list attaches a prompt to someone else's answer — which the gate
    // then scores as a mismatch and blames on the English. Leaving the slot
    // empty counts it as unparseable, which is at least a claim about the
    // teacher rather than a lie about the data.
    let exact = positional.len() == want;
    numbered
        .into_iter()
        .enumerate()
        .map(|(i, n)| match n {
            Some(v) => v,
            None if exact => positional[i].clone(),
            None => String::new(),
        })
        .collect()
}

/// Write the verified pairs, one JSON object per line.
pub fn write_pairs(path: &Path, rows: &[Pair]) -> Result<(), String> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))?;
    }
    let mut fh = std::fs::File::create(path)
        .map_err(|e| format!("could not create {}: {e}", path.display()))?;
    for r in rows {
        let line = serde_json::to_string(r).map_err(|e| format!("serialise row: {e}"))?;
        writeln!(fh, "{line}").map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_nql_strips_what_chat_models_add_anyway() {
        // Told "output only the query", a model still fences it, explains it,
        // and adds a semicolon. Each of these would fail the parser and be
        // counted against the TEACHER's syntax rather than recognised as a
        // correct answer the harness failed to read.
        assert_eq!(
            clean_nql("```sql\nFROM orders WHERE total > 99\n```"),
            "FROM orders WHERE total > 99"
        );
        assert_eq!(clean_nql("FROM a LIMIT 5;"), "FROM a LIMIT 5");
        assert_eq!(
            clean_nql("Sure! Here it is:\nFROM b ORDER BY x DESC"),
            "FROM b ORDER BY x DESC"
        );
        // Lower-case keyword is still a query — the parser is case-insensitive
        // and rejecting it here would discard a valid answer before the gate
        // ever saw it.
        assert_eq!(clean_nql("from c limit 1"), "from c limit 1");
        assert_eq!(clean_nql(""), "");
    }

    #[test]
    fn clean_prompts_drops_narration_and_keeps_prompts() {
        let text = "Here are 3 ways:\n\
                    1. show me orders over 99\n\
                    - which orders cleared 99\n\
                    \"pull up big orders\"\n\
                    \n\
                    Questions:\n\
                    ok\n";
        assert_eq!(
            clean_prompts(text, 8),
            vec![
                "show me orders over 99",
                "which orders cleared 99",
                "pull up big orders"
            ]
        );
    }

    #[test]
    fn clean_prompts_respects_the_cap() {
        let text: String = (0..20)
            .map(|i| format!("question number {i} about orders\n"))
            .collect();
        assert_eq!(clean_prompts(&text, 5).len(), 5);
    }

    #[test]
    fn answers_are_matched_by_number_not_by_luck() {
        // The teacher answering out of order is normal. Attaching answer 3 to
        // prompt 1 would be scored as ambiguous English, blaming the prompt for
        // the harness's bookkeeping.
        let text = "2. FROM b LIMIT 2\n1. FROM a LIMIT 1\n3. FROM c LIMIT 3";
        assert_eq!(
            match_answers(text, 3),
            vec!["FROM a LIMIT 1", "FROM b LIMIT 2", "FROM c LIMIT 3"]
        );
    }

    #[test]
    fn unnumbered_answers_are_positional_only_when_the_count_is_exact() {
        // Three answers for three prompts: safe to use in order.
        let ok = "FROM a LIMIT 1\nFROM b LIMIT 2\nFROM c LIMIT 3";
        assert_eq!(match_answers(ok, 3).len(), 3);
        assert_eq!(match_answers(ok, 3)[1], "FROM b LIMIT 2");

        // TWO answers for three prompts: the mapping is unknowable, so nothing
        // is guessed. A wrong guess would be counted as the English being
        // ambiguous, which is a lie about the data.
        let short = "FROM a LIMIT 1\nFROM b LIMIT 2";
        let got = match_answers(short, 3);
        assert_eq!(got.len(), 3);
        assert!(
            got.iter().all(|g| g.is_empty()),
            "a count mismatch must not be positionally guessed: {got:?}"
        );
    }

    #[test]
    fn a_missing_bridge_says_which_flag_fixes_it() {
        // `expect_err` would need Bridge: Debug, and deriving Debug on a
        // struct holding a live Child to satisfy a test is the test dictating
        // the design. Match instead.
        let err = match Bridge::spawn(Path::new("/definitely/not/here"), None) {
            Ok(_) => panic!("a missing bridge must not look like success"),
            Err(e) => e,
        };
        assert!(err.contains("--cast"), "must name the flag: {err}");
        assert!(err.contains("nedb-cast-slm"), "must name the repo: {err}");
    }

    #[test]
    fn pairs_are_written_one_json_object_per_line() {
        let dir = std::env::temp_dir().join(format!("crucible-nql-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("pairs.jsonl");
        let rows = vec![
            Pair {
                prompt: "show me orders over 99".into(),
                nql: "FROM orders WHERE total > 99".into(),
                plan: "{}".into(),
                domain: "shop".into(),
                coll: "orders".into(),
                clauses: vec!["where".into()],
                source: "teacher",
                teacher_model: "glm-5.3-flash".into(),
                reasoning: Some("mapped the predicate".into()),
            },
            Pair {
                prompt: "orders over 99".into(),
                nql: "FROM orders WHERE total > 99".into(),
                plan: "{}".into(),
                domain: "shop".into(),
                coll: "orders".into(),
                clauses: vec!["where".into()],
                source: "teacher",
                teacher_model: "glm-5.3-flash".into(),
                reasoning: None,
            },
        ];
        write_pairs(&path, &rows).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        // cast.tokenizer and cast.train read these keys by name. A renamed
        // field breaks the trainer in another repo, far from here.
        for key in ["prompt", "nql", "plan", "domain", "coll", "clauses"] {
            assert!(first.get(key).is_some(), "{key} missing from the row");
        }
        assert_eq!(first["source"], "teacher");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_tally_keeps_ambiguous_english_apart_from_bad_syntax() {
        // These have different fixes: mismatch means rewrite the paraphrase
        // instruction, unparseable means the teacher needs the grammar spelled
        // out. A single `rejected` counter would hide which.
        let t = Tally {
            offered: 4,
            verified: 1,
            mismatch: 2,
            unparseable: 1,
            ..Default::default()
        };
        assert_eq!(t.rate(), 0.25);
        assert_eq!(t.mismatch + t.unparseable + t.verified, t.offered);
        // An empty run reports 0.0 rather than dividing by zero.
        assert_eq!(Tally::default().rate(), 0.0);
    }
}
