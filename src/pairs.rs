//! Pair mutation: turn the survivors into the biggest data source there is.
//!
//! THE INSIGHT THIS MODULE EXISTS FOR. Roughly 65% of single mutations SURVIVE —
//! no suite notices them. Up to now that was reported as a coverage finding and
//! otherwise discarded, which meant two thirds of every run's work produced no
//! training data at all.
//!
//! But a survivor is not waste. It is a known-quantity defect: we have already
//! PROVEN, by execution, that it alone cannot be detected. Take two of them and
//! apply both, and one of three things happens:
//!
//! - Still green. Two invisible defects stay invisible. A deeper coverage
//!   finding, and honest evidence about how much of a module is unguarded.
//! - RED. **This is the prize.** Two changes that are individually undetectable
//!   become detectable together — an emergent defect. Neither site is
//!   individually blameable by the test output, so repairing it requires
//!   reasoning about the interaction rather than pattern-matching one line to
//!   one error. That is the class real bugs live in, and single-mutation corpora
//!   cannot produce it.
//! - Hung. Same as anywhere: a kill, recorded separately.
//!
//! MEASURED, AND THE FIRST HYPOTHESIS WAS WRONG. Pairing survivor-with-survivor
//! was expected to be the biggest data source available. On nedb it produced
//! **1 emergent kill in 40 pairs — 2.5%**, far worse than single mutation's
//! 22-39%.
//!
//! The reason is structural and should have been predicted: a survivor survives
//! because its code path is NOT EXERCISED. Two unexercised lines in the same
//! unexercised function are still unexercised — pairing does not manufacture
//! coverage. Nearly every sampled pair landed in `autoindex.query`, a function
//! the suite barely touches, and combining two invisible changes there stayed
//! invisible.
//!
//! So the mode that pays is `Mode::KillerPlusSurvivor`: pair a mutation already
//! PROVEN to kill with one already PROVEN to survive. The killer guarantees the
//! tree goes red, so yield is ~100%, and the resulting row has a property no
//! single-site row can have — **the test output points at one site while two
//! need repairing.** That is the shape of a real fix that treats the visible
//! symptom and leaves a latent defect behind.
//!
//! Survivor-with-survivor is kept, because the 2.5% it finds are genuinely
//! emergent and nothing else produces them. It is just not the volume play.
//!
//! ON MINIMALITY, stated rather than glossed. If A and B each survive alone,
//! then reverting EITHER one restores green, so "revert both" is a correct
//! repair but not the only one. The completion therefore reverts both and the
//! row is honest about being a two-site repair. Real fixes are not always
//! minimal either, and a model that restores green is a model that worked.
//!
//! PAIRS ARE SAMPLED, NOT ENUMERATED, and the sampling is DETERMINISTIC — the
//! same trials file and the same seed produce the same pairs, or a "reproducible
//! corpus" is not one.

use crate::model::{Candidate, Trial, Verdict};
use serde::{Deserialize, Serialize};

/// How two mutations are related, which is a proxy for whether they can
/// plausibly interact at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Affinity {
    /// Same file, same enclosing def/class. Highest chance of interaction, and
    /// the repair is a single localised region a model can actually reason about.
    SameScope,
    /// Same file, different scopes. Plausible: one function's output feeds
    /// another's input.
    SameFile,
    /// Different files. Kept at a low rate — mostly these are just two
    /// independent bugs, which teaches less than one coupled one.
    CrossFile,
}

impl Affinity {
    pub fn label(&self) -> &'static str {
        match self {
            Affinity::SameScope => "same-scope",
            Affinity::SameFile => "same-file",
            Affinity::CrossFile => "cross-file",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pair {
    pub a_file: String,
    pub a: Candidate,
    pub b_file: String,
    pub b: Candidate,
    pub affinity: Affinity,
}

impl Pair {
    /// Deterministic id over both sites, order-independent so (A,B) and (B,A)
    /// are the same pair and cannot both enter the corpus.
    pub fn key(&self) -> String {
        let mut ends = [
            format!("{}:{}:{}", self.a_file, self.a.start_byte, self.a.operator),
            format!("{}:{}:{}", self.b_file, self.b.start_byte, self.b.operator),
        ];
        ends.sort();
        format!("{}|{}", ends[0], ends[1])
    }
}

/// A tiny deterministic PRNG.
///
/// Hand-rolled rather than pulling `rand`: this needs reproducibility, not
/// statistical quality, and a dependency whose version could change the
/// sequence would silently change which corpus a given seed produces.
/// SplitMix64 — small, well-known, and fixed forever.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }
    /// Named `bits` rather than `next` so it cannot be confused with
    /// `Iterator::next` — clippy is right that a `next` on a non-iterator is a
    /// trap for a reader skimming a call site.
    pub fn bits(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.bits() % n as u64) as usize
        }
    }
}

