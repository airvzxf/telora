//! Recording/transcription state machine and the loop that drives it.
//!
//! [`Daemon`] is synchronous and does no I/O: socket commands, audio ticks
//! and finished background work come in as [`Event`]s, and the slow work
//! (transcribing, loading a model, telling the GUI about an auto-stop)
//! goes out as [`Effect`]s. [`run`] executes those effects on tokio and
//! feeds their outcome back as events.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use log::{error, info, warn};
use ringbuf::{Consumer, HeapRb};
use tokio::sync::{mpsc, oneshot, watch};

use crate::socket::{Command, StatusResponse, SttConfig};
use crate::transcriber::Transcriber;

/// Samples per second the engines expect and the buffer is sized for.
const SAMPLE_RATE: usize = 16_000;

/// Where captured samples come from.
pub trait AudioSource: Send {
    /// Throw away everything captured so far.
    fn discard(&mut self);
    /// Move captured samples into `buffer` until it holds `limit` samples.
    fn drain_into(&mut self, buffer: &mut Vec<f32>, limit: usize);
}

impl AudioSource for Consumer<f32, Arc<HeapRb<f32>>> {
    fn discard(&mut self) {
        while self.pop().is_some() {}
    }

    fn drain_into(&mut self, buffer: &mut Vec<f32>, limit: usize) {
        while buffer.len() < limit {
            match self.pop() {
                Some(sample) => buffer.push(sample),
                None => break,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Recording,
    Processing,
}

/// The model loads after the socket is up, so the daemon can be running
/// without a usable engine.
pub enum Engine {
    Loading,
    Ready(Arc<dyn Transcriber>),
    Failed(String),
}

impl Engine {
    fn status_label(&self) -> String {
        match self {
            Engine::Loading => "loading".to_string(),
            Engine::Ready(_) => "ready".to_string(),
            Engine::Failed(e) => format!("failed: {e}"),
        }
    }

    /// Why a recording cannot start, or `None` when the engine is usable.
    fn unavailable_reason(&self) -> Option<String> {
        match self {
            Engine::Loading => Some("el modelo todavía se está cargando".to_string()),
            Engine::Ready(_) => None,
            Engine::Failed(e) => Some(format!("no se pudo cargar el modelo: {e}")),
        }
    }
}

/// A loaded engine plus the on-disk model path, or why loading failed.
pub type LoadResult = Result<(Arc<dyn Transcriber>, String), String>;

pub enum Event {
    Command(Command),
    /// Periodic audio drain.
    Tick,
    TranscriptionDone {
        job: u64,
        text: String,
    },
    EngineLoaded {
        generation: u64,
        result: LoadResult,
    },
}

pub enum Effect {
    Transcribe {
        job: u64,
        engine: Arc<dyn Transcriber>,
        audio: Vec<f32>,
        language: String,
    },
    LoadEngine {
        generation: u64,
        config: SttConfig,
    },
    /// The recording hit `max_recording_seconds`; the GUI must be told
    /// so it collects the result with STOP.
    NotifyAutoStop,
}

pub struct Daemon<A: AudioSource> {
    audio: A,
    phase: Phase,
    buffer: Vec<f32>,
    config: SttConfig,
    engine: Engine,
    /// Bumped on every transcription and on CANCEL, so a result from a
    /// cancelled job is recognised and dropped.
    job: u64,
    stop_waiter: Option<oneshot::Sender<String>>,
    /// A result nobody asked for yet (auto-stop finished before STOP).
    unclaimed_result: Option<String>,
    /// Bumped on every model load, so a slower, older load cannot
    /// overwrite a newer one.
    load_generation: u64,
    reload_waiters: Vec<oneshot::Sender<anyhow::Result<()>>>,
}

impl<A: AudioSource> Daemon<A> {
    /// Returns the daemon plus the effect that loads the configured model.
    pub fn new(audio: A, config: SttConfig) -> (Self, Effect) {
        let daemon = Self {
            audio,
            phase: Phase::Idle,
            buffer: Vec::new(),
            config: config.clone(),
            engine: Engine::Loading,
            job: 0,
            stop_waiter: None,
            unclaimed_result: None,
            load_generation: 1,
            reload_waiters: Vec::new(),
        };
        (
            daemon,
            Effect::LoadEngine {
                generation: 1,
                config,
            },
        )
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn handle(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Command(cmd) => self.handle_command(cmd),
            Event::Tick => self.tick(),
            Event::TranscriptionDone { job, text } => {
                if job == self.job && self.phase == Phase::Processing {
                    self.finish(text);
                } else {
                    info!("Dropping result of cancelled transcription job {job}");
                }
                Vec::new()
            }
            Event::EngineLoaded { generation, result } => {
                self.engine_loaded(generation, result);
                Vec::new()
            }
        }
    }

    fn handle_command(&mut self, cmd: Command) -> Vec<Effect> {
        match cmd {
            Command::Start { response_tx } => {
                info!("Command: START");
                let refusal = if self.phase == Phase::Processing {
                    Some("todavía se está transcribiendo la grabación anterior".to_string())
                } else {
                    self.engine.unavailable_reason()
                };
                if let Some(reason) = refusal {
                    warn!("START refused: {reason}");
                    let _ = response_tx.send(Err(reason));
                } else {
                    // A START while recording restarts the capture.
                    self.audio.discard();
                    self.buffer.clear();
                    self.unclaimed_result = None;
                    self.phase = Phase::Recording;
                    let _ = response_tx.send(Ok(()));
                }
                Vec::new()
            }
            Command::Stop { response_tx } => {
                info!("Command: STOP");
                match self.phase {
                    Phase::Recording => {
                        let limit = self.sample_limit();
                        self.audio.drain_into(&mut self.buffer, limit);
                        self.stop_waiter = Some(response_tx);
                        self.begin_transcription().into_iter().collect()
                    }
                    Phase::Processing => {
                        if let Some(previous) = self.stop_waiter.replace(response_tx) {
                            let _ = previous
                                .send("ERROR: otro cliente pidió el mismo resultado".to_string());
                        }
                        Vec::new()
                    }
                    Phase::Idle => {
                        let _ = response_tx.send(self.unclaimed_result.take().unwrap_or_default());
                        Vec::new()
                    }
                }
            }
            Command::Cancel => {
                info!("Command: CANCEL");
                if self.phase == Phase::Processing {
                    self.job += 1;
                    if let Some(tx) = self.stop_waiter.take() {
                        let _ = tx.send("ERROR: transcripción cancelada".to_string());
                    }
                }
                self.phase = Phase::Idle;
                self.buffer.clear();
                self.unclaimed_result = None;
                Vec::new()
            }
            Command::GetStatus { response_tx } => {
                let _ = response_tx.send(self.status());
                Vec::new()
            }
            Command::ReloadConfig {
                new_config,
                response_tx,
            } => self.reload(new_config, response_tx).into_iter().collect(),
        }
    }

    fn tick(&mut self) -> Vec<Effect> {
        if self.phase != Phase::Recording {
            // Keep the ring buffer from filling up while not recording.
            self.audio.discard();
            return Vec::new();
        }
        let limit = self.sample_limit();
        self.audio.drain_into(&mut self.buffer, limit);
        if self.buffer.len() < limit {
            return Vec::new();
        }
        warn!(
            "Recording limit reached ({}s); stopping automatically.",
            self.config.max_recording_seconds
        );
        let mut effects: Vec<Effect> = self.begin_transcription().into_iter().collect();
        effects.push(Effect::NotifyAutoStop);
        effects
    }

    fn sample_limit(&self) -> usize {
        SAMPLE_RATE * self.config.max_recording_seconds as usize
    }

    /// Hand the recorded audio to the engine, or finish right away when
    /// there is nothing to transcribe.
    fn begin_transcription(&mut self) -> Option<Effect> {
        self.job += 1;
        self.phase = Phase::Processing;
        let audio = std::mem::take(&mut self.buffer);
        info!("Processing {} samples (job {})", audio.len(), self.job);
        match &self.engine {
            Engine::Ready(_) if audio.is_empty() => {
                warn!("Audio buffer empty, skipping transcription.");
                self.finish(String::new());
                None
            }
            Engine::Ready(engine) => Some(Effect::Transcribe {
                job: self.job,
                engine: Arc::clone(engine),
                audio,
                language: self.config.language.clone(),
            }),
            other => {
                let reason = other
                    .unavailable_reason()
                    .unwrap_or_else(|| "modelo no disponible".to_string());
                error!("Recording discarded: {reason}");
                self.finish(format!("ERROR: {reason}"));
                None
            }
        }
    }

    fn finish(&mut self, text: String) {
        match self.stop_waiter.take() {
            Some(tx) => {
                let _ = tx.send(text);
            }
            None => self.unclaimed_result = Some(text),
        }
        self.phase = Phase::Idle;
    }

    fn reload(
        &mut self,
        mut new_config: SttConfig,
        response_tx: oneshot::Sender<anyhow::Result<()>>,
    ) -> Option<Effect> {
        info!(
            "Command: REFRESH (model_kind={} model_id={})",
            new_config.model_kind, new_config.model_id
        );
        let same_model = new_config.model_id == self.config.model_id
            && new_config.model_kind == self.config.model_kind;
        // The client does not know where the model lives on disk.
        new_config.model_path = std::mem::take(&mut self.config.model_path);

        match (&self.engine, same_model) {
            (Engine::Ready(_), true) => {
                self.config = new_config;
                info!("Configuration updated (no model change).");
                let _ = response_tx.send(Ok(()));
                None
            }
            (Engine::Loading, true) => {
                self.config = new_config;
                self.reload_waiters.push(response_tx);
                None
            }
            _ if self.phase != Phase::Idle => {
                self.config.model_path = new_config.model_path;
                let _ = response_tx.send(Err(anyhow::anyhow!(
                    "hay una grabación en curso; vuelve a intentarlo al terminar"
                )));
                None
            }
            _ => {
                for waiter in self.reload_waiters.drain(..) {
                    let _ =
                        waiter.send(Err(anyhow::anyhow!("reemplazado por un REFRESH posterior")));
                }
                // Drop the old engine before loading the new one so both
                // never sit in (V)RAM at once.
                self.engine = Engine::Loading;
                new_config.model_path = String::new();
                self.config = new_config.clone();
                self.load_generation += 1;
                self.reload_waiters.push(response_tx);
                Some(Effect::LoadEngine {
                    generation: self.load_generation,
                    config: new_config,
                })
            }
        }
    }

    fn engine_loaded(&mut self, generation: u64, result: LoadResult) {
        if generation != self.load_generation {
            info!("Discarding superseded model load (generation {generation})");
            return;
        }
        let reply = match result {
            Ok((engine, path)) => {
                info!("Model loaded; ready to transcribe.");
                self.engine = Engine::Ready(engine);
                self.config.model_path = path;
                Ok(())
            }
            Err(msg) => {
                error!("Failed to load model: {msg}");
                self.engine = Engine::Failed(msg.clone());
                Err(msg)
            }
        };
        for waiter in self.reload_waiters.drain(..) {
            let _ = waiter.send(
                reply
                    .clone()
                    .map_err(|msg| anyhow::anyhow!("Failed to load model: {msg}")),
            );
        }
    }

    fn status(&self) -> StatusResponse {
        StatusResponse {
            active: true,
            pid: std::process::id(),
            model_id: self.config.model_id.clone(),
            model_kind: self.config.model_kind.clone(),
            model_path: self.config.model_path.clone(),
            language: self.config.language.clone(),
            max_recording_seconds: self.config.max_recording_seconds,
            state: format!("{:?}", self.phase),
            engine: self.engine.status_label(),
        }
    }
}

pub type LoadFuture = Pin<Box<dyn Future<Output = LoadResult> + Send>>;

/// The work [`run`] delegates to the outside world.
pub struct Hooks {
    pub load_engine: Box<dyn Fn(SttConfig) -> LoadFuture + Send + Sync>,
    pub notify_auto_stop: Box<dyn Fn() + Send + Sync>,
}

/// Drive `daemon` until `shutdown` flips or the command channel closes.
pub async fn run<A: AudioSource>(
    mut daemon: Daemon<A>,
    initial_load: Effect,
    mut commands: mpsc::Receiver<Command>,
    mut shutdown: watch::Receiver<bool>,
    hooks: Hooks,
    tick_every: Duration,
) {
    let (done_tx, mut done_rx) = mpsc::channel::<Event>(8);
    let mut tick = tokio::time::interval(tick_every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending = vec![initial_load];

    loop {
        for effect in pending.drain(..) {
            execute(effect, &hooks, &done_tx);
        }
        let event = tokio::select! {
            _ = shutdown.changed() => {
                info!("Shutdown requested; exiting event loop");
                return;
            }
            cmd = commands.recv() => match cmd {
                Some(cmd) => Event::Command(cmd),
                None => return,
            },
            Some(event) = done_rx.recv() => event,
            _ = tick.tick() => Event::Tick,
        };
        pending = daemon.handle(event);
    }
}

fn execute(effect: Effect, hooks: &Hooks, done_tx: &mpsc::Sender<Event>) {
    match effect {
        Effect::Transcribe {
            job,
            engine,
            audio,
            language,
        } => {
            let done_tx = done_tx.clone();
            tokio::spawn(async move {
                let joined =
                    tokio::task::spawn_blocking(move || engine.transcribe(&audio, Some(&language)))
                        .await;
                let text = match joined {
                    Ok(Ok(text)) => text,
                    Ok(Err(e)) => {
                        error!("Transcription failed: {e}");
                        format!("ERROR: {e}")
                    }
                    Err(e) => {
                        error!("Transcription task panicked: {e}");
                        "ERROR: la transcripción falló inesperadamente".to_string()
                    }
                };
                let _ = done_tx.send(Event::TranscriptionDone { job, text }).await;
            });
        }
        Effect::LoadEngine { generation, config } => {
            let load = (hooks.load_engine)(config);
            let done_tx = done_tx.clone();
            tokio::spawn(async move {
                let result = load.await;
                let _ = done_tx
                    .send(Event::EngineLoaded { generation, result })
                    .await;
            });
        }
        Effect::NotifyAutoStop => (hooks.notify_auto_stop)(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Microphone whose samples the test pushes while the daemon owns it.
    #[derive(Clone, Default)]
    struct FakeMic(Arc<Mutex<VecDeque<f32>>>);

    impl FakeMic {
        fn capture(&self, samples: usize) {
            self.0
                .lock()
                .unwrap()
                .extend(std::iter::repeat_n(0.1, samples));
        }
    }

    impl AudioSource for FakeMic {
        fn discard(&mut self) {
            self.0.lock().unwrap().clear();
        }
        fn drain_into(&mut self, buffer: &mut Vec<f32>, limit: usize) {
            let mut q = self.0.lock().unwrap();
            while buffer.len() < limit {
                match q.pop_front() {
                    Some(s) => buffer.push(s),
                    None => break,
                }
            }
        }
    }

    struct CountingTranscriber;

    impl Transcriber for CountingTranscriber {
        fn transcribe(&self, audio: &[f32], _language: Option<&str>) -> anyhow::Result<String> {
            Ok(format!("{} muestras", audio.len()))
        }
    }

    fn config(model_id: &str) -> SttConfig {
        SttConfig {
            model_id: model_id.to_string(),
            model_kind: "whisper".to_string(),
            model_path: String::new(),
            language: "es".to_string(),
            max_recording_seconds: 2,
        }
    }

    fn loaded(path: &str) -> LoadResult {
        Ok((Arc::new(CountingTranscriber), path.to_string()))
    }

    fn ready_daemon() -> (Daemon<FakeMic>, FakeMic) {
        let mic = FakeMic::default();
        let (mut d, _) = Daemon::new(mic.clone(), config("modelo-a"));
        d.handle(Event::EngineLoaded {
            generation: 1,
            result: loaded("/cache/modelo-a.bin"),
        });
        (d, mic)
    }

    fn start(d: &mut Daemon<FakeMic>) -> Result<(), String> {
        let (tx, mut rx) = oneshot::channel();
        d.handle(Event::Command(Command::Start { response_tx: tx }));
        rx.try_recv().expect("START must be answered at once")
    }

    fn stop(d: &mut Daemon<FakeMic>) -> (oneshot::Receiver<String>, Vec<Effect>) {
        let (tx, rx) = oneshot::channel();
        let effects = d.handle(Event::Command(Command::Stop { response_tx: tx }));
        (rx, effects)
    }

    fn refresh(
        d: &mut Daemon<FakeMic>,
        cfg: SttConfig,
    ) -> (oneshot::Receiver<anyhow::Result<()>>, Vec<Effect>) {
        let (tx, rx) = oneshot::channel();
        let effects = d.handle(Event::Command(Command::ReloadConfig {
            new_config: cfg,
            response_tx: tx,
        }));
        (rx, effects)
    }

    fn status(d: &mut Daemon<FakeMic>) -> StatusResponse {
        let (tx, mut rx) = oneshot::channel();
        d.handle(Event::Command(Command::GetStatus { response_tx: tx }));
        rx.try_recv().unwrap()
    }

    /// The single job a STOP or auto-stop started.
    fn transcribe_job(effects: &[Effect]) -> (u64, usize) {
        match effects {
            [Effect::Transcribe { job, audio, .. }, ..] => (*job, audio.len()),
            _ => panic!("expected a transcription job"),
        }
    }

    fn load_generation(effects: &[Effect]) -> u64 {
        match effects {
            [Effect::LoadEngine { generation, .. }] => *generation,
            _ => panic!("expected a model load"),
        }
    }

    #[test]
    fn start_is_refused_while_the_model_loads() {
        let (mut d, effect) = Daemon::new(FakeMic::default(), config("modelo-a"));
        assert!(matches!(effect, Effect::LoadEngine { generation: 1, .. }));
        let err = start(&mut d).unwrap_err();
        assert!(err.contains("cargando"), "{err}");
        assert_eq!(d.phase(), Phase::Idle);
    }

    #[test]
    fn start_is_refused_when_the_model_failed_to_load() {
        let (mut d, _) = Daemon::new(FakeMic::default(), config("modelo-a"));
        d.handle(Event::EngineLoaded {
            generation: 1,
            result: Err("archivo corrupto".to_string()),
        });
        let err = start(&mut d).unwrap_err();
        assert!(err.contains("archivo corrupto"), "{err}");
        assert_eq!(status(&mut d).engine, "failed: archivo corrupto");
    }

    #[test]
    fn start_then_stop_transcribes_what_was_recorded() {
        let (mut d, mic) = ready_daemon();
        mic.capture(500);
        start(&mut d).unwrap();
        mic.capture(1200);
        d.handle(Event::Tick);
        mic.capture(300);

        let (mut rx, effects) = stop(&mut d);
        let (job, samples) = transcribe_job(&effects);
        assert_eq!(samples, 1500, "audio from before START must be dropped");
        assert!(matches!(&effects[0], Effect::Transcribe { language, .. } if language == "es"));
        assert_eq!(d.phase(), Phase::Processing);
        assert!(rx.try_recv().is_err(), "STOP waits for the transcription");

        d.handle(Event::TranscriptionDone {
            job,
            text: "hola".to_string(),
        });
        assert_eq!(rx.try_recv().unwrap(), "hola");
        assert_eq!(d.phase(), Phase::Idle);
    }

    #[test]
    fn stop_with_no_audio_answers_empty_without_transcribing() {
        let (mut d, _mic) = ready_daemon();
        start(&mut d).unwrap();
        let (mut rx, effects) = stop(&mut d);
        assert!(effects.is_empty());
        assert_eq!(rx.try_recv().unwrap(), "");
        assert_eq!(d.phase(), Phase::Idle);
    }

    #[test]
    fn recording_stops_itself_at_max_recording_seconds() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        mic.capture(SAMPLE_RATE * 2 - 1);
        assert!(d.handle(Event::Tick).is_empty());
        mic.capture(SAMPLE_RATE);

        let effects = d.handle(Event::Tick);
        let (_, samples) = transcribe_job(&effects);
        assert_eq!(samples, SAMPLE_RATE * 2, "capped at the configured limit");
        assert!(matches!(effects.last(), Some(Effect::NotifyAutoStop)));
        assert_eq!(d.phase(), Phase::Processing);
    }

    #[test]
    fn auto_stopped_result_is_kept_for_the_next_stop_only_once() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        mic.capture(SAMPLE_RATE * 2);
        let (job, _) = transcribe_job(&d.handle(Event::Tick));
        d.handle(Event::TranscriptionDone {
            job,
            text: "dictado largo".to_string(),
        });
        assert_eq!(d.phase(), Phase::Idle);

        let (mut rx, _) = stop(&mut d);
        assert_eq!(rx.try_recv().unwrap(), "dictado largo");
        let (mut rx, _) = stop(&mut d);
        assert_eq!(rx.try_recv().unwrap(), "", "the result is handed out once");
    }

    #[test]
    fn stop_during_auto_stop_transcription_waits_for_it() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        mic.capture(SAMPLE_RATE * 2);
        let (job, _) = transcribe_job(&d.handle(Event::Tick));

        let (mut rx, effects) = stop(&mut d);
        assert!(effects.is_empty(), "no second transcription");
        d.handle(Event::TranscriptionDone {
            job,
            text: "listo".to_string(),
        });
        assert_eq!(rx.try_recv().unwrap(), "listo");
    }

    #[test]
    fn a_second_stop_while_processing_releases_the_first_one() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        mic.capture(100);
        let (mut first, effects) = stop(&mut d);
        let (job, _) = transcribe_job(&effects);
        let (mut second, _) = stop(&mut d);

        assert!(first.try_recv().unwrap().starts_with("ERROR:"));
        d.handle(Event::TranscriptionDone {
            job,
            text: "texto".to_string(),
        });
        assert_eq!(second.try_recv().unwrap(), "texto");
    }

    #[test]
    fn start_is_refused_while_processing() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        mic.capture(100);
        let _pending = stop(&mut d);
        let err = start(&mut d).unwrap_err();
        assert!(err.contains("transcribiendo"), "{err}");
        assert_eq!(d.phase(), Phase::Processing);
    }

    #[test]
    fn start_while_recording_restarts_the_capture() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        mic.capture(800);
        d.handle(Event::Tick);
        start(&mut d).unwrap();
        mic.capture(200);
        let (_, effects) = stop(&mut d);
        assert_eq!(transcribe_job(&effects).1, 200);
    }

