---
name: offload-benchmark
description: "Run local vs Offload benchmarks for mng and sculptor, then update the Benchmarks section of the Offload README."
---

# Offload Benchmark

Run local and Offload test suites for **mng** and **sculptor**, collect timing data, and update the `## Benchmarks` section of the Offload README with fresh numbers.

## Prerequisites

1. Offload must be installed: `cargo install offload` (or built from source in the offload repo).
2. Modal must be authenticated: `modal token new` (if credentials are expired).
3. Both repos must exist locally:
   - **mng**: `~/imbue/mng` (or wherever the monorepo is checked out)
   - **sculptor**: `~/imbue/sculptor`
4. Run `just install` from the offload repo root to ensure the binary is up to date.

Verify all four before proceeding. If any prerequisite is missing, stop and tell the user.

## Step 1: Read Current Test Commands

**Do not hardcode commands.** Read the justfile in each repo at invocation time, since commands change:

- **mng**: Read `~/imbue/mng/justfile` and locate:
  - `test-integration` -- the local baseline (unit + integration tests via pytest with xdist)
  - `test-offload` -- the Offload run on Modal
- **sculptor**: Read `~/imbue/sculptor/justfile` and locate:
  - `test-integration` -- the local baseline (Playwright integration tests via pytest with xdist)
  - `test-integration-offload` -- the Offload run on Modal

Record the exact commands from each justfile. If any target is missing or has changed significantly, stop and tell the user.

Also read each justfile to determine:
- The default xdist worker count for the local baseline (look for `-n auto --maxprocesses N` or `-n N`)
- The higher xdist worker count to use for a second local run (use double the default, capped at 8)

## Step 2: Run Benchmarks

**CRITICAL RULES:**
- NEVER run any two benchmark runs concurrently. Each run must have exclusive machine resources. Run them strictly sequentially.
- Close all other heavy processes (browsers, IDEs, Docker containers) before starting, or at minimum warn the user to do so.
- Use `time` (bash builtin) to measure wall-clock time for each run. Wrap each command in `time (...)` and record the real time.
- If any tests fail in a run, flag the run as **INVALID** but still record the timing. Report the failure to the user at the end.

### 2a: mng benchmarks

Run these three commands sequentially from the mng repo root (`~/imbue/mng`):

1. **Local baseline** (default xdist workers):
   ```
   time just test-integration
   ```
   Record the wall-clock time.

2. **Local high-xdist** (override to higher worker count):
   Read the default `-n` value from the justfile. Run with double that value (capped at 8). For example, if the default is `-n 4`:
   ```
   time PYTEST_NUMPROCESSES=8 just test-integration
   ```
   If the justfile uses `PYTEST_NUMPROCESSES` or a similar env var for xdist workers, use that. Otherwise, pass the appropriate flag or env var. Read the justfile carefully to determine the correct override mechanism.

3. **Offload (warm cache)**: Run the offload command TWICE. Discard the first run's timing (it warms the image cache). Record only the second run's timing.
   ```
   just test-offload          # warm-up run (discard timing)
   time just test-offload     # benchmark run (record timing)
   ```

Record the number of tests collected (visible in pytest output) and the xdist `-n` values used.

### 2b: sculptor benchmarks

Run these three commands sequentially from the sculptor repo root (`~/imbue/sculptor`):

1. **Local baseline** (default xdist workers):
   ```
   time just test-integration
   ```
   Record the wall-clock time.

2. **Local high-xdist** (override to higher worker count):
   Read the default `--maxprocesses` value from the justfile (currently 3). Run with a higher value:
   ```
   time XDIST_WORKERS=8 just test-integration
   ```
   Use the override mechanism from the justfile (`XDIST_WORKERS` env var).

3. **Offload (warm cache)**: Run the offload command TWICE. Discard the first run's timing. Record only the second.
   ```
   just test-integration-offload          # warm-up run (discard timing)
   time just test-integration-offload     # benchmark run (record timing)
   ```

Record the number of tests collected and the xdist `-n` values used.

## Step 3: Compute Metrics

For each project, compute:

| Metric | Formula |
|--------|---------|
| Time (s) | Wall-clock seconds from `time`, rounded to 1 decimal |
| Time (%) | `(run_time / baseline_time) * 100`, rounded to 1 decimal |
| Speedup | `baseline_time / run_time`, rounded to 2 decimals |
| Bar width (px) | `round((run_time / baseline_time) * 150)` -- baseline is always 150px |

The baseline is always the first run (default xdist) for each project.

## Step 4: Generate the Updated Benchmarks Section

Replace the entire `## Benchmarks` section in the Offload README (from `## Benchmarks` up to but not including the next `##` heading) with the template below, filled in with measured values.

Use the correct labels for each project's test suite:
- **sculptor**: "Integration Tests (Playwright)"
- **mng**: "Unit + Integration Tests"

The bar chart images reference `docs/bar-local.svg` (gray, `#444`) and `docs/bar-offload.svg` (green, `#22a355`). These are 1x1 SVG rectangles scaled via the `width` attribute.

### Template

