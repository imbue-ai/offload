# FastVM Provider -- Specification (Phase 1)

This document specifies the requirements for a Phase 1 FastVM provider POC in Offload.

## Problem

Offload currently supports only Modal as a remote execution provider. Modal uses container sandboxes, which have weaker isolation (shared kernel) and require a proprietary Python SDK for image builds. FastVM offers micro-VMs with sub-100ms boot times, zero-downtime snapshots, native branching (fork one VM into N copies), and per-VM kernel isolation. These properties are architecturally well-suited to Offload's checkpoint and fan-out patterns.

This POC validates that FastVM can run Offload's test suite end-to-end.

## Goals

1. Implement a `FastVMProvider` that satisfies Offload's `SandboxProvider` trait.
2. Support the sandbox lifecycle: create, exec, download, terminate.
3. Run Offload's existing example integration tests on FastVM infrastructure.
4. Use a pre-built snapshot as the VM image (hardcoded in `fastvm_sandbox.py`).
5. Bootstrap SSH access on each sandbox VM for streaming exec and file transfer.

## Non-Goals

- Dockerfile-based image build (future work -- no BuildKit, no `prepare()` pipeline).
- Production-grade reliability (this is a POC).
- Snapshot-based checkpointing (future work; the spec for `[checkpoint]` images is separate).
- Branching-based parallelism (future work; `create_sandbox` restores from snapshot individually).
- Cost estimation (FastVM SDK does not expose pricing).
- Multi-architecture support (x86_64 only for POC).
- `copy_dirs` support (future work; image is pre-built).

## Architecture

### Provider Model

Two options were evaluated:

**Option A (preferred): Call the FastVM REST API directly from Rust.**
The FastVM REST API at `https://api.fastvm.org` is a clean JSON-over-HTTP API with `X-API-Key` authentication and standard HTTP verbs. There is no complex SDK behavior to replicate -- the Python SDK is a thin `httpx` wrapper around these endpoints. Calling the API directly from Rust via `reqwest` gives us: real HTTP streaming potential, binary file handling without Python overhead, and elimination of the Python process spawn per operation.

**Option B: Python wrapper (fallback).**
Same approach as Modal: a `fastvm_sandbox.py` script wrapping the Python SDK, invoked via `uv run`. This is simpler to prototype but adds per-call process overhead and makes streaming impossible.

**Decision: Option A.** The REST API is simple enough that a direct Rust implementation is lower total complexity than maintaining a Python wrapper.

```
Rust (SandboxProvider trait)
  └── FastVMProvider
        └── reqwest HTTP client
              └── FastVM REST API (https://api.fastvm.org)
                    POST /v1/vms                    (create)
                    GET  /v1/vms/{id}               (poll status)
                    POST /v1/vms/{id}/exec           (execute command)
                    DELETE /v1/vms/{id}              (destroy)
                    POST /v1/snapshots               (create snapshot)
                    DELETE /v1/snapshots/{id}         (delete snapshot)
                    POST /v1/vms/{id}/console-token  (WebSocket console)
                    PUT/PATCH /v1/vms/{id}/firewall  (firewall)
```

FastVMProvider reuses `DefaultSandbox` for exec/download/terminate, exactly as `ModalProvider` does today.

### Lifecycle Mapping

| Offload Operation | Phase 1 Implementation |
|---|---|
| `prepare()` | Returns hardcoded snapshot ID (no image build) |
| `create_sandbox()` | `POST /v1/vms` with snapshot source → poll until running → SSH bootstrap (generate keypair, inject pubkey via exec, open firewall, verify SSH) → return VM ID |
| `exec_stream()` | SSH into VM via `ssh -o StrictHostKeyChecking=no -i <privkey> root@<ipv6> <command>`, stream stdout/stderr natively over the SSH channel |
| `download()` | `scp -o StrictHostKeyChecking=no -i <privkey> root@[ipv6]:/remote/path /local/path` over IPv6 |
| `terminate()` | `client.remove(vm)` |

### SSH Bootstrap

SSH bootstrap runs during `create_sandbox()`, after the VM is running:

1. **Restore VM from snapshot** via REST API `POST /v1/vms` with source.
2. **Generate an ephemeral SSH keypair** (ed25519) per sandbox, stored in Rust process memory (never written to disk).
3. **Inject public key** via exec endpoint: `POST /v1/vms/{id}/exec` with command `mkdir -p ~/.ssh && echo '<pubkey>' >> ~/.ssh/authorized_keys && chmod 700 ~/.ssh && chmod 600 ~/.ssh/authorized_keys`.
4. **Open firewall** for port 22 via `PUT /v1/vms/{id}/firewall` (allow TCP 22 inbound).
5. **Verify SSH connectivity**: `ssh -o StrictHostKeyChecking=no -i <privkey> root@<ipv6> echo ok`.
6. **SSH is now ready.** All subsequent exec and download operations use SSH.

