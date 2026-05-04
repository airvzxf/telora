use log::{error, info};
use std::process::Command;

pub fn type_text(text: &str) {
    if text.trim().is_empty() {
        return;
    }

    // wtype
    let res = Command::new("wtype").arg(text).output();

    match res {
        Ok(_) => {}
        Err(e) => {
            error!("wtype failed: {}. Trying clipboard fallback.", e);
            copy_text(text);
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
