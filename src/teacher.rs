//! The teacher: a real model, asked for a real repair, over an
//! OpenAI-compatible endpoint.
//!
//! This is the piece that turns crucible from a corpus forge into a MODEL
//! FACTORY. The loop it completes:
//!
//! 1. `forge` breaks a green repo and proves the break is detectable.
//! 2. This responder asks a large teacher model to fix it.
//! 3. `eval` applies the teacher's patch and RE-RUNS THE SUITE.
//! 4. Only `Score::Solved` rows — where the tree actually went green — become
//!    training data for a small student.
//!
//! Step 3 is the whole point, and it is what ordinary distillation does not
//! do. Copying a teacher's outputs copies its mistakes: a strong model is not
//! a correct one, and a plausible-looking patch that does not fix the defect
//! teaches the student to produce plausible-looking patches. Here the grader
//! is the judge, so a wrong teacher answer is discarded before it can poison
//! anything. No label without a verdict — the same rule the forge runs on,
//! applied to the teacher.
//!
//! **Reasoning is captured deliberately.** GLM-5.3-Flash spends most of its
//! budget in `reasoning_content` before writing `content`, and that trace is
//! the most valuable thing in the response — it is the difference between a
//! student that memorises patches and one that learns to localise a defect.
//! It is kept when present, and its absence is reported rather than assumed.
//!
//! HTTP via a `curl` subprocess, deliberately: this crate's entire dependency
//! list is `serde` and `serde_json`, and a corpus tool that can be audited in
//! an afternoon is worth more than one that saves twenty lines by pulling in a
//! TLS stack.

use std::io::Write;
use std::process::Command;

use crate::eval::Responder;
use crate::model::Example;

/// How the teacher is reached and how patient to be with it.
#[derive(Debug, Clone)]
pub struct Teacher {
    /// OpenAI-compatible base, e.g. `http://127.0.0.1:11434`.
    pub endpoint: String,
    /// Model name as the gateway routes it, e.g. `glm-5.3-flash`.
    pub model: String,
    /// TOTAL token budget — reasoning plus answer.
    ///
    /// Default 3000 is not arbitrary. At 400 the real teacher returned HTTP
    /// 200 with an EMPTY `content`: it had spent the entire budget thinking
    /// and never reached the answer, with `finish_reason: length` as the only
    /// clue. It needed 875. A budget that truncates a thinking model produces
    /// a silent empty string, which scores as MALFORMED and looks like the
    /// model failing the task rather than the harness starving it.
    pub max_tokens: u32,
    /// 0.0 for a repair task: there is one right answer and sampling away
    /// from it buys nothing.
    pub temperature: f32,
    /// Seconds before giving up on one request. A 271 GiB model at ~58 tok/s
    /// needs about 15 s for 875 tokens; 300 leaves room for a long reasoning
    /// pass without hanging a whole corpus run on one wedged request.
    pub timeout_secs: u32,
    /// Bearer token, when the gateway wants one. Never logged.
    pub api_key: Option<String>,
    /// Write every raw response here, one JSON object per line.
    ///
    /// Not optional in spirit: the reasoning traces are the expensive product
    /// of this run, and a scoreboard that reports 61% while discarding the
    /// bytes that earned it means paying for the same tokens twice.
    pub transcript: Option<std::path::PathBuf>,
}

impl Teacher {
    pub fn new(endpoint: &str, model: &str) -> Teacher {
        Teacher {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model: model.to_string(),
            max_tokens: 3000,
            temperature: 0.0,
            timeout_secs: 300,
            api_key: None,
            transcript: None,
        }
    }

    /// The instruction wrapped around the row's prompt.
    ///
    /// Two hard requirements, and both exist because of how the scorer works:
    /// the sentinel block is how `extract_patch` finds the answer at all, and
    /// the contract on which files may be touched is what separates a repair
    /// from `Score::Cheated`. Stating the anti-cheat rule up front is not
    /// leniency — a teacher that neuters the test file produces a row that is
    /// thrown away, so the tokens are wasted either way.
    fn system_prompt(&self) -> String {
        "You repair silent defects in source code.\n\
         \n\
         You will be shown a failing test suite's output and the source region \
         it implicates. Exactly one small edit was made to break it. Find it \
         and undo it.\n\
         \n\
         Answer with a unified diff wrapped in sentinels, and nothing outside \
         them:\n\
         \n\
         <<<PATCH>>>\n\
         --- a/path/to/file.py\n\
         +++ b/path/to/file.py\n\
         @@ -LINE,COUNT +LINE,COUNT @@\n\
          context line\n\
         -the broken line\n\
         +the repaired line\n\
          context line\n\
         <<<END>>>\n\
         \n\
         Rules:\n\
         - Edit ONLY the implementated source file. Editing a test file, or any \
         file outside the one named in the prompt, is scored as cheating and \
         discarded.\n\
         - Make the SMALLEST edit that fixes the defect. Do not reformat, \
         rename, or improve anything else.\n\
         - The diff must apply cleanly. Keep the context lines byte-exact."
            .to_string()
    }

