use log::{error, info, warn};
use std::process::Command;

use super::clipboard::{self, PasteOutcome};
use super::config::GuiConfig;

/// Run the synchronous paste cycle on a blocking thread with a hard timeout.
///
/// `paste_text_via_clipboard` is currently synchronous and may block indefinitely
/// when the chosen backend (notably `wl-clipboard-rs` against some wlroots
/// compositors) gets stuck inside a Wayland roundtrip. That call runs inside
/// the tokio worker that also services the OSD and the control server, so a
/// hang there freezes the whole GUI. Running the call on a `spawn_blocking`
/// task and wrapping it with `tokio::time::timeout` lets the worker recover:
/// when the timeout fires we fall back to the robust `wl-copy` subprocess
/// path while the hung worker thread continues in the background (it never
/// finishes on its own, but it does not interfere with future operations).
pub async fn type_text(text: &str, config: &GuiConfig) -> PasteOutcome {
    if text.trim().is_empty() {
        return PasteOutcome::Refused {
            reason: "transcription is empty".to_string(),
        };
    }

    info!(
        "Typing text via clipboard paste flow ({} chars)",
        text.chars().count()
    );

    // Clone the inputs so the spawning closure captures owned data. Spawning
    // itself requires `'static + Send + 'static`; `Config` is `Clone`, and
    // `text` is converted to a `String`.
    let text_owned = text.to_string();
    let config_owned = config.clone();
    let timeout = config.paste_timeout;

    // Run the synchronous paste on a blocking thread so the tokio worker
    // is free to keep processing OSD updates and control commands while the
    // call makes its wayland roundtrips.
    let paste_handle = tokio::task::spawn_blocking(move || {
        clipboard::paste_text_via_clipboard(&text_owned, &config_owned)
    });

    // Wait for the paste cycle with a hard timeout. If the timeout fires we
    // fall back to a known-good paste path; the spawned thread continues
    // running in the background but does not block any other GUI operation.
    match tokio::time::timeout(timeout, paste_handle).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(join_err)) => {
            warn!("Paste thread panicked ({}); aborting paste", join_err);
            PasteOutcome::Refused {
                reason: format!("paste thread panicked: {}", join_err),
            }
        }
        Err(_elapsed) => {
            warn!(
                "Paste via {:?} did not complete within {:?}; \
                 falling back to wl-copy subprocess and continuing anyway",
                config.paste_backend, timeout
            );
            // The hung `paste_text_via_clipboard` thread is leaked on
            // purpose: killing it from Rust is not safe (the thread holds
            // internal wayland state) and its actions are bounded. Future
            // toggles will keep working because this call site does not
            // depend on it.
            clipboard::paste_text_via_wl_copy_subprocess(text, config)
        }
    }
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
