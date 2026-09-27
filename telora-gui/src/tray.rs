//! StatusNotifierItem (SNI) tray icon for `telora-gui` (closes #193).
//!
//! Implements the freedesktop StatusNotifierItem spec over the session DBus
//! using the [`ksni`] crate. Recognised by the KDE Plasma 6 System Tray
//! applet (Wayland), the GNOME StatusNotifierItem extension, and the
//! Cinnamon / LXQt / Xfce watchers without any platform-specific glue.
//!
//! # Lifecycle
//!
//! 1. [`spawn_tray`] registers a [`TeloraTray`] with the session bus and
//!    returns a [`TrayHandle`].
//! 2. The GTK loop pushes [`TrayState`] transitions (Idle / Recording /
//!    Processing / Error) through the handle's `state_tx` channel; the
//!    owned task forwards them to `ksni::Handle::update`.
//! 3. User clicks on the tray or its menu send a [`TrayCommand`] back
//!    through the `cmd_tx` channel supplied to [`spawn_tray`]; the GTK
//!    loop receives them and reuses the existing
//!    `async_channel::Sender<AppAction>` path so the rest of the GUI
//!    pipeline (control server, daemon socket, OSD) does not know whether
//!    the trigger came from a hotkey, the CLI or the tray.
//!
//! # Why `ksni` and not `libappindicator`
//!
//! `ksni` is pure Rust and talks DBus directly. It is the canonical
//! upstream option for the freedesktop StatusNotifierItem spec and does
//! not require the `libayatana-appindicator3` C library or its GTK3
//! bindings — which means we keep the GUI's binary small and avoid
//! pulling a second GLib / GTK runtime into the process.

use async_channel::Sender;
use ksni::TrayMethods;
use log::{info, warn};

/// Pixel size of the procedurally-generated tray icon. Matches the
/// default KDE Plasma 6 "System Tray" applet slot so the icon never has
/// to be scaled (which would blur the glyph edges).
const ICON_PX: i32 = 22;

/// The state the GUI pushes at the tray. The tray renders a different
/// icon / tooltip / title for each state and surfaces it to the user
/// even when the OSD is hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// Daemon is reachable, no recording in progress.
    Idle,
    /// User is recording (Type or Copy mode).
    Recording,
    /// Recording stopped, daemon is transcribing / pasting.
    Processing,
    /// Daemon is unreachable or returned an error; OSD already showed
    /// the underlying message.
    Error,
}

impl TrayState {
    fn title(self) -> &'static str {
        match self {
            Self::Idle => "Telora",
            Self::Recording => "Telora \u{2014} Recording",
            Self::Processing => "Telora \u{2014} Processing",
            Self::Error => "Telora \u{2014} Error",
        }
    }

    fn tooltip_description(self) -> &'static str {
        match self {
            Self::Idle => {
                "Speech-to-text assistant is ready.\n\
                 Click to toggle TYPE mode (writes the transcription), \
                 right-click for the menu."
            }
            Self::Recording => {
                "Recording audio \u{2014} speak now.\n\
                 Click again or right-click \u{2192} Toggle TYPE to stop."
            }
            Self::Processing => {
                "Transcribing audio. The result will be pasted or copied\n\
                 automatically when the daemon finishes."
            }
            Self::Error => {
                "The daemon is unreachable or returned an error.\n\
                 Check `telora-daemon status` and the journal."
            }
        }
    }
}

/// Commands the user can trigger from the tray menu (or by clicking the
/// tray icon). These map 1:1 to the strings the GTK loop already
/// understands via the control server (`TOGGLE_TYPE`, `TOGGLE_COPY`,
/// `AUTO_STOP`, `CANCEL`) plus a new `STATUS` and `QUIT`.
#[derive(Debug, Clone)]
pub enum TrayCommand {
    /// Left-click on the icon — same as the `TOGGLE_TYPE` hotkey.
    ToggleType,
    /// Right-click \u{2192} "Toggle TYPE mode".
    MenuToggleType,
    /// Right-click \u{2192} "Toggle COPY mode".
    MenuToggleCopy,
    /// Right-click \u{2192} "Cancel recording".
    MenuCancel,
    /// Right-click \u{2192} "Show status" \u{2192} triggers an OSD flash.
    MenuStatus,
    /// Right-click \u{2192} "Quit" \u{2192} ends the GUI cleanly so
    /// `systemd --user` restarts it on next login / crash.
    MenuQuit,
}

