# FastVM Provider -- Implementation Plan (Phase 1)

Implementation plan for the FastVM provider POC (`fastvm-poc.spec.md`).

## Status: DRAFT -- awaiting spec approval, then plan approval

## Key Technical Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Provider pattern | `FastVMSandbox` implementing `Sandbox` trait directly | REST API calls from Rust; no shell-out model needed |
| Image strategy | Pre-built snapshot, hardcoded ID | Simplest thing that works; validates create/exec/download/terminate loop without BuildKit |
| Command execution | SSH over IPv6 | REST exec endpoint bootstraps SSH; then SSH provides native streaming for `exec_stream()` contract |
| REST API from Rust | `reqwest` HTTP client calling `https://api.fastvm.org` directly | Eliminates Python process overhead; enables future HTTP streaming; one fewer dependency |
| File download | SCP over IPv6 | Real file transfer, no base64/truncation risk |
| Streaming exec | Native SSH streaming | SSH channel streams stdout/stderr natively; satisfies `OutputStream` contract |
| SSH keys | Ephemeral ed25519, per-sandbox, in-memory only | No key material on disk; keys dropped on terminate |
| Config type | `type = "fastvm"` in `[provider]` | Parallel to `"modal"`, not a subtype of `"default"` |
| Config validation | Free-form `machine` string, no client-side validation | API rejects bad values; avoids coupling to FastVM's machine list |

## Commit Sequence

Six atomic commits. Each must pass `cargo fmt --check`, `cargo clippy`, `cargo nextest run`.

---

### Commit 1: Config schema -- `FastVMProviderConfig`

**Files:** `src/config/schema.rs`

Add `FastVMProviderConfig` struct:
```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FastVMProviderConfig {
    #[serde(default = "default_fastvm_machine")]
    pub machine: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_fastvm_machine() -> String {
    "c2m4".to_string()
}
```

Add `FastVM(FastVMProviderConfig)` variant to `ProviderConfig` enum.

No validation on `machine` -- the API will reject invalid values at runtime.

Auth: `FASTVM_API_KEY` environment variable, passed via `X-API-Key` header. Documented in example config and README.

Tests:
- `test_fastvm_config_round_trip`
- `test_fastvm_config_defaults`

---

### Commit 2: FastVM REST client -- `src/provider/fastvm_client.rs` (new)

**Files:** `src/provider/fastvm_client.rs` (new)

A thin Rust HTTP client wrapping the FastVM REST API using `reqwest`. This replaces the Python wrapper approach.

Base URL: `https://api.fastvm.org`
Auth: `X-API-Key` header (from `FASTVM_API_KEY` env var).

Structs:
- `FastVMClient` -- holds `reqwest::Client` + `api_key: String` + `base_url: String`
- `VM` -- `id: String`, `status: String`, `public_ipv6: Option<String>`, `machine_name: String`
- `CommandResult` -- `exit_code: i32`, `stdout: String`, `stderr: String`, `timed_out: bool`, `stdout_truncated: bool`, `stderr_truncated: bool`, `duration_ms: u64`
- `Snapshot` -- `id: String`, `name: String`

Methods:
- `FastVMClient::new(api_key: &str) -> Self`
- `create_vm(machine: &str) -> Result<VM>` -- POST /v1/vms, poll GET /v1/vms/{id} until status is "running"
- `get_vm(id: &str) -> Result<VM>` -- GET /v1/vms/{id}
- `delete_vm(id: &str) -> Result<()>` -- DELETE /v1/vms/{id}
- `exec(vm_id: &str, command: &str) -> Result<CommandResult>` -- POST /v1/vms/{id}/exec (used only for SSH bootstrap)
- `update_firewall(vm_id: &str, policy: &FirewallPolicy) -> Result<()>` -- PUT /v1/vms/{id}/firewall
- `create_snapshot(vm_id: &str) -> Result<Snapshot>` -- POST /v1/snapshots
- `delete_snapshot(id: &str) -> Result<()>` -- DELETE /v1/snapshots/{id}
- `restore_snapshot(snapshot_id: &str, machine: &str) -> Result<VM>` -- POST /v1/vms with `source` field

Note: The `exec` endpoint is used only for SSH bootstrap (injecting the public key into `authorized_keys`). All subsequent command execution uses SSH.

