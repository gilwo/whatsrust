# whatsrust — root Makefile
#
# Thin wrapper over `cargo`. See docs/HOWTO.md for the full operational playbook
# (configure, run, CLI usage, MCP mode, DB migration recovery).

.DEFAULT_GOAL := help

.PHONY: help build release release-all dist run embedder-build run-semantic embedder-smoke calibrate test test-all lint lint-strict fmt fmt-check clean py-sidecar-setup py-sidecar-smoke run-semantic-py

# Model for the Rust embedder sidecar: bge-m3 (default, stronger multilingual/cross-language
# recall) or minilm (lighter, faster). Override on the command line, e.g.:
#   make run-semantic EMBED_MODEL=minilm
# See docs/HOWTO.md #5b "Choosing a model" and docs/adr/0041-hybrid-recall-bounded-vector-scan.md.
EMBED_MODEL ?= bge-m3

# Optional per-run device name override for `run`/`run-semantic` (device-name-at-pairing
# feature; see .env.example WHATSRUST_DEVICE_NAME). Only takes effect when passed
# explicitly on the command line, e.g. `make run DEVICE_NAME=my-phone` — leaving it unset
# never overrides a WHATSRUST_DEVICE_NAME already configured in .env.
ifdef DEVICE_NAME
DEVICE_NAME_ENV := WHATSRUST_DEVICE_NAME="$(DEVICE_NAME)"
endif

help: ## Show this help (default target)
	@echo "whatsrust — available targets:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

build: ## Build the debug binary (cargo build)
	cargo build

release: ## Build the optimized release binary (cargo build --release; slow: LTO + codegen-units=1)
	cargo build --release

# `cargo run` starts the DAEMON: interactive REPL + REST API on :7270 (loopback
# by default). On first run it prints a QR code to scan (or use
# WHATSAPP_PAIR_PHONE for pair-code linking). It does NOT exit on its own —
# stop it with Ctrl+C (graceful shutdown: drains in-flight sends, backs up the
# DB). See docs/HOWTO.md #3 "Run the daemon".
run: ## Run the daemon (interactive: QR login + REPL; Ctrl+C to stop; optional DEVICE_NAME=<name>)
	$(DEVICE_NAME_ENV) cargo run

# Native Rust embedder sidecar (embedder-rs/ — separate crate, candle-based;
# ADR 0040 — Python-free deploy). Built independently so candle stays out of the
# daemon's dependency closure. Produces embedder-rs/target/release/whatsrust-embedder.
embedder-build: ## Build the Rust embedder sidecar (embedder-rs/ — separate crate)
	cd embedder-rs && cargo build --release

# Builds both release artifacts a deployment needs: the daemon (`release`) and the Rust
# embedder sidecar (`embedder-build`). Reuses both targets as prerequisites — no separate
# recipe body. Slow (LTO + opt-level z on the daemon); see `release` above.
release-all: release embedder-build ## Build the daemon AND the embedder sidecar in release mode

# Copies the two release-mode binaries `release-all` produces into dist/, so a deployment
# only needs to ship that one folder. Prints the resulting paths plus a ready-to-use
# WHATSRUST_EMBEDDER_CMD hint for wiring the sidecar into a copied-elsewhere daemon.
dist: release-all ## Collect the release binaries (daemon + embedder sidecar) into dist/
	@mkdir -p dist
	cp target/release/whatsrust dist/whatsrust
	cp embedder-rs/target/release/whatsrust-embedder dist/whatsrust-embedder
	@echo "Daemon:           $(CURDIR)/dist/whatsrust"
	@echo "Embedder sidecar: $(CURDIR)/dist/whatsrust-embedder"
	@echo "Hint: WHATSRUST_EMBEDDER_CMD=$(CURDIR)/dist/whatsrust-embedder"

# Runs the daemon with semantic search enabled via the Rust embedder sidecar
# (whatsrust spawns the sidecar as a child process via WHATSRUST_EMBEDDER_CMD,
# with WHATSRUST_EMBEDDER_ARGS="--model $(EMBED_MODEL)" selecting the model).
# Requires a linked account. On first run, the sidecar downloads model weights
# to ~/.cache/huggingface (lazy, on first embed): ~0.46 GB for minilm, ~2.2 GB
# for bge-m3. No Python/venv. See docs/HOWTO.md #5b for the full walkthrough.
run-semantic: embedder-build ## Run the daemon with Rust embedder sidecar enabled (EMBED_MODEL=bge-m3|minilm, default bge-m3; optional DEVICE_NAME=<name>; e.g. make run-semantic EMBED_MODEL=minilm)
	WHATSRUST_EMBEDDER_CMD=$(CURDIR)/embedder-rs/target/release/whatsrust-embedder \
	WHATSRUST_EMBEDDER_ARGS="--model $(EMBED_MODEL)" \
	$(DEVICE_NAME_ENV) \
	cargo run

test: ## Run the default test suite (cargo test)
	cargo test

