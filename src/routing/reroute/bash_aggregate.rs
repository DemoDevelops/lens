//! bash_aggregate: Bash(aggregate) → lens_run classifier.
//!
//! Rail 1c: a Bash command that counts, sorts, or reshapes data (`wc -l`,
//! `sort | uniq`, `uniq -c`, a pipe into `jq`, `grep -c`, `awk '{...}'`) is a
//! data transform that belongs inside `lens_run`'s darkroom, where only the
//! printed answer returns to context. Unlike the deny-shaped rails, this is a
//! `Decision::Context` nudge only — never a `Deny` — so a matching file-write
//! shape (`cat >`, `tee`, a `>`/`>>` redirect, a heredoc) always disqualifies
//! the match even when an aggregate signal also fires: a state-changing write
//! must never be blocked.

use std::sync::OnceLock;

use regex::Regex;

/// Aggregate/reshape signal: `wc -l`, `sort` piped into `uniq`, `uniq -c`, a
/// pipe into `jq`, `grep -c`, `awk '{`, or a pipe into `wc`.
fn aggregate_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"wc -l|sort\s*\|\s*uniq|uniq -c|\|\s*jq|grep -c|awk '\{|\|\s*wc\b")
            .expect("aggregate regex")
    })
}

/// File-write signal that disqualifies a match even if aggregate-shaped:
/// `cat >`, `tee `, a `>`/`>>` redirect, or a heredoc `<<`.
fn file_write_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"cat >|tee |>|<<").expect("file write regex"))
}

/// True iff `cmd` looks like it aggregates/reshapes data AND is not itself a
/// file write. A file-write signal always wins, even over a matching
/// aggregate signal (e.g. `wc -l < f > out.txt` is a write, not an aggregate
/// worth nudging).
pub fn is_data_aggregate(cmd: &str) -> bool {
    aggregate_re().is_match(cmd) && !file_write_re().is_match(cmd)
}

/// Context nudge (never a deny — extends the [`super::super::BASH_NUDGE`]
/// family) pointing an aggregate/reshape Bash pipeline at `lens_run`: the
/// shell pipeline runs inside the darkroom, so its bulk output never lands in
/// context — only what's printed comes back.
pub fn reason() -> String {
    "<context_guidance>\n  <tip>\n    This pipeline counts, sorts, or reshapes data — run it inside lens_run(language=\"shell\", code=\"...\") instead of Bash: the shell pipeline executes in the darkroom and only what you print comes back, so the raw rows never land in context. If lens_run isn't loaded yet, load it first: ToolSearch(query: \"select:lens_run,lens_recall\").\n  </tip>\n</context_guidance>".to_string()
}

/// Deny reason for a Bash `cmd` [`is_data_aggregate`] identified as a
/// counting/sorting/reshaping pipeline: names the exact `lens_run` call with
/// `cmd` embedded as its `code` arg, includes the `ToolSearch` bootstrap line
/// in case the lens tools aren't loaded yet, and promises the one-shot: a
/// verbatim retry of the same command passes.
///
/// ## Precision note
///
/// `is_data_aggregate` is a lexical regex match over the raw command text,
/// not a semantic one, so it is **not clearly safe to hard-deny by default**.
/// Two failure modes:
///
/// 1. **Session-state coupling.** Unlike a Grep/Read lookup, a Bash pipeline
///    can depend on this shell session's live state (an exported env var
///    from an earlier command, the current `cd`'d directory) and can feed a
///    later Bash step (`VAR=$(git status | wc -l)`, `if [ $(uniq -c ...) ]`).
///    `lens_run`'s darkroom is a fresh subprocess with none of that state, so
///    denying can silently break a workflow a nudge would only advise
///    against.
/// 2. **Substring false positives.** The regexes match anywhere in the raw
///    text (`grep -c`, `| jq`, …), so a matching substring inside a quoted
///    string or a longer non-aggregate command (e.g. an `echo` that merely
///    mentions `grep -c`) still classifies as an aggregate.
///
/// Recommendation: T3 should gate the bagg deny conservatively — leave the
/// existing nudge ([`reason`]) as the default arm, and not flip
/// `LENS_BASH_AGG_NUDGE` off in favor of the deny without further
/// false-positive measurement (T4).
pub fn deny_reason(cmd: &str) -> String {
    format!(
        "This Bash pipeline counts, sorts, or reshapes data (\"{cmd}\") — run it inside the darkroom instead of Bash: lens_run(language=\"shell\", code=\"{cmd}\"). Only what you print comes back, so the raw rows never land in context. If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_run,lens_recall\"). This fires once per prompt — the same command will pass if you re-run it verbatim."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_pipelines_are_true() {
        assert!(is_data_aggregate("find . -name '*.rs' | wc -l"));
        assert!(is_data_aggregate("git status | wc -l"));
        assert!(is_data_aggregate("sort | uniq -c"));
        assert!(is_data_aggregate("history | grep -c foo"));
        assert!(is_data_aggregate("cat data.json | jq '.items[]'"));
        assert!(is_data_aggregate("awk '{print $1}' access.log"));
    }

    #[test]
    fn file_writes_are_false_even_when_aggregate_shaped() {
        assert!(!is_data_aggregate("cat > f.txt"));
        assert!(!is_data_aggregate("echo hi"));
        assert!(!is_data_aggregate("echo x > out.txt"));
        // Aggregate signal (`wc -l`) present, but the write redirect wins.
        assert!(!is_data_aggregate("wc -l < f > out.txt"));
        assert!(!is_data_aggregate("sort | uniq -c | tee counts.txt"));
        assert!(!is_data_aggregate("cat <<'EOF' | wc -l\nhi\nEOF"));
    }

    #[test]
    fn reason_names_lens_run() {
        assert!(reason().contains("lens_run"));
    }

    #[test]
    fn deny_reason_names_lens_run_and_promises_retry() {
        let r = deny_reason("git status | wc -l");
        assert!(r.contains("lens_run"));
        assert!(r.contains("will pass if you re-run it verbatim"));
    }
}
