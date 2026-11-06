//! Syntax highlighting with syntect, emitting CSS classes so that the light
//! and dark themes are pure CSS.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;

use grenadine_core::api::Blob;
use syntect::highlighting::ThemeSet;
use syntect::html::{
    ClassStyle, ClassedHTMLGenerator, css_for_theme_with_class_style, line_tokens_to_classed_spans,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

const CLASS_STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "sy-" };

/// Files with more lines than this are shown without highlighting.
const MAX_LINES: usize = 20_000;

thread_local! {
    static SYNTAXES: OnceCell<SyntaxSet> = const { OnceCell::new() };
}

fn with_syntaxes<R>(f: impl FnOnce(&SyntaxSet) -> R) -> R {
    SYNTAXES.with(|cell| f(cell.get_or_init(SyntaxSet::load_defaults_newlines)))
}

/// The stylesheet for both themes. Every rule is scoped to
/// `:root[data-theme="light"]` or `:root[data-theme="dark"]` so that only
/// one theme's rules can ever match; the light theme's more specific
/// selectors would otherwise win over the dark theme's plainer ones.
pub fn stylesheet() -> String {
    let themes = ThemeSet::load_defaults();
    let light = css_for_theme_with_class_style(&themes.themes["InspiredGitHub"], CLASS_STYLE)
        .unwrap_or_default();
    let dark = css_for_theme_with_class_style(&themes.themes["base16-ocean.dark"], CLASS_STYLE)
        .unwrap_or_default();
    // Only token colors are wanted; the diff sets its own backgrounds.
    let strip = |css: String| {
        css.lines()
            .filter(|l| !l.trim_start().starts_with("background-color"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "{}\n{}\n",
        scope(&enforce(&strip(light), &LIGHT_THEME), LIGHT_THEME.scope),
        scope(&enforce(&strip(dark), &DARK_THEME), DARK_THEME.scope)
    )
}

/// The WCAG 2.x contrast ratio a token colour must reach against every
/// background it can sit on: the AA bar for normal text.
const MIN_CONTRAST: f64 = 4.5;

/// A background that highlighted code can appear on, named by its
/// variable in style.css. `bases` lists the variables the value is
/// alpha-composited onto when it is translucent; empty means opaque.
struct Background {
    var: &'static str,
    value: &'static str,
    bases: &'static [&'static str],
}

const fn bg(var: &'static str, value: &'static str, bases: &'static [&'static str]) -> Background {
    Background { var, value, bases }
}

/// One theme's selector scope, foreground, and the backgrounds behind
/// highlighted code. The values mirror style.css and a test keeps them
/// in sync.
struct ThemeColors {
    scope: &'static str,
    fg: Background,
    backgrounds: &'static [Background],
}

/// The backgrounds covered: the page background (`.diff td.code`), the
/// subtle background (`.markdown pre`, tool inputs, blank diff cells),
/// the added/removed line backgrounds, and the upstream/mixed stripes
/// composited over each of them. The word-diff marks inside added and
/// removed lines are not backgrounds to meet the bar against: text
/// inside a changed-word mark may fall slightly below 4.5:1, which is
/// accepted because meeting the bar there would wash the syntax
/// colours out toward the foreground.
const LIGHT_BACKGROUNDS: &[Background] = &[
    bg("--bg", "#ffffff", &[]),
    bg("--bg-subtle", "#f6f8fa", &[]),
    bg("--add-bg", "#e6ffec", &["--bg"]),
    bg("--del-bg", "#ffebe9", &["--bg"]),
    bg(
        "--upstream-stripe",
        "rgba(128, 128, 128, 0.12)",
        &["--bg", "--add-bg", "--del-bg"],
    ),
    bg(
        "--mixed-stripe",
        "rgba(212, 167, 44, 0.25)",
        &["--bg", "--add-bg", "--del-bg"],
    ),
];

