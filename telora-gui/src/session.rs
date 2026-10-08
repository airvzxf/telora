//! Recording-session state machine for the GUI, free of GTK and sockets.
//!
//! Events go in, side effects come out; `main.rs` performs the effects.
//! The GUI only shows "recording" after the daemon has accepted START.

use crate::tray::TrayState;

pub const COLOR_RECORDING: &str = "red";
pub const COLOR_BUSY: &str = "orange";
pub const COLOR_OK: &str = "green";
pub const COLOR_ERROR: &str = "#b00020";
pub const COLOR_NEUTRAL: &str = "gray";

/// Seconds an error stays on screen.
pub const ERROR_HOLD_SECS: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// START sent, waiting for the daemon to accept it.
    Starting,
    Recording,
    /// STOP sent, waiting for the transcription.
    Processing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Toggle,
    AutoStop,
    Cancel,
    StartAccepted,
    StartRejected(String),
    /// The STOP round trip ended. `error` is set when no text was delivered.
    Finished {
        message: String,
        color: String,
        hold_secs: u32,
        error: bool,
    },
    /// A message for the user (e.g. daemon status); only shown when idle.
    Info(String),
    /// A delayed hide fired; ignored unless it belongs to the latest OSD.
    HideOsd {
        generation: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    SendStart,
    SendStop,
    SendCancel,
    ShowOsd {
        text: String,
        color: String,
    },
    /// Hide the OSD after `secs`, unless a newer OSD replaced it meanwhile.
    HideOsdAfter {
        secs: u32,
        generation: u64,
    },
    HideOsd,
    Tray(TrayState),
}

#[derive(Debug)]
pub struct Session {
    phase: Phase,
    /// Bumped on every OSD change so stale hide timers do nothing.
    osd_generation: u64,
    /// Keep the tray in `Error` after the OSD fades, until the next action.
    last_was_error: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            osd_generation: 0,
            last_was_error: false,
        }
    }
}

impl Session {
    #[cfg(test)]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn handle(&mut self, event: Event) -> Vec<Effect> {
        match (self.phase, event) {
            (Phase::Idle, Event::Toggle) => {
                self.phase = Phase::Starting;
                self.last_was_error = false;
                vec![Effect::SendStart]
            }
            (Phase::Recording, Event::Toggle) => self.stop("Procesando..."),
            (Phase::Recording, Event::AutoStop) => self.stop("⏳ LÍMITE ALCANZADO"),
            (Phase::Starting, Event::StartAccepted) => {
                self.phase = Phase::Recording;
                let mut effects = self.show("● GRABANDO", COLOR_RECORDING);
                effects.push(Effect::Tray(TrayState::Recording));
                effects
            }
            // Cancelled while START was in flight: the daemon may still
            // begin recording, so tell it to stop.
            (Phase::Idle, Event::StartAccepted) => vec![Effect::SendCancel],
            (Phase::Starting, Event::StartRejected(reason)) => {
                self.phase = Phase::Idle;
                self.error(&format!("✘ {reason}"))
            }
            (Phase::Starting | Phase::Recording, Event::Cancel) => {
                self.phase = Phase::Idle;
                let mut effects = vec![Effect::SendCancel];
                effects.extend(self.show("Cancelado", COLOR_NEUTRAL));
                effects.push(Effect::Tray(TrayState::Idle));
                effects.push(self.hide_after(1));
                effects
            }
            (
                Phase::Processing,
                Event::Finished {
                    message,
                    color,
                    hold_secs,
                    error,
                },
            ) => {
                self.phase = Phase::Idle;
                if error {
                    return self.error(&message);
                }
                let mut effects = self.show(&message, &color);
                effects.push(Effect::Tray(TrayState::Idle));
                effects.push(self.hide_after(hold_secs));
                effects
            }
            (Phase::Idle, Event::Info(text)) => {
                let mut effects = self.show(&text, COLOR_NEUTRAL);
                effects.push(self.hide_after(4));
                effects
            }
            (_, Event::HideOsd { generation }) => {
                if generation != self.osd_generation || self.phase != Phase::Idle {
                    return Vec::new();
                }
                let tray = if self.last_was_error {
                    TrayState::Error
                } else {
                    TrayState::Idle
                };
                vec![Effect::HideOsd, Effect::Tray(tray)]
            }
            // Everything else (double toggles while busy, duplicate
            // AUTO_STOP, late results) is ignored on purpose.
            _ => Vec::new(),
        }
    }

    fn stop(&mut self, label: &str) -> Vec<Effect> {
        self.phase = Phase::Processing;
        let mut effects = self.show(label, COLOR_BUSY);
        effects.push(Effect::Tray(TrayState::Processing));
        effects.push(Effect::SendStop);
        effects
    }

    fn error(&mut self, message: &str) -> Vec<Effect> {
        self.last_was_error = true;
        let mut effects = self.show(message, COLOR_ERROR);
        effects.push(Effect::Tray(TrayState::Error));
        effects.push(self.hide_after(ERROR_HOLD_SECS));
        effects
    }

    fn show(&mut self, text: &str, color: &str) -> Vec<Effect> {
        self.osd_generation += 1;
        vec![Effect::ShowOsd {
            text: text.to_string(),
            color: color.to_string(),
        }]
    }

    fn hide_after(&self, secs: u32) -> Effect {
        Effect::HideOsdAfter {
            secs,
            generation: self.osd_generation,
        }
    }
}

