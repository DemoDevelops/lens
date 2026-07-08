//! Pure, deterministic credit classification for darkroom savings attribution.
//!
//! Splits ops into ones that stand in for context the agent would otherwise
//! have pulled inline (credited in full) versus ones where the agent handed
//! the darkroom data it never intended to read (credited only a plausible
//! slice). Tool + extension + size only — **no I/O, no model/LLM call**, so
//! the same classifier can run at write time ([`OpHandle::finish`]) and at
//! read time (`stats::aggregate`, replaying history).

/// Source-code / text extensions the agent would plausibly have Read inline;
/// a `lens_run_file`/`lens_skeleton` op on one of these stands in for that Read.
const SRC_EXTS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "cpp", "cc", "h", "hpp", "swift",
    "rb", "php", "kt", "scala", "lua", "sh", "md", "toml", "yaml", "yml",
];

/// Data/log extensions the agent would never have pasted into context whole;
/// darkroom analysis of these is volunteered work, not intercepted context.
const DATA_EXTS: &[&str] = &[
    "log", "jsonl", "ndjson", "json", "csv", "tsv", "parquet", "db", "sqlite", "bin",
];

/// Above this size, even an unrecognized extension is presumed volunteered
/// (no one pastes a multi-megabyte file into a conversation).
const HUGE_BYTES: u64 = 2 * 1024 * 1024;

/// Default plausible-slice credit for volunteered ops, overridable via
/// `LENS_SAVINGS_VOL_FLOOR`.
const DEFAULT_VOL_FLOOR: u64 = 32_768;

/// How an op's raw input should be credited toward darkroom savings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditClass {
    /// Stands in for context the agent would otherwise have pulled inline
    /// (a source-file read, a skeleton); credited in full.
    ContextBound,
    /// Data the agent handed the darkroom to crunch, not to paste into
    /// context; credited only up to the plausible-slice floor.
    Volunteered,
    /// No path in the input (e.g. `lens_run`); this classifier does not
    /// apply — the tool's own handler decides its credit.
    Neutral,
}

impl CreditClass {
    /// The exact string this class is recorded as on `OpRecord.credit_class`.
    pub fn as_str(self) -> &'static str {
        match self {
            CreditClass::ContextBound => "context_bound",
            CreditClass::Volunteered => "volunteered",
            CreditClass::Neutral => "neutral",
        }
    }
}

/// Classify an op from fields already on its record: tool + extension + size.
/// Pure — no I/O, no model call.
pub fn classify(tool: &str, input_summary: &serde_json::Value, raw_bytes_in: u64) -> CreditClass {
    if tool == "lens_skeleton" {
        return CreditClass::ContextBound;
    }
    let Some(path) = input_summary.get("path").and_then(|p| p.as_str()) else {
        return CreditClass::Neutral;
    };
    match extension_of(path) {
        Some(ext) if SRC_EXTS.contains(&ext.as_str()) => CreditClass::ContextBound,
        Some(ext) if DATA_EXTS.contains(&ext.as_str()) => CreditClass::Volunteered,
        Some(_) if raw_bytes_in > HUGE_BYTES => CreditClass::Volunteered,
        Some(_) => CreditClass::ContextBound,
        None if raw_bytes_in > HUGE_BYTES => CreditClass::Volunteered,
        None => CreditClass::Volunteered,
    }
}

