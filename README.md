# crucible

**Execution-gated training data.** No label without a verdict from a real test suite.

crucible breaks a green repository thousands of times, watches a real grader react, and keeps only the repairs that a suite *proved* were detectable. Nothing here asks a model or a human whether an example is correct. Execution is the only label.

```
┌──────────────────────────────────────────────────────────────────────────┐
│ CRUCIBLE  execution-gated corpus forge · no label without a verdict
├──────────────────────────────────────────────────────────────────────────┤
│ target   nedb  @ 5a83bfe1f
│ surface  29 files · 11 graders · 2 workers
└──────────────────────────────────────────────────────────────────────────┘

▌ BASELINE the grader must be green before anything is broken
  GREEN              test_nedb                       699 ms  deadline 13980 ms
  GREEN              test_wrap_sqlite_shadow         117 ms  deadline 3000 ms
  …

▌ FORGE 3669 trials · 2 workers · isolated worktrees
   KILLED    2/36 condition_negate   silent      client.py:135  op_put      7159 ms  caught by test_deploy
   BLIND     4/36 off_by_one         silent   autoindex.py:107  query       7671 ms  invisible to all 10 graders
```

## Why this exists

Two prior model projects taught the same lesson from opposite ends.

**`nedb-cast-slm`** — a 3.33M-parameter model trained from scratch in 41 minutes on 2 vCPU, reaching 92.3% exact plan match — worked because *the verifier already existed*. NEDB's own NQL parser was the corpus generator, the grader, **and** the entry gate: no example entered training unless it round-tripped to a canonically identical plan. We never wrote a verifier. One had already shipped.

**`imagine`** taught that fine-tuning is usually the wrong tool. Redesigning tool schemas moved usable tool calls from 2/7 to 6/7. Two full LoRA runs moved the same metric by *zero* and broke two behaviours nobody was measuring.

Put those together for a coding model: **the verifier that already exists is the compiler, the type checker, and the test suite.** So crucible never generates an example and hopes. It mutates a green tree, confirms a suite goes red, and admits the repair *because* reverting it goes green again.

## The double gate

1. **The mutated tree must go red.** If it stays green, the mutation is invisible to the target's tests — a coverage gap, not an example. Admitting it would produce a row whose prompt has no failing output to show.
2. **The pristine tree must be green**, measured in the same worktree, before anything is broken. Skip this and a flaky suite silently becomes a corpus of mislabelled rows: every mutation "kills" a suite that was already failing, and the model learns to associate an unrelated patch with an unrelated error. **A mislabelled row is worse than a missing one.**

If the grader is not green on an unmutated tree, the run is **refused**. It does not exclude the red suite and carry on — that would mean grading against a weaker suite than the operator believes, with nothing in the output saying so.

## Measured on nedb, 2026-09-10

Against `Eth-Interchained/nedb` @ `5a83bfe`, 8,460 lines of Python across 29 modules:

| | |
|---|---|
| Candidates located | **3,669** (4 refused as uncompilable) |
| Kill rate | **33–39%** |
| Verification cost | **0.9 s** focused · 7.5 s full grader |
| Baseline | 11 suites, 11/11 green, 7.5 s total |

Kill rate by operator, from a real run:

| operator | kill rate |
|---|---|
| `except_swallow` | 100% (1/1) |
| `return_none` | 75% (3/4) |
| `condition_negate` | 57% (4/7) |
| `off_by_one` | 44% (4/9) |
| `compare_flip` | 33% (1/3) |
| `augassign_flip` | 33% (1/3) |
| `boolop_swap` | 0% (0/4) |
| `guard_drop` | 0% (0/2) |
| `constant_perturb` | 0% (0/3) |

The zeroes are not broken operators. They are places nedb cannot currently detect being broken.

## Survivors are the other half of the product

A mutation no test notices is a **coverage gap**, and crucible reports it as a finding rather than as run noise. On the first real run against `wrap_sqlite.py` — the module where 3 of the 9 silent defects of 2026-09-08 lived — this list included:

```
blind except_swallow   wrap_sqlite.py:178  _execute   nedb.note_shadow_error(e)
blind except_swallow   wrap_sqlite.py:190  _execute   nedb.note_shadow_error(e)
```

That is the observability call *added in v3.2.2 to fix a silent-shadow bug*, and you can still replace it with `pass` while all 11 suites stay green.

## Design decisions that were measurements, not preferences

Every one of these came from running the thing, not from reasoning about it.

- **Mutations hang, they do not merely fail.** A `guard_drop` in `wrap_sqlite._host_scan` turned a 117 ms suite into an infinite loop. With the 180 s default a first prototype used, one hang cost more wall clock than 1,500 good trials. Deadlines are now **20× the measured baseline** (floor 3 s), and `Timeout` is its own outcome.
- **Isolation is mandatory.** A prototype died before its restore line and left the target repo dirty with a deliberate bug in it. Every verification now happens in its own detached `git worktree`; the source tree is never opened for writing.
- **Worktree paths must be absolute.** `git worktree add` runs with `cwd` = the repo, so a relative path is resolved against the *target's* tree. A first run created `nedb/work/baseline`, reported success, and then every grader failed with `ENOENT` on a cwd that never existed here — leaving a stray tree inside the repo we promised never to write to.
- **Grader breadth bought nothing.** 0 of 10 surviving mutations died when the grader went from 3 suites to 11. The focused subset is the same kill power at a quarter of the cost.
- **Prompts must lead with the failure.** nedb's suites narrate every success, so a naive tail is 1,600 characters of `ok  SELECT LIMIT` with the defect scrolled off. Output is now focused on the last failure marker — and when no marker is recognised, the prompt **says so** rather than quietly containing the wrong thing.
- **Harness paths are training-data poison.** Tracebacks from a worktree name a directory that will not exist at inference. They are rewritten to repo-relative.
- **`except_swallow` was broader than advertised.** It fired on an `except ImportError:` whose body was fallback *setup* (`import types as _types`), producing a missing-import bug whose own `NameError` names the fix — a copy-paste task mislabelled as silent-failure training. It now fires only where the handler raises, returns, or reports.

