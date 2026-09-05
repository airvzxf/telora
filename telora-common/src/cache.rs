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
/// [`sanitize_voxora_cache_override`]. Invalid environment overrides fall
/// through to the XDG default. Use [`resolve_voxora_cache`] when both CLI and
/// environment sources need to be considered.
///
/// # Errors
///
/// Returns an error when the process has no usable XDG cache directory.
pub fn default_voxora_cache_dir() -> Result<PathBuf> {
    if let Some(custom) = std::env::var_os("VOXORA_CACHE_DIR") {
        let custom = PathBuf::from(custom);
        if !is_blank(&custom) {
            if let Some(accepted) = sanitize_with_source(&custom, "VOXORA_CACHE_DIR") {
                return Ok(accepted);
            }
        }
    }

    xdg_default_cache_dir()
}

/// Resolve the Voxora model-cache root from CLI, environment, or XDG defaults.
///
/// The CLI override has precedence over the environment override. Both values
/// are represented as paths so non-UTF-8 command-line and environment values
/// are not silently discarded before validation. A rejected CLI override
/// falls directly to the XDG default; it never silently selects the lower-
/// priority environment value. A rejected environment override also falls to
/// the XDG default.
///
/// # Errors
///
/// Returns an error when no usable XDG cache directory can be determined.
pub fn resolve_voxora_cache(
    args_override: Option<&Path>,
    env_override: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(args_override) = args_override {
        if is_blank(args_override) {
            return xdg_default_cache_dir();
        }
        if let Some(accepted) = sanitize_with_source(args_override, "--voxora-cache") {
            return Ok(accepted);
        }
        return xdg_default_cache_dir();
    }

    if let Some(env_override) = env_override {
        if !is_blank(env_override) {
            if let Some(accepted) = sanitize_with_source(env_override, "VOXORA_CACHE_DIR") {
                return Ok(accepted);
            }
        }
    }

    xdg_default_cache_dir()
}

fn xdg_default_cache_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot determine a safe voxora cache directory: dirs::cache_dir() returned None; \
             set $XDG_CACHE_HOME to validate absolute overrides or use a relative \
             cache path"
        )
    })?;
    Ok(base.join("voxora").join("models").join("huggingface"))
}

/// Validate a cache override and return its accepted path.
///
/// The validator rejects empty values, any `..` component, whitespace-padded
/// values, absolute paths outside the canonical XDG cache root, dangling or
/// escaping symlink prefixes, and existing non-directory targets. Relative
/// paths that contain no parent traversal remain supported for backwards
/// compatibility; they intentionally bypass the XDG and symlink-boundary
/// checks and are resolved relative to the operator's working directory.
pub fn sanitize_voxora_cache_override(candidate: &Path) -> Option<PathBuf> {
    sanitize_with_source(candidate, "cache override")
}

fn sanitize_with_source(candidate: &Path, source: &str) -> Option<PathBuf> {
    if is_blank(candidate) {
        log::warn!("{source} is empty; falling back to the XDG default");
        return None;
    }

    let lossy_candidate = candidate.to_string_lossy();
    if lossy_candidate != lossy_candidate.trim() {
        log::warn!(
            "{source}={candidate:?} has leading or trailing whitespace; \
             ignoring and falling back to the XDG default"
        );
        return None;
    }

    if candidate
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        log::warn!(
            "{source}={candidate:?} contains a `..` component; \
             ignoring and falling back to the XDG default"
        );
        return None;
    }

    if candidate.exists() && !candidate.is_dir() {
        log::warn!(
            "{source}={candidate:?} exists but is not a directory; \
             ignoring and falling back to the XDG default"
        );
        return None;
    }

    if !candidate.is_absolute() {
        return Some(candidate.to_path_buf());
    }

    if has_dangling_symlink_prefix(candidate) {
        log::warn!(
            "{source}={candidate:?} contains a symlink with an unresolved target; \
             ignoring and falling back to the XDG default"
        );
        return None;
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
            "{source}={candidate:?} does not live under the XDG cache directory \
             {canonical_xdg:?}; ignoring and falling back to the XDG default"
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

    path.to_string_lossy().trim().is_empty()
}

