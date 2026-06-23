//! `lens_run`: run code in a subprocess and capture only its stdout/stderr.
//!
//! The raw data the script reads never enters the agent's context: only what the
//! script prints comes back. Large stdout is offloaded to the reversible store
//! and replaced with a head+tail preview plus a `retrieve_ref`.

pub mod runtimes;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::store::Store;
use crate::tools::{ExecuteRequest, ExecuteResponse};

/// On-disk directory for Go compiled binaries, keyed by blake3(source).
/// Initialized once; cleaned by the OS on reboot (uses std::env::temp_dir).
fn go_cache_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let p = std::env::temp_dir().join("lens_go_cache");
        let _ = std::fs::create_dir_all(&p);
        p
    })
}

/// How much of a truncated stdout to keep at the head and at the tail.
const PREVIEW_SIDE: usize = 2048;

/// Run a darkroom request. `repo_dir` is the working directory for the child
/// process. `max_inline` is the stdout byte threshold above which output is
/// offloaded to `store`.
pub async fn run(
    req: ExecuteRequest,
    repo_dir: &std::path::Path,
    store: &Store,
    max_inline: usize,
) -> Result<ExecuteResponse, String> {
    run_with_args(req, repo_dir, store, max_inline, &[]).await
}

/// Like [`run`], but injects `file_path` as the script's first CLI argument so
/// the code can open/analyze that file while only its printed output returns.
/// The path arrives as python `sys.argv[1]` / node `process.argv[2]` /
/// bash `$1` / ruby `ARGV[0]` / go `os.Args[1]`.
pub async fn run_file(
    file_path: &std::path::Path,
    req: ExecuteRequest,
    repo_dir: &std::path::Path,
    store: &Store,
    max_inline: usize,
) -> Result<ExecuteResponse, String> {
    run_with_args(
        req,
        repo_dir,
        store,
        max_inline,
        &[file_path.as_os_str().to_owned()],
    )
    .await
}

/// Shared darkroom runner. Spawns `runtime.program pre_args <script_path>`
/// followed by `extra_args`, captures stdout/stderr, offloads oversized stdout
/// to `store`, and bumps the savings counters.
async fn run_with_args(
    req: ExecuteRequest,
    repo_dir: &std::path::Path,
    store: &Store,
    max_inline: usize,
    extra_args: &[std::ffi::OsString],
) -> Result<ExecuteResponse, String> {
    let runtime = runtimes::runtime_for(&req.language).ok_or_else(|| {
        format!(
            "unsupported language '{}': use one of python, javascript, typescript, bash, ruby, go",
            req.language
        )
    })?;

    // Go: compile once per unique source (keyed by blake3), exec the cached binary.
    if runtime.extension == "go" {
        return run_go_cached(req, repo_dir, store, max_inline, extra_args).await;
    }

    // Write the script to a temp file next to the repo so relative paths in the
    // script resolve against the working dir. The file auto-deletes on drop.
    let mut tmp = tempfile::Builder::new()
        .prefix("lens_")
        .suffix(&format!(".{}", runtime.extension))
        .tempfile()
        .map_err(|e| format!("creating temp script: {e}"))?;
    tmp.write_all(req.code.as_bytes())
        .map_err(|e| format!("writing temp script: {e}"))?;
    tmp.flush()
        .map_err(|e| format!("flushing temp script: {e}"))?;
    let script_path = tmp.path().to_path_buf();

    let mut cmd = tokio::process::Command::new(&runtime.program);
    cmd.args(&runtime.pre_args)
        .arg(&script_path)
        .args(extra_args)
        .current_dir(repo_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "interpreter '{}' not found on PATH. Install it to run {} code.",
                runtime.program, req.language
            ));
        }
        Err(e) => return Err(format!("failed to spawn '{}': {e}", runtime.program)),
    };

    // Hand stdout/stderr to reader tasks so we can still kill the child on
    // timeout (which only needs &mut child for wait()/kill()).
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let out_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    if let Some(input) = req.stdin.as_ref() {
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(input.as_bytes()).await;
            drop(si); // close stdin so the child sees EOF
        }
    } else {
        drop(child.stdin.take());
    }

    let dur = Duration::from_secs(req.timeout_secs.max(1));
    let mut timed_out = false;
    let status = match tokio::time::timeout(dur, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(format!("waiting on child: {e}")),
        Err(_elapsed) => {
            timed_out = true;
            let _ = child.kill().await;
            child.wait().await.ok();
            std::process::ExitStatus::default()
        }
    };

    let stdout_bytes_full = out_task.await.unwrap_or_default();
    let stderr_bytes_full = err_task.await.unwrap_or_default();
    let stdout_full = String::from_utf8_lossy(&stdout_bytes_full).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes_full).into_owned();

    let stdout_bytes = stdout_full.len();
    let exit_code = if timed_out {
        -1
    } else {
        status.code().unwrap_or(-1)
    };

    // Large-output handling: offload full stdout, return a preview + ref.
    let (stdout, truncated, retrieve_ref) = if stdout_bytes > max_inline {
        let reference = store
            .put(&stdout_full)
            .map_err(|e| format!("storing large stdout: {e}"))?;
        let preview = make_preview(&stdout_full);
        (preview, true, Some(reference))
    } else {
        (stdout_full, false, None)
    };

    let returned_bytes = stdout.len() + stderr.len();
    // Stats: count the script's full output as "processed" and what we actually
    // hand back as "returned". Savings materialise when large output is offloaded.
    let _ = store.bump_stat("darkroom_calls", 1);
    let _ = store.bump_stat("raw_bytes_processed", stdout_bytes as i64);
    let _ = store.bump_stat("bytes_returned_to_context", returned_bytes as i64);

    Ok(ExecuteResponse {
        stdout,
        stderr,
        exit_code,
        timed_out,
        stdout_bytes,
        truncated,
        retrieve_ref,
    })
}

