//! Host/client detection: `Claude` (default, full backward compat) vs `Opencode`.
//!
//! `LENS_HOST=opencode` or `--client opencode` (scanned early) select the host.
//! All path decisions and client-specific behavior should go through this.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Host {
    Claude,
    Opencode,
}

/// Current host. Defaults to `Claude` when no override present.
pub fn detect_host() -> Host {
    from_env().unwrap_or(Host::Claude)
}

/// From `LENS_HOST` (preferred) or `--client` arg scan. `None` if unset or unknown.
pub fn from_env() -> Option<Host> {
    if let Ok(v) = std::env::var("LENS_HOST") {
        if !v.trim().is_empty() {
            if let Some(h) = parse_host(&v) {
                return Some(h);
            }
        }
    }
    from_cli_arg()
}

fn parse_host(s: &str) -> Option<Host> {
    match s.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(Host::Claude),
        "opencode" | "open-code" | "grok" => Some(Host::Opencode),
        _ => None,
    }
}

fn from_cli_arg() -> Option<Host> {
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        if a == "--client" {
            if let Some(v) = iter.next() {
                return parse_host(&v);
            }
            break;
        }
    }
    None
}

/// Config dir for the given host (honors the host-specific env override first).
pub fn config_dir_for(host: Host) -> Option<PathBuf> {
    match host {
        Host::Claude => claude_config_dir(),
        Host::Opencode => opencode_config_dir(),
    }
}

fn claude_config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        if !d.is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    home_dir().map(|h| h.join(".claude"))
}

fn opencode_config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("OPENCODE_CONFIG_DIR") {
        if !d.is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        if !d.is_empty() {
            return Some(PathBuf::from(d).join("opencode"));
        }
    }
    home_dir().map(|h| h.join(".config").join("opencode"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// True when `detect_host() == Host::Claude`.
pub fn is_claude() -> bool {
    detect_host() == Host::Claude
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_claude() {
        let _g = crate::rtk::env_test_lock();
        std::env::remove_var("LENS_HOST");
        assert_eq!(detect_host(), Host::Claude);
    }

    #[test]
    fn from_env_lens_host_opencode() {
        let _g = crate::rtk::env_test_lock();
        std::env::set_var("LENS_HOST", "opencode");
        assert_eq!(detect_host(), Host::Opencode);
        std::env::remove_var("LENS_HOST");
        assert_eq!(detect_host(), Host::Claude);
    }

    #[test]
    fn from_env_lens_host_claude() {
        let _g = crate::rtk::env_test_lock();
        std::env::set_var("LENS_HOST", "claude");
        assert_eq!(detect_host(), Host::Claude);
        std::env::remove_var("LENS_HOST");
    }

    #[test]
    fn config_dir_for_claude_uses_claude_env() {
        let _g = crate::rtk::env_test_lock();
        std::env::set_var("CLAUDE_CONFIG_DIR", "/tmp/claude-test-cfg");
        assert_eq!(
            config_dir_for(Host::Claude),
            Some(PathBuf::from("/tmp/claude-test-cfg"))
        );
        std::env::remove_var("CLAUDE_CONFIG_DIR");
    }

    #[test]
    fn config_dir_for_opencode_uses_opencode_env() {
        let _g = crate::rtk::env_test_lock();
        std::env::set_var("OPENCODE_CONFIG_DIR", "/tmp/opencode-test-cfg");
        assert_eq!(
            config_dir_for(Host::Opencode),
            Some(PathBuf::from("/tmp/opencode-test-cfg"))
        );
        std::env::remove_var("OPENCODE_CONFIG_DIR");
    }

    #[test]
    fn is_claude_matches_detect() {
        let _g = crate::rtk::env_test_lock();
        std::env::set_var("LENS_HOST", "claude");
        assert!(is_claude());
        std::env::set_var("LENS_HOST", "opencode");
        assert!(!is_claude());
        std::env::remove_var("LENS_HOST");
    }

    #[test]
    fn host_opencode_variants_compile_and_exercised() {
        let h: Host = Host::Opencode;
        assert_eq!(h, Host::Opencode);
        assert_ne!(h, Host::Claude);
        let _ = config_dir_for(h);
        let _ = format!("{:?}", h);
        // ensure detect can return it
        let _g = crate::rtk::env_test_lock();
        std::env::set_var("LENS_HOST", "opencode");
        assert_eq!(detect_host(), Host::Opencode);
        std::env::remove_var("LENS_HOST");
    }
}
