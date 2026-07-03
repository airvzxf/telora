use log::{debug, info, warn};
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::config::GuiConfig;
use crate::focus;

/// MIME type used by KDE / KPasswordManagerHint to flag sensitive content
/// (passwords, keys, etc.) in the clipboard. When present we must NOT back up
/// the clipboard contents, as that would briefly expose the secret in our
/// process memory / logs / crash dumps.
const SENSITIVE_MIME: &str = "x-kde-passwordManagerHint";

/// In-process snapshot of the Wayland clipboard contents.
///
/// Only the first MIME type offered by the source is preserved, along with its
/// raw bytes. Wayland's `wlr-data-control` protocol (and therefore `wl-copy`)
/// can only publish a single MIME type at a time, so restoring the first type
/// is the most faithful representation we can achieve from a CLI tool.
#[derive(Debug, Default)]
pub struct ClipboardSnapshot {
    pub had_content: bool,
    pub primary_type: Option<String>,
    pub data: Option<Vec<u8>>,
}

impl ClipboardSnapshot {
    fn empty() -> Self {
        Self::default()
    }

    fn from_bytes(mime: String, data: Vec<u8>) -> Self {
        Self {
            had_content: true,
            primary_type: Some(mime),
            data: Some(data),
        }
    }
}

/// Read the current clipboard contents into a [`ClipboardSnapshot`].
///
/// Returns an "empty" snapshot (with `had_content = false`) when:
/// - the clipboard is empty
/// - the clipboard contains sensitive data (passwords)
/// - `wl-paste` is not installed or fails
///
/// In all "empty" cases the snapshot can still be safely passed to
/// [`restore`], which will simply clear the clipboard.
pub fn backup() -> ClipboardSnapshot {
    let types_output = match Command::new("wl-paste").arg("--list-types").output() {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            debug!(
                "wl-paste --list-types exited with {:?}, treating as empty clipboard",
                out.status.code()
            );
            let _ = String::from_utf8_lossy(&out.stderr);
            return ClipboardSnapshot::empty();
        }
        Err(e) => {
            warn!("Could not run wl-paste ({}); clipboard backup skipped", e);
            return ClipboardSnapshot::empty();
        }
    };

    let types: Vec<String> = String::from_utf8_lossy(&types_output.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    if types.is_empty() {
        debug!("Clipboard reports no MIME types; nothing to back up");
        return ClipboardSnapshot::empty();
    }

    if types.iter().any(|t| t == SENSITIVE_MIME) {
        warn!(
            "Clipboard contains sensitive data ({}); backup skipped to avoid \
             holding secrets in memory",
            SENSITIVE_MIME
        );
        return ClipboardSnapshot::empty();
    }

    let primary = types[0].clone();

    let read_output = Command::new("wl-paste")
        .args(["--type", &primary, "--no-newline"])
        .output();

    match read_output {
        Ok(out) if out.status.success() => {
            let bytes = out.stdout;
            info!(
                "Backed up clipboard (mime={}, {} bytes)",
                primary,
                bytes.len()
            );
            ClipboardSnapshot::from_bytes(primary, bytes)
        }
        Ok(out) => {
            warn!(
                "wl-paste --type {} exited with {:?}; backup skipped",
                primary,
                out.status.code()
            );
            let _ = String::from_utf8_lossy(&out.stderr);
            ClipboardSnapshot::empty()
        }
        Err(e) => {
            warn!("Failed to read clipboard ({}); backup skipped", e);
            ClipboardSnapshot::empty()
        }
    }
}

/// Restore a previously captured snapshot to the clipboard.
///
/// If the snapshot was empty, the clipboard is cleared. Otherwise the
/// snapshot's primary MIME type is mapped to a canonical equivalent when
/// possible (`text/*` collapses to `text/plain;charset=utf-8`) and pushed
/// back via `wl-copy` using stdin so binary data is preserved verbatim.
pub fn restore(snap: &ClipboardSnapshot) {
    if !snap.had_content {
        debug!("Restoring: clearing clipboard (no prior content)");
        if let Err(e) = Command::new("wl-copy").arg("--clear").status() {
            warn!("Failed to clear clipboard during restore: {}", e);
        }
        return;
    }

    let (original_mime, data) = match (&snap.primary_type, &snap.data) {
        (Some(m), Some(d)) => (m.clone(), d.clone()),
        _ => {
            warn!("Snapshot marked as having content but is missing data; clearing");
            let _ = Command::new("wl-copy").arg("--clear").status();
            return;
        }
    };

    let target_mime = canonicalize_mime(&original_mime);

    match write_to_clipboard(&target_mime, &data) {
        Ok(()) => info!(
            "Restored clipboard (mime={}, {} bytes)",
            target_mime,
            data.len()
        ),
        Err(e) => warn!("Failed to restore clipboard ({}): {}", target_mime, e),
    }
}

