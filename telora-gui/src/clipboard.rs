//! Wayland clipboard lifecycle for the toggle-type flow.
//!
//! Uses [`wl_clipboard_rs`] to back up and restore every MIME type the
//! source application advertised, so the user's prior clipboard contents
//! survive the simulated paste. When the compositor lacks
//! `wlr-data-control` / `ext-data-control`, the module falls back to the
//! `wl-copy` / `wl-paste` shell tools (single-MIME only) and surfaces a
//! user-visible hint via [`PasteOutcome::FallbackSingleMime`].
//!
//! Sensitive data marked with the KDE password-manager hint MIME type
//! (`x-kde-passwordManagerHint`) is never copied into our process memory.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use wl_clipboard_rs::copy::{
    self, ClipboardType as CopyClipboardType, MimeSource, MimeType as CopyMimeType, Options,
    Seat as CopySeat, Source,
};
use wl_clipboard_rs::paste::{
    self, ClipboardType as PasteClipboardType, Error as PasteError, MimeType as PasteMimeType, Seat,
};

use crate::config::GuiConfig;
use crate::focus;

/// MIME type used by KDE / KPasswordManagerHint to flag sensitive content
/// (passwords, keys, etc.) in the clipboard. When present we must NOT back up
/// the clipboard contents, as that would briefly expose the secret in our
/// process memory / logs / crash dumps.
const SENSITIVE_MIME: &str = "x-kde-passwordManagerHint";

/// MIME type used to deliver the transcription to the focused application.
const TRANSCRIPTION_MIME: &str = "text/plain;charset=utf-8";

/// Grace period between the simulated paste and the restore, giving the
/// focused application time to read the clipboard data via the data-control
/// protocol before we overwrite it with the original.
const RESTORE_DELAY_MS: u64 = 150;

/// Per-MIME hard deadline for `paste::get_contents`.
///
/// `wl-clipboard-rs` 0.9.3 calls `queue.roundtrip` synchronously inside
/// `paste::get_contents`, and on some wlroots-based compositors (labwc,
/// possibly Hyprland in certain configurations) the compositor never fills
/// the pipe it created, leaving the calling thread blocked at
/// `wchan=anon_pipe_read` indefinitely. We do NOT silently fall back to
/// another path; instead we abandon each stuck MIME after this deadline,
/// log the MIME that was skipped, and continue with the rest of the
/// snapshot. This keeps the paste cycle from freezing the GUI when one
/// specific MIME type hangs. The thread that was blocked in the roundtrip
/// is intentionally leaked: Rust cannot safely kill a thread holding
/// native Wayland state, and the leaked thread does not block any other
/// operation. At worst we keep one such thread per hung MIME per cycle.
const PER_MIME_READ_DEADLINE: Duration = Duration::from_millis(1500);

/// Threshold above which we log the previous backup size at INFO instead of
/// DEBUG. Helps spot large image pastes without spamming routine text pastes.
const LARGE_BACKUP_BYTES: usize = 64 * 1024;

/// One MIME source from the prior clipboard contents, kept verbatim so we
/// can re-offer it as-is on restore.
#[derive(Debug, Clone)]
pub struct MimeSourceEntry {
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// In-process snapshot of the Wayland clipboard.
///
/// When `had_content` is `false`, `sources` is empty and `restore` clears
/// the clipboard (or leaves it cleared). When `true`, `restore` re-offers
/// every entry in `sources` in a single Wayland offer, so the receiving
/// application picks whichever MIME type it wants.
///
/// `skipped_mimes` records the MIME types that [`backup`] tried to read
/// but could not because `paste::get_contents` either errored or hit the
/// per-MIME read deadline. These MIMEs are NOT in `sources`, so they will
/// not appear in the restored offer; the caller surfaces this so the user
/// knows which types were dropped.
#[derive(Debug, Default)]
pub struct ClipboardSnapshot {
    pub had_content: bool,
    pub sources: Vec<MimeSourceEntry>,
    pub skipped_mimes: Vec<String>,
}

/// Outcome of [`paste_text_via_clipboard`]. Lets the caller pick the right
/// OSD message and decide whether to keep the temporary transcription in
/// the clipboard (for manual paste) when restore failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteOutcome {
    /// Full multi-MIME backup + paste + restore succeeded.
    Ok,
    /// The paste cycle completed and the receiving app got the text, but
    /// some MIME types were dropped from the backup because their
    /// `paste::get_contents` call errored or hit the per-MIME deadline.
    /// The clipboard was restored with the surviving types; `skipped`
    /// lists the MIME types that were lost so the user can decide whether
    /// to retry with a different compositor setting.
    Partial { skipped: Vec<String> },
    /// The compositor does not expose `wlr-data-control` /
    /// `ext-data-control`, so wl-clipboard-rs could not run. We fell back
    /// to the `wl-copy` / `wl-paste` shell tools, which can only handle a
    /// single MIME type per offer. The transcription was pasted and is
    /// left in the clipboard; the previous contents were not preserved.
    /// `reason` describes the underlying error for the log.
    FallbackSingleMime { reason: String },
    /// The cycle was refused outright (sensitive data already in the
    /// clipboard, empty text, etc.). The clipboard was not modified.
    Refused { reason: String },
}

