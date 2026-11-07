//! Line diffs, and finding the changes between two versions that a rebase
//! brought in rather than the PR's author.
//!
//! When two versions sit on different bases, the raw diff between them mixes
//! the author's changes with everything that landed upstream between the two
//! bases. A three-way merge of the older version and the newer base against
//! the older base tells which lines of the diff came from the rebase.

use std::collections::HashSet;
use std::ops::Range;

use similar::{Algorithm, DiffOp, TextDiff};

use crate::api::FileChange;

/// A run of changed lines: the lines `old` of the old text were replaced by
/// the lines `new` of the new text. Either range may be empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// Splits text into lines without their line terminators.
pub fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

/// The change blocks between two texts, in order.
pub fn change_blocks(old: &[&str], new: &[&str]) -> Vec<Block> {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .diff_slices(old, new);
    let mut blocks: Vec<Block> = Vec::new();
    for op in diff.ops() {
        if let DiffOp::Equal { .. } = op {
            continue;
        }
        let (o, n) = (op.old_range(), op.new_range());
        // Adjacent delete and insert ops form one block.
        match blocks.last_mut() {
            Some(last) if last.old.end == o.start && last.new.end == n.start => {
                last.old.end = o.end;
                last.new.end = n.end;
            }
            _ => blocks.push(Block { old: o, new: n }),
        }
    }
    blocks
}

type Key = (Vec<String>, Vec<String>);

fn key(old: &[&str], new: &[&str], b: &Block) -> Key {
    let owned = |s: &[&str]| s.iter().map(|l| l.trim_end().to_owned()).collect();
    (owned(&old[b.old.clone()]), owned(&new[b.new.clone()]))
}

/// Where a merged line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Both sides kept it.
    Base,
    /// Only the older version changed it.
    Ours,
    /// Only upstream changed it.
    Theirs,
    /// Both sides changed it the same way.
    Both,
    /// Both sides changed it differently; the older version's line is
    /// kept, flagged.
    Conflict,
}

/// The 3-way merge of `ours` and `theirs` from their common `base`.
pub struct Merged<'a> {
    pub lines: Vec<&'a str>,
    /// Per merged line: where it came from.
    pub origin: Vec<Origin>,
    /// Per line of `ours`: whether the merge dropped it because `theirs`
    /// cleanly removed or replaced it.
    pub ours_dropped_by_theirs: Vec<bool>,
    /// Per line of `ours`: whether it lies in a conflict region.
    pub ours_conflict: Vec<bool>,
    /// Per line of `ours`: its index in `lines`, `None` when the merge
    /// dropped it.
    pub ours_to_merged: Vec<Option<usize>>,
}

/// A span of base lines that `ours` or `theirs` changed, or both.
struct Region {
    base: Range<usize>,
    ours: Vec<Block>,
    theirs: Vec<Block>,
}

/// Groups change blocks of both sides into regions: blocks whose base
/// ranges overlap or touch are one region, and so are two insertions at
/// the same base position.
fn regions(ours: Vec<Block>, theirs: Vec<Block>) -> Vec<Region> {
    let mut tagged: Vec<(bool, Block)> = ours
        .into_iter()
        .map(|b| (false, b))
        .chain(theirs.into_iter().map(|b| (true, b)))
        .collect();
    tagged.sort_by_key(|(_, b)| b.old.start);
    let mut out: Vec<Region> = Vec::new();
    for (is_theirs, b) in tagged {
        match out.last_mut() {
            Some(last) if b.old.start <= last.base.end => {
                last.base.end = last.base.end.max(b.old.end);
                if is_theirs {
                    last.theirs.push(b);
                } else {
                    last.ours.push(b);
                }
            }
            _ => {
                let base = b.old.clone();
                out.push(Region {
                    base,
                    ours: if is_theirs {
                        Vec::new()
                    } else {
                        vec![b.clone()]
                    },
                    theirs: if is_theirs { vec![b] } else { Vec::new() },
                });
            }
        }
    }
    out
}

