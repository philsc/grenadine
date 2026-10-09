//! Line diffs, and finding the changes between two versions that a rebase
//! brought in rather than the PR's author.
//!
//! When two versions sit on different bases, the raw diff between them mixes
//! the author's changes with everything that landed upstream between the two
//! bases. Like Gerrit, a change block of that diff is marked as coming from
//! the rebase when the upstream diff (base A to base B) has a block with the
//! same removed and added lines.

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

/// For each of `blocks` (the diff of `old` to `new`), whether the same change
/// is in the upstream diff of `up_old` to `up_new`.
pub fn from_upstream(
    old: &[&str],
    new: &[&str],
    blocks: &[Block],
    up_old: &[&str],
    up_new: &[&str],
) -> Vec<bool> {
    let upstream: HashSet<Key> = change_blocks(up_old, up_new)
        .iter()
        .map(|b| key(up_old, up_new, b))
        .collect();
    blocks
        .iter()
        .map(|b| upstream.contains(&key(old, new, b)))
        .collect()
}

/// Splits the files of the diff between two versions into those the PR
/// changed in at least one of them (`pr_a` and `pr_b` are each version's diff
/// against its own base) and the number of the rest, which only differ
/// because of the rebase.
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

    #[test]
    fn marks_blocks_that_upstream_also_made() {
        // Version A sits on base A; version B is the same change rebased onto
        // base B, which renamed `old_name`.
        let base_a = ["fn old_name() {}", "", "fn f() {}"];
        let base_b = ["fn new_name() {}", "", "fn f() {}"];
        let v_a = ["fn old_name() {}", "", "fn f() { work(); }"];
        let v_b = ["fn new_name() {}", "", "fn f() { work(); more(); }"];

        let blocks = change_blocks(&v_a, &v_b);
        assert_eq!(blocks.len(), 2);
        let marks = from_upstream(&v_a, &v_b, &blocks, &base_a, &base_b);
        assert_eq!(marks, [true, false]);
    }

    #[test]
    fn trailing_whitespace_does_not_matter() {
        let blocks = change_blocks(&["x "], &["y"]);
        assert_eq!(
            from_upstream(&["x "], &["y"], &blocks, &["x"], &["y"]),
            [true]
        );
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
        let (kept, hidden) = touched_by_pr(&interdiff, &[], &[file(None, Some("here.rs"))]);
        assert_eq!(kept.len(), 1);
        assert_eq!(hidden, 0);
    }
}
