CARGO ?= cargo
BIN   := dbar
# Where `install` puts the binary: $(PREFIX)/bin/dbar, so /usr/bin/dbar.
# Writing there needs root, hence `sudo make install`. Override PREFIX for a
# home install (PREFIX=$$HOME/.local), and DESTDIR to stage into a package root.
PREFIX ?= /usr
DESTDIR ?=

.PHONY: all prod run check test fmt clippy clean install uninstall release-bundle release

# Fast iteration build.
all:
	$(CARGO) build

# Optimized build.
prod:
	$(CARGO) build --release

run:
	$(CARGO) run

# Type-check only; fastest feedback loop.
check:
	$(CARGO) check

test:
	$(CARGO) test

fmt:
	$(CARGO) fmt

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

install: prod
	install -Dm755 target/release/$(BIN) $(DESTDIR)$(PREFIX)/bin/$(BIN)

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/$(BIN)

clean:
	$(CARGO) clean

# Build the downloadable Linux binary and its checksum without publishing it.
release-bundle:
	CARGO=$(CARGO) sh scripts/release.sh --bundle-only

# Build, tag the version from Cargo.toml, push the tag, and publish the assets.
release:
	CARGO=$(CARGO) sh scripts/release.sh
