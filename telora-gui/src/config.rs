use log::{info, warn};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Runtime configuration for the GUI client (`telora-gui`).
///
/// Resolved once at startup from `~/.config/telora/gui.toml`. If the file is
/// missing or malformed the defaults are used; in either case the GUI keeps
/// working with a sensible baseline.
#[derive(Debug, Clone)]
pub struct GuiConfig {
    pub paste_shortcut: String,
    pub paste_shortcut_by_app: HashMap<String, String>,
}

impl Default for GuiConfig {
    fn default() -> Self {
        let mut map = HashMap::new();
        map.insert("Alacritty".to_string(), "shift+insert".to_string());
        map.insert("kitty".to_string(), "ctrl+shift+v".to_string());
        map.insert("foot".to_string(), "ctrl+shift+v".to_string());
        map.insert("wezterm".to_string(), "ctrl+shift+v".to_string());
        map.insert("konsole".to_string(), "ctrl+shift+v".to_string());
        map.insert("org.gnome.Terminal".to_string(), "ctrl+shift+v".to_string());
        map.insert("xfce4-terminal".to_string(), "ctrl+shift+v".to_string());
        Self {
            paste_shortcut: "ctrl+v".to_string(),
            paste_shortcut_by_app: map,
        }
    }
}

/// On-disk representation. The `paste_shortcut_by_app` map can be overridden
/// entirely from the TOML file; defaults are merged at load time.
#[derive(Debug, Deserialize, Default)]
struct RawGuiConfig {
    paste_shortcut: Option<String>,
    paste_shortcut_by_app: Option<HashMap<String, String>>,
}

impl GuiConfig {
    /// Load the user configuration. Never panics: missing files, missing
    /// fields, or parse errors all fall back to [`GuiConfig::default`].
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            info!("No config path resolved; using built-in defaults");
            return Self::default();
        };

        if !path.exists() {
            info!(
                "Config file {} not found; using built-in defaults",
                path.display()
            );
            return Self::default();
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warn!(
                    "Could not read config {} ({}); using defaults",
                    path.display(),
                    e
                );
                return Self::default();
            }
        };

        let raw: RawGuiConfig = match toml::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "Config {} is not valid TOML ({}); using defaults",
                    path.display(),
                    e
                );
                return Self::default();
            }
        };

        let mut cfg = Self::default();

        if let Some(s) = raw.paste_shortcut {
            if !s.trim().is_empty() {
                cfg.paste_shortcut = s;
            } else {
                warn!("paste_shortcut in config is empty; keeping default");
            }
        }

        if let Some(map) = raw.paste_shortcut_by_app {
            for (k, v) in map {
                cfg.paste_shortcut_by_app.insert(k, v);
            }
        }

        info!(
            "Loaded config from {} (default shortcut: {}, {} per-app overrides)",
            path.display(),
            cfg.paste_shortcut,
            cfg.paste_shortcut_by_app.len()
        );

        cfg
    }

    /// Resolve which shortcut to use for the currently focused app.
    pub fn resolve_paste_shortcut(&self, app_id: Option<&str>) -> String {
        if let Some(id) = app_id
            && let Some(s) = self.paste_shortcut_by_app.get(id)
        {
            return s.clone();
        }
        self.paste_shortcut.clone()
    }
}

fn config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("telora").join("gui.toml"));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("telora")
                .join("gui.toml"),
        );
    }
    None
}
