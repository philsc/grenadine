//! Char-level diffs within a change block: which removed and added lines
//! pair up, and which chars of a paired line actually changed.

use std::ops::Range;

use similar::{Algorithm, DiffOp, TextDiff};

use crate::rebase::Block;

/// Lines less similar than this never pair up.
pub const MIN_SIMILARITY: f32 = 0.5;
/// Blocks with more removed×added line pairs than this are aligned
/// positionally.
pub const MAX_ALIGN_CELLS: usize = 10_000;
/// Lines longer than this (in chars) are never paired.
pub const MAX_LINE_CHARS: usize = 1_000;
/// Chars-gap below which two changed ranges on the same side merge.
const MIN_EQUAL_RUN: usize = 3;

/// One side-by-side row of a change block. Line indexes are absolute
/// 0-based indexes into the old and new texts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aligned {
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// The two lines are similar enough that char-level highlighting makes
    /// sense.
    pub paired: bool,
}

/// How similar two lines are, in [0, 1]. Long lines and lines whose lengths
/// alone rule out pairing score 0 without running a diff.
fn sim(a: &str, b: &str) -> f32 {
    let (la, lb) = (a.chars().count(), b.chars().count());
    if la > MAX_LINE_CHARS || lb > MAX_LINE_CHARS {
        return 0.0;
    }
    if la + lb == 0 {
        return 1.0;
    }
    if (2 * la.min(lb)) as f32 / ((la + lb) as f32) < MIN_SIMILARITY {
        return 0.0;
    }
    TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_chars(a, b)
        .ratio()
}

/// Pairs the `r`-th removed and added lines positionally.
fn positional(olds: &[&str], news: &[&str], block: &Block, check_sim: bool) -> Vec<Aligned> {
    (0..olds.len().max(news.len()))
        .map(|r| Aligned {
            old: (r < olds.len()).then(|| block.old.start + r),
            new: (r < news.len()).then(|| block.new.start + r),
            paired: check_sim
                && r < olds.len()
                && r < news.len()
                && sim(olds[r], news[r]) >= MIN_SIMILARITY,
        })
        .collect()
}

/// Emits unpaired rows packing `olds_len` removed and `news_len` added
/// lines starting at `os`/`ns` (indexes within the block).
fn pack(
    rows: &mut Vec<Aligned>,
    block: &Block,
    os: usize,
    ns: usize,
    olds_len: usize,
    news_len: usize,
) {
    for r in 0..olds_len.max(news_len) {
        rows.push(Aligned {
            old: (r < olds_len).then(|| block.old.start + os + r),
            new: (r < news_len).then(|| block.new.start + ns + r),
            paired: false,
        });
    }
}

