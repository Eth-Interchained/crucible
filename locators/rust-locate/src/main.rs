//! Locate candidate mutation sites in a Rust file. Emit the same JSON contract
//! the Python locator emits, so the forge cannot tell them apart.
//!
//! WHY THIS IS RUST AND NOT PYTHON. Same principle as the other direction: the
//! locator is written in whatever language owns the truth about the target's
//! syntax. `syn` and `proc-macro2` are the crates the Rust ecosystem itself
//! parses with, so their spans agree with the compiler that will build the
//! mutated tree.
//!
//! `span-locations` IS LOAD-BEARING. Without that feature `Span::byte_range()`
//! reports `0..0` for every token outside a proc macro, so every candidate
//! would splice at the top of the file — silently, producing a file that often
//! still parses. It is a Cargo feature, which means the failure mode is a
//! dependency edit away at all times, so there is a test asserting a nonzero
//! range.
//!
//! TEST MODULES ARE EXCLUDED, and this is not cosmetic. nedb's engine carries 71
//! tests INLINE under `#[cfg(test)] mod tests`. Mutating inside one mutates the
//! GRADER: the suite then fails because the test changed, not because the code
//! broke, and the resulting row would teach a model to repair a test it should
//! never touch. `db.rs` alone has 115 `.unwrap()` calls and most of them are in
//! test code.
//!
//! WHY `syn`'s AST AND NOT A TOKEN WALK. The first version of this file walked
//! a flattened `proc_macro2` token stream, which is exactly how the Python
//! locator's token pass works. Rust's grammar punishes that: `<` and `>` are
//! overloaded for GENERICS, so `std::io::Result<()>` yielded two `compare_flip`
//! candidates, and flipping the `<` of a generic to `<=` is a syntax error
//! rather than a semantic bug. There is no local token rule that separates the
//! two — `Vec<u8>` and `a < b` are indistinguishable without knowing you are in
//! a type position. So comparisons are taken from `Expr::Binary` nodes, where a
//! generic's angle bracket is simply never an operator. My own test caught this
//! before a single trial ran.
//!
//! ON WHAT IS NOT EMITTED. Rust's compiler is part of the grader, so a mutation
//! that does not typecheck still "kills" the suite — but the error names the fix
//! and the row is worthless, exactly like the SyntaxError case in Python. So the
//! operators here are chosen to be TYPE-PRESERVING BY CONSTRUCTION: comparison
//! flips, boolean-operator swaps, integer-literal perturbation, bool flips, and
//! compound-assignment flips. Each leaves the expression's type identical, so
//! anything red is red for a semantic reason.

use proc_macro2::Span;
use serde::Serialize;
use std::collections::BTreeSet;
use syn::spanned::Spanned;
use syn::visit::Visit;

#[derive(Serialize)]
struct Candidate {
    operator: String,
    start_byte: usize,
    end_byte: usize,
    before: String,
    after: String,
    line: usize,
    context: String,
    scope: String,
    severity: String,
}

#[derive(Serialize)]
struct Report {
    file: String,
    sha256: String,
    candidates: Vec<Candidate>,
    rejected_uncompilable: usize,
    operators_available: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Bytes skipped for being test code. Reported rather than silently applied,
    /// because "why did this file yield so few candidates" must be answerable
    /// from the output.
    excluded_test_bytes: usize,
}