/// Maps each line of `old` that `blocks` keep to its index in `new`.
fn carried_map(old_len: usize, blocks: &[Block]) -> Vec<usize> {
    let mut map = vec![usize::MAX; old_len];
    let (mut o, mut n) = (0, 0);
    for b in blocks {
        while o < b.old.start {
            map[o] = n;
            o += 1;
            n += 1;
        }
        o = b.old.end;
        n = b.new.end;
    }
    while o < old_len {
        map[o] = n;
        o += 1;
        n += 1;
    }
    map
}

/// What `side` makes of `region`: the unchanged base lines and the side's
/// own lines, in order. Each entry is the line's index in `side` (the
/// carried index for base lines) and the line.
fn side_output<'a>(
    base: &[&'a str],
    side: &[&'a str],
    blocks: &[Block],
    region: &Range<usize>,
    carried: &[usize],
) -> Vec<(usize, &'a str)> {
    let mut out = Vec::new();
    let mut cursor = region.start;
    for b in blocks {
        for p in cursor..b.old.start {
            out.push((carried[p], base[p]));
        }
        out.extend(b.new.clone().map(|i| (i, side[i])));
        cursor = b.old.end;
    }
    for p in cursor..region.end {
        out.push((carried[p], base[p]));
    }
    out
}

/// Pushes the base lines of `range`, noting in `ours_to_merged` where each
/// of `ours`' carried lines lands.
fn push_carried<'a>(
    lines: &mut Vec<&'a str>,
    ours_to_merged: &mut [Option<usize>],
    carried: &[usize],
    base: &[&'a str],
    range: Range<usize>,
) {
    for p in range {
        if carried[p] != usize::MAX {
            ours_to_merged[carried[p]] = Some(lines.len());
        }
        lines.push(base[p]);
    }
}

/// The 3-way merge of `ours` and `theirs` from their common `base`.
/// Conflicts resolve to `ours`' lines, flagged as such.
pub fn merge3<'a>(base: &[&'a str], ours: &[&'a str], theirs: &[&'a str]) -> Merged<'a> {
    let blocks_ours = change_blocks(base, ours);
    let blocks_theirs = change_blocks(base, theirs);
    let carried_ours = carried_map(base.len(), &blocks_ours);
    let mut lines: Vec<&'a str> = Vec::new();
    let mut origin: Vec<Origin> = Vec::new();
    let mut ours_dropped_by_theirs = vec![false; ours.len()];
    let mut ours_conflict = vec![false; ours.len()];
    let mut ours_to_merged = vec![None; ours.len()];

    let mut pos = 0;
    for region in regions(blocks_ours, blocks_theirs) {
        push_carried(
            &mut lines,
            &mut ours_to_merged,
            &carried_ours,
            base,
            pos..region.base.start,
        );
        origin.resize(lines.len(), Origin::Base);
        pos = region.base.end;
        let ours_out = side_output(base, ours, &region.ours, &region.base, &carried_ours);
        if region.theirs.is_empty() {
            // Only the older version changed the region: the lines of its
            // blocks are its own, the base lines between them unchanged.
            let changed: HashSet<usize> = region.ours.iter().flat_map(|b| b.new.clone()).collect();
            for (i, l) in &ours_out {
                ours_to_merged[*i] = Some(lines.len());
                lines.push(l);
                origin.push(if changed.contains(i) {
                    Origin::Ours
                } else {
                    Origin::Base
                });
            }
            continue;
        }
        if region.ours.is_empty() {
            for b in &region.theirs {
                for p in b.old.clone() {
                    ours_dropped_by_theirs[carried_ours[p]] = true;
                }
            }
            let mut cursor = region.base.start;
            for b in &region.theirs {
                push_carried(
                    &mut lines,
                    &mut ours_to_merged,
                    &carried_ours,
                    base,
                    cursor..b.old.start,
                );
                origin.resize(lines.len(), Origin::Base);
                for i in b.new.clone() {
                    lines.push(theirs[i]);
                    origin.push(Origin::Theirs);
                }
                cursor = b.old.end;
            }
            push_carried(
                &mut lines,
                &mut ours_to_merged,
                &carried_ours,
                base,
                cursor..region.base.end,
            );
            origin.resize(lines.len(), Origin::Base);
            continue;
        }
        let theirs_out: Vec<&str> = {
            let mut v = Vec::new();
            let mut cursor = region.base.start;
            for b in &region.theirs {
                v.extend(&base[cursor..b.old.start]);
                v.extend(&theirs[b.new.clone()]);
                cursor = b.old.end;
            }
            v.extend(&base[cursor..region.base.end]);
            v
        };
        let ours_lines: Vec<&str> = ours_out.iter().map(|(_, l)| *l).collect();
        let same = ours_lines.len() == theirs_out.len()
            && ours_lines
                .iter()
                .zip(&theirs_out)
                .all(|(a, b)| a.trim_end() == b.trim_end());
        if same {
            for (i, l) in &ours_out {
                ours_to_merged[*i] = Some(lines.len());
                lines.push(l);
                origin.push(Origin::Both);
            }
        } else {
            for (i, l) in &ours_out {
                ours_to_merged[*i] = Some(lines.len());
                lines.push(l);
                origin.push(Origin::Conflict);
                ours_conflict[*i] = true;
            }
        }
    }
    push_carried(
        &mut lines,
        &mut ours_to_merged,
        &carried_ours,
        base,
        pos..base.len(),
    );
    origin.resize(lines.len(), Origin::Base);
    Merged {
        lines,
        origin,
        ours_dropped_by_theirs,
        ours_conflict,
        ours_to_merged,
    }
}