impl PasteOutcome {
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            PasteOutcome::FallbackSingleMime { .. } | PasteOutcome::Refused { .. }
        )
    }
}

impl ClipboardSnapshot {
    fn empty() -> Self {
        Self::default()
    }
}

/// Check whether the current clipboard contents include the
/// password-manager hint MIME type. Used as a safety check before
/// overwriting the clipboard with a transcription, so a
/// password-manager secret is never silently clobbered.
pub fn has_sensitive_content() -> bool {
    match paste::get_mime_types(PasteClipboardType::Regular, Seat::Unspecified) {
        Ok(types) => types.iter().any(|t| t == SENSITIVE_MIME),
        Err(_) => false,
    }
}

/// Read the current clipboard contents into a [`ClipboardSnapshot`].
///
/// Returns an "empty" snapshot (with `had_content = false`) when:
/// - the clipboard is empty or only advertises types we cannot read,
/// - the clipboard contains sensitive data (passwords),
/// - `wl-clipboard-rs` could not connect (no `wlr-data-control` /
///   `ext-data-control`); in that case the snapshot is still empty, and
///   the caller can detect the missing protocol through the returned
///   `backup_error` flag.
pub fn backup() -> (ClipboardSnapshot, bool) {
    let types = match paste::get_mime_types_ordered(PasteClipboardType::Regular, Seat::Unspecified)
    {
        Ok(t) => t,
        Err(e) => {
            log_missing_protocol(&e);
            return (ClipboardSnapshot::empty(), true);
        }
    };

    if types.is_empty() {
        log::debug!("Clipboard reports no MIME types; nothing to back up");
        return (ClipboardSnapshot::empty(), false);
    }

    if types.iter().any(|t| t == SENSITIVE_MIME) {
        log::warn!(
            "Clipboard contains sensitive data ({}); backup skipped to avoid \
             holding secrets in memory",
            SENSITIVE_MIME
        );
        return (ClipboardSnapshot::empty(), false);
    }

    let mut sources: Vec<MimeSourceEntry> = Vec::with_capacity(types.len());
    let mut skipped: Vec<String> = Vec::new();
    for mime in &types {
        match read_mime_with_deadline(mime, PER_MIME_READ_DEADLINE) {
            ReadOutcome::Read(data) => sources.push(MimeSourceEntry {
                mime_type: mime.clone(),
                data,
            }),
            ReadOutcome::Failed(e) => {
                log::warn!(
                    "wl-clipboard-rs paste failed for mime={} ({}); skipping that type",
                    mime,
                    e
                );
                skipped.push(mime.clone());
            }
            ReadOutcome::TimedOut => {
                log::warn!(
                    "wl-clipboard-rs paste for mime={} did not complete within {:?}; \
                     skipping that type (compositor likely failed to deliver data)",
                    mime,
                    PER_MIME_READ_DEADLINE
                );
                skipped.push(mime.clone());
            }
        }
    }

    if sources.is_empty() {
        if skipped.is_empty() {
            log::debug!("No MIME types yielded contents; treating as empty snapshot");
        } else {
            log::warn!(
                "All {} MIME type(s) failed to read or timed out: {:?}; treating as empty snapshot",
                skipped.len(),
                skipped
            );
        }
        return (
            ClipboardSnapshot {
                had_content: false,
                sources: Vec::new(),
                skipped_mimes: skipped,
            },
            false,
        );
    }

    log_total_backup_size(&sources);
    if !skipped.is_empty() {
        log::warn!(
            "Backup preserved {} of {} MIME type(s); dropped: {:?}",
            sources.len(),
            sources.len() + skipped.len(),
            skipped
        );
    }
    (
        ClipboardSnapshot {
            had_content: true,
            sources,
            skipped_mimes: skipped,
        },
        false,
    )
}

