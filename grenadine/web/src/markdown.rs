//! Renders PR descriptions and comments as GitHub-flavored Markdown.

use pulldown_cmark::{Options, Parser, html};

/// Markdown to sanitized HTML.
pub fn to_html(markdown: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM;
    let mut out = String::new();
    html::push_html(&mut out, Parser::new_ext(markdown, options));
    ammonia::Builder::default()
        .add_generic_attributes(["align"])
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .add_tags(["input", "details", "summary"])
        .clean(&out)
        .to_string()
}
