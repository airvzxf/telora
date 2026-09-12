# Makefile for Telora
#
# GNU make only. Targets prefixed with `##` appear in `make help`.
# A bare `make` prints the help screen (default goal).
#
# Help-line grammar used by the awk pattern below:
#   ## Section Name   ← printed as a section banner (no target)
#   target: deps ## Description   ← printed as "  target  Description"
#
# The longest target name is `uninstall-systemd-user` (22 chars); the
# help formatter pads to 22 so columns align.

# ─── Filesystem locations ───────────────────────────────────────────
PREFIX          ?= /usr
BINDIR          ?= $(PREFIX)/bin
DATADIR         ?= $(PREFIX)/share
SYSTEMD_USER_DIR ?= $(PREFIX)/lib/systemd/user
DESTDIR         ?=
SKIP_SYSTEMD    ?=

# NEWLINE is the line-separator consumed by $(foreach ...) so each
# install lands on its own physical (and therefore its own logical
# recipe) line. Defined once near the top.
define NEWLINE


endef

# ─── Workspace binaries & systemd units ──────────────────────────────
BINARIES      := telora-daemon telora-gui telora telora-models
SYSTEMD_UNITS := telora-daemon.service telora-daemon.socket telora.service

# ─── Toolchain detection ────────────────────────────────────────────
# Resolve the container engine once so `build`, `clean-all`, and
# `doctor` all see the same value. Empty when neither is on PATH.
CONTAINER_ENGINE ?= $(shell command -v podman 2>/dev/null || command -v docker 2>/dev/null)

# Prefer release artefacts over the container-extracted bin/ tree when
# both are present, so a developer who ran `cargo build --release`
# installs the freshly-built binaries instead of stale container ones.
# Require ALL $(BINARIES) to be present in target/release/; otherwise
# fall back to bin/. We compare $(words ...) of the wildcard results
# against $(words $(BINARIES)) — a plain $(if $(foreach ...)) would
# treat four whitespace-separated empty strings as truthy and pick
# target/release even on an empty tree. Counting guarantees a partial
# release tree (e.g. only telora-daemon built) safely falls back to
# bin/ instead of failing the install mid-way.
BIN_DIR := $(if $(filter $(words $(BINARIES)),$(words $(foreach b,$(BINARIES),$(wildcard target/release/$(b))))),target/release,bin)

.DEFAULT_GOAL := help

.PHONY: audit build build-native build-release check ci clean clean-all \
        doctor fmt fmt-check help install install-systemd-user lint package \
        reinstall run-ctl run-daemon run-gui run-models setup-env simulate \
        test test-one uninstall uninstall-systemd-user verify

# ─── Utility ─────────────────────────────────────────────────────────
## Utility

help: ## Show this help screen
	@awk 'BEGIN {FS = ":.*## "; printf "Telora — Makefile targets\n"} \
		/^## / {printf "\n%s\n", $$0; next} \
		/^[a-zA-Z][a-zA-Z0-9_.-]+:.*## / {printf "  %-22s ## %s\n", $$1, $$2}' \
		$(MAKEFILE_LIST)

doctor: ## Print non-destructive diagnostics (rustc/cargo/gtk4/cuda/bin/*)
	@status=0; \
	echo "--> Running doctor"; \
	if command -v rustc >/dev/null 2>&1; then \
		printf "  [OK]       rustc : %s\n" "$$(rustc --version)"; \
	else \
		printf "  [CRITICAL] rustc : MISSING\n"; \
		status=1; \
	fi; \
	if command -v cargo >/dev/null 2>&1; then \
		printf "  [OK]       cargo : %s\n" "$$(cargo --version)"; \
	else \
		printf "  [CRITICAL] cargo : MISSING\n"; \
		status=1; \
	fi; \
	if [ -n "$(CONTAINER_ENGINE)" ]; then \
		printf "  [OK]       engine: %s\n" "$(CONTAINER_ENGINE)"; \
	else \
		printf "  [MISSING]  engine: podman/docker not on PATH\n"; \
	fi; \
	if [ -d "$(SYSTEMD_USER_DIR)" ]; then \
		printf "  [OK]       systemd: %s\n" "$(SYSTEMD_USER_DIR)"; \
	else \
		printf "  [MISSING]  systemd: %s\n" "$(SYSTEMD_USER_DIR)"; \
	fi; \
	gtk4_v=$$(pkg-config --modversion gtk4 2>/dev/null); \
	if [ -n "$$gtk4_v" ]; then \
		printf "  [OK]       gtk4  : %s\n" "$$gtk4_v"; \
	else \
		printf "  [MISSING]  gtk4  : pkg-config could not resolve gtk4\n"; \
	fi; \
	gtk4ls_v=$$(pkg-config --modversion gtk4-layer-shell-0 2>/dev/null); \
	if [ -n "$$gtk4ls_v" ]; then \
		printf "  [OK]       gtk4ls: %s\n" "$$gtk4ls_v"; \
	else \
		printf "  [MISSING]  gtk4ls: pkg-config could not resolve gtk4-layer-shell-0\n"; \
	fi; \
	nvcc_v=$$(nvcc --version 2>/dev/null | head -1); \
	if [ -n "$$nvcc_v" ]; then \
		printf "  [OK]       cuda  : %s\n" "$$nvcc_v"; \
	else \
		printf "  [MISSING]  cuda  : no CUDA\n"; \
	fi; \
	if [ -d bin ]; then \
		printf "  [OK]       bin/* : %s\n" "$$(ls -1 bin 2>/dev/null | sort | tr '\n' ' ' | sed 's/ $$//')"; \
	else \
		printf "  [MISSING]  bin/* : bin/ not found (run 'make build' or 'make build-release')\n"; \
	fi; \
	exit $$status

