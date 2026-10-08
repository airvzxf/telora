use log::{info, warn};
use serde::Deserialize;
use std::path::PathBuf;

/// GUI settings from `~/.config/telora/gui.toml`. A missing or invalid
/// file falls back to the defaults.
#[derive(Debug, Clone)]
pub struct GuiConfig {
    /// Show the StatusNotifierItem tray icon.
    pub enable_tray: bool,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self { enable_tray: true }
    }
}

#[derive(Debug, Deserialize, Default)]
struct RawGuiConfig {
    enable_tray: Option<bool>,
}

impl GuiConfig {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            info!("No config path resolved; using built-in defaults");
            return Self::default();
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("Config file {} not found; using defaults", path.display());
                return Self::default();
            }
            Err(e) => {
                warn!("Could not read {} ({e}); using defaults", path.display());
                return Self::default();
            }
        };
        Self::parse(&text).unwrap_or_else(|e| {
            warn!("{} is not valid TOML ({e}); using defaults", path.display());
            Self::default()
        })
    }

    fn parse(text: &str) -> Result<Self, toml::de::Error> {
        let raw: RawGuiConfig = toml::from_str(text)?;
        let mut cfg = Self::default();
        if let Some(enable) = raw.enable_tray {
            cfg.enable_tray = enable;
        }
        Ok(cfg)
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

#[cfg(test)]
mod tests {
    use super::GuiConfig;

    /// gui.toml files written for the removed TYPE mode must keep loading.
    #[test]
    fn legacy_paste_shortcut_keys_are_ignored() {
        let cfg = GuiConfig::parse(
            "paste_shortcut = \"ctrl+v\"\nenable_tray = false\n[paste_shortcut_by_app]\nkitty = \"ctrl+shift+v\"\n",
        )
        .expect("legacy file parses");
        assert!(!cfg.enable_tray);
    }
}