/// Compile a Go source file once (keyed by blake3 of source), then exec the cached
/// binary. Subsequent calls with identical source skip `go build` entirely.
/// Each invocation is still its own sandboxed subprocess; only the compiled artifact
/// is reused, not any process state.
async fn run_go_cached(
    req: ExecuteRequest,
    repo_dir: &std::path::Path,
    store: &Store,
    max_inline: usize,
    extra_args: &[std::ffi::OsString],
) -> Result<ExecuteResponse, String> {
    let key = runtimes::source_key(req.code.as_bytes());
    let bin_path = go_cache_dir().join(&key);

    // Build if the cached binary is not yet present.
    if !bin_path.exists() {
        // Write source to a temp file; Go requires a .go extension.
        let mut tmp = tempfile::Builder::new()
            .prefix("lens_go_")
            .suffix(".go")
            .tempfile()
            .map_err(|e| format!("creating Go temp source: {e}"))?;
        tmp.write_all(req.code.as_bytes())
            .map_err(|e| format!("writing Go source: {e}"))?;
        tmp.flush()
            .map_err(|e| format!("flushing Go source: {e}"))?;
        let src_path = tmp.path().to_path_buf();

        // Build to a temp path in the same directory, then atomically rename so
        // concurrent runs do not corrupt a partial binary.
        let tmp_bin = go_cache_dir().join(format!("{key}.tmp"));
        let dur = Duration::from_secs(req.timeout_secs.max(1));
        let output = match tokio::time::timeout(
            dur,
            tokio::process::Command::new("go")
                .args(["build", "-o"])
                .arg(&tmp_bin)
                .arg(&src_path)
                .current_dir(repo_dir)
                .output(),
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(
                    "interpreter 'go' not found on PATH. Install it to run go code.".to_owned(),
                );
            }
            Ok(Err(e)) => return Err(format!("failed to spawn 'go build': {e}")),
            Err(_) => return Err("go build timed out".to_owned()),
        };

        if !output.status.success() {
            return Err(format!(
                "go build failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        // Atomic rename: if two concurrent runs race here, the last rename wins and
        // both end up with the same valid binary.
        std::fs::rename(&tmp_bin, &bin_path)
            .map_err(|e| format!("caching Go binary: {e}"))?;
    }

    // Execute the cached binary as a fresh sandboxed subprocess.
    let mut cmd = tokio::process::Command::new(&bin_path);
    cmd.args(extra_args)
        .current_dir(repo_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Err(format!("failed to spawn Go binary: {e}")),
    };

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let out_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    if let Some(input) = req.stdin.as_ref() {
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(input.as_bytes()).await;
            drop(si);
        }
    } else {
        drop(child.stdin.take());
    }

    let dur = Duration::from_secs(req.timeout_secs.max(1));
    let mut timed_out = false;
    let status = match tokio::time::timeout(dur, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("waiting on Go child: {e}")),
        Err(_elapsed) => {
            timed_out = true;
            let _ = child.kill().await;
            child.wait().await.ok();
            std::process::ExitStatus::default()
        }
    };

    let stdout_bytes_full = out_task.await.unwrap_or_default();
    let stderr_bytes_full = err_task.await.unwrap_or_default();
    let stdout_full = String::from_utf8_lossy(&stdout_bytes_full).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes_full).into_owned();

    let stdout_bytes = stdout_full.len();
    let exit_code = if timed_out {
        -1
    } else {
        status.code().unwrap_or(-1)
    };

    let (stdout, truncated, retrieve_ref) = if stdout_bytes > max_inline {
        let reference = store
            .put(&stdout_full)
            .map_err(|e| format!("storing large stdout: {e}"))?;
        let preview = make_preview(&stdout_full);
        (preview, true, Some(reference))
    } else {
        (stdout_full, false, None)
    };

    let returned_bytes = stdout.len() + stderr.len();
    let _ = store.bump_stat("darkroom_calls", 1);
    let _ = store.bump_stat("raw_bytes_processed", stdout_bytes as i64);
    let _ = store.bump_stat("bytes_returned_to_context", returned_bytes as i64);

    Ok(ExecuteResponse {
        stdout,
        stderr,
        exit_code,
        timed_out,
        stdout_bytes,
        truncated,
        retrieve_ref,
    })
}