/// Type `text` by routing it through the Wayland clipboard:
///
/// 1. Back up whatever is currently in the clipboard.
/// 2. Put `text` in the clipboard as `text/plain;charset=utf-8`.
/// 3. Simulate the configured paste shortcut so the focused application
///    pastes it (`ctrl+v` by default; per-app overrides in `gui.toml`
///    cover terminals that use `ctrl+shift+v` or `shift+insert`).
/// 4. Wait briefly so the receiving app has time to read the clipboard.
/// 5. Restore the original clipboard contents.
pub fn paste_text_via_clipboard(text: &str, config: &GuiConfig) {
    if text.is_empty() {
        return;
    }

    let snap = backup();

    if let Err(e) = write_to_clipboard("text/plain;charset=utf-8", text.as_bytes()) {
        warn!("Failed to put text in clipboard ({}); aborting paste", e);
        return;
    }

    let app_id = focus::focused_app_id();
    let shortcut = config.resolve_paste_shortcut(app_id.as_deref());
    let args = parse_shortcut(&shortcut);

    info!(
        "Simulating paste shortcut '{}' (app_id={})",
        shortcut,
        app_id.as_deref().unwrap_or("<unknown>")
    );

    match Command::new("wtype").args(&args).status() {
        Ok(status) if status.success() => {}
        Ok(status) => warn!("wtype exited with {:?}", status.code()),
        Err(e) => warn!("Failed to run wtype ({}); text remains in clipboard", e),
    }

    // Give the focused app a moment to read the clipboard data via the
    // data-control protocol before we overwrite it with the original.
    thread::sleep(Duration::from_millis(150));

    restore(&snap);
}

/// Convert a human shortcut like `"ctrl+shift+v"` into `wtype` arguments.
///
/// `wtype`'s argument model is split into:
/// - `-M <mod>` / `-m <mod>` — press / release a modifier
/// - `-k <key>` — type (press + release) a named key resolved via
///   `xkb_keysym_from_name` (case-insensitive, e.g. `Insert` and `insert`
///   both work). Using `-k` is essential: any *positional* argument to
///   wtype is treated as **text to type**, not as a key. So
///   `wtype -M shift Insert` would type the literal word "Insert" instead
///   of pressing the Insert key.
/// - `<text>` (positional) — typed as text via `mbstowcs`. We do not use
///   this at all; shortcuts always go through `-k`.
///
/// Modifiers recognized: `ctrl`, `shift`, `alt`, `super`. The final key
/// is normalized through [`normalize_key_name`] for friendlier aliases
/// like `del` → `Delete` and `pgup` → `Page_Up`.
fn parse_shortcut(shortcut: &str) -> Vec<String> {
    let fallback = || {
        warn!(
            "Paste shortcut '{}' is invalid; falling back to ctrl+v",
            shortcut
        );
        vec![
            "-M".to_string(),
            "ctrl".to_string(),
            "-k".to_string(),
            "v".to_string(),
            "-m".to_string(),
            "ctrl".to_string(),
        ]
    };

    let mut mods: Vec<String> = Vec::new();
    let mut key: Option<String> = None;
    for part in shortcut.split('+') {
        let token = part.trim();
        if token.is_empty() {
            continue;
        }
        match token {
            "ctrl" | "shift" | "alt" | "super" => mods.push(token.to_string()),
            _ => key = Some(token.to_string()),
        }
    }

    let key = match key.and_then(|k| normalize_key_name(&k)) {
        Some(k) => k,
        None => return fallback(),
    };

    let mut args: Vec<String> = Vec::new();
    for m in &mods {
        args.push("-M".to_string());
        args.push(m.clone());
    }
    args.push("-k".to_string());
    args.push(key);
    for m in mods.iter().rev() {
        args.push("-m".to_string());
        args.push(m.clone());
    }
    args
}

/// Map common user-friendly key aliases to their libxkbcommon canonical
/// names. Single-character keys (a-z, 0-9) and `F1`-`F24` are accepted
/// directly. Returns `None` for empty / unknown key names so the caller
/// can fall back to `ctrl+v`.
///
/// `wtype -k` is itself case-insensitive (`xkb_keysym_from_name` is called
/// with `XKB_KEYSYM_CASE_INSENSITIVE`), so the canonical strings here are
/// only for cosmetic normalization in logs and for the aliases that the
/// raw xkb symbol table doesn't shorten.
fn normalize_key_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let canonical = match name.to_ascii_lowercase().as_str() {
        "insert" => "Insert",
        "delete" | "del" => "Delete",
        "home" => "Home",
        "end" => "End",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "pageup" | "pgup" | "prior" => "Page_Up",
        "pagedown" | "pgdn" | "next" => "Page_Down",
        "return" | "enter" => "Return",
        "tab" => "Tab",
        "escape" | "esc" => "Escape",
        "backspace" | "bs" => "BackSpace",
        "space" => "space",
        other if other.len() == 1 => return Some(other.to_string()),
        other if other.starts_with('f') && other.len() <= 3 => {
            let n: u32 = match other[1..].parse() {
                Ok(n) if (1..=24).contains(&n) => n,
                _ => return None,
            };
            return Some(format!("F{n}"));
        }
        _ => return None,
    };
    Some(canonical.to_string())
}

fn write_to_clipboard(mime: &str, data: &[u8]) -> std::io::Result<()> {
    let mut child = Command::new("wl-copy")
        .args(["--type", mime])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(data)?;
    }

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "wl-copy exited with {:?}",
            status.code()
        )))
    }
}

/// Map a stored MIME type to a canonical equivalent for restoration.
///
/// `wl-copy` cannot publish multiple types simultaneously, so when the
/// original offered several `text/*` variants we collapse them to
/// `text/plain;charset=utf-8`, which is the only one any modern app needs.
/// For `image/*` and other types we keep the original since there is no
/// canonical fallback.
fn canonicalize_mime(original: &str) -> String {
    if original.starts_with("text/") {
        "text/plain;charset=utf-8".to_string()
    } else {
        original.to_string()
    }
}