    /// POST one chat completion and return `(content, reasoning)`.
    fn ask(&self, prompt: &str) -> Result<(String, Option<String>), String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": self.system_prompt()},
                {"role": "user", "content": prompt},
            ],
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
        });

        // Body via a temp file, not an argv string. A prompt carries newlines,
        // quotes and whole source files; passing that through a shell argument
        // is how a corpus run dies on row 400 with a quoting error.
        let dir = std::env::temp_dir();
        let body_path = dir.join(format!("crucible-teacher-{}.json", std::process::id()));
        {
            let mut fh = std::fs::File::create(&body_path)
                .map_err(|e| format!("could not stage the request body: {e}"))?;
            fh.write_all(body.to_string().as_bytes())
                .map_err(|e| format!("could not write the request body: {e}"))?;
        }

        let url = format!("{}/v1/chat/completions", self.endpoint);
        let mut cmd = Command::new("curl");
        cmd.arg("--silent")
            .arg("--show-error")
            .arg("--fail-with-body")
            .arg("--max-time")
            .arg(self.timeout_secs.to_string())
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("--data-binary")
            .arg(format!("@{}", body_path.display()));
        if let Some(k) = &self.api_key {
            // Passed as an argument, so it is visible in this process's own
            // argv while the request is in flight. Acceptable for a local
            // gateway; never printed by us, and never echoed on failure.
            cmd.arg("-H").arg(format!("Authorization: Bearer {k}"));
        }
        cmd.arg(&url);

        let out = cmd
            .output()
            .map_err(|e| format!("could not run curl (is it installed?): {e}"))?;
        let _ = std::fs::remove_file(&body_path);

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            // Name the status, the stderr AND the body. A gateway refusing a
            // request answers in the body, and reporting only the exit code
            // turns "model not found" into "curl failed with 22".
            return Err(format!(
                "teacher request to {url} failed (curl exit {}): {}{}",
                out.status.code().unwrap_or(-1),
                if stderr.is_empty() {
                    "no stderr".to_string()
                } else {
                    stderr
                },
                if stdout.trim().is_empty() {
                    String::new()
                } else {
                    format!(" — body: {}", truncate(&stdout, 400))
                }
            ));
        }

        let v: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| {
            format!(
                "teacher returned something that is not JSON ({e}): {}",
                truncate(&stdout, 400)
            )
        })?;

        // An OpenAI-shaped error is a 200 with an `error` object on some
        // gateways. Read it rather than reporting "no choices".
        if let Some(err) = v.get("error") {
            return Err(format!(
                "teacher refused: {}",
                truncate(&err.to_string(), 300)
            ));
        }

        let choice = v
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| format!("no choices in the response: {}", truncate(&stdout, 300)))?;
        let msg = choice
            .get("message")
            .ok_or_else(|| "a choice with no message".to_string())?;

        let content = msg
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        // Both spellings seen in the wild: llama.cpp emits `reasoning_content`,
        // some gateways emit `thinking`.
        let reasoning = msg
            .get("reasoning_content")
            .or_else(|| msg.get("thinking"))
            .and_then(|c| c.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());

        if content.trim().is_empty() {
            // THE THINKING-MODEL TRAP, named rather than returned as an empty
            // string. An empty content scores MALFORMED and looks like the
            // model failing; the real cause is usually a budget that ran out
            // mid-thought, and `finish_reason` says which.
            let finish = choice
                .get("finish_reason")
                .and_then(|f| f.as_str())
                .unwrap_or("unknown");
            let reasoned = reasoning.as_ref().map(|r| r.len()).unwrap_or(0);
            return Err(format!(
                "teacher returned EMPTY content (finish_reason: {finish}, {reasoned} chars of \
                 reasoning). A thinking model spends max_tokens on reasoning BEFORE the answer \
                 — if finish_reason is `length`, raise --teacher-max-tokens above {}.",
                self.max_tokens
            ));
        }

        Ok((content, reasoning))
    }

    /// Append one raw exchange to the transcript, if one was asked for.
    ///
    /// Best-effort by design — losing a transcript line must not fail a corpus
    /// run — but never silent: a transcript with holes nobody was told about
    /// is worse than no transcript.
    fn record(&self, ex: &Example, content: &str, reasoning: Option<&str>) {
        let Some(path) = &self.transcript else {
            return;
        };
        let row = serde_json::json!({
            "id": ex.id,
            "operator": ex.operator,
            "file": ex.file,
            "line": ex.line,
            "teacher_model": self.model,
            "prompt": ex.prompt,
            "content": content,
            "reasoning": reasoning,
        });
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut fh| writeln!(fh, "{row}"));
        if let Err(e) = appended {
            eprintln!(
                "crucible: could not append to the teacher transcript {} ({e}) — this row's \
                 reasoning trace is LOST even though the request was paid for",
                path.display()
            );
        }
    }
}

