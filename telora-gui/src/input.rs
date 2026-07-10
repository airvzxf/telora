use log::{error, info, warn};
use std::process::Command;

use super::clipboard::{self, PasteOutcome};
use super::config::GuiConfig;

pub fn type_text(text: &str, config: &GuiConfig) -> PasteOutcome {
    if text.trim().is_empty() {
        return PasteOutcome::Refused {
            reason: "transcription is empty".to_string(),
        };
    }

    info!(
        "Typing text via clipboard paste flow ({} chars)",
        text.chars().count()
    );

    // Primary path: put the text in the clipboard, simulate the configured
    // paste shortcut (per-app override or default), then restore whatever
    // was there before. This is more reliable than wtype's
    // character-by-character synthesis (which mangles non-ASCII, dead keys,
    // IMEs, etc.) and preserves the user's prior clipboard contents.
    //
    // If wtype is missing entirely (e.g. minimal Wayland setups), the
    // routine logs a warning and the text stays in the clipboard, so the
    // user can paste it manually.
    clipboard::paste_text_via_clipboard(text, config)
}

/// Backwards-compatible direct fallback for callers that specifically want
/// character-by-character synthesis instead of the clipboard round-trip.
/// Kept private to the module so it doesn't grow stale.
#[allow(dead_code)]
fn type_text_direct(text: &str) -> PasteOutcome {
    if text.trim().is_empty() {
        return PasteOutcome::Refused {
            reason: "transcription is empty".to_string(),
        };
    }

    match Command::new("wtype").arg(text).output() {
        Ok(_) => PasteOutcome::Ok,
        Err(e) => {
            warn!("wtype failed: {}. Falling back to clipboard copy.", e);
            copy_text(text);
            PasteOutcome::Refused {
                reason: format!("wtype failed and no clipboard paste: {}", e),
            }
        }
    }
}

pub fn copy_text(text: &str) {
    if text.trim().is_empty() {
        return;
    }
    info!("Copying text to clipboard");

    let mut child = match Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            error!("Failed to spawn wl-copy: {}", e);
            return;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if let Err(e) = stdin.write_all(text.as_bytes()) {
            error!("Failed to write to wl-copy stdin: {}", e);
        }
    }

    let _ = child.wait();
}