/// What a changed line of a version diff is: the PR's own work, brought
/// in by the rebase, or the two mixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Pr,
    Rebase,
    Mixed,
}

/// Kinds of the lines of `old` and `new`, indexed by line; only changed
/// lines of the old->new diff matter, unchanged ones are `Kind::Pr`.
pub struct LineKinds {
    pub old: Vec<Kind>,
    pub new: Vec<Kind>,
}

/// Maps each line of `new` to the line of `old` it equals, if any.
fn equal_map(old: &[&str], new: &[&str]) -> Vec<Option<usize>> {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .diff_slices(old, new);
    let mut map = vec![None; new.len()];
    for op in diff.ops() {
        if let DiffOp::Equal { .. } = op {
            for (o, n) in op.old_range().zip(op.new_range()) {
                map[n] = Some(o);
            }
        }
    }
    map
}

/// Classifies the changed lines of the diff of `old` to `new`, where
/// `old` sits on `base_old` and `new` on `base_new`.
pub fn classify(base_old: &[&str], old: &[&str], base_new: &[&str], new: &[&str]) -> LineKinds {
    let merged = merge3(base_old, old, base_new);
    let blocks = change_blocks(old, new);
    let eq = equal_map(&merged.lines, new);
    let mn_blocks = change_blocks(&merged.lines, new);
    let mn_conflict: Vec<bool> = mn_blocks
        .iter()
        .map(|b| merged.origin[b.old.clone()].contains(&Origin::Conflict))
        .collect();
    let mut mn_block_of_new = vec![usize::MAX; new.len()];
    for (k, b) in mn_blocks.iter().enumerate() {
        for n in b.new.clone() {
            mn_block_of_new[n] = k;
        }
    }
    let mut kinds = LineKinds {
        old: vec![Kind::Pr; old.len()],
        new: vec![Kind::Pr; new.len()],
    };
    for b in &blocks {
        for n in b.new.clone() {
            kinds.new[n] = match eq[n] {
                Some(m) => match merged.origin[m] {
                    Origin::Theirs => Kind::Rebase,
                    Origin::Conflict => Kind::Mixed,
                    _ => Kind::Pr,
                },
                None if mn_block_of_new[n] != usize::MAX && mn_conflict[mn_block_of_new[n]] => {
                    Kind::Mixed
                }
                None => Kind::Pr,
            };
        }
        for o in b.old.clone() {
            kinds.old[o] = if merged.ours_conflict[o] {
                Kind::Mixed
            } else if merged.ours_dropped_by_theirs[o] {
                Kind::Rebase
            } else {
                Kind::Pr
            };
        }
    }
    kinds
}