/// Which population to draw the two sites from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Both sites individually undetectable. Finds genuinely emergent defects,
    /// at ~2.5% on nedb — low, because survivors share the cause of their
    /// survival (an unexercised path).
    SurvivorPair,
    /// One site proven to kill, one proven to survive. Red is guaranteed by the
    /// killer, so nearly every pair is usable, and the failing output
    /// under-determines the repair — which is the point.
    KillerPlusSurvivor,
}

/// Every mutation a previous run PROVED to be individually undetectable.
pub fn survivors(trials: &[Trial]) -> Vec<(&str, &Candidate)> {
    trials
        .iter()
        .filter(|t| t.verdict == Verdict::Survived)
        .map(|t| (t.file.as_str(), &t.candidate))
        .collect()
}

/// Every mutation a previous run PROVED to be detectable.
pub fn killers(trials: &[Trial]) -> Vec<(&str, &Candidate)> {
    trials
        .iter()
        .filter(|t| t.verdict != Verdict::Survived)
        .map(|t| (t.file.as_str(), &t.candidate))
        .collect()
}

/// Sample pairs from the survivor set.
///
/// The mix is deliberate and stated in one place so it can be argued with:
/// same-scope pairs are the ones worth having, so they are drawn first and
/// exhaustively within a scope; same-file next; cross-file is capped at a small
/// fraction because two unrelated bugs in two unrelated modules is mostly just
/// two easy problems in one prompt.
pub fn sample(
    trials: &[Trial],
    want: usize,
    seed: u64,
    cross_file_pct: usize,
    mode: Mode,
) -> Vec<Pair> {
    match mode {
        Mode::SurvivorPair => {
            let pool = survivors(trials);
            sample_within(&pool, want, seed, cross_file_pct)
        }
        Mode::KillerPlusSurvivor => {
            let k = killers(trials);
            let s = survivors(trials);
            sample_across(&k, &s, want, seed, cross_file_pct)
        }
    }
}

fn affinity_of(af: &str, ac: &Candidate, bf: &str, bc: &Candidate) -> Affinity {
    if af != bf {
        Affinity::CrossFile
    } else if ac.scope == bc.scope {
        Affinity::SameScope
    } else {
        Affinity::SameFile
    }
}

fn mk((af, ac): (&str, &Candidate), (bf, bc): (&str, &Candidate)) -> Pair {
    Pair {
        a_file: af.to_string(),
        a: ac.clone(),
        b_file: bf.to_string(),
        b: bc.clone(),
        affinity: affinity_of(af, ac, bf, bc),
    }
}