fn line_starts(data: &[u8]) -> Vec<usize> {
    let mut v = vec![0usize, 0usize];
    for (i, b) in data.iter().enumerate() {
        if *b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

fn line_of(starts: &[usize], byte: usize) -> usize {
    let mut lo = 1usize;
    let mut hi = starts.len() - 1;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if starts[mid] <= byte {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Read a possibly-suffixed integer literal: `7`, `7u64`, `1_000i32`.
///
/// Returns the incremented text with the suffix preserved. A literal that cannot
/// be confidently rebuilt is refused rather than guessed at: emitting `8u64`
/// where the source had a float would be a TYPE change, and a type change is a
/// compile error wearing a semantic bug's clothes — the error names the fix, so
/// the row would be worthless.
fn bump_int(text: &str) -> Option<String> {
    // Check the WHOLE text for float markers first. An earlier version only
    // checked the numeric prefix, so `1e9` split into num="1" / suffix="e9",
    // parsed as 1, and came back as "2e9" — a different number in a different
    // notation. Caught by its own test.
    if text.contains('.') {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("0x") || lower.starts_with("0b") || lower.starts_with("0o") {
        return None;
    }
    // Any `e` means exponent notation. Rust has no `e` in an integer suffix —
    // they are u8..u128, i8..i128, usize, isize, f32, f64 — so this is
    // unambiguous.
    if lower.contains('e') {
        return None;
    }
    let split = text.find(['u', 'i', 'f']).unwrap_or(text.len());
    let (num, suffix) = text.split_at(split);
    if suffix.starts_with('f') {
        return None; // f32/f64
    }
    let cleaned: String = num.chars().filter(|c| *c != '_').collect();
    if cleaned.is_empty() {
        return None;
    }
    let v: u128 = cleaned.parse().ok()?;
    Some(format!("{}{}", v + 1, suffix))
}

/// Visitor: collects candidates from the places where the syntax is unambiguous.
struct Collector<'a> {
    src: &'a str,
    starts: &'a [usize],
    lines: Vec<&'a str>,
    out: Vec<Candidate>,
    rejected: usize,
    excluded: usize,
    /// Innermost enclosing fn name, for log readability and pair affinity.
    scope: Vec<String>,
}

impl<'a> Collector<'a> {
    fn push(&mut self, operator: &str, span: Span, before: &str, after: &str) {
        let r = span.byte_range();
        if r.end <= r.start || r.end > self.src.len() {
            // A span that does not point into this file is a locator defect, not
            // a candidate. Counted so it cannot be silent.
            self.rejected += 1;
            return;
        }
        // The span must contain exactly what we say it does. The forge re-checks
        // this, but a locator wrong about its own spans should fail here rather
        // than 3,000 trials later.
        if &self.src[r.start..r.end] != before {
            self.rejected += 1;
            return;
        }
        let line = line_of(self.starts, r.start);
        self.out.push(Candidate {
            operator: operator.into(),
            start_byte: r.start,
            end_byte: r.end,
            before: before.into(),
            after: after.into(),
            line,
            context: self
                .lines
                .get(line - 1)
                .map(|l| l.trim().to_string())
                .unwrap_or_default(),
            scope: self.scope.last().cloned().unwrap_or_default(),
            severity: "silent".into(),
        });
    }
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        let toks = match &a.meta {
            syn::Meta::List(l) => l.tokens.to_string(),
            _ => String::new(),
        };
        a.path().is_ident("cfg") && toks.contains("test")
    })
}

impl<'ast, 'a> Visit<'ast> for Collector<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        // TEST MODULES ARE THE GRADER. Mutating inside one makes the suite fail
        // because the TEST changed, and the row would teach a model to edit a
        // test rather than repair code. nedb's engine carries 71 tests inline,
        // and db.rs alone has 115 `.unwrap()` calls, most of them in test code.
        if is_cfg_test(&i.attrs) {
            let r = i.span().byte_range();
            self.excluded += r.end.saturating_sub(r.start);
            return; // do not descend
        }
        syn::visit::visit_item_mod(self, i);
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if is_cfg_test(&i.attrs) {
            let r = i.span().byte_range();
            self.excluded += r.end.saturating_sub(r.start);
            return;
        }
        self.scope.push(i.sig.ident.to_string());
        syn::visit::visit_item_fn(self, i);
        self.scope.pop();
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.scope.push(i.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, i);
        self.scope.pop();
    }

    fn visit_expr_binary(&mut self, i: &'ast syn::ExprBinary) {
        use syn::BinOp::*;
        // Every arm here is TYPE-PRESERVING: a comparison stays a comparison, a
        // bool op stays a bool op. That matters because the Rust compiler is
        // part of the grader — a mutation that fails to typecheck still turns
        // the suite red, but the error names the fix, so the row is worthless.
        let (op, before, after) = match &i.op {
            Lt(t) => (t.span(), "<", "<="),
            Le(t) => (t.span(), "<=", "<"),
            Gt(t) => (t.span(), ">", ">="),
            Ge(t) => (t.span(), ">=", ">"),
            Eq(t) => (t.span(), "==", "!="),
            Ne(t) => (t.span(), "!=", "=="),
            And(t) => (t.span(), "&&", "||"),
            Or(t) => (t.span(), "||", "&&"),
            AddAssign(t) => (t.span(), "+=", "-="),
            SubAssign(t) => (t.span(), "-=", "+="),
            BitOrAssign(t) => (t.span(), "|=", "&="),
            BitAndAssign(t) => (t.span(), "&=", "|="),
            _ => {
                syn::visit::visit_expr_binary(self, i);
                return;
            }
        };
        let operator = match &i.op {
            Lt(_) | Le(_) | Gt(_) | Ge(_) | Eq(_) | Ne(_) => "compare_flip",
            And(_) | Or(_) => "boolop_swap",
            _ => "augassign_flip",
        };
        self.push(operator, op, before, after);
        syn::visit::visit_expr_binary(self, i);
    }

    fn visit_lit(&mut self, i: &'ast syn::Lit) {
        match i {
            syn::Lit::Int(n) => {
                let text = n.to_string();
                match bump_int(&text) {
                    Some(after) => self.push("off_by_one", n.span(), &text, &after),
                    None => self.rejected += 1,
                }
            }
            syn::Lit::Bool(b) => {
                let text = if b.value { "true" } else { "false" };
                let after = if b.value { "false" } else { "true" };
                self.push("constant_perturb", b.span(), text, after);
            }
            _ => {}
        }
        syn::visit::visit_lit(self, i);
    }
}

