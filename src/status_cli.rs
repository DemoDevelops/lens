//! `lens status [path]` — one screen answering "what does lens think of this
//! directory, and where does its state live?".
//!
//! Read-only, and that is a correctness property rather than a nicety: a
//! `<root>/.lens` dir is itself a project marker, so a status run that created
//! one would flip the very scope verdict it just printed, and a status run on a
//! tree lens deliberately refuses to index would leave the dropping that refusal
//! exists to prevent. So nothing here opens a `Store` or an `Index` (both
//! `create_dir_all` their data dir), nothing here creates a directory, and every
//! fact below comes from a stat, a read, or the pure resolver. The one file this
//! can touch is `discovery`'s own probe-verdict cache in the global lens home,
//! which `indexable_root` maintains for every caller and which never lands in the
//! reported tree.

use std::path::{Path, PathBuf};

use crate::obs::stats::human_bytes;
use crate::{disabled, discovery, obs, server, warmup};

/// `lens status [path]`: report on `path` (default cwd).
pub fn run_cli(args: &[String]) {
    let raw = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let root = raw.canonicalize().unwrap_or(raw);
    println!("{}", report(&root));
}

/// [`run_cli`]'s core, returning the screen instead of printing it, so the whole
/// report is assertable in tests (same split as `disabled`'s `off`/`on`).
fn report(root: &Path) -> String {
    let mut lines = vec![row("root:", &root.display().to_string())];

    // Scope, asked in the order the server asks it (`server::Forge::with_paths`):
    // `lens off` is a separate axis from "is this a code project" and is the
    // cheaper check, and the marker check answers every real repo before the
    // probe walk is reached.
    let entry = disabled::covering_entry(root);
    let (verdict, scoped) = match &entry {
        Some(e) => (format!("disabled (lens off {})", e.display()), false),
        None => match discovery::first_marker(root) {
            Some(marker) => (format!("in scope — project marker {marker}"), true),
            None if discovery::indexable_root(root) => (
                "in scope — no marker, within the file-count probe budget".to_string(),
                true,
            ),
            None => (
                "out of scope — no project marker, over the file-count probe budget".to_string(),
                false,
            ),
        },
    };
    lines.push(row("scope:", &verdict));

    // The dir the SERVER would use, resolved the way the server resolves it: the
    // shared resolver for a scoped root, the unscoped redirect otherwise. Naming
    // `data_dir_for`'s answer for an unscoped root would name a dir nothing ever
    // writes to.
    let resolved = obs::data_dir_for(root);
    let data_dir = if scoped {
        resolved.clone()
    } else {
        server::unscoped_data_dir(root, resolved.clone())
    };
    lines.push(row(
        "data dir:",
        &format!(
            "{} ({})",
            data_dir.display(),
            data_dir_rule(root, &resolved, &data_dir)
        ),
    ));

    let index_manifest = data_dir.join("index.manifest.json");
    lines.push(row(
        "index:",
        &match manifest_entries(&index_manifest) {
            Some(files) => format!(
                "{files} files, built {}, {} on disk",
                age(&index_manifest).unwrap_or_else(|| "at an unknown time".to_string()),
                human_bytes(dir_size(&data_dir))
            ),
            None => "not built".to_string(),
        },
    ));

    // Node/edge counts would mean opening the graph DB, which creates the data
    // dir — the file's own size and the manifest's age are what a read-only
    // report can honestly say.
    let graph = data_dir.join("graph.json");
    lines.push(row(
        "graph:",
        &match std::fs::metadata(&graph).ok().map(|m| m.len()) {
            Some(bytes) => format!(
                "graph.json {}, built {}",
                human_bytes(bytes),
                age(&data_dir.join("graph.manifest.json"))
                    .unwrap_or_else(|| "at an unknown time".to_string())
            ),
            None => "not built".to_string(),
        },
    ));

    if let Some(state) = warmup::read_progress(&data_dir) {
        lines.push(row(
            "build:",
            &if server::pid_alive(state.pid) {
                format!(
                    "in flight — {}/{} files ({}), pid {}",
                    state.done,
                    state.total,
                    if state.indexing { "indexing" } else { "graphing" },
                    state.pid
                )
            } else {
                format!("stale build.progress from a dead builder (pid {})", state.pid)
            },
        ));
    }

    // Gated on `scoped` for the same reason federation is: the walk that finds
    // nested repos is exactly the traversal the scope guard refuses to run on an
    // unscoped tree (a home directory, a workspace of unrelated checkouts).
    if scoped {
        let nested = discovery::nested_repo_roots(root);
        lines.push(row(
            "nested:",
            &match nested.len() {
                0 => "none".to_string(),
                1 => "1 repo".to_string(),
                n => format!("{n} repos"),
            },
        ));
        for repo in &nested {
            let rel = repo.strip_prefix(root).unwrap_or(repo);
            let built = if is_built(&obs::data_dir_for(repo)) {
                "built"
            } else {
                "not built"
            };
            lines.push(format!("{:<12}{} — {built}", "", rel.display()));
        }
    }

    if let Some(e) = &entry {
        lines.push(row(
            "disabled:",
            &format!("run `lens on {}` to re-enable this tree", e.display()),
        ));
    }

    lines.join("\n")
}

