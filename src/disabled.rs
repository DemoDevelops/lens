//! Global denylist: `lens off <path>` / `lens on <path>` mark a tree as
//! ignored without ever touching it. One absolute, canonicalized path per
//! line in `~/.lens/disabled`; subtree semantics via `Path::starts_with` on
//! path components (never a string prefix -- `/a/b` must not cover `/a/bc`).
//! Disabled is a separate axis from "not a code project": the server
//! computes `scoped = indexable_root(root) && covering_entry(root).is_none()`.

use std::path::{Path, PathBuf};

use crate::rtk;

/// The first denylist entry that equals `path` or is a component-wise
/// ancestor of it, if any. Canonicalizes `path` first (falling back to the
/// given path if canonicalization fails, e.g. it doesn't exist yet).
/// `None` with no `$LENS_HOME`/`$HOME`, or when the denylist file is missing.
pub fn covering_entry(path: &Path) -> Option<PathBuf> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let denylist = rtk::home_root()?.join("disabled");
    let contents = std::fs::read_to_string(&denylist).ok()?;
    covering_entry_in(&canon, &contents)
}

/// [`covering_entry`] with the file contents injected, so the ancestor logic
/// is exercisable without touching `$HOME`/`$LENS_HOME` or the filesystem.
fn covering_entry_in(path: &Path, file_contents: &str) -> Option<PathBuf> {
    file_contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .find(|entry| path == entry || path.starts_with(entry))
}

/// Resolve the CLI's optional path argument, defaulting to cwd, canonicalized
/// (falling back to the given path if canonicalization fails).
fn resolve_target(args: &[String]) -> PathBuf {
    let raw = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    raw.canonicalize().unwrap_or(raw)
}

/// `lens off [path]`: append `path` (default cwd) to the denylist.
pub fn run_off_cli(args: &[String]) {
    let target = resolve_target(args);
    match rtk::home_root() {
        Some(home) => println!("{}", off(&home.join("disabled"), &target)),
        None => println!("lens off: no home directory found; set $LENS_HOME"),
    }
}

/// `lens on [path]`: remove `path` (default cwd) from the denylist.
pub fn run_on_cli(args: &[String]) {
    let target = resolve_target(args);
    match rtk::home_root() {
        Some(home) => println!("{}", on(&home.join("disabled"), &target)),
        None => println!("lens on: no home directory found; set $LENS_HOME"),
    }
}

/// [`run_off_cli`]'s core: append `target` to the denylist at `denylist`
/// unless it's already listed exactly (idempotent). Injected path so tests
/// drive it against a temp file instead of the real `~/.lens/disabled`.
fn off(denylist: &Path, target: &Path) -> String {
    let target_str = target.to_string_lossy();
    let contents = std::fs::read_to_string(denylist).unwrap_or_default();
    if contents.lines().any(|line| line.trim() == target_str) {
        return format!(
            "lens off: {} is already disabled (undo: lens on {})",
            target.display(),
            target.display()
        );
    }
    if let Some(parent) = denylist.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut updated = contents;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&target_str);
    updated.push('\n');
    match std::fs::write(denylist, updated) {
        Ok(()) => format!(
            "lens off: {} — lens will ignore this tree (undo: lens on {})",
            target.display(),
            target.display()
        ),
        Err(e) => format!("lens off: failed to write {}: {e}", denylist.display()),
    }
}

/// [`run_on_cli`]'s core: remove `target`'s exact entry from the denylist at
/// `denylist`. A covering ancestor (not an exact match) is left untouched --
/// only `lens on <ancestor>` re-enables the whole tree.
fn on(denylist: &Path, target: &Path) -> String {
    let contents = std::fs::read_to_string(denylist).unwrap_or_default();
    match covering_entry_in(target, &contents) {
        Some(entry) if entry == target => {
            let target_str = target.to_string_lossy();
            let updated: String = contents
                .lines()
                .filter(|line| line.trim() != target_str)
                .map(|line| format!("{line}\n"))
                .collect();
            match std::fs::write(denylist, updated) {
                Ok(()) => format!(
                    "lens on: {} — lens will index this tree again",
                    target.display()
                ),
                Err(e) => format!("lens on: failed to write {}: {e}", denylist.display()),
            }
        }
        Some(entry) => format!(
            "disabled via {}: run `lens on {}` to re-enable the whole tree",
            entry.display(),
            entry.display()
        ),
        None => format!("lens on: {} was not disabled", target.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covering_entry_in_matches_ancestor_by_component_not_string_prefix() {
        let denylist = "/a/b\n";
        assert_eq!(
            covering_entry_in(Path::new("/a/b/c"), denylist),
            Some(PathBuf::from("/a/b"))
        );
        assert_eq!(covering_entry_in(Path::new("/a/bc"), denylist), None);
    }

    #[test]
    fn covering_entry_in_matches_exact_entry() {
        let denylist = "/a/b\n";
        assert_eq!(
            covering_entry_in(Path::new("/a/b"), denylist),
            Some(PathBuf::from("/a/b"))
        );
    }

    #[test]
    fn covering_entry_in_ignores_blank_lines() {
        let denylist = "\n/a/b\n\n";
        assert_eq!(
            covering_entry_in(Path::new("/a/b/c"), denylist),
            Some(PathBuf::from("/a/b"))
        );
    }

    #[test]
    fn covering_entry_in_no_match_returns_none() {
        assert_eq!(covering_entry_in(Path::new("/a/b"), "/x/y\n"), None);
    }

    #[test]
    fn off_then_on_round_trips_on_an_injected_file() {
        let tmp = std::env::temp_dir().join(format!(
            "lens-disabled-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let denylist = tmp.join("disabled");
        let target = tmp.join("project");

        let off_msg = off(&denylist, &target);
        assert!(off_msg.contains("lens will ignore this tree"));
        let contents = std::fs::read_to_string(&denylist).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert_eq!(contents.trim(), target.to_string_lossy());

        let on_msg = on(&denylist, &target);
        assert!(on_msg.contains("lens will index this tree again"));
        let contents = std::fs::read_to_string(&denylist).unwrap();
        assert!(contents.trim().is_empty());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn on_under_covering_ancestor_edits_nothing_and_names_the_ancestor() {
        let tmp = std::env::temp_dir().join(format!(
            "lens-disabled-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let denylist = tmp.join("disabled");
        let ancestor = tmp.join("workspace");
        let child = ancestor.join("child");

        off(&denylist, &ancestor);
        let before = std::fs::read_to_string(&denylist).unwrap();

        let msg = on(&denylist, &child);
        assert!(msg.contains(&ancestor.display().to_string()));
        assert!(msg.contains("lens on"));

        let after = std::fs::read_to_string(&denylist).unwrap();
        assert_eq!(before, after, "on under a covering ancestor must not edit the file");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn double_off_appends_one_line() {
        let tmp = std::env::temp_dir().join(format!(
            "lens-disabled-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let denylist = tmp.join("disabled");
        let target = tmp.join("project");

        off(&denylist, &target);
        let first_msg = off(&denylist, &target);
        assert!(first_msg.contains("already disabled"));

        let contents = std::fs::read_to_string(&denylist).unwrap();
        assert_eq!(contents.lines().count(), 1);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