/// Two sites drawn from ONE population.
///
/// ROUND-ROBIN ACROSS SCOPES, not exhaustive within one. The first version
/// enumerated each scope to completion, and a single busy function
/// (`autoindex.query`) consumed the entire budget of a 40-pair run — so the
/// sample described one function rather than the module. Breadth first, depth
/// only if budget remains.
fn sample_within(
    pool: &[(&str, &Candidate)],
    want: usize,
    seed: u64,
    cross_file_pct: usize,
) -> Vec<Pair> {
    if pool.len() < 2 {
        return Vec::new();
    }
    let mut rng = Rng::new(seed);
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Pair> = Vec::new();

    let mut by_scope: std::collections::BTreeMap<(String, String), Vec<usize>> =
        std::collections::BTreeMap::new();
    let mut by_file: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, (f, c)) in pool.iter().enumerate() {
        by_scope
            .entry((f.to_string(), c.scope.clone()))
            .or_default()
            .push(i);
        by_file.entry(f.to_string()).or_default().push(i);
    }

    // Round-robin: take the k-th available pair from every scope in turn.
    let buckets: Vec<&Vec<usize>> = by_scope.values().filter(|v| v.len() >= 2).collect();
    let mut k = 0usize;
    loop {
        let mut progressed = false;
        for idxs in &buckets {
            let combos = idxs.len() * (idxs.len() - 1) / 2;
            if k >= combos {
                continue;
            }
            progressed = true;
            // k-th combination, deterministically.
            let mut n = k;
            let mut a = 0usize;
            while n >= idxs.len() - a - 1 {
                n -= idxs.len() - a - 1;
                a += 1;
            }
            let b = a + 1 + n;
            let p = mk(pool[idxs[a]], pool[idxs[b]]);
            if seen.insert(p.key()) {
                out.push(p);
                if out.len() >= want {
                    return out;
                }
            }
        }
        if !progressed {
            break;
        }
        k += 1;
    }

    // Same file, different scope.
    let files: Vec<&String> = by_file.keys().collect();
    let mut guard = 0usize;
    while out.len() < want && guard < want * 40 {
        guard += 1;
        let f = files[rng.below(files.len())];
        let idxs = &by_file[f];
        if idxs.len() < 2 {
            continue;
        }
        let i = idxs[rng.below(idxs.len())];
        let j = idxs[rng.below(idxs.len())];
        if i == j || pool[i].1.scope == pool[j].1.scope {
            continue;
        }
        let p = mk(pool[i], pool[j]);
        if seen.insert(p.key()) {
            out.push(p);
        }
    }

    // Cross-file, capped.
    let cross_budget = want * cross_file_pct / 100;
    let mut cross = 0usize;
    let mut guard = 0usize;
    while out.len() < want && cross < cross_budget && guard < want * 40 {
        guard += 1;
        let i = rng.below(pool.len());
        let j = rng.below(pool.len());
        if i == j || pool[i].0 == pool[j].0 {
            continue;
        }
        let p = mk(pool[i], pool[j]);
        if seen.insert(p.key()) {
            out.push(p);
            cross += 1;
        }
    }
    out
}

/// One site from each of TWO populations — the killer/survivor mode.
///
/// Biased hard toward same-file, and within that toward same-scope, because a
/// killer in one module plus a survivor in an unrelated one is just a
/// single-site problem with a decoration attached.
fn sample_across(
    a_pool: &[(&str, &Candidate)],
    b_pool: &[(&str, &Candidate)],
    want: usize,
    seed: u64,
    cross_file_pct: usize,
) -> Vec<Pair> {
    if a_pool.is_empty() || b_pool.is_empty() {
        return Vec::new();
    }
    let mut rng = Rng::new(seed);
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Pair> = Vec::new();

    // Pass 1: same scope. Pass 2: same file. Pass 3: cross-file, capped.
    for pass in 0..3u8 {
        let budget = if pass == 2 {
            want * cross_file_pct / 100
        } else {
            want
        };
        let mut taken = 0usize;
        for (i, a) in a_pool.iter().enumerate() {
            for (j, b) in b_pool.iter().enumerate() {
                if out.len() >= want || taken >= budget {
                    break;
                }
                // Never pair a site with itself, and never overlap.
                if a.0 == b.0 && a.1.start_byte == b.1.start_byte {
                    continue;
                }
                let aff = affinity_of(a.0, a.1, b.0, b.1);
                let wanted = match pass {
                    0 => aff == Affinity::SameScope,
                    1 => aff == Affinity::SameFile,
                    _ => aff == Affinity::CrossFile,
                };
                if !wanted {
                    continue;
                }
                // Deterministic thinning so a huge cross product does not all
                // come from the first killer in the list.
                if pass == 2 && rng.below(4) != 0 {
                    continue;
                }
                let _ = (i, j);
                let p = mk(*a, *b);
                if seen.insert(p.key()) {
                    out.push(p);
                    taken += 1;
                }
            }
            if out.len() >= want || taken >= budget {
                break;
            }
        }
    }
    out
}

/// Apply two mutations to (possibly the same) file content.
///
/// Sites in the SAME file are applied from the highest offset down, because
/// splicing the earlier one first shifts every later offset and the second
/// splice would land in the wrong place — silently, producing a file that
/// compiles and means something nobody asked for.
pub fn apply_same_file(original: &[u8], a: &Candidate, b: &Candidate) -> Result<Vec<u8>, String> {
    let (first, second) = if a.start_byte >= b.start_byte {
        (a, b)
    } else {
        (b, a)
    };
    if second.end_byte > first.start_byte {
        return Err(format!(
            "overlapping sites ({}..{} and {}..{}) — a pair must be two distinct edits",
            second.start_byte, second.end_byte, first.start_byte, first.end_byte
        ));
    }
    let mut out = original.to_vec();
    for c in [first, second] {
        if c.end_byte > out.len() {
            return Err("site is past the end of the file".into());
        }
        let mut next = Vec::with_capacity(out.len());
        next.extend_from_slice(&out[..c.start_byte]);
        next.extend_from_slice(c.after.as_bytes());
        next.extend_from_slice(&out[c.end_byte..]);
        out = next;
    }
    Ok(out)
}

