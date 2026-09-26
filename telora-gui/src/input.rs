use log::{error, info, warn};
use std::process::Command;

use wl_clipboard_rs::copy::{MimeSource, MimeType as CopyMimeType, Options, Source};

use super::clipboard::{self, PasteOutcome};
use super::config::GuiConfig;

/// MIME type used by `copy_text` when publishing a single plaintext string
/// to the clipboard. Matches the `text/plain;charset=utf-8` value used
/// elsewhere in the GUI's paste flow so the receiving app sees the same
/// canonical set of plain-text aliases regardless of which entry point
/// (type / copy) was used.
const COPY_MIME: &str = "text/plain;charset=utf-8";

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
    // If wtype is missing entirely (e.g. minimal Wayland setups, KDE
    // Plasma 6 without wlroots-ecosystem tools), the routine logs a warning
    // and the text stays in the clipboard, so the user can paste it
    // manually. The OSD surfaces a hint to that effect.
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

/// Publish `text` to the clipboard as `text/plain;charset=utf-8`.
///
/// Uses `wl-clipboard-rs` directly (a Rust crate that speaks the
/// `wlr-data-control` / `ext-data-control` Wayland protocols), so this
/// works on any compositor that exposes either protocol — including
/// KDE Plasma 6's KWin — **without requiring `wl-copy` to be installed**.
///
/// Pre-fix behaviour was `Command::new("wl-copy")` with no fallback;
/// that silently failed on systems without the `wl-clipboard` package
/// (typical for KDE distros). Empirically verified against KWin 6.7.5
/// with `cargo run --example probe` (see `examples/probe.rs`).
///
/// Errors are logged but otherwise swallowed: this function is a
/// fire-and-forget copy and `TOGGLE_COPY` does not surface a separate
/// outcome to the OSD, so the only way for the user to notice is via
/// the daemon log.
pub fn copy_text(text: &str) {
    if text.trim().is_empty() {
        return;
    }
    info!("Copying text to clipboard");

    let opts = Options::new();
    let source = MimeSource {
        mime_type: CopyMimeType::Specific(COPY_MIME.to_string()),
        source: Source::Bytes(text.as_bytes().to_vec().into_boxed_slice()),
    };
    if let Err(e) = opts.copy_multi(vec![source]) {
        error!(
            "Failed to put text in clipboard via wl-clipboard-rs ({}). \
             The transcription was not copied. Check that the compositor \
             exposes wlr-data-control or ext-data-control (KDE Plasma 6+ \
             and wlroots compositors do).",
            e
        );
    }
}
