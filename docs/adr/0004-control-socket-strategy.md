# ADR 0004 — Control socket strategy

- **Status:** Accepted
- **Date:** 2026-09-05
- **Decision:** Keep the GUI control socket filesystem-bound; activate only the daemon's listening socket through systemd.

## Context

`telora-gui` owns a persistent GTK session and binds `control.sock` for short
one-shot commands from `telora-ctl`. The daemon's `daemon.sock` is a separate
long-lived stream endpoint that is a natural systemd socket-activation target.

The two sockets therefore have different lifecycle owners. Making the GUI
control endpoint an `Accept=yes` socket would introduce per-connection process
semantics into a persistent GUI, while sharing the daemon's activation FD would
make ownership and restart ordering ambiguous.

## Options considered

| Option | Trade-off |
| --- | --- |
| **A. Filesystem-only control socket** | The GUI continues to call `bind_unix_socket`; it preserves the existing persistent OSD model and manual foreground fallback. |
| **B. `telora-gui.socket` with `Accept=no`** | Symmetric with the daemon, but adds another activation lifecycle and FD handoff without a user-visible benefit. |
| **C. `Accept=yes`** | Rejected: systemd would manage connections rather than the long-lived GUI process. |

## Decision

**Option A.** `telora-gui` creates `control.sock` with the shared bind helper.
The daemon's `telora-daemon.socket` owns only `daemon.sock`; the GUI and CLI
locate the control endpoint through the canonical path environment/configuration
surface.

The decision does not prevent a future GUI activation design, but that would
require a separate lifecycle and protocol decision rather than silently sharing
the daemon's socket unit.

## Consequences

- The daemon can start on demand from `telora-daemon.socket` without changing
the GUI's persistent process model.
- The GUI retains the manual-bind fallback used by foreground/development runs.
- `control.sock` cleanup remains the GUI's responsibility; the daemon service
must not unlink it in `ExecStopPost`.
- Both services publish the canonical socket paths explicitly; `PassEnvironment=`
alone is not used as sibling-service inheritance.

## References

- [EPIC #29](https://github.com/airvzxf/telora/issues/29) — systemd socket activation.
- [Issue #52](https://github.com/airvzxf/telora/issues/52) — control socket strategy.
- [ADR 0003](0003-telora-common-scope.md) — shared crate scope.
