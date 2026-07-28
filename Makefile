# whatsrust — root Makefile
#
# Thin wrapper over `cargo`. See docs/HOWTO.md for the full operational playbook
# (configure, run, CLI usage, MCP mode, DB migration recovery).

.DEFAULT_GOAL := help

.PHONY: help build release run test test-all lint lint-strict fmt fmt-check clean

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

clean: ## Remove build artifacts (cargo clean)
	cargo clean
