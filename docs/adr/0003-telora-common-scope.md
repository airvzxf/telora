# ADR 0003 — Scope of `telora-common`

- **Status:** Accepted
- **Date:** 2026-09-04
- **Decision:** Keep `telora-common` as a small, leaf workspace crate for shared filesystem and cache policy.

## Context

Telora has five workspace members: `telora-common`, `telora-daemon`,
`telora-gui`, `telora-ctl`, and `telora-models`. Before the extraction in
[EPIC #28](https://github.com/airvzxf/telora/issues/28), socket-path
resolution and Unix-socket binding were duplicated across binary crates. The
legacy model-management wrapper also carried a separate Voxora cache resolver
whose validation diverged from the daemon.

The duplicated implementations created drift in directory permissions, socket
creation, and cache-override validation. The cache drift was closed by
[issue #103](https://github.com/airvzxf/telora/issues/103) when the shared
resolver moved into `telora-common::cache`.

## Decision

`telora-common` owns exactly these shared concerns:

| Module | Responsibility | Consumers |
| --- | --- | --- |
| `paths` | Runtime/socket path resolution and secure directory creation | daemon, GUI, CLI |
| `socket_bind` | Unix-stream socket creation, stale-socket ownership checks, and permissions | daemon, GUI |
| `cache` | Voxora cache layout and override validation | daemon, `telora-models` |

The crate remains a leaf: it must not depend on the binary crates or pull in
GTK, audio, CUDA, or Voxora engine dependencies. Small filesystem-oriented
dependencies are acceptable when they serve one of these shared concerns
(e.g. `libsystemd = "0.7"` is a Linux-only dep on `socket_bind` for
FD adoption via `sd_listen_fds`; it is not exposed in the crate's public
API).

The wire protocol types (`Command`, responses, and future protocol error
traits) are deliberately not part of this decision. They remain daemon-owned
until a separate IPC design is accepted.

## Alternatives considered

1. **Keep one implementation per binary.** Rejected because the copies had
   already diverged in security-sensitive behavior.
2. **Move all daemon types into `telora-common`.** Rejected for now because it
   would make every consumer depend on daemon protocol details and enlarge the
   leaf crate's compile surface.
3. **Use a separate cache crate.** Rejected because the cache resolver shares
   the same filesystem-policy boundary as the path and bind helpers, and the
   current three-module crate is sufficient.

## Consequences

- Socket paths, bind permissions, and Voxora cache validation have one source
  of truth across the current consumers.
- Adding a dependency to `telora-common` affects every binary, so changes must
  remain narrowly scoped and must update `Cargo.lock` in the same pull request.
- `telora-models` remains a compatibility wrapper and can eventually be
  retired without changing the shared cache policy.
- Systemd socket activation and the remaining bind TOCTOU hardening are
  separate decisions; they do not expand this crate's scope automatically.

## References

- [EPIC #28](https://github.com/airvzxf/telora/issues/28) — extraction of the shared paths and bind helper.
- [Issue #103](https://github.com/airvzxf/telora/issues/103) — shared Voxora cache validation.
- `telora-common/src/lib.rs` and `telora-common/README.md` — current public
  surface and dependency policy.
- `CONTRIBUTING.md` — workspace and lockfile contribution rules.
