//! Rewraps markdown at a fixed width. GitHub's squash merge hard-wraps a
//! PR's body at 72 columns for the commit message, splitting long lines
//! without joining short ones, which leaves one-word lines in paragraphs
//! and list items without their indent. A body whose lines already fit is
//! left alone.

/// `text` with every paragraph and list item that has a line over `width`
/// chars rewrapped at `width`. Code, headings, tables, quotes, HTML and the
/// trailers at the end are kept as they are, as are paragraphs and items
/// that already fit.
pub fn reflow(text: &str, width: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let trailers = trailers_start(&lines);
    let mut out: Vec<String> = Vec::new();
    let mut fence: Option<&str> = None;
    let mut in_list = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(f) = fence {
            out.push(line.to_owned());
            if line.trim_start().starts_with(f) {
                fence = None;
            }
            i += 1;
            continue;
        }
        if i >= trailers {
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.is_empty() {
            out.push(String::new());
            i += 1;
            continue;
        }
        if let Some(f) = fence_marker(trimmed) {
            fence = Some(f);
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        let marker = list_marker(trimmed);
        if marker.is_none() && (is_verbatim(trimmed) || (indent >= 4 && !in_list)) {
            out.push(line.to_owned());
            in_list = false;
            i += 1;
            continue;
        }
        // A paragraph or list item: this line and the ones that continue
        // it, up to a blank line or the start of another block.
        let (prefix, hang) = match marker {
            Some(m) => {
                in_list = true;
                let prefix = line[..indent + m].to_owned();
                let hang = " ".repeat(prefix.chars().count());
                (prefix, hang)
            }
            None => {
                if indent == 0 {
                    in_list = false;
                }
                let prefix = " ".repeat(indent);
                (prefix.clone(), prefix)
            }
        };
        let start = i;
        i += 1;
        while i < lines.len() && i < trailers {
            let t = lines[i].trim_start();
            if t.is_empty() || list_marker(t).is_some() || fence_marker(t).is_some() {
                break;
            }
            if is_verbatim(t) {
                break;
            }
            i += 1;
        }
        let unit = &lines[start..i];
        if unit.iter().all(|l| l.chars().count() <= width) {
            out.extend(unit.iter().map(|l| (*l).to_owned()));
            continue;
        }
        let mut first = true;
        for (segment, hard_break) in segments(unit, prefix.len()) {
            let lead = if first { &prefix } else { &hang };
            first = false;
            let mut wrapped = wrap(&segment, lead, &hang, width);
            if let Some(last) = wrapped.last_mut() {
                last.push_str(hard_break);
            }
            out.extend(wrapped);
        }
    }
    let mut s = out.join("\n");
    if text.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Where the trailers at the end of `lines` start: the last paragraph, when
/// another paragraph comes before it and every one of its lines is a
/// `Token: value` trailer. `lines.len()` when there are none.
fn trailers_start(lines: &[&str]) -> usize {
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(0, |e| e + 1);
    let Some(blank) = lines[..end].iter().rposition(|l| l.trim().is_empty()) else {
        return lines.len();
    };
    let last = &lines[blank + 1..end];
    let has_body = lines[..blank].iter().any(|l| !l.trim().is_empty());
    if has_body && !last.is_empty() && last.iter().all(|l| is_trailer(l)) {
        blank + 1
    } else {
        lines.len()
    }
}

fn is_trailer(line: &str) -> bool {
    line.split_once(": ").is_some_and(|(token, value)| {
        !token.is_empty()
            && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !value.trim().is_empty()
    })
}

/// The fence that opens a fenced code block, if `trimmed` starts one.
fn fence_marker(trimmed: &str) -> Option<&'static str> {
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// The length of the list marker `trimmed` starts with, the space after it
/// included: 2 for "- ", 4 for "10. ".
fn list_marker(trimmed: &str) -> Option<usize> {
    if ["- ", "* ", "+ "].iter().any(|m| trimmed.starts_with(m)) {
        return Some(2);
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    let rest = &trimmed[digits..];
    (digits > 0 && (rest.starts_with(". ") || rest.starts_with(") "))).then_some(digits + 2)
}

/// Whether a line is one of the blocks that's never rewrapped: a heading,
/// table row, quote, HTML or thematic break.
fn is_verbatim(trimmed: &str) -> bool {
    trimmed.starts_with('#')
        || trimmed.starts_with('|')
        || trimmed.starts_with('>')
        || trimmed.starts_with('<')
        || (trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_'))
}

/// The text of a paragraph or list item, with the first line's `prefix`
/// bytes left off, split at its hard line breaks. Each piece comes with
/// the break that ends it: two spaces, a backslash, or "" for the last.
fn segments(unit: &[&str], prefix: usize) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for (n, line) in unit.iter().enumerate() {
        let text = if n == 0 {
            &line[prefix..]
        } else {
            line.trim_start()
        };
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(text.trim_end());
        if let Some(t) = cur.strip_suffix('\\') {
            cur.truncate(t.len());
            out.push((std::mem::take(&mut cur), "\\"));
        } else if text.ends_with("  ") {
            out.push((std::mem::take(&mut cur), "  "));
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push((cur, ""));
    }
    out
}

/// `text` wrapped at `width`, the first line after `lead` and the others
/// after `hang`. A word longer than a line gets a line of its own.
fn wrap(text: &str, lead: &str, hang: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = lead.to_owned();
    let mut empty = true;
    for word in words(text) {
        let len = line.chars().count() + word.chars().count() + usize::from(!empty);
        if !empty && len > width {
            lines.push(std::mem::replace(&mut line, hang.to_owned()));
            empty = true;
        }
        if !empty {
            line.push(' ');
        }
        line.push_str(&word);
        empty = false;
    }
    lines.push(line);
    lines
}

/// The words of `text`, split on spaces, except that a code span stays one
/// word: `git branch --list` isn't split across lines.
fn words(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // The length of the backtick run that opened the code span we're in.
    let mut open = 0;
    let mut joining = false;
    for word in text.split_whitespace() {
        if joining {
            let last = out.last_mut().unwrap();
            last.push(' ');
            last.push_str(word);
        } else {
            out.push(word.to_owned());
        }
        let mut chars = word.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '`' {
                continue;
            }
            let mut run = 1;
            while chars.next_if_eq(&'`').is_some() {
                run += 1;
            }
            if open == 0 {
                open = run;
            } else if open == run {
                open = 0;
            }
        }
        joining = open != 0;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fits(text: &str) -> bool {
        text.lines().all(|l| l.chars().count() <= 72)
    }

    #[test]
    fn wraps_paragraphs_and_lists() {
        let body = "\
The page learns a session's status from an `AgentsChanged` refetch, separately from the transcript events. Right after a turn ended, a click on a wrap-up button could:

- reach the server before its run loop had cleared `running`, so `wrap_up` answered 409 \"the session is busy\", or
- be dropped.

Assisted-By: Claude
";
        assert_eq!(
            reflow(body, 72),
            "\
The page learns a session's status from an `AgentsChanged` refetch,
separately from the transcript events. Right after a turn ended, a click
on a wrap-up button could:

- reach the server before its run loop had cleared `running`, so
  `wrap_up` answered 409 \"the session is busy\", or
- be dropped.

Assisted-By: Claude
"
        );
    }

    #[test]
    fn rewraps_a_line_just_too_long() {
        let body = "\
A paragraph whose first line is just one character too long for GitHub's.
It goes on here.";
        assert_eq!(
            reflow(body, 72),
            "\
A paragraph whose first line is just one character too long for
GitHub's. It goes on here."
        );
    }

    #[test]
    fn joins_lazy_continuations_into_the_item() {
        // What GitHub's own wrapping makes of an unwrapped list item.
        let body = "\
- **Server:** `wrap_up` now accepts a wrap-up while a turn runs. The run
loop goes on with it when its turns are done, which it already did for wrap-ups.";
        assert_eq!(
            reflow(body, 72),
            "\
- **Server:** `wrap_up` now accepts a wrap-up while a turn runs. The run
  loop goes on with it when its turns are done, which it already did for
  wrap-ups."
        );
    }

    #[test]
    fn leaves_text_that_fits() {
        let body = "\
The picker now starts on General, and the session gets an empty
directory of its own. A repository can still be picked by hand.

## Testing

- `bazel test //grenadine/server/... //grenadine/testing:ui_test`, with
  `CleanupTest` checking that the picker shows General
- Not tried.

Assisted-By: Claude
";
        assert_eq!(reflow(body, 72), body);
    }

    #[test]
    fn keeps_code_tables_headings_and_trailers() {
        let long = "word ".repeat(20);
        let body = format!(
            "\
# A heading that is {long}

```
code that is {long}
```

    indented code {long}

| a table | {long} |
|---|---|

> a quote {long}

Text.

Assisted-By: Claude
Co-authored-by: Someone With A Long Name <someone.with.a.long.name@example.com>"
        );
        assert_eq!(reflow(&body, 72), body);
    }

    #[test]
    fn keeps_long_words_and_code_spans_whole() {
        let url = format!("https://example.com/{}", "x".repeat(70));
        let body = format!("See {url} for more. Then run `git branch --contains` on the branch.");
        let out = reflow(&body, 72);
        assert_eq!(
            out,
            format!("See\n{url}\nfor more. Then run `git branch --contains` on the branch.")
        );
        let body =
            "Some more words to push the span past the edge: `git branch --contains abc`. More.";
        assert_eq!(
            reflow(body, 72),
            "Some more words to push the span past the edge:\n`git branch --contains abc`. More."
        );
    }

    #[test]
    fn indents_numbered_items_by_their_marker() {
        let body = format!("10. {}", "word ".repeat(20).trim_end());
        let out = reflow(&body, 72);
        assert!(fits(&out));
        assert_eq!(
            out.lines().nth(1).unwrap(),
            format!("    {}", "word ".repeat(7).trim_end())
        );
    }

    #[test]
    fn keeps_hard_breaks() {
        let body = format!(
            "First line{}  \nSecond line {}",
            " word".repeat(13),
            "word ".repeat(14).trim_end()
        );
        let out = reflow(&body, 72);
        assert!(fits(&out));
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines[1], "word  ");
        assert!(lines[2].starts_with("Second line"), "{out}");
    }

    #[test]
    fn is_idempotent() {
        let body = format!("{}\n\n- {}", "word ".repeat(40), "item ".repeat(40));
        let once = reflow(&body, 72);
        assert!(fits(&once));
        assert_eq!(reflow(&once, 72), once);
    }
}