const DARK_BACKGROUNDS: &[Background] = &[
    bg("--bg", "#0d1117", &[]),
    bg("--bg-subtle", "#161b22", &[]),
    bg("--add-bg", "rgba(46, 160, 67, 0.15)", &["--bg"]),
    bg("--del-bg", "rgba(248, 81, 73, 0.1)", &["--bg"]),
    bg(
        "--upstream-stripe",
        "rgba(200, 200, 200, 0.08)",
        &["--bg", "--add-bg", "--del-bg"],
    ),
    bg(
        "--mixed-stripe",
        "rgba(210, 153, 34, 0.18)",
        &["--bg", "--add-bg", "--del-bg"],
    ),
];

const LIGHT_THEME: ThemeColors = ThemeColors {
    scope: ":root[data-theme=\"light\"]",
    fg: bg("--fg", "#1f2328", &[]),
    backgrounds: LIGHT_BACKGROUNDS,
};

const DARK_THEME: ThemeColors = ThemeColors {
    scope: ":root[data-theme=\"dark\"]",
    fg: bg("--fg", "#e6edf3", &[]),
    backgrounds: DARK_BACKGROUNDS,
};

/// An sRGB colour; `a` is the alpha for compositing.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Color {
    r: u8,
    g: u8,
    b: u8,
    a: f64,
}

/// Parses `#rgb`, `#rrggbb`, `rgb(r, g, b)` and `rgba(r, g, b, a)`.
fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some(match hex.len() {
            3 => Color {
                r: ((v >> 8) & 0xf) as u8 * 0x11,
                g: ((v >> 4) & 0xf) as u8 * 0x11,
                b: (v & 0xf) as u8 * 0x11,
                a: 1.0,
            },
            6 => Color {
                r: (v >> 16) as u8,
                g: (v >> 8) as u8,
                b: v as u8,
                a: 1.0,
            },
            _ => return None,
        });
    }
    let inner = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))?
        .strip_suffix(')')?;
    let parts: Vec<f64> = inner
        .split(',')
        .map(|p| p.trim().parse().ok())
        .collect::<Option<_>>()?;
    match *parts.as_slice() {
        [r, g, b] | [r, g, b, _] => Some(Color {
            r: r as u8,
            g: g as u8,
            b: b as u8,
            a: *parts.get(3).unwrap_or(&1.0),
        }),
        _ => None,
    }
}

/// `over` alpha-composited onto `under`.
fn composite(over: Color, under: Color) -> Color {
    let mix = |o: u8, u: u8| (over.a * o as f64 + (1.0 - over.a) * u as f64).round() as u8;
    Color {
        r: mix(over.r, under.r),
        g: mix(over.g, under.g),
        b: mix(over.b, under.b),
        a: 1.0,
    }
}

