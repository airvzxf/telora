//! Put the transcription on the Wayland clipboard.

use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};

const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// Copy `text` to the regular clipboard. Needs the compositor to expose
/// `wlr-data-control` or `ext-data-control` (KDE Plasma 6 and wlroots do).
///
/// # Errors
///
/// Returns a user-facing reason when the clipboard could not be set.
pub fn copy(text: &str) -> Result<(), String> {
    let source = MimeSource {
        mime_type: MimeType::Specific(TEXT_MIME.to_string()),
        source: Source::Bytes(text.as_bytes().to_vec().into_boxed_slice()),
    };
    Options::new().copy_multi(vec![source]).map_err(|e| {
        log::error!("wl-clipboard-rs copy failed: {e}");
        format!("no se pudo copiar al portapapeles ({e})")
    })
}
