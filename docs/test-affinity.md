# Test affinity in batch scheduling

## Problem

The LPT scheduler (`src/orchestrator/scheduler.rs`) models a batch's cost as the
flat sum of its tests' predicted durations. This is blind to a real cost in
interpreted/heavyweight-import languages: **per-module load overhead**. In a
large Python project, importing a test module (and its transitive imports,
`conftest.py` fixtures, etc.) is paid once per module, per process. Two batches
with identical summed test runtime can have very different *actual* runtime if
one spreads its tests across 20 modules and the other concentrates them in 3.

The scheduler currently has no notion of which module a test belongs to, so it
cannot exploit this.

## Concept: test affinity

Give each test an **affinity key** — the unit that carries shared load cost,
typically the source module/file. Model a batch's effective cost as:

```
batch_cost = Σ test_durations  +  affinity_overhead × (number of distinct affinity keys in batch)
```

Then make LPT assignment minimize *effective cost*, which naturally rewards
packing tests that share a key into the same batch (the key's overhead is paid
once, not once per batch it appears in).

## Design

The mechanism is **generic**. Only two things are language-specific, and they
are the two pluggable knobs:

### Knob 1 — affinity-key derivation (Rust, per framework)

How to extract the key from a test is framework structural knowledge, so it
lives in the framework layer alongside `discover()`. Each framework sets the key
on the `TestRecord` it produces during discovery. For pytest the key is the
module: the substring of the test ID before `::`
(`tests/test_foo.py::test_bar` → `tests/test_foo.py`). Frameworks that opt out
leave the key `None`.

A `None` key means "no affinity" — the test contributes no overhead and gets no
packing preference, so the scheduler degrades to today's pure-duration LPT.

### Knob 2 — overhead magnitude (config, with a per-framework default)

The cost of loading one key's worth of modules is a tuning number, exposed as
config and defaulted in Rust per framework:

```toml
[framework]
type = "pytest"
affinity_overhead_secs = 2.0   # default for pytest; override or set 0.0 to disable
```

- pytest default: `2.0` seconds.
- all other frameworks default: `0.0` (feature is a no-op).

`FrameworkConfig` exposes the resolved value as a `Duration` via an
`affinity_overhead()` accessor; the orchestrator passes it into the scheduler.

### Scheduler change

`Scheduler::new` gains an `affinity_overhead: Duration` parameter. The internal
`Batch` additionally tracks the set of distinct affinity keys it contains. LPT
assignment changes from "assign to the batch with the smallest current load" to
"assign to the batch with the smallest **marginal cost increase**":

```
marginal_cost(batch, test) =
    test_duration
    + if test.key is None or batch already contains test.key { 0 }
      else { affinity_overhead }
```

The greedy structure (sort longest-first, place each test into the
lowest-marginal-cost eligible batch) is unchanged; only the comparison key is
extended. With `affinity_overhead = Duration::ZERO` or all-`None` keys, the
behavior is byte-for-byte identical to today.

## Properties

- **Backward compatible.** Zero overhead or absent keys ⇒ no behavior change.
  Non-pytest frameworks pay nothing.
- **Generic scheduler.** The scheduler consumes an opaque `Option<String>` key
  and a `Duration`; it contains no Python-specific logic.
- **Tunable.** Overhead too high → over-concentration → worse load balance and a
  longer makespan tail. The value is user-overridable per framework.

## Risk / open follow-ups

- The fixed per-key overhead is a coarse model; a future iteration could learn it
  from history (e.g. the `JsonlHistoryStore`) instead of a config constant. The
  config seam is designed to allow swapping the source later without touching the
  scheduler.
- Affinity packing trades load-balance evenness for fewer module loads. If real
  suites show a regression in makespan, consider a guardrail capping how far
  affinity may skew batch balance.

## Implementation breakdown (beads)

1. Add `affinity_key: Option<String>` to `TestRecord`/`TestInstance` (data layer).
2. Derive the pytest affinity key (module prefix) during discovery.
3. Add `affinity_overhead_secs` config + `FrameworkConfig::affinity_overhead()`.
4. Extend the LPT scheduler with the marginal-cost / distinct-key cost model.
5. Wire the framework overhead from config into the scheduler in the orchestrator.
</content>
</invoke>
