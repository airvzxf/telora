//! Voxora model-cache path resolution for `telora-daemon`.
//!
//! Daemon-only — `telora-gui` and `telora-ctl` never touch the model
//! cache, so the helper stayed out of `telora-common`. `telora-models`
//! ships its own mirror; the divergence there is tracked separately
//! (sanitiser drift).
//!
//! Honours the cascade (first match wins):
//!
//!   1. `$VOXORA_CACHE_DIR` — explicit override, no suffix appended.
//!      The override is sanitised: paths containing `..` components
//!      are rejected, and absolute paths must live under
//!      [`dirs::cache_dir`] (canonicalised comparison). Anything
//!      else is logged and the cascade falls through to the XDG
//!      default.
//!   2. `$XDG_CACHE_HOME/voxora/models/huggingface`. If
//!      `dirs::cache_dir()` returns `None` (no `HOME`,
//!      no `XDG_CACHE_HOME`, etc.), the helper returns
//!      [`anyhow::Error`] instead of silently falling back to a
//!      relative `.cache` — the daemon has nowhere to land and we
//!      would rather fail loudly than route model downloads to
//!      `/.cache/voxora/...`.
//!
//! The `models/huggingface` suffix is **load-bearing** for
//! backwards compatibility with every on-disk cache telora has
//! shipped since 0.1.x. voxora-hf 0.4's default-features change
//! enabled `voxora-config`, whose `cache_root()` returns just
//! `$XDG_CACHE_HOME/voxora` — passing that to
//! `HuggingFaceSource::cache_dir(...)` orphans every existing
//! cached model and forces a 3 GB re-download. The fix lives in
//! `main.rs`: it ALWAYS passes an explicit `cache_dir` here, with
//! this function as the fallback when neither `--voxora-cache`
//! nor `$VOXORA_CACHE_DIR` is set.
//!
//! Mirrors `telora_models::default_cache_dir` (private); the
//! duplication is deliberate and noted in the plan
//! (`adopt voxora 0.4` / `RATIONALE §1`). Do NOT switch to
//! `voxora-config` here.

use anyhow::Result;
use config::Environment;
use std::path::{Path, PathBuf};

/// Default on-disk location for the voxora Hugging Face model cache.
pub fn default_voxora_cache_dir() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("VOXORA_CACHE_DIR")
        && !custom.is_empty()
        && let Some(accepted) = sanitize_voxora_cache_override(Path::new(&custom))
    {
        return Ok(accepted);
    }
    let base = dirs::cache_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot determine a safe voxora cache directory: dirs::cache_dir() returned None; \
             set $VOXORA_CACHE_DIR or $XDG_CACHE_HOME"
        )
    })?;
    Ok(base.join("voxora").join("models").join("huggingface"))
}

/// Resolve the voxora model-cache root, mirroring the daemon's
/// production wiring (`main.rs::main`). Honours the override
/// sources in priority order:
///
///   1. `args_override` — `--voxora-cache` from the CLI.
///   2. `env_override` — `$VOXORA_CACHE_DIR` from the environment.
///   3. The XDG default from [`default_voxora_cache_dir`].
///
/// BOTH override sources flow through
/// [`sanitize_voxora_cache_override`]. Empty / whitespace-only
/// inputs are filtered out so `--voxora-cache ""` and
/// `VOXORA_CACHE_DIR=""` both fall through cleanly (asymmetric
/// handling here would silently point the cache at the CWD's
/// `.cache`, per airvzxf/telora#79). When the sanitiser rejects a
/// candidate (because it contains a `..` component or otherwise
/// escapes the XDG cache root), this helper falls through to the
/// XDG default rather than honouring the unsafe path — same
/// security posture as [`default_voxora_cache_dir`].
pub fn resolve_voxora_cache(
    args_override: Option<&str>,
    env_override: Option<&str>,
) -> Result<PathBuf> {
    let candidate = args_override
        .filter(|s| !s.is_empty())
        .or_else(|| env_override.filter(|s| !s.is_empty()));
    if let Some(s) = candidate
        && let Some(accepted) = sanitize_voxora_cache_override(Path::new(s))
    {
        return Ok(accepted);
    }
    default_voxora_cache_dir()
}