/// Whether `range` of the merged lines touches a conflict line. An empty
/// range counts when it sits strictly inside a run of conflict lines.
fn overlaps_conflict(merged: &Merged, range: &Range<usize>) -> bool {
    if !range.is_empty() {
        return merged.origin[range.clone()].contains(&Origin::Conflict);
    }
    let p = range.start;
    p > 0
        && p < merged.origin.len()
        && merged.origin[p - 1] == Origin::Conflict
        && merged.origin[p] == Origin::Conflict
}

/// Kinds of the change blocks of `merged.lines` to `new`: `Mixed` when a
/// block's merged range overlaps a conflict line, `Rebase` when it
/// repeats an upstream change of `base_old` to `base_new`, else `Pr`.
pub fn rebased_block_kinds(
    merged: &Merged,
    new: &[&str],
    blocks: &[Block],
    base_old: &[&str],
    base_new: &[&str],
) -> Vec<Kind> {
    let upstream: HashSet<Key> = change_blocks(base_old, base_new)
        .iter()
        .map(|b| key(base_old, base_new, b))
        .collect();
    blocks
        .iter()
        .map(|b| {
            if overlaps_conflict(merged, &b.old) {
                Kind::Mixed
            } else if upstream.contains(&key(&merged.lines, new, b)) {
                Kind::Rebase
            } else {
                Kind::Pr
            }
        })
        .collect()
}