/// Aligns the removed and added lines of `block` into side-by-side rows,
/// pairing the lines that look like edits of each other.
pub fn align(old: &[&str], new: &[&str], block: &Block) -> Vec<Aligned> {
    let (olds, news) = (&old[block.old.clone()], &new[block.new.clone()]);
    let (m, n) = (olds.len(), news.len());
    if m == 0 || n == 0 {
        return positional(olds, news, block, false);
    }
    if m * n > MAX_ALIGN_CELLS {
        return positional(olds, news, block, true);
    }
    let s: Vec<Vec<f32>> = olds
        .iter()
        .map(|a| news.iter().map(|b| sim(a, b)).collect())
        .collect();
    // Order-preserving alignment maximizing total similarity; the diagonal
    // is only a candidate when the lines are similar enough.
    let mut dp = vec![vec![0f32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            let mut best = dp[i - 1][j].max(dp[i][j - 1]);
            if s[i - 1][j - 1] >= MIN_SIMILARITY {
                best = best.max(dp[i - 1][j - 1] + s[i - 1][j - 1]);
            }
            dp[i][j] = best;
        }
    }
    // Backtrack, preferring the diagonal on ties so matches are kept.
    let mut matches = Vec::new();
    let (mut i, mut j) = (m, n);
    while i > 0 && j > 0 {
        if s[i - 1][j - 1] >= MIN_SIMILARITY && dp[i][j] == dp[i - 1][j - 1] + s[i - 1][j - 1] {
            matches.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if dp[i][j] == dp[i - 1][j] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    let mut rows = Vec::new();
    let (mut oi, mut ni) = (0, 0);
    for &(mi, mj) in matches.iter().rev() {
        pack(&mut rows, block, oi, ni, mi - oi, mj - ni);
        rows.push(Aligned {
            old: Some(block.old.start + mi),
            new: Some(block.new.start + mj),
            paired: true,
        });
        oi = mi + 1;
        ni = mj + 1;
    }
    pack(&mut rows, block, oi, ni, m - oi, n - ni);
    rows
}

/// Merges ranges whose gap is under MIN_EQUAL_RUN chars.
fn merge(ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start - last.end < MIN_EQUAL_RUN => last.end = r.end,
            _ => out.push(r),
        }
    }
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Scores the gap before `t[i]` as a boundary for a changed range: line
/// edges and token starts beat gaps inside a word.
fn boundary(t: &[char], i: usize) -> u32 {
    if i == 0 || i == t.len() {
        return 6;
    }
    let (a, b) = (t[i - 1], t[i]);
    let mut score = if a.is_whitespace() || b.is_whitespace() {
        2
    } else if is_word(a) && is_word(b) {
        0
    } else {
        1
    };
    if !is_word(a) && is_word(b) {
        score += 1;
    }
    score
}

/// Slides a pure insertion or deletion within its surrounding equal runs
/// to the position that best sits on word boundaries. Equally minimal diffs
/// can put the same change in several spots; word boundaries read better.
/// `lo`/`hi` bound the slide so the range never enters another change.
fn slide(t: &[char], r: Range<usize>, lo: usize, hi: usize) -> Range<usize> {
    let len = r.end - r.start;
    let mut start = r.start;
    while start > lo && t[start - 1] == t[start + len - 1] {
        start -= 1;
    }
    let mut best = start;
    let mut best_score = boundary(t, start) + boundary(t, start + len);
    while start + len < hi && t[start] == t[start + len] {
        start += 1;
        let score = boundary(t, start) + boundary(t, start + len);
        if score > best_score {
            best_score = score;
            best = start;
        }
    }
    best..best + len
}

/// Char-index ranges (Unicode scalar values, not bytes) of the changed text
/// on each side.
pub fn changed_ranges(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_chars(old, new);
    let ops = diff.ops();
    let (old_chars, new_chars): (Vec<char>, Vec<char>) =
        (old.chars().collect(), new.chars().collect());
    let (mut dels, mut inss) = (Vec::new(), Vec::new());
    for (k, op) in ops.iter().enumerate() {
        // Slide only pure insertions/deletions; Replace ops don't move.
        let old_side = match op {
            DiffOp::Delete { .. } => true,
            DiffOp::Insert { .. } => false,
            DiffOp::Replace { .. } => {
                dels.push(op.old_range());
                inss.push(op.new_range());
                continue;
            }
            DiffOp::Equal { .. } => continue,
        };
        let side = |o: &DiffOp| {
            if old_side {
                o.old_range()
            } else {
                o.new_range()
            }
        };
        let r = side(op);
        let lo = (k > 0)
            .then(|| &ops[k - 1])
            .filter(|o| matches!(o, DiffOp::Equal { .. }))
            .map(|o| side(o).start)
            .unwrap_or(r.start);
        let hi = ops
            .get(k + 1)
            .filter(|o| matches!(o, DiffOp::Equal { .. }))
            .map(|o| side(o).end)
            .unwrap_or(r.end);
        let t = if old_side { &old_chars } else { &new_chars };
        if old_side {
            dels.push(slide(t, r, lo, hi));
        } else {
            inss.push(slide(t, r, lo, hi));
        }
    }
    (merge(dels), merge(inss))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(old: Range<usize>, new: Range<usize>) -> Block {
        Block { old, new }
    }

    /// The line's chars covered by each range.
    fn highlighted(line: &str, ranges: &[Range<usize>]) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        ranges
            .iter()
            .map(|r| chars[r.clone()].iter().collect())
            .collect()
    }

    #[test]
    fn example() {
        let old = "server_status_->AddSentPacket(server_index_, channel_, fetch_now);";
        let new = "server_status_->AddSentPacketLater(server_index_, fetch_now);";
        let (dels, inss) = changed_ranges(old, new);
        assert_eq!(highlighted(new, &inss), ["Later"]);
        assert_eq!(highlighted(old, &dels), ["channel_, "]);
    }

    #[test]
    fn slides_to_word_boundaries() {
        let new = "f(a, x, b)";
        let (dels, inss) = changed_ranges("f(a, b)", new);
        assert_eq!(highlighted(new, &inss), ["x, "]);
        assert!(dels.is_empty());
    }

    #[test]
    fn small_islands_merge() {
        // Two changes with a 2-char unchanged run between them merge.
        let (dels, _) = changed_ranges("abXcdYef", "abPcdQef");
        assert_eq!(highlighted("abXcdYef", &dels), ["XcdY"]);
        // A 3-char unchanged run keeps the changes split.
        let (dels, _) = changed_ranges("abXcdeYf", "abPcdeQf");
        assert_eq!(highlighted("abXcdeYf", &dels), ["X", "Y"]);
    }

    #[test]
    fn leading_whitespace() {
        let new = "    foo();";
        let (dels, inss) = changed_ranges("foo();", new);
        assert_eq!(highlighted(new, &inss), ["    "]);
        assert!(dels.is_empty());
    }

    #[test]
    fn ranges_are_char_indexes() {
        let (dels, inss) = changed_ranges("café au lait", "café au lait!");
        assert!(dels.is_empty());
        assert_eq!(inss[0], 12..13);
        let new = "→ y";
        let (dels, inss) = changed_ranges("→ x", new);
        assert_eq!(highlighted("→ x", &dels), ["x"]);
        assert_eq!(highlighted(new, &inss), ["y"]);
    }

    #[test]
    fn pairs_edited_lines() {
        let old = ["aaa fn foo(x)", "bbb fn foo(x)"];
        let new = ["unrelated", "aaa fn bar(x)", "bbb fn bar(x)"];
        assert_eq!(
            align(&old, &new, &block(0..2, 0..3)),
            [
                Aligned {
                    old: None,
                    new: Some(0),
                    paired: false
                },
                Aligned {
                    old: Some(0),
                    new: Some(1),
                    paired: true
                },
                Aligned {
                    old: Some(1),
                    new: Some(2),
                    paired: true
                },
            ]
        );
    }

    #[test]
    fn dissimilar_lines_pack_unpaired() {
        let old = ["p", "q"];
        let new = ["r", "s"];
        assert_eq!(
            align(&old, &new, &block(0..2, 0..2)),
            [
                Aligned {
                    old: Some(0),
                    new: Some(0),
                    paired: false
                },
                Aligned {
                    old: Some(1),
                    new: Some(1),
                    paired: false
                },
            ]
        );
    }

    #[test]
    fn barely_dissimilar_stays_unpaired() {
        // Three equal chars of seven: a ratio under 0.5 does not pair.
        let old = ["abcdefg"];
        let new = ["abcxwyz"];
        let rows = align(&old, &new, &block(0..1, 0..1));
        assert!(!rows[0].paired);
    }

    #[test]
    fn long_lines_never_pair() {
        let long = "x".repeat(MAX_LINE_CHARS + 1);
        let old = [long.as_str()];
        let new = [long.as_str()];
        let rows = align(&old, &new, &block(0..1, 0..1));
        assert!(!rows[0].paired);
    }

    #[test]
    fn huge_blocks_align_positionally() {
        let old: Vec<String> = (0..101).map(|i| format!("line {i} foo")).collect();
        let mut new: Vec<String> = (0..101).map(|i| format!("line {i} bar")).collect();
        new[50] = "completely different".into();
        let old: Vec<&str> = old.iter().map(|s| s.as_str()).collect();
        let new: Vec<&str> = new.iter().map(|s| s.as_str()).collect();
        let rows = align(&old, &new, &block(0..101, 0..101));
        assert_eq!(rows.len(), 101);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.old, Some(i));
            assert_eq!(r.new, Some(i));
            assert_eq!(r.paired, i != 50);
        }
    }
}
