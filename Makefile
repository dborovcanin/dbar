CARGO ?= cargo
BIN   := dbar
# Where `install` puts the binary: $(PREFIX)/bin/dbar, so /usr/bin/dbar.
# Writing there needs root, hence `sudo make install`. Override PREFIX for a
# home install (PREFIX=$$HOME/.local), and DESTDIR to stage into a package root.
PREFIX ?= /usr
DESTDIR ?=

.PHONY: all prod run check test bench fmt clippy clean install uninstall release-bundle release

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

# Release timing probes. Deliberately not part of `make test`: they measure the
# machine they run on, so they report numbers rather than pass or fail. Run
# before and after anything that touches layout or painting, and compare.
bench:
	$(CARGO) test --release -- --ignored --nocapture \
		--test-threads=1 \
		paint_costs_this_much_per_frame \
		benchmark_joined_ribbon \
		benchmark_group_collapse_layout \
		benchmark_module_collapse

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

# The same binary and checksum the Release workflow uploads, built locally and
# published nowhere - for checking what a release would ship.
release-bundle:
	$(CARGO) build --release --locked
	@mkdir -p target/release-assets
	install -m755 target/release/$(BIN) target/release-assets/$(BIN)-linux-x86_64
	@cd target/release-assets && \
		sha256sum $(BIN)-linux-x86_64 >$(BIN)-linux-x86_64.sha256
	@echo "Bundled target/release-assets/$(BIN)-linux-x86_64"

# Tag the version in Cargo.toml and push it. GitHub Actions builds and
# publishes the release from that tag.
release:
	sh scripts/release.sh
