# Makefile for Telora

PREFIX ?= /usr
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
SYSTEMD_USER_DIR ?= $(PREFIX)/lib/systemd/user

# Binary names
ifneq ("$(wildcard target/release/telora-daemon)","")
    DAEMON_BIN = target/release/telora-daemon
    GUI_BIN = target/release/telora-gui
    CTL_BIN = target/release/telora
    MANAGER_BIN = target/release/telora-models
else
    DAEMON_BIN = bin/telora-daemon
    GUI_BIN = bin/telora-gui
    CTL_BIN = bin/telora
    MANAGER_BIN = bin/telora-models
endif

.PHONY: all install clean

all:
	@echo "Use ./scripts/build to compile the project."

install:
	install -Dm755 $(DAEMON_BIN) $(DESTDIR)$(BINDIR)/telora-daemon
	install -Dm755 $(GUI_BIN) $(DESTDIR)$(BINDIR)/telora-gui
	install -Dm755 $(CTL_BIN) $(DESTDIR)$(BINDIR)/telora
	install -Dm755 $(MANAGER_BIN) $(DESTDIR)$(BINDIR)/telora-models
	install -Dm644 systemd/telora-daemon.service $(DESTDIR)$(SYSTEMD_USER_DIR)/telora-daemon.service
	install -Dm644 systemd/telora.service $(DESTDIR)$(SYSTEMD_USER_DIR)/telora.service
	install -Dm644 telora.toml $(DESTDIR)/etc/telora.toml
	mkdir -p $(DESTDIR)$(DATADIR)/telora/models

clean:
	./scripts/clean