/// Defensive sanitiser for `VOXORA_CACHE_DIR`. Returns `Some(path)`
/// if the override is acceptable, `None` (after a `log::warn!`) if
/// the caller should fall through to the XDG default.
///
/// Rejects:
///   * any `..` path component (an attacker who controls the env
///     var could otherwise redirect the cache to a system path);
///   * absolute paths that do not live under `dirs::cache_dir()`,
///     comparing canonical forms when the path exists on disk and
///     the raw path otherwise.
///
/// Relative paths with no `..` components are accepted — the
/// operator is responsible for where they resolve to.
pub fn sanitize_voxora_cache_override(candidate: &Path) -> Option<PathBuf> {
    if candidate
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        log::warn!(
            "VOXORA_CACHE_DIR={candidate:?} contains a `..` component; \
             ignoring and falling back to the XDG default"
        );
        return None;
    }
    let xdg = dirs::cache_dir()?;
    let accepted = if candidate.exists() {
        match std::fs::canonicalize(candidate) {
            Ok(c) => c,
            Err(e) => {
                log::warn!(
                    "VOXORA_CACHE_DIR={candidate:?} canonicalize failed ({e}); \
                     ignoring and falling back to the XDG default"
                );
                return None;
            }
        }
    } else {
        candidate.to_path_buf()
    };
    if accepted.is_absolute() && !accepted.starts_with(&xdg) {
        log::warn!(
            "VOXORA_CACHE_DIR={candidate:?} does not live under the XDG cache \
             directory {xdg:?}; ignoring and falling back to the XDG default"
        );
        return None;
    }
    Some(accepted)
}