clean: ## Remove target/, bin/, models/, logs, and pkg artifacts
	./scripts/clean

clean-all: ## clean PLUS remove the telora-daemon container image
	@echo "--> Running clean-all"
	@$(MAKE) clean
	@if [ -n "$(CONTAINER_ENGINE)" ]; then \
		if $(CONTAINER_ENGINE) images --format '{{.Repository}}:{{.Tag}}' 2>/dev/null | grep -qx 'telora-daemon:latest'; then \
			echo "--> Removing telora-daemon:latest image"; \
			$(CONTAINER_ENGINE) rmi telora-daemon:latest; \
		else \
			echo "--> telora-daemon:latest not present, nothing to remove"; \
		fi; \
	else \
		echo "--> No container engine on PATH, skipping image removal"; \
	fi

# ─── Build ───────────────────────────────────────────────────────────
## Build

build: ## Build via container (./scripts/build → ./bin/*)
	@./scripts/build

build-native: ## cargo build --locked --workspace --all-targets (debug, fast iteration)
	cargo build --locked --workspace --all-targets

# `--bins`, not `--all-targets`: release.yml:334 only builds the four
# bin targets (no examples, no benches, no tests). Mirrors what the
# GitHub Release artefact actually contains.
build-release: ## cargo build --release --locked --workspace --bins (mirrors release.yml)
	cargo build --release --locked --workspace --bins

check: ## cargo check --locked --workspace --all-targets
	cargo check --locked --workspace --all-targets

# ─── Quality ─────────────────────────────────────────────────────────
## Quality

fmt: ## cargo fmt --all
	cargo fmt --all

fmt-check: ## cargo fmt --all -- --check (CI mirror)
	cargo fmt --all -- --check

lint: ## cargo clippy --locked --workspace --all-targets -- -D warnings (CI mirror)
	cargo clippy --locked --workspace --all-targets -- -D warnings

audit: ## Audit dependencies for known security advisories (no-op if cargo-audit missing)
	@if command -v cargo-audit >/dev/null 2>&1; then \
		echo "--> Running cargo audit"; \
		cargo audit --deny warnings; \
	else \
		echo "cargo-audit not installed; skipping. Install with: cargo install cargo-audit --locked"; \
	fi

# Run the same gates as `.github/workflows/ci.yml`, in the same order,
# with `--no-fail-fast` skipped intentionally to fail fast.
ci: fmt-check build-native test lint ## Run the same gates as .github/workflows/ci.yml

# ─── Test ────────────────────────────────────────────────────────────
## Test

test: ## cargo test --locked --workspace --no-fail-fast (CI mirror)
	cargo test --locked --workspace --no-fail-fast

test-one: ## Run a single test: make test-one TEST=voxora_020_resolution
	@test -n "$(TEST)" || { echo "Usage: make test-one TEST=<name>"; exit 1; }
	cargo test --locked --workspace $(TEST)

# Note: an explicit `test-integration` target is intentionally absent.
# The three integration tests under telora-daemon/tests/ run under
# the plain `cargo test --workspace` invocation above; no Cargo feature
# called `integration` gates them. `test-compat` is also absent; use
# the `verify` target in the Compatibility section below.

simulate: ## Run scripts/compatibility/simulate.sh (full stack: daemon + GUI)
	./scripts/compatibility/simulate.sh

# ─── Packaging ───────────────────────────────────────────────────────
## Packaging

install: ## Install binaries, config, models dir, and (unless SKIP_SYSTEMD=1) systemd units
	@test -f $(BIN_DIR)/telora-daemon || { echo "Run 'make build', 'make build-native', or 'make build-release' first."; exit 1; }
	@echo "--> Installing binaries to $(DESTDIR)$(BINDIR)"
	@$(foreach b,$(BINARIES),install -Dm755 $(BIN_DIR)/$(b) $(DESTDIR)$(BINDIR)/$(b)$(NEWLINE))
	@echo "--> Installing default config"
	@install -Dm644 telora.toml $(DESTDIR)/etc/telora.toml
	@echo "--> Creating models directory"
	@mkdir -p $(DESTDIR)$(DATADIR)/telora/models