/// Result of a single `paste::get_contents` attempt bounded by a deadline.
enum ReadOutcome {
    /// The clipboard delivered the bytes before the deadline.
    Read(Vec<u8>),
    /// `paste::get_contents` returned an error before the deadline.
    Failed(paste::Error),
    /// The deadline elapsed before `paste::get_contents` returned. The
    /// spawned worker thread continues running; we abandon its result.
    TimedOut,
}

/// Run `paste::get_contents` for `mime` on a worker thread, abandoning the
/// result if the call does not return within `deadline`.
///
/// See [`PER_MIME_READ_DEADLINE`] for why this exists. The spawned thread
/// is intentionally NOT joined on timeout; killing a thread that holds
/// Wayland state is unsafe in Rust and the leaked thread is bounded (one
/// per hung MIME per cycle).
fn read_mime_with_deadline(mime: &str, deadline: Duration) -> ReadOutcome {
    let (tx, rx) = mpsc::channel();
    let mime_owned = mime.to_string();
    let join = thread::Builder::new()
        .name(format!("telora-paste-{}", mime_owned))
        .spawn(move || {
            let result: Result<Vec<u8>, String> = (|| -> Result<Vec<u8>, String> {
                let (mut pipe, _actual_mime) = paste::get_contents(
                    PasteClipboardType::Regular,
                    Seat::Unspecified,
                    PasteMimeType::Specific(&mime_owned),
                )
                .map_err(|e| e.to_string())?;
                let mut data = Vec::new();
                pipe.read_to_end(&mut data).map_err(|e| e.to_string())?;
                Ok(data)
            })();
            let _ = tx.send(result);
        });
    let spawn_result = match join {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    };
    if let Err(e) = spawn_result {
        log::warn!("Failed to spawn reader thread for {}: {}", mime, e);
        return ReadOutcome::Failed(paste::Error::PipeCreation(std::io::Error::other(e)));
    }

    match rx.recv_timeout(deadline) {
        Ok(Ok(data)) => ReadOutcome::Read(data),
        Ok(Err(e)) => {
            // Try to map back into a paste::Error variant when possible so
            // the failure log stays informative, otherwise synthesise a
            // generic PipeCreation error.
            ReadOutcome::Failed(classify_read_error(&e))
        }
        Err(_timeout) => ReadOutcome::TimedOut,
    }
}

/// Best-effort conversion from a thread-channel error string into a
/// `paste::Error`. Most error messages produced by `get_contents` /
/// `read_to_end` are MIME-aware only in their formatting; we keep the
/// `PipeCreation` variant to surface "we couldn't read from the pipe".
fn classify_read_error(message: &str) -> paste::Error {
    paste::Error::PipeCreation(std::io::Error::other(message))
}

/// Restore a previously captured snapshot to the clipboard via
/// `wl-clipboard-rs`'s multi-MIME path. Returns an error if the protocol
/// is unavailable; the caller decides whether to fall back to the
/// single-MIME `wl-copy` shell tool.
pub fn restore(snap: &ClipboardSnapshot) -> Result<(), copy::Error> {
    if !snap.had_content {
        log::debug!("Restoring: clearing clipboard (no prior content)");
        if let Err(e) = copy::clear(CopyClipboardType::Regular, CopySeat::All) {
            log::warn!("Failed to clear clipboard during restore: {}", e);
        }
        return Ok(());
    }

    let mime_sources: Vec<MimeSource> = snap
        .sources
        .iter()
        .map(|s| MimeSource {
            source: Source::Bytes(s.data.clone().into_boxed_slice()),
            mime_type: CopyMimeType::Specific(s.mime_type.clone()),
        })
        .collect();

    let opts = Options::new();
    log::info!(
        "Restoring clipboard ({} MIME type{} via wl-clipboard-rs)",
        mime_sources.len(),
        if mime_sources.len() == 1 { "" } else { "s" }
    );
    opts.copy_multi(mime_sources)
}

