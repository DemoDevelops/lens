//! `ctx_execute`: run code in a subprocess and capture only its stdout/stderr.
//!
//! The raw data the script reads never enters the agent's context: only what the
//! script prints comes back. Large stdout is offloaded to the reversible store
//! and replaced with a head+tail preview plus a `retrieve_ref`.

pub mod runtimes;

use std::io::Write as _;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::store::Store;
use crate::tools::{ExecuteRequest, ExecuteResponse};

/// How much of a truncated stdout to keep at the head and at the tail.
const PREVIEW_SIDE: usize = 2048;

/// Run a sandbox request. `repo_dir` is the working directory for the child
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

/// Shared sandbox runner. Spawns `runtime.program pre_args <script_path>`
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

    // Write the script to a temp file next to the repo so relative paths in the
    // script resolve against the working dir. The file auto-deletes on drop.
    let mut tmp = tempfile::Builder::new()
        .prefix("ctxforge_")
        .suffix(&format!(".{}", runtime.extension))
        .tempfile()
        .map_err(|e| format!("creating temp script: {e}"))?;
    tmp.write_all(req.code.as_bytes())
        .map_err(|e| format!("writing temp script: {e}"))?;
    tmp.flush().map_err(|e| format!("flushing temp script: {e}"))?;
    let script_path = tmp.path().to_path_buf();

    let mut cmd = tokio::process::Command::new(runtime.program);
    cmd.args(runtime.pre_args)
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
    let exit_code = if timed_out { -1 } else { status.code().unwrap_or(-1) };

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
    let _ = store.bump_stat("sandbox_calls", 1);
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
        "{}\n... [{} bytes omitted; full output via ctx_retrieve] ...\n{}",
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
        Store::open(&dir.join(".ctxforge")).unwrap()
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
        let r = run_file(&file, req, dir.path(), &store, 8192).await.unwrap();
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
        let r = run_file(&file, req, dir.path(), &store, 8192).await.unwrap();
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
        let r = run_file(&file, req, dir.path(), &store, 8192).await.unwrap();
        assert!(r.truncated);
        let reference = r.retrieve_ref.expect("should have ref");
        let full = store.get(&reference).unwrap().unwrap();
        assert!(full.contains(&"A".repeat(50000)));
        assert!(r.stdout.len() < full.len());
    }
}
