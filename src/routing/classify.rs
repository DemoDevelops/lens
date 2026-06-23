//! Command risk classification for the Bash routing path.
//!
//! One Aho-Corasick automaton over the bounded-command lead keywords (plus the
//! verbose/recursive carve-outs the old regex set handled separately) decides
//! `Risk::Safe`; a `shell-words` first-token check flags destructive roots as
//! `Risk::Block`. `is_structurally_bounded` (the routing skip predicate) is
//! exactly `classify(cmd) == Risk::Safe`. Successor to `mod.rs`'s `PLAIN_ALLOW`,
//! `bounded_patterns`, and the old `is_structurally_bounded`.

use std::collections::HashSet;
use std::sync::OnceLock;

use aho_corasick::{AhoCorasick, MatchKind};

/// Routing risk of a Bash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Structurally bounded output: skip wrap/nudge (passthrough).
    Safe,
    /// Unknown output size: eligible for wrap/nudge.
    Warn,
    /// Destructive root command (logged; routing decision unchanged).
    Block,
}

/// Programs read-only regardless of arguments (the former `PLAIN_ALLOW`). Backs
/// the wrap allowlist ([`super::segment_allowlisted`]); high-output members
/// (`find`, `cat`, `grep`, …) are deliberately NOT `Risk::Safe`, so they still
/// get wrapped.
static SAFE_COMMANDS: OnceLock<HashSet<&'static str>> = OnceLock::new();

fn safe_commands() -> &'static HashSet<&'static str> {
    SAFE_COMMANDS.get_or_init(|| {
        [
            "find", "cat", "ls", "tree", "rg", "grep", "egrep", "fgrep", "tail", "head", "wc",
            "sort", "uniq", "nl", "curl", "wget", "gradle", "gradlew", "mvn", "sbt", "pytest",
            "jest", "vitest",
        ]
        .into_iter()
        .collect()
    })
}

/// Is `prog` read-only regardless of arguments? (wrap-allowlist membership)
pub(crate) fn is_safe_command(prog: &str) -> bool {
    safe_commands().contains(prog)
}

/// Tail rule for a bounded lead-keyword match anchored at offset 0.
#[derive(Clone, Copy)]
enum Tail {
    /// keyword, then a space or end-of-string (the optional-argument patterns).
    SpaceOrEnd,
    /// keyword already ends in a space; a prefix match is sufficient.
    Prefix,
    /// `git log -` followed by a digit.
    Digit,
}

/// Bounded lead keywords, ported from the old `bounded_patterns()` regex set.
/// Index order matches [`bounded_ac`]'s pattern ids.
const BOUNDED: &[(&str, Tail)] = &[
    ("pwd", Tail::SpaceOrEnd),
    ("whoami", Tail::SpaceOrEnd),
    ("hostname", Tail::SpaceOrEnd),
    ("uname", Tail::SpaceOrEnd),
    ("id", Tail::SpaceOrEnd),
    ("date", Tail::SpaceOrEnd),
    ("readlink", Tail::SpaceOrEnd),
    ("basename", Tail::SpaceOrEnd),
    ("dirname", Tail::SpaceOrEnd),
    ("realpath", Tail::SpaceOrEnd),
    ("cd", Tail::SpaceOrEnd),
    ("mkdir", Tail::SpaceOrEnd),
    ("git status", Tail::SpaceOrEnd),
    ("git rev-parse", Tail::SpaceOrEnd),
    ("git remote", Tail::SpaceOrEnd),
    ("git branch", Tail::SpaceOrEnd),
    ("git config --get", Tail::SpaceOrEnd),
    ("git diff --stat", Tail::SpaceOrEnd),
    ("git diff --name-only", Tail::SpaceOrEnd),
    ("git stash list", Tail::SpaceOrEnd),
    ("git tag", Tail::SpaceOrEnd),
    ("echo ", Tail::Prefix),
    ("printf ", Tail::Prefix),
    ("which ", Tail::Prefix),
    ("type ", Tail::Prefix),
    ("command -v ", Tail::Prefix),
    ("touch ", Tail::Prefix),
    ("git log -", Tail::Digit),
];

/// One automaton over every [`BOUNDED`] needle (leftmost-longest); a match whose
/// `start() == 0` means the command leads with a bounded keyword.
static BOUNDED_AC: OnceLock<AhoCorasick> = OnceLock::new();

fn bounded_ac() -> &'static AhoCorasick {
    BOUNDED_AC.get_or_init(|| {
        AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(BOUNDED.iter().map(|(needle, _)| *needle))
            .expect("static bounded needles compile")
    })
}

/// Destructive root commands: `Risk::Block` unless `--dry-run` is present.
const DESTRUCTIVE_ROOTS: &[&str] = &[
    "rm", "dd", "mkfs", "chmod", "chown", "kill", "pkill", "truncate", "shred",
];

/// Shell control operators that disqualify `Risk::Safe`: any one can compose a
/// bounded command with an unbounded sink (`git status | xargs cat`).
fn has_shell_op(cmd: &str) -> bool {
    cmd.contains(['|', ';', '&', '<', '>', '$', '`', '\n', '\r'])
}

/// First token (basename) via `shell-words`, matched against [`DESTRUCTIVE_ROOTS`].
fn is_destructive(cmd: &str) -> bool {
    let tokens = match shell_words::split(cmd) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let first = match tokens.first() {
        Some(t) => super::basename(t),
        None => return false,
    };
    DESTRUCTIVE_ROOTS.contains(&first) && !tokens.iter().any(|t| t == "--dry-run")
}