/// Type `text` by routing it through the Wayland clipboard:
///
/// 1. Back up every MIME type the current clipboard advertises (multi-MIME).
/// 2. Put `text` in the clipboard as `text/plain;charset=utf-8`.
/// 3. Simulate the configured paste shortcut so the focused application
///    pastes it (`ctrl+v` by default; per-app overrides in `gui.toml`
///    cover terminals that use `ctrl+shift+v` or `shift+insert`).
/// 4. Wait briefly so the receiving app has time to read the clipboard.
/// 5. Restore the original clipboard contents, re-offering every MIME
///    type it previously held.
///
/// If `wl-clipboard-rs` cannot talk to the compositor (no
/// `wlr-data-control` / `ext-data-control`), the routine falls back to
/// `wl-copy` / `wl-paste` and returns [`PasteOutcome::FallbackSingleMime`].
/// The transcription is still pasted and left in the clipboard so the user
/// can recover either by re-copying or by manual paste.
///
/// Refuses to run when the clipboard currently contains the KDE password
/// manager hint, so we never overwrite a password/key with the
/// transcription.
pub fn paste_text_via_clipboard(text: &str, config: &GuiConfig) -> PasteOutcome {
    if text.is_empty() {
        return PasteOutcome::Refused {
            reason: "transcription is empty".to_string(),
        };
    }

    if has_sensitive_content() {
        log::warn!(
            "Clipboard contains sensitive data ({}); refusing to overwrite \
             it with the transcription. Paste manually after clearing the \
             clipboard.",
            SENSITIVE_MIME
        );
        return PasteOutcome::Refused {
            reason: format!("clipboard contains {}", SENSITIVE_MIME),
        };
    }

    let (snap, backup_protocol_missing) = backup();
    if backup_protocol_missing {
        let reason = "wlr-data-control / ext-data-control not available".to_string();
        log::warn!(
            "wl-clipboard-rs cannot read the clipboard ({}); falling back \
             to wl-copy / wl-paste single-MIME for this round",
            reason
        );
        return paste_text_via_wl_copy_fallback(text, config, reason);
    }

    if let Err(e) = write_to_clipboard_multi(text) {
        if is_protocol_error(&e) {
            let reason = copy_error_reason(&e);
            log::warn!(
                "wl-clipboard-rs copy failed ({}); falling back to wl-copy \
                 single-MIME for this round",
                reason
            );
            return paste_text_via_wl_copy_fallback(text, config, reason);
        }
        log::warn!(
            "Failed to put text in clipboard via wl-clipboard-rs ({}); aborting paste",
            e
        );
        return PasteOutcome::Refused {
            reason: format!("wl-clipboard-rs copy failed: {}", e),
        };
    }

    let app_id = focus::focused_app_id();
    let shortcut = config.resolve_paste_shortcut(app_id.as_deref());
    let args = parse_shortcut(&shortcut);

    log::info!(
        "Simulating paste shortcut '{}' (app_id={})",
        shortcut,
        app_id.as_deref().unwrap_or("<unknown>")
    );

    match Command::new("wtype").args(&args).status() {
        Ok(status) if status.success() => {}
        Ok(status) => log::warn!("wtype exited with {:?}", status.code()),
        Err(e) => {
            log::warn!("Failed to run wtype ({}); text remains in clipboard", e);
        }
    }

    if snap.had_content {
        thread::sleep(Duration::from_millis(RESTORE_DELAY_MS));
        match restore(&snap) {
            Ok(()) => outcome_for_skipped(&snap),
            Err(e) if is_protocol_error(&e) => {
                let reason = copy_error_reason(&e);
                log::warn!(
                    "wl-clipboard-rs restore failed at restore time ({}); the \
                     transcription is left in the clipboard for manual paste",
                    reason
                );
                PasteOutcome::FallbackSingleMime { reason }
            }
            Err(e) => {
                log::warn!(
                    "wl-clipboard-rs restore failed ({}); transcription left in clipboard",
                    e
                );
                PasteOutcome::FallbackSingleMime {
                    reason: e.to_string(),
                }
            }
        }
    } else {
        outcome_for_skipped(&snap)
    }
}

