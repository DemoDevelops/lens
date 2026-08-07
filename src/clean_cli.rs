//! `lens clean [--all] [--yes]` — reclaim the data dirs lens has accumulated.
//!
//! Central storage means a repo's index outlives the repo: delete the checkout
//! and its `<lens home>/projects/<hash>` dir becomes unattributable garbage. The
//! registry (`<lens home>/registry.tsv`) is what maps a dir back to the root it
//! belongs to, so this walks the registry plus the two dir listings under the
//! lens home, and never anything else — `bin`, `ops.log*`, `usage*`,
//! `current_session` and every other thing living at the home root are not
//! reachable from any source here.
//!
//! Every deletion is recursive and irreversible, so two guards sit in front of
//! it: nothing in use is ever removed (a live `build.pid` holder or a heartbeat
//! inside the last minute), and nothing that does not look like a lens data dir
//! is ever a candidate, however a hand-edited registry line spells it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::obs::stats::human_bytes;
use crate::rtk;
use crate::status_cli::dir_size;

/// How fresh a heartbeat has to be for its data dir to count as in use. Mirrors
/// `warmup`'s own `HEARTBEAT_TTL`, the interval a live server re-touches at.
const LIVE_HEARTBEAT_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// `lens clean [--all] [--yes]`.
pub fn run_cli(args: &[String]) {
    let all = args.iter().any(|a| a == "--all");
    let yes = args.iter().any(|a| a == "--yes");
    let Some(home) = rtk::home_root() else {
        println!("lens clean: no home directory found; set $LENS_HOME");
        return;
    };
    clean(&home, all, yes, confirm_on_stdin, &mut |line| println!("{line}"));
}

/// What `lens clean` knows about one data dir.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// The root the registry attributes this dir to: the live one when several
    /// spellings map here, else the first recorded, else `None` for a dir no
    /// registry line ever claimed.
    root: Option<PathBuf>,
    data_dir: PathBuf,
    size: u64,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// A registry root that still exists, with its state kept centrally.
    Active,
    /// A registry root that still exists, with its state in-tree (`<root>/.lens`),
    /// from before central storage. Never moved, only ever offered for deletion.
    Legacy,
    /// The registry knows the root; the root is gone.
    OrphanRootGone,
    /// A dir under `projects/`/`unscoped/` no live registry root claims. Every
    /// unscoped dir lands here by construction: the server deliberately keeps
    /// unscoped roots out of the registry.
    OrphanUnattributed,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Legacy => "legacy in-tree",
            Self::OrphanRootGone => "orphan: root path gone",
            Self::OrphanUnattributed => "orphan: no registry root",
        }
    }

    fn is_orphan(self) -> bool {
        matches!(self, Self::OrphanRootGone | Self::OrphanUnattributed)
    }
}

/// One thing this run will try to delete.
struct Target {
    path: PathBuf,
    size: u64,
    probe: bool,
}

/// [`run_cli`]'s core with both effects injected — the confirmation and the
/// output sink — so tests drive either answer without a terminal and read back
/// the exact screen. Emitting line by line rather than returning the screen is
/// what puts the table in front of the user BEFORE the prompt asks them to
/// approve it.
fn clean(
    home: &Path,
    all: bool,
    yes: bool,
    confirm: impl FnOnce(&str) -> bool,
    emit: &mut impl FnMut(&str),
) {
    let entries = survey(home);
    let probes = probe_files(home);
    for line in table(home, &entries, &probes) {
        emit(&line);
    }

    let targets: Vec<Target> = entries
        .iter()
        .filter(|e| all || e.state.is_orphan())
        .map(|e| Target {
            path: e.data_dir.clone(),
            size: e.size,
            probe: false,
        })
        .chain(probes.iter().map(|p| Target {
            path: p.clone(),
            size: file_size(p),
            probe: true,
        }))
        .collect();

    if targets.is_empty() {
        emit("lens clean: nothing to reclaim");
        return;
    }

    let dirs = targets.iter().filter(|t| !t.probe).count();
    let prompt = format!(
        "delete {dirs} data dir(s) and {} probe file(s), {}?",
        targets.len() - dirs,
        human_bytes(targets.iter().map(|t| t.size).sum())
    );
    if !yes && !confirm(&prompt) {
        emit("lens clean: aborted, nothing deleted");
        return;
    }

    let (mut reclaimed, mut removed_dirs, mut removed_probes) = (0u64, 0usize, 0usize);
    for target in &targets {
        // The live guard runs before EVERY deletion, with no exception for the
        // probe files: a dir in use is a dir some process is reading an index out
        // of right now, and the survey above is old the instant it is taken.
        if in_use(guard_dir(&target.path)) {
            emit(&format!(
                "  skipped {} — in use (live build or heartbeat)",
                target.path.display()
            ));
            continue;
        }
        match remove(&target.path) {
            Ok(()) => {
                reclaimed += target.size;
                if target.probe {
                    removed_probes += 1;
                } else {
                    removed_dirs += 1;
                }
            }
            Err(e) => emit(&format!("  skipped {} — {e}", target.path.display())),
        }
    }

    emit(&format!(
        "lens clean: reclaimed {} ({removed_dirs} data dir(s), {removed_probes} probe file(s))",
        human_bytes(reclaimed)
    ));
}

