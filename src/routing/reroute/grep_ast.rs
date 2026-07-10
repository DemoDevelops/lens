//! grep_ast: Syntax-shaped Grep → lens_grep_ast nudge. Filled by T5.
//!
//! `syntax_shape` recognizes a Grep `pattern` that is really describing a Rust
//! syntax shape (an `impl` block, an attribute, a method call, …) rather than
//! text to search for. `nudge` translates the matched shape into the
//! equivalent tree-sitter query for `lens_grep_ast`, since grep would also
//! match that same text inside a comment or string literal, where the shape
//! doesn't apply.

use std::sync::OnceLock;

use regex::Regex;

/// A Rust syntax shape recognized in a Grep pattern, carrying whatever
/// identifier the pattern named so [`nudge`] can translate it into a
/// tree-sitter query.
#[derive(Debug, Clone, PartialEq)]
pub enum AstHint {
    /// `impl Type` — the type name.
    ImplBlock(String),
    /// `#[attr]` — the attribute identifier.
    Attribute(String),
    /// `.method()` call syntax.
    MethodCall,
    /// `->` return-type arrow.
    ReturnArrow,
    /// `async fn`.
    AsyncFn,
    /// `trait Name` definition.
    TraitDef,
    /// `for x in ...` loop.
    ForIn,
}

/// Compiled shape-detection regexes, built once.
struct Shapes {
    impl_block: Regex,
    attribute: Regex,
    async_fn: Regex,
    trait_def: Regex,
    for_in: Regex,
    method_call: Regex,
}

static SHAPES: OnceLock<Shapes> = OnceLock::new();

fn shapes() -> &'static Shapes {
    SHAPES.get_or_init(|| Shapes {
        impl_block: Regex::new(r"impl\s+(\w+)").expect("impl regex"),
        attribute: Regex::new(r"#\[(\w+)").expect("attribute regex"),
        async_fn: Regex::new(r"async\s+fn").expect("async fn regex"),
        trait_def: Regex::new(r"trait\s+\w*").expect("trait regex"),
        for_in: Regex::new(r"for\s+\w+\s+in").expect("for-in regex"),
        method_call: Regex::new(r"\.\w+\(\)").expect("method-call regex"),
    })
}

/// Undoes the two regex idioms that hide a syntax shape from the [`shapes`]
/// regexes: `pattern` is itself a regex (it's a Grep argument), so the model
/// often spells a shape as regex idiom rather than literal source form —
/// escaping metacharacters (`#\[tool\]`, `\.clone\(\)`) or spelling a gap as
/// `.*`/`.+`/`\s+` (`impl.*Forge`, `impl\s+Forge`). Stripping the backslash
/// before `[ ] ( ) { } .` undoes the first; collapsing regex gaps and runs of
/// literal spaces to a single space undoes the second, so `impl.*Forge`,
/// `impl\s+Forge`, and `impl Forge` all normalize to the same shape. The raw
/// `pattern` is kept for anything user-facing — this is only for matching.
fn normalize_pattern(pattern: &str) -> String {
    static ESCAPE_RE: OnceLock<Regex> = OnceLock::new();
    let escape_re =
        ESCAPE_RE.get_or_init(|| Regex::new(r"\\([\[\](){}.])").expect("escape regex"));
    let unescaped = escape_re.replace_all(pattern, "$1");

    static GAP_RE: OnceLock<Regex> = OnceLock::new();
    let gap_re = GAP_RE.get_or_init(|| Regex::new(r"(?:\.\*|\.\+|\\s\+| +)+").expect("gap regex"));
    gap_re.replace_all(&unescaped, " ").into_owned()
}

/// Does `pattern` (a Grep search string) describe a Rust syntax shape that
/// `lens_grep_ast` answers directly, instead of matching it as plain text?
/// Checked most-specific first (impl/attribute/async fn/trait/for-in) before
/// the generic method-call/return-arrow shapes, so a pattern that could read
/// as several shapes reports its most useful one. Plain text (`"error
/// message"`) returns `None`. Matched against [`normalize_pattern`]'s output,
/// not the raw pattern, so regex-escaped or regex-gapped shapes are caught
/// too.
pub fn syntax_shape(pattern: &str) -> Option<AstHint> {
    let normalized = normalize_pattern(pattern);
    let normalized = normalized.as_str();
    let s = shapes();
    if let Some(c) = s.impl_block.captures(normalized) {
        return Some(AstHint::ImplBlock(c[1].to_string()));
    }
    if let Some(c) = s.attribute.captures(normalized) {
        return Some(AstHint::Attribute(c[1].to_string()));
    }
    if s.async_fn.is_match(normalized) {
        return Some(AstHint::AsyncFn);
    }
    if s.trait_def.is_match(normalized) {
        return Some(AstHint::TraitDef);
    }
    if s.for_in.is_match(normalized) {
        return Some(AstHint::ForIn);
    }
    if s.method_call.is_match(normalized) {
        return Some(AstHint::MethodCall);
    }
    if normalized.contains("->") {
        return Some(AstHint::ReturnArrow);
    }
    None
}

