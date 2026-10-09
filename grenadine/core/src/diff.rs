//! Lays out change blocks as hunks with context, for unified and
//! side-by-side display.

use crate::inline::Aligned;
use crate::rebase::Block;

/// One line of a unified diff. Line numbers are 0-based indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    Context { old: usize, new: usize },
    Removed { old: usize, block: usize },
    Added { new: usize, block: usize },
}

/// One row of a side-by-side diff: the old line on the left and the new on
/// the right, either of which may be blank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// The change block the row belongs to; `None` for context.
    pub block: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub lines: Vec<Line>,
    pub rows: Vec<Row>,
    /// Lines hidden between the previous hunk (or the start) and this one.
    pub skipped_before: usize,
}

/// Groups `blocks` into hunks with `context` unchanged lines around each
/// change. `aligned[k]` is the row alignment of `blocks[k]`, and `old_len`
/// and `new_len` are the line counts of the two texts.
pub fn hunks(
    blocks: &[Block],
    aligned: &[Vec<Aligned>],
    old_len: usize,
    new_len: usize,
    context: usize,
) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    // The old line just past the last emitted one.
    let mut old_pos = 0usize;
    let mut i = 0;
    while i < blocks.len() {
        // Extend the hunk while the gap to the next block is small enough
        // that the context would touch.
        let mut j = i;
        while j + 1 < blocks.len() && blocks[j + 1].old.start - blocks[j].old.end <= 2 * context {
            j += 1;
        }
        let start_old = blocks[i].old.start.saturating_sub(context).max(old_pos);
        let delta = blocks[i].old.start - start_old;
        let start_new = blocks[i].new.start - delta;
        let mut hunk = Hunk {
            lines: Vec::new(),
            rows: Vec::new(),
            skipped_before: start_old - old_pos,
        };
        let (mut o, mut n) = (start_old, start_new);
        for (k, b) in blocks.iter().enumerate().take(j + 1).skip(i) {
            while o < b.old.start {
                hunk.lines.push(Line::Context { old: o, new: n });
                hunk.rows.push(Row {
                    old: Some(o),
                    new: Some(n),
                    block: None,
                });
                o += 1;
                n += 1;
            }
            for old in b.old.clone() {
                hunk.lines.push(Line::Removed { old, block: k });
            }
            for new in b.new.clone() {
                hunk.lines.push(Line::Added { new, block: k });
            }
            for a in &aligned[k] {
                hunk.rows.push(Row {
                    old: a.old,
                    new: a.new,
                    block: Some(k),
                });
            }
            o = b.old.end;
            n = b.new.end;
        }
        let end_old = (o + context).min(old_len);
        while o < end_old && n < new_len {
            hunk.lines.push(Line::Context { old: o, new: n });
            hunk.rows.push(Row {
                old: Some(o),
                new: Some(n),
                block: None,
            });
            o += 1;
            n += 1;
        }
        old_pos = o;
        out.push(hunk);
        i = j + 1;
    }
    out
}

/// The unchanged lines after the last hunk.
pub fn skipped_after(hunks: &[Hunk], old_len: usize) -> usize {
    let end = hunks
        .last()
        .and_then(|h| {
            h.lines.iter().rev().find_map(|l| match l {
                Line::Context { old, .. } | Line::Removed { old, .. } => Some(old + 1),
                Line::Added { .. } => None,
            })
        })
        .unwrap_or(0);
    old_len.saturating_sub(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inline::align;
    use crate::rebase::{change_blocks, lines};

    fn alignments(o: &[&str], n: &[&str], blocks: &[Block]) -> Vec<Vec<Aligned>> {
        blocks.iter().map(|b| align(o, n, b)).collect()
    }

    fn render(old: &str, new: &str, context: usize) -> Vec<String> {
        let (o, n) = (lines(old), lines(new));
        let blocks = change_blocks(&o, &n);
        hunks(
            &blocks,
            &alignments(&o, &n, &blocks),
            o.len(),
            n.len(),
            context,
        )
        .iter()
        .flat_map(|h| {
            std::iter::once(format!("@@ skip {}", h.skipped_before)).chain(h.lines.iter().map(
                |l| match *l {
                    Line::Context { old, .. } => format!(" {}", o[old]),
                    Line::Removed { old, .. } => format!("-{}", o[old]),
                    Line::Added { new, .. } => format!("+{}", n[new]),
                },
            ))
        })
        .collect()
    }

    #[test]
    fn context_around_one_change() {
        let old = "1\n2\n3\n4\n5\n6\n7\n";
        let new = "1\n2\n3\nfour\n5\n6\n7\n";
        assert_eq!(
            render(old, new, 1),
            ["@@ skip 2", " 3", "-4", "+four", " 5"]
        );
    }

    #[test]
    fn nearby_changes_share_a_hunk() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let new = "one\n2\n3\n4\n5\nsix\n7\n8\n9\n";
        assert_eq!(render(old, new, 2).len(), 1 + 10);
        assert_eq!(
            render(old, new, 1)
                .iter()
                .filter(|l| l.starts_with("@@"))
                .count(),
            2
        );
    }

    #[test]
    fn additions_at_the_end() {
        assert_eq!(render("a\n", "a\nb\n", 3), ["@@ skip 0", " a", "+b"]);
    }

    #[test]
    fn side_by_side_pairs_lines() {
        let (o, n) = (lines("a\nb\nc\n"), lines("a\nx\ny\nc\n"));
        let blocks = change_blocks(&o, &n);
        let h = hunks(&blocks, &alignments(&o, &n, &blocks), o.len(), n.len(), 0);
        assert_eq!(
            h[0].rows,
            [
                Row {
                    old: Some(1),
                    new: Some(1),
                    block: Some(0)
                },
                Row {
                    old: None,
                    new: Some(2),
                    block: Some(0)
                },
            ]
        );
    }

    #[test]
    fn counts_trailing_lines() {
        let (o, n) = (lines("a\nb\nc\nd\n"), lines("A\nb\nc\nd\n"));
        let blocks = change_blocks(&o, &n);
        let h = hunks(&blocks, &alignments(&o, &n, &blocks), o.len(), n.len(), 1);
        assert_eq!(skipped_after(&h, o.len()), 2);
    }
}
