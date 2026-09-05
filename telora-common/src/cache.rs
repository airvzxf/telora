//! Shared Voxora model-cache path resolution.
//!
//! The cache layout keeps the `voxora/models/huggingface` suffix used by
//! Telora releases since 0.1.x. Override paths are checked before they are
//! handed to a downloader so the daemon and the legacy model-management CLI
//! have the same traversal and XDG-boundary policy.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// Default on-disk location for the Voxora Hugging Face model cache.
///
/// A non-empty `VOXORA_CACHE_DIR` override is accepted only when it passes
/// [`sanitize_voxora_cache_override`]. Invalid overrides fall through to the
/// XDG default, matching the daemon's historical behaviour.
pub fn default_voxora_cache_dir() -> Result<PathBuf> {
    if let Some(custom) = std::env::var_os("VOXORA_CACHE_DIR") {
        let custom = PathBuf::from(custom);
        if !is_blank(&custom)
            && let Some(accepted) = sanitize_voxora_cache_override(&custom)
        {
            return Ok(accepted);
        }
    }

    xdg_default_cache_dir()
}

/// Resolve the Voxora model-cache root from CLI, environment, or XDG defaults.
///
/// The CLI override has precedence over the environment override. Both values
/// are represented as paths so non-UTF-8 command-line and environment values
/// are not silently discarded before validation.
pub fn resolve_voxora_cache(
    args_override: Option<&Path>,
    env_override: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(args_override) = args_override.filter(|path| !is_blank(path))
        && let Some(accepted) = sanitize_voxora_cache_override(args_override)
    {
        return Ok(accepted);
    }
    if let Some(env_override) = env_override.filter(|path| !is_blank(path))
        && let Some(accepted) = sanitize_voxora_cache_override(env_override)
    {
        return Ok(accepted);
    }

    xdg_default_cache_dir()
}

fn xdg_default_cache_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot determine a safe voxora cache directory: dirs::cache_dir() returned None; \
             set $VOXORA_CACHE_DIR or $XDG_CACHE_HOME"
        )
    })?;
    Ok(base.join("voxora").join("models").join("huggingface"))
}

/// Validate a cache override and return its accepted path.
///
/// The validator rejects empty values, any `..` component, absolute paths
/// outside the canonical XDG cache root, symlinked prefixes that resolve
/// outside that root, and existing non-directory targets. Relative paths that
/// contain no parent traversal remain supported for backwards compatibility;
/// their location is controlled by the operator's working directory.
pub fn sanitize_voxora_cache_override(candidate: &Path) -> Option<PathBuf> {
    if is_blank(candidate) {
        log::warn!("VOXORA_CACHE_DIR is empty; falling back to the XDG default");
        return None;
    }

    if candidate
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        log::warn!(
            "VOXORA_CACHE_DIR={candidate:?} contains a `..` component; \
             ignoring and falling back to the XDG default"
        );
        return None;
    }

    if candidate.exists() && !candidate.is_dir() {
        log::warn!(
            "VOXORA_CACHE_DIR={candidate:?} exists but is not a directory; \
             ignoring and falling back to the XDG default"
        );
        return None;
    }

    if !candidate.is_absolute() {
        return Some(candidate.to_path_buf());
    }

    let xdg = dirs::cache_dir()?;
    let canonical_xdg = match canonicalize_existing_prefix(&xdg) {
        Ok(path) => path,
        Err(error) => {
            log::warn!(
                "cannot canonicalize the XDG cache directory {xdg:?} ({error}); \
                 ignoring the cache override"
            );
            return None;
        }
    };
    let canonical_candidate = match canonicalize_existing_prefix(candidate) {
        Ok(path) => path,
        Err(error) => {
            log::warn!(
                "cannot canonicalize cache override {candidate:?} ({error}); \
                 ignoring and falling back to the XDG default"
            );
            return None;
        }
    };

    if !canonical_candidate.starts_with(&canonical_xdg) {
        log::warn!(
            "VOXORA_CACHE_DIR={candidate:?} does not live under the XDG cache \
             directory {canonical_xdg:?}; ignoring and falling back to the XDG default"
        );
        return None;
    }

    // Preserve the old observable path for a not-yet-created directory while
    // still comparing its resolved existing prefix above.
    if candidate.exists() {
        Some(canonical_candidate)
    } else {
        Some(candidate.to_path_buf())
    }
}

fn is_blank(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return true;
    }

    path.to_str().is_some_and(|value| value.trim().is_empty())
}

