//! Maps a language name to the interpreter command used to run a source file.

use std::path::PathBuf;
use std::sync::OnceLock;

/// A runtime: the program to invoke and how it takes the script.
pub struct Runtime {
    /// Executable to invoke (may be a resolved absolute path or a name).
    pub program: String,
    /// Extra args placed before the script path.
    pub pre_args: Vec<&'static str>,
    /// File extension for the temp script (no dot).
    pub extension: &'static str,
}

/// Probe for `bun` on PATH, resolving the full path once and caching it.
/// Returns `Some(path)` if bun is available, `None` otherwise.
pub fn bun_path() -> Option<&'static PathBuf> {
    static BUN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUN.get_or_init(|| which_binary("bun")).as_ref()
}

/// Probe the PATH for a binary by name, returning its full path or None.
/// Used to resolve interpreter paths once at startup.
fn which_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path_var| {
        std::env::split_paths(&path_var).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

/// Choose the TypeScript runtime command based on bun availability.
/// Pure function; `bun_available` is injected so it is testable without bun installed.
pub fn ts_program_for(bun_available: bool) -> (&'static str, Vec<&'static str>) {
    if bun_available {
        ("bun", vec!["run"])
    } else {
        ("npx", vec!["--yes", "tsx"])
    }
}

/// Python bootstrap run via `-c`: pre-imports the `lens` prelude as a global
/// before handing off to the user script with `runpy` (correct file names and
/// line numbers in tracebacks, `__main__` semantics preserved). The 2026-07-21
/// bad-set audit found scripts calling `lens.…` without `import lens` burning a
/// full round on the NameError; with this, `lens` just works and an explicit
/// `import lens` stays a no-op. `sys.argv`/`sys.path` are reshaped to match the
/// plain `python3 script.py` invocation this replaces.
const PY_BOOTSTRAP: &str = concat!(
    "import os, sys, runpy\n",
    "sys.argv = sys.argv[1:]\n",
    "sys.path.insert(0, os.path.dirname(os.path.abspath(sys.argv[0])))\n",
    "try:\n",
    "    import builtins, lens\n",
    "    builtins.lens = lens\n",
    "except Exception:\n",
    "    pass\n",
    "runpy.run_path(sys.argv[0], run_name='__main__')\n",
);

/// Resolve a language string to its runtime, or `None` if unsupported.
pub fn runtime_for(language: &str) -> Option<Runtime> {
    let lang = language.trim().to_ascii_lowercase();
    let rt = match lang.as_str() {
        "python" | "python3" | "py" => Runtime {
            program: "python3".to_owned(),
            pre_args: vec!["-c", PY_BOOTSTRAP],
            extension: "py",
        },
        "javascript" | "js" | "node" => Runtime {
            program: "node".to_owned(),
            pre_args: vec![],
            extension: "js",
        },
        "typescript" | "ts" => {
            let (prog, args) = ts_program_for(bun_path().is_some());
            Runtime {
                program: prog.to_owned(),
                pre_args: args,
                extension: "ts",
            }
        }
        "bash" | "sh" | "shell" => Runtime {
            program: "bash".to_owned(),
            pre_args: vec![],
            extension: "sh",
        },
        "ruby" | "rb" => Runtime {
            program: "ruby".to_owned(),
            pre_args: vec![],
            extension: "rb",
        },
        "go" => Runtime {
            // Handled by the Go compile-cache path in mod.rs; pre_args unused.
            program: "go".to_owned(),
            pre_args: vec!["build"],
            extension: "go",
        },
        _ => return None,
    };
    Some(rt)
}

/// Compute the blake3 hex digest of source bytes; used as a Go compile-cache key.
pub fn source_key(source: &[u8]) -> String {
    blake3::hash(source).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_languages_resolve() {
        for lang in ["python", "PY", "javascript", "ts", "bash", "ruby", "go"] {
            assert!(runtime_for(lang).is_some(), "{lang} should resolve");
        }
    }

    #[test]
    fn unknown_language_is_none() {
        assert!(runtime_for("brainfuck").is_none());
    }

    // TS runtime selection: pure function testable without bun installed.
    #[test]
    fn ts_runtime_selects_bun_when_available() {
        let (prog, args) = ts_program_for(true);
        assert_eq!(prog, "bun");
        assert_eq!(args, vec!["run"]);
    }

    #[test]
    fn ts_runtime_falls_back_to_tsx_when_bun_absent() {
        let (prog, args) = ts_program_for(false);
        assert_eq!(prog, "npx");
        assert_eq!(args, vec!["--yes", "tsx"]);
    }

    // Go cache key: same source => same key; changed source => different key.
    #[test]
    fn go_cache_key_is_deterministic() {
        let src = b"package main\nfunc main() {}";
        let k1 = source_key(src);
        let k2 = source_key(src);
        assert_eq!(k1, k2, "same source must produce identical key");
    }

    #[test]
    fn go_cache_key_differs_for_changed_source() {
        let k1 = source_key(b"package main\nfunc main() { println(1) }");
        let k2 = source_key(b"package main\nfunc main() { println(2) }");
        assert_ne!(k1, k2, "changed source must produce a different key");
    }
}
