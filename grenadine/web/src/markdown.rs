//! Renders PR descriptions and comments as GitHub-flavored Markdown.

use std::borrow::Cow;

use linkify::{LinkFinder, LinkKind};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, html};

use crate::highlight;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    escape(s).replace('"', "&quot;")
}

/// The in-app hash for a GitHub PR or issue URL, `None` for anything else.
/// Issues only match for links grenadine generated itself; explicitly
/// written `[text](url)` links only get the attribute for PRs.
fn grenadine_hash(href: &str, issues: bool) -> Option<String> {
    let mut parts = href.strip_prefix("https://github.com/")?.split('/');
    let (owner, name, kind, number) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some()
        || (kind != "pull" && !(issues && kind == "issues"))
        || owner.is_empty()
        || name.is_empty()
        || number.is_empty()
        || !number.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(format!("#/{owner}/{name}/{number}"))
}

/// A `data-grenadine` hash only has `#/owner/name/number`; anything else is
/// dropped by the sanitizer.
fn valid_grenadine_hash(value: &str) -> bool {
    let mut parts = value.split('/');
    parts.next() == Some("#")
        && parts.next().is_some_and(|p| !p.is_empty())
        && parts.next().is_some_and(|p| !p.is_empty())
        && parts
            .next()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        && parts.next().is_none()
}

/// `<inner>` is HTML; the text callers put there is escaped first.
/// Generated links are all external, so they open in a new tab.
fn anchor(href: &str, data: Option<&str>, inner: &str) -> Event<'static> {
    let data = data
        .map(|d| format!(" data-grenadine=\"{}\"", escape_attr(d)))
        .unwrap_or_default();
    Event::InlineHtml(
        format!(
            "<a href=\"{}\" target=\"_blank\"{data}>{inner}</a>",
            escape_attr(href)
        )
        .into(),
    )
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn word_boundary(prev: Option<char>) -> bool {
    prev.is_none_or(|c| !is_word(c))
}

/// A GitHub user or organization name: alphanumerics and single hyphens.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 39
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && !name.ends_with('-')
        && !name.contains("--")
}

fn name_len(s: &str) -> usize {
    s.bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-')
        .count()
}

/// `@user` or `@org/team` at the start of `rest` (which starts with `@`).
fn mention_at(rest: &str) -> Option<(usize, Event<'static>)> {
    let s = &rest[1..];
    let name = &s[..name_len(s)];
    if !valid_name(name) {
        return None;
    }
    if let Some(t) = s[name.len()..].strip_prefix('/') {
        let team = &t[..name_len(t)];
        if valid_name(team) {
            let link = anchor(
                &format!("https://github.com/orgs/{name}/teams/{team}"),
                None,
                &format!("@{name}/{team}"),
            );
            return Some((1 + name.len() + 1 + team.len(), link));
        }
    }
    let link = anchor(
        &format!("https://github.com/{name}"),
        None,
        &format!("@{name}"),
    );
    Some((1 + name.len(), link))
}

fn digits_len(s: &str) -> usize {
    s.bytes().take_while(|b| b.is_ascii_digit()).count()
}

/// `owner/repo#123` at the start of `rest`.
fn repo_ref_at(rest: &str) -> Option<(usize, Event<'static>)> {
    let owner = &rest[..name_len(rest)];
    if !valid_name(owner) {
        return None;
    }
    let s = rest[owner.len()..].strip_prefix('/')?;
    let name_len = s
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_' || *b == b'.')
        .count();
    let name = &s[..name_len];
    if name.is_empty() || name.starts_with('.') || name.ends_with('.') {
        return None;
    }
    let s = s[name_len..].strip_prefix('#')?;
    let dlen = digits_len(s);
    if dlen == 0 || s[dlen..].chars().next().is_some_and(is_word) {
        return None;
    }
    let n = &s[..dlen];
    let link = anchor(
        &format!("https://github.com/{owner}/{name}/issues/{n}"),
        Some(&format!("#/{owner}/{name}/{n}")),
        &format!("{owner}/{name}#{n}"),
    );
    Some((owner.len() + 1 + name_len + 1 + dlen, link))
}

