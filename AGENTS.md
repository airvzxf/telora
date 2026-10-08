# Reglas para agentes (y humanos) en este repo

1. **Una tarea = una rama = un PR.** `main` está protegida. El merge lo hace el dueño, nunca el agente.
2. **Primero reproduce el fallo.** Un test que falle, o un comando o log que muestre el bug. El PR incluye la evidencia del antes y el después.
3. **Nada se cierra sólo con texto.** Cambiar un comentario, el README o un issue no arregla un comportamiento.
4. **No confíes en comentarios, docs, mensajes de commit ni issues.** Buena parte se escribió sin verificar. Comprueba contra el código y el runtime.
5. **Comentarios**: sólo el *porqué* no obvio (un invariante, una restricción externa). Sin números de issue o PR, sin "closes #", sin historia ("antes hacíamos…") y sin parafrasear el código.
6. **Tests con propósito**: cada test atrapa un fallo concreto y realista. Sin tests de píxeles, de texto de manifiestos ni que pasen cuando no se pueden ejecutar.
7. **Host de referencia**: KDE Plasma 6 Wayland, GTX 1080 (sm_61), PipeWire, CachyOS. Si algo no se puede probar ahí, dilo en el PR.
8. **Sin releases ni tags** sin pedido explícito del dueño.
9. **Checklist del PR**:
   - [ ] `cargo fmt --all -- --check`
   - [ ] `cargo clippy --workspace --all-targets --no-default-features -- -D warnings`
   - [ ] `cargo test --workspace --no-default-features` (y con CUDA en el contenedor si el cambio lo toca)
   - [ ] Evidencia antes/después
   - [ ] Toda afirmación nueva en docs o comentarios está verificada

## Build en el host de referencia
- Sin CUDA: `cargo build -p telora-daemon --no-default-features`. Whisper y Qwen3-ASR corren en CPU.
- Con CUDA: `./scripts/build` (contenedor Podman con CUDA 12). El CUDA 13.4 del sistema no lo soporta `cudarc 0.19.9`.