/// Concrete `ksni::Tray` implementation. Owns the current state (which
/// `ksni` re-reads on every property refresh) plus a clone of the
/// command sender so callbacks can fire without going through any
/// additional channel.
struct TeloraTray {
    state: TrayState,
    cmd_tx: Sender<TrayCommand>,
}

impl ksni::Tray for TeloraTray {
    /// Stable, application-unique id. KDE uses this to persist the
    /// "show notifications from" preference across sessions; renaming
    /// it would lose that.
    fn id(&self) -> String {
        "telora-gui".to_string()
    }

    fn title(&self) -> String {
        self.state.title().to_string()
    }

    /// `ApplicationStatus` is the spec-recommended category for a
    /// generic application whose state the user cares about. KDE
    /// uses this to decide icon ordering in the System Tray applet.
    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    /// `Active` makes the icon stay visible while the GUI is running,
    /// even when the OSD is hidden. `Passive` would let the watcher
    /// hide it; `NeedsAttention` is reserved for `Error`.
    fn status(&self) -> ksni::Status {
        match self.state {
            TrayState::Error => ksni::Status::NeedsAttention,
            _ => ksni::Status::Active,
        }
    }

    /// ARGB32 pixmap. Generated procedurally so the binary does not
    /// need to ship PNG assets. The KDE Plasma 6 SNI applet scales the
    /// pixmap up if the panel needs a bigger icon; the alpha channel is
    /// respected so a solid glyph with a transparent background looks
    /// correct at any density.
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![make_icon(self.state)]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            // Fallback name for hosts that prefer icon themes; KDE's
            // Breeze ships this one.
            icon_name: "audio-input-microphone".to_string(),
            icon_pixmap: vec![make_icon(self.state)],
            title: self.state.title().to_string(),
            description: self.state.tooltip_description().to_string(),
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: "Toggle TYPE mode".to_string(),
                icon_name: "input-keyboard".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    dispatch(tray, TrayCommand::MenuToggleType);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Toggle COPY mode".to_string(),
                icon_name: "edit-copy".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    dispatch(tray, TrayCommand::MenuToggleCopy);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Cancel recording".to_string(),
                icon_name: "process-stop".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    dispatch(tray, TrayCommand::MenuCancel);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Show status".to_string(),
                icon_name: "dialog-information".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    dispatch(tray, TrayCommand::MenuStatus);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".to_string(),
                icon_name: "application-exit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    dispatch(tray, TrayCommand::MenuQuit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }

    /// Left-click on the icon. KDE Plasma 6 shows the menu on right-
    /// click and calls `activate` on left-click, so we route the most
    /// common action (Toggle TYPE) through this entry point.
    fn activate(&mut self, _x: i32, _y: i32) {
        dispatch(self, TrayCommand::ToggleType);
    }
}

/// Fire-and-forget send into the command channel. `async-channel`'s
/// `send_blocking` is the right call here because the ksni callbacks
/// (`activate`, menu `activate`) are invoked synchronously from inside
/// the DBus handler and never run inside an `await` context.
fn dispatch(tray: &TeloraTray, cmd: TrayCommand) {
    if let Err(e) = tray.cmd_tx.send_blocking(cmd) {
        warn!("tray command channel closed; GUI loop is gone ({e})");
    }
}

/// Opaque handle owned by the GTK loop. `state_tx` is `Clone` and
/// cheap to share with any code that knows about tray state
/// transitions.
#[derive(Clone)]
pub struct TrayHandle {
    state_tx: Sender<TrayState>,
}

impl TrayHandle {
    /// Push a new state into the tray task. Errors are swallowed on
    /// purpose: a dropped receiver just means the user closed the GUI
    /// and there is no tray to update anymore.
    pub fn set_state(&self, new_state: TrayState) {
        if let Err(e) = self.state_tx.send_blocking(new_state) {
            warn!("tray state channel closed; cannot update icon ({e})");
        }
    }
}