## Architecture

**Rust forge, per-language locators out of process.**

The forge schedules, splices, isolates, verifies, and reports. It never parses a target language. To mutate Python you must know exactly where its syntax nodes are, and CPython's own `ast` and `tokenize` **are** the grammar that will execute the code — a third-party reimplementation can disagree with the interpreter about where a node begins, and a mutation spliced at the wrong offset is a syntax error wearing a semantic bug's clothes.

So the locator is whatever language owns the truth about the target's syntax, and it emits JSON with **absolute byte offsets**:

```json
{"operator": "compare_flip", "start_byte": 4192, "end_byte": 4194,
 "before": "<=", "after": "<", "line": 135, "scope": "op_put",
 "severity": "silent"}
```

Absolute bytes because `col_offset` in Python's `ast` is a UTF-8 byte offset *within a line* — a footgun the moment a non-ASCII character appears above the mutation site. Resolving it in Python means the forge does a pure byte splice and cannot get it wrong.

That boundary is also why this scales past one language: `python/nedb` is located by `ast`, and `rust/nedb-v2` will be located by `syn`. The locator is per-language; the forge is universal.

**`ast.unparse` is banned.** It reformats a whole file and strips comments, so the diff would be thousands of noise lines with the real change buried. The locator uses `ast` to *find* a site and splices text surgically, so everything else stays byte-for-byte and the training target is a minimal diff.

## Corpus format

One JSON object per line. The prompt carries what a real agent would have — the failing test output — and the completion is **applicable**, not readable:

````
Repository: nedb
Commit: 5a83bfe1f

The test suite `test_deploy` is failing.

── what the suite reported ──
  FAIL daemon started
  daemon did not start — aborting

── the file it points into: python/nedb/client.py ──
   131 |     if if_seq is not None:
   132 |         op["if_seq"] = if_seq
>  135 |     if not (caused_by is not None):
   136 |         op["caused_by"] = caused_by
   137 |     if evidence is not None:

Find the defect and repair it. Reply with ONLY a unified diff inside a
sentinel block.
````

```
<<<PATCH>>>
--- a/python/nedb/client.py
+++ b/python/nedb/client.py
@@ -132,7 +132,7 @@
-    if not (caused_by is not None):
+    if caused_by is not None:
<<<END>>>
```

Sentinel blocks ([`sentinel-blocks`](https://github.com/Eth-Interchained/sentinel-blocks), npm/PyPI) rather than prose or JSON: content inside sentinels is lifted verbatim by regex and never re-parsed, so quotes, braces and newlines in a patch cannot corrupt the structure around them the way they corrupt JSON.

Every row carries provenance — `caused_by: ["trial:<id>", "tree:<commit>"]` — the way `nedb-cast-slm` chained its training lineage, so `TRACE caused_by` gives the full ancestry of any example.

Trial ids are **content addresses** over (file sha, operator, span, replacement), so a re-run over the same tree writes nothing new. The corpus is idempotent and reproducible.

## Usage

```bash
cargo install crucible-forge          # or: cargo build --release

crucible operators                    # what each mutation does, and why
crucible locate --repo /path/to/nedb  # candidate count per file, no execution
crucible forge  --repo /path/to/nedb --workers 8 --out out --work work
crucible forge  --repo /path/to/nedb --operator except_swallow --limit 200
```

Outputs `out/corpus.jsonl` (admitted rows) and `out/trials.json` (every trial, including survivors).

**Requires** `git` ≥ 2.5 (worktrees), `python3` for the Python locator, and coreutils `timeout` — the deadline is enforced by `timeout -k` rather than a hand-rolled poll loop, because a hung suite can leave children behind and `timeout` already handles process-group teardown correctly.

## Honest limits

- **Yield.** 3,669 candidates at ~35% is roughly **1,200 rows** from nedb's Python. That is enough for a focused LoRA, not enough for a strong model on its own. The multipliers are the Rust core, git-history mining, multi-mutation examples, and more repos.
- **`severity` does not discriminate yet.** All ten operators are classified `silent`, so the field is currently a constant. The taxonomy is right in principle and needs genuinely *loud* operators (ones that crash) before it carries information.
- **Grader precision varies, and it affects row quality.** `test_adapters` reports per-assertion; `test_deploy` says "daemon did not start", which does not localise the defect at all. Rows from coarse graders are weaker labels.
- **Single-mutation only.** Every example is one localised change. Real defects are often coupled across files.
- **Python only.** The Rust locator (`syn`) is not written.

## License

BUSL-1.1 · Licensor Interchained LLC · Change Date 2030-09-10 · Change License GPL-3.0-only

---

© Interchained LLC
