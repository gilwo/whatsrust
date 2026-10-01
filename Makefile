# whatsrust — root Makefile
#
# Thin wrapper over `cargo`. See docs/HOWTO.md for the full operational playbook
# (configure, run, CLI usage, MCP mode, DB migration recovery).

.DEFAULT_GOAL := help

.PHONY: help build release run embedder-build run-semantic embedder-smoke test test-all lint lint-strict fmt fmt-check clean py-sidecar-setup py-sidecar-smoke run-semantic-py

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
run: ## Run the daemon (interactive: QR login + REPL; Ctrl+C to stop)
	cargo run

# Native Rust embedder sidecar (embedder-rs/ — separate crate, candle-based;
# ADR 0040 — Python-free deploy). Built independently so candle stays out of the
# daemon's dependency closure. Produces embedder-rs/target/release/whatsrust-embedder.
embedder-build: ## Build the Rust embedder sidecar (embedder-rs/ — separate crate)
	cd embedder-rs && cargo build --release

# Runs the daemon with semantic search enabled via the Rust embedder sidecar
# (whatsrust spawns the sidecar as a child process via WHATSRUST_EMBEDDER_CMD).
# Requires a linked account. On first run, the sidecar downloads ~420 MB of
# weights to ~/.cache/huggingface (lazy, on first embed, ~2s). No Python/venv.
# See docs/HOWTO.md #5a "Embedder sidecar" for the full setup walkthrough.
run-semantic: embedder-build ## Run the daemon with Rust embedder sidecar enabled (requires linked account)
	WHATSRUST_EMBEDDER_CMD=$(CURDIR)/embedder-rs/target/release/whatsrust-embedder cargo run

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
# before wiring it into the daemon). Safe and non-interactive.
embedder-smoke: embedder-build ## Test the Rust embedder sidecar standalone (JSON-RPC protocol check)
	@echo "Sending model_info + embed requests to Rust sidecar..."
	@(echo '{"jsonrpc":"2.0","id":1,"method":"model_info"}'; \
	  echo '{"jsonrpc":"2.0","id":2,"method":"embed","params":{"texts":["Hello world"]}}') | \
	  $(CURDIR)/embedder-rs/target/release/whatsrust-embedder

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

clean: ## Remove build artifacts (cargo clean)
	cargo clean