/// Canonicalize the longest existing prefix and append the missing suffix.
///
/// This catches a symlink in an existing parent even when the final cache
/// directory has not been created yet. It also canonicalizes an XDG root that
/// itself is a symlink, avoiding a lexical prefix comparison against the wrong
/// inode tree.
fn canonicalize_existing_prefix(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path;
    let mut suffix: Vec<OsString> = Vec::new();

    loop {
        match std::fs::canonicalize(current) {
            Ok(mut canonical) => {
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = current.file_name().ok_or(error)?;
                suffix.push(name.to_os_string());
                current = current.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "cache path has no existing ancestor",
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct CacheEnv {
        xdg_cache_home: Option<OsString>,
        home: Option<OsString>,
        voxora_cache_dir: Option<OsString>,
    }

    impl CacheEnv {
        fn new(root: &Path) -> Self {
            let previous = Self {
                xdg_cache_home: std::env::var_os("XDG_CACHE_HOME"),
                home: std::env::var_os("HOME"),
                voxora_cache_dir: std::env::var_os("VOXORA_CACHE_DIR"),
            };
            unsafe {
                std::env::set_var("XDG_CACHE_HOME", root);
                std::env::set_var("HOME", root);
                std::env::remove_var("VOXORA_CACHE_DIR");
            }
            previous
        }
    }

    impl Drop for CacheEnv {
        fn drop(&mut self) {
            restore_env("XDG_CACHE_HOME", self.xdg_cache_home.take());
            restore_env("HOME", self.home.take());
            restore_env("VOXORA_CACHE_DIR", self.voxora_cache_dir.take());
        }
    }

    fn restore_env(name: &str, value: Option<OsString>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    fn test_cache_root() -> (TempDir, CacheEnv) {
        let root = tempfile::tempdir().expect("create cache test root");
        let env = CacheEnv::new(root.path());
        (root, env)
    }

    #[test]
    fn default_cache_uses_legacy_suffix() {
        let _lock = ENV_LOCK.lock().unwrap();
        let (_root, _env) = test_cache_root();

        let resolved = default_voxora_cache_dir().expect("cache path should resolve");
        assert!(resolved.ends_with(Path::new("voxora/models/huggingface")));
    }

    #[test]
    fn default_cache_honours_safe_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let (root, _env) = test_cache_root();
        let override_path = root.path().join("custom-cache");
        unsafe {
            std::env::set_var("VOXORA_CACHE_DIR", &override_path);
        }

        let resolved = default_voxora_cache_dir().expect("safe override should resolve");
        assert_eq!(resolved, override_path);
    }

    #[test]
    fn rejects_parent_dir_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let (_root, _env) = test_cache_root();

        assert!(sanitize_voxora_cache_override(Path::new("/tmp/foo/../bar")).is_none());
    }

    #[test]
    fn rejects_absolute_path_outside_xdg() {
        let _lock = ENV_LOCK.lock().unwrap();
        let (_root, _env) = test_cache_root();

        assert!(sanitize_voxora_cache_override(Path::new("/etc")).is_none());
        assert!(sanitize_voxora_cache_override(Path::new("/this/path/does/not/exist")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_prefix_outside_xdg() {
        let _lock = ENV_LOCK.lock().unwrap();
        let (root, _env) = test_cache_root();
        let outside = tempfile::tempdir().expect("create outside root");
        let link = root.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).expect("create escape symlink");

        assert!(
            sanitize_voxora_cache_override(&link.join("new-cache")).is_none(),
            "a missing child below an escaping symlink must be rejected"
        );
    }

    #[test]
    fn accepts_nonexistent_directory_under_xdg() {
        let _lock = ENV_LOCK.lock().unwrap();
        let (root, _env) = test_cache_root();
        let candidate = root.path().join("new/cache");

        assert_eq!(sanitize_voxora_cache_override(&candidate), Some(candidate));
    }

    #[test]
    fn rejects_existing_non_directory() {
        let _lock = ENV_LOCK.lock().unwrap();
        let (root, _env) = test_cache_root();
        let candidate = root.path().join("cache-file");
        std::fs::write(&candidate, b"not a directory").expect("create cache file");

        assert!(sanitize_voxora_cache_override(&candidate).is_none());
    }

    #[test]
    fn accepts_relative_path_without_parent_traversal() {
        let _lock = ENV_LOCK.lock().unwrap();

        assert_eq!(
            sanitize_voxora_cache_override(Path::new("voxora-cache")),
            Some(PathBuf::from("voxora-cache"))
        );
    }

    #[test]
    fn resolves_cli_before_environment() {
        let _lock = ENV_LOCK.lock().unwrap();
        let (root, _env) = test_cache_root();
        let cli = root.path().join("cli-cache");
        let env = root.path().join("env-cache");

        let resolved = resolve_voxora_cache(Some(&cli), Some(&env)).expect("overrides resolve");
        assert_eq!(resolved, cli);
    }

    #[test]
    fn rejects_unsafe_cli_and_environment_overrides() {
        let _lock = ENV_LOCK.lock().unwrap();
        let (root, _env) = test_cache_root();
        let bad_cli = Path::new("/tmp/another-cli-test/cache/../elsewhere");
        let bad_env = Path::new("/tmp/this-is-a-test/cache-root/../../etc");
        let safe_env = root.path().join("env-cache");
        let expected = Path::new("voxora/models/huggingface");

        let cli_result = resolve_voxora_cache(Some(bad_cli), None).expect("default resolves");
        assert!(cli_result.ends_with(expected));
        let env_result = resolve_voxora_cache(None, Some(bad_env)).expect("default resolves");
        assert!(env_result.ends_with(expected));
        let safe_env_result =
            resolve_voxora_cache(Some(bad_cli), Some(&safe_env)).expect("env resolves");
        assert_eq!(safe_env_result, safe_env);
    }

    #[test]
    fn treats_empty_and_whitespace_overrides_as_absent() {
        let _lock = ENV_LOCK.lock().unwrap();
        let (_root, _env) = test_cache_root();
        let empty = Path::new("");
        let whitespace = Path::new("   ");
        let expected = Path::new("voxora/models/huggingface");

        let resolved =
            resolve_voxora_cache(Some(empty), Some(whitespace)).expect("default resolves");
        assert!(resolved.ends_with(expected));
    }
}
