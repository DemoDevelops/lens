//! read_skeleton: Read(whole code file, unedited) → lens_skeleton classifier.

use std::collections::HashSet;

/// Code extensions eligible for the skeleton reroute (case-insensitive,
/// matched after the last `.`). The plan's minimum set plus sensible
/// siblings (`jsx` alongside `js`/`tsx`; `h`/`hpp` alongside `c`/`cpp`).
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "swift", "java", "c", "cpp", "h", "hpp", "rb", "kt",
];

/// Lowercased file extension of `path`, if any (`src/Foo.RS` → `rs`).
fn extension(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Is this Read a whole, unedited code file — the shape [`reason`] answers with
/// `lens_skeleton` instead of a full-file dump? True only when: `path` is a code
/// file ([`CODE_EXTENSIONS`]), the call has no `offset`/`limit` (an
/// already-bounded Read is left alone), and `path` hasn't been edited yet this
/// session (`edited`) — once a file has been edited, a further Read of it is
/// assumed to be for the next edit and must stay a real, full Read.
pub fn read_is_skeletonizable(
    path: &str,
    has_offset_or_limit: bool,
    edited: &HashSet<String>,
) -> bool {
    !has_offset_or_limit
        && !edited.contains(path)
        && extension(path).is_some_and(|ext| CODE_EXTENSIONS.contains(&ext.as_str()))
}

/// Deny reason for a skeletonizable Read: mirrors
/// [`crate::routing::GREP_FIRST_DENY_REASON`]'s shape, keyed on the Read's
/// `path` instead of the prompt's phrasing.
pub fn reason(path: &str) -> String {
    format!(
        "This Read pulls in a whole code file you haven't edited — lens_skeleton answers it without the full-file dump. First: lens_skeleton(path=\"{path}\"). Need one function's body verbatim? lens_skeleton(path=\"{path}\", include_bodies=[\"the_fn\"]) returns it in the same call. Read is for when you're about to Edit (Edit must match exact bytes) — once you've edited this file this session, Read passes through untouched. If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_skeleton,lens_recall\")."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_unedited_rust_file_is_skeletonizable() {
        assert!(read_is_skeletonizable("src/lib.rs", false, &HashSet::new()));
    }

    #[test]
    fn already_edited_path_is_not_skeletonizable() {
        let mut edited = HashSet::new();
        edited.insert("src/lib.rs".to_string());
        assert!(!read_is_skeletonizable("src/lib.rs", false, &edited));
    }

    #[test]
    fn bounded_read_is_not_skeletonizable() {
        assert!(!read_is_skeletonizable("src/lib.rs", true, &HashSet::new()));
    }

    #[test]
    fn markdown_is_not_skeletonizable() {
        assert!(!read_is_skeletonizable("README.md", false, &HashSet::new()));
    }

    #[test]
    fn json_is_not_skeletonizable() {
        assert!(!read_is_skeletonizable(
            "package.json",
            false,
            &HashSet::new()
        ));
    }

    #[test]
    fn whole_unedited_python_file_is_skeletonizable() {
        assert!(read_is_skeletonizable(
            "scripts/build.py",
            false,
            &HashSet::new()
        ));
    }

    #[test]
    fn reason_names_the_skeleton_call_and_include_bodies() {
        let r = reason("src/x.rs");
        assert!(r.contains("lens_skeleton"));
        assert!(r.contains("include_bodies"));
    }
}
