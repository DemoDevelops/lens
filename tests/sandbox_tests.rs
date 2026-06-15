//! Integration tests for the sandbox core invariant.

use ctxforge::sandbox;
use ctxforge::store::Store;
use ctxforge::tools::ExecuteRequest;
use tempfile::tempdir;

#[tokio::test]
async fn reads_big_file_returns_only_printed_line() {
    let repo = tempdir().unwrap();
    let store = Store::open(&repo.path().join(".ctxforge")).unwrap();

    // A large input the agent should never see.
    let big = "secret-token-".repeat(100_000);
    std::fs::write(repo.path().join("data.txt"), &big).unwrap();

    let req = ExecuteRequest {
        language: "python".into(),
        code: "print(len(open('data.txt').read()))".into(),
        timeout_secs: 30,
        stdin: None,
    };
    let resp = sandbox::run(req, repo.path(), &store, 8192).await.unwrap();

    assert_eq!(resp.stdout.trim(), big.len().to_string());
    assert!(!resp.stdout.contains("secret-token"));
    assert_eq!(resp.exit_code, 0);
}

#[tokio::test]
async fn large_output_offloaded_and_retrievable() {
    let repo = tempdir().unwrap();
    let store = Store::open(&repo.path().join(".ctxforge")).unwrap();
    let req = ExecuteRequest {
        language: "bash".into(),
        code: "yes ABCDEFGH | head -c 40000".into(),
        timeout_secs: 30,
        stdin: None,
    };
    let resp = sandbox::run(req, repo.path(), &store, 8192).await.unwrap();
    assert!(resp.truncated);
    let reference = resp.retrieve_ref.unwrap();
    let full = store.get(&reference).unwrap().unwrap();
    assert_eq!(full.len(), resp.stdout_bytes);
    assert!(full.len() > resp.stdout.len());
}
