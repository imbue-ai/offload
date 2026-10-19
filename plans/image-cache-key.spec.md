# Image cache key

Status: approved 2026-10-19. This document supersedes the "notes keyed by TOML config path" decision in `plans/checkpoint-images.plan.md` and the matching text in `plans/checkpoint-images.spec.md`.

## Terms

- Checkpoint: the nearest ancestor of HEAD that touches a file listed in `[checkpoint].build_inputs`. In latest-commit mode (no `[checkpoint]` section) the checkpoint is HEAD.
- Base image: the provider image built from the checkpoint's exported tree plus the build settings in the config.
- Thin diff: the patch from the checkpoint to the working tree, applied on top of the base image on every run.
- Note: the JSON object stored in `refs/notes/offload-images` on the checkpoint commit. It maps a key to a cached base image.
- Recipe: the inputs other than the checkpoint tree that determine the base image.
- False reuse: serving a cached image that does not match what the current inputs would build. False invalidation: rebuilding when nothing relevant changed.

## Principle

False reuse is never acceptable. Offload's cache is part of correct operation: a stale image makes tests report results for an environment nobody asked for. Every trade-off below is resolved toward rebuilding.

## Problem

Today the note key is the pair (checkpoint SHA, repo-relative config path). The config path stands in for the recipe, and it is wrong in both directions.

1. False reuse. Editing a field that shapes the base image, such as `sandbox_init_cmd` or `provider.dockerfile`, without touching a `build_inputs` file leaves the checkpoint SHA and the config path unchanged. The next run hits the note and thin-diffs the old image. The new value is never used and nothing is logged. Reproduced against Offload 0.9.14 for a committed `sandbox_init_cmd` edit, an uncommitted edit, a `provider.dockerfile` path change, and latest-commit mode.
2. False invalidation. Configs that share every base-image input but differ in `[framework]`, `[report]`, or `sandbox_project_root` are keyed apart and each builds its own base. In `imbue-ai/default-workspace-template` the three Modal configs rebuilt the same base three times on every checkpoint commit: 155 of 191 noted checkpoints carry three entries, 521 base builds for 191 checkpoints, and about 19 minutes of image preparation per checkpoint commit where one build plus two thin diffs takes about 9.
3. Working-tree leak. In a checkpoint build the Modal script resolves `--include-cwd`, each `--copy-dir`, and `.dockerignore` against the process working directory, which is the live working tree, not the exported checkpoint tree. The gitignored-but-not-dockerignored files that `export_tree` copies in come from the live tree as well. A cached "checkpoint" image therefore depends on what the first builder had on disk, and later thin diffs cannot correct it.

Problems 1 and 2 have one cause and one fix. Problem 3 is independent of the key and is fixed alongside it because it is also false reuse.

## What identifies a cached base image

A cached base image is identified by the checkpoint SHA and the digest of a recipe. The note key is `recipe:` followed by the first 16 hex characters of the SHA-256 of the recipe's canonical JSON.

### Recipe fields

| Field | Source | Why it is in |
|---|---|---|
| `format` | constant in Offload | Lets maintainers invalidate every cache when assembly semantics change in a way the other fields cannot see |
| `provider` | `[provider].type` | Image IDs are provider-specific opaque strings |
| `assembler` | SHA-256 of the bundled `modal_sandbox.py` | The script assembles the image; a change to it or to its pinned Modal SDK may change the image. Invalidates on exactly those releases |
| `dockerfile` | `[provider].dockerfile` | Selects which Dockerfile in the checkpoint tree is built |
| `include_cwd` | `[provider].include_cwd` | Adds the repository tree at `/app` |
| `copy_dirs` | `[provider].copy_dirs`, in order | Directories added to the image |
| `runtime_copy_dirs` | `--copy-dir` flags of `offload run`, in order | Same effect as `copy_dirs`; today `offload build` and `offload run --copy-dir` silently share one entry |
| `sandbox_init_cmd` | `[offload].sandbox_init_cmd` | Runs as the last base-image layer. Empty is normalized to absent |
| `prepare_command` | `[provider].prepare_command` (default provider) | The user's assembler |
| `working_dir` | `[provider].working_dir` (default provider) | Where `prepare_command` runs; relative paths resolve against it |
| `tree_extras` | content digest of the working-tree files that enter the base build outside the checkpoint tree | See "Working-tree inputs" |

Values are taken verbatim from the loaded config. Environment expansion applies only to `[provider].env`, so no value enters a note that is not already in the committed TOML.

### Not in the recipe

| Field | Phase it feeds | Why it is out |
|---|---|---|
| `post_patch_cmd` | thin diff, every run | Including it would rebuild the base on every edit for nothing |
| `sandbox_repo_root` | thin diff (patch apply root) | Does not change the base image |
| `sandbox_project_root` | test execution (`OFFLOAD_ROOT`) | Never reaches the builder |
| `[provider].env`, `cpu_cores`, `memory_gb`, `experimental_options` | sandbox creation | Never reach the builder |
| `[framework]`, `[groups]`, `[report]`, `[history]`, `max_parallel`, `test_timeout_secs`, `[offload].working_dir` | discovery, scheduling, reporting | Never reach the builder |
| `[checkpoint].build_inputs` | checkpoint selection | For a given checkpoint SHA the image is the same however the SHA was chosen |
| Offload's crate version | none | A release that leaves the assembler alone builds the same image; `assembler` and `format` cover the releases that do not |

