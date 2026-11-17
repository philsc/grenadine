//! The order diffs list their files in: git's path order, except that a
//! C/C++ header comes right before the other files with its directory and
//! stem, so `foo.h` is read before `foo.cpp`.

use std::borrow::Cow;
use std::cmp::Ordering;

const HEADERS: &[&str] = &["h", "hh", "hpp", "hxx", "h++", "cuh"];

/// What to sort `path` by. A header's path gets a NUL, which no path has,
/// right after its stem's dot, so it sorts before every path that starts
/// with `dir/stem.`. All other paths sort as themselves.
pub fn key(path: &str) -> Cow<'_, str> {
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    let Some(dot) = path[name_start..].rfind('.').map(|i| name_start + i) else {
        return Cow::Borrowed(path);
    };
    let ext = &path[dot + 1..];
    // A dotfile like `.h` has no stem to pair with.
    if dot == name_start || !HEADERS.iter().any(|h| h.eq_ignore_ascii_case(ext)) {
        return Cow::Borrowed(path);
    }
    Cow::Owned(format!("{}.\0{ext}", &path[..dot]))
}

/// Compares two paths in the order diffs list their files.
pub fn cmp(a: &str, b: &str) -> Ordering {
    key(a).cmp(&key(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(paths: &[&str]) -> Vec<String> {
        let mut paths: Vec<&str> = paths.to_vec();
        paths.sort_by(|a, b| cmp(a, b));
        paths.into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn headers_come_before_their_sources() {
        assert_eq!(
            sorted(&["bar.cpp", "bar.h", "foo.cc", "foo.hh", "main.cpp"]),
            ["bar.h", "bar.cpp", "foo.hh", "foo.cc", "main.cpp"]
        );
        assert_eq!(
            sorted(&["src/a.cpp", "src/a.inl", "src/a.hpp"]),
            ["src/a.hpp", "src/a.cpp", "src/a.inl"]
        );
    }

    #[test]
    fn every_header_extension_counts() {
        for ext in ["h", "hh", "hpp", "hxx", "h++", "cuh", "H", "HPP"] {
            let header = format!("x.{ext}");
            assert_eq!(sorted(&["x.c", &header]), [header.as_str(), "x.c"]);
        }
        assert_eq!(sorted(&["x.c", "x.ipp"]), ["x.c", "x.ipp"]);
    }

    #[test]
    fn other_files_keep_path_order() {
        let paths = [
            "a/foo.h",
            "foo-bar.cpp",
            "foo.h",
            "foo_test.cpp",
            "foob.cpp",
            "z.h",
        ];
        assert_eq!(sorted(&paths), paths);
    }

    #[test]
    fn headers_sharing_a_stem_keep_path_order() {
        assert_eq!(
            sorted(&["x.cpp", "x.hpp", "x.h"]),
            ["x.h", "x.hpp", "x.cpp"]
        );
    }

    #[test]
    fn a_header_comes_before_anything_starting_with_its_stem() {
        assert_eq!(
            sorted(&["foo", "foo.cpp", "foo.d/x", "foo.h"]),
            ["foo", "foo.h", "foo.cpp", "foo.d/x"]
        );
    }

    #[test]
    fn dotfiles_are_not_headers() {
        assert_eq!(sorted(&[".c", ".h"]), [".c", ".h"]);
        assert_eq!(key("dir.h/x"), "dir.h/x");
    }
}