/// Return `PasteOutcome::Partial` when the snapshot has MIME types that
/// `backup` could not read; otherwise `PasteOutcome::Ok`. The receiving
/// app still received the transcription — only clipboard fidelity is
/// degraded, and the OSD surfaces a degraded-mode warning.
fn outcome_for_skipped(snap: &ClipboardSnapshot) -> PasteOutcome {
    if snap.skipped_mimes.is_empty() {
        PasteOutcome::Ok
    } else {
        PasteOutcome::Partial {
            skipped: snap.skipped_mimes.clone(),
        }
    }
}

fn write_to_clipboard_multi(text: &str) -> Result<(), copy::Error> {
    let opts = Options::new();
    opts.copy_multi(vec![MimeSource {
        source: Source::Bytes(text.as_bytes().to_vec().into_boxed_slice()),
        mime_type: CopyMimeType::Specific(TRANSCRIPTION_MIME.to_string()),
    }])
}

fn paste_text_via_wl_copy_fallback(text: &str, config: &GuiConfig, reason: String) -> PasteOutcome {
    let snap = backup_via_wl_paste();

    if let Err(e) = write_to_clipboard_via_wl_copy(TRANSCRIPTION_MIME, text.as_bytes()) {
        log::warn!("wl-copy fallback write failed ({}); aborting paste", e);
        return PasteOutcome::Refused {
            reason: format!("wl-copy fallback write failed: {}", e),
        };
    }

    let app_id = focus::focused_app_id();
    let shortcut = config.resolve_paste_shortcut(app_id.as_deref());
    let args = parse_shortcut(&shortcut);

    log::info!(
        "Simulating paste shortcut '{}' (app_id={}, fallback)",
        shortcut,
        app_id.as_deref().unwrap_or("<unknown>")
    );

    if let Err(e) = Command::new("wtype").args(&args).status() {
        log::warn!("Failed to run wtype ({}); text remains in clipboard", e);
    }

    if snap.had_content {
        thread::sleep(Duration::from_millis(RESTORE_DELAY_MS));
        restore_via_wl_copy(&snap);
        // We deliberately do not return Ok: the multi-MIME promise was
        // not kept. The caller will surface an OSD hint to the user.
        PasteOutcome::FallbackSingleMime { reason }
    } else {
        // Nothing to restore, transcription is in the clipboard and ready
        // for manual paste if needed. Still semantically a fallback.
        PasteOutcome::FallbackSingleMime { reason }
    }
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
        log::warn!(
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

// ---------------------------------------------------------------------------
// wl-copy / wl-paste fallback path
// ---------------------------------------------------------------------------
//
// This path is only used when wl-clipboard-rs cannot talk to the
// compositor. It mirrors the pre-migration behavior (PR #4): single MIME
// type per offer via the wl-copy CLI.

fn write_to_clipboard_via_wl_copy(mime: &str, data: &[u8]) -> std::io::Result<()> {
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

fn backup_via_wl_paste() -> ClipboardSnapshot {
    let types_output = match Command::new("wl-paste").arg("--list-types").output() {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            log::debug!(
                "wl-paste --list-types exited with {:?}, treating as empty clipboard",
                out.status.code()
            );
            let _ = String::from_utf8_lossy(&out.stderr);
            return ClipboardSnapshot::empty();
        }
        Err(e) => {
            log::warn!("Could not run wl-paste ({}); clipboard backup skipped", e);
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
        log::debug!("wl-paste: no MIME types; nothing to back up");
        return ClipboardSnapshot::empty();
    }

    if types.iter().any(|t| t == SENSITIVE_MIME) {
        log::warn!(
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
            log::info!(
                "Backed up clipboard (mime={}, {} bytes, wl-copy fallback)",
                primary,
                bytes.len()
            );
            ClipboardSnapshot {
                had_content: true,
                sources: vec![MimeSourceEntry {
                    mime_type: primary,
                    data: bytes,
                }],
                skipped_mimes: Vec::new(),
            }
        }
        Ok(out) => {
            log::warn!(
                "wl-paste --type {} exited with {:?}; backup skipped",
                primary,
                out.status.code()
            );
            let _ = String::from_utf8_lossy(&out.stderr);
            ClipboardSnapshot::empty()
        }
        Err(e) => {
            log::warn!("Failed to read clipboard ({}); backup skipped", e);
            ClipboardSnapshot::empty()
        }
    }
}

fn restore_via_wl_copy(snap: &ClipboardSnapshot) {
    if !snap.had_content {
        let _ = Command::new("wl-copy").arg("--clear").status();
        return;
    }
    let entry = match snap.sources.first() {
        Some(e) => e,
        None => {
            let _ = Command::new("wl-copy").arg("--clear").status();
            return;
        }
    };
    match write_to_clipboard_via_wl_copy(&entry.mime_type, &entry.data) {
        Ok(()) => log::info!(
            "Restored clipboard (mime={}, {} bytes, wl-copy fallback)",
            entry.mime_type,
            entry.data.len()
        ),
        Err(e) => log::warn!(
            "Failed to restore clipboard via wl-copy ({}): {}",
            entry.mime_type,
            e
        ),
    }
}

// ---------------------------------------------------------------------------
// Error classification helpers (testable)
// ---------------------------------------------------------------------------

/// Returns true when the copy error indicates the compositor does not
/// support `wlr-data-control` / `ext-data-control`.
fn is_protocol_error(e: &copy::Error) -> bool {
    matches!(
        e,
        copy::Error::MissingProtocol { .. } | copy::Error::WaylandConnection(_)
    )
}

fn copy_error_reason(e: &copy::Error) -> String {
    match e {
        copy::Error::MissingProtocol { name, version } => {
            format!("compositor missing {} v{}", name, version)
        }
        copy::Error::WaylandConnection(_) => "could not connect to Wayland server".to_string(),
        other => other.to_string(),
    }
}

fn log_missing_protocol(e: &PasteError) {
    match e {
        PasteError::MissingProtocol { name, version } => {
            log::debug!(
                "wl-clipboard-rs: compositor does not expose {} v{}; \
                 fallback will be used",
                name,
                version
            );
        }
        other => {
            log::debug!("wl-clipboard-rs paste types failed: {}", other);
        }
    }
}

fn log_total_backup_size(sources: &[MimeSourceEntry]) {
    let total: usize = sources.iter().map(|s| s.data.len()).sum();
    let largest: usize = sources.iter().map(|s| s.data.len()).max().unwrap_or(0);
    if total >= LARGE_BACKUP_BYTES {
        log::info!(
            "Backed up clipboard ({} MIME types, total {} bytes; largest {} bytes)",
            sources.len(),
            total,
            largest
        );
    } else {
        log::debug!(
            "Backed up clipboard ({} MIME types, total {} bytes)",
            sources.len(),
            total
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_has_no_content() {
        let s = ClipboardSnapshot::empty();
        assert!(!s.had_content);
        assert!(s.sources.is_empty());
    }

    #[test]
    fn copy_error_reason_describes_missing_protocol() {
        let e = copy::Error::MissingProtocol {
            name: "ext-data-control",
            version: 1,
        };
        let r = copy_error_reason(&e);
        assert!(r.contains("ext-data-control"));
        assert!(r.contains("v1"));
    }

    #[test]
    fn sensitive_mime_is_in_safety_list() {
        assert_eq!(SENSITIVE_MIME, "x-kde-passwordManagerHint");
    }

    #[test]
    fn paste_outcome_ok_is_not_failure() {
        assert!(!PasteOutcome::Ok.is_failure());
        assert!(
            PasteOutcome::Refused {
                reason: "x".to_string()
            }
            .is_failure()
        );
        assert!(
            PasteOutcome::FallbackSingleMime {
                reason: "x".to_string()
            }
            .is_failure()
        );
        // Partial is degraded success, not a hard failure: the user
        // received the text but lost some MIME types from the snapshot.
        assert!(
            !PasteOutcome::Partial {
                skipped: vec!["text/x-moz-url-priv".to_string()],
            }
            .is_failure()
        );
    }

    #[test]
    fn empty_snapshot_has_no_content_and_no_skipped() {
        let s = ClipboardSnapshot::empty();
        assert!(!s.had_content);
        assert!(s.sources.is_empty());
        assert!(s.skipped_mimes.is_empty());
    }

    #[test]
    fn outcome_for_skipped_returns_partial_when_dropped_mimes_present() {
        let snap = ClipboardSnapshot {
            had_content: true,
            sources: vec![MimeSourceEntry {
                mime_type: "text/plain".to_string(),
                data: b"hello".to_vec(),
            }],
            skipped_mimes: vec![
                "text/_moz_htmlcontext".to_string(),
                "SAVE_TARGETS".to_string(),
            ],
        };
        match outcome_for_skipped(&snap) {
            PasteOutcome::Partial { skipped } => {
                assert_eq!(skipped.len(), 2);
                assert!(skipped.contains(&"SAVE_TARGETS".to_string()));
            }
            other => panic!("expected Partial, got {:?}", other),
        }
    }

    #[test]
    fn outcome_for_skipped_returns_ok_when_no_drops() {
        let snap = ClipboardSnapshot {
            had_content: true,
            sources: vec![MimeSourceEntry {
                mime_type: "text/plain".to_string(),
                data: b"hello".to_vec(),
            }],
            skipped_mimes: Vec::new(),
        };
        assert_eq!(outcome_for_skipped(&snap), PasteOutcome::Ok);
    }

    #[test]
    fn outcome_for_skipped_returns_ok_for_empty_snapshot() {
        let snap = ClipboardSnapshot::empty();
        assert_eq!(outcome_for_skipped(&snap), PasteOutcome::Ok);
    }

    #[test]
    fn snapshot_preserves_multiple_mime_types() {
        let snap = ClipboardSnapshot {
            had_content: true,
            sources: vec![
                MimeSourceEntry {
                    mime_type: "text/plain".to_string(),
                    data: b"hola".to_vec(),
                },
                MimeSourceEntry {
                    mime_type: "text/html".to_string(),
                    data: b"<b>hola</b>".to_vec(),
                },
                MimeSourceEntry {
                    mime_type: "image/png".to_string(),
                    data: vec![0x89, 0x50, 0x4e, 0x47],
                },
            ],
            skipped_mimes: Vec::new(),
        };
        assert_eq!(snap.sources.len(), 3);
        assert!(snap.had_content);
    }

    #[test]
    fn parse_shortcut_known_combo_yields_correct_args() {
        let args = parse_shortcut("ctrl+shift+v");
        assert_eq!(
            args,
            vec![
                "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl",
            ]
        );
    }

    #[test]
    fn parse_shortcut_single_key_yields_press_release() {
        let args = parse_shortcut("ctrl+v");
        assert_eq!(args, vec!["-M", "ctrl", "-k", "v", "-m", "ctrl"]);
    }

    #[test]
    fn parse_shortcut_invalid_falls_back_to_ctrl_v() {
        let args = parse_shortcut("ctrl+??");
        assert_eq!(args, vec!["-M", "ctrl", "-k", "v", "-m", "ctrl"]);
    }

    #[test]
    fn parse_shortcut_insert_normalizes_to_xkb_canonical() {
        let args = parse_shortcut("shift+insert");
        assert_eq!(args, vec!["-M", "shift", "-k", "Insert", "-m", "shift"]);
    }

    #[test]
    fn parse_shortcut_function_key_passes_through() {
        let args = parse_shortcut("ctrl+F5");
        assert_eq!(args, vec!["-M", "ctrl", "-k", "F5", "-m", "ctrl"]);
    }

    #[test]
    fn normalize_key_name_aliases() {
        assert_eq!(normalize_key_name("insert"), Some("Insert".to_string()));
        assert_eq!(normalize_key_name("DEL"), Some("Delete".to_string()));
        assert_eq!(normalize_key_name("pgup"), Some("Page_Up".to_string()));
        assert_eq!(normalize_key_name("pgdn"), Some("Page_Down".to_string()));
        assert_eq!(normalize_key_name("Return"), Some("Return".to_string()));
        assert_eq!(normalize_key_name("space"), Some("space".to_string()));
        assert_eq!(normalize_key_name("v"), Some("v".to_string()));
        assert_eq!(normalize_key_name("F12"), Some("F12".to_string()));
        assert_eq!(normalize_key_name("F25"), None);
        assert_eq!(normalize_key_name("nonsense"), None);
    }
}
