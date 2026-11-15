//! Writes syntect's default syntaxes plus the ones bundled in
//! src/syntaxes/ as one uncompressed dump, the format syntect ships its
//! defaults in. Built at build time because linking a set takes ~100ms,
//! while loading a dump takes ~1ms.

use syntect::dumps::dump_to_uncompressed_file;
use syntect::parsing::{SyntaxDefinition, SyntaxSet};

const EXTRA_SYNTAXES: &[&str] = &[include_str!("src/syntaxes/salt.sublime-syntax")];

fn main() {
    let out = std::env::args().nth(1).expect("usage: syntax_dump OUT");
    let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
    for source in EXTRA_SYNTAXES {
        builder.add(
            SyntaxDefinition::load_from_str(source, true, None)
                .expect("a bundled syntax doesn't parse"),
        );
    }
    dump_to_uncompressed_file(&builder.build(), out).expect("can't write the dump");
}