/// Build the `TELORA_*` environment-variable source for the config
/// cascade.
///
/// `config` 0.13's `Environment::with_prefix("TELORA")` defaults its
/// key separator to `""` (no splitting) and its prefix separator to
/// `"_"`. With those defaults `TELORA_PATHS__SOCKET_DIR` is
/// registered as a single flat key `paths__socket_dir` (with two
/// underscores as part of the name) and is silently dropped during
/// deserialisation — no field in [`crate::socket::DaemonConfig`]
/// matches. The fix sets both separators explicitly:
///
///   * `.prefix_separator("_")` keeps the `TELORA_` prefix matching
///     (without it, `config` auto-derives the prefix separator from
///     the key separator and the `TELORA_` prefix no longer matches
///     once `.separator("__")` is set).
///   * `.separator("__")` makes the env parser treat any remaining
///     double-underscore as a path separator, turning
///     `paths__socket_dir` into the nested key `paths.socket_dir`
///     that the struct expects.
#[allow(dead_code)]
pub fn telora_env_source() -> Environment {
    Environment::with_prefix("TELORA")
        .prefix_separator("_")
        .separator("__")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // XDG / `VOXORA_CACHE_DIR` are process-global; the same lock
    // used in `telora-common::paths::tests::ENV_LOCK` serialises
    // them, but a dedicated local lock keeps this module's tests
    // self-contained if `telora-common`'s tests ever change their
    // serialisation policy.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn xdg_cache_root() -> PathBuf {
        dirs::cache_dir().expect("dirs::cache_dir() must be Some in the test environment")
    }

    #[test]
    fn default_voxora_cache_dir_uses_voxora_models_huggingface_suffix() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("VOXORA_CACHE_DIR");
        }

        let resolved = default_voxora_cache_dir().expect("XDG path must resolve");

        let expected_suffix = Path::new("voxora").join("models").join("huggingface");
        assert!(
            resolved.ends_with(&expected_suffix),
            "default voxora cache must end with `voxora/models/huggingface`, got: {}",
            resolved.display()
        );
        let xdg = xdg_cache_root();
        assert!(
            resolved.starts_with(&xdg),
            "default voxora cache must live under the XDG cache root {xdg:?}, got: {}",
            resolved.display()
        );
    }

    #[test]
    fn default_voxora_cache_dir_honours_voxora_cache_dir_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let override_path = xdg_cache_root().join("telora-voxora-cache-override");
        unsafe {
            std::env::set_var("VOXORA_CACHE_DIR", &override_path);
        }

        let resolved = default_voxora_cache_dir().expect("override path must resolve");
        assert_eq!(resolved, override_path);

        unsafe {
            std::env::remove_var("VOXORA_CACHE_DIR");
        }
    }

    #[test]
    fn default_voxora_cache_dir_rejects_parent_dir_segments() {
        let _guard = ENV_LOCK.lock().unwrap();
        let bad = PathBuf::from("/tmp/foo/../bar");
        unsafe {
            std::env::set_var("VOXORA_CACHE_DIR", &bad);
        }

        let resolved = default_voxora_cache_dir().expect("XDG path must resolve");

        assert_ne!(
            resolved,
            PathBuf::from("/tmp/bar"),
            "sanitiser must not honour a VOXORA_CACHE_DIR with a `..` component"
        );
        let expected_suffix = Path::new("voxora").join("models").join("huggingface");
        assert!(
            resolved.ends_with(&expected_suffix),
            "fallback path must be the XDG default, got: {}",
            resolved.display()
        );

        unsafe {
            std::env::remove_var("VOXORA_CACHE_DIR");
        }
    }

    #[test]
    fn resolve_voxora_cache_rejects_parent_dir_via_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("VOXORA_CACHE_DIR");
        }

        let bad = "/tmp/this-is-a-test/cache-root/../../etc";
        let resolved = resolve_voxora_cache(None, Some(bad)).expect("XDG path must resolve");

        assert_ne!(
            resolved,
            PathBuf::from("/tmp/etc"),
            "resolve_voxora_cache must not honour an env override with a `..` component; got: {}",
            resolved.display()
        );
        let expected_suffix = Path::new("voxora").join("models").join("huggingface");
        assert!(
            resolved.ends_with(&expected_suffix),
            "fallback path must be the XDG default, got: {}",
            resolved.display()
        );

        unsafe {
            std::env::remove_var("VOXORA_CACHE_DIR");
        }
    }

    #[test]
    fn resolve_voxora_cache_rejects_parent_dir_via_cli_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let bad = "/tmp/another-cli-test/cache/../elsewhere";
        let resolved = resolve_voxora_cache(Some(bad), None).expect("XDG path must resolve");

        assert_ne!(
            resolved,
            PathBuf::from("/tmp/another-cli-test/elsewhere"),
            "resolve_voxora_cache must not honour a CLI override with a `..` component; got: {}",
            resolved.display()
        );
        let expected_suffix = Path::new("voxora").join("models").join("huggingface");
        assert!(
            resolved.ends_with(&expected_suffix),
            "fallback path must be the XDG default, got: {}",
            resolved.display()
        );
    }

    #[test]
    fn resolve_voxora_cache_cli_override_wins_over_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let cli = xdg_cache_root().join("telora-voxora-cache-cli-wins");
        let env = "/tmp/foo/../bar";

        let resolved = resolve_voxora_cache(Some(cli.to_str().unwrap()), Some(env))
            .expect("override must resolve");

        assert_eq!(resolved, cli);
        assert_ne!(resolved, PathBuf::from("/tmp/bar"));
    }

    #[test]
    fn resolve_voxora_cache_treats_empty_inputs_as_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("VOXORA_CACHE_DIR");
        }

        let resolved = resolve_voxora_cache(Some(""), Some("")).expect("XDG path must resolve");

        let expected_suffix = Path::new("voxora").join("models").join("huggingface");
        assert!(
            resolved.ends_with(&expected_suffix),
            "empty inputs must fall through to the XDG default, got: {}",
            resolved.display()
        );
    }
}
