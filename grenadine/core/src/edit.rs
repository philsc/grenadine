//! The pieces of a file that an agent's file tool changed, for showing the
//! change as a diff.

use crate::api::PatchHunk;

/// A piece of a file before and after an edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    /// The 0-based line of the old file that `old` starts on.
    pub old_start: usize,
    /// The 0-based line of the new file that `new` starts on.
    pub new_start: usize,
    pub old: String,
    pub new: String,
}

/// A fragment whose place in the file is unknown, so it is numbered from
/// the first line.
pub fn from_strings(old: &str, new: &str) -> Fragment {
    Fragment {
        old_start: 0,
        new_start: 0,
        old: old.to_owned(),
        new: new.to_owned(),
    }
}

/// One fragment per hunk of a patch Claude Code reported.
pub fn from_patch(hunks: &[PatchHunk]) -> Vec<Fragment> {
    hunks
        .iter()
        .map(|h| {
            let (mut old, mut new) = (String::new(), String::new());
            for line in &h.lines {
                let (sides, text) = match line.split_at_checked(1) {
                    Some((" ", t)) => ((true, true), t),
                    Some(("-", t)) => ((true, false), t),
                    Some(("+", t)) => ((false, true), t),
                    // "\ No newline at end of file", or an empty context line.
                    Some(("\\", _)) => continue,
                    _ => ((true, true), ""),
                };
                if sides.0 {
                    old.push_str(text);
                    old.push('\n');
                }
                if sides.1 {
                    new.push_str(text);
                    new.push('\n');
                }
            }
            Fragment {
                // An empty side starts at 0 rather than 1.
                old_start: h.old_start.saturating_sub(1),
                new_start: h.new_start.saturating_sub(1),
                old,
                new,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old_start: usize, new_start: usize, lines: &[&str]) -> PatchHunk {
        PatchHunk {
            old_start,
            new_start,
            lines: lines.iter().map(|l| l.to_string()).collect(),
        }
    }

    #[test]
    fn splits_a_patch_into_sides() {
        assert_eq!(
            from_patch(&[hunk(10, 10, &[" a", "-b", "+c", "+d", " e"])]),
            [Fragment {
                old_start: 9,
                new_start: 9,
                old: "a\nb\ne\n".into(),
                new: "a\nc\nd\ne\n".into(),
            }]
        );
    }

    #[test]
    fn creates_a_file() {
        assert_eq!(
            from_patch(&[hunk(0, 1, &["+x", "+y"])]),
            [Fragment {
                old_start: 0,
                new_start: 0,
                old: String::new(),
                new: "x\ny\n".into(),
            }]
        );
    }

    #[test]
    fn skips_missing_newline_markers() {
        assert_eq!(
            from_patch(&[hunk(
                3,
                3,
                &["-a", "\\ No newline at end of file", "+b", ""]
            )]),
            [Fragment {
                old_start: 2,
                new_start: 2,
                old: "a\n\n".into(),
                new: "b\n\n".into(),
            }]
        );
    }
}