/// One `label` + `value` line of the report.
fn row(label: &str, value: &str) -> String {
    format!("{label:<10}{value}")
}

/// Which of the four rules in [`obs::data_dir_for`] put this root's state where
/// it is, plus the unscoped redirect layered on top by the server.
///
/// Deliberately NOT a second resolver: `data_dir_for` has already computed the
/// path (`resolved`), and the server's redirect has already been applied
/// (`effective`). This only re-asks the same conditions, in the same order, with
/// the same env reads, and names the first that holds — so the two can never
/// disagree about WHERE state lives, only about what to call it. Any change to
/// `data_dir_for`'s rules has to be mirrored here, which is why the rules are
/// spelled out in the same sequence.
fn data_dir_rule(root: &Path, resolved: &Path, effective: &Path) -> &'static str {
    // Layered on top of the four rules: an unscoped (or disabled) root is
    // redirected away from whatever the resolver picked.
    if effective != resolved {
        return "unscoped";
    }
    if std::env::var_os("LENS_DIR").is_some_and(|d| !d.is_empty()) {
        return "env ($LENS_DIR)";
    }
    let in_tree = root.join(".lens");
    if in_tree.join("index.db").exists() || in_tree.join("graph.json").exists() {
        return "legacy (in-tree .lens)";
    }
    if std::env::var("LENS_CENTRAL_STORE").ok().as_deref() == Some("0") {
        return "in-tree (LENS_CENTRAL_STORE=0)";
    }
    // Rule 4 is central storage, except for a process with no lens home at all,
    // which the resolver falls back into the tree rather than scattering state
    // somewhere `lens clean` would never look.
    if effective == in_tree {
        "in-tree (no lens home)"
    } else {
        "central"
    }
}

/// Number of files an `index.manifest.json` covers, or `None` when it is absent
/// or unreadable (i.e. nothing has been built here).
fn manifest_entries(path: &Path) -> Option<usize> {
    let raw = std::fs::read_to_string(path).ok()?;
    let manifest: std::collections::BTreeMap<String, u64> = serde_json::from_str(&raw).ok()?;
    Some(manifest.len())
}

/// Has anything been built into `dir`? The index manifest is written last by
/// both the foreground and the background builder, so its presence is the
/// cheapest honest signal that does not open the DB.
fn is_built(dir: &Path) -> bool {
    dir.join("index.manifest.json").exists() || dir.join("graph.json").exists()
}

