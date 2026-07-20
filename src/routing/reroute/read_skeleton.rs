//! read_skeleton: Read(whole code file, unedited) → lens_skeleton classifier.

use std::collections::HashSet;

use serde_json::Value;

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

/// Is this Read a whole, unedited code file — the shape [`deny_reason`] answers
/// with `lens_skeleton` instead of a full-file dump? True only when: `path` is a code
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

/// Per-file throttle key for the rskel rail, so each distinct file gets its
/// own one-shot instead of sharing a single per-session key. Mirrors the
/// `elink:{sym}` per-symbol pattern (`crate::routing::edited_decl_symbol`'s
/// caller in `mod.rs`), keyed on `path` instead of a symbol.
pub(crate) fn rskel_key(path: &str) -> String {
    format!("read-skeleton:{path}")
}

/// Is this Read's `tool_input` a bounded (`offset`/`limit`) read of a code
/// file? This is the shape [`read_is_skeletonizable`] exempts from the
/// whole-file skeleton deny — its correct target isn't `lens_skeleton` (it's
/// already bounded) but `lens_run`, so routing can point it there
/// instead of silently passing it through.
pub(crate) fn read_is_analysis_shaped(tool_input: &Value) -> bool {
    let path = tool_input["file_path"].as_str().unwrap_or("");
    let has_offset_or_limit =
        tool_input.get("offset").is_some() || tool_input.get("limit").is_some();
    has_offset_or_limit && extension(path).is_some_and(|ext| CODE_EXTENSIONS.contains(&ext.as_str()))
}

/// Deny reason for a skeletonizable Read: mirrors
/// [`crate::routing::GREP_FIRST_DENY_REASON`]'s shape, keyed on the Read's
/// `path` instead of the prompt's phrasing.
pub fn deny_reason(path: &str) -> String {
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
    fn rskel_key_is_per_file() {
        assert_eq!(rskel_key("src/lib.rs"), "read-skeleton:src/lib.rs");
        assert_ne!(rskel_key("src/lib.rs"), rskel_key("src/main.rs"));
    }

    #[test]
    fn offset_read_of_code_file_is_analysis_shaped() {
        let input = serde_json::json!({"file_path": "src/lib.rs", "offset": 10});
        assert!(read_is_analysis_shaped(&input));
    }

    #[test]
    fn limit_read_of_code_file_is_analysis_shaped() {
        let input = serde_json::json!({"file_path": "src/lib.rs", "limit": 50});
        assert!(read_is_analysis_shaped(&input));
    }

    #[test]
    fn whole_file_read_is_not_analysis_shaped() {
        let input = serde_json::json!({"file_path": "src/lib.rs"});
        assert!(!read_is_analysis_shaped(&input));
    }

    #[test]
    fn offset_read_of_non_code_file_is_not_analysis_shaped() {
        let input = serde_json::json!({"file_path": "README.md", "offset": 10});
        assert!(!read_is_analysis_shaped(&input));
    }

    #[test]
    fn deny_reason_names_the_skeleton_call_and_include_bodies() {
        let r = deny_reason("src/x.rs");
        assert!(r.contains("lens_skeleton"));
        assert!(r.contains("include_bodies"));
    }

}
