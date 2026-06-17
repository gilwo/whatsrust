# 0001. Increase tokio worker thread stack to 8 MB

**Status:** Accepted  
**Date:** 2026-06-17

## Context

The daemon was crashing silently on inbound message processing with no error logs. Root cause: huge `handle_event` / `extract_content_inner` async state machines exceeded the default 2 MB tokio worker thread stack, triggering stack overflow.

Tokio's default worker stack size is 2 MB per thread. WhatsApp message extraction is deeply recursive (nested message content, quoted messages, forwarded chains) and async state machines for these call chains can allocate large stack frames.

Committed in `0c8228a`.

## Decision

Increase tokio worker thread stack size to 8 MB via `tokio::runtime::Builder::thread_stack_size(8 * 1024 * 1024)` in `main.rs`.

## Consequences

**Positive:**
- Fixes silent stack overflow crashes on complex message processing
- Allows deeply nested message extraction without refactoring to heap allocation
- 8 MB provides headroom for future extraction complexity

**Negative:**
- Increases per-worker memory footprint (tokio default worker count = num_cpus)
- 8 MB × 8 cores = 64 MB additional baseline memory (acceptable for daemon workload)

**Deferred:**
- Could refactor `extract_content_inner` to use heap-allocated iterative traversal instead of recursive descent, but stack increase is simpler and sufficient for current complexity