pub fn apply_one(original: &[u8], c: &Candidate) -> Result<Vec<u8>, String> {
    if c.end_byte > original.len() {
        return Err("site is past the end of the file".into());
    }
    let mut out = Vec::with_capacity(original.len());
    out.extend_from_slice(&original[..c.start_byte]);
    out.extend_from_slice(c.after.as_bytes());
    out.extend_from_slice(&original[c.end_byte..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(
        op: &str,
        start: usize,
        end: usize,
        before: &str,
        after: &str,
        scope: &str,
    ) -> Candidate {
        Candidate {
            operator: op.into(),
            start_byte: start,
            end_byte: end,
            before: before.into(),
            after: after.into(),
            line: 1,
            context: String::new(),
            scope: scope.into(),
            severity: "silent".into(),
        }
    }

    #[test]
    fn two_sites_in_one_file_are_applied_from_the_end_backwards() {
        // THE BUG THIS PREVENTS: splicing the earlier site first shifts every
        // later offset, so the second splice lands in the wrong place — and it
        // does so SILENTLY, producing a file that still compiles and means
        // something nobody asked for.
        let src = b"aaaa BBBB cccc DDDD eeee".to_vec();
        // Replace "BBBB" (5..9) with "x" and "DDDD" (15..19) with "yy".
        let a = cand("one", 5, 9, "BBBB", "x", "f");
        let b = cand("two", 15, 19, "DDDD", "yy", "f");
        let got = apply_same_file(&src, &a, &b).unwrap();
        assert_eq!(String::from_utf8(got).unwrap(), "aaaa x cccc yy eeee");
        // Order of arguments must not matter.
        let got2 = apply_same_file(&src, &b, &a).unwrap();
        assert_eq!(String::from_utf8(got2).unwrap(), "aaaa x cccc yy eeee");
    }

    #[test]
    fn overlapping_sites_are_refused_not_silently_mangled() {
        let src = b"abcdefghij".to_vec();
        let a = cand("one", 2, 6, "cdef", "X", "f");
        let b = cand("two", 4, 8, "efgh", "Y", "f");
        let err = apply_same_file(&src, &a, &b).unwrap_err();
        assert!(err.contains("overlapping"), "{err}");
    }

    #[test]
    fn a_pair_key_is_order_independent() {
        // (A,B) and (B,A) are the same experiment; letting both into the corpus
        // would duplicate rows and inflate the yield number.
        let a = cand("one", 5, 9, "x", "y", "f");
        let b = cand("two", 15, 19, "p", "q", "f");
        let p1 = Pair {
            a_file: "f.py".into(),
            a: a.clone(),
            b_file: "f.py".into(),
            b: b.clone(),
            affinity: Affinity::SameScope,
        };
        let p2 = Pair {
            a_file: "f.py".into(),
            a: b,
            b_file: "f.py".into(),
            b: a,
            affinity: Affinity::SameScope,
        };
        assert_eq!(p1.key(), p2.key());
    }

    #[test]
    fn killer_plus_survivor_never_pairs_a_site_with_itself() {
        // Both pools come from the same trials file, so an identical site could
        // appear in both if the filter were wrong — producing a "pair" that is
        // one edit applied twice, which would silently be a single-site row
        // wearing a two-site label.
        let c = cand("one", 5, 9, "x", "y", "f");
        let k: Vec<(&str, &Candidate)> = vec![("a.py", &c)];
        let s: Vec<(&str, &Candidate)> = vec![("a.py", &c)];
        assert!(super::sample_across(&k, &s, 10, 1, 10).is_empty());
    }

    #[test]
    fn sampling_is_deterministic_for_a_given_seed() {
        // A corpus that cannot be regenerated is not reproducible, so the PRNG
        // is hand-rolled (SplitMix64) rather than taken from a dependency whose
        // version could change the sequence under us.
        let mut r1 = Rng::new(42);
        let mut r2 = Rng::new(42);
        let a: Vec<u64> = (0..8).map(|_| r1.bits()).collect();
        let b: Vec<u64> = (0..8).map(|_| r2.bits()).collect();
        assert_eq!(a, b);
        let mut r3 = Rng::new(43);
        assert_ne!(a[0], r3.bits());
    }
}