/// `^\S+\s+-V` (second token `-V`) or a `--version` token anywhere.
fn is_version_probe(cmd: &str) -> bool {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    toks.contains(&"--version") || toks.get(1) == Some(&"-V")
}

/// Single-dash flag bundle (e.g. `-rvf`) containing `ch` (ported from `mod.rs`).
fn flag_bundle_has(cmd: &str, ch: char) -> bool {
    cmd.split_whitespace().any(|tok| match tok.strip_prefix('-') {
        Some(rest) => {
            !rest.starts_with('-')
                && !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_alphabetic())
                && rest.contains(ch)
        }
        None => false,
    })
}

/// Does the command lead with a bounded keyword whose tail rule is satisfied?
fn matches_bounded(cmd: &str) -> bool {
    match bounded_ac().find(cmd) {
        Some(m) if m.start() == 0 => {
            let (needle, tail) = BOUNDED[m.pattern().as_usize()];
            match tail {
                Tail::Prefix => true,
                Tail::SpaceOrEnd => {
                    matches!(cmd.as_bytes().get(needle.len()), None | Some(&b' '))
                }
                Tail::Digit => cmd
                    .as_bytes()
                    .get(needle.len())
                    .is_some_and(u8::is_ascii_digit),
            }
        }
        _ => false,
    }
}

/// Structurally-bounded output (the old `is_structurally_bounded` accept-set):
/// the verbose/recursive carve-outs, a version probe, or a bounded lead keyword.
fn is_bounded(cmd: &str) -> bool {
    match cmd.split_whitespace().next().unwrap_or("") {
        // mv/cp/rm/ln are bounded only without a verbose flag (verbose prints one
        // line per file). rm here only reaches this branch with `--dry-run` set
        // (otherwise `is_destructive` already returned Block).
        "mv" | "cp" | "rm" | "ln" => {
            cmd.split_whitespace().count() >= 2
                && !flag_bundle_has(cmd, 'v')
                && !cmd.contains("--verbose")
        }
        "ls" => !flag_bundle_has(cmd, 'R') && !cmd.contains("--recursive"),
        _ => is_version_probe(cmd) || matches_bounded(cmd),
    }
}

/// Classify a Bash command: destructive root → [`Risk::Block`]; structurally
/// bounded with no shell operators → [`Risk::Safe`]; otherwise [`Risk::Warn`].
pub fn classify(cmd: &str) -> Risk {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Risk::Warn;
    }
    if is_destructive(cmd) {
        return Risk::Block;
    }
    if !has_shell_op(cmd) && is_bounded(cmd) {
        return Risk::Safe;
    }
    Risk::Warn
}

/// The routing skip predicate: output bounded enough that a nudge/wrap is noise.
pub fn is_structurally_bounded(cmd: &str) -> bool {
    classify(cmd) == Risk::Safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_when_examples() {
        assert_eq!(classify("ls -la"), Risk::Safe);
        assert_eq!(classify("rm -rf /"), Risk::Block);
        assert_eq!(classify("git log | head"), Risk::Warn);
    }

    #[test]
    fn safe_commands_are_bounded_via_carveouts_not_membership() {
        // High-output read-only programs are wrap-allowlisted but NOT Safe, so the
        // wrap path still fires for them.
        assert!(is_safe_command("find"));
        assert!(is_safe_command("cat"));
        assert!(is_safe_command("grep"));
        assert_eq!(classify("find ."), Risk::Warn);
        assert_eq!(classify("cat file.txt"), Risk::Warn);
        assert_eq!(classify("grep -r foo src"), Risk::Warn);
    }

    #[test]
    fn bounded_commands_are_safe() {
        for c in [
            "pwd",
            "whoami",
            "git status",
            "git status --short",
            "git rev-parse HEAD",
            "git branch",
            "git diff --stat",
            "git log -5",
            "git stash list",
            "node --version",
            "python3 --version",
            "cargo -V",
            "ls",
            "ls -la",
            "cd /tmp",
            "echo hi",
            "mkdir -p a/b",
            "mv a b",
            "cp a b",
        ] {
            assert_eq!(classify(c), Risk::Safe, "{c:?} should be Safe");
        }
    }

    #[test]
    fn unbounded_commands_are_not_safe() {
        for c in [
            "find .",
            "cat file.txt",
            "grep -r foo",
            "git log",
            "git diff",
            "ls -R",
            "ls --recursive",
            "cp -rv a b",
            "mv --verbose a b",
            "git status | xargs cat",
            "cat huge && echo done",
            "echo $(cat f)",
            "",
            "rg pattern",
        ] {
            assert!(!is_structurally_bounded(c), "{c:?} should NOT be Safe");
        }
    }

    #[test]
    fn destructive_roots_block_unless_dry_run() {
        for c in [
            "rm a",
            "rm -rf /",
            "rm -v x",
            "dd if=/dev/zero of=/dev/sda",
            "mkfs /dev/sda",
            "chmod -R 777 /",
            "chown root x",
            "kill 1",
            "pkill node",
            "truncate -s 0 f",
            "shred f",
            "/bin/rm -rf x",
        ] {
            assert_eq!(classify(c), Risk::Block, "{c:?} should Block");
        }
        // `--dry-run` downgrades the destructive flag.
        assert_ne!(classify("rm --dry-run foo"), Risk::Block);
    }

    #[test]
    fn git_log_dash_needs_a_digit() {
        assert_eq!(classify("git log -5"), Risk::Safe);
        assert_eq!(classify("git log -20 --oneline"), Risk::Safe);
        assert_eq!(classify("git log --oneline"), Risk::Warn);
        assert_eq!(classify("git log"), Risk::Warn);
    }
}
