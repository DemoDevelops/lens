//! Maps a language name to the interpreter command used to run a source file.

/// A runtime: the program to invoke and how it takes the script.
pub struct Runtime {
    /// Executable to look up on PATH (e.g. `python3`).
    pub program: &'static str,
    /// Extra args placed before the script path.
    pub pre_args: &'static [&'static str],
    /// File extension for the temp script (no dot).
    pub extension: &'static str,
}

/// Resolve a language string to its runtime, or `None` if unsupported.
pub fn runtime_for(language: &str) -> Option<Runtime> {
    let lang = language.trim().to_ascii_lowercase();
    let rt = match lang.as_str() {
        "python" | "python3" | "py" => Runtime {
            program: "python3",
            pre_args: &[],
            extension: "py",
        },
        "javascript" | "js" | "node" => Runtime {
            program: "node",
            pre_args: &[],
            extension: "js",
        },
        "typescript" | "ts" => Runtime {
            // Run TS without a build step via node's stripping-capable loader / ts-node.
            program: "npx",
            pre_args: &["--yes", "tsx"],
            extension: "ts",
        },
        "bash" | "sh" | "shell" => Runtime {
            program: "bash",
            pre_args: &[],
            extension: "sh",
        },
        "ruby" | "rb" => Runtime {
            program: "ruby",
            pre_args: &[],
            extension: "rb",
        },
        "go" => Runtime {
            // `go run` compiles and runs a single file.
            program: "go",
            pre_args: &["run"],
            extension: "go",
        },
        _ => return None,
    };
    Some(rt)
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
}
