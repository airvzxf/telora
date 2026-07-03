use log::{debug, warn};
use std::process::Command;
use std::sync::OnceLock;

/// Whether we've already warned about a missing `wlrctl`, to avoid spamming
/// the log on every transcription.
static MISSING_WLRCTL_WARNED: OnceLock<bool> = OnceLock::new();

/// Identify the `app_id` of the currently focused Wayland window.
///
/// Returns `None` when:
/// - `wlrctl` is not installed (only relevant on wlroots-based compositors)
/// - the compositor doesn't expose `wlr-foreign-toplevel-management`
/// - no toplevel is currently active
/// - we fail to parse the output of `wlrctl`
///
/// On compositors without `wlrctl` support (GNOME, KDE) the caller will fall
/// back to the default `paste_shortcut` from the config.
pub fn focused_app_id() -> Option<String> {
    let list_output = match Command::new("wlrctl").args(["toplevel", "list"]).output() {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            debug!(
                "wlrctl toplevel list exited with {:?}; assuming non-wlroots compositor",
                out.status.code()
            );
            let _ = String::from_utf8_lossy(&out.stderr);
            return None;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if MISSING_WLRCTL_WARNED.set(true).is_ok() {
                warn!(
                    "wlrctl not found; per-app paste shortcut is unavailable (set \
                     a global `paste_shortcut` in gui.toml to override)"
                );
            }
            return None;
        }
        Err(e) => {
            warn!(
                "Failed to run wlrctl ({}); falling back to default shortcut",
                e
            );
            return None;
        }
    };

    let stdout = String::from_utf8_lossy(&list_output.stdout);
    let app_ids = parse_app_ids(&stdout);

    if app_ids.is_empty() {
        debug!("wlrctl returned no toplevels");
        return None;
    }

    for id in &app_ids {
        let probe = Command::new("wlrctl")
            .args(["toplevel", "find", &format!("app_id:{id}"), "state:active"])
            .status();
        if let Ok(status) = probe
            && status.success()
        {
            debug!("Focused app_id: {}", id);
            return Some(id.clone());
        }
    }

    debug!("No toplevel reported as active");
    None
}

/// Parse the `app_id:title` lines returned by `wlrctl toplevel list`.
/// Preserves order and removes duplicates.
fn parse_app_ids(output: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for line in output.lines() {
        let Some((app_id, _title)) = line.split_once(':') else {
            continue;
        };
        let trimmed = app_id.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !seen.iter().any(|s| s == trimmed) {
            seen.push(trimmed.to_string());
        }
    }
    seen
}