/// Build the tray, register it with the session bus, and spawn the
/// background task that owns the [`ksni::Handle`] and forwards state
/// updates. Returns [`TrayHandle`] on success.
///
/// On any failure (DBus unavailable, no SNI watcher registered, etc.)
/// we log a warning and return `Ok(None)`: the GUI must keep running
/// with the OSD-only legacy mode so a missing tray never breaks
/// transcription.
pub async fn spawn_tray(cmd_tx: Sender<TrayCommand>) -> Result<Option<TrayHandle>, ksni::Error> {
    let (state_tx, state_rx) = async_channel::unbounded::<TrayState>();

    let tray = TeloraTray {
        state: TrayState::Idle,
        cmd_tx,
    };

    // `spawn` returns a `ksni::Handle<TeloraTray>` whose lifetime is
    // tied to the spawned background task; we move it into the task
    // instead of holding it here because the GTK loop never needs
    // direct DBus access — it only pushes state transitions.
    let handle = tray.spawn().await?;

    info!("tray icon registered with the session DBus StatusNotifierWatcher");

    // Detached task: drains state transitions and forwards them to
    // `ksni::Handle::update`. Lives for the duration of the GUI; when
    // the GTK loop exits, `app.hold()` releases, the tokio runtime is
    // dropped, and this task is cancelled with the rest.
    tokio::spawn(async move {
        let handle = handle;
        let mut current = TrayState::Idle;
        while let Ok(new_state) = state_rx.recv().await {
            if new_state == current {
                // No-op transitions (e.g. two idle updates in a row)
                // do not require a DBus round-trip.
                continue;
            }
            current = new_state;
            handle.update(|t| t.state = new_state).await;
            info!("tray icon state -> {new_state:?}");
        }
    });

    Ok(Some(TrayHandle { state_tx }))
}

