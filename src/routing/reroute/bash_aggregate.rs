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
}
