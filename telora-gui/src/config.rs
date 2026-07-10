use log::{info, warn};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Backend used to back up and restore the Wayland clipboard around a
/// `toggle-type` paste.
///
/// `wl-copy` is the default — robust on every Wayland compositor because the
/// CLI tool's own backing protocol is decoupled from the application's IPC
/// model; only one MIME type is preserved per cycle.
///
/// `wl-clipboard-rs` preserves every MIME type the source advertised
/// (`text/html`, `text/plain`, `image/png`, …) by talking to
/// `wlr-data-control` / `ext-data-control` directly. It is currently
/// experimental: some compositor / Wayland-backend combinations cause a
/// pipe read inside `paste::get_contents` to block indefinitely. If you see
/// the OSD stuck at "Procesando..." during a paste cycle, switch back to
/// `wl-copy` until the upstream is fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PasteBackend {
    /// Single-MIME backup/restore via the `wl-copy` / `wl-paste` CLI tools.
    #[default]
    WlCopy,
    /// Multi-MIME backup/restore via the `wl-clipboard-rs` Rust crate.
    WlClipboardRs,
}

/// Runtime configuration for the GUI client (`telora-gui`).
///
/// Resolved once at startup from `~/.config/telora/gui.toml`. If the file is
/// missing or malformed the defaults are used; in either case the GUI keeps
/// working with a sensible baseline.
#[derive(Debug, Clone)]
pub struct GuiConfig {
    pub paste_shortcut: String,
    pub paste_shortcut_by_app: HashMap<String, String>,
    pub paste_backend: PasteBackend,
    /// Maximum time the GUI waits for a `paste_text_via_clipboard` cycle to
    /// complete before falling back to the `wl-copy` subprocess path. The
    /// timeout exists so that a hung `wl-clipboard-rs` call (observed on
    /// labwc with multi-MIME clipboard contents) cannot freeze the tokio
    /// worker that drives the OSD and the control server. Configurable via
    /// `paste_timeout_ms` in `gui.toml` (in milliseconds).
    pub paste_timeout: Duration,
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
            paste_backend: PasteBackend::default(),
            // Generous default: large models + heavy wayland roundtrips +
            // a busy compositor can each consume a few hundred ms; 8 s is
            // the upper bound before we suspect a hang. Lower it on a fast
            // setup if you prefer snappier fallback to wl-copy.
            paste_timeout: Duration::from_secs(8),
        }
    }
}

/// On-disk representation. The `paste_shortcut_by_app` map can be overridden
/// entirely from the TOML file; defaults are merged at load time.
#[derive(Debug, Deserialize, Default)]
struct RawGuiConfig {
    paste_shortcut: Option<String>,
    paste_shortcut_by_app: Option<HashMap<String, String>>,
    paste_backend: Option<PasteBackend>,
    paste_timeout_ms: Option<u64>,
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

        if let Some(backend) = raw.paste_backend {
            cfg.paste_backend = backend;
        }

        if let Some(ms) = raw.paste_timeout_ms {
            if ms == 0 {
                warn!("paste_timeout_ms is zero; keeping default");
            } else {
                cfg.paste_timeout = Duration::from_millis(ms);
            }
        }

        info!(
            "Loaded config from {} (default shortcut: {}, {} per-app overrides, clipboard backend: {:?}, paste timeout: {:?})",
            path.display(),
            cfg.paste_shortcut,
            cfg.paste_shortcut_by_app.len(),
            cfg.paste_backend,
            cfg.paste_timeout
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_backend_defaults_to_wl_copy() {
        assert_eq!(PasteBackend::default(), PasteBackend::WlCopy);
    }

    #[test]
    fn paste_backend_parses_via_raw_config() {
        // Round-trip through the same path the loader uses. The RawGuiConfig
        // representation in this module accepts the key `paste_backend` with
        // `wl-copy` and `wl-clipboard-rs` strings.
        let parsed: RawGuiConfig = toml::from_str("paste_backend = \"wl-copy\"").unwrap();
        assert_eq!(parsed.paste_backend, Some(PasteBackend::WlCopy));

        let parsed: RawGuiConfig = toml::from_str("paste_backend = \"wl-clipboard-rs\"").unwrap();
        assert_eq!(parsed.paste_backend, Some(PasteBackend::WlClipboardRs));
    }

    #[test]
    fn paste_timeout_parses_milliseconds() {
        let parsed: RawGuiConfig = toml::from_str("paste_timeout_ms = 2000").unwrap();
        assert_eq!(parsed.paste_timeout_ms, Some(2000));
    }

    #[test]
    fn gui_config_default_paste_timeout_is_generous() {
        // Default is sane: > 0 and < 30s. Anything outside that should be a
        // deliberate choice.
        let cfg = GuiConfig::default();
        let ms = cfg.paste_timeout.as_millis();
        assert!(ms >= 1000, "default too short: {} ms", ms);
        assert!(ms <= 30_000, "default too long: {} ms", ms);
    }

    #[test]
    fn gui_config_default_backend_is_wl_copy() {
        let cfg = GuiConfig::default();
        assert_eq!(cfg.paste_backend, PasteBackend::WlCopy);
    }
}