fn has_dangling_symlink_prefix(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if std::fs::canonicalize(&current).is_err() {
                    return true;
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                log::warn!(
                    "cannot inspect cache override prefix {current:?} ({error}); \
                     rejecting the override"
                );
                return true;
            }
        }
    }
    false
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
    #[cfg(unix)]
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

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
            // SAFETY: cache tests serialize process-wide environment changes
            // with `ENV_LOCK` and restore each previous value on drop.
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
        // SAFETY: the caller holds `ENV_LOCK`; restoration cannot race another
        // cache test's environment mutation.
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
        let _lock = lock_env();
        let (root, _env) = test_cache_root();

        let resolved = default_voxora_cache_dir().expect("cache path should resolve");
        assert!(resolved.starts_with(root.path()));
        assert!(resolved.ends_with(Path::new("voxora/models/huggingface")));
    }

    #[test]
    fn default_cache_honours_safe_override() {
        let _lock = lock_env();
        let (root, _env) = test_cache_root();
        let override_path = root.path().join("custom-cache");
        // SAFETY: the test holds `ENV_LOCK` and `CacheEnv` restores the value.
        unsafe {
            std::env::set_var("VOXORA_CACHE_DIR", &override_path);
        }

        let resolved = default_voxora_cache_dir().expect("safe override should resolve");
        assert_eq!(resolved, override_path);
    }

    #[test]
    fn rejects_parent_dir_override() {
        let _lock = lock_env();
        let (_root, _env) = test_cache_root();

        assert!(sanitize_voxora_cache_override(Path::new("/tmp/foo/../bar")).is_none());
    }

    #[test]
    fn rejects_absolute_path_outside_xdg() {
        let _lock = lock_env();
        let (_root, _env) = test_cache_root();

        assert!(sanitize_voxora_cache_override(Path::new("/etc")).is_none());
        assert!(sanitize_voxora_cache_override(Path::new("/this/path/does/not/exist")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_prefix_outside_xdg() {
        let _lock = lock_env();
        let (root, _env) = test_cache_root();
        let outside = tempfile::tempdir().expect("create outside root");
        let link = root.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).expect("create escape symlink");

        assert!(
            sanitize_voxora_cache_override(&link.join("new-cache")).is_none(),
            "a missing child below an escaping symlink must be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlinked_prefix() {
        let _lock = lock_env();
        let (root, _env) = test_cache_root();
        let outside = tempfile::tempdir().expect("create outside root");
        let missing_target = outside.path().join("not-created");
        let link = root.path().join("dangling");
        std::os::unix::fs::symlink(&missing_target, &link).expect("create dangling symlink");

        assert!(
            sanitize_voxora_cache_override(&link.join("new-cache")).is_none(),
            "a dangling symlink prefix must not be treated as an ordinary missing directory"
        );
    }

    #[test]
    fn accepts_nonexistent_directory_under_xdg() {
        let _lock = lock_env();
        let (root, _env) = test_cache_root();
        let candidate = root.path().join("new/cache");

        assert_eq!(sanitize_voxora_cache_override(&candidate), Some(candidate));
    }

    #[test]
    fn rejects_existing_non_directory() {
        let _lock = lock_env();
        let (root, _env) = test_cache_root();
        let candidate = root.path().join("cache-file");
        std::fs::write(&candidate, b"not a directory").expect("create cache file");

        assert!(sanitize_voxora_cache_override(&candidate).is_none());
    }

    #[test]
    fn accepts_relative_path_without_parent_traversal() {
        let _lock = lock_env();

        assert_eq!(
            sanitize_voxora_cache_override(Path::new("voxora-cache")),
            Some(PathBuf::from("voxora-cache"))
        );
    }

    #[test]
    fn resolves_cli_before_environment() {
        let _lock = lock_env();
        let (root, _env) = test_cache_root();
        let cli = root.path().join("cli-cache");
        let env = root.path().join("env-cache");

        let resolved = resolve_voxora_cache(Some(&cli), Some(&env)).expect("overrides resolve");
        assert_eq!(resolved, cli);
    }

    #[test]
    fn rejects_unsafe_cli_and_environment_overrides() {
        let _lock = lock_env();
        let (_root, _env) = test_cache_root();
        let bad_cli = Path::new("/tmp/another-cli-test/cache/../elsewhere");
        let bad_env = Path::new("/tmp/this-is-a-test/cache-root/../../etc");
        let expected = Path::new("voxora/models/huggingface");

        let cli_result = resolve_voxora_cache(Some(bad_cli), None).expect("default resolves");
        assert!(cli_result.ends_with(expected));
        let env_result = resolve_voxora_cache(None, Some(bad_env)).expect("default resolves");
        assert!(env_result.ends_with(expected));
    }

    #[test]
    fn invalid_cli_does_not_fall_through_to_environment_override() {
        let _lock = lock_env();
        let (root, _env) = test_cache_root();
        let bad_cli = Path::new("/tmp/another-cli-test/cache/../elsewhere");
        let blank_cli = Path::new("\t");
        let safe_env = root.path().join("env-cache");

        let resolved =
            resolve_voxora_cache(Some(bad_cli), Some(&safe_env)).expect("default resolves");
        assert!(resolved.ends_with(Path::new("voxora/models/huggingface")));
        assert_ne!(resolved, safe_env);
        let blank_resolved =
            resolve_voxora_cache(Some(blank_cli), Some(&safe_env)).expect("default resolves");
        assert!(blank_resolved.ends_with(Path::new("voxora/models/huggingface")));
        assert_ne!(blank_resolved, safe_env);
    }

    #[test]
    fn treats_empty_and_whitespace_overrides_as_absent() {
        let _lock = lock_env();
        let (_root, _env) = test_cache_root();
        let empty = Path::new("");
        let whitespace = Path::new("   ");
        let padded = Path::new("/tmp/escape ");
        let expected = Path::new("voxora/models/huggingface");

        let resolved =
            resolve_voxora_cache(Some(empty), Some(whitespace)).expect("default resolves");
        assert!(resolved.ends_with(expected));
        let padded_resolved = resolve_voxora_cache(None, Some(padded)).expect("default resolves");
        assert!(padded_resolved.ends_with(expected));
    }

    #[test]
    fn default_cache_rejects_padded_environment_override() {
        let _lock = lock_env();
        let (_root, _env) = test_cache_root();
        // SAFETY: the test holds `ENV_LOCK` and `CacheEnv` restores the value.
        unsafe {
            std::env::set_var("VOXORA_CACHE_DIR", "/tmp/escape ");
        }

        let resolved = default_voxora_cache_dir().expect("default resolves");
        assert!(resolved.ends_with(Path::new("voxora/models/huggingface")));
    }

    #[test]
    fn rejects_whitespace_padded_and_relative_traversal_values() {
        assert!(sanitize_voxora_cache_override(Path::new(" /tmp/escape")).is_none());
        assert!(sanitize_voxora_cache_override(Path::new("/tmp/escape ")).is_none());
        assert!(sanitize_voxora_cache_override(Path::new("relative/../escape")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_path_with_trailing_whitespace() {
        let _lock = lock_env();
        let (root, _env) = test_cache_root();
        let mut bytes = root.path().as_os_str().as_bytes().to_vec();
        bytes.extend_from_slice(b"/cache");
        bytes.push(0xff);
        bytes.push(b' ');
        let candidate = PathBuf::from(OsString::from_vec(bytes));

        assert!(sanitize_voxora_cache_override(&candidate).is_none());
    }
}