fn locate(path: &str) -> Report {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            return Report {
                file: path.into(),
                sha256: String::new(),
                candidates: vec![],
                rejected_uncompilable: 0,
                operators_available: vec![],
                error: Some(format!("read: {e}")),
                excluded_test_bytes: 0,
            }
        }
    };
    let src = match String::from_utf8(data.clone()) {
        Ok(s) => s,
        Err(e) => {
            return Report {
                file: path.into(),
                sha256: String::new(),
                candidates: vec![],
                rejected_uncompilable: 0,
                operators_available: vec![],
                error: Some(format!("not utf-8: {e}")),
                excluded_test_bytes: 0,
            }
        }
    };
    let file = match syn::parse_file(&src) {
        Ok(f) => f,
        Err(e) => {
            // A target file that does not parse is a fact about the target.
            // Reported, never fatal.
            return Report {
                file: path.into(),
                sha256: sha256_hex(&data),
                candidates: vec![],
                rejected_uncompilable: 0,
                operators_available: vec![],
                error: Some(format!("parse error: {e}")),
                excluded_test_bytes: 0,
            };
        }
    };

    let starts = line_starts(&data);
    let mut c = Collector {
        src: &src,
        starts: &starts,
        lines: src.split('\n').collect(),
        out: Vec::new(),
        rejected: 0,
        excluded: 0,
        scope: Vec::new(),
    };
    c.visit_file(&file);
    let mut out = c.out;
    out.sort_by_key(|x| (x.start_byte, x.operator.clone()));
    let ops: BTreeSet<String> = out.iter().map(|x| x.operator.clone()).collect();
    Report {
        file: path.into(),
        sha256: sha256_hex(&data),
        candidates: out,
        rejected_uncompilable: c.rejected,
        operators_available: ops.into_iter().collect(),
        error: None,
        excluded_test_bytes: c.excluded,
    }
}