    #[test]
    fn idle_ticks_keep_the_microphone_queue_empty() {
        let (mut d, mic) = ready_daemon();
        mic.capture(5000);
        d.handle(Event::Tick);
        assert!(mic.0.lock().unwrap().is_empty());
    }

    #[test]
    fn cancel_while_recording_discards_the_audio() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        mic.capture(1000);
        d.handle(Event::Tick);
        d.handle(Event::Command(Command::Cancel));
        assert_eq!(d.phase(), Phase::Idle);

        start(&mut d).unwrap();
        mic.capture(10);
        let (_, effects) = stop(&mut d);
        assert_eq!(transcribe_job(&effects).1, 10);
    }

    #[test]
    fn cancel_while_processing_answers_stop_and_drops_the_late_result() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        mic.capture(100);
        let (mut rx, effects) = stop(&mut d);
        let (job, _) = transcribe_job(&effects);

        d.handle(Event::Command(Command::Cancel));
        assert_eq!(rx.try_recv().unwrap(), "ERROR: transcripción cancelada");
        assert_eq!(d.phase(), Phase::Idle);

        d.handle(Event::TranscriptionDone {
            job,
            text: "tarde".to_string(),
        });
        let (mut rx, _) = stop(&mut d);
        assert_eq!(
            rx.try_recv().unwrap(),
            "",
            "the cancelled text must not leak"
        );
    }

    #[test]
    fn cancel_while_idle_forgets_an_unclaimed_result() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        mic.capture(SAMPLE_RATE * 2);
        let (job, _) = transcribe_job(&d.handle(Event::Tick));
        d.handle(Event::TranscriptionDone {
            job,
            text: "olvidado".to_string(),
        });
        d.handle(Event::Command(Command::Cancel));
        let (mut rx, _) = stop(&mut d);
        assert_eq!(rx.try_recv().unwrap(), "");
    }

    #[test]
    fn refresh_without_model_change_applies_settings_and_keeps_the_path() {
        let (mut d, _mic) = ready_daemon();
        let mut cfg = config("modelo-a");
        cfg.language = "en".to_string();
        cfg.max_recording_seconds = 30;
        let (mut rx, effects) = refresh(&mut d, cfg);
        assert!(effects.is_empty(), "same model must not reload");
        assert!(rx.try_recv().unwrap().is_ok());

        let s = status(&mut d);
        assert_eq!(s.language, "en");
        assert_eq!(s.max_recording_seconds, 30);
        assert_eq!(s.model_path, "/cache/modelo-a.bin");
        assert_eq!(s.engine, "ready");
    }

    #[test]
    fn refresh_with_new_model_is_refused_while_recording() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        let (mut rx, effects) = refresh(&mut d, config("modelo-b"));
        assert!(effects.is_empty());
        assert!(rx.try_recv().unwrap().is_err());

        mic.capture(100);
        let (_, effects) = stop(&mut d);
        assert_eq!(transcribe_job(&effects).1, 100, "the recording survives");
        let s = status(&mut d);
        assert_eq!(s.model_id, "modelo-a");
        assert_eq!(s.model_path, "/cache/modelo-a.bin");
    }

    #[test]
    fn refresh_with_new_model_is_refused_while_processing() {
        let (mut d, mic) = ready_daemon();
        start(&mut d).unwrap();
        mic.capture(100);
        let (mut stop_rx, effects) = stop(&mut d);
        let (job, _) = transcribe_job(&effects);

        let (mut rx, effects) = refresh(&mut d, config("modelo-b"));
        assert!(effects.is_empty());
        assert!(rx.try_recv().unwrap().is_err());

        d.handle(Event::TranscriptionDone {
            job,
            text: "ok".to_string(),
        });
        assert_eq!(stop_rx.try_recv().unwrap(), "ok");
    }

    #[test]
    fn refresh_with_new_model_reloads_and_reports_the_new_path() {
        let (mut d, _mic) = ready_daemon();
        let (mut rx, effects) = refresh(&mut d, config("modelo-b"));
        let generation = load_generation(&effects);
        assert_eq!(status(&mut d).engine, "loading");
        assert!(start(&mut d).is_err());

        d.handle(Event::EngineLoaded {
            generation,
            result: loaded("/cache/modelo-b.bin"),
        });
        assert!(rx.try_recv().unwrap().is_ok());
        let s = status(&mut d);
        assert_eq!(s.model_id, "modelo-b");
        assert_eq!(s.model_path, "/cache/modelo-b.bin");
        assert!(start(&mut d).is_ok());
    }

    #[test]
    fn failed_refresh_is_reported_to_the_client_and_in_status() {
        let (mut d, _mic) = ready_daemon();
        let (mut rx, effects) = refresh(&mut d, config("modelo-roto"));
        d.handle(Event::EngineLoaded {
            generation: load_generation(&effects),
            result: Err("no existe".to_string()),
        });
        let err = rx.try_recv().unwrap().unwrap_err().to_string();
        assert!(err.contains("no existe"), "{err}");
        assert_eq!(status(&mut d).engine, "failed: no existe");
    }

    #[test]
    fn a_slower_older_load_cannot_overwrite_a_newer_one() {
        let (mut d, _mic) = ready_daemon();
        let (mut rx_b, effects_b) = refresh(&mut d, config("modelo-b"));
        let (mut rx_c, effects_c) = refresh(&mut d, config("modelo-c"));
        assert!(rx_b.try_recv().unwrap().is_err(), "B was superseded");

        d.handle(Event::EngineLoaded {
            generation: load_generation(&effects_c),
            result: loaded("/cache/modelo-c.bin"),
        });
        d.handle(Event::EngineLoaded {
            generation: load_generation(&effects_b),
            result: loaded("/cache/modelo-b.bin"),
        });
        assert!(rx_c.try_recv().unwrap().is_ok());
        let s = status(&mut d);
        assert_eq!(s.model_id, "modelo-c");
        assert_eq!(s.model_path, "/cache/modelo-c.bin");
    }

    #[test]
    fn refresh_of_the_model_being_loaded_waits_instead_of_loading_twice() {
        let (mut d, _) = Daemon::new(FakeMic::default(), config("modelo-a"));
        let (mut rx, effects) = refresh(&mut d, config("modelo-a"));
        assert!(effects.is_empty(), "must not start a second load");
        assert!(rx.try_recv().is_err(), "answered when the load finishes");

        d.handle(Event::EngineLoaded {
            generation: 1,
            result: loaded("/cache/modelo-a.bin"),
        });
        assert!(rx.try_recv().unwrap().is_ok());
    }

    #[test]
    fn refresh_after_a_failed_load_retries_the_same_model() {
        let (mut d, _) = Daemon::new(FakeMic::default(), config("modelo-a"));
        d.handle(Event::EngineLoaded {
            generation: 1,
            result: Err("sin red".to_string()),
        });
        let (_rx, effects) = refresh(&mut d, config("modelo-a"));
        assert_eq!(load_generation(&effects), 2);
    }

    #[test]
    fn status_reports_phase_and_engine() {
        let (mut d, _mic) = ready_daemon();
        assert_eq!(status(&mut d).state, "Idle");
        start(&mut d).unwrap();
        let s = status(&mut d);
        assert_eq!(s.state, "Recording");
        assert_eq!(s.engine, "ready");
    }

    // ── The event loop, with real tokio tasks ───────────────────────

    /// Blocks inside `transcribe` until the test releases it.
    struct GatedTranscriber(Mutex<std::sync::mpsc::Receiver<()>>);

    impl Transcriber for GatedTranscriber {
        fn transcribe(&self, audio: &[f32], _language: Option<&str>) -> anyhow::Result<String> {
            self.0.lock().unwrap().recv().ok();
            Ok(format!("{} muestras", audio.len()))
        }
    }

    struct Harness {
        commands: mpsc::Sender<Command>,
        mic: FakeMic,
        release: std::sync::mpsc::Sender<()>,
        _shutdown: watch::Sender<bool>,
    }

    impl Harness {
        fn spawn() -> Self {
            let (release, gate) = std::sync::mpsc::channel();
            let engine: Arc<dyn Transcriber> = Arc::new(GatedTranscriber(Mutex::new(gate)));
            let mic = FakeMic::default();
            let (daemon, initial) = Daemon::new(mic.clone(), config("modelo-a"));
            let (commands, rx) = mpsc::channel(8);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let hooks = Hooks {
                load_engine: Box::new(move |_| {
                    let engine = Arc::clone(&engine);
                    Box::pin(async move { Ok((engine, "/cache/modelo-a.bin".to_string())) })
                }),
                notify_auto_stop: Box::new(|| {}),
            };
            tokio::spawn(run(
                daemon,
                initial,
                rx,
                shutdown_rx,
                hooks,
                Duration::from_millis(5),
            ));
            Self {
                commands,
                mic,
                release,
                _shutdown: shutdown_tx,
            }
        }

        async fn status(&self) -> StatusResponse {
            let (tx, rx) = oneshot::channel();
            self.commands
                .send(Command::GetStatus { response_tx: tx })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(1), rx)
                .await
                .expect("STATUS must answer while a transcription runs")
                .unwrap()
        }

        async fn start(&self) -> Result<(), String> {
            let (tx, rx) = oneshot::channel();
            self.commands
                .send(Command::Start { response_tx: tx })
                .await
                .unwrap();
            rx.await.unwrap()
        }

        async fn stop(&self) -> oneshot::Receiver<String> {
            let (tx, rx) = oneshot::channel();
            self.commands
                .send(Command::Stop { response_tx: tx })
                .await
                .unwrap();
            rx
        }

        async fn wait_until_ready(&self) {
            for _ in 0..100 {
                if self.status().await.engine == "ready" {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("engine never became ready");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transcription_runs_off_the_loop_and_reaches_the_stop_caller() {
        let h = Harness::spawn();
        h.wait_until_ready().await;
        h.start().await.unwrap();
        h.mic.capture(320);
        let mut result = h.stop().await;

        assert_eq!(h.status().await.state, "Processing");
        assert!(result.try_recv().is_err());
        h.release.send(()).unwrap();
        let text = tokio::time::timeout(Duration::from_secs(1), result)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(text, "320 muestras");
        assert_eq!(h.status().await.state, "Idle");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_job_finishing_late_does_not_reach_the_next_recording() {
        let h = Harness::spawn();
        h.wait_until_ready().await;
        h.start().await.unwrap();
        h.mic.capture(100);
        let cancelled = h.stop().await;
        h.commands.send(Command::Cancel).await.unwrap();
        assert_eq!(cancelled.await.unwrap(), "ERROR: transcripción cancelada");

        h.release.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let late = h.stop().await;
        assert_eq!(late.await.unwrap(), "");
    }
}