/// The lowercase extension (final path component, after the last `.`), or
/// `None` for a no-dot component or a dotfile with no further dot (e.g.
/// `.bashrc`, which counts as no-extension).
fn extension_of(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let dot = name.rfind('.')?;
    if dot == 0 {
        // Dotfile with no other dot, e.g. ".bashrc" -> no extension.
        return None;
    }
    let ext = &name[dot + 1..];
    if ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// The plausible-slice credit for `class` given the true `raw_bytes_in`.
/// `ContextBound`/`Neutral` pass the raw byte count through unchanged;
/// `Volunteered` is floored to [`vol_floor`].
pub fn credited_raw(class: CreditClass, raw_bytes_in: u64) -> u64 {
    credited_raw_with_floor(class, raw_bytes_in, vol_floor())
}

/// Same as [`credited_raw`] with an explicit floor, so tests can exercise the
/// override deterministically without racing the process-global env var.
fn credited_raw_with_floor(class: CreditClass, raw_bytes_in: u64, floor: u64) -> u64 {
    match class {
        CreditClass::ContextBound | CreditClass::Neutral => raw_bytes_in,
        CreditClass::Volunteered => raw_bytes_in.min(floor),
    }
}

/// The plausible-slice credit ceiling for volunteered ops, from
/// `LENS_SAVINGS_VOL_FLOOR` (default 32768 bytes / 32 KB).
pub fn vol_floor() -> u64 {
    std::env::var("LENS_SAVINGS_VOL_FLOOR")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_VOL_FLOOR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_file_run_file_is_context_bound_full_credit() {
        let input = json!({"path": "src/main.rs"});
        let class = classify("lens_run_file", &input, 500_000);
        assert_eq!(class, CreditClass::ContextBound);
        assert_eq!(credited_raw(class, 500_000), 500_000);
    }

    #[test]
    fn log_file_run_file_is_volunteered_floored() {
        let input = json!({"path": "ops.log"});
        let raw = 5 * 1024 * 1024;
        let class = classify("lens_run_file", &input, raw);
        assert_eq!(class, CreditClass::Volunteered);
        assert_eq!(credited_raw(class, raw), DEFAULT_VOL_FLOOR);
    }

    #[test]
    fn jsonl_run_file_is_volunteered() {
        let input = json!({"path": "events.jsonl"});
        let raw = 5 * 1024 * 1024;
        assert_eq!(classify("lens_run_file", &input, raw), CreditClass::Volunteered);
    }

    #[test]
    fn extensionless_path_is_volunteered() {
        let input = json!({"path": "Makefile"});
        assert_eq!(
            classify("lens_run_file", &input, 1_000),
            CreditClass::Volunteered
        );
    }

    #[test]
    fn dotfile_with_no_other_dot_is_volunteered() {
        let input = json!({"path": "/home/user/.bashrc"});
        assert_eq!(
            classify("lens_run_file", &input, 1_000),
            CreditClass::Volunteered
        );
    }

    #[test]
    fn skeleton_is_always_context_bound_regardless_of_path() {
        let input = json!({"path": "data/huge.bin"});
        assert_eq!(
            classify("lens_skeleton", &input, 10 * 1024 * 1024),
            CreditClass::ContextBound
        );
    }

    #[test]
    fn missing_path_is_neutral_full_credit() {
        let input = json!({"language": "python", "code_bytes": 10});
        let class = classify("lens_run", &input, 8_000);
        assert_eq!(class, CreditClass::Neutral);
        assert_eq!(credited_raw(class, 8_000), 8_000);
    }

    #[test]
    fn small_unknown_extension_leans_context_bound() {
        let input = json!({"path": "config.ini"});
        assert_eq!(
            classify("lens_run_file", &input, 1_000),
            CreditClass::ContextBound
        );
    }

    #[test]
    fn huge_non_src_non_data_extension_is_volunteered() {
        let input = json!({"path": "archive.dat"});
        assert_eq!(
            classify("lens_run_file", &input, HUGE_BYTES + 1),
            CreditClass::Volunteered
        );
    }

    #[test]
    fn credited_raw_with_floor_applies_explicit_override() {
        // No env race: exercises the floor-taking helper directly rather than
        // mutating the process-global LENS_SAVINGS_VOL_FLOOR var.
        assert_eq!(
            credited_raw_with_floor(CreditClass::Volunteered, 5 * 1024 * 1024, 8192),
            8192
        );
        assert_eq!(
            credited_raw_with_floor(CreditClass::ContextBound, 5 * 1024 * 1024, 8192),
            5 * 1024 * 1024
        );
    }

    #[test]
    fn vol_floor_reads_env_override() {
        // vol_floor() itself does read the process-global env var; serialize
        // with other env-mutating tests via the shared lock so this can't race.
        let _g = crate::rtk::env_test_lock();
        let prev = std::env::var_os("LENS_SAVINGS_VOL_FLOOR");
        std::env::set_var("LENS_SAVINGS_VOL_FLOOR", "8192");
        assert_eq!(vol_floor(), 8192);
        match prev {
            Some(v) => std::env::set_var("LENS_SAVINGS_VOL_FLOOR", v),
            None => std::env::remove_var("LENS_SAVINGS_VOL_FLOOR"),
        }
    }
}
