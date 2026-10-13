# Telora

Dictado por voz para Linux (Wayland). Grabas con un atajo de teclado, el texto se transcribe en local y queda en el portapapeles.

- `telora-daemon`: captura el micrófono y transcribe con [voxora](https://github.com/airvzxf/voxora) (Whisper vía whisper.cpp, o Qwen3-ASR).
- `telora-gui`: OSD (GTK4 + layer-shell) e ícono en la bandeja del sistema.
- `telora`: CLI para conectar con los atajos de teclado.
- `telora-models`: descarga modelos de Hugging Face a la caché de voxora.

## Compilar

Con CUDA (contenedor Podman con CUDA 12):

```bash
./scripts/build          # deja los binarios en ./bin
```

Sin CUDA (CPU, compila con el toolchain del sistema):

```bash
cargo build --release --workspace --no-default-features
```

## Instalar

```bash
sudo make install PREFIX=/usr        # binarios, /etc/telora.toml, units de systemd
systemctl --user daemon-reload
systemctl --user enable --now telora-daemon.socket telora.service
telora-models download ggerganov/whisper.cpp/ggml-base.bin
```

## Uso

Asigna estos comandos a atajos de teclado:

```bash
telora toggle-copy   # empezar/terminar de grabar; el texto queda en el portapapeles
telora cancel        # descartar la grabación en curso
telora last          # volver a copiar la última transcripción
telora-daemon status # estado del daemon y modelo cargado
```

## Configuración

Se leen, en orden y sobrescribiendo: `/etc/telora.toml`, `~/.config/telora/config.toml`, `--config <archivo>` y las variables `TELORA_*`.

```toml
model_kind = "whisper"                                 # "whisper" o "qwen3-asr"
model_id   = "ggerganov/whisper.cpp/ggml-large-v3.bin" # o "Qwen/Qwen3-ASR-0.6B"
language   = "es"                                      # ISO 639-1
max_recording_seconds = 1800

[audio]
input_device = ""         # vacío = micrófono por defecto del escritorio
```

Los modelos se guardan en `~/.cache/voxora/models/huggingface`.

## Limitaciones conocidas

Están en proceso de corrección; ver los issues abiertos.

- Después de reiniciar el equipo, el daemon puede quedar inalcanzable hasta ejecutar `systemctl --user restart telora-daemon.socket telora-daemon.service telora.service`.
- Si el daemon no está disponible, el OSD muestra "GRABANDO" de todos modos y falla en silencio.
- Mientras transcribe, el daemon no responde a `status` ni a `cancel`.
- Qwen3-ASR con CUDA requiere una GPU sm_70 o superior; en GPUs más antiguas corre en CPU.

## Licencia

AGPL-3.0-only.