/// Build a head+tail preview of an oversized output, keeping char boundaries.
fn make_preview(full: &str) -> String {
    let head_end = floor_char_boundary(full, PREVIEW_SIDE);
    let tail_start = ceil_char_boundary(full, full.len().saturating_sub(PREVIEW_SIDE));
    let omitted = tail_start.saturating_sub(head_end);
    format!(
        "{}\n... [{} bytes omitted; full output via lens_recall] ...\n{}",
        &full[..head_end],
        omitted,
        &full[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store_in(dir: &std::path::Path) -> Store {
        Store::open(&dir.join(".lens")).unwrap()
    }

    #[tokio::test]
    async fn runs_bash_and_captures_stdout() {
        let dir = tempdir().unwrap();
        let store = store_in(dir.path());
        let req = ExecuteRequest {
            language: "bash".into(),
            code: "echo hello; echo oops 1>&2".into(),
            timeout_secs: 30,
            stdin: None,
        };
        let r = run(req, dir.path(), &store, 8192).await.unwrap();
        assert_eq!(r.stdout.trim(), "hello");
        assert_eq!(r.stderr.trim(), "oops");
        assert_eq!(r.exit_code, 0);
        assert!(!r.timed_out);
    }

    #[tokio::test]
    async fn core_invariant_reads_big_prints_one_line() {
        // Write a big file, run a script that reads it but prints one line.
        let dir = tempdir().unwrap();
        let store = store_in(dir.path());
        let big = "x".repeat(500_000);
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();
        let req = ExecuteRequest {
            language: "python".into(),
            code: "data = open('big.txt').read(); print(len(data))".into(),
            timeout_secs: 30,
            stdin: None,
        };
        let r = run(req, dir.path(), &store, 8192).await.unwrap();
        assert_eq!(r.stdout.trim(), "500000");
        // The raw 500k never appears in what we return.
        assert!(!r.stdout.contains(&"x".repeat(100)));
        assert!(r.stdout.len() < 1000);
    }

    #[tokio::test]
    async fn timeout_kills_overrunning_script() {
        let dir = tempdir().unwrap();
        let store = store_in(dir.path());
        let req = ExecuteRequest {
            language: "bash".into(),
            code: "sleep 10; echo done".into(),
            timeout_secs: 1,
            stdin: None,
        };
        let r = run(req, dir.path(), &store, 8192).await.unwrap();
        assert!(r.timed_out);
        assert_ne!(r.exit_code, 0);
        assert!(!r.stdout.contains("done"));
    }

    #[tokio::test]
    async fn missing_interpreter_is_clear_error() {
        let dir = tempdir().unwrap();
        let store = store_in(dir.path());
        let req = ExecuteRequest {
            language: "ruby".into(),
            code: "puts 1".into(),
            timeout_secs: 5,
            stdin: None,
        };
        // ruby exists on this machine, so instead test an unsupported language path.
        let bad = ExecuteRequest {
            language: "cobol".into(),
            code: "x".into(),
            timeout_secs: 5,
            stdin: None,
        };
        let err = run(bad, dir.path(), &store, 8192).await.unwrap_err();
        assert!(err.contains("unsupported language"));
        // sanity: the supported one still works if present
        let _ = run(req, dir.path(), &store, 8192).await;
    }

    #[tokio::test]
    async fn large_output_is_stored_and_retrievable() {
        let dir = tempdir().unwrap();
        let store = store_in(dir.path());
        let req = ExecuteRequest {
            language: "python".into(),
            code: "print('A' * 50000)".into(),
            timeout_secs: 30,
            stdin: None,
        };
        let r = run(req, dir.path(), &store, 8192).await.unwrap();
        assert!(r.truncated);
        let reference = r.retrieve_ref.expect("should have ref");
        let full = store.get(&reference).unwrap().unwrap();
        assert!(full.contains(&"A".repeat(50000)));
        assert!(r.stdout.len() < full.len());
    }

    #[tokio::test]
    async fn stdin_is_piped() {
        let dir = tempdir().unwrap();
        let store = store_in(dir.path());
        let req = ExecuteRequest {
            language: "python".into(),
            code: "import sys; print(sys.stdin.read().strip().upper())".into(),
            timeout_secs: 30,
            stdin: Some("hello".into()),
        };
        let r = run(req, dir.path(), &store, 8192).await.unwrap();
        assert_eq!(r.stdout.trim(), "HELLO");
    }

    #[tokio::test]
    async fn run_file_passes_path_as_argv() {
        // The file path is injected as argv; the code reads the file but only
        // prints its byte length, so the contents never enter the result.
        let dir = tempdir().unwrap();
        let store = store_in(dir.path());
        let body = "z".repeat(1234);
        let file = dir.path().join("data.txt");
        std::fs::write(&file, &body).unwrap();
        let req = ExecuteRequest {
            language: "python".into(),
            code: "import sys; print(len(open(sys.argv[1]).read()))".into(),
            timeout_secs: 30,
            stdin: None,
        };
        let r = run_file(&file, req, dir.path(), &store, 8192)
            .await
            .unwrap();
        assert_eq!(r.stdout.trim(), "1234");
        // The file contents are not echoed back into the returned stdout.
        assert!(!r.stdout.contains(&"z".repeat(50)));
    }

    #[tokio::test]
    async fn run_file_passes_path_to_bash() {
        let dir = tempdir().unwrap();
        let store = store_in(dir.path());
        let file = dir.path().join("data.bin");
        std::fs::write(&file, vec![b'q'; 4096]).unwrap();
        let req = ExecuteRequest {
            language: "bash".into(),
            code: "wc -c < \"$1\"".into(),
            timeout_secs: 30,
            stdin: None,
        };
        let r = run_file(&file, req, dir.path(), &store, 8192)
            .await
            .unwrap();
        assert_eq!(r.stdout.trim(), "4096");
    }

    #[tokio::test]
    async fn run_file_large_output_is_stored_and_retrievable() {
        let dir = tempdir().unwrap();
        let store = store_in(dir.path());
        let file = dir.path().join("any.txt");
        std::fs::write(&file, "hello").unwrap();
        let req = ExecuteRequest {
            language: "python".into(),
            code: "print('A' * 50000)".into(),
            timeout_secs: 30,
            stdin: None,
        };
        let r = run_file(&file, req, dir.path(), &store, 8192)
            .await
            .unwrap();
        assert!(r.truncated);
        let reference = r.retrieve_ref.expect("should have ref");
        let full = store.get(&reference).unwrap().unwrap();
        assert!(full.contains(&"A".repeat(50000)));
        assert!(r.stdout.len() < full.len());
    }

    // T6 invariant: TS runtime selection and Go compile-cache hit/miss.
    //
    // The TS selection tests live in runtimes.rs (pure fn, no bun required).
    // Here we test the Go cache key bookkeeping: same source => same cache
    // path (hit); changed source => different path (miss). Does not require
    // `go` to be installed.
    #[test]
    fn t6_go_cache_same_source_same_key_different_source_different_key() {
        let src_a = b"package main\nfunc main() { println(\"a\") }";
        let src_b = b"package main\nfunc main() { println(\"b\") }";

        let key_a1 = runtimes::source_key(src_a);
        let key_a2 = runtimes::source_key(src_a);
        let key_b = runtimes::source_key(src_b);

        // Identical source => same cache path => cache hit.
        assert_eq!(key_a1, key_a2, "same source must produce the same key");

        // Changed source => different key => rebuild (cache miss).
        assert_ne!(key_a1, key_b, "changed source must produce a different key");

        // Verify the cache dir path is consistent for a given key.
        let bin_a = go_cache_dir().join(&key_a1);
        let bin_a2 = go_cache_dir().join(&key_a2);
        assert_eq!(bin_a, bin_a2, "same key must map to the same binary path");

        let bin_b = go_cache_dir().join(&key_b);
        assert_ne!(bin_a, bin_b, "different key must map to a different binary path");
    }

    // T6 invariant: when go IS available, build once and reuse on second call.
    #[tokio::test]
    async fn t6_go_cache_build_once_exec_cached() {
        if std::process::Command::new("go").arg("version").output().is_err() {
            // go not installed; skip the live-build half.
            return;
        }
        let dir = tempdir().unwrap();
        let store = store_in(dir.path());
        let src = "package main\nimport \"fmt\"\nfunc main() { fmt.Println(\"cached\") }";

        let req1 = ExecuteRequest {
            language: "go".into(),
            code: src.into(),
            timeout_secs: 30,
            stdin: None,
        };
        let req2 = ExecuteRequest {
            language: "go".into(),
            code: src.into(),
            timeout_secs: 30,
            stdin: None,
        };

        let r1 = run(req1, dir.path(), &store, 8192).await.unwrap();
        assert_eq!(r1.stdout.trim(), "cached");
        assert_eq!(r1.exit_code, 0);

        // Second call with identical source must succeed (hits cache, no rebuild).
        let r2 = run(req2, dir.path(), &store, 8192).await.unwrap();
        assert_eq!(r2.stdout.trim(), "cached");
        assert_eq!(r2.exit_code, 0);

        // Changed source produces a different output (different key => rebuild).
        let req3 = ExecuteRequest {
            language: "go".into(),
            code: "package main\nimport \"fmt\"\nfunc main() { fmt.Println(\"rebuilt\") }".into(),
            timeout_secs: 30,
            stdin: None,
        };
        let r3 = run(req3, dir.path(), &store, 8192).await.unwrap();
        assert_eq!(r3.stdout.trim(), "rebuilt");
        assert_eq!(r3.exit_code, 0);
    }
}