/// Every data dir this command may consider, keyed by DATA DIR rather than by
/// root. The registry writes its root column verbatim while deriving the data-dir
/// column from a canonical hash, so two spellings of one root (a symlinked
/// checkout, a `../proj` argument) produce two lines pointing at one dir; keying
/// on the dir is what keeps that from being listed — and deleted — twice.
///
/// Dirs that no longer exist are dropped: a registry line outliving its dir is a
/// stale record, not something to reclaim, and listing it would mean `lens clean`
/// never converges to "nothing to reclaim".
fn survey(home: &Path) -> Vec<Entry> {
    let mut by_dir: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for (root, data_dir) in registry(home) {
        by_dir.entry(data_dir).or_default().push(root);
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for (data_dir, roots) in by_dir {
        if !is_cleanable(home, &data_dir) || !data_dir.exists() {
            continue;
        }
        let live = roots.iter().find(|r| r.exists()).cloned();
        let state = match &live {
            Some(r) if data_dir == r.join(".lens") => State::Legacy,
            Some(_) => State::Active,
            None => State::OrphanRootGone,
        };
        seen.insert(data_dir.clone());
        entries.push(Entry {
            root: live.or_else(|| roots.first().cloned()),
            size: dir_size(&data_dir),
            data_dir,
            state,
        });
    }

    for sub in ["projects", "unscoped"] {
        for data_dir in child_dirs(&home.join(sub)) {
            if seen.contains(&data_dir) {
                continue;
            }
            entries.push(Entry {
                root: None,
                size: dir_size(&data_dir),
                data_dir,
                state: State::OrphanUnattributed,
            });
        }
    }

    entries.sort_by(|a, b| a.data_dir.cmp(&b.data_dir));
    entries
}

/// `root\tdata_dir` pairs from the registry. Deduped on read: the writer's dedup
/// is a read-then-append under no lock, so two processes racing the same new root
/// can both miss and write the line twice.
fn registry(home: &Path) -> Vec<(PathBuf, PathBuf)> {
    let raw = std::fs::read_to_string(home.join("registry.tsv")).unwrap_or_default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    raw.lines()
        .filter(|line| seen.insert(line.to_string()))
        .filter_map(|line| line.split_once('\t'))
        .map(|(root, dir)| (PathBuf::from(root), PathBuf::from(dir)))
        .collect()
}

/// Immediate subdirectories of `dir`, sorted; empty when `dir` is absent.
fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    out.sort();
    out
}

/// The scope-probe verdict cache at the home root (`scope.<hash>.probe`): tiny,
/// TTL'd, and re-probed on demand, so always safe to drop. Matched by name at the
/// home root and nowhere else, which is what keeps `bin`, `ops.log*`, `usage*`,
/// `current_session`, `registry.tsv` and `disabled` from ever being candidates.
fn probe_files(home: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("scope.") && n.ends_with(".probe"))
        })
        .collect();
    out.sort();
    out
}

/// May this command delete `path`? Two independent gates, both required: it must
/// not be one of the lens home's own belongings, and it must actually look like a
/// data dir lens created.
fn is_cleanable(home: &Path, path: &Path) -> bool {
    !is_protected(home, path) && looks_like_a_data_dir(home, path)
}