ifneq ($(SKIP_SYSTEMD),1)
	@echo "--> Installing systemd user units to $(DESTDIR)$(SYSTEMD_USER_DIR)"
	@$(foreach u,$(SYSTEMD_UNITS),install -Dm644 systemd/$(u) $(DESTDIR)$(SYSTEMD_USER_DIR)/$(u)$(NEWLINE))
endif

# Sub-target: install ONLY the systemd user units. Honours SKIP_SYSTEMD
# so a `make install-systemd-user SKIP_SYSTEMD=1` is a documented no-op.
install-systemd-user: ## Install only the systemd user units (honours SKIP_SYSTEMD=1)
ifneq ($(SKIP_SYSTEMD),1)
	@echo "--> Installing systemd user units to $(DESTDIR)$(SYSTEMD_USER_DIR)"
	@$(foreach u,$(SYSTEMD_UNITS),install -Dm644 systemd/$(u) $(DESTDIR)$(SYSTEMD_USER_DIR)/$(u)$(NEWLINE))
endif

uninstall: ## Reverse `install` (idempotent; honours SKIP_SYSTEMD=1)
	@echo "--> Removing binaries from $(DESTDIR)$(BINDIR)"
	@$(foreach b,$(BINARIES),rm -f $(DESTDIR)$(BINDIR)/$(b)$(NEWLINE))
	@echo "--> Removing default config"
	@rm -f $(DESTDIR)/etc/telora.toml
	@echo "--> Removing models directory if empty"
	@rmdir $(DESTDIR)$(DATADIR)/telora/models 2>/dev/null || true
ifneq ($(SKIP_SYSTEMD),1)
	@echo "--> Removing systemd user units from $(DESTDIR)$(SYSTEMD_USER_DIR)"
	@$(foreach u,$(SYSTEMD_UNITS),rm -f $(DESTDIR)$(SYSTEMD_USER_DIR)/$(u)$(NEWLINE))
endif

uninstall-systemd-user: ## Remove only the systemd user units (honours SKIP_SYSTEMD=1)
ifneq ($(SKIP_SYSTEMD),1)
	@echo "--> Removing systemd user units from $(DESTDIR)$(SYSTEMD_USER_DIR)"
	@$(foreach u,$(SYSTEMD_UNITS),rm -f $(DESTDIR)$(SYSTEMD_USER_DIR)/$(u)$(NEWLINE))
endif

reinstall: ## Stop the running daemon, uninstall, then install (mirrors pkg/telora-bin.install:pre_upgrade)
	@echo "--> Reinstalling"
	@systemctl --user stop telora-daemon.service 2>/dev/null || true
	@$(MAKE) --no-print-directory uninstall DESTDIR="$(DESTDIR)"
	@$(MAKE) --no-print-directory install DESTDIR="$(DESTDIR)"

# Build the Arch package. Refuses to run if Cargo.toml's workspace
# version and PKGBUILD's pkgver drift, so a local `make package`
# never silently produces a mis-versioned tarball. `-s` syncs
# makedepends; `-f` is intentionally omitted so a pre-existing
# *.pkg.tar* artefact triggers a makepkg error instead of being
# silently overwritten.
package: ## Build Arch package (refuses on Cargo.toml/PKGBUILD version drift)
	@cargo_ver=$$(awk '/^version = /{gsub(/"/,""); print $$3; exit}' Cargo.toml); \
	pkg_ver=$$(awk -F= '/^pkgver=/{gsub(/[ \t]+/,""); print $$2; exit}' pkg/PKGBUILD); \
	if [ "$$cargo_ver" != "$$pkg_ver" ]; then \
		echo "ERROR: Cargo.toml version=$$cargo_ver != PKGBUILD pkgver=$$pkg_ver"; \
		echo "Hint: release.yml rewrites pkgver from the tag. Tag v$$cargo_ver or sync PKGBUILD manually."; \
		exit 1; \
	fi; \
	cd pkg && makepkg -s

# ─── Run ─────────────────────────────────────────────────────────────
## Run

run-daemon: ## cargo run -p telora-daemon (debug)
	cargo run -p telora-daemon

run-gui: ## cargo run -p telora-gui (debug)
	cargo run -p telora-gui

# Note: `telora-ctl` is the crate directory name; cargo emits a binary
# named `telora` from that crate (see telora-ctl/Cargo.toml:
# `name = "telora"`). `cargo run -p telora-ctl` therefore launches the
# `telora` binary, not a literal `telora-ctl`.
run-ctl: ## cargo run -p telora-ctl (binary name is `telora`)
	cargo run -p telora-ctl

run-models: ## cargo run -p telora-models (debug)
	cargo run -p telora-models

# Note: `run-examples` is intentionally absent. There is no
# `examples/` directory at the workspace root nor in any member crate;
# `cargo run --example` would fail with "no example targets". Add
# examples before reintroducing this target.

# ─── Compatibility ───────────────────────────────────────────────────
## Compatibility

setup-env: ## Install runtime dependencies for the host distro (Debian/Fedora/Arch)
	./scripts/compatibility/setup-env.sh

verify: ## Smoke-test all four binaries (scripts/compatibility/verify.sh)
	./scripts/compatibility/verify.sh