### Canonical form

The recipe is a single struct serialized with every field present, in declaration order, with absent optional values as `null` and no whitespace. A golden test pins the digest of a fixed recipe so a field reorder or a change in null handling cannot silently split or merge keys. `format` starts at 1 and is bumped only by a deliberate decision.

## Working-tree inputs

The base build must be a function of the checkpoint tree and the recipe.

- The base-build prepare command runs with its working directory set to the exported checkpoint tree, at the same path relative to the repository root as the invoking working directory. `include_cwd`, relative `copy_dirs`, and `.dockerignore` therefore read the checkpoint tree.
- A `copy_dirs` or `--copy-dir` source given as an absolute path inside the repository is rewritten to the corresponding path in the export.
- A source outside the repository cannot come from the export. It is read as today and its content digest is part of `tree_extras`.
- The gitignored-but-not-dockerignored files that `export_tree` copies from the live tree are the only remaining live-tree input. Their paths and content digest are part of `tree_extras`. For a repository whose `.dockerignore` excludes everything `.gitignore` excludes, this set is empty and costs nothing.

The sandbox ends up with the same files as before: what the live tree used to bake into the base now arrives through the export plus the thin diff.

## Note format

The note stays a flat JSON object. Every value is an entry with a required `image_id` string. New entries add optional fields:

```json
{
  "recipe:3f9c2a7b1e5d4c08": {
    "image_id": "im-6Ys5w652ck375MHjQS6nDR",
    "recipe": {
      "format": 1,
      "provider": "modal",
      "assembler": "9b1d...",
      "dockerfile": "system/Dockerfile",
      "include_cwd": false,
      "copy_dirs": [],
      "runtime_copy_dirs": [],
      "sandbox_init_cmd": "default-workspace-template-seed && bash system/scripts/offload_sandbox_init.sh",
      "prepare_command": null,
      "working_dir": null,
      "tree_extras": null
    },
    "config_path": "offload-modal.toml",
    "offload_version": "0.9.15",
    "built_at": "2026-10-19T22:39:45Z"
  },
  "offload-modal.toml": { "image_id": "im-oGLrFv2W0AcxcUw7rFXIzI" }
}
```

`recipe` is stored inline so `git notes show` says what was hashed. `config_path` records which config wrote the entry and is informational only. The second entry above is a legacy entry written by an older Offload.

Two constraints keep older Offload versions working against new notes: never add a top-level key whose value is not an entry, and never drop `image_id`. An older version that cannot parse a note falls back to a full build on every run for that commit and never writes a note, because the full-build fallback deliberately writes none.

## Lookup and write

- Lookup computes the recipe, reads the note on the checkpoint, and takes the entry under the recipe key. The stored `recipe` must equal the current recipe; a key match with a different stored recipe is a miss and a warning.
- Entries without a `recipe` field are legacy entries. They are never used. When they are the only entries present, the miss is logged as "note carries entries written by an older Offload; ignoring".
- After a base build, the entry is written under the recipe key with the merge semantics of today, and pushed.
- `--no-cache` skips lookup and never writes, as today.

## Migration

No config changes. Every existing TOML keeps its meaning. The first run after upgrading misses once per distinct recipe per active checkpoint and rebuilds; from then on configs with the same recipe share one base image. Legacy entries remain in the notes and are ignored. Older Offload versions keep reading and writing their path-keyed entries beside the new ones.

## Debuggability

`offload checkpoint-status` prints the current recipe digest and, on a miss, which entries exist on the checkpoint and which recipe fields differ from the current recipe, so a reader sees "sandbox_init_cmd differs" rather than "none". Legacy entries are listed as such.

## Providers

- Local builds no images and is unaffected.
- Default uses the same pipeline and the same key; `prepare_command` and `working_dir` enter the recipe.
- Modal as described above.

## Costs

- One rebuild per distinct recipe per active checkpoint on the first run after upgrading. The reporting repository pins its Offload version, and bumping it touches a listed build input, so for them the miss coincides with a new checkpoint.
- A rebuild whenever a recipe field changes, including on releases that change the bundled script. These are real inputs.
- For the reporting repository, one base build per checkpoint commit instead of three.

## Implementation plan

Each item is one bead and one commit.

1. `Recipe` struct, canonical serialization, digest, `assembler` from the embedded script, golden-digest test. Adds the `sha2` dependency.
2. `ImageEntry` optional fields, recipe key naming, tests that a legacy note parses and that a new note deserializes under the old entry shape.
3. Lookup and write by recipe key; legacy entries ignored with a log line; `Recipe` threaded through base resolution; tests for "recipe change on the same checkpoint misses" and "two config paths with one recipe hit". Doc comments in `src/config/schema.rs`, `src/git.rs`, and `src/image_cache.rs` updated.
4. `offload checkpoint-status` recipe digest and miss explanation.
5. Base build runs in the exported tree; in-repository absolute copy sources rewritten; out-of-repository sources and the gitignored extras digested into `tree_extras`; a test that the sandbox file set is unchanged for `include_cwd`.