Retry: Built-in transient retry for 502/503/504 responses (matching the Python SDK's default of 3 retries).

Tests:
- `test_command_result_deserialize` -- verify JSON deserialization of CommandResult
- `test_vm_deserialize` -- verify JSON deserialization of VM

---

### Commit 3: Snapshot setup script (Python, one-off dev tool)

**Files:** `scripts/fastvm_setup_snapshot.py` (new)

A standalone script to create the pre-built snapshot for Phase 1. Run manually by a developer when the base image needs updating. Uses the `fastvm` Python SDK (not the Rust client) since this is a development tool, not part of the `offload run` flow.

```
uv run scripts/fastvm_setup_snapshot.py --machine=c2m4
```

Flow:
1. Launch FastVM VM with specified machine type.
2. Run setup commands equivalent to our `.devcontainer/Dockerfile`:
   - Install system deps (apt-get), Rust toolchain, cargo-nextest.
   - Install `openssh-server`, ensure `sshd` is enabled and starts on boot.
   - Clone/copy Offload source.
   - `cargo build` to warm the build cache.
3. Snapshot the VM: `client.snapshot(vm)`.
4. Print snapshot ID to stdout.
5. Cleanup: `client.remove(vm)`.

The snapshot must have SSH server ready to accept connections once a key is injected post-restore.

The snapshot ID is then manually configured (hardcoded in the provider or passed via config). This script is a development tool, not part of the `offload run` flow.

---

### Commit 4: `FastVMProvider` + `FastVMSandbox` -- Rust provider implementation

**Files:** `src/provider/fastvm.rs` (new), `src/provider.rs`

Instead of reusing `DefaultSandbox` with shell-out commands, we implement the `Sandbox` trait directly on a `FastVMSandbox` struct that holds a `FastVMClient`, a VM ID, and SSH connection info. After restoring a VM from snapshot, we inject an ephemeral SSH keypair via the exec REST endpoint, open the firewall, then use SSH for all exec and file transfer operations.

```rust
pub struct FastVMProvider {
    config: FastVMProviderConfig,
    client: Arc<FastVMClient>,
    sandbox_project_root: String,
}

pub struct FastVMSandbox {
    client: Arc<FastVMClient>,
    vm_id: String,
    vm_ipv6: String,
    ssh_private_key: String,  // PEM-encoded ephemeral ed25519 key (in memory)
    ssh_key_path: PathBuf,    // Temp file for ssh -i (cleaned up on terminate)
}
```

**POC trade-off:** SSH commands require a key file path (`ssh -i` requires a file). For Phase 1, we write the private key to a temporary file with restricted permissions (0600) in a secure temp directory, cleaned up on `terminate()`. Production would use ssh-agent or in-memory key handling.

Implement `SandboxProvider` trait on `FastVMProvider`:
- `prepare()` -- returns `None` (hardcoded snapshot; no image build in Phase 1)
- `create_sandbox()` flow:
  1. Call `client.restore_snapshot(snapshot_id, machine)` to get a running VM with `public_ipv6`.
  2. Generate ephemeral ed25519 keypair (use `ssh-keygen -t ed25519 -f <tmpfile> -N "" -q`).
  3. Inject public key via `client.exec(vm_id, "mkdir -p ~/.ssh && echo '<pubkey>' >> ~/.ssh/authorized_keys && chmod 700 ~/.ssh && chmod 600 ~/.ssh/authorized_keys")`.
  4. Open firewall: `client.update_firewall(vm_id, allow_tcp_22_inbound)`.
  5. Verify SSH: run `ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 -i <keyfile> root@<ipv6> echo ok` with retries.
  6. Return `FastVMSandbox` with SSH details.

Implement `Sandbox` trait on `FastVMSandbox`:
- `exec_stream()`:
  - Spawn `ssh -o StrictHostKeyChecking=no -i <keyfile> root@<ipv6> <command>` as a child process.
  - Read stdout/stderr as streams, emit `OutputLine::Stdout`/`OutputLine::Stderr` items.
  - When process exits, emit `OutputLine::ExitCode`.
  - This gives real streaming, matching the `OutputStream` contract.
- `download()`:
  - Run `scp -o StrictHostKeyChecking=no -i <keyfile> root@[ipv6]:<remote> <local>` for each path pair.
  - Create parent directories as needed.
- `terminate()`:
  - Call `client.delete_vm(vm_id)`.
  - Delete the temp SSH key file.
- `cost_estimate()`:
  - Return zero for Phase 1 (cost tracking deferred).

Register `fastvm` module in `src/provider.rs`.

Tests:
- `test_ssh_bootstrap_command_generation` -- verify the exec command for key injection
- `test_ssh_exec_command_generation` -- verify the ssh command string
- `test_scp_download_command_generation` -- verify the scp command string

---

### Commit 5: Wire into `main.rs` dispatch

**Files:** `src/main.rs`

Add `ProviderConfig::FastVM` arm to the match in `run_tests()`:
- Construct `FastVMProvider::from_config(...)`.
- Set `sandbox_project_root`.
- `prepare()` returns `None`; proceed to discovery + sandbox creation.
- Dispatch to framework (same pattern as Modal/Default).

Tests:
- `test_fastvm_provider_config_validates` (parse example TOML, verify config loads)

---

### Commit 6: Example config + documentation

**Files:** `offload-cargo-fastvm.toml` (new), `README.md`

Example config:
```toml
[offload]
max_parallel = 4
test_timeout_secs = 600
sandbox_project_root = "/workspace"

[provider]
type = "fastvm"
machine = "c2m4"

[framework]
type = "nextest"

[groups.all]
retry_count = 1

[report]
output_dir = "test-results"
```

No bundled script registration needed (no Python script in the run path).

Update README.md:
- Add FastVM to the provider type table in the Configuration Reference.
- Add `type = "fastvm"` provider config fields.
- Add Prerequisites section for FastVM (`FASTVM_API_KEY` env var, pre-built snapshot).
- Document IPv6 requirement (FastVM VMs are accessed over IPv6).
- Document that snapshots must include `openssh-server`.
- Document `FASTVM_API_KEY` requirement.
- Note Phase 1 / POC status and known limitations.
- Document how to regenerate the snapshot (`scripts/fastvm_setup_snapshot.py`).

---

## Verification Checklist

Automated (every commit):
- [ ] `cargo fmt --check`
- [ ] `cargo clippy` (no warnings)
- [ ] `cargo nextest run`

Manual (after all commits, requires FastVM account):
- [ ] Run `scripts/fastvm_setup_snapshot.py` to create initial snapshot
- [ ] Update hardcoded snapshot ID in provider config
- [ ] `offload run -c offload-cargo-fastvm.toml` completes
- [ ] Test results match local run (same pass/fail/skip counts)
- [ ] Create (snapshot restore) time measured
- [ ] Total wall-clock time measured and compared to Modal
- [ ] SSH bootstrap completes successfully on sandbox creation
- [ ] Streaming exec output appears incrementally (not buffered until completion)
- [ ] SCP file download works for JUnit XML

## Critical Files

| File | Change |
|------|--------|
| `src/config/schema.rs` | Add `FastVMProviderConfig`, wire into `ProviderConfig` enum |
| `src/provider/fastvm_client.rs` | **New** -- thin Rust HTTP client wrapping FastVM REST API via `reqwest` |
| `scripts/fastvm_setup_snapshot.py` | **New** -- manual snapshot creation tool (Python, dev-only) |
| `src/provider/fastvm.rs` | **New** -- `FastVMProvider` + `FastVMSandbox` implementing provider and sandbox traits |
| `src/provider.rs` | Register `fastvm` module |
| `src/main.rs` | Add FastVM dispatch arm |
| `offload-cargo-fastvm.toml` | **New** -- example config |
| `README.md` | FastVM provider documentation |
| `Cargo.toml` | Add `reqwest` dependency (if not already present) |

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| FastVM REST API differs from SDK inspection | Medium | High | Test each endpoint early in commit 2; adapt client |
| IPv6 not available in CI | Medium | High | Verify CI runner IPv6 support early |
| SSH bootstrap adds latency to `create_sandbox()` | Medium | Low | Measure; key injection + firewall update should be <1s |
| Snapshot restore slower than claimed | Low | Medium | Measure; document; still validates the integration |
| `reqwest` dependency | Low | Low | Likely already in Cargo.toml; if not, adding it is justified by eliminating Python process overhead |