# The fake-embedder sidecar test bin (src/bin/fake-embedder.rs) is gated behind
# the non-default `fake-embedder` feature so a plain `cargo build`/`cargo test`
# never produces or depends on it. This target opts in to also run the
# real-subprocess round-trip tests against it.
test-all: ## Run the full suite including fake-embedder-gated sidecar tests
	cargo test --features fake-embedder

# Plain `cargo clippy` (no -D warnings): the crate currently carries ~30
# pre-existing clippy findings, so a `-D warnings` run fails out of the box.
# This target reports lints without failing the build. Use `lint-strict` for
# a fatal-on-warning pass (expected to fail until the backlog is cleaned up).
lint: ## Run clippy (warnings reported, non-fatal)
	cargo clippy

lint-strict: ## Run clippy with -D warnings (fatal; currently fails — pre-existing backlog)
	cargo clippy -- -D warnings

fmt: ## Format the codebase in place (cargo fmt)
	cargo fmt

fmt-check: ## Check formatting without writing changes (cargo fmt --check)
	cargo fmt --check

# Quick protocol check: pipes model_info + a one-text embed request into the
# Rust sidecar and prints the responses (confirms the sidecar answers correctly
# before wiring it into the daemon). Safe and non-interactive. Honors EMBED_MODEL.
embedder-smoke: embedder-build ## Test the Rust embedder sidecar standalone (JSON-RPC protocol check; EMBED_MODEL=bge-m3|minilm, default bge-m3)
	@echo "Sending model_info + embed requests to Rust sidecar (model=$(EMBED_MODEL))..."
	@(echo '{"jsonrpc":"2.0","id":1,"method":"model_info"}'; \
	  echo '{"jsonrpc":"2.0","id":2,"method":"embed","params":{"texts":["Hello world"]}}') | \
	  $(CURDIR)/embedder-rs/target/release/whatsrust-embedder --model $(EMBED_MODEL)

# Measures the cosine-similarity floor for EMBED_MODEL against the real, read-only
# whatsapp.db (word-free queries vs. their targets vs. the unrelated-message
# distribution — ADR 0041). scripts/calibrate_semantic.py needs numpy, which isn't
# in stdlib, so this reuses .venv-embedder (the same venv `py-sidecar-setup`
# creates for the legacy Python sidecar — sentence-transformers pulls numpy as a
# dependency). Re-run after switching WHATSRUST_SEMANTIC_MIN_SCORE's target model.
calibrate: embedder-build ## Measure the semantic-recall floor for EMBED_MODEL against whatsapp.db (needs .venv-embedder: make py-sidecar-setup)
	@if [ ! -f .venv-embedder/bin/python3 ]; then \
		echo "Error: .venv-embedder not found (calibrate_semantic.py needs numpy). Run 'make py-sidecar-setup' first." >&2; \
		exit 1; \
	fi
	.venv-embedder/bin/python3 scripts/calibrate_semantic.py \
	  --binary $(CURDIR)/embedder-rs/target/release/whatsrust-embedder \
	  --models $(EMBED_MODEL)

# ---------------------------------------------------------------------------
# Legacy Python embedder sidecar (reference/fallback per ADR 0040)
# ---------------------------------------------------------------------------

# Create the .venv-embedder virtualenv and install sentence-transformers.
# Uses `uv` if available (fast), else falls back to `python3 -m venv` + `pip`.
# The embedding model (~420 MB: paraphrase-multilingual-MiniLM-L12-v2)
# downloads on first run to ~/.cache/huggingface.
py-sidecar-setup: ## [Python sidecar] Create .venv-embedder venv and install sentence-transformers
	@if command -v uv >/dev/null 2>&1; then \
		echo "Using uv to create venv..."; \
		uv venv .venv-embedder && \
		uv pip install --python .venv-embedder/bin/python3 sentence-transformers; \
	else \
		echo "Using python3 -m venv (uv not found; install uv for faster setup)..."; \
		python3 -m venv .venv-embedder && \
		.venv-embedder/bin/pip install sentence-transformers; \
	fi
	@echo "✓ .venv-embedder ready. Run 'make run-semantic-py' to start the daemon with the Python sidecar."

# Quick protocol check: pipe a model_info JSON-RPC request to the Python sidecar
# and print the response (confirms the sidecar answers before wiring it into the
# daemon). Fails fast if the venv is missing.
py-sidecar-smoke: ## [Python sidecar] Test the Python embedder sidecar standalone (JSON-RPC protocol check)
	@if [ ! -f .venv-embedder/bin/python3 ]; then \
		echo "Error: .venv-embedder not found. Run 'make py-sidecar-setup' first." >&2; \
		exit 1; \
	fi
	@echo "Sending model_info request to Python sidecar..."
	@echo '{"jsonrpc":"2.0","id":1,"method":"model_info"}' | .venv-embedder/bin/python3 scripts/embedder-sidecar.py

# Run the daemon with the legacy Python embedder sidecar (reference/fallback).
run-semantic-py: ## [Python sidecar] Run the daemon with Python embedder sidecar (legacy; requires .venv-embedder)
	WHATSRUST_EMBEDDER_CMD=./scripts/run-embedder.sh cargo run

clean: ## Remove build artifacts (cargo clean) and dist/
	cargo clean
	rm -rf dist