/// Splits the files of the diff between two versions into those the PR
/// changed in at least one of them (`pr_a` and `pr_b` are each version's
/// diff against its own base) and the number of the rest, which only
/// differ because of the rebase.
pub fn touched_by_pr(
    interdiff: &[FileChange],
    pr_a: &[FileChange],
    pr_b: &[FileChange],
) -> (Vec<FileChange>, usize) {
    let mut touched: HashSet<&str> = HashSet::new();
    for f in pr_a.iter().chain(pr_b) {
        touched.extend(f.old_path.iter().chain(&f.new_path).map(String::as_str));
    }
    let mut kept = Vec::new();
    let mut hidden = 0;
    for f in interdiff {
        if f.old_path
            .iter()
            .chain(&f.new_path)
            .any(|p| touched.contains(p.as_str()))
        {
            kept.push(f.clone());
        } else {
            hidden += 1;
        }
    }
    (kept, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ChangeStatus;

    #[test]
    fn blocks_merge_deletes_and_inserts() {
        let old = ["a", "b", "c", "d"];
        let new = ["a", "B", "c", "d", "e"];
        let blocks = change_blocks(&old, &new);
        assert_eq!(
            blocks,
            [
                Block {
                    old: 1..2,
                    new: 1..2
                },
                Block {
                    old: 4..4,
                    new: 4..5
                },
            ]
        );
    }

    #[test]
    fn identical_texts_have_no_blocks() {
        assert!(change_blocks(&["a"], &["a"]).is_empty());
    }

    fn origins(m: &Merged) -> Vec<Origin> {
        m.origin.clone()
    }

    #[test]
    fn merge_with_no_changes_is_the_base() {
        let base = ["a", "b", "c"];
        let m = merge3(&base, &base, &base);
        assert_eq!(m.lines, base);
        assert_eq!(origins(&m), [Origin::Base; 3]);
        assert_eq!(m.ours_dropped_by_theirs, [false; 3]);
        assert_eq!(m.ours_conflict, [false; 3]);
        assert_eq!(m.ours_to_merged, [Some(0), Some(1), Some(2)]);
    }

    #[test]
    fn merge_keeps_only_our_changes() {
        let base = ["a", "b", "c"];
        let ours = ["a", "B", "c", "d"];
        let m = merge3(&base, &ours, &base);
        assert_eq!(m.lines, ours);
        assert_eq!(
            origins(&m),
            [Origin::Base, Origin::Ours, Origin::Base, Origin::Ours]
        );
    }

    #[test]
    fn merge_keeps_only_their_changes() {
        let base = ["a", "b", "c"];
        let theirs = ["a", "B", "c"];
        let m = merge3(&base, &base, &theirs);
        assert_eq!(m.lines, theirs);
        assert_eq!(origins(&m), [Origin::Base, Origin::Theirs, Origin::Base]);
        // Theirs replaced base's "b", which is ours' line 1.
        assert_eq!(m.ours_dropped_by_theirs, [false, true, false]);
        assert_eq!(m.ours_to_merged, [Some(0), None, Some(2)]);
    }

    #[test]
    fn identical_edits_on_both_sides_merge_cleanly() {
        let base = ["a", "b", "c"];
        let ours = ["a", "B", "c"];
        let theirs = ["a", "B ", "c"];
        let m = merge3(&base, &ours, &theirs);
        assert_eq!(m.lines, ["a", "B", "c"]);
        assert_eq!(origins(&m), [Origin::Base, Origin::Both, Origin::Base]);
        assert_eq!(m.ours_conflict, [false; 3]);
        assert_eq!(m.ours_to_merged, [Some(0), Some(1), Some(2)]);
    }

    #[test]
    fn touching_edits_conflict() {
        // Ours edits line 1, theirs the adjacent line 2.
        let base = ["a", "b", "c", "d"];
        let ours = ["a", "B", "c", "d"];
        let theirs = ["a", "b", "C", "d"];
        let m = merge3(&base, &ours, &theirs);
        assert_eq!(m.lines, ours);
        assert_eq!(
            origins(&m),
            [
                Origin::Base,
                Origin::Conflict,
                Origin::Conflict,
                Origin::Base
            ]
        );
        assert_eq!(m.ours_conflict, [false, true, true, false]);
        assert_eq!(m.ours_to_merged, [Some(0), Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn upstream_insertion_shifts_ours_lines() {
        let base = ["a", "b", "c"];
        let theirs = ["a", "x", "b", "c"];
        let m = merge3(&base, &base, &theirs);
        assert_eq!(m.lines, theirs);
        assert_eq!(m.ours_to_merged, [Some(0), Some(2), Some(3)]);
    }

    #[test]
    fn insertions_at_the_same_position_conflict() {
        let base = ["a", "b"];
        let ours = ["a", "x", "b"];
        let theirs = ["a", "y", "b"];
        let m = merge3(&base, &ours, &theirs);
        assert_eq!(m.lines, ours);
        assert_eq!(origins(&m), [Origin::Base, Origin::Conflict, Origin::Base]);
        assert_eq!(m.ours_conflict, [false, true, false]);
    }

    #[test]
    fn identical_insertions_at_the_same_position_merge() {
        let base = ["a", "b"];
        let ours = ["a", "x", "b"];
        let m = merge3(&base, &ours, &ours);
        assert_eq!(m.lines, ours);
        assert_eq!(origins(&m), [Origin::Base, Origin::Both, Origin::Base]);
    }

    #[test]
    fn deletion_at_the_end_merges() {
        let base = ["a", "b", "c", "d"];
        let ours = ["a", "b", "c"];
        let theirs = ["A", "b", "c", "d"];
        let m = merge3(&base, &ours, &theirs);
        assert_eq!(m.lines, ["A", "b", "c"]);
        assert_eq!(origins(&m), [Origin::Theirs, Origin::Base, Origin::Base]);
        assert_eq!(m.ours_conflict, [false; 3]);
    }

    #[test]
    fn theirs_adds_a_file_to_an_empty_ours() {
        let file = ["one", "two", "three"];
        let m = merge3(&[], &[], &file);
        assert_eq!(m.lines, file);
        assert_eq!(origins(&m), [Origin::Theirs; 3]);
    }

    #[test]
    fn a_real_conflict_keeps_ours_flagged() {
        let base = ["a", "b", "c"];
        let ours = ["a", "ours", "c"];
        let theirs = ["a", "theirs", "c"];
        let m = merge3(&base, &ours, &theirs);
        assert_eq!(m.lines, ours);
        assert_eq!(origins(&m), [Origin::Base, Origin::Conflict, Origin::Base]);
        assert_eq!(m.ours_conflict, [false, true, false]);
    }

    #[test]
    fn upstream_file_shows_as_rebase_lines() {
        // The reported case: upstream added a file between the bases;
        // the newer version edits two of its lines and adds one.
        let base_new: Vec<&str> = (1..=20).map(|_| "line").collect();
        let mut new = base_new.clone();
        new[4] = "edited";
        new[9] = "edited";
        new.push("added");
        let k = classify(&[], &[], &base_new, &new);
        let kinds: Vec<Kind> = k.new;
        assert_eq!(kinds.len(), 21);
        assert_eq!(kinds[4], Kind::Pr);
        assert_eq!(kinds[9], Kind::Pr);
        assert_eq!(kinds[20], Kind::Pr);
        assert!(kinds[..4].iter().all(|k| *k == Kind::Rebase));
        assert!(kinds[5..9].iter().all(|k| *k == Kind::Rebase));
        assert!(kinds[10..20].iter().all(|k| *k == Kind::Rebase));
    }

    #[test]
    fn upstream_edit_far_from_the_pr_edit_is_rebase() {
        let base_old = ["a", "up", "m", "pr", "z"];
        let old = ["a", "up", "m", "PR", "z"];
        let base_new = ["a", "UP", "m", "pr", "z"];
        let new = ["a", "UP", "m", "PR+", "z"];
        let k = classify(&base_old, &old, &base_new, &new);
        // "pr" -> "PR+" is the author's; "up" -> "UP" is upstream's.
        assert_eq!(
            k.old,
            [Kind::Pr, Kind::Rebase, Kind::Pr, Kind::Pr, Kind::Pr]
        );
        assert_eq!(k.new[1], Kind::Rebase);
        assert_eq!(k.new[3], Kind::Pr);
    }

    #[test]
    fn conflicting_changes_are_mixed() {
        let base_old = ["a", "b", "c"];
        let old = ["a", "ours", "c"];
        let base_new = ["a", "theirs", "c"];
        // The newer version resolves the conflict its own way.
        let new = ["a", "resolved", "c"];
        let k = classify(&base_old, &old, &base_new, &new);
        assert_eq!(k.old[1], Kind::Mixed);
        assert_eq!(k.new[1], Kind::Mixed);
    }

    #[test]
    fn upstream_deletion_marks_the_removed_old_line() {
        // Upstream deleted a line the older version still had; dropping
        // it in the newer version is the rebase's work.
        let base_old = ["a", "gone", "c"];
        let old = ["a", "gone", "c", "pr"];
        let base_new = ["a", "c"];
        let new = ["a", "c", "pr"];
        let k = classify(&base_old, &old, &base_new, &new);
        assert_eq!(k.old[1], Kind::Rebase);
        // The new side gained no lines, so none are classified.
        assert_eq!(k.new, [Kind::Pr; 3]);
    }

    #[test]
    fn rebased_blocks_are_pr_without_conflicts() {
        let base_old = ["a", "b", "c"];
        let ours = ["a", "B", "c"];
        let base_new = ["a", "b", "c", "d"];
        let merged = merge3(&base_old, &ours, &base_new);
        // The rebased text against the new version: the PR also adds "e".
        let new = ["a", "B", "c", "d", "e"];
        let blocks = change_blocks(&merged.lines, &new);
        let kinds = rebased_block_kinds(&merged, &new, &blocks, &base_old, &base_new);
        assert_eq!(kinds, [Kind::Pr]);
    }

    #[test]
    fn rebased_blocks_over_a_conflict_are_mixed() {
        let base_old = ["a", "b", "c"];
        let ours = ["a", "ours", "c"];
        let base_new = ["a", "theirs", "c"];
        let merged = merge3(&base_old, &ours, &base_new);
        let new = ["a", "resolved", "c"];
        let blocks = change_blocks(&merged.lines, &new);
        let kinds = rebased_block_kinds(&merged, &new, &blocks, &base_old, &base_new);
        assert_eq!(kinds, [Kind::Mixed]);
    }

    fn file(old_path: Option<&str>, new_path: Option<&str>) -> FileChange {
        FileChange {
            status: ChangeStatus::Modified,
            old_path: old_path.map(String::from),
            new_path: new_path.map(String::from),
            old_blob: None,
            new_blob: None,
        }
    }

    #[test]
    fn upstream_only_files_are_hidden() {
        let interdiff = [file(None, Some("upstream.rs")), file(None, Some("pr.rs"))];
        let pr_a = [file(None, Some("pr.rs"))];
        let (kept, hidden) = touched_by_pr(&interdiff, &pr_a, &pr_a);
        assert_eq!(kept, [file(None, Some("pr.rs"))]);
        assert_eq!(hidden, 1);
    }

    #[test]
    fn file_touched_only_in_version_a_is_kept() {
        let interdiff = [file(None, Some("a_only.rs"))];
        let (kept, hidden) = touched_by_pr(&interdiff, &[file(None, Some("a_only.rs"))], &[]);
        assert_eq!(kept.len(), 1);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn file_touched_only_in_version_b_is_kept() {
        let interdiff = [file(None, Some("b_only.rs"))];
        let (kept, hidden) = touched_by_pr(&interdiff, &[], &[file(None, Some("b_only.rs"))]);
        assert_eq!(kept.len(), 1);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn interdiff_rename_matches_either_side() {
        // Version A renamed old.rs to shared.rs; version B kept old.rs.
        // Version B renamed shared.rs to new.rs; version A kept new.rs.
        let interdiff = [
            file(Some("old.rs"), Some("shared.rs")),
            file(Some("shared.rs"), Some("new.rs")),
        ];
        let pr_a = [
            file(Some("old.rs"), Some("shared.rs")),
            file(None, Some("new.rs")),
        ];
        let pr_b = [
            file(None, Some("old.rs")),
            file(Some("shared.rs"), Some("new.rs")),
        ];
        let (kept, hidden) = touched_by_pr(&interdiff, &pr_a, &pr_b);
        assert_eq!(kept.len(), 2);
        assert_eq!(hidden, 0);

        // pr_a has only the old path of an interdiff rename.
        let interdiff = [file(Some("gone.rs"), Some("here.rs"))];
        let (kept, hidden) = touched_by_pr(&interdiff, &[file(None, Some("gone.rs"))], &[]);
        assert_eq!(kept.len(), 1);
        assert_eq!(hidden, 0);

        // pr_b has only the new path of an interdiff rename.
        let interdiff = [file(Some("gone.rs"), Some("here.rs"))];
        let (kept, hidden) = touched_by_pr(&interdiff, &[], &[file(None, Some("here.rs"))]);
        assert_eq!(kept.len(), 1);
        assert_eq!(hidden, 0);
    }
}