/// Everything at the lens home root except the `projects/<hash>` and
/// `unscoped/<hash>` dirs themselves: the installed binary, the machine-global op
/// log and usage mirror, the current-session pointer, the registry, the denylist,
/// and the two container dirs. Off limits whatever a registry line claims.
fn is_protected(home: &Path, path: &Path) -> bool {
    if path == home {
        return true;
    }
    let Ok(rel) = path.strip_prefix(home) else {
        // Not under the lens home at all — a legacy `<root>/.lens` or a pinned
        // `$LENS_DIR`. Nothing here protects it; `looks_like_a_data_dir` does.
        return false;
    };
    let mut parts = rel.components();
    let Some(first) = parts.next().and_then(|c| c.as_os_str().to_str()) else {
        return true;
    };
    !matches!(first, "projects" | "unscoped") || parts.next().is_none()
}

/// Does `path` look like a dir lens itself created? Deletion here is recursive,
/// and the registry is a plain text file a user can edit — the one input that is
/// not a directory listing — so a line in it never gets to name an arbitrary
/// path. A dir under the home's own `projects/`/`unscoped/`, a dir literally
/// named `.lens`, and a dir holding a lens artifact all qualify; `/`, a home
/// directory and a repo root do not.
fn looks_like_a_data_dir(home: &Path, path: &Path) -> bool {
    if path.starts_with(home.join("projects")) || path.starts_with(home.join("unscoped")) {
        return true;
    }
    if path.file_name().is_some_and(|n| n == ".lens") {
        return true;
    }
    ["index.db", "graph.json", "index.manifest.json", "ops.log", "heartbeats"]
        .iter()
        .any(|artifact| path.join(artifact).exists())
}

/// The data dir a deletion target belongs to: itself when it is one, its parent
/// otherwise (a probe file at the home root), so [`in_use`] can run in front of
/// every deletion without a special case.
fn guard_dir(path: &Path) -> &Path {
    if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    }
}