/// Recursive on-disk size of `dir`, 0 when it is absent. Symlinks are not
/// followed and unreadable entries are skipped: a size is a report, never a
/// reason to fail. Shared with `lens clean`, which sizes the same dirs.
pub(crate) fn dir_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Coarse "N ago" for a file's mtime; `None` when it cannot be read. Same
/// buckets as `session::hook`'s `rel_age`.
fn age(path: &Path) -> Option<String> {
    let secs = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?
        .as_secs();
    Some(match secs {
        d if d < 90 => "just now".to_string(),
        d if d < 90 * 60 => format!("{}m ago", d / 60),
        d if d < 36 * 3600 => format!("{}h ago", d / 3600),
        d => format!("{}d ago", d / 86400),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Every test that calls [`report`] scopes `$LENS_HOME` to a temp dir under
    /// the shared env guard: `home_root()` is process-global, and central storage,
    /// the denylist and the probe cache all hang off it. Restores the previous
    /// value so siblings that run after us see what they expect.
    struct TempHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
        dir: tempfile::TempDir,
    }

    impl TempHome {
        fn new() -> Self {
            let guard = crate::rtk::env_test_lock();
            let prev = std::env::var_os("LENS_HOME");
            let dir = tempdir().unwrap();
            std::env::set_var("LENS_HOME", dir.path());
            Self {
                _guard: guard,
                prev,
                dir,
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("LENS_HOME", v),
                None => std::env::remove_var("LENS_HOME"),
            }
        }
    }

    /// Every entry under `dir`, sorted — the before/after snapshot that proves a
    /// status run left a tree exactly as it found it.
    fn snapshot(dir: &Path) -> Vec<PathBuf> {
        let mut all: Vec<PathBuf> = walkdir::WalkDir::new(dir)
            .into_iter()
            .flatten()
            .map(|e| e.path().to_path_buf())
            .collect();
        all.sort();
        all
    }

    /// A repo with a `.git` marker and `files` source files.
    fn repo_with(files: usize) -> tempfile::TempDir {
        let repo = tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        for i in 0..files {
            std::fs::write(
                repo.path().join(format!("f{i}.rs")),
                format!("fn f{i}() -> i32 {{ {i} }}\n"),
            )
            .unwrap();
        }
        repo
    }

    /// The headline case: a real index built under central storage. The report
    /// must name the central dir, label the rule `central`, and count the files
    /// the manifest actually covers.
    #[test]
    fn fresh_indexed_repo_reports_central_storage_and_file_counts() {
        let home = TempHome::new();
        let repo = repo_with(3);
        let root = repo.path().canonicalize().unwrap();

        let data_dir = obs::data_dir_for(&root);
        assert_eq!(
            data_dir,
            home.path().join("projects").join(obs::project_hash(&root)),
            "fresh repo must resolve centrally"
        );
        std::fs::create_dir_all(&data_dir).unwrap();
        warmup::warmup(&root, &data_dir).unwrap();

        let out = report(&root);

        assert!(
            out.contains(&format!("root:     {}", root.display())),
            "{out}"
        );
        assert!(out.contains("scope:    in scope — project marker .git"), "{out}");
        assert!(
            out.contains(&format!("{} (central)", data_dir.display())),
            "{out}"
        );
        assert!(out.contains("index:    3 files, built "), "{out}");
        assert!(out.contains("graph:    graph.json "), "{out}");
        assert!(out.contains("nested:   none"), "{out}");
        // Central storage means the tree itself stays clean.
        assert!(!root.join(".lens").exists(), "{out}");
    }

    /// A disabled tree: the covering entry is named, the undo is spelled out, and
    /// the data dir is the unscoped redirect (a disabled root takes the same path
    /// an out-of-scope one takes), so nothing points into the tree.
    #[test]
    fn disabled_dir_names_the_covering_entry_and_redirects_unscoped() {
        let home = TempHome::new();
        let ws = tempdir().unwrap();
        let root = ws.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            home.path().join("disabled"),
            format!("{}\n", root.display()),
        )
        .unwrap();

        let before = snapshot(&root);
        let out = report(&root);

        assert!(
            out.contains(&format!("scope:    disabled (lens off {})", root.display())),
            "{out}"
        );
        assert!(
            out.contains(&format!("disabled: run `lens on {}`", root.display())),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "{} (unscoped)",
                home.path().join("unscoped").join(obs::project_hash(&root)).display()
            )),
            "{out}"
        );
        assert!(out.contains("index:    not built"), "{out}");
        assert_eq!(snapshot(&root), before, "status wrote into a disabled tree");
    }

    /// THE read-only proof: a never-indexed repo must come back byte-identical,
    /// with neither an in-tree `.lens` nor its central dir conjured into being.
    #[test]
    fn status_creates_nothing_on_a_never_indexed_dir() {
        let home = TempHome::new();
        let repo = repo_with(2);
        let root = repo.path().canonicalize().unwrap();
        let data_dir = obs::data_dir_for(&root);

        let before = snapshot(&root);
        let home_before = snapshot(home.path());
        let out = report(&root);

        assert_eq!(snapshot(&root), before, "status changed the tree:\n{out}");
        assert_eq!(
            snapshot(home.path()),
            home_before,
            "status wrote into the lens home:\n{out}"
        );
        assert!(!root.join(".lens").exists(), "status created an in-tree .lens");
        assert!(
            !data_dir.exists(),
            "status created {}",
            data_dir.display()
        );
        assert!(out.contains("index:    not built"), "{out}");
        assert!(out.contains("graph:    not built"), "{out}");
    }

    /// A repo indexed before central storage keeps its in-tree dir, and a live
    /// builder's `build.progress` is reported as in flight (pid-checked).
    #[test]
    fn legacy_in_tree_repo_is_labelled_legacy_and_shows_a_live_build() {
        let _home = TempHome::new();
        let repo = tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let data_dir = root.join(".lens");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("index.db"), b"legacy").unwrap();
        std::fs::write(
            data_dir.join("build.progress"),
            format!("{} 40 120 index", std::process::id()),
        )
        .unwrap();

        let out = report(&root);

        assert!(
            out.contains(&format!("{} (legacy (in-tree .lens))", data_dir.display())),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "build:    in flight — 40/120 files (indexing), pid {}",
                std::process::id()
            )),
            "{out}"
        );
    }

    /// A `build.progress` left behind by a killed builder is reported as stale,
    /// never as a build that will never finish.
    #[test]
    fn dead_builder_progress_is_reported_as_stale() {
        let _home = TempHome::new();
        let repo = tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let data_dir = root.join(".lens");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("index.db"), b"legacy").unwrap();
        // pid 0 never names a real process, so `pid_alive` reads it as dead.
        std::fs::write(data_dir.join("build.progress"), "0 40 120 index").unwrap();

        let out = report(&root);

        assert!(out.contains("build:    stale build.progress"), "{out}");
    }

    /// The rule labeller classifies the path the resolver already produced; it
    /// never produces one of its own.
    #[test]
    fn data_dir_rule_names_the_rule_that_fired() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let central = tmp.path().join("home/projects/abc");

        assert_eq!(data_dir_rule(&root, &central, &central), "central");
        // The redirect is layered on top of every rule.
        let unscoped = tmp.path().join("home/unscoped/abc");
        assert_eq!(data_dir_rule(&root, &central, &unscoped), "unscoped");
        // Keep-if-present outranks central once real artifacts exist.
        std::fs::create_dir_all(root.join(".lens")).unwrap();
        std::fs::write(root.join(".lens").join("graph.json"), b"{}").unwrap();
        let in_tree = root.join(".lens");
        assert_eq!(
            data_dir_rule(&root, &in_tree, &in_tree),
            "legacy (in-tree .lens)"
        );
    }

    /// Nested repos are listed with per-repo built/not, resolved through the
    /// shared resolver so a centrally-built nested index still reads as built.
    #[test]
    fn nested_repos_are_listed_with_their_build_state() {
        let home = TempHome::new();
        let repo = repo_with(1);
        let root = repo.path().canonicalize().unwrap();
        for name in ["built_dep", "cold_dep"] {
            std::fs::create_dir_all(root.join(name).join(".git")).unwrap();
            std::fs::write(root.join(name).join("dep.rs"), "fn dep() {}\n").unwrap();
        }
        // Only one of them has a central index behind it.
        let built = obs::data_dir_for(&root.join("built_dep"));
        std::fs::create_dir_all(&built).unwrap();
        std::fs::write(built.join("index.manifest.json"), "{}").unwrap();
        assert!(built.starts_with(home.path()));

        let out = report(&root);

        assert!(out.contains("nested:   2 repos"), "{out}");
        assert!(out.contains("built_dep — built"), "{out}");
        assert!(out.contains("cold_dep — not built"), "{out}");
    }
}
