# TODO: Telora

## Priority
- [ ] **Configurable Hotkeys**: Allow users to define their own shortcuts for toggle-type/toggle-copy.
- [ ] **Visual Feedback Improvements**: Add a volume meter or waveform to the OSD while recording.
- [ ] **Wayland Protocol Support**: Explore `wlr-virtual-keyboard-unstable-v1` for more robust typing on all Wayland compositors (currently uses a generic approach).

## Features
- [ ] **Continuous Dictation Mode**: A mode where the daemon transcribes in real-time without manual toggling.
- [ ] **Multi-language Auto-detection**: Leverage Whisper's language detection capabilities.
- [ ] **Architecture Refactor**: Move core logic from `telora-daemon/src/main.rs` to a `lib.rs` and implement a `Transcriber` trait for future engine support.

## UI/UX
- [ ] **Tray Icon**: Add a system tray icon for status monitoring and quick settings.
- [ ] **Configuration GUI**: A simple GTK window to edit `telora.toml`.
- [ ] **Integrated Model Manager**: A GUI for `telora-models` with download progress bars.
- [ ] **Model Detection UX**: Enhance the client (`telora`) to detect when the daemon fails due to a missing model and provide an interactive dialog to download it via `telora-models`.
*Focus: Making the tool accessible to everyone, not just power users.*
- [ ] **Visual Feedback**: Add a VU Meter (audio level indicator) to the OSD while recording.

## Maintenance
- [ ] **Unit Tests**: Increase coverage for audio processing and socket communication.
- [ ] **Integrity Checks**: Add SHA256 checksum verification for model downloads in `telora-models`.
- [ ] **CI/CD**: Automate binary releases for different distributions.

## Core & Stability (Developer & DevOps).
- [ ] **Modernize IPC**: Replace custom text-based protocol with a structured format (JSON-RPC or Varlink) to support metadata (confidence, durat
ion, latency).
- [ ] **Implement Launcher Architecture for Hardware Backends**:
    - [ ] **Phase 1: Launcher Implementation**:
        - [ ] Rename the current `telora-daemon` to `telora-daemon-cpu` to serve as the base backend.
        - [ ] Create a new `telora-daemon` binary to act as the lightweight launcher.
        - [ ] Implement hardware detection, backend downloader (with security checksums), and fallback logic in the launcher.
    - [ ] **Phase 2: Backend Implementation**:
        - [ ] **NVIDIA Backend (CUDA)**:
            - [ ] Refactor the existing CUDA implementation into its own `feature flag` to compile a dedicated `telora-daemon-cuda` binary.
        - [ ] **Intel Backend (OpenVINO)**:
            - [ ] Investigate requirements for compiling `whisper-rs` with the OpenVINO backend.
            - [ ] Create an `openvino` `feature flag` and the necessary configuration to compile the `telora-daemon-openvino` binary.
        - [ ] **AMD Backend (ROCm)**:
            - [ ] Investigate requirements for compiling `whisper-rs` with the `hipblas` `feature flag` (ROCm).
            - [ ] Create a `rocm` `feature flag` to compile the `telora-daemon-rocm` binary.
    - [ ] **Phase 3: Integration & CI/CD**:
        - [ ] Update the launcher to recognize and manage all new backends.
        - [ ] Configure the CI/CD pipeline to build all binaries (launcher, cpu, cuda, openvino, rocm) and publish them as release assets with a checksum manifest.

## Security & Performance (SecOps & Enthusiast)
- [ ] **Process Sandboxing**: Use `Landlock` or `seccomp` to restrict the daemon's access to only necessary files/directories.
- [ ] **Resource Stats**: Add a `status --verbose` command showing VRAM usage, CPU load, and temperature.
- [ ] **Power Management**: Ensure audio streams are fully suspended when idle to save battery on laptops.

## Expansion & Ecosystem (Sponsor & Cloner)
- [ ] **Flatpak Support**: Investigate packaging via Flatpak with CUDA extensions.
- [ ] **Plugin System**: Allow post-transcription actions (e.g., "Send to GPT", "Auto-Translate", "Log to file").
- [ ] **History Logs**: A local, searchable history of recent transcriptions.
- [ ] **Remote Daemon**: Support for connecting a local client to a powerful GPU server over the network.

## User Experience & Customization
- [ ] **Pause/Resume Recording**: Allow pausing and resuming a recording session for continuous transcription, handling interruptions gracefully.
- [ ] **'Correct Last' Command**: Add a hotkey to delete the last transcribed text block, making it easy to retry a mis-transcription.
- [ ] **'Append to Last' Command**: Allow starting a new recording that appends its result directly to the previous transcription.
- [ ] **'Save Last Audio' Command**: Implement a command to save the audio from the last recording to a user-defined location (e.g., as a .wav file).
- [ ] **Dynamic Mode Switching**: Introduce commands to quickly switch between operational modes (e.g., 'type mode', 'lecture mode') without editing config files.
- [ ] **'Repeat Last' Command**: Add a command to re-type or re-copy the last transcribed text without a new recording.
- [ ] **Custom UI Styling**: Allow users to apply custom CSS to the GTK4 OSD for themes (colors, fonts).
- [ ] **OSD Placement Control**: Add configuration options for OSD position (e.g., top-left, bottom-center) and margins.
- [ ] **Input Audio Control**:
    - [ ] **Input Gain**: Add a config option to boost microphone volume before processing.
    - [ ] **Noise Gate**: Implement a volume threshold to ignore quiet background noise.
- [ ] **Audible Feedback**: Option to play sounds for events (e.g., record start, record stop, cancel).
- [ ] **Text Post-Processing**:
    - [ ] **Automatic Capitalization**: Smartly capitalize the beginning of sentences.
    - [ ] **Custom Word Replacements**: A user-defined dictionary for correcting common mis-transcriptions (e.g., "telora" -> "Telora").
- [ ] **Output Delay**: Add an optional delay before typing to allow for cancellation.