/// The equivalent tree-sitter query for a matched [`AstHint`], verified
/// against tree-sitter-rust 0.23's `node-types.json`: `impl_item.type`,
/// `trait_item.name` and `function_item.return_type` are typed fields;
/// `attribute_item`'s only child is `attribute`; a method call is a
/// `call_expression` whose `function` is a `field_expression`.
fn query_for(hint: &AstHint) -> String {
    match hint {
        AstHint::ImplBlock(name) => {
            format!("(impl_item type: (type_identifier) @t (#eq? @t \"{name}\"))")
        }
        AstHint::Attribute(ident) => {
            format!("(attribute_item (attribute (identifier) @a (#eq? @a \"{ident}\")))")
        }
        AstHint::MethodCall => {
            "(call_expression function: (field_expression field: (field_identifier) @m))"
                .to_string()
        }
        AstHint::ReturnArrow => "(function_item return_type: (_) @r)".to_string(),
        AstHint::AsyncFn => {
            "(function_item (function_modifiers) @m (#match? @m \"async\"))".to_string()
        }
        AstHint::TraitDef => "(trait_item name: (type_identifier) @t)".to_string(),
        AstHint::ForIn => "(for_expression pattern: (_) @p value: (_) @v)".to_string(),
    }
}

/// A Context nudge for a syntax-shaped Grep: names the exact `lens_grep_ast`
/// call with the translated query, since grep would also match this same
/// text inside a comment or string literal, where the shape doesn't hold.
pub fn nudge(hint: &AstHint) -> String {
    let query = query_for(hint);
    format!(
        "This grep pattern describes Rust syntax, not text to search for — grep also matches it inside comments and string literals, where the shape doesn't apply. lens_grep_ast matches real syntax nodes instead: lens_grep_ast(language=\"rust\", query=\"{query}\"). If lens_grep_ast isn't loaded yet: ToolSearch(query: \"select:lens_grep_ast\")."
    )
}

/// Deny reason for a Grep `pattern` [`syntax_shape`] identified as a Rust
/// syntax shape: names the exact `lens_grep_ast` call with the translated
/// query, offers the `ToolSearch` bootstrap in case the lens tools aren't
/// loaded yet, and promises the retry passes (mirrors the shape of
/// `grep_symbol::reason`).
pub fn deny_reason(hint: &AstHint) -> String {
    let query = query_for(hint);
    format!(
        "This grep pattern describes Rust syntax, not text to search for — grep also matches it inside comments and string literals, where the shape doesn't apply. Answer it with lens_grep_ast(language=\"rust\", query=\"{query}\") instead. If lens_grep_ast isn't loaded yet, load it first: ToolSearch(query: \"select:lens_grep_ast\"). This fires once per prompt — the same grep will pass if you re-run it verbatim."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impl_block_shape_and_nudge() {
        let hint = syntax_shape("impl Forge");
        assert_eq!(hint, Some(AstHint::ImplBlock("Forge".to_string())));
        let n = nudge(&hint.unwrap());
        assert!(n.contains("impl_item"), "{n}");
        assert!(n.contains("lens_grep_ast"), "{n}");
        assert!(n.contains("\"Forge\""), "{n}");
    }

    #[test]
    fn attribute_shape() {
        assert_eq!(
            syntax_shape("#[tool]"),
            Some(AstHint::Attribute("tool".to_string()))
        );
    }

    #[test]
    fn plain_text_is_none() {
        assert_eq!(syntax_shape("error message"), None);
    }

    #[test]
    fn async_fn_shape() {
        assert_eq!(syntax_shape("async fn handler"), Some(AstHint::AsyncFn));
    }

    #[test]
    fn trait_def_shape() {
        assert_eq!(syntax_shape("trait Store"), Some(AstHint::TraitDef));
    }

    #[test]
    fn for_in_shape() {
        assert_eq!(syntax_shape("for item in list"), Some(AstHint::ForIn));
    }

    #[test]
    fn method_call_shape() {
        assert_eq!(syntax_shape(".unwrap()"), Some(AstHint::MethodCall));
    }

    #[test]
    fn return_arrow_shape() {
        assert_eq!(syntax_shape("-> Result<()>"), Some(AstHint::ReturnArrow));
    }

    #[test]
    fn escaped_impl_block_shape() {
        assert_eq!(
            syntax_shape("impl.*Forge"),
            Some(AstHint::ImplBlock("Forge".to_string()))
        );
    }

    #[test]
    fn escaped_attribute_shape() {
        assert_eq!(
            syntax_shape(r"#\[tool\]"),
            Some(AstHint::Attribute("tool".to_string()))
        );
    }

    #[test]
    fn escaped_method_call_shape() {
        assert_eq!(syntax_shape(r"\.clone\(\)"), Some(AstHint::MethodCall));
    }

    #[test]
    fn escaped_whitespace_gap_impl_block_shape() {
        assert_eq!(
            syntax_shape(r"impl\s+Forge"),
            Some(AstHint::ImplBlock("Forge".to_string()))
        );
    }

    #[test]
    fn deny_reason_names_tool_query_and_retry_promise() {
        let hint = AstHint::ImplBlock("Forge".to_string());
        let r = deny_reason(&hint);
        assert!(r.contains("lens_grep_ast"), "{r}");
        assert!(r.contains("impl_item"), "{r}");
        assert!(r.contains("\"Forge\""), "{r}");
        assert!(r.contains("re-run it"), "{r}");
    }

    #[test]
    fn every_hint_nudge_names_the_tool_and_a_query() {
        for hint in [
            AstHint::ImplBlock("Forge".to_string()),
            AstHint::Attribute("tool".to_string()),
            AstHint::MethodCall,
            AstHint::ReturnArrow,
            AstHint::AsyncFn,
            AstHint::TraitDef,
            AstHint::ForIn,
        ] {
            let n = nudge(&hint);
            assert!(n.contains("lens_grep_ast"), "{n}");
            assert!(n.contains("ToolSearch"), "{n}");
        }
    }
}
