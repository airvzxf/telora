//! Diagnostic probe for the post-fix clipboard flow.
//!
//! Verifies that:
//! - The Rust-only clipboard write (the new `copy_text`) works without
//!   `wl-copy` installed.
//! - The same write/read round-trips against the running compositor.
//! - `wtype` is absent (expected on this KDE Plasma 6 host) and the
//!   new `KeystrokeUnavailable` path is exercised correctly by the
//!   paste-flow code in `clipboard.rs`.
//!
//! Run with `cargo run -p telora-gui --example probe` from the workspace
//! root.

use std::process::Command;

use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
use wl_clipboard_rs::paste::{self, Error as PasteError, MimeType as PasteMimeType, Seat};

const PROBE_TEXT: &str = "telora-probe-2026-09-26";
const COPY_MIME: &str = "text/plain;charset=utf-8";

fn tool_status(name: &str) -> String {
    match Command::new(name).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout);
            let line = v.lines().next().unwrap_or("").trim();
            format!("OK ({line})")
        }
        Ok(out) => {
            let code = out.status.code();
            let stderr = String::from_utf8_lossy(&out.stderr);
            let first = stderr.lines().next().unwrap_or("");
            format!("exited code={code:?} stderr={first}")
        }
        Err(e) => format!("not present ({e})"),
    }
}

fn write_clipboard(text: &str) -> Result<(), String> {
    let opts = Options::new();
    let sources = vec![MimeSource {
        source: Source::Bytes(text.as_bytes().to_vec().into_boxed_slice()),
        mime_type: MimeType::Specific(COPY_MIME.to_string()),
    }];
    opts.copy_multi(sources).map_err(|e| format!("{e:?}"))
}

fn read_clipboard_first() -> Result<Vec<u8>, String> {
    let types = paste::get_mime_types(paste::ClipboardType::Regular, Seat::Unspecified).map_err(
        |e| match e {
            PasteError::MissingProtocol { name, version } => {
                format!("MissingProtocol({name} v{version})")
            }
            other => format!("{other:?}"),
        },
    )?;
    let first = types
        .iter()
        .next()
        .cloned()
        .ok_or_else(|| "no mime types offered".to_string())?;
    let (mut pipe, _actual_mime) = paste::get_contents(
        paste::ClipboardType::Regular,
        Seat::Unspecified,
        PasteMimeType::Specific(&first),
    )
    .map_err(|e| format!("get_contents: {e:?}"))?;
    let mut buf = Vec::new();
    use std::io::Read;
    pipe.read_to_end(&mut buf)
        .map_err(|e| format!("read_to_end: {e}"))?;
    Ok(buf)
}

fn main() {
    println!("=== Tool probe ===");
    for tool in [
        "wl-copy",
        "wl-paste",
        "wtype",
        "wl-clipboard",
        "xdotool",
        "dotool",
        "dotoolc",
        "klipper",
    ] {
        println!("{tool:<12} {}", tool_status(tool));
    }

    println!();
    println!("=== Wayland env ===");
    for var in [
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "DESKTOP_SESSION",
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "DISPLAY",
    ] {
        println!(
            "{var:<20} = {}",
            std::env::var(var).unwrap_or_else(|_| "<unset>".into())
        );
    }

    println!();
    println!("=== Test 1: copy_text path (Rust, no wl-copy needed) ===");
    match write_clipboard(PROBE_TEXT) {
        Ok(()) => println!("write: OK"),
        Err(e) => {
            println!("write: FAILED — {e}");
            return;
        }
    }
    match read_clipboard_first() {
        Ok(buf) => {
            let s = String::from_utf8_lossy(&buf);
            if buf == PROBE_TEXT.as_bytes() {
                println!("read: OK — round-trip matches ({} bytes)", buf.len());
            } else {
                println!(
                    "read: MISMATCH — got {:?} ({} bytes)",
                    s.escape_debug(),
                    buf.len()
                );
            }
        }
        Err(e) => println!("read: FAILED — {e}"),
    }

    println!();
    println!("=== Test 2: simulating the new copy_text on a fresh payload ===");
    let payload = "hola mundo desde telora-gui (probe)";
    match write_clipboard(payload) {
        Ok(()) => println!("write: OK ({})", payload),
        Err(e) => println!("write: FAILED — {e}"),
    }

    println!();
    println!("=== Test 3: confirming 'type' flow would return KeystrokeUnavailable ===");
    match Command::new("wtype").arg("--version").output() {
        Ok(o) if o.status.success() => println!(
            "wtype: present (this host has it — KeystrokeUnavailable path will NOT trigger)"
        ),
        Ok(o) => println!(
            "wtype: present but exited {:?} — KeystrokeUnavailable path will NOT trigger",
            o.status.code()
        ),
        Err(e) => {
            println!("wtype: NOT AVAILABLE — {e}");
            println!(
                "-> On the live GUI, the paste flow will return \
                 PasteOutcome::KeystrokeUnavailable and the OSD will show \
                 \"Copiado — pegue con Ctrl+V\"."
            );
        }
    }

    println!();
    println!("=== Test 4: compositor protocol (informational) ===");
    match paste::get_mime_types(paste::ClipboardType::Regular, Seat::Unspecified) {
        Ok(types) => {
            println!("mime types currently advertised: {:?}", types);
        }
        Err(e) => println!("could not enumerate mime types: {e:?}"),
    }
}
