//! Lays out change blocks as hunks with context, for unified and
//! side-by-side display.

use std::ops::Range;

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
    /// The unchanged lines hidden between the previous hunk (or the start)
    /// and this one, if any.
    pub skipped_before: Option<Skip>,
}

/// Extra unchanged lines revealed in one gap, on top of the context.
/// Values past the gap's length are capped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Expand {
    /// Lines revealed just below the previous block, or the file's start.
    pub below_prev: usize,
    /// Lines revealed just above the next block, or the file's end.
    pub above_next: usize,
}

/// The unchanged lines hidden in one gap: the stretch before `blocks[gap]`,
/// or after the last block for `gap == blocks.len()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skip {
    pub gap: usize,
    /// The old lines still hidden.
    pub old: Range<usize>,
}

/// The old lines still hidden in gap `gap`: its stretch of unchanged lines
/// minus the context and the expansion at each end. The end toward a block
/// gets `context` lines; the file's start and end don't.
fn hidden_range(
    blocks: &[Block],
    expand: &[Expand],
    old_len: usize,
    context: usize,
    gap: usize,
) -> Option<Range<usize>> {
    let e = expand.get(gap).copied().unwrap_or_default();
    let start = gap.checked_sub(1).map_or(0, |b| blocks[b].old.end);
    let end = blocks.get(gap).map_or(old_len, |b| b.old.start);
    let top = start
        .saturating_add(if gap > 0 { context } else { 0 })
        .saturating_add(e.below_prev)
        .min(end);
    let bottom = end.saturating_sub(e.above_next.saturating_add(if gap < blocks.len() {
        context
    } else {
        0
    }));
    (top < bottom).then_some(top..bottom)
}

/// Groups `blocks` into hunks with `context` unchanged lines around each
/// change, plus the lines `expand` reveals. `expand` has an entry per gap:
/// gap `k` is the unchanged stretch before `blocks[k]`, and gap
/// `blocks.len()` the one after the last block; missing entries reveal
/// nothing extra. `aligned[k]` is the row alignment of `blocks[k]`, and
/// `old_len` and `new_len` are the line counts of the two texts.
pub fn hunks(
    blocks: &[Block],
    aligned: &[Vec<Aligned>],
    old_len: usize,
    new_len: usize,
    context: usize,
    expand: &[Expand],
) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    let hidden = |gap| hidden_range(blocks, expand, old_len, context, gap);
    // With no blocks the whole file is one gap; its visible top and bottom
    // each become a context-only hunk.
    if blocks.is_empty() {
        let mut push = |range: Range<usize>, skipped_before: Option<Skip>| {
            if range.is_empty() {
                return;
            }
            let mut hunk = Hunk {
                lines: Vec::new(),
                rows: Vec::new(),
                skipped_before,
            };
            for o in range {
                hunk.lines.push(Line::Context { old: o, new: o });
                hunk.rows.push(Row {
                    old: Some(o),
                    new: Some(o),
                    block: None,
                });
            }
            out.push(hunk);
        };
        match hidden(0) {
            None => push(0..old_len, None),
            Some(skip) => {
                push(0..skip.start, None);
                push(skip.end..old_len, Some(Skip { gap: 0, old: skip }));
            }
        }
        return out;
    }
    let mut i = 0;
    while i < blocks.len() {
        // Extend the hunk while the gap to the next block is fully shown.
        let mut j = i;
        while j + 1 < blocks.len() && hidden(j + 1).is_none() {
            j += 1;
        }
        let gap_start = i.checked_sub(1).map_or(0, |b| blocks[b].old.end);
        let start_old = hidden(i).map_or(gap_start, |r| r.end);
        let delta = blocks[i].old.start - start_old;
        let start_new = blocks[i].new.start - delta;
        let mut hunk = Hunk {
            lines: Vec::new(),
            rows: Vec::new(),
            skipped_before: hidden(i).map(|old| Skip { gap: i, old }),
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
        // A fully shown gap after the last block runs to the file's end.
        let end_old = hidden(j + 1).map_or(old_len, |r| r.start);
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
        out.push(hunk);
        i = j + 1;
    }
    out
}

