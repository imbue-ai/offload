# Testing

## Required test runner
Use `cargo-nextest` for running tests.

Rationale:
- consistent reporting
- better control of timeouts
- parallel execution

## Hard timeout rule (agent safety)
**Every full test run must complete within 120 seconds.**

If tests exceed 120 seconds, that is a failure. The goal is to prevent agent workflows from getting stuck in infinite loops or pathological hangs.

## Repository configuration
The repo includes `.config/nextest.toml` with the following configuration:

- `profile.default.slow-timeout = { period = "60s", terminate-after = 2 }` -- enforces the 120-second deadline.
- `profile.default.junit` -- writes JUnit XML to `/tmp/junit.xml`, stores failure output only.
- `profile.ci.junit` -- same JUnit settings for the CI profile.

## Standard commands
Run these before declaring a task "done":

- Format:
  - `cargo fmt --check`
- Lint:
  - `cargo clippy --all-targets --all-features`
- Tests (strict timeout):
  - `cargo nextest run`

The `justfile` also provides a `just test` recipe that runs `cargo nextest run`.

## Running offload tests

**Agents validating offload's own code should use `just test-cargo-modal`.** This is the primary target for verifying correctness of offload itself. `offload.toml` is a symlink to `offload-cargo-modal.toml`, so bare `offload run` also works.

### Test matrix design

The full test matrix is **frameworks x providers**, used to validate offload's machinery for handling different test frameworks and provider abstractions:

- **Frameworks**: `cargo`, `pytest`
- **Providers**: `local`, `modal`, `default`

The `cargo` tests exercise offload's own Rust code. The `pytest` entries exercise offload's ability to discover and run pytest suites. The `local`/`default` provider entries validate that offload works across provider implementations — they do not test offload's core logic.

The `default` provider has no implementation of its own. It must be backed by a real provider. Currently it is backed by Modal. The `default` matrix entries verify that the default-provider abstraction correctly delegates to the underlying implementation.

### Targets

The `justfile` provides targets for all matrix combinations using the naming convention `test-{framework}-{provider}`:

| Target | Config file | Description |
|--------|-------------|-------------|
| `just test-cargo-local` | `offload-cargo-local.toml` | Cargo tests, local provider |
| `just test-pytest-local` | `offload-pytest-local.toml` | Pytest tests, local provider |
| `just test-cargo-modal` | `offload-cargo-modal.toml` | Cargo tests, Modal provider |
| `just test-pytest-modal` | `offload-pytest-modal.toml` | Pytest tests, Modal provider |
| `just test-cargo-default` | `offload-cargo-default.toml` | Cargo tests, default provider |
| `just test-pytest-default` | `offload-pytest-default.toml` | Pytest tests, default provider |

## Guidance for writing tests
- Prefer deterministic tests (avoid timing sensitivity).
- If a test is intentionally slow, mark it `#[ignore]` and document how/when to run it.
- When using randomness, seed it.
- Keep unit tests close to the code they verify; keep integration tests in `tests/`.

## If tests exceed the deadline
- Treat it as a bug
- Add a reproduction and fix the underlying deadlock/infinite loop/slow test
- Do not "just bump the timeout". Timeout bumping is only performed by humans
- If you cannot get all tests to complete in less time, do not delete tests by other agents or existing tests
- Prefer to fail your task instead