/// Shorten a daemon error for the OSD; full text stays in the log.
pub fn short_error(message: &str) -> String {
    const MAX_CHARS: usize = 120;
    let trimmed = message.trim().trim_start_matches("ERROR:").trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(MAX_CHARS - 1).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toggle() -> Event {
        Event::Toggle
    }

    fn sends_start(effects: &[Effect]) -> bool {
        effects.contains(&Effect::SendStart)
    }

    fn shows_text(effects: &[Effect], needle: &str) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, Effect::ShowOsd { text, .. } if text.contains(needle)))
    }

    fn hide_generation(effects: &[Effect]) -> u64 {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::HideOsdAfter { generation, .. } => Some(*generation),
                _ => None,
            })
            .expect("a delayed hide")
    }

    /// The original bug: "● GRABANDO" appeared before the daemon answered.
    #[test]
    fn toggle_does_not_show_recording_before_daemon_accepts() {
        let mut s = Session::default();
        let effects = s.handle(toggle());
        assert!(sends_start(&effects));
        assert!(!shows_text(&effects, "GRABANDO"));
        assert_eq!(s.phase(), Phase::Starting);

        let effects = s.handle(Event::StartAccepted);
        assert!(shows_text(&effects, "GRABANDO"));
        assert!(effects.contains(&Effect::Tray(TrayState::Recording)));
        assert_eq!(s.phase(), Phase::Recording);
    }

    #[test]
    fn rejected_start_shows_error_and_sets_tray_error() {
        let mut s = Session::default();
        s.handle(toggle());
        let effects = s.handle(Event::StartRejected("daemon no disponible".into()));
        assert!(shows_text(&effects, "daemon no disponible"));
        assert!(effects.contains(&Effect::Tray(TrayState::Error)));
        assert_eq!(s.phase(), Phase::Idle);

        // The tray keeps showing the error after the OSD fades.
        let generation = hide_generation(&effects);
        let effects = s.handle(Event::HideOsd { generation });
        assert!(effects.contains(&Effect::Tray(TrayState::Error)));
    }

    #[test]
    fn failed_transcription_is_reported_as_error() {
        let mut s = Session::default();
        s.handle(toggle());
        s.handle(Event::StartAccepted);
        s.handle(toggle());
        let effects = s.handle(Event::Finished {
            message: "✘ no se pudo conectar".into(),
            color: COLOR_ERROR.into(),
            hold_secs: ERROR_HOLD_SECS,
            error: true,
        });
        assert!(effects.contains(&Effect::Tray(TrayState::Error)));
        assert_eq!(s.phase(), Phase::Idle);
    }

    #[test]
    fn successful_cycle_returns_to_idle_tray() {
        let mut s = Session::default();
        s.handle(toggle());
        s.handle(Event::StartAccepted);
        let effects = s.handle(toggle());
        assert!(effects.contains(&Effect::SendStop));
        let effects = s.handle(Event::Finished {
            message: "Copiado".into(),
            color: COLOR_OK.into(),
            hold_secs: 1,
            error: false,
        });
        assert!(effects.contains(&Effect::Tray(TrayState::Idle)));
        let generation = hide_generation(&effects);
        assert_eq!(
            s.handle(Event::HideOsd { generation }),
            vec![Effect::HideOsd, Effect::Tray(TrayState::Idle)]
        );
    }

    /// A hide timer from the previous result must not hide the next recording.
    #[test]
    fn stale_hide_timer_is_ignored() {
        let mut s = Session::default();
        s.handle(toggle());
        let rejected = s.handle(Event::StartRejected("x".into()));
        let stale = hide_generation(&rejected);
        s.handle(toggle());
        s.handle(Event::StartAccepted);
        assert!(s.handle(Event::HideOsd { generation: stale }).is_empty());
        assert_eq!(s.phase(), Phase::Recording);
    }

    #[test]
    fn toggles_while_busy_are_ignored() {
        let mut s = Session::default();
        s.handle(toggle());
        assert!(s.handle(toggle()).is_empty(), "toggle while starting");
        s.handle(Event::StartAccepted);
        s.handle(toggle());
        assert!(s.handle(toggle()).is_empty(), "toggle while processing");
        assert!(s.handle(Event::AutoStop).is_empty(), "duplicate auto-stop");
    }

    #[test]
    fn cancel_during_start_tells_daemon_to_stop_if_it_accepts_late() {
        let mut s = Session::default();
        s.handle(toggle());
        let effects = s.handle(Event::Cancel);
        assert!(effects.contains(&Effect::SendCancel));
        assert_eq!(s.handle(Event::StartAccepted), vec![Effect::SendCancel]);
        assert_eq!(s.phase(), Phase::Idle);
    }

    #[test]
    fn auto_stop_stops_with_limit_message() {
        let mut s = Session::default();
        s.handle(toggle());
        s.handle(Event::StartAccepted);
        let effects = s.handle(Event::AutoStop);
        assert!(shows_text(&effects, "LÍMITE"));
        assert_eq!(s.phase(), Phase::Processing);
    }

    #[test]
    fn info_is_not_shown_over_a_recording() {
        let mut s = Session::default();
        assert!(shows_text(&s.handle(Event::Info("listo".into())), "listo"));
        s.handle(toggle());
        s.handle(Event::StartAccepted);
        assert!(s.handle(Event::Info("listo".into())).is_empty());
    }

    #[test]
    fn short_error_strips_prefix_and_truncates() {
        assert_eq!(short_error("ERROR: sin modelo"), "sin modelo");
        let long = "x".repeat(300);
        assert_eq!(short_error(&long).chars().count(), 120);
    }
}