/// Minimal SHA-256, so the locator has no dependency beyond parsing.
///
/// The forge content-addresses trials by (file sha, operator, span, after), and
/// the Python locator reports a sha256 — the two locators must agree on what a
/// file's identity is or the same file would produce different ids depending on
/// which locator read it.
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for t in 0..16 {
            w[t] = u32::from_be_bytes([
                chunk[t * 4],
                chunk[t * 4 + 1],
                chunk[t * 4 + 2],
                chunk[t * 4 + 3],
            ]);
        }
        for t in 16..64 {
            let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
            let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
            w[t] = w[t - 16]
                .wrapping_add(s0)
                .wrapping_add(w[t - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for t in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[t])
                .wrapping_add(w[t]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: rust-locate FILE [FILE...]");
        std::process::exit(2);
    }
    let reports: Vec<Report> = args.iter().map(|p| locate(p)).collect();
    let json = if reports.len() == 1 {
        serde_json::to_string(&reports[0])
    } else {
        serde_json::to_string(&reports)
    };
    match json {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("serialize: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_not_all_zero() {
        // THE `span-locations` GUARD. Without that Cargo feature every
        // byte_range() is 0..0, so every candidate would splice at the top of
        // the file — silently, in a file that often still parses. It is one
        // dependency edit away from being true at any time.
        let r = locate_str("fn f(a: u8, b: u8) -> bool { a < b }");
        assert!(!r.candidates.is_empty());
        assert!(
            r.candidates.iter().any(|c| c.end_byte > 0),
            "all spans are 0..0 — the span-locations feature is off"
        );
    }

    #[test]
    fn a_lone_comparison_is_found_and_a_joint_one_is_not_split() {
        // `<=` is one operator. Splicing the `<` of a `<=` yields `<==`, a
        // syntax error rather than a semantic bug — the class we refuse to
        // generate. Taking the op from `Expr::Binary` makes it unrepresentable.
        let r = locate_str("fn f(a: u8, b: u8) -> bool { a <= b }");
        let ops: Vec<&str> = r.candidates.iter().map(|c| c.operator.as_str()).collect();
        assert_eq!(ops, vec!["compare_flip"]);
        assert_eq!(r.candidates[0].before, "<=");
        assert_eq!(r.candidates[0].after, "<");
    }

    #[test]
    fn generics_and_arrows_are_not_comparisons() {
        // THE TEST THAT KILLED THE FIRST IMPLEMENTATION. A flattened token walk
        // reported the `<` and `>` of `Result<()>` as two compare_flips, and
        // flipping a generic's angle bracket is a syntax error. Rust overloads
        // `<` for generics and no LOCAL token rule separates the two — `Vec<u8>`
        // and `a < b` are indistinguishable without knowing you are in a type
        // position. syn knows.
        let r = locate_str("fn f() -> std::io::Result<Vec<u8>> { Ok(vec![]) }");
        assert!(
            r.candidates.is_empty(),
            "found {:?}",
            r.candidates
                .iter()
                .map(|c| (&c.operator, &c.before))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_modules_are_excluded_because_they_are_the_grader() {
        // nedb's engine carries 71 tests inline. A mutation inside one makes the
        // suite fail because the TEST changed, and the row would teach a model
        // to edit a test rather than repair code.
        //
        // The assertion is about WHERE candidates come from, not how many. An
        // earlier version expected exactly one candidate from `1 < 2` and got
        // three — because the comparison AND both integer literals are sites.
        // The code was right; the expectation was wrong. Counting is the fragile
        // way to test this; provenance is the durable one.
        let src =
            "fn real() -> bool { 1 < 2 }\n#[cfg(test)]\nmod tests { fn t() -> bool { 3 < 4 } }\n";
        let r = locate_str(src);
        assert!(!r.candidates.is_empty(), "the non-test fn must yield sites");
        assert!(
            r.candidates.iter().all(|c| c.line == 1),
            "every candidate must come from the real fn on line 1, got lines {:?}",
            r.candidates.iter().map(|c| c.line).collect::<Vec<_>>()
        );
        // And specifically: nothing from inside the module.
        assert!(
            !r.candidates
                .iter()
                .any(|c| c.before == "3" || c.before == "4"),
            "a literal from inside the test module leaked in"
        );
        assert!(
            r.excluded_test_bytes > 0,
            "the module's bytes were not counted"
        );
    }

    #[test]
    fn suffixed_integers_keep_their_suffix() {
        // `7u64` -> `8u64`. Dropping the suffix is a TYPE CHANGE, which the
        // compiler catches — a loud defect whose error names the fix, i.e. a
        // worthless row.
        assert_eq!(bump_int("7u64").as_deref(), Some("8u64"));
        assert_eq!(bump_int("1_000i32").as_deref(), Some("1001i32"));
        assert_eq!(bump_int("0").as_deref(), Some("1"));
    }

    #[test]
    fn floats_hex_and_binary_are_refused_rather_than_guessed() {
        assert_eq!(bump_int("1.5"), None);
        assert_eq!(bump_int("0xFF"), None);
        assert_eq!(bump_int("0b1010"), None);
        assert_eq!(bump_int("1e9"), None);
    }

    #[test]
    fn bools_flip() {
        let r = locate_str("fn f() -> bool { true }");
        assert_eq!(r.candidates.len(), 1);
        assert_eq!(r.candidates[0].operator, "constant_perturb");
        assert_eq!(r.candidates[0].after, "false");
    }

    #[test]
    fn the_sha_matches_a_known_vector() {
        // Computed independently, not from memory: SHA-256 of "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_candidates_span_contains_exactly_what_it_claims() {
        // The forge re-checks this before splicing, but a locator that is wrong
        // about its own spans should fail HERE, not 3,000 trials later.
        let src = "fn f(a: u8) -> bool { a >= 3 && true }";
        let r = locate_str(src);
        assert!(!r.candidates.is_empty());
        for c in &r.candidates {
            assert_eq!(
                &src[c.start_byte..c.end_byte],
                c.before,
                "operator {} claims {:?} at {}..{}",
                c.operator,
                c.before,
                c.start_byte,
                c.end_byte
            );
        }
    }

    fn locate_str(src: &str) -> Report {
        let dir = std::env::temp_dir().join(format!("rl{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("t{}.rs", src.len()));
        std::fs::write(&p, src).unwrap();
        locate(p.to_str().unwrap())
    }
}
