# 0026. Backfill worker topology and abort granularity

**Status:** Accepted  
**Date:** 2026-06-22

## Context

The backfill worker must safely handle cancellation (explicit user cancel, daemon shutdown) without corrupting the frontier cursor or leaving partial batches. The worker shares a single SQLite connection with outbound/embedding-drain workers, so concurrency must be controlled.

**Existing pattern:** Outbound worker is a SINGLE task inside `run_bot_session`, lives/dies per bot session, restarts on reconnect. `claim_next_job` atomic via single `Mutex<Connection>`: SELECT + UPDATE status='inflight' in one TX. Shutdown drain exists (`bridge.rs:2246` cancel arm): stop claiming → flush in-flight → drain_timeout → disconnect.

**Design tensions:**
- Parallelism vs ban-risk: anti-ban pacer budget (~4s/batch) is GLOBAL → N workers share the same rate → zero throughput gain, only fairness.
- Fairness rarely matters for infrequent explicitly-triggered + cooldown'd operations.
- Abort/cancel must not corrupt atomic batch operations (PDO send → response → persist messages + advance cursor in one TX).

## Decision

**Single worker, sequential FIFO.** Genuine twin of outbound worker. "Concurrency cap" = 1 by construction; excess requests enqueue (status='queued'). Dissolves "sequential loop vs cap 1-2" contradiction from design review.

**Three-level abort granularity:**
1. **BATCH** = {send PDO → await response → persist msgs + advance cursor in ONE TX} — NEVER interrupted once started (atomic; crash-before-commit → re-fetch idempotent via `INSERT OR IGNORE` on message_id; crash-after → cursor advanced, no gap).
2. **JOB** = one chat, many batches — abortable only AT batch boundaries → terminal status 'cancelled', resumable via cursor.
3. **TASK** = worker loop — never hard-aborted (no `JoinHandle::abort`), cooperative stop on cancel token.

**Inter-batch sleep IS interruptible:** `tokio::select!` on sleep vs cancel signal — safe (no connection/PDO/TX in flight), keeps cancel responsive (instant if sleeping vs up to 120s if we waited out a long pause). Batch itself never torn.

**Cancel-race fix:** Terminal status write is CASE-guarded:
```sql
UPDATE backfill_jobs 
SET status = CASE 
    WHEN status='cancelled' THEN 'cancelled' 
    ELSE ?computed 
END 
WHERE id=?
```
Worker never downgrades a cancel set by the API. Single worker ⇒ only one race (worker vs cancel-API), closed transactionally.

**Cancel signal:** TWO channels — durable (backfill_jobs.status='cancelled', survives restart, = truth) + in-memory (CancellationToken/Notify, wakes the sleep immediately).

**Shutdown unification:** SIGINT/shutdown REUSES the same abort model (batch inviolable, sleep interruptible, stop at boundary) via the bridge CancellationToken (same path as Ctrl-C fix 36ae622). **KEY DIVERGENCE:** terminal state is a function of STOP REASON:
- **cancel-API** → 'cancelled' (dead)
- **shutdown** → 'queued'/resumable (NOT cancelled — else every daemon bounce silently kills long backfills)

Worker boundary branches on "is THIS job cancelled (row) vs is the DAEMON shutting down (token fired, row not cancelled)". Backfill on shutdown = "STOP at batch boundary + requeue", NOT "drain the whole job" (diverges from outbound's flush-on-shutdown — a 5000-msg backfill can't finish in a 10s drain window). Respect `drain_timeout_secs`: don't start a new batch on shutdown; in-flight batch TX either commits within window or rolls back & re-runs next start (safe either way).

**Connection-gated:** PDO needs live WA connection, worker pauses when disconnected (like outbound). Embedding-drain worker is SEPARATE always-on task (talks only to sidecar, not WA).

## Consequences

**Positive:**
- Single worker eliminates concurrency coordination complexity (no semaphore, no claim contention)
- Three-level granularity makes abort reasoning straightforward: batch atomic, job resumable, task cooperative
- CASE-guarded status prevents cancel races without distributed locks
- Shutdown preserves long-running backfills (resumable across daemon restarts)
- Interruptible sleep keeps cancel responsive without sacrificing batch atomicity

**Negative:**
- No fairness across multiple pending backfills (FIFO only) — acceptable given per-chat cooldown + infrequent triggers
- Shutdown diverges from outbound (requeue vs drain) — adds a restart path to test

**Future:**
- If fairness becomes a stated requirement (rapid triggers across N chats), add priority queue or round-robin claim, but keep single worker (parallelism still buys nothing).