/// The relative luminance of an opaque colour, per WCAG 2.x.
fn luminance(c: Color) -> f64 {
    let channel = |v: u8| {
        let s = v as f64 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
}

/// The WCAG 2.x contrast ratio between two opaque colours.
fn contrast(a: Color, b: Color) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The effective opaque colour of each background, composited over each
/// of its bases.
fn backgrounds(theme: &ThemeColors) -> Vec<Color> {
    // A translucent colour resolved as a base sits on `--bg`.
    fn resolve(theme: &ThemeColors, var: &str) -> Color {
        let b = theme
            .backgrounds
            .iter()
            .find(|b| b.var == var)
            .expect("unknown `bases` background");
        let c = parse_color(b.value).expect("unparseable colour constant");
        if c.a >= 1.0 {
            c
        } else {
            composite(c, resolve(theme, "--bg"))
        }
    }
    theme
        .backgrounds
        .iter()
        .flat_map(|b| {
            let c = parse_color(b.value).expect("unparseable colour constant");
            if b.bases.is_empty() {
                vec![resolve(theme, b.var)]
            } else {
                b.bases
                    .iter()
                    .map(|&base| composite(c, resolve(theme, base)))
                    .collect()
            }
        })
        .collect()
}

/// Rewrites each `color: #…` declaration in `css` so that it contrasts at
/// least MIN_CONTRAST with every one of the theme's backgrounds. A
/// colour that falls short is blended toward the theme's foreground in
/// 2% steps; the syntect themes were designed for their own
/// backgrounds, not these, so their off-target colours are pulled
/// toward the foreground rather than re-picked.
fn enforce(css: &str, theme: &ThemeColors) -> String {
    let fg = parse_color(theme.fg.value).expect("unparseable --fg");
    let bgs = backgrounds(theme);
    let passes = |c: Color| bgs.iter().all(|&b| contrast(c, b) >= MIN_CONTRAST);
    css.lines()
        .map(|line| {
            let Some(decl) = line.trim_start().strip_prefix("color:") else {
                return line.to_owned();
            };
            let value = decl.trim().trim_end_matches(';');
            let Some(color) = parse_color(value) else {
                return line.to_owned();
            };
            let mut fixed = color;
            for step in 1..=50 {
                if passes(fixed) {
                    break;
                }
                let t = step as f64 / 50.0;
                let blend = |c: u8, f: u8| (c as f64 + (f as f64 - c as f64) * t).round() as u8;
                fixed = Color {
                    r: blend(color.r, fg.r),
                    g: blend(color.g, fg.g),
                    b: blend(color.b, fg.b),
                    a: 1.0,
                };
            }
            let indent = &line[..line.len() - line.trim_start().len()];
            format!(
                "{indent}color: #{:02x}{:02x}{:02x};",
                fixed.r, fixed.g, fixed.b
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Prepends `prefix` to every selector in `css`. Rule lines end in `{` and
/// carry a comma-separated selector list; comments and closing braces pass
/// through untouched.
fn scope(css: &str, prefix: &str) -> String {
    css.lines()
        .map(|line| {
            let trimmed = line.trim_end();
            let Some(selectors) = trimmed.strip_suffix('{') else {
                return line.to_owned();
            };
            selectors
                .split(',')
                .map(|s| format!("{prefix} {}", s.trim()))
                .collect::<Vec<_>>()
                .join(", ")
                + " {"
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn syntax_for<'a>(set: &'a SyntaxSet, path: &str) -> Option<&'a SyntaxReference> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or(name);
    set.find_syntax_by_extension(ext)
        .or_else(|| set.find_syntax_by_extension(name))
        .or_else(|| match ext {
            "bzl" | "bazel" | "star" => set.find_syntax_by_extension("py"),
            "ts" | "tsx" | "mjs" | "cjs" => set.find_syntax_by_extension("js"),
            "toml" | "lock" => set.find_syntax_by_extension("ini"),
            _ => None,
        })
        .or_else(|| match name {
            "BUILD" | "WORKSPACE" => set.find_syntax_by_extension("py"),
            _ => None,
        })
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Each line of `text`, escaped but not highlighted.
pub fn plain_lines(text: &str) -> Vec<String> {
    text.lines().map(escape).collect()
}

/// Highlights `text` and returns one HTML fragment per line (without the
/// line terminator), or escaped plain text when the language is unknown or
/// the file is too big.
pub fn highlight_lines(path: &str, text: &str) -> Vec<String> {
    let mut lines = Lines::new(path, text);
    while !lines.step(text, usize::MAX) {}
    lines.finish(text)
}

/// A text being highlighted a few lines at a time, so that a big file can
/// be highlighted between frames.
struct Lines {
    /// `None` when the text is shown plain.
    state: Option<(ParseState, ScopeStack)>,
    open_spans: isize,
    html: String,
    /// The byte offset of the next line to highlight.
    pos: usize,
}

impl Lines {
    fn new(path: &str, text: &str) -> Lines {
        let state = (text.lines().count() <= MAX_LINES)
            .then(|| with_syntaxes(|set| syntax_for(set, path).map(ParseState::new)))
            .flatten()
            .map(|p| (p, ScopeStack::new()));
        Lines {
            state,
            open_spans: 0,
            html: String::new(),
            pos: 0,
        }
    }

    /// Highlights up to `n` more lines; true once the whole text is done.
    fn step(&mut self, text: &str, n: usize) -> bool {
        let Some((parse, scopes)) = &mut self.state else {
            return true;
        };
        for line in LinesWithEndings::from(&text[self.pos..]).take(n) {
            let spans = with_syntaxes(|set| parse.parse_line(line, set))
                .map_err(syntect::Error::from)
                .and_then(|ops| line_tokens_to_classed_spans(line, &ops, CLASS_STYLE, scopes));
            let Ok((html, delta)) = spans else {
                self.state = None;
                return true;
            };
            self.html += &html;
            self.open_spans += delta;
            self.pos += line.len();
        }
        self.pos == text.len()
    }

    fn finish(mut self, text: &str) -> Vec<String> {
        if self.state.is_none() {
            return plain_lines(text);
        }
        for _ in 0..self.open_spans {
            self.html += "</span>";
        }
        let n = text.lines().count();
        let mut out = split_lines(&self.html);
        out.truncate(n);
        out.resize(n, String::new());
        out
    }
}

/// A blob's lines, highlighted.
pub type Highlighted = Arc<Vec<String>>;

/// How many highlighted blobs are kept.
const CACHE_SIZE: usize = 500;

/// How long the background highlighter works before letting the page
/// paint and handle input, in milliseconds.
const SLICE_MS: f64 = 12.0;

/// How many lines are highlighted between looks at the clock.
const LINES_PER_CHECK: usize = 16;

/// A blob as `request` knows it: its path, for the language, and an ID
/// that is the same exactly when the contents are.
type Key = (String, String);

struct Job {
    key: Key,
    urgent: bool,
    blob: Arc<Blob>,
    lines: Lines,
    done: Vec<Box<dyn FnOnce(Highlighted)>>,
}

#[derive(Default)]
struct Cache {
    blobs: HashMap<Key, Highlighted>,
    order: VecDeque<Key>,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::default();
    static QUEUE: RefCell<VecDeque<Job>> = RefCell::default();
    static RUNNING: Cell<bool> = const { Cell::new(false) };
}

/// The blob's highlighted lines, if they are ready.
pub fn cached(path: &str, id: &str) -> Option<Highlighted> {
    CACHE.with_borrow(|c| c.blobs.get(&(path.to_owned(), id.to_owned())).cloned())
}

/// Highlights a blob in the background and calls `done` with its lines.
/// Urgent requests, for what's about to be shown, go before all others;
/// within each kind, requests are worked through in order.
pub fn request(
    path: &str,
    id: &str,
    blob: Arc<Blob>,
    urgent: bool,
    done: impl FnOnce(Highlighted) + 'static,
) {
    if let Some(h) = cached(path, id) {
        done(h);
        return;
    }
    let key = (path.to_owned(), id.to_owned());
    QUEUE.with_borrow_mut(|q| {
        let job = match q.iter().position(|j| j.key == key) {
            Some(i) if urgent && !q[i].urgent => q.remove(i),
            Some(i) => {
                q[i].done.push(Box::new(done));
                return;
            }
            None => {
                let text = blob.text.as_deref().unwrap_or_default();
                let lines = Lines::new(path, text);
                Some(Job {
                    key,
                    urgent,
                    blob,
                    lines,
                    done: Vec::new(),
                })
            }
        };
        let Some(mut job) = job else { return };
        job.done.push(Box::new(done));
        job.urgent = urgent;
        let at = if urgent {
            q.iter().position(|j| !j.urgent).unwrap_or(q.len())
        } else {
            q.len()
        };
        q.insert(at, job);
    });
    if !RUNNING.replace(true) {
        leptos::task::spawn_local(work());
    }
}

/// Lets the requests made from now on go before all queued ones, e.g.
/// when the page shows another diff.
pub fn demote() {
    QUEUE.with_borrow_mut(|q| {
        for j in q {
            j.urgent = false;
        }
    });
}

/// Works through the queue, a slice at a time.
async fn work() {
    loop {
        let deadline = js_sys::Date::now() + SLICE_MS;
        while js_sys::Date::now() < deadline {
            let Some(job) = QUEUE.with_borrow_mut(|q| {
                let job = q.front_mut()?;
                let text = job.blob.text.as_deref().unwrap_or_default();
                job.lines
                    .step(text, LINES_PER_CHECK)
                    .then(|| q.pop_front())
                    .flatten()
            }) else {
                if QUEUE.with_borrow(VecDeque::is_empty) {
                    RUNNING.set(false);
                    return;
                }
                continue;
            };
            let text = job.blob.text.as_deref().unwrap_or_default();
            let lines: Highlighted = Arc::new(job.lines.finish(text));
            CACHE.with_borrow_mut(|c| {
                if c.order.len() >= CACHE_SIZE
                    && let Some(old) = c.order.pop_front()
                {
                    c.blobs.remove(&old);
                }
                c.order.push_back(job.key.clone());
                c.blobs.insert(job.key, lines.clone());
            });
            // Outside the borrows: callbacks may request more.
            for done in job.done {
                done(lines.clone());
            }
        }
        next_task().await;
    }
}

/// Resolves in a later task, after the page had a chance to paint.
pub async fn next_task() {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = web_sys::window()
            .expect("no window")
            .set_timeout_with_callback(&resolve);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Loads the syntaxes ahead of the first highlight.
pub fn warm() {
    leptos::task::spawn_local(async {
        next_task().await;
        with_syntaxes(|_| ());
    });
}

/// Highlights a fenced code block's `code`; `None` when the language token
/// is unknown or the block is too big. Markdown info strings name the
/// language ("rust") rather than a file extension, so the token is tried
/// both ways.
pub fn highlight_code(lang: &str, code: &str) -> Option<String> {
    if lang.is_empty() || code.lines().count() > MAX_LINES {
        return None;
    }
    with_syntaxes(|set| {
        let syntax = set
            .find_syntax_by_token(lang)
            .or_else(|| syntax_for(set, &format!("x.{lang}")))?;
        let mut generator = ClassedHTMLGenerator::new_with_class_style(syntax, set, CLASS_STYLE);
        for line in LinesWithEndings::from(code) {
            generator
                .parse_html_for_line_which_includes_newline(line)
                .ok()?;
        }
        Some(generator.finalize())
    })
}

/// Wraps the chars of a highlighted line that fall in `ranges` (char
/// indexes into the line's text) in `<span class="{class}">`.
pub fn mark(html: &str, ranges: &[Range<usize>], class: &str) -> String {
    if ranges.is_empty() {
        return html.to_owned();
    }
    let open_tag = format!("<span class=\"{class}\">");
    let mut out = String::with_capacity(html.len() + ranges.len() * 40);
    let mut pos = 0;
    let mut ri = 0;
    let mut open = false;
    let mut rest = html;
    while !rest.is_empty() {
        while ri < ranges.len() && pos >= ranges[ri].end {
            ri += 1;
        }
        let inside = ri < ranges.len() && pos >= ranges[ri].start;
        if rest.starts_with('<') {
            // A tag counts no chars; a mark span closes before it and is
            // reopened lazily at the next text char still in range.
            if open {
                out += "</span>";
                open = false;
            }
            let end = rest.find('>').map_or(rest.len(), |i| i + 1);
            out += &rest[..end];
            rest = &rest[end..];
            continue;
        }
        match (inside, open) {
            (true, false) => {
                out += &open_tag;
                open = true;
            }
            (false, true) => {
                out += "</span>";
                open = false;
            }
            _ => {}
        }
        // An entity is one char of the line's text.
        let end = if rest.starts_with('&') {
            rest.find(';').map_or(1, |i| i + 1)
        } else {
            rest.chars().next().map_or(1, |c| c.len_utf8())
        };
        out += &rest[..end];
        rest = &rest[end..];
        pos += 1;
    }
    if open {
        out += "</span>";
    }
    out
}

/// Splits highlighted HTML into lines, closing the spans still open at the
/// end of each line and reopening them on the next, so that every line
/// stands alone.
fn split_lines(html: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut open: Vec<&str> = Vec::new();
    let mut cur = String::new();
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with("</span>") {
            open.pop();
            cur += "</span>";
            rest = &rest["</span>".len()..];
        } else if rest.starts_with("<span") {
            let end = rest.find('>').map_or(rest.len(), |i| i + 1);
            open.push(&rest[..end]);
            cur += &rest[..end];
            rest = &rest[end..];
        } else if let Some(stripped) = rest.strip_prefix('\n') {
            cur.extend(std::iter::repeat_n("</span>", open.len()));
            lines.push(std::mem::take(&mut cur));
            cur.extend(open.iter().copied());
            rest = stripped;
        } else {
            let end = rest.find(['<', '\n']).unwrap_or(rest.len()).max(1);
            cur += rest[..end].trim_end_matches('\r');
            rest = &rest[end..];
        }
    }
    lines.push(cur);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stylesheet_scopes_every_rule_to_a_theme() {
        const LIGHT: &str = ":root[data-theme=\"light\"]";
        const DARK: &str = ":root[data-theme=\"dark\"]";
        let css = stylesheet();
        assert!(!css.contains("@media"));
        let mut saw_light = false;
        let mut saw_dark = false;
        let mut json_string_selectors = Vec::new();
        for line in css.lines() {
            let Some(selectors) = line.trim_end().strip_suffix('{') else {
                continue;
            };
            for selector in selectors.split(',').map(str::trim) {
                if let Some(rest) = selector.strip_prefix(LIGHT) {
                    assert!(rest.starts_with(' '), "{selector}");
                    saw_light = true;
                } else if let Some(rest) = selector.strip_prefix(DARK) {
                    assert!(rest.starts_with(' '), "{selector}");
                    saw_dark = true;
                } else {
                    panic!("unscoped selector: {selector}");
                }
                if selector.contains(".sy-json") && selector.contains(".sy-string") {
                    json_string_selectors.push(selector.to_owned());
                }
            }
        }
        assert!(saw_light && saw_dark);
        assert!(!json_string_selectors.is_empty());
        assert!(
            json_string_selectors.iter().all(|s| s.starts_with(LIGHT)),
            "{json_string_selectors:?}"
        );
    }

    /// The `color:` declarations of `css`, each with the theme whose
    /// scope introduces its rule.
    fn declared_colors(css: &str) -> Vec<(&'static ThemeColors, Color)> {
        let mut theme = &LIGHT_THEME;
        let mut out = Vec::new();
        for line in css.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with(DARK_THEME.scope) {
                theme = &DARK_THEME;
            } else if trimmed.starts_with(LIGHT_THEME.scope) {
                theme = &LIGHT_THEME;
            } else if let Some(decl) = trimmed.strip_prefix("color:") {
                let value = decl.trim().trim_end_matches(';');
                out.push((
                    theme,
                    parse_color(value).unwrap_or_else(|| panic!("unparseable {line}")),
                ));
            }
        }
        out
    }

    #[test]
    fn stylesheet_colors_contrast_with_their_backgrounds() {
        let colors = declared_colors(&stylesheet());
        assert!(!colors.is_empty());
        assert!(colors.iter().any(|(t, _)| t.scope == LIGHT_THEME.scope));
        assert!(colors.iter().any(|(t, _)| t.scope == DARK_THEME.scope));
        for (theme, color) in colors {
            let bgs = backgrounds(theme);
            let worst = bgs
                .iter()
                .map(|&b| contrast(color, b))
                .fold(f64::INFINITY, f64::min);
            assert!(
                worst >= MIN_CONTRAST - 0.001,
                "{color:?} is only {worst:.2}:1 on a {} background",
                theme.scope
            );
        }
    }

    /// The variables of a `:root[data-theme="…"]` block in `css`.
    fn theme_vars<'a>(css: &'a str, scope: &str) -> HashMap<&'a str, &'a str> {
        let start = css.find(scope).expect("theme block missing");
        let end = css[start..].find("\n}").map_or(css.len(), |i| start + i);
        css[start..end]
            .lines()
            .filter_map(|l| l.trim().split_once(':'))
            .filter(|(name, _)| name.starts_with("--"))
            .map(|(n, v)| (n, v.trim().trim_end_matches(';')))
            .collect()
    }

    #[test]
    fn background_constants_match_style_css() {
        let css = include_str!("../static/style.css");
        for theme in [&LIGHT_THEME, &DARK_THEME] {
            let vars = theme_vars(css, theme.scope);
            let mut checked = HashMap::new();
            for b in theme.backgrounds.iter().chain([&theme.fg]) {
                checked.insert(b.var, b.value);
            }
            for (var, value) in checked {
                assert_eq!(
                    vars.get(var).copied(),
                    Some(value),
                    "{var} in {}",
                    theme.scope
                );
            }
        }
    }

    fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 1.0 }
    }

    #[test]
    fn contrast_ratio_is_wcag() {
        assert!((contrast(rgb(0, 0, 0), rgb(255, 255, 255)) - 21.0).abs() < 0.01);
        assert!((contrast(rgb(255, 255, 255), rgb(255, 255, 255)) - 1.0).abs() < 0.01);
    }

    #[test]
    fn composite_blends_by_alpha() {
        let over = parse_color("rgba(248, 81, 73, 0.4)").unwrap();
        let under = parse_color("#0d1117").unwrap();
        let c = composite(over, under);
        assert_eq!((c.r, c.g, c.b), (107, 43, 43));
    }

    #[test]
    fn enforce_lightens_dark_comment_but_keeps_passing_colors() {
        // The dark theme's comment colour fails on `--bg` and worse on
        // the added/removed lines; the brighter text colour already passes.
        let css = ".sy-comment {\n color: #65737e;\n}\n.sy-text {\n color: #c0c5ce;\n}\n";
        let out = enforce(css, &DARK_THEME);
        assert!(out.contains("color: #c0c5ce;"));
        let comment = declared_colors(&scope(&out, DARK_THEME.scope))[0].1;
        assert_ne!(comment, rgb(0x65, 0x73, 0x7e));
        assert!(
            backgrounds(&DARK_THEME)
                .iter()
                .all(|&b| contrast(comment, b) >= MIN_CONTRAST)
        );
    }

    const RUST: &str = "/* a comment\n   over lines */\nfn main() {\n    let s = \"a < b\";\n}\n";

    #[test]
    fn highlighting_in_steps_matches_syntect() {
        let expected = with_syntaxes(|set| {
            let syntax = syntax_for(set, "x.rs").unwrap();
            let mut generator =
                ClassedHTMLGenerator::new_with_class_style(syntax, set, CLASS_STYLE);
            for line in LinesWithEndings::from(RUST) {
                generator
                    .parse_html_for_line_which_includes_newline(line)
                    .unwrap();
            }
            split_lines(&generator.finalize())[..RUST.lines().count()].to_vec()
        });
        assert_eq!(highlight_lines("x.rs", RUST), expected);
        let mut lines = Lines::new("x.rs", RUST);
        let mut steps = 1;
        while !lines.step(RUST, 1) {
            steps += 1;
        }
        assert_eq!(steps, RUST.lines().count());
        assert_eq!(lines.finish(RUST), expected);
    }

    #[test]
    fn bash_and_json_tokens_highlight() {
        assert!(highlight_code("bash", "ls -la | wc").is_some());
        assert!(highlight_code("json", "{\"a\": 1}").is_some());
    }

    #[test]
    fn unknown_languages_stay_plain() {
        assert_eq!(
            highlight_lines("x.unknown", "a < b\nc\n"),
            ["a &lt; b", "c"]
        );
    }

    #[test]
    fn mark_counts_entities_as_one_char() {
        // `&lt;` is the first char of the line's text.
        assert_eq!(
            mark("a &lt; b", std::slice::from_ref(&(2..4)), "word-del"),
            "a <span class=\"word-del\">&lt; </span>b"
        );
    }

    #[test]
    fn mark_nests_inside_highlight_spans() {
        let html = "<span class=\"a\">foo</span><span class=\"b\">bar</span>";
        let marked = mark(html, std::slice::from_ref(&(1..5)), "w");
        assert_eq!(
            marked,
            "<span class=\"a\">f<span class=\"w\">oo</span></span><span class=\"b\"><span class=\"w\">ba</span>r</span>"
        );
        assert!(!marked.contains("class=\"w\"></span><span class=\"w\""));
    }

    #[test]
    fn mark_with_no_ranges_is_identity() {
        let html = "<span class=\"a\">x</span>";
        assert_eq!(mark(html, &[], "w"), html);
    }

    #[test]
    fn spans_across_lines_are_reopened() {
        let html = "<span class=\"a\">x\n<span class=\"b\">y</span>\nz</span>\n";
        assert_eq!(
            split_lines(html),
            [
                "<span class=\"a\">x</span>",
                "<span class=\"a\"><span class=\"b\">y</span></span>",
                "<span class=\"a\">z</span>",
                "",
            ]
        );
    }
}