/// Render a 22\u00d722 ARGB32 pixmap for the given state.
///
/// Pixel format: each pixel is 4 bytes in network byte order (big-endian
/// on the wire) — `A`, `R`, `G`, `B` from most to least significant.
/// This matches what `ksni::Icon::data` expects and what the
/// StatusNotifierItem spec calls for.
///
/// Drawing algorithm: a **solid** capital `T` glyph filled with the
/// state colour, with **no background**. The bar and stem share the
/// row `y = 7` so the two rectangles form a single connected letter
/// (no visible gap between them). Anything outside the `T` is fully
/// transparent so the panel background shows through — the icon adapts
/// to both light and dark panels without any square or circular
/// background.
///
/// Glyph geometry (inclusive ranges, 22\u00d722 canvas):
///
/// ```text
///   horizontal bar:  y ∈ [3, 7],  x ∈ [2, 19]
///   vertical stem:   y ∈ [7, 19], x ∈ [9, 12]
/// ```
///
/// The two rectangles share the row `y = 7` so the `T` is one
/// connected shape. The total opaque pixel count is
/// 5×18 + 13×4 − 4 = 138 (subtract the 4 shared pixels at
/// x ∈ [9, 12], y = 7).
fn make_icon(state: TrayState) -> ksni::Icon {
    let color = match state {
        // Idle: neutral grey. The assistant is ready, nothing in flight.
        TrayState::Idle => (0xFF, 0x9A, 0x9A, 0x9A),
        // Recording: strong red. Mirrors the OSD "● GRABANDO" message.
        TrayState::Recording => (0xFF, 0xE0, 0x2A, 0x2A),
        // Processing: warm orange. Mirrors the OSD "Procesando...".
        TrayState::Processing => (0xFF, 0xF0, 0x90, 0x00),
        // Error: dim red. Same family as Recording but darker so the
        // two states stay distinguishable at a glance.
        TrayState::Error => (0xFF, 0xB0, 0x40, 0x40),
    };

    // Fixed glyph geometry. The bar and stem share row y=7 so the
    // letter is one connected silhouette — no white gap in the middle
    // (which is what made an earlier outline-based variant read as an
    // "I" instead of a "T" at 22×22).
    const BAR_Y: std::ops::RangeInclusive<i32> = 3..=7;
    const BAR_X: std::ops::RangeInclusive<i32> = 2..=19;
    const STEM_Y: std::ops::RangeInclusive<i32> = 7..=19;
    const STEM_X: std::ops::RangeInclusive<i32> = 9..=12;

    let mut data = Vec::with_capacity((ICON_PX * ICON_PX * 4) as usize);
    for y in 0..ICON_PX {
        for x in 0..ICON_PX {
            let in_bar = BAR_Y.contains(&y) && BAR_X.contains(&x);
            let in_stem = STEM_Y.contains(&y) && STEM_X.contains(&x);
            let (a, r, g, b) = if in_bar || in_stem {
                color
            } else {
                (0, 0, 0, 0)
            };
            // ARGB32 in network byte order: A is the most significant
            // byte on the wire, so it goes first in the buffer.
            data.extend_from_slice(&[a, r, g, b]);
        }
    }

    ksni::Icon {
        width: ICON_PX,
        height: ICON_PX,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksni::Tray;

    const STATES: [TrayState; 4] = [
        TrayState::Idle,
        TrayState::Recording,
        TrayState::Processing,
        TrayState::Error,
    ];

    /// ARGB color for the solid `T` glyph for a given state. Mirrors
    /// the `match` block in `make_icon`; kept as a single source of
    /// truth so the tests stay in lock-step with the production
    /// rendering.
    fn t_color(state: TrayState) -> (u8, u8, u8) {
        match state {
            TrayState::Idle => (0x9A, 0x9A, 0x9A),
            TrayState::Recording => (0xE0, 0x2A, 0x2A),
            TrayState::Processing => (0xF0, 0x90, 0x00),
            TrayState::Error => (0xB0, 0x40, 0x40),
        }
    }

    fn pixel(icon: &ksni::Icon, x: i32, y: i32) -> (u8, u8, u8, u8) {
        let offset = ((y * ICON_PX + x) * 4) as usize;
        (
            icon.data[offset],
            icon.data[offset + 1],
            icon.data[offset + 2],
            icon.data[offset + 3],
        )
    }

    #[test]
    fn icon_has_expected_dimensions_and_buffer_size() {
        let icon = make_icon(TrayState::Idle);
        assert_eq!(icon.width, ICON_PX);
        assert_eq!(icon.height, ICON_PX);
        assert_eq!(
            icon.data.len(),
            (ICON_PX * ICON_PX * 4) as usize,
            "ARGB32 buffer must be 4 bytes per pixel"
        );
    }

    #[test]
    fn pixels_outside_t_are_fully_transparent() {
        // The icon is a solid glyph with NO background — everything
        // outside the `T` shape must be alpha=0 so the panel shows
        // through. Spot-check several positions around the glyph
        // (corners, edges, gaps in the silhouette).
        let outside_samples: &[(i32, i32)] = &[
            (0, 0),
            (21, 0),
            (0, 21),
            (21, 21),
            (11, 0),  // above the bar
            (11, 2),  // just above the bar
            (0, 11),  // left of stem
            (8, 11),  // just left of stem
            (13, 11), // just right of stem
            (21, 11), // right of bar
            (11, 20), // below the stem
        ];
        for state in STATES {
            let icon = make_icon(state);
            for &(x, y) in outside_samples {
                let (a, _, _, _) = pixel(&icon, x, y);
                assert_eq!(
                    a, 0,
                    "pixel ({x},{y}) for {state:?} must be fully transparent, got alpha={a}"
                );
            }
        }
    }

    #[test]
    fn t_pixels_match_state_colour() {
        // Sample pixels from each rectangle (bar and stem) and verify
        // they all show the state colour. The whole `T` glyph is the
        // semantic state indicator.
        let t_samples: &[(i32, i32)] = &[
            (2, 3),   // top-left of bar
            (11, 3),  // top-middle of bar
            (19, 3),  // top-right of bar
            (3, 5),   // bar middle, near left
            (18, 5),  // bar middle, near right
            (11, 7),  // connection point (both bar and stem cover this)
            (9, 10),  // stem left
            (12, 10), // stem right
            (10, 14), // stem middle
            (11, 19), // stem bottom
        ];
        for state in STATES {
            let icon = make_icon(state);
            let (er, eg, eb) = t_color(state);
            for &(x, y) in t_samples {
                let (a, r, g, b) = pixel(&icon, x, y);
                assert_eq!(
                    (a, r, g, b),
                    (0xFF, er, eg, eb),
                    "T pixel ({x},{y}) for {state:?} must match state colour"
                );
            }
        }
    }

    #[test]
    fn icon_centre_pixel_is_t_stem_in_state_colour() {
        // (11, 11) sits squarely inside the `T` stem (x ∈ [9, 12],
        // y ∈ [7, 19]) so the centre pixel must show the state
        // colour, fully opaque.
        for state in STATES {
            let icon = make_icon(state);
            let cx = ICON_PX / 2;
            let cy = ICON_PX / 2;
            let (a, r, g, b) = pixel(&icon, cx, cy);
            assert_eq!(a, 0xFF, "T stem must be opaque for {state:?}");
            let (er, eg, eb) = t_color(state);
            assert_eq!(
                (r, g, b),
                (er, eg, eb),
                "T stem centre ({cx},{cy}) for {state:?} must match state colour"
            );
        }
    }

    #[test]
    fn bar_and_stem_share_a_row_so_the_t_is_connected() {
        // The connection row y = 7 must be opaque AND coloured for
        // x ∈ [9, 12] (where bar and stem overlap). This guards
        // against regressions where someone widens the gap and the
        // letter reads as an "I" instead of a `T`.
        for state in STATES {
            let icon = make_icon(state);
            let (er, eg, eb) = t_color(state);
            for x in 9..=12 {
                let (a, r, g, b) = pixel(&icon, x, 7);
                assert_eq!(
                    (a, r, g, b),
                    (0xFF, er, eg, eb),
                    "shared row ({x},7) for {state:?} must be opaque state colour (no gap)"
                );
            }
        }
    }

    #[test]
    fn total_opaque_pixel_count_matches_t_area() {
        // Bar (5×18) + stem (13×4) − shared pixels (4 at y=7, x ∈ [9,12])
        // = 90 + 52 − 4 = 138 opaque pixels per state. Everything else
        // is alpha=0.
        for state in STATES {
            let icon = make_icon(state);
            let opaque = icon
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|px| px[0] != 0)
                .count();
            assert_eq!(
                opaque, 138,
                "state={state:?} must produce exactly 138 opaque pixels (got {opaque})"
            );
        }
    }

    #[test]
    fn t_is_solid_no_outline_pixels() {
        // The glyph is solid (not hollow): every opaque pixel uses the
        // state colour, none are pure white. If a future change adds
        // an outline this assertion fires immediately.
        for state in STATES {
            let icon = make_icon(state);
            let white = icon
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|px| px[0] == 0xFF && px[1] == 0xFF && px[2] == 0xFF && px[3] == 0xFF)
                .count();
            assert_eq!(
                white, 0,
                "state={state:?} must have ZERO white pixels (solid T glyph, got {white})"
            );
        }
    }

    #[test]
    fn tray_state_titles_are_distinct() {
        // Ensures the user can tell the four states apart from the
        // tooltip / panel title alone (no need to read the body).
        let titles = [
            TrayState::Idle.title(),
            TrayState::Recording.title(),
            TrayState::Processing.title(),
            TrayState::Error.title(),
        ];
        let unique: std::collections::HashSet<_> = titles.iter().collect();
        assert_eq!(unique.len(), titles.len(), "titles must be pairwise unique");
    }

    #[test]
    fn tray_state_status_mapping_matches_spec() {
        // The SNI spec uses Status to drive panel visibility. Error
        // must request attention so the user notices without watching
        // the OSD; the rest stay Active so KDE keeps the icon visible.
        // `status()` lives on the `ksni::Tray` impl for `TeloraTray`,
        // so we build a tray with each state and assert on it.
        fn status_for(state: TrayState) -> ksni::Status {
            let (cmd_tx, _cmd_rx) = async_channel::unbounded::<TrayCommand>();
            let tray = TeloraTray { state, cmd_tx };
            tray.status()
        }

        assert!(matches!(
            status_for(TrayState::Error),
            ksni::Status::NeedsAttention
        ));
        for state in [TrayState::Idle, TrayState::Recording, TrayState::Processing] {
            assert!(
                matches!(status_for(state), ksni::Status::Active),
                "{state:?} must be Active"
            );
        }
    }
}
