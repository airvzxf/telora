//! The `telora.toml` cascade shared by the daemon, the GUI and the CLI.
//!
//! Later layers win: `/etc/telora.toml`, the user's
//! `$XDG_CONFIG_HOME/telora/config.toml`, an explicit `--config` file and
//! `TELORA_*` environment variables. A file that exists but does not
//! parse, or an explicit file that does not exist, is an error: running
//! on silent defaults made the daemon load a different model than the one
//! configured.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use config::{Config, File};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::env::telora_env_source;
use crate::paths::{PathsConfig, ResolvedPaths};

pub const SYSTEM_CONFIG: &str = "/etc/telora.toml";

/// `$XDG_CONFIG_HOME/telora/config.toml`, with the XDG default of
/// `~/.config` when the variable is unset, empty or relative.
pub fn user_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("telora").join("config.toml"))
}

/// The files of one cascade; [`Cascade::standard`] is the one every
/// binary uses, the fields exist so tests can point elsewhere.
#[derive(Debug, Clone)]
pub struct Cascade {
    pub system: PathBuf,
    pub user: Option<PathBuf>,
    pub explicit: Option<PathBuf>,
}

impl Cascade {
    pub fn standard(explicit: Option<&Path>) -> Self {
        Self {
            system: PathBuf::from(SYSTEM_CONFIG),
            user: user_config_path(),
            explicit: explicit.map(Path::to_path_buf),
        }
    }

    /// Merge every layer and deserialize the result.
    ///
    /// # Errors
    ///
    /// When the explicit file does not exist, a file does not parse, or
    /// the merged values do not fit `T`. The message names the file and,
    /// for syntax errors, the line and column.
    pub fn load<T: DeserializeOwned>(&self) -> Result<T> {
        let mut builder =
            Config::builder().add_source(File::from(self.system.as_path()).required(false));
        if let Some(user) = &self.user {
            builder = builder.add_source(File::from(user.as_path()).required(false));
        }
        if let Some(explicit) = &self.explicit {
            if !explicit.is_file() {
                bail!(
                    "el archivo de configuración {} no existe",
                    explicit.display()
                );
            }
            builder = builder.add_source(File::from(explicit.as_path()));
        }
        builder
            .add_source(telora_env_source())
            .build()
            .and_then(Config::try_deserialize)
            .context("configuración inválida")
    }
}

#[derive(Deserialize)]
struct PathsOnly {
    #[serde(default)]
    paths: PathsConfig,
}

/// The `[paths]` section of the standard cascade.
///
/// # Errors
///
/// See [`Cascade::load`].
pub fn load_paths_config() -> Result<PathsConfig> {
    load_paths_config_from(&Cascade::standard(None))
}

/// Socket paths from the standard cascade's `[paths]` section.
///
/// # Errors
///
/// See [`Cascade::load`] and [`crate::paths::resolve`].
pub fn resolve_paths() -> Result<ResolvedPaths> {
    crate::paths::resolve(&load_paths_config()?)
}

/// [`load_paths_config`] over an arbitrary cascade.
///
/// # Errors
///
/// See [`Cascade::load`].
pub fn load_paths_config_from(cascade: &Cascade) -> Result<PathsConfig> {
    Ok(cascade.load::<PathsOnly>()?.paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::tests::ENV_LOCK;

    fn cascade(dir: &Path, user: &str) -> Cascade {
        let user_path = dir.join("config.toml");
        std::fs::write(&user_path, user).unwrap();
        Cascade {
            system: dir.join("no-system.toml"),
            user: Some(user_path),
            explicit: None,
        }
    }

    #[derive(Debug, Deserialize)]
    struct Sample {
        #[serde(default)]
        language: String,
    }

    #[test]
    fn a_syntax_error_names_the_file_and_line() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let c = cascade(dir.path(), "language = \"es\"\nmodel_id = \"sin cerrar\n");
        let err = format!("{:#}", c.load::<Sample>().unwrap_err());
        assert!(err.contains("config.toml"), "{err}");
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn a_missing_explicit_file_is_an_error() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut c = cascade(dir.path(), "language = \"es\"\n");
        c.explicit = Some(dir.path().join("no-existe.toml"));
        let err = format!("{:#}", c.load::<Sample>().unwrap_err());
        assert!(err.contains("no-existe.toml"), "{err}");
        assert!(err.contains("no existe"), "{err}");
    }

    #[test]
    fn missing_optional_files_are_fine() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let c = Cascade {
            system: dir.path().join("a.toml"),
            user: Some(dir.path().join("b.toml")),
            explicit: None,
        };
        assert_eq!(c.load::<Sample>().unwrap().language, "");
    }

    #[test]
    fn explicit_file_overrides_user_file() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut c = cascade(dir.path(), "language = \"es\"\n");
        let explicit = dir.path().join("otro.toml");
        std::fs::write(&explicit, "language = \"en\"\n").unwrap();
        c.explicit = Some(explicit);
        assert_eq!(c.load::<Sample>().unwrap().language, "en");
    }

    #[test]
    fn paths_section_is_read_by_every_binary() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let c = cascade(dir.path(), "[paths]\nsocket_dir = \"/run/user/1000/x\"\n");
        let paths = load_paths_config_from(&c).unwrap();
        assert_eq!(paths.socket_dir.as_deref(), Some("/run/user/1000/x"));
    }

    #[test]
    fn user_file_overrides_system_file() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut c = cascade(dir.path(), "language = \"en\"\n");
        std::fs::write(
            &c.system,
            "language = \"fr\"\n[paths]\nsocket_dir = \"/s\"\n",
        )
        .unwrap();
        assert_eq!(c.load::<Sample>().unwrap().language, "en");
        c.user = None;
        assert_eq!(c.load::<Sample>().unwrap().language, "fr");
        assert_eq!(
            load_paths_config_from(&c).unwrap().socket_dir.as_deref(),
            Some("/s")
        );
    }

    #[test]
    fn environment_overrides_files() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let c = cascade(dir.path(), "[paths]\nsocket_dir = \"/desde-archivo\"\n");
        let prev = std::env::var_os("TELORA_PATHS__SOCKET_DIR");
        // SAFETY: ENV_LOCK serialises every test that touches the env.
        unsafe { std::env::set_var("TELORA_PATHS__SOCKET_DIR", "/desde-env") };
        let paths = load_paths_config_from(&c);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("TELORA_PATHS__SOCKET_DIR", v),
                None => std::env::remove_var("TELORA_PATHS__SOCKET_DIR"),
            }
        }
        assert_eq!(paths.unwrap().socket_dir.as_deref(), Some("/desde-env"));
    }

    #[test]
    fn user_config_follows_xdg_config_home() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        // SAFETY: ENV_LOCK serialises every test that touches these.
        unsafe {
            std::env::set_var("HOME", "/home/alguien");
            std::env::set_var("XDG_CONFIG_HOME", "/xdg");
        }
        let with_xdg = user_config_path();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "relativo") };
        let relative_xdg = user_config_path();
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        let without_xdg = user_config_path();
        unsafe {
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        assert_eq!(with_xdg, Some(PathBuf::from("/xdg/telora/config.toml")));
        let home_default = Some(PathBuf::from("/home/alguien/.config/telora/config.toml"));
        assert_eq!(relative_xdg, home_default);
        assert_eq!(without_xdg, home_default);
    }
}
