//! API-key file loading.

use std::path::{Path, PathBuf};

use crate::error::LlmError;

/// Expand a leading `~` in a path using `$HOME`. A path without a
/// leading tilde is returned unchanged. Still used by the `kres test`
/// and `kres turn` subcommands (which take a key-file CLI argument);
/// agent configs now carry the key inline as the `key` field and do
/// not go through this helper.
pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    p.to_path_buf()
}

/// Read an API key from disk, trim whitespace, and reject empty files.
/// Tilde-expands the path first so configs can use `~/...`.
pub fn load_api_key(path: &Path) -> Result<String, LlmError> {
    let expanded = expand_tilde(path);
    let raw = std::fs::read_to_string(&expanded)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(LlmError::EmptyKey {
            path: expanded.display().to_string(),
        });
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(contents: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("kres-llm-test-{}.key", nonce()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        p
    }

    fn nonce() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn loads_and_trims() {
        let p = write_tmp("  sk-abc123\n  ");
        let k = load_api_key(&p).unwrap();
        assert_eq!(k, "sk-abc123");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn empty_file_rejected() {
        let p = write_tmp("   \n  \t  ");
        let e = load_api_key(&p).unwrap_err();
        assert!(matches!(e, LlmError::EmptyKey { .. }));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn missing_file_returns_io_error() {
        let p = std::path::PathBuf::from("/nonexistent/kres-test/does-not-exist");
        let e = load_api_key(&p).unwrap_err();
        assert!(matches!(e, LlmError::Io(_)));
    }

    #[test]
    fn expand_tilde_expands_home_prefix() {
        let home = dirs::home_dir().unwrap();
        let e = expand_tilde(std::path::Path::new("~/foo.key"));
        assert_eq!(e, home.join("foo.key"));
    }

    #[test]
    fn expand_tilde_passes_through_absolute() {
        let abs = std::path::PathBuf::from("/etc/passwd");
        assert_eq!(expand_tilde(&abs), abs);
    }

    #[test]
    fn expand_tilde_bare_tilde_maps_to_home() {
        let home = dirs::home_dir().unwrap();
        let e = expand_tilde(std::path::Path::new("~"));
        assert_eq!(e, home);
    }
}