```markdown
## Benchmarks

Speedups measured on Imbue projects using Offload with the Modal provider. All local baselines were run on a {MACHINE_DESCRIPTION}.

### Sculptor Integration Tests (Playwright)

| Run Kind | Time (s) | Time (%) | Speedup |
|----------|----------|----------|---------|
| pytest with xdist, n={SCULPTOR_BASELINE_N} (baseline) | <img src="docs/bar-local.svg" width="150" height="4"> {SCULPTOR_BASELINE_TIME} | 100.0% | 1.00x |
| pytest with xdist, n={SCULPTOR_HIGH_N} | <img src="docs/bar-local.svg" width="{SCULPTOR_HIGH_BAR_WIDTH}" height="4"> {SCULPTOR_HIGH_TIME} | {SCULPTOR_HIGH_PCT}% | {SCULPTOR_HIGH_SPEEDUP}x |
| Offload (Modal, max {SCULPTOR_MAX_PARALLEL}) | <img src="docs/bar-offload.svg" width="{SCULPTOR_OFFLOAD_BAR_WIDTH}" height="4"> {SCULPTOR_OFFLOAD_TIME} | {SCULPTOR_OFFLOAD_PCT}% | **{SCULPTOR_OFFLOAD_SPEEDUP}x** |

<details>
<summary><strong>Notes</strong></summary>

{SCULPTOR_TEST_COUNT} Playwright integration tests (browser-based, each launching a full Sculptor instance).
Individual tests are heavyweight (Chromium + backend server per worker), so the default xdist cap is n={SCULPTOR_BASELINE_N}.
Offload bypasses xdist entirely, fanning out across up to {SCULPTOR_MAX_PARALLEL} isolated Modal sandboxes -- each running a single test against its own Sculptor instance. The high per-test cost makes Offload's per-sandbox overhead negligible, yielding a {SCULPTOR_OFFLOAD_SPEEDUP}x speedup.

</details>

### Mng Unit + Integration Tests

| Run Kind | Time (s) | Time (%) | Speedup |
|----------|----------|----------|---------|
| pytest with xdist, n={MNG_BASELINE_N} (baseline) | <img src="docs/bar-local.svg" width="150" height="4"> {MNG_BASELINE_TIME} | 100.0% | 1.00x |
| pytest with xdist, n={MNG_HIGH_N} | <img src="docs/bar-local.svg" width="{MNG_HIGH_BAR_WIDTH}" height="4"> {MNG_HIGH_TIME} | {MNG_HIGH_PCT}% | {MNG_HIGH_SPEEDUP}x |
| Offload (Modal, max {MNG_MAX_PARALLEL}) | <img src="docs/bar-offload.svg" width="{MNG_OFFLOAD_BAR_WIDTH}" height="4"> {MNG_OFFLOAD_TIME} | {MNG_OFFLOAD_PCT}% | **{MNG_OFFLOAD_SPEEDUP}x** |

<details>
<summary><strong>Notes</strong></summary>

{MNG_TEST_COUNT} tests collected (unit + integration, excluding acceptance and release).
Individual tests are lightweight and fast-running, so the default xdist cap is n={MNG_BASELINE_N}.
Offload bypasses xdist entirely, fanning out across up to {MNG_MAX_PARALLEL} isolated Modal sandboxes. The low per-test cost makes Offload's per-sandbox overhead proportionally larger, yielding a more modest {MNG_OFFLOAD_SPEEDUP}x speedup vs Sculptor's {SCULPTOR_OFFLOAD_SPEEDUP}x.

</details>
```

### Placeholder Reference

| Placeholder | Source |
|-------------|--------|
| `{MACHINE_DESCRIPTION}` | Ask the user, or read from the existing README intro paragraph |
| `{*_BASELINE_N}` | Default xdist `-n` value from the justfile |
| `{*_HIGH_N}` | The higher xdist count used in run 2 |
| `{*_BASELINE_TIME}` | Wall-clock seconds from run 1 |
| `{*_HIGH_TIME}` | Wall-clock seconds from run 2 |
| `{*_OFFLOAD_TIME}` | Wall-clock seconds from run 3 (second invocation only) |
| `{*_HIGH_PCT}` | `(high_time / baseline_time) * 100` |
| `{*_OFFLOAD_PCT}` | `(offload_time / baseline_time) * 100` |
| `{*_HIGH_SPEEDUP}` | `baseline_time / high_time` |
| `{*_OFFLOAD_SPEEDUP}` | `baseline_time / offload_time` |
| `{*_HIGH_BAR_WIDTH}` | `round((high_time / baseline_time) * 150)` |
| `{*_OFFLOAD_BAR_WIDTH}` | `round((offload_time / baseline_time) * 150)` |
| `{*_MAX_PARALLEL}` | Read from the relevant `offload*.toml` config (`max_parallel` field) |
| `{*_TEST_COUNT}` | Number of tests collected, from pytest output |

## Step 5: Update the README

1. Read `offload/README.md`.
2. Find the `## Benchmarks` section (starts at `## Benchmarks`, ends just before the next `## ` heading).
3. Replace that entire section with the generated content from Step 4.
4. Write the file back.
5. Verify the replacement by reading the file again and confirming the new numbers appear.

## Step 6: Report Results

Summarize to the user:

1. A table of all six runs with their wall-clock times and pass/fail status.
2. Any runs flagged as INVALID (test failures).
3. The computed speedups.
4. Confirmation that the README was updated (or not, if any run was INVALID -- in that case, ask the user whether to update anyway).

## Rules

- **Never run benchmarks concurrently.** Sequential execution only.
- **Run Offload twice, take the second time.** The first run warms the Modal image cache.
- **If any tests fail, report results but mark as INVALID.** Ask the user before updating the README with invalid data.
- **Maintain parallel phrasing in Notes sections.** Both projects' Notes blocks should follow the same structure: test count and type, why the default xdist cap is what it is, how Offload bypasses xdist, and why the speedup is high or low.
- **Read justfiles at invocation time.** Commands may have changed since this skill was written.
- **Preserve the rest of the README.** Only replace the `## Benchmarks` section.