/// The unchanged lines hidden after the last hunk: the gap after the last
/// block, which is the whole file when there are no blocks.
pub fn skipped_after(
    blocks: &[Block],
    expand: &[Expand],
    old_len: usize,
    context: usize,
) -> Option<Skip> {
    hidden_range(blocks, expand, old_len, context, blocks.len()).map(|old| Skip {
        gap: blocks.len(),
        old,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inline::align;
    use crate::rebase::{change_blocks, lines};

    fn alignments(o: &[&str], n: &[&str], blocks: &[Block]) -> Vec<Vec<Aligned>> {
        blocks.iter().map(|b| align(o, n, b)).collect()
    }

    fn render(old: &str, new: &str, context: usize, expand: &[Expand]) -> Vec<String> {
        let (o, n) = (lines(old), lines(new));
        let blocks = change_blocks(&o, &n);
        hunks(
            &blocks,
            &alignments(&o, &n, &blocks),
            o.len(),
            n.len(),
            context,
            expand,
        )
        .iter()
        .flat_map(|h| {
            let skipped = h.skipped_before.as_ref().map_or(0, |s| s.old.len());
            std::iter::once(format!("@@ skip {skipped}")).chain(h.lines.iter().map(|l| match *l {
                Line::Context { old, .. } => format!(" {}", o[old]),
                Line::Removed { old, .. } => format!("-{}", o[old]),
                Line::Added { new, .. } => format!("+{}", n[new]),
            }))
        })
        .collect()
    }

    /// Changes at lines 4 and 17, far enough apart to form separate hunks
    /// at `context` 1; the gaps hide lines 1..3, 5..16, and 18..20.
    fn two_changes() -> (String, String) {
        let mut new = (1..=20).map(|i| i.to_string()).collect::<Vec<_>>();
        new[3] = "four".into();
        new[16] = "seventeen".into();
        let old = (1..=20).map(|i| i.to_string()).collect::<Vec<_>>();
        (old.join("\n") + "\n", new.join("\n") + "\n")
    }

    #[test]
    fn context_around_one_change() {
        let old = "1\n2\n3\n4\n5\n6\n7\n";
        let new = "1\n2\n3\nfour\n5\n6\n7\n";
        assert_eq!(
            render(old, new, 1, &[]),
            ["@@ skip 2", " 3", "-4", "+four", " 5"]
        );
    }

    #[test]
    fn nearby_changes_share_a_hunk() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let new = "one\n2\n3\n4\n5\nsix\n7\n8\n9\n";
        assert_eq!(render(old, new, 2, &[]).len(), 1 + 10);
        assert_eq!(
            render(old, new, 1, &[])
                .iter()
                .filter(|l| l.starts_with("@@"))
                .count(),
            2
        );
    }

    #[test]
    fn additions_at_the_end() {
        assert_eq!(render("a\n", "a\nb\n", 3, &[]), ["@@ skip 0", " a", "+b"]);
    }

    #[test]
    fn side_by_side_pairs_lines() {
        let (o, n) = (lines("a\nb\nc\n"), lines("a\nx\ny\nc\n"));
        let blocks = change_blocks(&o, &n);
        let h = hunks(
            &blocks,
            &alignments(&o, &n, &blocks),
            o.len(),
            n.len(),
            0,
            &[],
        );
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
        assert_eq!(
            skipped_after(&blocks, &[], o.len(), 1),
            Some(Skip { gap: 1, old: 2..4 })
        );
    }

    #[test]
    fn expand_down_reveals_the_top_of_a_gap() {
        let (old, new) = two_changes();
        let expand = [
            Expand::default(),
            Expand {
                below_prev: 4,
                ..Expand::default()
            },
            Expand::default(),
        ];
        let out = render(&old, &new, 1, &expand);
        // The first hunk's context runs 4 lines further, and the gap's
        // hidden stretch shrinks to lines 10..16.
        assert_eq!(
            out[..10],
            [
                "@@ skip 2",
                " 3",
                "-4",
                "+four",
                " 5",
                " 6",
                " 7",
                " 8",
                " 9",
                "@@ skip 6"
            ]
        );
    }

    #[test]
    fn expand_up_reveals_the_bottom_of_a_gap() {
        let (old, new) = two_changes();
        let expand = [
            Expand::default(),
            Expand {
                above_next: 3,
                ..Expand::default()
            },
            Expand::default(),
        ];
        let out = render(&old, &new, 1, &expand);
        // The second hunk starts 3 lines earlier; the gap hides 6..13.
        assert_eq!(
            out[5..],
            [
                "@@ skip 7",
                " 13",
                " 14",
                " 15",
                " 16",
                "-17",
                "+seventeen",
                " 18"
            ]
        );
    }

    #[test]
    fn expanding_a_gap_merges_its_hunks() {
        let (old, new) = two_changes();
        let expand = [
            Expand::default(),
            Expand {
                below_prev: usize::MAX,
                above_next: usize::MAX,
            },
            Expand::default(),
        ];
        let out = render(&old, &new, 1, &expand);
        assert_eq!(out.iter().filter(|l| l.starts_with("@@")).count(), 1);
        assert!(out.contains(&"-17".to_owned()));
        assert!(out.contains(&" 16".to_owned()));
    }

    #[test]
    fn expand_reveals_the_file_start_and_end() {
        let (old, new) = two_changes();
        assert_eq!(render(&old, &new, 1, &[]).first().unwrap(), "@@ skip 2");
        let expand = [
            Expand {
                below_prev: 2,
                ..Expand::default()
            },
            Expand::default(),
            Expand {
                above_next: 2,
                ..Expand::default()
            },
        ];
        let out = render(&old, &new, 1, &expand);
        assert_eq!(out.first().unwrap(), "@@ skip 0");
        assert_eq!(out[1], " 1");
        assert_eq!(out.last().unwrap(), " 20");
    }

    #[test]
    fn expansion_past_the_gap_is_capped() {
        let (old, new) = two_changes();
        let expand = [
            Expand::default(),
            Expand {
                below_prev: usize::MAX,
                ..Expand::default()
            },
            Expand::default(),
        ];
        let out = render(&old, &new, 1, &expand);
        // The whole gap is shown below the first block: one merged hunk.
        assert_eq!(out.iter().filter(|l| l.starts_with("@@")).count(), 1);
    }

    #[test]
    fn expand_in_a_file_with_no_changes() {
        let text = "a\nb\nc\nd\ne\n";
        // Without expansion there's nothing but the hidden stretch.
        assert!(render(text, text, 1, &[]).is_empty());
        let expand = [Expand {
            below_prev: 2,
            above_next: 1,
        }];
        assert_eq!(
            render(text, text, 1, &expand),
            ["@@ skip 0", " a", " b", "@@ skip 2", " e"]
        );
    }
}
