# Migrate off deprecated Modal Sandbox APIs

> I would like to deprecate our dependency on Sandbox.open() which is deprecated in Modal.
> * Audit `scripts/modal_sandbox.py` and migrate **only** Modal SDK calls that are genuinely deprecated; leave non-deprecated calls untouched (no drive-by refactors).
> * Hard-pin the script's `modal` dependency to a specific latest-stable Modal version that exposes `Sandbox.filesystem`.
> * Confirmed deprecated calls in scope: `sandbox.open()` (2 sites in `copy_dir_to_sandbox` and `copy_from_sandbox`) and `sandbox.mkdir()` (1 site, deprecated 2026-04-15). No other deprecated SDK surface is used.
> * Replace `sandbox.open(tar_remote_path, "wb").write(tar_data)` with `sandbox.filesystem.write_bytes(...)` — minimal 1:1 swap; defer the "write tar directly to disk" memory optimization.
> * Replace the download path with `sandbox.filesystem.copy_to_local(remote, local)` so Modal streams to disk.
> * Replace `sandbox.mkdir(remote_dir, parents=True)` with `sandbox.filesystem.make_directory(remote_dir, parents=True)`.
> * Two commits in one PR: (i) bump `modal` pin (no other changes), (ii) migrate the three deprecated call sites. Allows clean bisection.
> * Verification = existing `.github/workflows/test-modal.yml` CI + manual end-to-end run via `mngr`. No new automated tests.
> * Incidental log/error-message/timing drift from the new APIs is acceptable; no effort spent matching old output verbatim.

## Overview

- Modal deprecated the `Sandbox.open()` API on 2026-03-09 and `Sandbox.mkdir()` on 2026-04-15, moving file-I/O onto a `Sandbox.filesystem` namespace (`write_bytes`, `read_bytes`, `copy_from_local`, `copy_to_local`, `make_directory`, etc.). Our pinned `modal==1.4.1` predates the new namespace, so we are sitting on warnings today and will eventually break when Modal removes the old methods.
- Scope is intentionally narrow: only `scripts/modal_sandbox.py` calls these APIs. The Rust side never touches Modal's Python SDK directly. No other deprecated SDK surface (`environment_name`, `pty_info`, `client`, `Sandbox.ls`, `Sandbox.rm`, `Sandbox.watch`) is in use, so we leave the rest of the script alone.
- Download path swaps from "read whole file into Python memory, then write locally" to `copy_to_local`, which streams to disk. This is a behaviour improvement for large artifact bundles.
- Upload path keeps the existing in-memory tar approach and does a 1:1 swap to `filesystem.write_bytes`. The tar buffer is already fully materialized in memory before the write, so going to a temp file would add disk I/O for zero memory benefit; that optimization is deferred to a follow-up bead if it ever matters.
- Change ships as one PR with two commits — Modal version bump first, then API migration — so either step can be bisected if regressions surface.

## Expected behavior

- `offload run` and `offload prepare` against a Modal provider continue to work end-to-end: image builds, sandbox creation, directory uploads, command execution, artifact downloads, and sandbox teardown all succeed.
- `scripts/modal_sandbox.py download` produces byte-identical local files compared to the previous implementation.
- `scripts/modal_sandbox.py create --copy-dir local:remote` produces a sandbox with the same files at the same remote paths as before.
- No `DeprecationWarning` from Modal is emitted for `Sandbox.open` or `Sandbox.mkdir` during normal use.
- Large artifact downloads (close to or above Python's comfortable in-memory threshold) succeed without loading the whole file into Python memory.
- The script's CLI surface (commands, subcommands, flags, exit codes, stdout/stderr contracts consumed by the Rust orchestrator) is unchanged.
- Log lines, error message text, and timing may differ in incidental ways where the new API's internals surface differently; the functional contract (file ends up at the right place with the right bytes) is preserved.
- The pinned Modal version in the script's PEP 723 header is a single concrete `modal==X.Y.Z` value, not a range.

## Changes

**Commit 1 — bump `modal` pin**
- Update the PEP 723 dependency header at the top of `scripts/modal_sandbox.py` to hard-pin a current Modal release that exposes the `Sandbox.filesystem` namespace. No code changes in this commit.
- Confirm the existing CI workflow (`.github/workflows/test-modal.yml`) still passes with the new pin and the old `Sandbox.open` / `Sandbox.mkdir` call sites still in place (proves the bump alone is non-breaking).

**Commit 2 — migrate the three deprecated call sites in `scripts/modal_sandbox.py`**
- `copy_dir_to_sandbox`: replace `sandbox.mkdir(remote_dir, parents=True)` with the equivalent call on `sandbox.filesystem.make_directory`.
- `copy_dir_to_sandbox`: replace `with sandbox.open(tar_remote_path, "wb") as f: f.write(tar_data)` with a single `sandbox.filesystem.write_bytes` call against the same path with the same payload.
- `copy_from_sandbox`: replace the `sandbox.open(remote_path, "rb").read()` + local-write block with `sandbox.filesystem.copy_to_local(remote_path, local_path)`, ensuring the local parent directory is still created first (preserve the existing `os.makedirs(local_parent, exist_ok=True)` behavior).
- Keep all surrounding orchestration (logging calls, `sandbox.exec("tar", "-xf", ...)`, `sandbox.exec("rm", ...)`, error messages from the calling commands) unchanged unless the new API forces a tweak.

**Out of scope (explicitly not touched)**
- The Rust crate (`src/**`) — no Modal Python SDK calls there.
- The in-memory tar build (`io.BytesIO()` + `tarfile.open(fileobj=...)`) — restructuring to write the tar directly to disk is a separate, optional optimization tracked as a follow-up if memory pressure ever bites.
- `sandbox.exec`, `sandbox.terminate`, `sandbox.from_id`, `Sandbox.create`, `Sandbox.snapshot_filesystem`, image build APIs — none of these are deprecated.

**Verification**
- CI: `.github/workflows/test-modal.yml` must pass on the migration commit.
- Manual: end-to-end run via `mngr` against Modal exercises both the directory upload (tar path in `copy_dir_to_sandbox`) and the artifact download (`copy_from_sandbox`). Bar for the manual test is at the user's discretion.
- No new automated tests are added.