The ephemeral keypair is per-sandbox and lives only in memory. When the sandbox is terminated, the VM is deleted and the key is dropped. No key material touches disk.

The pre-built snapshot must have `openssh-server` installed and `sshd` running (add this to the setup script requirements).

### Pre-Built Snapshot (Phase 1 Image Strategy)

Phase 1 does not build images from Dockerfiles. Instead:

1. A **setup script** (`scripts/fastvm_setup_snapshot.py`) is checked into the repo. This script launches a FastVM VM, runs the equivalent of our Dockerfile's setup commands (install deps, copy source, etc.), snapshots it, and prints the snapshot ID.
2. The snapshot ID is **hardcoded** in `fastvm_sandbox.py` as the default image.
3. `prepare()` returns this hardcoded ID without doing any work.

When the base image needs updating (e.g. dependency changes), a developer runs the setup script manually and updates the hardcoded ID. This is intentionally manual for the POC.

Automating Dockerfile → FastVM snapshot is future work (Phase 2).

### Command Execution

Phase 1 uses **SSH for command execution**, not the exec REST endpoint. The exec REST endpoint (`POST /v1/vms/{id}/exec`) is used **only** for the bootstrap step (injecting the SSH public key into the VM).

**SSH-based exec:** The Rust provider runs `ssh -o StrictHostKeyChecking=no -i <privkey> root@<ipv6> <command>`, which streams stdout/stderr natively over the SSH channel. This satisfies Offload's `OutputStream` streaming contract without polling or workarounds.

**Why not the exec REST endpoint for commands?** The exec endpoint is non-streaming (blocks until completion, returns full output in a single response) and subject to server-side stdout/stderr truncation at an undocumented limit. SSH avoids both problems.

The Python SDK includes built-in transient retry for 502/503/504 responses (default 3 retries). Our Rust implementation will replicate this retry behavior for the bootstrap exec call.

### File Transfer

Phase 1 uses **SCP for file transfer** over IPv6. No base64/cat workaround needed.

**Download:**
```
scp -o StrictHostKeyChecking=no -i <privkey> root@[ipv6]:/remote/path /local/path
```

**Upload (if needed):**
```
scp -o StrictHostKeyChecking=no -i <privkey> /local/path root@[ipv6]:/remote/path
```

Phase 1 only needs download (for JUnit XML results). Upload is available but not required.

### Host Dependencies

With Option A (Rust-direct, preferred):

| Dependency | Purpose | Installation |
|---|---|---|
| `reqwest` (Rust crate) | HTTP client for FastVM REST API | Already a common dependency in the workspace |
| `ssh` / `scp` (host binary) | SSH exec and SCP file transfer to VMs | Pre-installed on macOS and Linux |

With Option B (Python wrapper, fallback):

| Dependency | Purpose | Installation |
|---|---|---|
| `fastvm` Python package | VM lifecycle (launch, run, snapshot, restore, remove) | `pip install fastvm` (auto-installed by `uv run`) |

## Configuration

### Environment Variables

| Variable | Purpose | Required |
|---|---|---|
| `FASTVM_API_KEY` | API key for FastVM REST API authentication (`X-API-Key` header) | Yes |

The API key is resolved in priority order: constructor param > `FASTVM_API_KEY` env var > `~/.config/fastvm/config.toml`. For Offload, we require `FASTVM_API_KEY` to be set in the environment.

### New provider type in `offload.toml`

```toml
[provider]
type = "fastvm"
machine = "c2m4"
env = {}
```