/// Is something using `dir` right now? A `build.pid` held by a live process means
/// a build is writing into it; a heartbeat inside [`LIVE_HEARTBEAT_TTL`] means an
/// MCP server is serving out of it. Either way, deleting it recursively would
/// pull the index out from under a running process.
fn in_use(dir: &Path) -> bool {
    let pid = std::fs::read_to_string(dir.join("build.pid"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok());
    if pid.is_some_and(crate::server::pid_alive) {
        return true;
    }
    std::fs::read_dir(dir.join("heartbeats"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                // A future mtime (clock skew) reads as fresh: erring toward
                // "in use" costs a skipped dir, erring the other way costs a
                // live session its index.
                .is_ok_and(|t| t.elapsed().map(|age| age < LIVE_HEARTBEAT_TTL).unwrap_or(true))
        })
}

fn remove(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// The report header: one row per data dir, plus a count of the probe files.
fn table(home: &Path, entries: &[Entry], probes: &[PathBuf]) -> Vec<String> {
    let mut lines = vec![format!("lens clean: {}", home.display())];
    if entries.is_empty() && probes.is_empty() {
        lines.push("  (nothing found)".to_string());
        return lines;
    }
    let rows: Vec<(String, String, String, &str)> = entries
        .iter()
        .map(|e| {
            (
                e.root
                    .as_ref()
                    .map_or_else(|| "-".to_string(), |r| r.display().to_string()),
                // Relative to the home the header just named, so two full-path
                // columns don't wrap every row off the side of the terminal. A
                // legacy in-tree dir is not under it and stays absolute.
                e.data_dir
                    .strip_prefix(home)
                    .unwrap_or(&e.data_dir)
                    .display()
                    .to_string(),
                human_bytes(e.size),
                e.state.label(),
            )
        })
        .collect();
    let root_w = rows.iter().map(|r| r.0.len()).chain([4]).max().unwrap_or(4);
    let dir_w = rows.iter().map(|r| r.1.len()).chain([8]).max().unwrap_or(8);
    lines.push(format!(
        "  {:<root_w$}  {:<dir_w$}  {:>9}  {}",
        "root", "data dir", "size", "state"
    ));
    for (root, dir, size, state) in &rows {
        lines.push(format!(
            "  {root:<root_w$}  {dir:<dir_w$}  {size:>9}  {state}"
        ));
    }
    if !probes.is_empty() {
        lines.push(format!("  + {} scope probe file(s)", probes.len()));
    }
    lines
}

/// The y/N prompt. Anything but an explicit `y` is a no.
fn confirm_on_stdin(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok() && answer.trim().eq_ignore_ascii_case("y")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Drive [`clean`] with its output collected instead of printed.
    fn run(home: &Path, all: bool, yes: bool, confirm: impl FnOnce(&str) -> bool) -> String {
        let mut lines: Vec<String> = Vec::new();
        clean(home, all, yes, confirm, &mut |line| {
            lines.push(line.to_string())
        });
        lines.join("\n")
    }

    /// A data dir at `home/projects/<name>` holding `bytes` of payload.
    fn central_dir(home: &Path, name: &str, bytes: usize) -> PathBuf {
        let dir = home.join("projects").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.db"), vec![b'x'; bytes]).unwrap();
        dir
    }

    fn write_registry(home: &Path, lines: &[(&Path, &Path)]) {
        let body: String = lines
            .iter()
            .map(|(root, dir)| format!("{}\t{}\n", root.display(), dir.display()))
            .collect();
        std::fs::write(home.join("registry.tsv"), body).unwrap();
    }

    /// The headline case: a registry root that no longer exists, whose dir carries
    /// a dead builder's lock, is reclaimed and its bytes reported.
    #[test]
    fn orphan_with_a_dead_pid_is_reclaimed() {
        let home = tempdir().unwrap();
        let gone = home.path().join("was-a-repo");
        let dir = central_dir(home.path(), "deadbeef", 4096);
        // pid 0 never names a real process, so `pid_alive` reads it as dead.
        std::fs::write(dir.join("build.pid"), "0").unwrap();
        write_registry(home.path(), &[(gone.as_path(), dir.as_path())]);

        let out = run(home.path(), false, true, |_| false);

        assert!(!dir.exists(), "orphan survived:\n{out}");
        assert!(out.contains("orphan: root path gone"), "{out}");
        assert!(out.contains("reclaimed 4.0 KB (1 data dir(s)"), "{out}");
    }

    /// A live `build.pid` holder is never deleted, however orphaned it looks.
    #[test]
    fn live_build_pid_is_skipped() {
        let home = tempdir().unwrap();
        let gone = home.path().join("was-a-repo");
        let dir = central_dir(home.path(), "livebuild", 128);
        std::fs::write(dir.join("build.pid"), std::process::id().to_string()).unwrap();
        write_registry(home.path(), &[(gone.as_path(), dir.as_path())]);

        let out = run(home.path(), true, true, |_| false);

        assert!(dir.exists(), "clean deleted a dir with a live build:\n{out}");
        assert!(out.contains("in use (live build or heartbeat)"), "{out}");
        assert!(out.contains("reclaimed 0 B (0 data dir(s)"), "{out}");
    }

    /// A fresh heartbeat means a live MCP server is serving out of this dir.
    #[test]
    fn fresh_heartbeat_is_skipped() {
        let home = tempdir().unwrap();
        let gone = home.path().join("was-a-repo");
        let dir = central_dir(home.path(), "livesession", 128);
        std::fs::create_dir_all(dir.join("heartbeats")).unwrap();
        std::fs::write(dir.join("heartbeats").join("4242.pid"), "4242").unwrap();
        write_registry(home.path(), &[(gone.as_path(), dir.as_path())]);

        let out = run(home.path(), true, true, |_| false);

        assert!(dir.exists(), "clean deleted a dir with a live heartbeat:\n{out}");
        assert!(out.contains("in use (live build or heartbeat)"), "{out}");
    }

    /// The lens home's own belongings are never candidates — not from the dir
    /// listings, and not even when a hand-edited registry line names them.
    #[test]
    fn home_belongings_survive_all_yes() {
        let home = tempdir().unwrap();
        let live_root = home.path().join("still-here");
        std::fs::create_dir_all(&live_root).unwrap();
        std::fs::create_dir_all(home.path().join("bin")).unwrap();
        std::fs::write(home.path().join("bin").join("lens"), b"binary").unwrap();
        for name in ["ops.log", "ops.log.1", "usage.jsonl", "current_session"] {
            std::fs::write(home.path().join(name), b"keep").unwrap();
        }
        let orphan = central_dir(home.path(), "orphan", 64);
        write_registry(
            home.path(),
            &[
                // A hostile line pointing the cleaner at the home's own dirs.
                (live_root.as_path(), home.path().join("bin").as_path()),
                (live_root.as_path(), home.path()),
                (live_root.as_path(), home.path().join("projects").as_path()),
                (home.path().join("gone").as_path(), orphan.as_path()),
            ],
        );

        let out = run(home.path(), true, true, |_| false);

        assert!(home.path().join("bin").join("lens").exists(), "{out}");
        for name in ["ops.log", "ops.log.1", "usage.jsonl", "current_session"] {
            assert!(home.path().join(name).exists(), "{name} deleted:\n{out}");
        }
        assert!(home.path().join("projects").is_dir(), "{out}");
        assert!(home.path().join("registry.tsv").exists(), "{out}");
        assert!(!orphan.exists(), "the real orphan survived:\n{out}");
    }

    /// Default mode keeps active and legacy dirs; `--all` takes everything listed.
    /// Probe files go in both modes.
    #[test]
    fn default_keeps_active_dirs_and_all_takes_them() {
        let home = tempdir().unwrap();
        // Repos live outside the lens home, as real ones do: everything under the
        // home that is not `projects/<hash>` or `unscoped/<hash>` is off limits.
        let repos = tempdir().unwrap();
        let live_root = repos.path().join("repo");
        std::fs::create_dir_all(&live_root).unwrap();
        let legacy = live_root.join(".lens");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("index.db"), vec![b'x'; 32]).unwrap();
        let active = central_dir(home.path(), "active", 64);
        let active_root = repos.path().join("repo2");
        std::fs::create_dir_all(&active_root).unwrap();
        let probe = home.path().join("scope.0123456789abcdef.probe");
        std::fs::write(&probe, "1").unwrap();
        write_registry(
            home.path(),
            &[
                (live_root.as_path(), legacy.as_path()),
                (active_root.as_path(), active.as_path()),
            ],
        );

        let out = run(home.path(), false, true, |_| false);
        assert!(active.exists(), "default mode deleted an active dir:\n{out}");
        assert!(legacy.exists(), "default mode deleted a legacy dir:\n{out}");
        assert!(!probe.exists(), "default mode kept a probe file:\n{out}");
        assert!(out.contains("active"), "{out}");
        assert!(out.contains("legacy in-tree"), "{out}");

        let out = run(home.path(), true, true, |_| false);
        assert!(!active.exists(), "--all kept an active dir:\n{out}");
        assert!(!legacy.exists(), "--all kept a legacy dir:\n{out}");
    }

    /// Two spellings of one root produce two registry lines pointing at one dir;
    /// the dir is listed once, and a live root anywhere keeps it active.
    #[test]
    fn one_dir_reached_by_two_roots_is_listed_once() {
        let home = tempdir().unwrap();
        let live = home.path().join("repo");
        std::fs::create_dir_all(&live).unwrap();
        let gone = home.path().join("repo-old-spelling");
        let dir = central_dir(home.path(), "shared", 64);
        write_registry(
            home.path(),
            &[
                (gone.as_path(), dir.as_path()),
                (live.as_path(), dir.as_path()),
                // A duplicate line: the writer's dedup is racy, readers dedup.
                (live.as_path(), dir.as_path()),
            ],
        );

        let out = run(home.path(), false, true, |_| false);

        assert_eq!(
            out.matches("projects/shared").count(),
            1,
            "listed more than once:\n{out}"
        );
        assert!(out.contains("active"), "{out}");
        assert!(dir.exists(), "an active dir was deleted by default:\n{out}");
    }

    /// Declining the prompt deletes nothing.
    #[test]
    fn declined_confirmation_deletes_nothing() {
        let home = tempdir().unwrap();
        let dir = central_dir(home.path(), "orphan", 64);

        let out = run(home.path(), false, false, |prompt| {
            assert!(prompt.contains("delete 1 data dir(s)"), "{prompt}");
            false
        });

        assert!(dir.exists(), "declined but deleted:\n{out}");
        assert!(out.contains("aborted, nothing deleted"), "{out}");
    }

    /// A `projects/<hash>` dir no registry line claims is an orphan by default.
    #[test]
    fn unattributed_dirs_are_orphans() {
        let home = tempdir().unwrap();
        let stray = central_dir(home.path(), "stray", 64);
        let unscoped = home.path().join("unscoped").join("abcd");
        std::fs::create_dir_all(&unscoped).unwrap();
        std::fs::write(unscoped.join("ops.log"), vec![b'x'; 16]).unwrap();

        let out = run(home.path(), false, true, |_| false);

        assert!(out.contains("orphan: no registry root"), "{out}");
        assert!(!stray.exists(), "{out}");
        assert!(!unscoped.exists(), "{out}");
    }

    /// An empty home reports nothing to do rather than prompting.
    #[test]
    fn empty_home_reclaims_nothing() {
        let home = tempdir().unwrap();
        let out = run(home.path(), true, false, |_| {
            panic!("must not prompt with nothing to delete")
        });
        assert!(out.contains("nothing to reclaim"), "{out}");
    }
}