/// `#123` at the start of `rest` (which starts with `#`).
fn issue_ref_at(rest: &str, repo: &str) -> Option<(usize, Event<'static>)> {
    let s = &rest[1..];
    let dlen = digits_len(s);
    if dlen == 0 || s[dlen..].chars().next().is_some_and(is_word) {
        return None;
    }
    let n = &s[..dlen];
    let link = anchor(
        &format!("https://github.com/{repo}/issues/{n}"),
        Some(&format!("#/{repo}/{n}")),
        &format!("#{n}"),
    );
    Some((1 + dlen, link))
}

/// A commit SHA at the start of `rest`: 7-40 lowercase hex chars with at
/// least one digit and one letter, so issue numbers and words like
/// `deadbeef` aren't linked.
fn sha_at(rest: &str, repo: &str) -> Option<(usize, Event<'static>)> {
    let len = rest
        .bytes()
        .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        .count();
    if !(7..=40).contains(&len) || rest[len..].chars().next().is_some_and(is_word) {
        return None;
    }
    let sha = &rest[..len];
    if !sha.bytes().any(|b| b.is_ascii_digit()) || !sha.bytes().any(|b| (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let link = anchor(
        &format!("https://github.com/{repo}/commit/{sha}"),
        None,
        &format!("<code>{}</code>", &sha[..7]),
    );
    Some((len, link))
}

/// `:shortcode:` at the start of `rest` (which starts with `:`).
fn emoji_at(rest: &str) -> Option<(usize, &'static str)> {
    let s = rest.strip_prefix(':')?;
    let len = s
        .bytes()
        .take_while(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'+' || *b == b'-'
        })
        .count();
    if len == 0 || len > 40 || !s[len..].starts_with(':') {
        return None;
    }
    Some((len + 2, emojis::get_by_shortcode(&s[..len])?.as_str()))
}

/// Emoji are substituted inside link text too, but nothing is linked.
fn emojify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        match emoji_at(&text[i..]) {
            Some((end, emoji)) => {
                out += emoji;
                i += end;
            }
            None => {
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// Splits plain text into Text events and generated links for URLs,
/// emails, mentions, issue references and commit SHAs.
fn autolink(text: &str, repo: &str) -> Vec<Event<'static>> {
    let mut finder = LinkFinder::new();
    finder.url_must_have_scheme(false);
    let mut links = finder.links(text).peekable();
    let mut out = Vec::new();
    let mut run = String::new();
    let mut i = 0;
    while i < text.len() {
        while links.peek().is_some_and(|l| l.end() <= i) {
            links.next();
        }
        if links.peek().is_some_and(|l| l.start() == i) {
            let l = links.next().unwrap();
            if !run.is_empty() {
                out.push(Event::Text(std::mem::take(&mut run).into()));
            }
            let href = match l.kind() {
                LinkKind::Email => format!("mailto:{}", l.as_str()),
                _ if l.as_str().starts_with("www.") => format!("https://{}", l.as_str()),
                _ => l.as_str().to_owned(),
            };
            let data = grenadine_hash(&href, true);
            out.push(anchor(&href, data.as_deref(), &escape(l.as_str())));
            i = l.end();
            continue;
        }
        let c = text[i..].chars().next().unwrap();
        let prev = text[..i].chars().next_back();
        let rest = &text[i..];
        if let Some((end, emoji)) = emoji_at(rest) {
            run += emoji;
            i += end;
            continue;
        }
        // `@` can't be part of a `owner/repo#n` token, and `#` needs no
        // leading token; SHAs and repo refs both start on a word char.
        let found = if c == '@' && word_boundary(prev) {
            mention_at(rest)
        } else if c == '#' && word_boundary(prev) {
            issue_ref_at(rest, repo)
        } else if is_word(c) {
            if repo_ref_boundary(prev) {
                repo_ref_at(rest).or_else(|| sha_at(rest, repo))
            } else {
                sha_at(rest, repo).filter(|_| word_boundary(prev))
            }
        } else {
            None
        };
        match found {
            Some((end, link)) => {
                if !run.is_empty() {
                    out.push(Event::Text(std::mem::take(&mut run).into()));
                }
                out.push(link);
                i += end;
            }
            None => {
                run.push(c);
                i += c.len_utf8();
            }
        }
    }
    if !run.is_empty() {
        out.push(Event::Text(run.into()));
    }
    out
}

/// A `owner/repo#n` token can't be glued to a preceding word, `/`, `#`,
/// `.` or `-`, so `a/b/c#1` and `x-o/r#1` aren't parsed as references.
fn repo_ref_boundary(prev: Option<char>) -> bool {
    prev.is_none_or(|c| !is_word(c) && c != '/' && c != '#' && c != '.' && c != '-')
}

/// `<pre><code>` for a fenced code block; with a known language the code
/// is highlighted like the diff is.
fn code_block_html(lang: &str, code: &str) -> String {
    let token = lang.split_whitespace().next().unwrap_or_default();
    match highlight::highlight_code(token, code) {
        Some(spans) => format!(
            "<pre><code class=\"language-{}\">{spans}</code></pre>",
            escape_attr(token)
        ),
        None => format!("<pre><code>{}</code></pre>", escape(code)),
    }
}

/// Markdown to sanitized HTML. `repo` is the `owner/name` the text belongs
/// to, for `#123` and commit links.
pub fn to_html(markdown: &str, repo: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM;
    let mut parser = Parser::new_ext(markdown, options);
    let mut events: Vec<Event> = Vec::new();
    // Text inside a link or image is only emoji-expanded; link text the
    // parser produced is never autolinked.
    let mut in_link = false;
    // The current link's `<a>` tag was already emitted with data-grenadine;
    // its End tag is replaced too.
    let mut own_link = false;
    while let Some(event) = parser.next() {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match &kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                let mut code = String::new();
                for e in parser.by_ref() {
                    match e {
                        Event::Text(t) | Event::Code(t) => code.push_str(&t),
                        Event::SoftBreak | Event::HardBreak => code.push('\n'),
                        Event::End(TagEnd::CodeBlock) => break,
                        _ => {}
                    }
                }
                events.push(Event::Html(code_block_html(&lang, &code).into()));
            }
            Event::Start(Tag::Link {
                ref dest_url,
                ref title,
                ..
            }) => {
                in_link = true;
                let external = ["https://", "http://", "mailto:"]
                    .iter()
                    .any(|p| dest_url.starts_with(p));
                if !external {
                    events.push(event);
                    continue;
                }
                // External links get a hand-written tag so that they open
                // in a new tab and PR links carry their in-app hash.
                own_link = true;
                let title = if title.is_empty() {
                    String::new()
                } else {
                    format!(" title=\"{}\"", escape_attr(title))
                };
                let data = grenadine_hash(dest_url, false)
                    .map(|h| format!(" data-grenadine=\"{}\"", escape_attr(&h)))
                    .unwrap_or_default();
                events.push(Event::InlineHtml(
                    format!(
                        "<a href=\"{}\"{title} target=\"_blank\"{data}>",
                        escape_attr(dest_url)
                    )
                    .into(),
                ));
            }
            Event::Start(Tag::Image { .. }) => {
                in_link = true;
                events.push(event);
            }
            Event::End(TagEnd::Link) => {
                in_link = false;
                if own_link {
                    own_link = false;
                    events.push(Event::InlineHtml("</a>".into()));
                } else {
                    events.push(event);
                }
            }
            Event::End(TagEnd::Image) => {
                in_link = false;
                events.push(event);
            }
            Event::SoftBreak => events.push(Event::HardBreak),
            Event::Text(t) if in_link => events.push(Event::Text(emojify(&t).into())),
            Event::Text(t) => events.extend(autolink(&t, repo)),
            _ => events.push(event),
        }
    }
    let mut out = String::new();
    html::push_html(&mut out, events.into_iter());
    ammonia::Builder::default()
        .add_generic_attributes(["align"])
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .add_tag_attributes("a", ["data-grenadine", "target"])
        .add_tag_attributes("span", ["class"])
        .add_tag_attributes("code", ["class"])
        .add_tags(["input", "details", "summary"])
        .link_rel(Some("noopener noreferrer"))
        .attribute_filter(|tag, attr, value| match (tag, attr) {
            ("span" | "code", "class")
                if !value.is_empty()
                    && value
                        .split(' ')
                        .all(|c| c.starts_with("sy-") || c.starts_with("language-")) =>
            {
                Some(Cow::Borrowed(value))
            }
            ("a", "data-grenadine") if valid_grenadine_hash(value) => Some(Cow::Borrowed(value)),
            ("a", "target") if value == "_blank" => Some(Cow::Borrowed(value)),
            ("span" | "code", "class") | ("a", "data-grenadine" | "target") => None,
            _ => Some(Cow::Borrowed(value)),
        })
        .clean(&out)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(markdown: &str) -> String {
        to_html(markdown, "o/r")
    }

    #[test]
    fn soft_breaks_become_line_breaks() {
        assert_eq!(html("a\nb"), "<p>a<br>\nb</p>\n");
    }

    #[test]
    fn links_bare_urls_and_emails() {
        let h = html("see https://example.com/c");
        assert!(h.contains(r#"href="https://example.com/c""#));
        assert!(h.contains(">https://example.com/c</a>"));
        let h = html("see www.example.com/c");
        assert!(h.contains(r#"href="https://www.example.com/c""#), "{h}");
        assert!(h.contains(">www.example.com/c</a>"));
        let h = html("mail a@b.com");
        assert!(h.contains(r#"href="mailto:a@b.com""#));
        assert!(h.contains(">a@b.com</a>"));
    }

    #[test]
    fn urls_in_code_are_not_linked() {
        assert!(!html("`https://a.b`").contains("<a "));
        assert!(!html("```\nhttps://a.b\n```").contains("<a "));
    }

    #[test]
    fn existing_links_are_not_double_linked() {
        assert_eq!(
            html("[x](https://a.b)"),
            "<p><a href=\"https://a.b\" target=\"_blank\" rel=\"noopener noreferrer\">x</a></p>\n"
        );
    }

    #[test]
    fn only_external_links_open_in_new_tabs() {
        assert!(html("[x](https://a.b)").contains(r#"target="_blank""#));
        assert!(!html("[x](#frag)").contains("target"));
        let h = html("a[^1]\n\n[^1]: b");
        assert!(h.contains(r##"href="#1""##));
        assert!(!h.contains("target"));
        let h = html(r#"<a href="https://a.b" target="_top">x</a>"#);
        assert!(h.contains("x</a>"));
        assert!(!h.contains("_top"));
    }

    #[test]
    fn links_mentions_but_not_emails() {
        let h = html("hi @user");
        assert!(h.contains(r#"href="https://github.com/user""#));
        assert!(h.contains(">@user</a>"));
        let h = html("hi @org/team");
        assert!(h.contains(r#"href="https://github.com/orgs/org/teams/team""#));
        assert!(h.contains(">@org/team</a>"));
        let h = html("mail a@b.com");
        assert!(h.contains("mailto:a@b.com"));
        assert!(!h.contains(r#"href="https://github.com/b""#));
    }

    #[test]
    fn links_issue_references() {
        let h = html("#12");
        assert!(h.contains(r#"href="https://github.com/o/r/issues/12""#));
        assert!(h.contains(r##"data-grenadine="#/o/r/12""##));
        let h = html("x/y#34");
        assert!(h.contains(r#"href="https://github.com/x/y/issues/34""#));
        assert!(h.contains(r##"data-grenadine="#/x/y/34""##));
    }

    #[test]
    fn marks_explicit_pull_links_but_not_issue_links() {
        let h = html("[x](https://github.com/o/r/pull/5)");
        assert!(h.contains(r##"data-grenadine="#/o/r/5""##));
        let h = html("[x](https://github.com/o/r/issues/5)");
        assert!(!h.contains("data-grenadine"));
    }

    #[test]
    fn links_commit_shas() {
        let h = html("see abc1234");
        assert!(h.contains(r#"href="https://github.com/o/r/commit/abc1234""#));
        assert!(h.contains("<code>abc1234</code>"));
        for plain in ["deadbeef", "1234567"] {
            assert!(!html(plain).contains("<a "), "{plain} was linked");
        }
        assert!(!html("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c").contains("<a "));
    }

    #[test]
    fn substitutes_emoji() {
        assert!(html(":tada:").contains('🎉'));
        assert!(html(":nope_xyz:").contains(":nope_xyz:"));
        assert!(html("`:tada:`").contains(":tada:"));
        assert!(html("[:tada:](https://a.b)").contains('🎉'));
    }

    #[test]
    fn highlights_known_code_block_languages() {
        let h = html("```rust\nfn f() {}\n```");
        assert!(h.contains(r#"<code class="language-rust">"#));
        assert!(h.contains("sy-"));
    }

    #[test]
    fn unknown_code_block_languages_stay_plain() {
        let h = html("```unknownlang\nfn f() {}\n```");
        assert!(!h.contains("sy-"));
        assert!(h.contains("fn f() {}"));
    }

    #[test]
    fn strips_scripts_and_bad_classes() {
        assert!(!html("<script>alert(1)</script>").contains("script"));
        assert!(!html("<span class=\"evil\">x</span>").contains("evil"));
    }
}
