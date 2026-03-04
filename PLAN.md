# Plan: Perfetto Trace Visualization for Offload

## Execution Workflow

1. Create a new jj change on top of `main`
2. Write this content as `PLAN.md` in the repo root — this is **Revision 1** (the Plan)
3. Describe the revision and create a bookmark `feature/perfetto-trace`
4. Create subsequent revisions (one per implementation step) on top of the Plan revision
5. Each implementation revision follows TDD: failing test first, then minimal code

## Branch Rules

This is a **floating feature branch**. It is never merged to main.

- **Revision 1** (PLAN.md): The Plan. Permanent. Never rewritten.
- **Revisions 2..n**: Implementation. Ephemeral. Rewritten on each rebase onto main.

### Rebase Protocol

1. Rebase implementation revisions onto latest main, keeping the Plan revision as the base.
2. If conflicts arise, consult PLAN.md and the current state of main. Re-implement rather than force-resolve.
3. The Plan revision is the source of truth. Implementation revisions serve it.

---

## Feature: `--trace` (Perfetto Trace Output)

### Goal

Emit a Chrome Trace Event JSON file during `offload run` that visualizes where time is spent across the distributed test execution pipeline. Users open the file in [Perfetto UI](https://ui.perfetto.dev/).

### Latency Categories

| Category | Phase | Clock Source |
|----------|-------|-------------|
| Config loading | Local | Local Instant |
| Test discovery | Local (subprocess) | Local Instant |
| Image preparation | Local→Remote (Modal API) | Local Instant |
| Sandbox pool creation | Local→Remote (Modal API) | Local Instant |
| Duration loading | Local (file I/O) | Local Instant |
| Scheduling (LPT/RR) | Local (CPU) | Local Instant |
| Batch execution | Remote (per-sandbox) | Local Instant (wraps remote call) |
| JUnit XML download | Remote→Local (I/O) | Local Instant |
| Result aggregation | Local (CPU + I/O) | Local Instant |
| Sandbox cleanup | Local→Remote (Modal API) | Local Instant |

All timestamps use a single local `Instant` epoch. Remote-reported times (from JUnit XML `time` attributes) are annotated with `{"source": "remote_junit"}` in trace event args to flag potential clock skew.

### Trace Layout (Process/Thread IDs)

```
PID 0: "Offload (Local)"
  TID 0: Main — discovery, scheduling, aggregation, cleanup

PID 1..N: "Sandbox 0".."Sandbox N-1"
  TID 0: API  — create, terminate
  TID 1: Exec — batch execution spans
  TID 2: I/O  — JUnit download, result parsing
```

### Activation

- CLI: `offload run --trace`
- Output: `{output_dir}/trace.json` (default: `test-results/trace.json`)
- When disabled: zero-cost no-op (enum dispatch, no allocation)

---

## Implementation Plan

### Step 1: Create `src/trace.rs` — Core module

New file. Contains:

- `TraceEvent` struct (serde-serializable to Chrome Trace Event JSON)
- `Tracer` enum: `Active(Arc<ActiveTracer>)` | `Noop`
- `ActiveTracer`: holds `epoch: Instant` and `events: Mutex<Vec<TraceEvent>>`
- `SpanGuard`: RAII guard that records an "X" (complete) event on drop
- Methods: `complete_event`, `instant_event`, `metadata_event`, `span`, `to_json`, `write_to_file`
- Constants: `PID_LOCAL`, `TID_MAIN`, `TID_API`, `TID_EXEC`, `TID_IO`, `sandbox_pid(index)`
- Unit tests: noop tracer no-ops, active tracer collects events, SpanGuard measures duration, JSON output is valid array

No new crate dependencies — uses existing `serde`, `serde_json`.

### Step 2: Register module in `src/lib.rs`

Add `pub mod trace;` and `pub use trace::Tracer;`.

### Step 3: Add `--trace` CLI flag in `src/main.rs`

- Add `trace: bool` field to `Commands::Run`
- Create `Tracer::new()` or `Tracer::noop()` based on flag
- After run completes, call `tracer.write_to_file()`
- Print path to stderr

### Step 4: Instrument `src/main.rs` — local phases

Wrap with `tracer.span(...)`:
- Test discovery loop (lines ~236-275)
- Provider `from_config()` / image preparation
- `sandbox_pool.populate()` call

### Step 5: Thread Tracer through Orchestrator (`src/orchestrator.rs`)

- Add `tracer: Tracer` field to `Orchestrator`
- Emit `metadata_event` for process/thread names
- Instrument: duration loading, scheduling, result aggregation, sandbox cleanup

### Step 6: Thread Tracer through SpawnConfig (`src/orchestrator/spawn.rs`)

- Add `tracer: Tracer` and `sandbox_index: usize` to `SpawnConfig`
- In `spawn_task()`: wrap each batch pull+execute cycle with a span
- Attach batch metadata (test IDs, batch index, sandbox ID) to span args

### Step 7: Thread Tracer through TestRunner (`src/orchestrator/runner.rs`)

- Add `tracer: Tracer` and `sandbox_pid: u32` to `TestRunner`
- Instrument: `exec_with_streaming` (TID_EXEC), `try_download_results` (TID_IO), `add_junit_xml` (TID_IO)

### Step 8: Document in `README.md`

Add section for `--trace` flag and Perfetto UI usage.

---

## Files Changed

| File | Action | Scope |
|------|--------|-------|
| `src/trace.rs` | CREATE | Core module (~200 lines + tests) |
| `src/lib.rs` | MODIFY | 2 lines added |
| `src/main.rs` | MODIFY | CLI flag + tracer lifecycle + local phase spans |
| `src/orchestrator.rs` | MODIFY | Tracer field + orchestration spans |
| `src/orchestrator/spawn.rs` | MODIFY | SpawnConfig fields + batch spans |
| `src/orchestrator/runner.rs` | MODIFY | TestRunner fields + exec/IO spans |
| `README.md` | MODIFY | Documentation section |

## Commit Sequence

1. "Add trace module with Tracer, TraceEvent, and SpanGuard"
2. "Add --trace CLI flag and tracer lifecycle"
3. "Instrument local phases: discovery, image prepare, pool creation"
4. "Instrument orchestrator: scheduling, aggregation, cleanup"
5. "Instrument spawn workers with per-batch trace spans"
6. "Instrument test runner with exec and I/O trace spans"
7. "Document --trace flag in README"

Each commit: `cargo fmt --check` + `cargo clippy` + `cargo nextest run` pass.

## Verification

1. `cargo nextest run` — all existing + new unit tests pass
2. `offload run --trace` with a local provider — produces `test-results/trace.json`
3. Open `trace.json` in https://ui.perfetto.dev/ — timeline shows labeled processes and spans
4. Verify no overhead when `--trace` is omitted (no allocations, no file written)
