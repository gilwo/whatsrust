#!/bin/bash
cd "$(dirname "$0")/.."
exec .venv-embedder/bin/python3 scripts/embedder-sidecar.py