impl Responder for Teacher {
    fn name(&self) -> &str {
        "teacher"
    }

    fn answer(&self, ex: &Example) -> Result<String, String> {
        let (content, reasoning) = self.ask(&ex.prompt)?;
        self.record(ex, &content, reasoning.as_deref());
        Ok(content)
    }
}

/// Clip a payload for an error message without hiding that it was clipped.
fn truncate(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n {
        return t.to_string();
    }
    let head: String = t.chars().take(n).collect();
    format!("{head}… [{} chars total]", t.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Example {
        Example {
            id: "abc".into(),
            prompt: "the suite failed".into(),
            completion: "<<<PATCH>>>\n<<<END>>>\n".into(),
            operator: "off_by_one".into(),
            severity: "silent".into(),
            file: "python/nedb/log.py".into(),
            line: 27,
            scope: "<module>".into(),
            verdict: "FAILED".into(),
            caused_by: vec![],
            repo_commit: "deadbeef".into(),
        }
    }

    #[test]
    fn the_system_prompt_states_both_rules_the_scorer_enforces() {
        let t = Teacher::new("http://127.0.0.1:11434/", "glm-5.3-flash");
        let p = t.system_prompt();
        // Without the sentinels `extract_patch` cannot find an answer at all,
        // so every response would score MALFORMED regardless of quality.
        assert!(p.contains("<<<PATCH>>>"), "{p}");
        assert!(p.contains("<<<END>>>"), "{p}");
        // And the anti-cheat contract, because a patch that edits the test
        // file is discarded — the tokens are spent either way, so the rule
        // belongs in the prompt rather than only in the scorer.
        assert!(p.to_lowercase().contains("cheating"), "{p}");
        assert!(p.contains("SMALLEST"), "{p}");
    }

    #[test]
    fn a_trailing_slash_on_the_endpoint_does_not_double_up() {
        // "http://host//v1/chat/completions" is a 404 on some gateways and a
        // silent redirect on others; neither is worth debugging at row 400.
        let t = Teacher::new("http://127.0.0.1:11434/", "m");
        assert_eq!(t.endpoint, "http://127.0.0.1:11434");
    }

    #[test]
    fn truncate_says_it_truncated() {
        let long = "x".repeat(500);
        let out = truncate(&long, 100);
        assert!(out.contains("500 chars total"), "{out}");
        // A short payload is passed through whole — clipping a 12-character
        // error message would hide the entire error.
        assert_eq!(truncate("  short  ", 100), "short");
    }

    #[test]
    fn a_transcript_write_failure_is_reported_not_swallowed() {
        // The reasoning traces are the expensive product of a teacher run.
        // Losing one silently means paying for the same tokens twice.
        let mut t = Teacher::new("http://127.0.0.1:11434", "m");
        // A directory that cannot exist as a file: the append must fail.
        t.transcript = Some(std::path::PathBuf::from(
            "/proc/definitely/not/writable.jsonl",
        ));
        // Must not panic, and must not pretend it worked.
        t.record(&example(), "content", Some("reasoning"));
    }

    #[test]
    fn a_transcript_keeps_the_reasoning_trace() {
        let dir = std::env::temp_dir().join(format!("crucible-transcript-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");

        let mut t = Teacher::new("http://127.0.0.1:11434", "glm-5.3-flash");
        t.transcript = Some(path.clone());
        t.record(
            &example(),
            "<<<PATCH>>>\nx\n<<<END>>>",
            Some("I localised the off-by-one"),
        );
        t.record(&example(), "second", None);

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "one JSON object per line");
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["operator"], "off_by_one");
        assert_eq!(first["teacher_model"], "glm-5.3-flash");
        assert_eq!(first["reasoning"], "I localised the off-by-one");
        // A missing reasoning field is recorded as null, not as an empty
        // string — "the model did not reason" and "the model reasoned to no
        // characters" are different facts about the run.
        let second: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        assert!(second["reasoning"].is_null());

        std::fs::remove_dir_all(&dir).ok();
    }
}