### Config Schema

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FastVMProviderConfig {
    /// Machine type passed through to FastVM SDK (e.g. "c1m2", "c2m4").
    /// No validation -- the SDK will reject invalid values.
    #[serde(default = "default_fastvm_machine")]
    pub machine: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
}
```

The config is minimal for Phase 1. Fields like `dockerfile`, `include_cwd`, `copy_dirs` are deferred to Phase 2 when image build is implemented. The `machine` field is a free-form string passed directly to the SDK -- no client-side validation of allowed values.

### Machine Types (informational, not validated)

| FastVM Machine | vCPUs | RAM (GiB) |
|---|---|---|
| `c1m2` | 1 | 2 |
| `c2m4` | 2 | 4 |
| `c4m8` | 4 | 8 |
| `c8m16` | 8 | 16 |

## Constraints

1. **Rust owns all logic.** With Option A, there is no Python layer at all. All VM interaction is direct Rust-to-REST-API via `reqwest`.
2. **No new Rust crate dependencies** unless strictly necessary. `reqwest` is already in the dependency tree. The provider reuses `DefaultSandbox` and existing command infrastructure.
3. **Atomic commits.** Each commit must pass `cargo fmt --check`, `cargo clippy`, `cargo nextest run`.
4. **SSH is used for exec and file transfer.** The exec REST endpoint is used only for the bootstrap step (injecting the SSH public key).
5. **No BuildKit dependency.** Image preparation is manual for Phase 1.
6. **No Python dependency.** Option A eliminates the Python SDK entirely.
7. **Ephemeral SSH keys only.** No key material written to disk. Keys are generated per-sandbox in Rust process memory and dropped on sandbox termination.
8. **Pre-built snapshot must include `openssh-server` with `sshd` enabled.**

## Edge Cases

| Case | Behavior |
|---|---|
| Snapshot ID invalid or expired | Propagate provider error; Offload's retry logic handles transient failures |
| VM fails to launch | Propagate error with VM ID and machine type for debugging |
| FastVM API rate limit | Propagate error; Offload's retry logic retries with backoff |
| Network connectivity loss | SSH/SCP fails; propagate error |
| SSH connection refused after bootstrap | Retry SSH connection with backoff (max 3 attempts, 1s delay). If still failing, fall back to exec endpoint for this sandbox |
| VM has no public_ipv6 | Error: "VM {id} has no public IPv6 address. Cannot establish SSH." |
| Firewall update fails | Error: propagate; sandbox creation fails |

## Success Criteria

1. `offload run -c offload-cargo-fastvm.toml` completes Offload's existing cargo nextest suite on FastVM VMs.
2. Test results match the local provider run (same pass/fail/skip counts).
3. Per-sandbox create time (snapshot restore) is measured and documented.
4. Total wall-clock time for the test suite is measured and compared to Modal.

## Resolved Questions

1. **SSH key management**: SSH keys are ephemeral, generated per-sandbox in Rust process memory, injected via the exec REST endpoint during `create_sandbox()`. No SDK-provided keys needed. No key material written to disk.
2. **IPv6 connectivity**: Required. SSH and SCP connect to the VM's `public_ipv6` address. IPv6 connectivity from the host is a hard requirement.
3. **SDK streaming support (resolved)**: Confirmed non-streaming. The `run()` method is async-only (`httpx.AsyncClient(http2=True)`) but returns a complete `CommandResult` -- there is no streaming variant. The only streaming path is the WebSocket console (`POST /v1/vms/{id}/console-token`), which provides raw TTY access via a short-lived token.
4. **SDK source inspection (resolved)**: Inspected FastVM Python SDK v0.2.3. Findings: clean REST API at `https://api.fastvm.org` with `X-API-Key` auth. No file upload/download API exists -- only `run()` and the WebSocket console. No `branch()`/`fork()` -- just snapshot/restore, where each `restore()` creates an independent VM. VM creation can return 202 (queued), requiring polling `GET /v1/vms/{id}` until `status=="running"` (SDK polls at 0.25s interval). Built-in transient retry for 502/503/504 (default 3 retries).
5. **Branching/forking (resolved)**: There is no `branch()` or `fork()` API. The SDK supports snapshot and restore only. Each `restore()` creates an independent VM. The "branching-based parallelism" mentioned in Non-Goals would need to be built on top of snapshot/restore.

## Open Questions

1. **IPv6 connectivity in CI**: Do our CI runners have IPv6? This is now a hard requirement for SSH and SCP to reach VMs.
2. **VM lifecycle costs**: Is there a per-VM-minute charge even when the VM is idle (post-snapshot, pre-terminate)?
3. **Snapshot durability**: How long do snapshots persist? Do they expire?
4. **VM creation 202 handling**: When `POST /v1/vms` returns 202 (queued), what is the expected wait time? Is there a timeout or failure mode?

## Phase 2 (future work, out of scope)

- Dockerfile → FastVM snapshot pipeline (mechanism TBD; BuildKit is not viable on macOS).
- `copy_dirs` and `include_cwd` support.
- Branching-based parallelism (`client.branch(vm, count=N)`).
