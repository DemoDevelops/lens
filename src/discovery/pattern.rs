//! `$META` pattern compiler for `lens_grep_ast`: turns an ast-grep-style code
//! pattern (`$X.unwrap()`, `print($X)`, `f($$$)`) into a tree-sitter query,
//! in-binary, using the grammars lens already links (no ast-grep dependency).
//!
//! The slice is deliberately narrow: the pattern is parsed AS CODE with the
//! target grammar, `$UPPERCASE` metavariables become `(_)` wildcards, `$$$` /
//! `$$$NAME` become quantified `((_))*` wildcards (zero-or-more siblings),
//! concrete tokens become `#eq?` text predicates, and the CST structure (node
//! kinds + field names) is emitted verbatim. Repeating a single metavariable
//! requires equal text across its occurrences; repeating a variadic name does
//! not. Raw S-expressions remain the power-user path.
//!
//! Because `$X` is not a valid token in most grammars, metavariables are first
//! substituted with placeholder identifiers (`lens_meta_<i>`, one per
//! occurrence), the substituted snippet is parsed (trying a short list of
//! fragment wrappers per language until one is ERROR-free), and the placeholder
//! positions are turned back into wildcards at emission time. Languages where
//! `$X` is itself lexable (php, bash variables) are outside this slice; `$$$`
//! is still recognized before parse so JS/TS cannot pin a literal `$$$`.

use std::fmt::Write as _;

use anyhow::{bail, Context, Result};
use tree_sitter::{Node, Parser, Query};

use super::tags_adapter::AnySpec;

/// Capture name given to the pattern root. The grep engine filters matches to
/// this capture so the bookkeeping captures (`@c0`, `@c1`, ...) never surface.
pub const MATCH_CAPTURE: &str = "match";

/// Placeholder identifier prefix substituted for `$METAVAR` / `$$$` occurrences
/// before parsing. Chosen to lex as a plain identifier in every supported grammar.
const PLACEHOLDER: &str = "lens_meta_";

/// One metavariable occurrence after [`substitute_metavars`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum MetaKind {
    /// `$NAME` — exactly one node. Repeated names get `#eq?` between captures.
    Single(String),
    /// `$$$` or `$$$NAME` — zero or more sibling nodes. Never `#eq?`, even when
    /// the same name repeats (variadic holes are independent).
    Variadic(String),
}

/// Compile a `$META` pattern into a tree-sitter query string for `spec`'s
/// grammar. The emitted query captures the whole pattern as `@match` and
/// enforces concrete tokens / repeated single metavariables via `#eq?`
/// predicates (which the tree-sitter Rust binding applies during matching).
pub fn compile_pattern(pattern: &str, spec: &AnySpec) -> Result<String> {
    let pat = pattern.trim();
    if pat.is_empty() {
        bail!("pattern is empty");
    }
    let (substituted, metavars) = substitute_metavars(pat);

    let lang = spec.language();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .with_context(|| format!("grammar for '{}' failed to load", spec.name()))?;

    let parsed =
        parse_with_candidates(&mut parser, &substituted, &metavars, spec.name(), pat)?;

    let roots = pattern_roots(&parsed.tree, parsed.span)?;
    let mut emitter = Emitter {
        src: parsed.source.as_bytes(),
        metavars: &metavars,
        captures: 0,
        predicates: Vec::new(),
        meta_captures: Vec::new(),
        has_concrete_token: false,
        pattern_end: parsed.pattern_end,
        whole_node_metas: &parsed.whole_node_metas,
    };
    let body = emitter.emit_roots(&roots)?;
    if !emitter.has_concrete_token {
        bail!(
            "pattern must pin at least one concrete token (a literal identifier, method \
             name, operator, etc.); a pattern made only of `$METAVAR` / `$$$` wildcards \
             matches everything: `{pat}`"
        );
    }
    let mut predicates = emitter.predicates;
    // Only single metavars participate in repeated-name equality. Variadic
    // names (even repeated `$$$ARGS`) intentionally do not.
    for (kind, caps) in &emitter.meta_captures {
        if matches!(kind, MetaKind::Variadic(_)) {
            continue;
        }
        for later in &caps[1..] {
            predicates.push(format!("(#eq? {} {})", caps[0], later));
        }
    }
    let query = if predicates.is_empty() {
        format!("({body} @{MATCH_CAPTURE})")
    } else {
        format!("({body} @{MATCH_CAPTURE} {})", predicates.join(" "))
    };
    // The emission is grammar-driven, so this only fails on an emitter bug;
    // validating here keeps that failure clear instead of surfacing later as a
    // confusing "invalid query" from the grep engine.
    Query::new(&lang, &query)
        .map_err(|e| anyhow::anyhow!("internal: compiled pattern query invalid ({e}): {query}"))?;
    Ok(query)
}

