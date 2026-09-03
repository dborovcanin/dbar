CARGO ?= cargo
BIN   := dbar
PREFIX ?= $(HOME)/.local

.PHONY: all prod run check test fmt clippy clean install

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
	install -Dm755 target/release/$(BIN) $(PREFIX)/bin/$(BIN)

clean:
	$(CARGO) clean