/// Replace each `$UPPER` / `$$$` / `$$$NAME` occurrence with a placeholder
/// identifier (`lens_meta_<i>`), returning the substituted text and the kind of
/// each occurrence (index = occurrence order).
///
/// `$$$` / `$$$NAME` are recognized before the single-`$` check so a bare
/// `$$$` is never left for the lexer (JS/TS would otherwise treat it as a
/// literal identifier and pin `#eq? "$$$"`).
fn substitute_metavars(pattern: &str) -> (String, Vec<MetaKind>) {
    let mut out = String::with_capacity(pattern.len());
    let mut names: Vec<MetaKind> = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        // Variadic: `$$$` or `$$$NAME` (NAME = ASCII uppercase / digit / _).
        if chars.peek() == Some(&'$') {
            let mut look = chars.clone();
            look.next(); // second `$`
            if look.peek() == Some(&'$') {
                chars.next(); // second `$`
                chars.next(); // third `$`
                let mut name = String::new();
                while let Some(&n) = chars.peek() {
                    if n.is_ascii_uppercase() || n.is_ascii_digit() || n == '_' {
                        name.push(n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let _ = write!(out, "{PLACEHOLDER}{}", names.len());
                names.push(MetaKind::Variadic(name));
                continue;
            }
        }
        // Single: `$NAME` starting with ASCII uppercase.
        if chars.peek().is_some_and(char::is_ascii_uppercase) {
            let mut name = String::new();
            while let Some(&n) = chars.peek() {
                if n.is_ascii_uppercase() || n.is_ascii_digit() || n == '_' {
                    name.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            let _ = write!(out, "{PLACEHOLDER}{}", names.len());
            names.push(MetaKind::Single(name));
            continue;
        }
        out.push(c);
    }
    (out, names)
}

/// One parse candidate: source text, optional byte-span of the user pattern
/// inside it, and single-metavar occurrences that should match a whole node
/// (body-promote scaffolding) rather than a child inside braces we added.
struct Candidate {
    source: String,
    /// When set, only the CST overlapping `[start, end)` is structural; children
    /// at/after `end` are wrapper scaffolding (empty `{}`, method body, …).
    span: Option<(usize, usize)>,
    whole_node_metas: Vec<usize>,
}

/// Grammars whose top level rejects bare expressions/statements/fragments get
/// an ordered list of wrappers tried until one parses without real errors.
fn snippet_candidates(lang: &str, substituted: &str, metavars: &[MetaKind]) -> Vec<Candidate> {
    let as_is = Candidate {
        source: substituted.to_string(),
        span: None,
        whole_node_metas: Vec::new(),
    };
    let mut out = vec![as_is];
    if lang != "rust" {
        return out;
    }

    // Bare fn signature → item: `fn foo(a: i32)` + ` { }`.
    out.push(Candidate {
        source: format!("{substituted} {{ }}"),
        span: Some((0, substituted.len())),
        whole_node_metas: Vec::new(),
    });

    // Expression / statement in a fn body (legacy wrapper).
    out.push(Candidate {
        source: format!("fn __lens_p__() {{ {substituted} }}"),
        span: Some((
            "fn __lens_p__() { ".len(),
            "fn __lens_p__() { ".len() + substituted.len(),
        )),
        whole_node_metas: Vec::new(),
    });

    // Bare method: needs an impl + empty body scaffolding.
    out.push(Candidate {
        source: format!("impl __LensP__ {{ {substituted} {{ }} }}"),
        span: Some((
            "impl __LensP__ { ".len(),
            "impl __LensP__ { ".len() + substituted.len(),
        )),
        whole_node_metas: Vec::new(),
    });

    // Match arm (trailing comma is part of match_arm CST; span stays on the arm text).
    out.push(Candidate {
        source: format!("match __lens_m__ {{ {substituted} , }}"),
        span: Some((
            "match __lens_m__ { ".len(),
            "match __lens_m__ { ".len() + substituted.len(),
        )),
        whole_node_metas: Vec::new(),
    });

    // Attr + bare fn name: `#[test] fn $NAME` → add `() {}`.
    out.push(Candidate {
        source: format!("{substituted}() {{}}"),
        span: Some((0, substituted.len())),
        whole_node_metas: Vec::new(),
    });

    // `fn … $BODY` where $BODY stands for the whole block: wrap the trailing
    // placeholder in braces so the fn item parses, and mark that occurrence so
    // emission collapses the scaffolding block to a single-node wildcard.
    if let Some((promoted, occ)) = promote_trailing_meta_to_block(substituted, metavars) {
        out.push(Candidate {
            source: promoted,
            span: None,
            whole_node_metas: vec![occ],
        });
    }

    out
}

/// If `substituted` looks like a fn signature ending in a single-metavar
/// placeholder (ast-grep `$BODY` for the function body), return the source with
/// that placeholder wrapped in `{ … }` and the occurrence index.
fn promote_trailing_meta_to_block(
    substituted: &str,
    metavars: &[MetaKind],
) -> Option<(String, usize)> {
    let trimmed = substituted.trim_end();
    if !trimmed.contains("fn ") {
        return None;
    }
    // Must not already end with a block.
    if trimmed.ends_with('}') {
        return None;
    }
    let (occ, meta_tok) = trailing_placeholder(trimmed)?;
    if !matches!(metavars.get(occ), Some(MetaKind::Single(_))) {
        return None;
    }
    let before = trimmed[..trimmed.len() - meta_tok.len()].trim_end();
    // Signature-ish: closes params or a return type, not mid-token.
    if before.is_empty() {
        return None;
    }
    Some((format!("{before} {{ {meta_tok} }}"), occ))
}

/// Last token if it is exactly `lens_meta_<i>`.
fn trailing_placeholder(text: &str) -> Option<(usize, &str)> {
    let tok_start = text.rfind(PLACEHOLDER)?;
    // Token must be at the end (only trailing whitespace already trimmed).
    let tok = &text[tok_start..];
    if tok.contains(char::is_whitespace) {
        return None;
    }
    // Preceding char must be a separator, not part of a larger ident.
    if tok_start > 0 {
        let prev = text[..tok_start].chars().next_back()?;
        if prev.is_ascii_alphanumeric() || prev == '_' {
            return None;
        }
    }
    let idx: usize = tok.strip_prefix(PLACEHOLDER)?.parse().ok()?;
    Some((idx, tok))
}

/// Successful fragment parse: tree + source + where the user pattern sits.
struct ParsedPattern {
    tree: tree_sitter::Tree,
    source: String,
    span: Option<(usize, usize)>,
    pattern_end: Option<usize>,
    whole_node_metas: Vec<usize>,
}

fn parse_with_candidates(
    parser: &mut Parser,
    substituted: &str,
    metavars: &[MetaKind],
    lang_name: &str,
    pat: &str,
) -> Result<ParsedPattern> {
    let candidates = snippet_candidates(lang_name, substituted, metavars);
    for cand in candidates {
        let tree = parser
            .parse(&cand.source, None)
            .with_context(|| format!("parser failure for '{lang_name}'"))?;
        if !parse_acceptable(tree.root_node(), cand.source.as_bytes(), metavars) {
            continue;
        }
        let pattern_end = cand.span.map(|(_, e)| e);
        return Ok(ParsedPattern {
            tree,
            source: cand.source,
            span: cand.span,
            pattern_end,
            whole_node_metas: cand.whole_node_metas,
        });
    }
    bail!("{}", diagnose_parse_failure(pat, lang_name));
}

/// Shape-specific diagnosis when no fragment wrapper produced an ERROR-free parse.
fn diagnose_parse_failure(pat: &str, lang_name: &str) -> String {
    let mut shapes: Vec<&str> = Vec::new();
    let opens = pat
        .chars()
        .filter(|c| matches!(c, '{' | '(' | '['))
        .count();
    let closes = pat
        .chars()
        .filter(|c| matches!(c, '}' | ')' | ']'))
        .count();
    if opens != closes {
        shapes.push("unbalanced braces/parens");
    }
    if pat.contains("=>") && !pat.contains("match") {
        shapes.push("`=>` arm outside match");
    }
    let codeish = pat.contains('(')
        || pat.contains('{')
        || pat.contains("fn ")
        || pat.contains('$')
        || pat.contains("impl ")
        || pat.contains("def ")
        || pat.contains("function ");
    if !codeish || pat.split_whitespace().count() > 12 {
        shapes.push("non-code prose");
    }
    let shape = if shapes.is_empty() {
        "unrecognized fragment".to_string()
    } else {
        shapes.join(" / ")
    };
    format!(
        "pattern does not parse as {lang_name} (likely {shape}): `{pat}`. \
         Try a code-shaped fragment, e.g. `fn $NAME($$$) {{ $$$BODY }}` or \
         `Some($X) => $X` (match arms are auto-wrapped)."
    )
}

/// `has_error` is fine when every ERROR node is solely a placeholder (e.g.
/// `impl Forge { $$$ }` puts the variadic hole in an ERROR child). Missing
/// tokens (bare `fn foo()`'s implied `;`) still fail so a later candidate can
/// supply braces.
fn parse_acceptable(root: Node<'_>, src: &[u8], metavars: &[MetaKind]) -> bool {
    if !root.has_error() {
        return true;
    }
    !has_disallowed_error(root, src, metavars)
}

fn has_disallowed_error(node: Node<'_>, src: &[u8], metavars: &[MetaKind]) -> bool {
    if node.is_missing() {
        return true;
    }
    if node.is_error() {
        let text = node.utf8_text(src).unwrap_or("").trim();
        if placeholder_index(text, metavars.len()).is_some() {
            return false;
        }
        // ERROR wrapping a single placeholder identifier child.
        if node.named_child_count() == 1 {
            if let Some(ch) = node.named_child(0) {
                let ct = ch.utf8_text(src).unwrap_or("").trim();
                if placeholder_index(ct, metavars.len()).is_some() {
                    return false;
                }
            }
        }
        return true;
    }
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            if has_disallowed_error(c.node(), src, metavars) {
                return true;
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

fn placeholder_index(text: &str, n_metas: usize) -> Option<usize> {
    let idx: usize = text.strip_prefix(PLACEHOLDER)?.parse().ok()?;
    (idx < n_metas).then_some(idx)
}

/// Locate the node(s) the pattern denotes. Without a wrapper span: the named
/// children of the file root (one expression/item, or rust `attribute_item`+
/// item siblings). With a span: the smallest named node covering it (may
/// extend past `end` when the candidate appended scaffolding like ` { }` or a
/// match-arm comma).
fn pattern_roots<'a>(
    tree: &'a tree_sitter::Tree,
    span: Option<(usize, usize)>,
) -> Result<Vec<Node<'a>>> {
    let root = tree.root_node();
    match span {
        None => {
            let nodes = top_level_pattern_nodes(root)?;
            ensure_single_or_attr_prefix(&nodes)?;
            Ok(nodes)
        }
        Some((start, end)) => {
            let node = root
                .named_descendant_for_byte_range(start, end)
                .context("pattern node not found in wrapped parse")?;
            // Whole-file hit (attr + item with suffix scaffolding): take the
            // top-level named children and let pattern_end trim each.
            if node.id() == root.id()
                || (node.start_byte() == root.start_byte() && node.end_byte() == root.end_byte())
            {
                let nodes = top_level_pattern_nodes(root)?;
                ensure_single_or_attr_prefix(&nodes)?;
                return Ok(nodes);
            }
            // Must cover the pattern bytes (may extend past `end` for suffix
            // bodies / match-arm commas). Reject nodes that start after `start`
            // or end before `end`.
            if node.start_byte() > start || node.end_byte() < end {
                bail!("pattern must be a single expression or statement");
            }
            Ok(vec![unwrap_expr_stmt(node)])
        }
    }
}

fn top_level_pattern_nodes<'a>(root: Node<'a>) -> Result<Vec<Node<'a>>> {
    let mut nodes = Vec::new();
    for i in 0..root.named_child_count() {
        let n = root.named_child(i).expect("count checked");
        if !n.is_extra() || n.is_error() {
            nodes.push(unwrap_expr_stmt(n));
        }
    }
    if nodes.is_empty() {
        bail!(
            "pattern must be a single expression or statement (found {})",
            root.named_child_count()
        );
    }
    Ok(nodes)
}

/// Multi-root is only for rust-style attribute prefixes (`#[test] fn …`).
/// Two statements (`a()\nb()`) stay a clear error.
fn ensure_single_or_attr_prefix(nodes: &[Node<'_>]) -> Result<()> {
    if nodes.len() <= 1 {
        return Ok(());
    }
    let prefix_ok = nodes[..nodes.len() - 1]
        .iter()
        .all(|n| n.kind() == "attribute_item");
    if !prefix_ok {
        bail!(
            "pattern must be a single expression or statement (found {})",
            nodes.len()
        );
    }
    Ok(())
}

fn unwrap_expr_stmt(mut node: Node<'_>) -> Node<'_> {
    while node.kind() == "expression_statement" && node.named_child_count() == 1 {
        node = node.named_child(0).expect("count checked");
    }
    node
}

/// Recursive CST -> query-S-expression emitter. Token leaves become wildcards
/// (metavariables) or text-constrained captures (concrete tokens); interior
/// structure is emitted verbatim with field names; comments (extras) and
/// unfielded anonymous punctuation are skipped. Variadic holes emit
/// `((_) @cN)*`. Children at/after [`Emitter::pattern_end`] are wrapper-only.
struct Emitter<'a> {
    src: &'a [u8],
    metavars: &'a [MetaKind],
    captures: usize,
    /// Accumulated `#eq?` text predicates for concrete tokens.
    predicates: Vec<String>,
    /// Metavar kind + capture names, in first-occurrence order (singles only
    /// used for `#eq?`; variadics are recorded but skipped at predicate time).
    meta_captures: Vec<(MetaKind, Vec<String>)>,
    /// Set once the pattern pins any concrete token (text `#eq?` or a
    /// fielded literal like an operator); false means the compiled query is
    /// metavariables-only and would match essentially everything.
    has_concrete_token: bool,
    /// Exclusive end of the user pattern inside a wrapped/suffixed source.
    pattern_end: Option<usize>,
    /// Single-metavar occurrences that stand for a whole node (body promote).
    whole_node_metas: &'a [usize],
}

impl Emitter<'_> {
    fn next_capture(&mut self) -> String {
        let cap = format!("@c{}", self.captures);
        self.captures += 1;
        cap
    }

    fn placeholder_occurrence(&self, text: &str) -> Option<usize> {
        placeholder_index(text, self.metavars.len())
    }

    fn in_pattern(&self, node: Node<'_>) -> bool {
        match self.pattern_end {
            Some(end) => node.start_byte() < end,
            None => true,
        }
    }

    fn emit_roots(&mut self, roots: &[Node<'_>]) -> Result<String> {
        if roots.len() == 1 {
            return self.emit(roots[0]);
        }
        // Adjacent siblings (e.g. attribute_item . function_item). The `.`
        // anchor keeps the attr tied to the immediately following item.
        let mut parts = Vec::with_capacity(roots.len());
        for r in roots {
            parts.push(self.emit(*r)?);
        }
        Ok(parts.join(" . "))
    }

    fn emit(&mut self, node: Node<'_>) -> Result<String> {
        if !self.in_pattern(node) {
            // Should not be called on out-of-span roots; treat as empty.
            bail!("internal: emit called on wrapper-only node");
        }

        // Body-promote: `{ $BODY }` scaffolding collapses so `$BODY` matches
        // the whole block node (any arity of statements), not one statement.
        if let Some(part) = self.maybe_collapse_whole_node(node)? {
            return Ok(part);
        }

        // ERROR holding only a placeholder (variadic hole in impl body, etc.).
        if node.is_error() {
            if let Some(text) = self.error_placeholder_text(node) {
                return self.emit_placeholder(&text);
            }
            bail!("pattern contains an unhandled ERROR node");
        }

        let text = node.utf8_text(self.src).unwrap_or("");
        if node.child_count() == 0 {
            return self.emit_token(node, text);
        }
        if node.named_child_count() == 0 {
            // Only anonymous children (e.g. rust `arguments` for `()`): match the
            // node kind, not its text — a text `#eq?` would be whitespace-fragile
            // and hand-written queries don't constrain arity here either.
            return Ok(format!("({})", node.kind()));
        }

        // Parent of a lone variadic child still needs the container kind so
        // `arguments: (arguments ((_) @c)*)` matches any arity including zero.
        let mut out = format!("({}", node.kind());
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if !self.in_pattern(child) {
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                    continue;
                }
                let field = cursor.field_name();
                if child.is_error() {
                    if let Some(text) = self.error_placeholder_text(child) {
                        let part = self.emit_placeholder(&text)?;
                        match field {
                            Some(f) => {
                                let _ = write!(out, " {f}: {part}");
                            }
                            None => {
                                let _ = write!(out, " {part}");
                            }
                        }
                    }
                } else if child.is_named() {
                    // Extras (comments) in the pattern are not structural.
                    if !child.is_extra() {
                        let part = self.emit(child)?;
                        match field {
                            Some(f) => {
                                let _ = write!(out, " {f}: {part}");
                            }
                            None => {
                                let _ = write!(out, " {part}");
                            }
                        }
                    }
                } else if let Some(f) = field {
                    // A fielded anonymous token carries meaning (e.g.
                    // `operator: "=="`); unfielded punctuation does not.
                    let tok = child.utf8_text(self.src).unwrap_or("");
                    self.has_concrete_token = true;
                    let _ = write!(out, " {f}: \"{}\"", escape(tok));
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        out.push(')');
        Ok(out)
    }

    /// If `node` is a block whose only named child is a body-promoted single
    /// metavar, emit that metavar as a whole-node wildcard.
    fn maybe_collapse_whole_node(&mut self, node: Node<'_>) -> Result<Option<String>> {
        if node.kind() != "block" {
            return Ok(None);
        }
        let mut only: Option<Node<'_>> = None;
        for i in 0..node.named_child_count() {
            let ch = node.named_child(i).expect("count checked");
            if ch.is_extra() && !ch.is_error() {
                continue;
            }
            if only.is_some() {
                return Ok(None);
            }
            only = Some(ch);
        }
        let Some(ch) = only else {
            return Ok(None);
        };
        let text = ch.utf8_text(self.src).unwrap_or("");
        let Some(occ) = self.placeholder_occurrence(text) else {
            return Ok(None);
        };
        if !self.whole_node_metas.contains(&occ) {
            return Ok(None);
        }
        if !matches!(self.metavars.get(occ), Some(MetaKind::Single(_))) {
            return Ok(None);
        }
        Ok(Some(self.emit_placeholder(text)?))
    }

    fn error_placeholder_text(&self, node: Node<'_>) -> Option<String> {
        let text = node.utf8_text(self.src).unwrap_or("").trim().to_string();
        if self.placeholder_occurrence(&text).is_some() {
            return Some(text);
        }
        if node.named_child_count() == 1 {
            let ch = node.named_child(0)?;
            let ct = ch.utf8_text(self.src).unwrap_or("").trim().to_string();
            if self.placeholder_occurrence(&ct).is_some() {
                return Some(ct);
            }
        }
        None
    }

    fn emit_placeholder(&mut self, text: &str) -> Result<String> {
        let occ = self
            .placeholder_occurrence(text)
            .ok_or_else(|| anyhow::anyhow!("internal: not a placeholder: {text}"))?;
        let kind = self.metavars[occ].clone();
        match kind {
            MetaKind::Variadic(_) => {
                let cap = self.next_capture();
                // Zero-or-more siblings; capture is bookkeeping only (dedup of
                // multi-match sites stays in server.rs).
                Ok(format!("((_) {cap})*"))
            }
            MetaKind::Single(ref name) => {
                let cap = self.next_capture();
                match self
                    .meta_captures
                    .iter_mut()
                    .find(|(k, _)| matches!(k, MetaKind::Single(n) if n == name))
                {
                    Some((_, caps)) => caps.push(cap.clone()),
                    None => self
                        .meta_captures
                        .push((MetaKind::Single(name.clone()), vec![cap.clone()])),
                }
                Ok(format!("(_) {cap}"))
            }
        }
    }

    /// A leaf token: metavariable wildcard, or concrete text pinned by `#eq?`.
    fn emit_token(&mut self, node: Node<'_>, text: &str) -> Result<String> {
        if self.placeholder_occurrence(text).is_some() {
            return self.emit_placeholder(text);
        }
        if text.contains(PLACEHOLDER) {
            // Show the original `$NAME` / `$$$NAME`, not the `lens_meta_<i>` /
            // `$0`-style placeholder index the emitter uses internally.
            let mut shown = text.to_string();
            for (i, kind) in self.metavars.iter().enumerate() {
                let ph = format!("{PLACEHOLDER}{i}");
                let name = match kind {
                    MetaKind::Single(n) => format!("${n}"),
                    MetaKind::Variadic(n) if n.is_empty() => "$$$".to_string(),
                    MetaKind::Variadic(n) => format!("$$${n}"),
                };
                shown = shown.replace(&ph, &name);
            }
            bail!("a $METAVAR must stand alone as a whole token, not inside `{shown}`");
        }
        if text.is_empty() {
            return Ok(format!("({})", node.kind()));
        }
        let cap = self.next_capture();
        self.has_concrete_token = true;
        self.predicates
            .push(format!("(#eq? {cap} \"{}\")", escape(text)));
        Ok(format!("({}) {cap}", node.kind()))
    }
}

/// Escape a token for a double-quoted tree-sitter query string.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::structural::grep_ast_filtered;
    use crate::discovery::tags_adapter::any_spec_for_language;
    use crate::tools::AstMatch;
    use std::fs;
    use tempfile::tempdir;

    fn compile(pattern: &str, lang: &str) -> Result<String> {
        compile_pattern(pattern, &any_spec_for_language(lang).unwrap())
    }

    fn matches(dir: &std::path::Path, pattern: &str, lang: &str) -> Vec<AstMatch> {
        let q = compile(pattern, lang).unwrap();
        grep_ast_filtered(dir, &q, Some(lang), 100, Some(MATCH_CAPTURE)).unwrap()
    }

    /// The T4 oracle row `$X.unwrap()`: the emitted query is deterministic and
    /// carries the wildcard receiver, the pinned method name, and `@match`.
    #[test]
    fn rust_unwrap_emits_expected_query() {
        let q = compile("$X.unwrap()", "rust").unwrap();
        assert_eq!(
            q,
            "((call_expression function: (field_expression value: (_) @c0 \
             field: (field_identifier) @c1) arguments: (arguments)) \
             @match (#eq? @c1 \"unwrap\"))"
        );
    }

    /// Predicate 3: `$X.unwrap()` matches real `.unwrap()` call sites — not
    /// comment mentions, not string literals, not other methods.
    #[test]
    fn rust_unwrap_matches_calls_not_comments() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "// v.unwrap() mentioned in a comment\n\
             fn f(v: Option<i32>, r: Result<i32, i32>) {\n\
                 let _ = v.unwrap();\n\
                 let _ = r.expect(\"other method\");\n\
                 let s = \"w.unwrap() in a string\";\n\
                 let _ = s.len();\n\
             }\n",
        )
        .unwrap();
        let hits = matches(dir.path(), "$X.unwrap()", "rust");
        assert_eq!(hits.len(), 1, "exactly the one real call site: {hits:?}");
        assert_eq!(hits[0].line, 3, "the `.unwrap()` call, not the comment");
    }

    /// The T4 oracle row `print($X)` (python).
    #[test]
    fn python_print_matches_only_print_calls() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.py"),
            "# print(x) in a comment\ndef f(x):\n    print(x)\n    log(x)\n    pprint(x)\n",
        )
        .unwrap();
        let hits = matches(dir.path(), "print($X)", "python");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].line, 3);
    }

    /// The T4 oracle row `$A.map($F)` (typescript).
    #[test]
    fn typescript_map_matches_only_map_calls() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.ts"),
            "// xs.map(f) in a comment\nconst ys = xs.map(f);\nconst zs = xs.filter(f);\n",
        )
        .unwrap();
        let hits = matches(dir.path(), "$A.map($F)", "typescript");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].line, 2);
    }

    /// A repeated metavariable compiles to an `#eq?` between its captures:
    /// `$X == $X` matches `a == a` but not `a == b` (nor `a != a`).
    #[test]
    fn repeated_metavar_requires_equal_text() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn f(a: i32, b: i32) -> bool {\n\
                 let x = a == a;\n\
                 let y = a == b;\n\
                 let z = a != a;\n\
                 x && y && z\n\
             }\n",
        )
        .unwrap();
        let q = compile("$X == $X", "rust").unwrap();
        assert!(q.contains("(#eq? @c0 @c1)"), "capture-to-capture eq: {q}");
        let hits = grep_ast_filtered(dir.path(), &q, Some("rust"), 100, Some(MATCH_CAPTURE))
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].line, 2, "`a == a` only");
    }

    /// A pattern that fails to parse (ERROR nodes even inside the wrapper)
    /// errors clearly instead of emitting a garbage query.
    #[test]
    fn syntax_error_pattern_is_a_clear_error() {
        let err = compile("fn (", "rust").unwrap_err().to_string();
        assert!(err.contains("does not parse as rust"), "{err}");
    }

    #[test]
    fn empty_pattern_is_a_clear_error() {
        let err = compile("   ", "rust").unwrap_err().to_string();
        assert!(err.contains("pattern is empty"), "{err}");
    }

    #[test]
    fn multiple_roots_is_a_clear_error() {
        let err = compile("a()\nb()", "python").unwrap_err().to_string();
        assert!(err.contains("single expression or statement"), "{err}");
    }

    /// A metavariable glued to other identifier characters cannot survive the
    /// lexer as its own token; that must be an error, not a silent mismatch.
    /// The message names the original `$X`, not a `$0` placeholder index.
    #[test]
    fn embedded_metavar_is_a_clear_error() {
        let err = compile("foo_$X()", "python").unwrap_err().to_string();
        assert!(err.contains("whole token"), "{err}");
        assert!(
            err.contains("$X"),
            "must name the original metavar, not a placeholder index: {err}"
        );
        assert!(
            !err.contains("$0") && !err.contains("lens_meta_"),
            "must not leak internal placeholder spelling: {err}"
        );
    }

    /// Post-wrapper parse failure diagnoses the likely shape and gives an example.
    #[test]
    fn unparseable_prose_is_a_clear_error() {
        let err = compile(
            "please find every function that returns a result type somehow",
            "rust",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not parse as rust"), "{err}");
        assert!(
            err.contains("non-code prose") || err.contains("unrecognized fragment"),
            "must diagnose shape: {err}"
        );
        assert!(
            err.contains("fn $NAME") || err.contains("Try a code-shaped"),
            "must include a corrected example: {err}"
        );
    }

    /// T4: a bare metavariable pins nothing concrete and would match nearly
    /// every node in a file, so it must be rejected with a clear message.
    #[test]
    fn bare_metavar_is_a_clear_error() {
        let err = compile("$X", "rust").unwrap_err().to_string();
        assert!(
            err.contains("concrete token"),
            "must name the concrete-token rule: {err}"
        );
    }

    /// `$$$` alone is still metavariables-only and must fail the concrete-token
    /// guard (not silently match every node list).
    #[test]
    fn bare_variadic_is_a_clear_error() {
        let err = compile("$$$", "rust").unwrap_err().to_string();
        assert!(
            err.contains("concrete token"),
            "must name the concrete-token rule: {err}"
        );
    }

    /// T4 non-regression: `$X.unwrap()` has a concrete token (`unwrap`)
    /// alongside its metavariable receiver, so it must still compile.
    #[test]
    fn metavar_with_concrete_token_still_compiles() {
        compile("$X.unwrap()", "rust").unwrap();
    }

    /// `f($$$)` emits a quantified arguments hole and matches any arity,
    /// including zero.
    #[test]
    fn rust_variadic_args_match_any_arity() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn f() {\n\
                 g();\n\
                 g(1);\n\
                 g(1, 2);\n\
                 h(1);\n\
             }\n\
             fn g() {}\n\
             fn g(_: i32) {}\n\
             fn g(_: i32, _: i32) {}\n\
             fn h(_: i32) {}\n",
        )
        .unwrap();
        let q = compile("g($$$)", "rust").unwrap();
        assert!(
            q.contains("(_)") && q.contains('*'),
            "variadic must quantify: {q}"
        );
        let hits = matches(dir.path(), "g($$$)", "rust");
        assert_eq!(hits.len(), 3, "g() / g(1) / g(1,2), not h: {hits:?}");
    }

    /// Repeated `$$$NAME` does NOT get capture-to-capture `#eq?` (unlike `$NAME`).
    /// Concrete tokens may still pin with `#eq? "…"`.
    #[test]
    fn repeated_variadic_name_does_not_eq() {
        let q = compile("f($$$ARGS, $$$ARGS)", "python").unwrap();
        // Capture-to-capture form is `(#eq? @cN @cM)`; string pins are fine.
        let has_cap_eq = q.split("(#eq?").any(|chunk| {
            let t = chunk.trim_start();
            t.starts_with('@') && t.contains("@c") && !t.contains('"')
        });
        assert!(
            !has_cap_eq,
            "variadic repeats must not constrain equality: {q}"
        );
        assert!(
            q.matches("((_)").count() >= 2,
            "two independent variadic holes: {q}"
        );
    }

    /// JS/TS: `$$$` must not compile to a literal-`$$$` `#eq?` pin.
    #[test]
    fn js_variadic_is_not_literal_dollar_pin() {
        let q = compile("x.append($$$)", "javascript").unwrap();
        assert!(!q.contains("$$$"), "must not pin literal $$$: {q}");
        assert!(q.contains('*'), "must emit a quantified wildcard: {q}");
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.js"),
            "x.append(1, 2, 3);\nx.append();\ny.append(1);\n",
        )
        .unwrap();
        let hits = matches(dir.path(), "x.append($$$)", "javascript");
        assert_eq!(hits.len(), 2, "{hits:?}");
    }

    /// Mined GA-VARIADIC / GA-FRAGMENT acceptance set: each must compile for rust.
    #[test]
    fn mined_acceptance_patterns_still_compile() {
        let patterns = [
            "fn $NAME($$$) -> Result<$$$> $BODY",
            "#[tool]\nasync fn $NAME($$$ARGS) -> $RET { $$$BODY }",
            "#[test] fn $NAME",
            "fn $NAME(stream: $T) $BODY",
            "fn route_inner($TOOL: &str, $INPUT: &Value, $CTX: &RouteCtx) -> Decision",
            "Some(x) => x",
            "fn foo(a: i32)",
            "impl Forge { $$$ }",
        ];
        for p in patterns {
            compile(p, "rust").unwrap_or_else(|e| panic!("compile {p:?}: {e}"));
        }
    }

    /// Fragment: bare fn signature matches a real item with a non-empty body.
    #[test]
    fn bare_fn_signature_matches_item_with_body() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn foo(a: i32) {\n    let _ = a + 1;\n}\nfn foo(a: i64) {}\n",
        )
        .unwrap();
        let hits = matches(dir.path(), "fn foo(a: i32)", "rust");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].text.contains("foo"), "{hits:?}");
    }

    /// Fragment: match arm pattern matches a real arm.
    #[test]
    fn match_arm_fragment_still_compiles_and_matches() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn f(o: Option<i32>) -> i32 {\n\
                 match o {\n\
                     Some(x) => x,\n\
                     None => 0,\n\
                 }\n\
             }\n",
        )
        .unwrap();
        let hits = matches(dir.path(), "Some(x) => x", "rust");
        assert_eq!(hits.len(), 1, "{hits:?}");
    }

    /// `fn $NAME($$$) -> Result<$$$> $BODY` matches a real Result-returning fn.
    #[test]
    fn result_returning_fn_pattern_matches() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn ok() -> Result<i32, ()> { Ok(1) }\n\
             fn bad() -> Option<i32> { None }\n\
             fn also(a: i32) -> Result<(), String> { Err(a.to_string()) }\n",
        )
        .unwrap();
        let hits = matches(dir.path(), "fn $NAME($$$) -> Result<$$$> $BODY", "rust");
        assert_eq!(hits.len(), 2, "{hits:?}");
    }
}
